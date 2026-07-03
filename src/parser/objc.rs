// parser::objc — tree-sitter-based Objective-C source parser.
//
// Handles ``.m`` (Objective-C) and ``.mm`` (Objective-C++). The grammar
// covers ``@interface`` / ``@implementation`` / ``@protocol`` / method
// declarations and definitions, as well as C constructs embedded inside.
//
// Grammar reference: https://github.com/tree-sitter-grammars/tree-sitter-objc
// pinned to v3.0.2 in Cargo.lock as crate `tree-sitter-objc`. NOTE: this is the
// tree-sitter-grammars grammar, NOT jiyee/tree-sitter-objc — its node kinds
// differ. In particular: categories are a `category` FIELD on class_interface/
// class_implementation (there are no category_interface / category_implementation
// node kinds), and selectors are reconstructed from `keyword_declarator` children
// (there are no keyword_selector / unary_selector / selector node kinds). Verify
// any node kind / field name used here against that tag's src/node-types.json.

use tree_sitter::{Node, Parser};

use super::{
    node_field_text, node_text, qual, ExtractedNode, ExtractedRef, ParseResult, LABEL_CALL_SITE,
    LABEL_FUNCTION, LABEL_IMPORT, LABEL_METHOD, LABEL_STRUCT, LABEL_TRAIT,
};

const TS_CLASS_INTERFACE: &str = "class_interface";
const TS_CLASS_IMPL: &str = "class_implementation";
const TS_PROTOCOL_DECL: &str = "protocol_declaration";
const TS_METHOD_DECL: &str = "method_declaration";
const TS_METHOD_DEF: &str = "method_definition";
const TS_FUNCTION_DEF: &str = "function_definition";
const TS_IMPORT: &str = "preproc_include";
const TS_MODULE_IMPORT: &str = "module_import";
const TS_CALL: &str = "call_expression";
const TS_MSG_EXPR: &str = "message_expression";

pub fn parse_objc_file(source: &str, file_path: &str) -> Result<ParseResult, String> {
    let lang: tree_sitter::Language = tree_sitter_objc::LANGUAGE.into();
    let mut parser = Parser::new();
    parser
        .set_language(&lang)
        .map_err(|e| format!("failed to set Objective-C language: {e}"))?;
    let tree = super::parse_with_timeout(&mut parser, source)?;

    let mut ctx = Ctx {
        source,
        file_path,
        nodes: Vec::new(),
        refs: Vec::new(),
        next_seq: 0,
    };
    extract_top(&mut ctx, tree.root_node(), file_path);
    Ok(ParseResult {
        nodes: ctx.nodes,
        refs: ctx.refs,
        parse_errors: super::count_parse_errors(tree.root_node()),
    })
}

struct Ctx<'a> {
    source: &'a str,
    #[allow(dead_code)]
    file_path: &'a str,
    nodes: Vec<ExtractedNode>,
    refs: Vec<ExtractedRef>,
    next_seq: u64,
}

fn find_name(source: &str, node: Node) -> String {
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

fn extract_top(ctx: &mut Ctx, parent: Node, scope: &str) {
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

fn extract_class(ctx: &mut Ctx, node: Node, scope: &str, is_category: bool) {
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

fn extract_protocol(ctx: &mut Ctx, node: Node, scope: &str) {
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

fn method_selector(source: &str, node: Node) -> String {
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
            "identifier" => {
                // Unary selector — the method name with no keyword args.
                if parts.is_empty() {
                    parts.push(node_text(source, child));
                }
            }
            _ => {}
        }
    }
    parts.join("")
}

/// Returns the text of the first `identifier` child of `node`, or empty.
fn first_identifier(source: &str, node: Node) -> String {
    let mut cursor = node.walk();
    // Bind before returning so the `children` iterator temporary (which borrows
    // `cursor`) is dropped at the end of this statement, before `cursor` itself —
    // otherwise it outlives `cursor` as the block's tail expression (E0597).
    let text = node
        .children(&mut cursor)
        .find(|c| c.kind() == "identifier")
        .map(|c| node_text(source, c))
        .unwrap_or_default();
    text
}

/// Reconstructs a message-send selector from a `message_expression`.
///
/// Grammar 3.0.2 exposes the selector keywords as the `method` field (which is
/// `multiple` — one `identifier` per keyword) and the argument expressions as
/// non-field `expression` children (the `receiver` is a separate field). A
/// message with arguments is a keyword send → `doThing:andY:`; a message with no
/// arguments is a unary send → `start`. source: tree-sitter-objc v3.0.2.
fn message_selector(source: &str, node: Node) -> String {
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

fn extract_method(ctx: &mut Ctx, node: Node, scope: &str) {
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

fn extract_function(ctx: &mut Ctx, node: Node, scope: &str, enclosing: Option<&str>) {
    let decl = node.child_by_field_name("declarator");
    let name = decl
        .map(|d| node_field_text(ctx.source, d, "declarator"))
        .unwrap_or_default();
    let name = if name.is_empty() {
        find_name(ctx.source, node)
    } else {
        name
    };
    if name.is_empty() {
        return;
    }
    let seq = {
        ctx.next_seq += 1;
        ctx.next_seq
    };
    let qn = format!("{}::{}#{}", scope, name, seq);
    let label = if enclosing.is_some() {
        LABEL_METHOD
    } else {
        LABEL_FUNCTION
    };
    ctx.nodes.push(ExtractedNode {
        label: label.to_string(),
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
    if let Some(body) = node
        .child_by_field_name("body")
        .or_else(|| node_child_of_kind(node, "compound_statement"))
    {
        extract_calls(ctx, body, &qn);
    }
}

fn node_child_of_kind<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    for c in node.children(&mut cursor) {
        if c.kind() == kind {
            return Some(c);
        }
    }
    None
}

fn extract_import(ctx: &mut Ctx, node: Node, scope: &str) {
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

fn extract_module_import(ctx: &mut Ctx, node: Node, scope: &str) {
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

fn extract_calls(ctx: &mut Ctx, root: Node, caller_qn: &str) {
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

fn emit_call(ctx: &mut Ctx, n: Node, caller_qn: &str, callee: &str) {
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
