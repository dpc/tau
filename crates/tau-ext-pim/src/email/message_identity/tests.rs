use super::{ImapUid, MailboxName, UidValidity};

/// Leading-zero UIDs retain caller bytes while exposing the positive value.
#[test]
fn imap_uid_retains_exact_spelling_and_numeric_value() {
    let uid = ImapUid::parse_exact("0001").expect("leading-zero UID is accepted");

    assert_eq!(uid.raw(), "0001");
    assert_eq!(uid.value.get(), 1);
}

/// Zero, overflow, nondigit, sign, and empty UIDs remain invalid.
#[test]
fn imap_uid_rejects_every_previously_invalid_shape() {
    for raw in ["", "0", "4294967296", "1x", "+1", "-1", " 1"] {
        assert_eq!(
            ImapUid::parse_exact(raw).expect_err("invalid UID shape must fail"),
            "uid must be a positive integer"
        );
    }
}

/// Mailbox parsing preserves accepted whitespace, slash, case, and Unicode.
#[test]
fn mailbox_name_retains_exact_accepted_text() {
    let raw = " Projects/Été ";
    let mailbox = MailboxName::parse_exact(raw).expect("mailbox is valid");

    assert_eq!(mailbox.raw(), raw);
}

/// UIDVALIDITY remains opaque exact provider text rather than a number.
#[test]
fn uidvalidity_retains_opaque_provider_text() {
    let uidvalidity = UidValidity::from_provider("uv1".to_owned());

    assert_eq!(uidvalidity.raw(), "uv1");
}
