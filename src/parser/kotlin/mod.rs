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

use super::{ExtractedNode, ExtractedRef, ParseResult};

mod extract;

// Grammar reference: tree-sitter-grammars/tree-sitter-kotlin (the -ng crate)
// v1.1.0 src/node-types.json. The vocabulary below is verified against that
// file — the earlier fwcd/tree-sitter-kotlin names (import_header, object_body,
// simple_identifier, and the body/callee/receiver/delegation_specifiers FIELDS)
// do not exist in this grammar and silently extracted nothing.
pub(crate) const TS_PACKAGE_HEADER: &str = "package_header";
pub(crate) const TS_IMPORT: &str = "import"; // was "import_header" (fwcd)
pub(crate) const TS_CLASS_DECL: &str = "class_declaration";
pub(crate) const TS_OBJECT_DECL: &str = "object_declaration";
pub(crate) const TS_FUNCTION_DECL: &str = "function_declaration";
pub(crate) const TS_PROPERTY_DECL: &str = "property_declaration";
pub(crate) const TS_CLASS_BODY: &str = "class_body";
pub(crate) const TS_ENUM_CLASS_BODY: &str = "enum_class_body"; // enum members live here
pub(crate) const TS_FUNCTION_BODY: &str = "function_body";
pub(crate) const TS_ENUM_ENTRY: &str = "enum_entry";
pub(crate) const TS_DELEGATION_SPECIFIERS: &str = "delegation_specifiers";
pub(crate) const TS_CALL_EXPR: &str = "call_expression";

pub fn parse_kotlin_file(source: &str, file_path: &str) -> Result<ParseResult, String> {
    let lang: tree_sitter::Language = tree_sitter_kotlin_ng::LANGUAGE.into();
    let mut parser = Parser::new();
    parser
        .set_language(&lang)
        .map_err(|e| format!("failed to set Kotlin language: {e}"))?;
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
        error_ranges: super::collect_error_ranges(tree.root_node()),
    })
}

pub(crate) struct Ctx<'a> {
    pub(crate) source: &'a str,
    #[allow(dead_code)]
    pub(crate) file_path: &'a str,
    pub(crate) nodes: Vec<ExtractedNode>,
    pub(crate) refs: Vec<ExtractedRef>,
    // Per-file monotonic counter used as the last segment of call-site
    // and overloaded-method qualified_names. Two overloaded Kotlin funs
    // ``fun toDomain(x: A)`` and ``fun toDomain(x: B)`` would otherwise
    // collide on the LadybugDB primary key; the counter disambiguates.
    pub(crate) next_seq: u64,
}

/// Returns the first direct child of `node` whose kind matches `kind`.
/// The cursor-borrow pattern can't be closed over, so this is a small helper
/// used wherever the grammar exposes structure as child nodes rather than
/// named fields (common in tree-sitter-kotlin-ng).
pub(crate) fn child_of_kind<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    // Bind before returning so the `children` iterator temporary (borrowing
    // `cursor`) drops before `cursor` rather than as the block tail (E0597).
    let found = node.children(&mut cursor).find(|c| c.kind() == kind);
    found
}
