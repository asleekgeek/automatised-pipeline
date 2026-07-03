// parser::python::extract::g1 — see ../extract/mod.rs.

use tree_sitter::Node;
use crate::parser::*;      // ExtractedNode, ExtractedRef, node_text, qual, LABEL_*, …
use super::super::*;       // parent module: Ctx, TS_* consts, kept helpers
use super::*;              // sibling extract fns (glob re-export)


// ---------------------------------------------------------------------------
// Top-level extraction
// ---------------------------------------------------------------------------

fn extract_top_level(
    ctx: &mut ExtractCtx,
    parent: Node,
    scope: &str,
    enclosing_class: Option<&str>,
) {
    let mut cursor = parent.walk();
    for child in parent.children(&mut cursor) {
        match child.kind() {
            TS_FUNCTION_DEF => {
                extract_function_or_method(ctx, child, scope, enclosing_class, &[]);
            }
            TS_CLASS_DEF => extract_class(ctx, child, scope),
            TS_IMPORT_STMT => extract_import(ctx, child, scope),
            TS_IMPORT_FROM => extract_import_from(ctx, child, scope),
            // source: Spike B' BUG #13 — route __future__ imports through the
            // same emit path. They have no module_name field (module is
            // implicitly __future__), so extract_future_import hardcodes it.
            TS_FUTURE_IMPORT => extract_future_import(ctx, child, scope),
            TS_DECORATED_DEF => extract_decorated(ctx, child, scope, enclosing_class),
            TS_EXPRESSION_STMT => {
                // Check for module-level constant assignments
                if enclosing_class.is_none() {
                    extract_module_constant(ctx, child, scope);
                }
            }
            _ => {}
        }
    }
}


// ---------------------------------------------------------------------------
// Function / Method extraction
// ---------------------------------------------------------------------------

fn extract_function_or_method(
    ctx: &mut ExtractCtx,
    node: Node,
    scope: &str,
    enclosing_class: Option<&str>,
    decorators: &[String],
) {
    let name = node_field_text(ctx.source, node, "name");
    if name.is_empty() {
        return;
    }

    let is_async = is_async_function(ctx.source, node);
    let visibility = python_visibility(&name);
    let raw_qn = qual(scope, &name);
    let start_line = node.start_position().row as u64 + 1;
    // Disambiguate @property/@setter/@deleter overloads (and any other
    // legitimately-same-named symbols within the same scope) so each
    // gets a unique primary key. Resolver name-based lookups still work.
    let qn = ctx.dedup_qn(raw_qn, start_line);

    let mut props = vec![
        ("is_async".to_string(), is_async.to_string()),
    ];
    if !decorators.is_empty() {
        props.push(("decorators".to_string(), decorators.join(",")));
    }

    if let Some(class_name) = enclosing_class {
        // It's a method
        props.push(("receiver_type".to_string(), class_name.to_string()));
        ctx.nodes.push(ExtractedNode {
            label: LABEL_METHOD.to_string(),
            name: name.clone(),
            qualified_name: qn.clone(),
            start_line,
            end_line: node.end_position().row as u64 + 1,
            visibility,
            properties: props,
        });
        ctx.refs.push(ExtractedRef {
            kind: "HasMethod".to_string(),
            from_qualified_name: scope.to_string(),
            to_qualified_name: qn.clone(),
        });
    } else {
        // Top-level function
        ctx.nodes.push(ExtractedNode {
            label: LABEL_FUNCTION.to_string(),
            name: name.clone(),
            qualified_name: qn.clone(),
            start_line,
            end_line: node.end_position().row as u64 + 1,
            visibility,
            properties: props,
        });
        ctx.refs.push(ExtractedRef {
            kind: "Defines".to_string(),
            from_qualified_name: scope.to_string(),
            to_qualified_name: qn.clone(),
        });
    }

    // Extract call sites from function body
    if let Some(body) = node.child_by_field_name("body") {
        extract_call_sites(ctx, body, &qn);
    }
}


// ---------------------------------------------------------------------------
// Class extraction (maps to Struct label — closest equivalent)
// ---------------------------------------------------------------------------

fn extract_class(ctx: &mut ExtractCtx, node: Node, scope: &str) {
    let name = node_field_text(ctx.source, node, "name");
    if name.is_empty() {
        return;
    }
    let qn = qual(scope, &name);
    let visibility = python_visibility(&name);

    // source: Spike B' BUG #9 — emit base-class names as a CSV property on
    // the Struct node so the resolver can later look them up in the symbol
    // index and produce resolved Extends_Struct_Struct edges. We collect them
    // here BEFORE calling extract_base_classes (which still emits Extends
    // refs that the indexer drops — kept only for downstream code that
    // greps for them).
    let bases_csv = collect_base_names(ctx.source, node);

    ctx.nodes.push(ExtractedNode {
        label: LABEL_STRUCT.to_string(),
        name: name.clone(),
        qualified_name: qn.clone(),
        start_line: node.start_position().row as u64 + 1,
        end_line: node.end_position().row as u64 + 1,
        visibility,
        properties: vec![("bases".to_string(), bases_csv)],
    });
    ctx.refs.push(ExtractedRef {
        kind: "Defines".to_string(),
        from_qualified_name: scope.to_string(),
        to_qualified_name: qn.clone(),
    });

    // Extract base classes (superclass_list is the "superclasses" field).
    // These Extends refs are still emitted for backward compatibility but
    // the indexer drops them — the property above is the source of truth.
    extract_base_classes(ctx, node, &qn);

    // Recurse into class body for methods and nested classes
    if let Some(body) = node.child_by_field_name("body") {
        extract_top_level(ctx, body, &qn, Some(&qn));
    }
}


/// Returns comma-separated base-class names (`identifier` and `attribute`
/// children of the superclasses field). Attribute access like `typing.NamedTuple`
/// is preserved verbatim — the resolver looks up by the last segment.
fn collect_base_names(source: &str, class_node: Node) -> String {
    let superclasses = match class_node.child_by_field_name("superclasses") {
        Some(s) => s,
        None => return String::new(),
    };
    let mut names = Vec::new();
    let mut cursor = superclasses.walk();
    for child in superclasses.children(&mut cursor) {
        let kind = child.kind();
        if kind == "identifier" || kind == "attribute" {
            let text = node_text(source, child);
            if !text.is_empty() {
                names.push(text);
            }
        }
    }
    names.join(",")
}


fn extract_base_classes(ctx: &mut ExtractCtx, class_node: Node, class_qn: &str) {
    let superclasses = match class_node.child_by_field_name("superclasses") {
        Some(s) => s,
        None => return,
    };
    let mut cursor = superclasses.walk();
    for child in superclasses.children(&mut cursor) {
        let kind = child.kind();
        if kind == "identifier" || kind == "attribute" {
            let base_name = node_text(ctx.source, child);
            if !base_name.is_empty() {
                // Normalize dots to :: for consistent qualified names
                let normalized = base_name.replace('.', "::");
                ctx.refs.push(ExtractedRef {
                    kind: "Extends".to_string(),
                    from_qualified_name: class_qn.to_string(),
                    to_qualified_name: normalized,
                });
            }
        }
    }
}


// ---------------------------------------------------------------------------
// Decorated definition extraction
// ---------------------------------------------------------------------------

fn extract_decorated(
    ctx: &mut ExtractCtx,
    node: Node,
    scope: &str,
    enclosing_class: Option<&str>,
) {
    let mut decorators = Vec::new();
    let mut definition = None;

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "decorator" {
            let text = node_text(ctx.source, child);
            // Strip the leading '@'
            let dec_name = text.trim_start_matches('@').trim().to_string();
            decorators.push(dec_name);
        } else if child.kind() == TS_FUNCTION_DEF {
            definition = Some(child);
        } else if child.kind() == TS_CLASS_DEF {
            // Decorated class — extract the class, ignoring decorators for now
            extract_class(ctx, child, scope);
            return;
        }
    }

    if let Some(func_node) = definition {
        extract_function_or_method(ctx, func_node, scope, enclosing_class, &decorators);
    }
}
