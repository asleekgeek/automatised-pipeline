// tool_schemas — MCP tool JSON Schema definitions.
//
// Pure data: every function returns a `serde_json::Value` containing the
// JSON-RPC tool schema. No logic, no I/O. Extracted from main.rs when
// tools_list() exceeded 200 LOC (NOTES.md growth rule).

use serde_json::{json, Value};

/// Returns the full `tools/list` response payload.
pub fn tools_list() -> Value {
    json!({
        "tools": [
            health_check_schema(),
            extract_finding_schema(),
            refine_finding_schema(),
            start_verification_schema(),
            append_clarification_schema(),
            finalize_verification_schema(),
            abort_verification_schema(),
            index_codebase_schema(),
            index_status_schema(),
            query_graph_schema(),
            get_symbol_schema(),
            resolve_graph_schema(),
            cluster_graph_schema(),
            get_processes_schema(),
            get_impact_schema(),
            index_history_schema(),
            search_codebase_schema(),
            get_context_schema(),
            analyze_codebase_schema(),
            detect_changes_schema(),
            lsp_resolve_schema(),
            prepare_prd_input_schema(),
            validate_prd_against_graph_schema(),
            check_security_gates_schema(),
            verify_semantic_diff_schema(),
        ]
    })
}

fn health_check_schema() -> Value {
    json!({
        "name": "health_check",
        "description": "Stage 0 — Healthcheck + handshake verification. Returns server identity, protocol version, and the registered stage count. Use this before calling any other stage tool to confirm the MCP is live.",
        "annotations": { "readOnlyHint": true },
        "inputSchema": {
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }
    })
}

fn extract_finding_schema() -> Value {
    json!({
        "name": "extract_finding",
        "description": "Stage 1a — Deterministic extraction. Normalizes one incoming finding (inline object or absolute path to a .json file) to the canonical schema, writes stage-1.source.json + stage-1.extracted.json atomically under <output_dir>/runs/<run_id>/findings/<finding_id>/, and creates or updates an index.json entry. Does NOT call an LLM. The caller runs the orchestrator refinement and then calls refine_finding with the payload.",
        "annotations": { "destructiveHint": true },
        "inputSchema": {
            "type": "object",
            "required": ["finding", "output_dir"],
            "additionalProperties": false,
            "properties": {
                "finding": {
                    "description": "Either an inline finding object (must match spec §3.2) or an absolute path to a .json file containing a finding or a {findings: [...]} wrapper with exactly one entry. .md paths are rejected in v1 (spec §9.3 Q1).",
                    "oneOf": [
                        { "type": "object" },
                        { "type": "string", "pattern": "^/.+\\.json$" }
                    ]
                },
                "output_dir": {
                    "type": "string",
                    "pattern": "^/.+",
                    "description": "Absolute directory where the artifact will be staged."
                },
                "run_id": {
                    "type": "string",
                    "description": "Optional run identifier. Auto-generated as YYYYMMDD-HHMMSS-<6 lowercase alphanumeric> (UTC) when absent."
                }
            }
        }
    })
}

fn refine_finding_schema() -> Value {
    json!({
        "name": "refine_finding",
        "description": "Stage 1b — Orchestrator-aware persistence. Reads an existing stage-1.extracted.json, composes stage-1.refined.json with the agent-produced refined_prompt + refinement payload, and updates index.json atomically. Pure persistence — no LLM call, no network. Requires extract_finding to have been called first for the same (run_id, finding_id).",
        "annotations": { "destructiveHint": true },
        "inputSchema": {
            "type": "object",
            "required": ["run_id", "finding_id", "output_dir", "refined_prompt", "refinement"],
            "additionalProperties": false,
            "properties": {
                "run_id":     { "type": "string" },
                "finding_id": { "type": "string" },
                "output_dir": { "type": "string", "pattern": "^/.+" },
                "refined_prompt": refined_prompt_schema(),
                "refinement": refinement_schema(),
            }
        }
    })
}

fn refined_prompt_schema() -> Value {
    json!({
        "type": "object",
        "required": ["text", "role_hint"],
        "additionalProperties": false,
        "properties": {
            "text":           { "type": "string", "minLength": 1 },
            "role_hint":      { "type": "string" },
            "token_estimate": { "type": ["integer", "null"] }
        }
    })
}

fn refinement_schema() -> Value {
    json!({
        "type": "object",
        "required": ["added_context", "orchestrator_version"],
        "additionalProperties": false,
        "properties": {
            "added_context": {
                "type": "array",
                "items": {
                    "type": "object",
                    "required": ["kind", "content"],
                    "additionalProperties": false,
                    "properties": {
                        "kind":       { "type": "string" },
                        "content":    { "type": "string" },
                        "provenance": { "type": "string" }
                    }
                }
            },
            "orchestrator_version": { "type": "string" }
        }
    })
}

fn start_verification_schema() -> Value {
    json!({
        "name": "start_verification",
        "description": "Stage 2a — Create a clarification session for a refined finding. Verifies stage-1.refined.json exists and parses (schema_ok), then atomically writes stage-2.session.json with state 'open'. Rejects if an existing session is finalized; overwrites an aborted session. No LLM call.",
        "annotations": { "destructiveHint": true },
        "inputSchema": {
            "type": "object",
            "required": ["run_id", "finding_id", "output_dir"],
            "additionalProperties": false,
            "properties": {
                "run_id":     { "type": "string" },
                "finding_id": { "type": "string" },
                "output_dir": { "type": "string", "pattern": "^/.+" }
            }
        }
    })
}

fn append_clarification_schema() -> Value {
    json!({
        "name": "append_clarification",
        "description": "Stage 2b — Append one turn (agent_question or user_answer) to stage-2.session.json. Enforces the alternation invariant (two consecutive same-kind turns rejected) and the §3 state machine. Whole-file atomic rewrite per spec §12.3. No LLM call.",
        "annotations": { "destructiveHint": true },
        "inputSchema": {
            "type": "object",
            "required": ["run_id", "finding_id", "output_dir", "kind", "content"],
            "additionalProperties": false,
            "properties": {
                "run_id":     { "type": "string" },
                "finding_id": { "type": "string" },
                "output_dir": { "type": "string", "pattern": "^/.+" },
                "kind":       { "enum": ["agent_question", "user_answer"] },
                "content":    { "type": "string", "minLength": 1 },
                "meta":       { "type": "object" }
            }
        }
    })
}

fn finalize_verification_schema() -> Value {
    json!({
        "name": "finalize_verification",
        "description": "Stage 2c — Consume the user-ready signal. Rejects from state 'open' (no_clarification_round) or 'waiting_for_user' (unanswered_question) per spec §12.2. Computes sha256 over the canonical transcript bytes, writes stage-2.verified.json atomically, flips the session to 'finalized', and updates index.json with verified+stage2_path. No LLM call.",
        "annotations": { "destructiveHint": true },
        "inputSchema": {
            "type": "object",
            "required": ["run_id", "finding_id", "output_dir"],
            "additionalProperties": false,
            "properties": {
                "run_id":     { "type": "string" },
                "finding_id": { "type": "string" },
                "output_dir": { "type": "string", "pattern": "^/.+" }
            }
        }
    })
}

fn abort_verification_schema() -> Value {
    json!({
        "name": "abort_verification",
        "description": "Stage 2d — Kill a non-terminal session. Atomically rewrites stage-2.session.json with state 'aborted', aborted_at, and optional abort_reason. Does NOT touch index.json (aborted sessions are invisible to stage 3). A fresh start_verification after abort overwrites the session.",
        "annotations": { "destructiveHint": true },
        "inputSchema": {
            "type": "object",
            "required": ["run_id", "finding_id", "output_dir"],
            "additionalProperties": false,
            "properties": {
                "run_id":     { "type": "string" },
                "finding_id": { "type": "string" },
                "output_dir": { "type": "string", "pattern": "^/.+" },
                "reason":     { "type": "string" }
            }
        }
    })
}

fn index_status_schema() -> Value {
    json!({
        "name": "index_status",
        "description": "Stage 3a — Report an indexed graph's status and its indexing-COVERAGE (issue #57): node/edge counts, files indexed, and which files the indexer could NOT fully cover — 'parse_incomplete' (WERE indexed, but tree-sitter left ERROR/MISSING line ranges, so constructs there MAY be missing from the graph — grep those ranges), 'skipped' (not indexed at all: oversized/read-failure/parse-timeout), and 'quarantined' (the parser panicked; isolated so it could not kill the index). Counts are exact; example lists are capped (full lists in the index_coverage.json sidecar). IMPORTANT: absence of a flag is NOT a completeness guarantee — the signal only marks what the indexer can detect. Use before trusting graph completeness on a file; for a structural view of the misses use query_graph(graph=\"missed\").",
        "annotations": { "readOnlyHint": true },
        "inputSchema": {
            "type": "object",
            "required": ["graph_path"],
            "additionalProperties": false,
            "properties": {
                "graph_path": {
                    "type": "string",
                    "description": "Absolute path to the graph directory (the path returned by index_codebase)."
                }
            }
        }
    })
}

fn index_codebase_schema() -> Value {
    json!({
        "name": "index_codebase",
        "description": "Stage 3a — Index a codebase. Walks the directory, parses source files with tree-sitter (Rust, Python, TypeScript), and persists a code-intelligence graph (nodes: functions, structs/classes, enums, traits/interfaces, etc.; edges: contains, defines, has_method, etc.) into a LadybugDB database at <output_dir>/graph/. Returns node/edge counts, elapsed time, and a COVERAGE report (issue #57) listing files that were parse-incomplete, skipped, or quarantined — absence of a flag is NOT a completeness guarantee; query the full report any time via index_status or query_graph(graph=\"missed\").",
        "annotations": { "destructiveHint": true },
        "inputSchema": {
            "type": "object",
            "required": ["path", "output_dir"],
            "additionalProperties": false,
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Absolute path to the codebase root to index."
                },
                "language": {
                    "type": "string",
                    "enum": ["auto", "rust", "python", "typescript", "java", "kotlin", "swift", "objc", "c", "cpp", "go"],
                    "default": "auto",
                    "description": "Language to parse. 'auto' detects per-file by extension (.rs, .py, .ts/.tsx, .java, .kt/.kts, .swift, .m/.mm, .c/.h, .cc/.cpp/.hpp, .go). Specific values restrict to that language only."
                },
                "output_dir": {
                    "type": "string",
                    "description": "Absolute directory where the graph will be stored (at <output_dir>/graph/)."
                },
                "dependency_scope": {
                    "type": "string",
                    "enum": ["none", "public_api", "full"],
                    "default": "none",
                    "description": "Tri-tier control over dependency-directory ingestion. 'none': prune build/dependency dirs (node_modules, .venv, vendor, target, dist, …); only .git is always skipped. 'public_api': descend into those dirs but persist only publicly-visible symbols from files under them — project files are still indexed in full. 'full': descend and persist everything (equivalent to the deprecated include_dependencies=true). Supersedes 'include_dependencies'; if both are given, dependency_scope wins."
                },
                "include_dependencies": {
                    "type": "boolean",
                    "default": false,
                    "description": "Deprecated — use 'dependency_scope' instead ('true' maps to 'full', 'false' maps to 'none'). Kept as a compatibility alias for one release; emits a deprecation warning."
                },
                "export_artifact": {
                    "type": "boolean",
                    "default": false,
                    "description": "Issue #55 — after a successful index, write a team-shared compressed graph snapshot to <path>/.automatised-pipeline/graph.zst (+ graph.meta.json sidecar with git sha, tool version, node/edge counts) and a .gitattributes 'merge=ours' entry so the committed blob never produces merge conflicts. Uses the best-ratio (zstd-9) tier. Export failure is logged but does not fail the index."
                },
                "bootstrap": {
                    "type": "boolean",
                    "default": false,
                    "description": "Issue #55/#62 — when there is no local graph at <output_dir>/graph but a committed artifact exists at <path>/.automatised-pipeline/graph.zst, import (decompress) that snapshot instead of cold-indexing, so a fresh clone skips the full index. Staleness is checked first: a FRESH artifact (sha == HEAD) is imported as-is (response source='artifact_bootstrap', graph_state='fresh'). A STALE artifact is, by DEFAULT, imported AND then incrementally filled up to the working tree — only the artifact→HEAD diff is re-parsed (response source='artifact_bootstrap_fill', graph_state='filled_to_working_tree', with fill_method and {changed,added,deleted,renamed,unchanged} counts). The fill derives its change set from 'git diff <artifact_sha> <working tree>' (renames included), falling back to the bundled manifest's content hashes when the repo is not a git tree. If the import or fill fails, it falls back to a full index explicitly (logged, with a 'bootstrap_skipped' note)."
                },
                "accept_stale": {
                    "type": "boolean",
                    "default": false,
                    "description": "Issue #55/#62 — only meaningful with bootstrap=true. Repurposed by the incremental-fill contract: when the committed artifact is stale, accept_stale=true imports the snapshot AS-IS and SKIPS the incremental fill (a deliberate fast path when HEAD-accuracy is not needed). The response carries graph_state='accepted_stale' and a 'stale_artifact' object {artifact_sha, head_sha, commits_behind} so the caller can never mistake the stale graph for a current one; the skipped delta is filled on the next local index_codebase run (which classifies the working tree against the bundled manifest). Leave false (the default) to bootstrap-then-fill up to the working tree."
                },
                "full": {
                    "type": "boolean",
                    "default": false,
                    "description": "Issue #62 — force a from-scratch full rebuild. By DEFAULT index_codebase is incremental: when a prior graph at <output_dir>/graph and its file_manifest.json exist, only the files that changed since the last index are re-parsed (the response carries mode='incremental' and {changed, added, deleted, renamed, unchanged} counts). Pass full=true to bypass that and rebuild everything — required when you change 'language' or 'dependency_scope', which the manifest does not capture."
                }
            }
        }
    })
}

fn query_graph_schema() -> Value {
    json!({
        "name": "query_graph",
        "description": "Stage 3a — Execute a Cypher query against an indexed code graph. The graph must have been created by a prior index_codebase call. Returns column names and rows. COVERAGE HONESTY (issue #57): pass graph=\"missed\" to instead enumerate what the index does NOT fully cover (parse-incomplete + skipped + quarantined files) so you know where to prefer grep. IMPORTANT: absence of a file from graph results — or from the 'missed' list — is NOT a completeness guarantee; the signal only marks what the indexer can detect.",
        "annotations": { "readOnlyHint": true },
        "inputSchema": {
            "type": "object",
            "required": ["graph_path"],
            "additionalProperties": false,
            "properties": {
                "graph_path": {
                    "type": "string",
                    "description": "Absolute path to the graph directory (the path returned by index_codebase)."
                },
                "graph": {
                    "type": "string",
                    "enum": ["default", "missed"],
                    "default": "default",
                    "description": "Which view to query. 'default': run the Cypher 'query' against the code graph. 'missed' (issue #57): ignore 'query' and return the coverage report — the files the index could NOT fully cover (parse_incomplete with flagged line ranges, skipped, quarantined), so an agent can pivot to grep. Absence from the missed list is NOT a completeness guarantee."
                },
                "query": {
                    "type": "string",
                    "description": "Cypher query to execute against the graph. Required unless graph=\"missed\"."
                },
                "offset": {
                    "type": "integer",
                    "minimum": 0,
                    "default": 0,
                    "description": "Number of result rows to skip before filling the byte budget (cursor pagination). Page through a large result by re-calling with offset = the previous response's next_offset until next_offset is absent. IMPORTANT: paging is only cursor-safe when your query declares an ORDER BY — the response reports order_stable; without ORDER BY the row order is unspecified and pages may skip or duplicate rows."
                }
            }
        }
    })
}

fn get_symbol_schema() -> Value {
    json!({
        "name": "get_symbol",
        "description": "Stage 3a — Look up a symbol by qualified name in the code graph. Returns the node properties plus all incoming and outgoing edges. Qualified names follow the pattern 'file_path::symbol_name' (e.g., 'src/main.rs::handle_tool_call').",
        "annotations": { "readOnlyHint": true },
        "inputSchema": {
            "type": "object",
            "required": ["graph_path", "qualified_name"],
            "additionalProperties": false,
            "properties": {
                "graph_path": {
                    "type": "string",
                    "description": "Absolute path to the graph directory."
                },
                "qualified_name": {
                    "type": "string",
                    "description": "The qualified name of the symbol to look up (e.g., 'src/main.rs::handle_tool_call')."
                },
                "sibling_graphs": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional cross-repo bridge: paths to sibling repo graph directories. When the symbol is not defined in this graph, the bridge looks for its definition in these siblings and returns repo-tagged foreign_definitions (re-query the owning repo via the returned `repo`). Omit for single-repo lookups."
                }
            }
        }
    })
}

fn resolve_graph_schema() -> Value {
    json!({
        "name": "resolve_graph",
        "description": "Stage 3b — Resolve cross-file edges in the code graph. Runs AFTER index_codebase. Adds Imports, Calls, Implements, Extends, and Uses edges by matching string references to concrete target nodes. Returns resolution statistics including edge counts and resolution rate.",
        "annotations": { "destructiveHint": true },
        "inputSchema": {
            "type": "object",
            "required": ["graph_path"],
            "additionalProperties": false,
            "properties": {
                "graph_path": {
                    "type": "string",
                    "description": "Path to the graph directory (created by index_codebase)."
                },
                "sibling_graphs": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional cross-repo bridge: paths to sibling repo graph directories. Reports cross_repo_resolvable — how many of this graph's unresolved references a sibling repo can define (a cross-service edge rather than a true third-party dependency) — plus a sample. Omit for single-repo resolution."
                }
            }
        }
    })
}

fn cluster_graph_schema() -> Value {
    json!({
        "name": "cluster_graph",
        "description": "Stage 3c — Run community detection and process tracing on an indexed+resolved graph. Groups symbols into functional communities via Louvain+C2 repair, detects entry points (main, test, handler, lib_entry), and traces BFS call chains to create Process nodes. Requires resolve_graph to have been called first.",
        "annotations": { "destructiveHint": true },
        "inputSchema": {
            "type": "object",
            "required": ["graph_path"],
            "additionalProperties": false,
            "properties": {
                "graph_path": {
                    "type": "string",
                    "description": "Path to the graph directory (created by index_codebase, resolved by resolve_graph)."
                },
                "resolution_param": {
                    "type": "number",
                    "default": 1.0,
                    "description": "Resolution parameter gamma for community detection. Higher = more, smaller communities. Default 1.0."
                }
            }
        }
    })
}

fn get_processes_schema() -> Value {
    json!({
        "name": "get_processes",
        "description": "Stage 3c — List all detected processes (execution flows from entry points). Each process has an entry point, entry kind (main/test/handler/lib_entry), BFS depth, and symbol count. Requires cluster_graph to have been called first.",
        "annotations": { "readOnlyHint": true },
        "inputSchema": {
            "type": "object",
            "required": ["graph_path"],
            "additionalProperties": false,
            "properties": {
                "graph_path": {
                    "type": "string",
                    "description": "Path to the graph directory."
                },
                "offset": {
                    "type": "integer",
                    "minimum": 0,
                    "default": 0,
                    "description": "Number of processes to skip before filling the byte budget (cursor pagination). Processes are returned in a stable total order (name, then entry_point). Page through them all by re-calling with offset = the previous response's next_offset until next_offset is absent."
                },
                "sibling_graphs": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Accepted for cross-repo API symmetry but NOT acted on: a Process is an intra-graph execution flow (BFS over one graph's call edges), and the bridge topology forbids the super-graph merge that a cross-repo process would require. To trace a flow across repos, follow get_impact's foreign_callers / get_symbol's foreign_definitions into the sibling graph and call get_processes there."
                }
            }
        }
    })
}

fn get_impact_schema() -> Value {
    json!({
        "name": "get_impact",
        "description": "Stage 3c — Blast radius analysis for a symbol. Returns the symbol's reverse dependencies — callers (reverse Calls), importers (reverse Imports), users (reverse Uses), and implementors (reverse Implements) — each as a re-queryable {id, qualified_name, label} handle you can traverse further via get_symbol/get_context/query_graph, plus the communities the symbol belongs to and the processes it participates in. Community/process fields require cluster_graph to have been called first; reverse-dependency fields work on any resolved graph.",
        "annotations": { "readOnlyHint": true },
        "inputSchema": {
            "type": "object",
            "required": ["graph_path", "qualified_name"],
            "additionalProperties": false,
            "properties": {
                "graph_path": {
                    "type": "string",
                    "description": "Path to the graph directory."
                },
                "qualified_name": {
                    "type": "string",
                    "description": "The qualified name of the symbol to analyze (e.g., 'src/main.rs::handle_tool_call')."
                },
                "offset": {
                    "type": "integer",
                    "minimum": 0,
                    "default": 0,
                    "description": "Number of CALLERS to skip before filling the byte budget (cursor pagination). 'callers' is the PRIMARY paged list, ordered by a stable total order (qualified_name, then id); page through every caller via next_offset. The other reverse-dependency lists (importers, users, implementors) are byte-capped SUMMARIES starting at index 0, not cursored (secondary_lists_paged=false) — to page one of those at scale, query it directly via query_graph with an explicit ORDER BY."
                },
                "sibling_graphs": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional cross-repo bridge: paths to sibling repo graph directories. Adds a foreign_callers section — callers of this symbol that live in OTHER repos, repo-tagged and kept separate from the local callers (and from dependents_total) so blast radius distinguishes local from cross-repo impact. Foreign callers are name-matched without a shared linker (confidence 0.50) and make the blast radius a lower bound. Omit for single-repo impact."
                }
            }
        }
    })
}

fn index_history_schema() -> Value {
    json!({
        "name": "index_history",
        "description": "History layer — ingests git commit history into an already-indexed graph as a traversable version spine (not a flat diff report). Creates Commit nodes with author/timestamp/message, PreviousVersion commit ancestry, and a Version node per (entity, commit) for every File and symbol a commit changed — linked by ChangedIn (version→commit) and VersionOf (version→entity), and chained by PreviousVersion (version→prior version). Lets a consumer walk: entity ← VersionOf ← Version → ChangedIn → Commit → PreviousVersion → Commit, and the reverse. File attribution is exact; symbol attribution maps changed lines onto the current graph's symbol ranges (best-effort on older commits). Call after index_codebase + resolve_graph on the same graph_path.",
        "annotations": { "destructiveHint": true },
        "inputSchema": {
            "type": "object",
            "required": ["graph_path", "codebase_path"],
            "additionalProperties": false,
            "properties": {
                "graph_path": {
                    "type": "string",
                    "description": "Path to the graph directory (must already be indexed)."
                },
                "codebase_path": {
                    "type": "string",
                    "description": "Path to the git working tree that was indexed. Must be inside a git repository."
                },
                "max_commits": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Maximum number of commits to walk, newest first. Defaults to 200."
                }
            }
        }
    })
}

fn search_codebase_schema() -> Value {
    json!({
        "name": "search_codebase",
        "description": "Stage 3d — Search the code graph by keyword. Returns ranked symbols with name, kind, file path, community, process participation, and relevance score. Use this to find symbols without knowing their exact qualified names. Requires index_codebase + resolve_graph + cluster_graph to have been called first (or use analyze_codebase for all-in-one).",
        "annotations": { "readOnlyHint": true },
        "inputSchema": {
            "type": "object",
            "required": ["graph_path", "query"],
            "additionalProperties": false,
            "properties": {
                "graph_path": {
                    "type": "string",
                    "description": "Path to the graph directory."
                },
                "query": {
                    "type": "string",
                    "description": "Search query — one or more keywords (e.g., 'handle_tool', 'GraphStore', 'search result')."
                },
                "limit": {
                    "type": "integer",
                    "default": 20,
                    "description": "Maximum number of ranked candidate results to consider. Bounds the universe the cursor pages within."
                },
                "offset": {
                    "type": "integer",
                    "minimum": 0,
                    "default": 0,
                    "description": "Number of ranked results to skip before filling the byte budget (cursor pagination). Results are in a stable total order: descending score, then ascending qualified_name. Page through them via next_offset until it is absent."
                },
                "label_filter": {
                    "type": "string",
                    "enum": ["Function", "Method", "Struct", "Enum", "Trait", "Module", "Constant", "TypeAlias"],
                    "description": "Optional: only return symbols of this kind."
                },
                "sibling_graphs": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional cross-repo bridge: paths to sibling repo graph directories. Federates the query across them and returns a repo-tagged foreign_results section, kept separate from the primary cursored results so local pagination stays exact. Omit for single-repo search."
                }
            }
        }
    })
}

fn get_context_schema() -> Value {
    json!({
        "name": "get_context",
        "description": "Stage 3d — 360° symbol view. Returns the symbol plus ALL its relationships grouped by kind: what it imports, what imports it, what it calls, what calls it, what it implements, what implements it, community membership, and process participation. Richer than get_symbol — use this when you need full context for PRD generation or impact analysis.",
        "annotations": { "readOnlyHint": true },
        "inputSchema": {
            "type": "object",
            "required": ["graph_path", "qualified_name"],
            "additionalProperties": false,
            "properties": {
                "graph_path": {
                    "type": "string",
                    "description": "Path to the graph directory."
                },
                "qualified_name": {
                    "type": "string",
                    "description": "The qualified name of the symbol (e.g., 'src/main.rs::handle_tool_call')."
                }
            }
        }
    })
}

fn analyze_codebase_schema() -> Value {
    json!({
        "name": "analyze_codebase",
        "description": "Stage 3 — All-in-one codebase analysis. Runs index_codebase + resolve_graph + cluster_graph in sequence, producing a fully searchable code graph in one call. Supports Rust, Python, and TypeScript (auto-detected by extension). Returns combined statistics from all phases.",
        "annotations": { "destructiveHint": true },
        "inputSchema": {
            "type": "object",
            "required": ["path", "output_dir"],
            "additionalProperties": false,
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Absolute path to the codebase root to index."
                },
                "language": {
                    "type": "string",
                    "enum": ["auto", "rust", "python", "typescript", "java", "kotlin", "swift", "objc", "c", "cpp", "go"],
                    "default": "auto",
                    "description": "Language to parse. 'auto' detects per-file by extension (.rs, .py, .ts/.tsx, .java, .kt/.kts, .swift, .m/.mm, .c/.h, .cc/.cpp/.hpp, .go). Specific values restrict to that language only."
                },
                "output_dir": {
                    "type": "string",
                    "description": "Absolute directory where the graph will be stored (at <output_dir>/graph/)."
                },
                "resolution_param": {
                    "type": "number",
                    "default": 1.0,
                    "description": "Resolution parameter for community detection. Higher = more, smaller communities."
                },
                "lsp": {
                    "type": "boolean",
                    "default": false,
                    "description": "Enable LSP-enhanced resolution after the static resolve pass. Requires the language server to be installed. Default: false."
                },
                "dependency_scope": {
                    "type": "string",
                    "enum": ["none", "public_api", "full"],
                    "default": "none",
                    "description": "Tri-tier control over dependency-directory ingestion. 'none': prune build/dependency dirs (node_modules, .venv, vendor, target, dist, …); only .git is always skipped. 'public_api': descend into those dirs but persist only publicly-visible symbols from files under them — project files are still indexed in full. 'full': descend and persist everything (equivalent to the deprecated include_dependencies=true). Supersedes 'include_dependencies'; if both are given, dependency_scope wins."
                },
                "include_dependencies": {
                    "type": "boolean",
                    "default": false,
                    "description": "Deprecated — use 'dependency_scope' instead ('true' maps to 'full', 'false' maps to 'none'). Kept as a compatibility alias for one release; emits a deprecation warning."
                }
            }
        }
    })
}

fn lsp_resolve_schema() -> Value {
    json!({
        "name": "lsp_resolve",
        "description": "Stage 3b-v2 — LSP-enhanced resolution. Queries a Language Server Protocol server (rust-analyzer, pyright, typescript-language-server) to resolve method calls on inferred types that the static resolver cannot handle. Runs AFTER resolve_graph. Requires the LSP server to be installed; gracefully fails if not found.",
        "annotations": { "destructiveHint": true },
        "inputSchema": {
            "type": "object",
            "required": ["graph_path", "codebase_path"],
            "additionalProperties": false,
            "properties": {
                "graph_path": {
                    "type": "string",
                    "description": "Path to the graph directory (created by index_codebase)."
                },
                "codebase_path": {
                    "type": "string",
                    "description": "Absolute path to the codebase root."
                },
                "language": {
                    "type": "string",
                    "enum": ["auto", "rust", "python", "typescript", "java", "kotlin", "swift", "objc", "c", "cpp", "go"],
                    "default": "auto",
                    "description": "Language for LSP server selection. 'auto' detects from file extensions."
                },
                "lsp_command": {
                    "type": "string",
                    "description": "Override the LSP server command. Default: auto-detect (rust-analyzer, pyright, typescript-language-server)."
                },
                "timeout_ms": {
                    "type": "integer",
                    "default": 30000,
                    "description": "Total timeout in milliseconds for LSP resolution."
                }
            }
        }
    })
}

fn prepare_prd_input_schema() -> Value {
    json!({
        "name": "prepare_prd_input",
        "description": "Stage 4 — Bundle graph intel (matched symbols, impacted communities, impacted processes, graph stats) into stage-4.prd_input.json for the PRD generator. Read-only against the graph. TWO modes: (1) FINDING mode — pass finding_id to bundle a VERIFIED stage-2 finding (writes under runs/<run_id>/findings/<finding_id>/ and updates index.json); (2) FEATURE mode — pass feature_description (no finding_id) to ground a free-text feature directly on the code graph, skipping the stage-2 gate (writes under runs/<run_id>/features/<slug>/). Provide finding_id OR feature_description. Grounding trust (v1.1.0, issue #14): each `matched_symbols` entry carries `match_mode` ('verbatim' — identifier cited in backticks and resolved exactly; 'exact_name' — a description word equals the symbol's name exactly) and `confidence`; only these are counted as verified grounding. Substring/fuzzy hits with no exact-identity evidence are returned separately in `candidate_symbols` (same shape, `match_mode: 'lexical'`) and are NEVER folded into matched_symbols or into impacted_communities/impacted_processes — an empty matched_symbols is expected and correct when the description contains no cited or exactly-named identifiers. Cite identifiers in backticks in finding/feature descriptions to get verbatim-priority grounding.",
        "annotations": { "destructiveHint": true },
        "inputSchema": {
            "type": "object",
            "required": ["output_dir", "graph_path"],
            "additionalProperties": false,
            "properties": {
                "run_id":              { "type": "string", "description": "Pipeline run id (path segment). Defaults to 'adhoc' in feature mode." },
                "finding_id":          { "type": "string", "description": "Finding mode: the verified stage-2 finding to bundle. Omit for feature mode." },
                "feature_description": { "type": "string", "description": "Feature mode: free-text feature/intent to ground on the graph. Used when finding_id is absent." },
                "output_dir":          { "type": "string", "pattern": "^/.+" },
                "graph_path":          { "type": "string", "pattern": "^/.+" }
            }
        }
    })
}

fn validate_prd_against_graph_schema() -> Value {
    json!({
        "name": "validate_prd_against_graph",
        "description": "Stage 6 — Validate a PRD against the resolved+clustered graph. Three axes: (1) symbol hallucination — claimed symbols that don't exist (critical); (2) community-consistency — affected symbols spanning multiple Leiden communities (warning/critical); (3) process-impact contradiction — PRD claims 'does not affect X' while a changed symbol participates in X (critical). Contract-first on stage-5.affected_symbols.json with regex fallback from the PRD markdown. LLM-free. Read-only. When run_id+finding_id+output_dir are provided, writes stage-6.validation.json under findings/<finding_id>/.",
        "annotations": { "readOnlyHint": true },
        "inputSchema": {
            "type": "object",
            "required": ["prd_path", "graph_path"],
            "additionalProperties": false,
            "properties": {
                "prd_path":    { "type": "string", "pattern": "^/.+" },
                "graph_path":  { "type": "string", "pattern": "^/.+" },
                "affected_symbols_path": { "type": "string", "pattern": "^/.+", "description": "Optional absolute path to stage-5.affected_symbols.json. When absent, regex fallback extracts claims from the PRD text." },
                "output_dir":  { "type": "string", "pattern": "^/.+", "description": "Optional; required together with run_id + finding_id to write stage-6.validation.json." },
                "run_id":      { "type": "string" },
                "finding_id":  { "type": "string" }
            }
        }
    })
}

fn check_security_gates_schema() -> Value {
    json!({
        "name": "check_security_gates",
        "description": "Stage 8 — Graph-aware security gates. Runs five checks on the changed_symbols list: S1 auth-critical community touch (critical), S2 unsafe-symbol touch (info-skip until parser records is_unsafe), S3 public-API surface change (warning), S4 unresolved-import introduction (warning/critical), S5 test-coverage structural gap (warning). Returns gates_passed=true iff zero critical flags. LLM-free. Read-only. When run_id+finding_id+output_dir are provided, writes stage-8.security.json.",
        "annotations": { "readOnlyHint": true },
        "inputSchema": {
            "type": "object",
            "required": ["graph_path", "changed_symbols"],
            "additionalProperties": false,
            "properties": {
                "graph_path":      { "type": "string", "pattern": "^/.+" },
                "changed_symbols": { "type": "array", "items": { "type": "string" }, "minItems": 0 },
                "output_dir":      { "type": "string", "pattern": "^/.+", "description": "Optional; required together with run_id + finding_id to write stage-8.security.json." },
                "run_id":          { "type": "string" },
                "finding_id":      { "type": "string" }
            }
        }
    })
}

fn verify_semantic_diff_schema() -> Value {
    json!({
        "name": "verify_semantic_diff",
        "description": "Stage 9 — Compare a post-implementation graph against a pre-implementation graph to flag regressions: nodes added/removed, edges added/removed, dangling references (edges whose target disappeared), new unresolved imports, and new strongly-connected cycles. Returns a heuristic regression_score (cap 10.0, thresholds: <1 clean, <5 concerning, >=5 regression). Read-only against both graphs.",
        "annotations": { "readOnlyHint": true },
        "inputSchema": {
            "type": "object",
            "required": ["before_graph_path", "after_graph_path"],
            "additionalProperties": false,
            "properties": {
                "before_graph_path": { "type": "string", "pattern": "^/.+" },
                "after_graph_path":  { "type": "string", "pattern": "^/.+" },
                "report_path":       { "type": "string", "pattern": "^/.+", "description": "Optional absolute path where the full report JSON is written. If absent, the report is returned inline only." }
            }
        }
    })
}

fn detect_changes_schema() -> Value {
    json!({
        "name": "detect_changes",
        "description": "Stage 3e — Git diff impact analysis. Maps changed lines to affected symbols, communities, and processes in the code graph. Accepts either raw unified diff text OR base_ref/head_ref to run git diff internally. Returns affected symbols with change type, community membership, process participation, and a heuristic risk score (0.0-1.0).",
        "annotations": { "readOnlyHint": true },
        "inputSchema": {
            "type": "object",
            "required": ["graph_path"],
            "additionalProperties": false,
            "properties": {
                "graph_path": {
                    "type": "string",
                    "description": "Path to the graph directory (created by index_codebase or analyze_codebase)."
                },
                "diff_text": {
                    "type": "string",
                    "description": "Raw unified diff text. Mutually exclusive with base_ref/head_ref."
                },
                "codebase_path": {
                    "type": "string",
                    "description": "Absolute path to the git repository. Required when using base_ref/head_ref."
                },
                "base_ref": {
                    "type": "string",
                    "default": "HEAD~1",
                    "description": "Git ref for the base (e.g., 'HEAD~1', 'main', a commit hash)."
                },
                "head_ref": {
                    "type": "string",
                    "default": "HEAD",
                    "description": "Git ref for the head (e.g., 'HEAD', a branch name)."
                }
            }
        }
    })
}
