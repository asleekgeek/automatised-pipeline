// prd_validator::axis_community — Axis 2: community-consistency findings.
// Checks whether the resolved claims span more Leiden communities than the
// PRD's scope assertions would suggest.
//
// Split out of prd_validator::mod (coding-standards §4.1/§4.2 — see
// verdict.rs's header for the full split rationale).

use super::{ResolvedClaim, ScopeClaim, ValidationFinding};
use crate::graph_store::GraphStore;
use serde_json::json;
use std::collections::BTreeSet;

// source: stages/stage-6.md §5 V2 — threshold for community-consistency warning.
// ≥2 communities -> warning, ≥3 -> critical (matches specs §4/§5 severity ladder).
pub(super) const COMMUNITY_SPAN_WARNING_THRESHOLD: u64 = 2;
pub(super) const COMMUNITY_SPAN_CRITICAL_THRESHOLD: u64 = 3;

pub(super) fn communities_for_resolved(
    store: &GraphStore,
    resolved: &[ResolvedClaim],
) -> Vec<Option<String>> {
    resolved
        .iter()
        .filter_map(|r| r.resolved_qn.as_ref())
        .map(|qn| community_of(store, qn))
        .collect()
}

fn community_of(store: &GraphStore, qualified_name: &str) -> Option<String> {
    // Iterate per-label rather than using rel-type alternation — mirrors
    // search/mod.rs::lookup_community for lbug dialect compatibility.
    let escaped = qualified_name.replace('\'', "\\'");
    for label in [
        "Function",
        "Method",
        "Struct",
        "Enum",
        "Trait",
        "Constant",
        "TypeAlias",
        "Module",
    ] {
        let rel = format!("MemberOf_{label}_Community");
        let cypher = format!(
            "MATCH (n:{label})-[:{rel}]->(c:Community) \
             WHERE n.qualified_name = '{escaped}' \
             RETURN c.id LIMIT 1"
        );
        if let Ok(qr) = store.execute_query(&cypher) {
            if let Some(row) = qr.rows.first() {
                if let Some(cid) = row.first() {
                    if !cid.is_empty() {
                        return Some(cid.clone());
                    }
                }
            }
        }
    }
    None
}

pub(super) fn emit_community_consistency(
    scope_claims: &[ScopeClaim],
    communities: &[Option<String>],
    findings: &mut Vec<ValidationFinding>,
) {
    let distinct = distinct_count(communities) as u64;
    let has_scope_assertion = scope_claims
        .iter()
        .any(|s| matches!(s, ScopeClaim::CommunityScope { .. }));
    if !has_scope_assertion && distinct < COMMUNITY_SPAN_WARNING_THRESHOLD {
        return;
    }
    let severity = if distinct >= COMMUNITY_SPAN_CRITICAL_THRESHOLD {
        "critical"
    } else if distinct >= COMMUNITY_SPAN_WARNING_THRESHOLD {
        "warning"
    } else {
        return;
    };
    let touched: Vec<String> = communities.iter().filter_map(|c| c.clone()).collect();
    let mut unique: Vec<String> = touched
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    unique.sort();
    findings.push(ValidationFinding {
        axis: "community_consistency".into(),
        severity: severity.into(),
        message: format!(
            "affected symbols span {} distinct Leiden communities (threshold: warning at {}, critical at {})",
            distinct, COMMUNITY_SPAN_WARNING_THRESHOLD, COMMUNITY_SPAN_CRITICAL_THRESHOLD
        ),
        symbol: None,
        details: json!({
            "distinct_communities": distinct,
            "communities": unique,
            "has_scope_assertion": has_scope_assertion,
        }),
    });
}

pub(super) fn distinct_count(items: &[Option<String>]) -> usize {
    let set: BTreeSet<&String> = items.iter().filter_map(|x| x.as_ref()).collect();
    set.len()
}
