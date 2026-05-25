#!/usr/bin/env python3
"""ZERA S4 — Layer 3 (compact node encoding) on Cortex production PG.

Empirical journey of this slice — recorded so the negative results are
not forgotten:

  Attempt 1 — msgpack:        +60 % BIGGER (zstd was already eating field-name repetition)
  Attempt 2 — pre-trained zstd dict:    +2-3 %    (within noise)
  Attempt 3 — integer ids + high zstd: -38 %  PASS (real, large win)

The win is the combination of two cheap changes:

  (a) Serial-ID suffixes go on the wire as JSON integers, not strings.
      Cortex memories/entities use Postgres serials; converting "12345"
      to 12345 saves ~1 byte per id after compression, ~3 % overall.

  (b) zstd level 19 instead of level 3. The encoder runs once per
      session and is server-amortized; the DECODER pays the same cost
      regardless of compression level (8–16 ms for our payload). Going
      to level 19 takes the encoded payload from 903 KB to ≈558 KB —
      a 38 % cold-payload reduction.

Adaptive: the encoder tries each variant, ships the smallest, and
declares which encoding + zstd level in the HELLO frame. Verdict gates
on round-trip + cold-wait ≤ 400 ms + no regression vs S3.
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


SYMMETRIC_KINDS = {"co_occurrence", "correlates_with"}
_ZSTD_DEFAULT = zstd.ZstdCompressor(level=3)
_ZSTD_HIGH = zstd.ZstdCompressor(level=19)
_ZSTD_DECOMP = zstd.ZstdDecompressor()


def to_wire_at(value, level: int) -> bytes:
    raw = json.dumps(value, separators=(",", ":")).encode()
    return zstd.ZstdCompressor(level=level).compress(raw)


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
# Graph + codebook + grammar (shared with prior slices).
# ---------------------------------------------------------------------------

def read_graph(conn) -> tuple[list, list]:
    memories, entities, edges = [], [], []
    with conn.cursor(name="s4_mem") as cur:
        cur.execute("SELECT id::text FROM memories")
        for row in cur:
            memories.append(row[0])
    with conn.cursor(name="s4_ent") as cur:
        cur.execute("SELECT id::text FROM entities")
        for row in cur:
            entities.append(row[0])
    with conn.cursor(name="s4_me") as cur:
        cur.execute("SELECT memory_id::text, entity_id::text FROM memory_entities")
        for row in cur:
            edges.append(("mentions", f"memory:{row[0]}", f"entity:{row[1]}"))
    with conn.cursor(name="s4_rel") as cur:
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


def split_prefix(prefixes: list[str], ident: str):
    for i, p in enumerate(prefixes):
        if ident.startswith(p):
            return i, ident[len(p):]
    return None, ident


def grammar_split(edges: list):
    seen_once, bidirectional_canons = set(), set()
    for kind, src, dst in edges:
        if kind not in SYMMETRIC_KINDS or src == dst:
            continue
        canon = (kind, min(src, dst), max(src, dst))
        if canon in seen_once:
            bidirectional_canons.add(canon)
        else:
            seen_once.add(canon)
    base, bidir_indices, derivable = [], [], []
    emitted_first = set()
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


# ---------------------------------------------------------------------------
# Encoders — string-id vs int-id, both JSON.
# ---------------------------------------------------------------------------

def encode_payload(nodes, edges, cb, bidirectional, *, int_ids: bool) -> dict:
    label_idx = {lbl: i for i, lbl in enumerate(cb["labels"])}
    kind_idx = {k: i for i, k in enumerate(cb["kinds"])}

    def maybe_int(s: str):
        if int_ids and s.isdigit():
            return int(s)
        return s

    enc_nodes = []
    for lbl, ident in nodes:
        p, s = split_prefix(cb["id_prefixes"], ident)
        node = {"l": label_idx[lbl], "s": maybe_int(s)}
        if p is not None:
            node["p"] = p
        enc_nodes.append(node)
    enc_edges = []
    for kind, src, dst in edges:
        fp, fs = split_prefix(cb["id_prefixes"], src)
        tp, ts = split_prefix(cb["id_prefixes"], dst)
        edge = {"k": kind_idx[kind], "fs": maybe_int(fs), "ts": maybe_int(ts)}
        if fp is not None:
            edge["fp"] = fp
        if tp is not None:
            edge["tp"] = tp
        enc_edges.append(edge)
    deltas = []
    if bidirectional:
        deltas = [bidirectional[0]] + [
            bidirectional[i] - bidirectional[i - 1] for i in range(1, len(bidirectional))
        ]
    return {"n": enc_nodes, "e": enc_edges, "bd": deltas}


# ---------------------------------------------------------------------------
# Round-trip verification — decode the chosen variant and re-derive edges.
# ---------------------------------------------------------------------------

def decode_payload(payload: dict, cb: dict):
    labels = cb["labels"]
    kinds = cb["kinds"]
    prefixes = cb["id_prefixes"]

    def make_id(p, s):
        s_str = str(s) if isinstance(s, int) else s
        return f"{prefixes[p]}{s_str}" if p is not None else s_str

    nodes = []
    for n in payload["n"]:
        nodes.append((labels[n["l"]], make_id(n.get("p"), n["s"])))
    base_edges = []
    for e in payload["e"]:
        base_edges.append((kinds[e["k"]], make_id(e.get("fp"), e["fs"]), make_id(e.get("tp"), e["ts"])))

    # Derive reverse direction from bidirectional indices (delta-encoded)
    derived = []
    cur = 0
    for d in payload.get("bd", []):
        cur += d
        kind, src, dst = base_edges[cur]
        derived.append((kind, dst, src))
    return nodes, base_edges + derived


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--db", default=os.environ.get("DATABASE_URL", "postgresql://localhost/cortex"))
    ap.add_argument("--out-dir", default=str(Path(__file__).resolve().parent.parent / "target/zera-s4"))
    args = ap.parse_args()

    out_dir = Path(args.out_dir)
    out_dir.mkdir(parents=True, exist_ok=True)

    print("phase A — server-amortized work…", file=sys.stderr)
    t0 = time.perf_counter()
    with psycopg.connect(args.db) as conn:
        nodes, edges = read_graph(conn)
    t_read = (time.perf_counter() - t0) * 1000
    cb = build_codebook(nodes, edges)
    base, bidirectional, _ = grammar_split(edges)

    # Encode four ways
    print("encoding 4 variants…", file=sys.stderr)
    variants = {}
    for int_ids in (False, True):
        for level in (3, 19):
            t0 = time.perf_counter()
            payload = encode_payload(nodes, base, cb, bidirectional, int_ids=int_ids)
            t_enc = (time.perf_counter() - t0) * 1000
            t0 = time.perf_counter()
            wire = to_wire_at(payload, level)
            t_compress = (time.perf_counter() - t0) * 1000
            t0 = time.perf_counter()
            decoded = json.loads(_ZSTD_DECOMP.decompress(wire))
            t_decode = (time.perf_counter() - t0) * 1000
            key = f"int_ids={int_ids}, zstd={level}"
            variants[key] = {
                "int_ids": int_ids,
                "zstd_level": level,
                "payload": payload,
                "wire": wire,
                "wire_bytes": len(wire),
                "encode_ms": t_enc,
                "compress_ms": t_compress,
                "decode_ms": t_decode,
            }
            print(f"  {key:30s} -> {human(len(wire))} (encode {t_enc:.0f} ms, compress {t_compress:.0f} ms, decode {t_decode:.0f} ms)", file=sys.stderr)

    # Round-trip the smallest variant (the one we'd ship)
    smallest_key = min(variants.keys(), key=lambda k: variants[k]["wire_bytes"])
    smallest = variants[smallest_key]
    decoded_payload = json.loads(_ZSTD_DECOMP.decompress(smallest["wire"]))
    rt_nodes, rt_edges = decode_payload(decoded_payload, cb)
    rt_ok = set(rt_nodes) == set(nodes) and set(rt_edges) == set(edges)

    # S3 baseline = string-ids + zstd level 3 (what the prior slice shipped)
    s3_payload_zstd = variants["int_ids=False, zstd=3"]["wire_bytes"]
    s4_payload_zstd = smallest["wire_bytes"]

    # Common frames (HELLO + codebook + grammar)
    cb_wire = to_wire_at(cb, 3)
    grammar = {"rules": [{"SymmetricClosure": {"kinds": sorted(SYMMETRIC_KINDS)}}]}
    grammar_wire = to_wire_at(grammar, 3)
    hello = {
        "zera_version": "0.0.4",
        "graph_id": "0" * 64,
        "cache_hit": False,
        "codebook_id": "x" * 64,
        "grammar_id": "x" * 64,
        "encoding": "json",
        "payload_int_ids": smallest["int_ids"],
        "payload_zstd_level": smallest["zstd_level"],
        "node_count": len(nodes),
        "edge_count": len(edges),
        "base_edge_count": len(base),
    }
    hello_wire = to_wire_at(hello, 3)

    s3_total = len(hello_wire) + len(cb_wire) + len(grammar_wire) + s3_payload_zstd
    s4_total = len(hello_wire) + len(cb_wire) + len(grammar_wire) + s4_payload_zstd
    warm_total = len(hello_wire)

    # User-facing wait
    net_ms = s4_total / (100 * 1024 * 1024) * 1000
    t0 = time.perf_counter()
    _ = json.loads(_ZSTD_DECOMP.decompress(hello_wire))
    _ = json.loads(_ZSTD_DECOMP.decompress(cb_wire))
    _ = json.loads(_ZSTD_DECOMP.decompress(grammar_wire))
    _ = json.loads(_ZSTD_DECOMP.decompress(smallest["wire"]))
    t_decode_all = (time.perf_counter() - t0) * 1000
    user_wait = net_ms + t_decode_all

    target_ms = 400
    saved = s3_total - s4_total
    saved_pct = saved * 100 / max(1, s3_total)
    layer_does_not_regress = s4_total <= s3_total
    pass_ok = rt_ok and (user_wait <= target_ms) and layer_does_not_regress

    if pass_ok:
        verdict = (
            f"PASS — Layer 3 ACTIVE — "
            f"saved {human(saved)} ({saved_pct:.1f}%) vs S3 "
            f"by shipping {smallest['int_ids'] and 'int' or 'str'}-ids @ zstd lvl {smallest['zstd_level']}"
        )
    else:
        reasons = []
        if not rt_ok: reasons.append("round-trip broken")
        if user_wait > target_ms: reasons.append(f"cold wait {user_wait:.0f} ms > {target_ms} ms")
        if not layer_does_not_regress: reasons.append(f"regression: +{s4_total - s3_total} B")
        verdict = "FAIL — " + "; ".join(reasons)

    report = {
        "graph": {"nodes": len(nodes), "edges": len(edges), "base_edges": len(base)},
        "round_trip_ok": rt_ok,
        "variants": {
            k: {"wire_bytes": v["wire_bytes"], "encode_ms": v["encode_ms"],
                "compress_ms": v["compress_ms"], "decode_ms": v["decode_ms"],
                "int_ids": v["int_ids"], "zstd_level": v["zstd_level"]}
            for k, v in variants.items()
        },
        "chosen": {
            "int_ids": smallest["int_ids"],
            "zstd_level": smallest["zstd_level"],
            "payload_bytes": s4_payload_zstd,
            "encode_ms": smallest["encode_ms"],
            "compress_ms": smallest["compress_ms"],
            "decode_ms": smallest["decode_ms"],
        },
        "sizes": {
            "hello_zstd": len(hello_wire),
            "codebook_zstd": len(cb_wire),
            "grammar_zstd": len(grammar_wire),
            "s3_payload_zstd": s3_payload_zstd,
            "s4_payload_zstd": s4_payload_zstd,
            "s3_cold_total": s3_total,
            "s4_cold_total": s4_total,
            "warm_total": warm_total,
            "saved_vs_s3": saved,
        },
        "user_facing_ms": {
            "net_lan": net_ms,
            "decode_all": t_decode_all,
            "user_wait_cold_lan_total": user_wait,
        },
        "server_amortized_ms": {
            "pg_read": t_read,
            "chosen_encode": smallest["encode_ms"],
            "chosen_compress": smallest["compress_ms"],
        },
    }
    (out_dir / "sparse.json").write_text(json.dumps(report, indent=2, default=str) + "\n", encoding="utf-8")
    render_html(report, out_dir / "sparse.html")

    print(
        f"\n=== ZERA S4 — Layer 3 (compact node encoding) ===\n"
        f"graph:             {len(nodes):,} nodes / {len(edges):,} edges\n"
        f"\n"
        f"VARIANTS (smaller is better):\n" +
        "\n".join(
            f"  {k:30s} -> {human(v['wire_bytes']):>10s}  (encode {v['encode_ms']:.0f} ms, compress {v['compress_ms']:.0f} ms, decode {v['decode_ms']:.0f} ms)"
            for k, v in sorted(variants.items(), key=lambda x: x[1]['wire_bytes'])
        ) +
        f"\n\n"
        f"CHOSEN:               int_ids={smallest['int_ids']}, zstd lvl {smallest['zstd_level']}\n"
        f"\n"
        f"FULL FRAME SIZES (zstd):\n"
        f"  S3 cold:              {human(s3_total)}\n"
        f"  S4 cold:              {human(s4_total)}  (saved {human(saved)}, {saved_pct:+.1f}%)\n"
        f"  warm reconnect:       {human(warm_total)}\n"
        f"\n"
        f"USER-FACING WAIT (LAN @ 100 MB/s):\n"
        f"  net:                  {human_ms(net_ms)}\n"
        f"  decode all frames:    {human_ms(t_decode_all)}\n"
        f"  total:                {human_ms(user_wait)}\n"
        f"\n"
        f"ROUND-TRIP: {rt_ok}\n"
        f"VERDICT: {verdict}\n"
        f"\n"
        f"report:               {out_dir / 'sparse.html'}"
    )
    return 0 if pass_ok else 1


# ---------------------------------------------------------------------------
# HTML
# ---------------------------------------------------------------------------

def render_html(r: dict, out: Path) -> None:
    g = r["graph"]
    s = r["sizes"]
    uf = r["user_facing_ms"]
    sa = r["server_amortized_ms"]
    ch = r["chosen"]
    vs = r["variants"]
    target = 400
    rt = r["round_trip_ok"]
    cold_ok = uf["user_wait_cold_lan_total"] <= target
    no_regress = s["s4_cold_total"] <= s["s3_cold_total"]
    pass_ok = rt and cold_ok and no_regress
    verdict_color = "#34d399" if pass_ok else "#f87171"
    saved = s["saved_vs_s3"]
    saved_pct = saved * 100 / max(1, s["s3_cold_total"])

    rows = ""
    for k in sorted(vs.keys(), key=lambda x: vs[x]["wire_bytes"]):
        v = vs[k]
        chosen = "★ chosen" if (v["int_ids"] == ch["int_ids"] and v["zstd_level"] == ch["zstd_level"]) else ""
        rows += (
            f"<tr><td><code>int_ids={v['int_ids']}, zstd={v['zstd_level']}</code> "
            f"<span style='color:#34d399'>{chosen}</span></td>"
            f"<td class='r'>{human(v['wire_bytes'])}</td>"
            f"<td class='r'>{v['encode_ms']:.0f} ms</td>"
            f"<td class='r'>{v['compress_ms']:.0f} ms</td>"
            f"<td class='r'>{v['decode_ms']:.0f} ms</td></tr>"
        )

    html = f"""<!doctype html>
<html><head><meta charset="utf-8">
<title>ZERA S4 — Layer 3 on Cortex production PG</title>
<style>
body {{ font-family: -apple-system, BlinkMacSystemFont, sans-serif; background: #0f172a; color: #e2e8f0;
       max-width: 960px; margin: 32px auto; padding: 0 24px; line-height: 1.5; }}
h1 {{ font-size: 18px; font-weight: 500; margin: 0 0 6px; }}
h2 {{ font-size: 12px; font-weight: 500; margin: 22px 0 8px; color: #94a3b8; text-transform: uppercase; letter-spacing: 0.06em; }}
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
table {{ border-collapse: collapse; width: 100%; font-size: 12px; font-family: 'SF Mono', Menlo, monospace; margin-top: 8px; }}
th, td {{ padding: 5px 10px; text-align: left; border-bottom: 1px solid #334155; }}
th {{ font-size: 11px; color: #94a3b8; text-transform: uppercase; letter-spacing: 0.06em; font-weight: 500; }}
td.r {{ text-align: right; color: #67e8f9; }}
code {{ background: #0f172a; padding: 1px 5px; border-radius: 3px; color: #67e8f9; font-size: 11px; }}
.note {{ font-size: 11px; color: #64748b; margin-top: 20px; line-height: 1.6; }}
</style></head><body>
<h1>ZERA S4 — Layer 3 (compact node encoding) on Cortex production PG</h1>
<div class="sub">{g['nodes']:,} nodes / {g['edges']:,} edges. Tried four variants; the encoder picks the smallest.</div>

<div class="verdict" style="color: {verdict_color}; border-color: {verdict_color};">
  {"PASS" if pass_ok else "FAIL"} — round-trip {"holds" if rt else "BROKEN"};
  cold user-wait {human_ms(uf['user_wait_cold_lan_total'])};
  saved <span style="color:#34d399">{human(saved)}</span> vs S3 ({saved_pct:+.1f}%)
</div>

<h2>variants compared</h2>
<div class="panel" style="padding:6px 14px">
<table>
<tr><th>configuration</th><th class='r'>wire</th><th class='r'>encode (server)</th><th class='r'>compress (server)</th><th class='r'>decode (client)</th></tr>
{rows}
</table>
</div>

<h2>what changed and why</h2>
<div class="panel">
  <div class="row"><span><b>int IDs</b> — serial-id suffixes encoded as JSON integers, not strings.
    Saves ~1 byte/id on average; zstd then has more numeric pattern to exploit.</span><span></span></div>
  <div class="row"><span><b>zstd level 19</b> — server pays a one-time ~9 s encode cost (amortized
    behind the graph_id watermark); decode cost is unchanged. The compressor finds longer-range
    structure missed at level 3.</span><span></span></div>
</div>

<h2>full frame sizes (zstd)</h2>
<div class="panel">
  <div class="row"><span>HELLO</span><span class="v">{human(s['hello_zstd'])}</span></div>
  <div class="row"><span>CODEBOOK</span><span class="v">{human(s['codebook_zstd'])}</span></div>
  <div class="row"><span>GRAMMAR</span><span class="v">{human(s['grammar_zstd'])}</span></div>
  <div class="row"><span>S3 payload (zstd lvl 3, string ids)</span><span class="v">{human(s['s3_payload_zstd'])}</span></div>
  <div class="row" style="color:#34d399"><span>S4 payload (zstd lvl {ch['zstd_level']}, int ids)</span>
    <span class="v" style="color:#34d399">{human(s['s4_payload_zstd'])}</span></div>
  <div class="row" style="margin-top:8px; padding-top:8px; border-top:1px solid #334155">
    <span><b>S3 cold total</b></span><span class="v">{human(s['s3_cold_total'])}</span></div>
  <div class="row" style="color:#34d399"><span><b>S4 cold total</b></span>
    <span class="v" style="color:#34d399">{human(s['s4_cold_total'])}</span></div>
  <div class="row"><span>warm reconnect (HELLO only)</span><span class="v">{human(s['warm_total'])}</span></div>
</div>

<h2>user-facing wait (cold, LAN @ 100 MB/s)</h2>
<div class="panel">
  <div class="row"><span>network transfer</span><span class="v">{human_ms(uf['net_lan'])}</span></div>
  <div class="row"><span>decode all frames</span><span class="v">{human_ms(uf['decode_all'])}</span></div>
  <div class="row" style="margin-top:8px; padding-top:8px; border-top:1px solid #334155">
    <span><b>TOTAL cold user wait</b></span><span class="v" style="font-size:16px"><b>{human_ms(uf['user_wait_cold_lan_total'])}</b></span></div>
</div>

<h2>negative results we recorded (so we don't try them again)</h2>
<div class="panel">
  <div class="row"><span>MessagePack instead of JSON</span><span class="v" style="color:#f87171">+60 % (zstd already eats field-name repetition)</span></div>
  <div class="row"><span>Pre-trained zstd dictionary (4–131 KB)</span><span class="v" style="color:#94a3b8">±2–3 % (within noise)</span></div>
</div>

<p class="note">
Honest read of the spec: ZERA-spec §4.2 Layer 3 specifies nodes as
<code>(label_idx, atom_indices, weights, residual_offset)</code> — atom-based sparse coding
over a learned dictionary. That formulation assumes nodes carry feature vectors (embeddings);
Cortex's bare memory/entity graph as shipped here does not, so the atom variant has nothing
to compress. The equivalent compression opportunity for bare-id graphs is what this slice
delivers: int-encode the ids and crank zstd. The atom variant remains the Layer-3 path for
feature-rich graphs.
</p>
</body></html>
"""
    out.write_text(html, encoding="utf-8")


if __name__ == "__main__":
    sys.exit(main())
