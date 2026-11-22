//! Search quick-add intent is not an occurrence or a write receipt. The
//! account socket publishes shared state only, including other clients' adds.
use std::{
    collections::{HashMap, HashSet, VecDeque},
    time::Duration,
};

use tokio::time::Instant;

use crate::{
    api::library::{PlaylistKind, PlaylistSummary},
    personal::{AcceptedPlaylist, PersonalCommand, Replica},
    personal_bridge::{Link, Status},
};

pub(crate) use crate::personal::Membership;

const SEND_WAIT: Duration = Duration::from_secs(5);
const MAX_LIVE: usize = 8;
// Go admits into a shared FIFO, and a dequeued job can take 15 seconds.
// Wait from actual socket start, then show uncertainty, never failure.
// Later authoritative membership still settles the row.
const OBSERVE_WAIT: Duration = Duration::from_secs(30);
const PROBE_AFTER: Duration = Duration::from_secs(8);
// The legacy wire cannot order or attribute an outstanding add to this
// client across either shared projections or reconnects. Fail closed on
// exhaustion instead of expiring an attempt or evicting its hold.
// 4 MiB of key data at worst (two ≤512-byte IDs per valid command),
// plus bounded map/set storage; repeated targets share one key.
const MAX_ATTEMPTS: usize = 4096;
const READ_IDLE: Duration = Duration::from_secs(20);
const READ_LIMIT: Duration = Duration::from_secs(120);
// A capped Initial cannot prove that a remembered direct target still exists.
// A bad scoped read can close the whole Go connection, with no not-found frame.
const MAX_UNVERIFIED_LINKS: u8 = 2;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Ticket {
    nonce: u64,
    pub(crate) epoch: u64,
    pub(crate) generation: u64,
    pub(crate) mode_stamp: u64,
    interruption: u64,
    target: String,
    track: String,
}

impl Ticket {
    pub(crate) fn command(&self) -> PersonalCommand {
        PersonalCommand::AddEntry {
            playlist_id: self.target.clone(),
            track_id: self.track.clone(),
        }
    }

    fn matches(&self, status: Status) -> bool {
        status.epoch == self.epoch
            && status.generation == self.generation
            && status.interruption == self.interruption
            && status.link == Link::Ready
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    NotSent,
    Unconfirmed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Staged,
    Queued,
    Started,
}

struct Pending {
    ticket: Ticket,
    phase: Phase,
    due: Instant,
    probe_due: Option<Instant>,
}

struct Read {
    due: Instant,
    limit: Instant,
}

#[derive(Default)]
struct Revalidation {
    last_link: Option<u64>,
    links: u8,
    exhausted: bool,
}

pub(crate) struct View<'a> {
    pub(crate) target: Option<&'a PlaylistSummary>,
    pub(crate) hide: bool,
    pub(crate) at_capacity: bool,
    pub(crate) due: Option<Instant>,
    pub(crate) read_stalled: bool,
    pub(crate) read_retired: bool,
    pub(crate) session_limit: bool,
    status: Status,
    selection_version: u64,
    replica: &'a Replica,
    ready: bool,
    pending: &'a [Pending],
    attempted: Option<&'a HashSet<Box<str>>>,
    not_sent: &'a VecDeque<Ticket>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Row {
    pub(crate) membership: Membership,
    pub(crate) pending: bool,
    pub(crate) outcome: Option<Outcome>,
    pub(crate) visible: bool,
    pub(crate) can_add: bool,
}

impl View<'_> {
    pub(crate) fn scope(&self, mode_stamp: u64) -> String {
        format!(
            "{:x}:{:x}:{:x}:{:x}:{:x}:{}",
            self.status.epoch,
            self.status.generation,
            self.status.interruption,
            self.selection_version,
            mode_stamp,
            self.target.map_or("", |target| target.id.as_str()),
        )
    }

    pub(crate) fn row(&self, track_id: &str) -> Row {
        let membership = self
            .target
            .filter(|_| self.ready)
            .map_or(Membership::Unknown, |target| {
                self.replica.membership(&target.id, track_id)
            });
        let matches = |ticket: &Ticket| {
            self.target.is_some_and(|target| target.id == ticket.target) && track_id == ticket.track
        };
        let present = membership == Membership::Present;
        let pending = !present && self.pending.iter().any(|p| matches(&p.ticket));
        let held = self
            .attempted
            .is_some_and(|tracks| tracks.contains(track_id));
        let outcome = if present || pending {
            None
        } else if held {
            Some(Outcome::Unconfirmed)
        } else if self.not_sent.iter().any(matches) {
            Some(Outcome::NotSent)
        } else {
            None
        };
        Row {
            membership,
            pending,
            outcome,
            visible: !self.hide || !present,
            can_add: membership == Membership::Absent
                && !pending
                && !held
                && !self.at_capacity
                && !self.session_limit,
        }
    }
}

#[derive(Default)]
pub(crate) struct QuickAdd {
    target: Option<PlaylistSummary>,
    authorized: bool,
    hide: bool,
    searching: bool,
    selection_version: u64,
    nonce: u64,
    pending: Vec<Pending>,
    // Never-sent intents have no replay hold. Keep only a bounded recent
    // history for the rows; evicting it doesn't change write eligibility.
    not_sent: VecDeque<Ticket>,
    attempted: HashMap<Box<str>, HashSet<Box<str>>>,
    attempt_count: usize,
    read: Option<Read>,
    read_stalled: bool,
    revalidation: Revalidation,
}

pub(crate) fn editable(p: &PlaylistSummary) -> bool {
    p.kind == PlaylistKind::Manual && p.capabilities.edit_entries && p.capabilities.add_tracks
}

impl QuickAdd {
    pub(crate) fn view<'a>(
        &'a self,
        scoped: bool,
        replica: &'a Replica,
        status: Status,
    ) -> View<'a> {
        let target = self.target.as_ref().filter(|_| scoped);
        View {
            target,
            hide: scoped && self.hide,
            at_capacity: scoped && self.at_capacity(),
            due: scoped.then(|| self.due()).flatten(),
            read_stalled: scoped && self.read_unavailable(),
            read_retired: scoped && self.read_unavailable() && self.watched().is_none(),
            session_limit: scoped && self.attempt_count >= MAX_ATTEMPTS,
            status,
            selection_version: self.selection_version,
            ready: scoped && self.searching && self.authorized && status.link == Link::Ready,
            attempted: target.and_then(|t| self.attempted.get(t.id.as_str())),
            not_sent: &self.not_sent,
            pending: &self.pending,
            replica,
        }
    }

    pub(crate) fn selected(&self) -> Option<&str> {
        self.target.as_ref().map(|p| p.id.as_str())
    }

    pub(crate) fn authorized_target(&self) -> Option<&PlaylistSummary> {
        self.target.as_ref().filter(|_| self.authorized)
    }

    pub(crate) fn searching(&self) -> bool {
        self.searching
    }

    pub(crate) fn watched(&self) -> Option<&str> {
        self.selected().filter(|_| {
            self.searching
                && !self.revalidation.exhausted
                && !(self.read_stalled && self.read.is_none())
        })
    }

    fn read_unavailable(&self) -> bool {
        self.read_stalled || self.revalidation.exhausted
    }

    pub(crate) fn set_searching(&mut self, searching: bool) {
        if self.searching != searching {
            self.searching = searching;
            self.reset_read();
            if !searching {
                self.cancel_unsent();
            }
        }
    }

    pub(crate) fn select(&mut self, target: Option<PlaylistSummary>) {
        if self.selected() != target.as_ref().map(|p| p.id.as_str()) {
            self.selection_version = self
                .selection_version
                .checked_add(1)
                .expect("selection version exhausted");
            self.cancel_unsent();
            self.hide = false;
            self.reset_read();
            self.revalidation = Revalidation::default();
        }
        self.authorized = target.is_some();
        self.target = target;
    }

    pub(crate) fn filter(&mut self, hide: bool) {
        self.hide = hide && self.target.is_some();
    }

    fn summary(&mut self, summary: &PlaylistSummary) {
        if self.selected() == Some(&summary.id) {
            self.select(editable(summary).then(|| summary.clone()));
        }
    }

    pub(crate) fn observe(&mut self, accepted: AcceptedPlaylist<'_>) {
        let Some(id) = self.selected() else { return };
        match accepted {
            AcceptedPlaylist::Summaries(list) => {
                if let Some(p) = list.iter().find(|p| p.id == id) {
                    self.summary(p);
                } else if !Replica::summaries_capped(list) {
                    self.select(None);
                } else {
                    // Absence from Go's capped initial window is not deletion.
                    // Keep the exact selection; revalidate it via its next scoped pull.
                    self.authorized = false;
                    self.cancel_unsent();
                }
            }
            AcceptedPlaylist::Summary { summary, .. } => self.summary(summary),
            AcceptedPlaylist::Complete(playlist) => self.summary(&playlist.summary),
            AcceptedPlaylist::Deleted(deleted) if id == deleted => self.select(None),
            AcceptedPlaylist::Deleted(_) => {}
        }
    }

    pub(crate) fn observe_scoped(&mut self, replica: &Replica) {
        if let Some(summary) = replica.scoped_summary() {
            self.summary(summary);
        }
        let present = |ticket: &Ticket| {
            replica.membership(&ticket.target, &ticket.track) == Membership::Present
        };
        // Membership stops checking, not the dispatch hold. A foreign add
        // could be removed before our own delayed command is applied.
        self.pending.retain(|p| !present(&p.ticket));
        self.not_sent.retain(|ticket| !present(ticket));
    }

    fn at_capacity(&self) -> bool {
        // Staged and queued intents reserve space in the never-evicted hold
        // set. Starting consumes that reservation without increasing the sum.
        self.pending.len() >= MAX_LIVE
            || (self.attempt_count < MAX_ATTEMPTS
                && self.attempt_count
                    + self
                        .pending
                        .iter()
                        .filter(|p| p.phase != Phase::Started)
                        .count()
                    >= MAX_ATTEMPTS)
    }

    pub(crate) fn stage(
        &mut self,
        status: Status,
        replica: &Replica,
        mode_stamp: u64,
        expected_target: &str,
        track: &str,
        now: Instant,
    ) -> Option<Ticket> {
        self.expire_pending(now);
        if self.selected() != Some(expected_target)
            || !self.writable(status, replica, expected_target, track)
            || self.at_capacity()
            || self.attempt_count >= MAX_ATTEMPTS
            || self
                .pending
                .iter()
                .any(|p| p.ticket.target == expected_target && p.ticket.track == track)
            || self
                .attempted
                .get(expected_target)
                .is_some_and(|tracks| tracks.contains(track))
            || mode_stamp & 1 != 0
        {
            return None;
        }
        self.nonce = self
            .nonce
            .checked_add(1)
            .expect("quick add nonce exhausted");
        let ticket = Ticket {
            nonce: self.nonce,
            epoch: status.epoch,
            generation: status.generation,
            mode_stamp,
            interruption: status.interruption,
            target: expected_target.into(),
            track: track.into(),
        };
        if !ticket.command().valid() {
            return None;
        }
        self.not_sent
            .retain(|t| !(t.target == expected_target && t.track == track));
        self.pending.push(Pending {
            ticket: ticket.clone(),
            phase: Phase::Staged,
            due: now + SEND_WAIT,
            probe_due: None,
        });
        Some(ticket)
    }

    fn writable(&self, status: Status, replica: &Replica, target: &str, track: &str) -> bool {
        status.link == Link::Ready
            && self.authorized
            && self.watched() == Some(target)
            && self.target.as_ref().is_some_and(editable)
            && replica.membership(target, track) == Membership::Absent
    }

    pub(crate) fn admit(
        &mut self,
        status: Status,
        replica: &Replica,
        ticket: &Ticket,
        now: Instant,
        send: impl FnOnce() -> bool,
    ) -> bool {
        self.advance(
            status,
            replica,
            ticket,
            now,
            Phase::Staged,
            Phase::Queued,
            send,
        )
    }

    pub(crate) fn start(
        &mut self,
        status: Status,
        replica: &Replica,
        ticket: &Ticket,
        now: Instant,
        start: impl FnOnce() -> bool,
    ) -> bool {
        self.advance(
            status,
            replica,
            ticket,
            now,
            Phase::Queued,
            Phase::Started,
            start,
        )
    }

    #[expect(clippy::too_many_arguments)]
    fn advance(
        &mut self,
        status: Status,
        replica: &Replica,
        ticket: &Ticket,
        now: Instant,
        from: Phase,
        into: Phase,
        send: impl FnOnce() -> bool,
    ) -> bool {
        self.expire_pending(now);
        let Some(i) = self
            .pending
            .iter()
            .position(|p| p.ticket == *ticket && p.phase == from)
        else {
            return false;
        };
        if !ticket.matches(status)
            || !self.writable(status, replica, &ticket.target, &ticket.track)
            || !send()
        {
            self.settle(i);
            return false;
        }
        let pending = &mut self.pending[i];
        pending.phase = into;
        if into == Phase::Started {
            pending.due = now + OBSERVE_WAIT;
            pending.probe_due = Some(now + PROBE_AFTER);
            // Keep the hold even if foreign membership resolves checking,
            // and across links: a new reader may queue its read ahead of an
            // old reader that's still authenticating an earlier addEntry.
            let new = self
                .attempted
                .entry(ticket.target.as_str().into())
                .or_default()
                .insert(ticket.track.as_str().into());
            debug_assert!(new && self.attempt_count < MAX_ATTEMPTS);
            self.attempt_count += 1;
        }
        true
    }

    pub(crate) fn cancel(&mut self, ticket: &Ticket) -> bool {
        if let Some(i) = self
            .pending
            .iter()
            .position(|p| p.ticket == *ticket && p.phase != Phase::Started)
        {
            self.settle(i);
            true
        } else {
            false
        }
    }

    fn cancel_unsent(&mut self) {
        self.settle_matching(|p| p.phase != Phase::Started);
    }

    fn settle_matching(&mut self, matching: impl Fn(&Pending) -> bool) {
        let mut i = 0;
        while i < self.pending.len() {
            if matching(&self.pending[i]) {
                self.settle(i);
            } else {
                i += 1;
            }
        }
    }

    fn settle(&mut self, i: usize) {
        let pending = self.pending.remove(i);
        if pending.phase != Phase::Started {
            if self.not_sent.len() >= MAX_LIVE {
                self.not_sent.pop_front();
            }
            self.not_sent.push_back(pending.ticket);
        }
        // Started rows fall back to Unconfirmed from the permanent hold if
        // they are not currently present. No extra outcome to evict or clear.
    }

    pub(crate) fn started_read(&mut self, generation: u64, command: &PersonalCommand) {
        if let PersonalCommand::SyncPlaylist {
            playlist_id,
            known: None,
            offset: 0,
            ..
        } = command
            && self.watched() == Some(playlist_id)
            && self.revalidation.last_link != Some(generation)
        {
            self.revalidation.last_link = Some(generation);
            self.revalidation.links += 1;
        }
    }

    pub(crate) fn validated_read(&mut self, playlist_id: &str) {
        if self.selected() == Some(playlist_id) {
            // Called on a committed part set, never an account summary,
            // stale cache, partial result, or a metadata/entries broadcast.
            self.revalidation = Revalidation::default();
            self.reset_read();
        }
    }

    pub(crate) fn interrupted(&mut self) {
        self.authorized = false;
        self.reset_read();
        self.revalidation.exhausted = self.revalidation.links >= MAX_UNVERIFIED_LINKS;
        self.settle_matching(|_| true);
    }

    pub(crate) fn on_read(
        &mut self,
        issued: Option<&PersonalCommand>,
        complete: bool,
        progressed: bool,
        now: Instant,
    ) {
        if self.watched().is_none() {
            return;
        }
        if complete {
            self.reset_read(); // An optional conditional probe may send no part.
        } else if matches!(issued, Some(PersonalCommand::SyncPlaylist { .. })) {
            let read = self.read.get_or_insert_with(|| Read {
                due: now + READ_IDLE,
                limit: now + READ_LIMIT,
            });
            if progressed {
                read.due = (now + READ_IDLE).min(read.limit);
                self.read_stalled = false;
            }
        }
    }

    fn reset_read(&mut self) {
        self.read = None;
        self.read_stalled = false;
    }

    fn due(&self) -> Option<Instant> {
        let watched = self.watched();
        self.pending
            .iter()
            .flat_map(|p| {
                [
                    Some(p.due),
                    p.probe_due.filter(|_| watched == Some(&p.ticket.target)),
                ]
            })
            .flatten()
            .chain(
                self.read
                    .as_ref()
                    .map(|r| if self.read_stalled { r.limit } else { r.due }),
            )
            .min()
    }

    /// One optional catch-up read for this group of unresolved started intents.
    /// When a multipart repair already owns the scope, it serves the same purpose.
    pub(crate) fn take_probe_due(&mut self, now: Instant) -> bool {
        let Some(watched) = self.watched() else {
            return false;
        };
        if !self
            .pending
            .iter()
            .any(|p| p.ticket.target == watched && p.probe_due.is_some_and(|due| due <= now))
        {
            return false;
        }
        let watched = self.target.as_ref().expect("watched target").id.as_str();
        for p in &mut self.pending {
            if p.ticket.target == watched {
                p.probe_due = None;
            }
        }
        true
    }

    fn expire_pending(&mut self, now: Instant) {
        self.settle_matching(|p| now >= p.due);
    }

    pub(crate) fn retire_expired_read(&mut self, now: Instant) -> bool {
        if self.read.as_ref().is_some_and(|r| now >= r.limit) {
            self.read = None;
            self.read_stalled = true;
            self.cancel_unsent();
            return true;
        }
        false
    }

    /// Return whether the live quick pull must be retired. This never retries a write.
    pub(crate) fn tick(&mut self, now: Instant) -> bool {
        self.expire_pending(now);
        if self.retire_expired_read(now) {
            return true;
        }
        if self.read.as_ref().is_some_and(|r| now >= r.due) {
            // Slow or queued legacy part jobs may still yield valid data.
            // Keep accepting the scoped reply until the whole pull's bound.
            self.read_stalled = true;
            self.cancel_unsent();
        }
        false
    }
}
