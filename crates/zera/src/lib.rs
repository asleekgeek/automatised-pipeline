//! ZERA — Zero-latency Encoded Recursive Atlas.
//!
//! Reference implementation of the ZERA wire protocol.
//! Spec: `docs/protocols/ZERA-spec.md`.
//!
//! # S1 — HELLO (this slice)
//!
//! The cache-handshake primitive. Before any payload byte is sent, the
//! client tells the server which content hashes it already has, and the
//! server computes the content hash of the current graph state. If the
//! hashes match, the server sends nothing further. If they differ, the
//! server proceeds to subsequent layers (CODEBOOK, GRAMMAR, …) which
//! will be implemented in later slices.
//!
//! This module is intentionally minimal: nothing about codebooks, sparse
//! codes, eigenmodes, or attractors lives here yet. The only contract
//! it enforces is that the content hash is **deterministic, canonical,
//! and stable across runs** — every later layer depends on this.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub mod codebook;
pub use codebook::{Codebook, EncEdge, EncNode, EncodedGraph};

/// A node in the graph as seen by ZERA: just the addressable identity
/// and the label. The decoder layers expand this to the full node body.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NodeRef {
    pub id: String,
    pub label: String,
}

/// An edge: from-id, to-id, relation kind (rel-table name).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EdgeRef {
    pub from: String,
    pub to: String,
    pub kind: String,
}

/// The canonical, hashable representation of a graph state. Every other
/// ZERA frame is keyed off the hash of this structure.
///
/// Canonicalization rules (source: ZERA-spec §4.2 Layer 0):
///   * Nodes sorted by (label, id).
///   * Edges sorted by (kind, from, to).
///   * BLAKE3 over the canonical bytes.
///
/// Two graphs that produce equal `GraphState`s have equal content hashes.
/// Two graphs with even one different edge produce different hashes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GraphState {
    pub nodes: Vec<NodeRef>,
    pub edges: Vec<EdgeRef>,
}

impl GraphState {
    /// Build from unsorted inputs, canonicalize in place. The constructor
    /// IS the contract: callers cannot accidentally produce a non-
    /// canonical state.
    pub fn new(mut nodes: Vec<NodeRef>, mut edges: Vec<EdgeRef>) -> Self {
        nodes.sort_by(|a, b| (&a.label, &a.id).cmp(&(&b.label, &b.id)));
        nodes.dedup_by(|a, b| a.label == b.label && a.id == b.id);
        edges.sort_by(|a, b| (&a.kind, &a.from, &a.to).cmp(&(&b.kind, &b.from, &b.to)));
        edges.dedup_by(|a, b| a.kind == b.kind && a.from == b.from && a.to == b.to);
        GraphState { nodes, edges }
    }

    /// 32-byte BLAKE3 digest of the canonical bytes. The cache-key
    /// primitive every other ZERA frame is identified by.
    pub fn content_hash(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        // Length-prefixed records ensure id="a", label="bc" and
        // id="ab", label="c" hash distinctly.
        let nlen = self.nodes.len() as u64;
        hasher.update(&nlen.to_le_bytes());
        for n in &self.nodes {
            hasher.update(&(n.label.len() as u64).to_le_bytes());
            hasher.update(n.label.as_bytes());
            hasher.update(&(n.id.len() as u64).to_le_bytes());
            hasher.update(n.id.as_bytes());
        }
        let elen = self.edges.len() as u64;
        hasher.update(&elen.to_le_bytes());
        for e in &self.edges {
            hasher.update(&(e.kind.len() as u64).to_le_bytes());
            hasher.update(e.kind.as_bytes());
            hasher.update(&(e.from.len() as u64).to_le_bytes());
            hasher.update(e.from.as_bytes());
            hasher.update(&(e.to.len() as u64).to_le_bytes());
            hasher.update(e.to.as_bytes());
        }
        *hasher.finalize().as_bytes()
    }

    /// Lowercase-hex of the content hash. Used as the URL-safe identifier
    /// in HELLO frames and content-addressed cache keys.
    pub fn content_id(&self) -> String {
        hex_encode(&self.content_hash())
    }

    /// Size the current ZERA-naive wire format would take if we shipped
    /// the entire graph as a JSON list of {nodes, edges} (the baseline
    /// against which every later layer measures its compression).
    pub fn baseline_json_size(&self) -> usize {
        // serde_json's compact output. Not pretty-printed.
        serde_json::to_vec(self).map(|v| v.len()).unwrap_or(0)
    }

    /// Per-label and per-edge-kind histograms. Used by the visualization
    /// to surface what's in the graph at a glance.
    pub fn histograms(&self) -> Histograms {
        let mut by_label: BTreeMap<String, usize> = BTreeMap::new();
        for n in &self.nodes {
            *by_label.entry(n.label.clone()).or_default() += 1;
        }
        let mut by_kind: BTreeMap<String, usize> = BTreeMap::new();
        for e in &self.edges {
            *by_kind.entry(e.kind.clone()).or_default() += 1;
        }
        Histograms { by_label, by_kind }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Histograms {
    pub by_label: BTreeMap<String, usize>,
    pub by_kind: BTreeMap<String, usize>,
}

/// The client's offering at session start: every content hash it has
/// cached locally, by content-class. The server inspects these and
/// returns a `HelloResponse` indicating which payloads need to flow.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HelloRequest {
    /// Cached graph content hashes (from previous sessions on this
    /// client). Hex-encoded BLAKE3.
    pub cached_graph_ids: Vec<String>,
    /// Cached codebook hashes (later slices).
    pub cached_codebook_ids: Vec<String>,
    /// Cached grammar hash (later slice).
    pub cached_grammar_id: Option<String>,
}

/// Server's reply: the current graph's content id, whether the client
/// has it, and the size of the JSON baseline the server would otherwise
/// have shipped.
///
/// In S1 there is no payload yet beyond this manifest. The point of S1
/// is to prove that the hash itself is enough to *decide whether any
/// payload needs to flow*. Later slices fill in the layers when it does.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelloResponse {
    /// Content id of the current graph (BLAKE3 hex).
    pub graph_id: String,
    /// True iff the client already has this graph cached.
    pub cache_hit: bool,
    /// Bytes the server would have shipped under the JSON baseline if
    /// the client had nothing. The denominator for every compression
    /// claim in later slices.
    pub baseline_json_bytes: usize,
    /// Bytes this HELLO frame itself takes on the wire (sans transport
    /// framing). The numerator when `cache_hit == true`: the only thing
    /// the server sends is this manifest.
    pub manifest_bytes: usize,
    /// Counts the visualization renders without re-parsing the graph.
    pub node_count: usize,
    pub edge_count: usize,
    pub histograms: Histograms,
    /// Spec version this manifest targets.
    pub zera_version: &'static str,
}

/// Pure function: given the current graph state and the client's cache
/// hashes, decide what the server should ship.
///
/// In S1 the only outcome is the HelloResponse manifest. Later slices
/// add a `next_frame` payload when `cache_hit == false`.
pub fn hello(state: &GraphState, req: &HelloRequest) -> HelloResponse {
    let graph_id = state.content_id();
    let cache_hit = req.cached_graph_ids.iter().any(|h| h == &graph_id);
    let baseline_json_bytes = state.baseline_json_size();
    let histograms = state.histograms();

    let resp = HelloResponse {
        graph_id: graph_id.clone(),
        cache_hit,
        baseline_json_bytes,
        manifest_bytes: 0, // filled in below
        node_count: state.nodes.len(),
        edge_count: state.edges.len(),
        histograms,
        zera_version: "0.0.1",
    };
    // Serialize once to measure size, then re-attach the measured size
    // so the manifest is self-describing.
    let approx = serde_json::to_vec(&resp).map(|v| v.len()).unwrap_or(0);
    HelloResponse { manifest_bytes: approx, ..resp }
}

// ---------------------------------------------------------------------------
// Helpers (private)
// ---------------------------------------------------------------------------

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0xf) as usize] as char);
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> GraphState {
        let nodes = vec![
            NodeRef { id: "a".into(), label: "File".into() },
            NodeRef { id: "b".into(), label: "Function".into() },
        ];
        let edges = vec![EdgeRef {
            from: "a".into(),
            to: "b".into(),
            kind: "Defines_File_Function".into(),
        }];
        GraphState::new(nodes, edges)
    }

    #[test]
    fn content_hash_is_stable_across_input_order() {
        let g1 = GraphState::new(
            vec![
                NodeRef { id: "a".into(), label: "File".into() },
                NodeRef { id: "b".into(), label: "Function".into() },
            ],
            vec![],
        );
        let g2 = GraphState::new(
            vec![
                NodeRef { id: "b".into(), label: "Function".into() },
                NodeRef { id: "a".into(), label: "File".into() },
            ],
            vec![],
        );
        assert_eq!(g1.content_hash(), g2.content_hash());
    }

    #[test]
    fn content_hash_changes_when_an_edge_changes() {
        let g1 = sample();
        let mut g2 = g1.clone();
        g2.edges[0].kind = "Calls_Function_Function".into();
        // Re-canonicalize since we hand-mutated.
        let g2 = GraphState::new(g2.nodes, g2.edges);
        assert_ne!(g1.content_hash(), g2.content_hash());
    }

    #[test]
    fn content_id_is_64_lowercase_hex() {
        let id = sample().content_id();
        assert_eq!(id.len(), 64);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit() && (c.is_ascii_digit() || c.is_ascii_lowercase())));
    }

    #[test]
    fn duplicate_nodes_collapse_canonically() {
        // The canonical form must dedupe; otherwise two clients with
        // different upstream-dedup behaviour would hash differently.
        let g1 = GraphState::new(
            vec![
                NodeRef { id: "a".into(), label: "File".into() },
                NodeRef { id: "a".into(), label: "File".into() },
            ],
            vec![],
        );
        let g2 = GraphState::new(
            vec![NodeRef { id: "a".into(), label: "File".into() }],
            vec![],
        );
        assert_eq!(g1.nodes.len(), 1);
        assert_eq!(g1.content_hash(), g2.content_hash());
    }

    #[test]
    fn hello_marks_cache_hit_when_client_has_id() {
        let g = sample();
        let id = g.content_id();
        let req = HelloRequest {
            cached_graph_ids: vec![id.clone()],
            ..Default::default()
        };
        let resp = hello(&g, &req);
        assert!(resp.cache_hit);
        assert_eq!(resp.graph_id, id);
    }

    #[test]
    fn hello_marks_cache_miss_when_client_has_other() {
        let g = sample();
        let req = HelloRequest {
            cached_graph_ids: vec!["0".repeat(64)],
            ..Default::default()
        };
        let resp = hello(&g, &req);
        assert!(!resp.cache_hit);
    }

    #[test]
    fn manifest_is_orders_of_magnitude_smaller_than_baseline_at_scale() {
        // Synthesise 10 000 nodes + 30 000 edges — small compared to the
        // 1 GB production target but enough to demonstrate the ratio.
        let mut nodes = Vec::with_capacity(10_000);
        for i in 0..10_000 {
            nodes.push(NodeRef {
                id: format!("symbol:{i:08x}"),
                label: if i % 3 == 0 { "Function" } else { "Method" }.into(),
            });
        }
        let mut edges = Vec::with_capacity(30_000);
        for i in 0..30_000 {
            edges.push(EdgeRef {
                from: format!("symbol:{:08x}", i / 3),
                to: format!("symbol:{:08x}", (i * 7) % 10_000),
                kind: "Calls_Function_Function".into(),
            });
        }
        let state = GraphState::new(nodes, edges);
        let req = HelloRequest {
            cached_graph_ids: vec![state.content_id()],
            ..Default::default()
        };
        let resp = hello(&state, &req);
        // Manifest stays under 1 KB; baseline is well over 1 MB. That
        // gap is what makes the cache-hit case zero-payload in S1.
        // (Stricter compression claims belong to later slices.)
        assert!(
            resp.baseline_json_bytes > 1_000_000,
            "baseline should be > 1 MB at 10k nodes, got {} bytes",
            resp.baseline_json_bytes,
        );
        assert!(
            resp.manifest_bytes < 2_000,
            "manifest should be < 2 KB, got {} bytes",
            resp.manifest_bytes,
        );
    }
}
