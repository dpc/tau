//! Thread-safe runtime event sequencer.
//!
//! The harness assigns one globally monotonic [`EventLogSeq`] to every
//! committed runtime event. The same process-local owner retains one canonical
//! representation of each admitted live output until every frozen consumer
//! generation advances or retires. Neither sequence leaves the process.
//! Subscribe-time historical catch-up still comes from semantic state:
//! durable session/agent stores and current harness snapshots.

#[cfg(test)]
use std::collections::BTreeMap;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

#[cfg(test)]
use tau_proto::UnixMicros;
#[cfg(test)]
use tau_proto::{ConnectionId, Event};

/// Monotonic sequence assigned by the harness runtime event sequencer.
///
/// This sequence is relative to the running harness as a whole and is
/// harness-internal: it is not part of the wire protocol and is not
/// comparable to persisted agent/session event sequences. Production code
/// uses it only to order test observations; nothing on the wire carries it.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub(crate) struct EventLogSeq(u64);

impl EventLogSeq {
    /// Creates a sequence from a raw counter value.
    #[must_use]
    pub(crate) fn new(v: u64) -> Self {
        Self(v)
    }

    /// Returns the raw counter value.
    #[must_use]
    #[cfg(test)]
    pub(crate) fn get(self) -> u64 {
        self.0
    }

    /// Returns the next sequence value.
    #[must_use]
    pub(crate) fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

/// One committed event captured by the test-only observer.
#[cfg(test)]
#[derive(Clone, Debug)]
pub(crate) struct LogEntry {
    pub seq: EventLogSeq,
    pub recorded_at: UnixMicros,
    pub source: Option<ConnectionId>,
    pub event: Event,
}

/// Mutable state protected by one event-log mutex.
struct EventLogInner {
    /// Next committed-event observation sequence.
    next_seq: EventLogSeq,
    /// Test-only committed-event observations.
    #[cfg(test)]
    entries: BTreeMap<EventLogSeq, LogEntry>,
    /// Next contiguous position in the logical live egress stream.
    next_egress_seq: EgressPosition,
    /// Cursor-continuity positions with payloads only while targets require
    /// them.
    retained: VecDeque<LivePosition>,
    /// Active consumer generations and their independent cursors.
    consumers: HashMap<tau_core::SharedConsumerId, ConsumerState>,
    /// Next owner-allocated consumer generation value.
    next_consumer: u64,
    /// Guarded content-free measurement state, absent by default.
    delivery_memory: Option<Box<DeliveryMemoryState>>,
    /// Test-only per-log guard override for parallel measurement assertions.
    ///
    /// Thread-local subscribers still share tracing's callsite-interest cache,
    /// so unrelated tests may change the cached guard result. This flag
    /// bypasses only that guard; the local subscriber still owns trace
    /// publication.
    #[cfg(test)]
    force_delivery_memory: bool,
    /// Test-only exact work counters for complexity regression oracles.
    #[cfg(test)]
    work: EventLogWork,
}

/// Exact test-only operation counts for event-log hot paths.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct EventLogWork {
    /// Calls that recompute and prune the retained prefix.
    prune_calls: u64,
    /// Consumer cursors inspected while finding retained-prefix minima.
    prune_consumer_visits: u64,
    /// Calls that inspect the optional delivery-memory observation seam.
    observe_calls: u64,
    /// Logical positions inspected while searching for a consumer's next
    /// target.
    scan_position_visits: u64,
    /// Waits entered while replay catch-up held a consumer cursor.
    catch_up_waits: u64,
    /// Waits entered after a consumer reached the current live tail.
    tail_waits: u64,
}

/// Enabled-only estimates and high-water aggregates for the live suffix.
#[derive(Default)]
struct DeliveryMemoryState {
    /// Immutable per-frame estimates cached by live position.
    estimates: HashMap<EgressPosition, tau_delivery_memory::DecodedMemoryEstimate>,
    /// Largest encoded retained-byte estimate.
    high_encoded_bytes: u64,
    /// Largest decoded logical-byte estimate.
    high_logical_bytes: u64,
    /// Largest decoded requested-capacity estimate.
    high_requested_capacity: u64,
    /// Largest retained shared-allocation count.
    high_shared_allocations: u64,
    /// Largest aggregate strong-reference fanout.
    high_shared_fanout: u64,
    /// Largest aggregate frozen attachment fanout.
    high_pending_target_fanout: u64,
}

/// Contiguous process-local position in the logical egress stream.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct EgressPosition(
    /// Contiguous owner-local position value.
    u64,
);

impl EgressPosition {
    /// Returns the following logical stream position.
    fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }

    /// Returns the number of positions from `earlier` through this position.
    fn distance_from(self, earlier: Self) -> u64 {
        self.0.saturating_sub(earlier.0)
    }
}

/// One lightweight logical position and any still-required shared payload.
struct LivePosition {
    /// Contiguous position in the logical live egress stream.
    seq: EgressPosition,
    /// Canonical frame retained once while at least one frozen target requires
    /// it.
    payload: Option<Arc<tau_core::RoutedFrame>>,
    /// Frozen generations that have not yet acknowledged this position.
    pending_targets: HashSet<tau_core::SharedConsumerId>,
}

/// Mutable cursor metadata for one connected consumer generation.
#[derive(Clone, Copy, Debug)]
struct ConsumerState {
    /// Next stream position the follower must inspect.
    cursor: EgressPosition,
    /// Whether replay is still establishing the live-tail barrier.
    catch_up_paused: bool,
    /// Tail after which the writer retires instead of waiting for more work.
    close_after: Option<EgressPosition>,
}

/// One frame selected by a consumer without advancing its delivery cursor.
pub(crate) struct PendingEgress {
    /// Stream position acknowledged only after write, flush, and metering.
    seq: EgressPosition,
    /// Shared canonical routed frame.
    frame: Arc<tau_core::RoutedFrame>,
}

impl PendingEgress {
    /// Returns the protocol frame selected for this consumer.
    pub(crate) fn frame(&self) -> &tau_proto::HarnessOutputMessage {
        &self.frame.frame
    }
}

/// Thread-safe runtime event sequencer.
///
/// Production builds retain only the cursor-pinned live suffix. Tests also
/// keep a small committed-event observer log for behavioral assertions.
pub(crate) struct EventLog {
    /// Process-local identity shared by every sink attached to this log.
    group: tau_core::SharedDeliveryGroup,
    /// Mutable stream state.
    inner: Mutex<EventLogInner>,
    /// Wakeup for append, cursor advancement, retirement, and catch-up release.
    changed: Condvar,
    /// Test-only count of condition-variable broadcasts.
    #[cfg(test)]
    notifications: AtomicU64,
    /// Test-only wakeup when a follower enters a blocking wait.
    #[cfg(test)]
    waiter_entered: Condvar,
}

/// Allocates process-local stream identities without exposing pointer values.
static NEXT_DELIVERY_GROUP: AtomicU64 = AtomicU64::new(1);

impl EventLog {
    /// Creates an empty sequencer.
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            group: tau_core::SharedDeliveryGroup::new(
                NEXT_DELIVERY_GROUP.fetch_add(1, Ordering::Relaxed),
            ),
            inner: Mutex::new(EventLogInner {
                next_seq: EventLogSeq::new(0),
                #[cfg(test)]
                entries: BTreeMap::new(),
                next_egress_seq: EgressPosition::default(),
                retained: VecDeque::new(),
                consumers: HashMap::new(),
                next_consumer: 1,
                delivery_memory: None,
                #[cfg(test)]
                force_delivery_memory: false,
                #[cfg(test)]
                work: EventLogWork::default(),
            }),
            changed: Condvar::new(),
            #[cfg(test)]
            notifications: AtomicU64::new(0),
            #[cfg(test)]
            waiter_entered: Condvar::new(),
        })
    }

    /// Returns this log's process-local shared-stream identity.
    pub(crate) fn group(&self) -> tau_core::SharedDeliveryGroup {
        self.group
    }

    /// Registers a fresh consumer generation at the current live tail.
    pub(crate) fn register_consumer(&self) -> tau_core::SharedConsumerId {
        let mut inner = self.inner.lock().expect("event log mutex poisoned");
        let consumer = tau_core::SharedConsumerId::new(inner.next_consumer);
        inner.next_consumer = inner.next_consumer.saturating_add(1);
        let cursor = inner.next_egress_seq;
        inner.consumers.insert(
            consumer,
            ConsumerState {
                cursor,
                catch_up_paused: false,
                close_after: None,
            },
        );
        Self::observe_delivery_memory_locked(&mut inner);
        consumer
    }

    /// Admits one canonical frame with an immutable target-generation set.
    pub(crate) fn append_egress(
        &self,
        frame: tau_core::RoutedFrame,
        targets: &[tau_core::SharedDeliveryTarget],
    ) -> Vec<tau_core::SharedDeliveryTarget> {
        let mut inner = self.inner.lock().expect("event log mutex poisoned");
        let seq = inner.next_egress_seq;
        inner.next_egress_seq = inner.next_egress_seq.next();
        let admitted = targets
            .iter()
            .copied()
            .filter(|target| {
                target.group() == self.group && inner.consumers.contains_key(&target.consumer())
            })
            .collect::<Vec<_>>();
        let pending_targets = admitted
            .iter()
            .map(|target| target.consumer())
            .collect::<HashSet<_>>();
        let payload = (!pending_targets.is_empty()).then(|| Arc::new(frame));
        inner.retained.push_back(LivePosition {
            seq,
            payload,
            pending_targets,
        });
        Self::prune_locked(&mut inner);
        Self::observe_delivery_memory_locked(&mut inner);
        self.notify_changed(inner);
        admitted
    }

    /// Follows this consumer to its next target, close boundary, or live tail.
    ///
    /// The scan stops before the first targeted position so successful delivery
    /// acknowledgement remains the only operation that advances past and
    /// releases that target. Reaching a captured close boundary retires the
    /// consumer immediately; reaching the current tail waits for later work.
    /// Each non-empty skipped batch updates the cursor once, then prunes and
    /// observes memory once. Every such transition releases the mutex before
    /// broadcasting its one state-change notification.
    pub(crate) fn next_egress(
        &self,
        consumer: tau_core::SharedConsumerId,
    ) -> Option<PendingEgress> {
        let mut inner = self.inner.lock().expect("event log mutex poisoned");
        loop {
            let state = *inner.consumers.get(&consumer)?;
            if state.close_after == Some(state.cursor) {
                Self::retire_consumer_locked(&mut inner, consumer);
                self.notify_changed(inner);
                return None;
            }
            if state.catch_up_paused {
                #[cfg(test)]
                {
                    inner.work.catch_up_waits = inner.work.catch_up_waits.saturating_add(1);
                    self.waiter_entered.notify_all();
                }
                inner = self
                    .changed
                    .wait(inner)
                    .expect("event log mutex poisoned while catch-up paused");
                continue;
            }
            if state.cursor == inner.next_egress_seq {
                #[cfg(test)]
                {
                    inner.work.tail_waits = inner.work.tail_waits.saturating_add(1);
                    self.waiter_entered.notify_all();
                }
                inner = self
                    .changed
                    .wait(inner)
                    .expect("event log mutex poisoned while waiting");
                continue;
            }
            let first = inner
                .retained
                .front()
                .map_or(inner.next_egress_seq, |entry| entry.seq);
            if state.cursor < first {
                // Prefix pruning cannot pass an active cursor.
                unreachable!("active live cursor fell behind retained prefix");
            }
            let start_index = usize::try_from(state.cursor.distance_from(first))
                .expect("egress index fits usize");
            let boundary = state
                .close_after
                .map_or(inner.next_egress_seq, |close_after| {
                    close_after.min(inner.next_egress_seq)
                });
            let scan_len = usize::try_from(boundary.distance_from(state.cursor))
                .expect("scan length fits usize");
            let next_target = inner
                .retained
                .iter()
                .skip(start_index)
                .take(scan_len)
                .enumerate()
                .find(|(_, entry)| entry.pending_targets.contains(&consumer))
                .map(|(offset, entry)| {
                    (
                        offset,
                        entry.seq,
                        Arc::clone(
                            entry
                                .payload
                                .as_ref()
                                .expect("pending target must retain its shared payload"),
                        ),
                    )
                });
            #[cfg(test)]
            {
                let position_visits = next_target
                    .as_ref()
                    .map_or(scan_len, |(offset, _, _)| offset.saturating_add(1));
                inner.work.scan_position_visits = inner
                    .work
                    .scan_position_visits
                    .saturating_add(u64::try_from(position_visits).expect("scan length fits u64"));
            }
            let next_cursor = next_target
                .as_ref()
                .map_or(boundary, |(_, position, _)| *position);
            if next_cursor == state.cursor {
                let (_, seq, frame) = next_target.expect("current position is a target");
                Self::observe_delivery_memory_locked(&mut inner);
                return Some(PendingEgress { seq, frame });
            }
            inner
                .consumers
                .get_mut(&consumer)
                .expect("consumer remains registered")
                .cursor = next_cursor;
            if state.close_after == Some(next_cursor) {
                Self::retire_consumer_locked(&mut inner, consumer);
                self.notify_changed(inner);
                return None;
            }
            Self::prune_locked(&mut inner);
            Self::observe_delivery_memory_locked(&mut inner);
            self.notify_changed(inner);
            if let Some((_, seq, frame)) = next_target {
                return Some(PendingEgress { seq, frame });
            }
            inner = self.inner.lock().expect("event log mutex poisoned");
        }
    }

    /// Advances after successful encode, write, flush, and protocol metering.
    pub(crate) fn acknowledge_egress(
        &self,
        consumer: tau_core::SharedConsumerId,
        pending: &PendingEgress,
    ) {
        let mut inner = self.inner.lock().expect("event log mutex poisoned");
        if inner
            .consumers
            .get(&consumer)
            .is_some_and(|state| state.cursor == pending.seq)
        {
            let first = inner
                .retained
                .front()
                .map_or(inner.next_egress_seq, |entry| entry.seq);
            let index =
                usize::try_from(pending.seq.distance_from(first)).expect("egress index fits usize");
            let position = inner
                .retained
                .get_mut(index)
                .expect("acknowledged position remains retained");
            position.pending_targets.remove(&consumer);
            if position.pending_targets.is_empty() {
                position.payload = None;
            }
            inner
                .consumers
                .get_mut(&consumer)
                .expect("consumer remains registered")
                .cursor = pending.seq.next();
            Self::prune_locked(&mut inner);
        }
        Self::observe_delivery_memory_locked(&mut inner);
        self.notify_changed(inner);
    }

    /// Retires a generation unless terminal close ownership has been
    /// transferred.
    ///
    /// Once close-after-current is active, dropping the bus sink must not race
    /// the independent lifecycle owner and discard its terminal frame.
    pub(crate) fn retire_consumer(&self, consumer: tau_core::SharedConsumerId) {
        let mut inner = self.inner.lock().expect("event log mutex poisoned");
        if inner
            .consumers
            .get(&consumer)
            .is_some_and(|state| state.close_after.is_none())
        {
            Self::retire_consumer_locked(&mut inner, consumer);
        }
        self.notify_changed(inner);
    }

    /// Retires a generation after its writer finishes or fails transport I/O.
    pub(crate) fn retire_consumer_after_io(&self, consumer: tau_core::SharedConsumerId) {
        let mut inner = self.inner.lock().expect("event log mutex poisoned");
        Self::retire_consumer_locked(&mut inner, consumer);
        self.notify_changed(inner);
    }

    /// Captures the current tail and asks the consumer to retire after reaching
    /// it.
    ///
    /// Closing releases a replay pause because terminal connection teardown
    /// must not remain parked behind semantic catch-up.
    pub(crate) fn close_consumer_after_current(&self, consumer: tau_core::SharedConsumerId) {
        let mut inner = self.inner.lock().expect("event log mutex poisoned");
        let close_after = inner.next_egress_seq;
        if let Some(state) = inner.consumers.get_mut(&consumer) {
            state.close_after = Some(close_after);
            state.catch_up_paused = false;
        }
        self.notify_changed(inner);
    }

    /// Waits at most `timeout` for a consumer generation to retire.
    ///
    /// Returns `true` when the generation no longer owns a cursor.
    pub(crate) fn wait_for_consumer_retirement(
        &self,
        consumer: tau_core::SharedConsumerId,
        timeout: Duration,
    ) -> bool {
        let inner = self.inner.lock().expect("event log mutex poisoned");
        let (inner, _) = self
            .changed
            .wait_timeout_while(inner, timeout, |inner| {
                inner.consumers.contains_key(&consumer)
            })
            .expect("event log mutex poisoned while waiting for retirement");
        !inner.consumers.contains_key(&consumer)
    }

    /// Pauses or resumes a follower around semantic replay.
    pub(crate) fn set_catch_up_paused(&self, consumer: tau_core::SharedConsumerId, paused: bool) {
        let mut inner = self.inner.lock().expect("event log mutex poisoned");
        if let Some(state) = inner.consumers.get_mut(&consumer) {
            state.catch_up_paused = paused;
        }
        self.notify_changed(inner);
    }

    /// Captures the current tail and waits until the consumer reaches it or
    /// retires.
    pub(crate) fn flush_consumer(&self, consumer: tau_core::SharedConsumerId) {
        let mut inner = self.inner.lock().expect("event log mutex poisoned");
        let barrier = inner.next_egress_seq;
        while inner
            .consumers
            .get(&consumer)
            .is_some_and(|state| state.cursor < barrier)
        {
            inner = self
                .changed
                .wait(inner)
                .expect("event log mutex poisoned while flushing");
        }
    }

    /// Returns the largest active consumer lag in logical stream positions.
    pub(crate) fn max_consumer_lag(&self) -> u64 {
        let inner = self.inner.lock().expect("event log mutex poisoned");
        inner
            .consumers
            .values()
            .map(|state| inner.next_egress_seq.distance_from(state.cursor))
            .max()
            .unwrap_or_default()
    }

    /// Returns active consumer count for lifecycle regression tests.
    #[cfg(test)]
    pub(crate) fn consumer_count(&self) -> usize {
        self.inner
            .lock()
            .expect("event log mutex poisoned")
            .consumers
            .len()
    }

    /// Prunes every prefix position inspected by all active generations.
    fn prune_locked(inner: &mut EventLogInner) {
        #[cfg(test)]
        {
            inner.work.prune_calls = inner.work.prune_calls.saturating_add(1);
            inner.work.prune_consumer_visits = inner
                .work
                .prune_consumer_visits
                .saturating_add(u64::try_from(inner.consumers.len()).expect("count fits u64"));
        }
        let min_cursor = inner
            .consumers
            .values()
            .map(|state| state.cursor)
            .min()
            .unwrap_or(inner.next_egress_seq);
        while inner
            .retained
            .front()
            .is_some_and(|entry| entry.seq < min_cursor)
        {
            inner.retained.pop_front();
        }
    }

    /// Removes one consumer and releases every retained target it owned.
    fn retire_consumer_locked(inner: &mut EventLogInner, consumer: tau_core::SharedConsumerId) {
        inner.consumers.remove(&consumer);
        for position in &mut inner.retained {
            position.pending_targets.remove(&consumer);
            if position.pending_targets.is_empty() {
                position.payload = None;
            }
        }
        Self::prune_locked(inner);
        Self::observe_delivery_memory_locked(inner);
    }

    /// Recursively measures the canonical shared live suffix behind its
    /// explicit trace guard and emits no payload or process-local identity.
    fn observe_delivery_memory_locked(inner: &mut EventLogInner) {
        #[cfg(test)]
        {
            inner.work.observe_calls = inner.work.observe_calls.saturating_add(1);
        }
        let tracing_enabled = tracing::enabled!(
            target: "tau_harness::delivery_memory",
            tracing::Level::TRACE
        );
        #[cfg(not(test))]
        if !tracing_enabled {
            return;
        }
        #[cfg(test)]
        if !inner.force_delivery_memory && !tracing_enabled {
            return;
        }
        let mut measurement = inner
            .delivery_memory
            .take()
            .unwrap_or_else(|| Box::new(DeliveryMemoryState::default()));
        let retained_payloads = inner
            .retained
            .iter()
            .filter(|position| position.payload.is_some())
            .map(|position| position.seq)
            .collect::<HashSet<_>>();
        measurement
            .estimates
            .retain(|seq, _| retained_payloads.contains(seq));
        for position in &inner.retained {
            let Some(payload) = &position.payload else {
                continue;
            };
            measurement
                .estimates
                .entry(position.seq)
                .or_insert_with(|| {
                    tau_delivery_memory::DecodedMemoryEstimate::from_serializable_encoding(
                        &payload.frame,
                    )
                    .unwrap_or_default()
                });
        }
        let mut total = tau_delivery_memory::DecodedMemoryEstimate::default();
        let mut shared_allocations = 0_u64;
        let mut shared_fanout = 0_u64;
        let mut pending_target_fanout = 0_u64;
        for position in &inner.retained {
            let Some(payload) = &position.payload else {
                continue;
            };
            total = total.saturating_add(
                measurement
                    .estimates
                    .get(&position.seq)
                    .copied()
                    .unwrap_or_default(),
            );
            shared_allocations = shared_allocations.saturating_add(1);
            shared_fanout = shared_fanout.saturating_add(Arc::strong_count(payload) as u64);
            pending_target_fanout =
                pending_target_fanout.saturating_add(position.pending_targets.len() as u64);
        }
        measurement.high_encoded_bytes = measurement.high_encoded_bytes.max(total.encoded_bytes);
        measurement.high_logical_bytes = measurement
            .high_logical_bytes
            .max(total.logical_payload_bytes);
        measurement.high_requested_capacity = measurement
            .high_requested_capacity
            .max(total.requested_capacity_estimate);
        measurement.high_shared_allocations =
            measurement.high_shared_allocations.max(shared_allocations);
        measurement.high_shared_fanout = measurement.high_shared_fanout.max(shared_fanout);
        measurement.high_pending_target_fanout = measurement
            .high_pending_target_fanout
            .max(pending_target_fanout);
        tracing::trace!(
            target: "tau_harness::delivery_memory",
            process = "harness",
            cut = "live_suffix",
            items = shared_allocations,
            owners = inner.consumers.len() as u64,
            encoded_bytes = total.encoded_bytes,
            decoded_logical_bytes_estimate = total.logical_payload_bytes,
            decoded_requested_capacity_estimate = total.requested_capacity_estimate,
            decoded_containers = total.container_count,
            expansion_milli = total.expansion_milli(),
            shared_allocations,
            shared_fanout,
            pending_target_fanout,
            overlap_fanout = shared_fanout.saturating_sub(shared_allocations),
            high_water_encoded_bytes = measurement.high_encoded_bytes,
            high_water_decoded_logical_bytes_estimate = measurement.high_logical_bytes,
            high_water_decoded_requested_capacity_estimate = measurement.high_requested_capacity,
            high_water_shared_allocations = measurement.high_shared_allocations,
            high_water_shared_fanout = measurement.high_shared_fanout,
            high_water_pending_target_fanout = measurement.high_pending_target_fanout,
            kernel_bytes_observable = false,
            "decoded delivery memory ownership"
        );
        inner.delivery_memory = Some(measurement);
    }

    /// Forces measurement past shared callsite interest for this test log.
    #[cfg(test)]
    fn force_delivery_memory_for_test(&self) {
        self.inner
            .lock()
            .expect("event log mutex poisoned")
            .force_delivery_memory = true;
    }

    /// Unlocks one completed state transition before broadcasting it.
    fn notify_changed(&self, inner: std::sync::MutexGuard<'_, EventLogInner>) {
        drop(inner);
        #[cfg(test)]
        self.notifications.fetch_add(1, Ordering::Relaxed);
        self.changed.notify_all();
    }

    /// Resets exact operation counts for one focused complexity observation.
    #[cfg(test)]
    fn reset_work(&self) {
        self.inner.lock().expect("event log mutex poisoned").work = EventLogWork::default();
        self.notifications.store(0, Ordering::Relaxed);
    }

    /// Returns exact operation counts and broadcasts since the last reset.
    #[cfg(test)]
    fn work(&self) -> (EventLogWork, u64) {
        (
            self.inner.lock().expect("event log mutex poisoned").work,
            self.notifications.load(Ordering::Relaxed),
        )
    }

    /// Waits until a follower has entered the requested replay-pause wait.
    #[cfg(test)]
    fn wait_for_catch_up_wait(&self, count: u64, timeout: Duration) -> bool {
        let inner = self.inner.lock().expect("event log mutex poisoned");
        let (inner, _) = self
            .waiter_entered
            .wait_timeout_while(inner, timeout, |inner| inner.work.catch_up_waits < count)
            .expect("event log mutex poisoned while observing catch-up wait");
        inner.work.catch_up_waits >= count
    }

    /// Waits until a follower has entered the requested live-tail wait.
    #[cfg(test)]
    fn wait_for_tail_wait(&self, count: u64, timeout: Duration) -> bool {
        let inner = self.inner.lock().expect("event log mutex poisoned");
        let (inner, _) = self
            .waiter_entered
            .wait_timeout_while(inner, timeout, |inner| inner.work.tail_waits < count)
            .expect("event log mutex poisoned while observing tail wait");
        inner.work.tail_waits >= count
    }

    /// Reserves the next harness runtime event-log sequence.
    ///
    /// Durable-history replay uses this path: replayed transcript facts already
    /// live in agent logs, but their runtime deliveries still need fresh
    /// globally monotonic [`EventLogSeq`] values rather than reusing persisted
    /// per-agent/per-session sequences.
    pub(crate) fn reserve_seq(&self) -> EventLogSeq {
        let mut inner = self.inner.lock().expect("event log mutex poisoned");
        let seq = inner.next_seq;
        inner.next_seq = inner.next_seq.next();
        seq
    }

    /// Assigns a sequence and timestamp for focused event-log tests.
    #[cfg(test)]
    pub(crate) fn append(&self) -> (EventLogSeq, UnixMicros) {
        (self.reserve_seq(), UnixMicros::now())
    }

    /// Records a committed event for test assertions only.
    #[cfg(test)]
    pub(crate) fn record_for_test(
        &self,
        seq: EventLogSeq,
        recorded_at: UnixMicros,
        source: Option<ConnectionId>,
        event: Event,
    ) {
        let mut inner = self.inner.lock().expect("event log mutex poisoned");
        inner.entries.insert(
            seq,
            LogEntry {
                seq,
                recorded_at,
                source,
                event,
            },
        );
    }

    /// Returns the first test-observed entry with seq >= `from`, or `None` if
    /// no such entry exists yet.
    #[cfg(test)]
    pub(crate) fn get_next_from(&self, from: EventLogSeq) -> Option<LogEntry> {
        let inner = self.inner.lock().expect("event log mutex poisoned");
        inner
            .entries
            .range(from..)
            .next()
            .map(|(_, entry)| entry.clone())
    }

    /// Returns the next runtime event-log sequence. Used by tests to assert
    /// that no event-log sequence was consumed across a section of code.
    #[cfg(test)]
    pub(crate) fn next_seq(&self) -> EventLogSeq {
        self.inner
            .lock()
            .expect("event log mutex poisoned")
            .next_seq
    }
}

#[cfg(test)]
mod tests;
