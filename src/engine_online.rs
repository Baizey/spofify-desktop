//! Online actor. Socket admission is not a server acknowledgement. Player state
//! comes from Go; only a native EOF can advance the shared cursor. A local
//! crossfade can overlap its known successor under the attached output grant.
#[path = "engine_local_start.rs"]
mod local_start;
#[cfg(test)]
#[path = "engine_online_pcm_tests.rs"]
mod pcm_tests;
use super::{
    CreditView, Engine, LOAD_BUDGET, PlayerView, QueueEntryView, REPORT_INTERVAL, Reply,
    artist_subtitle, context_kind, credit_views, crossfade, metadata_id, output,
};
use crate::{
    DeviceRow,
    api::ApiError,
    audio::CROSSFADE_SECONDS,
    offline::{self, OnlineMedia},
    personal::{Command as PersonalCommand, Update as PersonalUpdate},
    player::{Action, Status},
    socket::{
        self, Cursor, Event, EventKind, FactError, Generation, Link, OutputAuthority, Outputs,
        State,
    },
};
use std::{
    collections::HashSet,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

pub(super) struct Work {
    generation: Generation,
    cursor: Cursor,
    id: String,
    duration: f64,
}

pub(super) struct Execution {
    cursor: Cursor,
    permit: output::RunPermit,
    pending_crossfade: Option<CrossfadeConfirmation>,
    status: Status,
    last_fact: Option<Instant>,
}

#[cfg(target_os = "linux")]
impl Execution {
    /// A detached run may still be audible under the existing grace permit.
    pub(super) fn media_pause_token(
        &self,
        displayed: &Cursor,
        room: u64,
        revision: u64,
    ) -> Option<String> {
        (output::same_execution(&self.cursor, displayed) && self.permit.granted()).then(|| {
            let cursor = &self.cursor;
            format!(
                "detached:{room}:{revision}:{}:{}:{}:{}",
                cursor.key, cursor.chain, cursor.visit, cursor.run
            )
        })
    }
}

struct PreviewKey {
    generation: Generation,
    from: Cursor,
    to: Cursor,
    id: String,
}
impl PreviewKey {
    fn matches_landing(&self, generation: Generation, from: &Cursor, to: &Cursor) -> bool {
        self.generation == generation
            && output::same_execution(&self.from, from)
            && output::same_execution(&self.to, to)
    }
}
struct PreviewPlayback {
    permit: output::RunPermit,
    confirmation: CrossfadeConfirmation,
}
struct CrossfadeConfirmation {
    generation: Generation,
    confirmed: Arc<AtomicBool>,
}

struct Advance {
    from: Cursor,
    to: Cursor,
}

#[derive(Default)]
pub(super) struct Online {
    pub state: Option<State>,
    pub revision: u64,
    pub work: Option<Work>,
    pub execution: Option<Execution>,
    connection: String,
    outputs: Outputs,
    local_start: Option<local_start::Start>,
    receipts: local_start::Receipts,
    // We saw this very socket retire and queued Release after stopping it.
    // An opening/unknown empty owner never establishes vacancy.
    released_here: bool,
    generation: Option<Generation>,
    personal_generation: Option<Generation>,
    repair: Option<Generation>,
    player_gap: bool,
    observed_at: Option<Instant>,
    advance: Option<Advance>,
    stopped: Option<Cursor>,
    lookahead: Option<crossfade::Lookahead<PreviewKey, PreviewPlayback>>,
    suppress_crossfade: Option<Cursor>,
}

impl Online {
    pub fn cursor(&self) -> Option<&Cursor> {
        self.advance
            .as_ref()
            .map(|advance| &advance.to)
            .or_else(|| self.state.as_ref().map(|s| &s.cursor))
    }
    pub fn wants_pause(&self) -> bool {
        self.cursor().is_some_and(|cursor| {
            cursor.desired
                && !self.stop_for(cursor)
                && self
                    .execution
                    .as_ref()
                    .is_none_or(|e| e.status == Status::Ok)
        })
    }
    fn is_owner(&self) -> bool {
        !self.connection.is_empty() && self.outputs.owner == self.connection
    }
    pub fn starting_locally(&self) -> bool {
        self.local_start.is_some()
    }
    pub fn clear_local_start(&mut self) {
        self.local_start = None;
    }
    fn locally_vacant(&self) -> bool {
        self.released_here
            && self.outputs.owner.is_empty()
            && self
                .outputs
                .devices
                .iter()
                .all(|d| !d.can_play || d.id == self.connection)
            && self
                .outputs
                .devices
                .iter()
                .any(|d| d.can_play && d.id == self.connection)
    }
    fn stop_for(&self, cursor: &Cursor) -> bool {
        self.stopped
            .as_ref()
            .is_some_and(|c| output::same_execution(c, cursor))
    }
    /// An observation from before the already-reported EOF cannot rewind a
    /// running successor. Canonical correction and newer snapshots can.
    fn adopt_cursor(&mut self, cursor: &Cursor, observed: bool) {
        if let Some(advance) = &self.advance {
            if observed && output::same_execution(&advance.from, cursor) {
                return;
            }
            self.advance = None;
        }
        if !self.stop_for(cursor) {
            self.stopped = None;
        }
        if self
            .suppress_crossfade
            .as_ref()
            .is_some_and(|c| !output::same_execution(c, cursor))
        {
            self.suppress_crossfade = None;
        }
    }
}

impl Engine {
    pub(super) fn online_generation(&self) -> Option<Generation> {
        let generation = self.online.generation?;
        (self.network_allowed()
            && self.link() == Link::Connected(generation)
            && !self.online.player_gap)
            .then_some(generation)
    }
    fn grant(&self) -> Option<OutputAuthority> {
        let socket = self.socket.as_ref()?;
        if socket.authority.has_changed().is_err() {
            return None;
        }
        (*socket.authority.borrow()).filter(|g| g.permits(Instant::now()))
    }
    fn attached(&self) -> Option<OutputAuthority> {
        let generation = self.online_generation()?;
        self.grant()
            .filter(|g| self.online.is_owner() && g.attached && g.generation == generation)
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
            || self.online_generation() != Some(generation)
            || self.online.personal_generation != Some(generation)
        {
            return;
        }
        if !self.personal.authorizes(epoch, generation, command)
            || !self
                .socket
                .as_ref()
                .is_some_and(|s| s.personal(generation, command).is_ok())
        {
            self.personal.refused(epoch, generation);
        }
    }
    fn repair(&mut self, generation: Generation) {
        // One repair per socket generation. Do not spin on a malformed peer
        // or repeat any user mutation while waiting for a fresh projection.
        if self.online.repair == Some(generation) {
            return;
        }
        self.online.repair = Some(generation);
        if let Some(socket) = &self.socket {
            let _ = socket.sync(generation);
        }
    }
    fn player_gap(&mut self, generation: Generation) {
        self.online.player_gap = true;
        self.online.advance = None;
        self.online.revision = self.online.revision.wrapping_add(1);
        if self.local_store.is_none() {
            self.cancel_work();
            self.stop();
        }
        self.action_status = "Playback state is out of sync.";
        self.repair(generation);
    }
    fn accept_outputs(&mut self, generation: Generation, outputs: Outputs) {
        let retiring = self.online.is_owner() && outputs.owner.is_empty();
        if !outputs.owner.is_empty() {
            self.online.released_here = false;
        }
        self.online.outputs = outputs;
        if retiring {
            if self.local_store.is_none() {
                self.cancel_work();
                self.stop();
            }
            self.online.advance = None;
            self.online.released_here = self
                .socket
                .as_ref()
                .is_some_and(|socket| socket.command(generation, socket::Command::Release).is_ok());
        }
        if self.online.is_owner() || self.online.locally_vacant() {
            if let Some(start) = &mut self.online.local_start {
                start.output_confirmed();
            }
        } else {
            self.cancel_local_start();
        }
    }
    fn handle_event(&mut self, event: Event) {
        let generation = event.generation;
        if self.link() != Link::Connected(generation) {
            return;
        }
        if self
            .online
            .local_start
            .as_ref()
            .is_some_and(|s| s.output_pulse.is_some())
            && !matches!(&event.kind, EventKind::Outputs(_))
        {
            self.cancel_local_start();
        }
        match event.kind {
            EventKind::Snapshot {
                connection_id,
                player,
                outputs,
            } => {
                self.cancel_local_start();
                self.online.released_here = false;
                let fresh = self.online.generation != Some(generation);
                if fresh {
                    self.online.receipts = local_start::Receipts::default();
                    self.disconnect_personal();
                    self.online.generation = Some(generation);
                    self.online.connection = connection_id;
                    self.online.personal_generation = Some(generation);
                    self.personal.begin(self.epoch, generation);
                }
                if !output::valid(&player) {
                    self.player_gap(generation);
                    return;
                }
                if self.online.player_gap {
                    self.action_status = "";
                }
                self.online.player_gap = false;
                self.online.revision = self.online.revision.wrapping_add(1);
                // While awaiting actual reattach confirmation, the opening
                // snapshot may still contain the pre-EOF base. Keep the local
                // successor only under the prior owner's live grace.
                let waiting = fresh
                    && self
                        .grant()
                        .is_some_and(|g| g.generation != generation || !g.attached);
                self.online.adopt_cursor(&player.cursor, waiting);
                self.online.observed_at = Some(Instant::now());
                self.online.state = Some(player);
                if fresh {
                    self.online.outputs = outputs;
                } else {
                    self.accept_outputs(generation, outputs);
                }
            }
            _ if self.online.generation != Some(generation) => {}
            EventKind::Player(delta) => {
                if self.online.player_gap {
                    return;
                }
                let Some(state) = &mut self.online.state else {
                    return;
                };
                let Ok(changed) = output::apply(state, &delta) else {
                    self.player_gap(generation);
                    return;
                };
                if changed {
                    self.online.revision = self.online.revision.wrapping_add(1);
                }
                let own = delta.source.as_deref() == Some(self.online.connection.as_str());
                if own {
                    self.online.receipts.received(&delta, state);
                    self.action_status = if delta.queue.is_none() {
                        "That selection is unavailable."
                    } else {
                        ""
                    };
                }
                let vacant = self.online.locally_vacant();
                if let (Some(start), Some(state)) =
                    (&mut self.online.local_start, &self.online.state)
                    && !start.observe(&delta, state, own, vacant)
                {
                    self.cancel_local_start();
                }
                if let Some(cursor) = &delta.cursor {
                    self.online.adopt_cursor(cursor, delta.observed);
                    self.online.observed_at = Some(Instant::now());
                }
            }
            EventKind::Outputs(outputs) => self.accept_outputs(generation, outputs),
            EventKind::Personal(value) => {
                let Ok(update) = serde_json::from_value::<PersonalUpdate>(value) else {
                    self.personal.gap(self.epoch, generation);
                    self.repair(generation);
                    return;
                };
                match self.personal.receive(self.epoch, generation, update) {
                    Ok(Some(command)) => self.send_personal(self.epoch, generation, &command),
                    Ok(None) => {}
                    Err(_) => self.repair(generation),
                }
            }
        }
    }
    pub(super) fn drain_online_events(&mut self, first: Option<Event>) {
        if self
            .online
            .personal_generation
            .is_some_and(|g| self.link() != Link::Connected(g))
        {
            self.disconnect_personal();
        }
        if let Some(first) = first {
            self.handle_event(first);
        }
        for _ in 0..16 {
            let Some(event) = self.socket.as_mut().and_then(|s| s.events.try_recv().ok()) else {
                break;
            };
            self.handle_event(event);
        }
    }
    pub(super) fn tick_online(&mut self, first: Option<Event>) {
        if self.link() == Link::Expired {
            self.expire();
            return;
        }
        self.drain_online_events(first);
        self.finish_initial_play();
        if self.local_store.is_some() {
            let room_needs_output = self.attached().is_some()
                && self
                    .online
                    .cursor()
                    .is_some_and(|c| c.desired && !self.online.stop_for(c));
            if !room_needs_output {
                self.tick_saved();
                return;
            }
            self.leave_saved_queue();
        }
        self.reconcile_online();
        self.tick_online_crossfade();
        if self.sample_online(true, false) {
            self.reconcile_online();
        }
    }
    pub(super) fn pause_room_for_saved(&mut self) -> bool {
        if self.online.is_owner()
            && self
                .online
                .cursor()
                .is_some_and(|c| c.desired && !self.online.stop_for(c))
        {
            self.online_action(Action::Pause);
            return self.action_status.is_empty();
        }
        true
    }
    #[cfg(target_os = "linux")]
    pub(super) fn pause_detached_media(&mut self) {
        let Some(execution) = &self.online.execution else {
            return;
        };
        // This path must never submit a room Pause: the transport can
        // reattach concurrently while this actor is stopping its old run.
        self.online.stopped = Some(execution.cursor.clone());
        self.cancel_work();
        self.stop();
        if let (Some(grant), Some(socket)) = (self.grant(), &self.socket) {
            let _ = socket.observe_local(grant.generation, Cursor::default());
        }
    }
    pub(super) fn online_action(&mut self, action: Action) {
        if !self.network_allowed() {
            return;
        }
        let Some(command) = action.wire(&self.device_id) else {
            return;
        };
        if matches!(
            command,
            socket::Command::Play { .. }
                | socket::Command::Resume
                | socket::Command::Pause
                | socket::Command::Close
                | socket::Command::Jump { .. }
                | socket::Command::Select { .. }
        ) {
            self.pending_initial_play = None;
        }
        // Keep one explicit catalog Play that arrived before the first hello.
        // Never defer selection/transport or replay a command on reconnect.
        if self.link() == Link::Connecting
            && self.online.generation.is_none()
            && matches!(command, socket::Command::Play { .. })
        {
            self.pending_initial_play =
                Some((Instant::now() + super::INITIAL_PLAY_TIMEOUT, command));
            self.action_status = "Connecting playback controls…";
            return;
        }
        // A later queue edit can itself drain the opening snapshot. Send any
        // deferred Play first so it cannot erase that edit by arriving later.
        self.finish_initial_play();
        self.send_online_command(command, true);
    }
    fn send_online_command(&mut self, command: socket::Command, explicit: bool) {
        let replaced_start = self.online.starting_locally();
        if matches!(
            command,
            socket::Command::Play { .. }
                | socket::Command::Resume
                | socket::Command::Pause
                | socket::Command::Close
                | socket::Command::Seek { .. }
                | socket::Command::Next
                | socket::Command::Previous
                | socket::Command::Jump { .. }
                | socket::Command::Select { .. }
        ) {
            self.cancel_local_start();
        }
        if matches!(command, socket::Command::Pause | socket::Command::Close)
            && (self.online.is_owner()
                || self.online.execution.is_some()
                || self.online.work.is_some())
        {
            // Preserve the real playback tail before Pause. Never send an end
            // as a consequence of a physical stop.
            self.sample_online(false, true);
            self.online.stopped = self.online.cursor().cloned();
            self.cancel_work();
            self.stop();
            if let (Some(grant), Some(socket)) = (self.grant(), &self.socket) {
                let _ = socket.observe_local(grant.generation, Cursor::default());
            }
        }
        let Some(generation) = self.online_generation() else {
            self.action_status = "Playback controls are disconnected.";
            return;
        };
        let changes_run = matches!(
            command,
            socket::Command::Play { .. }
                | socket::Command::Next
                | socket::Command::Previous
                | socket::Command::Seek { .. }
                | socket::Command::Jump { .. }
                | socket::Command::Select { .. }
        );
        let socket = self.socket.as_ref().expect("live generation has a socket");
        let start = if explicit
            && !replaced_start
            && socket.events.is_empty()
            && self.audio.is_some()
            && self.local_store.is_none()
            && self.online.execution.is_none()
            && self.online.work.is_none()
            && self
                .grant()
                .is_none_or(|g| g.attached && g.generation == generation)
            && (self.online.is_owner() || self.online.locally_vacant())
        {
            self.online.state.as_ref().and_then(|state| {
                let resume_through = self.attached().map_or(0, |g| g.resume_through);
                if !self.online.receipts.may_start(&command, resume_through) {
                    return None;
                }
                local_start::Start::new(
                    &command,
                    state,
                    generation,
                    socket.link.clone(),
                    socket.authority.clone(),
                    socket.direct_start.clone(),
                )
            })
        } else {
            None
        };
        self.action_status = if let Ok(resume) = socket.command(generation, command.clone()) {
            self.online.receipts.sent(&command, resume);
            if let Some(mut start) = start {
                // This cache contains only the current account's previously
                // authenticated media. Downloaded files always re-verify.
                if let Some(bytes) = self.audio.as_ref().and_then(|a| a.cached(&start.id)) {
                    start.ready = Some(OnlineMedia {
                        bytes,
                        downloaded: false,
                    });
                } else {
                    self.load_online_media(start.id.clone());
                }
                self.online.local_start = Some(start);
            }
            if changes_run && self.crossfade_enabled.load(Ordering::Acquire) {
                // Don't (re)start an overlap of the run a manual navigation
                // is about to replace. A rejected choice can simply use EOF.
                self.online.suppress_crossfade = self.online.cursor().cloned();
                self.cancel_online_crossfade();
            }
            ""
        } else {
            "The playback action was not sent."
        };
    }
    fn finish_initial_play(&mut self) {
        let Some((deadline, _)) = self.pending_initial_play.as_ref() else {
            return;
        };
        if Instant::now() >= *deadline
            || !matches!(self.link(), Link::Connecting | Link::Connected(_))
        {
            self.pending_initial_play = None;
            self.action_status = "The playback action was not sent.";
        } else if self.online_generation().is_some() {
            let (_, command) = self.pending_initial_play.take().unwrap();
            self.send_online_command(command, false);
        }
    }
    fn reconcile_online(&mut self) {
        if self.reconcile_local_start() {
            return;
        }
        let grant = self.grant();
        if grant.is_none() && self.online.advance.take().is_some() {
            self.online.revision = self.online.revision.wrapping_add(1);
        }
        let attached = self.attached();
        if let Some(execution) = &mut self.online.execution
            && let Some(pending) = &execution.pending_crossfade
        {
            if attached.is_none_or(|g| g.generation != pending.generation) {
                // A queued EOF might not have reached Go. Never export the
                // speculative successor or let it qualify for detached grace.
                if self.online.advance.take().is_some() {
                    self.online.revision = self.online.revision.wrapping_add(1);
                }
                self.stop();
            } else if self.online.advance.is_none()
                && self
                    .online
                    .state
                    .as_ref()
                    .is_some_and(|state| output::same_execution(&state.cursor, &execution.cursor))
            {
                pending.confirmed.store(true, Ordering::Release);
                execution.pending_crossfade = None;
            }
        }
        let cursor = self.online.cursor().cloned();
        let current = cursor.as_ref().filter(|cursor| {
            cursor.desired
                && !self.online.stop_for(cursor)
                && self
                    .online
                    .state
                    .as_ref()
                    .is_some_and(|s| s.queue.iter().any(|t| t.key == cursor.key))
        });
        if let Some(work) = &self.online.work
            && (attached.is_none_or(|g| g.generation != work.generation)
                || current.is_none_or(|c| !output::same_execution(c, &work.cursor)))
        {
            self.cancel_work();
        }
        if let Some(execution) = &self.online.execution {
            let keep = !self.online.player_gap
                && grant.is_some()
                && execution.permit.granted()
                && self.audio.as_ref().is_some_and(|audio| !audio.was_fenced())
                && current.is_some_and(|c| output::same_execution(c, &execution.cursor))
                && if let Some(grant) = attached {
                    execution.permit.adopt(grant)
                } else {
                    // A not-yet-audible source or failed start isn't grace.
                    self.audio.as_ref().is_some_and(|audio| audio.has_started())
                };
            if !keep {
                self.stop();
            }
        }
        let (Some(grant), Some(cursor)) = (attached, current) else {
            return;
        };
        if self.online.execution.is_some()
            || self.online.work.is_some()
            || self
                .socket
                .as_ref()
                .is_some_and(|socket| !socket.events.is_empty())
        {
            return;
        }
        let Some(track) = self
            .online
            .state
            .as_ref()
            .and_then(|s| s.queue.iter().find(|t| t.key == cursor.key))
        else {
            return;
        };
        let work = Work {
            generation: grant.generation,
            cursor: cursor.clone(),
            id: track.id.clone(),
            duration: track.duration,
        };
        self.cancel_work();
        self.load_online_media(work.id.clone());
        self.online.work = Some(work);
    }
    fn load_online_media(&mut self, id: String) {
        if let Some(account) = &self.account {
            let (api, session, slot, replies, id) = (
                account.api.clone(),
                account.session.clone(),
                self.local_read_slot.clone(),
                self.results.clone(),
                id,
            );
            let cached = self.audio.as_ref().and_then(|audio| audio.cached(&id));
            let (epoch, operation) = (self.epoch, self.operation);
            self.media = Some(tokio::spawn(async move {
                let result = tokio::time::timeout(
                    LOAD_BUDGET,
                    offline::online_media(&api, &session, slot, &id, cached),
                )
                .await
                .unwrap_or(Err(ApiError {
                    message: "Media could not be loaded in time.",
                    expired: false,
                    status: 0,
                }));
                let _ = replies
                    .send(Reply::Media {
                        epoch,
                        operation,
                        result,
                    })
                    .await;
            }));
        }
    }
    pub(super) fn complete_online(
        &mut self,
        operation: u64,
        result: Result<OnlineMedia, ApiError>,
    ) {
        self.drain_online_events(None);
        if !self.network_allowed() {
            if result.as_ref().is_err_and(|e| e.expired)
                && self.current()
                && !self.explicit_offline()
            {
                self.expire();
            }
            return;
        }
        if self.operation != operation {
            return;
        }
        if result.as_ref().is_err_and(|e| e.expired) {
            self.expire();
            return;
        }
        if let Some(start) = &mut self.online.local_start {
            match result {
                Ok(media) => start.ready = Some(media),
                Err(_) => self.cancel_local_start(),
            }
            self.reconcile_online();
        } else if let Some(work) = self.online.work.take() {
            self.start_online(work, result.map_err(|_| Status::Media));
        }
    }
    fn cancel_local_start(&mut self) {
        if self.online.starting_locally() {
            self.cancel_work();
            self.stop();
        }
    }
    /// Return true only while the separate local start owns native audio/load.
    /// Promotion never stops/re-appends the sink or creates a new decoder.
    fn reconcile_local_start(&mut self) -> bool {
        let Some(start) = self.online.local_start.as_ref() else {
            return false;
        };
        if self.online_generation() != Some(start.generation)
            || !start.permit.live()
            || !(self.online.is_owner() || self.online.locally_vacant())
            || start.started
                && self
                    .audio
                    .as_ref()
                    .is_none_or(|a| a.was_fenced() || a.facts().ended)
        {
            self.cancel_local_start();
            return false;
        }
        let Some(socket) = &self.socket else {
            return false;
        };
        if !socket.events.is_empty() || start.output_pulse.is_some() {
            return true;
        }
        let grant = self.attached();
        let start = self.online.local_start.as_mut().expect("checked above");
        if let (Some(grant), Some(cursor)) = (grant, start.canonical.as_ref()) {
            if start.started {
                let permit = output::RunPermit::new(socket.authority.clone(), grant);
                if start
                    .permit
                    .commit(permit.source(socket.link.clone(), grant.generation))
                {
                    self.online.execution = Some(Execution {
                        cursor: cursor.clone(),
                        permit,
                        pending_crossfade: None,
                        status: Status::Ok,
                        last_fact: None,
                    });
                    self.online.clear_local_start();
                    return false;
                }
                self.cancel_local_start();
                return false;
            }
            // Go won the race with media loading. Reuse the same fetch as
            // canonical work instead of starting it or opening audio twice.
            let mut start = self.online.local_start.take().unwrap();
            let work = Work {
                generation: grant.generation,
                cursor: start.canonical.take().unwrap(),
                id: start.id,
                duration: self
                    .online
                    .state
                    .as_ref()
                    .and_then(State::current)
                    .map_or(start.duration, |t| t.duration),
            };
            if let Some(media) = start.ready {
                self.start_online(work, Ok(media));
            } else {
                self.online.work = Some(work);
            }
            return false;
        }
        if let Some(OnlineMedia { bytes, downloaded }) = start.ready.take() {
            let audio = self.audio.as_mut().expect("local start has audio");
            let status = audio.play(
                start.id.clone(),
                bytes,
                start.elapsed,
                start.duration,
                start.permit.source(),
            );
            if downloaded {
                audio.discard_cache();
            }
            if status != Status::Ok {
                self.cancel_local_start();
                return false;
            }
            start.started = true;
        }
        true
    }
    fn start_online(&mut self, work: Work, media: Result<OnlineMedia, Status>) {
        let Some(grant) = self.attached().filter(|g| g.generation == work.generation) else {
            return;
        };
        if self.online.stop_for(&work.cursor)
            || self
                .online
                .cursor()
                .is_none_or(|c| !output::same_execution(c, &work.cursor))
        {
            return;
        }
        let (Some(socket), Some(audio)) = (&self.socket, &mut self.audio) else {
            return;
        };
        let permit = output::RunPermit::new(socket.authority.clone(), grant);
        audio.anchor(work.cursor.elapsed, work.duration);
        let status = match media {
            Ok(OnlineMedia { bytes, downloaded }) => {
                let status = audio.play(
                    work.id,
                    bytes,
                    work.cursor.elapsed,
                    work.duration,
                    permit.source(socket.link.clone(), grant.generation),
                );
                if downloaded {
                    // A later admission must re-verify the file in case it was removed.
                    audio.discard_cache();
                }
                status
            }
            Err(status) => status,
        };
        if status != Status::Rejected {
            self.online.execution = Some(Execution {
                cursor: work.cursor,
                permit,
                pending_crossfade: None,
                status,
                last_fact: None,
            });
        }
    }
    pub(super) fn cancel_online_crossfade(&mut self) {
        self.online.lookahead = None;
        if let Some(audio) = &mut self.audio {
            audio.cancel_crossfade();
        }
    }
    fn tick_online_crossfade(&mut self) {
        if !self.crossfade_enabled.load(Ordering::Acquire) {
            self.cancel_online_crossfade();
            return;
        }
        let Some(audio) = &self.audio else { return };
        let remaining = audio.crossfade_remaining();
        if self.online.lookahead.is_none()
            && remaining.is_none_or(|s| s > crossfade::ONLINE_LOOKAHEAD)
        {
            return;
        }
        let target = self.attached().and_then(|grant| {
            let execution = self.online.execution.as_ref()?;
            let state = self.online.state.as_ref()?;
            let from = &execution.cursor;
            if !output::same_execution(&state.cursor, from)
                || self.online.advance.is_some()
                || execution.status != Status::Ok
                || self
                    .online
                    .suppress_crossfade
                    .as_ref()
                    .is_some_and(|c| output::same_execution(c, from))
            {
                return None;
            }
            let to = output::successor(state, from)?;
            if !to.desired || to.key == from.key {
                return None;
            }
            let id = &state.queue.iter().find(|t| t.key == to.key)?.id;
            Some((grant, from, to, id))
        });
        let Some((grant, from, to, id)) = target else {
            self.cancel_online_crossfade();
            return;
        };
        let audio = self.audio.as_mut().unwrap();
        if self.online.lookahead.as_ref().is_some_and(|look| {
            !look.key.matches_landing(grant.generation, from, &to) || look.key.id != *id
        }) {
            self.online.lookahead = None;
            audio.cancel_crossfade();
        }
        audio.maintain_crossfade();
        let Some(remaining) = remaining else { return };
        let Some(socket) = &self.socket else { return };
        // Drain already queued canonical edits before admitting a new source,
        // without restarting a fade just because unrelated updates are busy.
        if !socket.events.is_empty() {
            return;
        }
        if self.online.lookahead.is_none() {
            let Some(account) = &self.account else { return };
            let (api, session, media_id) =
                (account.api.clone(), account.session.clone(), id.clone());
            let slot = self.local_preview_slot.clone();
            let cached = audio.cached(&media_id);
            let key = PreviewKey {
                generation: grant.generation,
                from: from.clone(),
                to,
                id: id.clone(),
            };
            self.online.lookahead = Some(crossfade::Lookahead::new(key, async move {
                let media = tokio::time::timeout(
                    LOAD_BUDGET,
                    offline::online_media(&api, &session, slot, &media_id, cached),
                )
                .await
                .ok()?
                .ok()?;
                Some(media.bytes)
            }));
        }
        if remaining <= CROSSFADE_SECONDS
            && let Some(look) = &mut self.online.lookahead
            && let Some(bytes) = look.take_bytes()
        {
            let permit = output::RunPermit::new(socket.authority.clone(), grant);
            let confirmation = CrossfadeConfirmation {
                generation: grant.generation,
                confirmed: Arc::default(),
            };
            let authority = permit.preview_source(
                socket.link.clone(),
                grant.generation,
                Arc::clone(&confirmation.confirmed),
            );
            if audio.start_crossfade(look.key.id.clone(), bytes, authority) {
                look.started(PreviewPlayback {
                    permit,
                    confirmation,
                });
            }
        }
    }
    fn keep_online_crossfade(
        &mut self,
        generation: Generation,
        from: &Cursor,
        to: &Cursor,
    ) -> bool {
        let Some(look) = self.online.lookahead.take() else {
            return false;
        };
        if self.attached().is_none_or(|g| g.generation != generation)
            || !self.crossfade_enabled.load(Ordering::Acquire)
            || !look.key.matches_landing(generation, from, to)
        {
            return false;
        }
        let Some(playing) = look.into_playing() else {
            return false;
        };
        let Some(audio) = &mut self.audio else {
            return false;
        };
        if !audio.promote_crossfade() {
            return false;
        }
        // A preview can be disk-backed; revalidate on the next admission.
        audio.discard_cache();
        self.online.execution = Some(Execution {
            cursor: to.clone(),
            permit: playing.permit,
            pending_crossfade: Some(playing.confirmation),
            status: Status::Ok,
            last_fact: None,
        });
        true
    }
    /// Publish actual native position to the transport even within transient
    /// grace. Facts are admitted only by the current attached generation.
    fn sample_online(&mut self, allow_end: bool, force: bool) -> bool {
        let grant = self.grant();
        let (Some(grant), Some(socket), Some(audio), Some(execution)) =
            (grant, &self.socket, &self.audio, &self.online.execution)
        else {
            return false;
        };
        if !execution.permit.granted()
            || execution.status == Status::Media
            || !self.network_allowed()
        {
            return false;
        }
        let facts = audio.facts();
        let now = Instant::now();
        let actual = Cursor {
            elapsed: facts.elapsed,
            playing: facts.playing,
            ..execution.cursor.clone()
        };
        if execution.pending_crossfade.is_none() {
            let _ = socket.observe_local(grant.generation, actual);
        }
        let generation = grant.generation;
        if !grant.attached
            || self.link() != Link::Connected(generation)
            || self.online.generation != Some(generation)
            || !self.online.is_owner()
            || self.online.player_gap
            || self.online.advance.is_some()
        {
            return false;
        }
        if allow_end && facts.ended && self.online.lookahead.is_some() && !socket.events.is_empty()
        {
            // A bounded drain can leave a queue correction pending. Don't
            // turn the preview up or let it continue alone while we catch up.
            // Retaining its attempted key prevents a restart of that intro.
            if let Some(audio) = &mut self.audio {
                audio.cancel_crossfade();
            }
            return false;
        }
        let Some(execution) = &mut self.online.execution else {
            return false;
        };
        let Some(state) = &self.online.state else {
            return false;
        };
        if !output::same_execution(&state.cursor, &execution.cursor) {
            return false;
        }
        let next = if allow_end && facts.ended && execution.status == Status::Ok {
            output::successor(state, &execution.cursor)
        } else {
            None
        };
        if !force
            && next.is_none()
            && execution.last_fact.is_some_and(|at| {
                execution.status == Status::Blocked || now.duration_since(at) < REPORT_INTERVAL
            })
        {
            return false;
        }
        if facts.ended && next.is_none() {
            return false;
        }
        let landed = next.as_ref().map(|c| if c.desired { c.key } else { 0 });
        let report = output::fact(
            &execution.cursor,
            facts.elapsed,
            facts.playing,
            facts.loading,
            landed,
            (execution.status == Status::Blocked).then_some(FactError::Blocked),
        );
        if socket.fact(generation, report).is_err() {
            if next.is_some() {
                self.cancel_online_crossfade();
            }
            return false;
        }
        execution.last_fact = Some(now);
        if let Some(to) = next {
            let from = execution.cursor.clone();
            if !self.keep_online_crossfade(generation, &from, &to) {
                self.stop();
            }
            self.online.advance = Some(Advance { from, to });
            self.online.revision = self.online.revision.wrapping_add(1);
            return true;
        }
        false
    }
    pub(super) fn project_online(&self, p: &mut PlayerView) {
        let live = self.online_generation().is_some();
        let outputs = &self.online.outputs;
        let active = outputs
            .devices
            .iter()
            .find(|d| live && d.id == outputs.owner);
        p.output = match active {
            Some(device) => device.name.clone(),
            None if !live => "Output unavailable".into(),
            None if !outputs.owner.is_empty() => "Output disconnected".into(),
            None => "No output selected".into(),
        };
        let Some(state) = &self.online.state else {
            return;
        };
        let cursor = self.online.cursor().unwrap_or(&state.cursor);
        let position = state.queue.iter().position(|t| t.key == cursor.key);
        p.shuffle = state.shuffle;
        p.repeat = match state.repeat {
            socket::Repeat::Off => "off",
            socket::Repeat::All => "all",
            socket::Repeat::One => "one",
        };
        p.transport = live && position.is_some();
        p.control = p.transport
            || self
                .online
                .execution
                .as_ref()
                .is_some_and(|e| e.permit.granted());
        p.elapsed = cursor.elapsed;
        if let Some(track) = position.and_then(|i| state.queue.get(i)) {
            p.title = track.name.clone();
            p.credits = credit_views(&track.artists);
            p.detail = artist_subtitle(&p.credits);
            p.track = track.id.clone();
            p.album = metadata_id(&track.album_id).into();
            p.album_name = track.album_name.clone();
            p.duration = track.duration;
            // The credit links already reach this page; don't repeat it as a source link.
            if let Some(context) = &track.playback_context
                && !(matches!(context.kind, socket::ContextKind::Artist)
                    && p.credits
                        .iter()
                        .any(|a: &CreditView| !a.id.is_empty() && a.id == context.id))
            {
                p.context_kind = context_kind(context.kind);
                p.context_id = context.id.clone();
                p.context_name = context.name.clone();
            }
        }
        if let (Some(execution), Some(audio)) = (&self.online.execution, &self.audio) {
            let facts = audio.facts();
            p.playing = facts.playing;
            p.elapsed = facts.elapsed;
            if facts.duration > 0.0 {
                p.duration = facts.duration;
            }
            if p.status.is_empty() {
                p.status = match execution.status {
                    Status::Blocked => "Audio output is unavailable on this desktop.",
                    Status::Media => "This desktop could not play the audio.",
                    _ => "",
                };
            }
        } else if active.is_some() && !self.online.is_owner() && cursor.playing {
            p.playing = true;
            if let Some(at) = self.online.observed_at
                && p.duration > 0.0
            {
                p.elapsed = (p.elapsed + at.elapsed().as_secs_f64().min(3.0)).min(p.duration);
            }
        }
        p.queue = state
            .queue
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let credits = credit_views(&t.artists);
                QueueEntryView {
                    key: t.key.to_string(),
                    track_id: t.id.clone(),
                    title: t.name.clone(),
                    subtitle: artist_subtitle(&credits),
                    credits,
                    album_id: metadata_id(&t.album_id).into(),
                    album_name: t.album_name.clone(),
                    current: position == Some(i),
                    upcoming: position.is_some_and(|pos| i > pos),
                }
            })
            .collect();
        let mut seen = HashSet::new();
        p.devices = self
            .online
            .outputs
            .devices
            .iter()
            .filter(|d| seen.insert(&d.device_id))
            .map(|d| {
                let selected = active.is_some_and(|owner| owner.device_id == d.device_id);
                let shown = if selected { active.unwrap_or(d) } else { d };
                DeviceRow {
                    id: d.device_id.clone().into(),
                    name: shown.name.clone().into(),
                    selected,
                    available: live,
                    can_play: shown.can_play,
                }
            })
            .collect();
        if p.status.is_empty() {
            if self.online.starting_locally() {
                p.status = "Starting playback…";
            } else if self.online.work.is_some() {
                p.status = "Loading audio…";
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cursor(key: u64, visit: u64, run: u64) -> Cursor {
        Cursor {
            key,
            visit,
            run,
            chain: "chain".into(),
            desired: true,
            ..Cursor::default()
        }
    }

    #[test]
    fn empty_owner_only_allows_a_locally_witnessed_release_with_no_competitor() {
        let mut online = Online {
            connection: "local".into(),
            ..Online::default()
        };
        let outputs = |owner, other| {
            let mut devices = vec![serde_json::json!({
                "id": "local", "deviceId": "desktop", "name": "Desktop", "canPlay": true
            })];
            if other {
                devices.push(serde_json::json!({
                    "id": "remote", "deviceId": "phone", "name": "Phone", "canPlay": true
                }));
            }
            serde_json::from_value(serde_json::json!({"owner":owner,"devices":devices})).unwrap()
        };
        online.outputs = outputs("", false);
        assert!(
            !online.locally_vacant(),
            "opening empty may be startup drain or an unseen handoff"
        );
        online.released_here = true;
        assert!(online.locally_vacant());
        online.outputs = outputs("", true);
        assert!(
            !online.locally_vacant(),
            "a handoff may still elect another eligible output"
        );
        online.outputs = outputs("detached-remote", false);
        assert!(
            !online.locally_vacant(),
            "missing roster row does not imply a detached owner is gone"
        );
    }

    #[test]
    fn disconnected_projection_does_not_keep_the_cached_device_selected() {
        let mut engine = super::super::tests::engine();
        engine.online.state = Some(State {
            collection_first: 0,
            queue: vec![],
            cursor: Cursor::default(),
            repeat: socket::Repeat::Off,
            shuffle: false,
        });
        engine.online.outputs = serde_json::from_value(serde_json::json!({
            "owner": "remote-connection",
            "devices": [{
                "id": "remote-connection", "deviceId": "remote",
                "name": "Remote speaker", "canPlay": true
            }],
        }))
        .unwrap();
        let mut view = PlayerView::default();
        engine.project_online(&mut view);
        assert_eq!(view.output, "Output unavailable");
        assert_eq!(view.devices.len(), 1);
        assert!(!view.devices[0].selected && !view.devices[0].available);
        assert!(!view.playing && !view.transport);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn detached_media_cannot_pause_a_projected_successor() {
        let old = cursor(5, 2, 1);
        let next = cursor(6, 3, 0);
        let grant = OutputAuthority {
            generation: 8,
            deadline: Instant::now() + std::time::Duration::from_secs(5),
            attached: false,
            resume_through: 0,
        };
        let (authority, receiver) = tokio::sync::watch::channel(Some(grant));
        let execution = Execution {
            cursor: old.clone(),
            permit: output::RunPermit::new(receiver, grant),
            pending_crossfade: None,
            status: Status::Ok,
            last_fact: None,
        };
        assert!(execution.media_pause_token(&old, 5, 3).is_some());
        assert!(execution.media_pause_token(&next, 5, 3).is_none());
        authority.send_replace(None);
        assert!(execution.media_pause_token(&old, 5, 3).is_none());
    }

    #[test]
    fn crossfade_promotion_is_bound_to_exact_run_occurrence_and_generation() {
        let from = cursor(5, 2, 1);
        let to = cursor(6, 3, 0);
        let key = PreviewKey {
            generation: 8,
            from: from.clone(),
            to: to.clone(),
            id: "track".into(),
        };
        let fact = Cursor {
            elapsed: 67.0,
            playing: true,
            ..from.clone()
        };
        assert!(key.matches_landing(8, &fact, &to));
        assert!(!key.matches_landing(9, &fact, &to));
        assert!(!key.matches_landing(
            8,
            &Cursor {
                run: 2,
                ..from.clone()
            },
            &to
        ));
        assert!(!key.matches_landing(
            8,
            &Cursor {
                desired: false,
                ..from.clone()
            },
            &to
        ));
        assert!(!key.matches_landing(8, &from, &Cursor { key: 7, ..to }));
    }

    #[test]
    fn eof_prediction_ignores_old_observations_and_keeps_exact_confirmation() {
        let from = cursor(5, 2, 1);
        let to = cursor(6, 3, 0);
        let mut state = Online {
            state: Some(State {
                cursor: from.clone(),
                ..State::default()
            }),
            advance: Some(Advance {
                from: from.clone(),
                to: to.clone(),
            }),
            ..Online::default()
        };
        state.adopt_cursor(
            &Cursor {
                elapsed: 72.0,
                ..from
            },
            true,
        );
        assert!(output::same_execution(&to, state.cursor().unwrap()));
        let confirmed = Cursor {
            elapsed: 0.4,
            playing: true,
            ..to.clone()
        };
        state.state.as_mut().unwrap().cursor = confirmed.clone();
        state.adopt_cursor(&confirmed, true);
        assert!(state.advance.is_none());
        assert!(output::same_execution(&to, state.cursor().unwrap()));
        assert_eq!(state.cursor().unwrap().elapsed, 0.4);
    }

    #[test]
    fn terminal_eof_projects_same_row_stopped_at_zero_before_go_confirms() {
        let from = Cursor {
            elapsed: 12.0,
            playing: true,
            ..cursor(5, 2, 1)
        };
        let state = State {
            queue: serde_json::from_value(serde_json::json!([
                {"key":5,"order":5,"id":"tail","intent":"low"}
            ]))
            .unwrap(),
            cursor: from.clone(),
            ..State::default()
        };
        let to = output::successor(&state, &from).unwrap();
        let mut online = Online {
            state: Some(state),
            advance: Some(Advance {
                from: from.clone(),
                to: to.clone(),
            }),
            ..Online::default()
        };
        online.adopt_cursor(&from, true);
        let projected = online.cursor().unwrap();
        assert_eq!((projected.key, projected.visit, projected.run), (5, 2, 2));
        assert!(!projected.desired && !projected.playing);
        assert_eq!(projected.elapsed, 0.0);

        online.state.as_mut().unwrap().cursor = to.clone();
        online.adopt_cursor(&to, true);
        assert!(online.advance.is_none());
        assert_eq!(online.cursor().unwrap().elapsed, 0.0);
    }

    #[test]
    fn eof_correction_and_later_resume_have_distinct_bindings() {
        let from = cursor(5, 2, 1);
        let to = cursor(6, 3, 0);
        let correct = cursor(7, 3, 0);
        let mut state = Online {
            state: Some(State {
                cursor: correct.clone(),
                ..State::default()
            }),
            stopped: Some(to.clone()),
            advance: Some(Advance {
                from,
                to: to.clone(),
            }),
            ..Online::default()
        };
        assert!(state.stop_for(&to));
        assert!(!state.stop_for(&Cursor { run: 1, ..to }));
        state.adopt_cursor(&correct, false);
        assert!(state.advance.is_none());
        assert!(output::same_execution(&correct, state.cursor().unwrap()));
        assert!(state.stopped.is_none());
    }
}
