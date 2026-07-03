// parser::go::extract::g1 — see ../extract/mod.rs.

use tree_sitter::Node;
use crate::parser::*;      // ExtractedNode, ExtractedRef, node_text, qual, LABEL_*, …
use super::super::*;       // parent module: Ctx, TS_* consts, kept helpers
use super::*;              // sibling extract fns (glob re-export)


pub(crate) fn extract_top(ctx: &mut Ctx, parent: Node, scope: &str) {
    let mut cursor = parent.walk();
    for child in parent.children(&mut cursor) {
        match child.kind() {
            TS_PACKAGE_CLAUSE => {}
            TS_IMPORT_DECL => extract_imports(ctx, child, scope),
            TS_TYPE_DECL => extract_types(ctx, child, scope),
            TS_FUNCTION_DECL => extract_function(ctx, child, scope),
            TS_METHOD_DECL => extract_method(ctx, child, scope),
            TS_CONST_DECL | TS_VAR_DECL => extract_value_decl(ctx, child, scope),
            _ => {}
        }
    }
}


pub(super) fn extract_imports(ctx: &mut Ctx, node: Node, scope: &str) {
    // Two shapes: single ``import "x"`` or ``import ( "a"; "b" )``.
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        if n.kind() == "import_spec" {
            let path = node_field_text(ctx.source, n, "path");
            let cleaned = path.trim_matches('"').to_string();
            if cleaned.is_empty() {
                continue;
            }
            let name = cleaned.rsplit('/').next().unwrap_or(&cleaned).to_string();
            let qn = qual(scope, &format!("import:{cleaned}"));
            ctx.nodes.push(ExtractedNode {
                label: LABEL_IMPORT.to_string(),
                name,
                qualified_name: qn,
                start_line: n.start_position().row as u64 + 1,
                end_line: n.end_position().row as u64 + 1,
                visibility: "public".to_string(),
                properties: vec![("path".to_string(), cleaned.clone())],
            });
            ctx.refs.push(ExtractedRef {
                kind: "Imports".to_string(),
                from_qualified_name: scope.to_string(),
                to_qualified_name: cleaned,
            });
        }
        let mut cursor = n.walk();
        for c in n.children(&mut cursor) {
            stack.push(c);
        }
    }
}


pub(super) fn extract_types(ctx: &mut Ctx, node: Node, scope: &str) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "type_spec" => extract_type_spec(ctx, child, scope),
            "type_alias" => extract_type_spec(ctx, child, scope),
            _ => {}
        }
    }
}


pub(super) fn extract_type_spec(ctx: &mut Ctx, node: Node, scope: &str) {
    let name = node_field_text(ctx.source, node, "name");
    if name.is_empty() {
        return;
    }
    let qn = qual(scope, &name);
    // Classify by looking at the ``type`` field — struct/interface/other.
    let mut label = LABEL_TYPE_ALIAS;
    let mut struct_type: Option<Node> = None;
    if let Some(ty) = node.child_by_field_name("type") {
        match ty.kind() {
            "struct_type" => {
                label = LABEL_STRUCT;
                struct_type = Some(ty);
            }
            "interface_type" => label = LABEL_TRAIT,
            _ => {}
        }
    }
    ctx.nodes.push(ExtractedNode {
        label: label.to_string(),
        name: name.clone(),
        qualified_name: qn.clone(),
        start_line: node.start_position().row as u64 + 1,
        end_line: node.end_position().row as u64 + 1,
        visibility: go_visibility(&name),
        properties: Vec::new(),
    });
    ctx.refs.push(ExtractedRef {
        kind: "Defines".to_string(),
        from_qualified_name: scope.to_string(),
        to_qualified_name: qn.clone(),
    });
    if let Some(st) = struct_type {
        extract_struct_fields(ctx, st, &qn);
    }
}


/// Extracts struct fields: descend `struct_type -> field_declaration_list ->
/// field_declaration`. A field_declaration's `name` is a `multiple` field
/// (`X, Y int` declares two fields) — emit one Field node + HasField edge per
/// name. Embedded fields (no name, just a type) are skipped.
/// source: tree-sitter-go v0.23.4 (struct_type/field_declaration).
pub(super) fn extract_struct_fields(ctx: &mut Ctx, struct_type: Node, owner_qn: &str) {
    let mut c1 = struct_type.walk();
    for fdl in struct_type.children(&mut c1) {
        if fdl.kind() != "field_declaration_list" {
            continue;
        }
        let mut c2 = fdl.walk();
        for fd in fdl.children(&mut c2) {
            if fd.kind() != "field_declaration" {
                continue;
            }
            let type_text = node_field_text(ctx.source, fd, "type");
            // The `name` field is `multiple` (`X, Y int`); collect every child
            // whose field name is "name". Uses the cursor.field_name() idiom
            // (the same one objc.rs uses) rather than children_by_field_name.
            let mut names: Vec<Node> = Vec::new();
            let mut c3 = fd.walk();
            if c3.goto_first_child() {
                loop {
                    if c3.field_name() == Some("name") {
                        names.push(c3.node());
                    }
                    if !c3.goto_next_sibling() {
                        break;
                    }
                }
            }
            for name_node in names {
                let fname = node_text(ctx.source, name_node);
                if fname.is_empty() {
                    continue;
                }
                let fqn = qual(owner_qn, &fname);
                let mut props = Vec::new();
                if !type_text.is_empty() {
                    props.push(("type_annotation".to_string(), type_text.clone()));
                }
                ctx.nodes.push(ExtractedNode {
                    label: crate::parser::LABEL_FIELD.to_string(),
                    name: fname.clone(),
                    qualified_name: fqn.clone(),
                    start_line: fd.start_position().row as u64 + 1,
                    end_line: fd.end_position().row as u64 + 1,
                    visibility: go_visibility(&fname),
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
}


pub(super) fn extract_function(ctx: &mut Ctx, node: Node, scope: &str) {
    let name = node_field_text(ctx.source, node, "name");
    if name.is_empty() {
        return;
    }
    let seq = {
        ctx.next_seq += 1;
        ctx.next_seq
    };
    let qn = format!("{}::{}#{}", scope, name, seq);
    ctx.nodes.push(ExtractedNode {
        label: LABEL_FUNCTION.to_string(),
        name: name.clone(),
        qualified_name: qn.clone(),
        start_line: node.start_position().row as u64 + 1,
        end_line: node.end_position().row as u64 + 1,
        visibility: go_visibility(&name),
        properties: Vec::new(),
    });
    ctx.refs.push(ExtractedRef {
        kind: "Defines".to_string(),
        from_qualified_name: scope.to_string(),
        to_qualified_name: qn.clone(),
    });
    if let Some(body) = node.child_by_field_name("body") {
        extract_calls(ctx, body, &qn);
    }
}


pub(super) fn extract_method(ctx: &mut Ctx, node: Node, scope: &str) {
    let name = node_field_text(ctx.source, node, "name");
    if name.is_empty() {
        return;
    }
    // Receiver type — strip ``(*T)`` or ``(T)`` to ``T``.
    let recv = node_field_text(ctx.source, node, "receiver");
    let recv_type = recv
        .trim()
        .trim_start_matches('(')
        .trim_end_matches(')')
        .split_whitespace()
        .last()
        .unwrap_or("")
        .trim_start_matches('*')
        .to_string();
    let scope_qn = if recv_type.is_empty() {
        scope.to_string()
    } else {
        qual(scope, &recv_type)
    };
    let seq = {
        ctx.next_seq += 1;
        ctx.next_seq
    };
    let qn = format!("{}::{}#{}", scope_qn, name, seq);
    ctx.nodes.push(ExtractedNode {
        label: LABEL_METHOD.to_string(),
        name: name.clone(),
        qualified_name: qn.clone(),
        start_line: node.start_position().row as u64 + 1,
        end_line: node.end_position().row as u64 + 1,
        visibility: go_visibility(&name),
        properties: vec![("receiver_type".to_string(), recv_type.clone())],
    });
    ctx.refs.push(ExtractedRef {
        kind: "HasMethod".to_string(),
        from_qualified_name: scope_qn.clone(),
        to_qualified_name: qn.clone(),
    });
    if let Some(body) = node.child_by_field_name("body") {
        extract_calls(ctx, body, &qn);
    }
}
