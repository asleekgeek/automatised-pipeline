//! ZERA Layer 1 — CODEBOOK.
//!
//! Spec: `docs/protocols/ZERA-spec.md` §4.2 Layer 1.
//!
//! The codebook is a small, immutable artifact shared between server and
//! client. Once a client has cached it (by content hash), it is never sent
//! again — every subsequent layer ships the *indices* into the codebook
//! instead of the original strings, plus the residual ids that the
//! codebook cannot enumerate.
//!
//! Three palettes for a Cortex-shaped graph:
//!   * label_palette — the small finite set of node labels (e.g. Memory,
//!     Entity). Encoded as u8 indices.
//!   * kind_palette — the small finite set of edge kinds
//!     (mentions, calls, …). Encoded as u8 indices.
//!   * id_prefix_palette — common id prefixes that appear on most nodes
//!     (e.g. "memory:", "entity:"); when an id starts with one, the
//!     prefix is replaced by an index. Saves ~7 bytes per node at the
//!     Cortex scale.
//!
//! Later layers (SPARSE, EIGEN, SEED) will extend this with sparse
//! dictionary atoms and HDC role names, but the principle is the same:
//! the codebook is the static vocabulary; ship it once.

use serde::{Deserialize, Serialize};

use crate::GraphState;

/// Codebook: vocabularies that the encoder builds once from a graph and
/// the decoder caches forever (until its content hash changes).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Codebook {
    pub labels: Vec<String>,
    pub kinds: Vec<String>,
    pub id_prefixes: Vec<String>,
}

impl Codebook {
    /// Build a codebook by scanning the graph for the closed sets of
    /// labels and kinds, plus extracting common id prefixes.
    pub fn from_graph(state: &GraphState) -> Self {
        // Labels — first observation order, deduplicated.
        let mut labels = Vec::<String>::new();
        for n in &state.nodes {
            if !labels.iter().any(|l| l == &n.label) {
                labels.push(n.label.clone());
            }
        }
        labels.sort();

        // Kinds — same approach.
        let mut kinds = Vec::<String>::new();
        for e in &state.edges {
            if !kinds.iter().any(|k| k == &e.kind) {
                kinds.push(e.kind.clone());
            }
        }
        kinds.sort();

        // ID prefixes — any prefix up to the first ':' that occurs on
        // ≥ 50 % of nodes earns a slot. This is the simplest defensible
        // threshold; later slices can replace this with sparse-dict
        // atoms over id-fragment bigrams.
        let id_prefixes = extract_common_prefixes(&state.nodes);

        Codebook {
            labels,
            kinds,
            id_prefixes,
        }
    }

    /// 32-byte BLAKE3 of the canonical serialization. The content-id
    /// the client offers in HELLO.
    pub fn content_hash(&self) -> [u8; 32] {
        let mut h = blake3::Hasher::new();
        h.update(&(self.labels.len() as u64).to_le_bytes());
        for s in &self.labels {
            h.update(&(s.len() as u64).to_le_bytes());
            h.update(s.as_bytes());
        }
        h.update(&(self.kinds.len() as u64).to_le_bytes());
        for s in &self.kinds {
            h.update(&(s.len() as u64).to_le_bytes());
            h.update(s.as_bytes());
        }
        h.update(&(self.id_prefixes.len() as u64).to_le_bytes());
        for s in &self.id_prefixes {
            h.update(&(s.len() as u64).to_le_bytes());
            h.update(s.as_bytes());
        }
        *h.finalize().as_bytes()
    }

    pub fn content_id(&self) -> String {
        let bytes = self.content_hash();
        let mut s = String::with_capacity(64);
        for &b in &bytes {
            s.push_str(&format!("{b:02x}"));
        }
        s
    }

    /// Wire size of the codebook as compact JSON. Tiny and constant w.r.t.
    /// graph size — that's the whole point.
    pub fn wire_size(&self) -> usize {
        serde_json::to_vec(self).map(|v| v.len()).unwrap_or(0)
    }

    /// zstd-compressed wire size at the spec's default level (3).
    pub fn wire_compressed_size(&self) -> usize {
        let raw = serde_json::to_vec(self).unwrap_or_default();
        zstd::bulk::compress(&raw, 3).map(|v| v.len()).unwrap_or(0)
    }
}

/// A node encoded against a codebook. `label` is an index into
/// `codebook.labels`; `id_prefix` is `None` (no codebook prefix matched)
/// or `Some(idx)` into `codebook.id_prefixes` with `id_suffix` carrying
/// what follows the prefix.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EncNode {
    pub l: u32,
    pub p: Option<u32>,
    pub s: String,
}

/// An edge encoded against a codebook. The label of the source/target
/// node is recovered from the node table at decode time, so we only
/// transmit the id_prefix index + suffix per endpoint.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EncEdge {
    pub k: u32,
    pub fp: Option<u32>,
    pub fs: String,
    pub tp: Option<u32>,
    pub ts: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EncodedGraph {
    pub codebook_id: String,
    pub nodes: Vec<EncNode>,
    pub edges: Vec<EncEdge>,
}

impl EncodedGraph {
    /// Encode a graph against a codebook. Lookups are linear over small
    /// palettes (<100 entries) — fine.
    pub fn encode(state: &GraphState, codebook: &Codebook) -> Self {
        let nodes = state
            .nodes
            .iter()
            .map(|n| {
                let l = idx_of(&codebook.labels, &n.label).expect("label in codebook") as u32;
                let (p, s) = split_prefix(&codebook.id_prefixes, &n.id);
                EncNode { l, p, s }
            })
            .collect();

        let edges = state
            .edges
            .iter()
            .map(|e| {
                let k = idx_of(&codebook.kinds, &e.kind).expect("kind in codebook") as u32;
                let (fp, fs) = split_prefix(&codebook.id_prefixes, &e.from);
                let (tp, ts) = split_prefix(&codebook.id_prefixes, &e.to);
                EncEdge { k, fp, fs, tp, ts }
            })
            .collect();

        EncodedGraph {
            codebook_id: codebook.content_id(),
            nodes,
            edges,
        }
    }

    /// Compact-JSON wire size for the encoded graph (excluding the
    /// codebook itself, which is shipped once).
    pub fn wire_size(&self) -> usize {
        serde_json::to_vec(self).map(|v| v.len()).unwrap_or(0)
    }

    /// Wire size *after zstd compression at the spec's default level
    /// (3 — fast enough to stream in real time, ~5× on text)*. The
    /// spec mandates zstd on every payload (§4.1), so this is the
    /// number that goes on the wire.
    pub fn wire_compressed_size(&self) -> usize {
        let raw = serde_json::to_vec(self).unwrap_or_default();
        zstd::bulk::compress(&raw, 3).map(|v| v.len()).unwrap_or(0)
    }
}

/// Helper: zstd-compressed size of any serializable value at level 3.
pub fn compressed_size<T: Serialize>(value: &T) -> usize {
    let raw = serde_json::to_vec(value).unwrap_or_default();
    zstd::bulk::compress(&raw, 3).map(|v| v.len()).unwrap_or(0)
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn idx_of(palette: &[String], needle: &str) -> Option<usize> {
    palette.iter().position(|s| s == needle)
}

fn split_prefix(prefixes: &[String], id: &str) -> (Option<u32>, String) {
    for (i, p) in prefixes.iter().enumerate() {
        if let Some(stripped) = id.strip_prefix(p.as_str()) {
            return (Some(i as u32), stripped.to_string());
        }
    }
    (None, id.to_string())
}

fn extract_common_prefixes(nodes: &[crate::NodeRef]) -> Vec<String> {
    use std::collections::HashMap;
    let mut counts: HashMap<String, usize> = HashMap::new();
    for n in nodes {
        if let Some(colon) = n.id.find(':') {
            let prefix = n.id[..=colon].to_string();
            *counts.entry(prefix).or_default() += 1;
        }
    }
    let threshold = nodes.len().max(1) / 50; // 2 % of nodes; small bar
    let mut prefixes: Vec<(String, usize)> = counts
        .into_iter()
        .filter(|(_, c)| *c > threshold)
        .collect();
    prefixes.sort_by_key(|(_, c)| std::cmp::Reverse(*c));
    prefixes.into_iter().map(|(p, _)| p).collect()
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EdgeRef, NodeRef};

    fn sample() -> GraphState {
        let nodes = vec![
            NodeRef { id: "memory:1".into(), label: "Memory".into() },
            NodeRef { id: "memory:2".into(), label: "Memory".into() },
            NodeRef { id: "entity:1".into(), label: "Entity".into() },
        ];
        let edges = vec![
            EdgeRef { from: "memory:1".into(), to: "entity:1".into(), kind: "mentions".into() },
            EdgeRef { from: "memory:2".into(), to: "entity:1".into(), kind: "mentions".into() },
        ];
        GraphState::new(nodes, edges)
    }

    #[test]
    fn codebook_captures_unique_labels_and_kinds() {
        let g = sample();
        let cb = Codebook::from_graph(&g);
        assert_eq!(cb.labels, vec!["Entity".to_string(), "Memory".to_string()]);
        assert_eq!(cb.kinds, vec!["mentions".to_string()]);
    }

    #[test]
    fn codebook_extracts_common_id_prefixes() {
        let g = sample();
        let cb = Codebook::from_graph(&g);
        // memory: is on 2/3 nodes, entity: on 1/3 — both above the 2% bar.
        assert!(cb.id_prefixes.contains(&"memory:".to_string()));
        assert!(cb.id_prefixes.contains(&"entity:".to_string()));
    }

    #[test]
    fn codebook_content_id_is_stable() {
        let g = sample();
        let cb1 = Codebook::from_graph(&g);
        let cb2 = Codebook::from_graph(&g);
        assert_eq!(cb1.content_hash(), cb2.content_hash());
        assert_eq!(cb1.content_id().len(), 64);
    }

    #[test]
    fn encoded_graph_round_trips_node_and_edge_count() {
        let g = sample();
        let cb = Codebook::from_graph(&g);
        let enc = EncodedGraph::encode(&g, &cb);
        assert_eq!(enc.nodes.len(), g.nodes.len());
        assert_eq!(enc.edges.len(), g.edges.len());
        assert_eq!(enc.codebook_id, cb.content_id());
    }

    #[test]
    fn encoded_graph_is_smaller_than_raw_json_at_scale() {
        // Tiny graphs are dominated by codebook overhead; the codebook
        // pays for itself only at scale. Use a larger fixture that's
        // still cheap to compute.
        let mut nodes = Vec::with_capacity(1_000);
        for i in 0..1_000 {
            nodes.push(NodeRef {
                id: format!("memory:{i:08x}"),
                label: "Memory".into(),
            });
        }
        let edges: Vec<EdgeRef> = (0..2_000)
            .map(|i| EdgeRef {
                from: format!("memory:{:08x}", i / 2),
                to: format!("memory:{:08x}", (i * 11) % 1_000),
                kind: "calls".into(),
            })
            .collect();
        let g = GraphState::new(nodes, edges);
        let cb = Codebook::from_graph(&g);
        let enc = EncodedGraph::encode(&g, &cb);
        let baseline = g.baseline_json_size();
        let with_codebook = cb.wire_size() + enc.wire_size();
        assert!(
            with_codebook < baseline,
            "encoded ({}) should be smaller than baseline ({})",
            with_codebook,
            baseline
        );
    }

    #[test]
    fn zstd_compression_amplifies_codebook_savings() {
        // The spec mandates zstd on every payload (§4.1). The codebook's
        // value is partly to enlarge the dictionary zstd has to work
        // with: repeated short integer indices compress much better
        // than repeated long strings (which already compress, but less).
        let mut nodes = Vec::with_capacity(10_000);
        for i in 0..10_000 {
            nodes.push(NodeRef {
                id: format!("memory:{i:08x}"),
                label: "Memory".into(),
            });
        }
        let edges: Vec<EdgeRef> = (0..20_000)
            .map(|i| EdgeRef {
                from: format!("memory:{:08x}", i / 2),
                to: format!("memory:{:08x}", (i * 13) % 10_000),
                kind: "calls".into(),
            })
            .collect();
        let g = GraphState::new(nodes, edges);
        let cb = Codebook::from_graph(&g);
        let enc = EncodedGraph::encode(&g, &cb);
        let baseline_raw = g.baseline_json_size();
        let baseline_zstd = compressed_size(&g);
        let codebook_zstd = cb.wire_compressed_size();
        let encoded_zstd = enc.wire_compressed_size();
        let zera_total = codebook_zstd + encoded_zstd;
        // The compound win (codebook + zstd) vs raw JSON must be ≥ 4×.
        let ratio_vs_raw = baseline_raw as f64 / zera_total as f64;
        assert!(
            ratio_vs_raw >= 4.0,
            "expected ≥ 4× over raw JSON, got {ratio_vs_raw:.2}× ({} → {})",
            baseline_raw,
            zera_total
        );
        // And ZERA must still beat zstd-of-JSON (not always by much at
        // this synthetic scale, but it must beat it — that's the bar).
        assert!(
            zera_total < baseline_zstd,
            "ZERA ({}) must beat plain zstd ({}) at this scale",
            zera_total,
            baseline_zstd
        );
    }
}
