// parser::swift — tree-sitter-based Swift source parser for code-intelligence graph.
//
// Parses ``.swift`` files and extracts class/struct/enum/protocol/extension
// declarations plus functions, methods, properties, imports, and calls.
//
// Grammar reference: https://github.com/alex-pinkus/tree-sitter-swift

use tree_sitter::{Node, Parser};

use super::{
    node_field_text, node_text, qual, ExtractedNode, ExtractedRef, ParseResult, LABEL_CALL_SITE,
    LABEL_CONSTANT, LABEL_ENUM, LABEL_FUNCTION, LABEL_IMPORT, LABEL_METHOD, LABEL_STRUCT,
    LABEL_TRAIT,
};

mod extract;


pub(crate) const TS_CLASS_DECL: &str = "class_declaration";
pub(crate) const TS_PROTOCOL_DECL: &str = "protocol_declaration";
pub(crate) const TS_FUNCTION_DECL: &str = "function_declaration";
pub(crate) const TS_PROPERTY_DECL: &str = "property_declaration";
pub(crate) const TS_IMPORT_DECL: &str = "import_declaration";
pub(crate) const TS_TYPEALIAS_DECL: &str = "typealias_declaration";
// source: alex-pinkus/tree-sitter-swift v0.7.3 — member declaration kinds that
// previously fell through the `_` arm and were dropped.
pub(crate) const TS_INIT_DECL: &str = "init_declaration";
pub(crate) const TS_DEINIT_DECL: &str = "deinit_declaration";
pub(crate) const TS_SUBSCRIPT_DECL: &str = "subscript_declaration";
pub(crate) const TS_ENUM_ENTRY: &str = "enum_entry";
pub(crate) const TS_CALL_EXPR: &str = "call_expression";


pub fn parse_swift_file(source: &str, file_path: &str) -> Result<ParseResult, String> {
    let lang: tree_sitter::Language = tree_sitter_swift::LANGUAGE.into();
    let mut parser = Parser::new();
    parser
        .set_language(&lang)
        .map_err(|e| format!("failed to set Swift language: {e}"))?;
    let tree = super::parse_with_timeout(&mut parser, source)?;

    let mut ctx = Ctx {
        source,
        file_path,
        nodes: Vec::new(),
        refs: Vec::new(),
        next_seq: 0,
    };
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
    pub(crate) next_seq: u64,
}
