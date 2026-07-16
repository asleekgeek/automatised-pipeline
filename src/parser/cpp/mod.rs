// parser::cpp — tree-sitter-based C++ source parser.
//
// Builds on the C grammar's node shapes but adds class, namespace,
// template declarations, and member functions. Uses the tree-sitter-cpp
// grammar directly (not the C grammar).
//
// Grammar reference: https://github.com/tree-sitter/tree-sitter-cpp

use tree_sitter::Parser;

use super::{ExtractedNode, ExtractedRef, ParseResult};

mod extract;

pub(crate) const TS_NAMESPACE: &str = "namespace_definition";
pub(crate) const TS_CLASS: &str = "class_specifier";
pub(crate) const TS_STRUCT: &str = "struct_specifier";
pub(crate) const TS_UNION: &str = "union_specifier";
pub(crate) const TS_ENUM: &str = "enum_specifier";
pub(crate) const TS_TEMPLATE: &str = "template_declaration";
pub(crate) const TS_FUNCTION_DEF: &str = "function_definition";
pub(crate) const TS_FIELD_DECL: &str = "field_declaration";
pub(crate) const TS_TYPEDEF: &str = "type_definition";
pub(crate) const TS_INCLUDE: &str = "preproc_include";
pub(crate) const TS_USING: &str = "using_declaration";
pub(crate) const TS_CALL: &str = "call_expression";

pub fn parse_cpp_file(source: &str, file_path: &str) -> Result<ParseResult, String> {
    let lang: tree_sitter::Language = tree_sitter_cpp::LANGUAGE.into();
    let mut parser = Parser::new();
    parser
        .set_language(&lang)
        .map_err(|e| format!("failed to set C++ language: {e}"))?;
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
