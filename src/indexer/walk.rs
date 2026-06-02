use crate::parser::Language;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Directory walking
// ---------------------------------------------------------------------------

/// Recursively collects source files, skipping hidden dirs, target/, node_modules/.
/// When `language_filter` is Some, only collects files for that language.
/// When None, collects all files with recognized extensions.
///
/// Symlinks are intentionally NOT followed — source: security hardening (C4).
/// This prevents a symlink inside the codebase from causing `read_dir` to
/// silently traverse outside the tree (e.g. to `/etc/passwd` or `~/.ssh`).
pub(super) fn collect_source_files(
    root: &Path,
    language_filter: Option<Language>,
) -> Result<Vec<PathBuf>, String> {
    let mut result = Vec::new();
    walk_dir_recursive(root, &mut result, language_filter, 0)?;
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
    language_filter: Option<Language>,
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
        if should_skip(&name_str) {
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
            walk_dir_recursive(&path, out, language_filter, depth + 1)?;
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
            if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                let detected = Language::from_extension(ext);
                match (language_filter, detected) {
                    (Some(filter), Some(lang)) if filter == lang => out.push(path),
                    (None, Some(_)) => out.push(path),
                    _ => {}
                }
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
fn should_skip(name: &str) -> bool {
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
        // VCS already filtered by ``starts_with('.')`` (covers .git)
}
