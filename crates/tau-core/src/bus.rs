//! [`EventBus`]: routes protocol events between connections and tracks
//! per-connection subscription state.

use std::collections::{HashMap, HashSet};

use tau_proto::{ClientKind, ConnectionId, EventSelector, HarnessOutputMessage};

use crate::connection::{
    Connection, ConnectionMetadata, ConnectionSink, DeliveryFailure, RouteError, RouteReport,
    RoutedFrame, VisibilityFilter,
};

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct SubscriptionSet {
    historical_selectors: Vec<EventSelector>,
    live_selectors: Vec<EventSelector>,
    catch_up_blocked: bool,
}

impl SubscriptionSet {
    pub(crate) fn replace(
        &mut self,
        historical_selectors: Vec<EventSelector>,
        live_selectors: Vec<EventSelector>,
    ) {
        let was_catch_up_blocked = self.catch_up_blocked;
        self.historical_selectors = historical_selectors;
        self.live_selectors = live_selectors;
        // Pending live frames already matched the live selectors at publish
        // time. Preserve them across blocked subscription replacement so a
        // catch-up resubscribe cannot retroactively drop committed delivery.
        self.catch_up_blocked = was_catch_up_blocked;
    }

    pub(crate) fn matches(&self, message: &HarnessOutputMessage) -> bool {
        self.live_selectors
            .iter()
            .any(|selector| selector_matches(selector, message))
    }

    pub(crate) fn historical_selectors(&self) -> &[EventSelector] {
        &self.historical_selectors
    }

    pub(crate) fn live_selectors(&self) -> &[EventSelector] {
        &self.live_selectors
    }

    fn is_catch_up_blocked(&self) -> bool {
        self.catch_up_blocked
    }

    fn finish_catch_up(&mut self) {
        self.catch_up_blocked = false;
    }
}

pub(crate) fn selector_matches(selector: &EventSelector, message: &HarnessOutputMessage) -> bool {
    // Subscriptions match only event deliveries. Other harness output messages
    // are point-to-point control plane and are not subscribable.
    let Some(event) = message.delivered_event() else {
        return false;
    };
    let target_name = event.name();
    match selector {
        EventSelector::Exact(name) => *name == target_name,
        EventSelector::Prefix(prefix) => target_name.matches_prefix(prefix),
    }
}

pub(crate) struct ConnectionEntry {
    pub(crate) metadata: ConnectionMetadata,
    pub(crate) sink: Box<dyn ConnectionSink>,
    pub(crate) visibility_filter: Box<dyn VisibilityFilter>,
    pub(crate) subscriptions: SubscriptionSet,
}

/// Validates the event families selected by a socket connection.
fn validate_socket_subscription(
    connection: &ConnectionMetadata,
    selectors: &[EventSelector],
) -> Result<(), &'static str> {
    if connection.origin != crate::ConnectionOrigin::Socket {
        return Ok(());
    }

    fn category_allowed(category: &tau_proto::EventCategory) -> bool {
        use tau_proto::EventCategory as C;
        match category {
            C::Tool
            | C::Action
            | C::Agent
            | C::Message
            | C::Extension
            | C::Provider
            | C::Session
            | C::Ui
            | C::Harness
            | C::Shell
            | C::Term => true,
            C::Other(_) => false,
        }
    }

    let all_allowed = selectors.iter().all(|selector| match selector {
        EventSelector::Exact(name) => category_allowed(name.category()),
        EventSelector::Prefix(prefix) => {
            let category = prefix
                .split_once('.')
                .map_or(prefix.as_str(), |(value, _)| value);
            category_allowed(&tau_proto::EventCategory::from_wire(category))
        }
    });
    if all_allowed {
        Ok(())
    } else {
        Err("socket clients may only subscribe to allowed event families")
    }
}

/// Internal event bus and subscription registry.
#[derive(Default)]
pub struct EventBus {
    next_connection_id: u64,
    connections: HashMap<ConnectionId, ConnectionEntry>,
    #[cfg(test)]
    last_route_work: RouteWork,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct RouteWork {
    /// Connection identifiers cloned solely into a detailed route report.
    pub(crate) report_id_clones: usize,
    /// Entries materialized solely in detailed route-report vectors.
    pub(crate) report_entries: usize,
    /// Shared admitted-target membership probes performed during
    /// reconciliation.
    pub(crate) admitted_membership_probes: usize,
}

trait RouteCollector {
    /// Records a subscriber skipped by kind or selector.
    fn skipped(&mut self, connection_id: &ConnectionId);

    /// Records a subscriber rejected by its visibility filter.
    fn blocked(&mut self, connection_id: &ConnectionId);

    /// Records a successful delivery.
    fn delivered(&mut self, connection_id: ConnectionId);

    /// Records a failed delivery.
    fn failed(&mut self, connection_id: ConnectionId, error: crate::ConnectionSendError);

    /// Records a failed delivery whose group error must be copied into a
    /// report.
    fn failed_shared(&mut self, connection_id: ConnectionId, error: &crate::ConnectionSendError);

    /// Records a shared consumer that retired before admission.
    fn retired(&mut self, connection_id: ConnectionId);

    /// Records one constant-time admitted-target membership probe.
    #[cfg(test)]
    fn probed_admitted_target(&mut self);
}

#[derive(Default)]
struct DetailedRouteCollector {
    /// Externally visible routing result.
    report: RouteReport,
    #[cfg(test)]
    /// Work performed only to construct or reconcile the result.
    work: RouteWork,
}

impl RouteCollector for DetailedRouteCollector {
    fn skipped(&mut self, connection_id: &ConnectionId) {
        self.report
            .skipped_by_subscription
            .push(connection_id.clone());
        #[cfg(test)]
        {
            self.work.report_id_clones += 1;
            self.work.report_entries += 1;
        }
    }

    fn blocked(&mut self, connection_id: &ConnectionId) {
        self.report.blocked_by_filter.push(connection_id.clone());
        #[cfg(test)]
        {
            self.work.report_id_clones += 1;
            self.work.report_entries += 1;
        }
    }

    fn delivered(&mut self, connection_id: ConnectionId) {
        self.report.delivered_to.push(connection_id);
        #[cfg(test)]
        {
            self.work.report_entries += 1;
        }
    }

    fn failed(&mut self, connection_id: ConnectionId, error: crate::ConnectionSendError) {
        self.report.failed_deliveries.push(DeliveryFailure {
            connection_id,
            error,
        });
        #[cfg(test)]
        {
            self.work.report_entries += 1;
        }
    }

    fn failed_shared(&mut self, connection_id: ConnectionId, error: &crate::ConnectionSendError) {
        self.failed(connection_id, error.clone());
    }

    fn retired(&mut self, connection_id: ConnectionId) {
        self.report.failed_deliveries.push(DeliveryFailure {
            connection_id,
            error: crate::ConnectionSendError::new(
                "shared consumer generation retired before admission",
            ),
        });
        #[cfg(test)]
        {
            self.work.report_entries += 1;
        }
    }

    #[cfg(test)]
    fn probed_admitted_target(&mut self) {
        self.work.admitted_membership_probes += 1;
    }
}

#[derive(Default)]
struct NoReportRouteCollector {
    #[cfg(test)]
    /// Routing work retained for the test-only work oracle.
    work: RouteWork,
}

impl RouteCollector for NoReportRouteCollector {
    fn skipped(&mut self, _connection_id: &ConnectionId) {}

    fn blocked(&mut self, _connection_id: &ConnectionId) {}

    fn delivered(&mut self, _connection_id: ConnectionId) {}

    fn failed(&mut self, _connection_id: ConnectionId, _error: crate::ConnectionSendError) {}

    fn failed_shared(&mut self, _connection_id: ConnectionId, _error: &crate::ConnectionSendError) {
    }

    fn retired(&mut self, _connection_id: ConnectionId) {}

    #[cfg(test)]
    fn probed_admitted_target(&mut self) {
        self.work.admitted_membership_probes += 1;
    }
}

impl EventBus {
    /// Creates an empty event bus.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Reserves a fresh connection identifier for transport setup that must
    /// know the final identity before registration.
    pub fn reserve_connection_id(&mut self) -> ConnectionId {
        self.allocate_connection_id()
    }

    /// Registers a connection and returns its assigned connection ID.
    pub fn connect(&mut self, connection: Connection) -> ConnectionId {
        let connection_id = connection
            .metadata
            .id
            .unwrap_or_else(|| self.allocate_connection_id());

        let metadata = ConnectionMetadata {
            id: connection_id.clone(),
            name: connection.metadata.name,
            kind: connection.metadata.kind,
            origin: connection.metadata.origin,
        };

        let entry = ConnectionEntry {
            metadata,
            sink: connection.sink,
            visibility_filter: connection.visibility_filter,
            subscriptions: SubscriptionSet::default(),
        };

        if let Some(mut replaced) = self.connections.insert(connection_id.clone(), entry) {
            replaced.sink.retire();
        }
        connection_id
    }

    /// Removes a connection from the bus and returns its metadata if present.
    pub fn disconnect(&mut self, connection_id: &ConnectionId) -> Option<ConnectionMetadata> {
        self.connections.remove(connection_id).map(|mut entry| {
            entry.sink.retire();
            entry.metadata
        })
    }

    /// Returns immutable metadata for one connection.
    #[must_use]
    pub fn connection(&self, connection_id: &ConnectionId) -> Option<&ConnectionMetadata> {
        self.connections
            .get(connection_id)
            .map(|entry| &entry.metadata)
    }

    /// Returns a snapshot of all connected clients.
    #[must_use]
    pub fn connections(&self) -> Vec<ConnectionMetadata> {
        self.connections
            .values()
            .map(|entry| entry.metadata.clone())
            .collect()
    }

    /// Replaces the historical and live subscription selectors for one
    /// connection.
    ///
    /// The bus validates the union of both selector sets before committing
    /// either set. Historical selectors
    /// are used by the harness catch-up/replay path; live selectors are
    /// used for publish-time live routing. If a connection is currently
    /// catch-up blocked and the replacement clears all historical
    /// selectors, the bus treats that as canceling the replay phase and
    /// immediately releases the shared follower's catch-up barrier. Frames
    /// already admitted retain their publication-time eligibility.
    pub fn set_subscriptions(
        &mut self,
        connection_id: &ConnectionId,
        historical_selectors: Vec<EventSelector>,
        live_selectors: Vec<EventSelector>,
    ) -> Result<(), RouteError> {
        let metadata = self
            .connections
            .get(connection_id)
            .map(|entry| entry.metadata.clone())
            .ok_or_else(|| RouteError::UnknownConnection {
                connection_id: connection_id.clone(),
            })?;
        let mut selectors = historical_selectors.clone();
        for selector in &live_selectors {
            if !selectors.contains(selector) {
                selectors.push(selector.clone());
            }
        }
        validate_socket_subscription(&metadata, &selectors).map_err(|reason| {
            RouteError::SubscriptionDenied {
                connection_id: connection_id.clone(),
                reason: reason.to_owned(),
            }
        })?;
        let entry = self.connections.get_mut(connection_id).ok_or_else(|| {
            RouteError::UnknownConnection {
                connection_id: connection_id.clone(),
            }
        })?;
        let should_release_catch_up =
            entry.subscriptions.is_catch_up_blocked() && historical_selectors.is_empty();
        entry
            .subscriptions
            .replace(historical_selectors, live_selectors);
        if should_release_catch_up {
            let _ = self.finish_catch_up(connection_id)?;
        }
        Ok(())
    }

    /// Pauses live delivery for one connection while catch-up replay runs.
    pub fn begin_catch_up(&mut self, connection_id: &ConnectionId) -> Result<(), RouteError> {
        let entry = self.connections.get_mut(connection_id).ok_or_else(|| {
            RouteError::UnknownConnection {
                connection_id: connection_id.clone(),
            }
        })?;
        entry.subscriptions.catch_up_blocked = true;
        entry.sink.begin_catch_up();
        Ok(())
    }

    /// Returns the active historical selectors for one connection.
    #[must_use]
    pub fn historical_subscriptions(
        &self,
        connection_id: &ConnectionId,
    ) -> Option<&[EventSelector]> {
        self.connections
            .get(connection_id)
            .map(|entry| entry.subscriptions.historical_selectors())
    }

    /// Returns the active live selectors for one connection.
    #[must_use]
    pub fn live_subscriptions(&self, connection_id: &ConnectionId) -> Option<&[EventSelector]> {
        self.connections
            .get(connection_id)
            .map(|entry| entry.subscriptions.live_selectors())
    }

    /// Releases one connection's shared-follower catch-up barrier.
    pub fn finish_catch_up(
        &mut self,
        connection_id: &ConnectionId,
    ) -> Result<RouteReport, RouteError> {
        let entry = self.connections.get_mut(connection_id).ok_or_else(|| {
            RouteError::UnknownConnection {
                connection_id: connection_id.clone(),
            }
        })?;
        entry.subscriptions.finish_catch_up();
        entry.sink.finish_catch_up();
        Ok(RouteReport::default())
    }

    /// Broadcasts one harness output message to subscribed and visible clients.
    pub fn publish(&mut self, message: HarnessOutputMessage) -> RouteReport {
        self.publish_from(None, message)
    }

    /// Broadcasts one harness output message from a specific source connection.
    pub fn publish_from(
        &mut self,
        source_id: Option<&ConnectionId>,
        message: HarnessOutputMessage,
    ) -> RouteReport {
        self.publish_from_excluding_kinds(source_id, message, &[])
    }

    /// Broadcasts one harness output message from a specific source connection
    /// while skipping subscribers whose connection kind is in
    /// `excluded_kinds`.
    pub fn publish_from_excluding_kinds(
        &mut self,
        source_id: Option<&ConnectionId>,
        message: HarnessOutputMessage,
        excluded_kinds: &[ClientKind],
    ) -> RouteReport {
        let collector = self.publish_with_collector(
            source_id,
            message,
            excluded_kinds,
            DetailedRouteCollector::default(),
        );
        #[cfg(test)]
        {
            self.last_route_work = collector.work;
        }
        collector.report
    }

    /// Broadcasts one message without constructing a [`RouteReport`].
    ///
    /// This is intended for internal callers that deliberately ignore all
    /// per-connection routing diagnostics.
    pub fn publish_from_excluding_kinds_without_report(
        &mut self,
        source_id: Option<&ConnectionId>,
        message: HarnessOutputMessage,
        excluded_kinds: &[ClientKind],
    ) {
        let collector = self.publish_with_collector(
            source_id,
            message,
            excluded_kinds,
            NoReportRouteCollector::default(),
        );
        #[cfg(test)]
        {
            self.last_route_work = collector.work;
        }
        #[cfg(not(test))]
        let _ = collector;
    }

    fn publish_with_collector<C: RouteCollector>(
        &mut self,
        source_id: Option<&ConnectionId>,
        message: HarnessOutputMessage,
        excluded_kinds: &[ClientKind],
        mut collector: C,
    ) -> C {
        let routed = RoutedFrame::new(source_id.cloned(), message);
        let mut eligible = Vec::new();
        for (connection_id, entry) in &self.connections {
            if excluded_kinds.contains(&entry.metadata.kind) {
                collector.skipped(connection_id);
                continue;
            }
            if !entry.subscriptions.matches(&routed.frame) {
                collector.skipped(connection_id);
                continue;
            }
            if !entry.visibility_filter.allows(&routed) {
                collector.blocked(connection_id);
                continue;
            }

            eligible.push(connection_id.clone());
        }
        self.deliver_eligible(routed, eligible, &mut collector);
        collector
    }

    /// Sends one directed harness output message to a specific connection.
    pub fn send_to(
        &mut self,
        target_id: &ConnectionId,
        source_id: Option<&ConnectionId>,
        message: HarnessOutputMessage,
    ) -> Result<RouteReport, RouteError> {
        let routed = RoutedFrame::new(source_id.cloned(), message);
        let entry =
            self.connections
                .get_mut(target_id)
                .ok_or_else(|| RouteError::UnknownConnection {
                    connection_id: target_id.clone(),
                })?;

        let mut report = RouteReport::default();
        if !entry.visibility_filter.allows(&routed) {
            report.blocked_by_filter.push(target_id.clone());
            return Ok(report);
        }

        match entry.sink.send(routed) {
            Ok(()) => report.delivered_to.push(target_id.clone()),
            Err(error) => report.failed_deliveries.push(DeliveryFailure {
                connection_id: target_id.clone(),
                error,
            }),
        }

        Ok(report)
    }

    /// Delivers one immutable routing decision through shared streams where
    /// available, falling back to legacy per-connection sinks for tests and
    /// embedders that have not opted into shared delivery.
    fn deliver_eligible(
        &mut self,
        routed: RoutedFrame,
        eligible: Vec<ConnectionId>,
        collector: &mut impl RouteCollector,
    ) {
        let mut shared: HashMap<
            crate::SharedDeliveryGroup,
            Vec<(ConnectionId, crate::SharedDeliveryTarget)>,
        > = HashMap::new();
        let mut legacy = Vec::new();
        for connection_id in eligible {
            let target = self
                .connections
                .get(&connection_id)
                .and_then(|entry| entry.sink.shared_delivery_target());
            if let Some(target) = target {
                shared
                    .entry(target.group())
                    .or_default()
                    .push((connection_id, target));
            } else {
                legacy.push(connection_id);
            }
        }
        for members in shared.into_values() {
            let targets = members
                .iter()
                .map(|(_, target)| *target)
                .collect::<Vec<_>>();
            let first = members
                .first()
                .expect("shared delivery group must contain one member")
                .0
                .clone();
            let result = self
                .connections
                .get_mut(&first)
                .expect("eligible connection must remain registered")
                .sink
                .send_shared(routed.clone(), &targets);
            match result {
                Ok(admitted) => {
                    let admitted = admitted.into_iter().collect::<HashSet<_>>();
                    for (connection_id, target) in members {
                        #[cfg(test)]
                        collector.probed_admitted_target();
                        if admitted.contains(&target) {
                            collector.delivered(connection_id);
                        } else {
                            collector.retired(connection_id);
                        }
                    }
                }
                Err(error) => {
                    for (connection_id, _) in members {
                        collector.failed_shared(connection_id, &error);
                    }
                }
            }
        }
        for connection_id in legacy {
            let result = self
                .connections
                .get_mut(&connection_id)
                .expect("eligible connection must remain registered")
                .sink
                .send(routed.clone());
            match result {
                Ok(()) => collector.delivered(connection_id),
                Err(error) => collector.failed(connection_id, error),
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn last_route_work(&self) -> RouteWork {
        self.last_route_work
    }

    fn allocate_connection_id(&mut self) -> ConnectionId {
        self.next_connection_id += 1;
        ConnectionId::parse(format!("conn-{}", self.next_connection_id))
            .expect("generated connection id must satisfy the connection identifier grammar")
    }
}

#[cfg(test)]
#[path = "bus_tests.rs"]
mod tests;
