// parser::kotlin — tree-sitter-based Kotlin source parser for code-intelligence graph.
//
// Parses ``.kt`` / ``.kts`` files using tree-sitter-kotlin-ng and emits
// the same ParseResult shape as the sister parsers.
//
// Grammar reference: https://github.com/tree-sitter-grammars/tree-sitter-kotlin
// pinned to v1.1.0 in Cargo.lock as crate `tree-sitter-kotlin-ng`. NOTE: this
// is a ground-up rewrite, NOT the fwcd/tree-sitter-kotlin grammar — its node
// kinds and (few) named fields differ. Verify any node kind / field name used
// here against that tag's src/node-types.json before adding it.

use tree_sitter::{Node, Parser};

use super::{
    node_field_text, node_text, qual, ExtractedNode, ExtractedRef, ParseResult, LABEL_CALL_SITE,
    LABEL_CONSTANT, LABEL_ENUM, LABEL_FUNCTION, LABEL_IMPORT, LABEL_METHOD, LABEL_STRUCT,
    LABEL_TRAIT,
};

// Grammar reference: tree-sitter-grammars/tree-sitter-kotlin (the -ng crate)
// v1.1.0 src/node-types.json. The vocabulary below is verified against that
// file — the earlier fwcd/tree-sitter-kotlin names (import_header, object_body,
// simple_identifier, and the body/callee/receiver/delegation_specifiers FIELDS)
// do not exist in this grammar and silently extracted nothing.
const TS_PACKAGE_HEADER: &str = "package_header";
const TS_IMPORT: &str = "import"; // was "import_header" (fwcd)
const TS_CLASS_DECL: &str = "class_declaration";
const TS_OBJECT_DECL: &str = "object_declaration";
const TS_FUNCTION_DECL: &str = "function_declaration";
const TS_PROPERTY_DECL: &str = "property_declaration";
const TS_CLASS_BODY: &str = "class_body";
const TS_ENUM_CLASS_BODY: &str = "enum_class_body"; // enum members live here
const TS_FUNCTION_BODY: &str = "function_body";
const TS_ENUM_ENTRY: &str = "enum_entry";
const TS_DELEGATION_SPECIFIERS: &str = "delegation_specifiers";
const TS_CALL_EXPR: &str = "call_expression";

pub fn parse_kotlin_file(source: &str, file_path: &str) -> Result<ParseResult, String> {
    let lang: tree_sitter::Language = tree_sitter_kotlin_ng::LANGUAGE.into();
    let mut parser = Parser::new();
    parser
        .set_language(&lang)
        .map_err(|e| format!("failed to set Kotlin language: {e}"))?;
    parser.set_timeout_micros(super::PARSE_TIMEOUT_MICROS);
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| "parse_timeout_or_none: tree-sitter returned None".to_string())?;

    let mut ctx = Ctx {
        source,
        file_path,
        nodes: Vec::new(),
        refs: Vec::new(),
        next_seq: 0,
    };
    extract_children(&mut ctx, tree.root_node(), file_path, None);
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
    // Per-file monotonic counter used as the last segment of call-site
    // and overloaded-method qualified_names. Two overloaded Kotlin funs
    // ``fun toDomain(x: A)`` and ``fun toDomain(x: B)`` would otherwise
    // collide on the LadybugDB primary key; the counter disambiguates.
    next_seq: u64,
}

// Kotlin declaration kinds the grammar exposes as ``class_declaration``
// are distinguished by a leading modifier: ``interface``, ``enum class``,
// ``data class``, etc. We walk the node text once to classify.
fn classify_class(source: &str, node: Node) -> &'static str {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        let kind = child.kind();
        if kind == "interface" {
            return LABEL_TRAIT;
        }
        // An `enum class` always carries an `enum_class_body` child (holding the
        // enum_entry members) — the most reliable enum signal in the -ng grammar.
        // The `enum` keyword token and the `enum` class_modifier are secondary.
        if kind == "enum" || kind == TS_ENUM_CLASS_BODY {
            return LABEL_ENUM;
        }
        // ``modifiers`` node contains ``annotation``, ``class_modifier`` etc.
        if kind == "modifiers" {
            let text = node_text(source, child);
            if text.contains("enum") {
                return LABEL_ENUM;
            }
        }
    }
    LABEL_STRUCT
}

fn visibility_modifier(source: &str, node: Node) -> String {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "modifiers" {
            let t = node_text(source, child);
            for v in ["public", "private", "protected", "internal"] {
                if t.split_whitespace().any(|w| w == v) {
                    return v.to_string();
                }
            }
        }
    }
    // Kotlin default is ``public``.
    "public".to_string()
}

fn extract_children(ctx: &mut Ctx, parent: Node, scope: &str, enclosing_type: Option<&str>) {
    let mut cursor = parent.walk();
    for child in parent.children(&mut cursor) {
        match child.kind() {
            TS_CLASS_DECL | TS_OBJECT_DECL => extract_class_like(ctx, child, scope),
            TS_FUNCTION_DECL => extract_function(ctx, child, scope, enclosing_type),
            TS_PROPERTY_DECL => extract_property(ctx, child, scope),
            TS_IMPORT => extract_import(ctx, child, scope),
            TS_PACKAGE_HEADER => {}
            TS_ENUM_ENTRY => {
                // Enum variant — emit as a constant of the enum type. In the -ng
                // grammar an enum_entry's name is an `identifier` child (there is
                // no `simple_identifier` node type), so use first_identifier.
                let name = first_identifier(ctx.source, child);
                if !name.is_empty() {
                    let qn = qual(scope, &name);
                    ctx.nodes.push(ExtractedNode {
                        label: LABEL_CONSTANT.to_string(),
                        name: name.clone(),
                        qualified_name: qn.clone(),
                        start_line: child.start_position().row as u64 + 1,
                        end_line: child.end_position().row as u64 + 1,
                        visibility: "public".to_string(),
                        properties: vec![("enum_entry".to_string(), "true".to_string())],
                    });
                    ctx.refs.push(ExtractedRef {
                        kind: "Defines".to_string(),
                        from_qualified_name: scope.to_string(),
                        to_qualified_name: qn,
                    });
                }
            }
            TS_CLASS_BODY | TS_ENUM_CLASS_BODY => {
                extract_children(ctx, child, scope, enclosing_type);
            }
            _ => {}
        }
    }
}

fn first_identifier(source: &str, node: Node) -> String {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "simple_identifier" || child.kind() == "identifier" {
            return node_text(source, child);
        }
    }
    String::new()
}

fn extract_class_like(ctx: &mut Ctx, node: Node, scope: &str) {
    let name = node_field_text(ctx.source, node, "name");
    let name = if name.is_empty() {
        first_identifier(ctx.source, node)
    } else {
        name
    };
    if name.is_empty() {
        return;
    }
    let label = classify_class(ctx.source, node);
    let qn = qual(scope, &name);
    ctx.nodes.push(ExtractedNode {
        label: label.to_string(),
        name: name.clone(),
        qualified_name: qn.clone(),
        start_line: node.start_position().row as u64 + 1,
        end_line: node.end_position().row as u64 + 1,
        visibility: visibility_modifier(ctx.source, node),
        properties: Vec::new(),
    });
    ctx.refs.push(ExtractedRef {
        kind: "Defines".to_string(),
        from_qualified_name: scope.to_string(),
        to_qualified_name: qn.clone(),
    });
    // Supertype list — ``: Parent, Interface`` after the class name. In the -ng
    // grammar `delegation_specifiers` is a CHILD node of class_declaration (not
    // a field), containing one `delegation_specifier` per supertype. Walk to it.
    if let Some(supers) = child_of_kind(node, TS_DELEGATION_SPECIFIERS) {
        let text = node_text(ctx.source, supers);
        for piece in text
            .split(',')
            .map(|s| s.trim().trim_end_matches("()"))
            .filter(|s| !s.is_empty())
        {
            ctx.refs.push(ExtractedRef {
                kind: "Extends".to_string(),
                from_qualified_name: qn.clone(),
                to_qualified_name: piece.to_string(),
            });
        }
    }
    // Body — recurse with this type as the enclosing scope. The -ng grammar has
    // no `body` field; the body is a `class_body` child, or an `enum_class_body`
    // child for enums (which holds the enum_entry members).
    let body = child_of_kind(node, TS_CLASS_BODY)
        .or_else(|| child_of_kind(node, TS_ENUM_CLASS_BODY));
    if let Some(body) = body {
        extract_children(ctx, body, &qn, Some(&qn));
    }
}

/// Returns the first direct child of `node` whose kind matches `kind`.
/// The cursor-borrow pattern can't be closed over, so this is a small helper
/// used wherever the grammar exposes structure as child nodes rather than
/// named fields (common in tree-sitter-kotlin-ng).
fn child_of_kind<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    // Bind before returning so the `children` iterator temporary (borrowing
    // `cursor`) drops before `cursor` rather than as the block tail (E0597).
    let found = node.children(&mut cursor).find(|c| c.kind() == kind);
    found
}

fn extract_function(ctx: &mut Ctx, node: Node, scope: &str, enclosing_type: Option<&str>) {
    let name = node_field_text(ctx.source, node, "name");
    let name = if name.is_empty() {
        first_identifier(ctx.source, node)
    } else {
        name
    };
    if name.is_empty() {
        return;
    }
    // Disambiguate overloaded functions by appending a per-file sequence
    // number — the primary key must be unique across all Function and
    // Method nodes in the graph, and LadybugDB rejects duplicates at
    // insert time.
    let seq = {
        ctx.next_seq += 1;
        ctx.next_seq
    };
    let qn = format!("{}::{}#{}", scope, name, seq);
    let label = if enclosing_type.is_some() {
        LABEL_METHOD
    } else {
        LABEL_FUNCTION
    };
    let mut props = Vec::new();
    if let Some(rec) = enclosing_type {
        props.push(("receiver_type".to_string(), rec.to_string()));
    }
    // Kotlin extension functions declare a receiver: ``fun String.foo()``.
    // In the -ng grammar the receiver is a `receiver` node child (there is no
    // `receiver` field), sitting before the function name.
    if let Some(recv) = child_of_kind(node, "receiver") {
        props.push((
            "extension_receiver".to_string(),
            node_text(ctx.source, recv),
        ));
    }
    ctx.nodes.push(ExtractedNode {
        label: label.to_string(),
        name: name.clone(),
        qualified_name: qn.clone(),
        start_line: node.start_position().row as u64 + 1,
        end_line: node.end_position().row as u64 + 1,
        visibility: visibility_modifier(ctx.source, node),
        properties: props,
    });
    let edge_kind = if enclosing_type.is_some() {
        "HasMethod"
    } else {
        "Defines"
    };
    ctx.refs.push(ExtractedRef {
        kind: edge_kind.to_string(),
        from_qualified_name: scope.to_string(),
        to_qualified_name: qn.clone(),
    });
    // The -ng grammar has no `body` field; the body is a `function_body` child
    // (which wraps either a `block` or an expression). Fall back to scanning the
    // whole declaration for expression-bodied functions: ``fun f() = x.bar()``.
    if let Some(body) = child_of_kind(node, TS_FUNCTION_BODY) {
        extract_calls(ctx, body, &qn);
    } else {
        extract_calls(ctx, node, &qn);
    }
}

fn extract_property(ctx: &mut Ctx, node: Node, scope: &str) {
    let name = first_identifier(ctx.source, node);
    if name.is_empty() {
        return;
    }
    let qn = qual(scope, &name);
    ctx.nodes.push(ExtractedNode {
        label: LABEL_CONSTANT.to_string(),
        name: name.clone(),
        qualified_name: qn.clone(),
        start_line: node.start_position().row as u64 + 1,
        end_line: node.end_position().row as u64 + 1,
        visibility: visibility_modifier(ctx.source, node),
        properties: Vec::new(),
    });
    ctx.refs.push(ExtractedRef {
        kind: "Defines".to_string(),
        from_qualified_name: scope.to_string(),
        to_qualified_name: qn,
    });
}

fn extract_import(ctx: &mut Ctx, node: Node, scope: &str) {
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

fn extract_calls(ctx: &mut Ctx, root: Node, caller_qn: &str) {
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
            // Keep only the tail identifier to match the file::name convention
            // used by the rest of the graph.
            let callee_tail = callee
                .rsplit('.')
                .next()
                .unwrap_or("")
                .trim_end_matches('(')
                .to_string();
            if !callee_tail.is_empty() && callee_tail.chars().next().map_or(false, |c| c.is_alphabetic() || c == '_') {
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
                    properties: vec![("callee_name".to_string(), callee_tail.clone())],
                });
                ctx.refs.push(ExtractedRef {
                    kind: "Calls".to_string(),
                    from_qualified_name: caller_qn.to_string(),
                    to_qualified_name: callee_tail,
                });
            }
        }
        let mut cursor = n.walk();
        for c in n.children(&mut cursor) {
            stack.push(c);
        }
    }
}
