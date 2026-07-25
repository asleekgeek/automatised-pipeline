// mcp_prompts — MCP `prompts/list` + `prompts/get` for ai-architect.
//
// AP's tools are strongly ordered and the order is not guessable:
// `index_codebase` → `resolve_graph` → `query_graph`; the finding lifecycle
// `extract_finding` → `refine_finding` → `start_verification` →
// `append_clarification` → `finalize_verification`; PRD grounding
// `prepare_prd_input` → `validate_prd_against_graph`. A caller that gets the
// order wrong gets an empty or misleading result rather than an error — the
// failure is silent. Tool descriptions carry the ordering in prose; prompts
// carry it as protocol the client can enumerate (issue #65).
//
// SOURCE OF TRUTH (issue #65 criterion 3, §1.2): every step names a tool and
// pulls that tool's one-line summary from `tool_schemas::tool_summary`. The
// prompts here own only the *workflow* — the ordering and the per-step
// guidance — never a hand-copied duplicate of a tool description. Adding or
// renaming a tool therefore cannot leave two descriptions that disagree; the
// `steps_reference_only_real_tools` test fails loudly if a step names a tool
// that `tools_list()` does not register.
//
// PROFILE AWARENESS: a prompt is offered under a profile iff the profile
// registers every tool the prompt's workflow calls — the profile IS the
// registry, exactly as `handle_tool_call` treats tools. So `verify_finding`
// and `ground_prd` (which drive the internal pipeline stages) are hidden
// under `core`, and `explore_codebase` renders its granular
// index→resolve→query path under `full` but the composed `analyze_codebase`
// path under `core`.

use serde_json::{json, Value};

use crate::tool_profile::ToolProfile;
use crate::tool_schemas;

// JSON-RPC error code for invalid parameters (MCP uses this for a bad or
// unknown prompt name and for missing/malformed arguments).
// source: JSON-RPC 2.0 §5.1 — -32602 "Invalid params".
const INVALID_PARAMS: i32 = -32602;

/// A single declared prompt argument. Every argument carries `name`, `title`,
/// `description`, and `required` (issue #65 criterion 2).
struct ArgSpec {
    name: &'static str,
    title: &'static str,
    description: &'static str,
    required: bool,
}

/// One step of a prompt's workflow: the tool to call and why/when. The tool's
/// canonical description is NOT stored here — it is pulled from `tool_schemas`.
struct StepSpec {
    tool: &'static str,
    guidance: &'static str,
}

/// A prompt's static metadata (everything except the profile-dependent steps
/// and the argument-substituted body).
struct PromptMeta {
    name: &'static str,
    title: &'static str,
    description: &'static str,
    args: &'static [ArgSpec],
}

// ---------------------------------------------------------------------------
// Prompt catalog — metadata
// ---------------------------------------------------------------------------

const EXPLORE: PromptMeta = PromptMeta {
    name: "explore_codebase",
    title: "Explore codebase",
    description: "Graph-first structural discovery: index/resolve then query, \
                  look up, and search a codebase to answer a question.",
    args: &[
        ArgSpec {
            name: "project",
            title: "Project",
            description: "Absolute path to the codebase to index and explore.",
            required: true,
        },
        ArgSpec {
            name: "question",
            title: "Question",
            description: "Architecture or implementation question to investigate.",
            required: true,
        },
    ],
};

const REVIEW: PromptMeta = PromptMeta {
    name: "review_change_impact",
    title: "Review change impact",
    description: "Map a change's blast radius: affected symbols, reverse \
                  dependencies, communities, processes, and risks.",
    args: &[
        ArgSpec {
            name: "project",
            title: "Project",
            description: "Absolute path to the indexed codebase.",
            required: true,
        },
        ArgSpec {
            name: "change",
            title: "Change",
            description: "The change, symbol, or area whose impact to review.",
            required: true,
        },
        ArgSpec {
            name: "base_branch",
            title: "Base branch",
            description: "Git ref for detect_changes to diff against; defaults to main.",
            required: false,
        },
    ],
};

const VERIFY: PromptMeta = PromptMeta {
    name: "verify_finding",
    title: "Verify a finding",
    description: "Drive the finding lifecycle: extract, refine, open a \
                  clarification session, and finalize a verified receipt.",
    args: &[
        ArgSpec {
            name: "run_id",
            title: "Run ID",
            description: "The run under output_dir whose finding is being verified.",
            required: true,
        },
        ArgSpec {
            name: "finding_id",
            title: "Finding ID",
            description: "The finding to extract, refine, and verify.",
            required: true,
        },
    ],
};

const GROUND_PRD: PromptMeta = PromptMeta {
    name: "ground_prd",
    title: "Ground a PRD on the graph",
    description: "Bundle graph intel for a verified finding or a free-text \
                  feature, then validate the generated PRD against the graph.",
    args: &[
        ArgSpec {
            name: "project",
            title: "Project",
            description: "Absolute path to the resolved+clustered codebase graph.",
            required: true,
        },
        ArgSpec {
            name: "target",
            title: "Target",
            description: "A verified finding_id, or a free-text feature description \
                          to ground directly against the graph.",
            required: true,
        },
    ],
};

/// Every prompt the server can publish, in list order. Which of these is
/// actually offered depends on the active profile (see `is_available`).
const ALL_PROMPTS: [&PromptMeta; 4] = [&EXPLORE, &REVIEW, &VERIFY, &GROUND_PRD];

// ---------------------------------------------------------------------------
// Workflow steps (profile-aware where the pipeline offers two paths)
// ---------------------------------------------------------------------------

/// The ordered tool workflow for `name` under `profile`, or `None` when no
/// prompt by that name exists. The steps for a real prompt always reference
/// tools; whether the *profile* registers them is checked by `is_available`.
fn steps_for(name: &str, profile: ToolProfile) -> Option<Vec<StepSpec>> {
    match name {
        "explore_codebase" => Some(explore_steps(profile)),
        "review_change_impact" => Some(review_steps()),
        "verify_finding" => Some(verify_steps()),
        "ground_prd" => Some(ground_steps()),
        _ => None,
    }
}

fn explore_steps(profile: ToolProfile) -> Vec<StepSpec> {
    // The granular index→resolve→query path exposes internal stages, so it is
    // only offered under `full`. Under `core` the composed `analyze_codebase`
    // (which runs index + resolve + cluster in one call) stands in — the same
    // graph, one call.
    let mut steps = match profile {
        ToolProfile::Full => vec![
            StepSpec {
                tool: "index_codebase",
                guidance: "Parse the tree into a code graph. On a fresh repo you may \
                           instead call analyze_codebase to do index+resolve+cluster at once.",
            },
            StepSpec {
                tool: "resolve_graph",
                guidance: "Resolve cross-file Imports/Calls/Implements/Uses edges before querying.",
            },
        ],
        ToolProfile::Core => vec![StepSpec {
            tool: "analyze_codebase",
            guidance: "One call gives a fully indexed, resolved, clustered graph.",
        }],
    };
    steps.extend([
        StepSpec {
            tool: "query_graph",
            guidance: "Answer structural questions with read-only Cypher; page via next_offset.",
        },
        StepSpec {
            tool: "get_symbol",
            guidance: "Look up an exact qualified name and its immediate edges.",
        },
        StepSpec {
            tool: "get_context",
            guidance: "Get a symbol's grouped 360° relationships before drilling in.",
        },
        StepSpec {
            tool: "search_codebase",
            guidance: "When you have a keyword but not a qualified name, rank symbols first.",
        },
    ]);
    steps
}

fn review_steps() -> Vec<StepSpec> {
    vec![
        StepSpec {
            tool: "detect_changes",
            guidance: "Map the diff to affected symbols, communities, and processes with a risk score.",
        },
        StepSpec {
            tool: "get_impact",
            guidance: "Expand each high-risk symbol's reverse-dependency blast radius; page callers via next_offset.",
        },
        StepSpec {
            tool: "get_symbol",
            guidance: "Inspect an affected symbol's exact definition and edges.",
        },
        StepSpec {
            tool: "query_graph",
            guidance: "Cross-boundary or multi-hop questions the typed tools do not cover.",
        },
    ]
}

fn verify_steps() -> Vec<StepSpec> {
    vec![
        StepSpec {
            tool: "extract_finding",
            guidance: "Normalize the incoming finding to the canonical schema (no LLM).",
        },
        StepSpec {
            tool: "refine_finding",
            guidance:
                "Persist your orchestrator-produced refined prompt over the extracted finding.",
        },
        StepSpec {
            tool: "start_verification",
            guidance: "Open a clarification session against the refined finding.",
        },
        StepSpec {
            tool: "append_clarification",
            guidance: "Append alternating agent_question / user_answer turns until user-ready.",
        },
        StepSpec {
            tool: "finalize_verification",
            guidance: "Consume the user-ready signal and write the verified receipt.",
        },
    ]
}

fn ground_steps() -> Vec<StepSpec> {
    vec![
        StepSpec {
            tool: "prepare_prd_input",
            guidance: "Bundle matched symbols, impacted communities/processes, and graph stats. \
                       Cite identifiers in backticks for verbatim grounding.",
        },
        StepSpec {
            tool: "validate_prd_against_graph",
            guidance: "After the PRD is generated, check it for hallucinated symbols, \
                       community-consistency, and process contradictions.",
        },
    ]
}

/// Whether `name` is a real prompt AND the active profile registers every tool
/// its workflow calls. Mirrors `ToolProfile::allows` for tools: the profile is
/// the registry, so a prompt whose steps the profile cannot run is not offered.
fn is_available(name: &str, profile: ToolProfile) -> bool {
    match steps_for(name, profile) {
        Some(steps) => !steps.is_empty() && steps.iter().all(|s| profile.allows(s.tool)),
        None => false,
    }
}

fn meta_by_name(name: &str) -> Option<&'static PromptMeta> {
    ALL_PROMPTS.into_iter().find(|m| m.name == name)
}

// ---------------------------------------------------------------------------
// prompts/list
// ---------------------------------------------------------------------------

/// The `prompts/list` result for `profile`: every prompt the profile can run,
/// each with its declared arguments (name/title/description/required).
///
/// Precondition: none. Postcondition: returns `{"prompts": [...]}` containing
/// exactly the prompts for which `is_available(name, profile)` holds, in
/// `ALL_PROMPTS` order.
pub fn prompts_list(profile: ToolProfile) -> Value {
    let prompts: Vec<Value> = ALL_PROMPTS
        .into_iter()
        .filter(|meta| is_available(meta.name, profile))
        .map(prompt_descriptor)
        .collect();
    json!({ "prompts": prompts })
}

fn prompt_descriptor(meta: &PromptMeta) -> Value {
    let arguments: Vec<Value> = meta
        .args
        .iter()
        .map(|arg| {
            json!({
                "name": arg.name,
                "title": arg.title,
                "description": arg.description,
                "required": arg.required,
            })
        })
        .collect();
    json!({
        "name": meta.name,
        "title": meta.title,
        "description": meta.description,
        "arguments": arguments,
    })
}

// ---------------------------------------------------------------------------
// prompts/get
// ---------------------------------------------------------------------------

/// The `prompts/get` result for `params` under `profile`.
///
/// Precondition: `params` is the raw JSON-RPC `params` value.
/// Postcondition: `Ok(result)` with a `messages` array carrying the composed
/// workflow text when the prompt exists, is available under the profile, and
/// every required argument is a non-empty string; otherwise
/// `Err((code, message))` with `code == INVALID_PARAMS` for each failure arm:
///   - `params` is not an object, or `name` is missing/not-a-string;
///   - `name` is unknown or not available under the profile;
///   - `arguments` is present but not an object;
///   - a required argument is missing or not a non-empty string.
pub fn prompt_get(params: &Value, profile: ToolProfile) -> Result<Value, (i32, String)> {
    let params_obj = params.as_object().ok_or((
        INVALID_PARAMS,
        "Invalid params: expected an object".to_string(),
    ))?;

    let name = params_obj.get("name").and_then(Value::as_str).ok_or((
        INVALID_PARAMS,
        "Invalid params: 'name' must be a string".to_string(),
    ))?;

    // Distinguish "no such prompt exists" from "exists but this profile does
    // not register the tools it drives" — the advice differs, so the message
    // must too (§8 honesty). Only the latter is fixed by switching to --full.
    let meta = match meta_by_name(name) {
        None => {
            return Err((INVALID_PARAMS, format!("Unknown prompt: {name}")));
        }
        Some(_) if !is_available(name, profile) => {
            return Err((
                INVALID_PARAMS,
                format!(
                    "Prompt '{name}' is not available under the '{}' profile; \
                     restart with --profile full to expose every prompt",
                    profile.name()
                ),
            ));
        }
        Some(meta) => meta,
    };

    // `arguments` may be absent (all-optional prompt) but never a non-object.
    let arguments = match params_obj.get("arguments") {
        None => &Value::Null,
        Some(v) if v.is_object() => v,
        Some(_) => {
            return Err((
                INVALID_PARAMS,
                "Invalid params: 'arguments' must be an object".to_string(),
            ))
        }
    };

    // Validate every required argument is a present, non-empty string.
    for arg in meta.args {
        if arg.required && string_arg(arguments, arg.name).is_none() {
            return Err((
                INVALID_PARAMS,
                format!("Missing required prompt argument: {}", arg.name),
            ));
        }
    }

    let steps = steps_for(name, profile).expect("available prompt always has steps");
    let text = render_body(name, profile, arguments, &steps);

    Ok(json!({
        "description": meta.description,
        "messages": [{
            "role": "user",
            "content": { "type": "text", "text": text }
        }]
    }))
}

/// A non-empty string argument by name, or `None` when absent/empty/non-string.
fn string_arg<'a>(arguments: &'a Value, name: &str) -> Option<&'a str> {
    arguments
        .get(name)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

// ---------------------------------------------------------------------------
// Body composition — intro + ordered steps (summaries from tool_schemas) + close
// ---------------------------------------------------------------------------

fn render_body(name: &str, profile: ToolProfile, arguments: &Value, steps: &[StepSpec]) -> String {
    let intro = intro_for(name, arguments);
    let closing = closing_for(name);
    let _ = profile; // steps already reflect the profile
    format!(
        "{intro}\n\n\
         AP's tools are strongly ordered — calling them out of order yields an empty or \
         misleading result, not an error. Work them in this order:\n\n\
         {steps}\n\
         {closing}",
        steps = render_steps(steps),
    )
}

fn render_steps(steps: &[StepSpec]) -> String {
    let mut out = String::new();
    for (i, step) in steps.iter().enumerate() {
        // The tool's canonical summary comes from tool_schemas — never copied
        // here. If a tool is renamed out from under a step, the summary lookup
        // returns None and the guarding test fails; we degrade to the raw name
        // rather than emit a stale description.
        let summary =
            tool_schemas::tool_summary(step.tool).unwrap_or_else(|| format!("`{}`", step.tool));
        out.push_str(&format!(
            "{}. `{}` — {} {}\n",
            i + 1,
            step.tool,
            summary,
            step.guidance
        ));
    }
    out
}

fn intro_for(name: &str, arguments: &Value) -> String {
    let arg = |key: &str| string_arg(arguments, key).unwrap_or("");
    match name {
        "explore_codebase" => format!(
            "Explore project \"{}\" to answer: {}",
            arg("project"),
            arg("question")
        ),
        "review_change_impact" => {
            let base = string_arg(arguments, "base_branch").unwrap_or("main");
            format!(
                "Review the impact of \"{}\" in project \"{}\" (base ref: {base}).",
                arg("change"),
                arg("project"),
            )
        }
        "verify_finding" => format!(
            "Run the finding-verification lifecycle for finding \"{}\" in run \"{}\".",
            arg("finding_id"),
            arg("run_id"),
        ),
        "ground_prd" => format!(
            "Ground the PRD input \"{}\" in project \"{}\" against the resolved code graph.",
            arg("target"),
            arg("project"),
        ),
        _ => String::new(),
    }
}

fn closing_for(name: &str) -> &'static str {
    match name {
        "explore_codebase" => {
            "Page any result via next_offset until it is absent. Absence from the graph is \
             NOT proof of absence — a file may be parse-incomplete; verify negatives with \
             query_graph(graph=\"missed\") before concluding a symbol does not exist. Do not \
             modify files."
        }
        "review_change_impact" => {
            "Report affected symbols, callers, tests, communities/processes, and risks. \
             get_impact reports epistemic='lower-bound' when a target is reached via dynamic \
             dispatch — real impact may exceed what is shown. Do not modify files."
        }
        "verify_finding" => {
            "Every step is atomic and persists to disk; append_clarification enforces \
             question/answer alternation and finalize_verification refuses an open or \
             unanswered session. No step calls an LLM — you supply the refined prompt and the \
             clarification turns."
        }
        "ground_prd" => {
            "Only verbatim / exact-name symbol matches count as grounding — cite identifiers \
             in backticks for verbatim priority; an empty matched_symbols is expected and \
             correct when the target cites no real identifiers. Read-only against the graph."
        }
        _ => "",
    }
}

// ---------------------------------------------------------------------------
// Tests — JSON-RPC round-trips at the payload level, per issue #65 criteria.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn names(payload: &Value) -> Vec<String> {
        payload["prompts"]
            .as_array()
            .expect("prompts array")
            .iter()
            .map(|p| p["name"].as_str().unwrap().to_string())
            .collect()
    }

    // Criterion 3 (source of truth): every tool a step names must be a real
    // registered tool. If this fails, a prompt is referencing a tool that
    // tools_list() does not publish — the two would silently disagree.
    #[test]
    fn steps_reference_only_real_tools() {
        for profile in [ToolProfile::Core, ToolProfile::Full] {
            for meta in ALL_PROMPTS {
                if let Some(steps) = steps_for(meta.name, profile) {
                    for step in steps {
                        assert!(
                            tool_schemas::tool_summary(step.tool).is_some(),
                            "prompt '{}' step references unknown tool '{}'",
                            meta.name,
                            step.tool
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn full_profile_lists_all_four_prompts() {
        let listed = names(&prompts_list(ToolProfile::Full));
        assert_eq!(
            listed,
            vec![
                "explore_codebase",
                "review_change_impact",
                "verify_finding",
                "ground_prd"
            ]
        );
    }

    #[test]
    fn core_profile_hides_pipeline_only_prompts() {
        // verify_finding and ground_prd drive stages the core profile does not
        // register, so they must not be offered.
        let listed = names(&prompts_list(ToolProfile::Core));
        assert_eq!(listed, vec!["explore_codebase", "review_change_impact"]);
    }

    #[test]
    fn every_argument_carries_the_four_fields() {
        // Criterion 2: name/title/description/required on every argument.
        let payload = prompts_list(ToolProfile::Full);
        for prompt in payload["prompts"].as_array().unwrap() {
            for arg in prompt["arguments"].as_array().unwrap() {
                assert!(arg["name"].is_string(), "arg name");
                assert!(arg["title"].is_string(), "arg title");
                assert!(arg["description"].is_string(), "arg description");
                assert!(arg["required"].is_boolean(), "arg required");
            }
        }
    }

    #[test]
    fn get_explore_embeds_arguments_and_canonical_summary() {
        let params = json!({
            "name": "explore_codebase",
            "arguments": { "project": "/repo", "question": "how does auth work?" }
        });
        let result = prompt_get(&params, ToolProfile::Full).expect("ok");
        let text = result["messages"][0]["content"]["text"].as_str().unwrap();
        assert!(text.contains("/repo"));
        assert!(text.contains("how does auth work?"));
        // Full profile shows the granular index→resolve→query path.
        assert!(text.contains("`index_codebase`"));
        assert!(text.contains("`resolve_graph`"));
        assert!(text.contains("`query_graph`"));
        // The step summary is the canonical one pulled from tool_schemas.
        let canonical = tool_schemas::tool_summary("index_codebase").unwrap();
        assert!(text.contains(&canonical));
    }

    #[test]
    fn get_explore_core_uses_composed_analyze_path() {
        let params = json!({
            "name": "explore_codebase",
            "arguments": { "project": "/repo", "question": "q" }
        });
        let result = prompt_get(&params, ToolProfile::Core).expect("ok");
        let text = result["messages"][0]["content"]["text"].as_str().unwrap();
        assert!(text.contains("`analyze_codebase`"));
        assert!(!text.contains("`index_codebase`"));
    }

    #[test]
    fn get_review_defaults_base_branch_to_main() {
        let params = json!({
            "name": "review_change_impact",
            "arguments": { "project": "/repo", "change": "rename Foo" }
        });
        let result = prompt_get(&params, ToolProfile::Full).expect("ok");
        let text = result["messages"][0]["content"]["text"].as_str().unwrap();
        assert!(text.contains("base ref: main"));
        assert!(text.contains("`detect_changes`"));
    }

    #[test]
    fn get_verify_and_ground_available_under_full() {
        for name in ["verify_finding", "ground_prd"] {
            let params = json!({
                "name": name,
                "arguments": { "project": "/repo", "target": "t", "run_id": "r", "finding_id": "f" }
            });
            assert!(prompt_get(&params, ToolProfile::Full).is_ok(), "{name}");
        }
    }

    // --- Failure arms (criterion 7 / §13 A3) ---

    #[test]
    fn get_unknown_prompt_name_is_invalid_params() {
        let params = json!({ "name": "no_such_prompt", "arguments": {} });
        let (code, msg) = prompt_get(&params, ToolProfile::Full).unwrap_err();
        assert_eq!(code, INVALID_PARAMS);
        assert_eq!(msg, "Unknown prompt: no_such_prompt");
    }

    #[test]
    fn get_pipeline_prompt_under_core_is_unavailable() {
        let params = json!({
            "name": "verify_finding",
            "arguments": { "run_id": "r", "finding_id": "f" }
        });
        let (code, msg) = prompt_get(&params, ToolProfile::Core).unwrap_err();
        assert_eq!(code, INVALID_PARAMS);
        assert!(msg.contains("not available under the 'core' profile"));
        assert!(msg.contains("verify_finding"));
    }

    #[test]
    fn get_missing_required_argument_is_invalid_params() {
        let params = json!({
            "name": "explore_codebase",
            "arguments": { "project": "/repo" }
        });
        let (code, msg) = prompt_get(&params, ToolProfile::Full).unwrap_err();
        assert_eq!(code, INVALID_PARAMS);
        assert!(msg.contains("question"));
    }

    #[test]
    fn get_empty_required_argument_is_rejected() {
        let params = json!({
            "name": "explore_codebase",
            "arguments": { "project": "/repo", "question": "" }
        });
        let (code, _) = prompt_get(&params, ToolProfile::Full).unwrap_err();
        assert_eq!(code, INVALID_PARAMS);
    }

    #[test]
    fn get_malformed_params_are_rejected() {
        // params not an object
        let (code, _) = prompt_get(&json!("nope"), ToolProfile::Full).unwrap_err();
        assert_eq!(code, INVALID_PARAMS);
        // name not a string
        let (code, _) = prompt_get(&json!({ "name": 7 }), ToolProfile::Full).unwrap_err();
        assert_eq!(code, INVALID_PARAMS);
        // arguments not an object
        let bad_args = json!({ "name": "explore_codebase", "arguments": "x" });
        let (code, _) = prompt_get(&bad_args, ToolProfile::Full).unwrap_err();
        assert_eq!(code, INVALID_PARAMS);
    }
}
