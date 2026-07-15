// parser::python — tree-sitter-based Python source parser for code-intelligence graph.
//
// Parses a single `.py` file and extracts typed symbols matching the
// graph_store schema. Produces the same ParseResult types as the Rust parser.
//
// Grammar reference: https://github.com/tree-sitter/tree-sitter-python

use tree_sitter::Parser;

use super::{ExtractedNode, ExtractedRef, ParseResult};

mod extract;

// ---------------------------------------------------------------------------
// Tree-sitter node type constants
// source: https://github.com/tree-sitter/tree-sitter-python/blob/master/src/node-types.json
// ---------------------------------------------------------------------------

pub(crate) const TS_FUNCTION_DEF: &str = "function_definition";
pub(crate) const TS_CLASS_DEF: &str = "class_definition";
pub(crate) const TS_IMPORT_STMT: &str = "import_statement";
pub(crate) const TS_IMPORT_FROM: &str = "import_from_statement";
// source: Spike B' BUG #13 — tree-sitter-python gives `from __future__ import
// X` its own node kind (Python treats __future__ specially because it must
// appear before any other code). Without this constant the dispatcher in
// extract_top_level fell through `_ => {}` and silently dropped every
// future-import in the corpus.
pub(crate) const TS_FUTURE_IMPORT: &str = "future_import_statement";
pub(crate) const TS_DECORATED_DEF: &str = "decorated_definition";
pub(crate) const TS_EXPRESSION_STMT: &str = "expression_statement";
pub(crate) const TS_ASSIGNMENT: &str = "assignment";
pub(crate) const TS_CALL: &str = "call";

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Parses a single `.py` file and extracts typed symbols and relationships.
pub fn parse_python_file(source: &str, file_path: &str) -> Result<ParseResult, String> {
    let lang: tree_sitter::Language = tree_sitter_python::LANGUAGE.into();
    let mut parser = Parser::new();
    parser
        .set_language(&lang)
        .map_err(|e| format!("failed to set Python language: {e}"))?;
    let tree = super::parse_with_timeout(&mut parser, source)?;

    let mut ctx = ExtractCtx {
        source,
        file_path,
        nodes: Vec::new(),
        refs: Vec::new(),
        emitted_qns: std::collections::HashSet::new(),
    };
    extract::extract_top_level(&mut ctx, tree.root_node(), file_path, None);
    Ok(ParseResult {
        nodes: ctx.nodes,
        refs: ctx.refs,
        parse_errors: super::count_parse_errors(tree.root_node()),
    })
}

// ---------------------------------------------------------------------------
// Extraction context
// ---------------------------------------------------------------------------

pub(crate) struct ExtractCtx<'a> {
    pub(crate) source: &'a str,
    #[allow(dead_code)]
    pub(crate) file_path: &'a str,
    pub(crate) nodes: Vec<ExtractedNode>,
    pub(crate) refs: Vec<ExtractedRef>,
    /// Qualified names already emitted in this file. Used to disambiguate
    /// Python @property/@setter pairs and other same-named overloads
    /// (Rust impl Trait for X & inherent impl can also share method names).
    pub(crate) emitted_qns: std::collections::HashSet<String>,
}

impl<'a> ExtractCtx<'a> {
    /// Returns a unique qn: the input if unseen, else `qn@{start_line}` so
    /// every Method/Function node has a unique primary key while preserving
    /// the readable name for resolver name-based lookups.
    fn dedup_qn(&mut self, qn: String, start_line: u64) -> String {
        if self.emitted_qns.insert(qn.clone()) {
            return qn;
        }
        let unique = format!("{qn}@{start_line}");
        self.emitted_qns.insert(unique.clone());
        unique
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Python visibility convention: leading underscore = private.
pub(crate) fn python_visibility(name: &str) -> String {
    // Dunder methods (__init__, __str__, etc.) are public by convention
    if name.starts_with("__") && name.ends_with("__") {
        return String::new();
    }
    if name.starts_with('_') {
        "private".to_string()
    } else {
        String::new()
    }
}

/// Checks if a name is UPPER_SNAKE_CASE (Python constant convention).
pub(crate) fn is_upper_snake_case(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_uppercase() || c == '_' || c.is_ascii_digit())
        && name.chars().any(|c| c.is_ascii_uppercase())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_python() {
        let src = r#"
import os
from pathlib import Path
from typing import *

MAX_SIZE = 100
_PRIVATE = "hidden"

def greet(name: str) -> str:
    return f"Hello, {name}"

async def fetch_data(url):
    pass

class Animal:
    def __init__(self, name):
        self.name = name

    def speak(self):
        pass

class Dog(Animal):
    def speak(self):
        return "Woof"
"#;
        let result = parse_python_file(src, "test.py").expect("parse should succeed");
        let labels: Vec<&str> = result.nodes.iter().map(|n| n.label.as_str()).collect();
        let names: Vec<&str> = result.nodes.iter().map(|n| n.name.as_str()).collect();

        // Functions
        assert!(labels.contains(&"Function"), "missing Function");
        assert!(names.contains(&"greet"), "missing greet function");
        assert!(names.contains(&"fetch_data"), "missing fetch_data function");

        // Classes (mapped to Struct)
        assert!(labels.contains(&"Struct"), "missing Struct (class)");
        assert!(names.contains(&"Animal"), "missing Animal class");
        assert!(names.contains(&"Dog"), "missing Dog class");

        // Methods
        assert!(labels.contains(&"Method"), "missing Method");
        assert!(names.contains(&"__init__"), "missing __init__ method");
        assert!(names.contains(&"speak"), "missing speak method");

        // Imports
        assert!(labels.contains(&"Import"), "missing Import");

        // Constants
        assert!(labels.contains(&"Constant"), "missing Constant");
        assert!(names.contains(&"MAX_SIZE"), "missing MAX_SIZE constant");

        // Async detection
        let fetch = result
            .nodes
            .iter()
            .find(|n| n.name == "fetch_data")
            .unwrap();
        let is_async = fetch
            .properties
            .iter()
            .find(|(k, _)| k == "is_async")
            .unwrap();
        assert_eq!(is_async.1, "true");

        // Extends edge for Dog(Animal)
        let extends = result
            .refs
            .iter()
            .any(|r| r.kind == "Extends" && r.from_qualified_name.contains("Dog"));
        assert!(extends, "missing Extends edge for Dog");

        // Glob import
        let glob_import = result.nodes.iter().any(|n| {
            n.label == "Import"
                && n.properties
                    .iter()
                    .any(|(k, v)| k == "is_glob" && v == "true")
        });
        assert!(
            glob_import,
            "missing glob import for 'from typing import *'"
        );
    }

    #[test]
    fn test_python_visibility() {
        assert_eq!(python_visibility("public_func"), "");
        assert_eq!(python_visibility("_private_func"), "private");
        assert_eq!(python_visibility("__mangled"), "private");
        // Dunder methods (__init__, __str__, etc.) are public by convention
        assert_eq!(python_visibility("__init__"), "");
        assert_eq!(python_visibility("__str__"), "");
    }

    #[test]
    fn test_upper_snake_case() {
        assert!(is_upper_snake_case("MAX_SIZE"));
        assert!(is_upper_snake_case("FOO"));
        assert!(is_upper_snake_case("HTTP_200"));
        assert!(!is_upper_snake_case("foo"));
        assert!(!is_upper_snake_case("Foo_Bar"));
        assert!(!is_upper_snake_case("_"));
    }

    #[test]
    fn test_python_import_normalization() {
        let src = r#"
import os.path
from collections.abc import Mapping
"#;
        let result = parse_python_file(src, "test.py").expect("parse");
        let imports: Vec<_> = result
            .nodes
            .iter()
            .filter(|n| n.label == "Import")
            .collect();

        // os.path should be normalized to os::path
        let has_normalized = imports.iter().any(|n| {
            n.properties
                .iter()
                .any(|(k, v)| k == "path" && v == "os::path")
        });
        assert!(
            has_normalized,
            "import paths should be normalized to :: separator"
        );
    }
}
