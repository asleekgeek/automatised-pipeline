// parser::typescript — tree-sitter-based TypeScript source parser for code-intelligence graph.
//
// Parses a single `.ts`/`.tsx` file and extracts typed symbols matching the
// graph_store schema. Produces the same ParseResult types as the Rust parser.
//
// Grammar reference: https://github.com/tree-sitter/tree-sitter-typescript

use tree_sitter::Parser;

use super::{ExtractedNode, ExtractedRef, ParseResult};

mod extract;

// ---------------------------------------------------------------------------
// Tree-sitter node type constants
// source: https://github.com/tree-sitter/tree-sitter-typescript/blob/master/typescript/src/node-types.json
// ---------------------------------------------------------------------------

pub(crate) const TS_FUNC_DECL: &str = "function_declaration";
pub(crate) const TS_CLASS_DECL: &str = "class_declaration";
// source: tree-sitter-typescript v0.23.2 — abstract classes, generator
// functions, and `var` declarations are distinct top-level node kinds that were
// previously not dispatched (so `abstract class`, `function*`, and `var x`
// symbols were dropped). They share extractor logic with their non-abstract /
// non-generator / lexical counterparts.
pub(crate) const TS_ABSTRACT_CLASS_DECL: &str = "abstract_class_declaration";
pub(crate) const TS_GENERATOR_FUNC_DECL: &str = "generator_function_declaration";
pub(crate) const TS_VARIABLE_DECL: &str = "variable_declaration";
pub(crate) const TS_INTERFACE_DECL: &str = "interface_declaration";
pub(crate) const TS_ENUM_DECL: &str = "enum_declaration";
pub(crate) const TS_TYPE_ALIAS_DECL: &str = "type_alias_declaration";
pub(crate) const TS_IMPORT_STMT: &str = "import_statement";
pub(crate) const TS_EXPORT_STMT: &str = "export_statement";
pub(crate) const TS_LEXICAL_DECL: &str = "lexical_declaration";
pub(crate) const TS_METHOD_DEF: &str = "method_definition";
pub(crate) const TS_PUBLIC_FIELD: &str = "public_field_definition";
pub(crate) const TS_ENUM_BODY: &str = "enum_body";
pub(crate) const TS_CLASS_BODY: &str = "class_body";
pub(crate) const TS_INTERFACE_BODY: &str = "interface_body";
pub(crate) const TS_CALL_EXPR: &str = "call_expression";
pub(crate) const TS_ARROW_FUNC: &str = "arrow_function";
pub(crate) const TS_VAR_DECLARATOR: &str = "variable_declarator";
pub(crate) const TS_PROPERTY_SIGNATURE: &str = "property_signature";
pub(crate) const TS_METHOD_SIGNATURE: &str = "method_signature";

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Parses a single `.ts`/`.tsx` file and extracts typed symbols and relationships.
pub fn parse_typescript_file(source: &str, file_path: &str) -> Result<ParseResult, String> {
    // tree-sitter-typescript ships two distinct grammars: `typescript` and
    // `tsx`. JSX syntax (`<Component/>`) is ONLY in the tsx grammar — parsing a
    // .tsx/.jsx file with LANGUAGE_TYPESCRIPT makes every JSX element an ERROR
    // node and drops the symbols inside it. JS-family files (.js/.jsx/.mjs/.cjs)
    // are routed here too and contain no type syntax, so tsx is safe for them.
    // source: tree-sitter-typescript v0.23.2 (typescript vs tsx node-types.json);
    // cross-ref GitNexus parser-loader.ts selects the `:tsx` variant for .tsx.
    let use_tsx = file_path
        .rsplit('.')
        .next()
        .map(|ext| matches!(ext, "tsx" | "jsx" | "js" | "mjs" | "cjs"))
        .unwrap_or(false);
    let lang: tree_sitter::Language = if use_tsx {
        tree_sitter_typescript::LANGUAGE_TSX.into()
    } else {
        tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()
    };
    let mut parser = Parser::new();
    parser
        .set_language(&lang)
        .map_err(|e| format!("failed to set TypeScript language: {e}"))?;
    let tree = super::parse_with_timeout(&mut parser, source)?;

    let mut ctx = ExtractCtx {
        source,
        file_path,
        nodes: Vec::new(),
        refs: Vec::new(),
    };
    extract::extract_top_level(&mut ctx, tree.root_node(), file_path, false);
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
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_typescript() {
        let src = r#"
import { Router } from 'express';
import * as fs from 'fs';

export const MAX_RETRIES = 3;

export function greet(name: string): string {
    return `Hello, ${name}`;
}

export async function fetchData(url: string): Promise<Response> {
    return fetch(url);
}

export const handler = (req: Request) => {
    greet("world");
};

export class Animal {
    public name: string;
    private age: number;

    constructor(name: string) {
        this.name = name;
    }

    speak(): string {
        return "";
    }
}

export class Dog extends Animal {
    speak(): string {
        return "Woof";
    }
}

export interface Serializable {
    serialize(): string;
    readonly id: number;
}

export enum Color {
    Red = "RED",
    Green = "GREEN",
    Blue = "BLUE",
}

export type StringOrNumber = string | number;
"#;
        let result = parse_typescript_file(src, "test.ts").expect("parse should succeed");
        let labels: Vec<&str> = result.nodes.iter().map(|n| n.label.as_str()).collect();
        let names: Vec<&str> = result.nodes.iter().map(|n| n.name.as_str()).collect();

        // Functions
        assert!(labels.contains(&"Function"), "missing Function");
        assert!(names.contains(&"greet"), "missing greet");
        assert!(names.contains(&"fetchData"), "missing fetchData");
        assert!(names.contains(&"handler"), "missing handler (arrow fn)");

        // Classes (Struct)
        assert!(labels.contains(&"Struct"), "missing Struct (class)");
        assert!(names.contains(&"Animal"), "missing Animal");
        assert!(names.contains(&"Dog"), "missing Dog");

        // Interface (Trait)
        assert!(labels.contains(&"Trait"), "missing Trait (interface)");
        assert!(names.contains(&"Serializable"), "missing Serializable");

        // Enum
        assert!(labels.contains(&"Enum"), "missing Enum");
        assert!(names.contains(&"Color"), "missing Color");

        // TypeAlias
        assert!(labels.contains(&"TypeAlias"), "missing TypeAlias");
        assert!(names.contains(&"StringOrNumber"), "missing StringOrNumber");

        // Methods
        assert!(labels.contains(&"Method"), "missing Method");

        // Fields
        assert!(labels.contains(&"Field"), "missing Field");

        // Imports
        assert!(labels.contains(&"Import"), "missing Import");

        // Constants
        assert!(labels.contains(&"Constant"), "missing Constant");
        assert!(names.contains(&"MAX_RETRIES"), "missing MAX_RETRIES");

        // Async detection
        let fetch_fn = result.nodes.iter().find(|n| n.name == "fetchData").unwrap();
        let is_async = fetch_fn
            .properties
            .iter()
            .find(|(k, _)| k == "is_async")
            .unwrap();
        assert_eq!(is_async.1, "true");

        // Export = pub visibility
        let greet_fn = result.nodes.iter().find(|n| n.name == "greet").unwrap();
        assert_eq!(greet_fn.visibility, "pub");

        // Extends edge for Dog extends Animal
        let extends = result
            .refs
            .iter()
            .any(|r| r.kind == "Extends" && r.from_qualified_name.contains("Dog"));
        assert!(extends, "missing Extends edge for Dog");
    }

    #[test]
    fn test_typescript_imports() {
        let src = r#"
import { foo, bar as baz } from './module';
import * as utils from '../utils';
import defaultExport from 'package';
"#;
        let result = parse_typescript_file(src, "test.ts").expect("parse");
        let imports: Vec<_> = result
            .nodes
            .iter()
            .filter(|n| n.label == "Import")
            .collect();

        assert!(
            imports.len() >= 3,
            "expected at least 3 imports, got {}",
            imports.len()
        );

        // Check path normalization (/ -> ::)
        let has_normalized = imports.iter().any(|n| {
            n.properties
                .iter()
                .any(|(k, v)| k == "path" && v.contains("::"))
        });
        assert!(
            has_normalized,
            "import paths should be normalized to :: separator"
        );
    }
}
