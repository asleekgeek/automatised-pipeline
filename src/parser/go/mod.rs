// parser::go — tree-sitter-based Go source parser for code-intelligence graph.
//
// Handles ``.go``. Extracts package, import, function, method (with receiver),
// struct, interface, type alias, const, var, and calls.
//
// Grammar reference: https://github.com/tree-sitter/tree-sitter-go

use tree_sitter::{Node, Parser};

use super::{
    node_field_text, node_text, qual, ExtractedNode, ExtractedRef, ParseResult, LABEL_CALL_SITE,
    LABEL_CONSTANT, LABEL_FUNCTION, LABEL_IMPORT, LABEL_METHOD, LABEL_STRUCT, LABEL_TRAIT,
    LABEL_TYPE_ALIAS,
};

mod extract;


pub(crate) const TS_PACKAGE_CLAUSE: &str = "package_clause";
pub(crate) const TS_IMPORT_DECL: &str = "import_declaration";
pub(crate) const TS_TYPE_DECL: &str = "type_declaration";
pub(crate) const TS_FUNCTION_DECL: &str = "function_declaration";
pub(crate) const TS_METHOD_DECL: &str = "method_declaration";
pub(crate) const TS_CONST_DECL: &str = "const_declaration";
pub(crate) const TS_VAR_DECL: &str = "var_declaration";
pub(crate) const TS_CALL_EXPR: &str = "call_expression";


pub fn parse_go_file(source: &str, file_path: &str) -> Result<ParseResult, String> {
    let lang: tree_sitter::Language = tree_sitter_go::LANGUAGE.into();
    let mut parser = Parser::new();
    parser
        .set_language(&lang)
        .map_err(|e| format!("failed to set Go language: {e}"))?;
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
    pub(crate) source: &'a str,
    #[allow(dead_code)]
    pub(crate) file_path: &'a str,
    pub(crate) nodes: Vec<ExtractedNode>,
    pub(crate) refs: Vec<ExtractedRef>,
    pub(crate) next_seq: u64,
}


pub(crate) fn go_visibility(name: &str) -> String {
    // Exported iff first letter is uppercase; idiomatic Go convention.
    match name.chars().next() {
        Some(c) if c.is_uppercase() => "public".to_string(),
        _ => "package".to_string(),
    }
}
