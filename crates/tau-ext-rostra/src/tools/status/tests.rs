//! Focused status formatting contracts.

use std::collections::BTreeSet;

use rostra_core::event::content_kind::{EventContentKind as _, Follow, PersonasTagsSelector};
use rostra_core::event::{Event as RostraEvent, EventKind, VerifiedEvent};
use rostra_core::id::RostraIdSecretKey;

use super::render;

/// Ensures status formats a large signed hostile fan-out without making the
/// formatting oracle pay for one database transaction per identity.
#[test]
fn formats_large_signed_fanout_without_database_visits() {
    const FANOUT: usize = 64;

    let self_id = RostraIdSecretKey::generate().id();
    let hostile_secret = RostraIdSecretKey::generate();
    let mut parent = None;
    let mut followees = BTreeSet::new();
    for _ in 0..FANOUT {
        let followee = RostraIdSecretKey::generate().id();
        let content = Follow {
            followee,
            persona: None,
            selector: None,
            persona_tags_selector: Some(PersonasTagsSelector::default()),
        }
        .serialize_cbor()
        .expect("follow content");
        let event = RostraEvent::builder_raw_content()
            .author(hostile_secret.id())
            .kind(EventKind::FOLLOW)
            .content(&content)
            .maybe_parent_prev(parent)
            .build();
        let signed = event.signed_by(hostile_secret);
        let verified = VerifiedEvent::verify_signed(hostile_secret.id(), signed)
            .expect("signed hostile follow");
        parent = Some(verified.event_id.into());
        assert!(followees.insert(followee), "generated followee is unique");
    }

    let output = render(self_id, 1, followees.len()).expect("bounded status");
    assert!(output.contains("known_direct_followees: 1\n"));
    assert!(output.contains(&format!("known_two_hop_identities: {FANOUT}\n")));
}
