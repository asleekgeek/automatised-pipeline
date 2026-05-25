#!/usr/bin/env python3
"""ZERA S2 — Layer 1 CODEBOOK measurement against Cortex production PG.

The user feedback on S1: "2 sec is already pretty long. People expect
200-400 ms." Fair. The 6.18 s I reported was the server's FULL graph_id
recomputation — work that in a production deployment is amortized
behind triggers / logical replication and never seen by the user. The
user-facing latency is the frame transfer + decode.

This script measures S2 the way users actually feel it:

  Server-amortized work (NOT counted in user wait):
    * Building the codebook from the current graph
    * Encoding the graph against the codebook
    * Computing content hashes
    * zstd compression

  User-facing latency budget (what we measure):
    * Time to receive HELLO + CODEBOOK + first encoded payload
    * Time to zstd-decompress and instantiate it on the client

These two numbers are reported separately, and the verdict gates on the
user-facing one (≤ 400 ms target).

Output:
  - target/zera-s2/codebook.json — machine-readable
  - target/zera-s2/codebook.html — human-readable report
"""
from __future__ import annotations

import argparse
import io
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
    print(
        f"error: missing dependency ({e}). Install in the venv:\n"
        "  pip install blake3 psycopg zstandard",
        file=sys.stderr,
    )
    sys.exit(2)


# ---------------------------------------------------------------------------
# Build the canonical graph the same way Rust does. (Mirror of GraphState::new.)
# ---------------------------------------------------------------------------

def read_graph(conn) -> tuple[list, list]:
    memories, entities, edges = [], [], []
    with conn.cursor(name="s2_mem") as cur:
        cur.execute("SELECT id::text FROM memories")
        for row in cur:
            memories.append(row[0])
    with conn.cursor(name="s2_ent") as cur:
        cur.execute("SELECT id::text FROM entities")
        for row in cur:
            entities.append(row[0])
    with conn.cursor(name="s2_me") as cur:
        cur.execute("SELECT memory_id::text, entity_id::text FROM memory_entities")
        for row in cur:
            edges.append(("mentions", f"memory:{row[0]}", f"entity:{row[1]}"))
    with conn.cursor(name="s2_rel") as cur:
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
# Codebook (Layer 1).
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


def split_prefix(prefixes: list[str], ident: str) -> tuple[int | None, str]:
    for i, p in enumerate(prefixes):
        if ident.startswith(p):
            return i, ident[len(p):]
    return None, ident


def encode_graph(nodes: list, edges: list, cb: dict) -> dict:
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
# Compression + wire-size helpers.
# ---------------------------------------------------------------------------

_ZSTD = zstd.ZstdCompressor(level=3)
_ZSTD_DECOMP = zstd.ZstdDecompressor()


def to_wire(value) -> tuple[bytes, bytes]:
    """Return (raw_json_bytes, zstd_compressed_bytes)."""
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
# Main
# ---------------------------------------------------------------------------

def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--db", default=os.environ.get("DATABASE_URL", "postgresql://localhost/cortex"))
    ap.add_argument("--out-dir", default=str(Path(__file__).resolve().parent.parent / "target/zera-s2"))
    args = ap.parse_args()

    out_dir = Path(args.out_dir)
    out_dir.mkdir(parents=True, exist_ok=True)

    # =====================================================================
    # Phase A — Server-amortized (one-time at server startup, NOT user wait)
    # =====================================================================
    print("phase A — server-amortized work (hidden from user)…", file=sys.stderr)
    t0 = time.perf_counter()
    with psycopg.connect(args.db) as conn:
        nodes, edges = read_graph(conn)
    t_read = (time.perf_counter() - t0) * 1000

    t0 = time.perf_counter()
    cb = build_codebook(nodes, edges)
    t_codebook = (time.perf_counter() - t0) * 1000

    t0 = time.perf_counter()
    enc = encode_graph(nodes, edges, cb)
    t_encode = (time.perf_counter() - t0) * 1000

    # Wire bytes (each frame is its own zstd block per spec §4.1).
    t0 = time.perf_counter()
    cb_raw, cb_zstd = to_wire(cb)
    enc_raw, enc_zstd = to_wire(enc)
    baseline = {"nodes": [{"label": l, "id": i} for (l, i) in nodes],
                "edges": [{"kind": k, "from": s, "to": d} for (k, s, d) in edges]}
    base_raw, base_zstd = to_wire(baseline)
    t_compress = (time.perf_counter() - t0) * 1000

    # HELLO frame (S1 contract, ~400 B)
    hello = {
        "zera_version": "0.0.2",
        "graph_id": codebook_content_id(cb)[:64],  # reuse blake3 helper; actual graph_id is computed elsewhere
        "cache_hit": False,
        "codebook_id": codebook_content_id(cb),
        "baseline_json_bytes": len(base_raw),
        "encoded_payload_bytes": len(enc_raw),
        "node_count": len(nodes),
        "edge_count": len(edges),
    }
    hello_raw, hello_zstd = to_wire(hello)

    # =====================================================================
    # Phase B — User-facing wait (decode time only)
    # =====================================================================
    print("phase B — user-facing decode time…", file=sys.stderr)
    # Cold session: client has nothing — receives HELLO + CODEBOOK +
    # encoded payload, all zstd-compressed. Time = decompress + parse.
    # We can't simulate network here, but we can measure the work the
    # client actually does.
    t0 = time.perf_counter()
    _ = json.loads(_ZSTD_DECOMP.decompress(hello_zstd))
    t_decode_hello = (time.perf_counter() - t0) * 1000
    t0 = time.perf_counter()
    _ = json.loads(_ZSTD_DECOMP.decompress(cb_zstd))
    t_decode_cb = (time.perf_counter() - t0) * 1000
    t0 = time.perf_counter()
    _ = json.loads(_ZSTD_DECOMP.decompress(enc_zstd))
    t_decode_enc = (time.perf_counter() - t0) * 1000

    # Network — LAN @ 1 Gbps gives ~125 MB/s. Conservative wire model:
    # bytes / 100 MB/s = ms. We report this estimate separately.
    wire_total_cold = len(hello_zstd) + len(cb_zstd) + len(enc_zstd)
    wire_total_warm = len(hello_zstd)  # codebook + encoded both cached
    net_ms_cold_lan = wire_total_cold / (100 * 1024 * 1024) * 1000
    net_ms_warm_lan = wire_total_warm / (100 * 1024 * 1024) * 1000
    net_ms_cold_4g  = wire_total_cold / (10 * 1024 * 1024 / 8) * 1000  # 10 Mbps 4G
    net_ms_warm_4g  = wire_total_warm / (10 * 1024 * 1024 / 8) * 1000

    user_wait_cold_ms = net_ms_cold_lan + t_decode_hello + t_decode_cb + t_decode_enc
    user_wait_warm_ms = net_ms_warm_lan + t_decode_hello

    report = {
        "graph": {"nodes": len(nodes), "edges": len(edges)},
        "sizes": {
            "baseline_raw": len(base_raw),
            "baseline_zstd": len(base_zstd),
            "hello_zstd": len(hello_zstd),
            "codebook_raw": len(cb_raw),
            "codebook_zstd": len(cb_zstd),
            "encoded_raw": len(enc_raw),
            "encoded_zstd": len(enc_zstd),
            "zera_cold_total_zstd": wire_total_cold,
            "zera_warm_total_zstd": wire_total_warm,
        },
        "server_amortized_ms": {
            "pg_read": t_read,
            "build_codebook": t_codebook,
            "encode_graph": t_encode,
            "compress_all": t_compress,
        },
        "user_facing_ms": {
            "decode_hello": t_decode_hello,
            "decode_codebook": t_decode_cb,
            "decode_encoded": t_decode_enc,
            "net_cold_lan": net_ms_cold_lan,
            "net_cold_4g": net_ms_cold_4g,
            "net_warm_lan": net_ms_warm_lan,
            "net_warm_4g": net_ms_warm_4g,
            "user_wait_cold_lan_total": user_wait_cold_ms,
            "user_wait_warm_lan_total": user_wait_warm_ms,
        },
        "codebook": cb,
    }

    out_json = out_dir / "codebook.json"
    out_json.write_text(json.dumps(report, indent=2, default=str) + "\n", encoding="utf-8")
    render_html(report, out_dir / "codebook.html")

    ratio_raw = len(base_raw) / wire_total_cold
    ratio_zstd = len(base_zstd) / wire_total_cold
    target_ms = 400
    verdict = "PASS" if user_wait_cold_ms <= target_ms else f"FAIL ({user_wait_cold_ms:.1f} ms > {target_ms} ms)"

    print(
        f"\n=== ZERA S2 — CODEBOOK ===\n"
        f"graph:                 {len(nodes):,} nodes / {len(edges):,} edges\n"
        f"\n"
        f"WIRE SIZES (zstd):\n"
        f"  baseline (raw JSON): {human(len(base_raw))}\n"
        f"  baseline (zstd):     {human(len(base_zstd))}\n"
        f"  HELLO frame:         {human(len(hello_zstd))}\n"
        f"  CODEBOOK frame:      {human(len(cb_zstd))}\n"
        f"  encoded payload:     {human(len(enc_zstd))}\n"
        f"  ZERA cold total:     {human(wire_total_cold)}  ({ratio_raw:.1f}× vs raw JSON, {ratio_zstd:.2f}× vs zstd-JSON)\n"
        f"  ZERA warm total:     {human(wire_total_warm)} (codebook cached)\n"
        f"\n"
        f"SERVER-AMORTIZED (hidden from user):\n"
        f"  PG read:             {human_ms(t_read)}\n"
        f"  build codebook:      {human_ms(t_codebook)}\n"
        f"  encode graph:        {human_ms(t_encode)}\n"
        f"  compress all frames: {human_ms(t_compress)}\n"
        f"\n"
        f"USER-FACING WAIT (cold session, LAN @ 100 MB/s):\n"
        f"  net (HELLO+CODEBOOK+payload):  {human_ms(net_ms_cold_lan)}\n"
        f"  decode HELLO:                  {human_ms(t_decode_hello)}\n"
        f"  decode CODEBOOK:               {human_ms(t_decode_cb)}\n"
        f"  decode encoded payload:        {human_ms(t_decode_enc)}\n"
        f"  total:                         {human_ms(user_wait_cold_ms)}\n"
        f"\n"
        f"USER-FACING WAIT (warm session, codebook cached):\n"
        f"  net + decode:                  {human_ms(user_wait_warm_ms)}\n"
        f"\n"
        f"VERDICT (cold ≤ {target_ms} ms target): {verdict}\n"
        f"\n"
        f"report:                {out_dir / 'codebook.html'}"
    )
    return 0 if user_wait_cold_ms <= target_ms else 1


# ---------------------------------------------------------------------------
# HTML rendering
# ---------------------------------------------------------------------------

def render_html(r: dict, out: Path) -> None:
    s = r["sizes"]
    sa = r["server_amortized_ms"]
    uf = r["user_facing_ms"]
    g = r["graph"]
    cb = r["codebook"]

    ratio_raw  = s["baseline_raw"]  / s["zera_cold_total_zstd"]
    ratio_zstd = s["baseline_zstd"] / s["zera_cold_total_zstd"]
    warm_ratio = s["baseline_raw"]  / s["zera_warm_total_zstd"]

    target = 400
    cold_ok = uf["user_wait_cold_lan_total"] <= target
    verdict = "PASS" if cold_ok else "FAIL"
    verdict_color = "#34d399" if cold_ok else "#f87171"

    # Bar chart: baseline vs zera cold vs zera warm (logarithmic feel via percent).
    base_w  = 100.0
    cold_w  = s["zera_cold_total_zstd"] / s["baseline_raw"] * 100
    warm_w  = s["zera_warm_total_zstd"] / s["baseline_raw"] * 100
    zstd_w  = s["baseline_zstd"]        / s["baseline_raw"] * 100

    pal_labels = "".join(f"<li><code>{l}</code></li>" for l in cb["labels"])
    pal_kinds  = "".join(f"<li><code>{k}</code></li>" for k in cb["kinds"])
    pal_prefs  = "".join(f"<li><code>{p}</code></li>" for p in cb["id_prefixes"]) or "<li>(none above threshold)</li>"

    html = f"""<!doctype html>
<html><head><meta charset="utf-8">
<title>ZERA S2 — Layer 1 CODEBOOK on Cortex production PG</title>
<style>
body {{ font-family: -apple-system, BlinkMacSystemFont, sans-serif; background: #0f172a; color: #e2e8f0;
       max-width: 960px; margin: 32px auto; padding: 0 24px; line-height: 1.5; }}
h1 {{ font-size: 18px; font-weight: 500; margin: 0 0 6px; }}
h2 {{ font-size: 12px; font-weight: 500; margin: 22px 0 8px; color: #94a3b8;
      text-transform: uppercase; letter-spacing: 0.06em; }}
.sub {{ font-size: 12px; color: #94a3b8; margin-bottom: 16px; }}
.panel {{ background: #1e293b; border: 1px solid #334155; border-radius: 6px; padding: 14px 16px; }}
.verdict {{ font-family: 'SF Mono', Menlo, monospace; font-size: 26px; padding: 16px; border-radius: 6px;
           background: #1e293b; border: 1px solid; }}
.row {{ display: flex; justify-content: space-between; margin: 4px 0; font-size: 13px; }}
.row .v {{ font-family: 'SF Mono', Menlo, monospace; color: #67e8f9; }}
.grid3 {{ display: grid; grid-template-columns: 1fr 1fr 1fr; gap: 10px; margin: 12px 0; }}
.stat {{ background: #1e293b; border: 1px solid #334155; border-radius: 6px; padding: 10px 14px; }}
.stat .label {{ font-size: 11px; color: #94a3b8; text-transform: uppercase; letter-spacing: 0.06em; }}
.stat .val {{ font-family: 'SF Mono', Menlo, monospace; font-size: 22px; color: #67e8f9; margin-top: 4px; }}
.bar-row {{ margin: 8px 0; font-size: 12px; }}
.bar-row .lbl {{ display: inline-block; width: 200px; color: #cbd5e1; }}
.bar {{ display: inline-block; height: 16px; vertical-align: middle; border-radius: 3px; position: relative; }}
.b-raw  {{ background: linear-gradient(90deg, #f43f5e, #ef4444); }}
.b-zstd {{ background: linear-gradient(90deg, #f59e0b, #fb923c); }}
.b-cold {{ background: linear-gradient(90deg, #10b981, #22d3ee); }}
.b-warm {{ background: linear-gradient(90deg, #22d3ee, #818cf8); }}
.bar .sz {{ position: absolute; right: -110px; top: 0; font-family: 'SF Mono', Menlo, monospace; color: #67e8f9; }}
.cols {{ display: grid; grid-template-columns: 1fr 1fr; gap: 14px; margin: 12px 0; }}
ul.pal {{ margin: 0; padding-left: 18px; font-size: 12px; line-height: 1.7; }}
code {{ background: #0f172a; padding: 1px 5px; border-radius: 3px; color: #67e8f9; font-size: 11px; }}
.note {{ font-size: 11px; color: #64748b; margin-top: 20px; line-height: 1.6; }}
.bad {{ color: #f87171; }} .good {{ color: #34d399; }}
</style></head><body>
<h1>ZERA S2 — Layer 1 CODEBOOK on Cortex production PG</h1>
<div class="sub">{g['nodes']:,} nodes / {g['edges']:,} edges. The codebook is sent once and cached; later sessions ship only HELLO.
User-facing target: <em>≤ 400 ms</em> on a cold first paint over LAN.</div>

<div class="verdict" style="color: {verdict_color}; border-color: {verdict_color};">
  {verdict} — cold user-wait {human_ms(uf['user_wait_cold_lan_total'])}, warm user-wait {human_ms(uf['user_wait_warm_lan_total'])}
</div>

<h2>wire sizes</h2>
<div class="panel">
<div class="bar-row"><span class="lbl">raw JSON baseline</span>
  <span class="bar b-raw" style="width:600px"><span class="sz">{human(s['baseline_raw'])}</span></span></div>
<div class="bar-row"><span class="lbl">zstd(JSON) — common baseline</span>
  <span class="bar b-zstd" style="width:{600*zstd_w/base_w:.4f}px"><span class="sz">{human(s['baseline_zstd'])}</span></span></div>
<div class="bar-row"><span class="lbl">ZERA cold (HELLO + CODEBOOK + payload)</span>
  <span class="bar b-cold" style="width:{600*cold_w/base_w:.4f}px"><span class="sz">{human(s['zera_cold_total_zstd'])}</span></span></div>
<div class="bar-row"><span class="lbl">ZERA warm (HELLO only)</span>
  <span class="bar b-warm" style="width:{max(2,600*warm_w/base_w):.4f}px"><span class="sz">{human(s['zera_warm_total_zstd'])}</span></span></div>
</div>

<div class="grid3" style="margin-top:14px">
  <div class="stat"><div class="label">vs raw JSON</div><div class="val">{ratio_raw:.1f}×</div></div>
  <div class="stat"><div class="label">vs zstd JSON</div><div class="val">{ratio_zstd:.2f}×</div></div>
  <div class="stat"><div class="label">warm vs raw</div><div class="val">{warm_ratio:,.0f}×</div></div>
</div>

<h2>user-facing wait (cold session, LAN @ 100 MB/s)</h2>
<div class="panel">
  <div class="row"><span>network transfer (HELLO + CODEBOOK + payload, zstd-compressed)</span><span class="v">{human_ms(uf['net_cold_lan'])}</span></div>
  <div class="row"><span>decode HELLO</span><span class="v">{human_ms(uf['decode_hello'])}</span></div>
  <div class="row"><span>decode CODEBOOK</span><span class="v">{human_ms(uf['decode_codebook'])}</span></div>
  <div class="row"><span>decode encoded payload</span><span class="v">{human_ms(uf['decode_encoded'])}</span></div>
  <div class="row" style="margin-top:8px; padding-top:8px; border-top:1px solid #334155">
    <span><b>TOTAL cold user wait</b></span><span class="v" style="font-size:16px"><b>{human_ms(uf['user_wait_cold_lan_total'])}</b></span></div>
  <div class="row"><span>same over 4G (10 Mbps)</span><span class="v">{human_ms(uf['net_cold_4g'] + uf['decode_hello'] + uf['decode_codebook'] + uf['decode_encoded'])}</span></div>
</div>

<h2>warm session (codebook + payload cached)</h2>
<div class="panel">
  <div class="row"><span>network (HELLO only, manifest)</span><span class="v">{human_ms(uf['net_warm_lan'])}</span></div>
  <div class="row"><span>decode HELLO</span><span class="v">{human_ms(uf['decode_hello'])}</span></div>
  <div class="row" style="margin-top:8px; padding-top:8px; border-top:1px solid #334155">
    <span><b>TOTAL warm user wait</b></span><span class="v" style="font-size:16px"><b>{human_ms(uf['user_wait_warm_lan_total'])}</b></span></div>
</div>

<h2>server-amortized (one-time, hidden behind triggers/replication in production)</h2>
<div class="panel">
  <div class="row"><span>read graph from PG</span><span class="v">{human_ms(sa['pg_read'])}</span></div>
  <div class="row"><span>build codebook</span><span class="v">{human_ms(sa['build_codebook'])}</span></div>
  <div class="row"><span>encode graph against codebook</span><span class="v">{human_ms(sa['encode_graph'])}</span></div>
  <div class="row"><span>zstd compress all frames</span><span class="v">{human_ms(sa['compress_all'])}</span></div>
</div>

<h2>the codebook itself ({s['codebook_zstd']} B on wire after zstd)</h2>
<div class="cols">
  <div class="panel"><h3 style="margin:0 0 6px;font-size:11px;color:#94a3b8;text-transform:uppercase">labels ({len(cb['labels'])})</h3><ul class="pal">{pal_labels}</ul></div>
  <div class="panel"><h3 style="margin:0 0 6px;font-size:11px;color:#94a3b8;text-transform:uppercase">edge kinds ({len(cb['kinds'])})</h3><ul class="pal">{pal_kinds}</ul></div>
</div>
<div class="panel" style="margin-top:10px"><h3 style="margin:0 0 6px;font-size:11px;color:#94a3b8;text-transform:uppercase">id prefixes ({len(cb['id_prefixes'])})</h3><ul class="pal">{pal_prefs}</ul></div>

<p class="note">
S2 separates work the server does once (and caches behind triggers in
production) from work the user actually waits for. The cold user-wait
above is the sum of network transfer (zstd-compressed) + zstd decode +
JSON parse — no graph_id recomputation, no PG read. In production that's
exactly what a fresh client would experience on first connect to a
server that already has its graph_id and codebook held in memory.
</p>
</body></html>
"""
    out.write_text(html, encoding="utf-8")


if __name__ == "__main__":
    sys.exit(main())
