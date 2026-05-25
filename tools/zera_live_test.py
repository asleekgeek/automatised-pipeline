#!/usr/bin/env python3
"""ZERA S1 — LIVE latency test against Cortex production PG.

The point: prove that across continuous real-data usage, there is AT MOST
ONE wait per session — the initial cold computation of graph_id. Every
subsequent HELLO handshake must complete sub-millisecond, including while
Cortex is actively writing memories and reinforcing relationships.

Protocol under test (S1 HELLO):
  1. Cold (once per session) — server computes canonical graph_id over
     the entire graph; the only wait allowed by the protocol.
  2. Warm (steady state) — client offers its cached graph_id. Server
     checks a low-cost watermark (table counts + max(updated_at)) to
     decide cache_hit vs cache_miss. Sub-millisecond by construction.

This script runs `--duration` seconds of continuous handshakes against
the live Cortex PG, recording the latency of each one, and renders a
timeline HTML showing the result.

source: docs/protocols/ZERA-spec.md §4.2 Layer 0; spec §3 "no latency".
"""
from __future__ import annotations

import argparse
import json
import os
import statistics
import sys
import time
from datetime import datetime, timezone
from pathlib import Path

try:
    import blake3
    import psycopg
except ImportError as e:
    print(f"error: missing dependency ({e}).", file=sys.stderr)
    sys.exit(2)


# ---------------------------------------------------------------------------
# Watermark — cheap signal that the graph hasn't changed since cold load.
# ---------------------------------------------------------------------------

# Constant-time watermark over the GRAPH TABLES only.
#
# pg_stat_user_tables maintains per-table counters (n_tup_ins/upd/del)
# in memory; reading them is O(1) regardless of how big the table is.
# Filtering to the four tables that participate in the graph means
# unrelated writes elsewhere in PG do NOT trip a false cache_miss —
# only changes to memories / entities / relationships / memory_entities do.
#
# In a real ZERA server, this would be a single SELECT against a 1-row
# state table maintained by triggers on those four tables. Functionally
# identical, even cheaper.
WATERMARK_SQL = """
SELECT sum(n_tup_ins + n_tup_upd + n_tup_del)::bigint
FROM pg_stat_user_tables
WHERE relname IN ('memories', 'entities', 'relationships', 'memory_entities')
"""


def cold_handshake(conn) -> tuple[str, int, dict]:
    """Compute the canonical graph_id from scratch. This is the SINGLE
    allowed wait per session under the S1 contract."""
    t0 = time.perf_counter_ns()

    memory_ids, entity_ids, edges = [], [], []
    with conn.cursor(name="cold_mem") as cur:
        cur.execute("SELECT id::text FROM memories")
        for row in cur:
            memory_ids.append(row[0])
    with conn.cursor(name="cold_ent") as cur:
        cur.execute("SELECT id::text FROM entities")
        for row in cur:
            entity_ids.append(row[0])
    with conn.cursor(name="cold_me") as cur:
        cur.execute("SELECT memory_id::text, entity_id::text FROM memory_entities")
        for row in cur:
            edges.append(("mentions", f"memory:{row[0]}", f"entity:{row[1]}"))
    with conn.cursor(name="cold_rel") as cur:
        cur.execute(
            "SELECT source_entity_id::text, target_entity_id::text, "
            "COALESCE(relationship_type, 'related') FROM relationships"
        )
        for row in cur:
            edges.append((str(row[2]), f"entity:{row[0]}", f"entity:{row[1]}"))

    nodes = sorted(set(
        [("Memory", f"memory:{m}") for m in memory_ids]
        + [("Entity", f"entity:{e}") for e in entity_ids]
    ))
    edges = sorted(set(edges))

    h = blake3.blake3()
    h.update(len(nodes).to_bytes(8, "little"))
    for (lbl, ident) in nodes:
        h.update(len(lbl).to_bytes(8, "little")); h.update(lbl.encode())
        h.update(len(ident).to_bytes(8, "little")); h.update(ident.encode())
    h.update(len(edges).to_bytes(8, "little"))
    for (kind, src, dst) in edges:
        h.update(len(kind).to_bytes(8, "little")); h.update(kind.encode())
        h.update(len(src).to_bytes(8, "little")); h.update(src.encode())
        h.update(len(dst).to_bytes(8, "little")); h.update(dst.encode())
    graph_id = h.digest().hex()

    elapsed_ns = time.perf_counter_ns() - t0
    return graph_id, elapsed_ns, {"nodes": len(nodes), "edges": len(edges)}


def read_watermark(conn) -> int:
    """O(1) by design — pg_stat counters filtered to the 4 graph tables."""
    with conn.cursor() as cur:
        cur.execute(WATERMARK_SQL)
        row = cur.fetchone()
        return int(row[0]) if row and row[0] is not None else 0


def warm_handshake(conn, last_wm: int) -> tuple[bool, int, int]:
    """Steady-state HELLO: read counter sum, compare. ~100 µs by design.

    Filtering to the 4 graph tables means writes to unrelated tables
    do NOT trip a cache_miss. Only changes that could affect the graph
    advance the watermark."""
    t0 = time.perf_counter_ns()
    wm = read_watermark(conn)
    elapsed_ns = time.perf_counter_ns() - t0
    return (wm == last_wm), wm, elapsed_ns


# ---------------------------------------------------------------------------
# Rendering
# ---------------------------------------------------------------------------

def human_ns(ns: float) -> str:
    if ns < 1_000:
        return f"{ns:.0f} ns"
    if ns < 1_000_000:
        return f"{ns/1_000:.1f} µs"
    if ns < 1_000_000_000:
        return f"{ns/1_000_000:.2f} ms"
    return f"{ns/1_000_000_000:.2f} s"


def render_html(report: dict, out: Path) -> None:
    waits_ns = [s["elapsed_ns"] for s in report["samples"]]
    misses = [i for i, s in enumerate(report["samples"]) if not s["cache_hit"]]
    p50 = statistics.median(waits_ns) if waits_ns else 0
    p95 = sorted(waits_ns)[int(len(waits_ns) * 0.95)] if waits_ns else 0
    p99 = sorted(waits_ns)[int(len(waits_ns) * 0.99)] if waits_ns else 0
    max_ns = max(waits_ns) if waits_ns else 0

    cold_ns = report["cold_ns"]
    warm_total_ns = sum(waits_ns)

    n_waits = 1 + len(misses)  # the cold + each subsequent miss
    pass_fail = "PASS" if n_waits == 1 else f"FAIL ({n_waits} waits)"
    pass_color = "#34d399" if n_waits == 1 else "#f87171"

    points_x = max(1, len(waits_ns))
    timeline_pts = "".join(
        f'<circle cx="{int(i * 720 / points_x) + 40}" '
        f'cy="{160 - min(140, int((ns / max(1, max_ns)) * 140))}" '
        f'r="2.5" fill="{"#34d399" if report["samples"][i]["cache_hit"] else "#f87171"}" />'
        for i, ns in enumerate(waits_ns)
    )

    html = f"""<!doctype html>
<html><head><meta charset="utf-8">
<title>ZERA S1 — LIVE on Cortex PG ({report['duration_s']}s, {len(waits_ns)} handshakes)</title>
<style>
body {{ font-family: -apple-system, BlinkMacSystemFont, sans-serif; background: #0f172a; color: #e2e8f0;
       max-width: 900px; margin: 32px auto; padding: 0 24px; line-height: 1.5; }}
h1 {{ font-size: 18px; font-weight: 500; margin: 0 0 6px; }}
h2 {{ font-size: 12px; font-weight: 500; margin: 22px 0 8px; color: #94a3b8;
      text-transform: uppercase; letter-spacing: 0.06em; }}
.sub {{ font-size: 12px; color: #94a3b8; margin-bottom: 18px; }}
.panel {{ background: #1e293b; border: 1px solid #334155; border-radius: 6px; padding: 14px 16px; }}
.verdict {{ font-family: 'SF Mono', Menlo, monospace; font-size: 28px; padding: 16px; border-radius: 6px;
           background: #1e293b; border: 1px solid; }}
.row {{ display: flex; justify-content: space-between; margin: 4px 0; font-size: 13px; }}
.row .v {{ font-family: 'SF Mono', Menlo, monospace; color: #67e8f9; }}
.grid3 {{ display: grid; grid-template-columns: 1fr 1fr 1fr; gap: 10px; margin: 12px 0; }}
.stat {{ background: #1e293b; border: 1px solid #334155; border-radius: 6px; padding: 10px 14px; }}
.stat .label {{ font-size: 11px; color: #94a3b8; text-transform: uppercase; letter-spacing: 0.06em; }}
.stat .val {{ font-family: 'SF Mono', Menlo, monospace; font-size: 22px; color: #67e8f9; margin-top: 4px; }}
svg {{ background: #1e293b; border: 1px solid #334155; border-radius: 6px; display: block; margin-top: 4px; }}
.legend {{ font-size: 11px; color: #94a3b8; margin: 6px 0 0; }}
.legend .dot {{ display: inline-block; width: 8px; height: 8px; border-radius: 50%; vertical-align: middle; margin: 0 4px 0 12px; }}
.note {{ font-size: 11px; color: #64748b; margin-top: 22px; line-height: 1.6; }}
</style></head><body>
<h1>ZERA S1 — LIVE latency on Cortex production PG</h1>
<div class="sub">{report['duration_s']} seconds of continuous HELLO handshakes against the live database, polling every {report['interval_ms']} ms.
Spec invariant: <em>at most one wait per session</em>. Graph: {report['stats']['nodes']:,} nodes / {report['stats']['edges']:,} edges deduped.</div>

<div class="verdict" style="color: {pass_color}; border-color: {pass_color};">{pass_fail}
  &nbsp;—&nbsp; 1 cold wait + {len(misses)} cache miss{'es' if len(misses) != 1 else ''} during {len(waits_ns)} warm reconnects</div>

<h2>the one allowed wait</h2>
<div class="panel">
  <div class="row"><span>Cold computation of graph_id (full graph read + BLAKE3)</span><span class="v">{human_ns(cold_ns)}</span></div>
  <div class="row"><span>Result graph_id (BLAKE3, first 16)</span><span class="v">{report['graph_id'][:16]}…</span></div>
  <div class="row"><span>Started at</span><span class="v">{report['started_at']}</span></div>
</div>

<h2>steady-state — {len(waits_ns)} warm handshakes</h2>
<div class="grid3">
  <div class="stat"><div class="label">p50</div><div class="val">{human_ns(p50)}</div></div>
  <div class="stat"><div class="label">p95</div><div class="val">{human_ns(p95)}</div></div>
  <div class="stat"><div class="label">p99</div><div class="val">{human_ns(p99)}</div></div>
</div>
<div class="grid3">
  <div class="stat"><div class="label">max single handshake</div><div class="val">{human_ns(max_ns)}</div></div>
  <div class="stat"><div class="label">total warm wall-clock</div><div class="val">{human_ns(warm_total_ns)}</div></div>
  <div class="stat"><div class="label">cache miss count</div><div class="val">{len(misses)}</div></div>
</div>

<h2>latency timeline (each dot = one warm handshake)</h2>
<svg viewBox="0 0 800 200" width="800" height="200" preserveAspectRatio="none">
  <line x1="40" y1="160" x2="760" y2="160" stroke="#334155" stroke-width="1" />
  <line x1="40" y1="20"  x2="40"  y2="160" stroke="#334155" stroke-width="1" />
  <text x="44" y="14" fill="#94a3b8" font-size="10">latency</text>
  <text x="744" y="178" fill="#94a3b8" font-size="10">time</text>
  <text x="44" y="172" fill="#64748b" font-size="9">0</text>
  <text x="44" y="28" fill="#64748b" font-size="9">{human_ns(max_ns)}</text>
  {timeline_pts}
</svg>
<div class="legend">
  <span class="dot" style="background:#34d399"></span> cache_hit (graph unchanged)
  <span class="dot" style="background:#f87171"></span> cache_miss (graph changed — extra wait)
</div>

<h2>what was measured</h2>
<div class="panel">
  <div class="row"><span>PG database</span><span class="v">{report['db']}</span></div>
  <div class="row"><span>Test duration (seconds)</span><span class="v">{report['duration_s']}</span></div>
  <div class="row"><span>Sample interval (ms)</span><span class="v">{report['interval_ms']}</span></div>
  <div class="row"><span>Handshakes recorded</span><span class="v">{len(waits_ns):,}</span></div>
  <div class="row"><span>Wall-clock total</span><span class="v">{report['duration_s']} s</span></div>
</div>

<p class="note">
The S1 contract says the cache-decision primitive is a watermark comparison: server holds the
current graph_id; client offers its cached one. If they match, zero data flows. Here the
watermark is <code>SELECT sum(n_tup_ins+n_tup_upd+n_tup_del) FROM pg_stat_user_tables
WHERE relname IN ('memories','entities','relationships','memory_entities')</code> — Postgres'
in-memory mutation counters, filtered to the four graph tables. Reading them is O(1) regardless
of how big those tables grow, and writes to unrelated tables do not trip false misses. In a
real ZERA server this would be a single SELECT on a 1-row state table maintained by triggers
on the same four tables — functionally identical, slightly cheaper.
</p>
</body></html>
"""
    out.write_text(html, encoding="utf-8")


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--db", default=os.environ.get("DATABASE_URL", "postgresql://localhost/cortex"))
    ap.add_argument("--duration", type=int, default=30, help="seconds of warm handshake measurement (default 30)")
    ap.add_argument("--interval-ms", type=int, default=100, help="poll interval in ms (default 100)")
    ap.add_argument("--out-dir", default=str(Path(__file__).resolve().parent.parent / "target/zera-s1"))
    args = ap.parse_args()

    out_dir = Path(args.out_dir)
    out_dir.mkdir(parents=True, exist_ok=True)

    print(f"connecting to {args.db}", file=sys.stderr)
    with psycopg.connect(args.db) as conn:
        # PHASE 1 — the one allowed wait
        print("cold handshake (the single allowed wait)…", file=sys.stderr)
        graph_id, cold_ns, stats = cold_handshake(conn)
        baseline_wm = read_watermark(conn)
        print(f"  graph_id = {graph_id[:16]}…  cold = {human_ns(cold_ns)}", file=sys.stderr)
        print(f"  nodes = {stats['nodes']:,}  edges = {stats['edges']:,}", file=sys.stderr)

        # PHASE 2 — warm reconnects for `duration` seconds
        print(f"running warm reconnects for {args.duration}s @ {args.interval_ms}ms intervals…", file=sys.stderr)
        samples = []
        deadline = time.monotonic() + args.duration
        next_tick = time.monotonic()
        miss_count = 0
        while time.monotonic() < deadline:
            cache_hit, wm, elapsed_ns = warm_handshake(conn, baseline_wm)
            sample = {
                "t_offset_s": round(time.monotonic() - (deadline - args.duration), 3),
                "elapsed_ns": elapsed_ns,
                "cache_hit": cache_hit,
            }
            samples.append(sample)
            if not cache_hit:
                miss_count += 1
                # On miss we'd normally recompute graph_id (another wait).
                # Update baseline so the next handshake compares against the
                # new state — but we count this as a wait.
                baseline_wm = wm
            next_tick += args.interval_ms / 1000.0
            sleep_for = next_tick - time.monotonic()
            if sleep_for > 0:
                time.sleep(sleep_for)

    report = {
        "started_at": datetime.now(timezone.utc).isoformat(timespec="seconds"),
        "db": args.db.split("@")[-1] if "@" in args.db else args.db,
        "duration_s": args.duration,
        "interval_ms": args.interval_ms,
        "stats": stats,
        "graph_id": graph_id,
        "cold_ns": cold_ns,
        "samples": samples,
        "n_misses": miss_count,
    }

    out_json = out_dir / "live_test.json"
    out_html = out_dir / "live_test.html"
    out_json.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    render_html(report, out_html)

    n_waits = 1 + miss_count
    waits_ns = [s["elapsed_ns"] for s in samples]
    p50 = statistics.median(waits_ns) if waits_ns else 0
    p95 = sorted(waits_ns)[int(len(waits_ns) * 0.95)] if waits_ns else 0
    p99 = sorted(waits_ns)[int(len(waits_ns) * 0.99)] if waits_ns else 0
    max_ns = max(waits_ns) if waits_ns else 0

    verdict = "PASS" if n_waits == 1 else f"FAIL ({n_waits} waits)"
    print(
        f"\n=== ZERA S1 LIVE — {verdict} ===\n"
        f"cold wait:      {human_ns(cold_ns)}\n"
        f"warm samples:   {len(samples):,} over {args.duration} s\n"
        f"cache_misses:   {miss_count}\n"
        f"warm p50:       {human_ns(p50)}\n"
        f"warm p95:       {human_ns(p95)}\n"
        f"warm p99:       {human_ns(p99)}\n"
        f"warm max:       {human_ns(max_ns)}\n"
        f"report:         {out_html}"
    )
    return 0 if n_waits == 1 else 1


if __name__ == "__main__":
    sys.exit(main())
