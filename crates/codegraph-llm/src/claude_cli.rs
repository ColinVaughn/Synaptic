//! claude-CLI backend: route extraction through the locally-installed Claude Code
//! CLI (`claude -p --output-format json`) instead of a pay-as-you-go API key, so
//! Pro/Max subscribers can run the semantic pass on their plan.
//!
//! The CLI invocation is injected as a closure (`runner`) so the envelope-parsing
//! logic — the part with real behavior — is unit-testable without the CLI on
//! PATH. The default runner shells out to `claude`.

use async_trait::async_trait;
use serde_json::Value;

use crate::error::LlmError;
use crate::provider::{Completion, LlmClient};

/// `(system_prompt, user_message) -> stdout` — runs the CLI and returns its raw
/// stdout (the JSON envelope), or an error.
type CliRunner = Box<dyn Fn(&str, &str) -> Result<String, LlmError> + Send + Sync>;

/// claude-CLI client. Default model is whatever `claude` is configured to use;
/// `CLAUDE_CLI_MODEL` (e.g. `haiku`) overrides it.
pub struct ClaudeCli {
    model: Option<String>,
    runner: CliRunner,
}

impl Default for ClaudeCli {
    fn default() -> Self {
        Self::new()
    }
}

impl ClaudeCli {
    /// A client that shells out to the real `claude` CLI.
    pub fn new() -> Self {
        ClaudeCli {
            model: None,
            runner: Box::new(run_claude_cli),
        }
    }

    /// Override the model passed to `claude --model` (e.g. `haiku`, `sonnet`, or a
    /// full model id). An empty string is ignored.
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        let m = model.into();
        self.model = (!m.trim().is_empty()).then_some(m);
        self
    }

    /// Inject a custom runner (used by tests to supply a canned envelope).
    pub fn with_runner(
        mut self,
        runner: impl Fn(&str, &str) -> Result<String, LlmError> + Send + Sync + 'static,
    ) -> Self {
        self.runner = Box::new(runner);
        self
    }
}

#[async_trait]
impl LlmClient for ClaudeCli {
    async fn complete(&self, system: &str, user: &str) -> Result<Completion, LlmError> {
        let stdout = (self.runner)(system, user)?;
        parse_cli_envelope(&stdout)
    }
}

/// Parse the JSON `claude -p --output-format json` writes to stdout into a
/// [`Completion`].
///
/// Older CLI versions emit a single envelope object; newer ones (>= ~2.1) emit a
/// JSON *array* of streamed events ending in a `{"type":"result"}` object. Both
/// shapes are normalized. `usage` sums the plain + cache-read + cache-creation
/// input tokens.
pub fn parse_cli_envelope(stdout: &str) -> Result<Completion, LlmError> {
    let parsed: Value = serde_json::from_str(stdout)
        .map_err(|e| LlmError::BadResponse(format!("claude -p produced unparseable JSON: {e}")))?;
    // Normalize the array shape (stream of events) to the single result object.
    let envelope = match &parsed {
        Value::Array(events) => events
            .iter()
            .rev()
            .find(|e| e.get("type").and_then(Value::as_str) == Some("result"))
            .or_else(|| events.last())
            .cloned()
            .ok_or_else(|| LlmError::BadResponse("claude -p returned an empty array".into()))?,
        _ => parsed.clone(),
    };

    let content = envelope
        .get("result")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let usage = envelope.get("usage");
    let finish_reason = match envelope.get("stop_reason").and_then(Value::as_str) {
        Some("max_tokens") => "length",
        _ => "stop",
    }
    .to_string();
    Ok(Completion {
        content,
        finish_reason,
        input_tokens: sum_input_tokens(usage) as u32,
        output_tokens: usage
            .and_then(|u| u.get("output_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32,
    })
}

/// Default runner: invoke the `claude` CLI. Not exercised by unit tests (needs
/// the CLI installed + authenticated); the envelope parser carries the logic.
fn run_claude_cli(system: &str, user: &str) -> Result<String, LlmError> {
    use std::process::Command;

    // On Windows npm installs both `claude.ps1` and `claude.cmd`; CreateProcess
    // can run `.cmd` but not `.ps1`, so prefer `claude.cmd` there.
    let program = if cfg!(windows) {
        which("claude.cmd")
            .or_else(|| which("claude"))
            .ok_or_else(claude_not_found)?
    } else {
        which("claude").ok_or_else(claude_not_found)?
    };

    let mut cmd = Command::new(program);
    cmd.arg("-p")
        .args(["--output-format", "json"])
        .arg("--no-session-persistence")
        .args(["--system-prompt", system]);
    if let Ok(model) = std::env::var("CLAUDE_CLI_MODEL") {
        if !model.trim().is_empty() {
            cmd.args(["--model", model.trim()]);
        }
    }
    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let mut child = cmd.spawn().map_err(LlmError::Io)?;
    // Write stdin on a separate thread so the parent drains stdout/stderr
    // concurrently. Writing the whole prompt up front and only then reading would
    // deadlock once the prompt and the child's output both exceed the OS pipe
    // buffers (~64 KB), a real risk for tens-of-KB extraction prompts.
    let writer = child.stdin.take().map(|mut stdin| {
        let payload = user.as_bytes().to_vec();
        std::thread::spawn(move || {
            use std::io::Write as _;
            // Ignore write errors: if the child exits early it closes the pipe
            // (broken pipe), which surfaces via the non-zero exit status below.
            let _ = stdin.write_all(&payload);
            // `stdin` drops here, sending EOF.
        })
    });
    let out = child.wait_with_output().map_err(LlmError::Io)?;
    if let Some(w) = writer {
        let _ = w.join();
    }
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(LlmError::BadResponse(format!(
            "claude -p exited {}: {}",
            out.status,
            stderr.trim().chars().take(500).collect::<String>()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn which(name: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    let exts: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".EXE;.CMD;.BAT".into())
            .split(';')
            .map(|s| s.to_string())
            .collect()
    } else {
        vec![String::new()]
    };
    for dir in std::env::split_paths(&path) {
        // If `name` already has an extension, try it verbatim first.
        let direct = dir.join(name);
        if direct.is_file() {
            return Some(direct);
        }
        for ext in &exts {
            let cand = dir.join(format!("{name}{ext}"));
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    None
}

fn claude_not_found() -> LlmError {
    LlmError::BadResponse(
        "Claude Code CLI not found on PATH. Install from https://claude.ai/code and run \
         `claude` once to authenticate."
            .into(),
    )
}

/// Sum the three input-token fields the CLI reports (plain + cache read + cache
/// creation), defaulting missing fields to 0.
fn sum_input_tokens(usage: Option<&Value>) -> u64 {
    let tok = |k: &str| {
        usage
            .and_then(|u| u.get(k))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    tok("input_tokens") + tok("cache_read_input_tokens") + tok("cache_creation_input_tokens")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_single_object_envelope() {
        let stdout = json!({
            "type": "result",
            "result": "{\"nodes\": []}",
            "usage": {"input_tokens": 10, "output_tokens": 3},
            "stop_reason": "end_turn"
        })
        .to_string();
        let c = parse_cli_envelope(&stdout).unwrap();
        assert_eq!(c.content, "{\"nodes\": []}");
        assert_eq!(c.input_tokens, 10);
        assert_eq!(c.output_tokens, 3);
        assert_eq!(c.finish_reason, "stop");
    }

    #[test]
    fn parses_array_envelope_picking_the_result_event() {
        // Newer CLI: a stream of events ending in the result object.
        let stdout = json!([
            {"type": "system", "subtype": "init"},
            {"type": "assistant", "message": {"content": "…"}},
            {"type": "result", "result": "{\"nodes\":[{\"id\":\"x\"}]}",
             "usage": {"output_tokens": 5}, "stop_reason": "end_turn"}
        ])
        .to_string();
        let c = parse_cli_envelope(&stdout).unwrap();
        assert!(c.content.contains("\"id\":\"x\""));
        assert_eq!(c.output_tokens, 5);
    }

    #[test]
    fn input_tokens_sum_includes_cache_reads() {
        let stdout = json!({
            "result": "{}",
            "usage": {
                "input_tokens": 4,
                "cache_read_input_tokens": 100,
                "cache_creation_input_tokens": 20,
                "output_tokens": 2
            }
        })
        .to_string();
        let c = parse_cli_envelope(&stdout).unwrap();
        assert_eq!(c.input_tokens, 124, "plain + cache_read + cache_creation");
        assert_eq!(c.output_tokens, 2);
    }

    #[test]
    fn max_tokens_stop_reason_maps_to_length() {
        let stdout =
            json!({"result": "{\"nodes\":[partial", "stop_reason": "max_tokens"}).to_string();
        let c = parse_cli_envelope(&stdout).unwrap();
        assert_eq!(c.finish_reason, "length");
        assert!(c.is_truncated());
    }

    #[test]
    fn unparseable_stdout_errors() {
        assert!(parse_cli_envelope("not json at all").is_err());
    }

    #[tokio::test]
    async fn complete_runs_the_injected_runner_and_parses_its_output() {
        let client = ClaudeCli::new().with_runner(|system, user| {
            // Echo the inputs back inside a valid envelope so we can assert wiring.
            Ok(json!({
                "result": format!("{{\"sys\":\"{system}\",\"usr\":\"{user}\"}}"),
                "usage": {"input_tokens": 7, "output_tokens": 1},
                "stop_reason": "end_turn"
            })
            .to_string())
        });
        let c = client.complete("SYS", "USR").await.unwrap();
        assert!(c.content.contains("\"sys\":\"SYS\""));
        assert!(c.content.contains("\"usr\":\"USR\""));
        assert_eq!(c.input_tokens, 7);
    }

    #[tokio::test]
    async fn complete_propagates_runner_errors() {
        let client = ClaudeCli::new()
            .with_runner(|_, _| Err(LlmError::BadResponse("claude -p exited 1".into())));
        assert!(client.complete("s", "u").await.is_err());
    }
}
