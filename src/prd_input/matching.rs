// prd_input::matching — token extraction, graph search, and match-mode
// classification for Stage 4 (prepare_prd_input).
//
// ROOT CAUSE (issue #14): the original implementation ran every
// natural-language word from the finding/feature description through
// `search::search_graph` with `min_score: 0.0` and folded every hit into
// `matched_symbols` alongside a bundle-level `verified: true` flag. A common
// English word that happens to be a SUBSTRING of an unrelated symbol name
// (e.g. "anchor" inside `_CONCRETE_ANCHOR`) scores indistinguishably from a
// genuine partial-word hit on the real target (e.g. "handle" inside
// `handle_tool_call`) — see `test_lexical_and_exact_score_identically_by_ratio`
// below, measured on this repo's own graph. A score threshold therefore
// CANNOT separate true positives from false positives; the score alone does
// not carry that information (Move 4 classification: (a) missing/wrong
// contract — the function accepted "search returned a hit" as evidence of
// relatedness when it isn't).
//
// FIX: classify every hit by its MATCH MODE, not its raw score:
//   - Verbatim  — the description cited the identifier in backticks AND the
//     graph has a symbol whose name or qualified-name tail equals it exactly.
//     Prioritized per issue #14 point 3.
//   - ExactName — a natural-language token equals a symbol's name or
//     qualified-name tail exactly (not merely contains it).
//   - Lexical   — anything else: the token is a substring/fuzzy hit only.
// Verbatim and ExactName hits are structural evidence of relatedness (the
// user or the codebase itself asserted the identifier) and are surfaced as
// `matched_symbols` (trustworthy grounding). Lexical hits carry no such
// evidence — they are surfaced separately as `candidate_symbols`, tagged
// with their raw score, and NEVER folded into `matched_symbols` or into
// impacted_communities/impacted_processes. An empty `matched_symbols` is the
// correct output when only lexical hits exist (issue #14: "a misleading
// bundle is worse than an empty one").
//
// source: stage-4 brief (token-per-word search cap); issue #14 root-cause
// discussion (this file's header); RRF citation retained in `crate::search`.

use crate::graph_store::GraphStore;
use crate::search;
use serde_json::{json, Value};

// Minimum token length — single-letter tokens ("a", "i") produce noise.
// source: Lucene StandardAnalyzer default behavior (min length 2+ for
// general-purpose text indexing). Stricter than BM25's bare minimum so the
// per-token search stays focused on symbol-like words.
pub const MIN_TOKEN_LEN: usize = 3;

// Cap token count so a pathologically-long description can't explode the
// search budget. Applied independently to natural-language tokens and to
// backtick-cited identifiers. source: stage-4 brief ("compact artifact") —
// no paper source.
pub const MAX_TOKENS: usize = 32;

// source: stage-4 brief — "top-3 matches per token" caps the cost of
// description-driven search. Not paper-backed; chosen to keep output
// compact while capturing the obvious per-token hit plus two backups.
pub const MATCHES_PER_TOKEN: usize = 3;

/// How a symbol earned its place in the output. Drives inclusion in
/// `matched_symbols` (Verbatim/ExactName) vs `candidate_symbols` (Lexical).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MatchMode {
    /// Description cited the identifier verbatim in backticks, and it
    /// resolved to an exact symbol name / qualified-name tail.
    Verbatim,
    /// A natural-language token equals the symbol's name (or qualified-name
    /// tail) exactly — case-insensitive, but not merely a substring.
    ExactName,
    /// Substring/fuzzy hit only — no exact-identity evidence.
    Lexical,
}

impl MatchMode {
    fn as_str(self) -> &'static str {
        match self {
            MatchMode::Verbatim => "verbatim",
            MatchMode::ExactName => "exact_name",
            MatchMode::Lexical => "lexical",
        }
    }

    /// Trust ordering: Verbatim > ExactName > Lexical. Used by
    /// `search_and_classify`/`upsert_best` to make sure a symbol hit by
    /// several tokens with different modes always keeps its BEST evidence,
    /// regardless of which token was processed first.
    fn rank(self) -> u8 {
        match self {
            MatchMode::Lexical => 0,
            MatchMode::ExactName => 1,
            MatchMode::Verbatim => 2,
        }
    }
}

pub struct MatchedSymbol {
    pub qualified_name: String,
    pub name: String,
    pub label: String,
    pub file_path: String,
    pub community_id: Option<String>,
    pub community_size: Option<u64>,
    pub processes: Vec<String>,
    pub calls: Vec<String>,
    pub called_by: Vec<String>,
    pub uses: Vec<String>,
    pub match_mode: MatchMode,
    /// Raw score from `search::search_graph` (RRF-fused or substring-fallback
    /// scale — the two are not on a common calibrated scale, which is
    /// exactly why `match_mode`, not this score, decides trust). Exposed so
    /// consumers can rank candidates without re-querying the graph.
    pub confidence: f64,
}

pub struct MatchOutcome {
    /// Verbatim + ExactName hits — trustworthy grounding.
    pub matched: Vec<MatchedSymbol>,
    /// Lexical-only hits — exposed for visibility, never treated as verified.
    pub candidates: Vec<MatchedSymbol>,
}

/// Extracts identifiers the description cited verbatim in backticks, e.g.
/// "the `grad_rgb` helper" → ["grad_rgb"]. Order-preserving, deduplicated,
/// capped at MAX_TOKENS. Punctuation adjacent to the identifier inside the
/// backticks (call parens, trailing commas) is stripped by `clean_token`.
pub fn extract_verbatim_identifiers(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find('`') {
        let after_open = &rest[open + 1..];
        let Some(close) = after_open.find('`') else {
            break;
        };
        let raw = &after_open[..close];
        let cleaned = clean_token(raw);
        if !cleaned.is_empty() && !out.iter().any(|t| t == &cleaned) {
            out.push(cleaned);
        }
        rest = &after_open[close + 1..];
        if out.len() >= MAX_TOKENS {
            break;
        }
    }
    out
}

/// Whitespace-split, lowercased, sanitized, deduplicated natural-language
/// tokens from a title+description pair. Tokens already captured as verbatim
/// identifiers are kept here too (dedup against verbatim happens at the
/// classification stage, not here) — this function stays a pure tokenizer.
pub fn tokenize_natural(title: &str, description: &str) -> Vec<String> {
    let combined = format!("{title} {description}");
    let mut seen: Vec<String> = Vec::new();
    for raw in combined.split_whitespace() {
        if seen.len() >= MAX_TOKENS {
            break;
        }
        let cleaned = clean_token(raw);
        if cleaned.len() < MIN_TOKEN_LEN {
            continue;
        }
        if !seen.iter().any(|t| t == &cleaned) {
            seen.push(cleaned);
        }
    }
    seen
}

pub fn clean_token(raw: &str) -> String {
    raw.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == ':')
        .collect::<String>()
        .to_ascii_lowercase()
}

/// Runs the search+classify pipeline: verbatim identifiers first (priority
/// per issue #14 point 3), then natural-language tokens.
///
/// A symbol can be hit by MULTIPLE tokens with DIFFERENT match modes — e.g.
/// the weak token "handle" (lexical, substring) and the exact token
/// "handle_tool_call" both resolve to the same qualified_name. Classifying
/// greedily on first-seen would let whichever token happens to be tokenized
/// first lock in the symbol's fate, silently downgrading a genuine exact
/// match to "already claimed as lexical" — this was a regression caught by
/// `tests::test_repro_issue14_lexical_false_positive_excluded_from_matched`'s
/// sibling `stage4_integration.rs` end-to-end test. Fix: accumulate the BEST
/// mode seen per qualified_name across every token before classifying, using
/// `MatchMode::rank` (Verbatim > ExactName > Lexical) so a later, stronger
/// hit always upgrades an earlier, weaker one and a later weaker hit never
/// downgrades an already-confirmed one.
pub fn search_and_classify(
    store: &GraphStore,
    verbatim_tokens: &[String],
    natural_tokens: &[String],
) -> MatchOutcome {
    let mut best: Vec<(search::SearchResult, MatchMode)> = Vec::new();

    for token in verbatim_tokens {
        for hit in search_hits(store, token) {
            if !is_exact_hit(token, &hit) {
                continue; // unverifiable citation — drop, not a candidate
            }
            upsert_best(&mut best, hit, MatchMode::Verbatim);
        }
    }

    for token in natural_tokens {
        for hit in search_hits(store, token) {
            let mode = if is_exact_hit(token, &hit) {
                MatchMode::ExactName
            } else {
                MatchMode::Lexical
            };
            upsert_best(&mut best, hit, mode);
        }
    }

    let mut matched: Vec<MatchedSymbol> = Vec::new();
    let mut candidates: Vec<MatchedSymbol> = Vec::new();
    for (hit, mode) in best {
        let enriched = enrich(store, hit, mode);
        if mode == MatchMode::Lexical {
            candidates.push(enriched);
        } else {
            matched.push(enriched);
        }
    }

    MatchOutcome {
        matched,
        candidates,
    }
}

/// Inserts `hit` into `best` keyed by qualified_name, keeping the
/// highest-ranked `MatchMode` seen so far for that symbol (see
/// `search_and_classify` doc comment for why this must never downgrade).
fn upsert_best(
    best: &mut Vec<(search::SearchResult, MatchMode)>,
    hit: search::SearchResult,
    mode: MatchMode,
) {
    if let Some(existing) = best
        .iter_mut()
        .find(|(h, _)| h.qualified_name == hit.qualified_name)
    {
        if mode.rank() > existing.1.rank() {
            existing.1 = mode;
        }
        return;
    }
    best.push((hit, mode));
}

fn search_hits(store: &GraphStore, token: &str) -> Vec<search::SearchResult> {
    let opts = search::SearchOptions {
        limit: MATCHES_PER_TOKEN,
        label_filter: None,
        min_score: 0.0,
    };
    search::search_graph(store, token, &opts, None).unwrap_or_default()
}

/// True iff `token` equals the hit's name or the tail of its qualified name
/// (both case-insensitive; `token` is already lowercased by `clean_token`).
/// This is INDEPENDENT of the hit's score — the substring-fallback and
/// hybrid-RRF search paths score on different, uncalibrated scales, so score
/// alone cannot distinguish exact identity from partial overlap. String
/// equality is the only signal that transfers across both paths.
fn is_exact_hit(token: &str, hit: &search::SearchResult) -> bool {
    if hit.name.to_ascii_lowercase() == token {
        return true;
    }
    let qn_tail = hit
        .qualified_name
        .rsplit("::")
        .next()
        .unwrap_or(&hit.qualified_name);
    qn_tail.to_ascii_lowercase() == token
}

fn enrich(store: &GraphStore, hit: search::SearchResult, match_mode: MatchMode) -> MatchedSymbol {
    let confidence = hit.score;
    let community_size = hit
        .community_id
        .as_ref()
        .and_then(|cid| lookup_community_size(store, cid));
    // 1-hop neighbors via get_context — reuses the SAME three-layer lookup
    // that get_context uses, so PRD consumers see exactly what the search
    // tools expose. Graceful degradation if the symbol is fuzzy-matched only.
    let (calls, called_by, uses) = match search::get_context(store, &hit.qualified_name) {
        Ok(ctx) => (
            ctx.calls.iter().map(|r| r.name.clone()).collect(),
            ctx.called_by.iter().map(|r| r.name.clone()).collect(),
            ctx.uses.iter().map(|r| r.name.clone()).collect(),
        ),
        Err(_) => (Vec::new(), Vec::new(), Vec::new()),
    };
    MatchedSymbol {
        qualified_name: hit.qualified_name,
        name: hit.name,
        label: hit.label,
        file_path: hit.file_path,
        community_id: hit.community_id,
        community_size,
        processes: hit.process_names,
        calls,
        called_by,
        uses,
        match_mode,
        confidence,
    }
}

fn lookup_community_size(store: &GraphStore, community_id: &str) -> Option<u64> {
    let escaped = community_id.replace('\'', "\\'");
    let cypher =
        format!("MATCH (c:Community) WHERE c.id = '{escaped}' RETURN c.member_count LIMIT 1");
    let qr = store.execute_query(&cypher).ok()?;
    let row = qr.rows.first()?;
    row.first().and_then(|s| s.parse::<u64>().ok())
}

pub fn matched_to_json(m: &MatchedSymbol) -> Value {
    json!({
        "qualified_name": m.qualified_name,
        "name": m.name,
        "label": m.label,
        "file_path": m.file_path,
        "community_id": m.community_id,
        "community_size": m.community_size,
        "processes": m.processes,
        "match_mode": m.match_mode.as_str(),
        "confidence": m.confidence,
        "relationships": {
            "calls": m.calls,
            "called_by": m.called_by,
            "uses": m.uses,
        }
    })
}

pub fn dedup_keys<F>(matched: &[MatchedSymbol], key: F) -> Vec<String>
where
    F: Fn(&MatchedSymbol) -> Option<String>,
{
    let mut out: Vec<String> = Vec::new();
    for m in matched {
        if let Some(k) = key(m) {
            if !k.is_empty() && !out.contains(&k) {
                out.push(k);
            }
        }
    }
    out
}

pub fn impacted_processes_from_symbols(matched: &[MatchedSymbol]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for m in matched {
        for p in &m.processes {
            if !out.contains(p) {
                out.push(p.clone());
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Tests — pure helpers that don't need a graph, plus the measured evidence
// that a score threshold cannot separate lexical false positives from
// genuine partial-word hits (the basis for the match_mode design above).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_verbatim_identifiers() {
        let text = "Use `grad_rgb` and `make_bar()` to render the gauge.";
        let ids = extract_verbatim_identifiers(text);
        assert_eq!(ids, vec!["grad_rgb".to_string(), "make_bar".to_string()]);
    }

    #[test]
    fn test_extract_verbatim_identifiers_none() {
        assert!(extract_verbatim_identifiers("no backticks here").is_empty());
    }

    #[test]
    fn test_tokenize_natural_basic() {
        let t = tokenize_natural(
            "Handle tool call should reject unknown tools",
            "The handle_tool_call function in main.rs silently returns",
        );
        assert!(t.iter().any(|x| x == "handle"));
        assert!(t.iter().any(|x| x == "tool"));
        assert!(t.iter().any(|x| x == "handle_tool_call"));
        assert!(!t.iter().any(|x| x.len() < MIN_TOKEN_LEN));
    }

    #[test]
    fn test_tokenize_natural_respects_max_tokens() {
        let description = (0..100)
            .map(|i| format!("word{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        let t = tokenize_natural("title", &description);
        assert!(t.len() <= MAX_TOKENS);
    }

    #[test]
    fn test_clean_token_strips_punctuation() {
        assert_eq!(clean_token("Foo,"), "foo");
        assert_eq!(clean_token("bar.baz"), "barbaz");
        assert_eq!(clean_token("x::y"), "x::y");
        assert_eq!(clean_token("(parens)"), "parens");
    }

    #[test]
    fn test_is_exact_hit_name_match() {
        let hit = search::SearchResult {
            qualified_name: "src/main.rs::handle_tool_call".into(),
            name: "handle_tool_call".into(),
            label: "Function".into(),
            file_path: "src/main.rs".into(),
            score: 0.5,
            community_id: None,
            process_names: vec![],
            start_line: None,
            end_line: None,
        };
        assert!(is_exact_hit("handle_tool_call", &hit));
        assert!(!is_exact_hit("handle", &hit));
    }

    // Measured evidence (2026-07-15, this repo's own scoring function
    // `search::score_candidate`'s per-term formula reproduced here at the
    // unit boundary): a genuine partial-word hit ("handle" inside
    // "handle_tool_call") and an accidental substring false positive
    // ("anchor" inside "_concrete_anchor") land on the SAME score when the
    // substring-to-name length ratio matches. This is why match_mode — not a
    // score cutoff — is the load-bearing signal: no threshold value can
    // separate these two cases because they are numerically identical.
    #[test]
    fn test_lexical_and_exact_score_identically_by_ratio() {
        fn term_score(term: &str, name_lower: &str) -> f64 {
            if name_lower == term {
                return 1.0;
            }
            let ratio = term.len() as f64 / name_lower.len() as f64;
            0.7 + 0.3 * ratio
        }
        let handle_score = term_score("handle", "handle__tool__call"); // 6 / 18
        let anchor_score = term_score("anchor", "_concrete__anchor_"); // 6 / 18
        assert!(
            (handle_score - anchor_score).abs() < 1e-9,
            "handle={handle_score} anchor={anchor_score} — expected identical scores at equal ratio"
        );
    }

    // Full reproduction of issue #14: a fixture graph carrying a symbol whose
    // name lexically contains a common English word ("anchor" inside
    // `_CONCRETE_ANCHOR`), plus the two real target symbols the original bug
    // report's description cited verbatim (`grad_rgb`, `make_bar`). No
    // reproduction, no fix (Move 4) — this is that reproduction, run against
    // the actual `search_and_classify` pipeline stage-4 uses in production.
    #[test]
    fn test_repro_issue14_lexical_false_positive_excluded_from_matched() {
        let tmp = std::env::temp_dir().join(format!("prd_input_issue14_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let src_dir = tmp.join("src");
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::write(
            src_dir.join("fixture.rs"),
            r#"
pub const _CONCRETE_ANCHOR: &str = "anchor";

pub fn grad_rgb(a: u8, b: u8) -> u8 {
    a.wrapping_add(b)
}

pub fn make_bar(v: u8) -> String {
    format!("bar-{v}")
}
"#,
        )
        .unwrap();
        let graph_dir = tmp.join("graph");
        let r = crate::indexer::index_codebase(&src_dir, &graph_dir).expect("index fixture");
        let store = GraphStore::open_or_create(&r.graph_path).unwrap();

        // Mirrors the real issue #14 report: description cites `grad_rgb`
        // verbatim and separately uses the natural word "anchor" (a pure
        // substring of _CONCRETE_ANCHOR) with no backticks.
        let title = "Gradient bar rendering";
        let description = "Continuous RGB gradient between color anchors, rendered by `grad_rgb`.";
        let combined = format!("{title} {description}");
        let verbatim = extract_verbatim_identifiers(&combined);
        let natural = tokenize_natural(title, description);
        let outcome = search_and_classify(&store, &verbatim, &natural);

        assert!(
            outcome
                .matched
                .iter()
                .any(|m| m.name == "grad_rgb" && m.match_mode == MatchMode::Verbatim),
            "grad_rgb must be matched via verbatim citation; matched={:?}",
            outcome.matched.iter().map(|m| &m.name).collect::<Vec<_>>()
        );
        assert!(
            !outcome.matched.iter().any(|m| m.name == "_CONCRETE_ANCHOR"),
            "issue #14: _CONCRETE_ANCHOR must never be verified grounding purely \
             because the description contains the substring word 'anchor'"
        );
        if let Some(c) = outcome
            .candidates
            .iter()
            .find(|m| m.name == "_CONCRETE_ANCHOR")
        {
            assert_eq!(
                c.match_mode,
                MatchMode::Lexical,
                "_CONCRETE_ANCHOR may surface as a candidate but must be tagged lexical"
            );
        }

        let _ = std::fs::remove_dir_all(&tmp);
    }

    // Complement: when nothing exact or verbatim resolves, matched_symbols
    // must be empty rather than filled with plausible-looking noise (issue
    // #14: "an empty bundle beats a misleading one").
    #[test]
    fn test_repro_issue14_pure_lexical_description_yields_empty_matched() {
        let tmp =
            std::env::temp_dir().join(format!("prd_input_issue14_empty_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let src_dir = tmp.join("src");
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::write(
            src_dir.join("fixture.rs"),
            "pub const _CONCRETE_ANCHOR: &str = \"anchor\";\n",
        )
        .unwrap();
        let graph_dir = tmp.join("graph");
        let r = crate::indexer::index_codebase(&src_dir, &graph_dir).expect("index fixture");
        let store = GraphStore::open_or_create(&r.graph_path).unwrap();

        let title = "Color gradient anchors";
        let description = "Continuous gradient rendered between color anchor points.";
        let combined = format!("{title} {description}");
        let verbatim = extract_verbatim_identifiers(&combined);
        let natural = tokenize_natural(title, description);
        let outcome = search_and_classify(&store, &verbatim, &natural);

        assert!(
            outcome.matched.is_empty(),
            "no verbatim/exact-name evidence exists — matched_symbols must be empty, got {:?}",
            outcome.matched.iter().map(|m| &m.name).collect::<Vec<_>>()
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    // Regression: a symbol can be hit by a WEAK token (lexical) before its
    // own EXACT token is processed later in tokenization order (e.g. "handle"
    // in the title, "handle_tool_call" in the description). Classifying on
    // first-seen would permanently lock the symbol in as a lexical candidate
    // and silently drop it from matched_symbols even though an exact hit
    // exists — caught by tests/stage4_integration.rs's end-to-end test.
    #[test]
    fn test_weak_token_before_exact_token_still_classifies_as_exact() {
        let tmp =
            std::env::temp_dir().join(format!("prd_input_issue14_order_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let src_dir = tmp.join("src");
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::write(
            src_dir.join("fixture.rs"),
            "pub fn handle_tool_call(name: &str) -> String { name.to_string() }\n",
        )
        .unwrap();
        let graph_dir = tmp.join("graph");
        let r = crate::indexer::index_codebase(&src_dir, &graph_dir).expect("index fixture");
        let store = GraphStore::open_or_create(&r.graph_path).unwrap();

        // "handle" (weak, lexical-only against handle_tool_call) appears in
        // the title, BEFORE the exact token "handle_tool_call" in the
        // description — tokenize_natural preserves that order.
        let title = "handle tool call should reject unknown tools";
        let description = "The handle_tool_call function must reject unknown tool names.";
        let natural = tokenize_natural(title, description);
        assert_eq!(
            natural[0], "handle",
            "test assumes 'handle' tokenizes first"
        );
        let outcome = search_and_classify(&store, &[], &natural);

        let m = outcome
            .matched
            .iter()
            .find(|m| m.name == "handle_tool_call")
            .expect("handle_tool_call must end up in matched_symbols despite the earlier weak 'handle' hit");
        assert_eq!(m.match_mode, MatchMode::ExactName);
        assert!(
            !outcome
                .candidates
                .iter()
                .any(|c| c.name == "handle_tool_call"),
            "handle_tool_call must not ALSO appear as a downgraded candidate"
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
