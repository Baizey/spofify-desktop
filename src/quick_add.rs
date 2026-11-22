//! Search quick-add intent is not an occurrence or a write receipt. The
//! account socket publishes shared state only, including other clients' adds.
use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};

use tokio::time::Instant;

use crate::{
    api::library::{PlaylistKind, PlaylistSummary},
    personal::{AcceptedPlaylist, PersonalCommand, Replica},
    personal_bridge::{Link, Status},
};

pub(crate) use crate::personal::Membership;

const WRITE_WAIT: Duration = Duration::from_secs(5);
// The legacy wire cannot order or attribute an outstanding add to this
// client across either shared projections or reconnects. Fail closed on
// exhaustion instead of expiring an attempt or evicting its hold.
// 4 MiB of key data at worst (two ≤512-byte IDs per valid command),
// plus bounded map/set storage; repeated targets share one key.
const MAX_ATTEMPTS: usize = 4096;
const READ_IDLE: Duration = Duration::from_secs(6);
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
    pub(crate) notice: Option<Outcome>,
    pub(crate) due: Option<Instant>,
    pub(crate) read_stalled: bool,
    pub(crate) session_limit: bool,
    status: Status,
    selection_version: u64,
    replica: &'a Replica,
    ready: bool,
    busy: bool,
    pending: Option<&'a str>,
    attempted: Option<&'a HashSet<Box<str>>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Row {
    pub(crate) membership: Membership,
    pub(crate) pending: bool,
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
        let pending = self.pending == Some(track_id);
        Row {
            membership,
            pending,
            visible: pending || !self.hide || membership != Membership::Present,
            can_add: membership == Membership::Absent
                && !self.busy
                && !self.session_limit
                && !self
                    .attempted
                    .is_some_and(|tracks| tracks.contains(track_id)),
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
    pending: Option<Pending>,
    notice: Option<(Ticket, Outcome)>,
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
        let pending = self.pending.as_ref();
        View {
            target,
            hide: scoped && self.hide,
            notice: self
                .notice
                .as_ref()
                .filter(|(ticket, _)| target.is_some_and(|p| p.id == ticket.target))
                .map(|(_, outcome)| *outcome),
            due: scoped.then(|| self.due()).flatten(),
            read_stalled: scoped && self.read_unavailable(),
            session_limit: scoped && self.attempt_count >= MAX_ATTEMPTS,
            status,
            selection_version: self.selection_version,
            ready: scoped && self.searching && self.authorized && status.link == Link::Ready,
            busy: pending.is_some(),
            attempted: target.and_then(|t| self.attempted.get(t.id.as_str())),
            pending: pending
                .filter(|p| target.is_some_and(|t| t.id == p.ticket.target))
                .map(|p| p.ticket.track.as_str()),
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
        self.selected()
            .filter(|_| self.searching && !self.read_unavailable())
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
        if self.pending.as_ref().is_some_and(|p| present(&p.ticket)) {
            // Membership stops the spinner, not the dispatch hold. A foreign
            // add could be removed before our own delayed command is applied.
            self.pending = None;
        }
        if self
            .notice
            .as_ref()
            .is_some_and(|(ticket, outcome)| *outcome == Outcome::Unconfirmed && present(ticket))
        {
            self.notice = None;
        }
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
            || self.pending.is_some()
            || self.attempt_count >= MAX_ATTEMPTS
            || self
                .attempted
                .get(expected_target)
                .is_some_and(|tracks| tracks.contains(track))
            || mode_stamp & 1 != 0
        {
            return None;
        }
        self.nonce = self.nonce.wrapping_add(1);
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
        self.notice = None;
        self.pending = Some(Pending {
            ticket: ticket.clone(),
            phase: Phase::Staged,
            due: now + WRITE_WAIT,
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
        if !self
            .pending
            .as_ref()
            .is_some_and(|p| p.ticket == *ticket && p.phase == from)
        {
            return false;
        }
        if !ticket.matches(status)
            || !self.writable(status, replica, &ticket.target, &ticket.track)
            || !send()
        {
            self.settle();
            return false;
        }
        if into == Phase::Started {
            // One pending ticket reserves a slot at staging. Keep this hold
            // even if foreign membership resolves the presentation, and even
            // across links: a new reader may queue its read ahead of an old
            // reader that's still authenticating an earlier addEntry.
            self.attempted
                .entry(ticket.target.as_str().into())
                .or_default()
                .insert(ticket.track.as_str().into());
            self.attempt_count += 1;
        }
        self.pending.as_mut().expect("checked pending").phase = into;
        true
    }

    pub(crate) fn cancel(&mut self, ticket: &Ticket) -> bool {
        if self
            .pending
            .as_ref()
            .is_some_and(|p| p.ticket == *ticket && p.phase != Phase::Started)
        {
            self.settle();
            true
        } else {
            false
        }
    }

    fn cancel_unsent(&mut self) {
        if self
            .pending
            .as_ref()
            .is_some_and(|p| p.phase != Phase::Started)
        {
            self.settle();
        }
    }

    fn settle(&mut self) {
        if let Some(pending) = self.pending.take() {
            let outcome = match pending.phase {
                Phase::Started => Outcome::Unconfirmed,
                Phase::Staged | Phase::Queued => Outcome::NotSent,
            };
            self.notice = Some((pending.ticket, outcome));
        }
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
        }
    }

    pub(crate) fn interrupted(&mut self) {
        self.authorized = false;
        self.reset_read();
        self.revalidation.exhausted = self.revalidation.links >= MAX_UNVERIFIED_LINKS;
        self.settle();
    }

    pub(crate) fn on_read(
        &mut self,
        issued: Option<&PersonalCommand>,
        complete: bool,
        progressed: bool,
        now: Instant,
    ) {
        if !self.searching || self.read_unavailable() {
            return;
        }
        if complete {
            self.read = None; // A matched conditional probe may send no part.
        } else if matches!(issued, Some(PersonalCommand::SyncPlaylist { .. })) {
            let read = self.read.get_or_insert_with(|| Read {
                due: now + READ_IDLE,
                limit: now + READ_LIMIT,
            });
            if progressed {
                read.due = (now + READ_IDLE).min(read.limit);
            }
        }
    }

    fn reset_read(&mut self) {
        self.read = None;
        self.read_stalled = false;
    }

    fn due(&self) -> Option<Instant> {
        self.pending
            .as_ref()
            .map(|p| p.due)
            .into_iter()
            .chain(self.read.as_ref().map(|r| r.due))
            .min()
    }

    fn expire_pending(&mut self, now: Instant) {
        if self.pending.as_ref().is_some_and(|p| now >= p.due) {
            self.settle();
        }
    }

    /// Return whether the live quick pull must be retired. This never retries a write.
    pub(crate) fn tick(&mut self, now: Instant) -> bool {
        self.expire_pending(now);
        if self.read.as_ref().is_some_and(|r| now >= r.due) {
            self.read = None;
            self.read_stalled = true;
            self.cancel_unsent();
            true
        } else {
            false
        }
    }
}
