// parser — Multi-language tree-sitter parser module.
//
// Dispatches to language-specific parsers (Rust, Python, TypeScript).
// All parsers produce the same ParseResult/ExtractedNode/ExtractedRef types.
// The indexer calls `parse_file(source, file_path, language)` and gets a
// uniform result regardless of language.

pub mod c;
pub mod cpp;
pub mod go;
pub mod java;
pub mod kotlin;
pub mod objc;
pub mod python;
pub mod rust;
pub mod swift;
pub mod typescript;

// source: H2 fix — per-file tree-sitter parse timeout. 5 seconds is ~100x
// the typical parse time for a 1 MB source file on an M-series Mac (measured
// ~50 ms in practice) and matches the default suggested in the tree-sitter
// CLI (`--time-limit 5`). Parser::parse returns None when this is exceeded.
pub(crate) const PARSE_TIMEOUT_MICROS: u64 = 5_000_000;

use std::time::{Duration, Instant};

use tree_sitter::{Node, ParseOptions, ParseState, Parser, Tree};

mod language;
pub use language::Language;

// ---------------------------------------------------------------------------
// Output labels — match graph_store NODE_* constants by value, not by import.
// ---------------------------------------------------------------------------

pub const LABEL_FUNCTION: &str = "Function";
pub const LABEL_METHOD: &str = "Method";
pub const LABEL_STRUCT: &str = "Struct";
pub const LABEL_ENUM: &str = "Enum";
pub const LABEL_VARIANT: &str = "Variant";
pub const LABEL_TRAIT: &str = "Trait";
pub const LABEL_FIELD: &str = "Field";
pub const LABEL_CONSTANT: &str = "Constant";
pub const LABEL_TYPE_ALIAS: &str = "TypeAlias";
pub const LABEL_IMPORT: &str = "Import";
pub const LABEL_MODULE: &str = "Module";
pub const LABEL_CALL_SITE: &str = "CallSite";

// ---------------------------------------------------------------------------
// Public types — shared contract for all language parsers
// ---------------------------------------------------------------------------

pub struct ParseResult {
    pub nodes: Vec<ExtractedNode>,
    pub refs: Vec<ExtractedRef>,
    // source: stages/stage-3.md §6.1 (parse_errors on the parser-port output)
    // and §10.5 ("Parse errors per file ... dropping it hides broken parses
    // behind clean node counts"). Count of tree-sitter ERROR/MISSING nodes in
    // the parsed tree — tree-sitter recovers from bad syntax into ERROR/MISSING
    // nodes rather than failing, so a non-fatal parse can still be degraded.
    // Cross-ref: GitNexus safe-parse.ts detects `root.hasError || root.isMissing`.
    pub parse_errors: u32,
}

pub struct ExtractedNode {
    pub label: String,
    pub name: String,
    pub qualified_name: String,
    pub start_line: u64,
    pub end_line: u64,
    pub visibility: String,
    pub properties: Vec<(String, String)>,
}

pub struct ExtractedRef {
    pub kind: String,
    pub from_qualified_name: String,
    pub to_qualified_name: String,
}

// ---------------------------------------------------------------------------
// Unified entry point
// ---------------------------------------------------------------------------

/// Parses a source file and extracts typed symbols and relationships.
/// Dispatches to the appropriate language-specific parser.
pub fn parse_file(source: &str, file_path: &str, lang: Language) -> Result<ParseResult, String> {
    match lang {
        Language::Rust => rust::parse_rust_file(source, file_path),
        Language::Python => python::parse_python_file(source, file_path),
        Language::TypeScript => typescript::parse_typescript_file(source, file_path),
        Language::Java => java::parse_java_file(source, file_path),
        Language::Kotlin => kotlin::parse_kotlin_file(source, file_path),
        Language::Swift => swift::parse_swift_file(source, file_path),
        Language::ObjC => objc::parse_objc_file(source, file_path),
        Language::C => c::parse_c_file(source, file_path),
        Language::Cpp => cpp::parse_cpp_file(source, file_path),
        Language::Go => go::parse_go_file(source, file_path),
    }
}

// ---------------------------------------------------------------------------
// Shared helpers used by all language parsers
// ---------------------------------------------------------------------------

/// Extracts the full text of a node from source.
pub(crate) fn node_text(source: &str, node: Node) -> String {
    source[node.byte_range()].to_string()
}

/// Extracts text from a named field of a node. Returns empty string if absent.
pub(crate) fn node_field_text(source: &str, node: Node, field: &str) -> String {
    node.child_by_field_name(field)
        .map(|n| node_text(source, n))
        .unwrap_or_default()
}

/// Builds a qualified name with `::` separator (normalized form).
pub(crate) fn qual(scope: &str, name: &str) -> String {
    format!("{scope}::{name}")
}

/// Counts tree-sitter ERROR and MISSING nodes anywhere in the tree.
///
/// tree-sitter never throws on malformed input — it recovers into ERROR
/// (unparseable span) and MISSING (inserted-to-recover) nodes and returns a
/// tree anyway. A clean `Ok(ParseResult)` with zero symbols can therefore mean
/// either "empty file" or "the parser is keyed to the wrong grammar and every
/// construct became an ERROR node". This count is the quality signal that
/// distinguishes the two. source: stages/stage-3.md §10.5.
pub(crate) fn count_parse_errors(root: Node) -> u32 {
    let mut errors: u32 = 0;
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.is_error() || node.is_missing() {
            errors += 1;
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            stack.push(child);
        }
    }
    errors
}

/// Parses `source` under the shared per-file timeout guard and returns the tree.
///
/// tree-sitter 0.25 deprecated `Parser::set_timeout_micros`; the supported
/// replacement is a progress callback supplied through `ParseOptions`. Per the
/// tree-sitter C API (`api.h`: "Parsing was cancelled due to the progress
/// callback returning true"), the callback returns `true` to CANCEL — so we
/// return `true` once the wall-clock deadline of `PARSE_TIMEOUT_MICROS` passes.
/// `parse_with_options` takes an input-reader closure; for an in-memory `&str`
/// it hands tree-sitter the remaining byte slice from each requested offset.
/// Returns `Err` on timeout-cancel, source rejection, or a `None` tree.
/// source: tree-sitter v0.25 api.h TSParseOptions / ts_parser_parse_with_options.
pub(crate) fn parse_with_timeout(parser: &mut Parser, source: &str) -> Result<Tree, String> {
    let deadline = Instant::now() + Duration::from_micros(PARSE_TIMEOUT_MICROS);
    let mut past_deadline = |_state: &ParseState| Instant::now() >= deadline;
    let options = ParseOptions::new().progress_callback(&mut past_deadline);
    let bytes = source.as_bytes();
    parser
        .parse_with_options(
            &mut |offset, _pos| bytes.get(offset..).unwrap_or(&[]),
            None,
            Some(options),
        )
        .ok_or_else(|| {
            "parse_timeout_or_none: tree-sitter returned None \
             (parse cancelled, timeout exceeded, or source rejected)"
                .to_string()
        })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_language_from_extension() {
        assert_eq!(Language::from_extension("rs"), Some(Language::Rust));
        assert_eq!(Language::from_extension("py"), Some(Language::Python));
        assert_eq!(Language::from_extension("ts"), Some(Language::TypeScript));
        assert_eq!(Language::from_extension("tsx"), Some(Language::TypeScript));
        assert_eq!(Language::from_extension("java"), Some(Language::Java));
        assert_eq!(Language::from_extension("go"), Some(Language::Go));
        // JS family is parsed with the TypeScript grammar (JS ⊂ TS).
        assert_eq!(Language::from_extension("js"), Some(Language::TypeScript));
        assert_eq!(Language::from_extension("jsx"), Some(Language::TypeScript));
        assert_eq!(Language::from_extension("mjs"), Some(Language::TypeScript));
    }

    #[test]
    fn test_language_from_str() {
        assert_eq!(Language::from_str_opt("rust"), Some(Language::Rust));
        assert_eq!(Language::from_str_opt("python"), Some(Language::Python));
        assert_eq!(
            Language::from_str_opt("typescript"),
            Some(Language::TypeScript)
        );
        assert_eq!(Language::from_str_opt("java"), Some(Language::Java));
        assert_eq!(Language::from_str_opt("go"), Some(Language::Go));
        // "auto" is not a concrete language — detection happens by extension.
        assert_eq!(Language::from_str_opt("auto"), None);
    }

    #[test]
    fn test_qual() {
        assert_eq!(qual("src/main.rs", "foo"), "src/main.rs::foo");
    }
}
