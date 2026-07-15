// prd_validator::axis_symbol — Axis 1: symbol-hallucination findings. Turns
// the `verdict` module's per-claim classification into `ValidationFinding`s.
//
// Split out of prd_validator::mod (coding-standards §4.1/§4.2 — see
// verdict.rs's header for the full split rationale).

use super::verdict::ClaimVerdict;
use super::{ResolvedClaim, ValidationFinding};
use serde_json::json;

pub(super) fn emit_symbol_hallucination(
    resolved: &[ResolvedClaim],
    verdicts: &[ClaimVerdict],
    findings: &mut Vec<ValidationFinding>,
) {
    for (r, verdict) in resolved.iter().zip(verdicts.iter()) {
        let kind = r.claim.change_kind.as_str();
        match verdict {
            ClaimVerdict::Hallucinated => {
                findings.push(ValidationFinding {
                    axis: "symbol_hallucination".into(),
                    severity: "critical".into(),
                    message: format!(
                        "claimed symbol '{}' (change_kind={}) not found in graph",
                        r.claim.token, kind
                    ),
                    symbol: Some(r.claim.token.clone()),
                    details: json!({
                        "change_kind": kind,
                        "did_you_mean": r.did_you_mean,
                    }),
                });
            }
            ClaimVerdict::Unverifiable(reason) => {
                findings.push(ValidationFinding {
                    axis: "symbol_hallucination".into(),
                    severity: "info".into(),
                    message: format!(
                        "claimed symbol '{}' (change_kind={}) could not be verified: {}",
                        r.claim.token, kind, reason
                    ),
                    symbol: Some(r.claim.token.clone()),
                    details: json!({
                        "change_kind": kind,
                        "did_you_mean": r.did_you_mean,
                        "unverifiable_reason": reason,
                    }),
                });
            }
            ClaimVerdict::Resolved | ClaimVerdict::Unscored => {}
        }
    }
}

pub(super) fn emit_unresolved_info(
    resolved: &[ResolvedClaim],
    is_regex_fallback: bool,
    findings: &mut Vec<ValidationFinding>,
) {
    if !is_regex_fallback {
        return;
    }
    for r in resolved {
        if r.resolved_qn.is_some() {
            continue;
        }
        findings.push(ValidationFinding {
            axis: "symbol_hallucination".into(),
            severity: "info".into(),
            message: format!(
                "unresolved token '{}' (regex fallback — likely prose)",
                r.claim.token
            ),
            symbol: Some(r.claim.token.clone()),
            details: json!({ "did_you_mean": r.did_you_mean, "extraction_mode": "regex_fallback" }),
        });
    }
}
