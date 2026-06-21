//! Image (vision) handling for the semantic extraction pass: image-ref
//! discovery, per-image notes, Anthropic/OpenAI content formatting, and
//! per-backend vision capability detection.
//!
//! This is **pure payload formatting** — building the `messages[].content` value
//! a vision model expects, plus the text "emit one node per image" block that
//! guarantees an image becomes a graph node even when the backend can't see
//! pixels. No network, so it's fully offline-testable. Sending the payload (and
//! packing images into extraction chunks) is left to the caller.
//!
//! Scope: the two requested wire shapes — Anthropic (`{type:image, source:
//! base64}`) and OpenAI-compatible (`{type:image_url, …data-URI…}`). Bedrock
//! (raw bytes) and the claude-cli Read-tool path remain deferred.

use std::path::{Path, PathBuf};

use base64::Engine;
use serde_json::{json, Value};

use crate::registry::BACKENDS;

/// Raster image types a vision model can actually look at. `.svg` is excluded
/// (vector — sent as text elsewhere).
pub const VISION_IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp"];
/// Per-image byte ceiling. 5 MB keeps every backend within request limits;
/// oversized images fall back to a text reference (the node is still created).
pub const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;
/// Flat token estimate per image for chunk packing (vision models bill an image
/// at a roughly fixed cost regardless of byte size).
pub const IMAGE_TOKEN_ESTIMATE: usize = 1_600;
/// Hard cap on images per chunk, independent of the token budget.
pub const MAX_IMAGES_PER_CHUNK: usize = 20;
/// Backends that read an image by file path (claude-cli's Read tool) rather than
/// inlining base64 — they don't need bytes loaded or size-capped.
pub const PATH_IMAGE_BACKENDS: &[&str] = &["claude-cli"];

/// Ollama opt-in: vision is off by default (its default model is text-only) and
/// enabled by setting this to `1` once a vision model is selected.
const OLLAMA_VISION_ENV: &str = "SYNAPTIC_OLLAMA_VISION";

/// Media type for a raster image path (defaults to `image/png`).
fn media_type_for(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        _ => "image/png",
    }
}

/// True when `path` is a raster image a vision model can view.
pub fn is_vision_image(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| VISION_IMAGE_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

/// Split files into `(text-like, raster-image)`.
pub fn partition_semantic_files(files: &[PathBuf]) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let mut text = Vec::new();
    let mut images = Vec::new();
    for f in files {
        if is_vision_image(f) {
            images.push(f.clone());
        } else {
            text.push(f.clone());
        }
    }
    (text, images)
}

/// A single image destined for a vision request. `raw` is `None` when the image
/// is unreadable, over [`MAX_IMAGE_BYTES`], or the backend can't see pixels — in
/// every such case the renderers emit a text reference so the image still
/// becomes a graph node.
#[derive(Debug, Clone, PartialEq)]
pub struct ImageRef {
    /// Absolute path (path-based backends read it directly).
    pub path: PathBuf,
    /// Path relative to the corpus root (posix) — the node's `source_file`.
    pub rel: String,
    /// e.g. `image/png`.
    pub media_type: String,
    pub raw: Option<Vec<u8>>,
}

impl ImageRef {
    /// Standard base64 of the pixels (empty when there are none).
    pub fn b64(&self) -> String {
        match &self.raw {
            Some(bytes) => base64::engine::general_purpose::STANDARD.encode(bytes),
            None => String::new(),
        }
    }

    /// The bare format token Bedrock Converse wants (`png`, not `image/png`).
    pub fn bedrock_format(&self) -> &str {
        self.media_type
            .split_once('/')
            .map(|(_, f)| f)
            .unwrap_or(&self.media_type)
    }
}

/// Build [`ImageRef`]s for raster images. `read_bytes=true` (inline backends)
/// loads the pixels and drops any image over [`MAX_IMAGE_BYTES`] to a reference;
/// `read_bytes=false` (path-based backends) skips the read entirely.
pub fn build_image_refs(image_files: &[PathBuf], root: &Path, read_bytes: bool) -> Vec<ImageRef> {
    let mut refs = Vec::with_capacity(image_files.len());
    for p in image_files {
        let rel = p
            .strip_prefix(root)
            .unwrap_or(p)
            .to_string_lossy()
            .replace('\\', "/");
        let media = media_type_for(p).to_string();
        let mut raw: Option<Vec<u8>> = None;
        if read_bytes {
            match std::fs::read(p) {
                Ok(bytes) if bytes.len() > MAX_IMAGE_BYTES => {
                    eprintln!(
                        "[synaptic] image {rel} is {} KB, over the {} MB inline-image limit; \
                         sending it as a reference node without inline pixels.",
                        bytes.len() / 1024,
                        MAX_IMAGE_BYTES / (1024 * 1024)
                    );
                }
                Ok(bytes) => raw = Some(bytes),
                Err(e) => eprintln!("[synaptic] could not read image {rel}: {e}"),
            }
        }
        let abs_path = std::fs::canonicalize(p).unwrap_or_else(|_| p.clone());
        refs.push(ImageRef {
            path: abs_path,
            rel,
            media_type: media,
            raw,
        });
    }
    refs
}

/// Drop pixel data from every ref (for non-vision backends).
pub fn strip_pixels(refs: &[ImageRef]) -> Vec<ImageRef> {
    refs.iter()
        .map(|r| ImageRef {
            raw: None,
            ..r.clone()
        })
        .collect()
}

/// Whether `backend`'s configured model can see images. Ollama is the opt-in
/// special case (`OLLAMA_VISION_ENV`=`1`); everything else uses the registry
/// `vision` flag.
pub fn backend_supports_vision(backend: &str, get: &impl Fn(&str) -> Option<String>) -> bool {
    if backend == "ollama" {
        return get(OLLAMA_VISION_ENV)
            .map(|v| v.trim() == "1")
            .unwrap_or(false);
    }
    BACKENDS
        .iter()
        .find(|c| c.name == backend)
        .map(|c| c.vision)
        .unwrap_or(false)
}

/// Text block listing the images so the model emits one node per image. Always
/// accompanies the visual payload (and stands alone when the backend is blind),
/// so an image becomes a graph node either way. `with_paths` lists the absolute
/// path and asks the model to open it with a Read tool (claude-cli).
pub fn image_notes(refs: &[ImageRef], with_paths: bool) -> String {
    if refs.is_empty() {
        return String::new();
    }
    let header = if with_paths {
        "Use the Read tool to open and view each image file at the path below, \
         then emit one node per image"
    } else {
        "The following image file(s) are attached as visual input. Emit one \
         node per image"
    };
    let mut lines = vec![
        "=== IMAGES ===".to_string(),
        format!(
            "{header} with \"file_type\":\"image\" and the listed source_file, a label \
             describing what it depicts (diagram, screenshot, chart, photo, UI, logo), \
             and edges to any code/doc nodes the image clearly references."
        ),
    ];
    for (i, r) in refs.iter().enumerate() {
        let mut note = format!("[image {}] source_file: {}", i + 1, r.rel);
        if with_paths {
            note.push_str(&format!("  path: {}", r.path.display()));
        }
        if r.raw.is_none() && !with_paths {
            note.push_str(" (not shown: unreadable or exceeds size limit)");
        }
        lines.push(note);
    }
    lines.join("\n")
}

/// Append the image notes to a user message (notes alone if the message is empty).
pub fn with_image_notes(user_message: &str, refs: &[ImageRef], with_paths: bool) -> String {
    let notes = image_notes(refs, with_paths);
    if notes.is_empty() {
        return user_message.to_string();
    }
    if user_message.trim().is_empty() {
        return notes;
    }
    format!("{user_message}\n\n{notes}")
}

/// Anthropic `messages[].content`: a plain string when there are no visible
/// images, else `[image blocks…, {type:text}]`.
pub fn anthropic_content(user_message: &str, refs: &[ImageRef]) -> Value {
    let blocks: Vec<Value> = refs
        .iter()
        .filter(|r| r.raw.is_some())
        .map(|r| {
            json!({
                "type": "image",
                "source": { "type": "base64", "media_type": r.media_type, "data": r.b64() }
            })
        })
        .collect();
    let text = with_image_notes(user_message, refs, false);
    if blocks.is_empty() {
        return Value::String(text);
    }
    let mut content = blocks;
    content.push(json!({ "type": "text", "text": text }));
    Value::Array(content)
}

/// OpenAI-compatible user `content`: a plain string when there are no visible
/// images, else `[{type:text}, image_url parts…]` (text first).
pub fn openai_content(user_message: &str, refs: &[ImageRef]) -> Value {
    let parts: Vec<Value> = refs
        .iter()
        .filter(|r| r.raw.is_some())
        .map(|r| {
            json!({
                "type": "image_url",
                "image_url": {
                    "url": format!("data:{};base64,{}", r.media_type, r.b64()),
                    "detail": "auto"
                }
            })
        })
        .collect();
    let text = with_image_notes(user_message, refs, false);
    if parts.is_empty() {
        return Value::String(text);
    }
    let mut content = vec![json!({ "type": "text", "text": text })];
    content.extend(parts);
    Value::Array(content)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn img(rel: &str, raw: Option<&[u8]>) -> ImageRef {
        ImageRef {
            path: PathBuf::from(rel),
            rel: rel.to_string(),
            media_type: media_type_for(Path::new(rel)).to_string(),
            raw: raw.map(|b| b.to_vec()),
        }
    }

    #[test]
    fn detects_raster_images_and_media_types() {
        assert!(is_vision_image(Path::new("a/b.PNG")));
        assert!(is_vision_image(Path::new("x.jpeg")));
        assert!(!is_vision_image(Path::new("x.svg")));
        assert!(!is_vision_image(Path::new("x.rs")));
        assert_eq!(media_type_for(Path::new("x.JPG")), "image/jpeg");
        assert_eq!(media_type_for(Path::new("x.webp")), "image/webp");
        assert_eq!(media_type_for(Path::new("x.bin")), "image/png");
    }

    #[test]
    fn build_refs_reads_caps_and_relativizes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let small = root.join("sub/a.png");
        std::fs::create_dir_all(small.parent().unwrap()).unwrap();
        std::fs::write(&small, b"\x89PNGsmall").unwrap();
        let big = root.join("b.png");
        std::fs::write(&big, vec![0u8; MAX_IMAGE_BYTES + 1]).unwrap();

        let refs = build_image_refs(&[small.clone(), big.clone()], root, true);
        assert_eq!(refs[0].rel, "sub/a.png", "posix-relative to root");
        assert!(refs[0].raw.is_some(), "small image loaded");
        assert!(
            refs[1].raw.is_none(),
            "oversized image dropped to a reference"
        );

        // read_bytes=false never loads pixels.
        let none = build_image_refs(&[small], root, false);
        assert!(none[0].raw.is_none());
    }

    #[test]
    fn b64_encodes_pixels() {
        let r = img("a.png", Some(b"hello"));
        assert_eq!(r.b64(), "aGVsbG8=");
        assert_eq!(img("a.png", None).b64(), "");
        assert_eq!(img("a.gif", Some(b"x")).bedrock_format(), "gif");
    }

    #[test]
    fn vision_support_follows_registry_and_ollama_optin() {
        let no = |_: &str| None;
        assert!(backend_supports_vision("gemini", &no));
        assert!(backend_supports_vision("openai", &no));
        assert!(!backend_supports_vision("deepseek", &no));
        assert!(!backend_supports_vision("unknown", &no));
        // Ollama: off unless the opt-in env is exactly "1".
        assert!(!backend_supports_vision("ollama", &no));
        let on = |k: &str| (k == OLLAMA_VISION_ENV).then(|| "1".to_string());
        assert!(backend_supports_vision("ollama", &on));
    }

    #[test]
    fn image_notes_lists_each_image() {
        assert_eq!(image_notes(&[], false), "");
        let refs = vec![img("docs/a.png", Some(b"x")), img("docs/big.png", None)];
        let notes = image_notes(&refs, false);
        assert!(notes.contains("=== IMAGES ==="));
        assert!(notes.contains("[image 1] source_file: docs/a.png"));
        assert!(notes.contains("[image 2] source_file: docs/big.png"));
        assert!(notes.contains("(not shown: unreadable or exceeds size limit)"));
        // with_paths switches the header + appends the absolute path.
        let p = image_notes(&refs, true);
        assert!(p.contains("Use the Read tool"));
        assert!(p.contains("path:"));
        assert!(!p.contains("(not shown")); // path backends always "see" the file
    }

    #[test]
    fn anthropic_content_shape() {
        // No visible image -> plain string carrying the notes.
        let txt = anthropic_content("extract", &[img("a.png", None)]);
        assert!(txt.is_string());
        assert!(txt.as_str().unwrap().contains("=== IMAGES ==="));
        // Visible image -> [image block, text block].
        let v = anthropic_content("extract", &[img("a.png", Some(b"hi"))]);
        let arr = v.as_array().unwrap();
        assert_eq!(arr[0]["type"], json!("image"));
        assert_eq!(arr[0]["source"]["type"], json!("base64"));
        assert_eq!(arr[0]["source"]["media_type"], json!("image/png"));
        assert_eq!(arr[0]["source"]["data"], json!("aGk="));
        assert_eq!(arr[1]["type"], json!("text"));
    }

    #[test]
    fn openai_content_shape() {
        let v = openai_content("extract", &[img("a.jpg", Some(b"hi"))]);
        let arr = v.as_array().unwrap();
        // Text first, then the image_url data-URI part.
        assert_eq!(arr[0]["type"], json!("text"));
        assert_eq!(arr[1]["type"], json!("image_url"));
        assert_eq!(
            arr[1]["image_url"]["url"],
            json!("data:image/jpeg;base64,aGk=")
        );
        assert_eq!(arr[1]["image_url"]["detail"], json!("auto"));
        // No visible image -> string.
        assert!(openai_content("extract", &[]).is_string());
    }
}
