//! Playback room actor. The socket binding fences messages; only device ownership
//! authorizes the native executor. The optimistic controller is never an input
//! to that executor.
use super::{
    Engine, PlayerView, QueueEntryView, Reply, album_route_id, artist_subtitle, context_kind,
    credit_views, native,
};
use crate::{
    DeviceRow,
    audio_input::AudioInput,
    offline,
    personal::{Command as PersonalCommand, Update as PersonalUpdate},
    playback_sync::Projection,
    player::{self, Action, LocalQueue},
    socket::{self, Claim, Event, EventKind, Generation, Link, State},
};
use std::{
    sync::atomic::Ordering,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const REFRESH_INTERVAL: Duration = Duration::from_secs(2);

fn cycle_repeat(repeat: socket::Repeat) -> player::Repeat {
    match repeat {
        socket::Repeat::Off => player::Repeat::All,
        socket::Repeat::All => player::Repeat::One,
        socket::Repeat::One => player::Repeat::Off,
    }
}

#[derive(Default)]
pub(super) struct Online {
    pub projection: Projection,
    transport: Option<Generation>,
    personal_generation: Option<Generation>,
    connection_id: String,
    pub detached: bool,
    pub(super) detached_owner: bool,
    pub superseded: bool,
    gap: bool,
    repair_sent: bool,
    refresh_at: Option<Instant>,
    claim_offer: Option<ClaimOffer>,
    claim_pending: Option<PendingClaim>,
    clock_offset_ms: Option<i64>,
    pub native: native::NativeV3,
    primary: Option<(native::MediaRequest, tokio::task::JoinHandle<()>)>,
    preview: Option<(native::MediaRequest, tokio::task::JoinHandle<()>)>,
    claim_media: [Option<(native::MediaRequest, Option<AudioInput>)>; 2],
}

struct ClaimOffer {
    cursor: socket::Cursor,
    source: ClaimSource,
    reattach_basis: Option<socket::ReattachBasis>,
}

enum ClaimSource {
    Saved {
        operation: u64,
        revision: u64,
        run: Option<u64>,
    },
    Detached {
        intent: socket::Cursor,
        run: Option<u64>,
    },
}

struct PendingClaim {
    id: String,
    deadline: Instant,
    target: socket::Cursor,
    saved_run: Option<u64>,
    saved_revision: u64,
}

fn claim_cursor_matches(offered: &socket::Cursor, accepted: &State) -> bool {
    let mut expected = offered.clone();
    if let Some(track) = accepted.current().filter(|row| row.duration > 0.0) {
        expected.position_ms = expected
            .position_ms
            .min((track.duration * 1000.0).round() as u64);
    }
    expected == accepted.cursor
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(socket::MAX_SAFE_INTEGER as u128) as u64
}

impl Online {
    pub fn visible(&self) -> Option<&State> {
        self.projection.projected()
    }
    pub fn is_owner(&self, device: &str) -> bool {
        self.projection
            .owner()
            .and_then(|owner| owner.device_id.as_deref())
            == Some(device)
    }
    pub fn reconciling_claim(&self) -> bool {
        self.claim_pending.is_some()
    }
    pub fn wants_pause(&self) -> bool {
        self.claim_pending.as_ref().map_or_else(
            || self.visible().is_some_and(|p| p.cursor.desired_playing),
            |pending| pending.target.desired_playing,
        )
    }
    pub fn cancel_loads(&mut self) {
        self.claim_media = Default::default();
        for (_, task) in self.primary.take().into_iter().chain(self.preview.take()) {
            task.abort();
        }
    }
    fn cancel(&mut self, request: &native::MediaRequest) {
        let slot = if request.preview {
            &mut self.preview
        } else {
            &mut self.primary
        };
        if slot.as_ref().is_some_and(|(r, _)| r == request)
            && let Some((_, task)) = slot.take()
        {
            task.abort();
        }
    }
}

impl Engine {
    fn transport_generation(&self) -> Option<Generation> {
        let generation = self.online.transport?;
        (self.network_allowed() && self.link() == Link::Connected(generation)).then_some(generation)
    }
    pub(super) fn online_generation(&self) -> Option<Generation> {
        (!self.online.gap)
            .then_some(self.transport_generation())
            .flatten()
    }
    pub(super) fn disconnect_personal(&mut self) {
        if let Some(generation) = self.online.personal_generation.take() {
            self.personal.disconnect(self.epoch, generation);
        }
    }
    pub(super) fn send_personal(
        &self,
        epoch: u64,
        generation: Generation,
        command: &PersonalCommand,
    ) {
        if epoch != self.epoch
            || self.transport_generation() != Some(generation)
            || self.online.personal_generation != Some(generation)
        {
            return;
        }
        if matches!(command, PersonalCommand::SyncPlaylist { .. }) {
            self.personal
                .send_playlist_sync(epoch, generation, command, || {
                    self.socket.as_ref().is_some_and(|s| {
                        s.personal_playlist_sync(
                            epoch,
                            generation,
                            command,
                            self.personal.access(),
                            self.shared_epoch.clone(),
                        )
                        .is_ok()
                    })
                });
        } else if matches!(command, PersonalCommand::SetPlaylistRules { .. })
            || !self.personal.authorizes(epoch, generation, command)
            || !self
                .socket
                .as_ref()
                .is_some_and(|s| s.personal(generation, command).is_ok())
        {
            self.personal.refused(epoch, generation);
        }
    }
    pub(super) fn send_quick_add(&self, ticket: super::QuickAddTicket) {
        let access = self.personal.access();
        if !self.current()
            || self.epoch != ticket.epoch
            || self.transport_generation() != Some(ticket.generation)
            || self.online.personal_generation != Some(ticket.generation)
            || !self.socket.as_ref().is_some_and(|socket| {
                self.personal.admit_quick_add(&ticket, || {
                    socket
                        .personal_quick_add(
                            access.clone(),
                            ticket.clone(),
                            self.shared_epoch.clone(),
                        )
                        .is_ok()
                })
            })
        {
            access.quick_cancel(&ticket);
        }
    }
    pub(super) fn send_rules_edit(
        &self,
        observed: super::PersonalStatus,
        prior: &crate::personal::RulesBasis,
        command: &PersonalCommand,
    ) -> bool {
        let generation = observed.generation;
        let (Some(account), Some(socket)) = (&self.account, &self.socket) else { return false; };
        if self.transport_generation() != Some(generation) || self.online.personal_generation != Some(generation) {
            return false;
        }
        let mode = *account.session.network_changes().borrow();
        self.personal.admit_rules_edit(observed.epoch, generation, prior, command, || {
            socket.personal_order(generation, mode, command).is_ok()
        })
    }

    pub(super) fn send_playlist_order(
        &self,
        observed: super::PersonalStatus,
        mode_stamp: u64,
        command: &PersonalCommand,
    ) -> bool {
        let generation = observed.generation;
        let (Some(account), Some(socket)) = (&self.account, &self.socket) else {
            return false;
        };
        if self.transport_generation() != Some(generation)
            || self.online.personal_generation != Some(generation)
            || !socket.events.is_empty()
        {
            return false;
        }
        let mode = *account.session.network_changes().borrow();
        mode == mode_stamp
            && self.personal.admit_move(observed, command, || {
                socket
                    .personal_order(generation, mode_stamp, command)
                    .is_ok()
            })
    }
    fn repair(&mut self, generation: Generation) {
        if !self.online.repair_sent
            && self
                .socket
                .as_ref()
                .is_some_and(|socket| socket.sync(generation).is_ok())
        {
            self.online.repair_sent = true;
        }
    }
    pub(super) fn drain_online_events(&mut self, first: Option<Event>) {
        if self
            .online
            .transport
            .is_some_and(|g| self.link() != Link::Connected(g))
        {
            self.leave_link();
        }
        if let Some(event) = first {
            self.apply_event(event);
        }
        for _ in 0..16 {
            let Some(event) = self.socket.as_mut().and_then(|s| s.events.try_recv().ok()) else {
                break;
            };
            self.apply_event(event);
        }
    }
    pub(super) fn leave_link(&mut self) {
        let departed = self.online.transport.take();
        self.disconnect_personal();
        self.online.claim_offer = None;
        self.online.claim_pending = None;
        self.online.clock_offset_ms = None;
        self.online.repair_sent = false;
        self.online.refresh_at = None;
        if let Some(generation) = departed {
            let link = self.link();
            let demoted = self
                .socket
                .as_ref()
                .and_then(|socket| socket.last_owner(generation))
                == Some(false);
            let eligible = link != Link::Superseded
                && self
                    .account
                    .as_ref()
                    .is_some_and(|a| !a.session.is_revoked());
            if eligible && !self.private_audio() {
                self.online.detached_owner = !self.online.gap && !demoted && self.online.is_owner(&self.device_id)
                    && self.online.native.state().is_some_and(|state| !state.queue.is_empty());
                self.online.detached = self.online.detached_owner;
                if self.online.detached_owner {
                    // The current run still owns its final PCM sample here.
                    // The very next detached tick (or a queued event) may
                    // advance at EOF and replace that occurrence.
                    self.record_local_listening();
                } else {
                    self.isolate_confirmed_follower();
                }
            } else if !eligible && !self.account.as_ref().is_some_and(|a| !a.session.is_revoked() && self.private_audio()) {
                self.retire_private_audio();
            }
        }
        self.online.projection.clear_pending();
        if departed.is_some() {
            self.publish_access();
        }
    }
    fn isolate_confirmed_follower(&mut self) {
        self.retire_saved_claim();
        self.stop();
        self.online.detached_owner = false;
        self.online.detached = false;
        if let (Some(state), Some(position_ms), Some(audio)) = (
            self.online.projection.confirmed(),
            self.online.projection.confirmed_position_ms(),
            &mut self.audio,
        ) {
            self.online
                .native
                .isolate_follower(state, position_ms, audio);
            self.online.detached = true;
            self.online.gap = false; // repair only applies to the retired binding
        }
    }
    fn apply_event(&mut self, event: Event) {
        let generation = event.generation;
        // A handshake requests a fresh, actor-serialized claim; a polling watch
        // can lose the final local action or EOF immediately before a reconnect.
        if let EventKind::ClaimRequested(reply) = event.kind {
            if self.network_allowed() {
                self.drive_native(NativeEvent::Tick);
                // Recovery reopens data services, not the private playback
                // queue. Never export offline commands or cursor/queue edits.
                let _ = reply.send(if self.private_audio() || self.online.detached {
                    Claim { local: None, reattach_basis: None }
                } else { self.current_claim() });
            }
            return;
        }
        if self.link() != Link::Connected(generation) {
            if self.link() != Link::Superseded && !self.private_audio()
                && self
                    .account
                    .as_ref()
                    .is_some_and(|a| !a.session.is_revoked())
                && self.socket.as_ref().and_then(|s| s.last_owner(generation)) == Some(false)
                && let EventKind::Snapshot(snapshot) = event.kind
                && self.online.projection.apply_snapshot(snapshot).applied()
            {
                // A losing first snapshot is still confirmed data even if
                // the socket closes before the actor installs its binding.
                self.isolate_confirmed_follower();
                self.online.projection.clear_pending();
                self.publish_access();
            }
            return;
        }
        let stamp = event.personal_stamp;
        match event.kind {
            EventKind::Snapshot(snapshot) => {
                let fresh = self.online.transport != Some(generation);
                let owns = snapshot.owner.device_id.as_deref() == Some(self.device_id.as_str());
                let offered = fresh.then(|| self.online.claim_offer.take()).flatten();
                let resolving = self.online.claim_pending.take();
                let newer_cursor = offered
                    .as_ref()
                    .and_then(|offer| self.newer_claim_cursor(offer, &snapshot));
                let run = self.audio.as_ref().and_then(|a| a.run_identity());
                let adopted = owns && self.local_store.is_some()
                    && self.local.current().is_some_and(|row| snapshot.player.current().is_some_and(|remote| remote.id == row.track.id))
                    && (offered.as_ref().is_some_and(|offer| {
                        matches!(&offer.source, ClaimSource::Saved { revision, run: sent, .. }
                            if *revision == self.local_revision && sent.is_none_or(|sent| Some(sent) == run))
                        && claim_cursor_matches(&offer.cursor, &snapshot.player)
                    }) || resolving.as_ref().is_some_and(|pending| {
                        pending.saved_revision == self.local_revision && pending.saved_run.is_none_or(|sent| Some(sent) == run)
                            && claim_cursor_matches(&pending.target, &snapshot.player)
                    }));
                let outcome = self.online.projection.apply_snapshot(snapshot.clone());
                if outcome.needs_sync() {
                    self.online.gap = true;
                    self.repair(generation);
                }
                if !outcome.applied() {
                    self.online.claim_offer = offered;
                    self.online.claim_pending = resolving;
                    return;
                }
                self.online.gap = false;
                self.online.repair_sent = false;
                self.online.refresh_at = None;
                let private = self.private_audio();
                if !private {
                    self.online.detached = false;
                    self.online.detached_owner = false;
                }
                if fresh {
                    self.disconnect_personal();
                    self.online.transport = Some(generation);
                    self.online
                        .connection_id
                        .clone_from(&snapshot.connection_id);
                    self.online.personal_generation = Some(generation);
                    self.personal.begin(self.epoch, generation);
                }
                self.update_bulk_device();
                if private {
                    if fresh { self.publish_access(); }
                    return;
                }
                if let Some(cursor) = newer_cursor
                    && self.begin_claim_reconcile(generation, &snapshot, cursor)
                {
                    self.publish_access();
                    return;
                }
                self.finish_local_claim(&snapshot.player, adopted);
                self.drive_native(NativeEvent::Snapshot);
                if fresh {
                    self.publish_access();
                }
            }
            EventKind::ClaimRequested(_) => unreachable!(),
            _ if self.online.transport != Some(generation) => return,
            EventKind::State(update) => {
                let command = update.command.clone();
                let cursor_changed = update.delta.cursor.is_some();
                let revision = update.revision;
                if !self.online.gap {
                    let outcome = self.online.projection.apply_state(update);
                    if outcome.needs_sync() {
                        self.online.gap = true;
                        self.repair(generation);
                        self.drive_native(NativeEvent::Tick);
                    } else if outcome.applied() && self.online.projection.revision() == revision {
                        if cursor_changed && self.online.claim_pending.take().is_some() {
                            // An already-admitted shared transport update wins
                            // even if it reaches us before the claim repair.
                            self.retire_saved_claim();
                            self.drive_native(NativeEvent::OpeningState(command.as_ref()));
                        } else {
                            self.drive_native(NativeEvent::State(command.as_ref()));
                        }
                    }
                }
            }
            EventKind::Execute(execute) => {
                if !self.online.gap
                    && self.online.projection.revision() == execute.revision
                    && self
                        .online
                        .projection
                        .owner()
                        .is_some_and(|owner| owner.generation == execute.generation)
                    && self.online.is_owner(&self.device_id)
                {
                    if self.online.claim_pending.take().is_some() {
                        self.retire_saved_claim();
                        self.drive_native(NativeEvent::Snapshot);
                    }
                    self.drive_native(NativeEvent::Execute(&execute.command));
                }
            }
            EventKind::Observation(update) => {
                if self
                    .online
                    .projection
                    .apply_observation(update)
                    .needs_sync()
                {
                    self.online.gap = true;
                    self.repair(generation);
                    self.drive_native(NativeEvent::Tick);
                }
            }
            EventKind::Ownership(update) => {
                if self.online.projection.apply_ownership(update).needs_sync() {
                    self.online.gap = true;
                    self.repair(generation);
                }
                self.update_bulk_device();
                self.drive_native(NativeEvent::Tick);
            }
            EventKind::Offer(offer) => {
                // Acceptance means we can assume the role, not that audio can
                // start. Native initialization opens devices only after commit.
                if self.network_allowed()
                    && !self.private_audio()
                    && self.audio.is_some()
                    && let Some(socket) = &self.socket
                {
                    let _ = socket.accept(generation, offer);
                }
            }
            EventKind::Result(result) => {
                if self
                    .online
                    .claim_pending
                    .as_ref()
                    .is_some_and(|p| p.id == result.id)
                {
                    // The matching or corrective snapshot is the authoritative
                    // response. Do not publish or execute a terminal echo twice.
                    return;
                }
                let status = result.status;
                if self.online.projection.apply_result(result) {
                    self.action_status = match status {
                        socket::ResultStatus::Failed => "Playback could not be changed.",
                        socket::ResultStatus::Unavailable => "Playback is unavailable.",
                        _ => "",
                    };
                }
            }
            EventKind::SampleClock(clock) => self.online.clock_offset_ms = clock.offset_ms,
            EventKind::Personal(value) => {
                let Ok(update) = serde_json::from_value::<PersonalUpdate>(value) else {
                    self.personal.gap(self.epoch, generation);
                    self.repair(generation);
                    return;
                };
                match self.personal.receive_stamped(self.epoch, generation, update, stamp) {
                    Ok(Some(command)) => self.send_personal(self.epoch, generation, &command),
                    Ok(None) => {}
                    Err(_) => self.repair(generation),
                }
            }
        }
        if let (Some(stamp), Some(socket)) = (stamp, &self.socket) {
            socket.mark_personal_applied(stamp);
        }
    }
    fn newer_claim_cursor(
        &self,
        offered: &ClaimOffer,
        snapshot: &socket::Snapshot,
    ) -> Option<socket::Cursor> {
        if snapshot.owner.device_id.as_deref() != Some(&self.device_id)
            || !claim_cursor_matches(&offered.cursor, &snapshot.player)
        {
            return None;
        }
        if let Some(basis) = &offered.reattach_basis
            && snapshot.session_id == basis.session_id
            && snapshot.owner.generation == basis.generation
            && !(basis.revision..=basis.revision.saturating_add(1)).contains(&snapshot.revision)
        {
            return None;
        }
        let local = match &offered.source {
            ClaimSource::Saved { operation, .. } => {
                let local = self.saved_claim()?;
                if *operation == self.operation
                    && local.cursor.key == offered.cursor.key
                    && local.cursor.occurrence == offered.cursor.occurrence
                    && local.cursor.desired_playing == offered.cursor.desired_playing
                {
                    return None;
                }
                local
            }
            ClaimSource::Detached { intent, run } => {
                let audio = self.audio.as_ref()?;
                if self.online.native.cursor() == Some(intent) && audio.run_identity() == *run {
                    return None;
                }
                self.online.native.local_snapshot(audio)?
            }
        };
        if local.cursor == snapshot.player.cursor || local.cursor.key == 0 {
            return None;
        }
        let id = &local.current()?.id;
        snapshot
            .player
            .queue
            .iter()
            .find(|row| row.key == local.cursor.key && &row.id == id)?;
        Some(local.cursor)
    }
    fn begin_claim_reconcile(
        &mut self,
        generation: Generation,
        snapshot: &socket::Snapshot,
        cursor: socket::Cursor,
    ) -> bool {
        let basis = socket::ReattachBasis {
            session_id: snapshot.session_id.clone(),
            generation: snapshot.owner.generation,
            revision: snapshot.revision,
            occurrence: snapshot.player.cursor.occurrence.clone(),
        };
        let Some(socket) = &self.socket else {
            return false;
        };
        let Ok(id) = socket.reconcile_claim(generation, basis, cursor.clone()) else {
            return false;
        };
        if self.local_store.is_none()
            && let Some(audio) = &self.audio
        {
            self.online.native.witness_claim_cursor(&cursor, audio);
        }
        self.online.claim_pending = Some(PendingClaim {
            id,
            deadline: Instant::now() + Duration::from_secs(5),
            target: cursor,
            saved_run: self.audio.as_ref().and_then(|a| a.run_identity()),
            saved_revision: self.local_revision,
        });
        true
    }
    fn finish_local_claim(&mut self, state: &State, adopt: bool) {
        if self.local_store.is_none() {
            return;
        }
        if adopt && let Some(audio) = &self.audio {
            self.online.native.adopt_local(state, audio);
            self.local_lookahead = None;
        } else {
            self.stop_local();
        }
        self.cancel_work();
        self.local = LocalQueue::default();
        self.local_store = None;
    }
    fn retire_saved_claim(&mut self) {
        if self.local_store.is_some() {
            self.stop_local();
            self.cancel_work();
            self.local = LocalQueue::default();
            self.local_store = None;
        }
    }
    fn update_bulk_device(&self) {
        let Some(socket) = &self.socket else {
            return;
        };
        if let Some(device) = self
            .online
            .projection
            .devices()
            .iter()
            .find(|d| d.device_id == self.device_id && d.can_play)
        {
            self.personal
                .set_bulk_device(crate::personal_bridge::BulkDevice {
                    id: device.device_id.clone(),
                    name: device.name.clone(),
                    link: socket.link.clone(),
                    order: socket.bulk_order(),
                });
        }
    }
    pub(super) fn tick_online(&mut self, first: Option<Event>) {
        self.drain_online_events(first);
        match self.link() {
            Link::Expired => {
                self.auth_transport_expired();
                if self.account.is_none() {
                    return;
                }
            }
            Link::Exhausted => {
                self.automatic_fallback();
            }
            Link::Superseded => {
                self.online.superseded = true;
                self.socket = None;
                if !self.private_audio() {
                    self.retire_private_audio();
                }
                self.publish_access();
            }
            _ => {}
        }
        if self
            .online
            .claim_pending
            .as_ref()
            .is_some_and(|pending| Instant::now() >= pending.deadline)
        {
            self.online.claim_pending = None;
            self.online.gap = true;
            self.online.repair_sent = false;
            self.retire_saved_claim();
            self.stop();
        }
        if self.online.claim_pending.is_none() {
            if self.local_store.is_some() {
                self.tick_saved();
            } else {
                self.drive_native(NativeEvent::Tick);
            }
        }
        if self.online.claim_pending.is_none() {
            for (request, source) in std::mem::take(&mut self.online.claim_media)
                .into_iter()
                .flatten()
            {
                self.apply_media(request, source);
            }
        }
        if let Some(generation) = self.transport_generation() {
            if self.personal.account_refresh_due(self.epoch, generation) && let Some(socket) = &self.socket {
                let _ = socket.sync(generation);
            }
            if let Some(command) = self.personal.refresh_due(self.epoch, generation) {
                self.send_personal(self.epoch, generation, &command);
            }
        }
        if self.online.gap
            && let Some(generation) = self.transport_generation()
        {
            self.repair(generation);
        }
        if let Some(generation) = self.online_generation()
            && self.online.is_owner(&self.device_id)
        {
            if self.online.refresh_at.is_none_or(|at| Instant::now() >= at)
                && let Some(owner) = self.online.projection.owner()
                && self
                    .socket
                    .as_ref()
                    .is_some_and(|s| s.refresh(generation, owner.generation).is_ok())
            {
                self.online.refresh_at = Some(Instant::now() + REFRESH_INTERVAL);
            }
        }
    }
    pub(super) fn current_claim(&mut self) -> Claim {
        let local = if self.local_store.is_some() {
            self.saved_claim()
        } else if self.online.detached || self.online.is_owner(&self.device_id) {
            self.audio
                .as_ref()
                .and_then(|audio| self.online.native.detached_snapshot(audio))
        } else {
            None
        };
        let reattach_basis = self
            .online
            .detached_owner
            .then(|| self.online.projection.reattach_basis())
            .flatten();
        self.online.claim_offer = local.as_ref().and_then(|offered| {
            let source = if self.local_store.is_some() {
                ClaimSource::Saved {
                    operation: self.operation,
                    revision: self.local_revision,
                    run: self.audio.as_ref().and_then(|a| a.run_identity()),
                }
            } else {
                ClaimSource::Detached {
                    intent: self.online.native.cursor()?.clone(),
                    run: self.audio.as_ref().and_then(|audio| audio.run_identity()),
                }
            };
            Some(ClaimOffer {
                cursor: offered.cursor.clone(),
                source,
                reattach_basis: reattach_basis.clone(),
            })
        });
        Claim {
            local,
            reattach_basis,
        }
    }
    pub(super) fn online_action(&mut self, action: Action) {
        let Some(generation) = self.online_generation() else {
            return;
        };
        let action = if matches!(action, Action::CycleRepeat) {
            let repeat = self
                .online
                .visible()
                .map_or(socket::Repeat::Off, |p| p.repeat);
            Action::Repeat(cycle_repeat(repeat))
        } else {
            action
        };
        let Some(socket) = &self.socket else {
            return;
        };
        let action = match action {
            Action::RenameDevice { device_id, name } => {
                let id = device_id.as_deref().unwrap_or(&self.device_id);
                let name = name.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}');
                if socket.rename(generation, id, name).is_err() {
                    self.action_status = "Device name was not changed.";
                }
                return;
            }
            other => other,
        };
        let Some(action) = action.wire() else {
            return;
        };
        let Some(basis) = self.online.projection.basis(&action) else {
            return;
        };
        if !self.online.projection.can_submit() {
            self.action_status = "Playback controls are busy.";
            return;
        }
        match socket.command(generation, basis.clone(), action.clone()) {
            Ok(id) => {
                self.online.projection.register(id, basis, action);
                self.action_status = "";
            }
            Err(_) => self.action_status = "Playback command was not sent.",
        }
    }
    pub(super) fn detached_action(&mut self, action: Action) {
        let action = if matches!(action, Action::CycleRepeat) {
            let repeat = self
                .online
                .native
                .state()
                .map_or(socket::Repeat::Off, |state| state.repeat);
            Action::Repeat(cycle_repeat(repeat))
        } else {
            action
        };
        if self.online.detached && self.local_transport()
            && let Some(action) = action.wire()
        {
            self.online.detached_owner = true;
            self.drive_native(NativeEvent::Detached(action));
        }
    }
    pub(super) fn complete_online(
        &mut self,
        request: native::MediaRequest,
        source: Option<AudioInput>,
    ) {
        self.drain_online_events(None);
        if self.online.claim_pending.is_some() {
            let slot = if request.preview {
                &self.online.preview
            } else {
                &self.online.primary
            };
            if slot
                .as_ref()
                .is_some_and(|(current, _)| *current == request)
            {
                let index = usize::from(request.preview);
                self.online.claim_media[index] = Some((request, source));
            }
        } else {
            self.apply_media(request, source);
        }
    }
    fn apply_media(&mut self, request: native::MediaRequest, source: Option<AudioInput>) {
        let slot = if request.preview {
            &mut self.online.preview
        } else {
            &mut self.online.primary
        };
        if slot.as_ref().is_none_or(|(r, _)| r != &request) {
            return;
        }
        *slot = None;
        self.drive_native(NativeEvent::Media(&request, source));
    }
    pub(super) fn drive_native(&mut self, event: NativeEvent<'_>) {
        if self.local_store.is_some() || self.online.claim_pending.is_some() {
            return;
        }
        if self.private_audio() && !matches!(event, NativeEvent::Tick | NativeEvent::Detached(_) | NativeEvent::Media(..)) { return; }
        let connected = if self.private_audio() { None } else { self.online_generation() };
        let known_demoted = connected
            .and_then(|generation| self.socket.as_ref()?.last_owner(generation))
            == Some(false);
        let mode = if connected.is_some() && !known_demoted && self.online.is_owner(&self.device_id)
        {
            native::Mode::Owner {
                connection_id: &self.online.connection_id,
            }
        } else if self.online.detached
            && connected.is_none()
            && !self.online.superseded
            && !self.online.gap
        {
            native::Mode::Detached
        } else {
            native::Mode::Follower
        };
        let Some(state) = self.online.projection.confirmed() else {
            return;
        };
        let Some(audio) = &mut self.audio else {
            return;
        };
        let cx = native::Context {
            mode,
            session_id: self.online.projection.session_id().unwrap_or_default(),
            generation: self.online.projection.owner().map_or(0, |o| o.generation),
            revision: self.online.projection.revision(),
            command: match &event {
                NativeEvent::Snapshot
                | NativeEvent::Tick
                | NativeEvent::Media(..)
                | NativeEvent::Detached(_) => None,
                NativeEvent::State(command) | NativeEvent::OpeningState(command) => *command,
                NativeEvent::Execute(command) => Some(command),
            },
            snapshot: matches!(event, NativeEvent::Snapshot | NativeEvent::OpeningState(_)),
            sampled_at_ms: now_ms(),
            clock_offset_ms: self.online.clock_offset_ms,
            crossfade: self.crossfade_enabled.load(Ordering::Acquire),
            report: connected.is_some(),
        };
        let work = match event {
            NativeEvent::Tick => self.online.native.tick(state, cx, audio),
            NativeEvent::Snapshot | NativeEvent::State(_) | NativeEvent::OpeningState(_) => {
                self.online.native.reconcile(state, cx, audio)
            }
            NativeEvent::Execute(_) => self.online.native.execute(state, cx, audio),
            NativeEvent::Media(request, source) => self
                .online
                .native
                .complete(state, cx, audio, request, source),
            NativeEvent::Detached(action) => {
                self.online.native.act_detached(action, state, cx, audio)
            }
        };
        self.submit_native(work);
    }
    pub(super) fn submit_native(&mut self, work: Vec<native::Work>) {
        for work in work {
            match work {
                native::Work::Cancel(request) => self.online.cancel(&request),
                native::Work::Prepare(request) => {
                    let connected = self.online_generation().is_some() && !request.portable_only;
                    let Some(account) = &self.account else {
                        continue;
                    };
                    let (api, session, replies, epoch) = (
                        account.api.clone(),
                        account.session.clone(),
                        self.results.clone(),
                        self.epoch,
                    );
                    let verify_slot = if request.preview {
                        self.local_preview_slot.clone()
                    } else {
                        self.local_read_slot.clone()
                    };
                    let request_for_task = request.clone();
                    let task = tokio::spawn(async move {
                        let id = request_for_task.id.clone();
                        let source = if connected {
                            offline::online_media(&api, &session, verify_slot, &id)
                                .await
                                .ok()
                                .map(|media| media.source)
                        } else {
                            offline::read_verified(api, session, verify_slot, id, || {
                                OfflineStore::open_existing()?.ok_or(offline::Error::NotDownloaded)
                            })
                            .await
                            .ok()
                            .map(|(_, input)| input.source)
                        };
                        let _ = replies
                            .send(Reply::Media {
                                epoch,
                                request: request_for_task,
                                source,
                            })
                            .await;
                    });
                    let slot = if request.preview {
                        &mut self.online.preview
                    } else {
                        &mut self.online.primary
                    };
                    if let Some((_, old)) = slot.replace((request, task)) {
                        old.abort();
                    }
                }
                native::Work::Report(report) => {
                    if let (Some(generation), Some(socket)) =
                        (self.online_generation(), &self.socket)
                        && socket.report(generation, report.clone()).is_ok()
                    {
                        self.online.native.report_sent(&report);
                    }
                }
            }
        }
    }

    pub(super) fn saved_claim(&self) -> Option<State> {
        let local = &self.local;
        let first = local.entries.first()?;
        let cursor = local.current().map_or_else(
            || socket::Cursor {
                key: first.key,
                occurrence: local.occurrence.clone(),
                position_ms: 0,
                desired_playing: false,
            },
            |current| {
                let elapsed = self
                    .audio
                    .as_ref()
                    .filter(|_| local.desired && self.media.is_none())
                    .map_or(local.elapsed, |a| a.facts().elapsed);
                socket::Cursor {
                    key: current.key,
                    occurrence: local.occurrence.clone(),
                    position_ms: (elapsed.max(0.0) * 1000.0).round() as u64,
                    desired_playing: local.desired,
                }
            },
        );
        let queue = local
            .entries
            .iter()
            .map(|e| {
                let t = &e.track;
                let source = &t.source;
                socket::Track {
                    id: t.id.clone(),
                    name: t.title.clone(),
                    artists: super::local_credits(t)
                        .into_iter()
                        .map(|credit| socket::Artist {
                            id: (!credit.id.is_empty()).then_some(credit.id),
                            name: credit.name,
                        })
                        .collect(),
                    album_name: t.album_name.clone(),
                    album_id: super::local_album(t),
                    duration: t.duration,
                    key: e.key,
                    order: e.order as u64 + 1,
                    intent: socket::Intent::Low,
                    entry_id: source.as_ref().and_then(|s| s.entry_id.clone()),
                    target_id: source.as_ref().and_then(|s| s.target_id.clone()),
                    occurrence_id: source.as_ref().and_then(|s| s.occurrence_id.clone()),
                    playback_context: t.playback_context.clone(),
                }
            })
            .collect();
        Some(State {
            queue,
            cursor,
            repeat: match local.repeat {
                player::Repeat::Off => socket::Repeat::Off,
                player::Repeat::All => socket::Repeat::All,
                player::Repeat::One => socket::Repeat::One,
            },
            shuffle: local.shuffle,
            collection_first: 0,
        })
    }
    pub(super) fn project_online(&self, p: &mut PlayerView) {
        let live = self.online_generation().is_some();
        let owner = self
            .online
            .projection
            .owner()
            .and_then(|o| o.device_id.as_deref());
        let active = self
            .online
            .projection
            .devices()
            .iter()
            .find(|d| live && Some(d.device_id.as_str()) == owner);
        p.output = if self.online.detached {
            "This desktop".into()
        } else {
            match active {
                Some(d) => d.name.clone(),
                None if !live => "Output unavailable".into(),
                None if owner.is_some() => "Output disconnected".into(),
                None => "No output selected".into(),
            }
        };
        p.devices = self
            .online
            .projection
            .devices()
            .iter()
            .filter(|_| live)
            .map(|d| DeviceRow {
                id: d.device_id.clone().into(),
                name: d.name.clone().into(),
                selected: Some(d.device_id.as_str()) == owner,
                available: true,
                can_play: d.can_play,
            })
            .collect();
        if !live && !self.online.detached {
            return;
        }
        let Some(state) = (if self.online.detached {
            self.online.native.state()
        } else {
            self.online.visible()
        }) else {
            return;
        };
        let cursor = if let Some(pending) = &self.online.claim_pending {
            &pending.target
        } else if self.online.detached {
            self.online.native.cursor().unwrap_or(&state.cursor)
        } else {
            &state.cursor
        };
        let pos = state.queue.iter().position(|t| t.key == cursor.key);
        p.shuffle = state.shuffle;
        p.repeat = match state.repeat {
            socket::Repeat::Off => "off",
            socket::Repeat::All => "all",
            socket::Repeat::One => "one",
        };
        p.wants_pause = cursor.desired_playing;
        p.transport = !self.online.reconciling_claim()
            && (live || self.online.detached)
            && pos.is_some()
            && self.room_token().is_some();
        p.control = p.transport;
        p.elapsed = cursor.position_ms as f64 / 1000.0;
        if let Some(track) = pos.and_then(|i| state.queue.get(i)) {
            p.track = track.id.clone();
            p.title = track.name.clone();
            p.credits = credit_views(&track.artists);
            p.detail = artist_subtitle(&p.credits);
            p.album = album_route_id(&track.album_id).into();
            p.album_name = track.album_name.clone();
            p.duration = track.duration;
            if let Some(c) = &track.playback_context
                && !(matches!(c.kind, socket::ContextKind::Artist)
                    && p.credits.iter().any(|a| !a.id.is_empty() && a.id == c.id))
            {
                p.context_kind = context_kind(c.kind);
                p.context_id = c.id.clone();
                p.context_name = c.name.clone();
            }
            if let Some(occurrence) = cursor.occurrence.as_ref() {
                p.media_track_token = format!("online:{}:{occurrence}", self.room_incarnation);
            }
        }
        if self.online.detached || self.online.is_owner(&self.device_id) {
            if p.status.is_empty() && self.online.native.failed() {
                p.status = if self.online.detached { "Audio unavailable offline." } else { "Audio could not start on this desktop." };
            }
            if let Some(audio) = &self.audio
                && self.online.native.cursor().is_some_and(|actual| {
                    actual.occurrence == cursor.occurrence
                        && actual.desired_playing == cursor.desired_playing
                        && actual.position_ms == cursor.position_ms
                })
            {
                let facts = audio.facts();
                p.elapsed = facts.elapsed;
                p.playing = facts.playing;
                if facts.duration > 0.0 {
                    p.duration = facts.duration;
                }
            }
        } else if live {
            let time = self
                .online
                .clock_offset_ms
                .map(|offset| now_ms().saturating_add_signed(offset));
            if let Some(actual) = self.online.projection.remote_playback(time) {
                p.playing = actual.playing();
                p.elapsed = actual.position_ms as f64 / 1000.0;
                if p.status.is_empty() && actual.status == Some(socket::ObservationStatus::Failed) {
                    p.status = "Audio could not start on the selected device.";
                }
            }
        }
        p.queue = state
            .queue
            .iter()
            .enumerate()
            .map(|(i, t)| QueueEntryView {
                key: t.key.to_string(),
                track_id: t.id.clone(),
                title: t.name.clone(),
                subtitle: artist_subtitle(&credit_views(&t.artists)),
                album_name: t.album_name.clone(),
                album_id: album_route_id(&t.album_id).into(),
                credits: credit_views(&t.artists),
                current: pos == Some(i),
                upcoming: pos.is_some_and(|p| i > p),
            })
            .collect();
    }
}

pub(super) enum NativeEvent<'a> {
    Tick,
    Snapshot,
    State(Option<&'a socket::Command>),
    OpeningState(Option<&'a socket::Command>),
    Execute(&'a socket::Command),
    Media(&'a native::MediaRequest, Option<AudioInput>),
    Detached(socket::Action),
}

use crate::offline::OfflineStore;

#[cfg(test)]
mod tests {
    use super::*;

    use crate::{
        audio::Audio,
        engine::{Account, connection},
        player::{Authority, Change, LocalTrack, Status},
    };
    use std::{os::unix::fs::DirBuilderExt, path::PathBuf, sync::Arc};

    struct Fixture(PathBuf);
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn wav() -> Arc<[u8]> {
        wav_seconds(1)
    }

    fn wav_seconds(seconds: usize) -> Arc<[u8]> {
        let samples = vec![1500_i16; 8000 * seconds];
        let len = samples.len() as u32 * 2;
        let mut data = b"RIFF".to_vec();
        data.extend((len + 36).to_le_bytes());
        data.extend(b"WAVEfmt ");
        data.extend(16_u32.to_le_bytes());
        data.extend(1_u16.to_le_bytes()); // PCM
        data.extend(1_u16.to_le_bytes()); // mono
        data.extend(8000_u32.to_le_bytes());
        data.extend(16000_u32.to_le_bytes());
        data.extend(2_u16.to_le_bytes());
        data.extend(16_u16.to_le_bytes());
        data.extend(b"data");
        data.extend(len.to_le_bytes());
        for sample in samples {
            data.extend(sample.to_le_bytes());
        }
        data.into()
    }

    #[tokio::test]
    async fn captured_derived_play_and_queue_intents_cannot_send_while_detail_is_stale() {
        use crate::personal::PersonalUpdate;
        let mut e = crate::engine::tests::engine();
        let api = Arc::new(connection("http://127.0.0.1:19433").unwrap());
        let session = api.test_session();
        e.account = Some(Account { api, session: session.clone() });
        e.publish_access(); // Bind the local account cache before the socket opens.
        let generation = 11;
        let (socket, mut peer) = socket::Socket::test_peer(session, generation);
        e.socket = Some(socket);
        let snap = socket::Snapshot {
            session_id: "room".into(), connection_id: "binding".into(), revision: 1,
            owner: socket::Owner { generation, device_id: Some("other".into()) }, devices: vec![],
            player: State::default(), observation: None, server_time_ms: now_ms(),
        };
        peer.set_opening_snapshot(&snap, false);
        e.tick_with_event(Some(Event { generation, kind: EventKind::Snapshot(snap), personal_stamp: None }));
        let personal = e.personal.access();
        let ready = serde_json::json!({"id":"rules","name":"Rules","description":"","kind":"derived",
            "countState":"exact","evaluationState":"ready","trackCount":1,"availableCount":1,"revision":1,
            "createdAt":"2026-08-03T00:00:00Z","updatedAt":"2026-08-03T00:00:00Z",
            "capabilities":{"editMetadata":true,"editEntries":false,"editDefinition":true,"play":true,
                "copyToManual":true,"delete":true,"addTracks":false}});
        let receive = |e: &Engine, update| e.personal.receive(e.epoch, generation,
            serde_json::from_value::<PersonalUpdate>(update).unwrap()).unwrap();
        receive(&e, serde_json::json!({"snapshot":{"playlists":[ready.clone()],"recent":[]}}));
        personal.watch_playlist(e.epoch, Some("rules".into())).unwrap();
        let complete = |summary| serde_json::json!({"part":{"summary":summary,"token":"ready","offset":0,"more":false,
            "definition":{"version":1,"sources":[{"kind":"allSaved","slotId":"root"}],"duplicates":"keep","sort":"sourceOrder"},
            "rows":[{"id":"o1_ready","position":0,"track":{"kind":"track","id":"song","name":"Song","hasAudio":true,
                "coverage":{"tracks":1,"inLibrary":1,"albums":1}}}]}});
        receive(&e, complete(ready.clone()));
        let selection = crate::player::Selection::collection(crate::player::Context::new("derived", "rules", "Rules"));
        let play = || Action::Play { selection: selection.clone(), shuffle: false };
        let queue = || Action::Enqueue { selection: selection.clone() };
        assert!(e.admits_shared_selection(&play()) && e.admits_shared_selection(&queue()));

        let mut stale = ready.clone();
        stale["evaluationState"] = "error".into();
        stale["countState"] = "stale".into();
        receive(&e, serde_json::json!({"change":{"playlist":stale,"rowsInvalid":true}}));
        assert!(!e.admits_shared_selection(&play()) && !e.admits_shared_selection(&queue()));
        while peer.read().is_some() {}
        e.action(play());
        e.action(queue());
        assert!(peer.read().is_none());

        receive(&e, serde_json::json!({"change":{"playlist":ready.clone()}}));
        assert!(!e.admits_shared_selection(&play()));
        receive(&e, complete(ready));
        assert!(e.admits_shared_selection(&play()) && e.admits_shared_selection(&queue()));
    }

    #[tokio::test]
    async fn automatic_cutover_on_exact_pcm_eof_credits_the_same_visit() {
        use crate::{desk::Access, pages::Route};
        let mut e = crate::engine::tests::engine();
        let api = Arc::new(connection("http://127.0.0.1:19431").unwrap());
        let session = api.test_session();
        let personal = e.personal.access();
        e.personal.bind_cache(Some(&Access {
            epoch: e.epoch, api: api.clone(), session: session.clone(), offline: false, personal: personal.clone(),
        }), &e.shared_epoch);
        let (mixer, mut samples) = rodio::mixer::mixer(1, 8000);
        let mut audio = Audio::new_account_scoped(session.clone(), e.shared_epoch.clone(), e.epoch, e.master_gain.clone());
        audio.use_test_output(mixer, "song".into(), wav_seconds(31));
        e.audio = Some(audio);
        e.account = Some(Account { api, session: session.clone() });
        let generation = 11;
        let (socket, peer) = socket::Socket::test_peer(session.clone(), generation);
        e.socket = Some(socket);
        let state: State = serde_json::from_value(serde_json::json!({
            "queue":[{"key":1,"order":1,"id":"song","name":"Cutover","intent":"low","duration":31.0}],
            "cursor":{"key":1,"occurrence":"same-visit","positionMs":0,"desiredPlaying":true},
            "repeat":"off","shuffle":false
        })).unwrap();
        let snap = socket::Snapshot {
            session_id: "eof-room".into(), connection_id: "eof-binding".into(), revision: 1,
            owner: socket::Owner { generation, device_id: Some(e.device_id.clone()) }, devices: vec![],
            player: state, observation: None, server_time_ms: now_ms(),
        };
        peer.set_opening_snapshot(&snap, true);
        e.tick_with_event(Some(Event { generation, kind: EventKind::Snapshot(snap), personal_stamp: None }));
        e.tick();
        assert_eq!(samples.by_ref().take(8000).count(), 8000);
        e.tick();
        for _ in 0..28 * 8000 { let _ = samples.next(); }
        e.tick();
        assert!(personal.cached_page(e.epoch, &Route::History).is_none());
        for _ in 0..3 * 8000 {
            let _ = samples.next();
            if e.audio.as_ref().unwrap().facts().ended { break; }
        }
        assert!(e.audio.as_ref().unwrap().facts().ended);
        assert!(e.audio.as_ref().unwrap().facts().elapsed > 30.0);
        peer.link.send_replace(Link::Disconnected);
        e.tick();
        assert!(e.online.detached_owner);
        assert_eq!(e.audio.as_ref().unwrap().run_identity(), None, "the same tick consumed EOF");
        e.tick();
        let page = personal.cached_page(e.epoch, &Route::History).unwrap();
        assert_eq!(page.rows.len(), 1);
        assert_eq!(page.rows[0].title, "Cutover");
        let digest = ring::digest::digest(&ring::digest::SHA256, b"eof-room\0same-visit");
        let id: String = digest.as_ref().iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(page.rows[0].key, crate::pages::RowKey::HistoryEvent(format!("online:v3:{id}")));
    }

    #[tokio::test]
    async fn same_account_reducer_reset_rejects_queued_reply_for_the_same_media_slot() {
        let mut e = crate::engine::tests::engine();
        let api = Arc::new(connection("http://127.0.0.1:1").unwrap());
        let session = api.test_session();
        let mut audio = Audio::new_account_scoped(session.clone(), e.shared_epoch.clone(), e.epoch, e.master_gain.clone());
        let (mixer, _) = rodio::mixer::mixer(1, 8000);
        audio.use_test_output(mixer, "unrelated".into(), wav());
        e.audio = Some(audio);
        e.account = Some(Account { api, session: session.clone() });
        let state: State = serde_json::from_value(serde_json::json!({
            "queue":[{"key":1,"order":1,"id":"x","name":"X","intent":"low","duration":1.0}],
            "cursor":{"key":1,"occurrence":"visit","positionMs":0,"desiredPlaying":true},
            "repeat":"off","shuffle":false
        })).unwrap();
        let opening = |e: &mut Engine, generation: u64| {
            let (socket, peer) = socket::Socket::test_peer(session.clone(), generation);
            e.socket = Some(socket);
            let snap = socket::Snapshot {
                session_id: format!("room-{generation}"), connection_id: format!("connection-{generation}"),
                revision: 1, owner: socket::Owner { generation, device_id: Some(e.device_id.clone()) },
                devices: vec![], player: state.clone(), observation: None, server_time_ms: now_ms(),
            };
            peer.set_opening_snapshot(&snap, true);
            e.tick_with_event(Some(Event { generation, kind: EventKind::Snapshot(snap), personal_stamp: None }));
            peer
        };
        let _old_peer = opening(&mut e, 11);
        let old = e.online.primary.as_ref().unwrap().0.clone();
        let epoch = e.epoch;
        e.stop();
        let _new_peer = opening(&mut e, 12);
        let new = e.online.primary.as_ref().unwrap().0.clone();
        assert_eq!(e.epoch, epoch);
        assert_eq!((&old.id, old.preview, old.portable_only), (&new.id, new.preview, new.portable_only));
        assert_ne!(old, new);
        e.reply(crate::engine::Reply::Media { epoch, request: old, source: Some(wav().into()) });
        assert_eq!(e.online.primary.as_ref().unwrap().0, new);
        assert!(e.audio.as_ref().unwrap().run_identity().is_none());
        e.online.primary.as_ref().unwrap().1.abort();
        e.reply(crate::engine::Reply::Media { epoch, request: new, source: Some(wav().into()) });
        assert!(e.online.primary.is_none());
        assert!(e.audio.as_ref().unwrap().run_identity().is_some());
    }

    async fn hello_then_local_pause(wins: bool, drops: bool) {
        let dir = Fixture(
            std::env::temp_dir()
                .canonicalize()
                .unwrap()
                .join(format!("spofify-claim-{}", uuid::Uuid::new_v4())),
        );
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&dir.0)
            .unwrap();
        let store = OfflineStore::open(&dir.0).unwrap();
        let mut e = crate::engine::tests::engine();
        let api = Arc::new(connection("http://127.0.0.1:1").unwrap());
        let session = api.test_session();
        let (mut audio, (mixer, mut samples)) = (
            Audio::new_account_scoped(
                session.clone(),
                e.shared_epoch.clone(),
                e.epoch,
                e.master_gain.clone(),
            ),
            rodio::mixer::mixer(1, 8000),
        );
        let bytes = wav();
        audio.use_test_output(mixer, "song".into(), bytes.clone());
        assert_eq!(
            audio.play("song".into(), bytes, 0.0, 1.0, Authority::offline()),
            Status::Ok
        );
        assert!(samples.by_ref().take(320).any(|s| s != 0.0));
        assert!(audio.facts().playing);
        e.audio = Some(audio);
        e.account = Some(Account {
            api,
            session: session.clone(),
        });
        e.local_store = Some(store);
        assert_eq!(
            e.local.replace(
                vec![LocalTrack {
                    id: "song".into(),
                    title: "Song".into(),
                    duration: 1.0,
                    ..Default::default()
                }],
                0,
                false
            ),
            Change::Start
        );
        let (socket, mut peer) = socket::Socket::test_peer(session, 11);
        e.socket = Some(socket);

        let (tx, rx) = tokio::sync::oneshot::channel();
        e.tick_with_event(Some(Event {
            generation: 0,
            kind: EventKind::ClaimRequested(tx),
            personal_stamp: None,
        }));
        let hello = rx.await.unwrap();
        assert!(hello.local.is_none() && hello.reattach_basis.is_none());
        let room = e.room_token().unwrap();
        let run = e.audio.as_ref().unwrap().run_identity();
        let mut snapshot = socket::Snapshot {
            session_id: "room".into(),
            connection_id: "test-connection".into(),
            revision: 1,
            owner: socket::Owner {
                generation: 7,
                device_id: Some(if wins {
                    e.device_id.clone()
                } else {
                    "other".into()
                }),
            },
            devices: vec![],
            player: e.saved_claim().unwrap(),
            observation: None,
            server_time_ms: 100,
        };
        if !wins {
            snapshot.player.cursor.occurrence = Some("other-visit".into());
        }
        peer.set_opening_snapshot(&snapshot, wins);
        if drops {
            peer.link.send_replace(Link::Disconnected);
        }
        e.tick_with_event(Some(Event {
            generation: 11,
            kind: EventKind::Snapshot(snapshot),
            personal_stamp: None,
        }));
        assert_eq!(e.audio.as_ref().unwrap().run_identity(), run);
        assert!(e.audio.as_ref().unwrap().facts().playing);
        assert!(e.local_store.is_some() && e.online.claim_pending.is_none());
        if !drops { assert!(e.online_generation().is_some()); }
        e.command(crate::engine::Command::GuardedToggle(room).scoped(e.epoch));
        assert!(!e.audio.as_ref().unwrap().facts().playing);
        assert!(!e.wants_pause());
        assert!(samples.by_ref().take(320).all(|v| v == 0.0));
        assert!(!std::iter::from_fn(|| peer.read()).any(|f| f["type"] == "reconcileClaim" || f["action"]["type"] == "pause"));
    }

    #[tokio::test]
    async fn shared_owner_snapshot_does_not_restart_private_pcm_or_replay_pause() {
        hello_then_local_pause(true, false).await;
    }

    #[tokio::test]
    async fn remote_owner_snapshot_does_not_retire_private_playback() {
        hello_then_local_pause(false, false).await;
    }

    #[tokio::test]
    async fn disconnected_opening_snapshot_does_not_retire_private_playback() {
        hello_then_local_pause(false, true).await;
    }

    async fn follower_takes_confirmed_queue_offline(manual: bool) {
        let dir = Fixture(
            std::env::temp_dir()
                .canonicalize()
                .unwrap()
                .join(format!("spofify-single-queue-{}", uuid::Uuid::new_v4())),
        );
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&dir.0)
            .unwrap();
        let mut e = crate::engine::tests::engine();
        let api = Arc::new(connection("http://127.0.0.1:1").unwrap());
        let session = api.test_session();
        let mut audio = Audio::new_account_scoped(
            session.clone(),
            e.shared_epoch.clone(),
            e.epoch,
            e.master_gain.clone(),
        );
        let (mixer, mut samples) = rodio::mixer::mixer(1, 8000);
        audio.use_test_output(mixer, "offline-a".into(), wav());
        e.audio = Some(audio);
        e.account = Some(Account {
            api,
            session: session.clone(),
        });
        let (socket, mut peer) = socket::Socket::test_peer(session, 12);
        e.socket = Some(socket);
        let state = State {
            queue: vec![socket::Track {
                id: "shared-b".into(),
                name: "Shared B".into(),
                key: 4,
                order: 1,
                duration: 1.0,
                artists: vec![],
                album_name: String::new(),
                album_id: String::new(),
                intent: socket::Intent::Low,
                entry_id: None,
                target_id: None,
                occurrence_id: None,
                playback_context: None,
            }],
            cursor: socket::Cursor {
                key: 4,
                occurrence: Some("shared-b-visit".into()),
                position_ms: 120,
                desired_playing: true,
            },
            repeat: socket::Repeat::One,
            shuffle: true,
            collection_first: 0,
        };
        let snap = socket::Snapshot {
            session_id: "room".into(),
            connection_id: "test-connection".into(),
            revision: 7,
            owner: socket::Owner {
                device_id: Some("producer".into()),
                generation: 8,
            },
            devices: vec![socket::Device {
                device_id: "producer".into(),
                name: "Producer".into(),
                can_play: true,
            }],
            player: state,
            observation: Some(socket::Observation {
                key: 4,
                occurrence: Some("shared-b-visit".into()),
                position_ms: 350,
                sampled_at_ms: now_ms(),
                status: socket::ObservationStatus::Playing,
            }),
            server_time_ms: now_ms(),
        };
        peer.set_opening_snapshot(&snap, false);
        e.tick_with_event(Some(Event {
            generation: 12,
            kind: EventKind::Snapshot(snap),
            personal_stamp: None,
        }));
        assert!(e.local.entries.is_empty());
        assert!(!e.audio.as_ref().unwrap().facts().playing);
        e.action(Action::Seek(0.8)); // unconfirmed; must not become the offline position
        assert!(std::iter::from_fn(|| peer.read()).any(|frame| frame["action"]["type"] == "seek"));
        if manual {
            e.set_offline(true);
        } else {
            peer.link.send_replace(Link::Disconnected);
            e.tick();
        }
        let mut view = PlayerView::default();
        e.project_online(&mut view);
        assert_eq!(view.track, "shared-b");
        assert_eq!(view.elapsed, 0.35);
        assert!(!view.playing && !view.wants_pause);
        assert!(view.shuffle);
        assert_eq!(view.repeat, "one");
        assert!(!e.online.detached_owner);
        let claim = e.current_claim();
        assert!(claim.reattach_basis.is_none());
        assert_eq!(claim.local.unwrap().current().unwrap().id, "shared-b");
        assert!(samples.by_ref().take(80).all(|sample| sample == 0.0));
        e.action(Action::Resume);
        let (request, task) = e.online.primary.as_ref().unwrap();
        let request = request.clone();
        assert_eq!(request.id, "shared-b");
        assert!(request.portable_only);
        task.abort();
        e.complete_online(request, Some(wav().into()));
        assert!(samples.by_ref().take(320).any(|s| s != 0.0));
        assert!(e.audio.as_ref().unwrap().facts().playing);
    }

    #[tokio::test]
    async fn follower_keeps_confirmed_b_paused_when_transport_drops_then_can_play_it() {
        follower_takes_confirmed_queue_offline(false).await;
    }

    #[tokio::test]
    async fn follower_keeps_confirmed_b_paused_in_manual_offline_then_can_play_it() {
        follower_takes_confirmed_queue_offline(true).await;
    }
}
