//! Session-scoped image attachments.
//!
//! One ingest path for every front-end (TUI Ctrl-V, REPL `/attach` and `/paste`,
//! the VS Code webview via `--image`): bytes land in a directory next to the
//! session JSONL, and the turn that follows carries them as `ContentBlock::Image`.
//!
//! Attachments live beside the session, not in the project tree: a screenshot in
//! the workspace would show up in `git status`, in `glob`, and in `code_map`. And
//! not in `/tmp` either — sessions are resumable, so the file has to outlive the
//! boot that created it.
//!
//! Whether the model can actually *see* the image is a mechanical property of the
//! endpoint, not something the prompt negotiates: `set_vision_supported` records
//! what `main` resolved — inferred for Anthropic, declared with `--vision` on an
//! OpenAI-compatible endpoint — and every front-end warns off the same fact.

use std::hash::{DefaultHasher, Hash as _, Hasher as _};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{bail, Context as _, Result};

use crate::types::ContentBlock;

/// Refuse anything past this. Anthropic rejects oversized images anyway, and a
/// stray clipboard bitmap should not silently push a session into the hundreds
/// of megabytes.
const MAX_BYTES: usize = 5 * 1024 * 1024;

static VISION_SUPPORTED: OnceLock<bool> = OnceLock::new();

/// Record whether the active endpoint really reads images (resolved in `main`
/// from the provider and base URL). First write wins.
pub fn set_vision_supported(ok: bool) {
    let _ = VISION_SUPPORTED.set(ok);
}

/// True when the active endpoint reads images. Defaults to `false`, so a caller
/// that never resolved a provider warns instead of promising vision.
pub fn vision_supported() -> bool {
    *VISION_SUPPORTED.get().unwrap_or(&false)
}

/// The warning shown when an image is attached to an endpoint that ignores it.
pub const NO_VISION_WARNING: &str =
    "attached, but the active endpoint is not declared vision-capable — the image \
     will be ignored or hallucinated; pass --vision (SIRBONE_VISION=1) on an \
     OpenAI-compatible endpoint that reads images";

/// One stored image, owned by the session that ingested it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    /// Short human-facing label (`image_1`), assigned by the front-end.
    pub label: String,
    pub path: PathBuf,
    pub media_type: String,
    pub bytes: usize,
    /// Pixel size when known — clipboard images always, files never (no decode).
    pub dimensions: Option<(u32, u32)>,
}

impl Attachment {
    /// `image_1 · 1440×900 · 214 KB`
    pub fn describe(&self) -> String {
        let size = format!("{} KB", self.bytes.div_ceil(1024));
        match self.dimensions {
            Some((w, h)) => format!("{} · {w}×{h} · {size}", self.label),
            None => format!("{} · {size}", self.label),
        }
    }

    /// Base64-encode for the wire. Reads from disk so the bytes are never held
    /// twice: the file is the attachment, the block is a projection of it.
    pub fn to_block(&self) -> Result<ContentBlock> {
        use base64::Engine as _;
        let data = std::fs::read(&self.path)
            .with_context(|| format!("reading attachment {}", self.path.display()))?;
        Ok(ContentBlock::Image {
            media_type: self.media_type.clone(),
            data: base64::engine::general_purpose::STANDARD.encode(&data),
        })
    }
}

/// Attachment directory for a session JSONL path: `<session>.attachments/`.
pub fn dir_for(session_path: &Path) -> PathBuf {
    let mut name = session_path.file_stem().unwrap_or_default().to_os_string();
    name.push(".attachments");
    session_path.with_file_name(name)
}

fn media_type_for(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => "image/png",
    }
}

/// Content-addressed name, so pasting the same screenshot twice stores one file.
/// Not a security boundary — just deduplication within a session.
fn digest(bytes: &[u8]) -> String {
    let mut h = DefaultHasher::new();
    bytes.hash(&mut h);
    format!("{:016x}", h.finish())
}

fn write_once(dir: &Path, name: &str, bytes: &[u8]) -> Result<PathBuf> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("creating attachment dir {}", dir.display()))?;
    let path = dir.join(name);
    if !path.exists() {
        std::fs::write(&path, bytes)
            .with_context(|| format!("writing attachment {}", path.display()))?;
    }
    Ok(path)
}

/// Copy an image file into the session store.
pub fn save_file(dir: &Path, label: impl Into<String>, src: &Path) -> Result<Attachment> {
    let bytes = std::fs::read(src).with_context(|| format!("reading image {}", src.display()))?;
    if bytes.len() > MAX_BYTES {
        bail!(
            "image is {} MB — the limit is {} MB",
            bytes.len() / (1024 * 1024),
            MAX_BYTES / (1024 * 1024)
        );
    }
    let media_type = media_type_for(src);
    let ext = media_type.rsplit('/').next().unwrap_or("png");
    let path = write_once(dir, &format!("{}.{ext}", digest(&bytes)), &bytes)?;
    Ok(Attachment {
        label: label.into(),
        path,
        media_type: media_type.to_string(),
        bytes: bytes.len(),
        dimensions: None,
    })
}

/// Encode raw RGBA into PNG and store it. PNG, not JPEG: screenshots are text,
/// icons and hairlines, and JPEG ringing is exactly what a vision model misreads.
pub fn save_rgba(
    dir: &Path,
    label: impl Into<String>,
    width: u32,
    height: u32,
    rgba: &[u8],
) -> Result<Attachment> {
    let mut encoded = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut encoded, width, height);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header().context("writing PNG header")?;
        writer.write_image_data(rgba).context("encoding PNG")?;
    }
    if encoded.len() > MAX_BYTES {
        bail!(
            "clipboard image encodes to {} MB — the limit is {} MB",
            encoded.len() / (1024 * 1024),
            MAX_BYTES / (1024 * 1024)
        );
    }
    let path = write_once(dir, &format!("{}.png", digest(&encoded)), &encoded)?;
    Ok(Attachment {
        label: label.into(),
        path,
        media_type: "image/png".to_string(),
        bytes: encoded.len(),
        dimensions: Some((width, height)),
    })
}

/// Read an image from the system clipboard. Only ever called from an explicit
/// user gesture (Ctrl-V, `/paste`) — sirbone never polls the clipboard on its own.
pub fn from_clipboard(dir: &Path, label: impl Into<String>) -> Result<Attachment> {
    let mut clipboard = arboard::Clipboard::new().context("opening the system clipboard")?;
    let image = clipboard
        .get_image()
        .context("no image in the clipboard (text paste is handled by the terminal)")?;
    let (w, h) = (image.width as u32, image.height as u32);
    save_rgba(dir, label, w, h, &image.bytes)
}

/// `[image_1] [image_2] ` — the inline marker a front-end shows in place of a
/// path, so the composer stays readable.
pub fn pills(items: &[Attachment]) -> String {
    items
        .iter()
        .map(|a| format!("[{}] ", a.label))
        .collect::<String>()
}

/// Build the user message content: images first, then the text. Anthropic reads
/// an image better when the question follows it.
pub fn user_content(items: &[Attachment], text: String) -> Vec<ContentBlock> {
    items
        .iter()
        .filter_map(|a| a.to_block().ok())
        .chain(std::iter::once(ContentBlock::Text { text }))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dir_sits_beside_the_session_file() {
        let dir = dir_for(Path::new("/home/u/.sirbone/sessions/abc.jsonl"));
        assert_eq!(
            dir,
            PathBuf::from("/home/u/.sirbone/sessions/abc.attachments")
        );
    }

    #[test]
    fn identical_bytes_store_one_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let rgba = vec![7u8; 4 * 2 * 2];
        let a = save_rgba(tmp.path(), "image_1", 2, 2, &rgba).expect("first save");
        let b = save_rgba(tmp.path(), "image_2", 2, 2, &rgba).expect("second save");
        assert_eq!(a.path, b.path);
        let stored = std::fs::read_dir(tmp.path()).expect("read dir").count();
        assert_eq!(stored, 1);
    }

    #[test]
    fn rgba_round_trips_to_a_png_block() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let att = save_rgba(tmp.path(), "image_1", 2, 2, &[9u8; 16]).expect("save");
        assert_eq!(att.dimensions, Some((2, 2)));
        assert_eq!(att.media_type, "image/png");
        assert!(att.describe().starts_with("image_1 · 2×2 · "));
        let ContentBlock::Image { media_type, data } = att.to_block().expect("block") else {
            panic!("expected an image block");
        };
        assert_eq!(media_type, "image/png");
        use base64::Engine as _;
        let raw = base64::engine::general_purpose::STANDARD
            .decode(&data)
            .expect("base64");
        assert_eq!(&raw[..8], b"\x89PNG\r\n\x1a\n");
    }

    #[test]
    fn files_keep_their_media_type_and_reject_oversize() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let src = tmp.path().join("shot.jpeg");
        std::fs::write(&src, b"not really a jpeg").expect("write");
        let att = save_file(tmp.path(), "image_1", &src).expect("save");
        assert_eq!(att.media_type, "image/jpeg");
        assert_eq!(att.dimensions, None);

        let big = tmp.path().join("big.png");
        std::fs::write(&big, vec![0u8; MAX_BYTES + 1]).expect("write");
        assert!(save_file(tmp.path(), "image_2", &big).is_err());
    }

    #[test]
    fn images_precede_the_text_in_a_turn() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let att = save_rgba(tmp.path(), "image_1", 1, 1, &[1, 2, 3, 4]).expect("save");
        let content = user_content(std::slice::from_ref(&att), "why does this fail?".into());
        assert!(matches!(content[0], ContentBlock::Image { .. }));
        assert!(matches!(content[1], ContentBlock::Text { .. }));
        assert_eq!(pills(&[att]), "[image_1] ");
    }
}
