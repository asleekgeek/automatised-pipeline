// parser::go::extract::g2 — see ../extract/mod.rs.

use tree_sitter::Node;
use crate::parser::*;      // ExtractedNode, ExtractedRef, node_text, qual, LABEL_*, …
use super::super::*;       // parent module: Ctx, TS_* consts, kept helpers
use super::*;              // sibling extract fns (glob re-export)


pub(super) fn extract_value_decl(ctx: &mut Ctx, node: Node, scope: &str) {
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        if n.kind() == "const_spec" || n.kind() == "var_spec" {
            let mut cursor = n.walk();
            for child in n.children(&mut cursor) {
                if child.kind() == "identifier" {
                    let name = node_text(ctx.source, child);
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
                        visibility: go_visibility(&name),
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
        let mut cursor = n.walk();
        for c in n.children(&mut cursor) {
            stack.push(c);
        }
    }
}


pub(super) fn extract_calls(ctx: &mut Ctx, root: Node, caller_qn: &str) {
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        if n.kind() == TS_CALL_EXPR {
            let callee = node_field_text(ctx.source, n, "function");
            let tail = callee
                .rsplit('.')
                .next()
                .unwrap_or("")
                .trim_end_matches('(')
                .trim()
                .to_string();
            if !tail.is_empty()
                && tail
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
                    name: tail.clone(),
                    qualified_name: site_qn.clone(),
                    start_line: n.start_position().row as u64 + 1,
                    end_line: n.end_position().row as u64 + 1,
                    visibility: "public".to_string(),
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
