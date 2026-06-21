//! URL ingestion. Fetches a URL through the SSRF-guarded
//! [`safe_fetch`] and writes a file into the target
//! dir for the normal extraction pass to pick up ("shape A").
//!
//! Webpage/arXiv/tweet/GitHub → HTML scraped to markdown with YAML frontmatter;
//! PDF/image → downloaded verbatim; YouTube → deferred (needs audio
//! transcription).

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use reqwest::Url;

use crate::security::{safe_fetch, safe_fetch_text, FetchError, MAX_FETCH_BYTES};

/// What kind of resource a URL points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UrlKind {
    Tweet,
    Arxiv,
    Github,
    Youtube,
    Pdf,
    Image,
    Webpage,
}

impl UrlKind {
    fn as_str(self) -> &'static str {
        match self {
            UrlKind::Tweet => "tweet",
            UrlKind::Arxiv => "paper",
            UrlKind::Github => "webpage",
            UrlKind::Youtube => "video",
            UrlKind::Pdf => "pdf",
            UrlKind::Image => "image",
            UrlKind::Webpage => "webpage",
        }
    }
}

const IMAGE_EXTS: &[&str] = &[".png", ".jpg", ".jpeg", ".webp", ".gif"];

/// Classify a URL by host/extension.
pub fn detect_url_type(url: &str) -> UrlKind {
    let u = url.to_lowercase();
    if u.contains("twitter.com") || u.contains("//x.com") || u.contains(".x.com") {
        return UrlKind::Tweet;
    }
    if u.contains("arxiv.org") {
        return UrlKind::Arxiv;
    }
    if u.contains("youtube.com") || u.contains("youtu.be") {
        return UrlKind::Youtube;
    }
    if u.contains("github.com") {
        return UrlKind::Github;
    }
    let path = u.split(['?', '#']).next().unwrap_or(&u);
    if path.ends_with(".pdf") {
        return UrlKind::Pdf;
    }
    if IMAGE_EXTS.iter().any(|e| path.ends_with(e)) {
        return UrlKind::Image;
    }
    UrlKind::Webpage
}

/// Escape a string as a double-quoted YAML scalar — injection-safe frontmatter.
pub fn yaml_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        let cp = c as u32;
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\0' => out.push_str("\\0"),
            // U+2028/U+2029 are YAML line breaks; must be escaped or a hostile
            // value can break out of the quoted scalar and inject sibling keys.
            '\u{2028}' => out.push_str("\\L"),
            '\u{2029}' => out.push_str("\\P"),
            _ if cp < 0x20 || cp == 0x7f => out.push_str(&format!("\\x{cp:02x}")),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

static SCRIPT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<script\b.*?</\s*script\s*>").expect("valid script regex"));
static STYLE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<style\b.*?</\s*style\s*>").expect("valid style regex"));
static TAG_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<[^>]+>").expect("valid html-tag regex"));
static WS_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\s+").expect("valid whitespace regex"));
static TITLE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)<title[^>]*>(.*?)</\s*title\s*>").expect("valid title regex")
});

/// Strip a page to readable text (drop script/style, strip tags, decode a few
/// entities, collapse whitespace, cap at 12k chars).
pub fn html_to_markdown(html: &str) -> String {
    let no_scripts = SCRIPT_RE.replace_all(html, " ");
    let no_style = STYLE_RE.replace_all(&no_scripts, " ");
    let no_tags = TAG_RE.replace_all(&no_style, " ");
    let decoded = no_tags
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ");
    let collapsed = WS_RE.replace_all(decoded.trim(), " ");
    collapsed.chars().take(12_000).collect()
}

fn extract_title(html: &str) -> Option<String> {
    TITLE_RE
        .captures(html)
        .and_then(|c| c.get(1))
        .map(|m| WS_RE.replace_all(m.as_str().trim(), " ").into_owned())
        .filter(|t| !t.is_empty())
}

/// A filesystem-safe filename derived from a URL (alnum runs → '-', capped).
pub(crate) fn safe_filename(url: &str, ext: &str) -> String {
    let mut slug = String::new();
    let mut prev_dash = false;
    for c in url.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            slug.push('-');
            prev_dash = true;
        }
    }
    let slug = slug.trim_matches('-');
    let slug: String = slug.chars().take(80).collect();
    let slug = if slug.is_empty() { "ingested" } else { &slug };
    format!("{slug}.{ext}")
}

fn url_extension(url: &str) -> Option<String> {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    path.rsplit('/')
        .next()
        .and_then(|seg| seg.rsplit_once('.'))
        .map(|(_, e)| e.to_ascii_lowercase())
        .filter(|e| e.chars().all(|c| c.is_ascii_alphanumeric()) && !e.is_empty())
}

// per-source scrapers (5.5): structured endpoints + pure parsers.
//
// These fetch a source's structured endpoint (oEmbed / Atom / REST) instead of
// scraping its JS-heavy HTML, yielding cleaner content. The fetch glue goes
// through the SSRF-guarded `safe_fetch_text`; the parsers are pure (unit-tested
// with canned payloads). Any failure falls back to the generic HTML path so a
// flaky third-party API never breaks ingestion.

/// Collapse whitespace + decode the common HTML entities + trim (the cleanup the
/// HTML path applies, reused by the structured parsers).
fn clean_text(s: &str) -> String {
    let decoded = s
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ");
    WS_RE.replace_all(decoded.trim(), " ").into_owned()
}

// arXiv Atom patterns, compiled once process-wide (not per parse). M3.
static ARXIV_ENTRY_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<entry\b[^>]*>").expect("valid arxiv-entry regex"));
static ARXIV_TITLE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)<title\b[^>]*>(.*?)</\s*title\s*>").expect("valid arxiv-title regex")
});
static ARXIV_SUMMARY_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)<summary\b[^>]*>(.*?)</\s*summary\s*>").expect("valid arxiv-summary regex")
});
static ARXIV_NAME_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)<name\b[^>]*>(.*?)</\s*name\s*>").expect("valid arxiv-name regex")
});

/// First match's group-1 text content, whitespace-collapsed, or `None`.
fn xml_first(s: &str, re: &Regex) -> Option<String> {
    re.captures(s)
        .and_then(|c| c.get(1))
        .map(|m| clean_text(m.as_str()))
        .filter(|t| !t.is_empty())
}

/// oEmbed endpoint for a tweet/X URL (publish.twitter.com returns the tweet text
/// + author as JSON — cleaner than scraping the timeline HTML).
pub fn tweet_oembed_endpoint(tweet_url: &str) -> String {
    let mut u = Url::parse("https://publish.twitter.com/oembed").expect("static base url");
    u.query_pairs_mut()
        .append_pair("url", tweet_url)
        .append_pair("omit_script", "true")
        .append_pair("dnt", "true");
    u.into()
}

/// arXiv id (`2401.00001`, `2401.00001v2`, `cs/0501001`) embedded in an
/// `/abs/<id>` or `/pdf/<id>` URL, or `None`.
fn arxiv_id(url: &str) -> Option<String> {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let after = path
        .split_once("/abs/")
        .or_else(|| path.split_once("/pdf/"))
        .map(|(_, rest)| rest)?;
    let id = after.trim_end_matches('/');
    let id = id.strip_suffix(".pdf").unwrap_or(id);
    (!id.is_empty()).then(|| id.to_string())
}

/// arXiv Atom API query for the paper in `url`, or `None` if no id is present.
pub fn arxiv_api_endpoint(url: &str) -> Option<String> {
    arxiv_id(url).map(|id| format!("https://export.arxiv.org/api/query?id_list={id}"))
}

/// `(owner, repo)` from a `github.com/<owner>/<repo>` URL, or `None`.
fn github_owner_repo(url: &str) -> Option<(String, String)> {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let after = path.split_once("github.com/").map(|(_, rest)| rest)?;
    let mut segs = after.split('/').filter(|s| !s.is_empty());
    let owner = segs.next()?.to_string();
    let repo = segs.next()?.trim_end_matches(".git").to_string();
    (!owner.is_empty() && !repo.is_empty()).then_some((owner, repo))
}

/// GitHub REST API endpoint for the repo in a github.com URL, or `None`.
pub fn github_api_endpoint(url: &str) -> Option<String> {
    github_owner_repo(url).map(|(o, r)| format!("https://api.github.com/repos/{o}/{r}"))
}

/// Parse a publish.twitter.com oEmbed JSON payload into `(title, body)`.
pub fn parse_tweet_oembed(json: &str) -> Option<(String, String)> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let html = v.get("html").and_then(|x| x.as_str())?;
    let body = html_to_markdown(html);
    if body.trim().is_empty() {
        return None;
    }
    let author = v.get("author_name").and_then(|x| x.as_str()).unwrap_or("");
    let title = if author.is_empty() {
        "Tweet".to_string()
    } else {
        format!("Tweet by {author}")
    };
    Some((title, body))
}

/// Parse an arXiv Atom API response into `(title, body)` (abstract + authors).
pub fn parse_arxiv_atom(xml: &str) -> Option<(String, String)> {
    // The first <entry> is the paper; everything before it is the feed header
    // (which has its own <title> we must not pick up). The open tag may carry
    // attributes/namespaces, so match it as a regex (like `xml_first`), not a
    // bare literal.
    let entry = ARXIV_ENTRY_RE
        .find(xml)
        .map(|m| {
            let rest = &xml[m.end()..];
            rest.split_once("</entry>").map_or(rest, |(e, _)| e)
        })
        .unwrap_or(xml);
    let title = xml_first(entry, &ARXIV_TITLE_RE)?;
    let summary = xml_first(entry, &ARXIV_SUMMARY_RE).unwrap_or_default();
    // Author <name>s, in document order.
    let authors: Vec<String> = ARXIV_NAME_RE
        .captures_iter(entry)
        .filter_map(|c| c.get(1).map(|m| clean_text(m.as_str())))
        .filter(|n| !n.is_empty())
        .collect();
    let mut body = String::new();
    if !authors.is_empty() {
        body.push_str(&format!("Authors: {}\n\n", authors.join(", ")));
    }
    body.push_str(&summary);
    if body.trim().is_empty() {
        return None;
    }
    Some((title, body))
}

/// Parse a GitHub repo REST API response into `(title, body)`.
pub fn parse_github_repo(json: &str) -> Option<(String, String)> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let full = v.get("full_name").and_then(|x| x.as_str());
    let desc = v
        .get("description")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .trim();
    let lang = v.get("language").and_then(|x| x.as_str()).unwrap_or("");
    let stars = v.get("stargazers_count").and_then(|x| x.as_u64());
    let topics: Vec<&str> = v
        .get("topics")
        .and_then(|x| x.as_array())
        .map(|a| a.iter().filter_map(|t| t.as_str()).collect())
        .unwrap_or_default();
    // No repo identity means not a repo payload (e.g. a 404 `{"message":...}`).
    let title = full?.to_string();
    let mut body = String::new();
    if !desc.is_empty() {
        body.push_str(desc);
        body.push_str("\n\n");
    }
    if !lang.is_empty() {
        body.push_str(&format!("Language: {lang}\n"));
    }
    if let Some(s) = stars {
        body.push_str(&format!("Stars: {s}\n"));
    }
    if !topics.is_empty() {
        body.push_str(&format!("Topics: {}\n", topics.join(", ")));
    }
    Some((title, body))
}

/// Fetch + parse a tweet via oEmbed; `None` on any failure (→ generic fallback).
fn scrape_tweet(url: &str) -> Option<(String, String)> {
    parse_tweet_oembed(&safe_fetch_text(&tweet_oembed_endpoint(url)).ok()?)
}

/// Fetch + parse an arXiv paper via the Atom API; `None` on any failure.
fn scrape_arxiv(url: &str) -> Option<(String, String)> {
    parse_arxiv_atom(&safe_fetch_text(&arxiv_api_endpoint(url)?).ok()?)
}

/// Fetch + parse a GitHub repo via the REST API; `None` on any failure.
fn scrape_github(url: &str) -> Option<(String, String)> {
    parse_github_repo(&safe_fetch_text(&github_api_endpoint(url)?).ok()?)
}

/// Fetch `url` and write a file into `target_dir`, returning its path. Webpage/
/// arXiv/tweet/GitHub become markdown with frontmatter; PDF/image are downloaded
/// verbatim; YouTube is deferred.
pub fn ingest_url(url: &str, target_dir: &Path) -> Result<PathBuf, FetchError> {
    std::fs::create_dir_all(target_dir).map_err(|e| FetchError::Http(e.to_string()))?;
    let kind = detect_url_type(url);
    match kind {
        UrlKind::Youtube => {
            #[cfg(feature = "media")]
            {
                crate::media::ingest_youtube(url, target_dir)
            }
            #[cfg(not(feature = "media"))]
            {
                Err(FetchError::Blocked(
                    "youtube ingest needs the `media` feature (shells out to yt-dlp)".into(),
                ))
            }
        }
        UrlKind::Pdf | UrlKind::Image => {
            let bytes = safe_fetch(url, MAX_FETCH_BYTES)?;
            let ext = if kind == UrlKind::Pdf {
                "pdf".to_string()
            } else {
                url_extension(url).unwrap_or_else(|| "img".to_string())
            };
            let path = target_dir.join(safe_filename(url, &ext));
            std::fs::write(&path, bytes).map_err(|e| FetchError::Http(e.to_string()))?;
            Ok(path)
        }
        _ => {
            // Prefer a per-source structured scraper (oEmbed / Atom / REST) for
            // cleaner content; fall back to generic HTML scraping on any failure.
            let scraped = match kind {
                UrlKind::Tweet => scrape_tweet(url),
                UrlKind::Arxiv => scrape_arxiv(url),
                UrlKind::Github => scrape_github(url),
                _ => None,
            };
            let (title, body) = match scraped {
                Some(tb) => tb,
                None => {
                    let html = safe_fetch_text(url)?;
                    let title = extract_title(&html).unwrap_or_else(|| url.to_string());
                    (title, html_to_markdown(&html))
                }
            };
            let md = format!(
                "---\ntitle: {}\nsource_url: {}\ntype: {}\n---\n\n{}\n",
                yaml_str(&title),
                yaml_str(url),
                kind.as_str(),
                body
            );
            let path = target_dir.join(safe_filename(url, "md"));
            std::fs::write(&path, md).map_err(|e| FetchError::Http(e.to_string()))?;
            Ok(path)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_url_types() {
        assert_eq!(
            detect_url_type("https://twitter.com/x/status/1"),
            UrlKind::Tweet
        );
        assert_eq!(
            detect_url_type("https://arxiv.org/abs/2401.00001"),
            UrlKind::Arxiv
        );
        assert_eq!(detect_url_type("https://youtu.be/abc"), UrlKind::Youtube);
        assert_eq!(detect_url_type("https://github.com/o/r"), UrlKind::Github);
        assert_eq!(detect_url_type("https://site.com/paper.pdf"), UrlKind::Pdf);
        assert_eq!(
            detect_url_type("https://site.com/img.PNG?x=1"),
            UrlKind::Image
        );
        assert_eq!(
            detect_url_type("https://blog.example.com/post"),
            UrlKind::Webpage
        );
    }

    #[test]
    fn yaml_str_escapes_injection() {
        // A newline + quote can't break out of the frontmatter scalar.
        let out = yaml_str("evil\ntitle: pwned\" x");
        assert!(!out.contains('\n'), "no raw newline: {out}");
        assert!(out.starts_with('"') && out.ends_with('"'));
        assert!(out.contains("\\n") && out.contains("\\\""));
    }

    #[test]
    fn yaml_str_escapes_unicode_line_breaks_and_del() {
        // U+2028/U+2029 are YAML line breaks; DEL + NUL are control chars. None
        // may pass through raw (the F-009/F-019 injection hardening).
        let out = yaml_str("a\u{2028}b\u{2029}c\u{7f}d\u{0}e");
        assert!(out.contains("\\L") && out.contains("\\P"), "{out}");
        assert!(out.contains("\\x7f") && out.contains("\\0"), "{out}");
        assert!(
            !out.contains('\u{2028}') && !out.contains('\u{2029}'),
            "{out}"
        );
        assert!(!out.contains('\u{7f}'), "{out}");
    }

    #[test]
    fn html_to_markdown_strips_scripts_tags_entities() {
        let html =
            "<html><head><title>T</title><style>x{}</style></head><body><script>evil()</script><p>Hello&nbsp;&amp; world</p></body></html>";
        let md = html_to_markdown(html);
        assert!(!md.contains("evil()"), "script dropped: {md}");
        assert!(!md.contains('<'), "tags stripped: {md}");
        assert!(
            md.contains("Hello & world"),
            "entities decoded + ws collapsed: {md}"
        );
    }

    #[test]
    fn extract_title_works() {
        assert_eq!(
            extract_title("<TITLE>  My  Page </TITLE>").as_deref(),
            Some("My Page")
        );
        assert!(extract_title("<body>no title</body>").is_none());
    }

    #[test]
    fn safe_filename_is_sane() {
        let f = safe_filename("https://example.com/a/b?c=1", "md");
        assert!(f.ends_with(".md"));
        assert!(!f.contains('/') && !f.contains('?') && !f.contains(':'));
    }

    // per-source scrapers (5.5)

    #[test]
    fn tweet_oembed_endpoint_encodes_the_url() {
        let ep = tweet_oembed_endpoint("https://x.com/jack/status/20");
        assert!(ep.starts_with("https://publish.twitter.com/oembed?"));
        // The tweet URL is percent-encoded into the `url` query param.
        assert!(
            ep.contains("url=https%3A%2F%2Fx.com%2Fjack%2Fstatus%2F20"),
            "encoded url param: {ep}"
        );
    }

    #[test]
    fn parse_tweet_oembed_extracts_author_and_text() {
        let json = r#"{"author_name":"Jack","html":"<blockquote><p>just setting up my twttr</p></blockquote>"}"#;
        let (title, body) = parse_tweet_oembed(json).expect("parsed");
        assert_eq!(title, "Tweet by Jack");
        assert!(body.contains("just setting up my twttr"), "body: {body}");
        assert!(!body.contains('<'), "html stripped: {body}");
    }

    #[test]
    fn parse_tweet_oembed_none_on_garbage() {
        assert!(parse_tweet_oembed("not json").is_none());
        assert!(parse_tweet_oembed(r#"{"author_name":"x"}"#).is_none()); // no html
    }

    #[test]
    fn arxiv_api_endpoint_extracts_id_from_abs_and_pdf() {
        assert_eq!(
            arxiv_api_endpoint("https://arxiv.org/abs/2401.00001"),
            Some("https://export.arxiv.org/api/query?id_list=2401.00001".to_string())
        );
        assert_eq!(
            arxiv_api_endpoint("https://arxiv.org/pdf/2401.00001v2.pdf"),
            Some("https://export.arxiv.org/api/query?id_list=2401.00001v2".to_string())
        );
        assert_eq!(arxiv_api_endpoint("https://arxiv.org/"), None);
    }

    #[test]
    fn parse_arxiv_atom_extracts_title_summary_authors() {
        let xml = r#"<feed><title>ArXiv Query</title>
            <entry>
              <title>Attention Is All You Need</title>
              <summary>We propose the Transformer, a model architecture.</summary>
              <author><name>Ashish Vaswani</name></author>
              <author><name>Noam Shazeer</name></author>
            </entry></feed>"#;
        let (title, body) = parse_arxiv_atom(xml).expect("parsed");
        assert_eq!(title, "Attention Is All You Need");
        assert!(body.contains("Transformer"), "abstract: {body}");
        assert!(
            body.contains("Ashish Vaswani") && body.contains("Noam Shazeer"),
            "authors: {body}"
        );
    }

    #[test]
    fn parse_arxiv_atom_handles_attributed_entry_tag() {
        // The entry tag may carry attributes/namespaces; the parser must still
        // pick the *entry* title, not the feed-header title.
        let xml = r#"<feed xmlns="http://www.w3.org/2005/Atom"><title>ArXiv Query: foo</title>
            <entry xmlns:arxiv="http://arxiv.org/schemas/atom">
              <title>Real Paper Title</title>
              <summary>An abstract.</summary>
              <author><name>A. Author</name></author>
            </entry></feed>"#;
        let (title, body) = parse_arxiv_atom(xml).expect("parsed");
        assert_eq!(title, "Real Paper Title", "entry title, not feed title");
        assert!(body.contains("An abstract"), "{body}");
    }

    #[test]
    fn github_api_endpoint_extracts_owner_repo() {
        assert_eq!(
            github_api_endpoint("https://github.com/torvalds/linux"),
            Some("https://api.github.com/repos/torvalds/linux".to_string())
        );
        assert_eq!(
            github_api_endpoint("https://github.com/owner/repo.git"),
            Some("https://api.github.com/repos/owner/repo".to_string())
        );
        assert_eq!(github_api_endpoint("https://github.com/justowner"), None);
    }

    #[test]
    fn parse_github_repo_extracts_description_and_meta() {
        let json = r#"{"full_name":"torvalds/linux","description":"Linux kernel source tree","language":"C","stargazers_count":170000,"topics":["kernel","linux"]}"#;
        let (title, body) = parse_github_repo(json).expect("parsed");
        assert_eq!(title, "torvalds/linux");
        assert!(body.contains("Linux kernel source tree"), "desc: {body}");
        assert!(body.contains("Language: C"), "lang: {body}");
        assert!(
            body.contains("kernel") && body.contains("linux"),
            "topics: {body}"
        );
    }

    #[test]
    fn parse_github_repo_none_on_error_payload() {
        // GitHub's 404 JSON has no full_name/description, so we fall back to HTML.
        assert!(parse_github_repo(r#"{"message":"Not Found"}"#).is_none());
    }
}

#[cfg(test)]
mod fuzz {
    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

        /// The HTML→markdown stripper runs its static regexes over untrusted page
        /// content; arbitrary input must never panic it.
        #[test]
        fn html_to_markdown_never_panics(s in ".{0,4096}") {
            let _ = super::html_to_markdown(&s);
        }

        /// Random bracket/tag soup (more likely to exercise the tag regexes).
        #[test]
        fn html_to_markdown_tag_soup_never_panics(
            s in proptest::collection::vec(prop_oneof![Just('<'), Just('>'), Just('/'),
                Just('a'), Just(' '), Just('"')], 0..2048)
        ) {
            let html: String = s.into_iter().collect();
            let _ = super::html_to_markdown(&html);
        }
    }
}
