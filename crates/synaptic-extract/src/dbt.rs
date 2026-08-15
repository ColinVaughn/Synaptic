//! dbt-templated SQL: Jinja neutralization and model lineage.
//!
//! A dbt model is a `.sql` file that is not valid SQL. It carries Jinja --
//! `{{ ref('stg_orders') }}`, `{% set x = [...] %}`, `{#- a comment -#}` -- which
//! defeats the SQL grammar outright, so the whole file parsed to an ERROR tree
//! and contributed nothing but its own file node. Measured on a real corpus,
//! every dbt model yielded zero declarations, which matters because dbt is where
//! a great deal of production SQL now lives and `ref()` is the edge that makes
//! its lineage a graph at all.
//!
//! Two things happen here:
//!
//! 1. **Neutralization** rewrites each Jinja span to the same byte length, so
//!    every offset and line number downstream stays exact. Where a span is a
//!    `ref()` or `source()` on one line, the referenced name is substituted in
//!    (padded), so the residual text reads `from stg_orders` and the ordinary SQL
//!    passes resolve it with no special cases.
//! 2. **Lineage** reports the model (named for its file, because that is how dbt
//!    names it) and the models it reads.

#[cfg(feature = "lang-sql")]
use std::sync::LazyLock;

#[cfg(feature = "lang-sql")]
use regex::Regex;

/// Any Jinja span: expression, statement, or comment.
#[cfg(feature = "lang-sql")]
static JINJA_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)\{\{.*?\}\}|\{%.*?%\}|\{#.*?#\}").expect("jinja regex"));

/// `{{ ref('name') }}` / `{{ ref("proj", 'name') }}` -- the last quoted argument
/// is the model name.
#[cfg(feature = "lang-sql")]
static REF_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?is)\{\{-?\s*ref\s*\(\s*(?:['"][^'"]*['"]\s*,\s*)*['"]([^'"]+)['"]\s*\)\s*-?\}\}"#,
    )
    .expect("dbt ref regex")
});

/// `{{ source('raw', 'orders') }}` -- the second argument is the table.
#[cfg(feature = "lang-sql")]
static SOURCE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?is)\{\{-?\s*source\s*\(\s*['"]([^'"]+)['"]\s*,\s*['"]([^'"]+)['"]\s*\)\s*-?\}\}"#,
    )
    .expect("dbt source regex")
});

/// Markers that identify a templated `.sql` file as dbt rather than as some
/// other Jinja user. Requiring one of these keeps ordinary SQL that happens to
/// contain braces from being reported as a dbt model.
#[cfg(feature = "lang-sql")]
static DBT_MARKER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)\{\{-?\s*(?:ref|source|config|var|this|target)\s*[\(\.]|\{%-?\s*(?:materialization|macro|snapshot|set|if|for)\b")
        .expect("dbt marker regex")
});

/// Whether the file carries Jinja at all.
#[cfg(feature = "lang-sql")]
pub fn is_templated(src: &str) -> bool {
    src.contains("{{") || src.contains("{%") || src.contains("{#")
}

/// Whether the file is a dbt model (templated, with a dbt-specific construct).
#[cfg(feature = "lang-sql")]
pub fn is_dbt(src: &str) -> bool {
    is_templated(src) && DBT_MARKER_RE.is_match(src)
}

/// One thing a model reads, with the 1-based line it was referenced on.
#[cfg(feature = "lang-sql")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    pub name: String,
    pub line: usize,
}

/// Every `ref()` and `source()` in the file, in source order and deduplicated.
#[cfg(feature = "lang-sql")]
pub fn references(src: &str) -> Vec<Reference> {
    let newlines: Vec<usize> = src.match_indices('\n').map(|(i, _)| i).collect();
    let line_at = |offset: usize| newlines.partition_point(|&n| n < offset) + 1;

    let mut out: Vec<Reference> = Vec::new();
    let mut push = |name: &str, offset: usize| {
        let name = name.trim();
        if name.is_empty() {
            return;
        }
        let r = Reference {
            name: name.to_string(),
            line: line_at(offset),
        };
        if !out.iter().any(|e| e.name.eq_ignore_ascii_case(&r.name)) {
            out.push(r);
        }
    };
    for c in REF_RE.captures_iter(src) {
        push(&c[1], c.get(0).map_or(0, |m| m.start()));
    }
    for c in SOURCE_RE.captures_iter(src) {
        push(&c[2], c.get(0).map_or(0, |m| m.start()));
    }
    out
}

/// Rewrite Jinja spans so the remainder can be parsed as SQL.
///
/// The replacement is always the same byte length as what it replaces, and every
/// newline inside a span is preserved, so line and column numbers downstream are
/// identical to the original file. A single-line `ref()`/`source()` becomes the
/// referenced name padded with spaces -- turning `from {{ ref('stg_orders') }}`
/// into `from stg_orders            ` -- so the existing SQL passes see a real
/// table name without knowing dbt exists. Anything else becomes blanks.
#[cfg(feature = "lang-sql")]
pub fn neutralize(src: &str) -> String {
    if !is_templated(src) {
        return src.to_string();
    }
    let mut out = String::with_capacity(src.len());
    let mut last = 0usize;
    for m in JINJA_RE.find_iter(src) {
        out.push_str(&src[last..m.start()]);
        out.push_str(&blank_span(m.as_str()));
        last = m.end();
    }
    out.push_str(&src[last..]);
    debug_assert_eq!(out.len(), src.len(), "neutralize must preserve byte length");
    out
}

/// The same-length replacement for one Jinja span.
#[cfg(feature = "lang-sql")]
fn blank_span(span: &str) -> String {
    // A multi-line span cannot carry a substituted identifier without moving the
    // text after it onto a different line, so it is blanked (newlines kept).
    if !span.contains('\n')
        && let Some(name) = single_line_target(span)
        && name.len() <= span.len()
        && is_sql_identifier(name)
    {
        let mut s = String::with_capacity(span.len());
        s.push_str(name);
        s.extend(std::iter::repeat_n(' ', span.len() - name.len()));
        return s;
    }
    // One space per *byte*, not per char: a multi-byte character replaced by a
    // single space would shorten the file and shift every line below it.
    let mut s = String::with_capacity(span.len());
    for c in span.chars() {
        if c == '\n' {
            s.push('\n');
        } else {
            s.extend(std::iter::repeat_n(' ', c.len_utf8()));
        }
    }
    s
}

/// The table name a one-line `ref()`/`source()` span resolves to.
#[cfg(feature = "lang-sql")]
fn single_line_target(span: &str) -> Option<&str> {
    if let Some(c) = SOURCE_RE.captures(span) {
        return c.get(2).map(|m| m.as_str().trim());
    }
    REF_RE
        .captures(span)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().trim())
}

/// Whether a substituted name is safe to splice into SQL text.
#[cfg(feature = "lang-sql")]
fn is_sql_identifier(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
        && !s.starts_with(|c: char| c.is_ascii_digit())
}

/// The model name dbt gives a file: its stem. dbt models are named by filename,
/// never by anything inside the file.
#[cfg(feature = "lang-sql")]
pub fn model_name(path: &str) -> Option<String> {
    let stem = std::path::Path::new(path).file_stem()?.to_str()?.trim();
    (!stem.is_empty()).then(|| stem.to_string())
}

#[cfg(all(test, feature = "lang-sql"))]
mod tests {
    use super::*;

    #[test]
    fn detects_dbt_templating() {
        assert!(is_dbt("select * from {{ ref('stg_orders') }}"));
        assert!(is_dbt("{% set x = 1 %}\nselect 1"));
        assert!(is_dbt("{{ config(materialized='table') }}\nselect 1"));
        // Plain SQL, and SQL with braces that are not Jinja, are not dbt.
        assert!(!is_dbt("select * from orders"));
        assert!(!is_dbt("select '{not jinja}' from t"));
    }

    /// Byte length and every newline must survive, or every line number in the
    /// file shifts and all anchors downstream become wrong.
    #[test]
    fn neutralize_preserves_byte_length_and_lines() {
        let src = "with a as (\n\n    {#-\n    a comment\n    -#}\n    select * from {{ ref('raw_customers') }}\n)\nselect * from a\n";
        let out = neutralize(src);
        assert_eq!(out.len(), src.len(), "byte length must be identical");
        assert_eq!(
            out.matches('\n').count(),
            src.matches('\n').count(),
            "newline count must be identical"
        );
        // Line 6 still holds the FROM, now naming a real table.
        let line6 = out.lines().nth(5).unwrap();
        assert!(line6.contains("from raw_customers"), "{line6:?}");
    }

    /// Blanking replaced each *char* with one space, so a multi-byte character
    /// inside a Jinja span shortened the file and shifted every line below it.
    /// The SQL fuzz corpus caught this; anchors would have silently drifted on
    /// any templated file containing non-ASCII text.
    #[test]
    fn neutralize_preserves_byte_length_with_multibyte_characters() {
        for src in [
            "{# коммент #}\nselect 1\n",
            "{% set x = 'héllo wörld' %}\nselect 2\n",
            "{{ config(alias='日本語') }}\nselect 3\n",
            "{#\n  多行\n  комментарий\n#}\nselect 4\n",
        ] {
            let out = neutralize(src);
            assert_eq!(out.len(), src.len(), "byte length changed for {src:?}");
            assert_eq!(
                out.matches('\n').count(),
                src.matches('\n').count(),
                "newlines changed for {src:?}"
            );
        }
    }

    #[test]
    fn neutralize_substitutes_ref_and_source_targets() {
        let out = neutralize("select * from {{ ref('stg_orders') }}");
        assert!(out.contains("from stg_orders"), "{out:?}");
        let out = neutralize("select * from {{ source('raw', 'orders') }}");
        assert!(out.contains("from orders"), "{out:?}");
    }

    /// A statement or comment span carries no table name and must simply vanish.
    #[test]
    fn neutralize_blanks_statements_and_comments() {
        let out = neutralize("{% set payment_methods = ['a', 'b'] %}\nselect 1\n");
        assert_eq!(out.lines().next().unwrap().trim(), "");
        assert_eq!(out.lines().nth(1).unwrap(), "select 1");
    }

    /// Plain SQL must come through byte-identical, so the non-dbt path is
    /// provably untouched.
    #[test]
    fn plain_sql_is_returned_unchanged() {
        let src = "CREATE TABLE users (id INT);\n";
        assert_eq!(neutralize(src), src);
    }

    #[test]
    fn collects_refs_and_sources_with_lines() {
        let src = "with o as (\n    select * from {{ ref('stg_orders') }}\n),\np as (\n    select * from {{ source('raw', 'payments') }}\n)\nselect 1\n";
        let r = references(src);
        assert_eq!(
            r,
            vec![
                Reference {
                    name: "stg_orders".into(),
                    line: 2
                },
                Reference {
                    name: "payments".into(),
                    line: 5
                },
            ]
        );
    }

    #[test]
    fn repeated_refs_are_deduplicated() {
        let src = "select * from {{ ref('a') }} union select * from {{ ref('a') }}";
        assert_eq!(references(src).len(), 1);
    }

    /// dbt allows a project-qualified two-argument ref; the model name is last.
    #[test]
    fn two_argument_ref_takes_the_model_name() {
        let r = references("select * from {{ ref('my_project', 'stg_orders') }}");
        assert_eq!(r[0].name, "stg_orders");
    }

    #[test]
    fn model_name_comes_from_the_filename() {
        assert_eq!(
            model_name("models/staging/stg_orders.sql").as_deref(),
            Some("stg_orders")
        );
        assert_eq!(model_name("customers.sql").as_deref(), Some("customers"));
    }

    /// A name that is not a bare identifier must not be spliced into the SQL
    /// text, or neutralization could inject syntax.
    #[test]
    fn unsafe_substitutions_are_blanked_instead() {
        let out = neutralize("select * from {{ ref('a-b; drop table x') }}");
        assert!(!out.contains("drop table x"), "{out:?}");
        assert!(out.contains("select * from"), "{out:?}");
    }
}
