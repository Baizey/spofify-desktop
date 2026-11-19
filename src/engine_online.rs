//! Online actor. Socket admission is not a server acknowledgement. Player state
//! comes from Go; only a native EOF may predict the next published occurrence.
use super::{
    CreditView, Engine, LOAD_BUDGET, PlayerView, QueueEntryView, REPORT_INTERVAL, Reply,
    artist_subtitle, context_kind, credit_views, metadata_id, output,
};
use crate::{
    DeviceRow,
    api::ApiError,
    personal::{Command as PersonalCommand, Update as PersonalUpdate},
    player::{Action, Status},
    socket::{
        self, Cursor, Event, EventKind, FactError, Generation, Link, OutputAuthority, Outputs,
        State,
    },
};
use std::{collections::HashSet, sync::Arc, time::Instant};

pub(super) struct Work {
    generation: Generation,
    cursor: Cursor,
    id: String,
    duration: f64,
}

pub(super) struct Execution {
    cursor: Cursor,
    permit: output::RunPermit,
    status: Status,
    last_fact: Option<Instant>,
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
    generation: Option<Generation>,
    personal_generation: Option<Generation>,
    repair: Option<Generation>,
    player_gap: bool,
    observed_at: Option<Instant>,
    advance: Option<Advance>,
    stopped: Option<Cursor>,
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
    }
}

fn output_intent(command: &socket::Command, own_device: &str) -> bool {
    match command {
        socket::Command::Select { device_id } => device_id == own_device,
        socket::Command::Play { .. } | socket::Command::Resume | socket::Command::Jump { .. } => {
            true
        }
        _ => false,
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
        self.cancel_work();
        self.stop();
        self.action_status = "Playback state is out of sync.";
        self.repair(generation);
    }
    fn accept_outputs(&mut self, generation: Generation, outputs: Outputs) {
        let retiring = self.online.is_owner() && outputs.owner.is_empty();
        self.online.outputs = outputs;
        if retiring {
            self.cancel_work();
            self.stop();
            self.online.advance = None;
            if let Some(socket) = &self.socket {
                let _ = socket.command(generation, socket::Command::Release);
            }
        }
    }
    fn handle_event(&mut self, event: Event) {
        let generation = event.generation;
        if self.link() != Link::Connected(generation) {
            return;
        }
        match event.kind {
            EventKind::Snapshot {
                connection_id,
                player,
                outputs,
            } => {
                let fresh = self.online.generation != Some(generation);
                if fresh {
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
                if delta.source.as_deref() == Some(self.online.connection.as_str()) {
                    self.action_status = if delta.queue.is_none() {
                        "That selection is unavailable."
                    } else {
                        ""
                    };
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
        self.finish_output_promotion();
        if self.output_promotion.is_some() {
            return;
        }
        self.reconcile_online();
        if self.sample_online(true, false) {
            self.reconcile_online();
        }
    }
    pub(super) fn online_action(&mut self, action: Action) {
        if !self.network_allowed() {
            return;
        }
        let Some(command) = action.wire(&self.device_id) else {
            return;
        };
        if self.output_promotion.is_some() {
            return;
        }
        if self.passive_output && output_intent(&command, &self.device_id) {
            let awaiting_room = self.online_generation().is_none();
            if awaiting_room || self.should_promote_output(&command) {
                let resume_key = if matches!(&command, socket::Command::Resume) {
                    self.online.cursor().map(|cursor| cursor.key)
                } else {
                    None
                };
                let pending = super::OutputPromotion {
                    command,
                    deadline: Instant::now() + super::PROMOTION_TIMEOUT,
                    resume_key,
                    awaiting_room,
                };
                if awaiting_room {
                    self.output_promotion = Some(pending);
                    self.action_status = "Connecting playback controls…";
                } else {
                    self.promote_output(pending);
                }
                return;
            }
        }
        if matches!(
            command,
            socket::Command::Pause | socket::Command::Close | socket::Command::Release
        ) && (self.online.is_owner()
            || self.online.execution.is_some()
            || self.online.work.is_some())
        {
            // Preserve the real playback tail before Pause. Never send an end
            // as a consequence of a physical stop or a release.
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
        self.action_status = if self
            .socket
            .as_ref()
            .is_some_and(|s| s.command(generation, command).is_ok())
        {
            ""
        } else {
            "The playback action was not sent."
        };
    }
    fn should_promote_output(&self, command: &socket::Command) -> bool {
        output_intent(command, &self.device_id)
            && (matches!(command, socket::Command::Select { .. })
                || self.online.outputs.owner.is_empty())
    }
    fn promote_output(&mut self, mut pending: super::OutputPromotion) {
        self.clear_output();
        self.passive_output = false;
        pending.awaiting_room = false;
        pending.deadline = Instant::now() + super::PROMOTION_TIMEOUT;
        self.output_promotion = Some(pending);
        self.action_status = "Connecting this output…";
        self.connect();
    }
    fn abandon_output_promotion(&mut self, message: &'static str) {
        self.clear_output();
        self.passive_output = true;
        self.connect();
        self.action_status = message;
    }
    fn finish_output_promotion(&mut self) {
        let Some(pending) = &self.output_promotion else {
            return;
        };
        if Instant::now() >= pending.deadline {
            self.abandon_output_promotion("This output could not connect. Try again.");
            return;
        }
        let Some(generation) = self.online_generation() else {
            return;
        };
        let pending = self.output_promotion.take().unwrap();
        if pending.awaiting_room && self.should_promote_output(&pending.command) {
            self.promote_output(pending);
            return;
        }
        let command = pending.command;
        let valid = match &command {
            socket::Command::Resume => self.online.cursor().map(|c| c.key) == pending.resume_key,
            socket::Command::Jump { key } => self
                .online
                .state
                .as_ref()
                .is_some_and(|s| s.queue.iter().any(|t| t.key == *key)),
            _ => true,
        };
        if !valid {
            self.abandon_output_promotion("The queue changed. Choose what to play again.");
            return;
        }
        let Some(socket) = &self.socket else { return };
        // Only explicit Select may transfer an existing owner. Go handles the
        // atomic claim-if-unowned for Play; other transport intents preserve
        // the existing owner even if it changed after the snapshot.
        let replacing = !self.passive_output
            && (self.online.is_owner() || self.online.outputs.owner.is_empty())
            && matches!(
                &command,
                socket::Command::Play { .. } | socket::Command::Jump { .. }
            );
        if socket.command(generation, command).is_err() {
            self.abandon_output_promotion("The playback action was not sent. Try again.");
        } else {
            // Do not render the old queue cursor while a requested selection
            // is resolving. A fresh execution will replace the stop marker.
            if replacing {
                self.online.stopped = self.online.cursor().cloned();
            }
            self.action_status = "";
        }
    }
    fn reconcile_online(&mut self) {
        let grant = self.grant();
        if grant.is_none() && self.online.advance.take().is_some() {
            self.online.revision = self.online.revision.wrapping_add(1);
        }
        let attached = self.attached();
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
        self.online.work = Some(work);
        let work = self.online.work.as_ref().expect("just installed");
        if let Some(bytes) = self.audio.as_ref().and_then(|audio| audio.cached(&work.id)) {
            if let Some(work) = self.online.work.take() {
                self.start_online(work, Ok(bytes));
            }
        } else if let Some(account) = &self.account {
            let (api, session, replies, id) = (
                account.api.clone(),
                account.session.clone(),
                self.results.clone(),
                work.id.clone(),
            );
            let (epoch, operation) = (self.epoch, self.operation);
            self.media = Some(tokio::spawn(async move {
                let result = tokio::time::timeout(LOAD_BUDGET, api.media(&session, &id))
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
    pub(super) fn complete_online(&mut self, result: Result<Vec<u8>, ApiError>) {
        self.drain_online_events(None);
        if let Some(work) = self.online.work.take() {
            if result.as_ref().is_err_and(|e| e.expired) {
                self.expire();
                return;
            }
            self.start_online(
                work,
                result.map(|bytes| bytes.into()).map_err(|_| Status::Media),
            );
        }
    }
    fn start_online(&mut self, work: Work, bytes: Result<Arc<[u8]>, Status>) {
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
        let status = match bytes {
            Ok(bytes) => audio.play(
                work.id,
                bytes,
                work.cursor.elapsed,
                work.duration,
                permit.source(socket.link.clone(), grant.generation),
            ),
            Err(status) => status,
        };
        if status != Status::Rejected {
            self.online.execution = Some(Execution {
                cursor: work.cursor,
                permit,
                status,
                last_fact: None,
            });
        }
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
        let _ = socket.observe_local(grant.generation, actual);
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
            output::successor(state, &execution.cursor, facts.elapsed)
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
            return false;
        }
        execution.last_fact = Some(now);
        if let Some(to) = next {
            let from = execution.cursor.clone();
            self.online.advance = Some(Advance { from, to });
            self.online.revision = self.online.revision.wrapping_add(1);
            self.stop();
            return true;
        }
        false
    }
    pub(super) fn project_online(&self, p: &mut PlayerView) {
        let Some(state) = &self.online.state else {
            return;
        };
        let live = self.online_generation().is_some();
        let active = self
            .online
            .outputs
            .devices
            .iter()
            .find(|d| d.id == self.online.outputs.owner);
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
        p.output = active.map(|d| d.name.clone()).unwrap_or_else(|| {
            if self.grant().is_some() {
                "This desktop".into()
            } else {
                "No output selected".into()
            }
        });
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
        } else if live && !self.online.is_owner() && cursor.playing {
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
                    // A restored desktop only becomes an output on selection.
                    can_play: shown.can_play
                        || (self.passive_output && d.device_id == self.device_id),
                }
            })
            .collect();
        if p.status.is_empty() && self.online.work.is_some() {
            p.status = "Loading audio…";
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
