use super::*;

#[test]
fn test_regex_extract_backticked() {
    let prd = "modify `handle_tool_call` in `src/main.rs` and the `GraphStore` helper.";
    let syms = regex_extract_symbols(prd);
    let tokens: Vec<String> = syms.iter().map(|s| s.token.clone()).collect();
    assert!(
        tokens.contains(&"handle_tool_call".to_string()),
        "got {:?}",
        tokens
    );
    assert!(
        tokens.contains(&"GraphStore".to_string()),
        "got {:?}",
        tokens
    );
    assert!(
        tokens.iter().any(|t| t == "src/main.rs"),
        "got {:?}",
        tokens
    );
}

#[test]
fn test_regex_min_token_len() {
    let prd = "touch `ab` and `abc` and `foo`.";
    let syms = regex_extract_symbols(prd);
    assert!(syms.iter().all(|s| s.token.len() >= FALLBACK_MIN_TOKEN_LEN));
    assert!(syms.iter().any(|s| s.token == "abc"));
    assert!(!syms.iter().any(|s| s.token == "ab"));
}

#[test]
fn test_compute_status_escalation() {
    let mut findings = Vec::new();
    findings.push(ValidationFinding {
        axis: "x".into(),
        severity: "info".into(),
        message: "".into(),
        symbol: None,
        details: json!({}),
    });
    assert_eq!(compute_status(&findings), "ok");
    findings.push(ValidationFinding {
        axis: "x".into(),
        severity: "warning".into(),
        message: "".into(),
        symbol: None,
        details: json!({}),
    });
    assert_eq!(compute_status(&findings), "warning");
    findings.push(ValidationFinding {
        axis: "x".into(),
        severity: "critical".into(),
        message: "".into(),
        symbol: None,
        details: json!({}),
    });
    assert_eq!(compute_status(&findings), "fail");
}

#[test]
fn test_looks_like_file_path() {
    assert!(looks_like_file_path("src/main.rs"));
    assert!(looks_like_file_path("a/b/c.tsx"));
    assert!(!looks_like_file_path("main.rs"));
    assert!(!looks_like_file_path("src/main"));
    assert!(!looks_like_file_path("not/a/file.xyz"));
}
