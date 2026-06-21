//! tsconfig `paths` parser. `compilerOptions.paths` maps an import alias to one or
//! more target paths: `"@app/*": ["../hub/src/*"]`. A trailing `/*` makes it a
//! **prefix** alias (`@app/Button` resolves through it); a bare key is **exact**. The
//! target paths contain the member directory (`…/hub/…`), so [`crate::alias`] resolves
//! the alias to that member. `baseUrl` is irrelevant here — we match the member dir as
//! a path segment regardless of whether the target is relative or absolute.

use crate::alias::{AliasKind, RawAlias};

/// Append a `RawAlias` per `compilerOptions.paths` entry in a tsconfig file's `text`.
pub(crate) fn parse(text: &str, out: &mut Vec<RawAlias>) {
    let Ok(json) = serde_json::from_str::<serde_json::Value>(text) else {
        return;
    };
    let Some(paths) = json
        .get("compilerOptions")
        .and_then(|c| c.get("paths"))
        .and_then(|p| p.as_object())
    else {
        return;
    };
    for (key, targets) in paths {
        // A bare `*` (catch-all) is too broad to attribute to one member.
        if key == "*" {
            continue;
        }
        let (alias, kind) = match key.strip_suffix("/*") {
            Some(prefix) => (prefix.to_string(), AliasKind::Prefix),
            None => (key.clone(), AliasKind::Exact),
        };
        let targets: Vec<String> = targets
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|t| t.as_str().map(str::to_string))
            .collect();
        if !targets.is_empty() {
            out.push(RawAlias {
                alias,
                kind,
                targets,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(text: &str) -> Vec<RawAlias> {
        let mut out = Vec::new();
        parse(text, &mut out);
        out
    }

    #[test]
    fn wildcard_key_is_prefix_alias() {
        let r = parsed(r#"{ "compilerOptions": { "paths": { "@app/*": ["../hub/src/*"] } } }"#);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].alias, "@app");
        assert_eq!(r[0].kind, AliasKind::Prefix);
        assert_eq!(r[0].targets, vec!["../hub/src/*".to_string()]);
    }

    #[test]
    fn bare_key_is_exact_alias() {
        let r =
            parsed(r#"{ "compilerOptions": { "paths": { "@shared": ["../shared/index.ts"] } } }"#);
        assert_eq!(r[0].alias, "@shared");
        assert_eq!(r[0].kind, AliasKind::Exact);
    }

    #[test]
    fn keeps_all_targets_in_array() {
        let r = parsed(
            r#"{ "compilerOptions": { "paths": { "@m/*": ["../a/src/*", "../b/src/*"] } } }"#,
        );
        assert_eq!(r[0].targets.len(), 2);
    }

    #[test]
    fn catch_all_star_skipped() {
        let r = parsed(r#"{ "compilerOptions": { "paths": { "*": ["./node_modules/*"] } } }"#);
        assert!(r.is_empty());
    }

    #[test]
    fn no_paths_yields_nothing() {
        assert!(parsed(r#"{ "compilerOptions": { "baseUrl": "." } }"#).is_empty());
        assert!(parsed("not json at all {").is_empty());
    }
}
