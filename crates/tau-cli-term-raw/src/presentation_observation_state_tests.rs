use std::collections::VecDeque;
use std::time::Instant;

use super::*;

fn key(value: u8) -> PresentationObservationKey {
    PresentationObservationKey::new(value).expect("test key must fit invalidation mask")
}

fn fact(
    label: &'static str,
    key: PresentationObservationKey,
    invalidates: PresentationInvalidation,
) -> OpaquePresentationFact {
    OpaquePresentationFact::new(label, key, invalidates)
}

/// Validated keys retain the fixed invalidation-mask boundary rather than
/// accepting an unrepresentable raw value.
#[test]
fn observation_keys_validate_mask_bounds() {
    assert_eq!(PresentationObservationKey::new(63), Some(key(63)));
    assert_eq!(PresentationObservationKey::new(64), None);
}

/// Overflow for duplicate keys stays in one typed aggregate and counts each
/// omitted observation without affecting the exact pending bound.
#[test]
fn duplicate_key_overflow_uses_one_aggregate() {
    let mut state = PresentationObservationState::new();
    for delivery_id in 0..MAX_PENDING_PRESENTATION_OBSERVATIONS as u64 + 3 {
        state.register(
            RendererDeliveryId::new(delivery_id),
            fact("updated", key(3), PresentationInvalidation::none()),
            Instant::now(),
        );
    }

    assert_eq!(state.pending.len(), MAX_PENDING_PRESENTATION_OBSERVATIONS);
    assert_eq!(state.omitted_by_key.len(), 1);
    assert!(state.omitted_by_key[0].has_key(key(3)));
    assert_eq!(state.omitted_by_key[0].count, 3);
    assert_eq!(state.capture().omitted, 3);
}

/// Omitted-work accounting saturates both its per-key aggregate and total,
/// preserving the bounded diagnostic contract at `u64::MAX`.
#[test]
fn omitted_counts_saturate() {
    let mut state = PresentationObservationState::new();
    state.pending = VecDeque::from_iter((0..MAX_PENDING_PRESENTATION_OBSERVATIONS as u64).map(
        |delivery_id| PresentationObservation {
            delivery_id: RendererDeliveryId::new(delivery_id),
            fact: "updated",
            key: key(3),
            generation: PresentationMutationGeneration::default(),
            observed_at: Instant::now(),
        },
    ));
    state.omitted_by_key = vec![OmittedPresentationObservations {
        key: key(3),
        count: u64::MAX,
    }];
    state.omitted_total = u64::MAX;

    state.register(
        RendererDeliveryId::new(64),
        fact("updated", key(3), PresentationInvalidation::none()),
        Instant::now(),
    );

    assert_eq!(state.omitted_by_key[0].count, u64::MAX);
    assert_eq!(state.omitted_total, u64::MAX);
}

/// Invalidation removes only superseded pending and count-only observations,
/// then capture clears the completed redraw lifecycle.
#[test]
fn invalidation_removes_typed_membership_before_capture() {
    let mut state = PresentationObservationState::new();
    state.pending.push_back(PresentationObservation {
        delivery_id: RendererDeliveryId::new(1),
        fact: "queued",
        key: key(0),
        generation: PresentationMutationGeneration::default(),
        observed_at: Instant::now(),
    });
    state.omitted_by_key = vec![
        OmittedPresentationObservations {
            key: key(0),
            count: 2,
        },
        OmittedPresentationObservations {
            key: key(1),
            count: 3,
        },
    ];
    state.omitted_total = 5;

    state.register(
        RendererDeliveryId::new(2),
        fact(
            "submitted",
            key(2),
            PresentationInvalidation::none().with(key(0)),
        ),
        Instant::now(),
    );

    assert_eq!(
        state
            .pending
            .iter()
            .map(|fact| fact.key)
            .collect::<Vec<_>>(),
        [key(2)]
    );
    assert_eq!(state.omitted_by_key.len(), 1);
    assert!(state.omitted_by_key[0].has_key(key(1)));
    assert_eq!(state.omitted_total, 3);

    let captured = state.capture();
    assert_eq!(captured.omitted, 3);
    assert!(state.omitted_by_key.is_empty());
    assert!(state.is_empty());
}
