// parser::rust::extract::g1 — see ../extract/mod.rs.

use tree_sitter::Node;
use crate::parser::*;      // ExtractedNode, ExtractedRef, node_text, qual, LABEL_*, …
use super::super::*;       // parent module: Ctx, TS_* consts, kept helpers
use super::*;              // sibling extract fns (glob re-export)


// ---------------------------------------------------------------------------
// Top-level extraction
// ---------------------------------------------------------------------------

fn extract_top_level(ctx: &mut ExtractCtx, parent: Node, scope: &str) {
    let mut cursor = parent.walk();
    // Track derive-trait names accumulated from preceding #[derive(...)]
    // attribute_item siblings. Reset after each non-attribute item.
    // source: stages/stage-3b-v2.md §5 Layer 4 (derive → Implements).
    let mut pending_derives: Vec<String> = Vec::new();
    for child in parent.children(&mut cursor) {
        match child.kind() {
            TS_ATTRIBUTE_ITEM => {
                collect_derives_from_attribute(ctx.source, child, &mut pending_derives);
            }
            TS_FUNCTION_ITEM => {
                extract_function(ctx, child, scope);
                pending_derives.clear();
            }
            TS_STRUCT_ITEM => {
                extract_struct(ctx, child, scope, &pending_derives);
                emit_derive_implements(ctx, child, scope, &pending_derives);
                pending_derives.clear();
            }
            TS_ENUM_ITEM => {
                extract_enum(ctx, child, scope, &pending_derives);
                emit_derive_implements(ctx, child, scope, &pending_derives);
                pending_derives.clear();
            }
            TS_TRAIT_ITEM => {
                extract_trait(ctx, child, scope);
                pending_derives.clear();
            }
            TS_IMPL_ITEM => {
                extract_impl(ctx, child);
                pending_derives.clear();
            }
            TS_CONST_ITEM | TS_STATIC_ITEM => {
                // A `static` is a constant-like symbol: same name/type fields as
                // const_item, so extract_const handles both → Constant node.
                extract_const(ctx, child, scope);
                pending_derives.clear();
            }
            TS_UNION_ITEM => {
                // A union is struct-like: name + field_declaration_list body.
                // extract_struct reads the name field and its fields identically.
                extract_struct(ctx, child, scope, &pending_derives);
                emit_derive_implements(ctx, child, scope, &pending_derives);
                pending_derives.clear();
            }
            TS_MACRO_DEFINITION => {
                extract_macro_definition(ctx, child, scope);
                pending_derives.clear();
            }
            TS_EXTERN_CRATE => {
                extract_extern_crate(ctx, child, scope);
                pending_derives.clear();
            }
            TS_TYPE_ITEM => {
                extract_type_alias(ctx, child, scope);
                pending_derives.clear();
            }
            TS_USE_DECL => {
                extract_use(ctx, child, scope);
                pending_derives.clear();
            }
            TS_MOD_ITEM => {
                extract_mod(ctx, child, scope);
                pending_derives.clear();
            }
            _ => {}
        }
    }
}


/// Parse an `#[derive(...)]` attribute_item and append the trait names to
/// `out`. Non-derive attributes are ignored. source: Rust Reference §9
/// https://doc.rust-lang.org/reference/attributes/derive.html.
fn collect_derives_from_attribute(source: &str, node: Node, out: &mut Vec<String>) {
    let text = node_text(source, node);
    // Attribute form: `#[derive(A, B, C)]` — strip prefix/suffix and split.
    let trimmed = text.trim();
    let inner = match trimmed
        .strip_prefix("#[")
        .and_then(|s| s.strip_suffix(']'))
    {
        Some(s) => s.trim(),
        None => return,
    };
    let payload = match inner
        .strip_prefix("derive(")
        .and_then(|s| s.strip_suffix(')'))
    {
        Some(s) => s,
        None => return,
    };
    for tok in payload.split(',') {
        let name = tok.trim();
        if !name.is_empty() {
            out.push(name.to_string());
        }
    }
}


/// Builds the node `implements` property (a CSV of derived trait names) so
/// resolve_implements can resolve each to a local Trait or a stdlib trait and
/// emit the Implements edge. Empty when nothing is derived.
/// source: implements fix — mirrors the `bases`/resolve_extends mechanism.
fn implements_props(derives: &[String]) -> Vec<(String, String)> {
    if derives.is_empty() {
        Vec::new()
    } else {
        vec![("implements".to_string(), derives.join(","))]
    }
}


/// Emit synthetic `Implements` refs with kind `"DeriveImplements"` so the
/// resolver's Layer 4 pass maps each derived trait through the macro table.
fn emit_derive_implements(
    ctx: &mut ExtractCtx,
    item: Node,
    scope: &str,
    derives: &[String],
) {
    if derives.is_empty() {
        return;
    }
    let name = node_field_text(ctx.source, item, "name");
    if name.is_empty() {
        return;
    }
    let from_qn = qual(scope, &name);
    for trait_name in derives {
        ctx.refs.push(ExtractedRef {
            kind: "DeriveImplements".to_string(),
            from_qualified_name: from_qn.clone(),
            to_qualified_name: trait_name.clone(),
        });
    }
}


// ---------------------------------------------------------------------------
// Function extraction
// ---------------------------------------------------------------------------

fn extract_function(ctx: &mut ExtractCtx, node: Node, scope: &str) {
    let name = node_field_text(ctx.source, node, "name");
    if name.is_empty() {
        return;
    }
    let qn = qual(scope, &name);
    let vis = extract_visibility(ctx.source, node);
    let is_async = has_async_modifier(node);
    ctx.nodes.push(ExtractedNode {
        label: LABEL_FUNCTION.to_string(),
        name: name.clone(),
        qualified_name: qn.clone(),
        start_line: node.start_position().row as u64 + 1,
        end_line: node.end_position().row as u64 + 1,
        visibility: vis,
        properties: vec![("is_async".to_string(), is_async.to_string())],
    });
    ctx.refs.push(ExtractedRef {
        kind: "Defines".to_string(),
        from_qualified_name: scope.to_string(),
        to_qualified_name: qn.clone(),
    });
    if let Some(body) = node.child_by_field_name("body") {
        extract_call_sites(ctx, body, &qn);
    }
}


// ---------------------------------------------------------------------------
// Struct extraction
// ---------------------------------------------------------------------------

fn extract_struct(ctx: &mut ExtractCtx, node: Node, scope: &str, derives: &[String]) {
    let name = node_field_text(ctx.source, node, "name");
    if name.is_empty() {
        return;
    }
    let qn = qual(scope, &name);
    let vis = extract_visibility(ctx.source, node);
    ctx.nodes.push(ExtractedNode {
        label: LABEL_STRUCT.to_string(),
        name: name.clone(),
        qualified_name: qn.clone(),
        start_line: node.start_position().row as u64 + 1,
        end_line: node.end_position().row as u64 + 1,
        visibility: vis,
        properties: implements_props(derives),
    });
    ctx.refs.push(ExtractedRef {
        kind: "Defines".to_string(),
        from_qualified_name: scope.to_string(),
        to_qualified_name: qn.clone(),
    });
    extract_fields(ctx, node, &qn, TS_FIELD_DECL_LIST);
}


// ---------------------------------------------------------------------------
// Enum extraction
// ---------------------------------------------------------------------------

fn extract_enum(ctx: &mut ExtractCtx, node: Node, scope: &str, derives: &[String]) {
    let name = node_field_text(ctx.source, node, "name");
    if name.is_empty() {
        return;
    }
    let qn = qual(scope, &name);
    let vis = extract_visibility(ctx.source, node);
    ctx.nodes.push(ExtractedNode {
        label: LABEL_ENUM.to_string(),
        name: name.clone(),
        qualified_name: qn.clone(),
        start_line: node.start_position().row as u64 + 1,
        end_line: node.end_position().row as u64 + 1,
        visibility: vis,
        properties: implements_props(derives),
    });
    ctx.refs.push(ExtractedRef {
        kind: "Defines".to_string(),
        from_qualified_name: scope.to_string(),
        to_qualified_name: qn.clone(),
    });
    extract_variants(ctx, node, &qn);
}
