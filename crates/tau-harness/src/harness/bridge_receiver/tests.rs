use super::*;

/// Approximate creation order is stable for ties and missing legacy dates.
#[test]
fn bridge_receiver_order_is_timestamp_then_id_with_unknown_last() {
    let id = |s| AgentId::parse(s).expect("id");
    let time = |n| Some(tau_proto::UnixMicros::new(n));
    let mut candidates = vec![
        receiver_order(None, id("a")),
        receiver_order(time(2), id("a")),
        receiver_order(time(1), id("z")),
        receiver_order(time(1), id("b")),
    ];
    candidates.sort();
    assert_eq!(
        candidates
            .into_iter()
            .map(|(_, _, id)| id)
            .collect::<Vec<_>>(),
        vec![id("b"), id("z"), id("a"), id("a")]
    );
}
