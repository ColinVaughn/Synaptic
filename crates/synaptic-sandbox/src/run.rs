//! Running a single command in the sandbox: a platform-shell invocation with a
//! wall-clock timeout and bounded output capture. The orchestration in
//! `speculate.rs` calls this for the build/check and each at-risk test.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

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
    pub duration_ms: u128,
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
fn lock_clone(buf: &Arc<Mutex<Vec<u8>>>) -> Vec<u8> {
    buf.lock().map(|b| b.clone()).unwrap_or_default()
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
    let (sh, flag) = shell();
    let started = Instant::now();
    let child = Command::new(sh)
        .arg(flag)
        .arg(command)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();

    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            return CommandResult {
                label: label.to_string(),
                command: command.to_string(),
                status: CommandStatus::Failed,
                exit_code: None,
                output: format!("failed to spawn `{command}`: {e}"),
                duration_ms: started.elapsed().as_millis(),
            };
        }
    };

    // Drain stdout and stderr on their own threads, appending into shared buffers
    // as bytes arrive, so a chatty command can't deadlock against a full pipe
    // buffer while we poll for the timeout, and so partial output is readable
    // even if we never join the threads.
    let stdout_buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    let stderr_buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    let spawn_drain = |pipe: Option<Box<dyn Read + Send>>, buf: Arc<Mutex<Vec<u8>>>| {
        std::thread::spawn(move || {
            if let Some(mut p) = pipe {
                let mut chunk = [0u8; 4096];
                loop {
                    match p.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if let Ok(mut b) = buf.lock() {
                                b.extend_from_slice(&chunk[..n]);
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
        command: command.to_string(),
        status,
        exit_code,
        output: tail_lines(&combined, max_output_lines),
        duration_ms: started.elapsed().as_millis(),
    }
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
}
