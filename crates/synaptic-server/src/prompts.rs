//! MCP prompts: user-selectable, parameterized workflows over the graph tools.

use serde_json::{json, Value};
use synaptic_core::sanitize_label;

/// The `prompts/list` payload.
pub fn prompts_list() -> Value {
    json!([
        { "name": "onboard", "description": "Get oriented in this codebase fast.", "arguments": [] },
        { "name": "explain_subsystem", "description": "Explain how a subsystem works.",
          "arguments": [{ "name": "topic", "description": "Subsystem or feature, e.g. 'authentication'.", "required": true }] },
        { "name": "assess_pr", "description": "Assess a pull request's risk via graph blast radius.",
          "arguments": [{ "name": "pr_number", "description": "PR number.", "required": true }] },
        { "name": "trace_flow", "description": "Trace the path between two symbols.",
          "arguments": [
            { "name": "from", "description": "Start symbol.", "required": true },
            { "name": "to", "description": "End symbol.", "required": true }] }
    ])
}

/// Build a `prompts/get` response, or `None` for an unknown name.
pub fn prompts_get(name: &str, args: &Value) -> Option<Value> {
    let arg = |k: &str| {
        args.get(k)
            .and_then(Value::as_str)
            .map(sanitize_label)
            .unwrap_or_default()
    };
    let text = match name {
        "onboard" => "Orient me in this codebase. Call graph_stats, then god_nodes, then read \
            synaptic://questions, and summarize the main subsystems and entry points."
            .to_string(),
        "explain_subsystem" => format!(
            "Explain how the '{}' subsystem works. Use query_graph for it, then get_source on the \
             key symbols, and find_callers/find_callees to map the flow.",
            arg("topic")
        ),
        "assess_pr" => format!(
            "Assess the risk of PR #{}. Call get_pr_impact, then affected on the changed symbols, \
             and summarize the blast radius and what to review.",
            arg("pr_number")
        ),
        "trace_flow" => format!(
            "Trace how '{}' reaches '{}'. Call shortest_path, then get_source on each hop.",
            arg("from"),
            arg("to")
        ),
        _ => return None,
    };
    Some(json!({ "messages": [{ "role": "user", "content": { "type": "text", "text": text } }] }))
}
