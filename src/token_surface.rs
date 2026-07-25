// token_surface — token-efficient response shaping for list-returning tools (#56).
//
// Layer: shared/utility. A pure `Value` → `Value` transform (no I/O) that renders
// a homogeneous list of result objects into one of three surfaces the caller
// selects, trading completeness for token economy:
//   * `detail: "full"` (default) — the objects unchanged. Every field, richest.
//   * `detail: "ids"` — bare identifiers + a total. For a wide cheap sweep where
//     the agent only needs "which symbols exist", then drills in with get_symbol.
//   * `format: "tabular"` — columns declared once, each row an array of cells in
//     column order. Homogeneous result sets stop repeating every key per row.
//
// Reference: DeusData/codebase-memory-mcp — `detail="ids"` in search_graph and
// the TOON tabular encoding in `src/mcp/compact_out.h` (columns-once, rows-as-
// arrays). This is an AP-idiomatic reimplementation over `serde_json`, NOT a
// port: AP stays JSON-valued (a client needs no new parser) but emits the
// compact arrays-of-cells shape plus a `columns` header, so the savings come
// from not repeating field names, not from a bespoke wire format.
//
// `detail` and `format` compose as: ids wins (a single-column identifier list
// needs no table header); otherwise tabular vs plain-JSON objects.

use serde_json::{json, Value};

/// How much of each result object to return.
pub enum Detail {
    /// The full objects (default — unchanged behavior).
    Full,
    /// Bare identifiers only.
    Ids,
}

/// How to encode the (full) result objects.
pub enum Format {
    /// Array of JSON objects (default).
    Json,
    /// `columns` header + rows as arrays of cells.
    Tabular,
}

/// Parses `detail` from a tool's argument object. Unknown/absent → `Full`.
pub fn parse_detail(args: &serde_json::Map<String, Value>) -> Detail {
    match args.get("detail").and_then(|v| v.as_str()) {
        Some("ids") => Detail::Ids,
        _ => Detail::Full,
    }
}

/// Parses `format` from a tool's argument object. Unknown/absent → `Json`.
pub fn parse_format(args: &serde_json::Map<String, Value>) -> Format {
    match args.get("format").and_then(|v| v.as_str()) {
        Some("tabular") => Format::Tabular,
        _ => Format::Json,
    }
}

/// The rendered list plus what was applied, so the response is self-describing.
pub struct ListView {
    /// The value to store under the tool's list key: an array of id strings
    /// (ids), an array of cell-arrays (tabular), or an array of objects (json).
    pub value: Value,
    /// The column header — `Some` only in tabular mode, so a client knows how to
    /// read each row array.
    pub columns: Option<Value>,
    /// The applied detail level: `"ids"` or `"full"`.
    pub detail: &'static str,
    /// The applied encoding: `"ids"`, `"tabular"`, or `"json"`.
    pub format: &'static str,
}

/// Renders `items` (a slice of homogeneous JSON objects) per `detail`/`format`.
///
/// Preconditions: every element of `items` is a JSON object; `columns` names the
/// scalar fields to project in tabular order; `id_field` names the bare
/// identifier for ids mode. Postconditions: in ids mode `value` is the array of
/// `item[id_field]` (elements missing the field are skipped); in tabular mode
/// `value` is one cell-array per item (`item[col]` or `null`, in `columns`
/// order) and `columns` carries the header; in json mode `value` is the objects
/// unchanged. The element order of `items` is preserved in every mode, so an
/// upstream cursor's paging contract is untouched.
pub fn render_list(
    items: &[Value],
    columns: &[&str],
    id_field: &str,
    detail: &Detail,
    format: &Format,
) -> ListView {
    match detail {
        Detail::Ids => {
            let ids: Vec<Value> = items
                .iter()
                .filter_map(|it| it.get(id_field).cloned())
                .collect();
            ListView {
                value: Value::Array(ids),
                columns: None,
                detail: "ids",
                format: "ids",
            }
        }
        Detail::Full => match format {
            Format::Tabular => {
                let rows: Vec<Value> = items
                    .iter()
                    .map(|it| {
                        Value::Array(
                            columns
                                .iter()
                                .map(|c| it.get(*c).cloned().unwrap_or(Value::Null))
                                .collect(),
                        )
                    })
                    .collect();
                ListView {
                    value: Value::Array(rows),
                    columns: Some(json!(columns)),
                    detail: "full",
                    format: "tabular",
                }
            }
            Format::Json => ListView {
                value: Value::Array(items.to_vec()),
                columns: None,
                detail: "full",
                format: "json",
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn json_chars(value: &Value) -> usize {
        serde_json::to_string(value).map(|s| s.len()).unwrap_or(0)
    }

    fn sample() -> Vec<Value> {
        vec![
            json!({"qualified_name": "a::f", "kind": "Function", "score": "0.9"}),
            json!({"qualified_name": "b::g", "kind": "Method", "score": "0.5"}),
        ]
    }

    #[test]
    fn ids_mode_returns_bare_identifiers() {
        let v = render_list(
            &sample(),
            &["qualified_name", "kind", "score"],
            "qualified_name",
            &Detail::Ids,
            &Format::Json,
        );
        assert_eq!(v.detail, "ids");
        assert_eq!(v.value, json!(["a::f", "b::g"]));
        assert!(v.columns.is_none());
    }

    #[test]
    fn tabular_mode_declares_columns_once_and_streams_rows() {
        let cols = ["qualified_name", "kind", "score"];
        let v = render_list(
            &sample(),
            &cols,
            "qualified_name",
            &Detail::Full,
            &Format::Tabular,
        );
        assert_eq!(v.format, "tabular");
        assert_eq!(v.columns, Some(json!(cols)));
        assert_eq!(
            v.value,
            json!([["a::f", "Function", "0.9"], ["b::g", "Method", "0.5"]])
        );
    }

    #[test]
    fn json_mode_is_unchanged_objects() {
        let v = render_list(
            &sample(),
            &["qualified_name"],
            "qualified_name",
            &Detail::Full,
            &Format::Json,
        );
        assert_eq!(v.format, "json");
        assert_eq!(v.value, json!(sample()));
    }

    #[test]
    fn tabular_is_smaller_than_json_on_homogeneous_rows() {
        // The point of the whole module: not repeating field names shrinks the
        // payload. Proven here so a regression that inflates tabular output fails.
        let cols = ["qualified_name", "kind", "score"];
        let json_v = render_list(
            &sample(),
            &cols,
            "qualified_name",
            &Detail::Full,
            &Format::Json,
        );
        let tab_v = render_list(
            &sample(),
            &cols,
            "qualified_name",
            &Detail::Full,
            &Format::Tabular,
        );
        let json_size = json_chars(&json_v.value);
        // Tabular must also count its columns header to be fair.
        let tab_size = json_chars(&tab_v.value) + json_chars(tab_v.columns.as_ref().unwrap());
        assert!(
            tab_size < json_size,
            "tabular ({tab_size}) must be smaller than json ({json_size})"
        );
    }

    #[test]
    fn missing_id_field_is_skipped_in_ids_mode() {
        let items = vec![json!({"qualified_name": "a::f"}), json!({"other": 1})];
        let v = render_list(
            &items,
            &["qualified_name"],
            "qualified_name",
            &Detail::Ids,
            &Format::Json,
        );
        assert_eq!(v.value, json!(["a::f"]));
    }
}
