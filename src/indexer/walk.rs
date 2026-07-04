use crate::parser::Language;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Directory walking
// ---------------------------------------------------------------------------

/// Options controlling a directory walk.
///
/// Bundled into one value so the recursive walker stays within the 4-parameter
/// limit (coding-standards §4.4) as new traversal knobs are added.
#[derive(Clone, Copy, Default)]
pub(super) struct WalkOptions {
    /// When `Some(L)`, only collect files of language `L`; `None` collects all.
    pub language_filter: Option<Language>,
    /// When true, descend into build/dependency directories (node_modules,
    /// .venv, vendor, target, …) that are pruned by default. Only `.git` is
    /// still skipped. source: checkpoint 2026-07-04 — full-dependency indexing
    /// for the Cortex brain; every vendored file becomes a navigable node.
    pub include_dependencies: bool,
}

/// Recursively collects source files, skipping hidden dirs, target/, node_modules/.
/// When `opts.language_filter` is Some, only collects files for that language.
/// When None, collects all files with recognized extensions.
/// When `opts.include_dependencies` is true, build/dependency dirs are also
/// descended into (only `.git` is skipped).
///
/// Symlinks are intentionally NOT followed — source: security hardening (C4).
/// This prevents a symlink inside the codebase from causing `read_dir` to
/// silently traverse outside the tree (e.g. to `/etc/passwd` or `~/.ssh`).
pub(super) fn collect_source_files(
    root: &Path,
    opts: WalkOptions,
) -> Result<Vec<PathBuf>, String> {
    let mut result = Vec::new();
    walk_dir_recursive(root, &mut result, opts, 0)?;
    if result.len() > super::MAX_FILES {
        return Err(format!(
            "too_many_files: codebase contains {} files, MAX_FILES is {}",
            result.len(), super::MAX_FILES
        ));
    }
    result.sort();
    Ok(result)
}

fn walk_dir_recursive(
    dir: &Path,
    out: &mut Vec<PathBuf>,
    opts: WalkOptions,
    depth: usize,
) -> Result<(), String> {
    if depth > super::MAX_DEPTH {
        return Err(format!(
            "walk_too_deep: exceeded MAX_DEPTH ({}) at {}",
            super::MAX_DEPTH,
            dir.display()
        ));
    }
    let entries = std::fs::read_dir(dir)
        .map_err(|e| format!("read_dir {}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("dir entry: {e}"))?;
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if should_skip(&name_str, opts.include_dependencies) {
            continue;
        }
        // Use symlink_metadata (lstat) instead of metadata (stat) so symlinks
        // are detected and skipped rather than silently followed.
        // source: C4 fix — POSIX lstat(2), does not follow the final symlink.
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.file_type().is_symlink() {
            continue; // intentionally skip symlinks
        }
        if meta.is_dir() {
            walk_dir_recursive(&path, out, opts, depth + 1)?;
            if out.len() > super::MAX_FILES {
                return Err(format!(
                    "too_many_files: exceeded MAX_FILES ({}) during walk",
                    super::MAX_FILES
                ));
            }
        } else if meta.is_file() {
            if meta.len() > super::MAX_FILE_BYTES {
                eprintln!(
                    "indexer: skipping oversized file ({} bytes > MAX_FILE_BYTES {}): {}",
                    meta.len(), super::MAX_FILE_BYTES, path.display()
                );
                continue;
            }
            // File collection policy:
            //   * language_filter = Some(L): collect ONLY files of language L
            //     (a scoped re-index of a single language).
            //   * language_filter = None: ALL-FILE indexing — collect every
            //     file regardless of extension. Code files in a supported
            //     language get a full AST; every other file still becomes a
            //     File node (path/name/extension/size), and .js-family files
            //     are light-linked (import/require → Imports_File_File) in a
            //     post-pass. Oversized files are already skipped above and
            //     build/dependency dirs are pruned by should_skip.
            //     source: "the pipeline should index any kind of files" — so
            //     every file a session touches is navigable in the graph.
            match opts.language_filter {
                Some(filter) => {
                    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                        if Language::from_extension(ext) == Some(filter) {
                            out.push(path);
                        }
                    }
                }
                None => out.push(path),
            }
        }
    }
    Ok(())
}

/// Returns true for directories that should be skipped during walk.
///
/// Covers build / dependency / cache directories across the languages
/// the indexer supports. source: empirical — without ``build`` and
/// ``Pods`` excluded, an Android repo's ``app/build/intermediates/``
/// alone produces tens of thousands of stat() calls and many hundred
/// MB of *.dex / *.aar / *.jar files that the indexer rejects per-file
/// after walking into them. Filtering at the directory level avoids
/// the descent entirely.
fn should_skip(name: &str, include_dependencies: bool) -> bool {
    // `.git` is never source — its object store is large and binary — so it is
    // skipped even in full-dependency mode. source: checkpoint 2026-07-04.
    if name == ".git" {
        return true;
    }
    // Full-dependency mode: descend into vendored/build/cache dirs so the
    // graph covers node_modules, .venv, vendor, target, etc.
    if include_dependencies {
        return false;
    }
    name.starts_with('.')
        // Rust
        || name == "target"
        // JS / TS / Node
        || name == "node_modules"
        // Python
        || name == "__pycache__"
        || name == ".venv"
        || name == "venv"
        || name == ".pytest_cache"
        || name == ".mypy_cache"
        || name == ".tox"
        || name == ".eggs"
        // JVM / Android (Gradle / Maven / Eclipse / IntelliJ)
        || name == "build"
        || name == "out"
        || name == ".gradle"
        || name == ".idea"
        // Apple (Xcode / SPM / CocoaPods / Carthage)
        || name == "Pods"
        || name == "DerivedData"
        || name == ".build"
        || name == "Carthage"
        || name == ".swiftpm"
        // Go
        || name == "vendor"
        // General build output
        || name == "dist"
        || name == "bin"
        || name == "obj"
        // Test / coverage
        || name == "coverage"
        || name == ".nyc_output"
    // Other VCS dirs are filtered by ``starts_with('.')``; ``.git`` itself is
    // handled explicitly above so it is excluded in full-dependency mode too.
}
