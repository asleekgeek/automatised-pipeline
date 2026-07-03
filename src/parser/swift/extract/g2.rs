// parser::swift::extract::g2 — see ../extract/mod.rs.

use tree_sitter::Node;
use crate::parser::*;      // ExtractedNode, ExtractedRef, node_text, qual, LABEL_*, …
use super::super::*;       // parent module: Ctx, TS_* consts, kept helpers
use super::*;              // sibling extract fns (glob re-export)


/// Emits init/deinit/subscript declarations as a Method (inside a type) or
/// Function (rare, top level). These have no `name` field in the grammar, so a
/// synthetic name is supplied by the caller. The body is a `function_body`
/// field, so call extraction works the same as for a normal function.
fn extract_member_fn(
    ctx: &mut Ctx,
    node: Node,
    scope: &str,
    enclosing_type: Option<&str>,
    synth_name: &str,
) {
    let seq = {
        ctx.next_seq += 1;
        ctx.next_seq
    };
    let qn = format!("{}::{}#{}", scope, synth_name, seq);
    let label = if enclosing_type.is_some() {
        LABEL_METHOD
    } else {
        LABEL_FUNCTION
    };
    let mut props = vec![("member_kind".to_string(), synth_name.to_string())];
    if let Some(rec) = enclosing_type {
        props.push(("receiver_type".to_string(), rec.to_string()));
    }
    ctx.nodes.push(ExtractedNode {
        label: label.to_string(),
        name: synth_name.to_string(),
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


/// Emits an enum case (`enum_entry`) as a Variant of the enclosing enum. The
/// case name is the `name` field (a `simple_identifier`).
/// source: alex-pinkus/tree-sitter-swift v0.7.3 enum_entry.name.
fn extract_enum_entry(ctx: &mut Ctx, node: Node, scope: &str) {
    // enum_entry's `name` field is `multiple` — `case red, green, blue` is ONE
    // enum_entry node carrying three simple_identifier names. Emit one Variant
    // per name; child_by_field_name would return only the first and silently
    // drop the rest. Uses the cursor.field_name() idiom (as objc.rs/go.rs).
    // source: alex-pinkus/tree-sitter-swift v0.7.3 enum_entry.name (multiple:true).
    let mut names: Vec<String> = Vec::new();
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            if cursor.field_name() == Some("name") {
                let t = node_text(ctx.source, cursor.node());
                if !t.is_empty() {
                    names.push(t);
                }
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    if names.is_empty() {
        let fallback = find_name(ctx.source, node);
        if !fallback.is_empty() {
            names.push(fallback);
        }
    }
    for name in names {
        let qn = qual(scope, &name);
        ctx.nodes.push(ExtractedNode {
            label: crate::parser::LABEL_VARIANT.to_string(),
            name: name.clone(),
            qualified_name: qn.clone(),
            start_line: node.start_position().row as u64 + 1,
            end_line: node.end_position().row as u64 + 1,
            visibility: "internal".to_string(),
            properties: Vec::new(),
        });
        ctx.refs.push(ExtractedRef {
            kind: "Defines".to_string(),
            from_qualified_name: scope.to_string(),
            to_qualified_name: qn,
        });
    }
}


fn extract_property(ctx: &mut Ctx, node: Node, scope: &str) {
    let name = find_name(ctx.source, node);
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


fn extract_typealias(ctx: &mut Ctx, node: Node, scope: &str) {
    let name = find_name(ctx.source, node);
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
        properties: vec![("typealias".to_string(), "true".to_string())],
    });
    ctx.refs.push(ExtractedRef {
        kind: "Defines".to_string(),
        from_qualified_name: scope.to_string(),
        to_qualified_name: qn,
    });
}


fn extract_import(ctx: &mut Ctx, node: Node, scope: &str) {
    let text = node_text(ctx.source, node);
    let cleaned = text
        .trim()
        .trim_start_matches("import")
        .trim()
        .to_string();
    if cleaned.is_empty() {
        return;
    }
    let qn = qual(scope, &format!("import:{cleaned}"));
    ctx.nodes.push(ExtractedNode {
        label: LABEL_IMPORT.to_string(),
        name: cleaned.clone(),
        qualified_name: qn,
        start_line: node.start_position().row as u64 + 1,
        end_line: node.end_position().row as u64 + 1,
        visibility: "internal".to_string(),
        properties: vec![("path".to_string(), cleaned.clone())],
    });
    ctx.refs.push(ExtractedRef {
        kind: "Imports".to_string(),
        from_qualified_name: scope.to_string(),
        to_qualified_name: cleaned,
    });
}


fn extract_calls(ctx: &mut Ctx, root: Node, caller_qn: &str) {
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        if n.kind() == TS_CALL_EXPR {
            let callee_text = n
                .named_child(0)
                .map(|c| node_text(ctx.source, c))
                .unwrap_or_default();
            let tail = callee_text
                .rsplit('.')
                .next()
                .unwrap_or("")
                .trim_end_matches('(')
                .trim()
                .to_string();
            if !tail.is_empty()
                && tail.chars().next().map_or(false, |c| c.is_alphabetic() || c == '_')
            {
                let seq = {
                    ctx.next_seq += 1;
                    ctx.next_seq
                };
                let site_qn = format!(
                    "{}::call@{}:{}#{}",
                    caller_qn,
                    n.start_position().row + 1,
                    n.start_position().column + 1,
                    seq,
                );
                ctx.nodes.push(ExtractedNode {
                    label: LABEL_CALL_SITE.to_string(),
                    name: tail.clone(),
                    qualified_name: site_qn.clone(),
                    start_line: n.start_position().row as u64 + 1,
                    end_line: n.end_position().row as u64 + 1,
                    visibility: "internal".to_string(),
                    properties: vec![("callee_name".to_string(), tail.clone())],
                });
                ctx.refs.push(ExtractedRef {
                    kind: "Calls".to_string(),
                    from_qualified_name: caller_qn.to_string(),
                    to_qualified_name: tail,
                });
            }
        }
        let mut cursor = n.walk();
        for c in n.children(&mut cursor) {
            stack.push(c);
        }
    }
}
