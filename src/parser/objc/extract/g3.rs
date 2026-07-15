// parser::objc::extract::g3 — see ../extract/mod.rs.

use super::super::*; // parent module: Ctx, TS_* consts, kept helpers
use super::*;
use crate::parser::*; // ExtractedNode, ExtractedRef, node_text, qual, LABEL_*, …
use tree_sitter::Node; // sibling extract fns (glob re-export)

/// The declared alias name of a typedef — the `type_identifier` reached through
/// the `declarator` field (unwrapping pointer/array declarators). Falls back to
/// the last `type_identifier` descendant.
pub(super) fn find_c_typedef_name(source: &str, node: Node) -> String {
    let start = node.child_by_field_name("declarator").unwrap_or(node);
    let mut last = String::new();
    let mut stack = vec![start];
    while let Some(n) = stack.pop() {
        if n.kind() == "type_identifier" {
            last = node_text(source, n);
        }
        let mut cursor = n.walk();
        for c in n.children(&mut cursor) {
            stack.push(c);
        }
    }
    last
}

pub(super) fn extract_import(ctx: &mut Ctx, node: Node, scope: &str) {
    let text = node_text(ctx.source, node);
    let cleaned = text
        .trim()
        .trim_start_matches("#import")
        .trim_start_matches("#include")
        .trim()
        .trim_matches('<')
        .trim_matches('>')
        .trim_matches('"')
        .trim()
        .to_string();
    if cleaned.is_empty() {
        return;
    }
    let name = cleaned.rsplit('/').next().unwrap_or(&cleaned).to_string();
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

pub(super) fn extract_module_import(ctx: &mut Ctx, node: Node, scope: &str) {
    let text = node_text(ctx.source, node);
    let cleaned = text
        .trim()
        .trim_start_matches("@import")
        .trim_end_matches(';')
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
        match n.kind() {
            TS_CALL => {
                let callee = node_field_text(ctx.source, n, "function");
                let callee = callee
                    .rsplit('.')
                    .next()
                    .unwrap_or("")
                    .trim_end_matches('(')
                    .to_string();
                emit_call(ctx, n, caller_qn, &callee);
            }
            TS_MSG_EXPR => {
                // ``[receiver method:arg other:arg]`` — the selector is the
                // `method` field, which is `multiple` in grammar 3.0.2: one
                // `identifier` per keyword. Concatenate them into the selector
                // (`method:other:`); a unary message yields a single identifier.
                // source: tree-sitter-objc v3.0.2 message_expression.method.
                let sel = message_selector(ctx.source, n);
                if !sel.is_empty() {
                    emit_call(ctx, n, caller_qn, &sel);
                }
            }
            _ => {}
        }
        let mut cursor = n.walk();
        for c in n.children(&mut cursor) {
            stack.push(c);
        }
    }
}

pub(super) fn emit_call(ctx: &mut Ctx, n: Node, caller_qn: &str, callee: &str) {
    if callee.is_empty()
        || !callee
            .chars()
            .next()
            .map_or(false, |c| c.is_alphabetic() || c == '_')
    {
        return;
    }
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
        name: callee.to_string(),
        qualified_name: site_qn,
        start_line: n.start_position().row as u64 + 1,
        end_line: n.end_position().row as u64 + 1,
        visibility: "public".to_string(),
        properties: vec![("callee_name".to_string(), callee.to_string())],
    });
    ctx.refs.push(ExtractedRef {
        kind: "Calls".to_string(),
        from_qualified_name: caller_qn.to_string(),
        to_qualified_name: callee.to_string(),
    });
}
