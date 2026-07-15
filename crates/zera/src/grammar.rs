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

use crate::{EdgeRef, GraphState};

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
                kinds: vec!["co_occurrence".into(), "correlates_with".into()],
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

    /// Factor a graph into `(base, bidirectional_indices, derivable)`.
    ///
    /// `base` is the edge list the server ships. `bidirectional_indices`
    /// is a sorted vector of u32 indices into `base` identifying which
    /// of those edges have a reverse direction the client should
    /// reconstruct. Storing indices (not (kind, src, dst) triples)
    /// is what makes the layer cheap: 596 markers over 230 K edges
    /// fit in ~1 KB after delta + zstd instead of ~30 KB as triples.
    ///
    /// Round-trip-safe by construction: only edges flagged in the
    /// bidirectional index list get a reverse derived at decode.
    pub fn split(&self, edges: &[EdgeRef]) -> (Vec<EdgeRef>, Vec<u32>, Vec<EdgeRef>) {
        // Two passes: first identify which canonical pairs are bidirectional;
        // second emit base + indices in base order.
        let symmetric_kinds: Vec<&Vec<String>> = self
            .rules
            .iter()
            .filter_map(|r| match r {
                Rule::SymmetricClosure { kinds } => Some(kinds),
            })
            .collect();

        let is_symmetric = |kind: &str| {
            symmetric_kinds
                .iter()
                .any(|ks| ks.iter().any(|k| k == kind))
        };
        let canon = |e: &EdgeRef| {
            let (lo, hi) = if e.from <= e.to {
                (e.from.clone(), e.to.clone())
            } else {
                (e.to.clone(), e.from.clone())
            };
            (e.kind.clone(), lo, hi)
        };

        // Pass 1 — identify bidirectional canons by counting both-directions.
        let mut seen_once: BTreeSet<(String, String, String)> = BTreeSet::new();
        let mut bidirectional_canons: BTreeSet<(String, String, String)> = BTreeSet::new();
        for e in edges {
            if !is_symmetric(&e.kind) || e.from == e.to {
                continue;
            }
            let c = canon(e);
            if seen_once.contains(&c) {
                bidirectional_canons.insert(c);
            } else {
                seen_once.insert(c);
            }
        }

        // Pass 2 — emit base (keep first observed direction; drop the
        // duplicate reverse) and record bidirectional indices into base.
        let mut base: Vec<EdgeRef> = Vec::with_capacity(edges.len());
        let mut bidir_indices: Vec<u32> = Vec::new();
        let mut emitted_first: BTreeSet<(String, String, String)> = BTreeSet::new();
        let mut derivable: Vec<EdgeRef> = Vec::new();

        for e in edges {
            if is_symmetric(&e.kind) && e.from != e.to {
                let c = canon(e);
                if emitted_first.contains(&c) {
                    // Reverse of an already-emitted base edge — dropped.
                    derivable.push(e.clone());
                } else {
                    emitted_first.insert(c.clone());
                    let idx = base.len() as u32;
                    base.push(e.clone());
                    if bidirectional_canons.contains(&c) {
                        bidir_indices.push(idx);
                    }
                }
            } else {
                base.push(e.clone());
            }
        }
        (base, bidir_indices, derivable)
    }

    /// Reconstruct the dropped reverse-direction edges from the base list
    /// and the bidirectional index list.
    pub fn derive(&self, base: &[EdgeRef], bidirectional_indices: &[u32]) -> Vec<EdgeRef> {
        bidirectional_indices
            .iter()
            .filter_map(|&i| base.get(i as usize))
            .map(|e| EdgeRef {
                from: e.to.clone(),
                to: e.from.clone(),
                kind: e.kind.clone(),
            })
            .collect()
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
    use crate::NodeRef;

    fn graph_with_symmetric_pair() -> GraphState {
        let nodes = vec![
            NodeRef {
                id: "a".into(),
                label: "Entity".into(),
            },
            NodeRef {
                id: "b".into(),
                label: "Entity".into(),
            },
        ];
        let edges = vec![
            EdgeRef {
                from: "a".into(),
                to: "b".into(),
                kind: "co_occurrence".into(),
            },
            EdgeRef {
                from: "b".into(),
                to: "a".into(),
                kind: "co_occurrence".into(),
            },
            EdgeRef {
                from: "a".into(),
                to: "b".into(),
                kind: "calls".into(),
            }, // asym
        ];
        GraphState::new(nodes, edges)
    }

    #[test]
    fn symmetric_closure_drops_the_reverse_direction() {
        let g = graph_with_symmetric_pair();
        let grammar = Grammar::for_cortex_memory();
        let (base, bidir, derivable) = grammar.split(&g.edges);
        // Two base edges (one direction of the symmetric pair + calls),
        // one derivable (the dropped reverse), and one bidirectional
        // INDEX (pointing to the base edge whose reverse to reconstruct).
        assert_eq!(base.len(), 2);
        assert_eq!(derivable.len(), 1);
        assert_eq!(bidir.len(), 1);
        // The index points to a symmetric base edge.
        let pointed = &base[bidir[0] as usize];
        assert_eq!(pointed.kind, "co_occurrence");
    }

    #[test]
    fn round_trip_recovers_the_full_graph() {
        let g = graph_with_symmetric_pair();
        let grammar = Grammar::for_cortex_memory();
        let (ok, orig, recomputed) = grammar.round_trip_check(&g);
        assert!(
            ok,
            "expected round-trip to reconstruct {} edges, got {}",
            orig, recomputed
        );
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
            NodeRef {
                id: "a".into(),
                label: "Entity".into(),
            },
            NodeRef {
                id: "b".into(),
                label: "Entity".into(),
            },
        ];
        let edges = vec![
            EdgeRef {
                from: "a".into(),
                to: "b".into(),
                kind: "calls".into(),
            },
            EdgeRef {
                from: "b".into(),
                to: "a".into(),
                kind: "calls".into(),
            },
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
            NodeRef {
                id: "a".into(),
                label: "Entity".into(),
            },
            NodeRef {
                id: "b".into(),
                label: "Entity".into(),
            },
        ];
        let edges = vec![EdgeRef {
            from: "a".into(),
            to: "b".into(),
            kind: "co_occurrence".into(),
        }];
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
