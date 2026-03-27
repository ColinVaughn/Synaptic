//! Media transcription (feature `media`). Handles audio/video and YouTube
//! sources, shelling out to externally-installed tools rather than pulling a
//! native transcription engine:
//! - **YouTube** → `yt-dlp` fetches the (auto-)subtitles as WebVTT, which we
//!   parse to plain text (no audio transcription needed, no heavy deps).
//! - **local audio/video** → a transcription CLI (`whisper` by default,
//!   overridable via `CODEGRAPH_TRANSCRIBE_CMD`) writes a `.txt`/`.vtt` we read.
//!
//! The subtitle parser and command builders are pure and unit-tested; the
//! subprocess calls are thin glue (need the tools installed).

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;

use crate::security::FetchError;
use crate::url::{safe_filename, yaml_str};

static SUBTITLE_TAG_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<[^>]+>").expect("valid subtitle-tag regex"));

/// Parse a WebVTT/SRT subtitle track into plain transcript text: drop the
/// `WEBVTT`/`NOTE` headers, cue numbers, and `-->` timing lines, strip inline
/// tags (`<c>`, `<00:00:00.000>`), decode a few entities, and collapse the
/// consecutive duplicate lines that auto-generated subtitles roll up. One cue
/// line per output line.
pub fn parse_subtitle(text: &str) -> String {
    let lines: Vec<&str> = text.lines().map(str::trim).collect();
    let is_timing = |s: &str| s.contains("-->");
    let mut out: Vec<String> = Vec::new();
    for (i, &line) in lines.iter().enumerate() {
        if line.is_empty()
            || line.starts_with("WEBVTT")
            || line.starts_with("NOTE")
            || is_timing(line)
        {
            continue;
        }
        // An all-digit line is a cue number only when the immediately following
        // line is a timing line (cue number and timing are adjacent). Otherwise
        // it's a real caption (a year, a count) and must be kept: across a blank
        // line the next timing belongs to the next cue.
        if line.chars().all(|c| c.is_ascii_digit())
            && lines.get(i + 1).is_some_and(|l| is_timing(l))
        {
            continue;
        }
        let cleaned = SUBTITLE_TAG_RE.replace_all(line, "");
        let cleaned = cleaned
            .replace("&amp;", "&")
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&nbsp;", " ");
        let cleaned = cleaned.trim();
        if cleaned.is_empty() {
            continue;
        }
        // Auto-generated subtitles roll up the same line across cues; collapse
        // consecutive duplicates.
        if out.last().map(String::as_str) == Some(cleaned) {
            continue;
        }
        out.push(cleaned.to_string());
    }
    out.join("\n")
}

/// `yt-dlp` args to fetch a video's subtitles (manual + auto) as WebVTT into
/// `out_dir`, without downloading the media itself.
pub fn yt_dlp_subtitle_args(url: &str, out_dir: &Path) -> Vec<String> {
    vec![
        "--skip-download".into(),
        "--write-subs".into(),
        "--write-auto-subs".into(),
        "--sub-langs".into(),
        "en.*,en".into(),
        "--sub-format".into(),
        "vtt".into(),
        "--convert-subs".into(),
        "vtt".into(),
        "-o".into(),
        out_dir
            .join("%(id)s.%(ext)s")
            .to_string_lossy()
            .into_owned(),
        url.into(),
    ]
}

/// Args for the transcription CLI (`whisper`-compatible) to transcribe `audio`
/// into a `.txt` in `out_dir`. `model` is the model name (e.g. `base`).
pub fn transcribe_args(audio: &Path, out_dir: &Path, model: &str) -> Vec<String> {
    vec![
        "--model".into(),
        model.into(),
        "--output_format".into(),
        "txt".into(),
        "--output_dir".into(),
        out_dir.to_string_lossy().into_owned(),
        audio.to_string_lossy().into_owned(),
    ]
}

/// Wrap a transcript in a markdown document with YAML frontmatter (shape A: the
/// normal extraction pass turns it into nodes).
fn transcript_markdown(title: &str, source: &str, kind: &str, body: &str) -> String {
    format!(
        "---\ntitle: {}\nsource_url: {}\ntype: {}\n---\n\n{}\n",
        yaml_str(title),
        yaml_str(source),
        kind,
        body
    )
}

/// Fetch a YouTube video's subtitles via `yt-dlp` and write them as a transcript
/// markdown document into `target_dir`. Requires `yt-dlp` on `PATH`.
pub fn ingest_youtube(url: &str, target_dir: &Path) -> Result<PathBuf, FetchError> {
    std::fs::create_dir_all(target_dir).map_err(|e| FetchError::Http(e.to_string()))?;
    // The CLI writes its intermediate (.vtt) into an isolated temp dir, not into
    // `target_dir`: leaving it there would clutter the ingested dir, and any
    // doc-extension intermediate would get re-indexed. Only the `.md` lands in
    // `target_dir`. The temp dir is removed when `tmp` drops.
    let tmp = tempfile::tempdir().map_err(|e| FetchError::Http(e.to_string()))?;
    let status = std::process::Command::new("yt-dlp")
        .args(yt_dlp_subtitle_args(url, tmp.path()))
        .status()
        .map_err(|e| FetchError::Http(format!("yt-dlp not available: {e}")))?;
    if !status.success() {
        return Err(FetchError::Http(format!("yt-dlp exited {status}")));
    }
    let vtt = newest_with_ext(tmp.path(), "vtt")
        .ok_or_else(|| FetchError::Http("yt-dlp produced no subtitle file".into()))?;
    let raw = std::fs::read_to_string(&vtt).map_err(|e| FetchError::Http(e.to_string()))?;
    let transcript = parse_subtitle(&raw);
    if transcript.trim().is_empty() {
        return Err(FetchError::Http("empty transcript".into()));
    }
    let md = transcript_markdown(url, url, "video", &transcript);
    let path = target_dir.join(safe_filename(url, "md"));
    std::fs::write(&path, md).map_err(|e| FetchError::Http(e.to_string()))?;
    Ok(path)
}

/// Transcribe a local audio/video file via the configured transcription CLI and
/// write the transcript as a markdown document into `target_dir`.
pub fn transcribe_media(media: &Path, target_dir: &Path) -> Result<PathBuf, FetchError> {
    std::fs::create_dir_all(target_dir).map_err(|e| FetchError::Http(e.to_string()))?;
    let cmd = std::env::var("CODEGRAPH_TRANSCRIBE_CMD").unwrap_or_else(|_| "whisper".into());
    let model = std::env::var("CODEGRAPH_WHISPER_MODEL").unwrap_or_else(|_| "base".into());
    // Isolate the CLI's `.txt` (a doc extension) in a temp dir so it isn't
    // written into, and then re-indexed from, the ingested dir alongside the
    // `.md` we produce. Only the `.md` lands in `target_dir`.
    let tmp = tempfile::tempdir().map_err(|e| FetchError::Http(e.to_string()))?;
    let status = std::process::Command::new(&cmd)
        .args(transcribe_args(media, tmp.path(), &model))
        .status()
        .map_err(|e| FetchError::Http(format!("transcription CLI `{cmd}` not available: {e}")))?;
    if !status.success() {
        return Err(FetchError::Http(format!("`{cmd}` exited {status}")));
    }
    let txt = newest_with_ext(tmp.path(), "txt")
        .ok_or_else(|| FetchError::Http("transcription produced no .txt".into()))?;
    let body = std::fs::read_to_string(&txt).map_err(|e| FetchError::Http(e.to_string()))?;
    if body.trim().is_empty() {
        return Err(FetchError::Http("empty transcript".into()));
    }
    let name = media.to_string_lossy();
    let md = transcript_markdown(&name, &name, "transcript", body.trim());
    let path = target_dir.join(safe_filename(&name, "md"));
    std::fs::write(&path, md).map_err(|e| FetchError::Http(e.to_string()))?;
    Ok(path)
}

/// Most-recently-modified file with `ext` in `dir` (the artifact a CLI just wrote).
fn newest_with_ext(dir: &Path, ext: &str) -> Option<PathBuf> {
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) == Some(ext) {
            if let Ok(m) = entry.metadata().and_then(|m| m.modified()) {
                if best.as_ref().is_none_or(|(t, _)| m >= *t) {
                    best = Some((m, p));
                }
            }
        }
    }
    best.map(|(_, p)| p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_subtitle_strips_timing_headers_tags_and_dedups() {
        let vtt = "WEBVTT\n\
            \n\
            NOTE auto-generated\n\
            \n\
            00:00:00.000 --> 00:00:02.000\n\
            <c>Hello</c> world\n\
            \n\
            00:00:02.000 --> 00:00:04.000\n\
            Hello world\n\
            \n\
            00:00:04.000 --> 00:00:06.000\n\
            this is a <00:00:05.000>test\n";
        let t = parse_subtitle(vtt);
        assert!(!t.contains("WEBVTT") && !t.contains("NOTE"), "headers: {t}");
        assert!(!t.contains("-->"), "timing dropped: {t}");
        assert!(!t.contains('<'), "tags stripped: {t}");
        assert!(
            t.contains("Hello world") && t.contains("this is a test"),
            "{t}"
        );
        // Repeated "Hello world" cue collapses to one line.
        assert_eq!(t.matches("Hello world").count(), 1, "deduped: {t}");
    }

    #[test]
    fn parse_subtitle_keeps_numeric_caption_lines() {
        // A caption that happens to be all digits (a year on a slide, a count)
        // must not be dropped; only cue numbers (a digit line immediately before
        // a timing line) are.
        let vtt = "WEBVTT\n\n00:00:00.000 --> 00:00:02.000\n2024\n\n00:00:02.000 --> 00:00:04.000\nthe year ahead\n";
        let t = parse_subtitle(vtt);
        assert!(t.contains("2024"), "numeric caption kept: {t:?}");
        assert!(t.contains("the year ahead"), "{t:?}");
    }

    #[test]
    fn parse_subtitle_handles_srt_numbering() {
        let srt = "1\n00:00:00,000 --> 00:00:02,000\nfirst line\n\n2\n00:00:02,000 --> 00:00:04,000\nsecond line\n";
        let t = parse_subtitle(srt);
        assert!(t.contains("first line") && t.contains("second line"), "{t}");
        assert!(
            !t.contains('1') || !t.lines().any(|l| l == "1"),
            "cue numbers dropped: {t}"
        );
    }

    #[test]
    fn yt_dlp_args_request_vtt_subs_without_download() {
        let args = yt_dlp_subtitle_args("https://youtu.be/abc", Path::new("/tmp/out"));
        assert!(args.iter().any(|a| a == "--skip-download"), "{args:?}");
        assert!(args.iter().any(|a| a == "--write-auto-subs"), "{args:?}");
        assert!(args.iter().any(|a| a == "vtt"), "vtt format: {args:?}");
        assert_eq!(
            args.last().map(String::as_str),
            Some("https://youtu.be/abc")
        );
    }

    #[test]
    fn transcribe_args_set_model_txt_output_and_audio() {
        let args = transcribe_args(Path::new("/a/clip.mp3"), Path::new("/out"), "small");
        assert!(
            args.windows(2).any(|w| w == ["--model", "small"]),
            "{args:?}"
        );
        assert!(args.iter().any(|a| a == "txt"), "txt output: {args:?}");
        assert!(
            args.iter().any(|a| a.ends_with("clip.mp3")),
            "audio: {args:?}"
        );
    }
}
