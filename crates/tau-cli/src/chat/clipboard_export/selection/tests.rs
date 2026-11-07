use super::super::command::Command;
use super::*;

/// Uses a valid content address without depending on the artifact store.
fn key() -> ArtifactKey {
    "a".repeat(24).parse().expect("valid key")
}

/// Exercises the actual command-to-policy seam, not a separate test-only mode.
fn select(options: &str, bytes: &[u8]) -> Result<Selection, &'static str> {
    let command = Command::parse(&format!("{} {options}", key()))?;
    Selection::select(&command.key, command.mode, bytes)
}

/// Checks both file operation choice and the complete deterministic basename.
fn assert_file(options: &str, bytes: &[u8], suffix: &str) {
    let Selection::File(name) = select(options, bytes).expect("selected file") else {
        panic!("expected copied file")
    };
    assert_eq!(
        name.as_str(),
        format!("artifact-{}.{suffix}", "a".repeat(24))
    );
}

/// Ordinary complete UTF-8 stays text, including source/HTML/SVG, while binary
/// controls and malformed tails cannot gain a text alias.
#[test]
fn conservative_text_and_unknown_binary() {
    for bytes in [
        b"".as_slice(),
        b"hello\tworld\r\n",
        "résumé".as_bytes(),
        b"<svg xmlns='http://www.w3.org/2000/svg'></svg>",
        b"<!DOCTYPE html><script>alert(1)</script>",
        b"{\"key\": 1}",
        b"ID3 is a tag format, not proof that this text is music.",
    ] {
        let Selection::Raw(media) = select("", bytes).expect("text") else {
            panic!("expected text")
        };
        assert_eq!(media.mime, "text/plain;charset=utf-8");
        assert!(media.plain_alias);
        assert_file("--file", bytes, "txt");
    }
    for bytes in [b"\0".as_slice(), b"hello\xff", b"hello\x1b[0m", b"\xef\xbb"] {
        assert_file("", bytes, "bin");
        let Selection::Raw(media) = select("--raw", bytes).expect("raw binary") else {
            panic!("expected raw binary")
        };
        assert_eq!(media.mime, "application/octet-stream");
        assert!(!media.plain_alias);
    }
}

/// Existing native-image behavior must survive automatic file defaults, while
/// explicit --file gets the matching suffix without transcoding.
#[test]
fn images_stay_native_unless_file_requested() {
    for (bytes, mime, suffix) in [
        (b"\x89PNG\r\n\x1a\n".as_slice(), "image/png", "png"),
        (b"\xff\xd8\xff", "image/jpeg", "jpg"),
        (b"GIF87a", "image/gif", "gif"),
        (b"GIF89a", "image/gif", "gif"),
        (b"RIFF\0\0\0\0WEBPVP8 ", "image/webp", "webp"),
    ] {
        let Selection::Raw(media) = select("", bytes).expect("native image") else {
            panic!("expected native image")
        };
        assert_eq!(media.mime, mime);
        assert!(!media.plain_alias);
        assert_file("--file", bytes, suffix);
    }
}

/// Common non-image signatures select files even when their short headers are
/// printable; --raw preserves a way to offer their MIME bytes directly.
#[test]
fn common_binary_formats_default_to_named_files() {
    for (bytes, mime, suffix) in [
        (b"%PDF-1.7".as_slice(), "application/pdf", "pdf"),
        (b"PK\x03\x04", "application/zip", "zip"),
        (b"PK\x05\x06", "application/zip", "zip"),
        (b"\x1f\x8b\x08", "application/gzip", "gz"),
        (b"Rar!\x1a\x07\x00", "application/vnd.rar", "rar"),
        (b"Rar!\x1a\x07\x01\x00", "application/vnd.rar", "rar"),
        (b"ID3\x04\0\0\0\0\0\0", "audio/mpeg", "mp3"),
        (b"OggS\0", "application/ogg", "ogg"),
        (b"RIFF\0\0\0\0WAVE", "audio/wav", "wav"),
        (b"RIFF\0\0\0\0AVI ", "video/avi", "avi"),
        (b"\0\0\0\x18ftypisom\0\0\0\0isommp42", "video/mp4", "mp4"),
    ] {
        assert_file("", bytes, suffix);
        assert_file("--file", bytes, suffix);
        let Selection::Raw(media) = select("--raw", bytes).expect("raw format") else {
            panic!("expected raw format")
        };
        assert_eq!(media.mime, mime);
        assert!(!media.plain_alias);
    }
}

/// Explicit names and MIME declarations win over sniffing, and a private MIME
/// declaration cannot select copied-file authority or bypass UTF-8 validation.
#[test]
fn overrides_remain_literal_and_authoritative() {
    let Selection::File(name) =
        select("--file --name 'report.custom'", b"%PDF-1.7").expect("literal name")
    else {
        panic!("expected file")
    };
    assert_eq!(name.as_str(), "report.custom");
    for mime in [
        "text/plain",
        "image/svg+xml",
        "application/vnd.tau.file-export-v1",
    ] {
        let Selection::Raw(media) =
            select(&format!("--mime {mime}"), b"%PDF-1.7").expect("explicit MIME")
        else {
            panic!("expected raw override")
        };
        assert_eq!(media.mime, mime);
        assert!(!media.plain_alias);
    }
    assert!(select("--mime text/plain;charset=utf-8", b"\xff").is_err());
    assert!(select("--raw --mime text/plain;charset=utf-8", b"\xff").is_err());
}

/// Incomplete, unrelated, and oversized containers must not masquerade as
/// recognized MP4, and the version field is not a compatible brand.
#[test]
fn bounded_signatures_reject_near_misses() {
    for bytes in [
        b"\0\0\0\x18ftypisom".as_slice(),
        b"\0\0\0\x10ftypisommp42",
        b"\0\0\0\x10ftypavif\0\0\0\0",
        b"\0\0\0\x11ftypmp42\0\0\0\0!",
        b"RIFF\0\0\0\0NOPE",
        b"PK\x03",
        b"\x1f\x8b",
    ] {
        assert!(binary_mime(bytes).is_none());
        assert_file("", bytes, "bin");
    }
    let mut oversized = vec![0; 516];
    oversized[..12].copy_from_slice(b"\0\0\x02\x04ftypmp42");
    assert!(binary_mime(&oversized).is_none());
    assert_file("", &oversized, "bin");
    for bytes in [
        b"ID3".as_slice(),
        b"ID3\x04\0\0\0\0\0",
        b"ID3\xff\0\0\0\0\0\0",
        b"ID3\x04\0\0\x80\0\0\0",
    ] {
        assert!(binary_mime(bytes).is_none());
    }
}
