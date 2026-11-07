use std::fs;
use std::io::Write;
use std::os::unix::fs::{PermissionsExt, symlink};

use zstd::stream::write::Encoder;

use super::*;

/// Round trips nested, empty, binary and executable source files privately;
/// source metadata and insertion order do not change same-version encoding.
#[test]
fn roundtrip_is_private_and_repeatable() {
    let source = tempfile::tempdir().expect("private source");
    fs::create_dir(source.path().join("empty")).expect("empty directory");
    fs::create_dir(source.path().join("nested")).expect("nested directory");
    fs::write(source.path().join("nested/data"), [0, 255, 1]).expect("binary fixture");
    fs::write(source.path().join("zero"), []).expect("empty fixture");
    let bytes = pack(source.path(), Limits::default(), || false).expect("encode directory");
    fs::set_permissions(
        source.path().join("nested/data"),
        fs::Permissions::from_mode(0o755),
    )
    .expect("change source permissions");
    assert_eq!(
        bytes,
        pack(source.path(), Limits::default(), || false).expect("encode again")
    );
    let decoded = unpack(&bytes, Limits::default(), || false).expect("extract valid bundle");
    assert_eq!(
        fs::read(decoded.path().join("nested/data")).expect("imported bytes"),
        [0, 255, 1]
    );
    assert!(decoded.path().join("empty").is_dir());
    assert_eq!(
        fs::metadata(decoded.path().join("nested/data"))
            .expect("imported metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(decoded.path())
            .expect("imported root")
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
}

/// Enforces exact aggregate and encoded-byte boundaries in both directions.
#[test]
fn byte_limits_and_cancellation() {
    let source = tempfile::tempdir().expect("private source");
    fs::write(source.path().join("data"), [1; 1024]).expect("source bytes");
    let mut limits = Limits {
        expanded_bytes: 1024,
        ..Limits::default()
    };
    let bytes = pack(source.path(), limits, || false).expect("encode at expanded limit");
    limits.stored_bytes = bytes.len() as u64;
    assert_eq!(
        bytes,
        pack(source.path(), limits, || false).expect("encode at stored limit")
    );
    unpack(&bytes, limits, || false).expect("extract at both limits");
    limits.stored_bytes -= 1;
    assert!(pack(source.path(), limits, || false).is_err());
    assert!(unpack(&bytes, limits, || false).is_err());
    limits = Limits {
        expanded_bytes: 1023,
        ..Limits::default()
    };
    assert!(pack(source.path(), limits, || false).is_err());
    assert!(unpack(&bytes, limits, || false).is_err());
    assert!(pack(source.path(), Limits::default(), || true).is_err());
    assert!(unpack(&bytes, Limits::default(), || true).is_err());
}

/// Symlink traversal is forbidden for both the selected root and its members.
#[test]
fn rejects_export_links() {
    let source = tempfile::tempdir().expect("private source");
    let outside = tempfile::tempdir().expect("outside root");
    symlink(outside.path(), source.path().join("link")).expect("source symlink");
    assert!(pack(source.path(), Limits::default(), || false).is_err());
    assert!(pack(&source.path().join("link"), Limits::default(), || false).is_err());
    for suffix in ["link/", "link/.", "link/./"] {
        assert!(pack(&source.path().join(suffix), Limits::default(), || false).is_err());
    }
}

/// Typed bundles fail closed for unknown versions, truncated frames, appended
/// bytes and checksum corruption rather than silently becoming regular files.
#[test]
fn rejects_malformed_frames() {
    let source = tempfile::tempdir().expect("private source");
    let bytes = pack(source.path(), Limits::default(), || false).expect("valid bundle");
    for size in MAGIC.len()..bytes.len() {
        assert!(is_directory(&bytes[..size]));
        assert!(unpack(&bytes[..size], Limits::default(), || false).is_err());
    }
    let mut bad = bytes.clone();
    bad[MAGIC.len()] = 2;
    assert!(unpack(&bad, Limits::default(), || false).is_err());
    bad = bytes.clone();
    bad.push(0);
    assert!(unpack(&bad, Limits::default(), || false).is_err());
    bad = bytes.clone();
    *bad.last_mut().expect("nonempty frame") ^= 1;
    assert!(unpack(&bad, Limits::default(), || false).is_err());
}

fn bundle(entries: &[(&str, tar::EntryType)]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for (path, kind) in entries {
        let mut header = tar::Header::new_ustar();
        header.set_path(path).expect("fixture path");
        header.set_entry_type(*kind);
        header.set_size(0);
        header.set_mode(0o777);
        header.set_cksum();
        builder
            .append(&header, std::io::empty())
            .expect("fixture tar entry");
    }
    let tar = builder.into_inner().expect("finish tar");
    let mut bytes = MAGIC.to_vec();
    bytes.push(VERSION);
    let mut encoder = Encoder::new(&mut bytes, 3).expect("zstd encoder");
    encoder.write_all(&tar).expect("compress tar");
    encoder.finish().expect("finish frame");
    bytes
}

/// Raw iteration prevents tar link, sparse and metadata interpretation;
/// duplicate paths and file/directory collisions cannot overwrite a previously
/// created file.
#[test]
fn rejects_unsupported_members_and_collisions() {
    for kind in [
        tar::EntryType::Symlink,
        tar::EntryType::Link,
        tar::EntryType::Fifo,
        tar::EntryType::GNUSparse,
        tar::EntryType::XGlobalHeader,
    ] {
        assert!(unpack(&bundle(&[("bad", kind)]), Limits::default(), || false).is_err());
    }
    for entries in [
        vec![
            ("same", tar::EntryType::Regular),
            ("same", tar::EntryType::Regular),
        ],
        vec![
            ("same", tar::EntryType::Regular),
            ("same/child", tar::EntryType::Regular),
        ],
        vec![("back\\slash", tar::EntryType::Regular)],
        vec![("C:prefix", tar::EntryType::Regular)],
    ] {
        assert!(unpack(&bundle(&entries), Limits::default(), || false).is_err());
    }
}

/// Compression cannot evade the logical size budget; the default intentionally
/// permits directories larger than the former 16 MiB original-byte ceiling.
#[test]
fn substantial_compressed_directory_obeys_expanded_limit() {
    let source = tempfile::tempdir().expect("private source");
    let file = fs::File::create(source.path().join("large")).expect("large source");
    file.set_len(32 * 1024 * 1024).expect("exact expanded size");
    let bytes = pack(source.path(), Limits::default(), || false).expect("encode large source");
    assert!(bytes.len() < 1024 * 1024);
    let decoded = unpack(&bytes, Limits::default(), || false).expect("extract large source");
    assert_eq!(
        fs::metadata(decoded.path().join("large"))
            .expect("expanded metadata")
            .len(),
        32 * 1024 * 1024
    );
    file.set_len(32 * 1024 * 1024 + 1)
        .expect("over-limit source size");
    assert!(pack(source.path(), Limits::default(), || false).is_err());
}

/// Path validation rejects traversal before touching any path outside staging,
/// including absolute, parent, empty, platform-prefixed and overdeep names.
#[test]
fn unsafe_paths_are_rejected() {
    for path in [
        "/absolute",
        "../parent",
        "a/../parent",
        "./dot",
        "a//b",
        "a/",
        "C:drive",
    ] {
        let mut header = tar::Header::new_ustar();
        header.set_size(0);
        header.set_mode(0o600);
        header.set_entry_type(tar::EntryType::Regular);
        header.as_mut_bytes()[..path.len()].copy_from_slice(path.as_bytes());
        header.set_cksum();
        let mut tar = header.as_bytes().to_vec();
        tar.extend_from_slice(&[0; 1024]);
        let mut bytes = MAGIC.to_vec();
        bytes.push(VERSION);
        bytes.extend(zstd::stream::encode_all(&tar[..], 3).expect("compress malicious tar"));
        assert!(
            unpack(&bytes, Limits::default(), || false).is_err(),
            "{path}"
        );
    }
    assert!(
        check_path(
            &std::iter::repeat_n("a", MAX_DEPTH + 1)
                .collect::<Vec<_>>()
                .join("/")
        )
        .is_err()
    );
}

/// Thousands of empty members still consume finite entry and metadata budgets.
#[test]
fn entry_count_is_bounded() {
    let names = (0..=MAX_ENTRIES)
        .map(|i| format!("f{i}"))
        .collect::<Vec<_>>();
    let entries = names
        .iter()
        .map(|name| (name.as_str(), tar::EntryType::Regular))
        .collect::<Vec<_>>();
    assert!(unpack(&bundle(&entries), Limits::default(), || false).is_err());
}
