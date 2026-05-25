// visualize_graph — render the indexed Lbug graph as DOT + interactive HTML.
//
// Spike B' validated the extractor's correctness (41 hand-annotated + 576
// auto-counted fixtures). This test exercises the visualization layer: index
// a synthetic fixture, query every node + edge via Cypher, emit two artifacts:
//
//   1. <artifact_dir>/intra_inheritance.dot
//      Plain-text Graphviz DOT — snapshot-testable, render with
//      `dot -Tsvg intra_inheritance.dot -o intra_inheritance.svg`.
//
//   2. <artifact_dir>/intra_inheritance.html
//      Self-contained interactive page using vis-network (loaded from CDN).
//      Open in a browser to drag nodes, zoom, and inspect labels. The
//      embedded JSON is the same query result that produced the DOT file,
//      so the two views are guaranteed consistent.
//
// The Animal / Dog(Animal) / Puppy(Dog) fixture is the cleanest visual
// topology in the corpus: 3 classes, 3 methods, 2 cross-class Extends
// edges, 3 HasMethod edges — every relationship type the extractor
// emits in a single picture.

use ai_architect_mcp::graph_store::GraphStore;
use ai_architect_mcp::indexer;
use ai_architect_mcp::parser::Language;
use ai_architect_mcp::resolver;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;

struct Node {
    id: String,
    label: String,        // schema label (Struct / Method / File / ...)
    display_name: String, // last QN segment for visualization
}

struct Edge {
    from: String,
    to: String,
    kind: String, // rel_table name, e.g. "Extends_Struct_Struct"
}

fn fetch_nodes(store: &GraphStore) -> Vec<Node> {
    let mut nodes = Vec::new();
    let labels = [
        "File", "Directory", "Function", "Method", "Struct", "Enum", "Trait",
        "Constant", "TypeAlias", "Import", "CallSite", "Module",
    ];
    for label in &labels {
        let qn_col = match *label {
            "File" => "path",
            "Directory" => "path",
            "Import" | "CallSite" => "id",
            _ => "qualified_name",
        };
        let name_col = match *label {
            "Import" | "CallSite" => "id",
            _ => "name",
        };
        let q = format!("MATCH (n:{label}) RETURN n.id, n.{name_col}, n.{qn_col}");
        let res = match store.execute_query(&q) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for row in res.rows {
            if row.len() < 3 {
                continue;
            }
            let display_name = if !row[1].is_empty() && row[1] != "Null(String)" {
                row[1].clone()
            } else {
                // Fallback: last segment of the QN (e.g., for Imports whose
                // `name` column is sometimes empty).
                row[2].rsplit("::").next().unwrap_or(&row[2]).to_string()
            };
            nodes.push(Node {
                id: row[0].clone(),
                label: label.to_string(),
                display_name,
            });
        }
    }
    nodes
}

fn fetch_edges(store: &GraphStore) -> Vec<Edge> {
    use ai_architect_mcp::graph_store::REL_TABLES;
    let mut edges = Vec::new();
    for (rel, _, _) in REL_TABLES {
        let q = format!("MATCH (a)-[r:{rel}]->(b) RETURN a.id, b.id");
        let res = match store.execute_query(&q) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for row in res.rows {
            if row.len() < 2 {
                continue;
            }
            edges.push(Edge {
                from: row[0].clone(),
                to: row[1].clone(),
                kind: rel.to_string(),
            });
        }
    }
    edges
}

// Color palette keyed on schema label — same palette as the production
// graph_builder_nodes.py so a viewer comparing the test artifact with
// Cortex's UI sees consistent semantics.
fn color_for_label(label: &str) -> &'static str {
    match label {
        "File" => "#7088D0",
        "Directory" => "#9090A0",
        "Function" => "#10b981",
        "Method" => "#E0A840",
        "Struct" | "Class" => "#B088E0",
        "Enum" => "#C070D0",
        "Trait" => "#50D0E8",
        "Constant" => "#E0C050",
        "TypeAlias" => "#60D8F0",
        "Import" => "#60A0E0",
        "CallSite" => "#94a3b8",
        "Module" => "#6366f1",
        _ => "#999999",
    }
}

fn edge_color(kind: &str) -> &'static str {
    if kind.starts_with("Extends_") {
        "#FF00FF"
    } else if kind.starts_with("HasMethod_") {
        "#22d3ee"
    } else if kind.starts_with("Calls_") {
        "#10b981"
    } else if kind.starts_with("Imports_") {
        "#3b82f6"
    } else if kind.starts_with("Defines_") {
        "#a78bfa"
    } else if kind.starts_with("Uses_") {
        "#f59e0b"
    } else if kind.starts_with("Contains_") {
        "#B0B0B0"
    } else {
        "#666666"
    }
}

fn render_dot(nodes: &[Node], edges: &[Edge]) -> String {
    let mut out = String::new();
    out.push_str(
        "// Cortex-style indexed graph — generated by visualize_graph.rs\n\
         // source: synthetic/intra_inheritance.py + Spike B'-validated extractor\n\
         digraph G {\n  graph [bgcolor=\"#0f172a\", rankdir=BT];\n  \
         node [style=filled, fontname=\"Helvetica\", fontcolor=\"#0f172a\", \
         shape=box, penwidth=0];\n  edge [fontname=\"Helvetica\", \
         fontcolor=\"#e2e8f0\", fontsize=9, penwidth=1.5];\n\n",
    );
    let mut by_label: BTreeMap<&str, Vec<&Node>> = BTreeMap::new();
    for n in nodes {
        by_label.entry(n.label.as_str()).or_default().push(n);
    }
    for (label, ns) in &by_label {
        let color = color_for_label(label);
        out.push_str(&format!("  // {label} nodes\n"));
        for n in ns {
            let safe_id = dot_escape(&n.id);
            let safe_label = dot_escape(&format!("{}\\n{}", label, n.display_name));
            out.push_str(&format!(
                "  \"{safe_id}\" [label=\"{safe_label}\", fillcolor=\"{color}\"];\n"
            ));
        }
        out.push('\n');
    }
    out.push_str("  // edges\n");
    for e in edges {
        let safe_from = dot_escape(&e.from);
        let safe_to = dot_escape(&e.to);
        let safe_kind = dot_escape(&short_kind(&e.kind));
        let color = edge_color(&e.kind);
        out.push_str(&format!(
            "  \"{safe_from}\" -> \"{safe_to}\" [label=\"{safe_kind}\", color=\"{color}\"];\n"
        ));
    }
    out.push_str("}\n");
    out
}

fn dot_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn short_kind(kind: &str) -> String {
    // Strip the schema-encoded prefix (Defines_File_Struct → Defines)
    kind.split('_').next().unwrap_or(kind).to_string()
}

fn render_html(nodes: &[Node], edges: &[Edge]) -> String {
    // vis-network expects {id, label, color} for nodes and {from, to, label,
    // color} for edges. We embed the data as JSON so the page is offline-
    // viewable; only vis-network itself loads from CDN.
    let mut node_jsons = Vec::with_capacity(nodes.len());
    for n in nodes {
        node_jsons.push(format!(
            "{{\"id\":{}, \"label\":{}, \"title\":{}, \"color\":\"{}\"}}",
            json_str(&n.id),
            json_str(&format!("{}\n{}", n.label, n.display_name)),
            json_str(&n.id),
            color_for_label(&n.label),
        ));
    }
    let mut edge_jsons = Vec::with_capacity(edges.len());
    for e in edges {
        edge_jsons.push(format!(
            "{{\"from\":{}, \"to\":{}, \"label\":{}, \"color\":{{\"color\":\"{}\"}}, \"arrows\":\"to\"}}",
            json_str(&e.from),
            json_str(&e.to),
            json_str(&short_kind(&e.kind)),
            edge_color(&e.kind),
        ));
    }

    format!(
        r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>Cortex indexed graph — intra_inheritance.py</title>
<script src="https://unpkg.com/vis-network/standalone/umd/vis-network.min.js"></script>
<style>
  html, body {{ margin: 0; height: 100%; background: #0f172a; color: #e2e8f0;
                font-family: -apple-system, BlinkMacSystemFont, sans-serif; }}
  #graph {{ width: 100vw; height: calc(100vh - 80px); border-top: 1px solid #1e293b; }}
  header {{ padding: 12px 18px; }}
  h1 {{ margin: 0 0 4px; font-size: 16px; font-weight: 500; }}
  .legend {{ display: flex; gap: 12px; font-size: 12px; opacity: 0.8; flex-wrap: wrap; }}
  .swatch {{ display: inline-block; width: 10px; height: 10px; margin-right: 4px;
            border-radius: 2px; vertical-align: middle; }}
</style>
</head>
<body>
<header>
  <h1>Cortex indexed graph — synthetic/intra_inheritance.py ({n_nodes} nodes, {n_edges} edges)</h1>
  <div class="legend">
    <span><span class="swatch" style="background:#B088E0"></span>Struct (class)</span>
    <span><span class="swatch" style="background:#E0A840"></span>Method</span>
    <span><span class="swatch" style="background:#7088D0"></span>File</span>
    <span><span class="swatch" style="background:#9090A0"></span>Directory</span>
    <span><span class="swatch" style="background:#60A0E0"></span>Import</span>
    <span><span class="swatch" style="background:#FF00FF; height: 2px; width: 16px"></span>Extends</span>
    <span><span class="swatch" style="background:#22d3ee; height: 2px; width: 16px"></span>HasMethod</span>
    <span><span class="swatch" style="background:#a78bfa; height: 2px; width: 16px"></span>Defines</span>
  </div>
</header>
<div id="graph"></div>
<script>
const data = {{
  nodes: new vis.DataSet([{nodes_json}]),
  edges: new vis.DataSet([{edges_json}]),
}};
const network = new vis.Network(document.getElementById("graph"), data, {{
  layout: {{ hierarchical: {{ direction: "DU", sortMethod: "directed", levelSeparation: 140, nodeSpacing: 160 }} }},
  nodes: {{ shape: "box", borderWidth: 0, font: {{ color: "#0f172a", size: 14 }} }},
  edges: {{ font: {{ color: "#e2e8f0", strokeWidth: 0, size: 11 }},
           smooth: {{ type: "cubicBezier" }} }},
  physics: {{ enabled: false }},
}});
</script>
</body>
</html>
"##,
        n_nodes = nodes.len(),
        n_edges = edges.len(),
        nodes_json = node_jsons.join(","),
        edges_json = edge_jsons.join(","),
    )
}

fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[test]
fn visualize_intra_inheritance() {
    // Copy the synthetic fixture into a tempdir, run the full pipeline,
    // then serialize the indexed graph.
    let corpus = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/graph_accuracy/synthetic");
    let tmp = std::env::temp_dir()
        .join(format!("visualize_intra_inheritance_{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).expect("mkdir tempdir");
    fs::copy(
        corpus.join("intra_inheritance.py"),
        tmp.join("intra_inheritance.py"),
    )
    .expect("copy fixture");

    let graph_path = tmp.join("graph.lbug");
    indexer::index_codebase_with_language(&tmp, &graph_path, Some(Language::Python))
        .expect("index");
    let store = GraphStore::open_or_create(&graph_path).expect("open");
    let _ = resolver::resolve_graph(&store);

    let nodes = fetch_nodes(&store);
    let edges = fetch_edges(&store);

    // Sanity invariants — these match the synthetic fixture's expected
    // shape (verified by tests/graph_accuracy.rs synthetic_intra_inheritance_py).
    let n_structs = nodes.iter().filter(|n| n.label == "Struct").count();
    let n_methods = nodes.iter().filter(|n| n.label == "Method").count();
    let n_extends = edges.iter().filter(|e| e.kind.starts_with("Extends_")).count();
    let n_has_method = edges.iter().filter(|e| e.kind.starts_with("HasMethod_")).count();
    assert_eq!(n_structs, 3, "expected 3 Struct nodes (Animal/Dog/Puppy)");
    assert_eq!(n_methods, 3, "expected 3 Method nodes (.speak x3)");
    assert_eq!(n_extends, 2, "expected 2 Extends edges (Dog→Animal, Puppy→Dog)");
    assert_eq!(n_has_method, 3, "expected 3 HasMethod edges");

    // Render artifacts to a stable test-artifact directory the developer
    // can open after running the test.
    let artifact_dir =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/visualize");
    fs::create_dir_all(&artifact_dir).expect("mkdir artifact_dir");

    let dot_path = artifact_dir.join("intra_inheritance.dot");
    let html_path = artifact_dir.join("intra_inheritance.html");
    fs::write(&dot_path, render_dot(&nodes, &edges)).expect("write dot");
    fs::write(&html_path, render_html(&nodes, &edges)).expect("write html");

    println!(
        "wrote visualization artifacts:\n  {}\n  {}\nrender SVG with: dot -Tsvg {} -o {}",
        dot_path.display(),
        html_path.display(),
        dot_path.display(),
        dot_path.with_extension("svg").display(),
    );

    // DOT format snapshot: deterministic ordering of node-label buckets +
    // edges (Cypher row order is stable per Lbug). If the extractor's
    // emitted shape ever changes, this asserts the visualization output
    // tracks the change.
    let dot = fs::read_to_string(&dot_path).expect("read dot back");
    assert!(dot.contains("\"intra_inheritance.py::Animal\""));
    assert!(dot.contains("\"intra_inheritance.py::Dog\""));
    assert!(dot.contains("\"intra_inheritance.py::Puppy\""));
    let edges_in_dot = dot.matches("->").count();
    // Expected: Defines (file→annotations Import + file→Animal/Dog/Puppy Struct = 4)
    //         + HasMethod (Animal/Dog/Puppy → speak = 3)
    //         + Extends (Dog→Animal, Puppy→Dog = 2)
    //         = 9.
    // The file lives at tempdir root so no Contains_Dir_File edge; if a
    // future fixture moves it into a subdir that should bump to ≥10.
    // ≥9 catches the historic regressions where Extends (BUG #9) or
    // HasMethod (BUG #12 router) stopped emitting.
    assert!(
        edges_in_dot >= 9,
        "DOT should have ≥9 edges (4 Defines + 3 HasMethod + 2 Extends), got {edges_in_dot}"
    );

    // Distinct edge kinds — there should be at least 3 (Defines, HasMethod,
    // Extends; plus Contains if File hierarchy traverses). Earlier in
    // Spike B' this collapsed to 2.
    let mut kinds = BTreeSet::new();
    for e in &edges {
        kinds.insert(short_kind(&e.kind));
    }
    assert!(
        kinds.len() >= 3,
        "graph should have ≥3 distinct edge kinds, got {kinds:?}"
    );
}
