// history::persist — graph writes for the temporal layer.
//
// The only part of the history module that touches the bulk-insert path.
// Takes the collected commit metadata and version rows and writes Commit /
// Version nodes plus the lineage, ChangedIn, VersionOf, and PreviousVersion
// edges. Separated from mapping/git so each file owns one concern.

use super::{CommitMeta, VersionRow};
use crate::graph_store::GraphStore;
use std::collections::HashMap;
use std::collections::HashSet;

pub(super) fn load_file_ids(store: &GraphStore) -> Result<HashSet<String>, String> {
    let qr = store.execute_query("MATCH (f:File) RETURN f.id")?;
    Ok(qr.rows.into_iter().filter_map(|mut r| r.drain(..).next()).collect())
}

pub(super) fn persist_commits(store: &GraphStore, commits: &[CommitMeta]) -> Result<u64, String> {
    let rows: Vec<Vec<(String, String)>> = commits
        .iter()
        .map(|c| {
            vec![
                ("id".to_string(), c.sha.clone()),
                ("sha".to_string(), c.sha.clone()),
                ("author".to_string(), c.author.clone()),
                ("author_email".to_string(), c.email.clone()),
                ("committed_at".to_string(), c.committed_at.to_string()),
                ("message".to_string(), c.message.clone()),
            ]
        })
        .collect();
    store.bulk_insert_nodes("Commit", &rows)
}

pub(super) fn persist_commit_lineage(
    store: &GraphStore,
    commits: &[CommitMeta],
) -> Result<u64, String> {
    let shas: HashSet<&str> = commits.iter().map(|c| c.sha.as_str()).collect();
    // Link to the first parent only — the mainline ancestry. Parents outside
    // the fetched window have no Commit node, so skip them (no dangling edge).
    let edges: Vec<(String, String, Vec<(String, String)>)> = commits
        .iter()
        .filter_map(|c| {
            let parent = c.parents.first()?;
            if !shas.contains(parent.as_str()) {
                return None;
            }
            Some((c.sha.clone(), parent.clone(), Vec::new()))
        })
        .collect();
    store.bulk_insert_edges("PreviousVersion_Commit_Commit", &edges)
}

pub(super) fn persist_versions(store: &GraphStore, versions: &[VersionRow]) -> Result<u64, String> {
    let rows: Vec<Vec<(String, String)>> = versions
        .iter()
        .map(|v| {
            vec![
                ("id".to_string(), v.id.clone()),
                ("entity_id".to_string(), v.entity.entity_id.clone()),
                ("entity_kind".to_string(), v.entity.label.clone()),
                ("qualified_name".to_string(), v.entity.qualified_name.clone()),
                ("change_type".to_string(), v.entity.change_type.clone()),
                ("commit_sha".to_string(), v.commit_sha.clone()),
                ("committed_at".to_string(), v.committed_at.to_string()),
                ("lines_changed".to_string(), v.entity.lines_changed.to_string()),
            ]
        })
        .collect();
    store.bulk_insert_nodes("Version", &rows)
}

pub(super) fn persist_changed_in(
    store: &GraphStore,
    changed_in: &[(String, String)],
) -> Result<u64, String> {
    let edges = to_propless_edges(changed_in);
    store.bulk_insert_edges("ChangedIn_Version_Commit", &edges)
}

pub(super) fn persist_version_of(
    store: &GraphStore,
    version_of: &HashMap<String, Vec<(String, String)>>,
) -> Result<u64, String> {
    let mut total = 0u64;
    for (label, pairs) in version_of {
        let rel = format!("VersionOf_Version_{label}");
        let edges = to_propless_edges(pairs);
        total += store.bulk_insert_edges(&rel, &edges)?;
    }
    Ok(total)
}

pub(super) fn persist_prev_version(
    store: &GraphStore,
    prev: &[(String, String)],
) -> Result<u64, String> {
    let edges = to_propless_edges(prev);
    store.bulk_insert_edges("PreviousVersion_Version_Version", &edges)
}

fn to_propless_edges(pairs: &[(String, String)]) -> Vec<(String, String, Vec<(String, String)>)> {
    pairs
        .iter()
        .map(|(from, to)| (from.clone(), to.clone(), Vec::new()))
        .collect()
}
