// parser::objc::extract::g1 — see ../extract/mod.rs.

use super::super::*; // parent module: Ctx, TS_* consts, kept helpers
use super::*;
use crate::parser::*; // ExtractedNode, ExtractedRef, node_text, qual, LABEL_*, …
use tree_sitter::Node; // sibling extract fns (glob re-export)

pub(super) fn find_name(source: &str, node: Node) -> String {
    // tree-sitter-objc uses ``name`` field or an inline ``identifier``.
    let n = node_field_text(source, node, "name");
    if !n.is_empty() {
        return n;
    }
    let mut cursor = node.walk();
    for c in node.children(&mut cursor) {
        let k = c.kind();
        if k == "identifier" || k == "class_name" || k == "type_identifier" {
            return node_text(source, c);
        }
    }
    String::new()
}

pub(crate) fn extract_top(ctx: &mut Ctx, parent: Node, scope: &str) {
    let mut cursor = parent.walk();
    for child in parent.children(&mut cursor) {
        match child.kind() {
            TS_CLASS_INTERFACE | TS_CLASS_IMPL => {
                // A category is an @interface/@implementation that carries a
                // `category` field (e.g. `@interface NSString (MyCategory)`).
                // In grammar 3.0.2 this is a field, not a distinct node kind.
                let is_category = child.child_by_field_name("category").is_some();
                extract_class(ctx, child, scope, is_category);
            }
            TS_PROTOCOL_DECL => extract_protocol(ctx, child, scope),
            TS_FUNCTION_DEF => extract_function(ctx, child, scope, None),
            TS_C_STRUCT | TS_C_UNION => extract_c_struct(ctx, child, scope),
            TS_C_ENUM => extract_c_enum(ctx, child, scope),
            TS_C_TYPEDEF => extract_c_typedef(ctx, child, scope),
            TS_IMPORT => extract_import(ctx, child, scope),
            TS_MODULE_IMPORT => extract_module_import(ctx, child, scope),
            _ => {
                if child.named_child_count() > 0 {
                    extract_top(ctx, child, scope);
                }
            }
        }
    }
}

pub(super) fn extract_class(ctx: &mut Ctx, node: Node, scope: &str, is_category: bool) {
    let name = find_name(ctx.source, node);
    if name.is_empty() {
        return;
    }
    let qn = qual(scope, &name);
    let mut props = Vec::new();
    if is_category {
        props.push(("is_category".to_string(), "true".to_string()));
        // The category name is the `category` field (e.g. `MyCategory`).
        let cat = node_field_text(ctx.source, node, "category");
        if !cat.is_empty() {
            props.push(("category".to_string(), cat));
        }
    }
    ctx.nodes.push(ExtractedNode {
        label: LABEL_STRUCT.to_string(),
        name: name.clone(),
        qualified_name: qn.clone(),
        start_line: node.start_position().row as u64 + 1,
        end_line: node.end_position().row as u64 + 1,
        visibility: "public".to_string(),
        properties: props,
    });
    ctx.refs.push(ExtractedRef {
        kind: "Defines".to_string(),
        from_qualified_name: scope.to_string(),
        to_qualified_name: qn.clone(),
    });
    // Superclass — the `superclass` field (e.g. `@interface Foo : NSObject`).
    // Emit an Extends ref so resolve_extends can link it. source: grammar 3.0.2
    // class_interface.fields = {category, superclass}.
    let superclass = node_field_text(ctx.source, node, "superclass");
    if !superclass.is_empty() {
        ctx.refs.push(ExtractedRef {
            kind: "Extends".to_string(),
            from_qualified_name: qn.clone(),
            to_qualified_name: superclass,
        });
    }
    // Walk all children for method declarations / definitions.
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            TS_METHOD_DECL | TS_METHOD_DEF => {
                extract_method(ctx, child, &qn);
            }
            _ => {
                if child.named_child_count() > 0 {
                    // Dive into compound groupings (``class_body``, etc.).
                    let mut inner = child.walk();
                    for gc in child.children(&mut inner) {
                        if gc.kind() == TS_METHOD_DECL || gc.kind() == TS_METHOD_DEF {
                            extract_method(ctx, gc, &qn);
                        }
                    }
                }
            }
        }
    }
}

pub(super) fn extract_protocol(ctx: &mut Ctx, node: Node, scope: &str) {
    let name = find_name(ctx.source, node);
    if name.is_empty() {
        return;
    }
    let qn = qual(scope, &name);
    ctx.nodes.push(ExtractedNode {
        label: LABEL_TRAIT.to_string(),
        name: name.clone(),
        qualified_name: qn.clone(),
        start_line: node.start_position().row as u64 + 1,
        end_line: node.end_position().row as u64 + 1,
        visibility: "public".to_string(),
        properties: Vec::new(),
    });
    ctx.refs.push(ExtractedRef {
        kind: "Defines".to_string(),
        from_qualified_name: scope.to_string(),
        to_qualified_name: qn.clone(),
    });
}

pub(super) fn method_selector(source: &str, node: Node) -> String {
    // Reconstruct the ObjC selector from the grammar-3.0.2 shape:
    //   - keyword selector: one or more `keyword_declarator` children, each
    //     contributing `<identifier>:` (the label of one argument). The keyword
    //     is the FIRST `identifier` child of the keyword_declarator (there is no
    //     `keyword` field in this grammar).
    //   - unary selector (no args): a single bare `identifier` child on the
    //     method_declaration (e.g. `- (void)start;` → `start`).
    // source: tree-sitter-objc v3.0.2 (method_declaration / keyword_declarator).
    let mut parts: Vec<String> = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "keyword_declarator" => {
                let kw = first_identifier(source, child);
                if !kw.is_empty() {
                    parts.push(format!("{kw}:"));
                }
            }
            // Unary selector — the method name with no keyword args.
            "identifier" if parts.is_empty() => {
                parts.push(node_text(source, child));
            }
            _ => {}
        }
    }
    parts.join("")
}

/// Reconstructs a message-send selector from a `message_expression`.
///
/// Grammar 3.0.2 exposes the selector keywords as the `method` field (which is
/// `multiple` — one `identifier` per keyword) and the argument expressions as
/// non-field `expression` children (the `receiver` is a separate field). A
/// message with arguments is a keyword send → `doThing:andY:`; a message with no
/// arguments is a unary send → `start`. source: tree-sitter-objc v3.0.2.
pub(super) fn message_selector(source: &str, node: Node) -> String {
    let mut keywords: Vec<String> = Vec::new();
    let mut arg_count: usize = 0;
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            let field = cursor.field_name();
            let child = cursor.node();
            match field {
                Some("method") => keywords.push(node_text(source, child)),
                Some("receiver") => {}
                _ => {
                    // Arguments are the non-field NAMED children (concrete
                    // subtypes of the `expression` supertype — `call_expression`,
                    // `identifier`, `number_literal`, …). We cannot test for the
                    // literal kind "expression": tree-sitter supertypes never
                    // appear as runtime node kinds. Unnamed tokens (`[` `]` `:`)
                    // are skipped by is_named(). A keyword selector always has
                    // >=1 argument; a unary selector has none.
                    if field.is_none() && child.is_named() {
                        arg_count += 1;
                    }
                }
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    if keywords.is_empty() {
        return String::new();
    }
    if arg_count > 0 {
        keywords.iter().map(|k| format!("{k}:")).collect::<String>()
    } else {
        keywords.join("")
    }
}

pub(super) fn extract_method(ctx: &mut Ctx, node: Node, scope: &str) {
    let sel = method_selector(ctx.source, node);
    if sel.is_empty() {
        return;
    }
    let seq = {
        ctx.next_seq += 1;
        ctx.next_seq
    };
    let qn = format!("{}::{}#{}", scope, sel, seq);
    ctx.nodes.push(ExtractedNode {
        label: LABEL_METHOD.to_string(),
        name: sel.clone(),
        qualified_name: qn.clone(),
        start_line: node.start_position().row as u64 + 1,
        end_line: node.end_position().row as u64 + 1,
        visibility: "public".to_string(),
        properties: vec![("receiver_type".to_string(), scope.to_string())],
    });
    ctx.refs.push(ExtractedRef {
        kind: "HasMethod".to_string(),
        from_qualified_name: scope.to_string(),
        to_qualified_name: qn.clone(),
    });
    if let Some(body) = node
        .child_by_field_name("body")
        .or_else(|| node_child_of_kind(node, "compound_statement"))
    {
        extract_calls(ctx, body, &qn);
    }
}
