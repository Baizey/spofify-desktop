//! Account/transport generation fence between the engine and the desktop actor.
//! A watch notification is only a wakeup: the bounded reducer applies every
//! socket frame before readers see it, including frames coalesced by the watch.
use crate::{
    api::library::{PlaylistKind, PlaylistSummary, RuleDefinition},
    personal::{PersonalCommand, PersonalUpdate, Replica},
};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};
use tokio::sync::watch;

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

struct State {
    status: Status,
    replica: Replica,
    observations: VecDeque<Observation>,
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
                sequence: 0,
            },
            replica: Replica::default(),
            observations: VecDeque::new(),
        }
    }
}
impl State {
    fn scoped(&self, epoch: u64, generation: u64) -> bool {
        self.status.epoch == epoch && self.status.generation == generation
    }
}

#[derive(Clone)]
pub struct PersonalAccess {
    state: Arc<Mutex<State>>,
    changed: watch::Sender<u64>,
}
impl Default for PersonalAccess {
    fn default() -> Self {
        Self {
            state: Arc::default(),
            changed: watch::channel(0).0,
        }
    }
}
impl PersonalAccess {
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
        let command = state.replica.watch(id);
        if state.status.link == Link::Ready {
            Some((state.status.generation, command?))
        } else {
            None
        }
    }
    /// Return evidence of matching server state, never a command receipt. A
    /// concurrent client can produce the same change; do not attribute it here.
    pub(crate) fn observed(
        &self,
        before: Status,
        effect: &Effect,
    ) -> Option<Option<crate::pages::Route>> {
        use crate::pages::Route;
        let state = self.state.lock().expect("personal state");
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

#[derive(Default)]
pub struct PersonalHub(PersonalAccess);
impl PersonalHub {
    pub fn access(&self) -> PersonalAccess {
        self.0.clone()
    }
    pub fn reset(&self, epoch: u64) {
        let mut state = self.0.state.lock().expect("personal state");
        if epoch > state.status.epoch {
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
        state.replica.disconnected();
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
            state.replica.disconnected();
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
        let mut state = self.0.state.lock().expect("personal state");
        if !state.scoped(epoch, generation) || state.status.link == Link::Disconnected {
            return Ok(None);
        }
        let snapshot = matches!(&update, PersonalUpdate::Snapshot(_));
        if !snapshot && state.status.link != Link::Ready {
            // A repair snapshot may be queued behind older in-flight updates.
            if state.status.link == Link::Gap {
                return Ok(None);
            }
            state.status.link = Link::Gap;
            state.status.interruption = state.status.interruption.wrapping_add(1);
            state.replica.disconnected();
            self.0.signal();
            return Err(PersonalGap);
        }
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
        let result = match state.replica.receive(update) {
            Ok(command) => {
                if snapshot {
                    state.status.link = Link::Ready;
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
                state.replica.disconnected();
                state.observations.clear();
                Err(PersonalGap)
            }
        };
        self.0.signal();
        result
    }
    pub fn refused(&self, epoch: u64, generation: u64) {
        let mut state = self.0.state.lock().expect("personal state");
        if state.scoped(epoch, generation) {
            state.status.refused = state.status.refused.wrapping_add(1);
            self.0.signal();
        }
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
