// parser::kotlin::extract::g2 — see ../extract/mod.rs.

use super::super::*;
use crate::parser::*; // ExtractedNode, ExtractedRef, node_text, qual, LABEL_*, …
use tree_sitter::Node; // parent module: Ctx, TS_* consts, kept helpers
                       // sibling extract fns (glob re-export)

pub(super) fn extract_import(ctx: &mut Ctx, node: Node, scope: &str) {
    let text = node_text(ctx.source, node);
    let cleaned = text
        .trim()
        .trim_start_matches("import")
        .trim()
        .trim_end_matches(';')
        .trim()
        .to_string();
    if cleaned.is_empty() {
        return;
    }
    let name = cleaned
        .rsplit('.')
        .next()
        .unwrap_or("")
        .trim_end_matches('*')
        .to_string();
    let qn = qual(scope, &format!("import:{cleaned}"));
    ctx.nodes.push(ExtractedNode {
        label: LABEL_IMPORT.to_string(),
        name,
        qualified_name: qn,
        start_line: node.start_position().row as u64 + 1,
        end_line: node.end_position().row as u64 + 1,
        visibility: "public".to_string(),
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
        if n.kind() == TS_CALL_EXPR {
            // In tree-sitter-kotlin-ng call_expression has no `callee` field; its
            // children are `expression, type_arguments, value_arguments,
            // annotated_lambda`, where `expression` is a tree-sitter SUPERTYPE
            // (it never appears as a node kind — only its concrete subtypes do,
            // e.g. `navigation_expression` for `x.bar`, or a bare `identifier`).
            // The callee is that first expression child, which is also simply the
            // first child of the call_expression. For a method call it is a
            // `navigation_expression`; the `.rsplit('.')` below reduces it to the
            // tail identifier either way.
            let callee = child_of_kind(n, "navigation_expression")
                .or_else(|| {
                    let mut cursor = n.walk();
                    let first = n.children(&mut cursor).next();
                    first
                })
                .map(|c| node_text(ctx.source, c))
                .unwrap_or_default();
            // Keep only the tail identifier to match the file::name convention
            // used by the rest of the graph.
            let callee_tail = callee
                .rsplit('.')
                .next()
                .unwrap_or("")
                .trim_end_matches('(')
                .to_string();
            if !callee_tail.is_empty()
                && callee_tail
                    .chars()
                    .next()
                    .map_or(false, |c| c.is_alphabetic() || c == '_')
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
                    name: callee_tail.clone(),
                    qualified_name: site_qn.clone(),
                    start_line: n.start_position().row as u64 + 1,
                    end_line: n.end_position().row as u64 + 1,
                    visibility: "public".to_string(),
                    properties: vec![("callee_name".to_string(), callee_tail.clone())],
                });
                ctx.refs.push(ExtractedRef {
                    kind: "Calls".to_string(),
                    from_qualified_name: caller_qn.to_string(),
                    to_qualified_name: callee_tail,
                });
            }
        }
        let mut cursor = n.walk();
        for c in n.children(&mut cursor) {
            stack.push(c);
        }
    }
}
