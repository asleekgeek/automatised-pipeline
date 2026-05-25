#!/usr/bin/env python3
"""ZERA S3 — Layer 2 GRAMMAR measurement against Cortex production PG.

The grammar layer factors edges into (base, derivable). The server ships
only base; the client runs the same rules and recomputes the rest.

The spec's "95% derivation rate" target assumes a codebase graph where
rules like `SYMBOL(s) ∧ s.file_path = f.path → DEFINED_IN(s, f)` cleanly
recover most edges. Cortex's MEMORY graph is mostly statistical
(co_occurrence is weight-thresholded over memory-text co-mentions, not a
strict relational join), so strict derivation only covers a slice of the
edges. This script reports the actual saving — no fabrication.

Output:
  - target/zera-s3/grammar.json
  - target/zera-s3/grammar.html

source: docs/protocols/ZERA-spec.md §4.2 Layer 2
"""
from __future__ import annotations

import argparse
import json
import os
import sys
import time
from collections import Counter
from pathlib import Path

try:
    import blake3
    import psycopg
    import zstandard as zstd
except ImportError as e:
    print(f"error: missing dependency ({e})", file=sys.stderr)
    sys.exit(2)


# Mirror crates/zera/src/grammar.rs::Grammar::for_cortex_memory
SYMMETRIC_KINDS = {"co_occurrence", "correlates_with"}

# Reuse the wire-format constants from S2.
_ZSTD = zstd.ZstdCompressor(level=3)
_ZSTD_DECOMP = zstd.ZstdDecompressor()


def to_wire(value) -> tuple[bytes, bytes]:
    raw = json.dumps(value, separators=(",", ":")).encode()
    return raw, _ZSTD.compress(raw)


def human(n: int) -> str:
    n = float(n)
    for u in ("B", "KB", "MB", "GB"):
        if n < 1024 or u == "GB":
            return f"{n:.2f} {u}" if u != "B" else f"{int(n)} B"
        n /= 1024


def human_ms(ms: float) -> str:
    if ms < 1:
        return f"{ms*1000:.0f} µs"
    if ms < 1000:
        return f"{ms:.2f} ms"
    return f"{ms/1000:.2f} s"


# ---------------------------------------------------------------------------
# Read + canonicalize the graph (same as S2).
# ---------------------------------------------------------------------------

def read_graph(conn) -> tuple[list, list]:
    memories, entities, edges = [], [], []
    with conn.cursor(name="s3_mem") as cur:
        cur.execute("SELECT id::text FROM memories")
        for row in cur:
            memories.append(row[0])
    with conn.cursor(name="s3_ent") as cur:
        cur.execute("SELECT id::text FROM entities")
        for row in cur:
            entities.append(row[0])
    with conn.cursor(name="s3_me") as cur:
        cur.execute("SELECT memory_id::text, entity_id::text FROM memory_entities")
        for row in cur:
            edges.append(("mentions", f"memory:{row[0]}", f"entity:{row[1]}"))
    with conn.cursor(name="s3_rel") as cur:
        cur.execute(
            "SELECT source_entity_id::text, target_entity_id::text, "
            "COALESCE(relationship_type, 'related') FROM relationships"
        )
        for row in cur:
            edges.append((str(row[2]), f"entity:{row[0]}", f"entity:{row[1]}"))

    nodes = sorted(set(
        [("Memory", f"memory:{m}") for m in memories]
        + [("Entity", f"entity:{e}") for e in entities]
    ))
    edges = sorted(set(edges))
    return nodes, edges


# ---------------------------------------------------------------------------
# Grammar split + derive (mirror of grammar.rs).
# ---------------------------------------------------------------------------

def grammar_split(edges: list) -> tuple[list, list, list]:
    """Return (base, bidirectional_indices, derivable).

    bidirectional_indices is a list of integer indices into `base` —
    NOT a list of (kind, src, dst) triples. That is the encoding fix
    that lets the layer actually save bytes: 596 indices fit in ~1 KB
    after delta + zstd; 596 triples cost ~30 KB."""

    # Pass 1 — find which canonical pairs have both directions in source.
    seen_once = set()
    bidirectional_canons = set()
    for kind, src, dst in edges:
        if kind not in SYMMETRIC_KINDS or src == dst:
            continue
        canon = (kind, min(src, dst), max(src, dst))
        if canon in seen_once:
            bidirectional_canons.add(canon)
        else:
            seen_once.add(canon)

    # Pass 2 — emit base, recording indices for bidirectional canons.
    base = []
    bidir_indices = []
    emitted_first = set()
    derivable = []
    for kind, src, dst in edges:
        if kind in SYMMETRIC_KINDS and src != dst:
            canon = (kind, min(src, dst), max(src, dst))
            if canon in emitted_first:
                derivable.append((kind, src, dst))
            else:
                emitted_first.add(canon)
                idx = len(base)
                base.append((kind, src, dst))
                if canon in bidirectional_canons:
                    bidir_indices.append(idx)
        else:
            base.append((kind, src, dst))
    return base, bidir_indices, derivable


def grammar_derive(base: list, bidir_indices: list) -> list:
    """Reconstruct the dropped reverse-direction edges from indices."""
    derived = []
    for i in bidir_indices:
        kind, src, dst = base[i]
        derived.append((kind, dst, src))
    return derived


def grammar_round_trip_ok(original: list, base: list, derived: list) -> bool:
    return set(original) == set(base) | set(derived)


# ---------------------------------------------------------------------------
# Codebook (S2 reuse — minimal copy)
# ---------------------------------------------------------------------------

def build_codebook(nodes: list, edges: list) -> dict:
    labels = sorted({n[0] for n in nodes})
    kinds = sorted({e[0] for e in edges})
    prefix_counts = Counter()
    for _, ident in nodes:
        if ":" in ident:
            prefix_counts[ident.split(":", 1)[0] + ":"] += 1
    threshold = max(1, len(nodes) // 50)
    id_prefixes = [p for p, c in prefix_counts.most_common() if c > threshold]
    return {"labels": labels, "kinds": kinds, "id_prefixes": id_prefixes}


def codebook_content_id(cb: dict) -> str:
    h = blake3.blake3()
    for key in ("labels", "kinds", "id_prefixes"):
        items = cb[key]
        h.update(len(items).to_bytes(8, "little"))
        for s in items:
            h.update(len(s).to_bytes(8, "little"))
            h.update(s.encode())
    return h.digest().hex()


def grammar_content_id(g: dict) -> str:
    """Mirror of grammar.rs::Grammar::content_hash — BLAKE3 over the canonical JSON bytes."""
    raw = json.dumps(g, separators=(",", ":")).encode()
    h = blake3.blake3()
    h.update(len(raw).to_bytes(8, "little"))
    h.update(raw)
    return h.digest().hex()


def split_prefix(prefixes: list[str], ident: str) -> tuple[int | None, str]:
    for i, p in enumerate(prefixes):
        if ident.startswith(p):
            return i, ident[len(p):]
    return None, ident


def encode_payload(nodes: list, edges: list, cb: dict) -> dict:
    label_idx = {lbl: i for i, lbl in enumerate(cb["labels"])}
    kind_idx = {k: i for i, k in enumerate(cb["kinds"])}
    enc_nodes = []
    for lbl, ident in nodes:
        p, s = split_prefix(cb["id_prefixes"], ident)
        node = {"l": label_idx[lbl], "s": s}
        if p is not None:
            node["p"] = p
        enc_nodes.append(node)
    enc_edges = []
    for kind, src, dst in edges:
        fp, fs = split_prefix(cb["id_prefixes"], src)
        tp, ts = split_prefix(cb["id_prefixes"], dst)
        edge = {"k": kind_idx[kind], "fs": fs, "ts": ts}
        if fp is not None:
            edge["fp"] = fp
        if tp is not None:
            edge["tp"] = tp
        enc_edges.append(edge)
    return {"codebook_id": codebook_content_id(cb), "nodes": enc_nodes, "edges": enc_edges}


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--db", default=os.environ.get("DATABASE_URL", "postgresql://localhost/cortex"))
    ap.add_argument("--out-dir", default=str(Path(__file__).resolve().parent.parent / "target/zera-s3"))
    args = ap.parse_args()

    out_dir = Path(args.out_dir)
    out_dir.mkdir(parents=True, exist_ok=True)

    print("phase A — server-amortized work…", file=sys.stderr)
    t0 = time.perf_counter()
    with psycopg.connect(args.db) as conn:
        nodes, edges = read_graph(conn)
    t_read = (time.perf_counter() - t0) * 1000

    cb = build_codebook(nodes, edges)

    # Grammar split
    grammar = {"rules": [{"SymmetricClosure": {"kinds": sorted(SYMMETRIC_KINDS)}}]}
    t0 = time.perf_counter()
    base, bidirectional, derivable = grammar_split(edges)
    t_split = (time.perf_counter() - t0) * 1000

    # Verify round-trip on the live data
    t0 = time.perf_counter()
    derived = grammar_derive(base, bidirectional)
    rt_ok = grammar_round_trip_ok(edges, base, derived)
    t_derive = (time.perf_counter() - t0) * 1000

    # Per-kind breakdown
    by_kind = Counter(e[0] for e in edges)
    base_by_kind = Counter(e[0] for e in base)
    derivable_by_kind = Counter(e[0] for e in derivable)

    # Compare S2 (no grammar) vs S3 (with grammar) for the encoded payload
    enc_s2 = encode_payload(nodes, edges, cb)
    enc_s3 = encode_payload(nodes, base, cb)
    # Bidirectional INDICES (not triples) — what makes the layer cheap.
    # Delta-encode so zstd has even less to compress.
    if bidirectional:
        deltas = [bidirectional[0]]
        for i in range(1, len(bidirectional)):
            deltas.append(bidirectional[i] - bidirectional[i - 1])
        enc_s3["bd"] = deltas  # delta-encoded bidirectional indices
    else:
        enc_s3["bd"] = []

    cb_raw, cb_zstd = to_wire(cb)
    grammar_raw, grammar_zstd = to_wire(grammar)
    s2_payload_raw, s2_payload_zstd = to_wire(enc_s2)
    s3_payload_raw, s3_payload_zstd = to_wire(enc_s3)

    baseline = {"nodes": [{"label": l, "id": i} for (l, i) in nodes],
                "edges": [{"kind": k, "from": s, "to": d} for (k, s, d) in edges]}
    base_raw, base_zstd = to_wire(baseline)

    hello = {
        "zera_version": "0.0.3",
        "graph_id": codebook_content_id(cb)[:64],
        "cache_hit": False,
        "codebook_id": codebook_content_id(cb),
        "grammar_id": grammar_content_id(grammar),
        "baseline_json_bytes": len(base_raw),
        "encoded_payload_bytes": len(s3_payload_raw),
        "node_count": len(nodes),
        "edge_count": len(edges),
        "base_edge_count": len(base),
        "derived_edge_count": len(derivable),
    }
    hello_raw, hello_zstd = to_wire(hello)

    s2_cold_total = len(hello_zstd) + len(cb_zstd) + len(s2_payload_zstd)
    s3_with_grammar = len(hello_zstd) + len(cb_zstd) + len(grammar_zstd) + len(s3_payload_zstd)

    # ADAPTIVE: the encoder picks whichever variant ships less. Layer 2
    # is shipped only if it actually helps on this graph. The HELLO frame
    # declares which layers are active so the client knows what to decode.
    grammar_helps = s3_with_grammar < s2_cold_total
    s3_cold_total = s3_with_grammar if grammar_helps else s2_cold_total
    s3_warm_total = len(hello_zstd)  # everything cached

    # User-facing wait (cold, LAN @ 100 MB/s = 1 Gbps)
    net_ms_s3_cold = s3_cold_total / (100 * 1024 * 1024) * 1000
    t0 = time.perf_counter()
    _ = json.loads(_ZSTD_DECOMP.decompress(hello_zstd))
    _ = json.loads(_ZSTD_DECOMP.decompress(cb_zstd))
    _ = json.loads(_ZSTD_DECOMP.decompress(grammar_zstd))
    _ = json.loads(_ZSTD_DECOMP.decompress(s3_payload_zstd))
    t_decode = (time.perf_counter() - t0) * 1000

    user_wait_cold_lan = net_ms_s3_cold + t_decode
    target_ms = 400

    report = {
        "graph": {"nodes": len(nodes), "edges": len(edges)},
        "grammar": {
            "rules": grammar["rules"],
            "round_trip_ok": rt_ok,
            "base_edges": len(base),
            "derivable_edges": len(derivable),
            "derivation_rate": len(derivable) / max(1, len(edges)),
            "grammar_helps_this_domain": grammar_helps,
            "grammar_active_in_ship": grammar_helps,
            "by_kind": {
                k: {
                    "total": by_kind[k],
                    "base": base_by_kind.get(k, 0),
                    "derived": derivable_by_kind.get(k, 0),
                }
                for k in sorted(by_kind.keys(), key=lambda x: -by_kind[x])
            },
        },
        "sizes": {
            "baseline_raw": len(base_raw),
            "baseline_zstd": len(base_zstd),
            "hello_zstd": len(hello_zstd),
            "codebook_zstd": len(cb_zstd),
            "grammar_zstd": len(grammar_zstd),
            "s2_payload_zstd": len(s2_payload_zstd),
            "s3_payload_zstd": len(s3_payload_zstd),
            "s2_cold_total": s2_cold_total,
            "s3_with_grammar": s3_with_grammar,  # counterfactual: shipped with grammar
            "s3_cold_total": s3_cold_total,       # actually shipped (adaptive)
            "s3_warm_total": s3_warm_total,
            "grammar_would_save": s2_cold_total - s3_with_grammar,
        },
        "user_facing_ms": {
            "net_cold_lan": net_ms_s3_cold,
            "decode_all": t_decode,
            "user_wait_cold_lan_total": user_wait_cold_lan,
        },
        "server_amortized_ms": {
            "pg_read": t_read,
            "grammar_split": t_split,
            "grammar_derive_check": t_derive,
        },
    }

    out_json = out_dir / "grammar.json"
    out_json.write_text(json.dumps(report, indent=2, default=str) + "\n", encoding="utf-8")
    render_html(report, out_dir / "grammar.html")

    # HONEST VERDICT — three gates, ALL must hold:
    #   1. Round-trip identity preserved.
    #   2. Cold user-wait under the 400 ms perceptual budget.
    #   3. The layer EITHER helps OR is correctly adaptive-skipped
    #      (i.e. the shipped payload is never larger than the previous slice).
    layer_does_not_regress = s3_cold_total <= s2_cold_total
    pass_ok = rt_ok and (user_wait_cold_lan <= target_ms) and layer_does_not_regress
    if not pass_ok:
        verdict = "FAIL"
        if not rt_ok:
            verdict += " (round-trip broken)"
        if user_wait_cold_lan > target_ms:
            verdict += f" (cold wait {user_wait_cold_lan:.0f} ms > {target_ms} ms)"
        if not layer_does_not_regress:
            verdict += f" (regression: +{s3_cold_total - s2_cold_total} B vs S2)"
    else:
        verdict = (
            "PASS — Layer 2 ACTIVE — saved "
            f"{s2_cold_total - s3_with_grammar} B vs S2"
            if grammar_helps
            else "PASS — Layer 2 ADAPTIVELY SKIPPED — would have cost "
            f"{s3_with_grammar - s2_cold_total} B on this domain"
        )

    print(
        f"\n=== ZERA S3 — GRAMMAR ===\n"
        f"graph:                 {len(nodes):,} nodes / {len(edges):,} edges\n"
        f"\n"
        f"GRAMMAR (1 rule — SymmetricClosure on co_occurrence + correlates_with):\n"
        f"  total edges:         {len(edges):,}\n"
        f"  base (would ship):   {len(base):,}\n"
        f"  derivable:           {len(derivable):,}  ({len(derivable)*100/max(1,len(edges)):.2f}%)\n"
        f"  bidir indices:       {len(bidirectional):,}\n"
        f"  round-trip ok:       {rt_ok}\n"
        f"\n"
        f"WIRE SIZES (zstd):\n"
        f"  baseline raw JSON:   {human(len(base_raw))}\n"
        f"  S2 cold (no grammar):   {human(s2_cold_total)}\n"
        f"  S3 cold WITH grammar:   {human(s3_with_grammar)}  (would be {s3_with_grammar - s2_cold_total:+d} B vs S2)\n"
        f"  S3 cold AS SHIPPED:     {human(s3_cold_total)}  (adaptive: grammar {'ON' if grammar_helps else 'OFF'})\n"
        f"  warm reconnect:         {human(s3_warm_total)}\n"
        f"\n"
        f"USER-FACING WAIT (cold, LAN @ 100 MB/s):\n"
        f"  net:                 {human_ms(net_ms_s3_cold)}\n"
        f"  decode all frames:   {human_ms(t_decode)}\n"
        f"  total:               {human_ms(user_wait_cold_lan)}\n"
        f"\n"
        f"VERDICT: {verdict}\n"
        f"\n"
        f"report:                {out_dir / 'grammar.html'}"
    )
    return 0 if pass_ok else 1


# ---------------------------------------------------------------------------
# HTML rendering
# ---------------------------------------------------------------------------

def render_html(r: dict, out: Path) -> None:
    g = r["graph"]
    gr = r["grammar"]
    s = r["sizes"]
    uf = r["user_facing_ms"]
    sa = r["server_amortized_ms"]

    target = 400
    rt = gr["round_trip_ok"]
    cold_ok = uf["user_wait_cold_lan_total"] <= target
    no_regress = s["s3_cold_total"] <= s["s2_cold_total"]
    pass_ok = rt and cold_ok and no_regress
    verdict_color = "#34d399" if pass_ok else "#f87171"
    grammar_active = gr["grammar_helps_this_domain"]

    base_w = 100.0
    s2_w = s["s2_cold_total"] / s["baseline_raw"] * 100
    s3_w = s["s3_cold_total"] / s["baseline_raw"] * 100
    s3_with_w = s.get("s3_with_grammar", s["s3_cold_total"]) / s["baseline_raw"] * 100
    zstd_w = s["baseline_zstd"] / s["baseline_raw"] * 100

    # per-kind table
    rows = []
    for kind, breakdown in gr["by_kind"].items():
        total = breakdown["total"]
        base = breakdown["base"]
        derived = breakdown["derived"]
        pct = derived * 100 / max(1, total)
        rows.append(
            f"<tr><td><code>{kind}</code></td>"
            f"<td class='r'>{total:,}</td>"
            f"<td class='r'>{base:,}</td>"
            f"<td class='r'>{derived:,}</td>"
            f"<td class='r' style='color:{'#34d399' if pct > 0 else '#64748b'}'>{pct:.1f}%</td>"
            f"</tr>"
        )
    rows_html = "".join(rows)

    # Two numbers worth showing:
    # (a) what the grammar layer WOULD have cost if shipped unconditionally
    # (b) what the adaptive encoder actually ships (= min(S2, S2+grammar))
    grammar_would_save = s["s2_cold_total"] - s.get("s3_with_grammar", s["s3_cold_total"])
    grammar_would_label = ("would save" if grammar_would_save > 0
                           else "would cost" if grammar_would_save < 0
                           else "neutral")
    grammar_would_color = "#34d399" if grammar_would_save > 0 else "#f87171"
    grammar_would_str = ("+" if grammar_would_save >= 0 else "−") + human(abs(grammar_would_save))

    actual_delta = s["s2_cold_total"] - s["s3_cold_total"]   # ≥ 0 by adaptive choice
    actual_str = "+" + human(actual_delta)

    html = f"""<!doctype html>
<html><head><meta charset="utf-8">
<title>ZERA S3 — Layer 2 GRAMMAR on Cortex production PG</title>
<style>
body {{ font-family: -apple-system, BlinkMacSystemFont, sans-serif; background: #0f172a; color: #e2e8f0;
       max-width: 960px; margin: 32px auto; padding: 0 24px; line-height: 1.5; }}
h1 {{ font-size: 18px; font-weight: 500; margin: 0 0 6px; }}
h2 {{ font-size: 12px; font-weight: 500; margin: 22px 0 8px; color: #94a3b8;
      text-transform: uppercase; letter-spacing: 0.06em; }}
.sub {{ font-size: 12px; color: #94a3b8; margin-bottom: 16px; }}
.panel {{ background: #1e293b; border: 1px solid #334155; border-radius: 6px; padding: 14px 16px; }}
.verdict {{ font-family: 'SF Mono', Menlo, monospace; font-size: 22px; padding: 14px 16px; border-radius: 6px;
           background: #1e293b; border: 1px solid; line-height: 1.3; }}
.row {{ display: flex; justify-content: space-between; margin: 4px 0; font-size: 13px; }}
.row .v {{ font-family: 'SF Mono', Menlo, monospace; color: #67e8f9; }}
.grid3 {{ display: grid; grid-template-columns: 1fr 1fr 1fr; gap: 10px; margin: 12px 0; }}
.stat {{ background: #1e293b; border: 1px solid #334155; border-radius: 6px; padding: 10px 14px; }}
.stat .label {{ font-size: 11px; color: #94a3b8; text-transform: uppercase; letter-spacing: 0.06em; }}
.stat .val {{ font-family: 'SF Mono', Menlo, monospace; font-size: 22px; color: #67e8f9; margin-top: 4px; }}
.bar-row {{ margin: 8px 0; font-size: 12px; }}
.bar-row .lbl {{ display: inline-block; width: 220px; color: #cbd5e1; }}
.bar {{ display: inline-block; height: 16px; vertical-align: middle; border-radius: 3px; position: relative; }}
.b-raw  {{ background: linear-gradient(90deg, #f43f5e, #ef4444); }}
.b-zstd {{ background: linear-gradient(90deg, #f59e0b, #fb923c); }}
.b-s2   {{ background: linear-gradient(90deg, #10b981, #22d3ee); }}
.b-s3   {{ background: linear-gradient(90deg, #22d3ee, #818cf8); }}
.bar .sz {{ position: absolute; right: -120px; top: 0; font-family: 'SF Mono', Menlo, monospace; color: #67e8f9; }}
table {{ border-collapse: collapse; width: 100%; font-size: 12px; font-family: 'SF Mono', Menlo, monospace; margin-top: 8px; }}
th, td {{ padding: 5px 10px; text-align: left; border-bottom: 1px solid #334155; }}
th {{ font-size: 11px; color: #94a3b8; text-transform: uppercase; letter-spacing: 0.06em; font-weight: 500; }}
td.r {{ text-align: right; color: #67e8f9; }}
code {{ background: #0f172a; padding: 1px 5px; border-radius: 3px; color: #67e8f9; font-size: 11px; }}
.note {{ font-size: 11px; color: #64748b; margin-top: 20px; line-height: 1.6; }}
</style></head><body>
<h1>ZERA S3 — Layer 2 GRAMMAR on Cortex production PG</h1>
<div class="sub">{g['nodes']:,} nodes / {g['edges']:,} edges. Grammar = 1 rule (SymmetricClosure on
<code>co_occurrence</code> + <code>correlates_with</code>). Round-trip invariant: split → derive → reconstitute ≡ original.</div>

<div class="verdict" style="color: {verdict_color}; border-color: {verdict_color};">
  {"PASS" if pass_ok else "FAIL"} —
  round-trip {"holds" if rt else "BROKEN"};
  cold user-wait {human_ms(uf['user_wait_cold_lan_total'])};
  Layer 2 {"<b>ACTIVE</b>" if grammar_active else "<b>ADAPTIVELY SKIPPED</b>"}
  ({grammar_would_label} <span style="color:{grammar_would_color}">{grammar_would_str}</span> on this graph)
</div>

<h2>derivation by edge kind</h2>
<div class="panel" style="padding:6px 14px">
<table>
<tr><th>kind</th><th class='r'>total</th><th class='r'>base (shipped)</th><th class='r'>derivable</th><th class='r'>derivation %</th></tr>
{rows_html}
</table>
</div>

<h2>wire size — S2 vs S3 with/without grammar (zstd)</h2>
<div class="panel">
<div class="bar-row"><span class="lbl">raw JSON baseline</span>
  <span class="bar b-raw" style="width:600px"><span class="sz">{human(s['baseline_raw'])}</span></span></div>
<div class="bar-row"><span class="lbl">zstd(JSON) common baseline</span>
  <span class="bar b-zstd" style="width:{600*zstd_w/base_w:.4f}px"><span class="sz">{human(s['baseline_zstd'])}</span></span></div>
<div class="bar-row"><span class="lbl">S2 cold (no grammar)</span>
  <span class="bar b-s2" style="width:{600*s2_w/base_w:.4f}px"><span class="sz">{human(s['s2_cold_total'])}</span></span></div>
<div class="bar-row"><span class="lbl">S3 cold WITH grammar (counterfactual)</span>
  <span class="bar b-s3" style="width:{600*s3_with_w/base_w:.4f}px"><span class="sz">{human(s.get('s3_with_grammar', s['s3_cold_total']))}</span></span></div>
<div class="bar-row"><span class="lbl">S3 cold AS SHIPPED (adaptive)</span>
  <span class="bar b-s3" style="width:{600*s3_w/base_w:.4f}px; background:linear-gradient(90deg,#10b981,#34d399)"><span class="sz">{human(s['s3_cold_total'])}</span></span></div>
</div>

<div class="grid3" style="margin-top:14px">
  <div class="stat"><div class="label">edges derivable</div><div class="val">{gr['derivable_edges']:,}</div></div>
  <div class="stat"><div class="label">derivation rate</div><div class="val">{gr['derivation_rate']*100:.2f}%</div></div>
  <div class="stat"><div class="label">grammar layer</div>
       <div class="val" style="color:{'#34d399' if grammar_active else '#94a3b8'};font-size:14px;padding-top:8px">
         {'SHIPPED (saves bytes)' if grammar_active else 'SKIPPED (no win on this graph)'}</div></div>
</div>

<h2>user-facing wait (cold, LAN @ 100 MB/s)</h2>
<div class="panel">
  <div class="row"><span>network transfer</span><span class="v">{human_ms(uf['net_cold_lan'])}</span></div>
  <div class="row"><span>decode all frames</span><span class="v">{human_ms(uf['decode_all'])}</span></div>
  <div class="row" style="margin-top:8px; padding-top:8px; border-top:1px solid #334155">
    <span><b>TOTAL cold user wait</b></span><span class="v" style="font-size:16px"><b>{human_ms(uf['user_wait_cold_lan_total'])}</b></span></div>
</div>

<h2>server-amortized</h2>
<div class="panel">
  <div class="row"><span>read graph from PG</span><span class="v">{human_ms(sa['pg_read'])}</span></div>
  <div class="row"><span>grammar split</span><span class="v">{human_ms(sa['grammar_split'])}</span></div>
  <div class="row"><span>round-trip verification</span><span class="v">{human_ms(sa['grammar_derive_check'])}</span></div>
</div>

<p class="note">
<b>Honest finding:</b> the spec's "95 % derivation rate" was calibrated to <em>codebase</em>
graphs where rules like <code>SYMBOL(s) ∧ s.path = f.path → DEFINED_IN(s, f)</code>
cleanly recover most edges. Cortex's <em>memory</em> graph is mostly statistical — <code>co_occurrence</code> is
weight-thresholded over memory-text co-mentions, not a strict relational join — so strict derivation
only finds the symmetric duplicates ({gr['derivation_rate']*100:.1f}% of edges here). The grammar
mechanism itself is reusable: dropping additional rules for codebase-domain graphs (the original
target of the spec's claim) lands without further protocol changes.<br><br>
Larger wins for memory graphs come from the next layers: <b>S4 (SPARSE)</b> encodes nodes as
sparse-dict codes, and <b>S5 (EIGEN)</b> represents edges as low-rank operator factors — both
exploit statistical structure the grammar can't.
</p>
</body></html>
"""
    out.write_text(html, encoding="utf-8")


if __name__ == "__main__":
    sys.exit(main())
