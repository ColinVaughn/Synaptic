//! Import-map parser (single-spa / SystemJS / native import maps). A micro-frontend
//! root references apps by an import-map **alias** (`@PCMatic/Hub`) decoupled from the
//! app's `package.json name`; the alias's target path contains the app's directory
//! (`…/hub/dist/…`). Import-map aliases are **exact** (the import specifier equals the
//! alias). The bounded walk and member resolution live in [`crate::alias`]; this module
//! only turns one file's `imports` object into `RawAlias` entries.

use crate::alias::{named_object, object_pairs, AliasKind, RawAlias};

/// Append an exact `RawAlias` per entry of the first `imports` object in `text`.
/// Tolerant of JSON importmaps and the JS object-literal (template-literal) form.
pub(crate) fn parse(text: &str, out: &mut Vec<RawAlias>) {
    let Some(obj) = named_object(text, "imports") else {
        return;
    };
    for (alias, target) in object_pairs(&obj, false) {
        out.push(RawAlias {
            alias,
            kind: AliasKind::Exact,
            targets: vec![target],
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aliases(text: &str) -> Vec<(String, Vec<String>)> {
        let mut out = Vec::new();
        parse(text, &mut out);
        out.into_iter()
            .map(|r| {
                assert_eq!(r.kind, AliasKind::Exact, "import-map aliases are exact");
                (r.alias, r.targets)
            })
            .collect()
    }

    #[test]
    fn parses_systemjs_importmap_template_form() {
        let text = r#"<script type="systemjs-importmap"></script>
            <script>const importMaps = { imports: {
              "@PCMatic/Hub": `${url}/hub/dist/assets/index.js`,
              "@PCMatic/Vpn": `${url}/vpn/dist/assets/index.js`,
              "single-spa": `${url}/root-config/dist/single-spa.js`
            }}</script>"#;
        let m = aliases(text);
        assert!(m
            .iter()
            .any(|(a, t)| a == "@PCMatic/Hub" && t[0].contains("hub/dist")));
        assert!(m.iter().any(|(a, _)| a == "single-spa"));
    }

    #[test]
    fn parses_json_importmap() {
        let m = aliases(r#"{ "imports": { "@x/Lib": "/libs/lib/dist/index.js" } }"#);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].0, "@x/Lib");
        assert_eq!(m[0].1, vec!["/libs/lib/dist/index.js".to_string()]);
    }

    #[test]
    fn no_imports_object_yields_nothing() {
        let mut out = Vec::new();
        parse("const x = { remotes: { a: 'b' } };", &mut out);
        assert!(out.is_empty());
    }
}
