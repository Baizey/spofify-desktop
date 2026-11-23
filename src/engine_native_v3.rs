//! Native v3 execution. The actor owns transport, clock and media I/O; only
//! this executor associates shared intent with sampled output and local visits.

use crate::{
    audio::{Audio, Facts},
    audio_input::AudioInput,
    player::Authority,
    socket::{
        Action, Advance, Command, Cursor, Intent, Observation, ObservationStatus, Repeat, Report,
        Settlement, SettlementStatus, State, Track,
    },
};
use std::collections::VecDeque;

const SETTLEMENT_LIMIT: usize = 64;
const RECEIPT_LIMIT: usize = 128;
const REPORT_INTERVAL_MS: u64 = 1_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode<'a> {
    Owner { connection_id: &'a str },
    Detached,
    Follower,
}

#[derive(Clone, Copy)]
pub(crate) struct Context<'a> {
    pub mode: Mode<'a>,
    pub session_id: &'a str,
    pub generation: u64,
    pub revision: u64,
    /// Echoed command for reconcile, conditional execution command for execute.
    pub command: Option<&'a Command>,
    pub snapshot: bool,
    pub sampled_at_ms: u64,
    pub clock_offset_ms: Option<i64>,
    pub crossfade: bool,
    /// Actor-selected periodic cadence. Transitions and settlements are urgent.
    pub report: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MediaRequest {
    pub token: u64,
    pub id: String,
    pub preview: bool,
    pub portable_only: bool,
}

#[derive(Debug)]
pub(crate) enum Work {
    Prepare(MediaRequest),
    Cancel(MediaRequest),
    /// Call `report_sent` only after successful enqueue on this binding.
    Report(Report),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Waiting,
    Running,
    Anchored,
    Exhausted,
    Failed,
}

struct Active {
    cursor: Cursor,
    track_id: Option<String>,
    phase: Phase,
    run: Option<u64>,
}

struct Media {
    track_id: String,
    source: Option<AudioInput>,
    request: Option<MediaRequest>,
    failed: Option<bool>, // whether the failed request was portable-only
}

struct Pending {
    origin: Cursor,
    kind: Transition,
}

enum Transition {
    Advance {
        origin_revision: u64,
        advance: Advance,
    },
    Conditional {
        id: String,
    },
}

struct Offered {
    cursor: Cursor,
    track_id: Option<String>,
    run: Option<u64>,
}

#[derive(Default)]
pub(crate) struct NativeV3 {
    binding: Option<String>,
    scope: Option<(String, u64)>,
    confirmed: Option<State>,
    active: Option<Active>,
    pending: Option<Pending>,
    offered: Option<Offered>,
    waiting: Option<String>,
    receipts: VecDeque<String>,
    settlements: Vec<Settlement>,
    // The server serializes conditional execution. Reserve space independently
    // of ordinary results so an already-performed operation always has a receipt.
    applied_conditional: Option<Settlement>,
    media: Vec<Media>,
    next_request: u64,
    fade: Option<(u64, String)>,
    last_sent: Option<(u64, ObservationStatus, u64)>,
    sent_advance: Option<u64>,
}

impl NativeV3 {
    pub(crate) fn reconcile(
        &mut self,
        state: &State,
        cx: Context<'_>,
        audio: &mut Audio,
    ) -> Vec<Work> {
        let mut work = Vec::new();
        if !self.enter(cx, audio, &mut work) {
            return work;
        }
        if matches!(cx.mode, Mode::Detached) {
            self.drive(cx, audio, &mut work);
            return work;
        }
        let same_scope = self.same_scope(cx);
        if !same_scope && !cx.snapshot {
            return work;
        }
        let current = &state.cursor;
        let row = row(state, current.key);
        let id = row.map(|r| r.id.as_str());
        let command = cx.command.filter(|c| {
            matches!(
                c.action,
                Action::Play { .. }
                    | Action::Pause
                    | Action::Resume
                    | Action::Previous
                    | Action::Jump { .. }
                    | Action::Close
            )
        });
        let new_command = command.filter(|c| !self.receipts.contains(&c.id));
        let local_match = self.active.as_ref().is_some_and(|a| {
            a.cursor == *current && a.track_id.as_deref() == id && a.run == audio.run_identity()
        });
        let retained = if cx.snapshot {
            self.last_sent = None;
            match self.offered.take() {
                // The offered position is already normalized to its known
                // duration. An old intent anchor is not evidence that the
                // server accepted that different, measured position.
                Some(offer) => {
                    offer.cursor == *current
                        && offer.track_id.as_deref() == id
                        && offer.run == audio.run_identity()
                        && self.active.as_ref().is_some_and(|a| {
                            a.run == offer.run
                                && same_visit(&a.cursor, current)
                                && a.cursor.desired_playing == current.desired_playing
                                && (offer.run.is_some()
                                    || compatible_position(audio.facts(), row)
                                        == current.position_ms)
                        })
                }
                None => same_scope && local_match,
            }
        } else {
            same_scope && local_match
        };
        let conditional_committed = same_scope && !cx.snapshot && retained
            && self.pending.as_ref().is_some_and(|p| {
                matches!(&p.kind, Transition::Conditional { id } if cx.command.is_some_and(|c| c.id == *id && conditional(&c.action)))
            });
        let keep_pending = same_scope
            && !cx.snapshot
            && !conditional_committed
            && self.pending.as_ref().is_some_and(|p| {
                p.origin == *current
                    && match &p.kind {
                        Transition::Advance { advance, .. } => {
                            successor(state, current.key, true) == Some(advance.to_key)
                        }
                        Transition::Conditional { .. } => true,
                    }
            });
        let execution = new_command;
        if let Some(c) = new_command {
            self.remember(&c.id);
        }
        if !keep_pending {
            if let Some(Pending {
                kind: Transition::Conditional { id },
                ..
            }) = self.pending.take()
                && self
                    .applied_conditional
                    .as_ref()
                    .is_some_and(|s| s.id == id)
            {
                self.applied_conditional = None;
            }
            self.sent_advance = None;
            let retry = execution.is_some_and(|c| matches!(c.action, Action::Resume))
                && self
                    .active
                    .as_ref()
                    .is_some_and(|a| a.phase == Phase::Failed);
            if !retained || retry {
                self.finish_waiting(SettlementStatus::Stale);
                let mut anchor = current.position_ms;
                if same_scope
                    && !cx.snapshot
                    && let Some(a) = &self.active
                    && same_visit(&a.cursor, current)
                    && a.track_id.as_deref() == id
                    && a.phase != Phase::Exhausted
                    && execution.is_some_and(|c| matches!(c.action, Action::Pause | Action::Resume))
                {
                    anchor = position_ms(audio.facts());
                }
                if execution.is_some() || !same_scope {
                    self.retry_media(id);
                }
                self.land(current.clone(), row, anchor, false, audio);
                if let Some(c) = execution {
                    self.waiting = Some(c.id.clone());
                }
            } else if let Some(c) = execution {
                if self
                    .active
                    .as_ref()
                    .is_some_and(|a| a.phase == Phase::Waiting || a.phase == Phase::Running)
                    && !audio.has_started()
                {
                    self.finish_waiting(SettlementStatus::Stale);
                    self.waiting = Some(c.id.clone());
                } else {
                    self.settle(c.id.clone(), SettlementStatus::Satisfied);
                }
            }
            // A retained native run can be ahead of the measured cursor in a
            // reattach snapshot; only its confirmed anchor changes here.
            if let Some(a) = &mut self.active {
                a.cursor = current.clone();
            }
        }
        self.scope = Some((cx.session_id.to_owned(), cx.generation));
        if self.confirmed.as_ref() != Some(state) {
            self.confirmed = Some(state.clone());
        }
        if conditional_committed {
            // Consume buffered EOF against the commit's revision in
            // this turn. The actor may process another transport update next.
            self.natural(cx, audio);
        }
        self.drive(cx, audio, &mut work);
        self.make_report(cx, audio, &mut work);
        work
    }

    pub(crate) fn tick(&mut self, _state: &State, cx: Context<'_>, audio: &mut Audio) -> Vec<Work> {
        let mut work = Vec::new();
        if self.enter(cx, audio, &mut work) && self.confirmed.is_some() {
            if matches!(cx.mode, Mode::Detached) || self.same_scope(cx) {
                self.natural(cx, audio);
                self.drive(cx, audio, &mut work);
                self.make_report(cx, audio, &mut work);
            }
        }
        work
    }

    pub(crate) fn execute(
        &mut self,
        state: &State,
        cx: Context<'_>,
        audio: &mut Audio,
    ) -> Vec<Work> {
        let mut work = Vec::new();
        if !matches!(cx.mode, Mode::Owner { .. })
            || !self.enter(cx, audio, &mut work)
            || !self.same_scope(cx)
        {
            return work;
        }
        let Some(c) = cx.command.filter(|c| conditional(&c.action)) else {
            return work;
        };
        // Sample here as well as on tick. An EOF between ticks must not let
        // either action target a visit that has already ended.
        self.natural(cx, audio);
        if !self.receipts.contains(&c.id) {
            self.remember(&c.id);
            let source = self
                .active
                .as_ref()
                .filter(|a| a.run == audio.run_identity())
                .map(|a| a.cursor.clone());
            // A pending Advance fences uncommitted EOF. After a tail EOF
            // commits, the server checks its actual revision before forwarding
            // execute; unrelated metadata revisions don't make a basis stale.
            let source = source.filter(|a| {
                self.pending.is_none()
                    && self.applied_conditional.is_none()
                    && c.basis.session_id == cx.session_id
                    && c.basis.generation == cx.generation
                    && a.key != 0
                    && matches_basis(c, a)
                    && same_visit(&state.cursor, a)
                    && state.cursor.desired_playing == a.desired_playing
            });
            let landing = source.and_then(|source| match c.action {
                Action::Next => {
                    let next = successor(state, source.key, false)?;
                    if c.basis.successor_key != Some(next) {
                        return None;
                    }
                    let (target, anchor) = if next == 0 {
                        let facts = audio.facts();
                        (
                            Cursor {
                                desired_playing: false,
                                position_ms: compatible_position(facts, row(state, source.key)),
                                ..source.clone()
                            },
                            position_ms(facts),
                        )
                    } else {
                        (new_visit(next), 0)
                    };
                    let replace = target != source;
                    Some((target, anchor, replace))
                }
                Action::Seek { position_ms } => {
                    let anchor = bounded_position(position_ms, row(state, source.key));
                    // Even when it equals the old intent anchor, sample time
                    // can have advanced and the user explicitly requested it.
                    Some((
                        Cursor {
                            position_ms: anchor,
                            ..source
                        },
                        anchor,
                        true,
                    ))
                }
                _ => None,
            });
            if let Some((target, anchor, replace)) = landing {
                if replace {
                    self.finish_waiting(SettlementStatus::Stale);
                    let track = row(state, target.key);
                    if matches!(c.action, Action::Seek { .. }) {
                        self.retry_media(track.map(|r| r.id.as_str()));
                    }
                    self.land(target.clone(), track, anchor, false, audio);
                }
                self.pending = Some(Pending {
                    origin: state.cursor.clone(),
                    kind: Transition::Conditional { id: c.id.clone() },
                });
                // Even a logical no-op requires a server commit.
                self.applied_conditional = Some(Settlement {
                    id: c.id.clone(),
                    status: SettlementStatus::Applied,
                    cursor: Some(target),
                });
            } else {
                self.settle(c.id.clone(), SettlementStatus::Stale);
            }
        }
        self.drive(cx, audio, &mut work);
        self.make_report(cx, audio, &mut work);
        work
    }

    pub(crate) fn complete(
        &mut self,
        _state: &State,
        cx: Context<'_>,
        audio: &mut Audio,
        request: &MediaRequest,
        source: Option<AudioInput>,
    ) -> Vec<Work> {
        let mut work = Vec::new();
        if !self.enter(cx, audio, &mut work)
            || self.confirmed.is_none()
            || (!matches!(cx.mode, Mode::Detached) && !self.same_scope(cx))
        {
            return work;
        }
        if let Some(slot) = self
            .media
            .iter_mut()
            .find(|slot| slot.request.as_ref() == Some(request))
        {
            slot.request = None;
            slot.source = source.filter(|s| !s.is_failed());
            if request.portable_only || matches!(cx.mode, Mode::Detached) {
                slot.source = slot.source.take().and_then(|s| s.isolated());
            }
            slot.failed = slot.source.is_none().then_some(request.portable_only);
        }
        self.drive(cx, audio, &mut work);
        self.make_report(cx, audio, &mut work);
        work
    }

    /// Last retained queue and settings, including detached-only edits.
    pub(crate) fn state(&self) -> Option<&State> {
        self.confirmed.as_ref()
    }

    /// Native visit and transport intent; actual position comes from Audio facts.
    pub(crate) fn cursor(&self) -> Option<&Cursor> {
        self.active.as_ref().map(|a| &a.cursor)
    }

    pub(crate) fn failed(&self) -> bool {
        self.active
            .as_ref()
            .is_some_and(|a| a.phase == Phase::Failed)
    }

    /// Local transport, settings and keyed queue edits never replay as commands.
    pub(crate) fn act_detached(
        &mut self,
        action: Action,
        _state: &State,
        cx: Context<'_>,
        audio: &mut Audio,
    ) -> Vec<Work> {
        let mut work = Vec::new();
        if !matches!(cx.mode, Mode::Detached) || !self.enter(cx, audio, &mut work) {
            return work;
        }
        let before = self.active.as_ref().unwrap().cursor.clone();
        self.natural(cx, audio);
        let a = self.active.as_ref().unwrap();
        if matches!(action, Action::Next | Action::Seek { .. }) && before != a.cursor {
            self.drive(cx, audio, &mut work);
            return work;
        }
        let state = self.confirmed.as_mut().unwrap();
        let base = a.cursor.clone();
        state.cursor = base.clone();
        if matches!(action, Action::Close) {
            state.queue.clear();
            state.collection_first = 0;
            state.shuffle = false;
            state.cursor = Cursor::default();
            self.land(Cursor::default(), None, 0, false, audio);
            self.drive(cx, audio, &mut work);
            return work;
        }
        let elapsed = position_ms(audio.facts());
        let Some(i) = state.queue.iter().position(|r| r.key == base.key) else {
            return work;
        };
        let target = match action {
            Action::Repeat { repeat } => {
                state.repeat = repeat;
                self.drive(cx, audio, &mut work);
                return work;
            }
            Action::Shuffle { shuffle, seed } => {
                if state.shuffle != shuffle {
                    state.shuffle = shuffle;
                    shuffle_tail(&mut state.queue[i + 1..], shuffle, seed.unwrap_or_default());
                }
                self.drive(cx, audio, &mut work);
                return work;
            }
            Action::Remove { key } => {
                if let Some(at) = state
                    .queue
                    .iter()
                    .position(|r| r.key == key)
                    .filter(|&at| at > i)
                {
                    state.queue.remove(at);
                    state.collection_first = 0;
                }
                self.drive(cx, audio, &mut work);
                return work;
            }
            Action::Move { key, after } => {
                move_upcoming(state, i, key, after);
                self.drive(cx, audio, &mut work);
                return work;
            }
            Action::Clear => {
                if i + 1 < state.queue.len() {
                    state.queue.truncate(i + 1);
                    state.collection_first = 0;
                }
                self.drive(cx, audio, &mut work);
                return work;
            }
            Action::Pause if base.desired_playing => Cursor {
                desired_playing: false,
                position_ms: elapsed,
                ..base
            },
            Action::Resume if !base.desired_playing || a.phase == Phase::Failed => Cursor {
                desired_playing: true,
                position_ms: elapsed,
                ..base
            },
            Action::Seek { position_ms } => Cursor {
                position_ms,
                ..base
            },
            Action::Next => match successor(state, base.key, false).unwrap() {
                0 => Cursor {
                    desired_playing: false,
                    position_ms: elapsed,
                    ..base
                },
                key => new_visit(key),
            },
            Action::Previous => {
                let j = if elapsed > 3000 {
                    i
                } else if i > 0 {
                    i - 1
                } else if state.repeat == Repeat::All {
                    state.queue.len() - 1
                } else {
                    i
                };
                new_visit(state.queue[j].key)
            }
            Action::Jump { key } if row(state, key).is_some() => new_visit(key),
            _ => {
                self.drive(cx, audio, &mut work);
                return work;
            }
        };
        let row = row(state, target.key).cloned();
        state.cursor = target.clone();
        let anchor = target.position_ms;
        self.retry_media(row.as_ref().map(|r| r.id.as_str()));
        self.land(target, row.as_ref(), anchor, false, audio);
        self.drive(cx, audio, &mut work);
        work
    }

    pub(crate) fn advance_pending(&self) -> Option<&Advance> {
        match &self.pending.as_ref()?.kind {
            Transition::Advance { advance, .. } => Some(advance),
            Transition::Conditional { .. } => None,
        }
    }

    pub(crate) fn stable_primary(&self, audio: &Audio) -> Option<AudioInput> {
        let id = self.active.as_ref()?.track_id.as_ref()?;
        self.stable_input(id, audio)
    }

    /// The actor can use either pinned input after converting to its local queue.
    pub(crate) fn stable_input(&self, track_id: &str, audio: &Audio) -> Option<AudioInput> {
        let eligible = self
            .active
            .as_ref()
            .is_some_and(|a| a.track_id.as_deref() == Some(track_id))
            || self.media.iter().any(|m| m.track_id == track_id);
        if !eligible {
            return None;
        }
        self.media
            .iter()
            .find(|m| m.track_id == track_id)
            .and_then(|m| m.source.as_ref())
            .and_then(AudioInput::isolated)
            .or_else(|| audio.cached(track_id)?.isolated())
    }

    /// Read the measured cursor and retained queue without changing the offer
    /// witness, which must continue to describe the last transmitted claim.
    pub(crate) fn local_snapshot(&self, audio: &Audio) -> Option<State> {
        let a = self.active.as_ref()?;
        if a.run != audio.run_identity() {
            return None;
        }
        let mut state = self.confirmed.clone()?;
        state.cursor = a.cursor.clone();
        state.cursor.position_ms = compatible_position(audio.facts(), row(&state, a.cursor.key));
        Some(state)
    }

    /// Record the exact cursor sent on this handshake. If the run has already
    /// changed, still remember the attempt so a reply cannot use the ordinary
    /// unchanged-intent fallback to bless unaccepted native progress.
    pub(crate) fn witness_claim_cursor(&mut self, cursor: &Cursor, audio: &Audio) {
        let run = audio.run_identity();
        let track_id = self
            .active
            .as_ref()
            .filter(|a| {
                a.run == run
                    && same_visit(&a.cursor, cursor)
                    && a.cursor.desired_playing == cursor.desired_playing
            })
            .and_then(|a| a.track_id.clone());
        self.offered = Some(Offered {
            cursor: cursor.clone(),
            track_id,
            run,
        });
    }

    /// Export and witness the initial Hello claim without replacing live PCM.
    pub(crate) fn detached_snapshot(&mut self, audio: &Audio) -> Option<State> {
        let state = self.local_snapshot(audio)?;
        self.witness_claim_cursor(&state.cursor, audio);
        Some(state)
    }

    /// Register the actor's exact Hello export for independent Saved PCM. If the
    /// run is replaced before the snapshot we reconcile normally, never relabel it.
    pub(crate) fn adopt_local(&mut self, exported: &State, audio: &Audio) {
        let cursor = exported.cursor.clone();
        let track_id = row(exported, cursor.key).map(|r| r.id.clone());
        let run = audio.run_identity();
        let phase = if !cursor.desired_playing && run.is_none() {
            Phase::Anchored
        } else if cursor.desired_playing
            && run.is_some()
            && audio.failure().is_none()
            && !audio.was_fenced()
            && track_id
                .as_ref()
                .is_some_and(|id| audio.cached(id).is_some())
        {
            Phase::Running
        } else {
            return;
        };
        self.active = Some(Active {
            cursor: cursor.clone(),
            track_id: track_id.clone(),
            phase,
            run,
        });
        self.offered = Some(Offered {
            cursor,
            track_id,
            run,
        });
    }

    /// Hand control to the independent Saved actor without stopping native PCM.
    /// Apply returned cancellations; preserve any desired inputs first.
    pub(crate) fn release_local(&mut self) -> Vec<Work> {
        let work = self
            .media
            .iter_mut()
            .filter_map(|m| m.request.take().map(Work::Cancel))
            .collect();
        let serial = self.next_request;
        *self = Self {
            next_request: serial,
            ..Self::default()
        };
        work
    }

    /// Confirm local enqueue, not server acceptance of a pending transition.
    pub(crate) fn report_sent(&mut self, report: &Report) {
        if !report.settlements.is_empty() {
            self.settlements
                .retain(|s| !report.settlements.iter().any(|sent| s.id == sent.id));
        }
        if self
            .applied_conditional
            .as_ref()
            .is_some_and(|next| report.settlements.iter().any(|s| s.id == next.id))
        {
            self.applied_conditional = None;
        }
        if report.advance.is_some() {
            self.sent_advance = Some(report.revision);
        }
        self.last_sent = Some((
            report.revision,
            report.observation.status,
            report.observation.sampled_at_ms,
        ));
    }

    fn enter(&mut self, cx: Context<'_>, audio: &mut Audio, work: &mut Vec<Work>) -> bool {
        match cx.mode {
            Mode::Follower => {
                for slot in &mut self.media {
                    if let Some(request) = slot.request.take() {
                        work.push(Work::Cancel(request));
                    }
                }
                let serial = self.next_request;
                *self = Self {
                    next_request: serial,
                    ..Self::default()
                };
                audio.clear();
                false
            }
            Mode::Owner { connection_id } => {
                if self.binding.as_deref() != Some(connection_id) {
                    // Only a validated snapshot can turn a new binding into a
                    // native admission, never an offer or stale tick.
                    if !cx.snapshot {
                        return false;
                    }
                    self.reset_reports();
                    self.binding = Some(connection_id.to_owned());
                    for slot in &mut self.media {
                        if let Some(request) = slot.request.take() {
                            work.push(Work::Cancel(request));
                        }
                        slot.failed = None;
                    }
                }
                true
            }
            Mode::Detached => {
                if self.binding.take().is_some() {
                    self.reset_reports();
                    self.pending = None;
                    if let (Some(state), Some(active)) = (&mut self.confirmed, &self.active) {
                        state.cursor = active.cursor.clone();
                    }
                    for slot in &mut self.media {
                        slot.source = slot.source.take().and_then(|s| s.isolated());
                        if let Some(request) = slot.request.take() {
                            work.push(Work::Cancel(request));
                        }
                        slot.failed = None;
                    }
                }
                self.confirmed.is_some() && self.active.is_some()
            }
        }
    }

    fn reset_reports(&mut self) {
        self.waiting = None;
        self.settlements.clear();
        self.applied_conditional = None;
        self.receipts.clear();
        self.last_sent = None;
        self.sent_advance = None;
    }

    fn same_scope(&self, cx: Context<'_>) -> bool {
        self.scope.as_ref().is_some_and(|(session, generation)| {
            session == cx.session_id && *generation == cx.generation
        })
    }

    fn land(
        &mut self,
        cursor: Cursor,
        row: Option<&Track>,
        anchor: u64,
        exhausted: bool,
        audio: &mut Audio,
    ) {
        self.pin_primary(audio);
        self.fade = None;
        audio.stop();
        if self
            .active
            .as_ref()
            .is_some_and(|a| a.track_id.as_deref() != row.map(|r| r.id.as_str()))
        {
            audio.discard_cache();
        }
        audio.anchor(
            anchor as f64 / 1000.0,
            row.map_or(0.0, |r| r.duration.max(0.0)),
        );
        let phase = if exhausted {
            Phase::Exhausted
        } else if cursor.desired_playing {
            if row.is_some() {
                Phase::Waiting
            } else {
                Phase::Failed
            }
        } else {
            Phase::Anchored
        };
        self.active = Some(Active {
            cursor,
            track_id: row.map(|r| r.id.clone()),
            phase,
            run: None,
        });
    }

    fn pin_primary(&mut self, audio: &Audio) {
        let Some(id) = self.active.as_ref().and_then(|a| a.track_id.as_ref()) else {
            return;
        };
        if let Some(input) = audio.cached(id).and_then(|s| s.isolated()) {
            if let Some(slot) = self.media.iter_mut().find(|m| m.track_id == *id) {
                slot.source = Some(input);
                slot.failed = None;
            } else {
                self.media.push(Media {
                    track_id: id.clone(),
                    source: Some(input),
                    request: None,
                    failed: None,
                });
            }
        }
    }

    fn retry_media(&mut self, id: Option<&str>) {
        if let Some(slot) = self
            .media
            .iter_mut()
            .find(|m| Some(m.track_id.as_str()) == id)
        {
            if slot.failed.is_some() || slot.source.as_ref().is_some_and(AudioInput::is_failed) {
                slot.source = None;
                slot.failed = None;
            }
        }
    }

    fn natural(&mut self, cx: Context<'_>, audio: &mut Audio) {
        let facts = audio.facts();
        if self.pending.is_some() || !facts.ended {
            return;
        }
        let Some(a) = self
            .active
            .as_ref()
            .filter(|a| a.phase == Phase::Running && a.run == audio.run_identity())
        else {
            return;
        };
        let Some(state) = &self.confirmed else { return };
        let origin = a.cursor.clone();
        let Some(next) = successor(state, origin.key, true) else {
            return;
        };
        let target = if next == 0 {
            Cursor {
                position_ms: 0,
                desired_playing: false,
                ..origin.clone()
            }
        } else {
            new_visit(next)
        };
        let row = row(state, target.key).cloned();
        self.finish_waiting(SettlementStatus::Applied);
        let promote = next != 0
            && self
                .fade
                .take()
                .is_some_and(|(key, id)| key == next && row.as_ref().is_some_and(|r| r.id == id))
            && audio.promote_crossfade();
        if promote {
            self.active = Some(Active {
                cursor: target.clone(),
                track_id: row.map(|r| r.id),
                phase: Phase::Running,
                run: audio.run_identity(),
            });
        } else {
            self.land(target.clone(), row.as_ref(), 0, next == 0, audio);
        }
        if matches!(cx.mode, Mode::Owner { .. }) {
            let advance = Advance {
                from_key: origin.key,
                from_occurrence: origin.occurrence.clone().expect("validated native visit"),
                to_key: next,
                to_occurrence: (next != 0).then(|| target.occurrence.clone()).flatten(),
                ended_position_ms: position_ms(facts),
            };
            self.pending = Some(Pending {
                origin,
                kind: Transition::Advance {
                    origin_revision: cx.revision,
                    advance,
                },
            });
            self.sent_advance = None;
        } else if let Some(state) = &mut self.confirmed {
            state.cursor = target;
        }
    }

    fn drive(&mut self, cx: Context<'_>, audio: &mut Audio, work: &mut Vec<Work>) {
        let Some(a) = &self.active else { return };
        let Some(state) = &self.confirmed else { return };
        let current = a.track_id.clone();
        let next = successor(state, a.cursor.key, true)
            .and_then(|key| row(state, key))
            .map(|r| (r.key, r.id.clone()));
        let detached = matches!(cx.mode, Mode::Detached);
        self.media.retain_mut(|slot| {
            let is_current = current.as_ref() == Some(&slot.track_id);
            let keep = is_current || next.as_ref().is_some_and(|(_, id)| *id == slot.track_id);
            if !keep
                || slot
                    .request
                    .as_ref()
                    .is_some_and(|r| r.preview == is_current)
            {
                if let Some(request) = slot.request.take() {
                    work.push(Work::Cancel(request));
                }
            }
            keep
        });
        for id in current.iter().chain(next.iter().map(|(_, id)| id)) {
            let is_current = current.as_ref() == Some(id);
            let slot = if let Some(i) = self.media.iter().position(|m| m.track_id == *id) {
                &mut self.media[i]
            } else {
                self.media.push(Media {
                    track_id: id.clone(),
                    source: None,
                    request: None,
                    failed: None,
                });
                self.media.last_mut().unwrap()
            };
            if slot.source.as_ref().is_some_and(AudioInput::is_failed) {
                slot.source = None;
                slot.failed = Some(detached);
            }
            if slot.failed == Some(true) && !detached {
                slot.failed = None;
            }
            if slot.source.is_none() && slot.request.is_none() && slot.failed.is_none() {
                let available = audio.cached(id).filter(|s| !s.is_failed());
                slot.source = if detached {
                    available.and_then(|s| s.isolated())
                } else {
                    available
                };
                if slot.source.is_none() {
                    self.next_request += 1;
                    let request = MediaRequest {
                        token: self.next_request,
                        id: id.clone(),
                        preview: !is_current,
                        portable_only: detached,
                    };
                    slot.request = Some(request.clone());
                    work.push(Work::Prepare(request));
                }
            }
        }
        let a = self.active.as_mut().unwrap();
        let slot = current
            .as_ref()
            .and_then(|id| self.media.iter().find(|m| m.track_id == *id));
        if a.phase == Phase::Waiting {
            if let Some(input) = slot.and_then(|slot| slot.source.clone()) {
                let facts = audio.facts();
                let result = audio.play(
                    current.unwrap(),
                    input,
                    facts.elapsed,
                    facts.duration,
                    Authority::offline(),
                );
                if result == crate::player::Status::Ok {
                    a.phase = Phase::Running;
                    a.run = audio.run_identity();
                } else {
                    a.phase = Phase::Failed;
                }
            } else if slot.is_none_or(|slot| slot.failed.is_some()) {
                a.phase = Phase::Failed;
            }
        }
        if a.phase == Phase::Running
            && (a.run != audio.run_identity() || audio.failure().is_some() || audio.was_fenced())
        {
            a.phase = Phase::Failed;
            audio.cancel_crossfade();
            self.fade = None;
        }
        let want_fade = cx.crossfade
            && a.phase == Phase::Running
            && a.cursor.desired_playing
            && self.pending.is_none()
            && next.as_ref().is_some_and(|(key, _)| *key != a.cursor.key);
        if !want_fade || self.fade.is_some() && self.fade != next {
            audio.cancel_crossfade();
            self.fade = None;
        }
        if want_fade {
            audio.maintain_crossfade();
            if self.fade.is_none()
                && let Some((key, id)) = next
                && let Some(input) = self
                    .media
                    .iter()
                    .find(|m| m.track_id == id)
                    .and_then(|m| m.source.as_ref())
                    .and_then(AudioInput::isolated)
                && audio.start_crossfade(id.clone(), input, Authority::offline())
            {
                self.fade = Some((key, id));
            }
        }
        match a.phase {
            Phase::Waiting => {}
            Phase::Running if !audio.has_started() => {}
            Phase::Failed => self.finish_waiting(SettlementStatus::Failed),
            _ => self.finish_waiting(SettlementStatus::Applied),
        }
    }

    fn remember(&mut self, id: &str) {
        if self.receipts.len() == RECEIPT_LIMIT {
            self.receipts.pop_front();
        }
        self.receipts.push_back(id.to_owned());
    }

    fn settle(&mut self, id: String, status: SettlementStatus) {
        if self.settlements.len() < SETTLEMENT_LIMIT {
            self.settlements.push(Settlement {
                id,
                status,
                cursor: None,
            });
        }
    }

    fn finish_waiting(&mut self, status: SettlementStatus) {
        if let Some(id) = self.waiting.take() {
            self.settle(id, status);
        }
    }

    fn make_report(&self, cx: Context<'_>, audio: &Audio, work: &mut Vec<Work>) {
        if !matches!(cx.mode, Mode::Owner { .. }) {
            return;
        }
        if self.applied_conditional.is_none()
            && self
                .pending
                .as_ref()
                .is_some_and(|p| matches!(p.kind, Transition::Conditional { .. }))
        {
            // After enqueue, wait for the ordered echo. In particular Seek
            // retains the occurrence, so a bare provisional sample must not
            // look like pre-seek progress at the older revision.
            return;
        }
        let Some(a) = &self.active else { return };
        let facts = audio.facts();
        let observation = Observation {
            key: a.cursor.key,
            occurrence: a.cursor.occurrence.clone(),
            position_ms: if a.cursor.key == 0 {
                0
            } else {
                position_ms(facts)
            },
            sampled_at_ms: cx.sampled_at_ms,
            status: match a.phase {
                Phase::Failed => ObservationStatus::Failed,
                Phase::Exhausted => ObservationStatus::Stopped,
                Phase::Anchored if a.cursor.key == 0 => ObservationStatus::Stopped,
                Phase::Anchored => ObservationStatus::Paused,
                Phase::Waiting => ObservationStatus::Loading,
                Phase::Running if facts.playing => ObservationStatus::Playing,
                Phase::Running if facts.loading && audio.has_started() => {
                    ObservationStatus::Buffering
                }
                Phase::Running if facts.loading => ObservationStatus::Loading,
                Phase::Running => ObservationStatus::Stopped,
            },
        };
        let make = |revision, advance, settlements| {
            Work::Report(Report {
                generation: cx.generation,
                revision,
                clock_offset_ms: cx.clock_offset_ms,
                observation: observation.clone(),
                advance,
                settlements,
            })
        };
        let mut advance_now = None;
        if let Some(Pending {
            kind:
                Transition::Advance {
                    origin_revision,
                    advance,
                },
            ..
        }) = &self.pending
            && self.sent_advance != Some(*origin_revision)
        {
            if *origin_revision == cx.revision {
                advance_now = Some(advance.clone());
            } else if *origin_revision < cx.revision {
                // Do not let newer intent authorize an EOF from old intent.
                // Settlements for newer commands need their own revision.
                work.push(make(*origin_revision, Some(advance.clone()), Vec::new()));
            }
        }
        let due = self.last_sent.is_none_or(|(revision, status, sampled)| {
            (revision, status) != (cx.revision, observation.status)
                || cx.report
                    && (cx.sampled_at_ms < sampled
                        || cx.sampled_at_ms - sampled >= REPORT_INTERVAL_MS)
        });
        if due
            || !self.settlements.is_empty()
            || self.applied_conditional.is_some()
            || advance_now.is_some()
        {
            let settlements = self
                .applied_conditional
                .iter()
                .chain(self.settlements.iter())
                .take(SETTLEMENT_LIMIT)
                .cloned()
                .collect();
            work.push(make(cx.revision, advance_now, settlements));
        }
    }
}

fn row(state: &State, key: u64) -> Option<&Track> {
    state.queue.iter().find(|r| r.key == key)
}

fn same_visit(a: &Cursor, b: &Cursor) -> bool {
    a.key == b.key && a.occurrence == b.occurrence
}

fn conditional(action: &Action) -> bool {
    matches!(action, Action::Next | Action::Seek { .. })
}

fn matches_basis(c: &Command, cursor: &Cursor) -> bool {
    c.basis.occurrence == cursor.occurrence && c.basis.key.is_none_or(|key| key == cursor.key)
}

fn move_upcoming(state: &mut State, current: usize, key: u64, after: u64) {
    let upcoming = current + 1;
    let Some(from) = state
        .queue
        .iter()
        .position(|r| r.key == key)
        .filter(|&i| i >= upcoming)
    else {
        return;
    };
    let insertion = if after == 0 {
        upcoming
    } else {
        let Some(anchor) = state
            .queue
            .iter()
            .position(|r| r.key == after)
            .filter(|&i| i >= upcoming && i != from)
        else {
            return;
        };
        anchor + 1
    };
    let row = state.queue.remove(from);
    state
        .queue
        .insert(insertion - usize::from(from < insertion), row);
    for (i, row) in state.queue.iter_mut().enumerate() {
        row.order = i as u64 + 1;
        if i >= upcoming {
            row.intent = Intent::High;
        }
    }
    state.collection_first = 0;
    state.shuffle = false;
}

fn shuffle_tail(tail: &mut [Track], shuffle: bool, mut seed: u32) {
    let (mut chosen, mut low): (Vec<_>, Vec<_>) =
        tail.iter().cloned().partition(|r| r.intent != Intent::Low);
    if shuffle {
        for i in (1..low.len()).rev() {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            low.swap(i, ((u64::from(seed) * (i as u64 + 1)) >> 32) as usize);
        }
    } else {
        low.sort_by_key(|r| r.order);
    }
    chosen.extend(low);
    for (dst, src) in tail.iter_mut().zip(chosen) {
        *dst = src;
    }
}

fn new_visit(key: u64) -> Cursor {
    Cursor {
        key,
        occurrence: Some(uuid::Uuid::new_v4().to_string()),
        position_ms: 0,
        desired_playing: true,
    }
}

fn position_ms(facts: Facts) -> u64 {
    (facts.elapsed * 1000.0).round() as u64
}

fn compatible_position(facts: Facts, row: Option<&Track>) -> u64 {
    bounded_position(position_ms(facts), row)
}

fn bounded_position(ms: u64, row: Option<&Track>) -> u64 {
    row.filter(|r| r.duration > 0.0)
        .map_or(ms, |r| ms.min((r.duration * 1000.0).round() as u64))
}

fn successor(state: &State, key: u64, natural: bool) -> Option<u64> {
    let i = state.queue.iter().position(|r| r.key == key)?;
    Some(if natural && state.repeat == Repeat::One {
        key
    } else if let Some(row) = state.queue.get(i + 1) {
        row.key
    } else if state.repeat == Repeat::All {
        state.queue[0].key
    } else {
        0
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::socket::CommandBasis;
    use rodio::mixer::{MixerSource, mixer};
    use serde_json::json;
    use std::sync::{Arc, atomic::AtomicU64};

    fn wav() -> Arc<[u8]> {
        let mut b = Vec::new();
        b.extend(b"RIFF");
        b.extend(1636u32.to_le_bytes());
        b.extend(b"WAVEfmt ");
        b.extend(16u32.to_le_bytes());
        b.extend(1u16.to_le_bytes());
        b.extend(1u16.to_le_bytes());
        b.extend(8000u32.to_le_bytes());
        b.extend(16000u32.to_le_bytes());
        b.extend(2u16.to_le_bytes());
        b.extend(16u16.to_le_bytes());
        b.extend(b"data");
        b.extend(1600u32.to_le_bytes());
        for _ in 0..800 {
            b.extend(8192i16.to_le_bytes());
        }
        b.into()
    }

    fn state() -> State {
        serde_json::from_value(json!({
            "queue": [
                {"key": 1, "order": 1, "id":"a", "intent":"low", "duration":0.1},
                {"key": 2, "order": 2, "id":"b", "intent":"low", "duration":0.1},
                {"key": 3, "order": 3, "id":"a", "intent":"low", "duration":0.1}
            ],
            "cursor":{"key":1,"occurrence":"visit-a","positionMs":0,"desiredPlaying":true},
            "repeat":"off", "shuffle":false
        }))
        .unwrap()
    }

    fn context() -> Context<'static> {
        Context {
            mode: Mode::Owner {
                connection_id: "binding",
            },
            session_id: "room",
            generation: 1,
            revision: 10,
            command: None,
            snapshot: false,
            sampled_at_ms: 5000,
            clock_offset_ms: None,
            crossfade: false,
            report: true,
        }
    }

    fn command(state: &State, action: Action, next: Option<u64>) -> Command {
        Command {
            id: "binding:1".into(),
            issued_at_ms: 5000,
            basis: CommandBasis {
                session_id: "room".into(),
                generation: 1,
                revision: 10,
                occurrence: state.cursor.occurrence.clone(),
                key: Some(state.cursor.key),
                successor_key: next,
            },
            action,
        }
    }

    fn setup() -> (NativeV3, State, Audio, MixerSource) {
        let (mixer, output) = mixer(1, 8000);
        let mut audio = Audio::new(
            None,
            false,
            Arc::new(AtomicU64::new(1)),
            1,
            Default::default(),
        );
        audio.use_test_output(mixer, "a".into(), wav());
        (NativeV3::default(), state(), audio, output)
    }

    fn report(work: &[Work]) -> &Report {
        work.iter()
            .find_map(|w| {
                if let Work::Report(report) = w {
                    Some(report)
                } else {
                    None
                }
            })
            .unwrap()
    }

    fn preview(work: &[Work]) -> MediaRequest {
        work.iter()
            .find_map(|w| {
                if let Work::Prepare(r) = w
                    && r.preview
                {
                    Some(r.clone())
                } else {
                    None
                }
            })
            .unwrap()
    }

    fn consume_eof(output: &mut MixerSource, audio: &Audio) {
        for _ in 0..2_000 {
            let _ = output.next();
            if audio.facts().ended {
                return;
            }
        }
        panic!("test PCM did not end");
    }

    #[test]
    fn reports_actual_pcm_keeps_identical_rebind_and_stops_on_follower() {
        let (mut native, state, mut audio, mut output) = setup();
        let work = native.reconcile(
            &state,
            Context {
                snapshot: true,
                ..context()
            },
            &mut audio,
        );
        assert_eq!(report(&work).observation.status, ObservationStatus::Loading);
        let run = audio.run_identity().unwrap();
        assert!(output.nth(159).unwrap() > 0.0);
        let work = native.tick(&state, context(), &mut audio);
        assert_eq!(report(&work).observation.status, ObservationStatus::Playing);
        assert_eq!(report(&work).observation.position_ms, 20);
        native.report_sent(report(&work));
        assert!(
            native
                .tick(&state, context(), &mut audio)
                .iter()
                .all(|w| !matches!(w, Work::Report(_)))
        );
        native.reconcile(
            &state,
            Context {
                snapshot: true,
                mode: Mode::Owner {
                    connection_id: "replacement",
                },
                ..context()
            },
            &mut audio,
        );
        assert_eq!(audio.run_identity(), Some(run));
        let work = native.tick(
            &state,
            Context {
                mode: Mode::Follower,
                ..context()
            },
            &mut audio,
        );
        assert!(
            !work
                .iter()
                .any(|w| matches!(w, Work::Report(_) | Work::Prepare(_)))
        );
        assert_eq!(audio.run_identity(), None);
        assert!(!audio.facts().playing);
    }

    #[test]
    fn eof_wins_if_next_arrives_before_tick_and_stale_settlement_survives_enqueue_failure() {
        let (mut native, mut state, mut audio, mut output) = setup();
        let work = native.reconcile(
            &state,
            Context {
                snapshot: true,
                ..context()
            },
            &mut audio,
        );
        let request = preview(&work);
        native.complete(&state, context(), &mut audio, &request, Some(wav().into()));
        consume_eof(&mut output, &audio);
        let next = command(&state, Action::Next, Some(2));
        let work = native.execute(
            &state,
            Context {
                command: Some(&next),
                ..context()
            },
            &mut audio,
        );
        let observed = report(&work);
        assert_eq!(observed.advance.as_ref().unwrap().to_key, 2);
        assert_eq!(observed.advance.as_ref().unwrap().ended_position_ms, 100);
        assert_eq!(observed.settlements[0].status, SettlementStatus::Stale);
        assert!(observed.settlements[0].cursor.is_none());
        assert_eq!(observed.observation.status, ObservationStatus::Loading);
        let successor = native.cursor().unwrap().clone();
        let run = audio.run_identity().unwrap();
        let work = native.tick(&state, context(), &mut audio);
        assert_eq!(
            report(&work),
            observed,
            "no enqueue ACK, report still pending"
        );
        native.report_sent(report(&work));
        assert_eq!(
            native.advance_pending().unwrap().to_occurrence,
            successor.occurrence
        );
        state.cursor = successor;
        native.reconcile(
            &state,
            Context {
                revision: 11,
                ..context()
            },
            &mut audio,
        );
        assert_eq!(audio.run_identity(), Some(run));
        assert!(native.advance_pending().is_none());
    }

    #[test]
    fn executed_next_drains_buffered_successor_eof_on_commit_before_the_next_command() {
        let (mut native, mut state, mut audio, mut output) = setup();
        let work = native.reconcile(
            &state,
            Context {
                snapshot: true,
                ..context()
            },
            &mut audio,
        );
        native.complete(
            &state,
            context(),
            &mut audio,
            &preview(&work),
            Some(wav().into()),
        );
        let next = command(&state, Action::Next, Some(2));
        let work = native.execute(
            &state,
            Context {
                command: Some(&next),
                ..context()
            },
            &mut audio,
        );
        let observed = report(&work);
        let applied = &observed.settlements[0];
        assert_eq!(applied.status, SettlementStatus::Applied);
        state.cursor = applied.cursor.clone().unwrap();
        assert_eq!(state.cursor.key, 2);
        assert!(observed.advance.is_none());
        native.report_sent(observed);
        consume_eof(&mut output, &audio);
        assert!(
            !native
                .tick(&state, context(), &mut audio)
                .iter()
                .any(|w| matches!(w, Work::Report(r) if r.advance.is_some()))
        );
        let work = native.reconcile(
            &state,
            Context {
                command: Some(&next),
                revision: 11,
                ..context()
            },
            &mut audio,
        );
        let observed = report(&work);
        assert_eq!(observed.revision, 11);
        let advance = observed.advance.as_ref().unwrap();
        assert_eq!(
            advance.from_occurrence,
            *state.cursor.occurrence.as_ref().unwrap()
        );
        assert_eq!(advance.to_key, 3);
        assert_eq!(advance.ended_position_ms, 100);
        assert_eq!(native.cursor().unwrap().key, 3);
        native.report_sent(observed);

        let mut seek = command(&state, Action::Seek { position_ms: 20 }, None);
        seek.id = "binding:2".into();
        let work = native.execute(
            &state,
            Context {
                command: Some(&seek),
                revision: 11,
                ..context()
            },
            &mut audio,
        );
        assert!(report(&work).advance.is_none());
        assert_eq!(report(&work).settlements[0].status, SettlementStatus::Stale);
        assert_eq!(native.cursor().unwrap().key, 3);
    }

    #[test]
    fn seek_after_pcm_eof_settles_stale_with_the_actual_advance() {
        let (mut native, state, mut audio, mut output) = setup();
        let work = native.reconcile(
            &state,
            Context {
                snapshot: true,
                ..context()
            },
            &mut audio,
        );
        native.complete(
            &state,
            context(),
            &mut audio,
            &preview(&work),
            Some(wav().into()),
        );
        consume_eof(&mut output, &audio);
        let seek = command(&state, Action::Seek { position_ms: 40 }, None);
        let work = native.execute(
            &state,
            Context {
                command: Some(&seek),
                ..context()
            },
            &mut audio,
        );
        let report = report(&work);
        assert_eq!(report.revision, 10);
        assert_eq!(report.settlements[0].status, SettlementStatus::Stale);
        assert!(report.settlements[0].cursor.is_none());
        let advance = report.advance.as_ref().unwrap();
        assert_eq!(
            (advance.from_key, advance.ended_position_ms, advance.to_key),
            (1, 100, 2)
        );
        assert_eq!(report.observation.position_ms, 0);
        assert!(output.nth(159).unwrap() > 0.0);
        assert_eq!(position_ms(audio.facts()), 20);
    }

    #[test]
    fn seek_applies_once_and_any_later_eof_waits_for_its_commit() {
        for ends_before_commit in [false, true] {
            let (mut native, mut state, mut audio, mut output) = setup();
            let work = native.reconcile(
                &state,
                Context {
                    snapshot: true,
                    ..context()
                },
                &mut audio,
            );
            native.complete(
                &state,
                context(),
                &mut audio,
                &preview(&work),
                Some(wav().into()),
            );
            assert!(output.nth(159).unwrap() > 0.0);
            let original_run = audio.run_identity();
            let seek = command(&state, Action::Seek { position_ms: 40 }, None);
            let work = native.execute(
                &state,
                Context {
                    command: Some(&seek),
                    ..context()
                },
                &mut audio,
            );
            let applied = report(&work);
            assert!(applied.advance.is_none());
            assert_eq!(applied.settlements[0].status, SettlementStatus::Applied);
            let target = applied.settlements[0].cursor.clone().unwrap();
            assert_eq!(
                target,
                Cursor {
                    position_ms: 40,
                    ..state.cursor.clone()
                }
            );
            let sought = audio.run_identity();
            assert_ne!(sought, original_run);
            native.report_sent(applied);

            // Duplicating execute or replaying it through the state echo must
            // never re-seek this run, even though the visit did not change.
            native.execute(
                &state,
                Context {
                    command: Some(&seek),
                    ..context()
                },
                &mut audio,
            );
            assert_eq!(audio.run_identity(), sought);
            assert!(output.nth(159).unwrap() > 0.0);
            assert_eq!(position_ms(audio.facts()), 60);
            if ends_before_commit {
                consume_eof(&mut output, &audio);
                let work = native.tick(&state, context(), &mut audio);
                assert!(work.iter().all(|w| !matches!(w, Work::Report(_))));
                assert_eq!(audio.run_identity(), sought);
            }
            state.cursor = target;
            let work = native.reconcile(
                &state,
                Context {
                    command: Some(&seek),
                    revision: 11,
                    ..context()
                },
                &mut audio,
            );
            let committed = report(&work);
            assert!(committed.settlements.is_empty());
            if ends_before_commit {
                assert_eq!(committed.revision, 11);
                let advance = committed.advance.as_ref().unwrap();
                assert_eq!(advance.from_occurrence, state.cursor.occurrence.unwrap());
                assert_eq!(
                    (advance.from_key, advance.ended_position_ms, advance.to_key),
                    (1, 100, 2)
                );
            } else {
                assert!(committed.advance.is_none());
                assert_eq!(audio.run_identity(), sought);
                assert_eq!(committed.observation.position_ms, 60);
            }
        }
    }

    #[test]
    fn delayed_seek_never_targets_a_repeat_of_the_same_row() {
        let (mut native, mut state, mut audio, mut output) = setup();
        state.repeat = Repeat::One;
        native.reconcile(
            &state,
            Context {
                snapshot: true,
                ..context()
            },
            &mut audio,
        );
        let seek = command(&state, Action::Seek { position_ms: 80 }, None);
        consume_eof(&mut output, &audio);
        let work = native.tick(&state, context(), &mut audio);
        native.report_sent(report(&work));
        let repeated = native.cursor().unwrap().clone();
        assert_eq!(repeated.key, state.cursor.key);
        assert_ne!(repeated.occurrence, state.cursor.occurrence);
        state.cursor = repeated;
        let confirmed = Context {
            revision: 11,
            ..context()
        };
        native.reconcile(&state, confirmed, &mut audio);
        assert!(output.nth(159).unwrap() > 0.0);
        let run = audio.run_identity();
        let work = native.execute(
            &state,
            Context {
                command: Some(&seek),
                ..confirmed
            },
            &mut audio,
        );
        let report = report(&work);
        assert_eq!(report.settlements[0].status, SettlementStatus::Stale);
        assert!(report.advance.is_none());
        assert_eq!(report.observation.position_ms, 20);
        assert_eq!(audio.run_identity(), run);
    }

    #[test]
    fn conditional_seek_clamps_position_and_preserves_paused_intent() {
        let (mut native, mut state, mut audio, _) = setup();
        state.cursor.desired_playing = false;
        native.reconcile(
            &state,
            Context {
                snapshot: true,
                ..context()
            },
            &mut audio,
        );
        let seek = command(&state, Action::Seek { position_ms: 500 }, None);
        let work = native.execute(
            &state,
            Context {
                command: Some(&seek),
                ..context()
            },
            &mut audio,
        );
        let applied = report(&work);
        assert_eq!(applied.settlements[0].status, SettlementStatus::Applied);
        assert_eq!(
            applied.settlements[0].cursor,
            Some(Cursor {
                position_ms: 100,
                ..state.cursor.clone()
            })
        );
        assert_eq!(applied.observation.status, ObservationStatus::Paused);
        assert_eq!(audio.run_identity(), None);
    }

    #[test]
    fn rejected_conditional_seek_restores_canonical_pcm_and_discards_the_receipt() {
        let (mut native, state, mut audio, mut output) = setup();
        native.reconcile(
            &state,
            Context {
                snapshot: true,
                ..context()
            },
            &mut audio,
        );
        assert!(output.nth(159).unwrap() > 0.0);
        let seek = command(&state, Action::Seek { position_ms: 40 }, None);
        let work = native.execute(
            &state,
            Context {
                command: Some(&seek),
                ..context()
            },
            &mut audio,
        );
        assert_eq!(
            report(&work).settlements[0].status,
            SettlementStatus::Applied
        );
        let sought = audio.run_identity();
        assert_eq!(position_ms(audio.facts()), 40);
        let work = native.reconcile(
            &state,
            Context {
                snapshot: true,
                ..context()
            },
            &mut audio,
        );
        assert!(report(&work).settlements.is_empty());
        assert_eq!(report(&work).observation.position_ms, 0);
        assert_ne!(audio.run_identity(), sought);
    }

    #[test]
    fn applied_next_survives_full_settlement_queue_and_echo_keeps_the_run() {
        let (mut native, mut state, mut audio, mut output) = setup();
        let work = native.reconcile(
            &state,
            Context {
                snapshot: true,
                ..context()
            },
            &mut audio,
        );
        native.complete(
            &state,
            context(),
            &mut audio,
            &preview(&work),
            Some(wav().into()),
        );
        for i in 0..64 {
            native.settle(format!("other:{}", i + 1), SettlementStatus::Applied);
        }
        let next = command(&state, Action::Next, Some(2));
        let work = native.execute(
            &state,
            Context {
                command: Some(&next),
                ..context()
            },
            &mut audio,
        );
        let sent = report(&work);
        assert_eq!(sent.settlements.len(), 64);
        assert_eq!(sent.settlements[0].id, next.id);
        state.cursor = sent.settlements[0].cursor.clone().unwrap();
        assert_eq!(state.cursor.key, 2);
        assert_eq!(
            native
                .tick(&state, context(), &mut audio)
                .iter()
                .find_map(|w| match w {
                    Work::Report(r) => Some(r.settlements[0].id.clone()),
                    _ => None,
                })
                .as_deref(),
            Some(next.id.as_str()),
            "without enqueue ACK, keep the applied receipt"
        );
        native.report_sent(sent);
        assert!(output.nth(159).unwrap() > 0.0);
        let run = audio.run_identity();
        let work = native.reconcile(
            &state,
            Context {
                command: Some(&next),
                revision: 11,
                ..context()
            },
            &mut audio,
        );
        assert_eq!(audio.run_identity(), run);
        assert_eq!(report(&work).observation.position_ms, 20);
        assert_eq!(report(&work).settlements.len(), 1);
        assert_eq!(report(&work).settlements[0].id, "other:64");
    }

    #[test]
    fn same_binding_corrective_snapshot_discards_uncommitted_next_and_its_settlement() {
        let (mut native, state, mut audio, _) = setup();
        let work = native.reconcile(
            &state,
            Context {
                snapshot: true,
                ..context()
            },
            &mut audio,
        );
        native.complete(
            &state,
            context(),
            &mut audio,
            &preview(&work),
            Some(wav().into()),
        );
        let next = command(&state, Action::Next, Some(2));
        let work = native.execute(
            &state,
            Context {
                command: Some(&next),
                ..context()
            },
            &mut audio,
        );
        assert_eq!(
            report(&work).settlements[0].status,
            SettlementStatus::Applied
        );
        assert_eq!(native.cursor().unwrap().key, 2);
        let successor_run = audio.run_identity();
        let work = native.reconcile(
            &state,
            Context {
                snapshot: true,
                ..context()
            },
            &mut audio,
        );
        assert_eq!(native.cursor(), Some(&state.cursor));
        assert_ne!(audio.run_identity(), successor_run);
        assert!(report(&work).settlements.is_empty());
        assert!(report(&work).advance.is_none());
    }

    #[test]
    fn detached_settings_change_retained_queue_without_touching_native_run() {
        let (mut native, state, mut audio, _) = setup();
        native.reconcile(
            &state,
            Context {
                snapshot: true,
                ..context()
            },
            &mut audio,
        );
        let run = audio.run_identity();
        let detached = Context {
            mode: Mode::Detached,
            ..context()
        };
        native.act_detached(
            Action::Repeat {
                repeat: Repeat::All,
            },
            &state,
            detached,
            &mut audio,
        );
        native.act_detached(
            Action::Shuffle {
                shuffle: true,
                seed: Some(u32::MAX),
            },
            &state,
            detached,
            &mut audio,
        );
        let state = native.state().unwrap();
        assert_eq!(state.repeat, Repeat::All);
        assert!(state.shuffle);
        assert_eq!(
            state.queue.iter().map(|r| r.key).collect::<Vec<_>>(),
            [1, 3, 2]
        );
        assert_eq!(native.detached_snapshot(&audio).unwrap().queue[1].key, 3);
        assert_eq!(audio.run_identity(), run);
    }

    #[test]
    fn detached_keyed_edits_export_modified_queue_without_reloading_pcm() {
        let (mut native, mut canonical, mut audio, mut output) = setup();
        canonical.collection_first = 2;
        native.reconcile(
            &canonical,
            Context {
                snapshot: true,
                ..context()
            },
            &mut audio,
        );
        assert!(output.nth(159).unwrap() > 0.0);
        let run = audio.run_identity();
        let detached = Context {
            mode: Mode::Detached,
            ..context()
        };
        native.act_detached(Action::Remove { key: 1 }, &canonical, detached, &mut audio);
        native.act_detached(
            Action::Move { key: 3, after: 0 },
            &canonical,
            detached,
            &mut audio,
        );
        let moved = native.detached_snapshot(&audio).unwrap();
        assert_eq!(
            moved.queue.iter().map(|r| r.key).collect::<Vec<_>>(),
            [1, 3, 2]
        );
        assert_eq!(moved.queue[1].intent, Intent::High);
        assert_eq!(moved.collection_first, 0);
        native.act_detached(Action::Remove { key: 2 }, &canonical, detached, &mut audio);
        assert_eq!(
            native
                .state()
                .unwrap()
                .queue
                .iter()
                .map(|r| r.key)
                .collect::<Vec<_>>(),
            [1, 3]
        );
        native.act_detached(Action::Clear, &canonical, detached, &mut audio);
        assert!(output.nth(159).unwrap() > 0.0);
        let exported = native.detached_snapshot(&audio).unwrap();
        assert_eq!(
            exported.queue.iter().map(|r| r.key).collect::<Vec<_>>(),
            [1]
        );
        assert_eq!(exported.cursor.position_ms, 40);
        assert_eq!(audio.run_identity(), run);

        // A live-owner rebind recognizes the offered cursor but keeps the
        // authoritative queue from its snapshot, not the detached list.
        canonical.cursor = exported.cursor;
        native.reconcile(
            &canonical,
            Context {
                snapshot: true,
                revision: 11,
                ..context()
            },
            &mut audio,
        );
        assert_eq!(native.state().unwrap().queue, canonical.queue);
        assert_eq!(audio.run_identity(), run);
    }

    #[test]
    fn detached_close_stops_and_cancels_pending_queue_media() {
        let (mut native, state, mut audio, _) = setup();
        let work = native.reconcile(
            &state,
            Context {
                snapshot: true,
                ..context()
            },
            &mut audio,
        );
        let old = preview(&work);
        let work = native.act_detached(
            Action::Close,
            &state,
            Context {
                mode: Mode::Detached,
                ..context()
            },
            &mut audio,
        );
        assert!(
            work.iter()
                .any(|w| matches!(w, Work::Cancel(r) if r == &old))
        );
        assert!(
            !work
                .iter()
                .any(|w| matches!(w, Work::Report(_) | Work::Prepare(_)))
        );
        let closed = native.detached_snapshot(&audio).unwrap();
        assert!(closed.queue.is_empty());
        assert_eq!(closed.cursor, Cursor::default());
        assert_eq!(audio.run_identity(), None);
        assert!(native.stable_primary(&audio).is_none());
    }

    #[test]
    fn changed_successor_stales_next_and_late_media_cannot_start_it() {
        let (mut native, mut state, mut audio, _) = setup();
        let work = native.reconcile(
            &state,
            Context {
                snapshot: true,
                ..context()
            },
            &mut audio,
        );
        let old = preview(&work);
        let next = command(&state, Action::Next, Some(2));
        state.queue.swap(1, 2);
        let work = native.reconcile(
            &state,
            Context {
                revision: 11,
                ..context()
            },
            &mut audio,
        );
        assert!(
            work.iter()
                .any(|w| matches!(w, Work::Cancel(r) if *r == old))
        );
        let run = audio.run_identity();
        let work = native.execute(
            &state,
            Context {
                command: Some(&next),
                revision: 11,
                ..context()
            },
            &mut audio,
        );
        assert_eq!(report(&work).settlements[0].status, SettlementStatus::Stale);
        native.complete(&state, context(), &mut audio, &old, Some(wav().into()));
        assert_eq!(audio.run_identity(), run);
        assert!(native.stable_input("b", &audio).is_none());
    }

    #[test]
    fn detached_owner_exports_advanced_measured_run_and_retains_it_on_reattach() {
        let (mut native, state, mut audio, mut output) = setup();
        let work = native.reconcile(
            &state,
            Context {
                snapshot: true,
                ..context()
            },
            &mut audio,
        );
        native.complete(
            &state,
            context(),
            &mut audio,
            &preview(&work),
            Some(wav().into()),
        );
        consume_eof(&mut output, &audio);
        let work = native.tick(
            &state,
            Context {
                mode: Mode::Detached,
                ..context()
            },
            &mut audio,
        );
        assert!(!work.iter().any(|w| matches!(w, Work::Report(_))));
        assert!(output.nth(159).unwrap() > 0.0);
        let offered = native.detached_snapshot(&audio).unwrap();
        assert_eq!(offered.cursor.key, 2);
        assert_eq!(offered.cursor.position_ms, 20);
        assert_eq!(offered.queue, state.queue);
        let run = audio.run_identity();
        let work = native.reconcile(
            &offered,
            Context {
                snapshot: true,
                revision: 11,
                mode: Mode::Owner {
                    connection_id: "reattached",
                },
                ..context()
            },
            &mut audio,
        );
        assert_eq!(report(&work).observation.status, ObservationStatus::Playing);
        assert_eq!(audio.run_identity(), run);
    }

    #[test]
    fn local_snapshot_reads_new_progress_without_overwriting_hello_witness() {
        let (mut native, state, mut audio, mut output) = setup();
        native.reconcile(
            &state,
            Context {
                snapshot: true,
                ..context()
            },
            &mut audio,
        );
        let detached = Context {
            mode: Mode::Detached,
            ..context()
        };
        native.tick(&state, detached, &mut audio);
        assert!(output.nth(159).unwrap() > 0.0);
        let hello = native.detached_snapshot(&audio).unwrap();
        assert!(output.nth(159).unwrap() > 0.0);
        assert_eq!(hello.cursor.position_ms, 20);
        assert_eq!(
            native.local_snapshot(&audio).unwrap().cursor.position_ms,
            40
        );
        let run = audio.run_identity();
        let work = native.reconcile(
            &hello,
            Context {
                snapshot: true,
                revision: 11,
                ..context()
            },
            &mut audio,
        );
        assert_eq!(audio.run_identity(), run);
        assert_eq!(report(&work).observation.position_ms, 40);
    }

    #[test]
    fn explicit_reconcile_claim_witness_retains_already_sought_pcm_on_matching_reply() {
        let (mut native, state, mut audio, mut output) = setup();
        native.reconcile(
            &state,
            Context {
                snapshot: true,
                ..context()
            },
            &mut audio,
        );
        let detached = Context {
            mode: Mode::Detached,
            ..context()
        };
        native.tick(&state, detached, &mut audio);
        assert!(output.nth(159).unwrap() > 0.0);
        let hello = native.detached_snapshot(&audio).unwrap();
        native.act_detached(
            Action::Seek { position_ms: 40 },
            &state,
            detached,
            &mut audio,
        );
        assert!(output.nth(159).unwrap() > 0.0);
        let claim = native.local_snapshot(&audio).unwrap();
        assert_eq!(claim.cursor.position_ms, 60);
        assert_eq!(native.offered.as_ref().unwrap().cursor, hello.cursor);
        native.witness_claim_cursor(&claim.cursor, &audio);
        let run = audio.run_identity();
        let work = native.reconcile(
            &claim,
            Context {
                snapshot: true,
                revision: 12,
                ..context()
            },
            &mut audio,
        );
        assert_eq!(audio.run_identity(), run);
        assert_eq!(report(&work).observation.position_ms, 60);
        assert_eq!(report(&work).observation.status, ObservationStatus::Playing);
    }

    #[test]
    fn rejected_detached_offer_does_not_keep_pcm_at_unaccepted_progress() {
        let (mut native, state, mut audio, mut output) = setup();
        native.reconcile(
            &state,
            Context {
                snapshot: true,
                ..context()
            },
            &mut audio,
        );
        let detached = Context {
            mode: Mode::Detached,
            ..context()
        };
        native.tick(&state, detached, &mut audio);
        assert!(output.nth(159).unwrap() > 0.0);
        let offered = native.detached_snapshot(&audio).unwrap();
        assert_eq!(offered.cursor.position_ms, 20);
        let detached_run = audio.run_identity();
        assert_eq!(native.cursor(), Some(&state.cursor)); // The old intent anchor is unchanged.

        // A newer shared revision invalidated the basis. The exact old
        // intent anchor is not acknowledgment of the offered measured position.
        let work = native.reconcile(
            &state,
            Context {
                mode: Mode::Owner {
                    connection_id: "replacement",
                },
                revision: 11,
                snapshot: true,
                ..context()
            },
            &mut audio,
        );
        assert_ne!(audio.run_identity(), detached_run);
        assert_eq!(report(&work).observation.position_ms, 0);
        assert_eq!(report(&work).observation.status, ObservationStatus::Loading);
    }

    #[test]
    fn accepted_duration_normalized_offer_retains_pcm() {
        let (mut native, mut state, mut audio, mut output) = setup();
        state.queue[0].duration = 0.015; // The decoder's WAV runs longer.
        native.reconcile(
            &state,
            Context {
                snapshot: true,
                ..context()
            },
            &mut audio,
        );
        native.tick(
            &state,
            Context {
                mode: Mode::Detached,
                ..context()
            },
            &mut audio,
        );
        assert!(output.nth(159).unwrap() > 0.0);
        let exported = native.detached_snapshot(&audio).unwrap();
        assert_eq!(exported.cursor.position_ms, 15);
        let run = audio.run_identity();
        let work = native.reconcile(
            &exported,
            Context {
                mode: Mode::Owner {
                    connection_id: "replacement",
                },
                revision: 11,
                snapshot: true,
                ..context()
            },
            &mut audio,
        );
        assert_eq!(audio.run_identity(), run);
        assert_eq!(report(&work).observation.position_ms, 20);
        assert_eq!(report(&work).observation.status, ObservationStatus::Playing);
    }

    #[test]
    fn newer_metadata_does_not_relabel_pending_eof() {
        let (mut native, mut state, mut audio, mut output) = setup();
        state.repeat = Repeat::One;
        native.reconcile(
            &state,
            Context {
                snapshot: true,
                ..context()
            },
            &mut audio,
        );
        consume_eof(&mut output, &audio);
        native.tick(&state, context(), &mut audio);
        let successor = native.cursor().unwrap().clone();
        let run = audio.run_identity();
        state.queue.swap(1, 2);
        let work = native.reconcile(
            &state,
            Context {
                revision: 11,
                ..context()
            },
            &mut audio,
        );
        let reports: Vec<_> = work
            .iter()
            .filter_map(|w| {
                if let Work::Report(r) = w {
                    Some(r)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(reports.len(), 2);
        assert!(reports[0].advance.is_some());
        assert_eq!(reports[0].revision, 10);
        assert!(reports[0].settlements.is_empty());
        let observed = reports[1];
        assert_eq!(observed.revision, 11);
        assert!(observed.advance.is_none());
        assert!(observed.settlements.is_empty());
        assert_eq!(observed.observation.occurrence, successor.occurrence);
        assert_eq!(observed.observation.position_ms, 0);
        assert_eq!(audio.run_identity(), run);
        for observed in reports {
            native.report_sent(observed);
        }
        assert!(
            native
                .tick(
                    &state,
                    Context {
                        revision: 11,
                        ..context()
                    },
                    &mut audio
                )
                .iter()
                .all(|w| !matches!(w, Work::Report(r) if r.advance.is_some()))
        );

        // A corrective snapshot restores the original visit when the server
        // rejects old-revision EOF against a newer traversal revision.
        let work = native.reconcile(
            &state,
            Context {
                snapshot: true,
                revision: 11,
                ..context()
            },
            &mut audio,
        );
        assert!(report(&work).advance.is_none());
        assert_eq!(report(&work).observation.position_ms, 0);
        assert_eq!(native.cursor(), Some(&state.cursor));
    }

    #[test]
    fn exported_saved_pcm_and_detached_pause_keep_the_measured_cursor() {
        let (mut native, mut state, mut audio, mut output) = setup();
        assert_eq!(
            audio.play("a".into(), wav(), 0.0, 0.1, Authority::offline()),
            crate::player::Status::Ok
        );
        assert!(output.nth(159).unwrap() > 0.0);
        state.cursor.position_ms = 20;
        native.adopt_local(&state, &audio);
        assert!(output.nth(159).unwrap() > 0.0);
        let run = audio.run_identity();
        let work = native.reconcile(
            &state,
            Context {
                snapshot: true,
                ..context()
            },
            &mut audio,
        );
        assert_eq!(audio.run_identity(), run);
        assert_eq!(report(&work).observation.position_ms, 40);
        let work = native.act_detached(
            Action::Pause,
            &state,
            Context {
                mode: Mode::Detached,
                ..context()
            },
            &mut audio,
        );
        assert!(!work.iter().any(|w| matches!(w, Work::Report(_))));
        let paused = native.detached_snapshot(&audio).unwrap();
        assert!(!paused.cursor.desired_playing);
        assert_eq!(paused.cursor.position_ms, 40);
        assert!(native.stable_primary(&audio).is_some());
    }

    #[test]
    fn detached_seek_does_not_land_on_an_eof_successor_or_repeat() {
        for repeat in [Repeat::Off, Repeat::One] {
            let (mut native, mut state, mut audio, mut output) = setup();
            state.repeat = repeat;
            let work = native.reconcile(
                &state,
                Context {
                    snapshot: true,
                    ..context()
                },
                &mut audio,
            );
            if repeat == Repeat::Off {
                native.complete(
                    &state,
                    context(),
                    &mut audio,
                    &preview(&work),
                    Some(wav().into()),
                );
            }
            consume_eof(&mut output, &audio);
            let work = native.act_detached(
                Action::Seek { position_ms: 60 },
                &state,
                Context {
                    mode: Mode::Detached,
                    ..context()
                },
                &mut audio,
            );
            assert!(!work.iter().any(|w| matches!(w, Work::Report(_))));
            let actual = native.cursor().unwrap();
            assert_eq!(actual.key, if repeat == Repeat::One { 1 } else { 2 });
            assert_ne!(actual.occurrence, state.cursor.occurrence);
            assert_eq!(actual.position_ms, 0);
            assert!(output.nth(159).unwrap() > 0.0);
            assert_eq!(position_ms(audio.facts()), 20);
        }
    }

    #[test]
    fn after_tail_eof_a_valid_conditional_basis_survives_unrelated_metadata() {
        for action in [Action::Seek { position_ms: 40 }, Action::Next] {
            let (mut native, mut state, mut audio, mut output) = setup();
            state.queue.truncate(1);
            native.reconcile(
                &state,
                Context {
                    snapshot: true,
                    ..context()
                },
                &mut audio,
            );
            consume_eof(&mut output, &audio);
            let work = native.tick(&state, context(), &mut audio);
            assert_eq!(report(&work).advance.as_ref().unwrap().to_key, 0);
            native.report_sent(report(&work));
            state.cursor = native.cursor().unwrap().clone();
            let confirmed = Context {
                revision: 11,
                ..context()
            };
            native.reconcile(&state, confirmed, &mut audio);
            let at = if matches!(action, Action::Next) {
                Some(0)
            } else {
                None
            };
            let mut cmd = command(&state, action, at);
            cmd.basis.revision = confirmed.revision; // At, not before, the canonical EOF fence.
            state.shuffle = true;
            let after_meta = Context {
                revision: 12,
                ..context()
            };
            native.reconcile(&state, after_meta, &mut audio);
            let work = native.execute(
                &state,
                Context {
                    command: Some(&cmd),
                    ..after_meta
                },
                &mut audio,
            );
            let reported = report(&work);
            assert!(reported.advance.is_none());
            let settlement = &reported.settlements[0];
            assert_eq!(settlement.status, SettlementStatus::Applied);
            let target = settlement.cursor.as_ref().unwrap();
            assert_eq!(target.occurrence, state.cursor.occurrence);
            assert!(!target.desired_playing);
            assert_eq!(target.position_ms, if at.is_some() { 0 } else { 40 });
            assert_eq!(audio.run_identity(), None);
        }
    }

    #[test]
    fn repeat_one_revisits_and_repeat_off_eof_preserves_selected_row_at_zero() {
        for repeat in [Repeat::Off, Repeat::One] {
            let (mut native, mut state, mut audio, mut output) = setup();
            state.repeat = repeat;
            state.queue.truncate(1);
            native.reconcile(
                &state,
                Context {
                    snapshot: true,
                    ..context()
                },
                &mut audio,
            );
            consume_eof(&mut output, &audio);
            let work = native.tick(&state, context(), &mut audio);
            let report = report(&work);
            let advance = report.advance.as_ref().unwrap();
            assert_eq!(native.cursor().unwrap().key, 1);
            if repeat == Repeat::Off {
                assert_eq!(advance.to_key, 0);
                assert_eq!(report.observation.status, ObservationStatus::Stopped);
                assert_eq!(report.observation.position_ms, 0);
            } else {
                assert_eq!(advance.to_key, 1);
                assert_ne!(advance.to_occurrence, state.cursor.occurrence);
            }
        }
    }
}
