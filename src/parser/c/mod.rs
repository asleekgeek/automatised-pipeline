// parser::c — tree-sitter-based C source parser for code-intelligence graph.
//
// Handles ``.c`` and ``.h``. C++ uses a separate parser (``parser::cpp``).
// Focuses on the constructs the graph_store schema can represent:
// struct, enum, typedef, function, #include, call.
//
// Grammar reference: https://github.com/tree-sitter/tree-sitter-c

use tree_sitter::Parser;

use super::{
    ExtractedNode, ExtractedRef, ParseResult,
};

mod extract;


pub(crate) const TS_STRUCT: &str = "struct_specifier";
pub(crate) const TS_UNION: &str = "union_specifier";
pub(crate) const TS_ENUM: &str = "enum_specifier";
pub(crate) const TS_TYPEDEF: &str = "type_definition";
pub(crate) const TS_FUNCTION_DEF: &str = "function_definition";
pub(crate) const TS_FUNCTION_DECL: &str = "declaration";
pub(crate) const TS_INCLUDE: &str = "preproc_include";
pub(crate) const TS_CALL: &str = "call_expression";


pub fn parse_c_file(source: &str, file_path: &str) -> Result<ParseResult, String> {
    let lang: tree_sitter::Language = tree_sitter_c::LANGUAGE.into();
    let mut parser = Parser::new();
    parser
        .set_language(&lang)
        .map_err(|e| format!("failed to set C language: {e}"))?;
    let tree = super::parse_with_timeout(&mut parser, source)?;

    let mut ctx = Ctx {
        source,
        file_path,
        nodes: Vec::new(),
        refs: Vec::new(),
        next_seq: 0,
    };
    extract::extract_top(&mut ctx, tree.root_node(), file_path);
    Ok(ParseResult {
        nodes: ctx.nodes,
        refs: ctx.refs,
        parse_errors: super::count_parse_errors(tree.root_node()),
    })
}


pub(crate) struct Ctx<'a> {
    pub source: &'a str,
    #[allow(dead_code)]
    pub file_path: &'a str,
    pub nodes: Vec<ExtractedNode>,
    pub refs: Vec<ExtractedRef>,
    pub next_seq: u64,
}
