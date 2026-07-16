// parser::cpp::extract::g1 — see ../extract/mod.rs.

use super::super::*; // parent module: Ctx, TS_* consts, kept helpers
use super::*;
use crate::parser::*; // ExtractedNode, ExtractedRef, node_text, qual, LABEL_*, …
use tree_sitter::Node; // sibling extract fns (glob re-export)

pub(super) fn find_identifier(source: &str, node: Node) -> String {
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        let k = n.kind();
        if k == "identifier" || k == "type_identifier" || k == "field_identifier" {
            return node_text(source, n);
        }
        let mut cursor = n.walk();
        for c in n.children(&mut cursor) {
            stack.push(c);
        }
    }
    String::new()
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
            TS_NAMESPACE => extract_namespace(ctx, child, scope),
            TS_CLASS => {
                extract_class_like(ctx, child, scope, LABEL_STRUCT, /*is_class=*/ true)
            }
            TS_STRUCT | TS_UNION => extract_class_like(ctx, child, scope, LABEL_STRUCT, false),
            TS_ENUM => extract_enum(ctx, child, scope),
            TS_TEMPLATE => {
                // Templates wrap a class/function; descend.
                extract_children(ctx, child, scope, enclosing_type);
            }
            TS_FUNCTION_DEF => extract_function(ctx, child, scope, enclosing_type),
            TS_FIELD_DECL => {
                if enclosing_type.is_some() {
                    // Inside a class body this may be a member function
                    // declaration OR a field; check for a function_declarator.
                    if has_function_declarator(child) {
                        extract_member_proto(ctx, child, scope);
                    } else {
                        extract_member_field(ctx, child, scope);
                    }
                }
            }
            TS_TYPEDEF => extract_typedef(ctx, child, scope),
            TS_INCLUDE => extract_include(ctx, child, scope),
            TS_USING => extract_using(ctx, child, scope),
            _ => {
                if child.named_child_count() > 0 {
                    extract_children(ctx, child, scope, enclosing_type);
                }
            }
        }
    }
}

pub(super) fn has_function_declarator(node: Node) -> bool {
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        if n.kind() == "function_declarator" {
            return true;
        }
        let mut cursor = n.walk();
        for c in n.children(&mut cursor) {
            stack.push(c);
        }
    }
    false
}

pub(super) fn extract_namespace(ctx: &mut Ctx, node: Node, scope: &str) {
    let name = node_field_text(ctx.source, node, "name");
    let name = if name.is_empty() {
        find_identifier(ctx.source, node)
    } else {
        name
    };
    if name.is_empty() {
        // Anonymous namespace — keep scope unchanged but still recurse.
        if let Some(body) = node.child_by_field_name("body") {
            extract_children(ctx, body, scope, None);
        }
        return;
    }
    let qn = qual(scope, &name);
    ctx.nodes.push(ExtractedNode {
        label: LABEL_STRUCT.to_string(),
        name: name.clone(),
        qualified_name: qn.clone(),
        start_line: node.start_position().row as u64 + 1,
        end_line: node.end_position().row as u64 + 1,
        visibility: "public".to_string(),
        properties: vec![("is_namespace".to_string(), "true".to_string())],
    });
    ctx.refs.push(ExtractedRef {
        kind: "Defines".to_string(),
        from_qualified_name: scope.to_string(),
        to_qualified_name: qn.clone(),
    });
    if let Some(body) = node.child_by_field_name("body") {
        extract_children(ctx, body, &qn, None);
    }
}

pub(super) fn extract_class_like(
    ctx: &mut Ctx,
    node: Node,
    scope: &str,
    label: &str,
    is_class: bool,
) {
    let name = node_field_text(ctx.source, node, "name");
    let name = if name.is_empty() {
        find_identifier(ctx.source, node)
    } else {
        name
    };
    if name.is_empty() {
        return;
    }
    let qn = qual(scope, &name);
    let mut props = Vec::new();
    if is_class {
        props.push(("is_class".to_string(), "true".to_string()));
    }
    ctx.nodes.push(ExtractedNode {
        label: label.to_string(),
        name: name.clone(),
        qualified_name: qn.clone(),
        start_line: node.start_position().row as u64 + 1,
        end_line: node.end_position().row as u64 + 1,
        visibility: "public".to_string(),
        properties: props,
    });
    ctx.refs.push(ExtractedRef {
        kind: "Defines".to_string(),
        from_qualified_name: scope.to_string(),
        to_qualified_name: qn.clone(),
    });
    // Base-class list. class_specifier has NO `bases` field — `base_class_clause`
    // is a DIRECT child of class_specifier, and each base type is a
    // type_identifier / qualified_identifier / template_type child of that clause
    // (access specifiers like `public` and virtual/attribute tokens are skipped).
    // source: tree-sitter-cpp v0.23.4 (class_specifier children include
    // base_class_clause; base_class_clause has no `bases` field).
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() != "base_class_clause" {
            continue;
        }
        let mut inner = child.walk();
        for base in child.children(&mut inner) {
            if matches!(
                base.kind(),
                "type_identifier" | "qualified_identifier" | "template_type"
            ) {
                let t = node_text(ctx.source, base).trim().to_string();
                if !t.is_empty() {
                    ctx.refs.push(ExtractedRef {
                        kind: "Extends".to_string(),
                        from_qualified_name: qn.clone(),
                        to_qualified_name: t,
                    });
                }
            }
        }
    }
    if let Some(body) = node.child_by_field_name("body") {
        extract_children(ctx, body, &qn, Some(&qn));
    }
}

pub(super) fn extract_enum(ctx: &mut Ctx, node: Node, scope: &str) {
    let name = node_field_text(ctx.source, node, "name");
    let name = if name.is_empty() {
        find_identifier(ctx.source, node)
    } else {
        name
    };
    if name.is_empty() {
        return;
    }
    let qn = qual(scope, &name);
    ctx.nodes.push(ExtractedNode {
        label: LABEL_ENUM.to_string(),
        name: name.clone(),
        qualified_name: qn.clone(),
        start_line: node.start_position().row as u64 + 1,
        end_line: node.end_position().row as u64 + 1,
        visibility: "public".to_string(),
        properties: Vec::new(),
    });
    ctx.refs.push(ExtractedRef {
        kind: "Defines".to_string(),
        from_qualified_name: scope.to_string(),
        to_qualified_name: qn,
    });
}

pub(super) fn extract_function(
    ctx: &mut Ctx,
    node: Node,
    scope: &str,
    enclosing_type: Option<&str>,
) {
    let declarator = node.child_by_field_name("declarator");
    let name = declarator
        .map(|d| find_identifier(ctx.source, d))
        .unwrap_or_default();
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
        visibility: "public".to_string(),
        properties: props,
    });
    let edge = if enclosing_type.is_some() {
        "HasMethod"
    } else {
        "Defines"
    };
    ctx.refs.push(ExtractedRef {
        kind: edge.to_string(),
        from_qualified_name: scope.to_string(),
        to_qualified_name: qn.clone(),
    });
    if let Some(body) = node.child_by_field_name("body") {
        extract_calls(ctx, body, &qn);
    }
}
