//! Google Workspace ingestion (feature `gws`). `.gdoc`/`.gsheet`/`.gslides`
//! files are tiny JSON *pointers* (an id + URL) created by Google Drive desktop
//! sync — not the document content. This module parses the pointer and shells
//! out to the externally-installed `gws` CLI to export the real document to
//! markdown ("shape A"). The pointer parser + command builder are pure and
//! tested; the export call is thin glue (needs the CLI + auth).

use std::path::{Path, PathBuf};

/// A parsed Google-Workspace pointer file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GwsPointer {
    pub doc_id: String,
    pub url: String,
}

/// True if `path` has a Google-Workspace pointer extension.
pub fn is_gws_pointer(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("gdoc") | Some("gsheet") | Some("gslides")
    )
}

/// Parse a `.gdoc`/`.gsheet`/`.gslides` pointer's JSON into its doc id + URL.
/// Handles the modern `{"doc_id","url"}` shape and the legacy
/// `{"resource_id":"document:<id>","url"}` shape.
pub fn parse_gws_pointer(json: &str) -> Option<GwsPointer> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let url = v
        .get("url")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    // Prefer an explicit id; else the legacy `resource_id` ("kind:<id>"); else
    // the `/d/<id>/` segment of the Drive URL.
    let doc_id = v
        .get("doc_id")
        .and_then(|x| x.as_str())
        .map(str::to_string)
        .or_else(|| {
            v.get("resource_id")
                .and_then(|x| x.as_str())
                .and_then(|r| r.rsplit(':').next())
                .map(str::to_string)
        })
        .or_else(|| doc_id_from_url(&url))?;
    if doc_id.is_empty() {
        return None;
    }
    Some(GwsPointer { doc_id, url })
}

/// Extract the `<id>` from a `.../d/<id>/...` Google Drive URL.
fn doc_id_from_url(url: &str) -> Option<String> {
    let after = url.split_once("/d/").map(|(_, rest)| rest)?;
    let id = after.split(['/', '?', '#']).next().unwrap_or(after);
    (!id.is_empty()).then(|| id.to_string())
}

/// Args for the `gws` CLI to export `doc_id` to markdown at `out_path`.
/// (Overridable shape — adjust to your installed `gws` if its flags differ.)
pub fn gws_export_args(doc_id: &str, out_path: &Path) -> Vec<String> {
    vec![
        "export".into(),
        "--id".into(),
        doc_id.into(),
        "--format".into(),
        "md".into(),
        "--out".into(),
        out_path.to_string_lossy().into_owned(),
    ]
}

/// Read a pointer file and export the underlying Google doc to markdown in
/// `target_dir` via the `gws` CLI (overridable via `CODEGRAPH_GWS_CMD`).
pub fn ingest_gdoc(pointer_path: &Path, target_dir: &Path) -> Result<PathBuf, String> {
    std::fs::create_dir_all(target_dir).map_err(|e| e.to_string())?;
    let json = std::fs::read_to_string(pointer_path).map_err(|e| e.to_string())?;
    let ptr = parse_gws_pointer(&json).ok_or("not a recognized Google-Workspace pointer")?;
    let stem = pointer_path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| ptr.doc_id.clone());
    let out_path = target_dir.join(format!("{stem}.md"));
    let cmd = std::env::var("CODEGRAPH_GWS_CMD").unwrap_or_else(|_| "gws".into());
    let status = std::process::Command::new(&cmd)
        .args(gws_export_args(&ptr.doc_id, &out_path))
        .status()
        .map_err(|e| format!("gws CLI `{cmd}` not available: {e}"))?;
    if !status.success() {
        return Err(format!("`{cmd}` exited {status}"));
    }
    Ok(out_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_pointer_extensions() {
        assert!(is_gws_pointer(Path::new("plan.gdoc")));
        assert!(is_gws_pointer(Path::new("budget.gsheet")));
        assert!(is_gws_pointer(Path::new("deck.gslides")));
        assert!(!is_gws_pointer(Path::new("notes.md")));
    }

    #[test]
    fn parses_modern_pointer() {
        let json = r#"{"doc_id":"1AbCdEf","email":"x@y.com","url":"https://docs.google.com/document/d/1AbCdEf/edit"}"#;
        let p = parse_gws_pointer(json).expect("parsed");
        assert_eq!(p.doc_id, "1AbCdEf");
        assert!(p.url.contains("1AbCdEf"));
    }

    #[test]
    fn parses_legacy_resource_id_pointer() {
        let json =
            r#"{"resource_id":"document:9ZZ","url":"https://docs.google.com/document/d/9ZZ/edit"}"#;
        let p = parse_gws_pointer(json).expect("parsed");
        assert_eq!(p.doc_id, "9ZZ");
    }

    #[test]
    fn falls_back_to_url_doc_id_when_only_url_present() {
        let json = r#"{"url":"https://docs.google.com/spreadsheets/d/SHEET123/edit#gid=0"}"#;
        let p = parse_gws_pointer(json).expect("parsed");
        assert_eq!(p.doc_id, "SHEET123");
    }

    #[test]
    fn none_on_non_pointer_json() {
        assert!(parse_gws_pointer(r#"{"foo":"bar"}"#).is_none());
        assert!(parse_gws_pointer("not json").is_none());
    }

    #[test]
    fn export_args_include_doc_id_and_out_path() {
        let args = gws_export_args("DOC1", Path::new("/out/plan.md"));
        assert!(args.iter().any(|a| a == "DOC1"), "doc id: {args:?}");
        assert!(
            args.iter().any(|a| a.contains("plan.md")),
            "out path: {args:?}"
        );
    }
}
