//! Byte-preserving clipboard policy, local to this UI invocation.

use tau_cli_term::ClipboardFileName;
use tau_client::ArtifactMedia;
use tau_proto::ArtifactKey;

use super::command::Mode;

/// Resolved terminal operation after complete original verification.
pub(super) enum Selection {
    /// Native MIME bytes (including ordinary text), not a desktop file.
    Raw(ArtifactMedia),
    /// Dedicated copied-file operation with a safe local basename.
    File(ClipboardFileName),
}

impl Selection {
    /// Resolves explicit intent first; neither filenames nor catalog hints
    /// influence inference or authorize a path.
    pub(super) fn select(
        key: &ArtifactKey,
        mode: Mode,
        bytes: &[u8],
    ) -> Result<Self, &'static str> {
        let mode = match mode {
            Mode::Raw(Some(mime)) => {
                return ArtifactMedia::select(bytes, Some(&mime)).map(Self::Raw);
            }
            Mode::File(Some(name)) => return Ok(Self::File(name)),
            mode => mode,
        };
        let media = infer(bytes)?;
        let native = media.plain_alias
            || matches!(
                media.mime.as_str(),
                "image/png" | "image/jpeg" | "image/gif" | "image/webp"
            );
        if matches!(mode, Mode::Raw(_)) || (matches!(mode, Mode::Auto) && native) {
            return Ok(Self::Raw(media));
        }
        let digest = tau_proto::artifact_digest(key).expect("validated artifact key");
        ClipboardFileName::new(format!("artifact-{digest}.{}", extension(&media.mime)))
            .map(Self::File)
    }
}

/// Existing image/PDF and complete-buffer text inference plus a small set of
/// inert audio/video/archive signatures. No HTML/XML/SVG or executable
/// sniffing, decompression, container traversal, or body validation.
fn infer(bytes: &[u8]) -> Result<ArtifactMedia, &'static str> {
    if let Some(mime) = binary_mime(bytes) {
        return Ok(ArtifactMedia {
            mime: mime.to_owned(),
            plain_alias: false,
        });
    }
    ArtifactMedia::select(bytes, None)
}

/// Fixed header recognition; MP4 brand scanning is limited to a 512-byte ftyp
/// box. Patterns follow the WHATWG MIME Sniffing audio/video/archive tables,
/// without adopting browser active-content or permissive text sniffing.
fn binary_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"PK\x03\x04") || bytes.starts_with(b"PK\x05\x06") {
        Some("application/zip")
    } else if bytes.starts_with(b"\x1f\x8b\x08") {
        Some("application/gzip")
    } else if bytes.starts_with(b"Rar!\x1a\x07\x00") || bytes.starts_with(b"Rar!\x1a\x07\x01\x00") {
        Some("application/vnd.rar")
    } else if bytes.starts_with(b"ID3")
        && matches!(bytes.get(3), Some(2..=4))
        && bytes.get(4).is_some_and(|revision| *revision != 0xff)
        && bytes
            .get(6..10)
            .is_some_and(|size| size.iter().all(|byte| *byte < 0x80))
    {
        Some("audio/mpeg")
    } else if bytes.starts_with(b"OggS\x00") {
        Some("application/ogg")
    } else if bytes.starts_with(b"RIFF") {
        match bytes.get(8..12) {
            Some(b"WAVE") => Some("audio/wav"),
            Some(b"AVI ") => Some("video/avi"),
            _ => None,
        }
    } else {
        mp4_mime(bytes)
    }
}

/// Recognizes MP4 brands, not every ISO-BMFF container (e.g. AVIF/HEIF).
fn mp4_mime(bytes: &[u8]) -> Option<&'static str> {
    let size = u32::from_be_bytes(bytes.get(..4)?.try_into().ok()?) as usize;
    if !(16..=512).contains(&size) || !size.is_multiple_of(4) || bytes.get(4..8)? != b"ftyp" {
        return None;
    }
    let brands = bytes.get(8..size)?;
    brands
        .as_chunks::<4>()
        .0
        .iter()
        .enumerate()
        .any(|(index, brand)| index != 1 && matches!(brand, b"mp41" | b"mp42"))
        .then_some("video/mp4")
}

/// Only recognized local declarations get a suffix; unknown bytes remain .bin.
fn extension(mime: &str) -> &'static str {
    match mime {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "application/pdf" => "pdf",
        "text/plain;charset=utf-8" => "txt",
        "application/zip" => "zip",
        "application/gzip" => "gz",
        "application/vnd.rar" => "rar",
        "audio/mpeg" => "mp3",
        "application/ogg" => "ogg",
        "audio/wav" => "wav",
        "video/avi" => "avi",
        "video/mp4" => "mp4",
        _ => "bin",
    }
}

#[cfg(test)]
mod tests;
