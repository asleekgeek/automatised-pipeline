// parser::rust::extract::g3 — see ../extract/mod.rs.

use super::super::*; // parent module: Ctx, TS_* consts, kept helpers
use super::*;
use crate::parser::*; // ExtractedNode, ExtractedRef, node_text, qual, LABEL_*, …
use tree_sitter::Node; // sibling extract fns (glob re-export)

/// Emits a `macro_rules!` definition. AP has no dedicated Macro label, so it is
/// recorded as a Constant carrying `is_macro=true` (queryable, and consistent
/// with how macro *invocations* are already tracked as CallSites with a `!`
/// marker). source: tree-sitter-rust v0.23.3 macro_definition.name.
pub(super) fn extract_macro_definition(ctx: &mut ExtractCtx, node: Node, scope: &str) {
    let name = node_field_text(ctx.source, node, "name");
    if name.is_empty() {
        return;
    }
    let qn = qual(scope, &name);
    ctx.nodes.push(ExtractedNode {
        label: LABEL_CONSTANT.to_string(),
        name,
        qualified_name: qn.clone(),
        start_line: node.start_position().row as u64 + 1,
        end_line: node.end_position().row as u64 + 1,
        visibility: extract_visibility(ctx.source, node),
        properties: vec![("is_macro".to_string(), "true".to_string())],
    });
    ctx.refs.push(ExtractedRef {
        kind: "Defines".to_string(),
        from_qualified_name: scope.to_string(),
        to_qualified_name: qn,
    });
}

/// Emits an `extern crate foo;` declaration as an Import (it brings an external
/// crate into scope, the same role as a `use`). The imported name is the `name`
/// field. source: tree-sitter-rust v0.23.3 extern_crate_declaration.
pub(super) fn extract_extern_crate(ctx: &mut ExtractCtx, node: Node, scope: &str) {
    let name = node_field_text(ctx.source, node, "name");
    if name.is_empty() {
        return;
    }
    let qn = qual(scope, &format!("import:{name}"));
    ctx.nodes.push(ExtractedNode {
        label: LABEL_IMPORT.to_string(),
        name: name.clone(),
        qualified_name: qn,
        start_line: node.start_position().row as u64 + 1,
        end_line: node.end_position().row as u64 + 1,
        visibility: extract_visibility(ctx.source, node),
        properties: vec![("path".to_string(), name.clone())],
    });
    ctx.refs.push(ExtractedRef {
        kind: "Imports".to_string(),
        from_qualified_name: scope.to_string(),
        to_qualified_name: name,
    });
}

// ---------------------------------------------------------------------------
// Type alias extraction
// ---------------------------------------------------------------------------

pub(super) fn extract_type_alias(ctx: &mut ExtractCtx, node: Node, scope: &str) {
    let name = node_field_text(ctx.source, node, "name");
    if name.is_empty() {
        return;
    }
    let qn = qual(scope, &name);
    let target = node_field_text(ctx.source, node, "type");
    ctx.nodes.push(ExtractedNode {
        label: LABEL_TYPE_ALIAS.to_string(),
        name,
        qualified_name: qn.clone(),
        start_line: node.start_position().row as u64 + 1,
        end_line: node.end_position().row as u64 + 1,
        visibility: extract_visibility(ctx.source, node),
        properties: vec![("target_type".to_string(), target)],
    });
    ctx.refs.push(ExtractedRef {
        kind: "Defines".to_string(),
        from_qualified_name: scope.to_string(),
        to_qualified_name: qn,
    });
}

// ---------------------------------------------------------------------------
// Use declaration extraction
// ---------------------------------------------------------------------------

pub(super) fn extract_use(ctx: &mut ExtractCtx, node: Node, scope: &str) {
    let arg = match node.child_by_field_name("argument") {
        Some(a) => a,
        None => return,
    };
    // source: tree-sitter-rust grammar — use_declaration::argument may be any
    // of { identifier, scoped_identifier, use_list, scoped_use_list,
    // use_as_clause, use_wildcard }. Brace lists expand into multiple atomic
    // Import nodes so downstream consumers can match individual leaves.
    let leaves = collect_use_leaves(ctx.source, arg, "");
    let start_line = node.start_position().row as u64 + 1;
    let end_line = node.end_position().row as u64 + 1;
    let visibility = extract_visibility(ctx.source, node);
    for (path, alias, is_glob) in leaves {
        emit_import(
            ctx,
            scope,
            path,
            alias,
            is_glob,
            start_line,
            end_line,
            &visibility,
        );
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn emit_import(
    ctx: &mut ExtractCtx,
    scope: &str,
    path: String,
    alias: String,
    is_glob: bool,
    start_line: u64,
    end_line: u64,
    visibility: &str,
) {
    if path.is_empty() {
        return;
    }
    let display_name = if !alias.is_empty() {
        alias.clone()
    } else if is_glob {
        format!("{path}::*")
    } else {
        path.clone()
    };
    let qn = qual(scope, &display_name);
    ctx.nodes.push(ExtractedNode {
        label: LABEL_IMPORT.to_string(),
        name: display_name,
        qualified_name: qn.clone(),
        start_line,
        end_line,
        visibility: visibility.to_string(),
        properties: vec![
            ("path".to_string(), path),
            ("alias".to_string(), alias),
            ("is_glob".to_string(), is_glob.to_string()),
        ],
    });
    ctx.refs.push(ExtractedRef {
        kind: "Defines".to_string(),
        from_qualified_name: scope.to_string(),
        to_qualified_name: qn,
    });
}

/// Walks a `use_declaration` argument subtree and returns one tuple per leaf
/// import in canonical `(path, alias, is_glob)` form. `prefix` is prepended
/// (with `::`) to each path string — this is what carries the scope across
/// nested brace lists like `use a::{b, c::{d, e}};`.
pub(super) fn collect_use_leaves(
    source: &str,
    node: Node,
    prefix: &str,
) -> Vec<(String, String, bool)> {
    match node.kind() {
        "use_list" => walk_use_list_children(source, node, prefix),
        "scoped_use_list" => leaf_from_scoped_use_list(source, node, prefix),
        TS_USE_AS_CLAUSE => vec![leaf_from_use_as_clause(source, node, prefix)],
        TS_USE_WILDCARD => vec![leaf_from_use_wildcard(source, node, prefix)],
        _ => vec![leaf_from_identifier(source, node, prefix)],
    }
}

pub(super) fn leaf_from_scoped_use_list(
    source: &str,
    node: Node,
    prefix: &str,
) -> Vec<(String, String, bool)> {
    let head = node
        .child_by_field_name("path")
        .map(|n| node_text(source, n))
        .unwrap_or_default();
    let new_prefix = join_use_path(prefix, &head);
    match node.child_by_field_name("list") {
        Some(list) => walk_use_list_children(source, list, &new_prefix),
        None => Vec::new(),
    }
}

pub(super) fn leaf_from_use_as_clause(
    source: &str,
    node: Node,
    prefix: &str,
) -> (String, String, bool) {
    let path = node
        .child_by_field_name("path")
        .map(|n| node_text(source, n))
        .unwrap_or_default();
    let alias = node
        .child_by_field_name("alias")
        .map(|n| node_text(source, n))
        .unwrap_or_default();
    (join_use_path(prefix, &path), alias, false)
}

pub(super) fn leaf_from_use_wildcard(
    source: &str,
    node: Node,
    prefix: &str,
) -> (String, String, bool) {
    // use_wildcard text is `<path>::*` (or just `*` when nested in
    // a brace list with an outer prefix). Strip the trailing `::*`
    // if present, otherwise treat the wildcard as attaching to the
    // current prefix verbatim.
    let text = node_text(source, node);
    let stripped = text.trim_end_matches("::*").trim_end_matches('*');
    let stripped = stripped.trim_end_matches("::");
    (join_use_path(prefix, stripped), String::new(), true)
}

pub(super) fn leaf_from_identifier(
    source: &str,
    node: Node,
    prefix: &str,
) -> (String, String, bool) {
    // identifier, scoped_identifier, crate, self, super, etc.
    // Inside a brace list, `self` refers to the brace-list prefix itself
    // (`use std::io::{self, BufRead}` → import `std::io` and `std::io::BufRead`).
    let leaf = node_text(source, node);
    let path = if leaf == "self" && !prefix.is_empty() {
        prefix.to_string()
    } else {
        join_use_path(prefix, &leaf)
    };
    (path, String::new(), false)
}

pub(super) fn walk_use_list_children(
    source: &str,
    list: Node,
    prefix: &str,
) -> Vec<(String, String, bool)> {
    let mut cursor = list.walk();
    let mut out: Vec<(String, String, bool)> = Vec::new();
    for child in list.children(&mut cursor) {
        // Skip punctuation — `{`, `,`, `}` — by filtering on named children.
        if !child.is_named() {
            continue;
        }
        out.extend(collect_use_leaves(source, child, prefix));
    }
    out
}

pub(super) fn join_use_path(prefix: &str, tail: &str) -> String {
    if prefix.is_empty() {
        tail.to_string()
    } else if tail.is_empty() {
        prefix.to_string()
    } else {
        format!("{prefix}::{tail}")
    }
}
