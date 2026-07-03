// parser::objc::extract::g2 — see ../extract/mod.rs.

use tree_sitter::Node;
use crate::parser::*;      // ExtractedNode, ExtractedRef, node_text, qual, LABEL_*, …
use super::super::*;       // parent module: Ctx, TS_* consts, kept helpers
use super::*;              // sibling extract fns (glob re-export)


pub(super) fn extract_function(ctx: &mut Ctx, node: Node, scope: &str, enclosing: Option<&str>) {
    let decl = node.child_by_field_name("declarator");
    let name = decl
        .map(|d| node_field_text(ctx.source, d, "declarator"))
        .unwrap_or_default();
    let name = if name.is_empty() {
        find_name(ctx.source, node)
    } else {
        name
    };
    if name.is_empty() {
        return;
    }
    let seq = {
        ctx.next_seq += 1;
        ctx.next_seq
    };
    let qn = format!("{}::{}#{}", scope, name, seq);
    let label = if enclosing.is_some() {
        LABEL_METHOD
    } else {
        LABEL_FUNCTION
    };
    ctx.nodes.push(ExtractedNode {
        label: label.to_string(),
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
        to_qualified_name: qn.clone(),
    });
    if let Some(body) = node
        .child_by_field_name("body")
        .or_else(|| node_child_of_kind(node, "compound_statement"))
    {
        extract_calls(ctx, body, &qn);
    }
}


pub(super) fn node_child_of_kind<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    for c in node.children(&mut cursor) {
        if c.kind() == kind {
            return Some(c);
        }
    }
    None
}


/// Emits a C `struct`/`union` declared in an ObjC file as a Struct plus its
/// fields (mirrors the C parser). source: tree-sitter-grammars/tree-sitter-objc
/// v3.0.2 struct_specifier/union_specifier.
pub(super) fn extract_c_struct(ctx: &mut Ctx, node: Node, scope: &str) {
    let name = node_field_text(ctx.source, node, "name");
    let name = if name.is_empty() {
        first_identifier(ctx.source, node)
    } else {
        name
    };
    if name.is_empty() {
        return; // anonymous struct (e.g. inside a typedef) — skip
    }
    let qn = qual(scope, &name);
    ctx.nodes.push(ExtractedNode {
        label: LABEL_STRUCT.to_string(),
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
        to_qualified_name: qn.clone(),
    });
    if let Some(body) = node.child_by_field_name("body") {
        extract_c_struct_fields(ctx, body, &qn);
    }
}


/// Field nodes + HasField edges for a C struct body. `field_declaration`'s
/// `declarator` field is `multiple` (`int a, b;`) — walk all declarator children
/// and descend each for its `field_identifier`.
pub(super) fn extract_c_struct_fields(ctx: &mut Ctx, body: Node, owner_qn: &str) {
    let mut cursor = body.walk();
    for fd in body.children(&mut cursor) {
        if fd.kind() != "field_declaration" {
            continue;
        }
        let type_text = node_field_text(ctx.source, fd, "type");
        let mut declarators: Vec<Node> = Vec::new();
        let mut dc = fd.walk();
        if dc.goto_first_child() {
            loop {
                if dc.field_name() == Some("declarator") {
                    declarators.push(dc.node());
                }
                if !dc.goto_next_sibling() {
                    break;
                }
            }
        }
        for declarator in declarators {
            let fname = find_c_field_identifier(ctx.source, declarator);
            if fname.is_empty() {
                continue;
            }
            let fqn = qual(owner_qn, &fname);
            let mut props = Vec::new();
            if !type_text.is_empty() {
                props.push(("type_annotation".to_string(), type_text.clone()));
            }
            ctx.nodes.push(ExtractedNode {
                label: LABEL_FIELD.to_string(),
                name: fname.clone(),
                qualified_name: fqn.clone(),
                start_line: fd.start_position().row as u64 + 1,
                end_line: fd.end_position().row as u64 + 1,
                visibility: "public".to_string(),
                properties: props,
            });
            ctx.refs.push(ExtractedRef {
                kind: "HasField".to_string(),
                from_qualified_name: owner_qn.to_string(),
                to_qualified_name: fqn,
            });
        }
    }
}


/// First `field_identifier` under a (possibly pointer/array) declarator.
pub(super) fn find_c_field_identifier(source: &str, node: Node) -> String {
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        if n.kind() == "field_identifier" {
            return node_text(source, n);
        }
        let mut cursor = n.walk();
        for c in n.children(&mut cursor) {
            stack.push(c);
        }
    }
    String::new()
}


/// Emits a C `enum` as an Enum plus its enumerators as Variant constants.
pub(super) fn extract_c_enum(ctx: &mut Ctx, node: Node, scope: &str) {
    let name = node_field_text(ctx.source, node, "name");
    let name = if name.is_empty() {
        first_identifier(ctx.source, node)
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
        to_qualified_name: qn.clone(),
    });
    if let Some(body) = node.child_by_field_name("body") {
        let mut cursor = body.walk();
        for child in body.children(&mut cursor) {
            if child.kind() != "enumerator" {
                continue;
            }
            let en = node_field_text(ctx.source, child, "name");
            let en = if en.is_empty() {
                first_identifier(ctx.source, child)
            } else {
                en
            };
            if en.is_empty() {
                continue;
            }
            let eqn = qual(&qn, &en);
            ctx.nodes.push(ExtractedNode {
                label: LABEL_CONSTANT.to_string(),
                name: en.clone(),
                qualified_name: eqn.clone(),
                start_line: child.start_position().row as u64 + 1,
                end_line: child.end_position().row as u64 + 1,
                visibility: "public".to_string(),
                properties: vec![("enum_entry".to_string(), "true".to_string())],
            });
            ctx.refs.push(ExtractedRef {
                kind: "Defines".to_string(),
                from_qualified_name: qn.clone(),
                to_qualified_name: eqn,
            });
        }
    }
}


/// Emits a C `typedef` as a Constant marked `typedef=true` (mirrors the C
/// parser, which AP models typedefs the same way). The declared name is the
/// `type_identifier` at the tail of the `declarator` field.
pub(super) fn extract_c_typedef(ctx: &mut Ctx, node: Node, scope: &str) {
    let name = find_c_typedef_name(ctx.source, node);
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
        visibility: "public".to_string(),
        properties: vec![("typedef".to_string(), "true".to_string())],
    });
    ctx.refs.push(ExtractedRef {
        kind: "Defines".to_string(),
        from_qualified_name: scope.to_string(),
        to_qualified_name: qn,
    });
}
