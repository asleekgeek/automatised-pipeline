// parser::typescript::extract::g1 — see ../extract/mod.rs.

use tree_sitter::Node;
use crate::parser::*;      // ExtractedNode, ExtractedRef, node_text, qual, LABEL_*, …
use super::super::*;       // parent module: Ctx, TS_* consts, kept helpers
use super::*;              // sibling extract fns (glob re-export)


// ---------------------------------------------------------------------------
// Top-level extraction
// ---------------------------------------------------------------------------

fn extract_top_level(ctx: &mut ExtractCtx, parent: Node, scope: &str, is_exported: bool) {
    let mut cursor = parent.walk();
    for child in parent.children(&mut cursor) {
        match child.kind() {
            TS_FUNC_DECL | TS_GENERATOR_FUNC_DECL => {
                extract_function(ctx, child, scope, is_exported)
            }
            TS_CLASS_DECL | TS_ABSTRACT_CLASS_DECL => {
                extract_class(ctx, child, scope, is_exported)
            }
            TS_INTERFACE_DECL => extract_interface(ctx, child, scope, is_exported),
            TS_ENUM_DECL => extract_enum(ctx, child, scope, is_exported),
            TS_TYPE_ALIAS_DECL => extract_type_alias(ctx, child, scope, is_exported),
            TS_IMPORT_STMT => extract_import(ctx, child, scope),
            TS_EXPORT_STMT => extract_export(ctx, child, scope),
            TS_LEXICAL_DECL | TS_VARIABLE_DECL => {
                extract_lexical_decl(ctx, child, scope, is_exported)
            }
            _ => {}
        }
    }
}


// ---------------------------------------------------------------------------
// Function extraction
// ---------------------------------------------------------------------------

fn extract_function(ctx: &mut ExtractCtx, node: Node, scope: &str, is_exported: bool) {
    let name = node_field_text(ctx.source, node, "name");
    if name.is_empty() {
        return;
    }
    let qn = qual(scope, &name);
    let vis = if is_exported || has_export_keyword(node) { "pub".to_string() } else { String::new() };
    let is_async = has_async_keyword(ctx.source, node);
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
    if let Some(body) = node.child_by_field_name("body") {
        extract_call_sites(ctx, body, &qn);
    }
}


// ---------------------------------------------------------------------------
// Class extraction
// ---------------------------------------------------------------------------

fn extract_class(ctx: &mut ExtractCtx, node: Node, scope: &str, is_exported: bool) {
    let name = node_field_text(ctx.source, node, "name");
    if name.is_empty() {
        return;
    }
    let qn = qual(scope, &name);
    let vis = if is_exported || has_export_keyword(node) { "pub".to_string() } else { String::new() };

    ctx.nodes.push(ExtractedNode {
        label: LABEL_STRUCT.to_string(),
        name: name.clone(),
        qualified_name: qn.clone(),
        start_line: node.start_position().row as u64 + 1,
        end_line: node.end_position().row as u64 + 1,
        visibility: vis,
        properties: vec![],
    });
    ctx.refs.push(ExtractedRef {
        kind: "Defines".to_string(),
        from_qualified_name: scope.to_string(),
        to_qualified_name: qn.clone(),
    });

    // Extract heritage (extends/implements)
    extract_class_heritage(ctx, node, &qn);

    // Extract class body: methods and fields
    if let Some(body) = node.child_by_field_name("body") {
        extract_class_body(ctx, body, &qn);
    }
}


fn extract_class_heritage(ctx: &mut ExtractCtx, class_node: Node, class_qn: &str) {
    let mut cursor = class_node.walk();
    for child in class_node.children(&mut cursor) {
        if child.kind() == "class_heritage" {
            let mut hcursor = child.walk();
            for heritage in child.children(&mut hcursor) {
                if heritage.kind() == "extends_clause" {
                    extract_heritage_clause(ctx, heritage, class_qn, "Extends");
                } else if heritage.kind() == "implements_clause" {
                    extract_heritage_clause(ctx, heritage, class_qn, "Implements");
                }
            }
        }
    }
}


fn extract_heritage_clause(
    ctx: &mut ExtractCtx,
    clause: Node,
    class_qn: &str,
    edge_kind: &str,
) {
    let mut cursor = clause.walk();
    for child in clause.children(&mut cursor) {
        if child.kind() == "identifier" || child.kind() == "type_identifier" {
            let name = node_text(ctx.source, child);
            if !name.is_empty() {
                ctx.refs.push(ExtractedRef {
                    kind: edge_kind.to_string(),
                    from_qualified_name: class_qn.to_string(),
                    to_qualified_name: name,
                });
            }
        } else if child.kind() == "generic_type" {
            // class Foo extends Bar<T> — extract "Bar"
            if let Some(type_name) = child.child_by_field_name("name") {
                let name = node_text(ctx.source, type_name);
                if !name.is_empty() {
                    ctx.refs.push(ExtractedRef {
                        kind: edge_kind.to_string(),
                        from_qualified_name: class_qn.to_string(),
                        to_qualified_name: name,
                    });
                }
            }
        }
    }
}


fn extract_class_body(ctx: &mut ExtractCtx, body: Node, class_qn: &str) {
    if body.kind() != TS_CLASS_BODY {
        return;
    }
    let mut cursor = body.walk();
    for child in body.children(&mut cursor) {
        match child.kind() {
            TS_METHOD_DEF => extract_method(ctx, child, class_qn),
            TS_PUBLIC_FIELD => extract_field(ctx, child, class_qn),
            _ => {}
        }
    }
}


fn extract_method(ctx: &mut ExtractCtx, node: Node, class_qn: &str) {
    let name = node_field_text(ctx.source, node, "name");
    if name.is_empty() {
        return;
    }
    let qn = qual(class_qn, &name);
    let is_async = has_async_keyword(ctx.source, node);
    let vis = extract_ts_member_visibility(ctx.source, node);

    ctx.nodes.push(ExtractedNode {
        label: LABEL_METHOD.to_string(),
        name: name.clone(),
        qualified_name: qn.clone(),
        start_line: node.start_position().row as u64 + 1,
        end_line: node.end_position().row as u64 + 1,
        visibility: vis,
        properties: vec![
            ("is_async".to_string(), is_async.to_string()),
            ("receiver_type".to_string(), class_qn.to_string()),
        ],
    });
    ctx.refs.push(ExtractedRef {
        kind: "HasMethod".to_string(),
        from_qualified_name: class_qn.to_string(),
        to_qualified_name: qn.clone(),
    });
    if let Some(body) = node.child_by_field_name("body") {
        extract_call_sites(ctx, body, &qn);
    }
}


fn extract_field(ctx: &mut ExtractCtx, node: Node, class_qn: &str) {
    let name = node_field_text(ctx.source, node, "name");
    if name.is_empty() {
        return;
    }
    let type_ann = node.child_by_field_name("type")
        .map(|n| node_text(ctx.source, n))
        .unwrap_or_default();
    let vis = extract_ts_member_visibility(ctx.source, node);
    let fqn = qual(class_qn, &name);

    ctx.nodes.push(ExtractedNode {
        label: LABEL_FIELD.to_string(),
        name,
        qualified_name: fqn.clone(),
        start_line: node.start_position().row as u64 + 1,
        end_line: node.end_position().row as u64 + 1,
        visibility: vis,
        properties: vec![("type_annotation".to_string(), type_ann)],
    });
    ctx.refs.push(ExtractedRef {
        kind: "HasField".to_string(),
        from_qualified_name: class_qn.to_string(),
        to_qualified_name: fqn,
    });
}
