// parser::rust::extract::g2 — see ../extract/mod.rs.

use super::super::*; // parent module: Ctx, TS_* consts, kept helpers
use super::*;
use crate::parser::*; // ExtractedNode, ExtractedRef, node_text, qual, LABEL_*, …
use tree_sitter::Node; // sibling extract fns (glob re-export)

pub(super) fn extract_variants(ctx: &mut ExtractCtx, enum_node: Node, enum_qn: &str) {
    let body = match enum_node.child_by_field_name("body") {
        Some(b) if b.kind() == TS_ENUM_VARIANT_LIST => b,
        _ => return,
    };
    let mut cursor = body.walk();
    for child in body.children(&mut cursor) {
        if child.kind() != TS_ENUM_VARIANT {
            continue;
        }
        let name = node_field_text(ctx.source, child, "name");
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

// ---------------------------------------------------------------------------
// Trait extraction
// ---------------------------------------------------------------------------

pub(super) fn extract_trait(ctx: &mut ExtractCtx, node: Node, scope: &str) {
    let name = node_field_text(ctx.source, node, "name");
    if name.is_empty() {
        return;
    }
    let qn = qual(scope, &name);
    let vis = extract_visibility(ctx.source, node);
    let supers = extract_supertraits(ctx.source, node);
    let mut props = vec![];
    if !supers.is_empty() {
        props.push(("supertraits".to_string(), supers.join(",")));
    }
    ctx.nodes.push(ExtractedNode {
        label: LABEL_TRAIT.to_string(),
        name: name.clone(),
        qualified_name: qn.clone(),
        start_line: node.start_position().row as u64 + 1,
        end_line: node.end_position().row as u64 + 1,
        visibility: vis,
        properties: props,
    });
    ctx.refs.push(ExtractedRef {
        kind: "Defines".to_string(),
        from_qualified_name: scope.to_string(),
        to_qualified_name: qn.clone(),
    });
    for sup in &supers {
        ctx.refs.push(ExtractedRef {
            kind: "Extends".to_string(),
            from_qualified_name: qn.clone(),
            to_qualified_name: sup.clone(),
        });
    }
    extract_trait_methods(ctx, node, &qn);
}

pub(super) fn extract_trait_methods(ctx: &mut ExtractCtx, trait_node: Node, trait_qn: &str) {
    let body = match trait_node.child_by_field_name("body") {
        Some(b) if b.kind() == TS_DECL_LIST => b,
        _ => return,
    };
    let mut cursor = body.walk();
    for child in body.children(&mut cursor) {
        let is_sig = child.kind() == TS_FUNCTION_SIG;
        let is_fn = child.kind() == TS_FUNCTION_ITEM;
        if !is_sig && !is_fn {
            continue;
        }
        let name = node_field_text(ctx.source, child, "name");
        if name.is_empty() {
            continue;
        }
        let mqn = qual(trait_qn, &name);
        let vis = extract_visibility(ctx.source, child);
        let is_async = if is_fn {
            has_async_modifier(child)
        } else {
            false
        };
        ctx.nodes.push(ExtractedNode {
            label: LABEL_METHOD.to_string(),
            name: name.clone(),
            qualified_name: mqn.clone(),
            start_line: child.start_position().row as u64 + 1,
            end_line: child.end_position().row as u64 + 1,
            visibility: vis,
            properties: vec![
                ("is_async".to_string(), is_async.to_string()),
                ("receiver_type".to_string(), trait_qn.to_string()),
            ],
        });
        ctx.refs.push(ExtractedRef {
            kind: "HasMethod".to_string(),
            from_qualified_name: trait_qn.to_string(),
            to_qualified_name: mqn,
        });
    }
}

// ---------------------------------------------------------------------------
// Impl extraction
// ---------------------------------------------------------------------------

pub(super) fn extract_impl(ctx: &mut ExtractCtx, node: Node) {
    let impl_type = node_field_text(ctx.source, node, "type");
    if impl_type.is_empty() {
        return;
    }
    let trait_name = node_field_text(ctx.source, node, "trait");
    let receiver_qn = qual(ctx.file_path, &impl_type);

    let body = match node.child_by_field_name("body") {
        Some(b) if b.kind() == TS_DECL_LIST => b,
        _ => return,
    };
    let mut cursor = body.walk();
    for child in body.children(&mut cursor) {
        if child.kind() != TS_FUNCTION_ITEM && child.kind() != TS_FUNCTION_SIG {
            continue;
        }
        extract_impl_method(ctx, child, &receiver_qn, &trait_name);
    }
}

pub(super) fn extract_impl_method(
    ctx: &mut ExtractCtx,
    node: Node,
    receiver_qn: &str,
    trait_name: &str,
) {
    let name = node_field_text(ctx.source, node, "name");
    if name.is_empty() {
        return;
    }
    let mqn = qual(receiver_qn, &name);
    let vis = extract_visibility(ctx.source, node);
    let is_async = has_async_modifier(node);
    let mut props = vec![
        ("is_async".to_string(), is_async.to_string()),
        ("receiver_type".to_string(), receiver_qn.to_string()),
    ];
    if !trait_name.is_empty() {
        props.push(("trait_name".to_string(), trait_name.to_string()));
    }
    ctx.nodes.push(ExtractedNode {
        label: LABEL_METHOD.to_string(),
        name: name.clone(),
        qualified_name: mqn.clone(),
        start_line: node.start_position().row as u64 + 1,
        end_line: node.end_position().row as u64 + 1,
        visibility: vis,
        properties: props,
    });
    ctx.refs.push(ExtractedRef {
        kind: "HasMethod".to_string(),
        from_qualified_name: receiver_qn.to_string(),
        to_qualified_name: mqn.clone(),
    });
    if let Some(body) = node.child_by_field_name("body") {
        extract_call_sites(ctx, body, &mqn);
    }
}

// ---------------------------------------------------------------------------
// Field extraction
// ---------------------------------------------------------------------------

pub(super) fn extract_fields(ctx: &mut ExtractCtx, parent: Node, parent_qn: &str, list_kind: &str) {
    let body = match parent.child_by_field_name("body") {
        Some(b) if b.kind() == list_kind => b,
        _ => return,
    };
    let mut cursor = body.walk();
    for child in body.children(&mut cursor) {
        if child.kind() != TS_FIELD_DECL {
            continue;
        }
        let name = node_field_text(ctx.source, child, "name");
        if name.is_empty() {
            continue;
        }
        let type_ann = node_field_text(ctx.source, child, "type");
        let vis = extract_visibility(ctx.source, child);
        let fqn = qual(parent_qn, &name);
        ctx.nodes.push(ExtractedNode {
            label: LABEL_FIELD.to_string(),
            name,
            qualified_name: fqn.clone(),
            start_line: child.start_position().row as u64 + 1,
            end_line: child.end_position().row as u64 + 1,
            visibility: vis,
            properties: vec![("type_annotation".to_string(), type_ann)],
        });
        ctx.refs.push(ExtractedRef {
            kind: "HasField".to_string(),
            from_qualified_name: parent_qn.to_string(),
            to_qualified_name: fqn,
        });
    }
}

// ---------------------------------------------------------------------------
// Const extraction
// ---------------------------------------------------------------------------

pub(super) fn extract_const(ctx: &mut ExtractCtx, node: Node, scope: &str) {
    let name = node_field_text(ctx.source, node, "name");
    if name.is_empty() {
        return;
    }
    let qn = qual(scope, &name);
    let type_ann = node_field_text(ctx.source, node, "type");
    ctx.nodes.push(ExtractedNode {
        label: LABEL_CONSTANT.to_string(),
        name,
        qualified_name: qn.clone(),
        start_line: node.start_position().row as u64 + 1,
        end_line: node.end_position().row as u64 + 1,
        visibility: extract_visibility(ctx.source, node),
        properties: vec![("type_annotation".to_string(), type_ann)],
    });
    ctx.refs.push(ExtractedRef {
        kind: "Defines".to_string(),
        from_qualified_name: scope.to_string(),
        to_qualified_name: qn,
    });
}
