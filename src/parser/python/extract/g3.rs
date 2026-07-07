// parser::python::extract::g3 — see ../extract/mod.rs.

use tree_sitter::Node;
use crate::parser::*;      // ExtractedNode, ExtractedRef, node_text, qual, LABEL_*, …
use super::super::*;       // parent module: Ctx, TS_* consts, kept helpers
              // sibling extract fns (glob re-export)


pub(super) fn extract_single_call_site(ctx: &mut ExtractCtx, node: Node, caller_qn: &str) {
    let func_node = match node.child_by_field_name("function") {
        Some(f) => f,
        None => return,
    };
    let callee = node_text(ctx.source, func_node);
    if callee.is_empty() {
        return;
    }
    // source: Spike B' BUG #10 fix. Previously skipped any callee containing
    // '.' as a known limitation, which dropped every method call (self.foo,
    // module.func, obj.attr) — the bulk of real Python call edges. We now
    // emit a CallSite node for every call_expression regardless of whether
    // the callee is a bare identifier or an attribute access. The resolver
    // decides whether it can resolve the target; unresolved targets stay as
    // call_sites with no Calls edge until BUG #11 is also fixed (see
    // resolver.rs resolve_calls).
    let line = node.start_position().row as u64 + 1;
    let col = node.start_position().column as u64;
    // Chained calls (f()()) share start_byte because the outer call's
    // function child is the inner call (same starting token). Use the
    // (start_byte, end_byte) span — outer call ends after the trailing
    // ``)``, inner ends earlier — to give every call_expression a unique
    // primary key while preserving the human-readable line:col prefix.
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
}


/// Detects async def by checking if "async" keyword precedes "def".
pub(super) fn is_async_function(source: &str, node: Node) -> bool {
    // In tree-sitter-python, async functions are still function_definition
    // but the parent or a sibling might be "async" keyword, or the node text starts with "async"
    let text = &source[node.byte_range()];
    text.starts_with("async ")
}
