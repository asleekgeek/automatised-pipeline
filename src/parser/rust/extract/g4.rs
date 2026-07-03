// parser::rust::extract::g4 — see ../extract/mod.rs.

use tree_sitter::Node;
use crate::parser::*;      // ExtractedNode, ExtractedRef, node_text, qual, LABEL_*, …
use super::super::*;       // parent module: Ctx, TS_* consts, kept helpers
use super::*;              // sibling extract fns (glob re-export)


// ---------------------------------------------------------------------------
// Module extraction
// ---------------------------------------------------------------------------

fn extract_mod(ctx: &mut ExtractCtx, node: Node, scope: &str) {
    let name = node_field_text(ctx.source, node, "name");
    if name.is_empty() {
        return;
    }
    let qn = qual(scope, &name);
    ctx.nodes.push(ExtractedNode {
        label: LABEL_MODULE.to_string(),
        name: name.clone(),
        qualified_name: qn.clone(),
        start_line: node.start_position().row as u64 + 1,
        end_line: node.end_position().row as u64 + 1,
        visibility: extract_visibility(ctx.source, node),
        properties: vec![],
    });
    ctx.refs.push(ExtractedRef {
        kind: "Defines".to_string(),
        from_qualified_name: scope.to_string(),
        to_qualified_name: qn.clone(),
    });
    if let Some(body) = node.child_by_field_name("body") {
        if body.kind() == TS_DECL_LIST {
            extract_top_level(ctx, body, &qn);
        }
    }
}


// ---------------------------------------------------------------------------
// Call-site extraction
// ---------------------------------------------------------------------------

fn extract_call_sites(ctx: &mut ExtractCtx, body: Node, caller_qn: &str) {
    let mut stack = vec![body];
    while let Some(node) = stack.pop() {
        match node.kind() {
            TS_CALL_EXPR => extract_single_call_site(ctx, node, caller_qn),
            TS_MACRO_INVOCATION => extract_macro_call_site(ctx, node, caller_qn),
            _ => {}
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            stack.push(child);
        }
    }
}


/// Emit a CallSite node for a `name!(...)` macro invocation. The callee_name
/// is stored with a trailing `!` so the resolver's Layer 4 pass can cheaply
/// distinguish macros from regular function calls. Q8 (Defines_File_Function)
/// does not match CallSite nodes and is unaffected. Q14 (unresolved external
/// refs in file F) consults the CallSite table; macro CallSites must be
/// flagged so they aren't counted as unresolved — the resolver wires them
/// to StdlibSymbol targets. source: stages/stage-3b-v2.md §5 Layer 4.
fn extract_macro_call_site(ctx: &mut ExtractCtx, node: Node, caller_qn: &str) {
    let macro_name = match node.child_by_field_name("macro") {
        Some(n) => node_text(ctx.source, n),
        None => return,
    };
    if macro_name.is_empty() {
        return;
    }
    let line = node.start_position().row as u64 + 1;
    let col = node.start_position().column as u64;
    let start_byte = node.start_byte() as u64;
    let end_byte = node.end_byte() as u64;
    let marker = format!("{macro_name}!");
    // Chained calls share start_byte; the (start, end) span is unique.
    let cs_id = format!("{caller_qn}::call@{line}:{col}#{start_byte}-{end_byte}");
    ctx.nodes.push(ExtractedNode {
        label: LABEL_CALL_SITE.to_string(),
        name: marker.clone(),
        qualified_name: cs_id.clone(),
        start_line: line,
        end_line: line,
        visibility: String::new(),
        properties: vec![
            ("callee_name".to_string(), marker),
            ("caller_qn".to_string(), caller_qn.to_string()),
        ],
    });
    ctx.refs.push(ExtractedRef {
        kind: "Defines".to_string(),
        from_qualified_name: caller_qn.to_string(),
        to_qualified_name: cs_id,
    });
}


fn extract_single_call_site(ctx: &mut ExtractCtx, node: Node, caller_qn: &str) {
    let func_node = match node.child_by_field_name("function") {
        Some(f) => f,
        None => return,
    };
    let callee = node_text(ctx.source, func_node);
    // source: Spike B' BUG #10 fix — was `callee.contains('.')` which dropped
    // every method call (obj.method, etc.). Now extract all call_expression
    // nodes; resolver decides what can be resolved.
    if callee.is_empty() {
        return;
    }
    let line = node.start_position().row as u64 + 1;
    let col = node.start_position().column as u64;
    // Chained calls share start_byte; (start, end) span is unique.
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


// ---------------------------------------------------------------------------
// Supertrait extraction
// ---------------------------------------------------------------------------

fn extract_supertraits(source: &str, trait_node: Node) -> Vec<String> {
    let mut supers = Vec::new();
    let bounds = match trait_node.child_by_field_name("bounds") {
        Some(b) => b,
        None => return supers,
    };
    let mut cursor = bounds.walk();
    for child in bounds.children(&mut cursor) {
        if child.kind() == "type_identifier" || child.kind() == "scoped_type_identifier" {
            let text = node_text(source, child);
            if !text.is_empty() {
                supers.push(text);
            }
        }
    }
    supers
}


// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn extract_visibility(source: &str, node: Node) -> String {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == TS_VIS_MOD {
            return node_text(source, child);
        }
    }
    String::new()
}


fn has_async_modifier(node: Node) -> bool {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == TS_FUNC_MODS {
            let mut inner = child.walk();
            for gc in child.children(&mut inner) {
                if gc.kind() == "async" {
                    return true;
                }
            }
        }
    }
    false
}
