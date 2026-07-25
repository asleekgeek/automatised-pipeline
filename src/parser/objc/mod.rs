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

use super::{node_text, ExtractedNode, ExtractedRef, ParseResult};

mod extract;

pub(crate) const TS_CLASS_INTERFACE: &str = "class_interface";
pub(crate) const TS_CLASS_IMPL: &str = "class_implementation";
pub(crate) const TS_PROTOCOL_DECL: &str = "protocol_declaration";
pub(crate) const TS_METHOD_DECL: &str = "method_declaration";
pub(crate) const TS_METHOD_DEF: &str = "method_definition";
pub(crate) const TS_FUNCTION_DEF: &str = "function_definition";
pub(crate) const TS_IMPORT: &str = "preproc_include";
pub(crate) const TS_MODULE_IMPORT: &str = "module_import";
pub(crate) const TS_CALL: &str = "call_expression";
pub(crate) const TS_MSG_EXPR: &str = "message_expression";
// Objective-C is a superset of C — `.m`/`.h` files routinely declare plain C
// structs, unions, enums and typedefs. The 3.0.2 grammar exposes them as
// standard C node kinds, so key them the same way the C parser does; without
// this, those declarations were silently dropped from ObjC files.
// source: tree-sitter-grammars/tree-sitter-objc v3.0.2 (embedded C rules).
pub(crate) const TS_C_STRUCT: &str = "struct_specifier";
pub(crate) const TS_C_UNION: &str = "union_specifier";
pub(crate) const TS_C_ENUM: &str = "enum_specifier";
pub(crate) const TS_C_TYPEDEF: &str = "type_definition";

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
    extract::extract_top(&mut ctx, tree.root_node(), file_path);
    Ok(ParseResult {
        nodes: ctx.nodes,
        refs: ctx.refs,
        parse_errors: super::count_parse_errors(tree.root_node()),
        error_ranges: super::collect_error_ranges(tree.root_node()),
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

/// Returns the text of the first `identifier` child of `node`, or empty.
pub(crate) fn first_identifier(source: &str, node: Node) -> String {
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
