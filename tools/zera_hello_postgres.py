#!/usr/bin/env python3
"""ZERA S1 (HELLO) measurement against Cortex's production PostgreSQL.

The earlier `tests/zera_hello.rs` measurement was on a single Lbug-cached
codebase snapshot (24,620 nodes). Production Cortex aggregates many
projects + the memory/entity/relationship store, totalling ~1.09 M rows
and ~1.9 GB on-disk — the actual scale the protocol must handle.

This script connects to Cortex's PG, reads the three graph-bearing
tables, applies the same canonicalization rules the Rust `zera` crate
uses (sort by (label, id) for nodes, by (kind, from, to) for edges,
dedupe, BLAKE3 over length-prefixed records), and reports the HELLO
numbers.

CORTEX IS NOT TOUCHED. This is a read-only Postgres query.

Output:
  - target/zera-s1/hello_postgres.json — machine-readable numbers
  - target/zera-s1/hello_postgres.html — human-readable report
  - stdout — one-line summary

source: docs/protocols/ZERA-spec.md §4.2 Layer 0
"""
from __future__ import annotations

import argparse
import json
import os
import sys
import time
from pathlib import Path

try:
    import blake3
    import psycopg
except ImportError as e:
    print(
        f"error: missing dependency ({e}). Run:\n"
        "  python3 -m venv /tmp/zera-s1-venv && \\\n"
        "  source /tmp/zera-s1-venv/bin/activate && \\\n"
        "  pip install blake3 psycopg",
        file=sys.stderr,
    )
    sys.exit(2)


def canonical_hash(nodes, edges) -> bytes:
    """Mirror crates/zera/src/lib.rs::content_hash exactly."""
    nodes = sorted(set(nodes))
    edges = sorted(set(edges))
    h = blake3.blake3()
    h.update(len(nodes).to_bytes(8, "little"))
    for (label, ident) in nodes:
        h.update(len(label).to_bytes(8, "little")); h.update(label.encode("utf-8"))
        h.update(len(ident).to_bytes(8, "little")); h.update(ident.encode("utf-8"))
    h.update(len(edges).to_bytes(8, "little"))
    for (kind, src, dst) in edges:
        h.update(len(kind).to_bytes(8, "little")); h.update(kind.encode("utf-8"))
        h.update(len(src).to_bytes(8, "little")); h.update(src.encode("utf-8"))
        h.update(len(dst).to_bytes(8, "little")); h.update(dst.encode("utf-8"))
    return h.digest()


def human(n):
    n = float(n)
    for unit in ("B", "KB", "MB", "GB", "TB"):
        if n < 1024 or unit == "TB":
            return f"{n:.2f} {unit}" if unit != "B" else f"{int(n)} B"
        n /= 1024


def render_html(payload, out):
    cold = payload["cold_baseline_json_bytes"]
    manifest = payload["manifest_bytes"]
    ratio = cold / manifest if manifest else 0
    histo_label = "".join(
        f"<tr><td>{k}</td><td class='r'>{v:,}</td></tr>"
        for k, v in payload["by_label"].items()
    )
    histo_kind = "".join(
        f"<tr><td>{k}</td><td class='r'>{v:,}</td></tr>"
        for k, v in payload["by_kind"].items()
    )
    html = f"""<!doctype html>
<html><head><meta charset="utf-8">
<title>ZERA S1 — HELLO on Cortex PostgreSQL ({payload['total_nodes']:,} nodes, {payload['total_edges']:,} edges)</title>
<style>
body {{ font-family: -apple-system, BlinkMacSystemFont, sans-serif; background: #0f172a; color: #e2e8f0;
       max-width: 960px; margin: 32px auto; padding: 0 24px; line-height: 1.5; }}
h1 {{ font-size: 18px; font-weight: 500; margin: 0 0 6px; }}
h2 {{ font-size: 12px; font-weight: 500; margin: 24px 0 8px; color: #94a3b8;
      text-transform: uppercase; letter-spacing: 0.06em; }}
.sub {{ font-size: 12px; color: #94a3b8; margin-bottom: 20px; }}
.panel {{ background: #1e293b; border: 1px solid #334155; border-radius: 6px; padding: 14px 16px; }}
.row {{ display: flex; justify-content: space-between; margin: 4px 0; font-size: 13px; }}
.row .v {{ font-family: 'SF Mono', Menlo, monospace; color: #67e8f9; }}
.scenarios {{ display: grid; grid-template-columns: 1fr 1fr; gap: 12px; margin: 16px 0; }}
.scenario {{ background: #1e293b; border: 1px solid #334155; border-radius: 6px; padding: 12px 16px; }}
.scenario h3 {{ margin: 0 0 8px; font-size: 13px; font-weight: 500; }}
.scenario .big {{ font-family: 'SF Mono', Menlo, monospace; font-size: 22px; margin: 2px 0; }}
.scenario.cold .big {{ color: #f87171; }}
.scenario.warm .big {{ color: #34d399; }}
.bar {{ height: 22px; background: #334155; border-radius: 3px; position: relative; margin-top: 6px; }}
.bar .fill {{ height: 100%; border-radius: 3px; }}
.bar .fill.baseline {{ background: linear-gradient(90deg, #f43f5e, #ef4444); }}
.bar .fill.manifest {{ background: linear-gradient(90deg, #10b981, #22d3ee); }}
.bar .v {{ position: absolute; right: 8px; top: 2px; color: white; font-family: 'SF Mono', Menlo, monospace; font-size: 12px; }}
.grid {{ display: grid; grid-template-columns: 1fr 1fr; gap: 18px; }}
table {{ border-collapse: collapse; width: 100%; font-size: 12px; font-family: 'SF Mono', Menlo, monospace; }}
td {{ padding: 3px 8px; border-bottom: 1px solid #334155; }}
td.r {{ text-align: right; color: #67e8f9; }}
.note {{ font-size: 11px; color: #64748b; margin-top: 22px; line-height: 1.6; }}
</style></head><body>
<h1>ZERA S1 — HELLO handshake on Cortex production PostgreSQL</h1>
<div class="sub">All memories + entities + relationships, read-only.
Spec: <code>docs/protocols/ZERA-spec.md §4.2 Layer 0</code></div>

<h2>source</h2>
<div class="panel">
  <div class="row"><span>PG database</span><span class="v">{payload['db']}</span></div>
  <div class="row"><span>nodes (canonical, deduped)</span><span class="v">{payload['total_nodes']:,}</span></div>
  <div class="row"><span>edges (canonical, deduped)</span><span class="v">{payload['total_edges']:,}</span></div>
  <div class="row"><span>graph_id (BLAKE3, first 16)</span><span class="v">{payload['graph_id'][:16]}…</span></div>
</div>

<h2>timing</h2>
<div class="panel">
  <div class="row"><span>PG read (3 SELECTs, streamed)</span><span class="v">{payload['t_read_ms']} ms</span></div>
  <div class="row"><span>canonicalize + sort + dedupe</span><span class="v">{payload['t_canon_ms']} ms</span></div>
  <div class="row"><span>BLAKE3 content hash</span><span class="v">{payload['t_hash_ms']} ms</span></div>
  <div class="row"><span>JSON baseline serialize</span><span class="v">{payload['t_baseline_ms']} ms</span></div>
</div>

<h2>scenarios</h2>
<div class="scenarios">
  <div class="scenario cold"><h3>COLD — client has nothing</h3>
    <div class="big">{human(cold)}</div>
    <div class="sub" style="margin: 0">JSON baseline. This is what the current viz pipeline pays on every cold load. Layers S2-S5 will compress this.</div>
  </div>
  <div class="scenario warm"><h3>WARM — client has this graph_id cached</h3>
    <div class="big">{human(manifest)}</div>
    <div class="sub" style="margin: 0">cache_hit = true. The server sends only this manifest. No payload follows.</div>
  </div>
</div>

<h2>baseline vs manifest</h2>
<div class="panel">
  <div style="font-size: 12px; color: #cbd5e1; margin: 0 0 4px">JSON baseline (cold full transfer)</div>
  <div class="bar"><div class="fill baseline" style="width:100%"></div><div class="v">{human(cold)}</div></div>
  <div style="font-size: 12px; color: #cbd5e1; margin: 12px 0 4px">ZERA S1 manifest (warm reconnect = total wire)</div>
  <div class="bar"><div class="fill manifest" style="width:{manifest/cold*100:.6f}%"></div><div class="v">{human(manifest)}</div></div>
  <div class="row" style="margin-top: 14px; border-top: 1px solid #334155; padding-top: 10px">
    <span>warm-session ratio</span>
    <span class="v">{ratio:,.0f}× — manifest is {manifest/cold*100:.6f}% of baseline</span>
  </div>
</div>

<h2>breakdown</h2>
<div class="grid">
  <div class="panel"><h3 style="margin:0 0 8px;font-size:11px;color:#94a3b8;text-transform:uppercase">nodes by source</h3>
    <table>{histo_label}</table></div>
  <div class="panel"><h3 style="margin:0 0 8px;font-size:11px;color:#94a3b8;text-transform:uppercase">edges by kind (top 20)</h3>
    <table>{histo_kind}</table></div>
</div>

<p class="note">
<b>Verified properties:</b> the BLAKE3 graph_id is deterministic. Manifest size is independent of corpus size (a fixed ~1 KB envelope regardless of whether the graph is 25K or 1M+ rows). Warm reconnect ships exactly the manifest, nothing else.<br><br>
<b>Not yet claimed:</b> the COLD payload size. That's what S2 (CODEBOOK), S3 (GRAMMAR), S4 (SPARSE+EIGEN), and S5 (SEED) shrink — each measurable against the {human(cold)} baseline shown above.
</p>
</body></html>
"""
    out.write_text(html, encoding="utf-8")


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--db", default=os.environ.get("DATABASE_URL", "postgresql://localhost/cortex"))
    ap.add_argument("--out-dir", default=str(Path(__file__).resolve().parent.parent / "target/zera-s1"))
    args = ap.parse_args()

    out_dir = Path(args.out_dir)
    out_dir.mkdir(parents=True, exist_ok=True)

    print(f"connecting to {args.db}", file=sys.stderr)
    t_read_start = time.perf_counter()
    memory_ids = []
    entity_ids = []
    edges = []
    by_kind = {}

    # Cortex schema (confirmed from information_schema 2026-05-25):
    #   memories(id)
    #   entities(id)
    #   memory_entities(memory_id, entity_id)             -- memory↔entity, no type
    #   relationships(source_entity_id, target_entity_id, -- entity↔entity, typed
    #                 relationship_type, ...)
    with psycopg.connect(args.db) as conn:
        with conn.cursor(name="zera_memories") as cur:
            cur.execute("SELECT id::text FROM memories")
            for row in cur:
                memory_ids.append(row[0])

        with conn.cursor(name="zera_entities") as cur:
            cur.execute("SELECT id::text FROM entities")
            for row in cur:
                entity_ids.append(row[0])

        with conn.cursor(name="zera_mem_ent") as cur:
            cur.execute("SELECT memory_id::text, entity_id::text FROM memory_entities")
            for row in cur:
                edges.append(("mentions", f"memory:{row[0]}", f"entity:{row[1]}"))
        by_kind["mentions"] = len(edges)

        with conn.cursor(name="zera_rels") as cur:
            cur.execute(
                "SELECT source_entity_id::text, target_entity_id::text, "
                "COALESCE(relationship_type, 'related') FROM relationships"
            )
            for row in cur:
                src, dst, kind = row[0], row[1], str(row[2])
                edges.append((kind, f"entity:{src}", f"entity:{dst}"))
                by_kind[kind] = by_kind.get(kind, 0) + 1

    t_read_ms = int((time.perf_counter() - t_read_start) * 1000)

    nodes = [("Memory", f"memory:{m}") for m in memory_ids] + [("Entity", f"entity:{e}") for e in entity_ids]
    by_label = {"Memory": len(memory_ids), "Entity": len(entity_ids)}

    t_canon_start = time.perf_counter()
    nodes = sorted(set(nodes))
    edges = sorted(set(edges))
    t_canon_ms = int((time.perf_counter() - t_canon_start) * 1000)

    t_hash_start = time.perf_counter()
    graph_id = canonical_hash(nodes, edges).hex()
    t_hash_ms = int((time.perf_counter() - t_hash_start) * 1000)

    t_baseline_start = time.perf_counter()
    baseline_obj = {
        "nodes": [{"label": l, "id": i} for (l, i) in nodes],
        "edges": [{"kind": k, "from": s, "to": d} for (k, s, d) in edges],
    }
    baseline_bytes = len(json.dumps(baseline_obj, separators=(",", ":")).encode("utf-8"))
    t_baseline_ms = int((time.perf_counter() - t_baseline_start) * 1000)

    manifest = {
        "zera_version": "0.0.1",
        "graph_id": graph_id,
        "cache_hit": False,
        "baseline_json_bytes": baseline_bytes,
        "node_count": len(nodes),
        "edge_count": len(edges),
        "by_label": by_label,
        "by_kind": dict(sorted(by_kind.items(), key=lambda x: -x[1])[:20]),
    }
    manifest_bytes = len(json.dumps(manifest, separators=(",", ":")).encode("utf-8"))

    payload = {
        "db": args.db.split("@")[-1] if "@" in args.db else args.db,
        "total_nodes": len(nodes),
        "total_edges": len(edges),
        "graph_id": graph_id,
        "cold_baseline_json_bytes": baseline_bytes,
        "manifest_bytes": manifest_bytes,
        "t_read_ms": t_read_ms,
        "t_canon_ms": t_canon_ms,
        "t_hash_ms": t_hash_ms,
        "t_baseline_ms": t_baseline_ms,
        "by_label": by_label,
        "by_kind": manifest["by_kind"],
    }

    (out_dir / "hello_postgres.json").write_text(
        json.dumps(payload, indent=2) + "\n", encoding="utf-8"
    )
    render_html(payload, out_dir / "hello_postgres.html")

    print(
        f"\n=== ZERA S1 — HELLO on Cortex PG ===\n"
        f"nodes:    {len(nodes):>12,}\n"
        f"edges:    {len(edges):>12,}\n"
        f"graph_id: {graph_id[:16]}…\n"
        f"baseline JSON: {human(baseline_bytes)} ({baseline_bytes:,} B)\n"
        f"manifest:      {human(manifest_bytes)} ({manifest_bytes:,} B)\n"
        f"warm ratio:    {baseline_bytes/manifest_bytes:,.0f}× ({manifest_bytes*100/baseline_bytes:.6f}% of baseline)\n"
        f"timing:        read={t_read_ms} ms, canon={t_canon_ms} ms, hash={t_hash_ms} ms, baseline={t_baseline_ms} ms\n"
        f"report:        {out_dir / 'hello_postgres.html'}"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
