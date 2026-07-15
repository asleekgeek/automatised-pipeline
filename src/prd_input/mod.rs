// prd_input — Stage 4: bundle verified finding + graph intel into a single
// artifact for the PRD generator (TypeScript) to consume.
//
// Read-only with respect to the graph. Writes one JSON artifact under
// <output_dir>/runs/<run_id>/findings/<finding_id>/stage-4.prd_input.json
// and updates <output_dir>/runs/<run_id>/index.json with stage4 markers.
//
// Pipeline:
//   1. Load stage-2.verified.json   (must have verified:true) — finding mode
//      only; feature mode has no stage-2 gate (see `matching` module note).
//   2. Load stage-1.refined.json    (for title/description — the verified
//      receipt intentionally omits the finding body per stage-2.md §5.3)
//   3. Extract backtick-verbatim identifiers + tokenize the remaining text
//      (see `matching` module for the exact/lexical classification that
//      fixes issue #14's false-positive grounding).
//   4. Search + classify → `matched_symbols` (verbatim/exact-name, verified
//      grounding) and `candidate_symbols` (lexical-only, exposed with score,
//      never treated as verified).
//   5. For each matched/candidate symbol load 1-hop context (community,
//      procs, calls, called_by, uses).
//   6. Write artifact + update index.
//
// source: stages/stage-2.md §5.3 (verified schema), stage-1.md §4.2
// (refined schema). The symbol-search-from-description pattern is the
// explicit spec for stage 4 (see the architect's brief embedded in
// docs/stage-4-spec when it lands). Match-mode classification: issue #14
// root-cause discussion, see `matching` module doc comment.

mod artifact;
mod matching;

use crate::graph_store::GraphStore;
use artifact::{build_artifact, ArtifactInputs};
use matching::MatchOutcome;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

// source: stage-4 brief — preparer schema version. "1.0.0" = first release.
// Bumped to "1.1.0" by issue #14: matched_symbols now carries match_mode +
// confidence (additive fields, old consumers unaffected), and a new
// candidate_symbols array separates lexical-only hits from verified
// grounding. No field was removed or renamed — additive/backward-compatible.
pub const PREPARER_VERSION: &str = "1.1.0";

// source: stage-4 brief — artifact filename (mirrors stage-1/2 conventions).
pub const PRD_INPUT_FILE_NAME: &str = "stage-4.prd_input.json";

// ---------------------------------------------------------------------------
// Public result types
// ---------------------------------------------------------------------------

/// Outcome of a successful prepare_prd_input run. Drives the MCP receipt.
pub struct PrdInputOutcome {
    pub artifact_path: PathBuf,
    /// Count of trustworthy (verbatim/exact-name) matches — the same set
    /// written to `prd_context.matched_symbols`.
    pub matched_symbol_count: usize,
    /// Count of lexical-only hits — the same set written to
    /// `prd_context.candidate_symbols`. Never counted as verified grounding.
    pub candidate_symbol_count: usize,
    pub impacted_community_count: usize,
    pub impacted_process_count: usize,
    /// The grounding payload (matched_symbols / candidate_symbols /
    /// impacted_communities / impacted_processes / graph_stats) returned
    /// INLINE so MCP consumers (the PRD generator) get the grounding from
    /// the tool response without a second file read. Same object that is
    /// also persisted in the artifact.
    pub prd_context: Value,
}

/// Arguments already validated by the handler in main.rs.
///
/// Two modes:
///  * Finding mode  — `finding_id: Some` → bundle a VERIFIED stage-2 finding
///    (the original Stage-4 contract; enforces the verified gate).
///  * Feature mode  — `finding_id: None` + `feature_description: Some` → ground
///    a free-text feature directly on the code graph (no stage-2 gate), so the
///    PRD generator can prepare input from intent alone. Same search/enrich
///    path; writes under `runs/<run_id>/features/<slug>/`.
pub struct PrdInputArgs {
    pub run_id: String,
    pub finding_id: Option<String>,
    pub feature_description: Option<String>,
    pub output_dir: PathBuf,
    pub graph_path: PathBuf,
}

// ---------------------------------------------------------------------------
// Orchestration
// ---------------------------------------------------------------------------

/// Resolves the working dir + summary + verified receipt for whichever mode
/// `args` selects. Both modes converge on the SAME search/enrich/artifact
/// path in `prepare` below — this function's only job is to produce the
/// three pieces that path needs, however each mode obtains them.
fn resolve_mode(
    args: &PrdInputArgs,
    prepared_at: &str,
) -> Result<(PathBuf, FindingSummary, VerifiedReceipt), String> {
    match (&args.finding_id, &args.feature_description) {
        (Some(_), _) => {
            // Finding mode — enforce the verified stage-2 gate.
            let finding_dir = finding_dir_for(args);
            let verified = load_verified(&finding_dir)?;
            let summary = load_finding_summary(&finding_dir)?;
            Ok((finding_dir, summary, verified))
        }
        (None, Some(desc)) => {
            // Feature mode — synthesize a summary from intent, no stage-2 gate.
            let desc = desc.trim();
            if desc.is_empty() {
                return Err("feature_description_empty".into());
            }
            let summary = synth_summary_from_description(desc);
            let verified = VerifiedReceipt {
                finalized_at: prepared_at.to_string(),
                stage1_refined_path: String::new(),
                verified: true,
            };
            Ok((
                feature_dir_for(args, &summary.finding_id),
                summary,
                verified,
            ))
        }
        (None, None) => Err(
            "prepare_prd_input: provide either finding_id (finding mode) \
             or feature_description (feature mode)"
                .into(),
        ),
    }
}

/// Runs stage 4 end to end (finding mode OR feature-description mode).
pub fn prepare(args: &PrdInputArgs, prepared_at: String) -> Result<PrdInputOutcome, String> {
    let (out_dir, summary, verified) = resolve_mode(args, &prepared_at)?;

    let store = GraphStore::open_or_create(&args.graph_path)?;
    let stats = collect_graph_stats(&store);
    let combined_text = format!("{} {}", summary.title, summary.description);
    let verbatim_tokens = matching::extract_verbatim_identifiers(&combined_text);
    let natural_tokens = matching::tokenize_natural(&summary.title, &summary.description);
    let MatchOutcome {
        matched,
        candidates,
    } = matching::search_and_classify(&store, &verbatim_tokens, &natural_tokens);
    // Impacted communities/processes are derived ONLY from trustworthy
    // matches — folding in lexical-only candidates would leak the same
    // false-positive grounding into the impact-analysis fields (issue #14).
    let impacted_communities = matching::dedup_keys(&matched, |s| s.community_id.clone());
    let impacted_processes = matching::impacted_processes_from_symbols(&matched);
    let artifact = build_artifact(&ArtifactInputs {
        args,
        verified: &verified,
        summary: &summary,
        prepared_at: &prepared_at,
        matched: &matched,
        candidates: &candidates,
        impacted_communities: &impacted_communities,
        impacted_processes: &impacted_processes,
        stats: &stats,
    });
    let artifact_path = out_dir.join(PRD_INPUT_FILE_NAME);
    write_json(&artifact_path, &artifact)?;
    // index.json is a finding-run artifact; only update it in finding mode.
    if args.finding_id.is_some() {
        update_index(args, &prepared_at)?;
    }
    let prd_context = artifact.get("prd_context").cloned().unwrap_or(Value::Null);
    Ok(PrdInputOutcome {
        artifact_path,
        matched_symbol_count: matched.len(),
        candidate_symbol_count: candidates.len(),
        impacted_community_count: impacted_communities.len(),
        impacted_process_count: impacted_processes.len(),
        prd_context,
    })
}

fn finding_dir_for(args: &PrdInputArgs) -> PathBuf {
    args.output_dir
        .join("runs")
        .join(&args.run_id)
        .join("findings")
        .join(args.finding_id.as_deref().unwrap_or("unknown"))
}

/// Feature-mode working dir: runs/<run_id>/features/<slug>/.
fn feature_dir_for(args: &PrdInputArgs, slug: &str) -> PathBuf {
    args.output_dir
        .join("runs")
        .join(&args.run_id)
        .join("features")
        .join(slug)
}

/// Builds a synthetic finding summary from a free-text feature description.
/// finding_id is a stable slug of the description; title is its first line.
fn synth_summary_from_description(desc: &str) -> FindingSummary {
    let title = desc.lines().next().unwrap_or(desc).trim();
    let title = if title.len() > 80 {
        &title[..80]
    } else {
        title
    };
    FindingSummary {
        finding_id: slugify(desc),
        title: title.to_string(),
        description: desc.to_string(),
        relevance_category: "feature".to_string(),
    }
}

/// Filesystem-safe, deterministic slug for a feature description.
fn slugify(text: &str) -> String {
    let mut slug: String = text
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    while slug.contains("--") {
        slug = slug.replace("--", "-");
    }
    let slug = slug.trim_matches('-');
    let slug: String = slug.chars().take(48).collect();
    if slug.is_empty() {
        "feature".to_string()
    } else {
        format!("feature-{slug}")
    }
}

// ---------------------------------------------------------------------------
// Stage-2 verified loader — enforces `verified: true` gate
// ---------------------------------------------------------------------------

struct VerifiedReceipt {
    finalized_at: String,
    stage1_refined_path: String,
    verified: bool,
}

fn load_verified(finding_dir: &Path) -> Result<VerifiedReceipt, String> {
    let path = finding_dir.join("stage-2.verified.json");
    if !path.exists() {
        return Err("stage_2_not_verified: stage-2.verified.json missing".into());
    }
    let raw = fs::read_to_string(&path)
        .map_err(|e| format!("stage_2_not_verified: read {:?}: {}", path, e))?;
    let v: Value = serde_json::from_str(&raw)
        .map_err(|e| format!("stage_2_not_verified: parse {:?}: {}", path, e))?;
    let verified = v.get("verified").and_then(|x| x.as_bool()).unwrap_or(false);
    if !verified {
        return Err("stage_2_not_verified: verified flag is false".into());
    }
    Ok(VerifiedReceipt {
        finalized_at: v
            .get("finalized_at")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        stage1_refined_path: v
            .get("stage1_refined_path")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        verified,
    })
}

// ---------------------------------------------------------------------------
// Stage-1 refined loader — pulls title/description for summary+tokenization
// ---------------------------------------------------------------------------

struct FindingSummary {
    finding_id: String,
    title: String,
    description: String,
    relevance_category: String,
}

fn load_finding_summary(finding_dir: &Path) -> Result<FindingSummary, String> {
    let path = finding_dir.join("stage-1.refined.json");
    if !path.exists() {
        return Err(format!(
            "stage_1_refined_missing: {} not found",
            path.display()
        ));
    }
    let raw =
        fs::read_to_string(&path).map_err(|e| format!("stage_1_refined_unreadable: {}", e))?;
    let v: Value =
        serde_json::from_str(&raw).map_err(|e| format!("stage_1_refined_corrupt: {}", e))?;
    let extracted = v.get("extracted").cloned().unwrap_or(Value::Null);
    let finding_id = str_field(&extracted, "finding_id");
    let title = str_field(&extracted, "title");
    let description = str_field(&extracted, "description");
    let relevance_category = str_field(&extracted, "relevance_category");
    Ok(FindingSummary {
        finding_id,
        title,
        description,
        relevance_category,
    })
}

fn str_field(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string()
}

// ---------------------------------------------------------------------------
// Graph stats — cheap sanity counts for the PRD generator
// ---------------------------------------------------------------------------

struct GraphStats {
    nodes: u64,
    edges: u64,
    communities: u64,
    processes: u64,
}

fn collect_graph_stats(store: &GraphStore) -> GraphStats {
    let nodes = store.node_count().unwrap_or(0);
    let edges = store.edge_count().unwrap_or(0);
    let communities = count_label(store, "Community");
    let processes = count_label(store, "Process");
    GraphStats {
        nodes,
        edges,
        communities,
        processes,
    }
}

fn count_label(store: &GraphStore, label: &str) -> u64 {
    let cypher = format!("MATCH (n:{label}) RETURN count(n)");
    match store.execute_query(&cypher) {
        Ok(qr) => qr
            .rows
            .first()
            .and_then(|r| r.first())
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0),
        Err(_) => 0,
    }
}

// ---------------------------------------------------------------------------
// Filesystem writes
// ---------------------------------------------------------------------------

fn write_json(path: &Path, value: &Value) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir {:?}: {}", parent, e))?;
    }
    let bytes = serde_json::to_vec_pretty(value).map_err(|e| format!("serialize: {}", e))?;
    fs::write(path, bytes).map_err(|e| format!("write {:?}: {}", path, e))?;
    Ok(())
}

/// Updates `<output_dir>/runs/<run_id>/index.json` with stage-4 markers.
/// Preserves all existing fields — stage-4 only appends two top-level keys.
fn update_index(args: &PrdInputArgs, prepared_at: &str) -> Result<(), String> {
    let index_path = args
        .output_dir
        .join("runs")
        .join(&args.run_id)
        .join("index.json");
    if !index_path.exists() {
        return Ok(());
    }
    let raw = fs::read_to_string(&index_path).map_err(|e| format!("read index: {}", e))?;
    let mut v: Value = serde_json::from_str(&raw).map_err(|e| format!("parse index: {}", e))?;
    if let Some(obj) = v.as_object_mut() {
        obj.insert(
            "stage4_prepared_at".into(),
            Value::String(prepared_at.to_string()),
        );
        let rel = format!(
            "findings/{}/{}",
            args.finding_id.as_deref().unwrap_or("unknown"),
            PRD_INPUT_FILE_NAME
        );
        obj.insert("stage4_path".into(), Value::String(rel));
    }
    let bytes = serde_json::to_vec_pretty(&v).map_err(|e| format!("serialize index: {}", e))?;
    fs::write(&index_path, bytes).map_err(|e| format!("write index: {}", e))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests — pure helpers that don't need a graph
// ---------------------------------------------------------------------------

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
