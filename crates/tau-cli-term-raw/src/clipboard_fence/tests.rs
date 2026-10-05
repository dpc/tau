use super::*;

/// Neither old replies nor an opening OK alone prove the stream boundary.
#[test]
fn only_complete_correlated_inventory_proves_the_fence() {
    let mut fence = ClipboardFence::new("fresh".into());
    assert_eq!(
        fence.request(),
        b"\x1b[?5522l\x1b]5522;type=read:id=fresh;Lg==\x1b\\"
    );
    assert!(
        !fence
            .receive(b"5522;type=read:id=old:status=DONE")
            .expect("ignore stale response")
    );
    assert!(
        !fence
            .receive(b"5522;type=read:id=fresh:status=OK")
            .expect("valid opening")
    );
    assert!(
        !fence
            .receive(b"5522;type=read:id=fresh:status=DATA:mime=Lg==;")
            .expect("valid empty inventory")
    );
    assert!(
        fence
            .receive(b"5522;type=read:id=fresh:status=DONE")
            .expect("valid completion")
    );
}

/// Errors, missing DATA, malformed metadata and invalid encoding never
/// become a false-positive boundary that launches a child.
#[test]
fn malformed_and_incomplete_fences_are_not_proof() {
    for reply in [
        "type=read:id=fresh:status=EPERM",
        "type=read:id=fresh:status=ENOSYS",
        "type=read:id=fresh:status=EBUSY",
        "type=read:id=fresh:status=DONE",
        "type=read:id=fresh:status=OK:id=fresh;",
        "type=write:id=fresh:status=OK",
        "type=read:id=fresh:status=UNKNOWN",
    ] {
        assert!(
            ClipboardFence::new("fresh".into())
                .receive(format!("5522;{reply}").as_bytes())
                .is_err()
        );
    }
    for reply in [
        "type=read:id=fresh:status=DONE;",
        "type=read:id=fresh:status=OK;",
        "type=read:id=fresh:status=DATA:mime=Lg==;!!!!",
        "type=read:id=fresh:status=DATA:mime=dGV4dC9wbGFpbg==;",
        "type=read:id=fresh:status=DATA:mime=Lg==;YQ",
        "type=read:id=fresh:status=DATA:mime=Lg==",
    ] {
        let mut fence = ClipboardFence::new("fresh".into());
        fence
            .receive(b"5522;type=read:id=fresh:status=OK;")
            .expect("valid opening");
        assert!(fence.receive(format!("5522;{reply}").as_bytes()).is_err());
    }
    let mut fence = ClipboardFence::new("fresh".into());
    fence
        .receive(b"5522;type=read:id=fresh:status=OK;")
        .expect("valid opening");
    fence
        .receive(b"5522;type=read:id=fresh:status=DATA:mime=Lg==;/w==")
        .expect("valid base64 with invalid inventory UTF-8");
    assert!(
        fence
            .receive(b"5522;type=read:id=fresh:status=DONE;")
            .is_err()
    );
}

/// Aggregate and per-chunk bounds prevent a metadata read from becoming an
/// unbounded content transfer.
#[test]
fn fence_inventory_limits_are_enforced() {
    let mut fence = ClipboardFence::new("fresh".into());
    fence
        .receive(b"5522;type=read:id=fresh:status=OK;")
        .expect("valid opening");
    let reply = format!(
        "5522;type=read:id=fresh:status=DATA:mime=Lg==;{}",
        STANDARD.encode(vec![b'a'; 4096])
    );
    for _ in 0..16 {
        assert!(
            !fence
                .receive(reply.as_bytes())
                .expect("inventory within limit")
        );
    }
    assert!(fence.receive(reply.as_bytes()).is_err());
    let mut fence = ClipboardFence::new("fresh".into());
    fence
        .receive(b"5522;type=read:id=fresh:status=OK;")
        .expect("valid opening");
    let reply = format!(
        "5522;type=read:id=fresh:status=DATA:mime=Lg==;{}",
        STANDARD.encode(vec![b'a'; 4097])
    );
    assert!(fence.receive(reply.as_bytes()).is_err());
}
