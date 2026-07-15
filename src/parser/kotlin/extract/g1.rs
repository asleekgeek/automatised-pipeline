// parser::kotlin::extract::g1 — see ../extract/mod.rs.

use super::super::*; // parent module: Ctx, TS_* consts, kept helpers
use super::*;
use crate::parser::*; // ExtractedNode, ExtractedRef, node_text, qual, LABEL_*, …
use tree_sitter::Node; // sibling extract fns (glob re-export)

// Kotlin declaration kinds the grammar exposes as ``class_declaration``
// are distinguished by a leading modifier: ``interface``, ``enum class``,
// ``data class``, etc. We walk the node text once to classify.
pub(super) fn classify_class(source: &str, node: Node) -> &'static str {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        let kind = child.kind();
        if kind == "interface" {
            return LABEL_TRAIT;
        }
        // An `enum class` always carries an `enum_class_body` child (holding the
        // enum_entry members) — the most reliable enum signal in the -ng grammar.
        // The `enum` keyword token and the `enum` class_modifier are secondary.
        if kind == "enum" || kind == TS_ENUM_CLASS_BODY {
            return LABEL_ENUM;
        }
        // ``modifiers`` node contains ``annotation``, ``class_modifier`` etc.
        if kind == "modifiers" {
            let text = node_text(source, child);
            if text.contains("enum") {
                return LABEL_ENUM;
            }
        }
    }
    LABEL_STRUCT
}

pub(super) fn visibility_modifier(source: &str, node: Node) -> String {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "modifiers" {
            let t = node_text(source, child);
            for v in ["public", "private", "protected", "internal"] {
                if t.split_whitespace().any(|w| w == v) {
                    return v.to_string();
                }
            }
        }
    }
    // Kotlin default is ``public``.
    "public".to_string()
}

pub(crate) fn extract_children(
    ctx: &mut Ctx,
    parent: Node,
    scope: &str,
    enclosing_type: Option<&str>,
) {
    let mut cursor = parent.walk();
    for child in parent.children(&mut cursor) {
        match child.kind() {
            TS_CLASS_DECL | TS_OBJECT_DECL => extract_class_like(ctx, child, scope),
            TS_FUNCTION_DECL => extract_function(ctx, child, scope, enclosing_type),
            TS_PROPERTY_DECL => extract_property(ctx, child, scope),
            TS_IMPORT => extract_import(ctx, child, scope),
            TS_PACKAGE_HEADER => {}
            TS_ENUM_ENTRY => {
                // Enum variant — emit as a constant of the enum type. In the -ng
                // grammar an enum_entry's name is an `identifier` child (there is
                // no `simple_identifier` node type), so use first_identifier.
                let name = first_identifier(ctx.source, child);
                if !name.is_empty() {
                    let qn = qual(scope, &name);
                    ctx.nodes.push(ExtractedNode {
                        label: LABEL_CONSTANT.to_string(),
                        name: name.clone(),
                        qualified_name: qn.clone(),
                        start_line: child.start_position().row as u64 + 1,
                        end_line: child.end_position().row as u64 + 1,
                        visibility: "public".to_string(),
                        properties: vec![("enum_entry".to_string(), "true".to_string())],
                    });
                    ctx.refs.push(ExtractedRef {
                        kind: "Defines".to_string(),
                        from_qualified_name: scope.to_string(),
                        to_qualified_name: qn,
                    });
                }
            }
            TS_CLASS_BODY | TS_ENUM_CLASS_BODY => {
                extract_children(ctx, child, scope, enclosing_type);
            }
            _ => {}
        }
    }
}

pub(super) fn first_identifier(source: &str, node: Node) -> String {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "simple_identifier" || child.kind() == "identifier" {
            return node_text(source, child);
        }
    }
    String::new()
}

pub(super) fn extract_class_like(ctx: &mut Ctx, node: Node, scope: &str) {
    let name = node_field_text(ctx.source, node, "name");
    let name = if name.is_empty() {
        first_identifier(ctx.source, node)
    } else {
        name
    };
    if name.is_empty() {
        return;
    }
    let label = classify_class(ctx.source, node);
    let qn = qual(scope, &name);
    ctx.nodes.push(ExtractedNode {
        label: label.to_string(),
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
    // Supertype list — ``: Parent, Interface`` after the class name. In the -ng
    // grammar `delegation_specifiers` is a CHILD node of class_declaration (not
    // a field), containing one `delegation_specifier` per supertype. Walk to it.
    if let Some(supers) = child_of_kind(node, TS_DELEGATION_SPECIFIERS) {
        let text = node_text(ctx.source, supers);
        for piece in text
            .split(',')
            .map(|s| s.trim().trim_end_matches("()"))
            .filter(|s| !s.is_empty())
        {
            ctx.refs.push(ExtractedRef {
                kind: "Extends".to_string(),
                from_qualified_name: qn.clone(),
                to_qualified_name: piece.to_string(),
            });
        }
    }
    // Body — recurse with this type as the enclosing scope. The -ng grammar has
    // no `body` field; the body is a `class_body` child, or an `enum_class_body`
    // child for enums (which holds the enum_entry members).
    let body =
        child_of_kind(node, TS_CLASS_BODY).or_else(|| child_of_kind(node, TS_ENUM_CLASS_BODY));
    if let Some(body) = body {
        extract_children(ctx, body, &qn, Some(&qn));
    }
}

pub(super) fn extract_function(
    ctx: &mut Ctx,
    node: Node,
    scope: &str,
    enclosing_type: Option<&str>,
) {
    let name = node_field_text(ctx.source, node, "name");
    let name = if name.is_empty() {
        first_identifier(ctx.source, node)
    } else {
        name
    };
    if name.is_empty() {
        return;
    }
    // Disambiguate overloaded functions by appending a per-file sequence
    // number — the primary key must be unique across all Function and
    // Method nodes in the graph, and LadybugDB rejects duplicates at
    // insert time.
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
    // Kotlin extension functions declare a receiver: ``fun String.foo()``.
    // In the -ng grammar the receiver is a `receiver` node child (there is no
    // `receiver` field), sitting before the function name.
    if let Some(recv) = child_of_kind(node, "receiver") {
        props.push((
            "extension_receiver".to_string(),
            node_text(ctx.source, recv),
        ));
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
    // The -ng grammar has no `body` field; the body is a `function_body` child
    // (which wraps either a `block` or an expression). Fall back to scanning the
    // whole declaration for expression-bodied functions: ``fun f() = x.bar()``.
    if let Some(body) = child_of_kind(node, TS_FUNCTION_BODY) {
        extract_calls(ctx, body, &qn);
    } else {
        extract_calls(ctx, node, &qn);
    }
}

pub(super) fn extract_property(ctx: &mut Ctx, node: Node, scope: &str) {
    let name = first_identifier(ctx.source, node);
    if name.is_empty() {
        return;
    }
    let qn = qual(scope, &name);
    ctx.nodes.push(ExtractedNode {
        label: LABEL_CONSTANT.to_string(),
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
        to_qualified_name: qn,
    });
}
