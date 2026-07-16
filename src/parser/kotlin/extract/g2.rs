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

/// Decides whether a call's receiver/qualifier text is safe disambiguating
/// evidence to preserve, or must be discarded down to the bare tail name.
///
/// source: issue #29 — the resolver previously received ONLY `callee_tail`
/// (`rsplit('.').next()`), throwing away every receiver/qualifier and
/// making same-named in-repo calls unresolvable. Preserving it blindly
/// re-introduces a DIFFERENT bug: `viewModel.load()` is a VALUE receiver
/// (a local val/property; this parser has no type information for it), and
/// feeding "viewModel.load" into the resolver's qualified-callee evidence
/// path risks a false string-match against an unrelated `load` symbol that
/// happens to live in a package/file whose name coincides with the local
/// variable (see call_evidence.rs / resolver.rs::resolve_single_call).
///
/// Kotlin coding conventions (kotlinlang.org/docs/coding-conventions.html):
/// package names are lowercase, class/object names are UpperCamelCase,
/// local vals/vars/properties are lowerCamelCase. A single-dot receiver
/// whose first segment starts lowercase (`viewModel.load`) is therefore
/// always a value/property access, never a package or object qualifier —
/// discard it back to the tail. Two or more dots (`com.foo.bar.process`,
/// a package-qualified call) or an uppercase-first single dot
/// (`Utils.process`, an object/class qualifier) are real disambiguating
/// evidence — preserved as-is.
/// postcondition: returns `tail` (the bare identifier) for a value
/// receiver; returns `raw` (the full dotted text) otherwise.
pub(super) fn qualifier_or_tail(raw: &str, tail: &str) -> String {
    let dot_count = raw.matches('.').count();
    if dot_count == 0 {
        return tail.to_string();
    }
    let first_seg_uppercase = raw
        .split('.')
        .next()
        .and_then(|s| s.chars().next())
        .map(|c| c.is_uppercase())
        .unwrap_or(false);
    if dot_count == 1 && !first_seg_uppercase {
        tail.to_string()
    } else {
        raw.to_string()
    }
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
            let raw_callee = callee.trim_end_matches('(').to_string();
            let callee_tail = raw_callee.rsplit('.').next().unwrap_or("").to_string();
            let callee_repr = qualifier_or_tail(&raw_callee, &callee_tail);
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
                    properties: vec![("callee_name".to_string(), callee_repr.clone())],
                });
                ctx.refs.push(ExtractedRef {
                    kind: "Calls".to_string(),
                    from_qualified_name: caller_qn.to_string(),
                    to_qualified_name: callee_repr,
                });
            }
        }
        let mut cursor = n.walk();
        for c in n.children(&mut cursor) {
            stack.push(c);
        }
    }
}
