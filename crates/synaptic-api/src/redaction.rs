use std::sync::OnceLock;

use regex::{Captures, Regex};

/// Redact credential-shaped values before repository or vendor text crosses an
/// agent, log, memory, or pull-request boundary.
pub(crate) fn redact_sensitive_text(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut in_private_key = false;
    for line in input.split_inclusive('\n') {
        let has_newline = line.ends_with('\n');
        let content = line.strip_suffix('\n').unwrap_or(line);
        let lowercase = content.to_ascii_lowercase();
        if lowercase.contains("-----begin private key-----")
            || lowercase.contains("-----begin rsa private key-----")
        {
            in_private_key = true;
            output.push_str("[REDACTED PRIVATE KEY]");
        } else if in_private_key {
            if lowercase.contains("-----end private key-----")
                || lowercase.contains("-----end rsa private key-----")
            {
                in_private_key = false;
            }
        } else {
            output.push_str(&redact_line(content));
        }
        if has_newline {
            output.push('\n');
        }
    }
    output
}

fn redact_line(line: &str) -> String {
    let line = redact_source_assignments(line);
    let lowercase = line.to_ascii_lowercase();
    if let Some(start) = lowercase.find("authorization:") {
        let value_start = start + "authorization:".len();
        if !line[value_start..].trim().is_empty() {
            return format!("{}[REDACTED]", &line[..value_start]);
        }
    }
    line.split_inclusive(char::is_whitespace)
        .map(redact_chunk)
        .collect()
}

fn redact_source_assignments(line: &str) -> String {
    static ASSIGNMENT: OnceLock<Regex> = OnceLock::new();
    let assignment_re = ASSIGNMENT.get_or_init(|| {
        Regex::new(
            r#"(?i)(\b([A-Za-z_][A-Za-z0-9_]*)(?:\s*:\s*[^=;\r\n]+)?\s*=\s*)("[^"\r\n]*"|'[^'\r\n]*'|[^\s=,;][^\s,;]*)"#,
        )
        .expect("valid regex")
    });
    assignment_re
        .replace_all(line, |captures: &Captures<'_>| {
            let name = captures.get(2).expect("assignment name").as_str();
            if !is_sensitive_assignment_name(name) {
                return captures
                    .get(0)
                    .expect("whole assignment")
                    .as_str()
                    .to_string();
            }
            let prefix = captures.get(1).expect("assignment prefix").as_str();
            let value = captures.get(3).expect("assignment value").as_str();
            match (value.as_bytes().first(), value.as_bytes().last()) {
                (Some(b'"'), Some(b'"')) => format!("{prefix}\"[REDACTED]\""),
                (Some(b'\''), Some(b'\'')) => format!("{prefix}'[REDACTED]'"),
                _ => format!("{prefix}[REDACTED]"),
            }
        })
        .into_owned()
}

fn is_sensitive_assignment_name(name: &str) -> bool {
    let lowercase = name.to_ascii_lowercase();
    [
        "password",
        "passwd",
        "client_secret",
        "clientsecret",
        "api_key",
        "apikey",
        "access_token",
        "accesstoken",
        "refresh_token",
        "refreshtoken",
        "token",
        "secret",
    ]
    .iter()
    .any(|marker| lowercase.contains(marker))
}

fn redact_chunk(chunk: &str) -> String {
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
    .any(|prefix| lowercase.contains(prefix))
    {
        return format!("[REDACTED]{suffix}");
    }
    for marker in [
        "password=",
        "password:",
        "client_secret=",
        "client-secret=",
        "api_key=",
        "api-key=",
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_assignments_tokens_and_private_key_blocks() {
        let value = "password=hunter2 key=sk_live_fixture browser=pk_live_fixture\n-----BEGIN PRIVATE KEY-----\nabc\n-----END PRIVATE KEY-----\nok";
        let redacted = redact_sensitive_text(value);
        assert!(!redacted.contains("hunter2"));
        assert!(!redacted.contains("sk_live_fixture"));
        assert!(!redacted.contains("pk_live_fixture"));
        assert!(!redacted.contains("abc"));
        assert!(redacted.ends_with("ok"));
    }

    #[test]
    fn redacts_spaced_source_assignments_without_destroying_syntax() {
        let value = r#"let _synthetic_live_test_token = "telegram_token_documents_rust_live_test";
const API_KEY: &str = "source-api-key";
let ordinary_value = "keep-me";"#;
        let redacted = redact_sensitive_text(value);
        assert!(!redacted.contains("telegram_token_documents_rust_live_test"));
        assert!(!redacted.contains("source-api-key"));
        assert!(redacted.contains("_synthetic_live_test_token = \"[REDACTED]\";"));
        assert!(redacted.contains("API_KEY: &str = \"[REDACTED]\";"));
        assert!(redacted.contains("ordinary_value = \"keep-me\";"));
    }
}
