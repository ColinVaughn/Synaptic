//! Running a single command in the sandbox: a platform-shell invocation with a
//! wall-clock timeout and bounded output capture. The orchestration in
//! `speculate.rs` calls this for the build/check and each at-risk test.

use std::collections::VecDeque;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

const MAX_CAPTURE_BYTES: usize = 2 * 1024 * 1024;

/// The outcome of running one command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandStatus {
    /// Exited 0.
    Passed,
    /// Exited non-zero.
    Failed,
    /// Killed after exceeding the wall-clock budget.
    TimedOut,
    /// Not run (no command, or a prior step short-circuited the run).
    Skipped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkPolicy {
    Disabled,
    Allow,
}

/// Process isolation controls. Network denial is fail-closed: a disabled
/// policy without a platform/worker guard does not execute the command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionPolicy {
    pub network: NetworkPolicy,
    /// Guard argv placed before the platform shell, e.g. `unshare -Urn --`.
    pub network_guard: Option<Vec<String>>,
    pub scrub_credentials: bool,
}

impl Default for ExecutionPolicy {
    fn default() -> Self {
        Self {
            network: NetworkPolicy::Disabled,
            network_guard: None,
            scrub_credentials: true,
        }
    }
}

/// The result of running one command in the sandbox.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommandResult {
    /// What this command represents (e.g. "build" or a test file path).
    pub label: String,
    /// The command line that was run.
    pub command: String,
    pub status: CommandStatus,
    /// Process exit code, if it exited on its own (not timed out / skipped).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub exit_code: Option<i32>,
    /// A bounded tail of combined stdout+stderr (the part an agent reads to see
    /// why it failed).
    pub output: String,
    /// Wall-clock duration in milliseconds (non-deterministic; informational).
    pub duration_ms: u64,
}

impl CommandResult {
    /// A result for a command that was never run.
    pub fn skipped(label: &str, reason: &str) -> CommandResult {
        CommandResult {
            label: label.to_string(),
            command: String::new(),
            status: CommandStatus::Skipped,
            exit_code: None,
            output: reason.to_string(),
            duration_ms: 0,
        }
    }
}

/// Substitute the `{files}` placeholder in a command template with the given
/// files (space-joined). A template with no placeholder is returned unchanged, so
/// a whole-suite command like `cargo test` runs as-is.
pub fn expand_template(template: &str, files: &[String]) -> String {
    if template.contains("{files}") {
        template.replace("{files}", &files.join(" "))
    } else {
        template.to_string()
    }
}

/// Keep only the last `max_lines` lines of `s` (the tail is where a failure
/// message lands). A leading marker notes how many lines were dropped so the
/// reader knows the output was truncated.
pub fn tail_lines(s: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    if lines.len() <= max_lines {
        return s.trim_end().to_string();
    }
    let dropped = lines.len() - max_lines;
    let mut out = format!("... ({dropped} earlier line(s) omitted)\n");
    out.push_str(&lines[lines.len() - max_lines..].join("\n"));
    out
}

/// The platform shell and the flag that runs a command string through it.
fn shell() -> (&'static str, &'static str) {
    if cfg!(windows) {
        ("cmd", "/C")
    } else {
        ("sh", "-c")
    }
}

/// Snapshot a shared output buffer without holding the lock past the clone.
fn lock_clone(buf: &Arc<Mutex<VecDeque<u8>>>) -> Vec<u8> {
    buf.lock()
        .map(|bytes| bytes.iter().copied().collect())
        .unwrap_or_default()
}

fn bounded_append(buffer: &mut VecDeque<u8>, bytes: &[u8]) {
    if bytes.len() >= MAX_CAPTURE_BYTES {
        buffer.clear();
        buffer.extend(&bytes[bytes.len() - MAX_CAPTURE_BYTES..]);
        return;
    }
    let overflow = buffer
        .len()
        .saturating_add(bytes.len())
        .saturating_sub(MAX_CAPTURE_BYTES);
    if overflow > 0 {
        buffer.drain(..overflow);
    }
    buffer.extend(bytes);
}

/// Run `command` (a shell command line) in `dir`, killing it after `timeout` and
/// capturing a bounded tail of its combined output. `label` describes the step.
pub fn run_command(
    label: &str,
    command: &str,
    dir: &Path,
    timeout: Duration,
    max_output_lines: usize,
) -> CommandResult {
    run_command_with_policy(
        label,
        command,
        dir,
        timeout,
        max_output_lines,
        &ExecutionPolicy {
            network: NetworkPolicy::Allow,
            network_guard: None,
            scrub_credentials: false,
        },
    )
}

pub fn run_command_with_policy(
    label: &str,
    command: &str,
    dir: &Path,
    timeout: Duration,
    max_output_lines: usize,
    policy: &ExecutionPolicy,
) -> CommandResult {
    let (sh, flag) = shell();
    let started = Instant::now();
    let display_command = if policy.scrub_credentials {
        redact_command_output(command)
    } else {
        command.to_string()
    };
    if policy.network == NetworkPolicy::Disabled && policy.network_guard.is_none() {
        return CommandResult::skipped(
            label,
            "network isolation is required but no platform guard is configured",
        );
    }
    let mut process = if let Some(guard) = policy.network_guard.as_ref() {
        if guard.is_empty() {
            return CommandResult::skipped(label, "configured network guard is empty");
        }
        let mut process = Command::new(&guard[0]);
        process.args(&guard[1..]).arg(sh).arg(flag).arg(command);
        process
    } else {
        let mut process = Command::new(sh);
        process.arg(flag).arg(command);
        process
    };
    process
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if policy.scrub_credentials {
        let safe_environment = [
            "PATH",
            "PATHEXT",
            "SystemRoot",
            "WINDIR",
            "COMSPEC",
            "TEMP",
            "TMP",
            "TMPDIR",
            "LANG",
            "LC_ALL",
            "CARGO_HOME",
            "RUSTUP_HOME",
        ];
        let values = safe_environment
            .iter()
            .filter_map(|name| std::env::var_os(name).map(|value| (*name, value)))
            .collect::<Vec<_>>();
        process.env_clear().envs(values);
        process.env(
            "SYNAPTIC_NETWORK",
            match policy.network {
                NetworkPolicy::Disabled => "disabled",
                NetworkPolicy::Allow => "allowed",
            },
        );
    }
    let child = process.spawn();

    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            return CommandResult {
                label: label.to_string(),
                command: display_command,
                status: CommandStatus::Failed,
                exit_code: None,
                output: format!("failed to spawn `{}`: {e}", redact_command_output(command)),
                duration_ms: elapsed_millis(started.elapsed()),
            };
        }
    };

    // Drain stdout and stderr on their own threads, appending into shared buffers
    // as bytes arrive, so a chatty command can't deadlock against a full pipe
    // buffer while we poll for the timeout, and so partial output is readable
    // even if we never join the threads.
    let stdout_buf = Arc::new(Mutex::new(VecDeque::<u8>::new()));
    let stderr_buf = Arc::new(Mutex::new(VecDeque::<u8>::new()));
    let spawn_drain = |pipe: Option<Box<dyn Read + Send>>, buf: Arc<Mutex<VecDeque<u8>>>| {
        std::thread::spawn(move || {
            if let Some(mut p) = pipe {
                let mut chunk = [0u8; 4096];
                loop {
                    match p.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if let Ok(mut b) = buf.lock() {
                                bounded_append(&mut b, &chunk[..n]);
                            }
                        }
                    }
                }
            }
        })
    };
    let out_h = spawn_drain(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
        Arc::clone(&stdout_buf),
    );
    let err_h = spawn_drain(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
        Arc::clone(&stderr_buf),
    );

    // Poll for completion until the deadline, then kill.
    let mut timed_out = false;
    let exit_status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if started.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    timed_out = true;
                    break None;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => break None,
        }
    };

    // On a clean exit, join the drains so the captured output is complete. On a
    // timeout do NOT join: a grandchild the shell spawned can hold an inherited
    // pipe open after we kill the shell, which would block us until it exits.
    // Read what was captured so far and move on; the detached drain ends when the
    // pipe finally closes. This leaks at most two threads per timed-out command
    // (bounded by the number of timeouts), each idle on a blocking read.
    if !timed_out {
        let _ = out_h.join();
        let _ = err_h.join();
    }
    let mut combined = String::from_utf8_lossy(&lock_clone(&stdout_buf)).into_owned();
    let err = lock_clone(&stderr_buf);
    if !err.is_empty() {
        if !combined.is_empty() && !combined.ends_with('\n') {
            combined.push('\n');
        }
        combined.push_str(&String::from_utf8_lossy(&err));
    }

    let (status, exit_code) = if timed_out {
        (CommandStatus::TimedOut, None)
    } else {
        match exit_status {
            Some(s) if s.success() => (CommandStatus::Passed, s.code()),
            Some(s) => (CommandStatus::Failed, s.code()),
            None => (CommandStatus::Failed, None),
        }
    };

    CommandResult {
        label: label.to_string(),
        command: display_command,
        status,
        exit_code,
        output: tail_lines(&redact_command_output(&combined), max_output_lines),
        duration_ms: elapsed_millis(started.elapsed()),
    }
}

fn elapsed_millis(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

fn redact_command_output(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut in_private_key = false;
    for line in input.split_inclusive('\n') {
        let has_newline = line.ends_with('\n');
        let content = line.strip_suffix('\n').unwrap_or(line);
        let lowercase = content.to_ascii_lowercase();
        if lowercase.contains("-----begin ") && lowercase.contains("private key-----") {
            in_private_key = true;
            output.push_str("[REDACTED PRIVATE KEY]");
        } else if in_private_key {
            if lowercase.contains("-----end ") && lowercase.contains("private key-----") {
                in_private_key = false;
            }
        } else {
            output.push_str(&redact_command_line(content));
        }
        if has_newline {
            output.push('\n');
        }
    }
    output
}

fn redact_command_line(input: &str) -> String {
    let lowercase_line = input.to_ascii_lowercase();
    if let Some(start) = lowercase_line.find("authorization:") {
        let value_start = start + "authorization:".len();
        return format!("{}[REDACTED]", &input[..value_start]);
    }
    input
        .split_inclusive(char::is_whitespace)
        .map(|chunk| {
            let token = chunk.trim_end_matches(char::is_whitespace);
            let suffix = &chunk[token.len()..];
            let lowercase = token.to_ascii_lowercase();
            if [
                "sk_live_",
                "sk_test_",
                "rk_live_",
                "rk_test_",
                "pk_live_",
                "pk_test_",
                "whsec_",
                "ghp_",
                "github_pat_",
                "akia",
            ]
            .iter()
            .any(|marker| lowercase.contains(marker))
            {
                return format!("[REDACTED]{suffix}");
            }
            for marker in [
                "password=",
                "password:",
                "client_secret=",
                "api_key=",
                "token=",
                "secret=",
            ] {
                if let Some(start) = lowercase.find(marker) {
                    let value_start = start + marker.len();
                    if value_start < token.len() {
                        return format!("{}[REDACTED]{suffix}", &token[..value_start]);
                    }
                }
            }
            chunk.to_string()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_template_substitutes_files() {
        let cmd = expand_template("pytest {files}", &["a_test.py".into(), "b_test.py".into()]);
        assert_eq!(cmd, "pytest a_test.py b_test.py");
    }

    #[test]
    fn expand_template_without_placeholder_is_unchanged() {
        assert_eq!(expand_template("cargo test", &["x".into()]), "cargo test");
    }

    #[test]
    fn tail_lines_keeps_the_last_lines() {
        let s = "1\n2\n3\n4\n5";
        let t = tail_lines(s, 2);
        assert!(t.contains("4\n5"), "{t}");
        assert!(t.contains("omitted"), "notes truncation: {t}");
        assert!(!t.contains("\n1"), "early lines dropped: {t}");
    }

    #[test]
    fn tail_lines_short_input_unchanged() {
        assert_eq!(tail_lines("a\nb", 10), "a\nb");
    }

    #[test]
    fn run_command_reports_success() {
        let tmp = tempfile::tempdir().unwrap();
        // git is a guaranteed dependency of this whole crate, so it is a portable
        // command that exists on every machine the tests run on.
        let r = run_command(
            "check",
            "git --version",
            tmp.path(),
            Duration::from_secs(30),
            50,
        );
        assert_eq!(r.status, CommandStatus::Passed, "{r:?}");
        assert_eq!(r.exit_code, Some(0));
        assert!(r.output.to_lowercase().contains("git"), "{r:?}");
    }

    #[test]
    fn run_command_reports_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let r = run_command(
            "check",
            "git definitely-not-a-real-subcommand",
            tmp.path(),
            Duration::from_secs(30),
            50,
        );
        assert_eq!(r.status, CommandStatus::Failed, "{r:?}");
        assert_ne!(r.exit_code, Some(0));
    }

    #[test]
    fn run_command_times_out_a_long_command() {
        let tmp = tempfile::tempdir().unwrap();
        // A command that sleeps well past the 1s budget, expressed per-shell.
        let slow = if cfg!(windows) {
            "ping -n 6 127.0.0.1 >NUL"
        } else {
            "sleep 5"
        };
        let r = run_command("slow", slow, tmp.path(), Duration::from_secs(1), 50);
        assert_eq!(r.status, CommandStatus::TimedOut, "{r:?}");
        assert!(r.duration_ms < 4000, "killed promptly, not after 5s: {r:?}");
    }

    #[test]
    fn disabled_network_without_guard_fails_closed_before_execution() {
        let tmp = tempfile::tempdir().unwrap();
        let marker = tmp.path().join("should-not-exist");
        let command = if cfg!(windows) {
            format!("type nul > {}", marker.display())
        } else {
            format!("touch {}", marker.display())
        };
        let result = run_command_with_policy(
            "guarded",
            &command,
            tmp.path(),
            Duration::from_secs(5),
            10,
            &ExecutionPolicy::default(),
        );
        assert_eq!(result.status, CommandStatus::Skipped);
        assert!(!marker.exists());
    }

    #[test]
    fn restricted_environment_scrubs_inherited_credentials() {
        let tmp = tempfile::tempdir().unwrap();
        let key = if cfg!(windows) { "USERNAME" } else { "USER" };
        let inherited = std::env::var(key).unwrap_or_default();
        let command = if cfg!(windows) {
            format!("echo %{key}%")
        } else {
            format!("printf '%s' \"${key}\"")
        };
        let result = run_command_with_policy(
            "scrub",
            &command,
            tmp.path(),
            Duration::from_secs(5),
            10,
            &ExecutionPolicy {
                network: NetworkPolicy::Allow,
                network_guard: None,
                scrub_credentials: true,
            },
        );
        assert_eq!(result.status, CommandStatus::Passed, "{result:?}");
        if !inherited.is_empty() {
            assert!(!result.output.contains(&inherited));
        }
    }

    #[test]
    fn restricted_command_output_redacts_secret_values() {
        let tmp = tempfile::tempdir().unwrap();
        let command = if cfg!(windows) {
            "echo token=command-secret"
        } else {
            "printf 'token=command-secret'"
        };
        let result = run_command_with_policy(
            "secret-output",
            command,
            tmp.path(),
            Duration::from_secs(5),
            10,
            &ExecutionPolicy {
                network: NetworkPolicy::Allow,
                network_guard: None,
                scrub_credentials: true,
            },
        );
        assert_eq!(result.status, CommandStatus::Passed, "{result:?}");
        assert!(!result.output.contains("command-secret"));
        assert!(!result.command.contains("command-secret"));
        assert!(result.output.contains("[REDACTED]"));
    }

    #[test]
    fn redaction_removes_authorization_values_and_complete_private_keys() {
        let output = "Authorization: Bearer bearer-secret\npk_live_browser-key\n-----BEGIN PRIVATE KEY-----\nprivate-material\n-----END PRIVATE KEY-----\nsafe";
        let redacted = redact_command_output(output);
        assert!(!redacted.contains("bearer-secret"));
        assert!(!redacted.contains("browser-key"));
        assert!(!redacted.contains("private-material"));
        assert!(redacted.contains("Authorization:[REDACTED]"));
        assert!(redacted.ends_with("safe"));
    }

    #[test]
    fn output_capture_keeps_a_bounded_tail() {
        let mut buffer = VecDeque::new();
        bounded_append(&mut buffer, &vec![b'a'; MAX_CAPTURE_BYTES]);
        bounded_append(&mut buffer, b"failure-tail");
        assert_eq!(buffer.len(), MAX_CAPTURE_BYTES);
        assert!(buffer
            .iter()
            .rev()
            .take(12)
            .copied()
            .collect::<Vec<_>>()
            .starts_with(b"liat"));
        let tail = buffer.iter().rev().take(12).copied().collect::<Vec<_>>();
        let restored = tail.into_iter().rev().collect::<Vec<_>>();
        assert_eq!(restored, b"failure-tail");
    }
}
