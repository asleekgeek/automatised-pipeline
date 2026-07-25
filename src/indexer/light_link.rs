// Light cross-file linking for files the AST parsers don't cover.
//
// Under all-file indexing every file becomes a File node, but only files in a
// supported language get a parsed AST (and therefore real Calls/Imports edges).
// JavaScript (.js/.jsx/.mjs/.cjs) has no tree-sitter parser wired in, yet its
// import/require graph is exactly the "what depends on what" signal the
// visualization needs. This module recovers that graph cheaply: a regex-free
// scan for relative `import ... from "X"`, `require("X")` and dynamic
// `import("X")`, resolved against the set of indexed File ids and emitted as
// `Imports_File_File` edges.
//
// Runs as a POST-PASS after the main index loop, so every File node already
// exists — no forward-reference problem (the AST resolver defers cross-file
// edges for the same reason). Best-effort: an unresolved specifier is skipped,
// never an error. source: all-file indexing follow-through.

use crate::graph_store::{GraphStore, PropEdgeList};
use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

/// JS-family extensions that have no AST parser but a meaningful import graph.
const JS_EXTS: &[&str] = &["js", "jsx", "mjs", "cjs"];

/// Markdown extensions — docs that reference other files via `[text](path)`.
const MD_EXTS: &[&str] = &["md", "markdown", "mdx"];

/// Candidate suffixes tried when resolving a bare relative specifier to a
/// concrete indexed File id (Node-style resolution, minus package lookup).
const JS_RESOLVE_SUFFIXES: &[&str] =
    &["", ".js", ".jsx", ".mjs", ".cjs", "/index.js", "/index.mjs"];

/// Emits `Imports_File_File` edges for relative imports in JS-family files.
/// Returns the number of edges created. Never fails the index — resolution
/// misses and per-file read errors are silently skipped.
///
/// Full-index convenience: scan every file and resolve against that same set.
pub(super) fn link_loose_file_imports(
    store: &GraphStore,
    root: &Path,
    files: &[PathBuf],
) -> Result<u64, String> {
    link_file_imports_for(store, root, files, files)
}

/// Incremental-friendly core: scan only `sources` for import/reference
/// specifiers, but resolve each specifier against the id set of `all_files`
/// (the full current file list). This is the split the incremental pass needs —
/// after a partial re-parse it must re-derive the light links *out of* the
/// changed/added files while still resolving them against every File node that
/// exists in the graph, not just the handful that were re-parsed.
///
/// Preconditions: every File node for `all_files` already exists in `store`
/// (this runs as a post-pass, like the full-index caller). Postconditions on
/// `Ok(n)`: `n` `Imports_File_File`/`References_File_File` edges whose source is
/// in `sources` and whose resolved target is in `all_files` have been inserted;
/// per-file read errors and unresolved specifiers are skipped, never fatal.
pub(super) fn link_file_imports_for(
    store: &GraphStore,
    root: &Path,
    sources: &[PathBuf],
    all_files: &[PathBuf],
) -> Result<u64, String> {
    // Set of indexed File ids (repo-relative, forward-slash) for O(1) lookup.
    let file_ids: HashSet<String> = all_files.iter().map(|f| rel_id(root, f)).collect();

    let mut seen: HashSet<(String, String)> = HashSet::new();
    // Staged per rel_table and bulk-inserted once at the end instead of one
    // insert_edge round-trip per specifier — same edge set as before, just
    // batched. source: ADR-4253701 §Decision 2 (levier 2, light_link.rs:93).
    let mut edges_by_table: HashMap<&'static str, PropEdgeList> = HashMap::new();

    for file_path in sources {
        let ext = file_path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        // Decide the link kind from the file type:
        //   JS family   → import/require   → Imports_File_File   (code dep)
        //   Markdown     → [text](path)     → References_File_File (doc link)
        let (rel_table, method, targets): (&'static str, &str, Vec<String>) =
            if JS_EXTS.contains(&ext.as_str()) {
                let src = match read_text(file_path) {
                    Some(s) => s,
                    None => continue,
                };
                (
                    "Imports_File_File",
                    "js-regex",
                    extract_relative_specifiers(&src),
                )
            } else if MD_EXTS.contains(&ext.as_str()) {
                let src = match read_text(file_path) {
                    Some(s) => s,
                    None => continue,
                };
                (
                    "References_File_File",
                    "md-link",
                    extract_markdown_targets(&src),
                )
            } else {
                continue;
            };

        let from_id = rel_id(root, file_path);
        for spec in targets {
            let Some(target) = resolve_specifier(&from_id, &spec, &file_ids) else {
                continue;
            };
            if target == from_id {
                continue;
            }
            if !seen.insert((from_id.clone(), target.clone())) {
                continue;
            }
            // Both rels are resolution rels (confidence DOUBLE,
            // resolution_method STRING) — mirror the AST resolver prop shape.
            let props = vec![
                ("confidence".to_string(), "0.9".to_string()),
                ("resolution_method".to_string(), format!("'{method}'")),
            ];
            edges_by_table
                .entry(rel_table)
                .or_default()
                .push((from_id.clone(), target, props));
        }
    }

    let mut edge_count: u64 = 0;
    for (rel_table, edges) in &edges_by_table {
        edge_count += store.bulk_insert_edges(rel_table, edges)?;
    }
    Ok(edge_count)
}

/// Reads a file as UTF-8 text within the parse cap; None on binary / oversize.
fn read_text(path: &Path) -> Option<String> {
    let s = std::fs::read_to_string(path).ok()?;
    if s.len() as u64 > super::MAX_PARSE_BYTES {
        return None;
    }
    Some(s)
}

/// Repo-relative, forward-slash File id for a path under `root`.
pub(super) fn rel_id(root: &Path, file: &Path) -> String {
    file.strip_prefix(root)
        .unwrap_or(file)
        .to_string_lossy()
        .replace('\\', "/")
}

/// Extracts the quoted specifiers of relative imports/requires on each line.
/// Only relative specifiers (starting with `.`) are returned — bare package
/// names ("react") have no File node to link to. Comment lines are skipped.
fn extract_relative_specifiers(src: &str) -> Vec<String> {
    let mut specs = Vec::new();
    for raw in src.lines() {
        let line = raw.trim_start();
        if line.starts_with("//") || line.starts_with('*') || line.starts_with("/*") {
            continue;
        }
        for anchor in [" from ", "from\"", "from'", "require(", "import("] {
            let mut search_from = 0usize;
            while let Some(rel_pos) = raw[search_from..].find(anchor) {
                let after = search_from + rel_pos + anchor.len();
                if let Some(spec) = first_quoted(&raw[after..]) {
                    if spec.starts_with('.') {
                        specs.push(spec);
                    }
                }
                search_from = after;
            }
        }
    }
    specs
}

/// Extracts the URL targets of inline Markdown links `[text](url)` that point
/// at other files in the repo. External URLs (`http`, `mailto:`, protocol-
/// relative `//`), in-page anchors (`#…`), and absolute paths (`/…`) are
/// dropped — only repo-local references remain. A trailing `#anchor` or
/// `"title"` on the URL is stripped.
fn extract_markdown_targets(src: &str) -> Vec<String> {
    let mut targets = Vec::new();
    let bytes = src.as_bytes();
    let mut i = 0;
    // Scan for the `](` sequence that opens a Markdown link destination.
    while i + 1 < bytes.len() {
        if bytes[i] == b']' && bytes[i + 1] == b'(' {
            let start = i + 2;
            let mut j = start;
            let mut depth = 1; // balance nested parens in the URL
            while j < bytes.len() {
                match bytes[j] {
                    b'(' => depth += 1,
                    b')' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
                j += 1;
            }
            if j <= bytes.len() && j > start {
                let raw = &src[start..j];
                if let Some(url) = clean_md_url(raw) {
                    targets.push(url);
                }
            }
            i = j + 1;
            continue;
        }
        i += 1;
    }
    targets
}

/// Normalizes a Markdown link destination to a repo-local path, or None if it
/// is external / an anchor / absolute.
fn clean_md_url(raw: &str) -> Option<String> {
    let mut url = raw.trim();
    // Drop an optional title: `(path "Title")`.
    if let Some(sp) = url.find(char::is_whitespace) {
        url = &url[..sp];
    }
    // Drop a fragment / query.
    url = url.split('#').next().unwrap_or(url);
    url = url.split('?').next().unwrap_or(url);
    if url.is_empty() {
        return None;
    }
    let lower = url.to_ascii_lowercase();
    if lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("mailto:")
        || lower.starts_with("tel:")
        || url.starts_with("//")
        || url.starts_with('/')
    {
        return None;
    }
    Some(url.to_string())
}

/// Reads the first single/double/back-quoted string at the start of `s`,
/// skipping leading whitespace and a single optional `(`. Returns the inner
/// text, or None if `s` does not begin with a quoted token.
fn first_quoted(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'(') {
        i += 1;
    }
    if i >= bytes.len() {
        return None;
    }
    let quote = bytes[i];
    if quote != b'"' && quote != b'\'' && quote != b'`' {
        return None;
    }
    i += 1;
    let begin = i;
    while i < bytes.len() && bytes[i] != quote {
        i += 1;
    }
    if i >= bytes.len() {
        return None;
    }
    Some(s[begin..i].to_string())
}

/// Resolves a relative specifier (`./x`, `../y/z`) against the set of indexed
/// File ids using Node-style suffix resolution. Returns the matched File id.
fn resolve_specifier(from_id: &str, spec: &str, file_ids: &HashSet<String>) -> Option<String> {
    let from_dir = Path::new(from_id).parent().unwrap_or_else(|| Path::new(""));
    // Try the specifier relative to the referrer's directory first, then as a
    // repo-root path (Markdown links are often written root-relative). For each
    // base, try Node-style suffixes so bare JS specifiers (`./util`) resolve.
    for base_path in [normalize(&from_dir.join(spec)), normalize(Path::new(spec))] {
        let base = base_path.to_string_lossy().replace('\\', "/");
        if base.is_empty() {
            continue;
        }
        for suffix in JS_RESOLVE_SUFFIXES {
            let candidate = format!("{base}{suffix}");
            if file_ids.contains(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

/// Lexically normalizes a path: resolves `.` and `..` components without
/// touching the filesystem (the targets live in the indexed-id set, not on
/// disk relative to cwd).
fn normalize(p: &Path) -> PathBuf {
    let mut out: Vec<std::ffi::OsString> = Vec::new();
    for comp in p.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(s) => out.push(s.to_os_string()),
            _ => {}
        }
    }
    let mut pb = PathBuf::new();
    for s in out {
        pb.push(s);
    }
    pb
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_relative_import_and_require() {
        let src = r#"
            import { a } from './util.js';
            import b from "../core/b";
            const c = require('./c');
            const dyn = import("./d.mjs");
            import react from "react";   // bare — must be ignored
            // import x from './commented';
        "#;
        let specs = extract_relative_specifiers(src);
        assert!(specs.contains(&"./util.js".to_string()));
        assert!(specs.contains(&"../core/b".to_string()));
        assert!(specs.contains(&"./c".to_string()));
        assert!(specs.contains(&"./d.mjs".to_string()));
        assert!(!specs.iter().any(|s| s == "react"));
    }

    #[test]
    fn resolves_with_node_suffixes() {
        let ids: HashSet<String> = ["ui/js/app.js", "ui/js/util.js", "ui/core/index.js"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            resolve_specifier("ui/js/app.js", "./util", &ids),
            Some("ui/js/util.js".to_string())
        );
        assert_eq!(
            resolve_specifier("ui/js/app.js", "../core", &ids),
            Some("ui/core/index.js".to_string())
        );
        assert_eq!(resolve_specifier("ui/js/app.js", "./missing", &ids), None);
    }
}
