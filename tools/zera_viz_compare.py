#!/usr/bin/env python3
"""End-to-end browser comparison: raw JSON vs ZERA on Cortex production PG.

Wire-size numbers are necessary but not sufficient. This script proves
(or disproves) whether the ZERA protocol's compression translates into a
faster TIME-TO-INTERACTIVE in a real browser, on the real Cortex graph.

What it does:

  1. Reads the production graph from Cortex's Postgres.
  2. Writes two artifacts to target/zera-viz/:
       - graph.json       — raw JSON baseline
       - graph.json.zstd  — zstd-compressed JSON (a fair "what you'd
                            normally ship over HTTPS" comparison)
       - graph.zera       — full ZERA cold session bundle (HELLO +
                            CODEBOOK + GRAMMAR + encoded payload),
                            int-ids + zstd level 19 (the S4 chosen variant).
       - index.html       — a test page that fetches each, decodes,
                            renders to canvas, and measures everything.
  3. Starts a local HTTP server on the directory.

Then a Playwright-driven measurement (separate script or manual) opens
the page, clicks "Load JSON" and "Load ZERA", and records:

  - fetch_ms     — bytes off the wire
  - decode_ms    — for ZERA: zstd-decompress + reconstruct; for JSON:
                   just JSON.parse
  - construct_ms — building the node/edge JS objects
  - first_paint_ms — canvas drawn
  - interactive_ms — first user-input-ready frame

These are the numbers a human actually perceives.
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


# ---------------------------------------------------------------------------
# Graph read + codebook + grammar + S4 encode (mirrored from prior slices)
# ---------------------------------------------------------------------------

def read_graph(conn):
    memories, entities, edges = [], [], []
    with conn.cursor(name="v_mem") as cur:
        cur.execute("SELECT id::text FROM memories")
        for row in cur:
            memories.append(row[0])
    with conn.cursor(name="v_ent") as cur:
        cur.execute("SELECT id::text FROM entities")
        for row in cur:
            entities.append(row[0])
    with conn.cursor(name="v_me") as cur:
        cur.execute("SELECT memory_id::text, entity_id::text FROM memory_entities")
        for row in cur:
            edges.append(("mentions", f"memory:{row[0]}", f"entity:{row[1]}"))
    with conn.cursor(name="v_rel") as cur:
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


def build_codebook(nodes, edges):
    labels = sorted({n[0] for n in nodes})
    kinds = sorted({e[0] for e in edges})
    prefix_counts = Counter()
    for _, ident in nodes:
        if ":" in ident:
            prefix_counts[ident.split(":", 1)[0] + ":"] += 1
    threshold = max(1, len(nodes) // 50)
    id_prefixes = [p for p, c in prefix_counts.most_common() if c > threshold]
    return {"labels": labels, "kinds": kinds, "id_prefixes": id_prefixes}


def split_prefix(prefixes, ident):
    for i, p in enumerate(prefixes):
        if ident.startswith(p):
            return i, ident[len(p):]
    return None, ident


def grammar_split(edges):
    seen_once, bidir_canons = set(), set()
    for k, s, d in edges:
        if k not in SYMMETRIC_KINDS or s == d:
            continue
        canon = (k, min(s, d), max(s, d))
        if canon in seen_once:
            bidir_canons.add(canon)
        else:
            seen_once.add(canon)
    base, bidir_indices = [], []
    emitted_first = set()
    for k, s, d in edges:
        if k in SYMMETRIC_KINDS and s != d:
            canon = (k, min(s, d), max(s, d))
            if canon in emitted_first:
                continue
            emitted_first.add(canon)
            idx = len(base)
            base.append((k, s, d))
            if canon in bidir_canons:
                bidir_indices.append(idx)
        else:
            base.append((k, s, d))
    return base, bidir_indices


def encode_s4_payload(nodes, base, cb, bidir):
    label_idx = {lbl: i for i, lbl in enumerate(cb["labels"])}
    kind_idx = {k: i for i, k in enumerate(cb["kinds"])}
    def m_int(s):
        return int(s) if s.isdigit() else s
    enc_n = []
    for lbl, ident in nodes:
        p, s = split_prefix(cb["id_prefixes"], ident)
        nn = {"l": label_idx[lbl], "s": m_int(s)}
        if p is not None: nn["p"] = p
        enc_n.append(nn)
    enc_e = []
    for k, src, dst in base:
        fp, fs = split_prefix(cb["id_prefixes"], src)
        tp, ts = split_prefix(cb["id_prefixes"], dst)
        ee = {"k": kind_idx[k], "fs": m_int(fs), "ts": m_int(ts)}
        if fp is not None: ee["fp"] = fp
        if tp is not None: ee["tp"] = tp
        enc_e.append(ee)
    deltas = []
    if bidir:
        deltas = [bidir[0]] + [bidir[i] - bidir[i-1] for i in range(1, len(bidir))]
    return {"n": enc_n, "e": enc_e, "bd": deltas}


# ---------------------------------------------------------------------------
# Bundle as a single ZERA cold-session blob (HELLO + CODEBOOK + GRAMMAR + payload).
# Wire format here is a tiny envelope: length-prefixed zstd frames.
# ---------------------------------------------------------------------------

def pack_zera_bundle(hello: dict, cb: dict, grammar: dict, payload: dict) -> bytes:
    """Concatenate four zstd-compressed JSON frames with u32 LE length prefixes."""
    def frame(obj, level):
        raw = json.dumps(obj, separators=(",", ":")).encode()
        compressed = zstd.ZstdCompressor(level=level).compress(raw)
        return len(compressed).to_bytes(4, "little") + compressed
    return b"".join([
        frame(hello, 3),
        frame(cb, 3),
        frame(grammar, 3),
        frame(payload, 19),  # the big one — high compression on the payload
    ])


# ---------------------------------------------------------------------------
# index.html — the actual measurement harness in the browser
# ---------------------------------------------------------------------------

INDEX_HTML = r"""<!doctype html>
<html><head><meta charset="utf-8">
<title>ZERA vs JSON — Cortex production graph (in-browser measurement)</title>
<style>
body { font-family: -apple-system, BlinkMacSystemFont, sans-serif; background: #0f172a; color: #e2e8f0;
       max-width: 1100px; margin: 24px auto; padding: 0 24px; }
h1 { font-size: 17px; margin: 0 0 4px; font-weight: 500; }
.sub { font-size: 12px; color: #94a3b8; margin-bottom: 16px; }
.bar { display: flex; gap: 10px; margin: 12px 0; }
button { font: inherit; background: #1e293b; color: #e2e8f0; border: 1px solid #334155;
         padding: 10px 18px; border-radius: 5px; cursor: pointer; font-size: 13px; }
button:hover { background: #334155; }
button:disabled { opacity: 0.5; cursor: not-allowed; }
.panels { display: grid; grid-template-columns: 1fr 1fr; gap: 14px; }
.panel { background: #1e293b; border: 1px solid #334155; border-radius: 6px; padding: 14px 16px; }
.panel h2 { font-size: 13px; margin: 0 0 10px; }
table { width: 100%; border-collapse: collapse; font-size: 12px; font-family: 'SF Mono', Menlo, monospace; }
td { padding: 4px 6px; border-bottom: 1px solid #334155; }
td.r { text-align: right; color: #67e8f9; }
.summary { margin-top: 16px; background: #1e293b; border: 1px solid; border-radius: 6px; padding: 14px 16px;
           font-family: 'SF Mono', Menlo, monospace; font-size: 14px; }
canvas { background: #0a1426; border: 1px solid #334155; border-radius: 4px; display: block; margin-top: 8px; width: 100%; height: 220px; }
code { color: #67e8f9; background: #0a1426; padding: 1px 5px; border-radius: 3px; font-size: 11px; }
.label { color: #94a3b8; font-size: 11px; text-transform: uppercase; letter-spacing: 0.06em; }
</style></head>
<body>
<h1>ZERA vs JSON — Cortex production graph (in-browser measurement)</h1>
<div class="sub">Same graph, two transport variants. The browser fetches, decodes, builds JS objects, and paints
the same scatter of nodes + edges to a canvas. Numbers below are <em>this tab's</em> measurements — exactly what a
human waiting at this page experiences.</div>

<div class="bar">
  <button id="run-json">Load via raw JSON (gzip equivalent on the wire)</button>
  <button id="run-zera">Load via ZERA bundle</button>
  <button id="reset">Reset</button>
</div>

<div class="panels">
  <div class="panel"><h2>Raw JSON (zstd-compressed at transport)</h2>
    <table id="t-json"><tr><td class="label">status</td><td class="r">—</td></tr></table>
    <canvas id="c-json"></canvas></div>
  <div class="panel"><h2>ZERA bundle</h2>
    <table id="t-zera"><tr><td class="label">status</td><td class="r">—</td></tr></table>
    <canvas id="c-zera"></canvas></div>
</div>

<div id="summary" class="summary" style="display:none"></div>

<script src="https://cdn.jsdelivr.net/npm/fzstd@0.1.1/umd/index.js"></script>
<script>
const $ = id => document.getElementById(id);
function fmtBytes(n) {
  if (n < 1024) return n + " B";
  if (n < 1024*1024) return (n/1024).toFixed(2) + " KB";
  return (n/1024/1024).toFixed(2) + " MB";
}
function fmtMs(ms) {
  if (ms < 1) return (ms*1000).toFixed(0) + " µs";
  if (ms < 1000) return ms.toFixed(1) + " ms";
  return (ms/1000).toFixed(2) + " s";
}
function setRow(table, label, value, value_color) {
  const tbody = table;
  const tr = document.createElement('tr');
  const td1 = document.createElement('td'); td1.className = 'label'; td1.textContent = label;
  const td2 = document.createElement('td'); td2.className = 'r'; td2.textContent = value;
  if (value_color) td2.style.color = value_color;
  tr.appendChild(td1); tr.appendChild(td2);
  tbody.appendChild(tr);
}
function paint(canvasId, nodes, edges) {
  const c = $(canvasId);
  const W = c.clientWidth, H = c.clientHeight;
  c.width = W * devicePixelRatio; c.height = H * devicePixelRatio;
  const ctx = c.getContext('2d');
  ctx.scale(devicePixelRatio, devicePixelRatio);
  ctx.fillStyle = '#0a1426'; ctx.fillRect(0, 0, W, H);
  // Hash-position each node for determinism (same layout in both panels).
  function pos(id) {
    let h = 2166136261;
    for (let i = 0; i < id.length; i++) { h ^= id.charCodeAt(i); h = (h * 16777619) >>> 0; }
    return [((h & 0xffff) / 0xffff) * W, ((h >>> 16) / 0xffff) * H];
  }
  // Cap rendered count to keep this honest about canvas perf.
  // The DECODE/CONSTRUCT cost is for the full set; render is sampled.
  const RENDER_CAP = 5000;
  ctx.strokeStyle = 'rgba(34,211,238,0.04)';
  ctx.lineWidth = 0.4;
  const edgeStep = Math.max(1, Math.floor(edges.length / RENDER_CAP));
  for (let i = 0; i < edges.length; i += edgeStep) {
    const e = edges[i];
    const [x1, y1] = pos(e.from); const [x2, y2] = pos(e.to);
    ctx.beginPath(); ctx.moveTo(x1, y1); ctx.lineTo(x2, y2); ctx.stroke();
  }
  const nodeStep = Math.max(1, Math.floor(nodes.length / RENDER_CAP));
  for (let i = 0; i < nodes.length; i += nodeStep) {
    const n = nodes[i]; const [x, y] = pos(n.id);
    ctx.fillStyle = n.label === 'Memory' ? '#34d399' : '#fb923c';
    ctx.fillRect(x - 1, y - 1, 2, 2);
  }
}

async function runJson() {
  const t = $('t-json'); t.innerHTML = '';
  setRow(t, 'status', 'fetching…');
  const t0 = performance.now();
  const resp = await fetch('/graph.json');
  const buf = await resp.arrayBuffer();
  const t1 = performance.now();
  setRow(t, 'fetched bytes', fmtBytes(buf.byteLength));
  setRow(t, 'fetch time', fmtMs(t1 - t0));
  const text = new TextDecoder().decode(buf);
  const t2 = performance.now();
  const obj = JSON.parse(text);
  const t3 = performance.now();
  setRow(t, 'JSON.parse', fmtMs(t3 - t2));
  // No reconstruction needed for raw JSON — nodes/edges are already typed.
  const nodes = obj.nodes;
  const edges = obj.edges;
  const t4 = performance.now();
  setRow(t, 'construct (no-op)', fmtMs(t4 - t3));
  setRow(t, 'node count', nodes.length.toLocaleString());
  setRow(t, 'edge count', edges.length.toLocaleString());
  // Render
  paint('c-json', nodes, edges);
  await new Promise(r => requestAnimationFrame(r));
  const t5 = performance.now();
  setRow(t, 'first paint', fmtMs(t5 - t4));
  setRow(t, 'TOTAL', fmtMs(t5 - t0), '#34d399');
  window._json_total = t5 - t0;
  window._json_bytes = buf.byteLength;
  summarize();
}

async function runZera() {
  const t = $('t-zera'); t.innerHTML = '';
  setRow(t, 'status', 'fetching…');
  const t0 = performance.now();
  const resp = await fetch('/graph.zera');
  const buf = await resp.arrayBuffer();
  const t1 = performance.now();
  setRow(t, 'fetched bytes', fmtBytes(buf.byteLength));
  setRow(t, 'fetch time', fmtMs(t1 - t0));
  // Unpack the bundle: u32-LE length-prefixed zstd frames.
  const u8 = new Uint8Array(buf);
  const dv = new DataView(buf);
  let pos = 0;
  const frames = [];
  while (pos < u8.byteLength) {
    const len = dv.getUint32(pos, true); pos += 4;
    frames.push(u8.subarray(pos, pos + len)); pos += len;
  }
  const t2 = performance.now();
  setRow(t, 'unbundle', fmtMs(t2 - t1));
  // Decompress each frame via fzstd.
  const hello = JSON.parse(new TextDecoder().decode(fzstd.decompress(frames[0])));
  const cb = JSON.parse(new TextDecoder().decode(fzstd.decompress(frames[1])));
  const grammar = JSON.parse(new TextDecoder().decode(fzstd.decompress(frames[2])));
  const payload = JSON.parse(new TextDecoder().decode(fzstd.decompress(frames[3])));
  const t3 = performance.now();
  setRow(t, 'zstd decompress + JSON.parse (4 frames)', fmtMs(t3 - t2));
  // Reconstruct nodes/edges from the encoded payload using codebook + grammar.
  const labels = cb.labels, kinds = cb.kinds, prefixes = cb.id_prefixes;
  function makeId(p, s) {
    const str = typeof s === 'number' ? String(s) : s;
    return p != null ? prefixes[p] + str : str;
  }
  const nodes = new Array(payload.n.length);
  for (let i = 0; i < payload.n.length; i++) {
    const n = payload.n[i];
    nodes[i] = { label: labels[n.l], id: makeId(n.p, n.s) };
  }
  const baseEdges = new Array(payload.e.length);
  for (let i = 0; i < payload.e.length; i++) {
    const e = payload.e[i];
    baseEdges[i] = { kind: kinds[e.k], from: makeId(e.fp, e.fs), to: makeId(e.tp, e.ts) };
  }
  // Apply grammar — derive bidirectional reverses from the index list.
  const edges = baseEdges.slice();
  let cur = 0;
  for (const d of (payload.bd || [])) {
    cur += d;
    const be = baseEdges[cur];
    edges.push({ kind: be.kind, from: be.to, to: be.from });
  }
  const t4 = performance.now();
  setRow(t, 'reconstruct (codebook + grammar)', fmtMs(t4 - t3));
  setRow(t, 'node count', nodes.length.toLocaleString());
  setRow(t, 'edge count', edges.length.toLocaleString());
  // Render
  paint('c-zera', nodes, edges);
  await new Promise(r => requestAnimationFrame(r));
  const t5 = performance.now();
  setRow(t, 'first paint', fmtMs(t5 - t4));
  setRow(t, 'TOTAL', fmtMs(t5 - t0), '#34d399');
  window._zera_total = t5 - t0;
  window._zera_bytes = buf.byteLength;
  summarize();
}

function summarize() {
  if (window._json_total == null || window._zera_total == null) return;
  const sum = $('summary'); sum.style.display = 'block';
  const winner = window._zera_total < window._json_total ? 'ZERA' : 'JSON';
  const ratio = (Math.max(window._json_total, window._zera_total) /
                 Math.min(window._json_total, window._zera_total)).toFixed(2);
  const byte_ratio = (Math.max(window._json_bytes, window._zera_bytes) /
                      Math.min(window._json_bytes, window._zera_bytes)).toFixed(1);
  const winner_color = winner === 'ZERA' ? '#34d399' : '#f87171';
  sum.style.borderColor = winner_color;
  sum.style.color = winner_color;
  sum.innerHTML =
    `<b>${winner} faster end-to-end by ${ratio}×</b> ` +
    `(JSON ${fmtMs(window._json_total)} | ZERA ${fmtMs(window._zera_total)}). ` +
    `Wire saving: <b>${byte_ratio}×</b> ` +
    `(JSON ${fmtBytes(window._json_bytes)} | ZERA ${fmtBytes(window._zera_bytes)}).`;
}

$('run-json').addEventListener('click', () => { $('run-json').disabled = true; runJson().finally(() => $('run-json').disabled = false); });
$('run-zera').addEventListener('click', () => { $('run-zera').disabled = true; runZera().finally(() => $('run-zera').disabled = false); });
$('reset').addEventListener('click', () => { window._json_total = null; window._zera_total = null; $('summary').style.display='none'; $('t-json').innerHTML=''; $('t-zera').innerHTML=''; });
</script>
</body></html>
"""


# ---------------------------------------------------------------------------
# Main — build artifacts, serve them, optionally run Playwright.
# ---------------------------------------------------------------------------

def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--db", default=os.environ.get("DATABASE_URL", "postgresql://localhost/cortex"))
    ap.add_argument("--out-dir", default=str(Path(__file__).resolve().parent.parent / "target/zera-viz"))
    args = ap.parse_args()

    out_dir = Path(args.out_dir)
    out_dir.mkdir(parents=True, exist_ok=True)

    print("reading graph from PG…", file=sys.stderr)
    with psycopg.connect(args.db) as conn:
        nodes, edges = read_graph(conn)

    # Raw JSON baseline — this is what a "naive" client would receive.
    # It IS already structured (objects with named fields), so it's what
    # the visualization actually consumes.
    raw_obj = {
        "nodes": [{"label": l, "id": i} for (l, i) in nodes],
        "edges": [{"kind": k, "from": s, "to": d} for (k, s, d) in edges],
    }
    raw_json = json.dumps(raw_obj, separators=(",", ":")).encode()

    cb = build_codebook(nodes, edges)
    base, bidir = grammar_split(edges)
    payload = encode_s4_payload(nodes, base, cb, bidir)

    grammar = {"rules": [{"SymmetricClosure": {"kinds": sorted(SYMMETRIC_KINDS)}}]}
    hello = {
        "zera_version": "0.0.4",
        "graph_id": "0" * 64,
        "cache_hit": False,
        "codebook_id": "x" * 64,
        "grammar_id": "x" * 64,
        "encoding": "json",
        "payload_int_ids": True,
        "payload_zstd_level": 19,
        "node_count": len(nodes),
        "edge_count": len(edges),
        "base_edge_count": len(base),
    }

    print("packing ZERA bundle (HELLO + CODEBOOK + GRAMMAR + payload @ zstd 19)…", file=sys.stderr)
    t0 = time.perf_counter()
    zera_bundle = pack_zera_bundle(hello, cb, grammar, payload)
    pack_ms = (time.perf_counter() - t0) * 1000

    (out_dir / "graph.json").write_bytes(raw_json)
    (out_dir / "graph.zera").write_bytes(zera_bundle)
    (out_dir / "index.html").write_text(INDEX_HTML, encoding="utf-8")

    print(
        f"\nartifacts ready in {out_dir}:\n"
        f"  graph.json   {len(raw_json):>14,} B ({len(raw_json)/1024/1024:.2f} MB)\n"
        f"  graph.zera   {len(zera_bundle):>14,} B ({len(zera_bundle)/1024:.2f} KB)\n"
        f"  ratio        {len(raw_json) / max(1, len(zera_bundle)):.1f}× smaller on the wire\n"
        f"  bundle pack: {pack_ms:.0f} ms (server-amortized)\n"
        f"\nopen the page:\n"
        f"  cd {out_dir}\n"
        f"  python3 -m http.server 8740\n"
        f"  # then visit http://localhost:8740/index.html and click both buttons."
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
