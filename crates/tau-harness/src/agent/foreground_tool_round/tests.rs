use std::collections::HashSet;

use tau_proto::ToolCallId;

use super::ForegroundToolRound;

/// A large round must preserve sampled provider-order projections while
/// settlement performs one exact membership operation per observation.
#[test]
fn large_round_matches_reference_with_linear_completion_membership_work() {
    const CALLS: usize = 4_096;
    let ids: Vec<ToolCallId> = (0..CALLS)
        .map(|index| format!("call-{index:04}").into())
        .collect();
    let orders = [
        (0..CALLS).collect::<Vec<_>>(),
        (0..CALLS).rev().collect(),
        (0..CALLS)
            .map(|index| (index.wrapping_mul(2_053)) % CALLS)
            .collect(),
    ];

    for order in orders {
        let mut round = ForegroundToolRound::new(ids.clone());
        let mut reference: HashSet<_> = ids.iter().cloned().collect();
        let mut closures = 0;
        for (settlement_index, index) in order.into_iter().enumerate() {
            let id = &ids[index];
            reference.remove(id);
            closures += usize::from(round.complete(id.as_str()));
            assert!(!round.complete("unknown-call"));
            assert!(!round.complete(id.as_str()));
            if settlement_index % 257 == 0 || reference.is_empty() {
                let expected = ids
                    .iter()
                    .filter(|candidate| reference.contains(*candidate))
                    .cloned()
                    .collect::<Vec<_>>();
                assert_eq!(round.ordered_remaining(), expected);
            }
        }
        assert!(round.is_empty());
        assert_eq!(closures, 1);
        assert!(!round.complete(ids[0].as_str()));
        assert_eq!(round.completion_work(), CALLS * 3 + 1);
    }
}
