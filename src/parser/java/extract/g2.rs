// parser::java::extract::g2 — see ../extract/mod.rs.

use super::super::*; // parent module: Ctx, TS_* consts, kept helpers
use super::*;
use crate::parser::*; // ExtractedNode, ExtractedRef, node_text, qual, LABEL_*, …
use tree_sitter::Node; // sibling extract fns (glob re-export)

pub(super) fn extract_field(ctx: &mut Ctx, node: Node, scope: &str) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "variable_declarator" {
            let name = node_field_text(ctx.source, child, "name");
            if name.is_empty() {
                continue;
            }
            let qn = qual(scope, &name);
            ctx.nodes.push(ExtractedNode {
                label: LABEL_CONSTANT.to_string(),
                name: name.clone(),
                qualified_name: qn.clone(),
                start_line: child.start_position().row as u64 + 1,
                end_line: child.end_position().row as u64 + 1,
                visibility: visibility_from_modifiers(ctx.source, node),
                properties: Vec::new(),
            });
            ctx.refs.push(ExtractedRef {
                kind: "Defines".to_string(),
                from_qualified_name: scope.to_string(),
                to_qualified_name: qn,
            });
        }
    }
}

pub(super) fn extract_import(ctx: &mut Ctx, node: Node, scope: &str) {
    let text = node_text(ctx.source, node);
    // ``import a.b.C;`` or ``import static a.b.C.method;``
    let cleaned = text
        .trim()
        .trim_start_matches("import")
        .trim()
        .trim_start_matches("static")
        .trim()
        .trim_end_matches(';')
        .trim()
        .to_string();
    if cleaned.is_empty() {
        return;
    }
    let name = cleaned.rsplit('.').next().unwrap_or("").to_string();
    let qn = qual(scope, &format!("import:{cleaned}"));
    ctx.nodes.push(ExtractedNode {
        label: LABEL_IMPORT.to_string(),
        name,
        qualified_name: qn.clone(),
        start_line: node.start_position().row as u64 + 1,
        end_line: node.end_position().row as u64 + 1,
        visibility: "package".to_string(),
        properties: vec![("path".to_string(), cleaned.clone())],
    });
    ctx.refs.push(ExtractedRef {
        kind: "Imports".to_string(),
        from_qualified_name: scope.to_string(),
        to_qualified_name: cleaned,
    });
}

pub(super) fn extract_calls(ctx: &mut Ctx, root: Node, caller_qn: &str) {
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        if n.kind() == TS_CALL || n.kind() == TS_OBJECT_CREATION {
            let callee = node_field_text(ctx.source, n, "name");
            let callee = if callee.is_empty() {
                // ``new X()`` uses the ``type`` field for the class name.
                node_field_text(ctx.source, n, "type")
            } else {
                callee
            };
            if !callee.is_empty() {
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
                    name: callee.clone(),
                    qualified_name: site_qn.clone(),
                    start_line: n.start_position().row as u64 + 1,
                    end_line: n.end_position().row as u64 + 1,
                    visibility: "package".to_string(),
                    properties: vec![("callee_name".to_string(), callee.clone())],
                });
                ctx.refs.push(ExtractedRef {
                    kind: "Calls".to_string(),
                    from_qualified_name: caller_qn.to_string(),
                    to_qualified_name: callee,
                });
            }
        }
        let mut cursor = n.walk();
        for c in n.children(&mut cursor) {
            stack.push(c);
        }
    }
}
