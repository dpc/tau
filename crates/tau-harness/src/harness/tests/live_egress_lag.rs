use super::*;

/// Read lag diagnostics through the ordinary published notice path.
fn lag_notices(harness: &Harness) -> Vec<tau_proto::HarnessNotice> {
    event_log_events(harness)
        .into_iter()
        .filter_map(|event| match event {
            Event::HarnessNotice(notice) if notice.message.starts_with("egress lag:") => {
                Some(notice)
            }
            _ => None,
        })
        .collect()
}

/// Warning recovery describes the global threshold, not zero lag or successful
/// delivery: either advancing a cursor or retiring it can clear the condition.
/// Healthy cycles stay silent, and recovery does not reset the warning
/// throttle.
#[test]
fn lag_warning_clears_once_after_progress_or_retirement_without_resetting_throttle() {
    for retire in [false, true] {
        let td = TempDir::new().expect("tempdir");
        let mut harness = echo_harness(td.path().join("state")).expect("start");
        let log = Arc::clone(&harness.runtime_io.event_log);
        let consumer = log.register_consumer();
        let target = tau_core::SharedDeliveryTarget::new(log.group(), consumer);
        let frame = RoutedFrame::new(None, HarnessOutputMessage::deliver(draft_event("lag")));
        let start = Instant::now();
        harness.process_runtime_deadlines_at(start);
        assert!(lag_notices(&harness).is_empty());

        for _ in 0..super::super::LIVE_EGRESS_LAG_WARNING_POSITIONS {
            let _ = log.append_egress(frame.clone(), &[target]);
        }
        harness.process_runtime_deadlines_at(start);
        let notices = lag_notices(&harness);
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].level, tau_proto::NoticeLevel::Warning);
        harness.process_runtime_deadlines_at(start + Duration::from_secs(1));
        assert_eq!(lag_notices(&harness).len(), 1);

        if retire {
            log.retire_consumer(consumer);
            // The fixture's provider and shell writers also own cursors. Let
            // them inspect the warning's egress position before asserting an
            // exact global zero, rather than racing their asynchronous
            // progress.
            assert!(
                log.wait_for_consumers_at_tail(Duration::from_secs(5)),
                "fixture writer did not reach the egress tail: {:?}",
                log.max_consumer_lag()
            );
        } else {
            let pending = log.next_egress(consumer).expect("pending delivery");
            log.acknowledge_egress(consumer, &pending);
        }
        harness.process_runtime_deadlines_at(start + Duration::from_secs(2));
        let notices = lag_notices(&harness);
        assert_eq!(notices.len(), 2);
        assert_eq!(notices[1].level, tau_proto::NoticeLevel::Info);
        let remaining = if retire { 0 } else { 9_999 };
        assert!(notices[1].message.contains("status=cleared"));
        assert!(
            notices[1]
                .message
                .contains(&format!("max_cursor_lag={remaining} "))
        );
        harness.process_runtime_deadlines_at(start + Duration::from_secs(3));
        assert_eq!(lag_notices(&harness).len(), 2);

        let late_ui = connect_test_client(&mut harness, "late-ui", tau_proto::ClientKind::Ui);
        harness.replay_harness_notice(
            &crate::test_connection_id("late-ui"),
            &[EventSelector::Exact(tau_proto::EventName::HARNESS_NOTICE)],
        );
        let replayed = late_ui
            .lock()
            .expect("late UI frames")
            .iter()
            .filter_map(|frame| match &frame.frame {
                HarnessOutputMessage::Deliver(delivery) => match delivery.event() {
                    Event::HarnessNotice(notice) if notice.message.starts_with("egress lag:") => {
                        assert!(delivery.is_replay());
                        Some(notice.clone())
                    }
                    _ => None,
                },
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            replayed, notices,
            "late UI sees warning and recovery in order"
        );

        let next_consumer = if retire {
            log.register_consumer()
        } else {
            consumer
        };
        let next_target = tau_core::SharedDeliveryTarget::new(log.group(), next_consumer);
        for _ in 0..super::super::LIVE_EGRESS_LAG_WARNING_POSITIONS {
            let _ = log.append_egress(frame.clone(), &[next_target]);
        }
        harness.process_runtime_deadlines_at(start + Duration::from_secs(4));
        assert_eq!(lag_notices(&harness).len(), 2, "warning remains throttled");
        harness.process_runtime_deadlines_at(start + Duration::from_secs(60));
        let notices = lag_notices(&harness);
        assert_eq!(notices.len(), 3);
        assert_eq!(notices[2].level, tau_proto::NoticeLevel::Warning);
    }
}
