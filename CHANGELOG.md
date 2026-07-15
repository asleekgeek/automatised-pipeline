# Changelog

All notable changes to this project will be documented here. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Fixed

- **`prepare_prd_input` now uses the hybrid BM25/vector search index when one
  exists, instead of always running the substring-only fallback scorer
  (issue #18).** `search_and_classify` (`src/prd_input/matching.rs`)
  unconditionally passed `index_dir: None` to `search::search_graph`
  regardless of whether `analyze_codebase` had already built a
  `search_index/` next to the graph — Stage 4 recall was capped at the
  weakest matcher unconditionally, even when Stage 3d's `search_codebase`
  on the same graph used the real hybrid index. Fix: extracted
  `search::resolve_search_index_dir` (the sibling-`search_index/`-directory
  logic previously inlined in `do_search_codebase`, `src/main.rs`) as the
  single source of truth for resolving a graph's index directory, and
  `prepare_prd_input` now calls it and threads the result through
  `search_and_classify` → `search_hits`. When no index exists, the fallback
  to substring search is now explicit and always logged
  (`eprintln!("[ap] prepare_prd_input: no search_index found ...")`) —
  never silent. Measured on a fixed fixture (`src/prd_input/matching_tests.rs::
  test_issue18_hybrid_index_reduces_spurious_candidates`): substring
  fallback (pre-fix behavior) surfaced 2 spurious `candidate_symbols` from
  unrelated filler words in the description; the hybrid index (post-fix)
  surfaced 1 on the identical graph and description — source: measured on
  2026-07-15, that test's fixture. The 2→1 count is specific to that small
  fixture, not a guaranteed reduction ratio for arbitrary descriptions/graphs
  — the regression test itself asserts `before >= after`, not a fixed delta.

### Changed — `prepare_prd_input` tool schema, `preparer_version` 1.1.0 → 1.2.0

Additive only. `prd_context` gains a new `search_backend` field —
`"hybrid"` when the search index was found and used, `"substring_fallback"`
when none was found — so consumers can see which scorer produced
`matched_symbols`/`candidate_symbols` for a given run.

- **`prepare_prd_input` (feature mode) no longer presents lexical substring
  matches as verified grounding (issue #14).** The matcher ran every
  natural-language word from the description through the graph search with
  `min_score: 0.0` and folded every hit into `matched_symbols` next to a
  bundle-level `verified: true`, so an accidental substring collision (e.g.
  the word "anchor" hitting `_CONCRETE_ANCHOR`) was indistinguishable from a
  real identifier reference. Measured proof: a genuine partial-word hit and
  a false-positive substring hit score IDENTICALLY under the existing scoring
  formula at equal substring-to-name ratio, so no score threshold can tell
  them apart — the fix classifies every hit's `match_mode` (verbatim exact
  citation / exact name match / lexical-only) instead.

### Changed — `prepare_prd_input` tool schema, `preparer_version` 1.0.0 → 1.1.0

**Consumed by `prd-spec-generator` — read this before bumping the pinned AP
version.** All changes are additive to the JSON shape; the semantic change
below is the one to check for in integrating code.

- **`matched_symbols` semantics changed: it can now be empty where it
  previously would not have been.** A description with only lexical
  (non-exact) word overlap against the graph now yields `matched_symbols:
  []` rather than a list of unverified guesses — an empty array is the
  correct, expected output when nothing can be verified, not a bug or a
  sign the pipeline failed. Any consumer that treated a non-empty
  `matched_symbols` as a given must handle the empty case.
- **New per-symbol fields** on every `matched_symbols` entry: `match_mode`
  (`"verbatim"` — identifier cited in the description in backticks and
  resolved exactly; `"exact_name"` — a description word equals the symbol's
  name/qualified-name tail exactly) and `confidence` (the raw search score;
  informational only — trust is carried by `match_mode`, not this score).
- **New `candidate_symbols` array** (`prd_context.candidate_symbols`, same
  shape as `matched_symbols` plus `match_mode: "lexical"`): substring/fuzzy
  hits with no exact-identity evidence. Never folded into `matched_symbols`
  or into `impacted_communities`/`impacted_processes`. Exposed for
  visibility only — do not treat as verified.
- **New `candidate_symbol_count`** field on the `prepare_prd_input` tool
  response, alongside the existing `matched_symbol_count`.
- Cite identifiers in backticks in finding/feature descriptions to get
  verbatim-priority grounding — this is now the reliable way to guarantee a
  specific symbol appears in `matched_symbols`.

## [0.5.0] — Cross-repo bridge: link per-repo graphs at query time

First tagged release since v0.2.2; folds in the untagged 0.3.0 and 0.4.0 work
(those bumped Cargo.toml but were never tagged, so no binaries shipped).

### Added

- **Cross-repo bridge (`src/bridge.rs`).** Links separate per-repo property
  graphs at QUERY TIME via a caller-supplied `sibling_graphs` argument — no
  super-graph merge, no re-index. A reference that dangles in repo A (no local
  definition) is resolved against registered sibling graphs on demand.
  - `resolve_definition` (forward): an unresolved local ref → its definition in
    a sibling repo. Surfaces in `get_symbol`'s miss path as repo-tagged
    `foreign_definitions`.
  - `foreign_callers` (reverse): sibling call sites of a local symbol. Homonym-
    safe — a sibling that locally defines the same short name is skipped, so a
    local call is never mis-reported as cross-repo. Surfaces in `get_impact` as
    a `foreign_callers` section kept distinct from local blast radius, flipping
    the epistemic boundary to lower-bound (name-matched, confidence 0.50).
  - `resolve_graph` reports `cross_repo_resolvable` (how many unresolved refs a
    sibling can define) + a sample.
  - `search_codebase` federates the query across siblings into a bounded,
    repo-tagged `foreign_results` section; the primary cursor stays exact.
  - Optional `sibling_graphs` arg added to all five tool schemas; absent → no-op
    (fully backward compatible). `get_processes` accepts it for API symmetry but
    is documented as not-acted-on (intra-graph by construction; cross-repo would
    require the forbidden super-graph).
- **(0.4.0) Cursor pagination on all bounded reads** — truncation becomes
  pacing across `get_processes`, `get_impact`, `search_codebase`.
- **(0.3.0) Bounded-io** — byte-budgeted MCP responses + read-path graph cache.
- **(0.3.0–0.4.0) Multi-language resolver** — `LanguageProvider` trait lights up
  7 dormant grammars (C/C++/Go/Java/Kotlin/ObjC/Swift); process-grouped search
  via an additive `by_process` index.

## [0.2.2] — Remove the search-index env-var channel (flaky-test root cause)

### Fixed

- **Root-caused the `stage3d_hybrid_search` flake.** v0.2.1 serialized the
  tests with a mutex — a band-aid. The structural cause was that
  `do_search_codebase` passed the search-index directory to
  `search::search_graph` through the PROCESS-GLOBAL env var
  `AA_SEARCH_INDEX_DIR`, a hidden channel that races across any parallel
  callers (and was wiped+rebuilt mid-read → tantivy `FileDoesNotExist`).
  `search_graph` now takes `index_dir: Option<&Path>` as an explicit
  parameter; the env var and `find_search_index_dir` are deleted. The test
  mutex is removed — the four tests run fully parallel, each passing its own
  index dir (verified 3× green). source: dijkstra root-cause audit.

## [0.2.1] — Release hygiene + flaky-test fix

### Fixed

- **CI flake in `stage3d_hybrid_search`.** The four hybrid-search tests share
  the process-global `AA_SEARCH_INDEX_DIR` env var; cargo runs them on parallel
  threads, so they stomped each other's index path and `build_search_index`
  wiped a dir mid-read, producing a tantivy `FileDoesNotExist` on the BM25
  store (CI run 26824494088). Serialized the four tests with a shared mutex
  held for each test's duration — deterministic, no new dependency.
- **Version consistency.** `Cargo.toml`, `.claude-plugin/plugin.json`, and
  both `.claude-plugin/marketplace.json` fields are now all `0.2.1` (the 0.2.0
  release shipped with `plugin.json`/`marketplace.json` lagging). `SERVER_VERSION`
  derives from `CARGO_PKG_VERSION`, so the MCP handshake follows automatically.

## [0.2.0] — All-file indexing

### Added

- **The indexer now indexes ANY file type, not just the tree-sitter language
  set.** Previously `collect_source_files` dropped every file whose extension
  had no parser (`.js`, `.md`, `.json`, `.css`, `.html`, `.txt`, `.pdf`,
  `.docx`, …), so a session touching those files had nothing to navigate to.
  Now, when no language filter is given, the walker collects every file and
  each becomes a `File` node (path / name / extension / size) — binary
  documents included (metadata only; content is never read for them, so
  `.pdf`/`.docx` are safe). Build/dependency dirs are still pruned and a
  language-scoped re-index (`language_filter = Some(L)`) is unchanged.
- **Light cross-file linking for non-AST files** (`src/indexer/light_link.rs`),
  run as a forward-reference-safe post-pass once every `File` node exists:
  - JavaScript family (`.js/.jsx/.mjs/.cjs`): relative `import … from "X"`,
    `require("X")`, dynamic `import("X")` → `Imports_File_File` (Node-style
    suffix resolution).
  - Markdown (`.md/.markdown/.mdx`): inline links `[text](path)` → new
    `References_File_File` edge (doc→file reference), resolved relative to the
    doc and repo-root. External URLs / anchors / absolute paths are dropped.

### Schema

- New `References_File_File` rel table (resolution rel: `confidence`,
  `resolution_method`).

### Tests

- `test_all_file_indexing_documents_and_links`: indexes code + JS + Markdown +
  JSON + txt + binary `.pdf`/`.docx`; asserts all 9 become `File` nodes and
  that Markdown References + JS Imports resolve.

### Fixed

- **Java `implements` and `extends` produced no graph edges.** The Java parser
  emitted them only as `ExtractedRef`s, which the indexer drops, and never
  populated the `bases` / `implements` node columns the resolver reads — so
  `resolve_extends` / `resolve_implements` had nothing to work from. The parser
  now writes both columns (mirroring `parser/rust.rs`). Additionally, the
  interface-name extraction iterated the `super_interfaces` node's direct
  children and so never found the type identifiers (they sit one level down in
  a `type_list`); `extract_interfaces` now descends into the `type_list`. Java
  `class Dog extends Animal implements Greeter` now yields `Extends_Struct_Struct`
  and `Implements_Struct_Trait` edges.

## [0.1.0] — History layer, declared-implements resolution, indexer batching, all-direction get_impact

### Added

- **Code-history temporal layer.** New `Commit` and `Version` node tables plus
  `PreviousVersion` (commit ancestry + per-entity version chain), `ChangedIn`
  (version→commit) and `VersionOf` (version→File/Function/Method/Struct/Enum/
  Trait) relationship tables. A new `index_history` MCP tool walks `git log`,
  persists commit metadata + ancestry, then records a `Version` per (entity,
  commit) for every File and symbol a commit changed. The graph is now
  traversable across time in both directions:
  `entity ← VersionOf ← Version → ChangedIn → Commit → PreviousVersion → Commit`.
  File attribution is exact; symbol attribution maps changed lines onto the
  current graph's symbol ranges. Implemented in `src/history/`.
- **Declared `Implements` resolution.** New `implements` column on Struct/Enum
  (derived/declared trait names) and `trait_name` column on Method (the trait
  of an `impl Trait for Type` block — already extracted by the parser but
  previously dropped for lack of a column). `resolve_implements` now resolves
  these **declared facts** — to a local `Trait` (`Implements_*_Trait`) or, for
  `#[derive(...)]`, to a stdlib trait via the macro-expansion table
  (`Implements_*_StdlibSymbol`, e.g. `Debug → std::fmt::Debug`) — wiring the
  previously-unread `macro_expansion::emit_implements`.

### Changed

- **`get_impact` returns the real blast radius.** Previously it returned only
  community + process membership. It now also returns reverse dependencies —
  `callers`, `importers`, `users`, `implementors` — each as a re-queryable
  `{id, qualified_name, label}` handle so a consumer (Cortex, an agent) keeps
  traversing through MCP instead of receiving a terminal digest.
- **Indexer batches inserts across files.** Symbol nodes/edges now accumulate
  into a `SymbolBatch` and flush in large batches instead of one small bulk
  call per file. Indexing the 500-file synthetic fixture dropped from ~140 s to
  ~8 s (~17×); the `scalability_bench` 60 s budget now passes with wide margin.
- **`clustering.rs` (1061 lines) and `indexer.rs` (832 lines)** split into
  `src/clustering/{community,process,impact}` and `src/indexer/{walk,persist}`
  directory modules to satisfy the 500-line-per-file limit. Behaviour-preserving.

### Fixed

- **Process call-chains were flattened.** `ParticipatesIn` edges hardcoded
  `depth = 0`, discarding the BFS distance that was already computed. They now
  carry the real per-step depth, so a process's participants can be ordered.
- **`#[derive(...)]`, `impl Trait for`, and Java `implements` produced no (or
  wrong) `Implements` edges.** The indexer dropped the parser's implements refs
  and the resolver fell back to a fuzzy method-name-match heuristic (false
  positives + missing every declared impl). Replaced by declared resolution
  (see Added).

## [0.0.9] — Skip build / dependency dirs at walk time (Android, iOS, Go, JVM)

### Fixed

- **Indexer wasted minutes walking into `build/`, `Pods/`, `DerivedData/`,
  `.gradle/`, `vendor/` etc.** on multi-language repos. The previous
  `should_skip` only filtered Rust / JS / Python conventions
  (`target`, `node_modules`, `__pycache__`, `.venv`, hidden dirs), so
  Android codebases (`app/build/intermediates/`, `feature/*/build/`)
  produced tens of thousands of file stat() calls and per-file size
  rejections after the walker had already descended into them. On a
  large Android tree this manifested as `ingest_codebase` appearing
  to hang. Filtering at the directory level avoids the descent
  entirely.
- Extended `should_skip` to cover: `build`, `out`, `.gradle`, `.idea`
  (JVM / Android), `Pods`, `DerivedData`, `.build`, `Carthage`,
  `.swiftpm` (Apple), `vendor` (Go), `dist`, `bin`, `obj`, `coverage`,
  `.nyc_output`, `.pytest_cache`, `.mypy_cache`, `.tox`, `.eggs`.

## [0.0.8] — Multi-language parser expansion (Java, Kotlin, Swift, Objective-C, C, C++, Go)

### Added

- **Seven new tree-sitter parsers** under `src/parser/`:
  `java.rs`, `kotlin.rs`, `swift.rs`, `objc.rs`, `c.rs`, `cpp.rs`, `go.rs`.
  Adds JVM (Java + Kotlin), Apple (Swift + Objective-C), systems
  (C + C++) and Go to the previously-shipped Rust / Python / TypeScript
  trio. `parser/mod.rs` registers all 10 languages; `tool_schemas.rs`
  exposes them in `index_codebase` / `analyze_codebase` language hints.
- **Grammar dependencies** (Cargo.toml): `tree-sitter-java`,
  `tree-sitter-kotlin-ng`, `tree-sitter-swift`, `tree-sitter-objc`,
  `tree-sitter-c`, `tree-sitter-cpp`, `tree-sitter-go`. All MIT or
  Apache-2.0; all official tree-sitter grammars on crates.io.

### Changed

- `do_analyze_codebase`: replaced the explicit Rust/Python/TypeScript
  match with a generic `lang.as_str()` dispatch so LSP-enhanced
  resolution flows through to every supported language.
- Each new parser extracts typed symbols matching the existing
  `graph_store` schema (entities + edges) so the property graph
  remains polyglot-uniform.

### Migration notes

- First build is slower: each new tree-sitter grammar carries C
  source that must compile through `cmake` / `cc`. Subsequent
  incremental builds reuse the per-grammar caches.

## [0.0.7] — Rename binary `ai-architect-mcp` → `automatised-pipeline`

### Changed

- **Binary renamed** from `ai-architect-mcp` to `automatised-pipeline`
  to match the project / plugin / repository name. The Cortex
  `ap_bridge.py` allowlist already accepts `automatised-pipeline`;
  the legacy `ai-architect-mcp` identifier was a stale carryover from
  the project's earlier life as the umbrella `ai-architect` pipeline.
  Affected files: `Cargo.toml` `[[bin]] name`, `bin/ensure-binary.sh`,
  `.mcp.json`, `.github/workflows/release.yml`, `.claude/hooks/session-start.sh`.

### Migration notes

- Release artifacts are now named `automatised-pipeline-{os}-{arch}.tar.gz`
  (was `ai-architect-mcp-*`). Consumers (e.g. Cortex `pipeline_install_release.py`)
  must update their download URLs.
- Built binary path is now `target/release/automatised-pipeline`.
- The Rust crate name (`[package].name`) is unchanged at `ai-architect-mcp`
  to preserve crate identity for any downstream Cargo dependents.

## [0.0.6] — Self-locating plugin MCP launcher

### Fixed

- **`ai-architect` MCP server failed to connect from any non-plugin CWD.**
  The `.mcp.json` launcher relied on Claude Code injecting
  `CLAUDE_PLUGIN_ROOT`, which was not happening reliably. The fallback
  `${CLAUDE_PLUGIN_ROOT:-$(cd "$(dirname "$0")" && pwd)}` is broken
  under `bash -c` (where `$0` is `bash`, not the script path), so
  `$ROOT` resolved to the user's project directory — where
  `target/release/ai-architect-mcp` does not exist. Replaced the bash
  command with a Python one-liner that reads
  `~/.claude/plugins/installed_plugins.json` (always at a fixed
  absolute path) to discover the plugin install path, then `execvp`s
  the Rust binary, falling back to `bin/ensure-binary.sh` if the
  binary is missing. No CWD or env dependency. Users in any project
  now get the MCP server on plugin update — no per-project
  configuration required.

## [0.0.5] — Resilient install: pre-build the MCP binary

### Fixed

- **Inline `cargo run --release` fallback in `.mcp.json` blocked MCP
  startup.** When `target/release/ai-architect-mcp` was absent (fresh
  install or first session after a checkout), the launcher invoked
  `cargo run --release`, which can take 2–3 minutes for a cold rust
  toolchain. Claude Code's MCP startup timeout fires long before that,
  so the server appeared "disconnected" with no actionable message.
  Replaced with a fail-fast launcher: check binary → if missing, run
  `bin/ensure-binary.sh verbose` → re-check → if still missing, exit
  1 with a `FATAL` message printing the exact `cargo build` command
  to run. Never compiles inline during MCP startup.

### Added

- `bin/ensure-binary.sh` — idempotent build script. Exits 0 fast when
  `target/release/ai-architect-mcp` exists and is newer than every
  file under `src/` and `Cargo.{toml,lock}`. Otherwise runs
  `cargo build --release` with progress on stderr only (stdout is
  reserved for the MCP protocol). Distinct exit codes:
  127 (cargo not in PATH), 1 (build failure or post-build sanity
  failure). Runs in two modes: `quiet` (default; errors only) and
  `verbose` (progress + timing).
- `session-start.sh` hook now invokes `ensure-binary.sh verbose`
  BEFORE Claude Code attempts to connect MCP servers. First-time
  install builds the binary synchronously during the session-start
  banner; subsequent sessions exit instantly. Hook continues even on
  build failure — the `.mcp.json` launcher surfaces the error
  cleanly on `/mcp`.

## [0.0.4] — Idempotent BM25 index rebuild

### Fixed

- `search::bm25::build_index` now wipes ``index_dir`` before calling
  `Index::create_in_dir`. Tantivy refuses to reuse a directory that
  already contains an index (`Index already exists`), so consecutive
  runs of `analyze_codebase` (e.g., Cortex's `ingest_codebase` with
  `force_reindex=true`) failed with that error. The BM25 index is a
  derived artifact rebuilt from the live graph, so removing it is
  safe.

## [0.0.3] — Schema-guarded edge resolution

### Added

- `is_known_rel_table` helper in `graph_store.rs` — public predicate
  over `REL_TABLES` so producers that build relationship-table names
  from runtime symbol labels can validate before insertion instead of
  failing inside the graph driver.
- `Imports_File_Method` declared in `REL_TABLES`; previously a method
  imported directly from a file produced a dropped edge with no
  recoverable target table.

### Fixed

- `resolver::resolve_single_import`, `resolve_glob_import`,
  `resolve_calls`, and `resolve_field_type_uses` now consult
  `is_known_rel_table` before staging an edge. Unknown labels are
  logged (first 8 occurrences via an `AtomicU64` counter to bound
  log volume) and the edge is dropped — this replaces the previous
  hard failure path when a new caller/target label combination
  appeared at runtime.
- `lsp_resolver::try_add_lsp_edge` applies the same guard to
  LSP-derived edges (rust-analyzer / pyright / tsserver).

### Added — public-readiness baseline (carried over from Unreleased)

- Public-readiness baseline: LICENSE (MIT, sole independent author),
  CONTRIBUTING.md, CODE_OF_CONDUCT.md, SECURITY.md.
- GitHub issue templates (bug / feature / audit-finding) and PR template
  with audit-cycle checklist.

## [0.0.2] — Stage 1–9 wired + 23 MCP tools

### Added

- 23 MCP tools across pipeline stages 0 through 9:
  - Stage 0: `health_check`
  - Stage 1: `extract_finding`, `refine_finding`
  - Stage 2: `start_verification`, `append_clarification`,
    `finalize_verification`, `abort_verification`
  - Stage 3a: `index_codebase`, `query_graph`, `get_symbol`
  - Stage 3b: `resolve_graph`, `lsp_resolve`
  - Stage 3c: `cluster_graph`, `get_processes`, `get_impact`
  - Stage 3d: `search_codebase`, `get_context`, `analyze_codebase`,
    `detect_changes`
  - Stage 4: `prepare_prd_input`
  - Stage 6: `validate_prd_against_graph`
  - Stage 8: `check_security_gates`
  - Stage 9: `verify_semantic_diff`
- LadybugDB property graph with 16 node labels, 36+ relationship tables.
- tree-sitter AST extractors for Rust, Python, TypeScript.
- Cross-file resolution (imports, calls, impls) with confidence scoring;
  optional LSP deep resolution (rust-analyzer / pyright /
  typescript-language-server).
- Inline Louvain community detection with C2 repair.
- BFS execution-flow tracing from entry points.
- Hybrid BM25 + sparse TF-IDF + RRF search index (Tantivy-backed).
- Tarjan SCC for cycle detection in semantic-diff.
- 220 tests passing, zero clippy warnings, every numeric constant sourced.

### Architecture

- Hand-rolled stdio JSON-RPC 2.0 (no SDK — owns the wire).
- Clean Architecture with strict module boundaries:
  `transport → server → handlers → core modules → persistence`.

---

For pre-0.0.2 history (initial scaffolding, dependency selection),
see git log. The project entered semantic-versioned releases at v0.0.2.
