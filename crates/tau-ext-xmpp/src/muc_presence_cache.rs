//! Bounded, room-scoped MUC occupant identity state.

use std::collections::{HashMap, HashSet};

use xmpp_parsers::jid::{BareJid, FullJid};

/// Maximum retained occupant mappings for one MUC room.
pub(crate) const MAX_MUC_OCCUPANTS_PER_ROOM: usize = 256;

/// Maximum retained occupant mappings across one XMPP worker.
pub(crate) const MAX_MUC_OCCUPANTS_TOTAL: usize = 1024;

/// Maximum rooms remembered for warning suppression during one connection.
pub(crate) const MAX_WARNED_MUC_ROOMS: usize = 1024;

/// Result of admitting one available occupant mapping.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Admission {
    /// The mapping was inserted or replaced while the cache stayed complete.
    Retained,
    /// A new mapping crossed a limit and made this room's cache incomplete.
    Quarantined,
    /// The room was already quarantined, so no mapping was retained.
    AlreadyQuarantined,
}

/// Complete occupant mappings and fail-closed quarantine state for one room.
#[derive(Default)]
struct RoomState {
    /// Full occupant JIDs mapped to server-asserted real JIDs.
    occupants: HashMap<FullJid, FullJid>,
    /// Whether an overflow made this room's roster incomplete.
    quarantined: bool,
}

/// Bounded MUC presence authentication state for one XMPP connection.
#[derive(Default)]
pub(crate) struct MucPresenceCache {
    /// Per-room complete mappings and quarantine markers.
    rooms: HashMap<BareJid, RoomState>,
    /// Number of mappings retained across all rooms.
    total_occupants: usize,
    /// Rooms whose one content-free overflow warning was already emitted.
    warned_rooms: HashSet<BareJid>,
    /// Whether warning metadata reached its bound, suppressing every later
    /// warning.
    warnings_suppressed: bool,
}

impl MucPresenceCache {
    /// Start a fresh room join with no stale mappings or quarantine marker.
    pub(crate) fn begin_join(&mut self, room: &BareJid) {
        self.purge_room(room);
    }

    /// Admit or replace one occupant mapping under both inclusive limits.
    pub(crate) fn admit(&mut self, occupant: FullJid, real_jid: FullJid) -> Admission {
        let room = occupant.to_bare();
        let state = self.rooms.entry(room.clone()).or_default();
        if state.quarantined {
            return Admission::AlreadyQuarantined;
        }
        if let Some(retained) = state.occupants.get_mut(&occupant) {
            *retained = real_jid;
            return Admission::Retained;
        }
        if MAX_MUC_OCCUPANTS_PER_ROOM <= state.occupants.len()
            || MAX_MUC_OCCUPANTS_TOTAL <= self.total_occupants
        {
            self.total_occupants -= state.occupants.len();
            state.occupants.clear();
            state.quarantined = true;
            return Admission::Quarantined;
        }
        state.occupants.insert(occupant, real_jid);
        self.total_occupants += 1;
        Admission::Retained
    }

    /// Remove one exact occupant mapping without changing quarantine state.
    pub(crate) fn remove(&mut self, occupant: &FullJid) {
        let room = occupant.to_bare();
        let Some(state) = self.rooms.get_mut(&room) else {
            return;
        };
        if state.occupants.remove(occupant).is_some() {
            self.total_occupants -= 1;
        }
    }

    /// Return the retained real JID for one exact occupant.
    pub(crate) fn get(&self, occupant: &FullJid) -> Option<&FullJid> {
        let room = occupant.to_bare();
        self.rooms
            .get(&room)
            .and_then(|state| state.occupants.get(occupant))
    }

    /// Return whether one room must fail closed after overflow.
    pub(crate) fn is_quarantined(&self, room: &BareJid) -> bool {
        self.rooms.get(room).is_some_and(|state| state.quarantined)
    }

    /// Claim the single active-room overflow warning allowed per connection.
    pub(crate) fn take_warning(&mut self, room: &BareJid) -> bool {
        if self.warnings_suppressed || self.warned_rooms.contains(room) {
            return false;
        }
        if MAX_WARNED_MUC_ROOMS <= self.warned_rooms.len() {
            self.warnings_suppressed = true;
            return false;
        }
        self.warned_rooms.insert(room.clone())
    }

    /// Purge mappings and quarantine state for one retired or rolled-back room.
    pub(crate) fn purge_room(&mut self, room: &BareJid) {
        if let Some(state) = self.rooms.remove(room) {
            self.total_occupants -= state.occupants.len();
        }
    }

    /// Purge every connection-scoped mapping, quarantine, and warning marker.
    pub(crate) fn clear_connection(&mut self) {
        *self = Self::default();
    }

    #[cfg(test)]
    /// Return the number of mappings retained in one room.
    pub(crate) fn room_len(&self, room: &BareJid) -> usize {
        self.rooms
            .get(room)
            .map_or(0, |state| state.occupants.len())
    }

    #[cfg(test)]
    /// Return the number of mappings retained across the worker.
    pub(crate) fn total_len(&self) -> usize {
        self.total_occupants
    }

    #[cfg(test)]
    /// Return warning-suppression state for bounded-metadata assertions.
    pub(crate) fn warning_state(&self) -> (usize, usize, bool) {
        (
            self.warned_rooms.len(),
            self.warned_rooms.capacity(),
            self.warnings_suppressed,
        )
    }
}
