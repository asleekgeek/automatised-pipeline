// incremental_index — issue #62 end-to-end gate (library level).
//
// External done-criteria (zetetic gate): incremental (changed-files-only)
// indexing is only "done" if, after mutating a tree and running an incremental
// pass, the graph is IDENTICAL to a from-scratch full index of the same tree —
// query-for-query — AND the pass touched only the changed files, AND it is
// dramatically faster than the full rebuild.
//
// This test:
//   1. Builds a fixture repo (many files, so the full index has real cost),
//   2. Full-indexes it into graph A and writes the file manifest,
//   3. Mutates the tree: edit one code file, edit one imported JS file, add a
//      file, delete a file, rename a file,
//   4. Runs the incremental pass over a fresh copy of the pristine graph A,
//   5. Full-indexes the MUTATED tree from scratch,
//   6. Asserts the filled graph == the full index query-for-query (parity),
//   7. Asserts the incremental counts partition the change set exactly
//      (touched only the changed files),
//   8. Asserts a cross-file inbound edge into an edited file survived,
//   9. Asserts incremental wall time ≤ 30% of the full rebuild.
//
// Wall-time threshold source: measured on this fixture (the test prints the
// ratio) — the full index parses 260 symbol-dense files, the incremental pass
// re-parses 4, measured at ~14% on the development machine. The 30% figure is
// the acceptance bound from issue #62 ("incremental ≤ 30% of full"); the ratio
// is measured paired (incremental + full in the same contention window, min of
// 3) so it is robust under the parallel-test load that runs many binaries.

use ai_architect_mcp::graph_store::GraphStore;
use ai_architect_mcp::indexer::{self, manifest, DependencyScope};
use std::fs;
use std::path::Path;

/// Number of generated Python modules. Large enough — together with rich
/// per-file bodies — that a full index has meaningful, measurable cost relative
/// to a 4-file incremental pass.
const N_MODULES: usize = 260;

/// A generated Python module body. Deliberately symbol-dense (many functions +
/// classes with methods) so each file is real work for the full index, while an
/// incremental pass that re-parses only four such files stays cheap — the whole
/// point of the wall-time ratio.
fn module_body(i: usize) -> String {
    let mut s = String::new();
    for j in 0..8 {
        s.push_str(&format!(
            "def mod{i}_fn{j}(x):\n    return x + {i} + {j}\n\n"
        ));
    }
    for k in 0..3 {
        s.push_str(&format!(
            "class Widget{i}_{k}:\n    def spin(self):\n        return {i}\n    \
             def flip(self, y):\n        return y\n\n"
        ));
    }
    s
}

/// The fixed battery of read queries used to compare two graphs. Covers node
/// totals, edge totals, per-label symbol sets, and the cross-file light-link
/// edge set — so a divergence in any of them fails the parity assertion.
fn snapshot_queries(store: &GraphStore) -> Vec<(String, Vec<Vec<String>>)> {
    let queries = [
        "MATCH (n) RETURN count(n)",
        "MATCH ()-[r]->() RETURN count(r)",
        "MATCH (f:Function) RETURN f.name ORDER BY f.name",
        "MATCH (m:Method) RETURN m.name ORDER BY m.name",
        "MATCH (c:Struct) RETURN c.name ORDER BY c.name",
        "MATCH (f:File) RETURN f.id ORDER BY f.id",
        "MATCH (d:Directory) RETURN d.id ORDER BY d.id",
        "MATCH (a:File)-[:Imports_File_File]->(b:File) RETURN a.id, b.id ORDER BY a.id, b.id",
        "MATCH (df:File)-[:Defines_File_Function]->(fn:Function) \
         RETURN df.id, fn.name ORDER BY df.id, fn.name",
    ];
    queries
        .iter()
        .map(|q| {
            let qr = store
                .execute_query(q)
                .unwrap_or_else(|e| panic!("query failed [{q}]: {e}"));
            (q.to_string(), qr.rows)
        })
        .collect()
}

fn write_fixture(root: &Path) {
    fs::create_dir_all(root.join("src")).expect("mk src");
    fs::create_dir_all(root.join("js")).expect("mk js");
    for i in 0..N_MODULES {
        fs::write(root.join(format!("src/mod{i:03}.py")), module_body(i)).expect("write module");
    }
    // A JS import pair: app.js (unchanged in the mutation) imports util.js
    // (edited in the mutation). Exercises inbound-edge preservation: the edge
    // app.js -> util.js must survive editing util.js.
    fs::write(
        root.join("js/app.js"),
        "import { u } from './util.js';\nexport const run = () => u;\n",
    )
    .expect("write app.js");
    fs::write(root.join("js/util.js"), "export const u = 1;\n").expect("write util.js");
}

#[test]
fn incremental_matches_full_reindex_and_is_faster() {
    let tmp = tempfile::Builder::new()
        .prefix("incremental_index_")
        .tempdir()
        .expect("temp dir");
    let repo = tmp.path().join("repo");
    write_fixture(&repo);

    // -- 2. Full index into graph A + manifest ------------------------------
    let out_a = tmp.path().join("out_a");
    fs::create_dir_all(&out_a).expect("mk out_a");
    let graph_a = out_a.join("graph");
    let manifest_a = manifest::manifest_path(&out_a);
    indexer::index_codebase_with_language(&repo, &graph_a, None, DependencyScope::None)
        .expect("full index A");
    indexer::write_full_manifest(&repo, &manifest_a, None, DependencyScope::None)
        .expect("write manifest A");

    // -- 3. Mutate the tree -------------------------------------------------
    // (a) edit a code file: replace mod000.py's bodies (names change too).
    fs::write(
        repo.join("src/mod000.py"),
        "def brand_new_fn():\n    return 999\n\ndef another_new():\n    return 0\n",
    )
    .expect("edit mod000");
    // (b) edit the imported JS file (app.js stays untouched).
    fs::write(repo.join("js/util.js"), "export const u = 42;\n").expect("edit util.js");
    // (c) add a new standalone file.
    fs::write(
        repo.join("src/mod_added.py"),
        "def added_symbol():\n    return 1\n",
    )
    .expect("add file");
    // (d) delete a file (src/ keeps many files, so no directory empties).
    fs::remove_file(repo.join("src/mod001.py")).expect("delete file");
    // (e) rename a file (same bytes → detected as a rename, not delete+add).
    fs::rename(
        repo.join("src/mod002.py"),
        repo.join("src/mod002_renamed.py"),
    )
    .expect("rename file");

    // -- 4/5. Paired best-of-5 timing over fresh copies of the pristine graph -
    // The incremental pass is short, so a transient CPU-starvation spike (many
    // test binaries run in parallel) can inflate a single wall-clock reading. We
    // measure the incremental pass AND a full index back-to-back in each attempt
    // — the same contention window — and take the attempt with the smallest
    // ratio. Pairing makes the ratio contention-invariant (both scale together
    // under load); the min-of-5 guards the short op against a spike that misses
    // its paired full. Parity/counts are checked once, on the first attempt.
    let mut best: Option<(u64, u64)> = None; // (inc_ms, full_ms) of the min-ratio attempt
    let mut first: Option<(
        indexer::IncrementalResult,
        std::path::PathBuf,
        std::path::PathBuf,
    )> = None;
    for attempt in 0..5 {
        let graph_try = tmp.path().join(format!("graph_try_{attempt}"));
        let manifest_try = tmp.path().join(format!("manifest_try_{attempt}.json"));
        copy_path(&graph_a, &graph_try);
        fs::copy(&manifest_a, &manifest_try).expect("copy manifest");
        let prior = manifest::load(&manifest_try).expect("manifest must load");
        let inc = indexer::index_incremental(
            &repo,
            &graph_try,
            &manifest_try,
            None,
            DependencyScope::None,
            &prior,
        )
        .expect("incremental pass");
        let full_try = tmp.path().join(format!("graph_full_{attempt}"));
        let full =
            indexer::index_codebase_with_language(&repo, &full_try, None, DependencyScope::None)
                .expect("full index");
        let (inc_ms, full_ms) = (inc.elapsed_ms, full.elapsed_ms.max(1));
        // Keep the attempt with the smallest inc/full ratio (cross-multiply to
        // avoid floats): inc_ms/full_ms < b_inc/b_full  ⇔  inc_ms*b_full < b_inc*full_ms.
        let better = match best {
            None => true,
            Some((b_inc, b_full)) => inc_ms * b_full < b_inc * full_ms,
        };
        if better {
            best = Some((inc_ms, full_ms));
        }
        if first.is_none() {
            first = Some((inc, graph_try, full_try));
        }
    }
    let (inc, graph_filled, graph_full) = first.unwrap();
    let (best_inc_ms, best_full_ms) = best.unwrap();

    // -- 7. Counts partition the change set exactly -------------------------
    assert_eq!(inc.changed, 2, "edited mod000.py + util.js");
    assert_eq!(inc.added, 1, "mod_added.py");
    assert_eq!(inc.deleted, 1, "mod001.py");
    assert_eq!(inc.renamed, 1, "mod002.py -> mod002_renamed.py");
    assert_eq!(
        inc.files_reparsed, 4,
        "only changed+added+renamed-new files are re-parsed (2+1+1)"
    );
    assert!(
        inc.unchanged >= (N_MODULES as u64) - 3,
        "the untouched files must all be classified unchanged, got {}",
        inc.unchanged
    );

    // -- 8. Inbound cross-file edge survived the edit of its target ---------
    {
        let store = GraphStore::open_or_create(&graph_filled).expect("open filled graph");
        let imp = store
            .execute_query(
                "MATCH (a:File)-[:Imports_File_File]->(b:File) \
                 WHERE a.id = 'js/app.js' RETURN b.id",
            )
            .expect("import query");
        assert!(
            imp.rows.iter().any(|r| r[0] == "js/util.js"),
            "app.js -> util.js must survive editing util.js, got {:?}",
            imp.rows
        );
    }

    // -- 6. Parity: filled == full, query-for-query -------------------------
    let snap_a = {
        let store = GraphStore::open_or_create(&graph_filled).expect("open filled graph");
        snapshot_queries(&store)
    };
    let snap_b = {
        let store = GraphStore::open_or_create(&graph_full).expect("open full graph");
        snapshot_queries(&store)
    };
    assert_eq!(
        snap_a, snap_b,
        "incremental graph must equal a from-scratch full index of the mutated tree"
    );

    // -- 9. Wall time: incremental ≤ 30% of full ----------------------------
    // source: issue #62 acceptance bound. Full parses 260 symbol-dense files,
    // incremental re-parses 4 → the ratio sits far below the 30% ceiling.
    eprintln!(
        "MEASURED incremental={best_inc_ms}ms full={best_full_ms}ms ratio={}%",
        best_inc_ms * 100 / best_full_ms
    );
    assert!(
        best_inc_ms * 100 <= best_full_ms * 30,
        "incremental wall time must be ≤ 30% of full: incremental={best_inc_ms}ms \
         full={best_full_ms}ms ({}%)",
        best_inc_ms * 100 / best_full_ms
    );
}

/// Copies a graph path (used to snapshot a pristine graph before each best-of-5
/// incremental attempt, so every attempt starts from the same state without
/// re-indexing). The lbug graph is a single file in this version, but older
/// layouts are a directory — handle both so the test is layout-agnostic.
fn copy_path(src: &Path, dst: &Path) {
    let meta = fs::symlink_metadata(src).expect("stat src");
    if meta.is_dir() {
        fs::create_dir_all(dst).expect("mk dst");
        for entry in fs::read_dir(src).expect("read_dir") {
            let entry = entry.expect("entry");
            copy_path(&entry.path(), &dst.join(entry.file_name()));
        }
    } else {
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent).expect("mk dst parent");
        }
        fs::copy(src, dst).expect("copy file");
    }
}

#[test]
fn incremental_preserves_resolved_cross_file_edges() {
    // Focused proof of the inbound-edge snapshot: a *resolved* cross-file edge
    // (the kind resolve_graph produces, and the kind the C reference's snapshot
    // exists to protect) into an edited file's SYMBOL must survive the purge.
    // The index stage never emits such edges, so we synthesise one, then edit
    // the target file and assert the edge is re-linked afterward.
    let tmp = tempfile::Builder::new()
        .prefix("incremental_relink_")
        .tempdir()
        .expect("temp dir");
    let repo = tmp.path().join("repo");
    fs::create_dir_all(repo.join("src")).expect("mk src");
    fs::write(
        repo.join("src/caller.py"),
        "def caller():\n    return callee()\n",
    )
    .expect("write caller");
    fs::write(repo.join("src/target.py"), "def callee():\n    return 1\n").expect("write target");

    let out = tmp.path().join("out");
    fs::create_dir_all(&out).expect("mk out");
    let graph = out.join("graph");
    let manifest_p = manifest::manifest_path(&out);
    indexer::index_codebase_with_language(&repo, &graph, None, DependencyScope::None)
        .expect("index");
    indexer::write_full_manifest(&repo, &manifest_p, None, DependencyScope::None)
        .expect("manifest");

    // Synthesise a resolved cross-file edge: caller() -> callee() across files.
    {
        let store = GraphStore::open_or_create(&graph).expect("open");
        store
            .insert_edge(
                "Calls_Function_Function",
                "src/caller.py::caller",
                "src/target.py::callee",
                &[("confidence", "1.0"), ("resolution_method", "'test'")],
            )
            .expect("insert synthetic cross-file call edge");
    }

    // Edit the TARGET file (keeps callee() so its qn is stable). The purge drops
    // callee and its inbound Calls edge; the snapshot must re-link it.
    fs::write(
        repo.join("src/target.py"),
        "def callee():\n    return 2  # edited\n",
    )
    .expect("edit target");

    let prior = manifest::load(&manifest_p).expect("load manifest");
    let inc = indexer::index_incremental(
        &repo,
        &graph,
        &manifest_p,
        None,
        DependencyScope::None,
        &prior,
    )
    .expect("incremental");
    assert_eq!(inc.changed, 1, "only target.py changed");

    let store = GraphStore::open_or_create(&graph).expect("reopen");
    let calls = store
        .execute_query(
            "MATCH (a:Function)-[:Calls_Function_Function]->(b:Function) \
             WHERE a.id = 'src/caller.py::caller' RETURN b.id",
        )
        .expect("calls query");
    assert!(
        calls.rows.iter().any(|r| r[0] == "src/target.py::callee"),
        "the inbound cross-file Calls edge into the edited file must be preserved, got {:?}",
        calls.rows
    );
}
