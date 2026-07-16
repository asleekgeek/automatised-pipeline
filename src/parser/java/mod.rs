// parser::java — tree-sitter-based Java source parser for code-intelligence graph.
//
// Parses a single `.java` file and extracts typed symbols matching the
// graph_store schema. Shares the ParseResult/ExtractedNode/ExtractedRef
// contract with parser::rust, parser::python, and parser::typescript.
//
// Grammar reference: https://github.com/tree-sitter/tree-sitter-java

use tree_sitter::Parser;

use super::{ExtractedNode, ExtractedRef, ParseResult};

mod extract;

// Tree-sitter node type constants — from
// https://github.com/tree-sitter/tree-sitter-java/blob/master/src/node-types.json
pub(crate) const TS_CLASS: &str = "class_declaration";
pub(crate) const TS_INTERFACE: &str = "interface_declaration";
pub(crate) const TS_ENUM: &str = "enum_declaration";
pub(crate) const TS_RECORD: &str = "record_declaration";
pub(crate) const TS_ANNOTATION: &str = "annotation_type_declaration";
pub(crate) const TS_METHOD: &str = "method_declaration";
pub(crate) const TS_CONSTRUCTOR: &str = "constructor_declaration";
pub(crate) const TS_FIELD: &str = "field_declaration";
// source: tree-sitter-java v0.23.5 — enum constants (`RED, GREEN`) are
// enum_constant nodes inside enum_body; previously dropped.
pub(crate) const TS_ENUM_CONSTANT: &str = "enum_constant";
pub(crate) const TS_IMPORT: &str = "import_declaration";
pub(crate) const TS_PACKAGE: &str = "package_declaration";
pub(crate) const TS_CALL: &str = "method_invocation";
pub(crate) const TS_OBJECT_CREATION: &str = "object_creation_expression";

pub fn parse_java_file(source: &str, file_path: &str) -> Result<ParseResult, String> {
    let lang: tree_sitter::Language = tree_sitter_java::LANGUAGE.into();
    let mut parser = Parser::new();
    parser
        .set_language(&lang)
        .map_err(|e| format!("failed to set Java language: {e}"))?;
    let tree = super::parse_with_timeout(&mut parser, source)?;

    let mut ctx = Ctx {
        source,
        file_path,
        nodes: Vec::new(),
        refs: Vec::new(),
        package: String::new(),
        next_seq: 0,
    };
    // Java files carry a ``package X.Y;`` at the top that seeds the scope
    // for every qualified name below; we capture it then prefix-qualify.
    if let Some(pkg) = extract::find_package(tree.root_node(), source) {
        ctx.package = pkg;
    }
    extract::extract_children(&mut ctx, tree.root_node(), file_path, None);
    Ok(ParseResult {
        nodes: ctx.nodes,
        refs: ctx.refs,
        parse_errors: super::count_parse_errors(tree.root_node()),
    })
}

pub(crate) struct Ctx<'a> {
    pub(crate) source: &'a str,
    #[allow(dead_code)]
    pub(crate) file_path: &'a str,
    pub(crate) nodes: Vec<ExtractedNode>,
    pub(crate) refs: Vec<ExtractedRef>,
    pub(crate) package: String,
    // See the note in parser::kotlin — a per-file sequence disambiguates
    // overloaded methods and multiple call sites at the same position
    // so the graph store's primary-key uniqueness holds.
    pub(crate) next_seq: u64,
}
