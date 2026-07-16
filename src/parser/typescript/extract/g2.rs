// parser::typescript::extract::g2 — see ../extract/mod.rs.

use super::super::*; // parent module: Ctx, TS_* consts, kept helpers
use super::*;
use crate::parser::*; // ExtractedNode, ExtractedRef, node_text, qual, LABEL_*, …
use tree_sitter::Node; // sibling extract fns (glob re-export)

// ---------------------------------------------------------------------------
// Interface extraction (maps to Trait label)
// ---------------------------------------------------------------------------

pub(super) fn extract_interface(ctx: &mut ExtractCtx, node: Node, scope: &str, is_exported: bool) {
    let name = node_field_text(ctx.source, node, "name");
    if name.is_empty() {
        return;
    }
    let qn = qual(scope, &name);
    let vis = if is_exported || has_export_keyword(node) {
        "pub".to_string()
    } else {
        String::new()
    };

    ctx.nodes.push(ExtractedNode {
        label: LABEL_TRAIT.to_string(),
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

    // Extract extends clauses for interface
    extract_interface_extends(ctx, node, &qn);

    // Extract interface body (method/property signatures)
    if let Some(body) = node.child_by_field_name("body") {
        extract_interface_body(ctx, body, &qn);
    }
}

pub(super) fn extract_interface_extends(ctx: &mut ExtractCtx, iface_node: Node, iface_qn: &str) {
    let mut cursor = iface_node.walk();
    for child in iface_node.children(&mut cursor) {
        if child.kind() == "extends_type_clause" {
            let mut hcursor = child.walk();
            for hchild in child.children(&mut hcursor) {
                if hchild.kind() == "type_identifier" || hchild.kind() == "identifier" {
                    let name = node_text(ctx.source, hchild);
                    if !name.is_empty() {
                        ctx.refs.push(ExtractedRef {
                            kind: "Extends".to_string(),
                            from_qualified_name: iface_qn.to_string(),
                            to_qualified_name: name,
                        });
                    }
                }
            }
        }
    }
}

pub(super) fn extract_interface_body(ctx: &mut ExtractCtx, body: Node, iface_qn: &str) {
    if body.kind() != TS_INTERFACE_BODY && body.kind() != "object_type" {
        return;
    }
    let mut cursor = body.walk();
    for child in body.children(&mut cursor) {
        match child.kind() {
            TS_METHOD_SIGNATURE => {
                let name = node_field_text(ctx.source, child, "name");
                if name.is_empty() {
                    continue;
                }
                let mqn = qual(iface_qn, &name);
                ctx.nodes.push(ExtractedNode {
                    label: LABEL_METHOD.to_string(),
                    name: name.clone(),
                    qualified_name: mqn.clone(),
                    start_line: child.start_position().row as u64 + 1,
                    end_line: child.end_position().row as u64 + 1,
                    visibility: String::new(),
                    properties: vec![
                        ("is_async".to_string(), "false".to_string()),
                        ("receiver_type".to_string(), iface_qn.to_string()),
                    ],
                });
                ctx.refs.push(ExtractedRef {
                    kind: "HasMethod".to_string(),
                    from_qualified_name: iface_qn.to_string(),
                    to_qualified_name: mqn,
                });
            }
            TS_PROPERTY_SIGNATURE => {
                let name = node_field_text(ctx.source, child, "name");
                if name.is_empty() {
                    continue;
                }
                let type_ann = child
                    .child_by_field_name("type")
                    .map(|n| node_text(ctx.source, n))
                    .unwrap_or_default();
                let fqn = qual(iface_qn, &name);
                ctx.nodes.push(ExtractedNode {
                    label: LABEL_FIELD.to_string(),
                    name,
                    qualified_name: fqn.clone(),
                    start_line: child.start_position().row as u64 + 1,
                    end_line: child.end_position().row as u64 + 1,
                    visibility: String::new(),
                    properties: vec![("type_annotation".to_string(), type_ann)],
                });
                ctx.refs.push(ExtractedRef {
                    kind: "HasField".to_string(),
                    from_qualified_name: iface_qn.to_string(),
                    to_qualified_name: fqn,
                });
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Enum extraction
// ---------------------------------------------------------------------------

pub(super) fn extract_enum(ctx: &mut ExtractCtx, node: Node, scope: &str, is_exported: bool) {
    let name = node_field_text(ctx.source, node, "name");
    if name.is_empty() {
        return;
    }
    let qn = qual(scope, &name);
    let vis = if is_exported || has_export_keyword(node) {
        "pub".to_string()
    } else {
        String::new()
    };

    ctx.nodes.push(ExtractedNode {
        label: LABEL_ENUM.to_string(),
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

    // Extract enum members
    if let Some(body) = node.child_by_field_name("body") {
        extract_enum_members(ctx, body, &qn);
    }
}

pub(super) fn extract_enum_members(ctx: &mut ExtractCtx, body: Node, enum_qn: &str) {
    if body.kind() != TS_ENUM_BODY {
        return;
    }
    let mut cursor = body.walk();
    for child in body.children(&mut cursor) {
        if child.kind() == "enum_assignment" || child.kind() == "property_identifier" {
            let name = if child.kind() == "enum_assignment" {
                node_field_text(ctx.source, child, "name")
            } else {
                node_text(ctx.source, child)
            };
            if name.is_empty() {
                continue;
            }
            let vqn = qual(enum_qn, &name);
            ctx.nodes.push(ExtractedNode {
                label: LABEL_VARIANT.to_string(),
                name,
                qualified_name: vqn.clone(),
                start_line: child.start_position().row as u64 + 1,
                end_line: child.end_position().row as u64 + 1,
                visibility: String::new(),
                properties: vec![],
            });
            ctx.refs.push(ExtractedRef {
                kind: "HasVariant".to_string(),
                from_qualified_name: enum_qn.to_string(),
                to_qualified_name: vqn,
            });
        }
    }
}

// ---------------------------------------------------------------------------
// Type alias extraction
// ---------------------------------------------------------------------------

pub(super) fn extract_type_alias(ctx: &mut ExtractCtx, node: Node, scope: &str, is_exported: bool) {
    let name = node_field_text(ctx.source, node, "name");
    if name.is_empty() {
        return;
    }
    let qn = qual(scope, &name);
    let vis = if is_exported || has_export_keyword(node) {
        "pub".to_string()
    } else {
        String::new()
    };
    let target = node
        .child_by_field_name("value")
        .map(|n| node_text(ctx.source, n))
        .unwrap_or_default();

    ctx.nodes.push(ExtractedNode {
        label: LABEL_TYPE_ALIAS.to_string(),
        name,
        qualified_name: qn.clone(),
        start_line: node.start_position().row as u64 + 1,
        end_line: node.end_position().row as u64 + 1,
        visibility: vis,
        properties: vec![("target_type".to_string(), target)],
    });
    ctx.refs.push(ExtractedRef {
        kind: "Defines".to_string(),
        from_qualified_name: scope.to_string(),
        to_qualified_name: qn,
    });
}

// ---------------------------------------------------------------------------
// Import extraction
// ---------------------------------------------------------------------------

pub(super) fn extract_import(ctx: &mut ExtractCtx, node: Node, scope: &str) {
    let source_node = node.child_by_field_name("source");
    let path = source_node
        .map(|n| {
            let text = node_text(ctx.source, n);
            // Strip quotes from the import path
            text.trim_matches(|c| c == '\'' || c == '"').to_string()
        })
        .unwrap_or_default();

    // Normalize path separators to ::
    let normalized_path = path.replace('/', "::");

    // Find import clause children
    let mut cursor = node.walk();
    let mut found_any = false;
    for child in node.children(&mut cursor) {
        if child.kind() == "import_clause" {
            extract_import_clause(ctx, child, scope, &normalized_path, node);
            found_any = true;
        }
    }
    if !found_any && !normalized_path.is_empty() {
        // Side-effect import: import 'foo'
        emit_ts_import(ctx, scope, &normalized_path, "", false, node);
    }
}
