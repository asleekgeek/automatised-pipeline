// parser::swift::extract::g1 — see ../extract/mod.rs.

use tree_sitter::Node;
use crate::parser::*;      // ExtractedNode, ExtractedRef, node_text, qual, LABEL_*, …
use super::super::*;       // parent module: Ctx, TS_* consts, kept helpers
use super::*;              // sibling extract fns (glob re-export)


// ``class_declaration`` is the Swift grammar's umbrella for class, struct,
// actor, extension, and enum. The grammar DOES expose the kind as the
// ``declaration_kind`` field (values: class | struct | enum | extension |
// actor), so classify off that instead of sniffing the node's head text —
// head-text matching mis-classified e.g. an attributed/`public` declaration or
// a struct whose name begins with "enum...". Returns (label, is_extension).
// source: alex-pinkus/tree-sitter-swift v0.7.3 class_declaration.declaration_kind.
pub(super) fn classify_class(source: &str, node: Node) -> (&'static str, bool) {
    let kind = node_field_text(source, node, "declaration_kind");
    match kind.trim() {
        "enum" => (LABEL_ENUM, false),
        "extension" => (LABEL_STRUCT, true),
        // class, struct, actor all map to Struct (AP has no dedicated Class/
        // Actor label); only enum and extension change behavior.
        _ => (LABEL_STRUCT, false),
    }
}


pub(super) fn visibility_modifier(source: &str, node: Node) -> String {
    let text = node_text(source, node);
    let head = text.lines().next().unwrap_or("");
    for v in ["public", "private", "internal", "fileprivate", "open"] {
        if head.split_whitespace().any(|w| w == v) {
            return v.to_string();
        }
    }
    "internal".to_string()
}


pub(crate) fn extract_children(ctx: &mut Ctx, parent: Node, scope: &str, enclosing_type: Option<&str>) {
    let mut cursor = parent.walk();
    for child in parent.children(&mut cursor) {
        match child.kind() {
            TS_CLASS_DECL => extract_class_like(ctx, child, scope),
            TS_PROTOCOL_DECL => extract_protocol(ctx, child, scope),
            TS_FUNCTION_DECL => extract_function(ctx, child, scope, enclosing_type),
            // init/deinit/subscript are member "functions" — emit as Method when
            // inside a type, else Function, with a synthesized name (the grammar
            // has no `name` field for them). Body location differs by kind:
            // init/deinit carry a `body` field (function_body); subscript has NO
            // `body` field — its getter/setter live under a `computed_property`
            // child. extract_member_fn handles both via find_block_child.
            TS_INIT_DECL => extract_member_fn(ctx, child, scope, enclosing_type, "init"),
            TS_DEINIT_DECL => extract_member_fn(ctx, child, scope, enclosing_type, "deinit"),
            TS_SUBSCRIPT_DECL => extract_member_fn(ctx, child, scope, enclosing_type, "subscript"),
            TS_ENUM_ENTRY => extract_enum_entry(ctx, child, scope),
            TS_PROPERTY_DECL => extract_property(ctx, child, scope),
            TS_IMPORT_DECL => extract_import(ctx, child, scope),
            TS_TYPEALIAS_DECL => extract_typealias(ctx, child, scope),
            _ => {
                // Swift top-level is a sequence of statements — recurse into
                // compound groupings so we pick up nested decls inside
                // guards / do-blocks / computed-property bodies.
                if child.named_child_count() > 0 {
                    extract_children(ctx, child, scope, enclosing_type);
                }
            }
        }
    }
}


pub(super) fn find_name(source: &str, node: Node) -> String {
    // Try the canonical field first.
    let n = node_field_text(source, node, "name");
    if !n.is_empty() {
        return n;
    }
    // Fall back: first ``type_identifier`` or ``simple_identifier``.
    let mut cursor = node.walk();
    for c in node.children(&mut cursor) {
        let k = c.kind();
        if k == "type_identifier" || k == "simple_identifier" || k == "identifier" {
            let t = node_text(source, c);
            if !t.is_empty() {
                return t;
            }
        }
    }
    String::new()
}


pub(super) fn extract_class_like(ctx: &mut Ctx, node: Node, scope: &str) {
    let (label, is_extension) = classify_class(ctx.source, node);
    let name = find_name(ctx.source, node);
    if name.is_empty() {
        return;
    }
    let qn = qual(scope, &name);
    // Extensions don't create a new type — we still emit a node so the
    // methods inside have a parent, but mark it so downstream tooling
    // can merge.
    let mut props = Vec::new();
    if is_extension {
        props.push(("is_extension".to_string(), "true".to_string()));
    }
    ctx.nodes.push(ExtractedNode {
        label: label.to_string(),
        name: name.clone(),
        qualified_name: qn.clone(),
        start_line: node.start_position().row as u64 + 1,
        end_line: node.end_position().row as u64 + 1,
        visibility: visibility_modifier(ctx.source, node),
        properties: props,
    });
    ctx.refs.push(ExtractedRef {
        kind: "Defines".to_string(),
        from_qualified_name: scope.to_string(),
        to_qualified_name: qn.clone(),
    });
    if let Some(body) = node
        .child_by_field_name("body")
        .or_else(|| find_block_child(node))
    {
        extract_children(ctx, body, &qn, Some(&qn));
    }
}


pub(super) fn find_block_child(node: Node) -> Option<Node> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        let k = child.kind();
        // `computed_property` holds a subscript's / computed var's getter+setter
        // (subscript_declaration has no `body` field). source: alex-pinkus/
        // tree-sitter-swift v0.7.3.
        if k.ends_with("_body")
            || k == "class_body"
            || k == "enum_class_body"
            || k == "computed_property"
        {
            return Some(child);
        }
    }
    None
}


pub(super) fn extract_protocol(ctx: &mut Ctx, node: Node, scope: &str) {
    let name = find_name(ctx.source, node);
    if name.is_empty() {
        return;
    }
    let qn = qual(scope, &name);
    ctx.nodes.push(ExtractedNode {
        label: LABEL_TRAIT.to_string(),
        name: name.clone(),
        qualified_name: qn.clone(),
        start_line: node.start_position().row as u64 + 1,
        end_line: node.end_position().row as u64 + 1,
        visibility: visibility_modifier(ctx.source, node),
        properties: Vec::new(),
    });
    ctx.refs.push(ExtractedRef {
        kind: "Defines".to_string(),
        from_qualified_name: scope.to_string(),
        to_qualified_name: qn.clone(),
    });
    if let Some(body) = node
        .child_by_field_name("body")
        .or_else(|| find_block_child(node))
    {
        extract_children(ctx, body, &qn, Some(&qn));
    }
}


pub(super) fn extract_function(ctx: &mut Ctx, node: Node, scope: &str, enclosing_type: Option<&str>) {
    let name = find_name(ctx.source, node);
    if name.is_empty() {
        return;
    }
    let seq = {
        ctx.next_seq += 1;
        ctx.next_seq
    };
    let qn = format!("{}::{}#{}", scope, name, seq);
    let label = if enclosing_type.is_some() {
        LABEL_METHOD
    } else {
        LABEL_FUNCTION
    };
    let mut props = Vec::new();
    if let Some(rec) = enclosing_type {
        props.push(("receiver_type".to_string(), rec.to_string()));
    }
    ctx.nodes.push(ExtractedNode {
        label: label.to_string(),
        name: name.clone(),
        qualified_name: qn.clone(),
        start_line: node.start_position().row as u64 + 1,
        end_line: node.end_position().row as u64 + 1,
        visibility: visibility_modifier(ctx.source, node),
        properties: props,
    });
    let edge_kind = if enclosing_type.is_some() {
        "HasMethod"
    } else {
        "Defines"
    };
    ctx.refs.push(ExtractedRef {
        kind: edge_kind.to_string(),
        from_qualified_name: scope.to_string(),
        to_qualified_name: qn.clone(),
    });
    if let Some(body) = node
        .child_by_field_name("body")
        .or_else(|| find_block_child(node))
    {
        extract_calls(ctx, body, &qn);
    }
}
