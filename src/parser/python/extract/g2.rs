// parser::python::extract::g2 — see ../extract/mod.rs.

use tree_sitter::Node;
use crate::parser::*;      // ExtractedNode, ExtractedRef, node_text, qual, LABEL_*, …
use super::super::*;       // parent module: Ctx, TS_* consts, kept helpers
use super::*;              // sibling extract fns (glob re-export)


// ---------------------------------------------------------------------------
// Import extraction
// ---------------------------------------------------------------------------

fn extract_import(ctx: &mut ExtractCtx, node: Node, scope: &str) {
    // `import foo` / `import foo.bar` / `import foo as f`
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "dotted_name" {
            let path = node_text(ctx.source, child).replace('.', "::");
            emit_import(ctx, scope, &path, "", false, node);
        } else if child.kind() == "aliased_import" {
            let name_node = child.child_by_field_name("name");
            let alias_node = child.child_by_field_name("alias");
            let path = name_node.map(|n| node_text(ctx.source, n).replace('.', "::")).unwrap_or_default();
            let alias = alias_node.map(|n| node_text(ctx.source, n)).unwrap_or_default();
            emit_import(ctx, scope, &path, &alias, false, node);
        }
    }
}


/// Handles `from __future__ import X [as Y][, Z ...]`. Tree-sitter-python
/// gives this its own node kind (`future_import_statement`) distinct from
/// the generic `import_from_statement`, so it needs its own routing — see
/// BUG #13. The module name is implicit (always `__future__`). Imported
/// names appear as direct identifier/dotted_name/aliased_import children.
fn extract_future_import(ctx: &mut ExtractCtx, node: Node, scope: &str) {
    let module_name = "__future__".to_string();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "identifier" | "dotted_name" => {
                let name = node_text(ctx.source, child);
                // Skip the literal `__future__` token if tree-sitter emits it
                // as an identifier child (depends on grammar version).
                if name == "__future__" || name.is_empty() {
                    continue;
                }
                let full_path = format!("{module_name}::{name}");
                emit_import(ctx, scope, &full_path, "", false, node);
            }
            "aliased_import" => {
                let name_node = child.child_by_field_name("name");
                let alias_node = child.child_by_field_name("alias");
                let name = name_node
                    .map(|n| node_text(ctx.source, n))
                    .unwrap_or_default();
                let alias = alias_node
                    .map(|n| node_text(ctx.source, n))
                    .unwrap_or_default();
                if name.is_empty() {
                    continue;
                }
                let full_path = format!("{module_name}::{name}");
                emit_import(ctx, scope, &full_path, &alias, false, node);
            }
            _ => {}
        }
    }
}


fn extract_import_from(ctx: &mut ExtractCtx, node: Node, scope: &str) {
    // `from foo import bar` / `from foo import *` / `from foo import bar as b`
    let module_name = node.child_by_field_name("module_name")
        .map(|n| node_text(ctx.source, n).replace('.', "::"))
        .unwrap_or_default();

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "wildcard_import" {
            emit_import(ctx, scope, &module_name, "", true, node);
            return;
        }
    }

    // Iterate named imports
    let mut cursor2 = node.walk();
    for child in node.children(&mut cursor2) {
        if child.kind() == "dotted_name" || child.kind() == "identifier" {
            // Skip the module_name node itself
            if Some(child.id()) == node.child_by_field_name("module_name").map(|n| n.id()) {
                continue;
            }
            let name = node_text(ctx.source, child);
            let full_path = if module_name.is_empty() {
                name.clone()
            } else {
                format!("{module_name}::{name}")
            };
            emit_import(ctx, scope, &full_path, "", false, node);
        } else if child.kind() == "aliased_import" {
            let name_node = child.child_by_field_name("name");
            let alias_node = child.child_by_field_name("alias");
            let name = name_node.map(|n| node_text(ctx.source, n)).unwrap_or_default();
            let alias = alias_node.map(|n| node_text(ctx.source, n)).unwrap_or_default();
            let full_path = if module_name.is_empty() {
                name
            } else {
                format!("{module_name}::{name}")
            };
            emit_import(ctx, scope, &full_path, &alias, false, node);
        }
    }
}


fn emit_import(
    ctx: &mut ExtractCtx,
    scope: &str,
    path: &str,
    alias: &str,
    is_glob: bool,
    node: Node,
) {
    if path.is_empty() {
        return;
    }
    let display_name = if !alias.is_empty() {
        alias.to_string()
    } else if is_glob {
        format!("{path}::*")
    } else {
        path.to_string()
    };
    let qn = qual(scope, &display_name);
    ctx.nodes.push(ExtractedNode {
        label: LABEL_IMPORT.to_string(),
        name: display_name,
        qualified_name: qn.clone(),
        start_line: node.start_position().row as u64 + 1,
        end_line: node.end_position().row as u64 + 1,
        visibility: String::new(),
        properties: vec![
            ("path".to_string(), path.to_string()),
            ("alias".to_string(), alias.to_string()),
            ("is_glob".to_string(), is_glob.to_string()),
        ],
    });
    ctx.refs.push(ExtractedRef {
        kind: "Defines".to_string(),
        from_qualified_name: scope.to_string(),
        to_qualified_name: qn,
    });
}


// ---------------------------------------------------------------------------
// Module-level constant extraction (UPPER_SNAKE assignments)
// ---------------------------------------------------------------------------

fn extract_module_constant(ctx: &mut ExtractCtx, expr_stmt: Node, scope: &str) {
    let mut cursor = expr_stmt.walk();
    for child in expr_stmt.children(&mut cursor) {
        if child.kind() != TS_ASSIGNMENT {
            continue;
        }
        let left = match child.child_by_field_name("left") {
            Some(l) if l.kind() == "identifier" => l,
            _ => continue,
        };
        let name = node_text(ctx.source, left);
        if !is_upper_snake_case(&name) {
            continue;
        }
        // Check for type annotation
        let type_ann = child.child_by_field_name("type")
            .map(|n| node_text(ctx.source, n))
            .unwrap_or_default();
        let qn = qual(scope, &name);
        ctx.nodes.push(ExtractedNode {
            label: LABEL_CONSTANT.to_string(),
            name,
            qualified_name: qn.clone(),
            start_line: child.start_position().row as u64 + 1,
            end_line: child.end_position().row as u64 + 1,
            visibility: String::new(),
            properties: vec![("type_annotation".to_string(), type_ann)],
        });
        ctx.refs.push(ExtractedRef {
            kind: "Defines".to_string(),
            from_qualified_name: scope.to_string(),
            to_qualified_name: qn,
        });
    }
}


// ---------------------------------------------------------------------------
// Call-site extraction
// ---------------------------------------------------------------------------

fn extract_call_sites(ctx: &mut ExtractCtx, body: Node, caller_qn: &str) {
    let mut stack = vec![body];
    while let Some(node) = stack.pop() {
        if node.kind() == TS_CALL {
            extract_single_call_site(ctx, node, caller_qn);
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            stack.push(child);
        }
    }
}
