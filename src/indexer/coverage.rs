// coverage — indexing-coverage honesty sidecar (issue #57).
//
// Layer: shared/persistence within the indexer. Records, per indexing run, which
// files the indexer could NOT fully cover — so an agent never overtrusts the
// graph. Three kinds of gap:
//   * ParsePartial — the file WAS indexed, but its parse tree had ERROR/MISSING
//     regions (1-based line ranges); constructs inside those lines may be absent
//     from the graph.
//   * Skipped — the file was not indexed at all (oversized, unreadable, or the
//     parse timed out); its File node exists but carries no symbols.
//   * Quarantined — the parser PANICKED on the file; the panic was isolated
//     (caught) so it could not kill the index, and the file was left uncovered.
//
// Storage decision (documented per the issue's "decide from what exists"):
// coverage lives in a DEDICATED `index_coverage.json` sidecar beside the graph,
// NOT inside the graph and NOT folded into the change-detection manifest —
// mirroring the reference's separate `index_coverage` SQLite table
// (DeusData/codebase-memory-mcp src/store/store.c: "coverage is metadata ABOUT
// the graph, not part of it"). Keeping it separate preserves single-responsibility
// (the manifest answers "what changed?"; coverage answers "what's a gap?"), lets
// the incremental pass carry it forward per file exactly like the manifest, and
// lets the artifact bundle it so a bootstrapped clone inherits the honesty signal.
//
// The map is SPARSE: it holds an entry only for files that are NOT fully covered.
// Absence from the map is therefore NOT a completeness guarantee — it only means
// the indexer detected no gap (a file keyed to a subtly wrong grammar can still
// parse "clean"). Every tool that surfaces this repeats that caveat.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Sidecar schema version. A newer schema is treated as "no coverage available"
/// rather than mis-parsed, so an old binary never reports against a format it
/// cannot read.
pub const COVERAGE_SCHEMA_VERSION: u32 = 1;

/// The coverage sidecar filename, written beside the graph in `output_dir`.
pub const COVERAGE_FILE: &str = "index_coverage.json";

/// The kind of coverage gap for a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoverageKind {
    /// Indexed, but the parse tree had ERROR/MISSING ranges.
    ParsePartial,
    /// Not indexed at all (oversized / unreadable / parse timeout).
    Skipped,
    /// The parser panicked; isolated and left uncovered.
    Quarantined,
}

/// One uncovered file's record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileCoverage {
    pub kind: CoverageKind,
    /// Human-readable reason (for Skipped/Quarantined) or a short summary.
    #[serde(default)]
    pub detail: String,
    /// 1-based inclusive (start, end) line ranges of the gaps (ParsePartial).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub error_ranges: Vec<(u32, u32)>,
}

/// The whole coverage sidecar for one graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoverageReport {
    pub schema_version: u32,
    /// "full" | "incremental" | "bootstrap_fill" — how this coverage was produced.
    pub index_mode: String,
    /// Files that got a File node (the covered surface). Informational.
    pub files_indexed: u64,
    /// Sparse map: rel_path → gap. Only NOT-fully-covered files appear.
    pub files: BTreeMap<String, FileCoverage>,
}

impl CoverageReport {
    pub fn new(index_mode: &str, files_indexed: u64) -> Self {
        CoverageReport {
            schema_version: COVERAGE_SCHEMA_VERSION,
            index_mode: index_mode.to_string(),
            files_indexed,
            files: BTreeMap::new(),
        }
    }

    /// Count of files by kind: (parse_partial, skipped, quarantined).
    pub fn counts(&self) -> (u64, u64, u64) {
        let mut partial = 0;
        let mut skipped = 0;
        let mut quarantined = 0;
        for c in self.files.values() {
            match c.kind {
                CoverageKind::ParsePartial => partial += 1,
                CoverageKind::Skipped => skipped += 1,
                CoverageKind::Quarantined => quarantined += 1,
            }
        }
        (partial, skipped, quarantined)
    }
}

/// The coverage sidecar path for a given tool `output_dir` (sibling of `graph/`).
pub fn coverage_path(output_dir: &Path) -> PathBuf {
    output_dir.join(COVERAGE_FILE)
}

/// Loads the coverage report at `path`. Returns `None` when absent, unreadable,
/// unparseable, or a newer schema — every "no usable coverage" case, so a caller
/// treats it as "coverage unavailable" (which is itself honest: absence of
/// coverage data is not a completeness claim).
pub fn load(path: &Path) -> Option<CoverageReport> {
    let bytes = fs::read(path).ok()?;
    let report: CoverageReport = serde_json::from_slice(&bytes).ok()?;
    if report.schema_version > COVERAGE_SCHEMA_VERSION {
        return None;
    }
    Some(report)
}

/// Writes `report` to `path` atomically (temp file + rename).
pub fn save(path: &Path, report: &CoverageReport) -> Result<(), String> {
    let json = serde_json::to_vec_pretty(report).map_err(|e| format!("coverage: encode: {e}"))?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, &json).map_err(|e| format!("coverage: write {}: {e}", tmp.display()))?;
    fs::rename(&tmp, path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("coverage: rename into place: {e}")
    })
}

/// Accumulates per-file coverage gaps during one indexing pass. The indexer
/// records each problematic file here; the composition root turns it into a
/// `CoverageReport` (merging carry-forward for the incremental case).
#[derive(Debug, Default)]
pub struct CoverageCollector {
    files: BTreeMap<String, FileCoverage>,
    files_indexed: u64,
}

impl CoverageCollector {
    /// Records that a file was successfully given a File node (covered surface).
    pub fn note_indexed(&mut self) {
        self.files_indexed += 1;
    }

    /// Records a parse-partial file with its error line ranges.
    pub fn record_partial(&mut self, rel: &str, error_ranges: Vec<(u32, u32)>) {
        let detail = format!("{} error range(s)", error_ranges.len());
        self.files.insert(
            rel.to_string(),
            FileCoverage {
                kind: CoverageKind::ParsePartial,
                detail,
                error_ranges,
            },
        );
    }

    /// Records a file skipped (not indexed) with a reason.
    pub fn record_skipped(&mut self, rel: &str, reason: String) {
        self.files.insert(
            rel.to_string(),
            FileCoverage {
                kind: CoverageKind::Skipped,
                detail: reason,
                error_ranges: Vec::new(),
            },
        );
    }

    /// Records a file quarantined after its parser panicked.
    pub fn record_quarantined(&mut self, rel: &str, reason: String) {
        self.files.insert(
            rel.to_string(),
            FileCoverage {
                kind: CoverageKind::Quarantined,
                detail: reason,
                error_ranges: Vec::new(),
            },
        );
    }

    pub fn files_indexed(&self) -> u64 {
        self.files_indexed
    }

    /// The set of rel_paths this pass produced a gap record for (used by the
    /// incremental merge to know which files were freshly classified).
    pub fn into_files(self) -> BTreeMap<String, FileCoverage> {
        self.files
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_counts() {
        let dir = tempfile::Builder::new()
            .prefix("coverage_rt_")
            .tempdir()
            .expect("temp dir");
        let path = coverage_path(dir.path());

        let mut report = CoverageReport::new("full", 10);
        report.files.insert(
            "src/a.rs".into(),
            FileCoverage {
                kind: CoverageKind::ParsePartial,
                detail: "1 error range(s)".into(),
                error_ranges: vec![(3, 5)],
            },
        );
        report.files.insert(
            "src/big.rs".into(),
            FileCoverage {
                kind: CoverageKind::Skipped,
                detail: "oversized".into(),
                error_ranges: vec![],
            },
        );
        report.files.insert(
            "src/boom.rs".into(),
            FileCoverage {
                kind: CoverageKind::Quarantined,
                detail: "panic in parser".into(),
                error_ranges: vec![],
            },
        );
        save(&path, &report).expect("save");

        let loaded = load(&path).expect("load");
        assert_eq!(loaded.files.len(), 3);
        assert_eq!(loaded.counts(), (1, 1, 1));
        assert_eq!(loaded.files["src/a.rs"].error_ranges, vec![(3, 5)]);
        assert_eq!(loaded.files["src/big.rs"].kind, CoverageKind::Skipped);
    }

    #[test]
    fn absent_or_newer_schema_is_none() {
        let dir = tempfile::Builder::new()
            .prefix("coverage_none_")
            .tempdir()
            .expect("temp dir");
        let path = coverage_path(dir.path());
        assert!(load(&path).is_none());
        let future = format!(
            "{{\"schema_version\":{},\"index_mode\":\"full\",\"files_indexed\":0,\"files\":{{}}}}",
            COVERAGE_SCHEMA_VERSION + 1
        );
        fs::write(&path, future).unwrap();
        assert!(load(&path).is_none());
    }

    #[test]
    fn collector_accumulates_each_kind() {
        let mut c = CoverageCollector::default();
        c.note_indexed();
        c.note_indexed();
        c.record_partial("a.rs", vec![(1, 2)]);
        c.record_skipped("b.rs", "oversized".into());
        c.record_quarantined("c.rs", "panic in parser".into());
        assert_eq!(c.files_indexed(), 2);
        let files = c.into_files();
        assert_eq!(files.len(), 3);
        assert_eq!(files["a.rs"].kind, CoverageKind::ParsePartial);
        assert_eq!(files["c.rs"].kind, CoverageKind::Quarantined);
    }
}
