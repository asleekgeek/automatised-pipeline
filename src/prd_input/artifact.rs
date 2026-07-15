// prd_input::artifact — builds the stage-4 JSON artifact (envelope +
// prd_context grounding payload) from already-resolved mode state and
// already-classified matched/candidate symbols. Pure; no I/O, no search.
//
// Split out of prd_input::mod (orchestration) so the "how do we build the
// artifact" question lives separately from "how do we get the inputs to
// build it" (coding-standards §4.1/§4.2 — this file plus mod.rs were one
// 661-line file before issue #14's matching-module split, and mod.rs alone
// grew back past the 500-line cap once match_mode/candidate_symbols/
// resolve_mode landed; this second split is the review's follow-up).

use super::matching::{self, MatchedSymbol};
use super::{FindingSummary, GraphStats, PrdInputArgs, VerifiedReceipt, PREPARER_VERSION};
use serde_json::{json, Value};

/// Parameter object for `build_artifact` — coding-standards §4.4 caps
/// function parameters at 4; this pure builder needs every one of these
/// pieces, so they're grouped into a single borrowed struct instead of nine
/// positional arguments.
#[derive(Clone, Copy)]
pub(super) struct ArtifactInputs<'a> {
    pub(super) args: &'a PrdInputArgs,
    pub(super) verified: &'a VerifiedReceipt,
    pub(super) summary: &'a FindingSummary,
    pub(super) prepared_at: &'a str,
    /// Verbatim/exact-name evidence — verified grounding.
    pub(super) matched: &'a [MatchedSymbol],
    /// Lexical-only hits, kept separate so a consumer can never mistake a
    /// substring coincidence for confirmed relatedness (issue #14).
    pub(super) candidates: &'a [MatchedSymbol],
    pub(super) impacted_communities: &'a [String],
    pub(super) impacted_processes: &'a [String],
    pub(super) stats: &'a GraphStats,
}

/// Computes the mode-dependent envelope fields: the effective finding id
/// (the real finding in finding mode, or the synthetic feature slug in
/// feature mode — stage-2 path only exists in finding mode), the relative
/// stage-2 path, and the mode label. Split out of `build_artifact` so the
/// "which mode produced this" question is answered in one small, reviewable
/// place (coding-standards §4.2).
fn artifact_mode_fields(
    args: &PrdInputArgs,
    summary: &FindingSummary,
) -> (String, String, &'static str) {
    let effective_finding_id = args
        .finding_id
        .clone()
        .unwrap_or_else(|| summary.finding_id.clone());
    let stage2_rel = if args.finding_id.is_some() {
        format!("findings/{}/stage-2.verified.json", effective_finding_id)
    } else {
        String::new()
    };
    let mode = if args.finding_id.is_some() {
        "finding"
    } else {
        "feature"
    };
    (effective_finding_id, stage2_rel, mode)
}

/// Parameter object for `build_prd_context` (same §4.4 rationale as
/// `ArtifactInputs`: more than 4 pieces of borrowed state, grouped instead
/// of positional).
struct PrdContextInputs<'a> {
    summary: &'a FindingSummary,
    verified: &'a VerifiedReceipt,
    matched: &'a [MatchedSymbol],
    candidates: &'a [MatchedSymbol],
    impacted_communities: &'a [String],
    impacted_processes: &'a [String],
    stats: &'a GraphStats,
}

/// Builds the `prd_context` grounding payload — everything the PRD generator
/// actually consumes (summary text, matched/candidate symbols, impact
/// fields, graph stats). Split out of `build_artifact` so the artifact
/// envelope (mode metadata, run/finding ids) and the grounding payload are
/// each independently reviewable (coding-standards §4.2).
fn build_prd_context(inputs: PrdContextInputs) -> Value {
    let PrdContextInputs {
        summary,
        verified,
        matched,
        candidates,
        impacted_communities,
        impacted_processes,
        stats,
    } = inputs;
    let matched_symbols: Vec<Value> = matched.iter().map(matching::matched_to_json).collect();
    let candidate_symbols: Vec<Value> = candidates.iter().map(matching::matched_to_json).collect();
    let summary_text = if summary.description.is_empty() {
        summary.title.clone()
    } else {
        format!("{} — {}", summary.title, summary.description)
    };

    json!({
        "finding_summary": summary_text,
        "finding_title": summary.title,
        "relevance_category": summary.relevance_category,
        "finalized_at": verified.finalized_at,
        "stage1_refined_path": verified.stage1_refined_path,
        // NOTE (issue #14): this flag reports whether the SOURCE FINDING
        // passed the stage-2 verification gate (finding mode) or is a
        // synthesized feature request (feature mode, always true here).
        // It says nothing about the reliability of individual
        // `matched_symbols` entries — that is now carried per-symbol by
        // `match_mode`/`confidence`, and lexical-only hits are excluded
        // from `matched_symbols` entirely (see `candidate_symbols`).
        "verified": verified.verified,
        "finding_id": summary.finding_id,
        "matched_symbols": matched_symbols,
        // Lexical-only hits (issue #14): substring/fuzzy matches with no
        // exact-identity evidence. Exposed with `match_mode: "lexical"`
        // and a raw `confidence` score for visibility, but deliberately
        // NOT folded into `matched_symbols` or into the impact fields
        // below — an empty array here (or an empty matched_symbols) is
        // the correct output when nothing can be verified, per issue #14
        // ("a misleading bundle is worse than an empty one").
        "candidate_symbols": candidate_symbols,
        "impacted_communities": impacted_communities,
        "impacted_processes": impacted_processes,
        "graph_stats": graph_stats_json(stats),
    })
}

/// Builds the `graph_stats` sub-object. Split out of `build_prd_context` to
/// keep that function under the coding-standards §4.2 50-line cap.
fn graph_stats_json(stats: &GraphStats) -> Value {
    json!({
        "nodes": stats.nodes,
        "edges": stats.edges,
        "communities": stats.communities,
        "processes": stats.processes,
    })
}

/// Bundles matched/candidate symbols and impact fields into the stage-4
/// artifact. Pure; no I/O.
pub(super) fn build_artifact(inputs: &ArtifactInputs) -> Value {
    let ArtifactInputs {
        args,
        verified,
        summary,
        prepared_at,
        matched,
        candidates,
        impacted_communities,
        impacted_processes,
        stats,
    } = *inputs;
    let (effective_finding_id, stage2_rel, mode) = artifact_mode_fields(args, summary);
    let prd_context = build_prd_context(PrdContextInputs {
        summary,
        verified,
        matched,
        candidates,
        impacted_communities,
        impacted_processes,
        stats,
    });

    json!({
        "run_id": args.run_id,
        "finding_id": effective_finding_id,
        "mode": mode,
        "stage2_verified_path": stage2_rel,
        "graph_path": args.graph_path.to_string_lossy(),
        "prepared_at": prepared_at,
        "prd_context": prd_context,
        "preparer_version": PREPARER_VERSION,
    })
}
