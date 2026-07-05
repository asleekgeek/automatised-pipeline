use crate::graph_store::GraphStore;
use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Instant;

use super::process::trace_processes;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

pub struct ClusteringResult {
    pub communities: u64,
    pub modularity: f64,
    pub processes: u64,
    pub elapsed_ms: u64,
}

/// One (symbol → community) membership row, derived from the persisted
/// MemberOf_<Label>_Community edge tables.
pub struct ClusterMembership {
    pub qualified_name: String,
    pub community_id: String,
    pub cluster_id: i64,
}

/// Bounded view over the full membership mapping. `total` is the pre-cap
/// count; when `truncated_at` is `Some(n)`, `entries.len() == n < total`
/// and the remainder is still reachable via `query_graph`.
pub struct ClusterMemberships {
    pub entries: Vec<ClusterMembership>,
    pub truncated_at: Option<usize>,
    pub total: usize,
}

const CLUSTERS_RESPONSE_CAP: usize = 10_000;

const MEMBEROF_LABELS: &[&str] = &[
    "Function", "Method", "Struct", "Enum", "Trait",
    "Constant", "TypeAlias", "Module",
];

/// Collect per-symbol community memberships by scanning every
/// `MemberOf_<Label>_Community` edge table. The response is capped at
/// `CLUSTERS_RESPONSE_CAP` entries; the full mapping remains queryable via
/// `query_graph` against the same edge tables.
pub fn collect_cluster_memberships(store: &GraphStore) -> Result<ClusterMemberships, String> {
    let mut entries: Vec<ClusterMembership> = Vec::new();
    for label in MEMBEROF_LABELS {
        let rel = format!("MemberOf_{label}_Community");
        let cypher = format!(
            "MATCH (n:{label})-[:{rel}]->(c:Community) \
             RETURN n.qualified_name, c.id"
        );
        let qr = match store.execute_query(&cypher) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for row in qr.rows {
            if row.len() < 2 {
                continue;
            }
            let cid = cluster_id_from_community_id(&row[1]);
            entries.push(ClusterMembership {
                qualified_name: row[0].clone(),
                community_id: row[1].clone(),
                cluster_id: cid,
            });
        }
    }
    Ok(sort_and_cap_memberships(entries))
}

/// Sort entries deterministically before applying the 10k truncation cap.
/// Must-fix from d-review.md §6: lbug/Kuzu row order per query is not
/// guaranteed, so an unsorted truncation would drop arbitrary entries per
/// run and break Q12 ARI reproducibility on graphs exceeding the cap.
fn sort_and_cap_memberships(mut entries: Vec<ClusterMembership>) -> ClusterMemberships {
    entries.sort_by(|a, b| {
        a.qualified_name
            .cmp(&b.qualified_name)
            .then_with(|| a.community_id.cmp(&b.community_id))
    });
    let total = entries.len();
    let truncated_at = if total > CLUSTERS_RESPONSE_CAP {
        entries.truncate(CLUSTERS_RESPONSE_CAP);
        Some(CLUSTERS_RESPONSE_CAP)
    } else {
        None
    };
    ClusterMemberships {
        entries,
        truncated_at,
        total,
    }
}

/// community_id persisted by `persist_communities` is
/// `community::louvain::<gamma>::<N>`. Extract the trailing integer so
/// the bench harness (which scores clusters via adjusted Rand index on
/// integer labels) can map community ids without parsing the prefix.
pub fn cluster_id_from_community_id(community_id: &str) -> i64 {
    community_id
        .rsplit("::")
        .next()
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(-1)
}

// ---------------------------------------------------------------------------
// Edge weight table — source: stages/stage-3c.md §2.4
// ---------------------------------------------------------------------------

fn edge_weight(rel_name: &str) -> f64 {
    if rel_name.starts_with("Calls_") {
        3.0
    } else if rel_name.starts_with("Implements_") || rel_name.starts_with("Extends_") {
        2.0
    } else if rel_name.starts_with("Imports_") || rel_name.starts_with("Uses_") {
        1.0
    } else if rel_name.starts_with("HasMethod_")
        || rel_name.starts_with("HasField_")
        || rel_name.starts_with("HasVariant_")
    {
        5.0
    } else {
        0.0
    }
}

// ---------------------------------------------------------------------------
// Adjacency extraction — source: stages/stage-3c.md §2.4
// ---------------------------------------------------------------------------

struct Adjacency {
    node_ids: Vec<String>,
    node_labels: Vec<String>,
    #[allow(dead_code)] // used by tests for constructing test adjacencies
    id_to_idx: HashMap<String, usize>,
    neighbors: Vec<Vec<(usize, f64)>>,
    total_weight: f64,
}

fn extract_adjacency(store: &GraphStore) -> Result<Adjacency, String> {
    let (node_ids, node_labels, id_to_idx) = collect_symbol_nodes(store)?;
    let n = node_ids.len();
    let (neighbors, total_weight) = collect_weighted_edges(store, &id_to_idx, n)?;
    Ok(Adjacency { node_ids, node_labels, id_to_idx, neighbors, total_weight })
}

fn collect_symbol_nodes(
    store: &GraphStore,
) -> Result<(Vec<String>, Vec<String>, HashMap<String, usize>), String> {
    let mut ids = Vec::new();
    let mut labels = Vec::new();
    let mut map: HashMap<String, usize> = HashMap::new();
    for label in super::SYMBOL_LABELS {
        let cypher = format!("MATCH (n:{label}) RETURN n.id");
        let qr = match store.execute_query(&cypher) {
            Ok(q) => q,
            Err(_) => continue,
        };
        for row in &qr.rows {
            if row.is_empty() { continue; }
            let id = &row[0];
            if !map.contains_key(id) {
                map.insert(id.clone(), ids.len());
                ids.push(id.clone());
                labels.push(label.to_string());
            }
        }
    }
    Ok((ids, labels, map))
}

fn collect_weighted_edges(
    store: &GraphStore, id_to_idx: &HashMap<String, usize>, n: usize,
) -> Result<(Vec<Vec<(usize, f64)>>, f64), String> {
    let mut neighbors: Vec<Vec<(usize, f64)>> = vec![Vec::new(); n];
    let mut total_weight = 0.0;
    for &(rel, from_label, to_label) in edge_rel_tables() {
        let w = edge_weight(rel);
        if w == 0.0 { continue; }
        let cypher = format!(
            "MATCH (a:{from_label})-[:{rel}]->(b:{to_label}) RETURN a.id, b.id"
        );
        let qr = match store.execute_query(&cypher) { Ok(q) => q, Err(_) => continue };
        for row in &qr.rows {
            if row.len() < 2 { continue; }
            if let (Some(&a), Some(&b)) = (id_to_idx.get(&row[0]), id_to_idx.get(&row[1])) {
                neighbors[a].push((b, w));
                neighbors[b].push((a, w));
                total_weight += w;
            }
        }
    }
    Ok((neighbors, total_weight))
}

fn edge_rel_tables() -> &'static [(&'static str, &'static str, &'static str)] {
    &[
        ("Calls_Function_Function", "Function", "Function"),
        ("Calls_Function_Method", "Function", "Method"),
        ("Calls_Method_Function", "Method", "Function"),
        ("Calls_Method_Method", "Method", "Method"),
        ("Imports_File_Function", "File", "Function"),
        ("Imports_File_Struct", "File", "Struct"),
        ("Imports_File_Enum", "File", "Enum"),
        ("Imports_File_Trait", "File", "Trait"),
        ("Implements_Struct_Trait", "Struct", "Trait"),
        ("Implements_Enum_Trait", "Enum", "Trait"),
        ("Extends_Trait_Trait", "Trait", "Trait"),
        ("Uses_Function_Struct", "Function", "Struct"),
        ("Uses_Function_Enum", "Function", "Enum"),
        ("Uses_Function_Trait", "Function", "Trait"),
        ("Uses_Method_Struct", "Method", "Struct"),
        ("Uses_Method_Enum", "Method", "Enum"),
        ("Uses_Method_Trait", "Method", "Trait"),
        ("HasMethod_Struct_Method", "Struct", "Method"),
        ("HasMethod_Enum_Method", "Enum", "Method"),
        ("HasMethod_Trait_Method", "Trait", "Method"),
        ("HasField_Struct_Field", "Struct", "Field"),
        ("HasField_Enum_Field", "Enum", "Field"),
        ("HasVariant_Enum_Variant", "Enum", "Variant"),
    ]
}

// ---------------------------------------------------------------------------
// Louvain algorithm — Blondel et al. 2008
// source: "Fast unfolding of communities in large networks"
// ---------------------------------------------------------------------------

fn louvain(adj: &Adjacency, gamma: f64) -> (Vec<usize>, f64) {
    let n = adj.node_ids.len();
    if n == 0 {
        return (vec![], 0.0);
    }
    let m = adj.total_weight; // sum of edge weights (each undirected edge once)
    if m == 0.0 {
        return ((0..n).collect(), 0.0);
    }
    let two_m = 2.0 * m; // Newman's 2m: sum of degrees = 2 * sum of edge weights

    // k[i] = sum of neighbor weights for node i (undirected degree)
    let k: Vec<f64> = adj.neighbors.iter()
        .map(|nbrs| nbrs.iter().map(|(_, w)| w).sum())
        .collect();

    let mut comm: Vec<usize> = (0..n).collect();
    // sigma_tot[c] = sum of degrees of nodes in community c
    let mut sigma_tot: Vec<f64> = k.clone();
    let max_passes = 100;

    for _ in 0..max_passes {
        let mut improved = false;
        for i in 0..n {
            let old_c = comm[i];
            let ki = k[i];

            // Weights from i to each neighboring community
            let mut ki_in: HashMap<usize, f64> = HashMap::new();
            for &(nbr, w) in &adj.neighbors[i] {
                *ki_in.entry(comm[nbr]).or_insert(0.0) += w;
            }

            // Remove i from its community for gain computation
            sigma_tot[old_c] -= ki;

            // Gain = ki_in_c - gamma * sigma_tot_c * ki / (2m)
            // source: Blondel 2008 eq. from section III
            let ki_in_old = ki_in.get(&old_c).copied().unwrap_or(0.0);
            let mut best_c = old_c;
            let mut best_gain = ki_in_old - gamma * sigma_tot[old_c] * ki / two_m;

            for (&c, &ki_in_c) in &ki_in {
                let gain = ki_in_c - gamma * sigma_tot[c] * ki / two_m;
                if gain > best_gain {
                    best_gain = gain;
                    best_c = c;
                }
            }

            comm[i] = best_c;
            sigma_tot[best_c] += ki;
            if best_c != old_c { improved = true; }
        }
        if !improved { break; }
    }

    let comm = renumber_communities(&comm);
    let q = compute_modularity(&adj.neighbors, &comm, &k, m);
    (comm, q)
}

fn renumber_communities(comm: &[usize]) -> Vec<usize> {
    let mut map: HashMap<usize, usize> = HashMap::new();
    let mut next = 0;
    let mut result = Vec::with_capacity(comm.len());
    for &c in comm {
        let new_c = *map.entry(c).or_insert_with(|| {
            let v = next;
            next += 1;
            v
        });
        result.push(new_c);
    }
    result
}

/// Newman 2004: Q = (1/2m) * sum_ij [A_ij - ki*kj/(2m)] * delta(ci,cj)
/// `m` = sum of undirected edge weights (each edge counted once).
fn compute_modularity(
    neighbors: &[Vec<(usize, f64)>],
    comm: &[usize],
    k: &[f64],
    m: f64,
) -> f64 {
    if m == 0.0 { return 0.0; }
    let two_m = 2.0 * m;
    let mut q = 0.0;
    // neighbors stores both directions, so the loop sums each pair (i,j) twice.
    // This cancels with the 1/(2m) factor, leaving division by two_m once.
    for (i, nbrs) in neighbors.iter().enumerate() {
        for &(j, w) in nbrs {
            if comm[i] == comm[j] {
                q += w - k[i] * k[j] / two_m;
            }
        }
    }
    q / two_m
}

// ---------------------------------------------------------------------------
// C2 repair: split disconnected communities — Traag 2019 §3.2
// ---------------------------------------------------------------------------

fn repair_c2(adj: &Adjacency, comm: &mut Vec<usize>) {
    let n = comm.len();
    let num_comms = comm.iter().copied().max().map_or(0, |m| m + 1);
    let mut next_comm = num_comms;

    for c in 0..num_comms {
        let members: Vec<usize> = (0..n).filter(|&i| comm[i] == c).collect();
        if members.len() <= 1 { continue; }

        let components = connected_components_within(&members, &adj.neighbors, comm, c);
        if components.len() <= 1 { continue; }

        // Keep first component as c, assign rest new IDs
        for component in components.iter().skip(1) {
            for &node in component {
                comm[node] = next_comm;
            }
            next_comm += 1;
        }
    }
    *comm = renumber_communities(comm);
}

fn connected_components_within(
    members: &[usize],
    neighbors: &[Vec<(usize, f64)>],
    comm: &[usize],
    community: usize,
) -> Vec<Vec<usize>> {
    let member_set: HashSet<usize> = members.iter().copied().collect();
    let mut visited = HashSet::new();
    let mut components = Vec::new();

    for &start in members {
        if visited.contains(&start) { continue; }
        let mut component = Vec::new();
        let mut queue = VecDeque::new();
        queue.push_back(start);
        visited.insert(start);
        while let Some(node) = queue.pop_front() {
            component.push(node);
            for &(nbr, _) in &neighbors[node] {
                if member_set.contains(&nbr)
                    && comm[nbr] == community
                    && visited.insert(nbr)
                {
                    queue.push_back(nbr);
                }
            }
        }
        components.push(component);
    }
    components
}

// ---------------------------------------------------------------------------
// Persist communities to graph — source: stages/stage-3c.md §4
// ---------------------------------------------------------------------------

/// Remove Community and Process nodes (and their edges) left by a prior
/// clustering pass. Both node tables use `id` as primary key, so re-running
/// `cluster_graph` on an already-clustered graph would otherwise abort with
/// a duplicate-primary-key error instead of re-clustering (bench q12 scored
/// 0.000 because the harness clusters once at setup and once per label).
fn purge_prior_clustering(store: &GraphStore) -> Result<(), String> {
    for label in ["Community", "Process"] {
        store
            .execute_query(&format!("MATCH (n:{label}) DETACH DELETE n"))
            .map_err(|e| format!("purge {label}: {e}"))?;
    }
    Ok(())
}

fn persist_communities(
    store: &GraphStore,
    adj: &Adjacency,
    comm: &[usize],
    modularity: f64,
    gamma: f64,
) -> Result<u64, String> {
    let num_comms = comm.iter().copied().max().map_or(0, |m| m + 1);

    // Count members per community
    let mut counts: HashMap<usize, u64> = HashMap::new();
    for &c in comm {
        *counts.entry(c).or_insert(0) += 1;
    }

    // Create Community nodes (bulk-insert).
    // source: Fermi audit April 2026 — was per-row CREATE, now batched.
    let community_rows: Vec<Vec<(String, String)>> = (0..num_comms)
        .map(|c| {
            let count = counts.get(&c).copied().unwrap_or(0);
            let cid = format!("community::louvain::{gamma}::{c}");
            let cid_esc = cid.replace('\'', "\\'");
            vec![
                ("id".into(), format!("'{cid_esc}'")),
                ("name".into(), format!("'community_{c}'")),
                ("algorithm".into(), "'louvain+c2'".into()),
                ("resolution_param".into(), gamma.to_string()),
                ("member_count".into(), count.to_string()),
                ("modularity_contribution".into(), format!("{:.6}", modularity)),
            ]
        })
        .collect();
    store.bulk_insert_nodes("Community", &community_rows)?;

    // Create MemberOf edges grouped per rel table.
    let mut by_rel: HashMap<String, Vec<(String, String, Vec<(String, String)>)>> =
        HashMap::new();
    for (idx, &c) in comm.iter().enumerate() {
        let node_id = &adj.node_ids[idx];
        let label = &adj.node_labels[idx];
        let cid = format!("community::louvain::{gamma}::{c}");
        let rel = format!("MemberOf_{label}_Community");
        by_rel.entry(rel).or_default().push((node_id.clone(), cid, Vec::new()));
    }
    for (rel, edges) in &by_rel {
        store.bulk_insert_edges(rel, edges)?;
    }
    Ok(num_comms as u64)
}

// ---------------------------------------------------------------------------
// Entry point: cluster_graph — source: stages/stage-3c.md §5
// ---------------------------------------------------------------------------

pub fn cluster_graph(
    store: &GraphStore,
    gamma: f64,
) -> Result<ClusteringResult, String> {
    let start = Instant::now();
    purge_prior_clustering(store)?;
    let adj = extract_adjacency(store)?;

    let (mut comm, modularity) = louvain(&adj, gamma);
    repair_c2(&adj, &mut comm);

    let communities = persist_communities(store, &adj, &comm, modularity, gamma)?;
    let processes = trace_processes(store)?;

    Ok(ClusteringResult {
        communities,
        modularity,
        processes,
        elapsed_ms: start.elapsed().as_millis() as u64,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[path = "community_tests.rs"]
mod tests;
