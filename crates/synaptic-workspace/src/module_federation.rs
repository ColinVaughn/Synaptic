//! webpack / Vite module-federation `remotes` parser. A host declares remotes it
//! consumes:
//!
//! ```js
//! // webpack ModuleFederationPlugin
//! remotes: { hub: 'hub@http://localhost:3001/remoteEntry.js' }
//! // Vite @originjs/vite-plugin-federation
//! remotes: { vpn: 'http://localhost:5001/assets/remoteEntry.js' }
//! ```
//!
//! Code then imports `hub/Button`, so each remote is a **prefix** alias. The member is
//! identified by either a path segment of the remote value (the `name@url` form names
//! it; a Vite URL often has it in the path) or — as a fallback — the alias key itself,
//! which is why the key is included in `targets`. The `remotes` object is found by
//! [`crate::alias::named_object`], tolerant of both quoted and unquoted keys.

use crate::alias::{named_object, object_pairs, AliasKind, RawAlias};

/// Append a prefix `RawAlias` per `remotes` entry in a config file's `text`.
pub(crate) fn parse(text: &str, out: &mut Vec<RawAlias>) {
    let Some(obj) = named_object(text, "remotes") else {
        return;
    };
    for (key, value) in object_pairs(&obj, true) {
        out.push(RawAlias {
            alias: key.clone(),
            kind: AliasKind::Prefix,
            // value first (its `name@url` / URL path usually names the member), key
            // as fallback (Vite URLs needn't contain the member name).
            targets: vec![value, key],
        });
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
    fn webpack_name_at_url_form() {
        let r = parsed(
            r#"new ModuleFederationPlugin({
                 name: 'host',
                 remotes: { hub: 'hub@http://localhost:3001/remoteEntry.js' },
               })"#,
        );
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].alias, "hub");
        assert_eq!(r[0].kind, AliasKind::Prefix);
        assert_eq!(
            r[0].targets,
            vec![
                "hub@http://localhost:3001/remoteEntry.js".to_string(),
                "hub".to_string()
            ]
        );
    }

    #[test]
    fn vite_url_form_with_quoted_key() {
        let r = parsed(
            r#"federation({
                 remotes: { "vpn": 'http://localhost:5001/vpn/assets/remoteEntry.js' }
               })"#,
        );
        assert_eq!(r[0].alias, "vpn");
        assert!(r[0].targets[0].contains("/vpn/"));
        assert_eq!(r[0].targets[1], "vpn");
    }

    #[test]
    fn non_string_remote_value_skipped() {
        // A function/promise remote has no literal specifier to resolve.
        let r = parsed(
            r#"remotes: {
                 dyn: () => import('somewhere'),
                 hub: 'hub@http://x/remoteEntry.js'
               }"#,
        );
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].alias, "hub");
    }

    #[test]
    fn no_remotes_yields_nothing() {
        assert!(parsed("module.exports = { mode: 'production' };").is_empty());
    }
}
