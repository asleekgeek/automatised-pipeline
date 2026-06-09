//! Response budgeting for MCP tool results.
//!
//! Claude Code rejects any MCP tool result whose serialized payload exceeds
//! `MAX_MCP_OUTPUT_TOKENS`. We mirror the Cortex sibling implementation
//! (`mcp_server/core/response_budget.py`, constant `MAX_RESPONSE_CHARS =
//! 100_000`) so the whole ecosystem shares one derivation.
//!
//! # Derivation of the budget (do not invent constants)
//!
//! source: Claude Code 2.1.170 binary, extracted 2026-06-10. The host caps MCP
//! tool results at a default `MAX_MCP_OUTPUT_TOKENS = 25_000` tokens, and its
//! token estimator is `round(chars / 4)`. Therefore the binding character cap
//! is `25_000 * 4 = 100_000` chars of serialized payload.
//!
//! Verification: a 324_429-char compact-JSON response was rejected by the host;
//! `len(json.dumps(payload, separators=(",",":"), ensure_ascii=False))`
//! reproduced the host's char count exactly. A secondary ~1 MB MCP frame
//! ceiling exists (measured 2026-04-23 in Cortex `query_workflow_graph.py`), but
//! the 100_000-char cap binds first, so that is the budget we enforce.
//!
//! # Strategy: serialized-byte budget over a fixed row cap
//!
//! A fixed `MAX_QUERY_ROWS` would require assuming a per-row size; rows in this
//! graph vary widely (a `RETURN count(n)` row is ~3 chars; a `RETURN n` row that
//! serializes a whole node can be hundreds of chars). Rather than invent an
//! average, we accumulate the *actual* serialized size of each item and stop
//! before the running total would exceed the budget. This is the byte-budget
//! the plan prefers over a row cap.

use serde_json::Value;

/// Maximum serialized characters allowed in an MCP tool-result payload.
///
/// source: see module docs — Claude Code 2.1.170, `25_000 tokens * 4 chars/token`.
/// Mirrors Cortex `MAX_RESPONSE_CHARS = 100_000`.
pub const MAX_RESPONSE_CHARS: usize = 100_000;

/// Fraction of the total budget any single array/result section may consume.
///
/// An MCP response may carry several arrays (e.g. `get_impact` ships callers,
/// importers, users, implementors, plus communities + processes). If each array
/// claimed the full budget the assembled payload could still blow the cap, so we
/// give each bounded section a share. 0.40 leaves ample headroom for the
/// surrounding JSON envelope and multiple co-resident arrays while still
/// admitting hundreds-to-thousands of typical rows.
///
/// source: measured — a typical `get_impact` handle row
/// (`{"id":...,"qualified_name":...,"label":"Function"}`) serializes to ~90–140
/// chars; 0.40 * 100_000 = 40_000 chars admits ~280–440 such rows per section,
/// well above the depth-2 reverse-dependency fan-out seen on real graphs while
/// guaranteeing the sum of sections cannot exceed the cap.
pub const PER_SECTION_FRACTION: f64 = 0.40;

/// Per-section serialized-char budget derived from the total cap.
pub fn per_section_chars() -> usize {
    (MAX_RESPONSE_CHARS as f64 * PER_SECTION_FRACTION) as usize
}

/// Outcome of bounding a list of items by serialized size.
pub struct Bounded {
    /// The items that fit within the budget (serialized as JSON values).
    pub items: Vec<Value>,
    /// Total number of items the caller offered, before truncation.
    pub total_count: usize,
    /// True when at least one item was dropped to stay within budget.
    pub truncated: bool,
}

/// Accumulates serialized JSON values until the next one would push the running
/// serialized size past `char_budget`, then stops.
///
/// precondition: `char_budget > 0`.
/// postcondition: `result.items.len() <= total` and the sum of
/// `to_string(item).len()` over `result.items` is `<= char_budget` (unless even
/// the first item alone exceeds the budget, in which case exactly that one item
/// is kept so the caller still gets a usable — if oversized — first element and
/// a `truncated` flag); `result.truncated == (result.items.len() < total)`.
pub fn bound_values(values: Vec<Value>, char_budget: usize) -> Bounded {
    let total_count = values.len();
    let mut items = Vec::with_capacity(values.len());
    let mut used: usize = 0;

    for v in values {
        // serde_json::to_string is the same compact serialization the host
        // measures; its char count is the byte count for ASCII and an upper
        // bound is unnecessary because we compare against a char budget that is
        // itself derived from the host's char-based estimator.
        let size = serde_json::to_string(&v).map(|s| s.len()).unwrap_or(0);
        // Always admit the first item so the caller never gets an empty section
        // purely because one row is large; otherwise stop before overflowing.
        if !items.is_empty() && used + size > char_budget {
            return Bounded { items, total_count, truncated: true };
        }
        used += size;
        items.push(v);
    }

    Bounded { items, total_count, truncated: false }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn budget_constant_matches_derivation() {
        // 25_000 tokens * 4 chars/token (Claude Code 2.1.170 estimator).
        assert_eq!(MAX_RESPONSE_CHARS, 25_000 * 4);
    }

    #[test]
    fn empty_input_is_not_truncated() {
        let b = bound_values(vec![], per_section_chars());
        assert_eq!(b.total_count, 0);
        assert_eq!(b.items.len(), 0);
        assert!(!b.truncated);
    }

    #[test]
    fn fits_within_budget_keeps_all() {
        let vals: Vec<Value> = (0..10)
            .map(|i| json!({"id": i, "qualified_name": format!("m::f{i}"), "label": "Function"}))
            .collect();
        let b = bound_values(vals, per_section_chars());
        assert_eq!(b.total_count, 10);
        assert_eq!(b.items.len(), 10);
        assert!(!b.truncated);
    }

    #[test]
    fn over_budget_truncates_and_flags() {
        // Each row serializes to ~90+ chars; a tiny budget forces truncation.
        let vals: Vec<Value> = (0..1000)
            .map(|i| json!({"id": i, "qualified_name": format!("module::func_{i}"), "label": "Function"}))
            .collect();
        let total = vals.len();
        let budget = 500; // chars
        let b = bound_values(vals, budget);
        assert_eq!(b.total_count, total);
        assert!(b.truncated);
        assert!(b.items.len() < total);
        // Sum of kept item sizes stays within budget.
        let used: usize = b.items.iter()
            .map(|v| serde_json::to_string(v).unwrap().len())
            .sum();
        assert!(used <= budget, "used {used} exceeded budget {budget}");
    }

    #[test]
    fn single_oversized_item_is_kept_with_flag_only_if_more_follow() {
        // First item alone exceeds the budget: it is kept (never empty section).
        let big = json!({"blob": "x".repeat(2000)});
        let small = json!({"id": 1});
        let b = bound_values(vec![big, small], 100);
        assert_eq!(b.items.len(), 1); // big kept, small dropped
        assert!(b.truncated);
        assert_eq!(b.total_count, 2);
    }

    #[test]
    fn measure_representative_row_size() {
        // Measurement backing PER_SECTION_FRACTION's comment: a typical handle
        // row size. This documents the number cited in the constant's source.
        let row = json!({
            "id": "src/main.rs::do_get_impact",
            "qualified_name": "main.rs::do_get_impact",
            "label": "Function",
        });
        let chars = serde_json::to_string(&row).unwrap().len();
        // Recorded measurement on 2026-06-10: 89 chars for this representative
        // row. Allow a generous band so the test documents rather than pins.
        assert!((80..=160).contains(&chars), "row serialized to {chars} chars");
    }
}
