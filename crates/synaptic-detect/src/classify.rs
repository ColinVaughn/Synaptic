use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;

use crate::file_type::*;

static PAPER_SIGNALS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        r"(?i)\barxiv\b",
        r"(?i)\bdoi\s*:",
        r"(?i)\babstract\b",
        r"(?i)\bproceedings\b",
        r"(?i)\bjournal\b",
        r"(?i)\bpreprint\b",
        r"\\cite\{",
        r"\[\d+\]",
        r"\[\n\d+\n\]",
        r"(?i)eq\.\s*\d+|equation\s+\d+",
        r"\d{4}\.\d{4,5}",
        r"(?i)\bwe propose\b",
        r"(?i)\bliterature\b",
    ]
    .iter()
    .map(|p| Regex::new(p).expect("valid built-in classify pattern"))
    .collect()
});

const PAPER_SIGNAL_THRESHOLD: usize = 3;

/// Lowercased final extension (no dot), or `None`.
fn ext(path: &Path) -> Option<String> {
    path.extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
}

fn in_set(set: &[&str], e: &str) -> bool {
    set.contains(&e)
}

/// Heuristic: does this text file read like an academic paper?
fn looks_like_paper(path: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    let head: String = text.chars().take(3000).collect();
    PAPER_SIGNALS.iter().filter(|re| re.is_match(&head)).count() >= PAPER_SIGNAL_THRESHOLD
}

/// True if any ancestor directory name ends with `.xcassets` (Xcode asset
/// catalog) — PDFs inside are vector icons, not papers.
fn under_asset_dir(path: &Path) -> bool {
    path.ancestors()
        .filter_map(|a| a.file_name())
        .any(|n| n.to_string_lossy().ends_with(".xcassets"))
}

/// Classify a file by extension (+ content sniff for papers). `None` for
/// unsupported files.
pub fn classify_file(path: &Path) -> Option<FileType> {
    let name = path.file_name()?.to_string_lossy().to_ascii_lowercase();
    if name.ends_with(".blade.php") {
        return Some(FileType::Code);
    }
    let e = ext(path)?;

    if in_set(PAPER_EXTENSIONS, &e) {
        // PDFs inside an asset catalog are icons, not papers.
        return if under_asset_dir(path) {
            None
        } else {
            Some(FileType::Paper)
        };
    }
    if in_set(CODE_EXTENSIONS, &e) {
        return Some(FileType::Code);
    }
    if in_set(IMAGE_EXTENSIONS, &e) {
        return Some(FileType::Image);
    }
    if in_set(VIDEO_EXTENSIONS, &e) {
        return Some(FileType::Video);
    }
    if in_set(OFFICE_EXTENSIONS, &e) {
        // Office files convert to markdown later (deferred); classify as document.
        return Some(FileType::Document);
    }
    if in_set(DOC_EXTENSIONS, &e) {
        // A document that reads like an academic paper is promoted to Paper
        // (the content sniff covers all DOC_EXTENSIONS, not just .md/.txt).
        if looks_like_paper(path) {
            return Some(FileType::Paper);
        }
        return Some(FileType::Document);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn classifies_by_extension() {
        assert_eq!(classify_file(Path::new("foo.py")), Some(FileType::Code));
        assert_eq!(classify_file(Path::new("bar.ts")), Some(FileType::Code));
        assert_eq!(
            classify_file(Path::new("README.md")),
            Some(FileType::Document)
        );
        assert_eq!(classify_file(Path::new("paper.pdf")), Some(FileType::Paper));
        assert_eq!(
            classify_file(Path::new("screenshot.png")),
            Some(FileType::Image)
        );
        assert_eq!(classify_file(Path::new("clip.mp4")), Some(FileType::Video));
        assert_eq!(classify_file(Path::new("archive.zip")), None);
    }

    #[test]
    fn uppercase_fortran_extension_is_code() {
        assert_eq!(classify_file(Path::new("legacy.F90")), Some(FileType::Code));
    }

    #[test]
    fn all_extractor_extensions_classify_as_code() {
        // Every extension the extract crate dispatches on must be discoverable as
        // Code, or those files are silently skipped on real repos.
        for ext in [
            "scala", "sc", "rb", "php", "lua", "ps1", "psm1", "kt", "swift", "dart", "ex", "exs",
            "jl", "zig", "groovy", "gradle", "asp", "asa", "vue", "svelte", "astro", "m", "f90",
            "for", "v", "sv", "vh", "yaml", "yml", "hcl", "tf", "sql", "json", "sh", "cjs", "mts",
            "hh",
        ] {
            assert_eq!(
                classify_file(Path::new(&format!("file.{ext}"))),
                Some(FileType::Code),
                ".{ext} should classify as Code"
            );
        }
    }

    #[test]
    fn blade_php_is_code() {
        assert_eq!(
            classify_file(Path::new("view.blade.php")),
            Some(FileType::Code)
        );
    }

    #[test]
    fn pdf_in_xcassets_is_skipped() {
        let p = Path::new("MyApp/Images.xcassets/icon.imageset/icon.pdf");
        assert_eq!(classify_file(p), None);
    }

    #[test]
    fn md_with_paper_signals_is_paper() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("paper.md");
        let mut f = std::fs::File::create(&p).unwrap();
        write!(
            f,
            "# Abstract\n\nWe propose a new method. See [1] and [23].\n\
             This work was published in the Journal of AI. ArXiv preprint.\n\
             See Equation 3 for details. \\cite{{vaswani2017}}.\n"
        )
        .unwrap();
        assert_eq!(classify_file(&p), Some(FileType::Paper));
    }

    #[test]
    fn plain_md_is_document() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("notes.md");
        std::fs::write(
            &p,
            "# Notes\n\nJust some ordinary notes about the project.\n",
        )
        .unwrap();
        assert_eq!(classify_file(&p), Some(FileType::Document));
    }
}
