# incremental_speed benchmark — MANIFEST

The performance evidence for incremental re-indexing (#62) and artifact
bootstrap-fill (#55): both are much cheaper than a full index. This claim was
**removed from the correctness test suite** in issue #74 — a wall-clock ratio is
not a deterministic, green-everywhere property — and lives here instead, with
recorded hardware and committed results (the `benchmarks/token_surface/` pattern).

Reproducible: the git fixture is generated in-code, so
`cargo run -p incremental-speed-bench --release` regenerates `results.json`.

## Provenance
- **Hardware:** Apple Silicon (`arm64` / `aarch64`), macOS 26.5.1.
- **Toolchain:** rustc 1.95.0 (59807616e 2026-04-14).
- **Date:** 2026-07-25.
- **Search/graph dependency set (issue #78):** tantivy 0.26.1, lbug 0.18.3.
  Re-run after the upgrade from tantivy 0.22.1 / lbug 0.15.4. A search-engine
  change can move ranking silently, so these numbers are not carried over from
  the previous versions; they were regenerated on this dependency set.
- **Command:** `cargo run -p incremental-speed-bench --release`.
- **Profile:** release (the headline `results.json`); debug numbers below for contrast.

## Methodology
- **Fixture:** a git repo of 260 symbol-dense Python modules (→ ~4.7k nodes,
  ~4.7k edges), then a 4-file diff (edit + add + delete + rename).
- **Real library:** drives `indexer::index_codebase_with_language`,
  `indexer::index_incremental`, `artifact::export_artifact` /
  `import_artifact`, and `indexer::fill_after_bootstrap` — the exact operations
  the tools run, not a re-implementation.
- **Best-of-N:** each operation is timed `ATTEMPTS=5` times and the **minimum**
  (least-contended, truest) is reported. The full index is re-run each attempt;
  the incremental copies the pristine graph each attempt; the fill re-imports the
  artifact into a fresh dir each attempt.

## Results (representative, otherwise-idle machine)
### Release
| operation | time | ratio to full |
|---|---:|---:|
| full index (260 files) | ~2500 ms | — |
| incremental (4-file diff) | ~280 ms | **~11%** |
| bootstrap-fill (4-file diff) | ~380 ms | **~15%** |

### Debug (same machine, for profile contrast)
| operation | time | ratio to full |
|---|---:|---:|
| full index | ~4500 ms | — |
| incremental | ~1100 ms | **~24%** |
| bootstrap-fill | ~1170 ms | **~26%** |

See `results.json` for the exact numbers of the last release run.

## Why this is a benchmark, not a correctness gate (issue #74)
A wall-clock RATIO is **hardware-, profile-, and load-dependent**, and its
variance exceeds any tight margin:

- The short op (incremental / fill) has a **fixed-cost floor** — a `git diff`,
  opening the DB, re-parsing a handful of files, and (for the fill) hashing the
  tree to write a local manifest. The full index instead **scales with repo
  size**. On faster hardware / the release profile the full index shrinks toward
  that floor, so the *ratio* can degrade even though both absolute times improve
  (the reporter of #74 saw a release `full` of 2037 ms give a 79% ratio).
- Observed spread on THIS machine at min-of-5, release: the fill ratio measured
  12%, 15%, and **62%** across three back-to-back runs (the 62% run was under
  concurrent load) — a >4× spread on identical inputs. A ≤30% gate below even the
  best observation on the reporter's machine could never be green everywhere.

That is a benchmark's job — report numbers with their hardware and their
variance — not a gate's. The correctness of #55/#62 (graph parity, exact change
partition, fill method) stays a hard gate in `tests/artifact_incremental_fill.rs`
and `tests/incremental_index.rs`, green on every machine and profile.
