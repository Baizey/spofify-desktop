//! One explicit native start, without a provisional queue row or wire cursor.
#[cfg(test)]
#[path = "engine_local_start_tests.rs"]
mod tests;
use crate::{
    offline::OnlineMedia,
    player::Authority,
    socket::{
        Command, Context, Cursor, DirectStartFence, Generation, Link, OutputAuthority, PlayerDelta,
        QueueEditKind, Selection, State, Track, TrackRef,
    },
};
use std::{
    collections::VecDeque,
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};
use tokio::sync::watch;

const START_WINDOW: Duration = Duration::from_millis(1200);

/// Go serializes the resolver and supersedes older Plays. Its source tag is
/// not an intent ID. Keep older possible replies until an ordered, distinct
/// replacement proves they cannot arrive later.
#[derive(Default)]
pub(super) struct Receipts {
    plays: VecDeque<Play>,
    adds: usize,
    overflow: bool,
    last_resume: u64,
}
struct Play {
    selection: Selection,
    shuffle: bool,
}
impl Play {
    fn could_land_on(
        &self,
        id: &str,
        entry: Option<&str>,
        target: Option<&str>,
        occurrence: Option<&str>,
        context: Option<&Context>,
    ) -> bool {
        match &self.selection {
            Selection::Track {
                track,
                context: source,
            } => {
                same_context(source.as_ref(), context)
                    && (self.shuffle && source.is_some()
                        || (track.id == id
                            && track.entry_id.as_deref() == entry
                            && track.target_id.as_deref() == target
                            && track.occurrence_id.as_deref() == occurrence))
            }
            Selection::Collection { context: source } => same_context(Some(source), context),
        }
    }
}
impl Receipts {
    pub fn may_start(&self, command: &Command, resume_through: u64) -> bool {
        if matches!(command, Command::Resume) {
            // A different paused cursor does not prove an earlier unlabeled
            // Resume was consumed. The normal grant is our ordered barrier.
            return self.last_resume <= resume_through;
        }
        let Command::Play {
            selection: Selection::Track { track, context },
            ..
        } = command
        else {
            return true;
        };
        !self.overflow
            && self.plays.len() < 32
            && !self.plays.iter().any(|p| {
                p.could_land_on(
                    &track.id,
                    track.entry_id.as_deref(),
                    track.target_id.as_deref(),
                    track.occurrence_id.as_deref(),
                    context.as_ref(),
                )
            })
    }
    pub fn sent(&mut self, command: &Command, resume: Option<u64>) {
        match command {
            Command::Play {
                selection, shuffle, ..
            } if !self.overflow => {
                // An unbounded stream of unresolved/superseded choices must
                // never grow the replica. Overflow keeps the canonical path.
                if self.plays.len() == 32 {
                    self.plays.clear();
                    self.overflow = true;
                } else {
                    self.plays.push_back(Play {
                        selection: selection.clone(),
                        shuffle: *shuffle == Some(true),
                    });
                }
            }
            Command::Add { .. } => self.adds = self.adds.saturating_add(1),
            Command::Resume => self.last_resume = resume.expect("admitted Resume has a sequence"),
            _ => {}
        }
    }
    pub fn received(&mut self, delta: &PlayerDelta, state: &State) {
        match delta.queue.as_ref().map(|q| q.kind) {
            Some(QueueEditKind::Insert) => self.adds = self.adds.saturating_sub(1),
            Some(QueueEditKind::Replace) => {
                if let Some(row) = state.current()
                    && let Some(earliest) = self.plays.iter().position(|p| {
                        p.could_land_on(
                            &row.id,
                            row.entry_id.as_deref(),
                            row.target_id.as_deref(),
                            row.occurrence_id.as_deref(),
                            row.playback_context.as_ref(),
                        )
                    })
                {
                    self.plays.drain(..=earliest);
                }
            }
            None if self.adds == 0 => {
                self.plays.pop_front();
            }
            None if self.plays.is_empty() && !self.overflow => {
                self.adds = self.adds.saturating_sub(1)
            }
            _ => {}
        }
    }
}

/// The native source is re-bound in place only when the actor matches the
/// canonical selection and grant. Before that it has no detached grace.
#[derive(Clone)]
pub(super) struct Permit {
    deadline: Instant,
    generation: Generation,
    link: watch::Receiver<Link>,
    grant: watch::Receiver<Option<OutputAuthority>>,
    output: Arc<DirectStartFence>,
    output_stamp: u64,
    committed: Arc<OnceLock<Authority>>,
}
impl Permit {
    pub fn live(&self) -> bool {
        let now = Instant::now();
        now < self.deadline
            && self.grant.borrow().is_none_or(|grant| {
                grant.attached && grant.generation == self.generation && grant.permits(now)
            })
            && self.output.stamp() == Some(self.output_stamp)
            && self.link.has_changed().is_ok()
            && *self.link.borrow() == Link::Connected(self.generation)
    }
    pub fn source(&self) -> Authority {
        let run = self.clone();
        let start = self.clone();
        Authority::online(
            move || {
                run.committed
                    .get()
                    .map_or_else(|| run.live(), Authority::is_valid)
            },
            move || {
                start
                    .committed
                    .get()
                    .map_or_else(|| start.live(), Authority::may_start)
            },
        )
    }
    pub fn commit(&self, source: Authority) -> bool {
        self.live() && source.may_start() && self.committed.set(source).is_ok()
    }
}

enum Choice {
    Play {
        track: TrackRef,
        context: Option<Context>,
    },
    Resume,
}

pub(super) struct Start {
    choice: Choice,
    pub generation: Generation,
    pub id: String,
    pub elapsed: f64,
    pub duration: f64,
    pub permit: Permit,
    pub canonical: Option<Cursor>,
    pub ready: Option<OnlineMedia>,
    pub started: bool,
    base: Cursor,
    // Go publishes SetOutput's run bump immediately before the Outputs update.
    // Only this adjacent pair can rebase a start from known local vacancy.
    pub output_pulse: Option<Cursor>,
}
impl Start {
    pub fn new(
        command: &Command,
        state: &State,
        generation: Generation,
        link: watch::Receiver<Link>,
        grant: watch::Receiver<Option<OutputAuthority>>,
        output: Arc<DirectStartFence>,
    ) -> Option<Self> {
        let output_stamp = output.stamp()?;
        let base = &state.cursor;
        if base.desired || base.playing {
            return None;
        }
        let (choice, id, elapsed, duration) = match command {
            Command::Play {
                selection: Selection::Track { track, context },
                shuffle,
                ..
            } if *shuffle != Some(true) => (
                Choice::Play {
                    track: track.clone(),
                    context: context.clone(),
                },
                track.id.clone(),
                0.0,
                0.0,
            ),
            Command::Resume => {
                let row = state.current()?;
                (Choice::Resume, row.id.clone(), base.elapsed, row.duration)
            }
            _ => return None,
        };
        Some(Self {
            choice,
            generation,
            id,
            elapsed,
            duration,
            permit: Permit {
                deadline: Instant::now() + START_WINDOW,
                generation,
                link,
                grant,
                output,
                output_stamp,
                committed: Arc::default(),
            },
            canonical: None,
            ready: None,
            started: false,
            base: base.clone(),
            output_pulse: None,
        })
    }
    pub fn output_confirmed(&mut self) {
        if let Some(pulse) = self.output_pulse.take() {
            self.base = pulse;
        }
    }
    /// Return false for a competing edit or rejection. Insertions do not
    /// acknowledge Play, even if the same source added the same recording.
    pub fn observe(&mut self, delta: &PlayerDelta, state: &State, own: bool, vacant: bool) -> bool {
        let cursor = &state.cursor;
        let edit = delta.queue.as_ref().map(|q| q.kind);
        if let Some(canonical) = &self.canonical {
            return edit != Some(QueueEditKind::Replace)
                && cursor.same_run(canonical)
                && cursor.desired
                && !cursor.playing
                && cursor.elapsed == self.elapsed;
        }
        if own && edit.is_none() {
            return false;
        }
        let accepted = match &self.choice {
            Choice::Play { track, context } => {
                own && edit == Some(QueueEditKind::Replace)
                    && delta.cursor.is_some()
                    && cursor.desired
                    && !cursor.playing
                    && cursor.elapsed == 0.0
                    && cursor.visit == 0
                    && cursor.run == 0
                    && cursor.chain != self.base.chain
                    && state
                        .current()
                        .is_some_and(|row| exact_row(row, track, context.as_ref()))
            }
            Choice::Resume => {
                delta.source.is_none()
                    && edit.is_none()
                    && delta.cursor.is_some()
                    && !delta.observed
                    && cursor.desired
                    && !cursor.playing
                    && cursor.elapsed == self.elapsed
                    && same_visit(&self.base, cursor)
                    && self.base.run.checked_add(1) == Some(cursor.run)
                    && state.current().is_some_and(|row| row.id == self.id)
            }
        };
        if accepted {
            self.canonical = Some(cursor.clone());
            return true;
        }
        if vacant
            && delta.source.is_none()
            && edit.is_none()
            && !delta.observed
            && delta.cursor.is_some()
            && same_visit(&self.base, cursor)
            && !cursor.desired
            && !cursor.playing
            && cursor.elapsed == self.base.elapsed
            && self.base.run.checked_add(1) == Some(cursor.run)
        {
            self.output_pulse = Some(cursor.clone());
            return true;
        }
        edit != Some(QueueEditKind::Replace) && cursor == &self.base
    }
}

fn same_visit(base: &Cursor, cursor: &Cursor) -> bool {
    base.key == cursor.key && base.chain == cursor.chain && base.visit == cursor.visit
}

fn exact_row(row: &Track, track: &TrackRef, context: Option<&Context>) -> bool {
    row.id == track.id
        && row.entry_id == track.entry_id
        && row.target_id == track.target_id
        && row.occurrence_id == track.occurrence_id
        && same_context(row.playback_context.as_ref(), context)
}

fn same_context(a: Option<&Context>, b: Option<&Context>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => a.kind == b.kind && a.id == b.id,
        _ => false,
    }
}
