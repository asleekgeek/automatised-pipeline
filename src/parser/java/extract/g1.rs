// parser::java::extract::g1 — see ../extract/mod.rs.

use tree_sitter::Node;
use crate::parser::*;      // ExtractedNode, ExtractedRef, node_text, qual, LABEL_*, …
use super::super::*;       // parent module: Ctx, TS_* consts, kept helpers
use super::*;              // sibling extract fns (glob re-export)


pub(crate) fn find_package(root: Node, source: &str) -> Option<String> {
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if child.kind() == TS_PACKAGE {
            let mut inner = child.walk();
            for n in child.children(&mut inner) {
                if n.kind() == "scoped_identifier" || n.kind() == "identifier" {
                    return Some(node_text(source, n).trim().to_string());
                }
            }
        }
    }
    None
}


pub(super) fn visibility_from_modifiers(source: &str, node: Node) -> String {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "modifiers" {
            let t = node_text(source, child);
            if t.contains("public") {
                return "public".to_string();
            }
            if t.contains("private") {
                return "private".to_string();
            }
            if t.contains("protected") {
                return "protected".to_string();
            }
        }
    }
    // Java default is package-private.
    "package".to_string()
}


pub(crate) fn extract_children(ctx: &mut Ctx, parent: Node, scope: &str, enclosing_type: Option<&str>) {
    let mut cursor = parent.walk();
    for child in parent.children(&mut cursor) {
        match child.kind() {
            TS_CLASS | TS_RECORD => extract_class_like(ctx, child, scope, LABEL_STRUCT),
            TS_INTERFACE | TS_ANNOTATION => {
                extract_class_like(ctx, child, scope, LABEL_TRAIT)
            }
            TS_ENUM => extract_class_like(ctx, child, scope, LABEL_ENUM),
            TS_METHOD | TS_CONSTRUCTOR => {
                extract_method(ctx, child, scope, enclosing_type)
            }
            TS_FIELD => extract_field(ctx, child, scope),
            TS_ENUM_CONSTANT => extract_enum_constant(ctx, child, scope),
            TS_IMPORT => extract_import(ctx, child, scope),
            // ``class_body`` / ``interface_body`` / ``enum_body`` wrap members.
            // ``enum_body_declarations`` (methods/fields after the enum
            // constants) does NOT end in ``_body`` — recurse it explicitly so
            // those members aren't dropped.
            _ if child.kind().ends_with("_body")
                || child.kind() == "enum_body_declarations" =>
            {
                extract_children(ctx, child, scope, enclosing_type);
            }
            _ => {}
        }
    }
}


/// Emits a Java enum constant as a Variant of the enclosing enum, with a
/// HasVariant edge (mirrors the Rust parser's enum-variant handling). `scope` is
/// the enum's qualified name (enum_constant nodes are reached while recursing
/// the enum_body). source: tree-sitter-java v0.23.5 enum_constant.name.
pub(super) fn extract_enum_constant(ctx: &mut Ctx, node: Node, scope: &str) {
    let name = node_field_text(ctx.source, node, "name");
    if name.is_empty() {
        return;
    }
    let qn = qual(scope, &name);
    ctx.nodes.push(ExtractedNode {
        label: crate::parser::LABEL_VARIANT.to_string(),
        name: name.clone(),
        qualified_name: qn.clone(),
        start_line: node.start_position().row as u64 + 1,
        end_line: node.end_position().row as u64 + 1,
        visibility: "public".to_string(),
        properties: Vec::new(),
    });
    ctx.refs.push(ExtractedRef {
        kind: "HasVariant".to_string(),
        from_qualified_name: scope.to_string(),
        to_qualified_name: qn,
    });
}


pub(super) fn extract_class_like(ctx: &mut Ctx, node: Node, scope: &str, label: &str) {
    let name = node_field_text(ctx.source, node, "name");
    if name.is_empty() {
        return;
    }
    let qn = qual(scope, &name);
    let visibility = visibility_from_modifiers(ctx.source, node);

    // Collect inheritance up front so it lands on the node as `bases` /
    // `implements` columns — consumed by resolver::resolve_extends and
    // resolve_implements respectively (name→QN resolution happens there, after
    // all nodes are indexed). The same names are still emitted below as
    // Extends/Implements refs for the parser-output integration tests.
    // source: implements fix (Java) — mirror parser/rust.rs column population;
    // Java previously emitted only refs, which the indexer drops, so Java
    // extends/implements produced no graph edges at all.
    let superclass = extract_superclass(ctx.source, node);
    let interfaces = extract_interfaces(ctx.source, node);

    let mut properties = Vec::new();
    if !superclass.is_empty() {
        properties.push(("bases".to_string(), superclass.clone()));
    }
    if !interfaces.is_empty() {
        properties.push(("implements".to_string(), interfaces.join(",")));
    }

    ctx.nodes.push(ExtractedNode {
        label: label.to_string(),
        name: name.clone(),
        qualified_name: qn.clone(),
        start_line: node.start_position().row as u64 + 1,
        end_line: node.end_position().row as u64 + 1,
        visibility,
        properties,
    });
    ctx.refs.push(ExtractedRef {
        kind: "Defines".to_string(),
        from_qualified_name: scope.to_string(),
        to_qualified_name: qn.clone(),
    });
    if !superclass.is_empty() {
        ctx.refs.push(ExtractedRef {
            kind: "Extends".to_string(),
            from_qualified_name: qn.clone(),
            to_qualified_name: superclass,
        });
    }
    for iface in interfaces {
        ctx.refs.push(ExtractedRef {
            kind: "Implements".to_string(),
            from_qualified_name: qn.clone(),
            to_qualified_name: iface,
        });
    }
    if let Some(body) = node.child_by_field_name("body") {
        extract_children(ctx, body, &qn, Some(&qn));
    }
}


/// The `extends` superclass name for a class (single; empty if none).
/// source: tree-sitter-java — the `superclass` field text is `extends Foo`.
pub(super) fn extract_superclass(source: &str, node: Node) -> String {
    match node.child_by_field_name("superclass") {
        Some(supers) => node_text(source, supers)
            .trim_start_matches("extends")
            .trim()
            .to_string(),
        None => String::new(),
    }
}


/// The implemented-interface names for a class (empty if none).
/// source: tree-sitter-java — the `interfaces` field is a `super_interfaces`
/// node holding the `implements` keyword plus a `type_list`; the type names
/// live one level down inside that `type_list`, so we descend into it.
pub(super) fn extract_interfaces(source: &str, node: Node) -> Vec<String> {
    let ifaces = match node.child_by_field_name("interfaces") {
        Some(i) => i,
        None => return Vec::new(),
    };
    let mut names = Vec::new();
    collect_type_names(source, ifaces, &mut names);
    names
}


/// Collects type_identifier / scoped_type_identifier names directly under
/// `node`, descending one level through a `type_list` wrapper (the shape
/// tree-sitter-java uses for `implements`/`extends`-interfaces clauses).
pub(super) fn collect_type_names(source: &str, node: Node, out: &mut Vec<String>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "type_identifier" | "scoped_type_identifier" => {
                let nm = node_text(source, child);
                if !nm.is_empty() {
                    out.push(nm);
                }
            }
            "type_list" => collect_type_names(source, child, out),
            _ => {}
        }
    }
}


pub(super) fn extract_method(ctx: &mut Ctx, node: Node, scope: &str, enclosing_type: Option<&str>) {
    let name = node_field_text(ctx.source, node, "name");
    if name.is_empty() {
        return;
    }
    let seq = {
        ctx.next_seq += 1;
        ctx.next_seq
    };
    let qn = format!("{}::{}#{}", scope, name, seq);
    let visibility = visibility_from_modifiers(ctx.source, node);
    let label = if enclosing_type.is_some() {
        LABEL_METHOD
    } else {
        LABEL_FUNCTION
    };
    let mut props = Vec::new();
    if let Some(rec) = enclosing_type {
        props.push(("receiver_type".to_string(), rec.to_string()));
    }
    ctx.nodes.push(ExtractedNode {
        label: label.to_string(),
        name: name.clone(),
        qualified_name: qn.clone(),
        start_line: node.start_position().row as u64 + 1,
        end_line: node.end_position().row as u64 + 1,
        visibility,
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
    if let Some(body) = node.child_by_field_name("body") {
        extract_calls(ctx, body, &qn);
    }
}
