use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use super::{
    MESSAGE_PAYLOAD_ENVELOPE, PayloadEnvelopeCarrier, PayloadEnvelopeOpening,
    PayloadEnvelopeRenderError, RegisteredPayloadEnvelope, TAU_BACKGROUND_RESULT_PAYLOAD_ENVELOPE,
    TAU_INTERNAL_PAYLOAD_ENVELOPE, TAU_WEB_CONTENT_PAYLOAD_ENVELOPE, USER_PAYLOAD_ENVELOPE,
    escape_exact_sentinel_close, escape_payload_envelope_attribute, registered_payload_envelopes,
};

/// The registry must remain unambiguous and each family must preserve
/// cross-family text while neutralizing only its own exact close.
#[test]
fn registered_families_have_unique_complete_lexical_contracts() {
    let families = registered_payload_envelopes();
    assert_eq!(
        families,
        &[
            USER_PAYLOAD_ENVELOPE,
            TAU_INTERNAL_PAYLOAD_ENVELOPE,
            MESSAGE_PAYLOAD_ENVELOPE,
            TAU_WEB_CONTENT_PAYLOAD_ENVELOPE,
            TAU_BACKGROUND_RESULT_PAYLOAD_ENVELOPE,
        ]
    );
    assert_eq!(
        USER_PAYLOAD_ENVELOPE,
        RegisteredPayloadEnvelope {
            name: "user",
            opening: PayloadEnvelopeOpening::Fixed("<user>"),
            ordered_attributes: &[],
            exact_close: "</user>",
            visible_close: "&lt;/user&gt;",
            carrier: PayloadEnvelopeCarrier::GenericUserText,
        }
    );
    assert_eq!(
        TAU_INTERNAL_PAYLOAD_ENVELOPE,
        RegisteredPayloadEnvelope {
            name: "tau_internal",
            opening: PayloadEnvelopeOpening::Fixed("<tau_internal>"),
            ordered_attributes: &[],
            exact_close: "</tau_internal>",
            visible_close: "&lt;/tau_internal&gt;",
            carrier: PayloadEnvelopeCarrier::GenericUserText,
        }
    );
    assert_eq!(
        MESSAGE_PAYLOAD_ENVELOPE,
        RegisteredPayloadEnvelope {
            name: "message",
            opening: PayloadEnvelopeOpening::Attributed("<message "),
            ordered_attributes: &[
                "event",
                "publisher",
                "message_ref",
                "sender_ref",
                "sender_display",
                "sender_auth",
                "recipient_ref",
                "recipient_display",
                "conversation",
                "reaction",
                "content_trust",
            ],
            exact_close: "</message>",
            visible_close: "&lt;/message&gt;",
            carrier: PayloadEnvelopeCarrier::GenericUserOrAssistantText,
        }
    );
    assert_eq!(
        TAU_WEB_CONTENT_PAYLOAD_ENVELOPE,
        RegisteredPayloadEnvelope {
            name: "tau_web_content",
            opening: PayloadEnvelopeOpening::Attributed("<tau_web_content "),
            ordered_attributes: &["adapter", "operation", "content_trust"],
            exact_close: "</tau_web_content>",
            visible_close: "&lt;/tau_web_content&gt;",
            carrier: PayloadEnvelopeCarrier::TypedToolResult,
        }
    );
    assert_eq!(
        TAU_BACKGROUND_RESULT_PAYLOAD_ENVELOPE,
        RegisteredPayloadEnvelope {
            name: "tau_background_result",
            opening: PayloadEnvelopeOpening::Attributed("<tau_background_result "),
            ordered_attributes: &[
                "call_id",
                "tool",
                "tool_outcome",
                "delivery",
                "rendered_bytes",
                "retrieval",
                "process_outcome",
                "process_source",
                "process_success",
                "termination_reason",
                "exit_code",
                "signal",
                "timed_out",
                "message_bytes",
                "message_truncated",
            ],
            exact_close: "</tau_background_result>",
            visible_close: "&lt;/tau_background_result&gt;",
            carrier: PayloadEnvelopeCarrier::GenericUserText,
        }
    );
    for (index, family) in families.iter().enumerate() {
        assert!(!family.name.is_empty());
        assert!(!family.exact_close.is_empty());
        assert!(!family.visible_close.is_empty());
        assert_eq!(
            family
                .ordered_attributes
                .iter()
                .collect::<BTreeSet<_>>()
                .len(),
            family.ordered_attributes.len(),
            "duplicate attribute in {}",
            family.name
        );
        for other in &families[index + 1..] {
            assert_ne!(family.name, other.name);
            assert_ne!(family.exact_close, other.exact_close);
        }

        let other_close = families[(index + 1) % families.len()].exact_close;
        let body = format!("before {} middle {other_close} after", family.exact_close);
        let escaped = family.escape_body(&body);
        if family.exact_close != family.visible_close {
            assert!(!escaped.contains(family.exact_close));
        }
        assert!(escaped.contains(other_close));
    }
}

/// Whole-envelope recognition must accept both registered opening shapes and
/// reject prefixes, suffixes, and duplicate trusted closes.
#[test]
fn registered_families_match_only_one_complete_outer_envelope() {
    assert!(USER_PAYLOAD_ENVELOPE.matches_whole("<user>body</user>"));
    assert!(MESSAGE_PAYLOAD_ENVELOPE.matches_whole("<message event=\"delivered\">body</message>"));
    assert!(!USER_PAYLOAD_ENVELOPE.matches_whole("x<user>body</user>"));
    assert!(!MESSAGE_PAYLOAD_ENVELOPE.matches_whole("<message>body</message>"));
    assert!(!USER_PAYLOAD_ENVELOPE.matches_whole("<user>a</user>b</user>"));
}

/// Exact-close framing replaces every own close and preserves all other
/// text.
#[test]
fn replaces_only_repeated_exact_close() {
    let body = "Don't &apos; <user> x</user></user > </USER> 雪\n</user>";
    assert_eq!(
        escape_exact_sentinel_close(body, "</user>", "&lt;/user&gt;"),
        "Don't &apos; <user> x&lt;/user&gt;</user > </USER> 雪\n&lt;/user&gt;"
    );
}

/// Collision-free bodies remain borrowed so ordinary framing avoids
/// allocation.
#[test]
fn borrows_body_without_exact_close() {
    assert!(matches!(
        escape_exact_sentinel_close("plain & <text>", "</user>", "&lt;/user&gt;"),
        std::borrow::Cow::Borrowed(_)
    ));
}

/// Dynamic envelope attributes escape every XML-significant character.
#[test]
fn payload_envelope_attributes_are_fully_escaped() {
    assert_eq!(
        escape_payload_envelope_attribute("<&>\"'"),
        "&lt;&amp;&gt;&quot;&apos;"
    );
}

/// The shared registry owns the exact background-preview opening, attribute
/// order, escaping, collision handling, and closing bytes.
#[test]
fn background_result_registry_renders_exact_attributed_envelope() {
    let rendered = TAU_BACKGROUND_RESULT_PAYLOAD_ENVELOPE
        .render_attributed(
            &[
                ("call_id", "call<&\"".to_owned()),
                ("tool", "read".to_owned()),
                ("tool_outcome", "result".to_owned()),
                ("delivery", "full".to_owned()),
                ("rendered_bytes", "31".to_owned()),
                ("retrieval", "wait".to_owned()),
            ],
            "before </tau_background_result> after",
        )
        .expect("registered attributes");
    assert_eq!(
        rendered,
        "<tau_background_result call_id=\"call&lt;&amp;&quot;\" tool=\"read\" \
         tool_outcome=\"result\" delivery=\"full\" rendered_bytes=\"31\" \
         retrieval=\"wait\">before &lt;/tau_background_result&gt; \
         after</tau_background_result>"
    );
    assert!(TAU_BACKGROUND_RESULT_PAYLOAD_ENVELOPE.matches_whole(&rendered));
}

/// Attributed rendering rejects unknown, duplicate, and out-of-order fields
/// rather than silently drifting from the registry contract.
#[test]
fn attributed_renderer_rejects_attribute_contract_drift() {
    for attributes in [
        vec![("unknown", "x".to_owned())],
        vec![("call_id", "c".to_owned()), ("call_id", "again".to_owned())],
        vec![("tool", "read".to_owned()), ("call_id", "c".to_owned())],
    ] {
        assert!(matches!(
            TAU_BACKGROUND_RESULT_PAYLOAD_ENVELOPE.render_attributed(&attributes, ""),
            Err(PayloadEnvelopeRenderError::UnknownOrMisorderedAttribute(_))
        ));
    }
}

/// Production XML-like open/close pairs must either name a registered top-level
/// family or remain in the explicit legacy, nested, or non-provenance
/// inventory.
#[test]
fn production_xml_like_wrapper_candidates_cannot_bypass_the_registry_silently() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root");
    let mut files = Vec::new();
    collect_rust_files(&workspace.join("crates"), &mut files);
    let registered = registered_payload_envelopes()
        .iter()
        .map(|family| family.name)
        .collect::<BTreeSet<_>>();
    let legacy_nested_or_non_provenance = [
        "activity_summary",
        "blocker_answer",
        "message",
        "prompt",
        "response",
        "skill",
        "tau_peer_message",
        "tau_rostra_content",
        "think",
        "user_shell",
    ]
    .into_iter()
    .collect::<BTreeSet<_>>();

    let mut unknown = Vec::new();
    let mut open_locations = BTreeMap::<String, Vec<PathBuf>>::new();
    let mut close_locations = BTreeMap::<String, Vec<PathBuf>>::new();
    for path in files {
        let source = std::fs::read_to_string(&path).expect("read Rust source");
        let (opens, closes) = xml_like_tag_candidates(&source);
        for name in opens {
            open_locations.entry(name).or_default().push(path.clone());
        }
        for name in closes {
            close_locations.entry(name).or_default().push(path.clone());
        }
    }
    for name in open_locations.keys() {
        if close_locations.contains_key(name)
            && !registered.contains(name.as_str())
            && !legacy_nested_or_non_provenance.contains(name.as_str())
        {
            unknown.push(format!(
                "{name}: opens in {:?}; closes in {:?}",
                open_locations[name], close_locations[name]
            ));
        }
    }
    assert!(
        unknown.is_empty(),
        "unregistered production XML-like close candidates:\n{}",
        unknown.join("\n")
    );
}

/// Extract lower-case XML-like opening and closing tag names from source text.
fn xml_like_tag_candidates(source: &str) -> (BTreeSet<String>, BTreeSet<String>) {
    let mut opens = BTreeSet::new();
    let mut closes = BTreeSet::new();
    for (index, _) in source.match_indices('<') {
        let rest = &source[index + 1..];
        let (closing, rest) = rest
            .strip_prefix('/')
            .map_or((false, rest), |rest| (true, rest));
        let end = rest
            .find(|character: char| {
                !character.is_ascii_lowercase()
                    && !character.is_ascii_digit()
                    && character != '_'
                    && character != '-'
            })
            .unwrap_or(rest.len());
        if end == 0 || !rest.as_bytes()[0].is_ascii_lowercase() {
            continue;
        }
        let delimiter = rest.as_bytes().get(end).copied();
        if !matches!(delimiter, Some(b' ' | b'>')) {
            continue;
        }
        let target = if closing { &mut closes } else { &mut opens };
        target.insert(rest[..end].to_owned());
    }
    (opens, closes)
}

/// Recursively collect production Rust sources while skipping build output.
fn collect_rust_files(directory: &Path, files: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(directory).expect("read source directory") {
        let path = entry.expect("source entry").path();
        if path.is_dir() {
            if path.file_name().is_none_or(|name| name != "target") {
                collect_rust_files(&path, files);
            }
        } else if path.extension().is_some_and(|extension| extension == "rs")
            && !path
                .components()
                .any(|component| component.as_os_str() == "tests")
            && path.file_name().is_none_or(|name| name != "tests.rs")
        {
            files.push(path);
        }
    }
}
