use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;

// Layer 1: parent directory names.
const SENSITIVE_DIRS: &[&str] = &[
    ".ssh",
    ".gnupg",
    ".aws",
    ".gcloud",
    "secrets",
    ".secrets",
    "credentials",
];

// Layer 2: filename patterns.
static SENSITIVE_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        r"(?i)(^|[\\/])\.(env|envrc)(\.|$)",
        r"(?i)\.(pem|key|p12|pfx|cert|crt|der|p8)$",
        r"(id_rsa|id_dsa|id_ecdsa|id_ed25519)(\.pub)?$",
        r"(?i)(\.netrc|\.pgpass|\.htpasswd)$",
        r"(?i)(aws_credentials|gcloud_credentials|service.account)",
    ]
    .iter()
    .map(|p| Regex::new(p).expect("valid built-in sensitive-file pattern"))
    .collect()
});

// Layer 3: load-bearing generic keywords. Rust's `regex`
// has no lookaround, so the word-boundary semantics are implemented explicitly.
const KEYWORDS: &[&str] = &[
    "credential",
    "secret",
    "passwd",
    "password",
    "private_key",
    "token",
];

fn is_ascii_alnum(b: u8) -> bool {
    b.is_ascii_alphanumeric()
}

fn is_ascii_letter(b: u8) -> bool {
    b.is_ascii_alphabetic()
}

/// A keyword is "load-bearing" when it ends the stem (names the file's
/// contents) or appears in a short (≤2-word) name.
fn generic_keyword_hit(name: &str) -> bool {
    // stem = up to the first dot, ignoring leading dots (so ".token" keeps it).
    let trimmed = name.trim_start_matches('.');
    let stem = trimmed.split('.').next().unwrap_or("");
    if stem.is_empty() {
        return false;
    }
    let lower = stem.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let word_count = stem
        .split(['-', '_', ' ', '\t'])
        .filter(|w| !w.is_empty())
        .count();

    let mut any = false;
    for kw in KEYWORDS {
        let mut start = 0usize;
        while let Some(rel) = lower[start..].find(kw) {
            let i = start + rel;
            let before_ok = i == 0 || !is_ascii_alnum(bytes[i - 1]);
            let mut end = i + kw.len();
            if end < bytes.len() && bytes[end] == b's' {
                end += 1; // optional trailing 's' (e.g. "tokens", "secrets")
            }
            let after_ok = end >= bytes.len() || !is_ascii_letter(bytes[end]);
            if before_ok && after_ok {
                any = true;
                if end == bytes.len() {
                    return true; // keyword ends the stem
                }
            }
            start = i + 1;
        }
    }
    any && word_count <= 2
}

/// Return true if this file likely contains secrets and should be skipped.
pub fn is_sensitive(path: &Path) -> bool {
    // Layer 1: any *parent* directory is a known secrets dir.
    let parents: Vec<String> = path
        .parent()
        .into_iter()
        .flat_map(|p| p.components())
        .filter_map(|c| c.as_os_str().to_str().map(str::to_string))
        .collect();
    if parents.iter().any(|p| SENSITIVE_DIRS.contains(&p.as_str())) {
        return true;
    }
    // Layer 2 + 3 operate on the filename.
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    if SENSITIVE_PATTERNS.iter().any(|re| re.is_match(name)) {
        return true;
    }
    // Generic words such as `token` and `password` describe ordinary source
    // modules (`authenticity_token.rb`, `password_policy.py`, and Snitz Forums'
    // `password.asp`). Exempt programming-language sources from the keyword rule;
    // exact secret-file patterns and sensitive parent directories above still apply
    // to every file regardless. Data and config formats are deliberately NOT exempt
    // -- `credentials.yaml` is precisely what the keyword rule is for.
    if let Some(ext) = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        && is_language_source_extension(&ext)
    {
        return false;
    }
    generic_keyword_hit(name)
}

/// Code extensions whose files routinely *hold* secrets, so a name like
/// `credentials.*` stays a red flag: data, config, build and project formats.
const SECRET_PRONE_DATA_EXTENSIONS: &[&str] = &[
    "json", "yaml", "yml", "tf", "tfvars", "hcl", "sql", "sh", "bash", "gradle", "xaml", "csproj",
    "fsproj", "vbproj", "sln", "slnx", "uproject", "uplugin", "uasset", "umap",
];

/// True when `ext` is a programming-language source file rather than a data or
/// config format. Derived from [`crate::file_type::CODE_EXTENSIONS`] minus
/// [`SECRET_PRONE_DATA_EXTENSIONS`], so a newly supported language is exempt
/// automatically instead of silently vanishing from the graph the way `.asp` did.
fn is_language_source_extension(ext: &str) -> bool {
    crate::file_type::CODE_EXTENSIONS.contains(&ext) && !SECRET_PRONE_DATA_EXTENSIONS.contains(&ext)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_dotenv_and_keys() {
        assert!(is_sensitive(Path::new(".env")));
        assert!(is_sensitive(Path::new("server.pem")));
        assert!(is_sensitive(Path::new("id_rsa")));
        assert!(is_sensitive(Path::new("id_ed25519.pub")));
    }

    #[test]
    fn flags_parent_secrets_dir() {
        assert!(is_sensitive(Path::new("home/.ssh/config")));
        assert!(is_sensitive(Path::new("project/.aws/credentials")));
    }

    #[test]
    fn flags_load_bearing_keywords() {
        assert!(is_sensitive(Path::new("api_token.txt")));
        assert!(is_sensitive(Path::new("oauth_token.json")));
        assert!(is_sensitive(Path::new("app_secret.yaml")));
        assert!(is_sensitive(Path::new("github-personal-access-token.txt")));
    }

    /// The "ordinary source module" exemption was a hand-written extension list that
    /// happened to cover the mainstream languages and nothing else, so a normal page
    /// like Snitz Forums' `password.asp` was dropped from the graph as a secrets
    /// file. Programming-language sources are code whatever the language.
    #[test]
    fn source_files_are_not_secrets_whatever_the_language() {
        for p in [
            "password.asp",
            "forum/password.asp",
            "lib/token.ex",
            "src/secret.jl",
            "src/credentials.lua",
            "Modules/password.psm1",
            "src/secret_store.zig",
            "lib/password.dart",
            "src/Token.pas",
            "classes/SecretManager.cls",
            "src/password.groovy",
            "rtl/secret_key.sv",
        ] {
            assert!(!is_sensitive(Path::new(p)), "{p} is source, not a secret");
        }
    }

    /// Data and config formats keep the keyword rule: a file actually named
    /// `credentials.yaml` is the case the rule exists for.
    #[test]
    fn data_and_config_formats_stay_sensitive() {
        for p in [
            "credentials.yaml",
            "secrets.json",
            "app_secret.yml",
            "terraform/secret.tfvars",
            "deploy/secrets.sh",
            "android/secrets.gradle",
        ] {
            assert!(is_sensitive(Path::new(p)), "{p} must stay flagged");
        }
    }

    /// Every extension Synaptic treats as code must be deliberately classified as
    /// either a programming-language source (exempt from the generic-keyword rule)
    /// or a secret-prone data/config format (not exempt). Without this, adding a
    /// language silently re-creates the `password.asp` bug, and adding a config
    /// format silently starts ingesting secrets.
    #[test]
    fn every_code_extension_is_classified() {
        let unclassified: Vec<&str> = crate::file_type::CODE_EXTENSIONS
            .iter()
            .copied()
            .filter(|e| !SECRET_PRONE_DATA_EXTENSIONS.contains(e))
            .filter(|e| !is_language_source_extension(e))
            .collect();
        assert!(
            unclassified.is_empty(),
            "code extensions not classified as language-source or secret-prone: {unclassified:?}"
        );
    }

    #[test]
    fn does_not_flag_topic_words_or_identifiers() {
        assert!(!is_sensitive(Path::new("tokenizer.py")));
        assert!(!is_sensitive(Path::new("tokenize.py")));
        assert!(!is_sensitive(Path::new("token-economics-of-recall.md")));
        assert!(!is_sensitive(Path::new("password-policy-discussion.md")));
        assert!(!is_sensitive(Path::new("lib/authenticity_token.rb")));
        assert!(!is_sensitive(Path::new("src/password_policy.py")));
    }
}
