use super::SymbolBatch;
use crate::graph_store::{cypher_str, GraphStore};
use crate::parser::{self, Language};
use std::collections::HashMap;
use std::path::Path;

// ---------------------------------------------------------------------------
// Directory and File node insertion
// ---------------------------------------------------------------------------

/// Inserts Directory nodes for all ancestor dirs of a file (relative to root).
pub(super) fn insert_ancestor_dirs(
    store: &GraphStore,
    root: &Path,
    file_path: &Path,
    seen: &mut std::collections::HashSet<std::path::PathBuf>,
    label_by_qn: &mut HashMap<String, String>,
) -> Result<(), String> {
    let rel = super::relative_path(root, file_path);
    let mut current = std::path::PathBuf::new();
    // Walk each component except the filename itself.
    if let Some(parent) = rel.parent() {
        for component in parent.components() {
            let prev = current.clone();
            current.push(component);
            if seen.contains(&current) {
                continue;
            }
            seen.insert(current.clone());
            let dir_id = current.to_string_lossy();
            let dir_name = component.as_os_str().to_string_lossy();
            insert_directory_node(store, &dir_id, &dir_name)?;
            label_by_qn.insert(dir_id.to_string(), "Directory".into());
            if !prev.as_os_str().is_empty() {
                insert_dir_dir_edge(store, &prev.to_string_lossy(), &dir_id)?;
            }
        }
    }
    Ok(())
}

fn insert_directory_node(store: &GraphStore, id: &str, name: &str) -> Result<(), String> {
    store.insert_node("Directory", &[
        ("id", &cypher_str(id)),
        ("path", &cypher_str(id)),
        ("name", &cypher_str(name)),
    ])
}

pub(super) fn insert_file_node(store: &GraphStore, abs_path: &Path, rel_path: &str) -> Result<(), String> {
    let name = abs_path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let ext = abs_path.extension()
        .map(|e| e.to_string_lossy().to_string())
        .unwrap_or_default();
    let size = std::fs::metadata(abs_path)
        .map(|m| m.len())
        .unwrap_or(0);
    store.insert_node("File", &[
        ("id", &cypher_str(rel_path)),
        ("path", &cypher_str(rel_path)),
        ("name", &cypher_str(&name)),
        ("extension", &cypher_str(&ext)),
        ("size_bytes", &size.to_string()),
        // Inserted before the parse runs; index_single_file backfills the real
        // count via set_file_parse_errors once the file is parsed. 0 = "no
        // errors" and also the correct value for non-code files (never parsed).
        ("parse_errors", "0"),
    ])
}

// ---------------------------------------------------------------------------
// Structural edges: Contains
// ---------------------------------------------------------------------------

pub(super) fn insert_dir_file_edge(store: &GraphStore, rel_path: &Path) -> Result<(), String> {
    let file_id = rel_path.to_string_lossy();
    let parent_id = rel_path.parent()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    if parent_id.is_empty() {
        return Ok(()); // file is at root level, no parent directory node
    }
    store.insert_edge("Contains_Dir_File", &parent_id, &file_id, &[])
}

fn insert_dir_dir_edge(store: &GraphStore, parent_id: &str, child_id: &str) -> Result<(), String> {
    store.insert_edge("Contains_Dir_Dir", parent_id, child_id, &[])
}

// ---------------------------------------------------------------------------
// Single-file indexing: parse → insert nodes → insert edges
// ---------------------------------------------------------------------------

pub(super) fn index_single_file(
    store: &GraphStore,
    batch: &mut SymbolBatch,
    abs_path: &Path,
    rel_path: &str,
    label_by_qn: &mut HashMap<String, String>,
    seen_node_ids: &mut std::collections::HashSet<String>,
) -> Result<(), String> {
    // Detect language FIRST (cheap, no I/O). Under all-file indexing the
    // walker yields every file, so most non-code files reach here: they are
    // NOT parsed (no grammar) — their File node was already inserted by the
    // caller, and any lightweight cross-file links (e.g. .js import/require)
    // are emitted in a dedicated post-pass once every File node exists
    // (forward-reference safe). Returning Ok keeps them out of the error log.
    // source: all-file indexing — every file is a File node; only supported
    // languages get a full AST.
    let ext = abs_path.extension().and_then(|e| e.to_str()).unwrap_or("");
    let lang = match Language::from_extension(ext) {
        Some(l) => l,
        None => return Ok(()),
    };
    let source = std::fs::read_to_string(abs_path)
        .map_err(|e| format!("read {}: {e}", abs_path.display()))?;
    // Defense-in-depth: even if the dir walker let a large file slip (e.g.
    // size changed between lstat and read), refuse to feed it to tree-sitter.
    // source: H2 fix — per-file parse cap, 1 MB is sufficient for all real code.
    if (source.len() as u64) > super::MAX_PARSE_BYTES {
        return Err(format!(
            "file_too_large_for_parser: {} bytes > MAX_PARSE_BYTES {}",
            source.len(), super::MAX_PARSE_BYTES
        ));
    }
    let parsed = parser::parse_file(&source, rel_path, lang)?;
    // Backfill the File node's parse-error count (inserted as 0 by the caller).
    // Only issue the update when non-zero — the common clean-parse case pays no
    // extra query. source: stages/stage-3.md §10.5.
    if parsed.parse_errors > 0 {
        set_file_parse_errors(store, rel_path, parsed.parse_errors)?;
    }
    accumulate_parsed_nodes(batch, &parsed.nodes, label_by_qn, seen_node_ids, lang.as_str());
    accumulate_parsed_edges(batch, &parsed.refs, label_by_qn);
    Ok(())
}

/// Records the tree-sitter parse-error count on an already-inserted File node.
/// The File node is created eagerly (before the parse) with parse_errors = 0;
/// this flips it to the real count so a degraded parse is visible downstream.
fn set_file_parse_errors(store: &GraphStore, rel_path: &str, errors: u32) -> Result<(), String> {
    let cypher = format!(
        "MATCH (f:File) WHERE f.id = {} SET f.parse_errors = {}",
        cypher_str(rel_path),
        errors
    );
    store.execute_query(&cypher).map(|_| ())
}

// ---------------------------------------------------------------------------
// Node insertion from parsed results
// ---------------------------------------------------------------------------

fn accumulate_parsed_nodes(
    batch: &mut SymbolBatch,
    nodes: &[parser::ExtractedNode],
    label_by_qn: &mut HashMap<String, String>,
    seen_node_ids: &mut std::collections::HashSet<String>,
    language: &str,
) {
    // Accumulate into the cross-file batch (flushed in large bulk calls).
    // source: Fermi audit — per-row CREATE was ~100x slower than batched;
    // the April 2026 scalability audit further found per-FILE batching still
    // dominated indexing time, so accumulation now spans files.
    //
    // Defensive dedup: parsers should produce unique ids per node, but a bug
    // there would abort the whole bulk flush (LadybugDB rejects duplicate
    // primary keys atomically), taking down every file in the batch, not one.
    // The id set is global to the run, so cross-file collisions are caught too.
    for node in nodes {
        if !seen_node_ids.insert(node.qualified_name.clone()) {
            eprintln!(
                "indexer: dropped duplicate-id {} node '{}'",
                node.label, node.qualified_name
            );
            continue;
        }
        label_by_qn.insert(node.qualified_name.clone(), node.label.clone());
        let props = build_node_properties(node, language);
        batch.push_node(&node.label, props);
    }
}

/// Builds the full property list for a node, mapping ExtractedNode fields
/// to the schema columns defined in graph_store.rs node_table_ddl().
///
/// source: Spike B' BUG #5 fix — `language` is appended for every
/// symbol-bearing label (anything that isn't File / Directory) so consumers
/// can filter by language without re-parsing.
fn build_node_properties(node: &parser::ExtractedNode, language: &str) -> Vec<(String, String)> {
    let mut props = vec![("id".to_string(), cypher_str(&node.qualified_name))];
    if has_name_col(&node.label) {
        props.push(("name".to_string(), cypher_str(&node.name)));
    }
    if has_qualified_name_col(&node.label) {
        props.push(("qualified_name".to_string(), cypher_str(&node.qualified_name)));
    }
    if has_line_cols(&node.label) {
        props.push(("start_line".to_string(), node.start_line.to_string()));
        props.push(("end_line".to_string(), node.end_line.to_string()));
    }
    if has_visibility_col(&node.label) {
        props.push(("visibility".to_string(), cypher_str(&node.visibility)));
    }
    append_label_properties(&mut props, node);
    if has_language_col(&node.label) {
        props.push(("language".to_string(), cypher_str(language)));
    }
    props
}

/// True for every symbol-bearing node label (everything carrying source-code
/// semantics). File and Directory are excluded — they cross language boundaries.
fn has_language_col(label: &str) -> bool {
    matches!(
        label,
        "Function" | "Method" | "Struct" | "Enum" | "Variant" | "Trait"
            | "Field" | "Constant" | "TypeAlias" | "Import" | "CallSite"
    )
}

/// Maps parser extra properties to schema columns by label.
fn append_label_properties(props: &mut Vec<(String, String)>, node: &parser::ExtractedNode) {
    let find = |key: &str| -> String {
        node.properties.iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    };
    match node.label.as_str() {
        "Function" => {
            props.push(("is_async".to_string(), find("is_async")));
        }
        "Method" => {
            props.push(("is_async".to_string(), find("is_async")));
            props.push(("receiver_type".to_string(), cypher_str(&find("receiver_type"))));
            // source: implements fix — trait_name set by the parser on methods
            // inside `impl Trait for Type` blocks; resolve_implements reads it.
            props.push(("trait_name".to_string(), cypher_str(&find("trait_name"))));
        }
        "Field" => {
            props.push(("type_annotation".to_string(), cypher_str(&find("type_annotation"))));
        }
        "Constant" => {
            props.push(("type_annotation".to_string(), cypher_str(&find("type_annotation"))));
        }
        "TypeAlias" => {
            props.push(("target_type".to_string(), cypher_str(&find("target_type"))));
        }
        // source: Spike B' BUG #9 — bases CSV emitted by parser/python.rs
        // for class/struct/trait/enum nodes; consumed by resolver.resolve_extends.
        // implements fix — `implements` CSV (derived/declared trait names) is
        // the parallel column consumed by resolver.resolve_implements.
        "Struct" | "Enum" | "Trait" => {
            props.push(("bases".to_string(), cypher_str(&find("bases"))));
            props.push(("implements".to_string(), cypher_str(&find("implements"))));
        }
        "Import" => {
            props.push(("path".to_string(), cypher_str(&find("path"))));
            props.push(("alias".to_string(), cypher_str(&find("alias"))));
            props.push(("is_glob".to_string(), find("is_glob")));
            // §10.1 span for the import statement; §10.4 is_resolved starts false
            // and is flipped by the resolver's resolve pass.
            props.push(("start_line".to_string(), node.start_line.to_string()));
            props.push(("end_line".to_string(), node.end_line.to_string()));
            props.push(("is_resolved".to_string(), "false".to_string()));
        }
        "CallSite" => {
            props.push(("callee_name".to_string(), cypher_str(&find("callee_name"))));
            props.push(("line".to_string(), node.start_line.to_string()));
            props.push(("col".to_string(), "0".to_string()));
            // §10.4 is_resolved starts false; the resolver flips it to true when
            // it emits the resolved Calls edge for this site.
            props.push(("is_resolved".to_string(), "false".to_string()));
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Edge insertion from parsed results
// ---------------------------------------------------------------------------

fn accumulate_parsed_edges(
    batch: &mut SymbolBatch,
    refs: &[parser::ExtractedRef],
    label_by_qn: &HashMap<String, String>,
) {
    // source: Fermi audit — per-edge probe_node_label was firing up to 9
    // MATCH queries; the in-memory label_by_qn map eliminates them entirely.
    // Edges accumulate into the cross-file batch and flush in large bulk calls.
    for edge_ref in refs {
        let table = resolve_edge_table(
            &edge_ref.kind,
            &edge_ref.from_qualified_name,
            &edge_ref.to_qualified_name,
            label_by_qn,
        );
        let table_name = match table {
            Some(t) => t,
            None => continue,
        };
        let props = structural_provenance_props(&table_name);
        batch.push_edge(
            &table_name,
            edge_ref.from_qualified_name.clone(),
            edge_ref.to_qualified_name.clone(),
            props,
        );
    }
}

/// Structural edges (Defines_*, HasMethod_*, HasField_*, HasVariant_*) are
/// ground-truth AST facts; they carry (confidence=1.0, "direct-ast") so
/// downstream consumers see uniform provenance across structural and
/// resolution edges. Resolution edges get their provenance from the resolver.
/// source: Spike B' BUG #4 — see graph_store::is_structural_provenance_rel.
fn structural_provenance_props(table_name: &str) -> Vec<(String, String)> {
    if table_name.starts_with("Defines_")
        || table_name.starts_with("HasMethod_")
        || table_name.starts_with("HasField_")
        || table_name.starts_with("HasVariant_")
    {
        vec![
            ("confidence".to_string(), "1.0".to_string()),
            ("resolution_method".to_string(), cypher_str("direct-ast")),
        ]
    } else {
        Vec::new()
    }
}

/// Resolves the multi-table edge name using the in-memory label map.
/// This eliminates the per-edge Cypher probes that used to dominate
/// indexing cost on large codebases.
fn resolve_edge_table(
    kind: &str,
    from_qn: &str,
    to_qn: &str,
    label_by_qn: &HashMap<String, String>,
) -> Option<String> {
    match kind {
        "Defines" => resolve_defines_table(from_qn, to_qn, label_by_qn),
        "HasMethod" => resolve_has_method_table(from_qn, label_by_qn),
        "HasField" => resolve_has_field_table(from_qn, label_by_qn),
        "HasVariant" => Some("HasVariant_Enum_Variant".to_string()),
        // 3b: Extends refs are deferred to the resolver pass — skip here.
        "Extends" => None,
        _ => None,
    }
}

fn resolve_defines_table(
    from_qn: &str,
    to_qn: &str,
    label_by_qn: &HashMap<String, String>,
) -> Option<String> {
    // source: Spike B' BUG #12 fix — added Function/Method to from-candidates
    // and CallSite to to-candidates. Previously the parser-emitted
    // `Defines: Function → CallSite` edges were silently dropped here because
    // the whitelist excluded both endpoints. CallSite nodes were orphans.
    let from_label = lookup_label_among(
        from_qn,
        label_by_qn,
        &["File", "Module", "Function", "Method"],
    )?;
    let to_candidates = &[
        "Function", "Struct", "Enum", "Trait", "Constant",
        "TypeAlias", "Module", "Import", "CallSite",
    ];
    let to_label = lookup_label_among(to_qn, label_by_qn, to_candidates)?;
    let table = format!("Defines_{from_label}_{to_label}");
    if is_valid_rel_table(&table) { Some(table) } else { None }
}

fn resolve_has_method_table(
    from_qn: &str,
    label_by_qn: &HashMap<String, String>,
) -> Option<String> {
    let from_label = lookup_label_among(from_qn, label_by_qn, &["Struct", "Enum", "Trait"])?;
    let table = format!("HasMethod_{from_label}_Method");
    if is_valid_rel_table(&table) { Some(table) } else { None }
}

fn resolve_has_field_table(
    from_qn: &str,
    label_by_qn: &HashMap<String, String>,
) -> Option<String> {
    let from_label = lookup_label_among(from_qn, label_by_qn, &["Struct", "Enum"])?;
    let table = format!("HasField_{from_label}_Field");
    if is_valid_rel_table(&table) { Some(table) } else { None }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Looks up the known label for an id and returns it only if it is one of the
/// allowed candidates for the edge kind. No DB access.
fn lookup_label_among(
    id: &str,
    label_by_qn: &HashMap<String, String>,
    candidates: &[&str],
) -> Option<String> {
    let lbl = label_by_qn.get(id)?;
    if candidates.iter().any(|c| *c == lbl.as_str()) {
        Some(lbl.clone())
    } else {
        None
    }
}

/// Checks if a rel table name exists in the known schema. Single source
/// of truth is `graph_store::REL_TABLES`; this thin shim avoids drift.
fn is_valid_rel_table(name: &str) -> bool {
    crate::graph_store::is_known_rel_table(name)
}

// Schema awareness — source: graph_store.rs node_table_ddl().
// Each function returns true iff the label's CREATE NODE TABLE includes that column.

fn has_name_col(label: &str) -> bool {
    // All node tables have `name` EXCEPT Import (path/alias only).
    // CallSite stores callee_name via properties, not via 'name' column.
    !matches!(label, "Import" | "CallSite")
}

fn has_qualified_name_col(label: &str) -> bool {
    matches!(label,
        "Module" | "Function" | "Method" | "Struct" | "Enum" | "Variant" |
        "Trait" | "Constant" | "TypeAlias"
    )
}

fn has_line_cols(label: &str) -> bool {
    // source: stages/stage-3.md §10.1 — every symbol carries its span. Variant,
    // Field, Constant and TypeAlias now have span columns too (Import keeps its
    // span via append_label_properties, alongside path/alias). CallSite records
    // position via its own line/col columns, not start_line/end_line.
    matches!(
        label,
        "Function" | "Method" | "Struct" | "Enum" | "Trait"
            | "Variant" | "Field" | "Constant" | "TypeAlias"
    )
}

fn has_visibility_col(label: &str) -> bool {
    matches!(label,
        "Function" | "Method" | "Struct" | "Enum" | "Trait" | "Field"
    )
}
