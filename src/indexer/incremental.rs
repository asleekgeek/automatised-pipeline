// incremental — changed-files-only re-indexing (issue #62).
//
// Layer: application/use-case within the indexer. This module owns the *policy*
// of "index only what changed": it classifies the current file tree against the
// persisted manifest (`super::manifest`), purges the graph nodes of the files
// that changed/vanished, re-parses only those files through the same
// walk→parse→persist machinery the full index uses, and re-derives the
// light-link edges out of them. It depends inward on `graph_store` (the store
// port), `parser`, and its sibling indexer submodules; nothing depends on it
// except the composition root (`do_index_codebase`) and its tests.
//
// Reference: DeusData/codebase-memory-mcp `src/pipeline/pipeline_incremental.c`.
// Their design, adapted to AP's store and improved:
//   * Classify by (mtime, size) against a stored per-file manifest — added /
//     modified / deleted / unchanged (their `classify_files` +
//     `find_deleted_files`). AP mirrors this on the hot path.
//   * The C reference purges a changed file's nodes INCLUDING its file node,
//     then snapshots inbound cross-file edges before the purge so cross-file
//     references survive (their `incr_capture_inbound_edge`). AP improves on
//     this for the common case: a *modified* file keeps its File node (its id —
//     the repo-relative path — is stable across an edit), so every inbound
//     File→File / Dir→File edge survives structurally with no snapshot needed.
//     Only the file's *symbols* are purged and re-parsed. AP still snapshots
//     inbound cross-file edges into a modified file's *symbols* (the case the C
//     reference targets) so a resolved graph's cross-file edges are preserved.
//   * Rename detection: the C reference has NONE (a rename is delete+add). AP's
//     documented improvement — the manifest stores a content hash per file, so
//     a (deleted, added) pair with an identical hash is reported as a rename.
//     The store makes an in-place primary-key rewrite unavailable (lbug/Kuzu
//     forbid SET on a PK), so a rename is executed as purge-old + parse-new
//     (equivalent to a full index of the renamed tree) but REPORTED as a
//     rename in the response counts.

use crate::graph_store::{cypher_str, GraphStore, REL_TABLES};
use crate::parser::Language;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

use super::manifest::{self, FileManifest, FileState};
use super::walk::{collect_source_files, is_dependency_path, DependencyScope, WalkOptions};
use super::{light_link, persist, relative_path, IncrementalResult, SymbolBatch};

// Every symbol the parser emits carries a file-scoped qualified-name id
// (`<rel_path>::…`): the top-level extraction scope is the file path and nested
// scopes recurse through it, while CallSite/Import/Module ids are all
// `caller_qn::…` or `qual(scope, …)` — verified across the language extractors.
// So `starts_with(id, "<rel>::")` selects exactly one file's symbols across
// every symbol label (Module, Function, Method, Struct, Enum, Variant, Trait,
// Field, Constant, TypeAlias, Import, CallSite). `File` and `Directory` are
// keyed by the bare path (no `::`), so the same predicate never matches them —
// which is why the purge can run label-agnostically (see `purge_file_symbols`).

/// The cross-file link tables re-derived from a modified file's own text on
/// re-parse. Their OUTBOUND edges from a changed file are purged and rebuilt;
/// their INBOUND edges into a changed file survive because the File node is kept.
const LIGHT_LINK_TABLES: &[&str] = &["Imports_File_File", "References_File_File"];

/// A file discovered on disk during the incremental scan, with the cheap change
/// signal (mtime, size) already read.
#[derive(Debug, Clone)]
struct Discovered {
    /// Repo-relative, forward-slash id — identical to the File node's `id`.
    rel: String,
    abs: PathBuf,
    mtime_ns: i64,
    size: u64,
}

/// A rename: the old file's nodes are purged, the new file is parsed fresh.
/// The pair is keyed on an identical content hash at detection time; the hash
/// itself is not retained past classification.
#[derive(Debug, Clone)]
struct Rename {
    old_rel: String,
    new_file: Discovered,
}

/// The classification of the current tree against the prior manifest, plus the
/// manifest to persist after the pass succeeds.
struct Plan {
    /// Modified files: File node kept, symbols re-parsed.
    changed: Vec<Discovered>,
    /// New files: inserted from scratch.
    added: Vec<Discovered>,
    /// Files gone from disk: File node + symbols purged.
    deleted: Vec<String>,
    /// Rename pairs (old purged, new parsed), reported distinctly.
    renamed: Vec<Rename>,
    /// Files whose (mtime, size) — or content hash — is unchanged: no work.
    unchanged: Vec<Discovered>,
    /// The manifest for the current tree, written on success. Carries every
    /// current file's next state (hashes reused for unchanged files).
    next_manifest: FileManifest,
}

/// Runs a changed-files-only re-index of `codebase` into the existing graph at
/// `graph_dir`, using `prior` (the loaded manifest) as the change baseline.
///
/// Preconditions: `graph_dir` is an existing, openable graph previously built by
/// a full index of (an ancestor state of) `codebase`; `manifest_path` is where
/// the refreshed manifest is written; `prior` is the manifest that graph was
/// left with. Postconditions on `Ok`: the graph equals what a full index of the
/// current `codebase` tree would produce at the index stage (File/Directory +
/// symbol nodes, structural + light-link edges), the manifest at
/// `manifest_path` reflects the current tree, and the returned counts sum the
/// files by class. Invariant preserved across the pass: for every unchanged
/// file, none of its nodes or edges are touched (asserted by the integration
/// test's per-file count check).
pub fn index_incremental(
    codebase: &Path,
    graph_dir: &Path,
    manifest_path: &Path,
    language_filter: Option<Language>,
    dependency_scope: DependencyScope,
    prior: &FileManifest,
) -> Result<IncrementalResult, String> {
    let start = Instant::now();
    let store = GraphStore::open_or_create(graph_dir)?;
    // No create_schema() here: the graph already exists (this path is reached
    // only when a prior full index built it with the current schema), and the
    // DDL pass is ~0.4s of pure fixed cost that would defeat the whole point of
    // an incremental re-index. A schema mismatch is instead the caller's cue to
    // pass `full: true` (documented on the tool). source: measured — skipping
    // the redundant CREATE TABLE IF NOT EXISTS pass is the single largest
    // incremental speedup on small change sets.

    let walk_opts = WalkOptions {
        language_filter,
        dependency_scope,
    };
    let current = discover(codebase, walk_opts)?;
    let plan = classify(prior, &current);

    // The set of files whose nodes are being purged-and-reparsed. Used both to
    // scope the inbound-edge snapshot (source must be OUTSIDE this set) and to
    // re-run light-linking only for these sources.
    let mut reparsed_set: HashSet<String> = HashSet::new();
    for d in &plan.changed {
        reparsed_set.insert(d.rel.clone());
    }
    for d in &plan.added {
        reparsed_set.insert(d.rel.clone());
    }
    for r in &plan.renamed {
        reparsed_set.insert(r.new_file.rel.clone());
    }

    // ---- 1. Snapshot inbound cross-file edges into MODIFIED files' symbols --
    // (Deleted/renamed-old files are NOT snapshotted: their inbound edges must
    // die, exactly as a full re-index would drop an edge to a vanished target.)
    let changed_rels: Vec<&str> = plan.changed.iter().map(|d| d.rel.as_str()).collect();
    let saved_edges = snapshot_inbound_edges(&store, &changed_rels, &reparsed_set)?;

    // ---- 2. Purge -----------------------------------------------------------
    for d in &plan.changed {
        purge_file_symbols(&store, &d.rel)?;
        purge_outbound_light_links(&store, &d.rel)?;
    }
    for rel in &plan.deleted {
        purge_file_symbols(&store, rel)?;
        purge_file_node(&store, rel)?;
    }
    for r in &plan.renamed {
        purge_file_symbols(&store, &r.old_rel)?;
        purge_file_node(&store, &r.old_rel)?;
    }

    // ---- 3. Re-parse changed/added/renamed-new files ------------------------
    // Seed the "already inserted" directory set with the Directory nodes the
    // graph already holds, so inserting a NEW file under an existing directory
    // does not try to re-CREATE that directory (a primary-key violation). Only
    // genuinely new directories are created, matching a full index.
    let mut dir_nodes_inserted: HashSet<PathBuf> = existing_directory_ids(&store)?;
    for d in &plan.changed {
        reparse_modified_file(&store, codebase, d, dependency_scope)?;
    }
    for d in &plan.added {
        reparse_new_file(
            &store,
            codebase,
            d,
            dependency_scope,
            &mut dir_nodes_inserted,
        )?;
    }
    for r in &plan.renamed {
        reparse_new_file(
            &store,
            codebase,
            &r.new_file,
            dependency_scope,
            &mut dir_nodes_inserted,
        )?;
    }

    // A deletion or rename can empty a directory. A full index never creates a
    // Directory node with no descendant file, so prune any that were orphaned to
    // keep the graph identical to a from-scratch index of the current tree.
    if !plan.deleted.is_empty() || !plan.renamed.is_empty() {
        prune_orphan_directories(&store)?;
    }

    // ---- 4. Re-derive light-link edges OUT of the re-parsed files -----------
    // Resolve against the full current file set so a changed file can still link
    // to an unchanged target's File node.
    let all_files: Vec<PathBuf> = current.iter().map(|d| d.abs.clone()).collect();
    let reparsed_files: Vec<PathBuf> = current
        .iter()
        .filter(|d| reparsed_set.contains(&d.rel))
        .map(|d| d.abs.clone())
        .collect();
    if !reparsed_files.is_empty() {
        match light_link::link_file_imports_for(&store, codebase, &reparsed_files, &all_files) {
            Ok(_) => {}
            Err(e) => eprintln!("incremental: light-link pass skipped: {e}"),
        }
    }

    // ---- 5. Re-link the snapshotted inbound cross-file edges -----------------
    relink_inbound_edges(&store, &saved_edges)?;

    // ---- 6. Persist the refreshed manifest ----------------------------------
    manifest::save(manifest_path, &plan.next_manifest)?;

    // Intentionally NO node_count()/edge_count() here — see `IncrementalResult`.
    Ok(IncrementalResult {
        graph_path: graph_dir.to_path_buf(),
        changed: plan.changed.len() as u64,
        added: plan.added.len() as u64,
        deleted: plan.deleted.len() as u64,
        renamed: plan.renamed.len() as u64,
        unchanged: plan.unchanged.len() as u64,
        files_reparsed: reparsed_set.len() as u64,
        elapsed_ms: start.elapsed().as_millis() as u64,
    })
}

/// Builds the manifest for a freshly full-indexed `codebase` and writes it to
/// `manifest_path`, so the NEXT `index_codebase` call can run incrementally.
///
/// Preconditions: `codebase` is the tree just indexed with the same
/// `language_filter`/`dependency_scope`; `manifest_path`'s parent exists.
/// Postcondition on `Ok`: the manifest records (mtime, size, content_hash) for
/// every file the full index visited, keyed by the File node id. Best-effort per
/// file: a stat/read failure records a zeroed/empty state rather than aborting
/// the whole index (the file simply re-classifies as changed next time).
pub fn write_full_manifest(
    codebase: &Path,
    manifest_path: &Path,
    language_filter: Option<Language>,
    dependency_scope: DependencyScope,
) -> Result<(), String> {
    let walk_opts = WalkOptions {
        language_filter,
        dependency_scope,
    };
    let current = discover(codebase, walk_opts)?;
    let mut m = FileManifest::new();
    for d in &current {
        m.files.insert(
            d.rel.clone(),
            FileState {
                mtime_ns: d.mtime_ns,
                size: d.size,
                content_hash: manifest::hash_file(&d.abs).unwrap_or_default(),
            },
        );
    }
    manifest::save(manifest_path, &m)
}

// ---------------------------------------------------------------------------
// Discovery + classification
// ---------------------------------------------------------------------------

/// Walks `codebase` and reads each file's (mtime, size). Postcondition: one
/// `Discovered` per source file the full index would visit, with `rel` equal to
/// the File node id the indexer assigns.
fn discover(codebase: &Path, walk_opts: WalkOptions) -> Result<Vec<Discovered>, String> {
    let files = collect_source_files(codebase, walk_opts)?;
    let mut out = Vec::with_capacity(files.len());
    for abs in files {
        let rel = light_link::rel_id(codebase, &abs);
        let (mtime_ns, size) = match std::fs::metadata(&abs) {
            Ok(m) => (manifest::mtime_ns(&m), m.len()),
            Err(_) => (0, 0),
        };
        out.push(Discovered {
            rel,
            abs,
            mtime_ns,
            size,
        });
    }
    Ok(out)
}

/// Classifies `current` against `prior`. See `Plan`. Rename detection pairs a
/// deleted file with an added file that carries an identical content hash (the
/// deleted file's hash comes from the manifest — the file is gone from disk, so
/// it cannot be re-hashed; the added file's hash is read fresh).
fn classify(prior: &FileManifest, current: &[Discovered]) -> Plan {
    let current_ids: HashSet<&str> = current.iter().map(|d| d.rel.as_str()).collect();

    let mut changed = Vec::new();
    let mut added_candidates = Vec::new();
    let mut unchanged = Vec::new();
    // next_manifest accumulates the state to persist for every current file.
    let mut next = FileManifest::new();

    for d in current {
        match prior.files.get(&d.rel) {
            None => {
                // Not previously known → a new file (may still be a rename's
                // new end; resolved below).
                added_candidates.push(d.clone());
            }
            Some(prev) if prev.mtime_ns == d.mtime_ns && prev.size == d.size => {
                // Cheap signal says unchanged: carry the prior hash forward.
                unchanged.push(d.clone());
                next.files.insert(d.rel.clone(), prev.clone());
            }
            Some(prev) => {
                // mtime or size moved → hash to confirm. Identical bytes despite
                // a moved mtime is the correctness fallback the C reference
                // lacks: a `touch` with no edit is NOT a change.
                let hash = manifest::hash_file(&d.abs).unwrap_or_default();
                if !hash.is_empty() && hash == prev.content_hash {
                    unchanged.push(d.clone());
                    next.files.insert(
                        d.rel.clone(),
                        FileState {
                            mtime_ns: d.mtime_ns,
                            size: d.size,
                            content_hash: hash,
                        },
                    );
                } else {
                    changed.push(d.clone());
                    next.files.insert(
                        d.rel.clone(),
                        FileState {
                            mtime_ns: d.mtime_ns,
                            size: d.size,
                            content_hash: hash,
                        },
                    );
                }
            }
        }
    }

    // Deleted candidates: prior files absent from the current discovery.
    let deleted_candidates: Vec<String> = prior
        .files
        .keys()
        .filter(|rel| !current_ids.contains(rel.as_str()))
        .cloned()
        .collect();

    // Rename detection: index deleted candidates by their stored content hash,
    // then match each added candidate's freshly-read hash against them.
    let mut deleted_by_hash: HashMap<String, Vec<String>> = HashMap::new();
    for rel in &deleted_candidates {
        if let Some(state) = prior.files.get(rel) {
            if !state.content_hash.is_empty() {
                deleted_by_hash
                    .entry(state.content_hash.clone())
                    .or_default()
                    .push(rel.clone());
            }
        }
    }

    let mut renamed = Vec::new();
    let mut added = Vec::new();
    let mut consumed_deleted: HashSet<String> = HashSet::new();
    for d in added_candidates {
        let hash = manifest::hash_file(&d.abs).unwrap_or_default();
        let matched_old = if hash.is_empty() {
            None
        } else {
            deleted_by_hash.get_mut(&hash).and_then(|olds| {
                olds.iter()
                    .position(|o| !consumed_deleted.contains(o))
                    .map(|i| olds[i].clone())
            })
        };
        // Every added/renamed-new file gets its fresh state in the manifest.
        next.files.insert(
            d.rel.clone(),
            FileState {
                mtime_ns: d.mtime_ns,
                size: d.size,
                content_hash: hash.clone(),
            },
        );
        match matched_old {
            Some(old_rel) => {
                consumed_deleted.insert(old_rel.clone());
                renamed.push(Rename {
                    old_rel,
                    new_file: d,
                });
            }
            None => added.push(d),
        }
    }

    // Truly-deleted = deleted candidates not consumed by a rename pairing.
    let deleted: Vec<String> = deleted_candidates
        .into_iter()
        .filter(|rel| !consumed_deleted.contains(rel))
        .collect();

    Plan {
        changed,
        added,
        deleted,
        renamed,
        unchanged,
        next_manifest: next,
    }
}

// ---------------------------------------------------------------------------
// Graph mutation — purge
// ---------------------------------------------------------------------------

/// Deletes every symbol node of `rel` (id prefixed `"<rel>::"`), cascading their
/// edges, in a single label-agnostic pass.
///
/// A file-scoped symbol is exactly a node whose id begins with `"<rel>::"`; the
/// File node's id is the bare `"<rel>"` (no `::`) and Directory/Community/…/
/// Version ids never carry a `"<rel>::"` prefix, so this deletes precisely the
/// file's symbols and nothing else. Uses one unlabeled `MATCH (n) … DETACH
/// DELETE n` (verified supported by lbug 0.15) instead of one query per label —
/// ~12× fewer round-trips per changed file. source: `SYMBOL_LABELS` documents
/// which labels this covers; the unlabeled scan is the measured-fast equivalent.
fn purge_file_symbols(store: &GraphStore, rel: &str) -> Result<(), String> {
    let prefix = cypher_str(&format!("{rel}::"));
    let cypher = format!("MATCH (n) WHERE starts_with(n.id, {prefix}) DETACH DELETE n");
    store.execute_query(&cypher)?;
    Ok(())
}

/// Deletes the File node for `rel` and every edge touching it. Used for deleted
/// and renamed-old files only (a modified file keeps its File node).
fn purge_file_node(store: &GraphStore, rel: &str) -> Result<(), String> {
    let id = cypher_str(rel);
    let cypher = format!("MATCH (f:File) WHERE f.id = {id} DETACH DELETE f");
    store.execute_query(&cypher)?;
    Ok(())
}

/// Deletes the light-link edges OUT of `rel` so the re-parse can re-derive them
/// from the file's current text (an import removed by the edit must disappear;
/// one added must appear). Inbound light-link edges are untouched — the File
/// node survives, so edges from unchanged files into `rel` are preserved.
fn purge_outbound_light_links(store: &GraphStore, rel: &str) -> Result<(), String> {
    let id = cypher_str(rel);
    for table in LIGHT_LINK_TABLES {
        let cypher = format!("MATCH (a:File)-[r:{table}]->(:File) WHERE a.id = {id} DELETE r");
        store.execute_query(&cypher)?;
    }
    Ok(())
}

/// The ids of every Directory node currently in the graph, as `PathBuf`s keyed
/// the same way `insert_ancestor_dirs` tracks them (the relative dir path).
fn existing_directory_ids(store: &GraphStore) -> Result<HashSet<PathBuf>, String> {
    let qr = store.execute_query("MATCH (d:Directory) RETURN d.id")?;
    Ok(qr
        .rows
        .into_iter()
        .filter_map(|row| row.into_iter().next())
        .map(PathBuf::from)
        .collect())
}

/// Deletes Directory nodes left with no children (no `Contains_Dir_File` and no
/// `Contains_Dir_Dir` out-edges) after a purge, to a fixpoint — deleting a leaf
/// directory can orphan its parent. A Directory's only out-edges are the two
/// containment kinds, so "zero out-edges" is exactly "childless". Bounded by
/// `MAX_DEPTH` iterations (the walker's own directory-depth cap), so a
/// pathological tree cannot loop unbounded.
fn prune_orphan_directories(store: &GraphStore) -> Result<(), String> {
    for _ in 0..super::MAX_DEPTH {
        let orphans = store.execute_query(
            "MATCH (d:Directory) OPTIONAL MATCH (d)-[e]->() \
             WITH d, count(e) AS c WHERE c = 0 RETURN d.id",
        )?;
        if orphans.rows.is_empty() {
            return Ok(());
        }
        for row in orphans.rows {
            if let Some(id) = row.into_iter().next() {
                let cypher = format!(
                    "MATCH (d:Directory) WHERE d.id = {} DETACH DELETE d",
                    cypher_str(&id)
                );
                store.execute_query(&cypher)?;
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Graph mutation — re-parse
// ---------------------------------------------------------------------------

/// Re-parses a MODIFIED file into the existing graph. The File node is kept
/// (its id is stable); its symbols were already purged. Resets the File node's
/// mutable columns (size, parse_errors) then re-inserts the parsed symbols and
/// intra-file edges. Postcondition: the file's symbol subgraph equals a fresh
/// parse; inbound File-targeted edges are untouched throughout.
fn reparse_modified_file(
    store: &GraphStore,
    codebase: &Path,
    d: &Discovered,
    dependency_scope: DependencyScope,
) -> Result<(), String> {
    // Reset the kept File node's mutable state. index_single_file re-bumps
    // parse_errors only when >0, so we clear it first (a fixed parse must drop
    // back to 0), and refresh size_bytes to the new length.
    let id = cypher_str(&d.rel);
    let reset = format!(
        "MATCH (f:File) WHERE f.id = {id} SET f.parse_errors = 0, f.size_bytes = {}",
        d.size
    );
    store.execute_query(&reset)?;

    let mut batch = SymbolBatch::default();
    let mut label_by_qn: HashMap<String, String> = HashMap::new();
    label_by_qn.insert(d.rel.clone(), "File".into());
    let mut seen_node_ids: HashSet<String> = HashSet::new();
    let restrict =
        dependency_scope == DependencyScope::PublicApi && is_dependency_path(codebase, &d.abs);
    if let Err(e) = persist::index_single_file(
        store,
        &mut batch,
        &d.abs,
        &d.rel,
        &mut label_by_qn,
        &mut seen_node_ids,
        restrict,
    ) {
        eprintln!("incremental: skipping {}: {e}", d.rel);
    }
    batch.flush(store)
}

/// Indexes a NEW (or renamed-new) file from scratch: ancestor Directory nodes,
/// the File node, the Dir→File containment edge, then the parsed symbols and
/// intra-file edges — exactly the per-file body of the full index.
fn reparse_new_file(
    store: &GraphStore,
    codebase: &Path,
    d: &Discovered,
    dependency_scope: DependencyScope,
    dir_nodes_inserted: &mut HashSet<PathBuf>,
) -> Result<(), String> {
    let mut batch = SymbolBatch::default();
    let mut label_by_qn: HashMap<String, String> = HashMap::new();
    let mut seen_node_ids: HashSet<String> = HashSet::new();

    persist::insert_ancestor_dirs(
        store,
        &mut batch,
        codebase,
        &d.abs,
        dir_nodes_inserted,
        &mut label_by_qn,
    )?;
    persist::insert_file_node(store, &d.abs, &d.rel)?;
    label_by_qn.insert(d.rel.clone(), "File".into());
    let rel_path = relative_path(codebase, &d.abs);
    persist::insert_dir_file_edge(&mut batch, &rel_path);

    let restrict =
        dependency_scope == DependencyScope::PublicApi && is_dependency_path(codebase, &d.abs);
    if let Err(e) = persist::index_single_file(
        store,
        &mut batch,
        &d.abs,
        &d.rel,
        &mut label_by_qn,
        &mut seen_node_ids,
        restrict,
    ) {
        eprintln!("incremental: skipping {}: {e}", d.rel);
    }
    batch.flush(store)
}

// ---------------------------------------------------------------------------
// Inbound cross-file edge snapshot + re-link
// ---------------------------------------------------------------------------

/// One captured inbound edge whose target sits in a modified file and whose
/// source sits elsewhere, saved so the purge does not orphan it.
struct SavedEdge {
    table: &'static str,
    source_id: String,
    target_id: String,
    confidence: String,
    resolution_method: String,
}

/// Prefixes of the resolution rel tables — the only tables that carry cross-file
/// edges (structural Defines/HasMethod/… are intra-file and re-emitted by the
/// re-parse). Mirrors `graph_store::is_resolution_rel`, kept local so this
/// module reasons from the public `REL_TABLES` list alone.
const RESOLUTION_PREFIXES: &[&str] = &[
    "Imports_",
    "Calls_",
    "Implements_",
    "Extends_",
    "Uses_",
    "References_",
];

/// Snapshots inbound cross-file edges into the symbols of `changed_rels`.
///
/// Captures an edge iff its target is a symbol inside a changed file
/// (`starts_with(t.id, "<rel>::")`) and its source's owning file is NOT in
/// `reparsed_set` (an edge from another re-parsed file is regenerated by that
/// file's own re-parse/re-link, so re-linking it here would be redundant). At
/// the index stage AP produces no cross-file symbol-targeted edges, so this
/// captures nothing; it becomes load-bearing the moment the graph also carries
/// resolved edges (a resolve_graph pass, or a future resolved artifact), which
/// is exactly the case the C reference's snapshot exists for.
fn snapshot_inbound_edges(
    store: &GraphStore,
    changed_rels: &[&str],
    reparsed_set: &HashSet<String>,
) -> Result<Vec<SavedEdge>, String> {
    let mut saved = Vec::new();
    if changed_rels.is_empty() {
        return Ok(saved);
    }
    // One `starts_with(t.id, 'p1') OR starts_with(t.id, 'p2') …` predicate covers
    // every changed file at once, so the scan costs one query per resolution
    // table instead of one per (table × file).
    let target_predicate = changed_rels
        .iter()
        .map(|rel| format!("starts_with(t.id, {})", cypher_str(&format!("{rel}::"))))
        .collect::<Vec<_>>()
        .join(" OR ");

    for &(name, _from, to) in REL_TABLES.iter() {
        if !is_resolution_table(name) {
            continue;
        }
        // File/Directory-targeted edges never need snapshotting: those nodes
        // are kept across a modification, so inbound edges survive structurally.
        if to == "File" || to == "Directory" {
            continue;
        }
        let cypher = format!(
            "MATCH (s)-[r:{name}]->(t) WHERE {target_predicate} \
             RETURN s.id, t.id, r.confidence, r.resolution_method"
        );
        let qr = store.execute_query(&cypher)?;
        for row in qr.rows {
            if row.len() < 2 {
                continue;
            }
            let source_id = row[0].clone();
            let target_id = row[1].clone();
            // Skip edges whose source is itself being re-parsed.
            if reparsed_set.contains(&owning_file(&source_id)) {
                continue;
            }
            saved.push(SavedEdge {
                table: name,
                source_id,
                target_id,
                confidence: row.get(2).cloned().unwrap_or_default(),
                resolution_method: row.get(3).cloned().unwrap_or_default(),
            });
        }
    }
    Ok(saved)
}

/// Re-inserts each snapshotted edge whose endpoints both still exist. A missing
/// endpoint makes `insert_edge`'s MATCH…MATCH…CREATE a no-op (target symbol was
/// deleted/renamed by the edit) — matching full-reindex semantics, which would
/// also not produce an edge to a vanished symbol.
fn relink_inbound_edges(store: &GraphStore, saved: &[SavedEdge]) -> Result<(), String> {
    for e in saved {
        let conf = if e.confidence.trim().is_empty() {
            "1.0".to_string()
        } else {
            e.confidence.clone()
        };
        let method = cypher_str(&e.resolution_method);
        let props: Vec<(&str, &str)> = vec![("confidence", &conf), ("resolution_method", &method)];
        // Best-effort: a schema mismatch on one edge must not abort the pass.
        if let Err(err) = store.insert_edge(e.table, &e.source_id, &e.target_id, &props) {
            eprintln!(
                "incremental: relink {} {} -> {} skipped: {err}",
                e.table, e.source_id, e.target_id
            );
        }
    }
    Ok(())
}

/// The repo-relative file that a node id belongs to: the substring before the
/// first `"::"` for a symbol, or the whole id for a File/Directory node.
fn owning_file(id: &str) -> String {
    match id.find("::") {
        Some(i) => id[..i].to_string(),
        None => id.to_string(),
    }
}

/// True for a resolution rel table (carries cross-file edges).
fn is_resolution_table(name: &str) -> bool {
    RESOLUTION_PREFIXES.iter().any(|p| name.starts_with(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owning_file_splits_on_first_separator() {
        assert_eq!(owning_file("src/a.rs::foo::bar"), "src/a.rs");
        assert_eq!(owning_file("src/a.rs"), "src/a.rs");
        assert_eq!(owning_file("src/a.rs::call@10:2#3-9"), "src/a.rs");
    }

    #[test]
    fn resolution_tables_recognised() {
        assert!(is_resolution_table("Calls_Function_Function"));
        assert!(is_resolution_table("Imports_File_File"));
        assert!(!is_resolution_table("Defines_File_Function"));
        assert!(!is_resolution_table("Contains_Dir_File"));
    }

    #[test]
    fn classify_detects_each_class_and_rename() {
        let dir = tempfile::Builder::new()
            .prefix("incremental_classify_")
            .tempdir()
            .expect("temp dir");
        let root = dir.path();

        // On-disk current tree: keep.py (unchanged), edit.py (modified),
        // new.py (added), moved_to.py (rename target). old_name.py and
        // gone.py exist only in the prior manifest (deleted / renamed-from).
        std::fs::write(root.join("keep.py"), "def keep():\n    return 1\n").unwrap();
        std::fs::write(root.join("edit.py"), "def edit():\n    return 2\n").unwrap();
        std::fs::write(root.join("new.py"), "def fresh():\n    return 3\n").unwrap();
        // Rename: moved_to.py holds the exact bytes old_name.py had.
        let moved_body = "def moved():\n    return 4\n";
        std::fs::write(root.join("moved_to.py"), moved_body).unwrap();

        let current = discover(root, WalkOptions::default()).expect("discover");
        let hash_of = |rel: &str| manifest::hash_file(&root.join(rel)).unwrap();

        // Prior manifest: keep.py unchanged; edit.py with a stale hash (edited);
        // old_name.py carries moved_to.py's content hash (the rename); gone.py
        // is a plain deletion.
        let keep = current.iter().find(|d| d.rel == "keep.py").unwrap();
        let mut prior = FileManifest::new();
        prior.files.insert(
            "keep.py".into(),
            FileState {
                mtime_ns: keep.mtime_ns,
                size: keep.size,
                content_hash: hash_of("keep.py"),
            },
        );
        prior.files.insert(
            "edit.py".into(),
            FileState {
                mtime_ns: 1,
                size: 999,
                content_hash: "stale".into(),
            },
        );
        prior.files.insert(
            "old_name.py".into(),
            FileState {
                mtime_ns: 1,
                size: moved_body.len() as u64,
                content_hash: hash_of("moved_to.py"),
            },
        );
        prior.files.insert(
            "gone.py".into(),
            FileState {
                mtime_ns: 1,
                size: 10,
                content_hash: "whatever".into(),
            },
        );

        let plan = classify(&prior, &current);
        let changed: Vec<&str> = plan.changed.iter().map(|d| d.rel.as_str()).collect();
        let added: Vec<&str> = plan.added.iter().map(|d| d.rel.as_str()).collect();
        let unchanged: Vec<&str> = plan.unchanged.iter().map(|d| d.rel.as_str()).collect();

        assert_eq!(changed, vec!["edit.py"], "edited file is changed");
        assert_eq!(added, vec!["new.py"], "genuinely new file is added");
        assert_eq!(unchanged, vec!["keep.py"], "untouched file is unchanged");
        assert_eq!(plan.deleted, vec!["gone.py"], "removed file is deleted");
        assert_eq!(plan.renamed.len(), 1, "one rename detected");
        assert_eq!(plan.renamed[0].old_rel, "old_name.py");
        assert_eq!(plan.renamed[0].new_file.rel, "moved_to.py");
        // Manifest carries every current file, and no stale prior-only entries.
        assert_eq!(plan.next_manifest.files.len(), 4);
        assert!(plan.next_manifest.files.contains_key("moved_to.py"));
        assert!(!plan.next_manifest.files.contains_key("old_name.py"));
    }
}
