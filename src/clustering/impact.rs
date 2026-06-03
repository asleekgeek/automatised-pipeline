use crate::graph_store::GraphStore;

/// A reverse-dependency edge endpoint, carried as a re-queryable handle
/// (id + qualified_name + label) rather than a flattened name string, so a
/// consumer can keep traversing the graph through MCP from this node instead
/// of receiving a terminal summary.
/// source: anti-flattening principle — `get_impact` must hand back traversal
/// handles, not a dead-end digest (the caller continues via get_symbol /
/// get_context / query_graph on `id`).
pub struct ImpactNode {
    pub id: String,
    pub qualified_name: String,
    pub label: String,
}

pub struct ImpactResult {
    pub communities: Vec<String>,
    pub processes: Vec<String>,
    /// Reverse `Calls` — functions/methods that call the target.
    pub callers: Vec<ImpactNode>,
    /// Reverse `Imports` — files/modules that import the target.
    pub importers: Vec<ImpactNode>,
    /// Reverse `Uses` — symbols that use the target type.
    pub users: Vec<ImpactNode>,
    /// Reverse `Implements` — types that implement the target trait.
    pub implementors: Vec<ImpactNode>,
}

// ---------------------------------------------------------------------------
// get_impact — blast radius for a symbol
// source: stages/stage-3c.md §5 get_impact
// ---------------------------------------------------------------------------

pub fn get_impact(
    store: &GraphStore,
    qualified_name: &str,
) -> Result<ImpactResult, String> {
    let esc = qualified_name.replace('\'', "\\'");

    // Find communities this symbol belongs to
    let mut communities = Vec::new();
    for label in super::SYMBOL_LABELS {
        let rel = format!("MemberOf_{label}_Community");
        let cypher = format!(
            "MATCH (n:{label})-[:{rel}]->(c:Community) \
             WHERE n.id = '{esc}' OR n.qualified_name = '{esc}' \
             RETURN c.id"
        );
        if let Ok(qr) = store.execute_query(&cypher) {
            for row in &qr.rows {
                if !row.is_empty() { communities.push(row[0].clone()); }
            }
        }
    }

    // Find processes this symbol participates in
    let mut processes = Vec::new();
    for label in &["Function", "Method"] {
        let rel = format!("ParticipatesIn_{label}_Process");
        let cypher = format!(
            "MATCH (n:{label})-[:{rel}]->(p:Process) \
             WHERE n.id = '{esc}' OR n.qualified_name = '{esc}' \
             RETURN p.name"
        );
        if let Ok(qr) = store.execute_query(&cypher) {
            for row in &qr.rows {
                if !row.is_empty() { processes.push(row[0].clone()); }
            }
        }
    }

    // Reverse-dependency traversal — the actual blast radius. The tool is
    // named for impact analysis but previously returned only community +
    // process membership; the set of symbols that DEPEND ON the target
    // (callers, importers, users, implementors) is what a "what breaks if I
    // change this?" query needs. Each is a re-queryable handle so the caller
    // can keep walking the graph through MCP rather than stopping at a digest.
    let callers = reverse_dependents(store, &esc, "Calls_");
    let importers = reverse_dependents(store, &esc, "Imports_");
    let users = reverse_dependents(store, &esc, "Uses_");
    let implementors = reverse_dependents(store, &esc, "Implements_");

    Ok(ImpactResult {
        communities,
        processes,
        callers,
        importers,
        users,
        implementors,
    })
}

/// Reverse-traverses every `REL_TABLES` edge whose name starts with `prefix`,
/// binding the escaped target to the edge's `to` endpoint and returning the
/// `from` endpoints as re-queryable handles. This is the inverse of the
/// forward "what does X reference?" walk: "what references X?".
///
/// `esc` must already be single-quote-escaped (see `get_impact`).
/// CallSite sources are skipped: they carry no `qualified_name`, so they
/// would contribute null-name noise — the function-level caller is the
/// meaningful dependent and is captured by the direct `Calls_Function_*` /
/// `Calls_Method_*` edges the resolver also emits.
fn reverse_dependents(store: &GraphStore, esc: &str, prefix: &str) -> Vec<ImpactNode> {
    let mut out = Vec::new();
    for &(rel, from_label, to_label) in crate::graph_store::REL_TABLES {
        if !rel.starts_with(prefix) {
            continue;
        }
        if from_label == crate::graph_store::NODE_CALL_SITE {
            continue;
        }
        let cypher = format!(
            "MATCH (a:{from_label})-[:{rel}]->(b:{to_label}) \
             WHERE b.id = '{esc}' OR b.qualified_name = '{esc}' \
             RETURN a.id, a.qualified_name"
        );
        if let Ok(qr) = store.execute_query(&cypher) {
            for row in &qr.rows {
                if row.len() >= 2 {
                    out.push(ImpactNode {
                        id: row[0].clone(),
                        qualified_name: row[1].clone(),
                        label: from_label.to_string(),
                    });
                }
            }
        }
    }
    out
}
