// parser::typescript::extract::g3 — see ../extract/mod.rs.

use tree_sitter::Node;
use crate::parser::*;      // ExtractedNode, ExtractedRef, node_text, qual, LABEL_*, …
use super::super::*;       // parent module: Ctx, TS_* consts, kept helpers
use super::*;              // sibling extract fns (glob re-export)


pub(super) fn extract_import_clause(
    ctx: &mut ExtractCtx,
    clause: Node,
    scope: &str,
    module_path: &str,
    import_node: Node,
) {
    let mut cursor = clause.walk();
    for child in clause.children(&mut cursor) {
        match child.kind() {
            "identifier" => {
                // Default import: import Foo from 'bar'
                let name = node_text(ctx.source, child);
                let full_path = format!("{module_path}::default");
                emit_ts_import(ctx, scope, &full_path, &name, false, import_node);
            }
            "named_imports" => {
                extract_named_imports(ctx, child, scope, module_path, import_node);
            }
            "namespace_import" => {
                // import * as Foo from 'bar'
                let alias = {
                    let mut found = child.child_by_field_name("name");
                    if found.is_none() {
                        let mut c = child.walk();
                        found = child.children(&mut c).find(|n| n.kind() == "identifier");
                    }
                    found.map(|n| node_text(ctx.source, n)).unwrap_or_default()
                };
                emit_ts_import(ctx, scope, module_path, &alias, true, import_node);
            }
            _ => {}
        }
    }
}


pub(super) fn extract_named_imports(
    ctx: &mut ExtractCtx,
    node: Node,
    scope: &str,
    module_path: &str,
    import_node: Node,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "import_specifier" {
            let name_node = child.child_by_field_name("name");
            let alias_node = child.child_by_field_name("alias");
            let name = name_node.map(|n| node_text(ctx.source, n)).unwrap_or_default();
            let alias = alias_node.map(|n| node_text(ctx.source, n)).unwrap_or_default();
            let full_path = format!("{module_path}::{name}");
            emit_ts_import(ctx, scope, &full_path, &alias, false, import_node);
        }
    }
}


pub(super) fn emit_ts_import(
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
// Export statement extraction
// ---------------------------------------------------------------------------

pub(super) fn extract_export(ctx: &mut ExtractCtx, node: Node, scope: &str) {
    // Export wraps another declaration — extract the inner with is_exported=true
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            TS_FUNC_DECL | TS_GENERATOR_FUNC_DECL => extract_function(ctx, child, scope, true),
            TS_CLASS_DECL | TS_ABSTRACT_CLASS_DECL => extract_class(ctx, child, scope, true),
            TS_INTERFACE_DECL => extract_interface(ctx, child, scope, true),
            TS_ENUM_DECL => extract_enum(ctx, child, scope, true),
            TS_TYPE_ALIAS_DECL => extract_type_alias(ctx, child, scope, true),
            TS_LEXICAL_DECL | TS_VARIABLE_DECL => extract_lexical_decl(ctx, child, scope, true),
            _ => {}
        }
    }
}


// ---------------------------------------------------------------------------
// Lexical declaration extraction (const/let)
// ---------------------------------------------------------------------------

pub(super) fn extract_lexical_decl(ctx: &mut ExtractCtx, node: Node, scope: &str, is_exported: bool) {
    let is_const = is_const_declaration(ctx.source, node);
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == TS_VAR_DECLARATOR {
            extract_variable_declarator(ctx, child, scope, is_const, is_exported);
        }
    }
}


pub(super) fn extract_variable_declarator(
    ctx: &mut ExtractCtx,
    node: Node,
    scope: &str,
    is_const: bool,
    is_exported: bool,
) {
    let name = node_field_text(ctx.source, node, "name");
    if name.is_empty() {
        return;
    }

    let value = node.child_by_field_name("value");
    let is_arrow = value.map_or(false, |v| v.kind() == TS_ARROW_FUNC);

    if is_arrow {
        // const foo = () => {} — extract as Function
        let arrow = value.unwrap();
        let qn = qual(scope, &name);
        let vis = if is_exported { "pub".to_string() } else { String::new() };
        let is_async = has_async_keyword(ctx.source, arrow);
        ctx.nodes.push(ExtractedNode {
            label: LABEL_FUNCTION.to_string(),
            name: name.clone(),
            qualified_name: qn.clone(),
            start_line: node.start_position().row as u64 + 1,
            end_line: node.end_position().row as u64 + 1,
            visibility: vis,
            properties: vec![("is_async".to_string(), is_async.to_string())],
        });
        ctx.refs.push(ExtractedRef {
            kind: "Defines".to_string(),
            from_qualified_name: scope.to_string(),
            to_qualified_name: qn.clone(),
        });
        if let Some(body) = arrow.child_by_field_name("body") {
            extract_call_sites(ctx, body, &qn);
        }
    } else if is_const {
        // const FOO = ... — extract as Constant
        let qn = qual(scope, &name);
        let vis = if is_exported { "pub".to_string() } else { String::new() };
        let type_ann = node.child_by_field_name("type")
            .map(|n| node_text(ctx.source, n))
            .unwrap_or_default();
        ctx.nodes.push(ExtractedNode {
            label: LABEL_CONSTANT.to_string(),
            name,
            qualified_name: qn.clone(),
            start_line: node.start_position().row as u64 + 1,
            end_line: node.end_position().row as u64 + 1,
            visibility: vis,
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

pub(super) fn extract_call_sites(ctx: &mut ExtractCtx, body: Node, caller_qn: &str) {
    let mut stack = vec![body];
    while let Some(node) = stack.pop() {
        if node.kind() == TS_CALL_EXPR {
            extract_single_call_site(ctx, node, caller_qn);
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            stack.push(child);
        }
    }
}
