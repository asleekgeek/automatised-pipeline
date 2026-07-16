// parser::typescript::extract::g4 — see ../extract/mod.rs.

use super::super::*;
use crate::parser::*; // ExtractedNode, ExtractedRef, node_text, qual, LABEL_*, …
use tree_sitter::Node; // parent module: Ctx, TS_* consts, kept helpers
                       // sibling extract fns (glob re-export)

pub(super) fn extract_single_call_site(ctx: &mut ExtractCtx, node: Node, caller_qn: &str) {
    let func_node = match node.child_by_field_name("function") {
        Some(f) => f,
        None => return,
    };
    let callee = node_text(ctx.source, func_node);
    // source: Spike B' BUG #10 fix — was `callee.contains('.')` which dropped
    // every member-access call (obj.method, etc.). Now extract all call
    // nodes; resolver decides what can be resolved.
    if callee.is_empty() {
        return;
    }
    // Last path segment (obj.method -> method), matching the `callee_tail`
    // convention the sister-language extractors use for their Calls refs.
    let callee_tail = callee.rsplit('.').next().unwrap_or(&callee).to_string();
    let line = node.start_position().row as u64 + 1;
    let col = node.start_position().column as u64;
    // Chained calls (f()()) share start_byte; (start_byte, end_byte) span
    // is uniquely identifying.
    let start_byte = node.start_byte() as u64;
    let end_byte = node.end_byte() as u64;
    let cs_id = format!("{caller_qn}::call@{line}:{col}#{start_byte}-{end_byte}");
    ctx.nodes.push(ExtractedNode {
        label: LABEL_CALL_SITE.to_string(),
        name: callee.clone(),
        qualified_name: cs_id.clone(),
        start_line: line,
        end_line: line,
        visibility: String::new(),
        properties: vec![
            ("callee_name".to_string(), callee),
            ("caller_qn".to_string(), caller_qn.to_string()),
        ],
    });
    ctx.refs.push(ExtractedRef {
        kind: "Defines".to_string(),
        from_qualified_name: caller_qn.to_string(),
        to_qualified_name: cs_id,
    });
    // Cross-language-consistent call edge: caller -> callee name. The sister
    // extractors (c/cpp/go/java/kotlin/objc/swift) all emit this "Calls" ref;
    // TypeScript previously emitted only the Defines edge to the call-site node,
    // so call graphs over .ts/.tsx were missing every Calls edge (F4).
    ctx.refs.push(ExtractedRef {
        kind: "Calls".to_string(),
        from_qualified_name: caller_qn.to_string(),
        to_qualified_name: callee_tail,
    });
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

pub(super) fn has_export_keyword(node: Node) -> bool {
    if let Some(prev) = node.prev_sibling() {
        if prev.kind() == "export" {
            return true;
        }
    }
    false
}

pub(super) fn has_async_keyword(source: &str, node: Node) -> bool {
    let text = &source[node.byte_range()];
    text.starts_with("async ")
}

pub(super) fn is_const_declaration(source: &str, node: Node) -> bool {
    let text = &source[node.byte_range()];
    text.starts_with("const ")
}

pub(super) fn extract_ts_member_visibility(source: &str, node: Node) -> String {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "accessibility_modifier" {
            return node_text(source, child);
        }
    }
    String::new()
}
