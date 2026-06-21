//! Heuristic: does a source path belong to test code? Used to pick out the
//! tests at risk from a change (predictive test selection). Path-based and
//! language-agnostic, so it works on any graph without re-extraction; it cannot
//! see inline unit tests (Rust `#[cfg(test)]`, Python tests not under a test
//! path), which is an accepted limitation.

/// True if `path` looks like test code, by directory convention (a `test` /
/// `tests` / `__tests__` / `testing` component; `spec(s)` is intentionally NOT a
/// directory trigger) or filename convention (`test_*`, `*_test`, `*_spec`,
/// `*.test.*`, `*.spec.*`, `*Test`/`*Tests`/`*Spec`, `conftest`).
pub fn is_test_path(path: &str) -> bool {
    let norm = path.replace('\\', "/");
    let comps: Vec<&str> = norm.split('/').filter(|c| !c.is_empty()).collect();
    let Some((file, dirs)) = comps.split_last() else {
        return false;
    };
    // A directory component that denotes a test tree. `spec(s)` is intentionally
    // NOT here: a `spec/` directory is too often non-test (OpenAPI specs, product
    // specs). Rspec's `spec/` files are still caught by the `_spec` filename rule.
    for d in dirs {
        if matches!(
            d.to_ascii_lowercase().as_str(),
            "test" | "tests" | "__tests__" | "testing"
        ) {
            return true;
        }
    }
    is_test_filename(file)
}

/// Filename-only conventions (the path's directories are checked separately).
fn is_test_filename(file: &str) -> bool {
    let lower = file.to_ascii_lowercase();
    // JS/TS infix: foo.test.ts, foo.spec.tsx.
    if lower.contains(".test.") || lower.contains(".spec.") {
        return true;
    }
    // Stem = filename without its final extension.
    let stem = file.rsplit_once('.').map(|(s, _)| s).unwrap_or(file);
    if stem.eq_ignore_ascii_case("conftest") {
        return true;
    }
    let stem_lower = stem.to_ascii_lowercase();
    // snake_case conventions (the underscore keeps "latest"/"contest" out).
    if stem_lower.starts_with("test_")
        || stem_lower.ends_with("_test")
        || stem_lower.ends_with("_tests")
        || stem_lower.ends_with("_spec")
    {
        return true;
    }
    // camelCase suffixes (Java/C#/Kotlin/Swift): UserTest, UserTests, LoginSpec.
    // Require a non-empty prefix so "Test.java" alone is not flagged here.
    (stem.ends_with("Test") && stem.len() > 4)
        || (stem.ends_with("Tests") && stem.len() > 5)
        || (stem.ends_with("Spec") && stem.len() > 4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_test_directories() {
        assert!(is_test_path("tests/foo.rs"));
        assert!(is_test_path("src/__tests__/foo.tsx"));
        assert!(is_test_path("project/test/Thing.java"));
        assert!(is_test_path("a\\b\\tests\\c.py")); // windows separators
    }

    #[test]
    fn detects_test_filenames_across_languages() {
        assert!(is_test_path("pkg/test_login.py")); // python prefix
        assert!(is_test_path("pkg/login_test.go")); // go suffix
        assert!(is_test_path("src/login.test.ts")); // js/ts infix
        assert!(is_test_path("src/login.spec.tsx"));
        assert!(is_test_path("app/user_spec.rb")); // ruby
        assert!(is_test_path("src/main/UserTest.java")); // camelCase suffix
        assert!(is_test_path("src/UserTests.cs"));
        assert!(is_test_path("Sources/LoginSpec.swift"));
        assert!(is_test_path("conftest.py")); // pytest fixture file
    }

    #[test]
    fn does_not_flag_production_code() {
        assert!(!is_test_path("src/login.py"));
        assert!(!is_test_path("src/latest.py")); // ends with "test" but no _ / camelCase
        assert!(!is_test_path("src/contest.go"));
        assert!(!is_test_path("lib/Tester.java")); // not a *Test suffix
        assert!(!is_test_path("src/attestation.ts"));
        assert!(!is_test_path("src/components/Button.tsx"));
        // A `spec/` directory is not, by itself, test code (OpenAPI / product specs).
        assert!(!is_test_path("openapi/spec/schema.json"));
        assert!(!is_test_path("docs/specs/product.md"));
    }
}
