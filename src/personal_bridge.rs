//! Account/transport generation fence between the engine and the desktop actor.
//! A watch notification is only a wakeup: the bounded reducer applies every
//! socket frame before readers see it, including frames coalesced by the watch.
use crate::{
    api::library::{PlaylistKind, PlaylistSummary, RuleDefinition},
    desk::Access,
    personal::{AcceptedPlaylist, PersonalCommand, PersonalUpdate, Replica},
    personal_cache::{self, Cache, FRESHNESS},
    playlist_pin_store::PUBLICATION_GATE,
    quick_add::{self, BulkEnd, BulkTicket, BulkWrite, QuickAdd, Ticket},
};
use std::{
    collections::{HashMap, VecDeque},
    time::Duration,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::{sync::watch, time::Instant};
#[cfg(test)]
use crate::playlist_observations::Writer;

#[derive(Clone, Debug)]
pub struct Mutation {
    pub command: PersonalCommand,
    pub effect: Effect,
}

#[derive(Clone, Debug)]
pub enum Effect {
    Create {
        name: String,
        description: String,
        kind: PlaylistKind,
        has_track: bool,
    },
    Rename {
        id: String,
        name: String,
        description: String,
        revision: u64,
    },
    Delete {
        id: String,
    },
    Rules {
        id: String,
        definition: RuleDefinition,
        revision: u64,
    },
    Add {
        id: String,
        track_id: String,
        revision: u64,
        visit: bool,
    },
    Remove {
        id: String,
        entry_id: String,
        revision: u64,
    },
    Move {
        id: String,
        entry_id: String,
        after: String,
        revision: u64,
    },
    ClearHistory,
}

#[derive(Clone, Copy, Debug)]
pub struct PersonalGap;
impl std::fmt::Display for PersonalGap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Account library needs a fresh snapshot")
    }
}
impl std::error::Error for PersonalGap {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Link {
    Disconnected,
    Waiting,
    Ready,
    Gap,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Status {
    pub epoch: u64,
    pub generation: u64,
    pub link: Link,
    pub refused: u64,
    pub interruption: u64,
    /// Changes only when a validated playlist part advances the scoped pull.
    pub playlist_progress: u64,
    /// Changes when the watched route or authoritative playlist detail changes.
    pub playlist_detail: u64,
    sequence: u64,
}

#[derive(Default)]
struct Observation {
    sequence: u64,
    playlist: Option<PlaylistSummary>,
    created: bool,
    deleted: Option<String>,
    added_track: Option<String>,
    removed: Option<String>,
    moved: Option<String>,
    after: Option<String>,
    emptied_recent: bool,
}

struct CacheBinding {
    scope: personal_cache::Scope,
    session: crate::api::Session,
    current_epoch: Arc<AtomicU64>,
    mode: Option<u64>,
    writer: personal_cache::Writer,
}

impl CacheBinding {
    fn local(&self, epoch: u64) -> bool {
        self.current_epoch.load(Ordering::SeqCst) == epoch && self.session.can_access_local()
    }

    fn network(&self, epoch: u64) -> bool {
        self.local(epoch) && !self.session.is_expired()
            && self.mode == Some(*self.session.network_changes().borrow()) && !self.session.is_offline()
    }
}

struct Attempt { count: u8, until: Instant }
impl Attempt {
    fn start(&mut self, tick: Instant) {
        self.count = if self.count >= 3 { 1 } else { self.count + 1 };
        self.extend(tick);
    }
    fn extend(&mut self, tick: Instant) {
        self.until = tick + match self.count { 1 => Duration::from_secs(10), 2 => Duration::from_secs(20), _ => FRESHNESS };
    }
}

struct State {
    status: Status,
    detail_attempts: HashMap<String, Attempt>,
    account_attempt: Option<Attempt>,
    cache: Cache,
    cache_binding: Option<CacheBinding>,
    replica: Replica,
    quick: QuickAdd,
    observations: VecDeque<Observation>,
    #[cfg(test)]
    observation_writer: Option<Writer>,
    #[cfg(test)]
    observation_mode: Option<u64>,
}
impl Default for State {
    fn default() -> Self {
        Self {
            status: Status {
                epoch: 0,
                generation: 0,
                link: Link::Disconnected,
                refused: 0,
                interruption: 0,
                playlist_progress: 0,
                playlist_detail: 0,
                sequence: 0,
            },
            cache: Cache::default(),
            cache_binding: None,
            detail_attempts: HashMap::new(),
            account_attempt: None,
            replica: Replica::default(),
            quick: QuickAdd::default(),
            observations: VecDeque::new(),
            #[cfg(test)]
            observation_writer: None,
            #[cfg(test)]
            observation_mode: None,
        }
    }
}
impl State {
    fn scoped(&self, epoch: u64, generation: u64) -> bool {
        self.status.epoch == epoch && self.status.generation == generation
    }

    fn watch(&mut self, id: Option<String>) -> (bool, Option<PersonalCommand>) {
        let before = self.replica.watched_playlist().map(str::to_owned);
        if self.cache_binding.is_some() && !self.quick.searching()
            && let Some(detail) = id.as_ref().and_then(|id| self.cache.details.get(id))
        {
            self.replica.seed_detail(detail.playlist.clone());
        }
        let command = self.replica.watch(id);
        let command = self.prepare_detail(command, personal_cache::now_ms(), Instant::now());
        let changed = before.as_deref() != self.replica.watched_playlist();
        if changed {
            self.status.playlist_detail = self.status.playlist_detail.wrapping_add(1);
        }
        (changed, command)
    }

    fn prepare_detail(&mut self, command: Option<PersonalCommand>, now: u64, tick: Instant) -> Option<PersonalCommand> {
        if self.cache_binding.is_none() || self.quick.searching() { return command; }
        let Some(PersonalCommand::SyncPlaylist { playlist_id, offset: 0, .. }) = &command else { return command; };
        if self.cache.details.get(playlist_id).is_some_and(|d| d.fresh(now, FRESHNESS)) {
            self.replica.defer_detail();
            return None;
        }
        let attempt = self.detail_attempts.entry(playlist_id.clone()).or_insert(Attempt { count: 0, until: tick });
        if tick < attempt.until {
            self.replica.defer_detail();
            return None;
        }
        attempt.start(tick);
        self.replica.read_full_detail()
    }

    fn retire_cache(&mut self) {
        if let Some(binding) = self.cache_binding.take() {
            binding.writer.retire();
        }
        self.cache = Cache::default();
    }

    fn quick_watch(&mut self) -> (bool, Option<PersonalCommand>) {
        let id = self.quick.watched().map(str::to_owned);
        self.watch(id)
    }

    fn quick_read(&mut self, command: Option<&PersonalCommand>, progressed: bool, now: Instant) {
        let complete = self.replica.playlist().is_some_and(|p| !p.stale);
        self.quick.on_read(command, complete, progressed, now);
    }

    fn quick_tick(&mut self, now: Instant) {
        if self.quick.tick(now) {
            self.quick_watch();
        }
    }

    fn expire_quick_read(&mut self, now: Instant) {
        if self.quick.retire_expired_read(now) {
            self.quick_watch();
        }
    }

    fn run_quick_timer(&mut self, now: Instant) -> Option<PersonalCommand> {
        self.quick_tick(now);
        if self.status.link == Link::Ready && self.quick.take_probe_due(now) {
            self.replica.probe_playlist()
        } else {
            None
        }
    }

    fn interrupt(&mut self) {
        self.replica.disconnected();
        self.quick.interrupted();
        if self.quick.searching() {
            self.quick_watch();
        }
    }

    fn accepts_playlist_sync(
        &self,
        epoch: u64,
        generation: u64,
        command: &PersonalCommand,
    ) -> bool {
        self.scoped(epoch, generation)
            && self.status.link == Link::Ready
            && self.replica.accepts_sync(command)
    }

    fn quick_candidate(&self, id: &str) -> Option<&PlaylistSummary> {
        if self.status.link != Link::Ready {
            return None;
        }
        self.replica
            .snapshot()?
            .playlists
            .iter()
            .find(|p| p.id == id)
            .or_else(|| {
                self.replica
                    .playlist()
                    .filter(|p| !p.stale && p.summary.id == id)
                    .map(|p| &p.summary)
            })
            .or_else(|| self.quick.authorized_target().filter(|p| p.id == id))
            .filter(|p| quick_add::editable(p))
    }
}

#[derive(Clone)]
pub struct PersonalAccess {
    state: Arc<Mutex<State>>,
    changed: watch::Sender<u64>,
    // The installation's existing output identity, not an account capability.
    bulk_device: Arc<Mutex<Option<BulkDevice>>>,
}

#[derive(Clone)]
pub(crate) struct BulkDevice {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) link: watch::Receiver<crate::socket::Link>,
    pub(crate) order: crate::socket::BulkOrder,
}

impl Default for PersonalAccess {
    fn default() -> Self {
        Self {
            state: Arc::default(),
            changed: watch::channel(0).0,
            bulk_device: Arc::default(),
        }
    }
}
impl PersonalAccess {
    #[cfg(test)]
    pub(crate) fn playlist_observations(&self, access: &Access) -> Option<Writer> {
        let state = self.state.lock().expect("personal state");
        let writer = state.observation_writer.as_ref()?;
        (state.status.epoch == access.epoch && writer.matches(access)).then(|| writer.clone())
    }

    pub(crate) fn account_fence(&self, epoch: u64) -> Option<(Arc<AtomicU64>, u64)> {
        let state = self.state.lock().expect("personal state");
        let binding = state.cache_binding.as_ref()?;
        (state.status.epoch == epoch).then(|| (binding.current_epoch.clone(), epoch))
    }

    pub(crate) fn cached_page(&self, epoch: u64, route: &crate::pages::Route) -> Option<crate::pages::Page> {
        let state = self.state.lock().expect("personal state");
        if state.status.epoch != epoch || !state.cache_binding.as_ref()?.local(epoch) {
            return None;
        }
        state.cache.projection(route)
    }

    pub(crate) async fn save_playlist_details(&self, epoch: u64, id: &str) -> bool {
        let writer = {
            let state = self.state.lock().expect("personal state");
            let Some(binding) = state.cache_binding.as_ref().filter(|b| b.local(epoch)) else { return false; };
            if state.status.epoch != epoch || !state.cache.details.contains_key(id) { return false; }
            binding.writer.clone()
        };
        tokio::time::timeout(Duration::from_secs(3), writer.flush()).await.unwrap_or(false)
    }

    pub(crate) fn changes(&self) -> watch::Receiver<u64> {
        self.changed.subscribe()
    }
    pub(crate) fn read<R>(&self, read: impl FnOnce(Status, &Replica) -> R) -> R {
        let state = self.state.lock().expect("personal state");
        read(state.status, &state.replica)
    }
    pub(crate) fn watch_playlist(
        &self,
        epoch: u64,
        id: Option<String>,
    ) -> Option<(u64, PersonalCommand)> {
        let mut state = self.state.lock().expect("personal state");
        if state.status.epoch != epoch {
            return None;
        }
        let (changed, command) = state.watch(id);
        if changed {
            self.signal();
        }
        if state.status.link != Link::Ready {
            return None;
        }
        Some((state.status.generation, command?))
    }

    /// A single owner of the scoped pull: Search temporarily watches the quick
    /// target, while the actual detail route owns it everywhere else.
    pub(crate) fn quick_watch(
        &self,
        epoch: u64,
        searching: bool,
        detail: Option<String>,
    ) -> Option<(u64, PersonalCommand)> {
        let mut state = self.state.lock().expect("personal state");
        if state.status.epoch != epoch {
            return None;
        }
        let mode_changed = state.quick.searching() != searching;
        state.quick.set_searching(searching);
        let (watch_changed, command) = if searching {
            state.quick_watch()
        } else {
            state.watch(detail)
        };
        if searching && mode_changed && command.is_none() {
            // Detail -> Search can inherit the exact in-progress pull.
            let inherited = state.replica.pending_sync().cloned();
            state.quick_read(inherited.as_ref(), false, Instant::now());
        } else {
            state.quick_read(command.as_ref(), false, Instant::now());
        }
        if mode_changed || watch_changed {
            self.signal();
        }
        Some((state.status.generation, command?))
    }

    pub(crate) fn quick_targets(&self, epoch: u64) -> Vec<PlaylistSummary> {
        let state = self.state.lock().expect("personal state");
        if state.status.epoch != epoch || state.status.link != Link::Ready {
            return Vec::new();
        }
        let mut targets: Vec<_> = state
            .replica
            .snapshot()
            .into_iter()
            .flat_map(|s| &s.playlists)
            .filter(|p| quick_add::editable(p))
            .cloned()
            .collect();
        for id in [state.replica.watched_playlist(), state.quick.selected()]
            .into_iter()
            .flatten()
        {
            if !targets.iter().any(|p| p.id == id)
                && let Some(p) = state.quick_candidate(id)
            {
                targets.push(p.clone());
            }
        }
        targets
    }

    pub(crate) fn quick_select(
        &self,
        epoch: u64,
        id: Option<&str>,
    ) -> Option<(u64, PersonalCommand)> {
        let mut state = self.state.lock().expect("personal state");
        if state.status.epoch != epoch || state.quick.selected() == id {
            return None;
        }
        let target = match id {
            Some(id) => Some(state.quick_candidate(id)?.clone()),
            None => None,
        };
        state.quick.select(target);
        let command = if state.quick.searching() {
            state.quick_watch().1
        } else {
            None
        };
        state.quick_read(command.as_ref(), false, Instant::now());
        self.signal();
        Some((state.status.generation, command?))
    }

    pub(crate) fn quick_view<R>(
        &self,
        epoch: u64,
        read: impl FnOnce(quick_add::View<'_>) -> R,
    ) -> R {
        let state = self.state.lock().expect("personal state");
        read(
            state
                .quick
                .view(state.status.epoch == epoch, &state.replica, state.status),
        )
    }

    pub(crate) fn quick_bulk_device(&self) -> Option<BulkDevice> {
        self.bulk_device
            .lock()
            .expect("bulk device identity")
            .clone()
    }

    pub(crate) fn quick_skip(&self, epoch: u64, skip: bool) {
        let mut state = self.state.lock().expect("personal state");
        if state.status.epoch == epoch {
            state.quick.bulk_skip(skip);
            self.signal();
        }
    }

    pub(crate) fn quick_abort_bulk(&self, epoch: u64) {
        let mut state = self.state.lock().expect("personal state");
        if state.status.epoch == epoch && state.quick.abort_bulk(BulkEnd::Cancelled) {
            self.signal();
        }
    }

    pub(crate) fn quick_abort_bulk_scoped(&self, epoch: u64, mode: u64, scope: &str) {
        let mut state = self.state.lock().expect("personal state");
        if state.status.epoch == epoch
            && state
                .quick
                .view(true, &state.replica, state.status)
                .bulk_scope(mode)
                == scope
            && state.quick.abort_bulk(BulkEnd::Cancelled)
        {
            self.signal();
        }
    }

    pub(crate) fn quick_begin_bulk(
        &self,
        epoch: u64,
        mode: u64,
        scope: &str,
        target: &str,
        kind: &str,
        source: &str,
    ) -> Option<BulkTicket> {
        let mut state = self.state.lock().expect("personal state");
        let State {
            quick,
            replica,
            status,
            ..
        } = &mut *state;
        if status.epoch != epoch || quick.view(true, replica, *status).bulk_scope(mode) != scope {
            return None;
        }
        let ticket = quick.begin_bulk(*status, replica, mode, target, kind, source)?;
        self.signal();
        Some(ticket)
    }

    pub(crate) fn quick_bulk_current(&self, ticket: &BulkTicket) -> bool {
        let state = self.state.lock().expect("personal state");
        state.quick.bulk_current(state.status, ticket)
    }

    pub(crate) fn quick_bulk_revision(&self, ticket: &BulkTicket) -> Option<u64> {
        let state = self.state.lock().expect("personal state");
        state
            .quick
            .bulk_revision(state.status, &state.replica, ticket)
    }

    #[cfg(test)]
    pub(crate) fn quick_bulk_send(
        &self,
        ticket: &BulkTicket,
        track: &str,
        start: impl FnOnce() -> bool,
    ) -> BulkWrite {
        self.quick_bulk_send_ordered(ticket, track, None, &AtomicU64::new(ticket.epoch), start)
    }

    pub(crate) fn quick_bulk_send_ordered(
        &self,
        ticket: &BulkTicket,
        track: &str,
        primary: Option<&BulkDevice>,
        current_epoch: &AtomicU64,
        start: impl FnOnce() -> bool,
    ) -> BulkWrite {
        // Actual writes call this while holding PUBLICATION_GATE, just as
        // individual quick adds do. Do not wait for Engine to retire Access.
        if current_epoch.load(Ordering::SeqCst) != ticket.epoch {
            return BulkWrite::Stop;
        }
        let mut state = self.state.lock().expect("personal state");
        let State {
            quick,
            replica,
            status,
            ..
        } = &mut *state;
        let outcome = if let Some(primary) = primary {
            // Follow the ordinary personal order-admission lock ordering.
            // The actual write guard additionally owns PUBLICATION_GATE.
            primary
                .order
                .when_applied(|| {
                    let link = primary.link.borrow();
                    if *link != crate::socket::Link::Connected(ticket.generation) {
                        return BulkWrite::Stop;
                    }
                    quick.bulk_start(*status, replica, ticket, track, start)
                })
                .unwrap_or(BulkWrite::Wait)
        } else {
            quick.bulk_start(*status, replica, ticket, track, start)
        };
        if outcome == BulkWrite::Started {
            self.signal();
        }
        outcome
    }

    pub(crate) fn quick_bulk_ack(&self, ticket: &BulkTicket, revision: u64) -> bool {
        let mut state = self.state.lock().expect("personal state");
        let ok = state.quick.bulk_ack(ticket, revision);
        if ok {
            self.signal();
        }
        ok
    }

    pub(crate) fn quick_bulk_checkpoint(&self, ticket: &BulkTicket) {
        let mut state = self.state.lock().expect("personal state");
        let State {
            quick,
            replica,
            status,
            ..
        } = &mut *state;
        quick.bulk_checkpoint(*status, replica, ticket);
        self.signal();
    }

    pub(crate) fn quick_finish_bulk(&self, ticket: &BulkTicket, end: BulkEnd) {
        let mut state = self.state.lock().expect("personal state");
        state.quick.finish_bulk(ticket, end);
        self.signal();
    }

    #[cfg(test)]
    pub(crate) fn quick_stage(
        &self,
        epoch: u64,
        mode_stamp: u64,
        expected_target: &str,
        track_id: &str,
    ) -> Option<Ticket> {
        let scope = self.quick_view(epoch, |view| view.scope(mode_stamp));
        self.quick_stage_scoped(epoch, mode_stamp, &scope, expected_target, track_id)
    }

    pub(crate) fn quick_stage_scoped(
        &self,
        epoch: u64,
        mode_stamp: u64,
        expected_scope: &str,
        expected_target: &str,
        track_id: &str,
    ) -> Option<Ticket> {
        let mut state = self.state.lock().expect("personal state");
        let State {
            quick,
            replica,
            status,
            ..
        } = &mut *state;
        if status.epoch != epoch
            || quick.view(true, replica, *status).scope(mode_stamp) != expected_scope
        {
            return None;
        }
        let ticket = quick.stage(
            *status,
            replica,
            mode_stamp,
            expected_target,
            track_id,
            Instant::now(),
        );
        self.signal();
        ticket
    }

    pub(crate) fn quick_cancel(&self, ticket: &Ticket) {
        let mut state = self.state.lock().expect("personal state");
        if state.quick.cancel(ticket) {
            self.signal();
        }
    }

    pub(crate) fn quick_tick(&self, epoch: u64) -> Option<(u64, PersonalCommand)> {
        let mut state = self.state.lock().expect("personal state");
        if state.status.epoch != epoch {
            return None;
        }
        let command = state.run_quick_timer(Instant::now());
        self.signal();
        Some((state.status.generation, command?))
    }

    pub(crate) fn inspect_playlist_sync<R>(
        &self,
        epoch: u64,
        generation: u64,
        command: &PersonalCommand,
        check: impl FnOnce(bool) -> R,
    ) -> Option<R> {
        let state = self.state.lock().expect("personal state");
        state
            .accepts_playlist_sync(epoch, generation, command)
            .then(|| check(state.quick.searching() || state.replica.is_catchup_sync()))
    }

    pub(crate) fn start_playlist_sync(
        &self,
        epoch: u64,
        generation: u64,
        command: &PersonalCommand,
        start: impl FnOnce() -> bool,
    ) -> bool {
        let mut state = self.state.lock().expect("personal state");
        if !state.accepts_playlist_sync(epoch, generation, command) || !start() {
            return false;
        }
        if state.replica.playlist().is_none_or(|p| p.stale) {
            state.quick.started_read(generation, command);
        }
        true
    }

    // Socket writer calls this immediately before committing the local send.
    // No async wait inside this guard; a successful start is still uncertain.
    pub(crate) fn start_quick_add(&self, ticket: &Ticket, start: impl FnOnce() -> bool) -> bool {
        let mut state = self.state.lock().expect("personal state");
        let State {
            quick,
            replica,
            status,
            ..
        } = &mut *state;
        let started = quick.start(*status, replica, ticket, Instant::now(), start);
        self.signal();
        started
    }

    pub(crate) fn refresh_playlist(&self, epoch: u64) -> Option<(u64, PersonalCommand)> {
        let mut state = self.state.lock().expect("personal state");
        if state.status.epoch != epoch || state.status.link != Link::Ready {
            return None;
        }
        Some((state.status.generation, state.replica.refresh_playlist()?))
    }

    pub(crate) fn browse_view(&self, epoch: u64, route: &crate::pages::Route, pending: Option<(Status, &Effect)>) -> BrowseView {
        let state = self.state.lock().expect("personal state");
        let current = state.status.epoch == epoch;
        let live_page = current.then(|| crate::pages::personal_projection(route, &state.replica)).flatten();
        let cached_page = state.cache_binding.as_ref().filter(|b| current && b.local(epoch))
            .and_then(|_| state.cache.projection(route));
        let detail_live = state.replica.playlist().is_some_and(|p| !p.stale);
        let confirmed = pending.and_then(|(before, effect)| {
            if matches!(effect, Effect::Move { .. } | Effect::Remove { .. }) && !detail_live { return None; }
            Self::observed_in(&state, before, effect)
        });
        BrowseView {
            status: state.status, live: live_page.is_some(), detail_live,
            page: cached_page.or(live_page), confirmed,
            unavailable: matches!(route, crate::pages::Route::Playlist(_)) && state.replica.playlist_unavailable(),
        }
    }

    #[cfg(test)]
    pub(crate) fn observed(&self, before: Status, effect: &Effect) -> Option<Option<crate::pages::Route>> {
        Self::observed_in(&self.state.lock().expect("personal state"), before, effect)
    }

    /// Return evidence of matching server state, never a command receipt. A
    /// concurrent client can produce the same change; do not attribute it here.
    fn observed_in(state: &State, before: Status, effect: &Effect) -> Option<Option<crate::pages::Route>> {
        use crate::pages::Route;
        if !state.scoped(before.epoch, before.generation)
            || state.status.link != Link::Ready
            || state.status.refused != before.refused
            || state.status.interruption != before.interruption
        {
            return None;
        }
        let changes = state
            .observations
            .iter()
            .filter(|change| change.sequence > before.sequence);
        for change in changes {
            let p = change.playlist.as_ref();
            let matches_id =
                |id: &str, revision: u64| p.is_some_and(|p| p.id == id && p.revision > revision);
            match effect {
                Effect::Create {
                    name,
                    description,
                    kind,
                    has_track,
                } if change.created => {
                    if let Some(p) = p.filter(|p| {
                        p.name == *name
                            && p.description == *description
                            && p.kind == *kind
                            && (!has_track || p.track_count == Some(1))
                    }) {
                        return Some(Some(Route::Playlist(p.id.clone())));
                    }
                }
                Effect::Rename {
                    id,
                    name,
                    description,
                    revision,
                } if matches_id(id, *revision)
                    && p.is_some_and(|p| p.name == *name && p.description == *description) =>
                {
                    return Some(None);
                }
                Effect::Delete { id } if change.deleted.as_ref() == Some(id) => {
                    return Some(Some(Route::Playlists));
                }
                Effect::Rules {
                    id,
                    definition,
                    revision,
                } if matches_id(id, *revision) => {
                    if state.replica.playlist().is_some_and(|p| {
                        p.summary.id == *id
                            && p.summary.revision > *revision
                            && !p.stale
                            && p.definition.as_ref() == Some(definition)
                    }) {
                        return Some(None);
                    }
                }
                Effect::Add {
                    id,
                    track_id,
                    revision,
                    visit,
                } if matches_id(id, *revision) && change.added_track.as_ref() == Some(track_id) => {
                    return Some(visit.then(|| Route::Playlist(id.clone())));
                }
                Effect::Remove {
                    id,
                    entry_id,
                    revision,
                } if matches_id(id, *revision) && change.removed.as_ref() == Some(entry_id) => {
                    return Some(None);
                }
                Effect::Move {
                    id,
                    entry_id,
                    after,
                    revision,
                } if matches_id(id, *revision)
                    && change.moved.as_ref() == Some(entry_id)
                    && change.after.as_deref().unwrap_or_default() == after =>
                {
                    return Some(None);
                }
                Effect::ClearHistory if change.emptied_recent => return Some(None),
                _ => {}
            }
        }
        None
    }
    fn signal(&self) {
        self.changed.send_modify(|n| *n = n.wrapping_add(1));
    }
}

pub(crate) struct BrowseView {
    pub status: Status,
    pub page: Option<crate::pages::Page>,
    pub live: bool,
    pub detail_live: bool,
    pub unavailable: bool,
    pub confirmed: Option<Option<crate::pages::Route>>,
}

#[derive(Default)]
pub struct PersonalHub(PersonalAccess);
impl PersonalHub {
    pub(crate) fn set_bulk_device(&self, identity: BulkDevice) {
        *self.0.bulk_device.lock().expect("bulk device identity") = Some(identity);
    }

    pub fn access(&self) -> PersonalAccess {
        self.0.clone()
    }
    /// Load last-good disk projections before publishing Access or opening the
    /// account transport. A successful reconnect can rebind the same scope.
    pub(crate) fn bind_cache(&self, access: Option<&Access>, current_epoch: &Arc<AtomicU64>) {
        if let Some(access) = access {
            self.reset(access.epoch);
        }
        let admitted = access.zip(access.and_then(personal_cache::Scope::new));
        {
            let _publication = PUBLICATION_GATE.lock().unwrap();
            let mut state = self.0.state.lock().expect("personal state");
            if let Some((access, scope)) = &admitted {
                if access.epoch != state.status.epoch || access.epoch != current_epoch.load(Ordering::SeqCst) { return; }
                if let Some(binding) = state.cache_binding.as_mut().filter(|b| b.scope == *scope) {
                    binding.session = access.session.clone();
                    binding.writer.rebind(access.session.clone());
                    return;
                }
            }
            state.retire_cache();
        }
        let Some((access, scope)) = admitted else { return; };
        let cache = scope.load(); // No publication or reader lock across disk I/O.
        let _publication = PUBLICATION_GATE.lock().unwrap();
        let mut state = self.0.state.lock().expect("personal state");
        if access.epoch != state.status.epoch || access.epoch != current_epoch.load(Ordering::SeqCst)
            || !access.session.can_access_local() || state.cache_binding.is_some()
        { return; }
        state.cache = cache;
        state.cache_binding = Some(CacheBinding {
            writer: personal_cache::Writer::start(scope.clone(), access.epoch, current_epoch.clone(), access.session.clone()),
            scope, session: access.session.clone(), current_epoch: current_epoch.clone(), mode: None,
        });
    }

    pub(crate) fn record_local(&self, epoch: u64, mut event: crate::api::library::HistoryEntry) {
        let _publication = PUBLICATION_GATE.lock().unwrap();
        let mut state = self.0.state.lock().expect("personal state");
        if state.status.epoch != epoch || !state.cache_binding.as_ref().is_some_and(|b| b.local(epoch)) {
            return;
        }
        // Prefer complete catalog metadata where a saved projection has it.
        if let Some(track) = state.cache.details.values().flat_map(|d| d.playlist.rows.iter())
            .find(|row| row.track.id == event.track.id).map(|row| row.track.clone())
        {
            event.track = track;
        }
        if state.cache.record(event) {
            state.cache_binding.as_ref().unwrap().writer.publish(&state.cache);
            self.0.signal();
        }
    }

    pub(crate) async fn flush_cache(&self) {
        let writer = self.0.state.lock().expect("personal state").cache_binding.as_ref().map(|b| b.writer.clone());
        if let Some(writer) = writer && !writer.flush().await {
            eprintln!("The latest account library changes were not saved");
        }
    }

    #[cfg(test)]
    pub(crate) fn bind_observations(
        &self,
        access: Option<&Access>,
        current_epoch: &Arc<AtomicU64>,
    ) {
        if let Some(access) = access {
            self.reset(access.epoch);
        }
        let mut state = self.0.state.lock().expect("personal state");
        if let Some(access) = access.filter(|a| a.epoch == state.status.epoch) {
            if !state
                .observation_writer
                .as_ref()
                .is_some_and(|w| w.matches(access))
            {
                state.observation_mode = None;
                state.observation_writer = Writer::start(access, current_epoch.clone()).ok();
            }
        } else if access.is_none() {
            state.observation_mode = None;
            state.observation_writer = None;
        }
    }

    #[cfg(test)]
    pub(crate) async fn flush_observations(&self) {
        let writer = self
            .0
            .state
            .lock()
            .expect("personal state")
            .observation_writer
            .clone();
        if let Some(writer) = writer
            && let Err(err) = writer.flush().await
        {
            eprintln!("{err}");
        }
    }

    #[cfg(test)]
    pub(crate) fn set_observation_writer(&self, epoch: u64, writer: Writer) {
        self.reset(epoch);
        self.0.state.lock().unwrap().observation_writer = Some(writer);
    }

    pub fn reset(&self, epoch: u64) {
        let _publication = PUBLICATION_GATE.lock().unwrap();
        let mut state = self.0.state.lock().expect("personal state");
        if epoch > state.status.epoch {
            state.retire_cache();
            *state = State::default();
            state.status.epoch = epoch;
            self.0.signal();
        }
    }
    pub fn begin(&self, epoch: u64, generation: u64) {
        self.reset(epoch);
        let mut state = self.0.state.lock().expect("personal state");
        if state.status.epoch != epoch || generation < state.status.generation {
            return;
        }
        if state.status.generation == generation && state.status.link != Link::Disconnected {
            return;
        }
        state.status.generation = generation;
        state.status.link = Link::Waiting;
        state.detail_attempts.clear();
        state.account_attempt = None;
        if let Some(binding) = state.cache_binding.as_mut() {
            binding.mode = Some(*binding.session.network_changes().borrow());
        }
        #[cfg(test)]
        {
            state.observation_mode = state.observation_writer.as_ref().and_then(Writer::online_mode);
        }
        state.interrupt();
        self.0.signal();
    }
    pub fn disconnect(&self, epoch: u64, generation: u64) {
        self.pause(epoch, generation, Link::Disconnected);
    }
    pub fn gap(&self, epoch: u64, generation: u64) {
        self.pause(epoch, generation, Link::Gap);
    }
    fn pause(&self, epoch: u64, generation: u64, link: Link) {
        let mut state = self.0.state.lock().expect("personal state");
        if state.scoped(epoch, generation)
            && state.status.link != link
            && !(state.status.link == Link::Disconnected && link == Link::Gap)
        {
            state.status.link = link;
            state.status.interruption = state.status.interruption.wrapping_add(1);
            state.interrupt();
            state.observations.clear();
            self.0.signal();
        }
    }
    pub fn receive(
        &self,
        epoch: u64,
        generation: u64,
        update: PersonalUpdate,
    ) -> Result<Option<PersonalCommand>, PersonalGap> {
        // Same order as socket admission: publication → reducer. Only
        // accepted projections reach the disk worker.
        let _publication = PUBLICATION_GATE.lock().unwrap();
        let mut state = self.0.state.lock().expect("personal state");
        if !state.scoped(epoch, generation) || state.status.link == Link::Disconnected
            || state.cache_binding.as_ref().is_some_and(|b| !b.network(epoch))
        {
            return Ok(None);
        }
        let snapshot = matches!(&update, PersonalUpdate::Snapshot(_));
        let part = matches!(&update, PersonalUpdate::Part(_));
        let now = Instant::now();
        state.expire_quick_read(now);
        if snapshot && state.status.link != Link::Ready && state.quick.searching() {
            // Reappraise a direct target only while its selection has a
            // revalidation budget. A capped snapshot can't clear that budget.
            if state.replica.playlist_unavailable() {
                state.watch(None);
            }
            state.quick_watch();
        }
        if !snapshot && state.status.link != Link::Ready {
            // A repair snapshot may be queued behind older in-flight updates.
            if state.status.link == Link::Gap {
                return Ok(None);
            }
            state.status.link = Link::Gap;
            state.status.interruption = state.status.interruption.wrapping_add(1);
            state.interrupt();
            self.0.signal();
            return Err(PersonalGap);
        }
        let final_part = match &update {
            PersonalUpdate::Part(part) if !part.more => {
                Some((part.summary.id.clone(), part.token.clone()))
            }
            _ => None,
        };
        let observation = if let PersonalUpdate::Change(change) = &update {
            let current = state.replica.snapshot();
            let entries = change.entries.as_ref();
            Some(Observation {
                sequence: state.status.sequence.wrapping_add(1),
                playlist: change.playlist.clone(),
                created: change.playlist.as_ref().is_some_and(|p| {
                    current.is_some_and(|s| !s.playlists.iter().any(|old| old.id == p.id))
                }),
                deleted: change.deleted.clone(),
                added_track: entries
                    .and_then(|e| e.added.as_ref())
                    .map(|r| r.track.id.clone()),
                removed: entries.and_then(|e| e.removed.clone()),
                moved: entries.and_then(|e| e.moved.clone()),
                after: entries.and_then(|e| e.after.clone()),
                emptied_recent: change
                    .recent
                    .as_ref()
                    .is_some_and(|e| !e.removed.is_empty() && e.inserted.is_empty())
                    && current.is_some_and(|s| {
                        change
                            .recent
                            .as_ref()
                            .is_some_and(|e| s.recent.len() == e.removed.len())
                    }),
            })
        } else {
            None
        };
        #[cfg(test)]
        let writer = state.observation_writer.clone().zip(state.observation_mode);
        if state.cache_binding.is_some() && let PersonalUpdate::Change(change) = &update
            && let Some(detail) = change.playlist.as_ref().and_then(|p| state.cache.details.get(&p.id))
        {
            let playlist = detail.playlist.clone();
            state.replica.seed_detail(playlist);
        }
        let at = personal_cache::now_ms();
        let State { replica, quick, cache, .. } = &mut *state;
        let mut cache_changed = false;
        let received = replica.receive_observed(update, |event| {
            cache.accept(event, part.then_some(at));
            cache_changed = true;
            #[cfg(test)]
            if let Some((writer, mode)) = &writer {
                writer.observe(*mode, event);
            }
            quick.observe(event);
            if part && let AcceptedPlaylist::Complete(playlist) = event {
                quick.validated_read(&playlist.summary.id);
            }
        });
        let result = match received {
            Ok(mut command) => {
                quick.observe_scoped(replica);
                if snapshot {
                    state.status.link = Link::Ready;
                    state.cache.snapshot_at = at;
                    state.account_attempt = None;
                    if state.cache_binding.is_some() && !state.quick.searching() {
                        let playlist = state.replica.watched_playlist().and_then(|id| state.cache.details.get(id)).map(|detail| detail.playlist.clone());
                        if let Some(playlist) = playlist {
                            state.replica.seed_detail(playlist);
                        }
                    }
                    command = state.prepare_detail(command, at, now);
                }
                if (cache_changed || observation.is_some()) && let Some(snapshot) = state.replica.snapshot() {
                    state.cache.snapshot = Some(Arc::new(snapshot.clone()));
                    if let Some(binding) = &state.cache_binding {
                        binding.writer.publish(&state.cache);
                    }
                }
                if state.quick.searching()
                    && state.quick.watched() != state.replica.watched_playlist()
                {
                    command = state.quick_watch().1;
                }
                let progressed = matches!(command, Some(PersonalCommand::SyncPlaylist { offset, .. }) if offset > 0);
                // A valid part at the deadline is evidence, not a timeout.
                // Reduce it before ticking the optional Search read budget.
                state.quick_read(command.as_ref(), progressed, now);
                state.quick_tick(now);
                if let Some(candidate) = &command
                    && !state.replica.accepts_sync(candidate)
                {
                    command = None;
                }
                if progressed {
                    state.status.playlist_progress = state.status.playlist_progress.wrapping_add(1);
                    if let Some(PersonalCommand::SyncPlaylist { playlist_id, .. }) = &command
                        && let Some(attempt) = state.detail_attempts.get_mut(playlist_id)
                    {
                        attempt.extend(now);
                    }
                }
                if let Some((id, token)) = &final_part
                    && state.replica.playlist().is_some_and(|p| !p.stale && p.summary.id == *id && p.token == *token)
                {
                    state.detail_attempts.remove(id);
                }
                let watched_changed = state.replica.watched_playlist().is_some_and(|id| {
                    observation.as_ref().is_some_and(|item| {
                        item.deleted.as_deref() == Some(id)
                            || item.playlist.as_ref().is_some_and(|p| p.id == id)
                    })
                });
                if snapshot
                    || watched_changed
                    || final_part.is_some_and(|(id, token)| {
                        state
                            .replica
                            .playlist()
                            .is_some_and(|p| !p.stale && p.summary.id == id && p.token == token)
                    })
                {
                    state.status.playlist_detail = state.status.playlist_detail.wrapping_add(1);
                }
                if let Some(observation) = observation {
                    state.status.sequence = observation.sequence;
                    if state.observations.len() >= 32 {
                        state.observations.pop_front();
                    }
                    state.observations.push_back(observation);
                }
                Ok(command)
            }
            Err(_) => {
                state.status.link = Link::Gap;
                state.status.interruption = state.status.interruption.wrapping_add(1);
                state.interrupt();
                state.observations.clear();
                Err(PersonalGap)
            }
        };
        self.0.signal();
        result
    }
    // v3 refreshes the list and recent history in one ordered snapshot.
    // Admission is shared by all consumers, including foreground and routes.
    pub(crate) fn account_refresh_due(&self, epoch: u64, generation: u64) -> bool {
        let mut state = self.0.state.lock().expect("personal state");
        if !state.scoped(epoch, generation) || state.status.link != Link::Ready
            || !state.cache_binding.as_ref().is_some_and(|b| b.network(epoch))
            || state.cache.snapshot_fresh(personal_cache::now_ms(), FRESHNESS)
        { return false; }
        let tick = Instant::now();
        let attempt = state.account_attempt.get_or_insert(Attempt { count: 0, until: tick });
        if tick < attempt.until { return false; }
        attempt.start(tick);
        true
    }

    pub(crate) fn refresh_due(&self, epoch: u64, generation: u64) -> Option<PersonalCommand> {
        let mut state = self.0.state.lock().expect("personal state");
        if !state.scoped(epoch, generation) || state.status.link != Link::Ready || state.quick.searching()
            || !state.cache_binding.as_ref().is_some_and(|b| b.network(epoch))
        { return None; }
        let id = state.replica.watched_playlist()?.to_owned();
        let now = personal_cache::now_ms();
        let tick = Instant::now();
        if state.cache.details.get(&id).is_some_and(|d| d.fresh(now, FRESHNESS))
            || state.detail_attempts.get(&id).is_some_and(|attempt| tick < attempt.until)
        { return None; }
        let command = state.replica.read_full_detail()?;
        state.prepare_detail(Some(command), now, tick)
    }

    pub fn refused(&self, epoch: u64, generation: u64) {
        let mut state = self.0.state.lock().expect("personal state");
        if state.scoped(epoch, generation) {
            state.status.refused = state.status.refused.wrapping_add(1);
            self.0.signal();
        }
    }
    // An obsolete read is not a mutation refusal. Atomically check the
    // current scoped request at queue admission, and use the quick read's
    // own deadline for a local refusal instead of poisoning normal writes.
    pub(crate) fn send_playlist_sync(
        &self,
        epoch: u64,
        generation: u64,
        command: &PersonalCommand,
        send: impl FnOnce() -> bool,
    ) {
        let mut state = self.0.state.lock().expect("personal state");
        if !state.accepts_playlist_sync(epoch, generation, command) {
            return;
        }
        if !send() && !state.quick.searching() && !state.replica.is_catchup_sync() {
            state.status.refused = state.status.refused.wrapping_add(1);
            self.0.signal();
        }
    }

    pub(crate) fn admit_quick_add(&self, ticket: &Ticket, send: impl FnOnce() -> bool) -> bool {
        let mut state = self.0.state.lock().expect("personal state");
        let State {
            quick,
            replica,
            status,
            ..
        } = &mut *state;
        let admitted = quick.admit(*status, replica, ticket, Instant::now(), send);
        self.0.signal();
        admitted
    }

    // Validate the exact observed detail and occurrence pair all the way to
    // transport admission. The live wire has no expected revision field.
    pub(crate) fn admit_move(
        &self,
        observed: Status,
        command: &PersonalCommand,
        send: impl FnOnce() -> bool,
    ) -> bool {
        let state = self.0.state.lock().expect("personal state");
        let PersonalCommand::MoveEntry {
            playlist_id,
            entry_id,
            after,
        } = command
        else {
            return false;
        };
        if state.status != observed || observed.link != Link::Ready || !command.valid() {
            return false;
        }
        let Some(playlist) = state.replica.playlist().filter(|p| {
            !p.stale
                && p.summary.id == *playlist_id
                && p.summary.kind == PlaylistKind::Manual
                && p.summary.capabilities.edit_entries
        }) else {
            return false;
        };
        let Some(index) = playlist.rows.iter().position(|row| row.id == *entry_id) else {
            return false;
        };
        let original = index
            .checked_sub(1)
            .map(|i| playlist.rows[i].id.as_str())
            .unwrap_or_default();
        if after == original
            || after == entry_id
            || !after.is_empty() && !playlist.rows.iter().any(|row| row.id == *after)
        {
            return false;
        }
        send()
    }

    pub fn authorizes(&self, epoch: u64, generation: u64, command: &PersonalCommand) -> bool {
        let state = self.0.state.lock().expect("personal state");
        state.scoped(epoch, generation)
            && state.status.link == Link::Ready
            && if matches!(command, PersonalCommand::SyncPlaylist { .. }) {
                state.replica.accepts_sync(command)
            } else {
                state.replica.command(command.clone()).is_some()
            }
    }
}

#[cfg(test)]
#[path = "personal_quick_add_tests.rs"]
mod quick_add_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn empty() -> PersonalUpdate {
        serde_json::from_value(json!({"snapshot":{"playlists":[],"recent":[]}})).unwrap()
    }

    #[test]
    fn only_the_active_epoch_and_socket_generation_can_commit_or_send() {
        let hub = PersonalHub::default();
        let command = PersonalCommand::ClearHistory;
        let access = hub.access();
        hub.reset(7);
        hub.begin(7, 11);
        assert!(!hub.authorizes(7, 11, &command));
        hub.receive(7, 10, empty()).unwrap();
        assert!(access.read(|_, r| r.snapshot().is_none()));
        hub.receive(7, 11, empty()).unwrap();
        assert!(hub.authorizes(7, 11, &command));
        hub.disconnect(7, 11);
        assert!(!hub.authorizes(7, 11, &command));
        assert!(access.read(|_, r| r.snapshot().is_some()));
        hub.begin(7, 12);
        hub.receive(7, 11, empty()).unwrap();
        assert!(!hub.authorizes(7, 12, &command));
        hub.receive(7, 12, empty()).unwrap();
        assert!(hub.authorizes(7, 12, &command));
        hub.reset(8);
        hub.receive(7, 12, empty()).unwrap();
        assert!(access.read(|_, r| r.snapshot().is_none()));
        assert!(!hub.authorizes(7, 12, &command));
    }

    #[test]
    fn only_accepted_continuations_signal_playlist_progress() {
        let hub = PersonalHub::default();
        hub.begin(1, 3);
        hub.receive(1, 3, empty()).unwrap();
        let access = hub.access();
        assert!(access.watch_playlist(1, Some("large".into())).is_some());
        let progress = || access.read(|status, _| status.playlist_progress);
        let part = || {
            serde_json::from_value(json!({"part": {
                "summary": {"id":"large", "name":"Large", "description":"", "kind":"manual",
                    "countState":"exact", "trackCount":2, "availableCount":2, "revision":1,
                    "createdAt":"2026-08-03T00:00:00Z", "updatedAt":"2026-08-03T00:00:00Z",
                    "capabilities":{"editMetadata":true,"editEntries":true,"editDefinition":false,
                        "play":true,"copyToManual":false,"delete":true,"addTracks":true}},
                "token":"t", "offset":0, "more":true, "rows":[{"id":"entry", "position":1,
                    "track":{"kind":"track","id":"song","name":"Song","artists":[],
                        "hasAudio":true,"coverage":{"tracks":1,"inLibrary":1,"albums":1}}}]
            }}))
            .unwrap()
        };
        hub.receive(1, 2, part()).unwrap();
        assert_eq!(progress(), 0);
        assert!(matches!(
            hub.receive(1, 3, part()).unwrap(),
            Some(PersonalCommand::SyncPlaylist { offset: 1, .. })
        ));
        assert_eq!(progress(), 1);
        assert_eq!(hub.receive(1, 3, part()).unwrap(), None);
        hub.receive(1, 3, serde_json::from_value(json!({"change":{}})).unwrap())
            .unwrap();
        assert_eq!(progress(), 1);
    }

    #[test]
    fn move_admission_rejects_same_revision_detail_replacement() {
        let hub = PersonalHub::default();
        let access = hub.access();
        hub.begin(7, 3);
        hub.receive(7, 3, empty()).unwrap();
        assert!(access.watch_playlist(7, Some("mix".into())).is_some());
        let part = |token: &str, keys: [&str; 3]| {
            let rows: Vec<_> = keys.into_iter().enumerate().map(|(i, key)| json!({
                "id":key, "position":i, "track":{"kind":"track","id":"same-song","name":"Song","artists":[],
                    "hasAudio":true,"coverage":{"tracks":1,"inLibrary":1,"albums":1}}})).collect();
            serde_json::from_value(json!({"part":{
                "summary":{"id":"mix","name":"Mix","description":"","kind":"manual","countState":"exact",
                    "trackCount":3,"availableCount":3,"revision":4,"createdAt":"2026-08-03T00:00:00Z",
                    "updatedAt":"2026-08-03T00:00:00Z","capabilities":{"editMetadata":true,"editEntries":true,
                    "editDefinition":false,"play":true,"copyToManual":false,"delete":true,"addTracks":true}},
                "token":token,"offset":0,"more":false,"rows":rows}})).unwrap()
        };
        hub.receive(7, 3, part("first", ["a", "b", "c"])).unwrap();
        let before = access.read(|s, _| s);
        let order = PersonalCommand::MoveEntry {
            playlist_id: "mix".into(),
            entry_id: "a".into(),
            after: "c".into(),
        };
        assert!(hub.admit_move(before, &order, || true));
        let past = PersonalCommand::MoveEntry {
            playlist_id: "mix".into(),
            entry_id: "b".into(),
            after: "a".into(),
        };
        assert!(!hub.admit_move(before, &past, || panic!("same-slot drop submitted")));
        assert!(access.refresh_playlist(7).is_some());
        assert!(!hub.admit_move(before, &order, || panic!("stale detail submitted")));
        hub.receive(7, 3, part("other", ["b", "a", "c"])).unwrap();
        let after = access.read(|s, _| s);
        assert_ne!(after.playlist_detail, before.playlist_detail);
        assert!(!hub.admit_move(before, &order, || panic!("old observation submitted")));
        assert!(hub.admit_move(after, &order, || true));
    }

    #[tokio::test]
    async fn v2_complete_detail_refresh_is_fresh_and_mode_fenced() {
        use crate::{api::{Api, Origin}, pages::Route, personal::{Change, Part}};
        let hub = PersonalHub::default();
        let personal = hub.access();
        let api = Arc::new(Api::new(Origin::parse("http://127.0.0.1:1").unwrap()).unwrap());
        let session = api.test_session();
        let access = Access { epoch: 1, api, session: session.clone(), offline: false, personal: personal.clone() };
        let epoch = Arc::new(AtomicU64::new(1));
        hub.bind_cache(Some(&access), &epoch);
        hub.begin(1, 2);
        hub.receive(1, 2, empty()).unwrap();
        let route = Route::Playlist("mix".into());
        let schedule = || personal.watch_playlist(1, Some("mix".into()));
        let part: PersonalUpdate = serde_json::from_value(json!({"part":{
            "summary":{"id":"mix","name":"Mix","description":"","kind":"manual","countState":"exact",
                "trackCount":2,"availableCount":2,"revision":1,"createdAt":"2026-08-03T00:00:00Z",
                "updatedAt":"2026-08-03T00:00:00Z","capabilities":{"editMetadata":true,"editEntries":true,
                   "editDefinition":false,"play":true,"copyToManual":false,"delete":true,"addTracks":true}},
            "token":"complete","offset":0,"more":false,"rows":[
                {"id":"a","position":0,"track":{"kind":"track","id":"same","name":"Song","hasAudio":true,"coverage":{"tracks":1,"inLibrary":1,"albums":1}}},
                {"id":"b","position":1,"track":{"kind":"track","id":"same","name":"Song","hasAudio":true,"coverage":{"tracks":1,"inLibrary":1,"albums":1}}}]
        }})).unwrap();
        let PersonalUpdate::Part(part) = part else { unreachable!() };
        assert!(schedule().is_some());
        assert!(schedule().is_none());
        hub.receive(1, 2, PersonalUpdate::Part(part.clone())).unwrap();
        let projection = personal.cached_page(1, &route).unwrap();
        assert_eq!(projection.rows.len(), 2);
        assert_ne!(projection.rows[0].key, projection.rows[1].key);
        personal.watch_playlist(1, None);
        assert!(schedule().is_none(), "fresh complete detail must not request the wire");
        {
            let mut state = personal.state.lock().unwrap();
            state.cache.details.get_mut("mix").unwrap().refreshed_at = personal_cache::now_ms() - FRESHNESS.as_millis() as u64;
        }
        let expired_at = personal.state.lock().unwrap().cache.details["mix"].refreshed_at;
        assert!(matches!(hub.refresh_due(1, 2), Some(PersonalCommand::SyncPlaylist { known: None, offset: 0, .. })));
        assert!(hub.refresh_due(1, 2).is_none());
        hub.receive(1, 2, PersonalUpdate::Part(part.clone())).unwrap(); // same token, full validated response
        let refreshed_at = personal.state.lock().unwrap().cache.details["mix"].refreshed_at;
        assert!(refreshed_at > expired_at && hub.refresh_due(1, 2).is_none());
        personal.watch_playlist(1, None);
        assert!(schedule().is_none());
        assert!(personal.refresh_playlist(1).is_some());
        hub.receive(1, 2, PersonalUpdate::Part(Part {
            token: "partial".into(), rows: part.rows[..1].to_vec(), more: true, ..part.clone()
        })).unwrap();
        assert_eq!(personal.cached_page(1, &route).unwrap(), projection);
        assert_eq!(personal.state.lock().unwrap().cache.details["mix"].refreshed_at, refreshed_at);

        let mut summary = part.summary.clone();
        summary.name = "Confirmed edit".into();
        summary.revision = 2;
        hub.receive(1, 2, PersonalUpdate::Change(Change { playlist: Some(summary), ..Change::default() })).unwrap();
        hub.receive(1, 2, PersonalUpdate::Part(part.clone())).unwrap();
        assert_eq!(personal.cached_page(1, &route).unwrap().title, "Confirmed edit");
        let mut summary = part.summary.clone();
        summary.name = "Confirmed edit".into();
        summary.revision = 2;
        assert!(personal.refresh_playlist(1).is_some());
        hub.receive(1, 2, PersonalUpdate::Part(Part { summary: summary.clone(), ..part.clone() })).unwrap();
        let before = personal.read(|status, _| status);
        let effect = Effect::Move { id: "mix".into(), entry_id: "a".into(), after: "b".into(), revision: 2 };
        let previous = personal.browse_view(1, &route, Some((before, &effect)));
        assert_eq!(previous.confirmed, None);
        summary.revision = 3;
        hub.receive(1, 2, PersonalUpdate::Change(Change { playlist: Some(summary),
            entries: Some(crate::personal::EntryChange { moved: Some("a".into()), after: Some("b".into()), ..Default::default() }),
            ..Change::default()
        })).unwrap();
        let first_confirmed = personal.browse_view(1, &route, Some((before, &effect)));
        assert_eq!(first_confirmed.confirmed, Some(None));
        let rows = first_confirmed.page.unwrap().rows;
        assert_eq!(rows[0].key, projection.rows[1].key);
        assert_eq!(rows[1].key, projection.rows[0].key);
        session.set_offline(true);
        hub.receive(1, 2, empty()).unwrap();
        assert_eq!(personal.cached_page(1, &route).unwrap().title, "Confirmed edit");
        epoch.store(2, Ordering::SeqCst);
        hub.reset(2);
        hub.receive(1, 2, PersonalUpdate::Part(part)).unwrap();
        assert!(personal.cached_page(2, &route).is_none());
    }

    #[test]
    fn publication_owner_can_read_hub_while_receive_is_waiting() {
        let hub = Arc::new(PersonalHub::default());
        hub.begin(7, 11);
        let (holding, held) = std::sync::mpsc::channel();
        let (inspect, inspection) = std::sync::mpsc::channel();
        let (done, finished) = std::sync::mpsc::channel();
        let admitting = hub.clone();
        let socket = std::thread::spawn(move || {
            let _publication = PUBLICATION_GATE.lock().unwrap();
            holding.send(()).unwrap();
            inspection.recv().unwrap();
            assert!(admitting.access().state.try_lock().is_ok(), "waiting receive held reducer ahead of publication gate");
            done.send(()).unwrap();
        });
        held.recv().unwrap();
        let receiving = hub.clone();
        let (start, started) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            start.send(()).unwrap();
            receiving.receive(7, 11, empty()).unwrap();
        });
        started.recv().unwrap();
        std::thread::sleep(Duration::from_millis(20));
        inspect.send(()).unwrap();
        finished.recv_timeout(Duration::from_secs(2)).unwrap();
        socket.join().unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn submission_and_an_empty_delta_do_not_confirm_a_write() {
        let hub = PersonalHub::default();
        hub.begin(1, 3);
        hub.receive(1, 3, empty()).unwrap();
        let access = hub.access();
        let before = access.read(|s, _| s);
        let effect = Effect::ClearHistory;
        assert!(hub.authorizes(1, 3, &PersonalCommand::ClearHistory));
        assert_eq!(access.observed(before, &effect), None);
        let no_op = serde_json::from_value(json!({"change":{}})).unwrap();
        hub.receive(1, 3, no_op).unwrap();
        assert_eq!(access.observed(before, &effect), None);
        hub.refused(1, 2);
        assert_eq!(access.read(|s, _| s.refused), before.refused);
        hub.refused(1, 3);
        assert_ne!(access.read(|s, _| s.refused), before.refused);
    }
}
