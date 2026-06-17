//! Best-effort detection of a project's test and build/check commands from the
//! marker files at its root. Detection is a convenience: every command can be
//! overridden explicitly, and a project with no recognized markers degrades to
//! "no command" (the run reports it as skipped rather than guessing wrong).

use serde::{Deserialize, Serialize};

/// The commands detected for a project. `test` may contain a `{files}`
/// placeholder that the runner expands to the at-risk test files.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DetectedCommands {
    /// The ecosystem the markers identified (e.g. "rust"), for reporting.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub language: Option<String>,
    /// A command that runs the tests (possibly file-scoped via `{files}`).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub test: Option<String>,
    /// A command that builds / type-checks the project.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub check: Option<String>,
}

fn has(files: &[String], name: &str) -> bool {
    files.iter().any(|f| f == name)
}

/// Detect test/check commands from the marker file names present at the repo
/// root. The first ecosystem (in a fixed priority order, for determinism) whose
/// marker is present wins; ties never depend on input order.
pub fn detect_commands(root_files: &[String]) -> DetectedCommands {
    // Rust.
    if has(root_files, "Cargo.toml") {
        return DetectedCommands {
            language: Some("rust".into()),
            test: Some("cargo test".into()),
            check: Some("cargo build".into()),
        };
    }
    // Go.
    if has(root_files, "go.mod") {
        return DetectedCommands {
            language: Some("go".into()),
            test: Some("go test ./...".into()),
            check: Some("go build ./...".into()),
        };
    }
    // Python: pytest over the at-risk files; no separate build step.
    if has(root_files, "pyproject.toml")
        || has(root_files, "setup.py")
        || has(root_files, "pytest.ini")
        || has(root_files, "tox.ini")
    {
        return DetectedCommands {
            language: Some("python".into()),
            test: Some("pytest {files}".into()),
            check: None,
        };
    }
    // Node / TypeScript. A tsconfig adds a type-check step.
    if has(root_files, "package.json") {
        let check = has(root_files, "tsconfig.json").then(|| "npx tsc --noEmit".to_string());
        return DetectedCommands {
            language: Some("node".into()),
            test: Some("npm test".into()),
            check,
        };
    }
    DetectedCommands::default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn detects_rust() {
        let d = detect_commands(&files(&["Cargo.toml", "src"]));
        assert_eq!(d.language.as_deref(), Some("rust"));
        assert_eq!(d.test.as_deref(), Some("cargo test"));
        assert_eq!(d.check.as_deref(), Some("cargo build"));
    }

    #[test]
    fn detects_python_pytest_with_file_placeholder() {
        let d = detect_commands(&files(&["pyproject.toml"]));
        assert_eq!(d.language.as_deref(), Some("python"));
        assert_eq!(d.test.as_deref(), Some("pytest {files}"));
        assert!(d.check.is_none());
    }

    #[test]
    fn detects_go() {
        let d = detect_commands(&files(&["go.mod", "main.go"]));
        assert_eq!(d.language.as_deref(), Some("go"));
        assert_eq!(d.test.as_deref(), Some("go test ./..."));
    }

    #[test]
    fn node_check_only_with_tsconfig() {
        let plain = detect_commands(&files(&["package.json"]));
        assert_eq!(plain.test.as_deref(), Some("npm test"));
        assert!(plain.check.is_none(), "no tsconfig -> no type-check");
        let ts = detect_commands(&files(&["package.json", "tsconfig.json"]));
        assert_eq!(ts.check.as_deref(), Some("npx tsc --noEmit"));
    }

    #[test]
    fn rust_wins_over_node_for_determinism() {
        // A polyglot repo root: the fixed priority order picks rust regardless of
        // the order the file names arrive in.
        let d = detect_commands(&files(&["package.json", "Cargo.toml"]));
        assert_eq!(d.language.as_deref(), Some("rust"));
    }

    #[test]
    fn nothing_detected_is_empty() {
        let d = detect_commands(&files(&["README.md"]));
        assert_eq!(d, DetectedCommands::default());
    }
}
