//! Account player replica and native output binding. No controller command or
//! catalog selection can create an execution: only published state plus a grant.
use crate::{
    player::{self, Authority},
    socket::{
        Cursor, Fact, FactError, Generation, Link, OutputAuthority, PlayerDelta, QueueEditKind,
        Repeat, State, Track,
    },
};
use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};
use tokio::sync::watch;

pub(super) fn apply(state: &mut State, update: &PlayerDelta) -> Result<bool, ()> {
    let cursor = update.cursor.as_ref().unwrap_or(&state.cursor);
    let changed = !state.cursor.same_run(cursor) || state.cursor.desired != cursor.desired;
    if let Some(edit) = &update.queue {
        let mut queue = match edit.kind {
            QueueEditKind::Replace => edit.rows.clone(),
            QueueEditKind::Insert => {
                let at = if edit.before == 0 {
                    state.queue.len()
                } else {
                    state
                        .queue
                        .iter()
                        .position(|r| r.key == edit.before)
                        .ok_or(())?
                };
                state.queue[..at]
                    .iter()
                    .chain(&edit.rows)
                    .chain(&state.queue[at..])
                    .cloned()
                    .collect()
            }
            QueueEditKind::Remove => {
                let gone: HashSet<_> = edit.keys.iter().copied().collect();
                let queue: Vec<_> = state
                    .queue
                    .iter()
                    .filter(|r| !gone.contains(&r.key))
                    .cloned()
                    .collect();
                if state.queue.len() - queue.len() != gone.len() || gone.len() != edit.keys.len() {
                    return Err(());
                }
                queue
            }
            QueueEditKind::Order => {
                if edit.keys.len() != state.queue.len() {
                    return Err(());
                }
                let mut rows: HashMap<_, _> = state.queue.iter().map(|r| (r.key, r)).collect();
                let rebase = if edit.rebase_after == 0 {
                    None
                } else {
                    Some(
                        edit.keys
                            .iter()
                            .position(|k| *k == edit.rebase_after)
                            .ok_or(())?,
                    )
                };
                edit.keys
                    .iter()
                    .enumerate()
                    .map(|(i, key)| {
                        let mut row = rows.remove(key).ok_or(())?.clone();
                        if let Some(pos) = rebase {
                            row.order = i as u64 + 1;
                            if i > pos {
                                row.intent = crate::socket::Intent::High;
                            }
                        }
                        Ok(row)
                    })
                    .collect::<Result<_, ()>>()?
            }
        };
        let first = match edit.kind {
            QueueEditKind::Replace => edit.collection_first,
            QueueEditKind::Insert if state.queue.is_empty() => edit.collection_first,
            QueueEditKind::Order if edit.rebase_after == 0 => state.collection_first,
            _ => 0,
        };
        if !valid_queue(&queue, cursor, first) {
            return Err(());
        }
        std::mem::swap(&mut queue, &mut state.queue);
        state.collection_first = first;
    } else if update.cursor.is_some() && !cursor_in(&state.queue, cursor) {
        return Err(());
    }
    if let Some(cursor) = &update.cursor {
        state.cursor = cursor.clone();
    }
    if let Some(repeat) = update.repeat {
        state.repeat = repeat;
    }
    if let Some(shuffle) = update.shuffle {
        state.shuffle = shuffle;
    }
    Ok(changed || update.queue.is_some() || update.repeat.is_some() || update.shuffle.is_some())
}

pub(super) fn valid(state: &State) -> bool {
    valid_queue(&state.queue, &state.cursor, state.collection_first)
}

fn valid_queue(queue: &[Track], cursor: &Cursor, first: u64) -> bool {
    let mut keys = HashSet::with_capacity(queue.len());
    queue
        .iter()
        .all(|r| player::key(r.key) && keys.insert(r.key))
        && (first == 0 || keys.contains(&first))
        && cursor_in(queue, cursor)
}

fn cursor_in(queue: &[Track], cursor: &Cursor) -> bool {
    // Go can bump an empty cursor's run when assigning/releasing an output.
    if cursor.key == 0 {
        queue.is_empty() && !cursor.desired && !cursor.playing
    } else {
        !cursor.chain.is_empty()
            && cursor.elapsed.is_finite()
            && cursor.elapsed >= 0.0
            && (!cursor.playing || cursor.desired)
            && queue.iter().any(|r| r.key == cursor.key)
    }
}

pub(super) fn same_execution(a: &Cursor, b: &Cursor) -> bool {
    a.same_run(b) && a.desired == b.desired
}

/// The exact Go natural traversal. The returned cursor is provisional until
/// the server publishes it; positional edits may change where Go lands.
pub(super) fn successor(state: &State, base: &Cursor) -> Option<Cursor> {
    let pos = state.queue.iter().position(|r| r.key == base.key)?;
    let to = match state.repeat {
        Repeat::One => Some(pos),
        Repeat::All => Some((pos + 1) % state.queue.len()),
        Repeat::Off => (pos + 1 < state.queue.len()).then_some(pos + 1),
    };
    Some(match to {
        Some(i) => Cursor {
            key: state.queue[i].key,
            chain: base.chain.clone(),
            visit: base.visit.checked_add(1)?,
            run: 0,
            elapsed: 0.0,
            desired: true,
            playing: false,
        },
        None => Cursor {
            elapsed: 0.0,
            desired: false,
            playing: false,
            run: base.run.checked_add(1)?,
            ..base.clone()
        },
    })
}

pub(super) fn fact(
    base: &Cursor,
    elapsed: f64,
    playing: bool,
    buffering: bool,
    landed: Option<u64>,
    error: Option<FactError>,
) -> Fact {
    Fact {
        key: base.key,
        chain: base.chain.clone(),
        visit: base.visit,
        run: base.run,
        elapsed,
        playing: playing && landed.is_none(),
        buffering: buffering && landed.is_none(),
        ended: landed.is_some(),
        landed,
        error,
    }
}

/// An active run accepts renewals from its validated generation. If transport
/// attaches another generation before the actor sees its canonical cursor,
/// the old run may continue only to its already-accepted deadline.
#[derive(Clone)]
pub(super) struct RunPermit {
    trusted: Arc<Mutex<OutputAuthority>>,
    watch: watch::Receiver<Option<OutputAuthority>>,
}
impl RunPermit {
    pub fn new(watch: watch::Receiver<Option<OutputAuthority>>, grant: OutputAuthority) -> Self {
        Self {
            trusted: Arc::new(Mutex::new(grant)),
            watch,
        }
    }
    pub fn adopt(&self, grant: OutputAuthority) -> bool {
        let now = Instant::now();
        self.trusted.lock().is_ok_and(|mut current| {
            if !current.permits(now)
                || !grant.attached
                || !grant.permits(now)
                || grant.generation < current.generation
            {
                return false;
            }
            if grant.generation != current.generation || grant.deadline > current.deadline {
                *current = grant;
            }
            true
        })
    }
    pub fn granted(&self) -> bool {
        let now = Instant::now();
        if self.watch.has_changed().is_err() {
            return false;
        }
        let Some(wire) = *self.watch.borrow() else {
            return false;
        };
        self.trusted.lock().is_ok_and(|mut current| {
            if !current.permits(now) || !wire.permits(now) {
                return false;
            }
            if current.generation == wire.generation && wire.deadline > current.deadline {
                *current = wire;
            }
            true
        })
    }
    pub fn source(&self, link: watch::Receiver<Link>, generation: Generation) -> Authority {
        self.source_with_preview(link, generation, None)
    }
    /// A queued successor may overlap locally only under a live attached
    /// grant. It acquires ordinary run grace only after canonical confirmation
    /// of the exact successor, not merely admission of the EOF fact.
    pub fn preview_source(
        &self,
        link: watch::Receiver<Link>,
        generation: Generation,
        confirmed: Arc<AtomicBool>,
    ) -> Authority {
        self.source_with_preview(link, generation, Some(confirmed))
    }
    fn source_with_preview(
        &self,
        link: watch::Receiver<Link>,
        generation: Generation,
        confirmed: Option<Arc<AtomicBool>>,
    ) -> Authority {
        let permit = self.clone();
        let start_grant = self.watch.clone();
        let start = move || {
            link.has_changed().is_ok()
                && *link.borrow() == Link::Connected(generation)
                && start_grant.borrow().is_some_and(|g| {
                    g.attached && g.generation == generation && g.permits(Instant::now())
                })
        };
        let preview_start = start.clone();
        Authority::online(
            move || {
                confirmed
                    .as_ref()
                    .is_none_or(|c| c.load(Ordering::Acquire) || preview_start())
                    && permit.granted()
            },
            start,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Duration;

    fn state() -> State {
        serde_json::from_value(json!({
            "queue": [
              {"key":8,"order":8,"id":"duplicate","intent":"low"},
              {"key":9,"order":9,"id":"duplicate","intent":"low"},
              {"key":10,"order":10,"id":"tail","intent":"low"}
            ],
            "cursor":{"key":8,"chain":"fixture","visit":2,"run":1,"elapsed":0,"desired":true,"playing":false},
            "collectionFirst":8,"repeat":"off","shuffle":false
        })).unwrap()
    }

    #[test]
    fn go_eof_lands_by_occurrence_and_requires_landed_even_at_terminal() {
        let mut state = state();
        let next = successor(&state, &state.cursor).unwrap();
        assert_eq!(
            (next.key, next.visit, next.run, next.elapsed),
            (9, 3, 0, 0.0)
        );
        let report = fact(&state.cursor, 61.0, true, true, Some(next.key), None);
        assert!(report.ended && !report.playing && !report.buffering);
        assert_eq!(report.landed, Some(9));
        assert_eq!((report.key, report.visit, report.run), (8, 2, 1));
        state.cursor.key = 10;
        let terminal = successor(&state, &state.cursor).unwrap();
        assert!(!terminal.desired && !terminal.playing);
        assert_eq!(
            (terminal.key, terminal.visit, terminal.run, terminal.elapsed),
            (10, 2, 2, 0.0)
        );
        let end = fact(&state.cursor, 23.0, false, false, Some(0), None);
        assert!(end.ended);
        assert_eq!(end.elapsed, 23.0);
        assert_eq!(serde_json::to_value(end).unwrap()["landed"], 0);
        state.repeat = Repeat::All;
        assert_eq!(successor(&state, &state.cursor).unwrap().key, 8);
        state.repeat = Repeat::One;
        let replay = successor(&state, &state.cursor).unwrap();
        assert_eq!((replay.key, replay.visit, replay.run), (10, 3, 0));
    }

    #[test]
    fn queue_delta_rebases_traversal_and_rejects_a_gap_without_partial_mutation() {
        let mut state = state();
        let order: PlayerDelta = serde_json::from_value(
            json!({"queue":{"kind":"order","keys":[8,10,9],"rebaseAfter":8},"shuffle":false}),
        )
        .unwrap();
        assert_eq!(apply(&mut state, &order), Ok(true));
        assert_eq!(state.collection_first, 0);
        assert_eq!(
            state
                .queue
                .iter()
                .map(|t| (t.key, t.order, t.intent))
                .collect::<Vec<_>>(),
            [
                (8, 1, crate::socket::Intent::Low),
                (10, 2, crate::socket::Intent::High),
                (9, 3, crate::socket::Intent::High)
            ]
        );
        let before = state.clone();
        let gap: PlayerDelta =
            serde_json::from_value(json!({"queue":{"kind":"remove","keys":[8]},"repeat":"one"}))
                .unwrap();
        assert_eq!(apply(&mut state, &gap), Err(()));
        assert_eq!(state, before);
        let fact: PlayerDelta = serde_json::from_value(json!({"observed":true,"cursor":{"key":8,"chain":"fixture","visit":2,"run":1,"elapsed":4,"desired":true,"playing":true}})).unwrap();
        assert_eq!(apply(&mut state, &fact), Ok(false));
        assert_eq!(state.cursor.elapsed, 4.0);
    }

    #[test]
    fn only_an_existing_run_can_continue_detached_and_adopt_a_validated_reattach() {
        let now = Instant::now();
        let first = OutputAuthority {
            generation: 7,
            deadline: now + Duration::from_secs(4),
            attached: true,
            resume_through: 0,
        };
        let (grants, rx) = watch::channel(Some(first));
        let (links, link) = watch::channel(Link::Connected(7));
        let permit = RunPermit::new(rx, first);
        let source = permit.source(link.clone(), 7);
        assert!(source.may_start());
        links.send_replace(Link::Disconnected);
        grants.send_replace(Some(OutputAuthority {
            attached: false,
            ..first
        }));
        assert!(source.is_valid());
        assert!(!source.may_start());
        let reattached = OutputAuthority {
            generation: 8,
            deadline: now + Duration::from_secs(8),
            attached: true,
            resume_through: 0,
        };
        links.send_replace(Link::Connected(8));
        grants.send_replace(Some(reattached));
        assert!(!source.may_start());
        assert!(permit.adopt(reattached));
        assert!(source.is_valid());
        assert!(
            !source.may_start(),
            "the old source has no new-generation start authority"
        );
        grants.send_replace(None);
        assert!(!source.is_valid());
    }

    #[test]
    fn queued_eof_cannot_give_an_unconfirmed_overlap_detached_grace() {
        let grant = OutputAuthority {
            generation: 4,
            deadline: Instant::now() + Duration::from_secs(5),
            attached: true,
            resume_through: 0,
        };
        let (grants, rx) = watch::channel(Some(grant));
        let (links, link) = watch::channel(Link::Connected(4));
        let permit = RunPermit::new(rx, grant);
        let confirmed: Arc<AtomicBool> = Arc::default();
        let committed = permit.preview_source(link.clone(), 4, confirmed.clone());
        let pending = permit.preview_source(link, 4, Arc::default());
        assert!(pending.may_start() && committed.may_start());
        confirmed.store(true, Ordering::Release); // matching successor published by Go
        links.send_replace(Link::Disconnected);
        grants.send_replace(Some(OutputAuthority {
            attached: false,
            ..grant
        }));
        assert!(!pending.is_valid());
        assert!(committed.is_valid());
        assert!(!committed.may_start());
        grants.send_replace(None);
        assert!(!committed.is_valid());
    }

    #[test]
    fn a_new_generation_cannot_extend_an_already_expired_run() {
        let now = Instant::now();
        let old = OutputAuthority {
            generation: 1,
            deadline: now,
            attached: false,
            resume_through: 0,
        };
        let next = OutputAuthority {
            generation: 2,
            deadline: now + Duration::from_secs(8),
            attached: true,
            resume_through: 0,
        };
        let (_grants, rx) = watch::channel(Some(next));
        let permit = RunPermit::new(rx, old);
        assert!(!permit.granted());
        assert!(!permit.adopt(next));
    }
}
