// artifact — team-shared graph snapshot (export / bootstrap-import).
//
// Issue #55. Highest-leverage onboarding feature borrowed from
// DeusData/codebase-memory-mcp (`src/pipeline/artifact.c`, verified): a
// compressed graph snapshot committed to the indexed repository so a teammate
// who clones never has to cold-index.
//
// Layer: infrastructure/adapter. This module performs I/O only — filesystem,
// `git` shell-out, and (de)compression. It archives the on-disk graph *path*
// and never touches `lbug`, `GraphStore`, or any inner-layer type; the node /
// edge counts it records in the sidecar are passed in by the composition root
// (the `index_codebase` handler). Dependencies point inward: this file depends
// on std + `tar` + `zstd` + `serde` only, never the reverse (coding-standards
// §2.2 dependency rule, §5.1 core-declares-what-it-needs).
//
// Shape (mirrors artifact.c):
//   export = archive the graph dir (tar) → zstd-compress → write
//            `<repo>/.automatised-pipeline/graph.zst` + a JSON sidecar
//            (schema version, git sha, tool version, node/edge counts); then
//            ensure a `.gitattributes` `merge=ours` entry so the binary blob
//            never produces a merge conflict across branches.
//   import = decompress → untar into `<output_dir>/graph`. The caller then
//            proceeds without a full cold index.
//
// AP-specific note vs. artifact.c: the C reference runs `VACUUM INTO` before
// snapshotting because its SQLite store is *live* (WAL frames could be torn,
// their #895). AP's store is CLOSED before export — `index_codebase` drops the
// `GraphStore` (flushing lbug) before this module runs — so a plain directory
// snapshot is already a consistent copy and no vacuum/checkpoint step exists.

use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Directory, relative to the indexed repo root, that holds the committed
/// artifact and its sidecar.
pub const ARTIFACT_DIR: &str = ".automatised-pipeline";
/// The compressed graph snapshot filename.
pub const ARTIFACT_FILE: &str = "graph.zst";
/// The JSON metadata sidecar filename.
pub const ARTIFACT_META: &str = "graph.meta.json";

/// Sidecar schema version. Bump when the sidecar or archive layout changes in
/// a way an older importer cannot read; import refuses a newer schema.
const SCHEMA_VERSION: u32 = 1;

/// zstd level for the explicit-index (best-ratio) tier.
// source: DeusData/codebase-memory-mcp src/pipeline/artifact.c — ART_ZSTD_BEST = 9.
const ZSTD_BEST: i32 = 9;
/// zstd level for the watcher / incremental (fast) tier.
// source: DeusData/codebase-memory-mcp src/pipeline/artifact.c — ART_ZSTD_FAST = 3.
const ZSTD_FAST: i32 = 3;

/// Hard ceiling on the decompressed archive size. A crafted `.zst` that
/// declares a larger stream is rejected mid-decode before it can exhaust disk,
/// so a malicious committed artifact cannot turn a bootstrap into a DoS.
// source: DeusData/codebase-memory-mcp src/pipeline/artifact.c —
// ART_MAX_DECOMPRESSED_BYTES = 64 GiB (their note: a full Linux-kernel index is
// ~14 GB and fits comfortably under this ceiling).
const MAX_DECOMPRESSED_BYTES: u64 = 64 * 1024 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Which compression tier to use. `Best` is the explicit-index tier (slower,
/// smaller); `Fast` is the watcher / incremental tier (faster, larger).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompressionTier {
    Best,
    // The watcher / incremental tier (zstd-3). AP has no file watcher yet, so
    // no production path constructs it — the variant is the reserved half of
    // the two-tier design the reference (artifact.c) mandates, exercised by the
    // unit tests. Kept (not deleted) so the watcher lands without an API change;
    // annotated per the repo idiom for a known-upcoming caller (see the
    // `#[allow(dead_code)] // used in stage 3b` markers in graph_store.rs).
    #[allow(dead_code)]
    Fast,
}

impl CompressionTier {
    fn level(self) -> i32 {
        match self {
            CompressionTier::Best => ZSTD_BEST,
            CompressionTier::Fast => ZSTD_FAST,
        }
    }
}

/// The sidecar metadata committed alongside the compressed graph.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ArtifactMeta {
    pub schema_version: u32,
    pub tool_version: String,
    /// git HEAD sha the artifact was exported at, or empty if the repo is not
    /// a git working tree.
    pub commit: String,
    pub node_count: u64,
    pub edge_count: u64,
    pub original_bytes: u64,
    pub compressed_bytes: u64,
    pub compression_level: i32,
}

/// Result of a successful export.
#[derive(Debug, Clone)]
pub struct ExportStats {
    pub artifact_path: PathBuf,
    pub original_bytes: u64,
    pub compressed_bytes: u64,
}

// ---------------------------------------------------------------------------
// Path helpers
// ---------------------------------------------------------------------------

fn artifact_dir(repo_path: &Path) -> PathBuf {
    repo_path.join(ARTIFACT_DIR)
}
fn artifact_file(repo_path: &Path) -> PathBuf {
    artifact_dir(repo_path).join(ARTIFACT_FILE)
}
fn meta_file(repo_path: &Path) -> PathBuf {
    artifact_dir(repo_path).join(ARTIFACT_META)
}

// ---------------------------------------------------------------------------
// Export
// ---------------------------------------------------------------------------

/// Exports the graph at `graph_path` into `<repo_path>/.automatised-pipeline/`.
///
/// Preconditions: `graph_path` exists (a closed lbug database — a directory in
/// lbug 0.15, but a single file is handled too) and the caller has already
/// dropped every open handle to it (so the snapshot is consistent). `node_count`
/// / `edge_count` are the counts the caller read from the just-built graph.
///
/// Postconditions on `Ok`: `graph.zst` (tar→zstd of the graph, written
/// atomically via a `.tmp` + rename) and `graph.meta.json` exist under the
/// artifact dir, and a `.gitattributes` `merge=ours` entry for the artifact
/// exists at the repo root. On `Err` nothing partial is left at the final
/// artifact path (the temp file is removed).
pub fn export_artifact(
    graph_path: &Path,
    repo_path: &Path,
    node_count: u64,
    edge_count: u64,
    tier: CompressionTier,
) -> Result<ExportStats, String> {
    if !graph_path.exists() {
        return Err(format!(
            "artifact export: graph path does not exist: {}",
            graph_path.display()
        ));
    }
    let dir = artifact_dir(repo_path);
    fs::create_dir_all(&dir)
        .map_err(|e| format!("artifact export: create {}: {e}", dir.display()))?;

    let level = tier.level();
    let out = artifact_file(repo_path);
    let tmp = out.with_extension("zst.tmp");
    let compressed_bytes = write_archive(graph_path, &tmp, level).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })?;
    fs::rename(&tmp, &out).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("artifact export: rename into place: {e}")
    })?;

    let original_bytes = path_size(graph_path);
    let meta = ArtifactMeta {
        schema_version: SCHEMA_VERSION,
        tool_version: env!("CARGO_PKG_VERSION").to_string(),
        commit: git_head(repo_path).unwrap_or_default(),
        node_count,
        edge_count,
        original_bytes,
        compressed_bytes,
        compression_level: level,
    };
    write_meta(repo_path, &meta)?;
    ensure_gitattributes(repo_path);

    Ok(ExportStats {
        artifact_path: out,
        original_bytes,
        compressed_bytes,
    })
}

/// Streams `graph_path` (dir or single file) through tar → zstd into `dest`.
/// Returns the compressed byte count. The archive stores the graph under its
/// own basename so import can unpack it back into `<parent>/<basename>`.
fn write_archive(graph_path: &Path, dest: &Path, level: i32) -> Result<u64, String> {
    let name = graph_path
        .file_name()
        .ok_or_else(|| "artifact export: graph path has no final component".to_string())?;
    let file =
        File::create(dest).map_err(|e| format!("artifact export: create {}: {e}", dest.display()))?;
    let encoder =
        zstd::Encoder::new(file, level).map_err(|e| format!("artifact export: zstd init: {e}"))?;
    let mut builder = tar::Builder::new(encoder);

    if graph_path.is_dir() {
        builder
            .append_dir_all(name, graph_path)
            .map_err(|e| format!("artifact export: tar dir: {e}"))?;
    } else {
        builder
            .append_path_with_name(graph_path, name)
            .map_err(|e| format!("artifact export: tar file: {e}"))?;
    }
    let encoder = builder
        .into_inner()
        .map_err(|e| format!("artifact export: tar finish: {e}"))?;
    let file = encoder
        .finish()
        .map_err(|e| format!("artifact export: zstd finish: {e}"))?;
    file.sync_all()
        .map_err(|e| format!("artifact export: fsync: {e}"))?;
    fs::metadata(dest)
        .map(|m| m.len())
        .map_err(|e| format!("artifact export: stat output: {e}"))
}

// ---------------------------------------------------------------------------
// Import (bootstrap)
// ---------------------------------------------------------------------------

/// Imports the committed artifact at `<repo_path>/.automatised-pipeline/` into
/// `graph_path` (typically `<output_dir>/graph`).
///
/// Preconditions: `artifact_exists(repo_path)` is true and `graph_path` does
/// not yet exist (bootstrap only fills a missing local index — the caller
/// guarantees this). Postconditions on `Ok`: `graph_path` is a populated graph
/// equivalent to the one the artifact was exported from, and the returned
/// `ArtifactMeta` is the committed sidecar. On `Err`, `graph_path` is not left
/// as a partial graph directory (the unpack target is cleaned).
///
/// Errors loudly (never silently) so the caller can log the reason and fall
/// back to a full index explicitly.
pub fn import_artifact(repo_path: &Path, graph_path: &Path) -> Result<ArtifactMeta, String> {
    let meta = read_meta(repo_path)?;
    if meta.schema_version > SCHEMA_VERSION {
        return Err(format!(
            "artifact import: sidecar schema {} is newer than supported {} — refusing",
            meta.schema_version, SCHEMA_VERSION
        ));
    }
    let dest_parent = graph_path
        .parent()
        .ok_or_else(|| "artifact import: graph path has no parent".to_string())?;
    fs::create_dir_all(dest_parent)
        .map_err(|e| format!("artifact import: create {}: {e}", dest_parent.display()))?;

    let src = artifact_file(repo_path);
    let file =
        File::open(&src).map_err(|e| format!("artifact import: open {}: {e}", src.display()))?;
    let decoder =
        zstd::Decoder::new(file).map_err(|e| format!("artifact import: zstd init: {e}"))?;
    // Cap the decoded stream so a crafted artifact cannot exhaust disk.
    let mut archive = tar::Archive::new(decoder.take(MAX_DECOMPRESSED_BYTES));
    // tar-rs rejects absolute paths and `..` components on unpack by default,
    // so a malicious archive cannot escape `dest_parent` (path-traversal safe).
    archive.unpack(dest_parent).map_err(|e| {
        let _ = fs::remove_dir_all(graph_path);
        format!("artifact import: unpack: {e}")
    })?;

    if !graph_path.exists() {
        return Err(format!(
            "artifact import: archive did not produce expected graph at {}",
            graph_path.display()
        ));
    }
    Ok(meta)
}

/// True when a committed, schema-compatible artifact is present in `repo_path`.
pub fn artifact_exists(repo_path: &Path) -> bool {
    let zst = artifact_file(repo_path);
    let present = fs::metadata(&zst).map(|m| m.len() > 0).unwrap_or(false);
    if !present {
        return false;
    }
    match read_meta(repo_path) {
        Ok(meta) => meta.schema_version <= SCHEMA_VERSION,
        Err(_) => false,
    }
}

// ---------------------------------------------------------------------------
// Metadata sidecar
// ---------------------------------------------------------------------------

fn write_meta(repo_path: &Path, meta: &ArtifactMeta) -> Result<(), String> {
    let json = serde_json::to_vec_pretty(meta)
        .map_err(|e| format!("artifact export: encode meta: {e}"))?;
    let path = meta_file(repo_path);
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, &json).map_err(|e| format!("artifact export: write meta: {e}"))?;
    fs::rename(&tmp, &path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("artifact export: rename meta: {e}")
    })
}

fn read_meta(repo_path: &Path) -> Result<ArtifactMeta, String> {
    let path = meta_file(repo_path);
    let bytes =
        fs::read(&path).map_err(|e| format!("artifact: read {}: {e}", path.display()))?;
    serde_json::from_slice(&bytes).map_err(|e| format!("artifact: parse sidecar: {e}"))
}

// ---------------------------------------------------------------------------
// .gitattributes — prevent merge conflicts on the binary artifact
// ---------------------------------------------------------------------------

/// Ensures `<repo>/.gitattributes` carries a `merge=ours` entry for the
/// artifact, and best-effort configures the local `ours` merge driver (git has
/// no built-in one). Idempotent: the line is appended only if absent, so
/// re-export never duplicates it. Best-effort — a failure here degrades to
/// "the binary may conflict on merge", never a failed export.
// source: DeusData/codebase-memory-mcp src/pipeline/artifact.c ensure_gitattributes —
// `<artifact> binary merge=ours`, with `merge.ours.driver true` configured so
// the attribute resolves. `binary` must precede `merge=ours` only inside the
// same macro expansion; as separate tokens on one line, order is not load-bearing.
fn ensure_gitattributes(repo_path: &Path) {
    let entry = format!("{ARTIFACT_DIR}/{ARTIFACT_FILE} binary merge=ours");
    let ga = repo_path.join(".gitattributes");
    let existing = fs::read_to_string(&ga).unwrap_or_default();
    if !existing.lines().any(|l| l.trim() == entry) {
        let mut next = existing;
        if !next.is_empty() && !next.ends_with('\n') {
            next.push('\n');
        }
        next.push_str("# Auto-generated by automatised-pipeline (issue #55):\n");
        next.push_str("# keep the committed graph snapshot from producing merge conflicts.\n");
        next.push_str(&entry);
        next.push('\n');
        if let Err(e) = fs::write(&ga, next) {
            eprintln!("[ap] artifact: write .gitattributes: {e}");
        }
    }
    // Best-effort local merge-driver registration. No shell — args are passed
    // directly to git, so no interpolation/injection surface.
    let _ = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["config", "merge.ours.driver", "true"])
        .output();
}

// ---------------------------------------------------------------------------
// git HEAD sha
// ---------------------------------------------------------------------------

/// Returns the git HEAD sha for `repo_path`, or `None` if it is not a git
/// working tree. Uses `Command` args (no shell) — injection-safe, matching the
/// pattern in `history/mod.rs`.
fn git_head(repo_path: &Path) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if sha.is_empty() {
        None
    } else {
        Some(sha)
    }
}

// ---------------------------------------------------------------------------
// Directory size (best-effort, for the sidecar's original_bytes)
// ---------------------------------------------------------------------------

/// Sum of regular-file sizes under `path` (or the file's own size). Best-effort:
/// unreadable entries contribute 0. Used only for the informational
/// `original_bytes` sidecar field / compression-ratio logging.
fn path_size(path: &Path) -> u64 {
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(_) => return 0,
    };
    if meta.is_file() {
        return meta.len();
    }
    if !meta.is_dir() {
        return 0;
    }
    let mut total = 0u64;
    if let Ok(entries) = fs::read_dir(path) {
        for entry in entries.flatten() {
            total = total.saturating_add(path_size(&entry.path()));
        }
    }
    total
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_import_round_trips_a_directory_graph() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("repo");
        let out = tmp.path().join("out");
        let graph = out.join("graph");
        fs::create_dir_all(graph.join("sub")).expect("mk graph");
        fs::write(graph.join("a.bin"), b"payload-a").expect("write a");
        fs::write(graph.join("sub/b.bin"), b"payload-b").expect("write b");
        fs::create_dir_all(&repo).expect("mk repo");

        let stats = export_artifact(&graph, &repo, 3, 5, CompressionTier::Best)
            .expect("export should succeed");
        assert!(stats.compressed_bytes > 0);
        assert!(artifact_exists(&repo));

        // Simulate a fresh clone: fresh output dir, no local graph.
        let fresh = tmp.path().join("fresh");
        let fresh_graph = fresh.join("graph");
        let meta = import_artifact(&repo, &fresh_graph).expect("import should succeed");
        assert_eq!(meta.node_count, 3);
        assert_eq!(meta.edge_count, 5);
        assert_eq!(meta.compression_level, ZSTD_BEST);

        assert_eq!(fs::read(fresh_graph.join("a.bin")).unwrap(), b"payload-a");
        assert_eq!(
            fs::read(fresh_graph.join("sub/b.bin")).unwrap(),
            b"payload-b"
        );
    }

    #[test]
    fn gitattributes_entry_is_created_once_and_not_duplicated() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("repo");
        let graph = tmp.path().join("out/graph");
        fs::create_dir_all(&graph).expect("mk graph");
        fs::write(graph.join("x.bin"), b"x").expect("write x");
        fs::create_dir_all(&repo).expect("mk repo");

        export_artifact(&graph, &repo, 1, 0, CompressionTier::Fast).expect("first export");
        export_artifact(&graph, &repo, 1, 0, CompressionTier::Fast).expect("second export");

        let ga = fs::read_to_string(repo.join(".gitattributes")).expect("read gitattributes");
        let entry = format!("{ARTIFACT_DIR}/{ARTIFACT_FILE} binary merge=ours");
        let count = ga.lines().filter(|l| l.trim() == entry).count();
        assert_eq!(count, 1, "entry must appear exactly once, got:\n{ga}");
    }

    #[test]
    fn artifact_exists_is_false_without_export() {
        let tmp = tempfile::tempdir().expect("tempdir");
        assert!(!artifact_exists(tmp.path()));
    }

    #[test]
    fn import_refuses_newer_schema() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("repo");
        let graph = tmp.path().join("out/graph");
        fs::create_dir_all(&graph).expect("mk graph");
        fs::write(graph.join("x.bin"), b"x").expect("write x");
        fs::create_dir_all(&repo).expect("mk repo");
        export_artifact(&graph, &repo, 1, 1, CompressionTier::Fast).expect("export");

        // Rewrite the sidecar with a future schema version.
        let mut meta = read_meta(&repo).expect("read meta");
        meta.schema_version = SCHEMA_VERSION + 1;
        write_meta(&repo, &meta).expect("rewrite meta");
        assert!(!artifact_exists(&repo));

        let err = import_artifact(&repo, &tmp.path().join("fresh/graph"))
            .expect_err("must refuse newer schema");
        assert!(err.contains("newer than supported"), "got: {err}");
    }
}
