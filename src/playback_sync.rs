//! Confirmed room intent plus an initiator-only projection. This reducer never
//! executes a command, turns a position sample into intent, or replays a write.
use std::collections::{HashMap, HashSet};

use crate::socket::{
    Action, Command, CommandBasis, CommandResult, Cursor, Device, Intent, MAX_SAFE_INTEGER,
    Observation, ObservationStatus, ObservationUpdate, Owner, Ownership, PlayerDelta,
    QueueEditKind, ReattachBasis, Repeat, ResultStatus, Snapshot, State, StateUpdate, Track,
};

const MAX_PENDING: usize = 64;
const OBSERVATION_FRESH_MS: u64 = 10_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Apply {
    Ignored,
    Applied,
    /// Request one sync on the current connection. Do not issue commands until
    /// the repair snapshot arrives.
    NeedSync,
}

impl Apply {
    pub(crate) fn needs_sync(self) -> bool {
        self == Self::NeedSync
    }

    /// Only a confirmed state transition should reach the native reconciler.
    pub(crate) fn applied(self) -> bool {
        self == Self::Applied
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RemotePlayback {
    pub(crate) position_ms: u64,
    /// None means there is no current evidence of actual playback, not Pause.
    pub(crate) status: Option<ObservationStatus>,
}

impl RemotePlayback {
    pub(crate) fn playing(self) -> bool {
        self.status == Some(ObservationStatus::Playing)
    }
}

struct Pending {
    id: String,
    basis: CommandBasis,
    action: Action,
    reflected: bool,
    terminal_revision: Option<u64>,
}

#[derive(Default)]
pub(crate) struct Projection {
    confirmed: Option<Snapshot>,
    projected: Option<State>,
    pending: Vec<Pending>,
    // Kept even after a receipt is retired: a command ID cannot be reused.
    last_counter: u64,
    attached: bool,
    needs_sync: bool,
    // Projection may deliberately suppress an otherwise current observation.
    suppress_observation: bool,
}

impl Projection {
    pub(crate) fn confirmed(&self) -> Option<&State> {
        self.confirmed.as_ref().map(|s| &s.player)
    }

    /// For the UI only. A speculative visit has a queue key but no occurrence;
    /// never use this state as a local claim, report, or native execution plan.
    pub(crate) fn projected(&self) -> Option<&State> {
        self.projected.as_ref().or_else(|| self.confirmed())
    }

    pub(crate) fn session_id(&self) -> Option<&str> {
        self.confirmed.as_ref().map(|s| s.session_id.as_str())
    }

    pub(crate) fn revision(&self) -> u64 {
        self.confirmed.as_ref().map_or(0, |s| s.revision)
    }

    pub(crate) fn owner(&self) -> Option<&Owner> {
        self.confirmed.as_ref().map(|s| &s.owner)
    }

    pub(crate) fn devices(&self) -> &[Device] {
        self.confirmed
            .as_ref()
            .map_or(&[], |s| s.devices.as_slice())
    }

    pub(crate) fn can_submit(&self) -> bool {
        self.attached && !self.needs_sync && self.pending.len() < MAX_PENDING
    }

    pub(crate) fn basis(&self, action: &Action) -> Option<CommandBasis> {
        if !self.can_submit() {
            return None;
        }
        let state = self.projected()?;
        let cursor = &state.cursor;
        // The displayed visit may not exist yet. Sending the old confirmed
        // occurrence here would target a row the user is no longer seeing.
        if transport_action(action) && cursor.key != 0 && cursor.occurrence.is_none() {
            return None;
        }
        let confirmed = self.confirmed.as_ref()?;
        Some(CommandBasis {
            session_id: confirmed.session_id.clone(),
            generation: confirmed.owner.generation,
            revision: confirmed.revision,
            occurrence: cursor.occurrence.clone(),
            key: cursor.occurrence.as_ref().map(|_| cursor.key),
            successor_key: matches!(action, Action::Next).then(|| successor_key(state, false)),
        })
    }

    pub(crate) fn reattach_basis(&self) -> Option<ReattachBasis> {
        let confirmed = self.confirmed.as_ref()?;
        Some(ReattachBasis {
            session_id: confirmed.session_id.clone(),
            generation: confirmed.owner.generation,
            revision: confirmed.revision,
            occurrence: confirmed.player.cursor.occurrence.clone(),
        })
    }

    /// Call immediately after the transport successfully enqueues the command,
    /// in send order, on the same serial actor that applies server frames.
    pub(crate) fn register(&mut self, id: String, basis: CommandBasis, action: Action) -> bool {
        if !self.can_submit() {
            return false;
        }
        let Some(current) = self.confirmed.as_ref() else {
            return false;
        };
        if basis.session_id != current.session_id
            || basis.generation != current.owner.generation
            || basis.revision != current.revision
        {
            return false;
        }
        let Some(counter) = command_counter(&current.connection_id, &id) else {
            return false;
        };
        if counter <= self.last_counter {
            return false;
        }
        self.last_counter = counter;
        self.pending.push(Pending {
            id,
            basis,
            action,
            reflected: false,
            terminal_revision: None,
        });
        self.reproject();
        true
    }

    /// On transport loss, retain only confirmed intent for reattach. Neither
    /// pending actions nor old observations belong to the detached engine.
    pub(crate) fn clear_pending(&mut self) {
        self.pending.clear();
        self.projected = None;
        self.attached = false;
        self.needs_sync = false;
        self.suppress_observation = false;
        if let Some(confirmed) = &mut self.confirmed {
            confirmed.observation = None;
        }
    }

    pub(crate) fn apply_snapshot(&mut self, mut frame: Snapshot) -> Apply {
        if frame.session_id.is_empty()
            || frame.connection_id.is_empty()
            || frame.revision > MAX_SAFE_INTEGER
            || frame.owner.generation > MAX_SAFE_INTEGER
            || !valid_queue(
                &frame.player.queue,
                &frame.player.cursor,
                frame.player.collection_first,
            )
        {
            return Apply::Ignored;
        }
        if let Some(old) = &self.confirmed {
            if old.session_id == frame.session_id
                && (frame.revision < old.revision
                    || frame.owner.generation < old.owner.generation
                    || (frame.owner.generation == old.owner.generation
                        && frame.owner.device_id != old.owner.device_id))
            {
                return Apply::Ignored;
            }
            if old.session_id != frame.session_id || old.connection_id != frame.connection_id {
                self.pending.clear();
            }
            if old.connection_id != frame.connection_id {
                self.last_counter = 0;
            }
        }
        if !owner_present(&frame.owner, &frame.devices)
            || frame
                .observation
                .as_ref()
                .is_some_and(|observation| !valid_observation(observation, &frame.player.cursor))
        {
            frame.observation = None;
        }
        self.confirmed = Some(frame);
        self.attached = true;
        self.needs_sync = false;
        self.retire_settled();
        self.reproject();
        Apply::Applied
    }

    pub(crate) fn apply_ownership(&mut self, frame: Ownership) -> Apply {
        let Some(current) = self.confirmed.as_mut().filter(|_| self.attached) else {
            return Apply::Ignored;
        };
        // Ownership changes arrive atomically with intent in a snapshot.
        if frame.owner.generation != current.owner.generation
            || frame.owner.device_id != current.owner.device_id
        {
            return Apply::Ignored;
        }
        current.devices = frame.devices;
        if !owner_present(&current.owner, &current.devices) {
            current.observation = None;
        }
        Apply::Applied
    }

    pub(crate) fn apply_state(&mut self, frame: StateUpdate) -> Apply {
        let Some(current) = self.confirmed.as_mut().filter(|_| self.attached) else {
            return Apply::Ignored;
        };
        if frame.generation != current.owner.generation || frame.revision > MAX_SAFE_INTEGER {
            return Apply::Ignored;
        }
        if frame.revision <= current.revision {
            // A repair snapshot can have covered the delta without its echo.
            if self.reflect(frame.command.as_ref(), frame.revision) {
                self.reproject();
            }
            return Apply::Ignored;
        }
        if current.revision.checked_add(1) != Some(frame.revision)
            || apply_delta(&mut current.player, &frame.delta).is_err()
        {
            return self.gap();
        }
        current.revision = frame.revision;
        if frame.delta.cursor.is_some() {
            // Seek/Pause can change execution without changing the occurrence.
            current.observation = None;
        }
        self.reflect(frame.command.as_ref(), frame.revision);
        self.retire_settled();
        self.reproject();
        Apply::Applied
    }

    /// True only for newly received non-stale terminal results. The state
    /// watermark governs retirement independently of this UI notification.
    pub(crate) fn apply_result(&mut self, frame: CommandResult) -> bool {
        if !self.attached || !frame.status.is_terminal() || frame.revision > MAX_SAFE_INTEGER {
            return false;
        }
        let Some(pending) = self.pending.iter_mut().find(|p| p.id == frame.id) else {
            return false;
        };
        if frame.revision < pending.basis.revision || pending.terminal_revision.is_some() {
            return false;
        }
        pending.terminal_revision = Some(frame.revision);
        if self.retire_settled() {
            self.reproject();
        }
        frame.status != ResultStatus::Stale
    }

    pub(crate) fn apply_observation(&mut self, frame: ObservationUpdate) -> Apply {
        let Some(current) = self.confirmed.as_mut().filter(|_| self.attached) else {
            return Apply::Ignored;
        };
        if frame.generation != current.owner.generation
            || frame.revision < current.revision
            || frame.revision > MAX_SAFE_INTEGER
        {
            return Apply::Ignored;
        }
        if frame.revision > current.revision {
            return self.gap();
        }
        if !owner_present(&current.owner, &current.devices)
            || !valid_observation(&frame.observation, &current.player.cursor)
        {
            return Apply::Ignored;
        }
        // WebSocket orders observations. Sample timestamps can step backwards
        // on clock re-estimation; they are not sequence numbers.
        current.observation = Some(frame.observation);
        Apply::Applied
    }

    /// Transport estimates server time via its own monotonic clock samples.
    /// Without that estimate don't extrapolate or assert fresh actual Playing.
    pub(crate) fn remote_playback(&self, server_time_ms: Option<u64>) -> Option<RemotePlayback> {
        let confirmed = self.confirmed.as_ref()?;
        let player = self.projected()?;
        let mut display = RemotePlayback {
            position_ms: player.cursor.position_ms,
            status: None,
        };
        if self.needs_sync || self.suppress_observation {
            return Some(display);
        }
        if let Some(obs) = &confirmed.observation {
            let duration = player
                .queue
                .iter()
                .find(|r| r.key == obs.key)
                .and_then(duration_ms);
            display.position_ms = duration.map_or(obs.position_ms, |ms| ms.min(obs.position_ms));
            if let Some(age) = server_time_ms.and_then(|now| now.checked_sub(obs.sampled_at_ms)) {
                if age < OBSERVATION_FRESH_MS {
                    display.status = Some(obs.status);
                }
                if obs.status == ObservationStatus::Playing {
                    // Freeze at the freshness limit rather than jumping back
                    // to the original sample when Playing expires.
                    if let Some(duration) = duration {
                        display.position_ms = duration.min(
                            obs.position_ms
                                .saturating_add(age.min(OBSERVATION_FRESH_MS)),
                        );
                    }
                }
            }
        }
        Some(display)
    }

    fn gap(&mut self) -> Apply {
        if self.needs_sync {
            Apply::Ignored
        } else {
            self.needs_sync = true;
            Apply::NeedSync
        }
    }

    fn reflect(&mut self, echo: Option<&Command>, revision: u64) -> bool {
        let Some(echo) = echo else { return false };
        let Some(pending) = self.pending.iter_mut().find(|p| p.id == echo.id) else {
            return false;
        };
        if pending.reflected
            || revision <= pending.basis.revision
            || pending.basis != echo.basis
            || pending.action != echo.action
        {
            return false;
        }
        pending.reflected = true;
        true
    }

    fn retire_settled(&mut self) -> bool {
        let revision = self.revision();
        let before = self.pending.len();
        self.pending
            .retain(|p| p.terminal_revision.is_none_or(|r| r > revision));
        self.pending.len() != before
    }

    fn reproject(&mut self) {
        self.projected = None;
        self.suppress_observation = false;
        let Some(confirmed) = &self.confirmed else {
            return;
        };
        let mut pending = self
            .pending
            .iter()
            .filter(|p| !p.reflected && p.basis.generation == confirmed.owner.generation)
            .peekable();
        if pending.peek().is_none() {
            return;
        }
        let mut state = confirmed.player.clone();
        for command in pending {
            let observed = (!self.suppress_observation)
                .then_some(confirmed.observation.as_ref())
                .flatten();
            self.suppress_observation |= project(&mut state, command, observed);
        }
        self.projected = Some(state);
    }
}

fn command_counter(connection: &str, id: &str) -> Option<u64> {
    let counter = id.strip_prefix(connection)?.strip_prefix(':')?;
    if counter.starts_with('0') || !counter.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    counter
        .parse::<u64>()
        .ok()
        .filter(|n| (1..=MAX_SAFE_INTEGER).contains(n))
}

fn owner_present(owner: &Owner, roster: &[Device]) -> bool {
    owner
        .device_id
        .as_ref()
        .is_some_and(|id| roster.iter().any(|d| &d.device_id == id && d.can_play))
}

fn valid_observation(obs: &Observation, cursor: &Cursor) -> bool {
    obs.key == cursor.key
        && obs.occurrence == cursor.occurrence
        && obs.position_ms <= MAX_SAFE_INTEGER
        && obs.sampled_at_ms <= MAX_SAFE_INTEGER
        && (obs.key != 0 || obs.status == ObservationStatus::Stopped)
        && (cursor.desired_playing || obs.status != ObservationStatus::Playing)
}

fn duration_ms(row: &Track) -> Option<u64> {
    let millis = row.duration * 1000.0;
    (millis.is_finite() && millis > 0.0 && millis <= MAX_SAFE_INTEGER as f64)
        .then_some(millis as u64)
}

/// Apply a keyed server delta transactionally; port of the legacy queue edit
/// interpreter, not its playback semantics or cursor format.
fn apply_delta(state: &mut State, delta: &PlayerDelta) -> Result<(), ()> {
    let cursor = delta.cursor.as_ref().unwrap_or(&state.cursor);
    if let Some(edit) = &delta.queue {
        let queue = match edit.kind {
            QueueEditKind::Replace => edit.rows.clone(),
            QueueEditKind::Insert => {
                let at = if edit.before == 0 {
                    state.queue.len()
                } else {
                    index(state, edit.before).ok_or(())?
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
                let mut rows: HashMap<_, _> =
                    state.queue.iter().map(|row| (row.key, row)).collect();
                let rebase = if edit.rebase_after == 0 {
                    None
                } else {
                    Some(
                        edit.keys
                            .iter()
                            .position(|key| *key == edit.rebase_after)
                            .ok_or(())?,
                    )
                };
                let mut queue = edit
                    .keys
                    .iter()
                    .map(|key| rows.remove(key).cloned().ok_or(()))
                    .collect::<Result<Vec<_>, _>>()?;
                if let Some(at) = rebase {
                    rebase_order(&mut queue, at);
                }
                queue
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
        state.queue = queue;
        state.collection_first = first;
    } else if delta.cursor.is_some() && !cursor_in(&state.queue, cursor) {
        return Err(());
    }
    if let Some(cursor) = &delta.cursor {
        state.cursor = cursor.clone();
    }
    if let Some(repeat) = delta.repeat {
        state.repeat = repeat;
    }
    if let Some(shuffle) = delta.shuffle {
        state.shuffle = shuffle;
    }
    Ok(())
}

fn valid_queue(queue: &[Track], cursor: &Cursor, first: u64) -> bool {
    let mut keys = HashSet::with_capacity(queue.len());
    queue.iter().all(|row| {
        (1..=MAX_SAFE_INTEGER).contains(&row.key)
            && row.order <= MAX_SAFE_INTEGER
            && !row.id.is_empty()
            && keys.insert(row.key)
    }) && (first == 0 || keys.contains(&first))
        && cursor_in(queue, cursor)
}

fn cursor_in(queue: &[Track], cursor: &Cursor) -> bool {
    if cursor.key == 0 {
        queue.is_empty()
            && cursor.occurrence.is_none()
            && !cursor.desired_playing
            && cursor.position_ms == 0
    } else {
        cursor.position_ms <= MAX_SAFE_INTEGER
            && cursor.occurrence.as_ref().is_some_and(|id| !id.is_empty())
            && queue.iter().any(|row| row.key == cursor.key)
    }
}

fn index(state: &State, key: u64) -> Option<usize> {
    state.queue.iter().position(|row| row.key == key)
}

/// Manual traversal ignores repeat-one; natural EOF repeats the same row.
/// Zero denotes exhaustion, not a new cursor to be sent to the server.
pub(crate) fn successor_key(state: &State, natural: bool) -> u64 {
    let Some(pos) = index(state, state.cursor.key) else {
        return 0;
    };
    if natural && state.repeat == Repeat::One {
        return state.cursor.key;
    }
    state
        .queue
        .get(pos + 1)
        .or_else(|| (state.repeat == Repeat::All).then(|| &state.queue[0]))
        .map_or(0, |r| r.key)
}

fn transport_action(action: &Action) -> bool {
    matches!(
        action,
        Action::Pause | Action::Resume | Action::Seek { .. } | Action::Next | Action::Previous
    )
}

fn choose(state: &mut State, key: u64) {
    state.cursor = Cursor {
        key,
        occurrence: None,
        position_ms: 0,
        desired_playing: true,
    };
}

/// Replay one user intent, returning whether it conflicts with the last actual
/// observation. Play/Add haven't resolved catalog provenance; project neither
/// invented queue rows nor replacement IDs for those requests.
fn project(state: &mut State, command: &Pending, obs: Option<&Observation>) -> bool {
    let action = &command.action;
    let basis = &command.basis;
    let pos = index(state, state.cursor.key);
    let upcoming = pos.map_or(0, |p| p + 1);
    let measured = obs.map_or(state.cursor.position_ms, |o| o.position_ms);
    if transport_action(action)
        && (pos.is_none()
            || state.cursor.occurrence.is_none()
            || basis.occurrence != state.cursor.occurrence
            || basis.key.is_some_and(|key| key != state.cursor.key))
    {
        return false;
    }
    match action {
        Action::Play { .. } => return true,
        Action::Add { .. } | Action::Select { .. } => {}
        Action::Repeat { repeat } => state.repeat = *repeat,
        Action::Shuffle { shuffle, seed } if *shuffle != state.shuffle => {
            state.shuffle = *shuffle;
            shuffle_upcoming(&mut state.queue, upcoming, *shuffle, *seed);
        }
        Action::Shuffle { .. } => {}
        Action::Close => {
            state.queue.clear();
            state.cursor = Cursor::default();
            state.collection_first = 0;
            state.shuffle = false;
            return true;
        }
        Action::Clear if upcoming < state.queue.len() => {
            state.queue.truncate(upcoming);
            state.collection_first = 0;
        }
        Action::Clear => {}
        Action::Remove { key } => {
            if let Some(at) = index(state, *key).filter(|&at| at >= upcoming) {
                state.queue.remove(at);
                state.collection_first = 0;
            }
        }
        Action::Move { key, after } => {
            let Some(at) = index(state, *key).filter(|&at| at >= upcoming) else {
                return false;
            };
            let insert_at = if *after == 0 {
                upcoming
            } else {
                let Some(anchor) = index(state, *after).filter(|&p| p >= upcoming && p != at)
                else {
                    return false;
                };
                anchor + 1
            };
            let row = state.queue.remove(at);
            state
                .queue
                .insert(insert_at - usize::from(at < insert_at), row);
            if let Some(pos) = pos {
                rebase_order(&mut state.queue, pos);
            }
            state.collection_first = 0;
            state.shuffle = false;
        }
        Action::Jump { key } => {
            if index(state, *key).is_some() {
                choose(state, *key);
                return true;
            }
        }
        Action::Pause => {
            state.cursor.desired_playing = false;
            state.cursor.position_ms = measured;
            return true;
        }
        Action::Resume => {
            if state.cursor.desired_playing
                && obs.is_some_and(|o| o.status == ObservationStatus::Playing)
            {
                return false;
            }
            state.cursor.desired_playing = true;
            state.cursor.position_ms = measured;
            return true;
        }
        Action::Seek { position_ms } => {
            if *position_ms <= MAX_SAFE_INTEGER {
                state.cursor.position_ms = *position_ms;
                return true;
            }
        }
        Action::Next => {
            let key = successor_key(state, false);
            if basis.successor_key != Some(key) {
                return false;
            }
            if key == 0 {
                state.cursor.desired_playing = false;
                state.cursor.position_ms = measured;
            } else {
                choose(state, key);
            }
            return true;
        }
        Action::Previous => {
            if let Some(pos) = pos {
                let target = if measured > 3000 {
                    pos
                } else if pos > 0 {
                    pos - 1
                } else if state.repeat == Repeat::All {
                    state.queue.len() - 1
                } else {
                    pos
                };
                choose(state, state.queue[target].key);
                return true;
            }
        }
    }
    false
}

fn rebase_order(queue: &mut [Track], at: usize) {
    for (i, row) in queue.iter_mut().enumerate() {
        row.order = i as u64 + 1;
        if i > at {
            row.intent = Intent::High;
        }
    }
}

fn shuffle_upcoming(queue: &mut [Track], upcoming: usize, enabled: bool, seed: Option<u32>) {
    let tail = &mut queue[upcoming..];
    let (mut high, mut low): (Vec<_>, Vec<_>) = tail
        .iter()
        .cloned()
        .partition(|row| row.intent != Intent::Low);
    if enabled {
        let mut n = seed.unwrap_or_default();
        for i in (1..low.len()).rev() {
            n = n.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let j = ((u64::from(n) * (i as u64 + 1)) >> 32) as usize;
            low.swap(i, j);
        }
    } else {
        low.sort_by_key(|row| row.order);
    }
    high.extend(low);
    for (dst, row) in tail.iter_mut().zip(high) {
        *dst = row;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::de::DeserializeOwned;
    use serde_json::{Value, json};

    fn decode<T: DeserializeOwned>(value: Value) -> T {
        serde_json::from_value(value).unwrap()
    }

    fn snapshot(session: &str, connection: &str, generation: u64, revision: u64) -> Value {
        json!({
            "type": "snapshot", "sessionId": session, "connectionId": connection,
            "owner": {"generation": generation, "deviceId": "owner"},
            "devices": [{"deviceId": "owner", "name": "Desktop", "canPlay": true}],
            "revision": revision, "serverTimeMs": 10_000,
            "player": {"queue": [
                {"id": "song", "key": 1, "order": 1, "intent": "low", "duration": 30.0,
                 "entryId": "entry-1", "targetId": "playlist", "occurrenceId": "catalog-1"},
                {"id": "song", "key": 2, "order": 2, "intent": "low", "duration": 30.0,
                 "entryId": "entry-2", "targetId": "playlist", "occurrenceId": "catalog-2"},
                {"id": "other", "key": 3, "order": 3, "intent": "low", "duration": 30.0}
            ], "cursor": cursor(1, "visit-a", 500, true),
            "repeat": "off", "shuffle": false, "collectionFirst": 1}
        })
    }

    fn cursor(key: u64, occurrence: &str, position: u64, desired: bool) -> Value {
        json!({"key": key, "occurrence": occurrence, "positionMs": position, "desiredPlaying": desired})
    }

    fn room() -> Projection {
        let mut room = Projection::default();
        assert_eq!(
            room.apply_snapshot(decode(snapshot("room", "conn", 1, 10))),
            Apply::Applied
        );
        room
    }

    fn update(room: &mut Projection, revision: u64, delta: Value, command: Option<Value>) -> Apply {
        let mut frame =
            json!({"type": "state", "generation": 1, "revision": revision, "delta": delta});
        if let Some(command) = command {
            frame["command"] = command;
        }
        room.apply_state(decode(frame))
    }

    fn result(room: &mut Projection, counter: u64, status: &str, revision: u64) -> bool {
        room.apply_result(decode(json!({"type": "result", "id": format!("conn:{counter}"), "status": status, "revision": revision})))
    }

    fn register(room: &mut Projection, counter: u64, action: Action) -> Value {
        let basis = room.basis(&action).unwrap();
        let id = format!("conn:{counter}");
        let envelope = json!({"type": "command", "id": &id, "issuedAtMs": 10_000, "basis": &basis, "action": &action});
        assert!(room.register(id, basis, action));
        envelope
    }

    fn observe(
        room: &mut Projection,
        revision: u64,
        occurrence: &str,
        position: u64,
        sampled: u64,
    ) -> Apply {
        room.apply_observation(decode(json!({
            "type": "observation", "generation": 1, "revision": revision,
            "observation": {"key": 1, "occurrence": occurrence, "positionMs": position,
                            "sampledAtMs": sampled, "status": "playing"}
        })))
    }

    fn shown(room: &Projection) -> &Cursor {
        &room.projected().unwrap().cursor
    }

    #[test]
    fn late_seek_does_not_apply_to_a_repeat_of_the_same_queue_row() {
        let mut room = room();
        assert_eq!(
            update(&mut room, 11, json!({"repeat": "one"}), None),
            Apply::Applied
        );
        let command = register(&mut room, 1, Action::Seek { position_ms: 9500 });
        assert_eq!(command["basis"]["occurrence"], "visit-a");
        assert_eq!(shown(&room).position_ms, 9500);
        assert_eq!(successor_key(room.confirmed().unwrap(), true), 1);
        assert_eq!(
            update(
                &mut room,
                12,
                json!({"cursor": cursor(1, "repeat-a", 0, true)}),
                None
            ),
            Apply::Applied
        );
        assert_eq!(shown(&room).occurrence.as_deref(), Some("repeat-a"));
        assert_eq!(shown(&room).position_ms, 0);
        assert!(!result(&mut room, 1, "stale", 12));
        assert!(room.pending.is_empty());
        assert_eq!(room.confirmed(), room.projected());
    }

    #[test]
    fn pending_next_is_never_retargeted_after_a_traversal_change() {
        let mut room = room();
        let command = register(&mut room, 1, Action::Next);
        assert_eq!(command["basis"]["key"], 1);
        assert_eq!(command["basis"]["successorKey"], 2);
        assert_eq!(shown(&room).key, 2);
        assert_eq!(shown(&room).occurrence, None);
        assert!(room.basis(&Action::Seek { position_ms: 50 }).is_none());
        assert_eq!(
            update(
                &mut room,
                11,
                json!({"queue":{"kind":"order", "keys":[1,3,2]}}),
                None
            ),
            Apply::Applied
        );
        assert_eq!(shown(&room).key, 1);
        assert_eq!(successor_key(room.projected().unwrap(), false), 3);
        assert!(!result(&mut room, 1, "stale", 11));
        assert_eq!(room.confirmed(), room.projected());
    }

    #[test]
    fn next_basis_uses_visible_keyed_edits_without_inventing_a_visit() {
        let mut room = room();
        let remove = register(&mut room, 1, Action::Remove { key: 2 });
        let next = register(&mut room, 2, Action::Next);
        assert_eq!(next["basis"]["successorKey"], 3);
        assert_eq!(next["basis"]["occurrence"], "visit-a");
        assert_eq!(shown(&room).key, 3);
        assert_eq!(room.confirmed().unwrap().queue[1].key, 2);
        assert_eq!(
            update(
                &mut room,
                11,
                json!({"queue":{"kind":"remove","keys":[2]}}),
                Some(remove)
            ),
            Apply::Applied
        );
        assert_eq!(room.confirmed().unwrap().queue[1].key, 3);
        assert_eq!(shown(&room).key, 3);
        // Receipt precedes its watermark: neither retire the Next early nor
        // announce its eventual stale result as an error.
        assert!(!result(&mut room, 2, "stale", 12));
        assert_eq!(shown(&room).key, 3);
        assert_eq!(room.pending.len(), 2);
        assert_eq!(
            update(
                &mut room,
                12,
                json!({"queue":{"kind":"insert","before":3,"rows":[
                    {"id":"new","key":4,"order":4,"intent":"next"}
                ]}}),
                None
            ),
            Apply::Applied
        );
        assert_eq!(shown(&room).key, 1);
        assert_eq!(successor_key(room.projected().unwrap(), false), 4);
        assert_eq!(room.pending.len(), 1);
    }

    #[test]
    fn pending_next_does_not_advance_again_after_natural_eof_to_requested_row() {
        let mut room = room();
        register(&mut room, 1, Action::Next);
        assert_eq!(shown(&room).key, 2);
        assert_eq!(
            update(
                &mut room,
                11,
                json!({"cursor": cursor(2, "native-b", 10, true)}),
                None
            ),
            Apply::Applied
        );
        assert_eq!(shown(&room).key, 2);
        assert_eq!(shown(&room).occurrence.as_deref(), Some("native-b"));
        assert_eq!(shown(&room).position_ms, 10);
        assert!(!result(&mut room, 1, "stale", 11));
        assert_eq!(shown(&room).key, 2);
    }

    #[test]
    fn pending_next_does_not_survive_repeat_one_same_row_new_visit() {
        let mut room = room();
        assert_eq!(
            update(&mut room, 11, json!({"repeat":"one"}), None),
            Apply::Applied
        );
        register(&mut room, 1, Action::Next);
        // Repeat-One affects natural traversal, not manual Next.
        assert_eq!(shown(&room).key, 2);
        assert_eq!(
            update(
                &mut room,
                12,
                json!({"cursor": cursor(1, "repeat-a", 0, true)}),
                None
            ),
            Apply::Applied
        );
        assert_eq!(shown(&room).occurrence.as_deref(), Some("repeat-a"));
        assert_eq!(shown(&room).key, 1);
        assert!(!result(&mut room, 1, "stale", 12));
        assert_eq!(room.confirmed(), room.projected());
    }

    #[test]
    fn repeat_change_can_invalidate_a_wrapping_next_without_a_cursor_change() {
        let mut room = room();
        assert_eq!(
            update(
                &mut room,
                11,
                json!({"repeat":"all","cursor":cursor(3, "outgoing", 4200, true)}),
                None
            ),
            Apply::Applied
        );
        let command = register(&mut room, 1, Action::Next);
        assert_eq!(command["basis"]["successorKey"], 1);
        assert_eq!(shown(&room).key, 1);
        assert_eq!(
            update(&mut room, 12, json!({"repeat":"off"}), None),
            Apply::Applied
        );
        assert_eq!(shown(&room).key, 3);
        assert_eq!(shown(&room).position_ms, 4200);
        assert!(shown(&room).desired_playing);
        assert!(!result(&mut room, 1, "stale", 12));
        let command = register(&mut room, 2, Action::Next);
        assert_eq!(command["basis"]["successorKey"], 0);
        assert!(!shown(&room).desired_playing);
        assert_eq!(shown(&room).position_ms, 4200);
    }

    #[test]
    fn rapid_repeat_echoes_apply_once_while_receipts_wait_for_their_watermarks() {
        let mut room = room();
        let all = register(
            &mut room,
            1,
            Action::Repeat {
                repeat: Repeat::All,
            },
        );
        let one = register(
            &mut room,
            2,
            Action::Repeat {
                repeat: Repeat::One,
            },
        );
        let off = register(
            &mut room,
            3,
            Action::Repeat {
                repeat: Repeat::Off,
            },
        );
        assert_eq!(room.projected().unwrap().repeat, Repeat::Off);
        assert!(!result(&mut room, 3, "accepted", 10));
        assert_eq!(room.pending.len(), 3);
        assert_eq!(
            update(&mut room, 11, json!({"repeat":"all"}), Some(all)),
            Apply::Applied
        );
        assert!(result(&mut room, 3, "applied", 13));
        assert_eq!(room.pending.len(), 3);
        assert_eq!(room.projected().unwrap().repeat, Repeat::Off);
        assert_eq!(
            update(&mut room, 12, json!({"repeat":"one"}), Some(one)),
            Apply::Applied
        );
        assert_eq!(room.confirmed().unwrap().repeat, Repeat::One);
        assert_eq!(room.projected().unwrap().repeat, Repeat::Off);
        assert_eq!(
            update(&mut room, 13, json!({"repeat":"off"}), Some(off)),
            Apply::Applied
        );
        assert_eq!(room.pending.len(), 2);
        // New shared settings must not be overwritten by the delayed receipts.
        assert_eq!(
            update(&mut room, 14, json!({"repeat":"all"}), None),
            Apply::Applied
        );
        assert!(result(&mut room, 2, "applied", 12));
        assert!(result(&mut room, 1, "applied", 11));
        assert!(!result(&mut room, 2, "applied", 12));
        assert_eq!(room.projected().unwrap().repeat, Repeat::All);
        assert!(room.pending.is_empty());
    }

    #[test]
    fn accepted_conditional_next_has_no_shared_effect_until_echoed() {
        let mut room = room();
        let command = register(&mut room, 1, Action::Next);
        assert!(!result(&mut room, 1, "accepted", 10));
        assert_eq!(room.confirmed().unwrap().cursor.key, 1);
        assert_eq!(shown(&room).key, 2);
        assert!(result(&mut room, 1, "applied", 11));
        assert_eq!(room.pending.len(), 1);
        assert_eq!(
            update(
                &mut room,
                11,
                json!({"cursor": cursor(2, "native-b", 0, true)}),
                Some(command)
            ),
            Apply::Applied
        );
        assert_eq!(shown(&room).occurrence.as_deref(), Some("native-b"));
        assert!(room.pending.is_empty());
    }

    #[test]
    fn gap_does_not_mutate_queue_or_revision_and_snapshot_reprojects() {
        let mut room = room();
        register(
            &mut room,
            1,
            Action::Repeat {
                repeat: Repeat::One,
            },
        );
        assert_eq!(
            update(
                &mut room,
                11,
                json!({"queue":{"kind":"order","keys":[1,2,99]},"shuffle":true}),
                None
            ),
            Apply::NeedSync
        );
        assert_eq!(room.revision(), 10);
        assert_eq!(room.confirmed().unwrap().queue[2].key, 3);
        assert!(!room.confirmed().unwrap().shuffle);
        assert!(!room.can_submit());
        assert_eq!(
            update(&mut room, 12, json!({"shuffle":true}), None),
            Apply::Ignored
        );
        let mut repair = snapshot("room", "conn", 1, 12);
        repair["player"]["shuffle"] = json!(true);
        assert_eq!(room.apply_snapshot(decode(repair)), Apply::Applied);
        assert!(room.projected().unwrap().shuffle);
        assert_eq!(room.projected().unwrap().repeat, Repeat::One);
        assert!(room.can_submit());
        assert_eq!(
            update(&mut room, 11, json!({"shuffle":false}), None),
            Apply::Ignored
        );
        assert_eq!(room.revision(), 12);
    }

    #[test]
    fn scopes_and_checked_revisions_survive_detach_replacement_and_room_reset() {
        let mut room = room();
        register(&mut room, 5, Action::Next);
        let reattach = room.reattach_basis().unwrap();
        assert_eq!(reattach.occurrence.as_deref(), Some("visit-a"));
        assert_eq!(reattach.revision, 10);
        room.clear_pending();
        assert!(!room.can_submit());
        assert_eq!(room.projected(), room.confirmed());
        assert_eq!(
            update(&mut room, 11, json!({"shuffle":true}), None),
            Apply::Ignored
        );
        assert_eq!(
            room.apply_snapshot(decode(snapshot("room", "replacement", 1, MAX_SAFE_INTEGER))),
            Apply::Applied
        );
        let action = Action::Pause;
        assert!(room.register("replacement:1".into(), room.basis(&action).unwrap(), action));
        assert_eq!(
            update(
                &mut room,
                MAX_SAFE_INTEGER + 1,
                json!({"shuffle":true}),
                None
            ),
            Apply::Ignored
        );
        assert_eq!(room.revision(), MAX_SAFE_INTEGER);
        assert!(!room.confirmed().unwrap().shuffle);
        assert_eq!(
            room.apply_snapshot(decode(snapshot("new-room", "new-conn", 0, 0))),
            Apply::Applied
        );
        assert_eq!(room.revision(), 0);
        assert_eq!(room.session_id(), Some("new-room"));
        assert!(room.pending.is_empty());
    }

    #[test]
    fn catalog_commands_never_invent_rows_or_replace_real_occurrence() {
        let mut room = room();
        let selection = json!({"kind":"track", "track":{"id":"choice"}});
        register(
            &mut room,
            1,
            decode(json!({"type":"add","placement":"next","selection":selection})),
        );
        register(
            &mut room,
            2,
            decode(json!({"type":"play","selection":selection})),
        );
        assert_eq!(room.projected(), room.confirmed());
        assert_eq!(shown(&room).occurrence.as_deref(), Some("visit-a"));
        assert_eq!(
            room.projected().unwrap().queue[0].occurrence_id.as_deref(),
            Some("catalog-1")
        );
    }

    #[test]
    fn remote_clock_is_separate_from_intent_and_does_not_outlive_ownership() {
        let mut room = room();
        assert_eq!(
            observe(&mut room, 10, "visit-a", 19_000, 10_000),
            Apply::Applied
        );
        assert_eq!(
            room.remote_playback(Some(11_200)),
            Some(RemotePlayback {
                position_ms: 20_200,
                status: Some(ObservationStatus::Playing)
            })
        );
        assert_eq!(room.confirmed().unwrap().cursor.position_ms, 500);
        assert_eq!(room.remote_playback(Some(9_990)).unwrap().status, None);
        assert_eq!(
            room.remote_playback(Some(21_000)),
            Some(RemotePlayback {
                position_ms: 29_000,
                status: None
            })
        );
        assert_eq!(
            observe(&mut room, 10, "visit-a", 29_900, 21_000),
            Apply::Applied
        );
        assert_eq!(
            room.remote_playback(Some(22_000)).unwrap().position_ms,
            30_000
        );
        assert_eq!(room.apply_ownership(decode(json!({"type":"ownership", "owner":{"generation":1,"deviceId":"owner"}, "devices":[]}))), Apply::Applied);
        assert_eq!(room.remote_playback(Some(22_000)).unwrap().status, None);
        assert_eq!(
            observe(&mut room, 10, "visit-a", 30_000, 22_000),
            Apply::Ignored
        );
    }

    #[test]
    fn remote_playing_is_invalidated_immediately_by_seek_and_its_echo() {
        let mut room = room();
        observe(&mut room, 10, "visit-a", 4600, 10_000);
        let command = register(&mut room, 1, Action::Seek { position_ms: 7600 });
        assert_eq!(
            room.remote_playback(Some(10_200)),
            Some(RemotePlayback {
                position_ms: 7600,
                status: None
            })
        );
        assert_eq!(
            update(
                &mut room,
                11,
                json!({"cursor":cursor(1,"visit-a",7600,true)}),
                Some(command)
            ),
            Apply::Applied
        );
        assert_eq!(
            observe(&mut room, 10, "visit-a", 4800, 10_200),
            Apply::Ignored
        );
        assert_eq!(
            observe(&mut room, 11, "other-visit", 4800, 10_200),
            Apply::Ignored
        );
        assert_eq!(room.remote_playback(Some(10_300)).unwrap().status, None);
        assert_eq!(
            observe(&mut room, 11, "visit-a", 7620, 10_320),
            Apply::Applied
        );
        assert_eq!(
            room.remote_playback(Some(10_400)),
            Some(RemotePlayback {
                position_ms: 7700,
                status: Some(ObservationStatus::Playing)
            })
        );
    }
}
