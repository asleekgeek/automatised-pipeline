//! ZERA Layer 2 — GRAMMAR.
//!
//! Spec: `docs/protocols/ZERA-spec.md` §4.2 Layer 2.
//!
//! The grammar is a small, declarative set of Pāṇini-style production
//! rules. Both server and client run the same rules; the server ships
//! only the *base facts*, the client *derives* the rest by running the
//! grammar locally.
//!
//! # Domain-honesty note
//!
//! The spec's "95 % derivation rate" claim is calibrated to codebase
//! graphs, where rules like `SYMBOL(s) ∧ FILE(f) ∧ s.path = f.path →
//! DEFINED_IN(s, f)` recover most of the edge set. Cortex's memory
//! graph is mostly statistical (co_occurrence is weight-thresholded
//! over memory-text co-mentions, not a strict relational join), so
//! strict derivation only covers a slice of the edges. This layer
//! reports the actual saving on the live graph — no fabrication.
//!
//! # What this slice provides
//!
//! 1. A `Rule` enum that can be extended without breaking the wire.
//! 2. `SymmetricClosure { kinds }`: ship one direction of symmetric
//!    kinds, derive the other. Strictly equivalent — no information loss.
//! 3. `Grammar::split` and `Grammar::derive`: pure functions that
//!    factor a graph into (base, derivable). Round-trippable.
//! 4. `Grammar::round_trip_check`: invariant — for every input graph,
//!    derive(split(g)) ≡ g.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

use crate::{EdgeRef, GraphState, NodeRef};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum Rule {
    /// For each listed kind, treat the edge (a, b, kind) as equivalent to
    /// (b, a, kind). The server only ships the canonical direction
    /// (a, b) where a ≤ b; the client emits both on decode.
    SymmetricClosure { kinds: Vec<String> },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Grammar {
    pub rules: Vec<Rule>,
}

impl Grammar {
    /// The minimum useful grammar for the Cortex memory graph:
    /// the two relation kinds Cortex's schema documents as symmetric
    /// (`co_occurrence`, `correlates_with`).
    pub fn for_cortex_memory() -> Self {
        Grammar {
            rules: vec![Rule::SymmetricClosure {
                kinds: vec![
                    "co_occurrence".into(),
                    "correlates_with".into(),
                ],
            }],
        }
    }

    /// Empty grammar — every edge is shipped explicitly.
    pub fn none() -> Self {
        Grammar { rules: vec![] }
    }

    /// Wire size of the grammar as compact JSON + zstd at level 3.
    pub fn wire_compressed_size(&self) -> usize {
        let raw = serde_json::to_vec(self).unwrap_or_default();
        zstd::bulk::compress(&raw, 3).map(|v| v.len()).unwrap_or(0)
    }

    /// 32-byte BLAKE3 of the canonical serialization. The content id
    /// the client offers in HELLO.
    pub fn content_hash(&self) -> [u8; 32] {
        let mut h = blake3::Hasher::new();
        let raw = serde_json::to_vec(self).unwrap_or_default();
        h.update(&(raw.len() as u64).to_le_bytes());
        h.update(&raw);
        *h.finalize().as_bytes()
    }

    /// Factor a graph into `(base, bidirectional_markers, derivable)`.
    /// `base` is shipped as-is. `bidirectional_markers` is a tiny list of
    /// `(kind, lo, hi)` triples meaning "both directions of this edge
    /// exist in the source"; the client uses them to reconstruct the
    /// reverse direction without ambiguity. `derivable` is informational
    /// (the count of edges the grammar removed from the payload).
    ///
    /// This is the round-trip-safe formulation: on graphs where the source
    /// already canonicalizes symmetric relations (the Cortex memory graph
    /// does), the naive "always-derive" rule would fabricate edges that
    /// were never in the source. The marker list eliminates that risk.
    pub fn split(
        &self,
        edges: &[EdgeRef],
    ) -> (Vec<EdgeRef>, Vec<(String, String, String)>, Vec<EdgeRef>) {
        let mut base: Vec<EdgeRef> = Vec::with_capacity(edges.len());
        let mut bidirectional: BTreeSet<(String, String, String)> = BTreeSet::new();
        let mut derivable: Vec<EdgeRef> = Vec::new();
        let mut seen_first: BTreeSet<(String, String, String)> = BTreeSet::new();
        let symmetric_kinds: Vec<&Vec<String>> = self
            .rules
            .iter()
            .filter_map(|r| match r {
                Rule::SymmetricClosure { kinds } => Some(kinds),
            })
            .collect();

        for e in edges {
            let is_symmetric = symmetric_kinds
                .iter()
                .any(|ks| ks.iter().any(|k| k == &e.kind))
                && e.from != e.to;
            if is_symmetric {
                let (lo, hi) = if e.from <= e.to {
                    (e.from.clone(), e.to.clone())
                } else {
                    (e.to.clone(), e.from.clone())
                };
                let canon = (e.kind.clone(), lo, hi);
                if seen_first.contains(&canon) {
                    bidirectional.insert(canon);
                    derivable.push(e.clone());
                } else {
                    seen_first.insert(canon);
                    base.push(e.clone());
                }
            } else {
                base.push(e.clone());
            }
        }
        (base, bidirectional.into_iter().collect(), derivable)
    }

    /// Reconstruct the dropped reverse-direction edges from the base list
    /// and the bidirectional marker list.
    pub fn derive(
        &self,
        base: &[EdgeRef],
        bidirectional: &[(String, String, String)],
    ) -> Vec<EdgeRef> {
        let bidir_set: BTreeSet<&(String, String, String)> = bidirectional.iter().collect();
        let symmetric_kinds: Vec<&Vec<String>> = self
            .rules
            .iter()
            .filter_map(|r| match r {
                Rule::SymmetricClosure { kinds } => Some(kinds),
            })
            .collect();
        let mut out = Vec::new();
        for e in base {
            let is_symmetric = symmetric_kinds
                .iter()
                .any(|ks| ks.iter().any(|k| k == &e.kind))
                && e.from != e.to;
            if !is_symmetric {
                continue;
            }
            let (lo, hi) = if e.from <= e.to {
                (e.from.clone(), e.to.clone())
            } else {
                (e.to.clone(), e.from.clone())
            };
            let canon = (e.kind.clone(), lo, hi);
            if bidir_set.contains(&canon) {
                out.push(EdgeRef {
                    from: e.to.clone(),
                    to: e.from.clone(),
                    kind: e.kind.clone(),
                });
            }
        }
        out
    }

    /// Round-trip invariant: split → derive → recombine === original.
    /// Returns `(ok, original_count, recomputed_count)`.
    pub fn round_trip_check(&self, state: &GraphState) -> (bool, usize, usize) {
        let (base, bidir, _drop) = self.split(&state.edges);
        let derived = self.derive(&base, &bidir);
        let mut combined: BTreeSet<(String, String, String)> = base
            .iter()
            .map(|e| (e.kind.clone(), e.from.clone(), e.to.clone()))
            .collect();
        for e in &derived {
            combined.insert((e.kind.clone(), e.from.clone(), e.to.clone()));
        }
        let original: BTreeSet<(String, String, String)> = state
            .edges
            .iter()
            .map(|e| (e.kind.clone(), e.from.clone(), e.to.clone()))
            .collect();
        (combined == original, original.len(), combined.len())
    }
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn graph_with_symmetric_pair() -> GraphState {
        let nodes = vec![
            NodeRef { id: "a".into(), label: "Entity".into() },
            NodeRef { id: "b".into(), label: "Entity".into() },
        ];
        let edges = vec![
            EdgeRef { from: "a".into(), to: "b".into(), kind: "co_occurrence".into() },
            EdgeRef { from: "b".into(), to: "a".into(), kind: "co_occurrence".into() },
            EdgeRef { from: "a".into(), to: "b".into(), kind: "calls".into() }, // asym
        ];
        GraphState::new(nodes, edges)
    }

    #[test]
    fn symmetric_closure_drops_the_reverse_direction() {
        let g = graph_with_symmetric_pair();
        let grammar = Grammar::for_cortex_memory();
        let (base, bidir, derivable) = grammar.split(&g.edges);
        // The asymmetric `calls` edge stays, and exactly one direction of
        // the symmetric pair stays as base; the other goes to derivable;
        // the bidirectional marker captures that both existed in source.
        assert_eq!(base.len(), 2);
        assert_eq!(derivable.len(), 1);
        assert_eq!(bidir.len(), 1);
        assert_eq!(derivable[0].kind, "co_occurrence");
    }

    #[test]
    fn round_trip_recovers_the_full_graph() {
        let g = graph_with_symmetric_pair();
        let grammar = Grammar::for_cortex_memory();
        let (ok, orig, recomputed) = grammar.round_trip_check(&g);
        assert!(ok, "expected round-trip to reconstruct {} edges, got {}", orig, recomputed);
    }

    #[test]
    fn empty_grammar_is_a_noop() {
        let g = graph_with_symmetric_pair();
        let grammar = Grammar::none();
        let (base, bidir, derivable) = grammar.split(&g.edges);
        assert_eq!(base.len(), g.edges.len());
        assert!(bidir.is_empty());
        assert_eq!(derivable.len(), 0);
    }

    #[test]
    fn asymmetric_edges_are_never_dropped() {
        let nodes = vec![
            NodeRef { id: "a".into(), label: "Entity".into() },
            NodeRef { id: "b".into(), label: "Entity".into() },
        ];
        let edges = vec![
            EdgeRef { from: "a".into(), to: "b".into(), kind: "calls".into() },
            EdgeRef { from: "b".into(), to: "a".into(), kind: "calls".into() },
        ];
        let g = GraphState::new(nodes, edges);
        let grammar = Grammar::for_cortex_memory();
        let (base, bidir, derivable) = grammar.split(&g.edges);
        assert_eq!(base.len(), 2);
        assert!(bidir.is_empty());
        assert!(derivable.is_empty());
    }

    #[test]
    fn unidirectional_symmetric_edge_does_not_get_a_fabricated_reverse() {
        // CRITICAL: if the source has only ONE direction of a symmetric-eligible
        // edge (Cortex canonicalizes at insert time), the grammar must NOT
        // fabricate the reverse. This was an earlier bug — the round-trip on
        // production data caught it.
        let nodes = vec![
            NodeRef { id: "a".into(), label: "Entity".into() },
            NodeRef { id: "b".into(), label: "Entity".into() },
        ];
        let edges = vec![
            EdgeRef { from: "a".into(), to: "b".into(), kind: "co_occurrence".into() },
        ];
        let g = GraphState::new(nodes, edges);
        let grammar = Grammar::for_cortex_memory();
        let (base, bidir, derivable) = grammar.split(&g.edges);
        assert_eq!(base.len(), 1);
        assert!(bidir.is_empty());
        assert!(derivable.is_empty());
        let derived = grammar.derive(&base, &bidir);
        assert!(derived.is_empty(), "must NOT fabricate reverse direction");
        let (ok, _, _) = grammar.round_trip_check(&g);
        assert!(ok);
    }

    #[test]
    fn grammar_wire_size_is_tiny() {
        let grammar = Grammar::for_cortex_memory();
        // The whole grammar is a JSON object with a tiny array of kinds.
        // Compressed it must be < 200 bytes regardless of graph size.
        assert!(
            grammar.wire_compressed_size() < 200,
            "grammar payload {} too large",
            grammar.wire_compressed_size()
        );
    }
}
