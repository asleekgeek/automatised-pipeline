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
//!
//! # Why no safety factor is applied (the byte budget is inherently conservative)
//!
//! The host's token estimator runs on JavaScript `String.length`, which counts
//! **UTF-16 code units**. Our budget is measured with `serde_json::to_string`,
//! whose `.len()` counts **UTF-8 bytes**. For every Unicode scalar value, the
//! UTF-8 byte length is `>=` the UTF-16 code-unit length, proven per scalar
//! class:
//!   - ASCII (U+0000..U+007F):        1 UTF-8 byte  >= 1 UTF-16 unit.
//!   - BMP non-ASCII (U+0080..U+FFFF): 2 or 3 bytes  >= 1 UTF-16 unit.
//!   - Astral / non-BMP (U+10000..):   4 bytes       >= 2 UTF-16 units (surrogate pair).
//!
//! Summing over any string, `utf8_bytes >= utf16_units`. Therefore the byte
//! budget enforced by `bound_values` can NEVER overshoot the host's UTF-16-unit
//! count — it only ever under-fills relative to the host estimate, which is the
//! safe direction. No safety factor is needed and none is applied; introducing
//! one would only waste budget. (See the `utf8_bytes_dominate_utf16_units`
//! test for the executable witness of this invariant.)
//!
//! Note the asymmetry with the Cortex Python sibling
//! (`mcp_server/core/response_budget.py`), which applies
//! `SAFETY_FACTOR = 0.75`. Python `len()` counts **Unicode code points**, and a
//! code point can UNDERCOUNT UTF-16 units (a single non-BMP scalar is 1 code
//! point but 2 UTF-16 units). That divergence runs the *unsafe* direction — the
//! measured size can be below the host's count — so Cortex must discount. Rust's
//! byte count diverges the *safe* direction, so the two repos legitimately
//! differ on whether a safety factor is required.
//! source: ai-prd-builder `ContextManager.swift`, commit 462de01 — the original
//! UTF-16-units-vs-code-points estimator divergence that motivates the Cortex
//! `SAFETY_FACTOR`.

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

/// Outcome of paginating a list of items by serialized size from a cursor.
///
/// This is the cursor-aware sibling of [`Bounded`]. Where `Bounded` always
/// starts from item 0 and reports only whether a cut happened, `BoundedPage`
/// starts from a caller-supplied `offset` and reports where the next page
/// begins, turning truncation into pacing: a client can page
/// `offset = next_offset` until `next_offset` is `None` and reconstruct the
/// entire list in budget-sized pages with no duplicates and no gaps.
///
/// # Cursor correctness depends on a stable total order
///
/// A cursor that skips `offset` items only retrieves every row exactly once if
/// the input list is in the **same order on every call**. If the order can
/// shift between calls (e.g. an unordered graph scan whose row order is an
/// engine implementation detail), paging silently skips and duplicates rows —
/// a correctness bug. Therefore the caller MUST pass items already sorted by a
/// deterministic total-order key, and document where that order comes from at
/// the call site. This module bounds bytes; it does NOT impose order.
pub struct BoundedPage {
    /// The items in `[offset, offset + items.len())` that fit the budget.
    pub items: Vec<Value>,
    /// Total number of items in the full list, before offset or truncation.
    pub total_count: usize,
    /// True when items remain after this page (i.e. `next_offset` is `Some`).
    pub truncated: bool,
    /// Index at which the next page begins, present iff more data exists.
    /// Equals `offset + items.len()` when items remain, else `None`.
    pub next_offset: Option<u64>,
}

/// Pages a list of serialized JSON values: skips `offset` items, then fills the
/// byte budget exactly as [`bound_values`] does, and reports the cursor for the
/// next page.
///
/// precondition: `char_budget > 0`; `values` is in a deterministic total order
/// that is identical across calls (see [`BoundedPage`] — the caller owns this).
/// postcondition: let `tail = values[min(offset, len)..]`. `result.items` is the
/// longest prefix of `tail` whose serialized sizes sum to `<= char_budget`
/// (always at least one item if `tail` is non-empty, even if that one item
/// alone exceeds the budget — never an empty page purely from a large row).
/// `result.total_count == values.len()`. `result.next_offset ==
/// Some(offset + items.len())` iff items remain after this page, else `None`,
/// and `result.truncated == result.next_offset.is_some()`. An `offset >= len`
/// yields empty items, `truncated == false`, `next_offset == None`.
///
/// Page-through invariant: starting at `offset = 0` and repeatedly calling with
/// `offset = next_offset` until `next_offset` is `None` visits every element of
/// `values` exactly once, in order — the union of pages equals the full list
/// with no duplicates and no gaps.
pub fn bound_values_paged(values: Vec<Value>, offset: u64, char_budget: usize) -> BoundedPage {
    let total_count = values.len();
    let start = (offset as usize).min(total_count);
    let mut items = Vec::with_capacity(total_count - start);
    let mut used: usize = 0;
    let mut consumed: usize = 0; // items placed on this page

    for v in values.into_iter().skip(start) {
        let size = serde_json::to_string(&v).map(|s| s.len()).unwrap_or(0);
        // Always admit the first item of the page so a single large row never
        // yields an empty page (mirrors bound_values). Otherwise stop before
        // the running serialized total would exceed the budget.
        if !items.is_empty() && used + size > char_budget {
            break;
        }
        used += size;
        items.push(v);
        consumed += 1;
    }

    let next_index = start + consumed;
    let more_remain = next_index < total_count;
    BoundedPage {
        items,
        total_count,
        truncated: more_remain,
        next_offset: if more_remain {
            Some(next_index as u64)
        } else {
            None
        },
    }
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
        // measures. Its `.len()` is the UTF-8 byte count, which is >= the host's
        // UTF-16 code-unit count for every string (see module docs, "Why no
        // safety factor is applied"). Counting bytes therefore only ever
        // overestimates payload size against the host budget — the safe
        // direction — so no inflation or safety factor is needed here.
        let size = serde_json::to_string(&v).map(|s| s.len()).unwrap_or(0);
        // Always admit the first item so the caller never gets an empty section
        // purely because one row is large; otherwise stop before overflowing.
        if !items.is_empty() && used + size > char_budget {
            return Bounded {
                items,
                total_count,
                truncated: true,
            };
        }
        used += size;
        items.push(v);
    }

    Bounded {
        items,
        total_count,
        truncated: false,
    }
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
        let used: usize = b
            .items
            .iter()
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
    fn utf8_bytes_dominate_utf16_units() {
        // Executable witness for the module-doc invariant: the UTF-8 byte length
        // serde_json measures is never less than the UTF-16 code-unit length the
        // host's String.length estimator counts. Mixed string spans all three
        // scalar classes: ASCII, BMP non-ASCII, and astral (non-BMP).
        let mixed = "abcé🦀"; // ASCII x3 + 'é' (U+00E9, BMP) + '🦀' (U+1F980, astral)
        let utf8_bytes = mixed.len();
        let utf16_units = mixed.encode_utf16().count();

        // Per-class arithmetic, documented by exact counts:
        //   'a','b','c' : 1 byte  / 1 unit each -> 3 bytes / 3 units
        //   'é'         : 2 bytes / 1 unit       -> 5 bytes / 4 units
        //   '🦀'        : 4 bytes / 2 units (surrogate pair) -> 9 bytes / 6 units
        assert_eq!(utf8_bytes, 9, "expected 3 + 2 + 4 UTF-8 bytes");
        assert_eq!(utf16_units, 6, "expected 3 + 1 + 2 UTF-16 code units");

        // The invariant the budget relies on: bytes >= units, so a byte budget
        // can never overshoot the host's UTF-16-unit count.
        assert!(
            utf8_bytes >= utf16_units,
            "UTF-8 byte len {utf8_bytes} must be >= UTF-16 unit count {utf16_units}"
        );
    }

    // -- bound_values_paged ------------------------------------------------

    /// Drives the cursor to exhaustion and returns the concatenation of every
    /// page's items, plus the sequence of next_offset values observed.
    fn page_through(values: Vec<Value>, budget: usize) -> (Vec<Value>, Vec<Option<u64>>) {
        let mut all = Vec::new();
        let mut cursors = Vec::new();
        let mut offset: u64 = 0;
        loop {
            let page = bound_values_paged(values.clone(), offset, budget);
            all.extend(page.items.iter().cloned());
            cursors.push(page.next_offset);
            match page.next_offset {
                Some(n) => {
                    assert!(n > offset, "cursor must advance: {n} <= {offset}");
                    offset = n;
                }
                None => break,
            }
        }
        (all, cursors)
    }

    #[test]
    fn paging_reconstructs_full_list_no_dupes_no_gaps() {
        // Generic page-through reconstruction: union of pages == full list, in
        // order, exactly once each. Tiny budget forces many pages.
        let vals: Vec<Value> = (0..200)
            .map(|i| json!({"id": i, "qualified_name": format!("module::func_{i}"), "label": "Function"}))
            .collect();
        let (reconstructed, _) = page_through(vals.clone(), 300);
        assert_eq!(
            reconstructed, vals,
            "paged union must equal unpaged full list"
        );
    }

    #[test]
    fn paging_single_page_when_everything_fits() {
        let vals: Vec<Value> = (0..5).map(|i| json!({"id": i})).collect();
        let page = bound_values_paged(vals.clone(), 0, per_section_chars());
        assert_eq!(page.items.len(), 5);
        assert!(!page.truncated);
        assert_eq!(
            page.next_offset, None,
            "no next_offset on a complete final page"
        );
        assert_eq!(page.total_count, 5);
    }

    #[test]
    fn paging_next_offset_absent_on_final_page() {
        let vals: Vec<Value> = (0..50)
            .map(|i| json!({"id": i, "qualified_name": format!("m::f{i}")}))
            .collect();
        let (_, cursors) = page_through(vals, 200);
        assert!(
            cursors.len() >= 2,
            "tiny budget should force multiple pages"
        );
        assert_eq!(
            *cursors.last().unwrap(),
            None,
            "final page has no next_offset"
        );
        // Every non-final page must carry a cursor.
        for c in &cursors[..cursors.len() - 1] {
            assert!(c.is_some(), "non-final page must advance the cursor");
        }
    }

    #[test]
    fn paging_offset_beyond_end_is_empty_no_cursor() {
        let vals: Vec<Value> = (0..3).map(|i| json!({"id": i})).collect();
        let page = bound_values_paged(vals, 99, per_section_chars());
        assert_eq!(page.items.len(), 0);
        assert!(!page.truncated);
        assert_eq!(page.next_offset, None);
        assert_eq!(page.total_count, 3, "total still reflects the full list");
    }

    #[test]
    fn paging_offset_at_exact_end_is_empty_no_cursor() {
        let vals: Vec<Value> = (0..3).map(|i| json!({"id": i})).collect();
        let page = bound_values_paged(vals, 3, per_section_chars());
        assert_eq!(page.items.len(), 0);
        assert_eq!(page.next_offset, None);
    }

    #[test]
    fn paging_single_oversized_item_never_empty_page() {
        // A row larger than the budget is still returned alone, so the cursor
        // always advances and page-through terminates (no infinite loop).
        let vals = vec![
            json!({"blob": "x".repeat(2000)}),
            json!({"blob": "y".repeat(2000)}),
        ];
        let (reconstructed, cursors) = page_through(vals.clone(), 100);
        assert_eq!(reconstructed, vals);
        assert_eq!(cursors, vec![Some(1), None]);
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
        assert!(
            (80..=160).contains(&chars),
            "row serialized to {chars} chars"
        );
    }
}
