// resolver — Stage 3b resolution pass.
//
// Reads existing nodes from the graph and adds cross-file semantic edges:
// Imports, Calls, Implements, Extends, Uses.
// Runs AFTER index_codebase (3a). Modifies the graph in-place.
//
// source: stages/stage-3b.md §4, §5

use crate::graph_store::{is_known_rel_table, GraphStore};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// Counter of edges dropped because their dynamically-formatted
/// table name doesn't appear in REL_TABLES. Producers that build
/// table names from runtime symbol labels (e.g., resolve_single_import
/// → ``Imports_File_<label>``) MUST validate against the schema or
/// risk a hard failure deep in graph_store. We surface drops via
/// eprintln! so missing labels (e.g., Method, Field, Variant) are
/// visible to the operator instead of silently degrading or aborting.
static UNKNOWN_REL_DROPS: AtomicU64 = AtomicU64::new(0);

/// Stage-3b helper: validate the dynamically-formed rel table against
/// the schema before staging. Returns true when it's safe to insert.
/// Logs the first few unknown labels at warn-equivalent level so the
/// operator can surface them as a missing-schema-entry without each
/// occurrence spamming the log.
fn check_known_rel_table(table: &str, from_id: &str, to_id: &str) -> bool {
    if is_known_rel_table(table) {
        return true;
    }
    let n = UNKNOWN_REL_DROPS.fetch_add(1, Ordering::Relaxed);
    if n < 8 {
        eprintln!(
            "resolver: dropped edge with unknown rel table '{table}' \
             ({from_id} -> {to_id}); add it to REL_TABLES in graph_store.rs"
        );
    }
    false
}

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

pub struct ResolutionResult {
    pub imports_resolved: u64,
    pub calls_resolved: u64,
    pub impls_resolved: u64,
    pub extends_resolved: u64,
    pub uses_resolved: u64,
    pub total_edges: u64,
    pub total_refs: u64,
    pub unresolved: Vec<UnresolvedRef>,
    pub elapsed_ms: u64,
}

/// Tracks a reference that could not be resolved.
/// Fields are read by downstream stages (3c/3d) and integration tests.
#[allow(dead_code)]
pub struct UnresolvedRef {
    pub kind: String,
    pub from_id: String,
    pub target_text: String,
    pub reason: String,
}

// ---------------------------------------------------------------------------
// Symbol index — in-memory map from name -> (id, label, qualified_name)
// source: stages/stage-3b.md §9 Q5 — HashMap index for O(1) lookups
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct SymbolEntry {
    id: String,
    label: String,
    qualified_name: String,
}

struct SymbolIndex {
    by_name: HashMap<String, Vec<SymbolEntry>>,
    by_qn: HashMap<String, SymbolEntry>,
    // Symbols grouped by their immediate parent module path (qualified_name
    // with the last "::segment" stripped). Populated once, O(total_symbols).
    // Lets glob-import resolution (`from x import *`) look up only the
    // symbols that live directly inside `x` instead of scanning every
    // symbol in the graph per glob import — see resolve_glob_import.
    by_parent_module: HashMap<String, Vec<SymbolEntry>>,
}

fn build_symbol_index(store: &GraphStore) -> Result<SymbolIndex, String> {
    let labels = &[
        "Function", "Method", "Struct", "Enum", "Trait",
        "Constant", "TypeAlias", "Module", "File",
    ];
    let mut by_name: HashMap<String, Vec<SymbolEntry>> = HashMap::new();
    let mut by_qn: HashMap<String, SymbolEntry> = HashMap::new();
    let mut by_parent_module: HashMap<String, Vec<SymbolEntry>> = HashMap::new();
    for label in labels {
        let qn_col = if *label == "File" { "path" } else { "qualified_name" };
        let name_col = if *label == "File" { "name" } else { "name" };
        let cypher = format!(
            "MATCH (n:{label}) RETURN n.id, n.{name_col}, n.{qn_col}"
        );
        let qr = match store.execute_query(&cypher) {
            Ok(q) => q,
            Err(_) => continue,
        };
        for row in &qr.rows {
            if row.len() < 3 {
                continue;
            }
            let entry = SymbolEntry {
                id: row[0].clone(),
                label: label.to_string(),
                qualified_name: row[2].clone(),
            };
            by_name.entry(row[1].clone()).or_default().push(entry.clone());
            if let Some((parent, _)) = row[2].rsplit_once("::") {
                by_parent_module.entry(parent.to_string()).or_default().push(entry.clone());
            }
            by_qn.insert(row[2].clone(), entry);
        }
    }
    Ok(SymbolIndex { by_name, by_qn, by_parent_module })
}

// ---------------------------------------------------------------------------
// Entry point — runs all resolution phases in order
// source: stages/stage-3b.md §4.3
// ---------------------------------------------------------------------------

pub fn resolve_graph(store: &GraphStore) -> Result<ResolutionResult, String> {
    let start = Instant::now();
    let idx = build_symbol_index(store)?;
    let file_imports = build_file_import_map(store)?;
    let existing = load_existing_edges(store)?;
    let mut buf = EdgeBuffer::new(existing);

    let (imp_resolved, imp_total, imp_unresolved) = resolve_imports(store, &idx, &mut buf)?;
    let (call_resolved, call_total, call_unresolved) =
        resolve_calls(store, &idx, &file_imports, &mut buf)?;
    let (impl_resolved, impl_total, impl_unresolved) =
        resolve_implements(store, &idx, &mut buf)?;
    let (ext_resolved, ext_total, ext_unresolved) = resolve_extends(store, &idx, &mut buf)?;
    let (uses_resolved, uses_total, uses_unresolved) =
        resolve_uses(store, &idx, &file_imports, &mut buf)?;

    // 3b-v2 Layer 4/5 — macro + stdlib expansion. Lives in resolver_layers
    // so resolver.rs's function surface stays stable for Q8 ground truth.
    // source: stages/stage-3b-v2.md §5.
    let idx_ref = &idx;
    let macro_resolved = crate::resolver_layers::run_macro_expansion(
        store,
        &mut buf,
        &|qn: &str| determine_caller_label(idx_ref, qn),
    )?;

    buf.flush(store)?;
    let call_resolved = call_resolved + macro_resolved;

    let total_edges = imp_resolved + call_resolved + impl_resolved + ext_resolved + uses_resolved;
    let total_refs = imp_total + call_total + impl_total + ext_total + uses_total;

    let mut unresolved = Vec::new();
    unresolved.extend(imp_unresolved);
    unresolved.extend(call_unresolved);
    unresolved.extend(impl_unresolved);
    unresolved.extend(ext_unresolved);
    unresolved.extend(uses_unresolved);

    Ok(ResolutionResult {
        imports_resolved: imp_resolved,
        calls_resolved: call_resolved,
        impls_resolved: impl_resolved,
        extends_resolved: ext_resolved,
        uses_resolved: uses_resolved,
        total_edges,
        total_refs,
        unresolved,
        elapsed_ms: start.elapsed().as_millis() as u64,
    })
}

// ---------------------------------------------------------------------------
// EdgeBuffer — in-memory staging area for resolution edges.
//
// Collects all resolved edges across phases, deduplicates via a HashSet of
// (rel_table, from, to) triples, and flushes grouped by rel_table through
// GraphStore::bulk_insert_edges at the end. Eliminates per-edge MATCH+CREATE
// round-trips and the idempotency sub-query that used to run before every
// insert_resolution_edge call.
// source: Fermi audit April 2026 — resolver was bottlenecked by this loop.
// ---------------------------------------------------------------------------

pub struct EdgeBuffer {
    by_table: HashMap<String, Vec<(String, String, Vec<(String, String)>)>>,
    seen: HashSet<(String, String, String)>,
}

impl EdgeBuffer {
    fn new(existing: HashSet<(String, String, String)>) -> Self {
        Self { by_table: HashMap::new(), seen: existing }
    }

    /// Stages an edge. Returns true if newly staged, false if duplicate.
    pub fn add(
        &mut self,
        rel_table: &str,
        from_id: &str,
        to_id: &str,
        confidence: f64,
        method: &str,
    ) -> bool {
        let key = (rel_table.to_string(), from_id.to_string(), to_id.to_string());
        if self.seen.contains(&key) {
            return false;
        }
        self.seen.insert(key);
        let props = vec![
            ("confidence".to_string(), confidence.to_string()),
            ("resolution_method".to_string(), format!("'{method}'")),
        ];
        self.by_table
            .entry(rel_table.to_string())
            .or_default()
            .push((from_id.to_string(), to_id.to_string(), props));
        true
    }

    fn flush(self, store: &GraphStore) -> Result<(), String> {
        // Tolerate unknown rel tables: the resolver builds rel-table
        // names dynamically from caller+target labels (e.g.
        // Calls_Method_Struct when a method calls a struct constructor),
        // but REL_TABLES only declares a subset of label pairs. Skipping
        // the unknown ones with a warning lets every valid table still
        // flush. Previously, one unknown table aborted the whole flush
        // via ``?`` and dropped every resolved edge — which is why
        // downstream consumers (Cortex viz) saw zero Calls/Imports rows.
        for (table, edges) in &self.by_table {
            match store.bulk_insert_edges(table, edges) {
                Ok(_) => {}
                Err(e) if e.contains("unknown relationship type") => {
                    eprintln!(
                        "resolver: skipping {} edges on unknown rel table {} ({})",
                        edges.len(),
                        table,
                        e,
                    );
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

/// Reads all resolution edges currently in the graph so the EdgeBuffer's
/// idempotency check works on a re-run without per-edge count(r) queries.
fn load_existing_edges(store: &GraphStore) -> Result<HashSet<(String, String, String)>, String> {
    let mut seen = HashSet::new();
    use crate::graph_store::REL_TABLES;
    for &(rel, from_label, to_label) in REL_TABLES {
        if !is_resolution_edge(rel) {
            continue;
        }
        let cypher = format!(
            "MATCH (a:{from_label})-[r:{rel}]->(b:{to_label}) RETURN a.id, b.id"
        );
        let qr = match store.execute_query(&cypher) {
            Ok(q) => q,
            Err(_) => continue,
        };
        for row in &qr.rows {
            if row.len() >= 2 {
                seen.insert((rel.to_string(), row[0].clone(), row[1].clone()));
            }
        }
    }
    Ok(seen)
}

fn is_resolution_edge(rel: &str) -> bool {
    rel.starts_with("Imports_")
        || rel.starts_with("Calls_")
        || rel.starts_with("Implements_")
        || rel.starts_with("Extends_")
        || rel.starts_with("Uses_")
}

// ---------------------------------------------------------------------------
// Phase 1: Import resolution
// source: stages/stage-3b.md §5.1
// ---------------------------------------------------------------------------

type PhaseResult = Result<(u64, u64, Vec<UnresolvedRef>), String>;

fn resolve_imports(
    store: &GraphStore,
    idx: &SymbolIndex,
    buf: &mut EdgeBuffer,
) -> PhaseResult {
    let qr = store.execute_query(
        "MATCH (i:Import) RETURN i.id, i.path, i.is_glob, i.language"
    )?;
    let mut resolved = 0u64;
    let total = qr.rows.len() as u64;
    let mut unresolved = Vec::new();
    // §10.4 — Import nodes whose target was found; is_resolved is flipped to
    // true for these (all others keep the false written at index time).
    let mut resolved_ids: Vec<&str> = Vec::new();

    for row in &qr.rows {
        if row.len() < 4 {
            continue;
        }
        let provider = crate::language_provider::provider_for(&row[3]);
        let (r, u) = resolve_one_import(idx, buf, provider, &row[0], &row[1], &row[2]);
        resolved += r;
        if r > 0 {
            resolved_ids.push(&row[0]);
        }
        unresolved.extend(u);
    }
    store.mark_nodes_resolved("Import", &resolved_ids)?;
    Ok((resolved, total, unresolved))
}

fn resolve_one_import(
    idx: &SymbolIndex,
    buf: &mut EdgeBuffer,
    provider: &dyn crate::language_provider::LanguageProvider,
    import_id: &str,
    path: &str,
    is_glob_str: &str,
) -> (u64, Vec<UnresolvedRef>) {
    if provider.is_external_import(path) {
        return (0, vec![UnresolvedRef {
            kind: "Imports".to_string(), from_id: import_id.to_string(),
            target_text: path.to_string(), reason: "external crate".to_string(),
        }]);
    }
    let file_id = extract_file_from_import_id(import_id);
    let normalized = provider.normalize_import_path(path).to_string();
    let is_glob = is_glob_str == "true" || is_glob_str == "True";

    if is_glob {
        return (resolve_glob_import(idx, buf, &file_id, &normalized), vec![]);
    }
    match resolve_single_import(idx, buf, provider, &file_id, &normalized) {
        Some(count) => (count, vec![]),
        None => (0, vec![UnresolvedRef {
            kind: "Imports".to_string(), from_id: import_id.to_string(),
            target_text: path.to_string(), reason: "no target found in graph".to_string(),
        }]),
    }
}

fn resolve_single_import(
    idx: &SymbolIndex,
    buf: &mut EdgeBuffer,
    provider: &dyn crate::language_provider::LanguageProvider,
    file_id: &str,
    normalized_path: &str,
) -> Option<u64> {
    let last_segment = provider.import_last_segment(normalized_path);
    let candidates = idx.by_name.get(last_segment)?;
    let matched = pick_best_candidate(candidates, normalized_path);
    let entry = matched?;
    let table = format!("Imports_File_{}", entry.label);
    if !check_known_rel_table(&table, file_id, &entry.id) {
        return Some(0);
    }
    let conf = compute_import_confidence(candidates.len());
    buf.add(&table, file_id, &entry.id, conf, "import-scope-lookup");
    Some(1)
}

fn resolve_glob_import(
    idx: &SymbolIndex,
    buf: &mut EdgeBuffer,
    file_id: &str,
    module_path: &str,
) -> u64 {
    // Non-termination fix (2026-07-04): this used to scan idx.by_qn (every
    // symbol in the whole graph) for EVERY glob import, i.e.
    // O(glob_imports * total_symbols). On a repo with a vendored dependency
    // tree (e.g. cortex-viz's 503MB .venv indexed via include_dependencies),
    // total_symbols is huge AND Python packages commonly re-export via
    // `from .submodule import *` inside __init__.py, so glob_imports also
    // scales with corpus size — the product is quadratic in corpus size.
    // Measured: 2_000 glob imports x 100_000 symbols took ~1.3s with the
    // linear-scan version (see bench_glob_import_scaling, "before" run,
    // commit message for the exact numbers). by_parent_module groups
    // symbols by parent qualified-name once (O(total_symbols) at index
    // build time), turning each glob import's cost into O(matches)
    // instead of O(total_symbols).
    let Some(candidates) = idx.by_parent_module.get(module_path) else {
        return 0;
    };
    let mut count = 0u64;
    for entry in candidates {
        let table = format!("Imports_File_{}", entry.label);
        if !check_known_rel_table(&table, file_id, &entry.id) {
            continue;
        }
        if buf.add(&table, file_id, &entry.id, 0.9, "import-scope-lookup") {
            count += 1;
        }
    }
    count
}

// ---------------------------------------------------------------------------
// Phase 2: Call resolution
// source: stages/stage-3b.md §5.2
// ---------------------------------------------------------------------------

fn resolve_calls(
    store: &GraphStore,
    idx: &SymbolIndex,
    file_imports: &HashMap<String, Vec<String>>,
    buf: &mut EdgeBuffer,
) -> PhaseResult {
    let qr = store.execute_query(
        "MATCH (cs:CallSite) RETURN cs.id, cs.callee_name, cs.language"
    )?;
    let mut resolved = 0u64;
    let total = qr.rows.len() as u64;
    let mut unresolved = Vec::new();
    // §10.4 — CallSite nodes whose callee was resolved to a graph target.
    let mut resolved_ids: Vec<String> = Vec::new();

    for row in &qr.rows {
        if row.len() < 3 {
            continue;
        }
        let cs_id = &row[0];
        let callee = &row[1];
        let provider = crate::language_provider::provider_for(&row[2]);
        let caller_qn = extract_caller_from_callsite_id(cs_id);
        let file_id = extract_file_from_qn(&caller_qn);
        let caller_label = determine_caller_label(idx, &caller_qn);

        match resolve_single_call(idx, provider, file_imports, callee, &file_id) {
            Some(target) => {
                // source: stages/stage-3b.md §2 — Calls is Function|Method
                // -> Function|Method only. Names resolving to Struct/Enum/
                // Trait/TypeAlias are tuple-struct ctors / enum-variant
                // calls / type uses — preserve them as Uses_* edges so the
                // full dependency chain remains queryable. Dropping them
                // would hide real dependencies from change_impact/navigate.
                let conf = if callee.contains("::") { 1.0 } else { 0.9 };
                let (rel_opt, kind) = match target.label.as_str() {
                    "Function" | "Method" => (
                        Some(format!("Calls_{}_{}", caller_label, target.label)),
                        "Calls",
                    ),
                    "Struct" | "Enum" | "Trait" | "TypeAlias"
                        if caller_label == "Function" || caller_label == "Method" =>
                    {
                        (
                            Some(format!("Uses_{}_{}", caller_label, target.label)),
                            "Uses",
                        )
                    }
                    _ => (None, "Calls"),
                };
                match rel_opt {
                    Some(rel) => {
                        if !check_known_rel_table(&rel, &caller_qn, &target.id) {
                            // Schema doesn't declare this label combination —
                            // already logged. Skip to next callee.
                        } else if buf.add(
                            &rel,
                            &caller_qn,
                            &target.id,
                            conf,
                            "import-scope-lookup",
                        ) {
                            resolved += 1;
                            // The callee resolved to a graph target — flip the
                            // CallSite's is_resolved (§10.4). Applies to both
                            // Calls and Uses edges (both mean "target found").
                            resolved_ids.push(cs_id.clone());
                        }
                    }
                    None => unresolved.push(UnresolvedRef {
                        kind: kind.to_string(),
                        from_id: cs_id.clone(),
                        target_text: callee.clone(),
                        reason: format!(
                            "no rel table for {caller_label} -> {} (callsite-as-call)",
                            target.label
                        ),
                    }),
                }
            }
            None => unresolved.push(UnresolvedRef {
                kind: "Calls".to_string(),
                from_id: cs_id.clone(),
                target_text: callee.clone(),
                reason: "no target found".to_string(),
            }),
        }
    }
    let id_refs: Vec<&str> = resolved_ids.iter().map(|s| s.as_str()).collect();
    store.mark_nodes_resolved("CallSite", &id_refs)?;
    Ok((resolved, total, unresolved))
}

fn resolve_single_call(
    idx: &SymbolIndex,
    provider: &dyn crate::language_provider::LanguageProvider,
    file_imports: &HashMap<String, Vec<String>>,
    callee: &str,
    file_id: &str,
) -> Option<SymbolEntry> {
    // Fully qualified: resolve directly. QNs are uniformly `::`-separated;
    // a callee carrying the language separator is treated as qualified.
    if callee.contains("::") || callee.contains(provider.import_separator()) {
        let last = provider.import_last_segment(callee);
        let candidates = idx.by_name.get(last)?;
        return pick_best_candidate(candidates, callee).cloned();
    }
    // Unqualified: check imports for this file
    if let Some(imports) = file_imports.get(file_id) {
        for imp_path in imports {
            let last = provider.import_last_segment(imp_path);
            if last == callee {
                if let Some(candidates) = idx.by_name.get(callee) {
                    return pick_best_candidate(candidates, imp_path).cloned();
                }
            }
        }
    }
    // Fallback: try direct name match (same-file definitions)
    let candidates = idx.by_name.get(callee)?;
    // Prefer same-file definitions
    let same_file: Vec<_> = candidates.iter()
        .filter(|e| e.qualified_name.starts_with(file_id))
        .collect();
    if same_file.len() == 1 {
        return Some(same_file[0].clone());
    }
    if candidates.len() == 1 {
        return Some(candidates[0].clone());
    }
    None
}

// ---------------------------------------------------------------------------
// Phase 3: Implements resolution
// source: stages/stage-3b.md §5.3
// ---------------------------------------------------------------------------

/// Resolves Implements edges from DECLARED facts, not method-name guesses.
/// Two sources:
///   (A) the `implements` CSV column on Struct/Enum (`#[derive(...)]` names,
///       and, for other languages, `implements`/interface clauses), and
///   (B) `impl Trait for Type` blocks, which the parser stamps onto each
///       method as `trait_name` + the receiver's QN.
///
/// source: implements fix — replaces the prior fuzzy `trait-name-match`
/// heuristic (which guessed Struct→Trait whenever a method name coincided
/// with a trait method, producing false edges and missing every declared
/// impl). Mirrors resolve_extends and finally wires the
/// macro_expansion.emit_implements table (Debug → std::fmt::Debug, …).
fn resolve_implements(
    store: &GraphStore,
    idx: &SymbolIndex,
    buf: &mut EdgeBuffer,
) -> PhaseResult {
    let mut resolved = 0u64;
    let mut total = 0u64;
    let mut unresolved = Vec::new();
    let mut created_stdlib: HashSet<String> = HashSet::new();

    // (A) Declared/derived trait names on Struct/Enum.
    for label in &["Struct", "Enum"] {
        let q = format!("MATCH (s:{label}) RETURN s.id, s.implements, s.language");
        let qr = match store.execute_query(&q) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for row in &qr.rows {
            if row.len() < 3 {
                continue;
            }
            let from_id = &row[0];
            let csv = &row[1];
            let provider = crate::language_provider::provider_for(&row[2]);
            if csv.is_empty() || csv == "Null(String)" {
                continue;
            }
            for raw in csv.split(',') {
                let name = raw.trim();
                if name.is_empty() {
                    continue;
                }
                total += 1;
                if resolve_one_implements(
                    store, idx, buf, provider, label, from_id, name, &mut created_stdlib,
                )? {
                    resolved += 1;
                } else {
                    unresolved.push(UnresolvedRef {
                        kind: "Implements".to_string(),
                        from_id: from_id.clone(),
                        target_text: name.to_string(),
                        reason: "no_target_in_corpus".to_string(),
                    });
                }
            }
        }
    }

    // (B) `impl Trait for Type` blocks.
    let (b_res, b_total, b_unresolved) = resolve_impl_trait_blocks(store, idx, buf)?;
    resolved += b_res;
    total += b_total;
    unresolved.extend(b_unresolved);

    Ok((resolved, total, unresolved))
}

/// Resolve one implemented-trait NAME for a Struct/Enum. Prefers a local Trait
/// in the corpus (`Implements_<Label>_Trait`, confidence 0.95); otherwise a
/// stdlib trait via the derive macro table (`Implements_<Label>_StdlibSymbol`,
/// 0.9). Returns true if at least one edge was newly staged.
fn resolve_one_implements(
    store: &GraphStore,
    idx: &SymbolIndex,
    buf: &mut EdgeBuffer,
    provider: &dyn crate::language_provider::LanguageProvider,
    label: &str,
    from_id: &str,
    name: &str,
    created_stdlib: &mut HashSet<String>,
) -> Result<bool, String> {
    let lookup = provider.import_last_segment(name);
    if let Some(t) = idx
        .by_name
        .get(lookup)
        .and_then(|c| c.iter().find(|e| e.label == "Trait"))
    {
        let table = format!("Implements_{label}_Trait");
        return Ok(buf.add(&table, from_id, &t.id, 0.95, "declared-implements"));
    }
    // Derive/decorator → stdlib-trait expansion, only for languages that
    // declare such a table (Rust derives). `None` → no fabricated edges.
    let macro_key = match provider.derive_macro_key() {
        Some(k) => k,
        None => return Ok(false),
    };
    if let Some(exp) = crate::macro_expansion::lookup(macro_key, &format!("derive_{name}")) {
        let mut any = false;
        let table = format!("Implements_{label}_StdlibSymbol");
        for canonical in exp.emit_implements {
            crate::resolver_layers::ensure_stdlib_symbol(
                store, created_stdlib, canonical, "rust",
            )?;
            if buf.add(&table, from_id, canonical, 0.9, "derive-macro") {
                any = true;
            }
        }
        return Ok(any);
    }
    Ok(false)
}

/// Resolve `impl Trait for Type` blocks. The parser stamps each method in such
/// a block with `trait_name` and the receiver's QN; we emit one
/// `Implements_<Label>_Trait` edge per block. buf.add collapses the repeated
/// methods of a block to a single edge.
fn resolve_impl_trait_blocks(
    store: &GraphStore,
    idx: &SymbolIndex,
    buf: &mut EdgeBuffer,
) -> PhaseResult {
    let mut resolved = 0u64;
    let mut total = 0u64;
    let mut unresolved = Vec::new();
    let qr = store.execute_query(
        "MATCH (m:Method) WHERE m.trait_name <> '' AND m.receiver_type <> '' \
         RETURN m.receiver_type, m.trait_name",
    )?;
    for row in &qr.rows {
        if row.len() < 2 {
            continue;
        }
        let receiver_qn = row[0].trim();
        let trait_name = row[1].trim();
        if receiver_qn.is_empty() || trait_name.is_empty() {
            continue;
        }
        let recv = match idx.by_qn.get(receiver_qn) {
            Some(e) if e.label == "Struct" || e.label == "Enum" => e,
            _ => continue,
        };
        total += 1;
        let lookup = trait_name.rsplit("::").next().unwrap_or(trait_name);
        match idx
            .by_name
            .get(lookup)
            .and_then(|c| c.iter().find(|e| e.label == "Trait"))
        {
            Some(t) => {
                let table = format!("Implements_{}_Trait", recv.label);
                if buf.add(&table, &recv.id, &t.id, 0.95, "impl-block") {
                    resolved += 1;
                }
            }
            None => unresolved.push(UnresolvedRef {
                kind: "Implements".to_string(),
                from_id: recv.id.clone(),
                target_text: trait_name.to_string(),
                reason: "no_target_in_corpus".to_string(),
            }),
        }
    }
    Ok((resolved, total, unresolved))
}

// ---------------------------------------------------------------------------
// Phase 4: Extends resolution (supertrait)
// source: stages/stage-3b.md §5.4
// ---------------------------------------------------------------------------

/// Resolves the `bases` CSV property on every Struct/Enum/Trait node, looks
/// each base name up in the symbol index, and emits the matching
/// Extends_X_Y edge. Names that resolve to an Import node (cross-file base)
/// are left unresolved — multi-file indexing surfaces those naturally.
///
/// source: Spike B' BUG #9 fix — previously a no-op stub that just counted
/// pre-existing edges. The parser writes bases to the node property; this
/// function does the deferred name→QN resolution that the indexer can't
/// perform (the indexer routes by labels via label_by_qn, but base names
/// aren't QNs yet at insert time).
fn resolve_extends(store: &GraphStore, idx: &SymbolIndex, buf: &mut EdgeBuffer) -> PhaseResult {
    let mut resolved = 0u64;
    let mut total = 0u64;
    let mut unresolved = Vec::new();

    for &(label, table_self) in &[
        ("Struct", "Extends_Struct_Struct"),
        ("Enum", "Extends_Enum_Enum"),
        ("Trait", "Extends_Trait_Trait"),
    ] {
        let q = format!("MATCH (s:{label}) RETURN s.id, s.bases");
        let qr = match store.execute_query(&q) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for row in &qr.rows {
            if row.len() < 2 {
                continue;
            }
            let child_qn = &row[0];
            let bases_csv = &row[1];
            if bases_csv.is_empty() || bases_csv == "Null(String)" {
                continue;
            }
            for raw_base in bases_csv.split(',') {
                let raw_base = raw_base.trim();
                if raw_base.is_empty() {
                    continue;
                }
                total += 1;
                if resolve_one_extends_base(
                    idx, buf, label, table_self, child_qn, raw_base, &mut unresolved,
                ) {
                    resolved += 1;
                }
            }
        }
    }

    Ok((resolved, total, unresolved))
}

/// Resolves one base-class NAME from a Struct/Enum/Trait's `bases` CSV entry
/// and stages the matching `Extends_<label>_<target-label>` edge via
/// EdgeBuffer. Returns true iff an edge was staged.
/// Extracted from `resolve_extends` to keep that function under the §4.2
/// size limit; behavior is identical to the pre-extraction inline loop body.
fn resolve_one_extends_base(
    idx: &SymbolIndex,
    buf: &mut EdgeBuffer,
    label: &str,
    table_self: &str,
    child_qn: &str,
    raw_base: &str,
    unresolved: &mut Vec<UnresolvedRef>,
) -> bool {
    // Look up by last `.`-separated segment so `typing.NamedTuple` resolves
    // on `NamedTuple` if present. Cortex uses `::` in QNs but base names came
    // from source so they may carry `.`.
    let lookup = raw_base.rsplit('.').next().unwrap_or(raw_base);
    let candidates = match idx.by_name.get(lookup) {
        Some(v) => v,
        None => {
            unresolved.push(UnresolvedRef {
                kind: "Extends".to_string(),
                from_id: child_qn.to_string(),
                target_text: raw_base.to_string(),
                reason: "no_target_in_corpus".to_string(),
            });
            return false;
        }
    };
    // Prefer same-label matches (Struct→Struct, etc.) over cross-label
    // (Struct→Trait) for symmetry with self_table.
    let preferred = candidates.iter().find(|c| c.label == label);
    let target = match preferred.or_else(|| candidates.first()) {
        Some(t) => t,
        None => return false,
    };
    let rel_table = if target.label == label {
        table_self.to_string()
    } else {
        format!("Extends_{label}_{}", target.label)
    };
    if !crate::graph_store::is_known_rel_table(&rel_table) {
        unresolved.push(UnresolvedRef {
            kind: "Extends".to_string(),
            from_id: child_qn.to_string(),
            target_text: raw_base.to_string(),
            reason: format!("no_rel_table_for_{label}_to_{}", target.label),
        });
        return false;
    }
    // Staged through EdgeBuffer like every other resolution phase
    // (bulk_insert_edges at flush) instead of one insert_edge per base.
    // 0.95/"declared-extends" mirrors resolve_one_implements's 0.95/
    // "declared-implements" for the parallel `implements` CSV.
    // source: ADR-4253701 §Decision 2 (levier 2, resolver.rs resolve_extends).
    buf.add(&rel_table, child_qn, &target.id, 0.95, "declared-extends");
    true
}

// ---------------------------------------------------------------------------
// Phase 5: Uses (type-usage) resolution
// source: stages/stage-3b.md §5.5
// ---------------------------------------------------------------------------

fn resolve_uses(
    store: &GraphStore,
    idx: &SymbolIndex,
    file_imports: &HashMap<String, Vec<String>>,
    buf: &mut EdgeBuffer,
) -> PhaseResult {
    let mut resolved = 0u64;
    let mut total = 0u64;
    let mut unresolved = Vec::new();

    // Resolve field type annotations -> Struct/Enum/Trait
    let field_result = resolve_field_type_uses(store, idx, file_imports, buf)?;
    resolved += field_result.0;
    total += field_result.1;
    unresolved.extend(field_result.2);

    Ok((resolved, total, unresolved))
}

fn resolve_field_type_uses(
    store: &GraphStore,
    idx: &SymbolIndex,
    _file_imports: &HashMap<String, Vec<String>>,
    buf: &mut EdgeBuffer,
) -> PhaseResult {
    let qr = store.execute_query(
        "MATCH (f:Field) RETURN f.id, f.type_annotation, f.language"
    )?;
    let mut resolved = 0u64;
    let total = qr.rows.len() as u64;
    let mut unresolved = Vec::new();

    for row in &qr.rows {
        if row.len() < 3 {
            continue;
        }
        let field_id = &row[0];
        let type_ann = &row[1];
        let provider = crate::language_provider::provider_for(&row[2]);
        let type_names = extract_type_identifiers(type_ann, provider.primitives());

        for type_name in &type_names {
            if let Some(target) = find_type_target(idx, type_name) {
                let table = format!("Uses_Field_{}", target.label);
                if !check_known_rel_table(&table, field_id, &target.id) {
                    continue;
                }
                if buf.add(&table, field_id, &target.id, 0.9, "type-annotation-parse") {
                    resolved += 1;
                }
            } else {
                unresolved.push(UnresolvedRef {
                    kind: "Uses".to_string(),
                    from_id: field_id.clone(),
                    target_text: type_name.clone(),
                    reason: "type not found in graph".to_string(),
                });
            }
        }
    }
    Ok((resolved, total, unresolved))
}

/// Extract type identifiers from a type annotation string.
/// Strips generics, references, lifetimes, and finds nominal types. The
/// `primitives` skip-set is language-specific (provided by LanguageProvider);
/// lowercase builtins are additionally skipped by the uppercase-identifier
/// convention below.
fn extract_type_identifiers(type_ann: &str, primitives: &[&str]) -> Vec<String> {
    let mut result = Vec::new();
    let cleaned = type_ann
        .replace('&', " ")
        .replace('*', " ")
        .replace('<', " ")
        .replace('>', " ")
        .replace(',', " ")
        .replace('(', " ")
        .replace(')', " ")
        .replace('[', " ")
        .replace(']', " ");
    for token in cleaned.split_whitespace() {
        // Skip lifetimes, keywords, primitives
        if token.starts_with('\'') || token == "mut" || token == "dyn" || token == "impl" {
            continue;
        }
        if primitives.contains(&token) {
            continue;
        }
        // Must start with uppercase to be a type name (convention)
        if token.chars().next().map_or(false, |c| c.is_uppercase()) {
            result.push(token.to_string());
        }
    }
    result
}

fn find_type_target<'a>(idx: &'a SymbolIndex, type_name: &str) -> Option<&'a SymbolEntry> {
    let candidates = idx.by_name.get(type_name)?;
    // Filter to type-like labels only
    let types: Vec<_> = candidates.iter()
        .filter(|e| matches!(e.label.as_str(), "Struct" | "Enum" | "Trait" | "TypeAlias"))
        .collect();
    if types.len() == 1 {
        return Some(types[0]);
    }
    if types.is_empty() {
        return None;
    }
    // Ambiguous: return first match with lower confidence (handled by caller)
    Some(types[0])
}

// ---------------------------------------------------------------------------
// File-import map: file_id -> [import paths]
// ---------------------------------------------------------------------------

fn build_file_import_map(store: &GraphStore) -> Result<HashMap<String, Vec<String>>, String> {
    let qr = store.execute_query("MATCH (i:Import) RETURN i.id, i.path")?;
    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    for row in &qr.rows {
        if row.len() < 2 {
            continue;
        }
        let file_id = extract_file_from_import_id(&row[0]);
        map.entry(file_id).or_default().push(row[1].clone());
    }
    Ok(map)
}

// ---------------------------------------------------------------------------
// Path normalization helpers
// ---------------------------------------------------------------------------

fn extract_file_from_import_id(import_id: &str) -> String {
    // Import IDs have format: "file_path::import_display_name". The file path
    // is recognized by its extension across ALL supported languages (the
    // extension set is disjoint enough that no per-node language is needed).
    // source: language_provider::ALL_EXTENSIONS (= parser::Language::from_extension).
    crate::language_provider::extract_file_prefix(import_id)
        .unwrap_or_else(|| import_id.to_string())
}

fn extract_file_from_qn(qn: &str) -> String {
    crate::language_provider::extract_file_prefix(qn).unwrap_or_else(|| qn.to_string())
}

pub(crate) fn extract_caller_from_callsite_id(cs_id: &str) -> String {
    // CallSite IDs: "caller_qn::call@line:col"
    if let Some(idx) = cs_id.rfind("::call@") {
        cs_id[..idx].to_string()
    } else {
        cs_id.to_string()
    }
}

fn determine_caller_label(idx: &SymbolIndex, caller_qn: &str) -> String {
    idx.by_qn.get(caller_qn)
        .map(|e| e.label.clone())
        .unwrap_or_else(|| "Function".to_string())
}

fn pick_best_candidate<'a>(
    candidates: &'a [SymbolEntry],
    path_hint: &str,
) -> Option<&'a SymbolEntry> {
    if candidates.is_empty() {
        return None;
    }
    if candidates.len() == 1 {
        return Some(&candidates[0]);
    }
    // Try matching the path hint against qualified names
    let last_segments = path_hint.replace("::", "/");
    for c in candidates {
        let c_segments = c.qualified_name.replace("::", "/");
        if c_segments.ends_with(&last_segments) {
            return Some(c);
        }
    }
    // Fallback: return first candidate
    Some(&candidates[0])
}

/// Confidence: 1.0 for unique match, 0.7 for ambiguous.
/// source: stages/stage-3b.md §2 "Confidence assignment rules"
fn compute_import_confidence(candidate_count: usize) -> f64 {
    if candidate_count == 1 { 1.0 } else { 0.7 }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_type_identifiers() {
        // Use the Rust primitive set (matches the prior module-level PRIMITIVES).
        let prims = crate::language_provider::provider_for("rust").primitives();
        let cases = vec![
            ("String", vec![]),           // primitive
            ("GraphStore", vec!["GraphStore"]),
            ("Vec<GraphStore>", vec!["GraphStore"]),
            ("&'a MyType", vec!["MyType"]),
            ("Option<Result<Foo, Bar>>", vec!["Foo", "Bar"]),
            ("i32", vec![]),
            ("HashMap<String, Value>", vec!["Value"]),
        ];
        for (input, expected) in cases {
            let result = extract_type_identifiers(input, prims);
            assert_eq!(result, expected, "for input: {input}");
        }
    }

    #[test]
    fn test_normalize_import_path_via_provider() {
        // normalize_import_path moved to LanguageProvider (Rust strips crate::).
        let rust = crate::language_provider::provider_for("rust");
        assert_eq!(rust.normalize_import_path("crate::graph_store::GraphStore"), "graph_store::GraphStore");
        assert_eq!(rust.normalize_import_path("std::io"), "std::io");
        assert_eq!(rust.normalize_import_path("self::foo"), "self::foo");
    }

    #[test]
    fn test_is_external_via_provider() {
        // is_external_crate moved to LanguageProvider::is_external_import.
        let rust = crate::language_provider::provider_for("rust");
        assert!(rust.is_external_import("std::io"));
        assert!(rust.is_external_import("serde::Serialize"));
        assert!(!rust.is_external_import("crate::graph_store"));
        assert!(!rust.is_external_import("self::foo"));
        assert!(!rust.is_external_import("super::bar"));
    }

    #[test]
    fn test_extract_file_from_import_id() {
        assert_eq!(
            extract_file_from_import_id("src/main.rs::graph_store::GraphStore"),
            "src/main.rs"
        );
    }

    #[test]
    fn test_extract_caller_from_callsite_id() {
        assert_eq!(
            extract_caller_from_callsite_id("src/main.rs::main::call@5:4"),
            "src/main.rs::main"
        );
    }

    // -----------------------------------------------------------------
    // Non-termination reproduction (2026-07-04) — glob-import resolution.
    //
    // resolve_glob_import scanned idx.by_qn (ALL symbols in the graph)
    // once per glob import. On a repo with a vendored dependency tree
    // (e.g. a .venv), by_qn holds every vendored symbol too, and Python
    // packages commonly re-export via `from .submodule import *` inside
    // __init__.py — so glob-import count scales with the number of
    // vendored packages/files. Cost was O(glob_imports * total_symbols):
    // quadratic in corpus size, not just "large but linear". This test
    // builds a synthetic index (M modules x K symbols each = total
    // symbols) and N glob imports (one per module) and measures wall
    // time. Run manually (ignored by default — timing, not CI-stable):
    //   cargo test --release resolver::tests::bench_glob_import_scaling -- --ignored --nocapture
    // source: measured on 2026-07-04 in this environment (Apple Silicon,
    // `cargo test --release`), numbers reported in the commit message.
    #[test]
    #[ignore]
    fn bench_glob_import_scaling() {
        fn build_index(modules: usize, symbols_per_module: usize) -> SymbolIndex {
            let mut by_name: HashMap<String, Vec<SymbolEntry>> = HashMap::new();
            let mut by_qn: HashMap<String, SymbolEntry> = HashMap::new();
            let mut by_parent_module: HashMap<String, Vec<SymbolEntry>> = HashMap::new();
            for m in 0..modules {
                let module_path = format!("pkg{m}");
                for s in 0..symbols_per_module {
                    let qn = format!("{module_path}::sym{s}");
                    let entry = SymbolEntry {
                        id: format!("file{m}.py::sym{s}"),
                        label: "Function".to_string(),
                        qualified_name: qn.clone(),
                    };
                    by_name.entry(format!("sym{s}")).or_default().push(entry.clone());
                    by_qn.insert(qn.clone(), entry.clone());
                    by_parent_module.entry(module_path.clone()).or_default().push(entry);
                }
            }
            SymbolIndex { by_name, by_qn, by_parent_module }
        }

        let modules = 2_000;
        let symbols_per_module = 50; // total_symbols = 100_000
        let idx = build_index(modules, symbols_per_module);
        let existing: HashSet<(String, String, String)> = HashSet::new();
        let mut buf = EdgeBuffer::new(existing);

        let start = Instant::now();
        let mut total_edges = 0u64;
        for m in 0..modules {
            let file_id = format!("caller{m}.py");
            let module_path = format!("pkg{m}");
            total_edges += resolve_glob_import(&idx, &mut buf, &file_id, &module_path);
        }
        let elapsed = start.elapsed();
        println!(
            "glob-import scaling: modules={modules} symbols/module={symbols_per_module} \
             total_symbols={} glob_imports={modules} edges_generated={total_edges} elapsed={elapsed:?}",
            modules * symbols_per_module
        );
        assert_eq!(total_edges as usize, modules * symbols_per_module);
    }
}
