// history — temporal layer: git commits + a per-entity version spine.
//
// Walks `git log` for commit metadata and ancestry, then for each commit maps
// its diff to the entities (File + symbols) that changed and records a
// `Version` node per (entity, commit), chained by `PreviousVersion`. The
// structural graph stays the HEAD snapshot; this layer hangs off it so every
// entity is traversable across time in both directions:
//
//   entity  <-VersionOf-  Version  -ChangedIn->  Commit  -PreviousVersion->  Commit
//
// and the reverse (a commit's changed entities; a version's successor).
//
// Source: second-brain history requirement — full symbol-level versioning.
// Symbol attribution maps a commit's changed lines onto the CURRENT graph's
// symbol ranges (exact for files, which are stable path keys; best-effort for
// symbols on older commits, where line numbers have drifted). This is stated
// so a consumer reads symbol-level history as "commits that touched the lines
// this symbol now occupies," not a perfect per-revision AST.

mod persist;

use crate::git_diff::parse_unified_diff;
use crate::graph_store::{cypher_str, GraphStore};
use persist::{
    load_file_ids, persist_changed_in, persist_commit_lineage, persist_commits,
    persist_prev_version, persist_version_of, persist_versions,
};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::Command;

// source: heuristic — bounds git work per call. The full ancestry of a large
// repo is unbounded; 200 commits is deep enough for "recent history" queries
// while keeping per-commit `git show` + graph mapping affordable. Tunable via
// the tool argument.
const DEFAULT_MAX_COMMITS: usize = 200;

// ASCII Unit Separator (0x1F) — a control char that never appears in commit
// metadata, so it is a safe in-record field delimiter for the git log format.
const GIT_FIELD_SEP: char = '\u{1f}';

// Symbol labels with line ranges that we attribute commits to. Mirrors the
// VersionOf_Version_<Label> tables declared in graph_store::REL_TABLES.
const SYMBOL_LABELS_WITH_LINES: &[&str] = &["Function", "Struct", "Enum", "Trait"];

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

pub struct HistoryResult {
    pub commits: u64,
    pub versions: u64,
    pub commit_edges: u64,
    pub version_edges: u64,
}

impl HistoryResult {
    fn empty() -> Self {
        HistoryResult { commits: 0, versions: 0, commit_edges: 0, version_edges: 0 }
    }
}

// ---------------------------------------------------------------------------
// Internal types
// ---------------------------------------------------------------------------

struct CommitMeta {
    sha: String,
    author: String,
    email: String,
    committed_at: i64,
    parents: Vec<String>,
    message: String,
}

/// An entity (File or symbol) that a commit changed, identified by the
/// graph node's `id` so a VersionOf edge can match it directly.
struct ChangedEntity {
    entity_id: String,
    qualified_name: String,
    label: String,
    change_type: String,
    lines_changed: u64,
}

struct SymRange {
    id: String,
    qn: String,
    label: &'static str,
    start: u64,
    end: u64,
}

struct VersionRow {
    id: String,
    entity: ChangedEntity,
    commit_sha: String,
    committed_at: i64,
}

#[derive(Default)]
struct Collected {
    versions: Vec<VersionRow>,
    changed_in: Vec<(String, String)>,                  // (version_id, commit_sha)
    version_of: HashMap<String, Vec<(String, String)>>, // label -> (version_id, entity_id)
    prev_version: Vec<(String, String)>,                // (version_id, prev_version_id)
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Ingests up to `max_commits` of git history (newest first) into the graph
/// at `store`, which must already be indexed (it is the HEAD snapshot the
/// version spine attaches to). Idempotency is the caller's concern: running
/// twice double-inserts, so call once per freshly built graph.
pub fn index_history(
    store: &GraphStore,
    codebase_path: &Path,
    max_commits: Option<usize>,
) -> Result<HistoryResult, String> {
    let limit = max_commits.unwrap_or(DEFAULT_MAX_COMMITS);
    let commits = read_git_log(codebase_path, limit)?;
    if commits.is_empty() {
        return Ok(HistoryResult::empty());
    }
    let prefix = repo_root_prefix(codebase_path)?;
    let known_files = load_file_ids(store)?;

    let n_commits = persist_commits(store, &commits)?;
    let commit_edges = persist_commit_lineage(store, &commits)?;

    let collected =
        collect_versions(store, codebase_path, &commits, &prefix, &known_files)?;
    let n_versions = persist_versions(store, &collected.versions)?;
    let mut version_edges = persist_changed_in(store, &collected.changed_in)?;
    version_edges += persist_version_of(store, &collected.version_of)?;
    version_edges += persist_prev_version(store, &collected.prev_version)?;

    Ok(HistoryResult {
        commits: n_commits,
        versions: n_versions,
        commit_edges,
        version_edges,
    })
}

// ---------------------------------------------------------------------------
// git log — commit metadata + ancestry
// ---------------------------------------------------------------------------

fn read_git_log(codebase_path: &Path, limit: usize) -> Result<Vec<CommitMeta>, String> {
    // %H sha, %an author, %ae email, %at author-date unix, %P parents, %s subject.
    // %x1f is the field separator; %s is single-line so each commit is one line.
    let fmt = format!(
        "--pretty=format:%H{s}%an{s}%ae{s}%at{s}%P{s}%s",
        s = "%x1f"
    );
    let output = Command::new("git")
        .arg("-C").arg(codebase_path)
        .arg("log").arg("--no-color")
        .arg("-n").arg(limit.to_string())
        .arg(fmt)
        .output()
        .map_err(|e| format!("failed to run git log: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // An empty repo ("does not have any commits yet") is not an error here.
        if stderr.contains("does not have any commits") {
            return Ok(Vec::new());
        }
        return Err(format!("git log failed: {stderr}"));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    Ok(text.lines().filter_map(parse_log_line).collect())
}

fn parse_log_line(line: &str) -> Option<CommitMeta> {
    let mut f = line.split(GIT_FIELD_SEP);
    let sha = f.next()?.trim().to_string();
    if sha.is_empty() {
        return None;
    }
    let author = f.next()?.to_string();
    let email = f.next()?.to_string();
    let committed_at = f.next()?.trim().parse::<i64>().ok()?;
    let parents: Vec<String> =
        f.next()?.split_whitespace().map(|s| s.to_string()).collect();
    let message = f.next().unwrap_or("").to_string();
    Some(CommitMeta { sha, author, email, committed_at, parents, message })
}

// ---------------------------------------------------------------------------
// Path reconciliation — git emits repo-root-relative paths; File node ids are
// relative to the indexed codebase_path. Compute and strip the offset.
// ---------------------------------------------------------------------------

fn repo_root_prefix(codebase_path: &Path) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C").arg(codebase_path)
        .arg("rev-parse").arg("--show-toplevel")
        .output()
        .map_err(|e| format!("failed to run git rev-parse: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "not a git repository: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let root = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let cp = codebase_path
        .canonicalize()
        .map_err(|e| format!("canonicalize {codebase_path:?}: {e}"))?;
    let rel = cp
        .strip_prefix(Path::new(&root))
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    Ok(rel)
}

/// Converts a repo-root-relative git path to a File node id (indexed-root
/// relative) by stripping the offset prefix. Returns None when the path is
/// outside the indexed subtree.
fn strip_prefix(git_path: &str, prefix: &str) -> Option<String> {
    if prefix.is_empty() {
        return Some(git_path.to_string());
    }
    git_path
        .strip_prefix(&format!("{prefix}/"))
        .map(|s| s.to_string())
}

// ---------------------------------------------------------------------------
// Per-commit entity mapping
// ---------------------------------------------------------------------------

fn collect_versions(
    store: &GraphStore,
    codebase_path: &Path,
    commits: &[CommitMeta],
    prefix: &str,
    known_files: &HashSet<String>,
) -> Result<Collected, String> {
    let mut c = Collected::default();
    let mut last_version: HashMap<String, String> = HashMap::new();
    let mut cache: HashMap<String, Vec<SymRange>> = HashMap::new();

    // Oldest-first so each entity's PreviousVersion chain builds forward.
    for commit in commits.iter().rev() {
        // Merge commits produce combined diffs the line parser does not model;
        // the commit + its lineage are still recorded, just not entity changes.
        if commit.parents.len() > 1 {
            continue;
        }
        let entities =
            changed_entities(store, codebase_path, commit, prefix, known_files, &mut cache)?;
        for e in entities {
            let vid = format!("{}@{}", e.entity_id, commit.sha);
            if let Some(prev) = last_version.get(&e.entity_id) {
                c.prev_version.push((vid.clone(), prev.clone()));
            }
            last_version.insert(e.entity_id.clone(), vid.clone());
            c.changed_in.push((vid.clone(), commit.sha.clone()));
            c.version_of
                .entry(e.label.clone())
                .or_default()
                .push((vid.clone(), e.entity_id.clone()));
            c.versions.push(VersionRow {
                id: vid,
                commit_sha: commit.sha.clone(),
                committed_at: commit.committed_at,
                entity: e,
            });
        }
    }
    Ok(c)
}

fn changed_entities(
    store: &GraphStore,
    codebase_path: &Path,
    commit: &CommitMeta,
    prefix: &str,
    known_files: &HashSet<String>,
    cache: &mut HashMap<String, Vec<SymRange>>,
) -> Result<Vec<ChangedEntity>, String> {
    let diff = git_show_diff(codebase_path, &commit.sha)?;
    let hunks = parse_unified_diff(&diff)?;
    let mut entities = Vec::new();
    for hunk in &hunks {
        let Some(file_id) = strip_prefix(&hunk.file_path, prefix) else {
            continue;
        };
        // Only attribute changes to entities that exist in the graph, so
        // Version nodes never dangle without a VersionOf target.
        if !known_files.contains(&file_id) {
            continue;
        }
        let ct = change_type(hunk.is_new, hunk.is_deleted);
        entities.push(ChangedEntity {
            entity_id: file_id.clone(),
            qualified_name: file_id.clone(),
            label: "File".to_string(),
            change_type: ct.to_string(),
            lines_changed: hunk.changed_lines.len() as u64,
        });
        let ranges = cache
            .entry(file_id.clone())
            .or_insert_with(|| file_symbol_ranges(store, &file_id));
        entities.extend(overlapping(
            ranges, &hunk.changed_lines, hunk.is_new, hunk.is_deleted, ct,
        ));
    }
    Ok(entities)
}

fn change_type(is_new: bool, is_deleted: bool) -> &'static str {
    if is_new {
        "added"
    } else if is_deleted {
        "deleted"
    } else {
        "modified"
    }
}

/// Loads every symbol with a line range defined in `file_path`, keyed for
/// overlap testing. Functions/Structs/Enums/Traits are File-defined directly;
/// Methods are reached through their container (Struct/Enum/Trait).
fn file_symbol_ranges(store: &GraphStore, file_path: &str) -> Vec<SymRange> {
    let esc = cypher_str(file_path);
    let mut out = Vec::new();
    for label in SYMBOL_LABELS_WITH_LINES {
        let q = format!(
            "MATCH (f:File)-[:Defines_File_{label}]->(n:{label}) \
             WHERE f.path = {esc} \
             RETURN n.id, n.qualified_name, n.start_line, n.end_line"
        );
        push_ranges(store, &q, label, &mut out);
    }
    for container in &["Struct", "Enum", "Trait"] {
        let q = format!(
            "MATCH (f:File)-[:Defines_File_{container}]->(c)\
             -[:HasMethod_{container}_Method]->(m:Method) \
             WHERE f.path = {esc} \
             RETURN m.id, m.qualified_name, m.start_line, m.end_line"
        );
        push_ranges(store, &q, "Method", &mut out);
    }
    out
}

fn push_ranges(store: &GraphStore, cypher: &str, label: &'static str, out: &mut Vec<SymRange>) {
    let qr = match store.execute_query(cypher) {
        Ok(q) => q,
        Err(_) => return,
    };
    for row in &qr.rows {
        if row.len() < 4 {
            continue;
        }
        let start: u64 = row[2].parse().unwrap_or(0);
        let end: u64 = row[3].parse().unwrap_or(0);
        out.push(SymRange {
            id: row[0].clone(),
            qn: row[1].clone(),
            label,
            start,
            end,
        });
    }
}

fn overlapping(
    ranges: &[SymRange],
    changed: &[u64],
    is_new: bool,
    is_deleted: bool,
    ct: &str,
) -> Vec<ChangedEntity> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for r in ranges {
        let overlap = changed.iter().filter(|&&l| l >= r.start && l <= r.end).count() as u64;
        if overlap == 0 && !is_new && !is_deleted {
            continue;
        }
        if !seen.insert(r.id.clone()) {
            continue;
        }
        let lines_changed = if is_new || is_deleted {
            r.end.saturating_sub(r.start) + 1
        } else {
            overlap
        };
        out.push(ChangedEntity {
            entity_id: r.id.clone(),
            qualified_name: r.qn.clone(),
            label: r.label.to_string(),
            change_type: ct.to_string(),
            lines_changed,
        });
    }
    out
}

fn git_show_diff(codebase_path: &Path, sha: &str) -> Result<String, String> {
    validate_sha(sha)?;
    let output = Command::new("git")
        .arg("-C").arg(codebase_path)
        .arg("show").arg(sha)
        .arg("--no-color").arg("--format=").arg("--unified=3")
        .output()
        .map_err(|e| format!("failed to run git show: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "git show {sha} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// A commit sha from our own `git log` is trusted, but validate before passing
/// to `git show` as defense in depth: hex digits only, plausible length.
/// source: mirrors git_diff::validate_git_ref's argument-injection posture.
fn validate_sha(sha: &str) -> Result<(), String> {
    if sha.len() < 4 || sha.len() > 64 {
        return Err(format!("invalid_sha: implausible length ({})", sha.len()));
    }
    if !sha.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("invalid_sha: non-hex characters in {sha:?}"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_log_line() {
        let line = "abc123\u{1f}Ada Lovelace\u{1f}ada@x.org\u{1f}1700000000\u{1f}def456 789aaa\u{1f}fix: don't crash";
        let c = parse_log_line(line).expect("must parse");
        assert_eq!(c.sha, "abc123");
        assert_eq!(c.author, "Ada Lovelace");
        assert_eq!(c.email, "ada@x.org");
        assert_eq!(c.committed_at, 1_700_000_000);
        assert_eq!(c.parents, vec!["def456".to_string(), "789aaa".to_string()]);
        assert_eq!(c.message, "fix: don't crash");
    }

    #[test]
    fn test_parse_log_line_root_commit_no_parents() {
        let line = "root1\u{1f}A\u{1f}a@x\u{1f}1700000000\u{1f}\u{1f}initial";
        let c = parse_log_line(line).expect("must parse");
        assert!(c.parents.is_empty());
        assert_eq!(c.message, "initial");
    }

    #[test]
    fn test_strip_prefix() {
        assert_eq!(strip_prefix("src/main.rs", ""), Some("src/main.rs".to_string()));
        assert_eq!(strip_prefix("sub/src/a.rs", "sub"), Some("src/a.rs".to_string()));
        assert_eq!(strip_prefix("other/a.rs", "sub"), None);
    }

    #[test]
    fn test_validate_sha() {
        assert!(validate_sha("abc123").is_ok());
        assert!(validate_sha("deadbeef").is_ok());
        assert!(validate_sha("xyz").is_err());           // too short
        assert!(validate_sha("not-a-sha-zz").is_err());  // non-hex
    }
}
