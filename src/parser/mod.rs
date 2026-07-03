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

use tree_sitter::Node;

// ---------------------------------------------------------------------------
// Supported languages
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    Rust,
    Python,
    TypeScript,
    Java,
    Kotlin,
    Swift,
    ObjC,
    C,
    Cpp,
    Go,
}

impl Language {
    /// Detects language from file extension. Returns None for unsupported.
    ///
    /// Ambiguity policy — a few extensions do not map 1:1 to a grammar. Each is
    /// resolved to the most common case below; when a file is mapped to a
    /// grammar that doesn't fit it, tree-sitter recovers into ERROR/MISSING
    /// nodes and `ParseResult.parse_errors` (see stage-3.md §10.5) becomes
    /// non-zero — so a mis-detection is observable rather than silent. Callers
    /// that know better can override via `from_str_opt` (the tool's `language:`
    /// arg). The specific rulings:
    ///   - `.h`  → C. Ambiguous across C / C++ / Objective-C headers; C is the
    ///     majority case and C constructs also parse under the C++ grammar. A
    ///     C++-only header (templates, namespaces) will show parse_errors; pass
    ///     `language: "cpp"` for such trees.
    ///   - `.mm` → ObjC. This is Objective-C++; the Objective-C grammar covers
    ///     the ObjC constructs and embedded C, but NOT full C++ (templates,
    ///     namespaces) — those portions become parse_errors. Full fidelity would
    ///     need a dedicated ObjC++ grammar, which tree-sitter has no crate for.
    ///   - `.js/.jsx/.mjs/.cjs` → TypeScript enum, parsed with the TSX grammar
    ///     dialect (see parser::typescript). JS is a syntactic subset of TSX
    ///     (minus type annotations, which JS files don't use), so functions /
    ///     classes / JSX all extract cleanly. source: viz "AST regardless of
    ///     file type" requirement, 2026-06-03.
    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext {
            "rs" => Some(Language::Rust),
            "py" => Some(Language::Python),
            "ts" | "tsx" => Some(Language::TypeScript),
            "java" => Some(Language::Java),
            "kt" | "kts" => Some(Language::Kotlin),
            "swift" => Some(Language::Swift),
            "m" | "mm" => Some(Language::ObjC),
            "c" | "h" => Some(Language::C),
            "cc" | "cpp" | "cxx" | "hh" | "hpp" | "hxx" => Some(Language::Cpp),
            "go" => Some(Language::Go),
            "js" | "jsx" | "mjs" | "cjs" => Some(Language::TypeScript),
            _ => None,
        }
    }

    #[allow(dead_code)]
    pub fn as_str(&self) -> &'static str {
        match self {
            Language::Rust => "rust",
            Language::Python => "python",
            Language::TypeScript => "typescript",
            Language::Java => "java",
            Language::Kotlin => "kotlin",
            Language::Swift => "swift",
            Language::ObjC => "objc",
            Language::C => "c",
            Language::Cpp => "cpp",
            Language::Go => "go",
        }
    }

    /// Parses a language string from the tool schema.
    pub fn from_str_opt(s: &str) -> Option<Self> {
        match s {
            "rust" => Some(Language::Rust),
            "python" => Some(Language::Python),
            "typescript" => Some(Language::TypeScript),
            "java" => Some(Language::Java),
            "kotlin" => Some(Language::Kotlin),
            "swift" => Some(Language::Swift),
            "objc" | "objective-c" => Some(Language::ObjC),
            "c" => Some(Language::C),
            "cpp" | "c++" => Some(Language::Cpp),
            "go" => Some(Language::Go),
            _ => None,
        }
    }
}

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
        assert_eq!(Language::from_str_opt("typescript"), Some(Language::TypeScript));
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
