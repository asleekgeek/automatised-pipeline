// parser::c::extract::g1 — see ../extract/mod.rs.

use tree_sitter::Node;
use crate::parser::*;      // ExtractedNode, ExtractedRef, node_text, qual, LABEL_*, …
use super::super::*;       // parent module: Ctx, TS_* consts, kept helpers
use super::*;              // sibling extract fns (glob re-export)


pub(crate) fn extract_top(ctx: &mut Ctx, parent: Node, scope: &str) {
    let mut cursor = parent.walk();
    for child in parent.children(&mut cursor) {
        match child.kind() {
            TS_STRUCT | TS_UNION => extract_struct(ctx, child, scope, LABEL_STRUCT),
            TS_ENUM => extract_enum(ctx, child, scope),
            TS_TYPEDEF => extract_typedef(ctx, child, scope),
            TS_FUNCTION_DEF => extract_function(ctx, child, scope),
            TS_INCLUDE => extract_include(ctx, child, scope),
            TS_FUNCTION_DECL => {
                // Function prototypes / forward declarations. The grammar
                // also uses ``declaration`` for globals — we only emit
                // function declarations.
                if is_function_prototype(child) {
                    extract_function_prototype(ctx, child, scope);
                }
            }
            _ => {
                if child.named_child_count() > 0 {
                    extract_top(ctx, child, scope);
                }
            }
        }
    }
}


fn is_function_prototype(node: Node) -> bool {
    let mut cursor = node.walk();
    for c in node.children(&mut cursor) {
        if c.kind() == "function_declarator" {
            return true;
        }
        if c.kind() == "init_declarator" {
            // ``int f(void) = ...;`` rare, but still has a function_declarator inside.
            let mut ic = c.walk();
            for gc in c.children(&mut ic) {
                if gc.kind() == "function_declarator" {
                    return true;
                }
            }
        }
    }
    false
}


fn find_identifier(source: &str, node: Node) -> String {
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        if n.kind() == "identifier" || n.kind() == "type_identifier" {
            return node_text(source, n);
        }
        let mut cursor = n.walk();
        for c in n.children(&mut cursor) {
            stack.push(c);
        }
    }
    String::new()
}


fn extract_struct(ctx: &mut Ctx, node: Node, scope: &str, label: &str) {
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
    // Struct members. struct_specifier's `body` field is a field_declaration_list
    // holding field_declaration nodes. Each field's name is a `field_identifier`
    // inside the (possibly pointer/array) `declarator` field.
    // source: tree-sitter-c v0.23.4 (struct_specifier/field_declaration).
    if let Some(body) = node.child_by_field_name("body") {
        extract_struct_fields(ctx, body, &qn);
    }
}


/// Emits Field nodes + HasField edges for each field_declaration in a
/// field_declaration_list. The field name is the first `field_identifier`
/// descendant of the `declarator` field (handles `int *p;`, `char buf[8];`).
fn extract_struct_fields(ctx: &mut Ctx, body: Node, owner_qn: &str) {
    let mut cursor = body.walk();
    for fd in body.children(&mut cursor) {
        if fd.kind() != "field_declaration" {
            continue;
        }
        let type_text = node_field_text(ctx.source, fd, "type");
        // field_declaration's `declarator` field is `multiple` — `int a, b, c;`
        // is ONE field_declaration carrying three declarators. child_by_field_name
        // would return only the first; walk with the cursor.field_name() idiom
        // (as go.rs) to emit one Field per declared variable. Each declarator may
        // be wrapped (pointer/array), so descend for its field_identifier.
        // source: tree-sitter-c v0.23.4 field_declaration.declarator (multiple:true).
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
            let fname = find_field_identifier(ctx.source, declarator);
            if fname.is_empty() {
                continue; // anonymous / unnamed member
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


/// Finds the first `field_identifier` anywhere under `node` (the field name,
/// unwrapping pointer/array/function declarators).
fn find_field_identifier(source: &str, node: Node) -> String {
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


fn extract_enum(ctx: &mut Ctx, node: Node, scope: &str) {
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
        to_qualified_name: qn.clone(),
    });
    // Enum entries as constants.
    if let Some(body) = node.child_by_field_name("body") {
        let mut cursor = body.walk();
        for child in body.children(&mut cursor) {
            if child.kind() == "enumerator" {
                let en = find_identifier(ctx.source, child);
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
}


fn extract_typedef(ctx: &mut Ctx, node: Node, scope: &str) {
    let name = find_identifier(ctx.source, node);
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
