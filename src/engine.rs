//! Account-scoped actor: canonical v2 queue online, independent verified queue offline.
use crate::{
    AppWindow, DeviceRow, QueueRow,
    api::{Api, ApiError, Item, Session},
    audio::Audio,
    desk::Access,
    mode::ModeControl,
    offline::{self, OfflineStore, PlaybackInput},
    player::{Action, Authority, Change, LocalQueue, LocalTrack, Repeat, Status},
    socket::{
        self, Binding, EventKind, Generation, Instruction, InstructionKind, Link, MediaError,
    },
};
use slint::{Model, ModelRc, VecModel};
use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    sync::{Semaphore, mpsc, watch},
    task::JoinHandle,
};

pub enum Command {
    Login {
        epoch: u64,
        origin: String,
        username: String,
        password: String,
        lan: bool,
    },
    Logout(u64),
    Scoped(u64, Box<Command>),
    Toggle,
    Transport(&'static str),
    Playback(Action),
    Guarded(String, Action),
    #[cfg(test)]
    Offline(bool),
    PlayOffline {
        queue: Vec<Item>,
        index: usize,
        input: PlaybackInput,
        store: OfflineStore,
    },
    Shutdown,
}
impl Command {
    pub fn scoped(self, epoch: u64) -> Self {
        Self::Scoped(epoch, Box::new(self))
    }
}

const REPORT_INTERVAL: Duration = Duration::from_secs(2);
const LOAD_BUDGET: Duration = Duration::from_secs(5);
const TICK: Duration = Duration::from_millis(25);
const SETTLEMENT_BATCH: usize = 64;
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const PROBE_FIRST_DELAY: Duration = Duration::from_millis(500);
const PROBE_MAX_DELAY: Duration = Duration::from_secs(30);

struct Account {
    api: Arc<Api>,
    session: Session,
}
struct Work {
    generation: Generation,
    instruction: Instruction,
    grant: Option<Authority>,
}
struct Execution {
    generation: Generation,
    binding: Binding,
    grant: Option<Authority>,
    status: Status,
    sent_at: Option<Instant>,
    ended: bool,
}
struct PauseFence {
    generation: Option<Generation>,
    binding: Option<Binding>,
    buffered: usize,
}
struct Recent {
    event: String,
    amount: f64,
    emitted: bool,
    observed: Option<(Instant, f64)>,
}
struct Reconnect {
    serial: u64,
    task: JoinHandle<()>,
}
enum Reply {
    Login(u64, Arc<Api>, Result<Session, ApiError>),
    Reconnect {
        epoch: u64,
        serial: u64,
        result: Result<(), ApiError>,
    },
    Media {
        epoch: u64,
        operation: u64,
        result: Result<Vec<u8>, ApiError>,
    },
    OfflineMedia {
        epoch: u64,
        operation: u64,
        input: Option<PlaybackInput>,
    },
}
struct Engine {
    epoch: u64,
    shared_epoch: Arc<AtomicU64>,
    mode: ModeControl,
    access: watch::Sender<Option<Access>>,
    results: mpsc::Sender<Reply>,
    window: slint::Weak<AppWindow>,
    device_id: String,
    login: Option<JoinHandle<()>>,
    reconnect: Option<Reconnect>,
    logouts: Vec<JoinHandle<()>>,
    account: Option<Account>,
    offline: bool,
    audio: Option<Audio>,
    local: LocalQueue,
    local_store: Option<OfflineStore>,
    local_read_slot: Arc<Semaphore>,
    local_status: Status,
    socket: Option<socket::Socket>,
    generation: Option<Generation>,
    snapshot: Option<Arc<socket::Snapshot>>,
    anchor: Option<(socket::Playback, Instant)>,
    connection_id: String,
    execution: Option<Execution>,
    pending: Option<Work>,
    media: Option<JoinHandle<()>>,
    operation: u64,
    request_id: u64,
    room_incarnation: u64,
    local_revision: u64,
    // A physical stop retires its exact binding and the authority prefix
    // already buffered locally. It is not a veto on later coordinator intent.
    pause_fence: Option<PauseFence>,
    recent: Option<Recent>,
    recent_change: u64,
    action_status: &'static str,
}
impl Engine {
    fn current(&self) -> bool {
        self.epoch == self.shared_epoch.load(Ordering::SeqCst)
    }
    fn room_token(&self) -> Option<String> {
        if !self.current() {
            return None;
        }
        if self.offline {
            return self
                .account
                .as_ref()
                .filter(|a| a.session.is_offline())
                .map(|_| format!("local:{}:{}", self.room_incarnation, self.local_revision));
        }
        let generation = self.generation?;
        if !self.network_allowed() || self.link() != Link::Connected(generation) {
            return None;
        }
        self.snapshot
            .as_ref()
            .map(|s| format!("{}:{}", self.room_incarnation, s.state_version))
    }
    fn network_allowed(&self) -> bool {
        self.current()
            && !self.offline
            && self
                .account
                .as_ref()
                .is_some_and(|a| !a.session.is_offline() && !a.session.is_expired())
    }
    fn publish_access(&self) {
        self.access
            .send_replace(self.account.as_ref().map(|a| Access {
                epoch: self.epoch,
                api: a.api.clone(),
                session: a.session.clone(),
                offline: self.offline,
                recent_change: self.recent_change,
            }));
    }
    fn login_view(&self, busy: bool, error: bool, status: &'static str) {
        let (weak, shared, epoch) = (self.window.clone(), self.shared_epoch.clone(), self.epoch);
        let signed = self.account.is_some();
        let _ = slint::invoke_from_event_loop(move || {
            if shared.load(Ordering::SeqCst) == epoch
                && let Some(ui) = weak.upgrade()
            {
                // Complete the login indicator before handing all browse
                // status fields to the signed-in desk.
                if signed && !ui.get_signed_in() {
                    ui.set_busy(false);
                }
                ui.set_signed_in(signed);
                if !signed {
                    ui.set_busy(busy);
                    ui.set_error(error);
                    ui.set_status(status.into());
                }
            }
        });
    }
    fn reset(&mut self, epoch: u64) {
        self.cancel_reconnect();
        if let Some(task) = self.login.take() {
            task.abort();
            self.logouts.push(task);
        }
        self.clear_output();
        if let Some(a) = self.account.take()
            && (a.session.is_offline() || !a.session.is_expired())
        {
            self.logouts.push(tokio::spawn(async move {
                // Api revokes the shared capability even offline, and its mode
                // fence returns before creating a request in that case.
                let _ = a.api.logout(&a.session).await;
            }));
        }
        self.logouts.retain(|task| !task.is_finished());
        self.epoch = epoch;
        self.offline = false;
        self.recent_change = 0;
        self.publish_access();
    }
    fn clear_output(&mut self) {
        self.room_incarnation = self.room_incarnation.wrapping_add(1);
        self.local_revision = 0;
        self.cancel_work();
        self.stop();
        self.audio = None;
        self.socket = None;
        self.generation = None;
        self.snapshot = None;
        self.anchor = None;
        self.connection_id.clear();
        self.pause_fence = None;
        self.recent = None;
        self.local = LocalQueue::default();
        self.local_store = None;
        self.local_status = Status::Ok;
        self.action_status = "";
    }
    fn connect(&mut self) {
        if self.network_allowed()
            && let Some(a) = &self.account
            && self.socket.is_none()
        {
            self.audio = Some(Audio::new(
                Some(a.session.clone()),
                false,
                self.shared_epoch.clone(),
                self.epoch,
            ));
            self.socket = socket::spawn(
                a.api.clone(),
                a.session.clone(),
                socket::DeviceIdentity {
                    device_id: self.device_id.clone(),
                    name: "Spofify Desktop".into(),
                    can_play: true,
                },
            )
            .ok();
        }
    }
    fn apply_mode(&mut self) {
        let mode = self.mode.clone();
        let mut retired = None;
        mode.apply(self.epoch, |desired, serial| {
            self.cancel_reconnect();
            // Even a superseded offline choice fences the Session immediately.
            // Retire old online work, but never discard an existing local queue
            // merely to validate a possible return online.
            let fenced = self
                .account
                .as_ref()
                .is_some_and(|a| a.session.is_offline());
            if (desired || fenced) && self.needs_mode_change(true) {
                self.stop();
                retired = self.audio.take();
                self.set_offline(true);
            }
            if !desired && self.offline {
                self.start_reconnect(serial);
            }
        });
        // Closing a native stream can join its backend thread. Never do that
        // under the shared mode lock, which the UI uses for immediate fencing.
        drop(retired);
        // The lease and API consult this same immediate transport fence.
        // An offline request that lands just after apply is also safe here.
        if !self.offline
            && self
                .account
                .as_ref()
                .is_some_and(|a| a.session.is_offline())
        {
            self.set_offline(true);
        }
    }
    fn cancel_reconnect(&mut self) {
        if let Some(reconnect) = self.reconnect.take() {
            reconnect.task.abort();
        }
    }
    fn start_reconnect(&mut self, serial: u64) {
        let Some(account) = &self.account else {
            return;
        };
        let (api, session) = (account.api.clone(), account.session.clone());
        let (epoch, current, mode, replies) = (
            self.epoch,
            self.shared_epoch.clone(),
            self.mode.clone(),
            self.results.clone(),
        );
        self.action_status = "";
        let task = tokio::spawn(async move {
            let result = reconnect_attempts(
                &mode,
                serial,
                || current.load(Ordering::SeqCst) == epoch,
                || api.validate_reconnect(&session),
            )
            .await;
            if let Some(result) = result {
                let _ = replies
                    .send(Reply::Reconnect {
                        epoch,
                        serial,
                        result,
                    })
                    .await;
            }
        });
        self.reconnect = Some(Reconnect { serial, task });
    }
    fn finish_reconnect(&mut self, epoch: u64, serial: u64, result: Result<(), ApiError>) {
        if epoch != self.epoch
            || !self.current()
            || !self.offline
            || self
                .reconnect
                .as_ref()
                .is_none_or(|probe| probe.serial != serial)
        {
            return;
        }
        self.cancel_reconnect();
        match result {
            Ok(()) => {
                let mode = self.mode.clone();
                let mut retired = None;
                mode.if_current_online(epoch, serial, || {
                    if !self.current() {
                        return;
                    }
                    self.stop();
                    retired = self.audio.take();
                    self.set_offline(false);
                });
                drop(retired);
            }
            Err(e) if e.expired || e.status == 401 => self.expire(),
            Err(_) => self.action_status = "Working offline",
        }
    }
    fn needs_mode_change(&self, offline: bool) -> bool {
        self.offline != offline
            || self
                .account
                .as_ref()
                .is_some_and(|a| a.session.is_offline() != offline)
    }
    fn set_offline(&mut self, offline: bool) {
        if !self.needs_mode_change(offline) {
            return;
        }
        // Going offline fences all credential clones before any old task can
        // resume. Going online retires the local decoder before lifting it.
        if offline && let Some(a) = &self.account {
            a.session.set_offline(true);
        }
        if let Some(task) = self.login.take() {
            task.abort();
            self.logouts.push(task);
        }
        self.clear_output();
        // Previous account teardown requests must not keep running in offline mode.
        if offline {
            for task in &self.logouts {
                task.abort();
            }
        }
        self.offline = offline;
        if self.offline {
            self.audio = self.account.as_ref().map(|a| {
                Audio::new(
                    Some(a.session.clone()),
                    true,
                    self.shared_epoch.clone(),
                    self.epoch,
                )
            });
            self.login_view(false, false, "");
        } else if let Some(a) = &self.account {
            if a.session.is_expired() {
                self.expire();
                return;
            }
            a.session.set_offline(false);
            self.connect();
        }
        self.publish_access();
    }
    fn expire(&mut self) {
        let offline = self.offline;
        self.reset(self.epoch);
        self.offline = offline;
        self.login_view(false, true, "Session expired. Sign in again.");
    }
    fn command(&mut self, command: Command) -> bool {
        match command {
            Command::Shutdown => {
                self.reset(self.shared_epoch.load(Ordering::SeqCst));
                return false;
            }
            Command::Logout(epoch) if epoch == self.shared_epoch.load(Ordering::SeqCst) => {
                let offline = self.offline;
                self.reset(epoch);
                // Sign-out must not silently switch network policy.
                self.offline = offline;
                self.login_view(false, false, "Signed out on this desktop.");
            }
            Command::Login {
                epoch,
                origin,
                username,
                password,
                lan,
            } if epoch == self.shared_epoch.load(Ordering::SeqCst) => {
                // Explicit sign-in is explicit Go Online. Retire the local
                // queue and grant; never try to reuse its network credential.
                self.reset(epoch);
                match connection(&origin, lan) {
                    Ok(api) => {
                        let api = Arc::new(api);
                        let results = self.results.clone();
                        self.login_view(true, false, "");
                        self.login = Some(tokio::spawn(async move {
                            let result = api.login(&username, &password).await;
                            drop(password);
                            let _ = results.send(Reply::Login(epoch, api, result)).await;
                        }));
                    }
                    Err(e) => self.login_view(false, true, e.message),
                }
            }
            Command::Scoped(epoch, command) if epoch == self.epoch && self.current() => {
                self.apply_mode();
                self.scoped(*command)
            }
            _ => {}
        }
        true
    }
    fn scoped(&mut self, command: Command) {
        match command {
            #[cfg(test)]
            Command::Offline(true) => self.set_offline(true),
            Command::PlayOffline {
                queue,
                index,
                input,
                store,
            } if self.offline && self.audio.is_some() => {
                if !queue
                    .get(index)
                    .is_some_and(|item| item.id == input.item.id)
                    || input.bytes.is_empty()
                    || input.bytes.len() > offline::MAX_TRACK_BYTES
                    || queue
                        .iter()
                        .any(|item| item.kind != "track" || !item.has_audio)
                {
                    return;
                }
                let tracks = queue
                    .into_iter()
                    .map(|item| LocalTrack {
                        id: item.id,
                        title: item.name,
                        subtitle: item.artists,
                        album_id: String::new(),
                        artist_id: String::new(),
                        duration: item.duration_ms as f64 / 1000.0,
                    })
                    .collect();
                if self.local.replace(tracks, index, false) == Change::Start {
                    self.cancel_work();
                    self.stop();
                    self.local_store = Some(store);
                    self.local_revision = self.local_revision.wrapping_add(1);
                    self.start_saved(Some(input));
                }
            }
            Command::Guarded(token, action) => {
                if !token.is_empty() && self.room_token().as_deref() == Some(token.as_str()) {
                    self.apply_action(action, true);
                }
            }
            Command::Playback(action) if !action.depends_on_visible_queue() => self.action(action),
            Command::Toggle => {
                let desired = if self.offline {
                    self.local.desired
                } else {
                    self.snapshot
                        .as_ref()
                        .is_some_and(|s| s.state.desired_playing)
                };
                self.action(if desired {
                    Action::Pause
                } else {
                    Action::Resume
                });
            }
            Command::Transport("next") => self.action(Action::Next),
            Command::Transport("previous") => self.action(Action::Previous),
            Command::Transport("pause") => self.action(Action::Pause),
            Command::Transport("resume") => self.action(Action::Resume),
            Command::Transport("close") => self.action(Action::Close),
            _ => {}
        }
    }
    fn action(&mut self, action: Action) {
        self.apply_action(action, false);
    }
    fn apply_action(&mut self, action: Action, displayed_version: bool) {
        if self.offline {
            if self.local.desired
                && self.local.current().is_some()
                && let Some(audio) = &self.audio
            {
                self.local.elapsed = audio.facts().elapsed;
            }
            let change = self.local.action(action);
            self.local_change(change);
            return;
        }
        if !self.network_allowed() {
            return;
        }
        let Some((command, guarded)) = action.wire() else {
            return;
        };
        let local_stop = matches!(command, socket::Command::Pause | socket::Command::Close)
            && (self.execution.is_some()
                || self.pending.as_ref().is_some_and(|w| w.grant.is_some())
                || self.snapshot.as_ref().is_some_and(|s| {
                    !self.connection_id.is_empty() && s.selected_connection_id == self.connection_id
                }));
        if local_stop {
            self.pause_local();
        }
        let Some(snapshot) = &self.snapshot else {
            self.action_status = "No output is available yet.";
            return;
        };
        let Some(generation) = self
            .generation
            .filter(|g| self.link() == Link::Connected(*g))
        else {
            self.action_status = "No output is available yet.";
            return;
        };
        self.request_id = self.request_id.wrapping_add(1);
        let request = self.request_id;
        let submitted = self.socket.as_ref().is_some_and(|s| {
            s.command(
                generation,
                request,
                command,
                (guarded || displayed_version).then_some(snapshot.state_version),
            )
            .is_ok()
        });
        if submitted {
            self.action_status = "";
        } else {
            self.action_status = "The playback action was not sent.";
        }
    }
    fn pause_local(&mut self) {
        let binding = self
            .pending
            .as_ref()
            .filter(|work| work.grant.is_some())
            .map(|work| work.instruction.binding())
            .or_else(|| self.execution.as_ref().map(|e| e.binding.clone()));
        let buffered = self.socket.as_ref().map_or(0, |s| s.events.len());
        if let Some(audio) = &mut self.audio {
            audio.stop();
        }
        self.sample(true, false);
        self.cancel_work();
        self.stop();
        self.pause_fence = Some(PauseFence {
            generation: self.generation,
            binding,
            buffered,
        });
    }
    fn local_change(&mut self, change: Change) {
        if change != Change::Invalid {
            self.local_revision = self.local_revision.wrapping_add(1);
        }
        match change {
            Change::Start => {
                self.cancel_work();
                self.stop();
                self.start_saved(None);
            }
            Change::Stop => {
                self.cancel_work();
                self.stop();
                self.local_status = Status::Ok;
            }
            Change::Clear => {
                self.cancel_work();
                if let Some(audio) = &mut self.audio {
                    audio.clear();
                }
                self.local_store = None;
                self.local_status = Status::Ok;
            }
            Change::Edit => {}
            Change::Invalid => {
                self.action_status = "This action is unavailable for saved music.";
                return;
            }
        }
        self.action_status = "";
    }
    fn start_saved(&mut self, input: Option<PlaybackInput>) {
        let Some(entry) = self.local.current() else {
            return;
        };
        let (id, duration) = (entry.track.id.clone(), entry.track.duration);
        let Some(audio) = &mut self.audio else {
            return;
        };
        audio.anchor(self.local.elapsed, duration);
        if let Some(input) = input {
            if input.item.id != id
                || input.bytes.is_empty()
                || input.bytes.len() > offline::MAX_TRACK_BYTES
            {
                self.local_status = Status::Media;
                self.local.desired = false;
                return;
            }
            audio.discard_cache();
            self.local_status = audio.play(
                id,
                input.bytes.into(),
                self.local.elapsed,
                duration,
                Authority::offline(),
            );
            if self.local_status != Status::Ok {
                self.local.desired = false;
            }
            return;
        }
        // Re-read even on same-track Resume/Seek: the store may have removed
        // or replaced the download since it was last admitted. The one slot
        // also bounds physically running blocking reads after task cancellation.
        audio.discard_cache();
        let (Some(account), Some(store)) = (&self.account, self.local_store.clone()) else {
            self.local.desired = false;
            self.local_status = Status::Media;
            return;
        };
        self.local_status = Status::Ok;
        let (api, session, slot, results) = (
            account.api.clone(),
            account.session.clone(),
            self.local_read_slot.clone(),
            self.results.clone(),
        );
        let (epoch, operation) = (self.epoch, self.operation);
        let stamp = *session.network_changes().borrow();
        self.media = Some(tokio::spawn(async move {
            let Ok(permit) = slot.acquire_owned().await else {
                return;
            };
            let input = tokio::task::spawn_blocking(move || {
                let _permit = permit;
                if *session.network_changes().borrow() != stamp || !session.is_offline() {
                    return None;
                }
                let input = store.read(&api, &session, &id).ok();
                if *session.network_changes().borrow() == stamp {
                    input
                } else {
                    None
                }
            })
            .await
            .ok()
            .flatten();
            let _ = results
                .send(Reply::OfflineMedia {
                    epoch,
                    operation,
                    input,
                })
                .await;
        }));
    }
    fn reply(&mut self, reply: Reply) {
        if !self.current() {
            return;
        }
        self.apply_mode();
        match reply {
            Reply::Reconnect {
                epoch,
                serial,
                result,
            } => self.finish_reconnect(epoch, serial, result),
            Reply::Login(epoch, api, result)
                if epoch == self.epoch && !self.offline && self.login.is_some() =>
            {
                self.login = None;
                match result {
                    Ok(session) => {
                        self.account = Some(Account { api, session });
                        self.login_view(false, false, "");
                        self.publish_access();
                        self.connect();
                    }
                    Err(e) => self.login_view(false, true, e.message),
                }
            }
            Reply::OfflineMedia {
                epoch,
                operation,
                input,
            } if self.epoch == epoch
                && self.operation == operation
                && self.offline
                && self.local.desired =>
            {
                self.media = None;
                if let Some(input) = input {
                    self.start_saved(Some(input));
                } else {
                    self.local.desired = false;
                    self.local_status = Status::Media;
                }
            }
            Reply::Media {
                epoch,
                operation,
                result,
            } if self.epoch == epoch && self.operation == operation && !self.offline => {
                self.media = None;
                if let Some(work) = self.pending.take() {
                    match result {
                        Ok(bytes) => self.complete(work, Some(bytes.into()), Status::Ok),
                        Err(e) if e.expired => self.expire(),
                        Err(_) => self.complete(work, None, Status::Media),
                    }
                }
            }
            _ => {}
        }
    }
    fn link(&self) -> Link {
        self.socket.as_ref().map_or(Link::Closed, |s| {
            if s.link.has_changed().is_err() {
                Link::Closed
            } else {
                *s.link.borrow()
            }
        })
    }
    fn cancel_work(&mut self) {
        self.operation = self.operation.wrapping_add(1);
        if let Some(task) = self.media.take() {
            task.abort();
        }
        if let Some(work) = self.pending.take()
            && let Some(g) = work.grant
        {
            g.revoke();
        }
    }
    fn stop(&mut self) {
        if let Some(e) = self.execution.take()
            && let Some(g) = e.grant
        {
            g.revoke();
        }
        if let Some(audio) = &mut self.audio {
            audio.stop();
        }
    }
    fn events(&mut self) {
        if self.link() == Link::Expired {
            self.expire();
            return;
        }
        if !matches!(self.link(), Link::Connected(g) if self.generation == Some(g)) {
            if self.pending.is_some() {
                self.cancel_work();
            }
            if self.execution.is_some() {
                self.stop();
            }
            // Keep the observed room for read-only presentation, not authority.
        }
        for _ in 0..32 {
            if !self.network_allowed() {
                break;
            }
            let Some(event) = self.socket.as_mut().and_then(|s| s.events.try_recv().ok()) else {
                break;
            };
            let before_stop = self.pause_fence.as_mut().is_some_and(|fence| {
                if fence.buffered == 0 {
                    return false;
                }
                fence.buffered -= 1;
                fence.generation == Some(event.generation)
            });
            if event.generation == 0 || self.link() != Link::Connected(event.generation) {
                continue;
            }
            self.event(event, before_stop);
        }
    }
    fn event(&mut self, event: socket::Event, before_stop: bool) {
        let generation = event.generation;
        if let EventKind::Snapshot {
            connection_id,
            snapshot,
            ..
        } = event.kind
        {
            if self.generation != Some(generation) {
                self.cancel_work();
                self.stop();
                self.pause_fence = None;
                self.snapshot = None;
                self.anchor = None;
                self.room_incarnation = self.room_incarnation.wrapping_add(1);
                self.generation = Some(generation);
                self.connection_id = connection_id;
            }
            self.state(snapshot);
            return;
        }
        if self.generation != Some(generation) {
            return;
        }
        match event.kind {
            EventKind::Snapshot { .. } => unreachable!(),
            EventKind::State(snapshot) => self.state(snapshot),
            EventKind::Playback(playback) => self.anchor(playback),
            EventKind::Instruction {
                instruction,
                deadline,
            } => self.instruction(generation, instruction, deadline, before_stop),
            EventKind::Lease { binding, deadline } => {
                let grant = self
                    .execution
                    .as_ref()
                    .filter(|e| e.binding == binding)
                    .and_then(|e| e.grant.as_ref());
                if let Some(g) = grant {
                    g.renew_until(deadline);
                }
            }
            EventKind::LeaseExpired(binding) => {
                if self
                    .pending
                    .as_ref()
                    .is_some_and(|w| w.instruction.binding() == binding)
                {
                    self.cancel_work();
                }
                if self
                    .execution
                    .as_ref()
                    .is_some_and(|e| e.binding == binding)
                {
                    self.stop();
                }
            }
            // Only the independent settlement queue may affect command feedback.
            EventKind::Result { .. }
            | EventKind::CommandNotSent { .. }
            | EventKind::CommandUncertain { .. } => {}
            // This engine never attaches a PlanID to an ended report. V2 will
            // advance canonically and send a normal absolute execute instead.
            EventKind::NextAdmitted { .. } => {
                self.cancel_work();
                self.stop();
            }
        }
    }
    fn drain_settlements(&mut self) {
        if let Some(socket) = &mut self.socket {
            for event in settlement_batch(&mut socket.settlements) {
                self.settlement(event);
            }
        }
    }
    fn settlement(&mut self, event: socket::Event) {
        if !self.current() || self.offline || self.account.is_none() || event.generation == 0 {
            return;
        }
        // Request IDs are actor-lifetime unique. A receipt for an old socket
        // generation can still settle its matching action, but this path
        // cannot admit instructions, leases, snapshots or native work.
        match event.kind {
            EventKind::Result { request_id, result } => {
                if !result.is_final {
                    return;
                }
                if request_id != self.request_id {
                    return;
                }
                self.action_status = match (result.outcome, result.execution) {
                    (socket::Outcome::Rejected, _) => {
                        "This playback action is no longer available."
                    }
                    (_, socket::Execution::Blocked) => {
                        "This output needs permission to play audio."
                    }
                    (_, socket::Execution::Media) => "This output could not play the audio.",
                    (_, socket::Execution::Uncertain) => "Playback action unconfirmed.",
                    _ => "",
                };
            }
            EventKind::CommandNotSent { request_id, .. } if request_id == self.request_id => {
                self.action_status = "The playback action was not sent.";
            }
            EventKind::CommandUncertain { request_id } if request_id == self.request_id => {
                self.action_status = "Playback action unconfirmed.";
            }
            _ => {}
        }
    }
    fn state(&mut self, snapshot: Arc<socket::Snapshot>) {
        if self
            .snapshot
            .as_ref()
            .is_some_and(|previous| previous.state_version > snapshot.state_version)
        {
            return;
        }
        // The source may continue during a different target's prepare, but it
        // must stop if this connection is neither selected nor a handoff target.
        let mine = snapshot.selected_connection_id == self.connection_id
            || snapshot
                .transfer
                .as_ref()
                .is_some_and(|t| t.target_connection_id == self.connection_id);
        if !mine
            || self
                .execution
                .as_ref()
                .is_some_and(|e| e.binding.output_version < snapshot.output_version)
            || self
                .pending
                .as_ref()
                .is_some_and(|w| w.instruction.output_version < snapshot.output_version)
        {
            self.cancel_work();
            self.stop();
        }
        if let Some(o) = &snapshot.state.occurrence {
            if self.recent.as_ref().is_none_or(|r| r.event != o.event_id) {
                self.recent = Some(Recent {
                    event: o.event_id.clone(),
                    amount: o.progress,
                    // First observation can be an already reported retained account snapshot.
                    emitted: o.reported && self.snapshot.is_none(),
                    observed: None,
                });
            }
            if let Some(r) = &mut self.recent {
                r.amount = r.amount.max(o.progress);
            }
            if o.reported {
                self.notify_recent();
            }
        } else {
            self.recent = None;
        }
        let playback = snapshot.playback.clone();
        self.snapshot = Some(snapshot);
        self.anchor(playback);
    }
    fn anchor(&mut self, playback: socket::Playback) {
        let Some(s) = &self.snapshot else {
            return;
        };
        if s.output_version != playback.output_version
            || s.state_version != playback.state_version
            || s.state.event_id() != playback.event_id
        {
            return;
        }
        if self.anchor.as_ref().is_some_and(|(p, _)| {
            p.output_version == playback.output_version
                && p.state_version == playback.state_version
                && p.playback_sequence >= playback.playback_sequence
        }) {
            return;
        }
        let now = Instant::now();
        if playback.has_reported_playback
            && let Some(r) = &mut self.recent
            && r.event == playback.event_id
        {
            if let Some((before, pos)) = r.observed {
                let heard = playback.elapsed - pos;
                if heard >= 0.0
                    && heard <= now.saturating_duration_since(before).as_secs_f64() + 1.0
                {
                    r.amount += heard;
                }
            }
            r.observed = (playback.is_playing
                && !playback.is_buffering
                && !playback.is_loading
                && playback.error == MediaError::None)
                .then_some((now, playback.elapsed));
            // History is qualified only after more than thirty seconds of
            // contiguous reported media advance. Catalog still reads it via REST.
            if r.amount > 30.0 {
                self.notify_recent();
            }
        }
        self.anchor = Some((playback, now));
    }
    fn notify_recent(&mut self) {
        if let Some(r) = &mut self.recent
            && !r.emitted
        {
            r.emitted = true;
            self.recent_change = self.recent_change.wrapping_add(1);
            self.publish_access();
        }
    }
    fn instruction(
        &mut self,
        generation: Generation,
        instruction: Instruction,
        deadline: Option<Instant>,
        before_stop: bool,
    ) {
        let link = self.socket.as_ref().map(|s| s.link.clone());
        self.admit_instruction(generation, instruction, deadline, before_stop, link);
    }
    /// The socket has already checked ordering, binding, and the lease's send
    /// basis. Only this exact grant may authorize the native continuation.
    fn admit_instruction(
        &mut self,
        generation: Generation,
        mut instruction: Instruction,
        deadline: Option<Instant>,
        before_stop: bool,
        link: Option<watch::Receiver<Link>>,
    ) {
        let binding = instruction.binding();
        match instruction.kind {
            // Staging is optional. Publish an exact real end and allow the
            // coordinator to choose the next occurrence non-speculatively.
            InstructionKind::StageNext | InstructionKind::CancelNext => return,
            InstructionKind::Release => {
                if let Some(audio) = &mut self.audio {
                    audio.stop();
                }
                self.sample(true, false);
                self.cancel_work();
                self.stop();
                self.ack(generation, binding, Status::Ok);
                return;
            }
            _ => {}
        }
        let starting = instruction.desired_playing && instruction.kind != InstructionKind::Prepare;
        if starting
            && (before_stop
                || self.pause_fence.as_ref().is_some_and(|fence| {
                    fence.generation == Some(generation) && fence.binding.as_ref() == Some(&binding)
                }))
        {
            // This operation was interrupted before native admission. Do not
            // ACK an unstarted execution or ack-reject a newer server binding.
            return;
        }
        if instruction.kind == InstructionKind::Execute
            && !instruction.desired_playing
            && self
                .execution
                .as_ref()
                .is_some_and(|e| e.binding.event_id == instruction.event_id)
            && let Some(audio) = &self.audio
        {
            instruction.elapsed = instruction.elapsed.max(audio.facts().elapsed);
        }
        self.cancel_work();
        self.stop();
        let grant = deadline
            .zip(link)
            .map(|(deadline, link)| Authority::online(deadline, link, generation));
        if instruction.kind != InstructionKind::Prepare
            && grant.as_ref().is_none_or(|g| !g.is_valid())
        {
            self.ack(generation, binding, Status::Rejected);
            return;
        }
        if starting {
            self.pause_fence = None;
        }
        let work = Work {
            generation,
            instruction,
            grant,
        };
        let id = work
            .instruction
            .track
            .as_ref()
            .map(|track| track.id.clone());
        if let Some(id) = id.filter(|_| {
            work.instruction.desired_playing || work.instruction.kind == InstructionKind::Prepare
        }) {
            if let Some(bytes) = self.audio.as_ref().and_then(|audio| audio.cached(&id)) {
                self.complete(work, Some(bytes), Status::Ok);
            } else if let Some(a) = &self.account {
                let deadline = deadline.map_or(LOAD_BUDGET, |d| {
                    d.saturating_duration_since(Instant::now()).min(LOAD_BUDGET)
                });
                let (api, session, results, epoch, operation) = (
                    a.api.clone(),
                    a.session.clone(),
                    self.results.clone(),
                    self.epoch,
                    self.operation,
                );
                self.pending = Some(work);
                self.media = Some(tokio::spawn(async move {
                    let result = tokio::time::timeout(deadline, api.media(&session, &id))
                        .await
                        .unwrap_or(Err(ApiError {
                            message: "Media could not be loaded in time.",
                            expired: false,
                            status: 0,
                        }));
                    let _ = results
                        .send(Reply::Media {
                            epoch,
                            operation,
                            result,
                        })
                        .await;
                }));
            }
        } else {
            self.complete(work, None, Status::Ok);
        }
    }
    fn complete(&mut self, work: Work, bytes: Option<Arc<[u8]>>, mut status: Status) {
        if !self.network_allowed() || self.link() != Link::Connected(work.generation) {
            return;
        }
        let instruction = work.instruction;
        let binding = instruction.binding();
        let Some(audio) = &mut self.audio else {
            return;
        };
        let prepare = instruction.kind == InstructionKind::Prepare;
        let mut grant = work.grant;
        if !prepare && grant.as_ref().is_none_or(|g| !g.is_valid()) {
            status = Status::Rejected;
        }
        if !prepare {
            let duration = instruction.track.as_ref().map_or(0.0, |t| t.duration);
            audio.anchor(instruction.elapsed, duration);
        }
        if status == Status::Ok {
            match (
                prepare,
                instruction.desired_playing,
                instruction.track,
                bytes,
            ) {
                (true, _, Some(track), Some(bytes)) => {
                    status = audio.prepare(track.id, bytes, instruction.elapsed)
                }
                (false, true, Some(track), Some(bytes)) => {
                    status = audio.play(
                        track.id,
                        bytes,
                        instruction.elapsed,
                        track.duration,
                        grant.as_ref().unwrap().clone(),
                    );
                }
                (true, _, None, _) | (false, false, _, _) => {}
                _ => status = Status::Media,
            }
        }
        if !instruction.desired_playing || status != Status::Ok {
            audio.stop();
            if let Some(g) = grant.take() {
                g.revoke();
            }
        }
        if !self.ack(work.generation, binding.clone(), status) {
            self.stop();
            return;
        }
        if !prepare && status != Status::Rejected {
            self.execution = Some(Execution {
                generation: work.generation,
                binding,
                grant,
                status,
                sent_at: None,
                ended: false,
            });
            self.sample(true, true);
        }
    }
    fn ack(&self, generation: Generation, binding: Binding, status: Status) -> bool {
        if !self.network_allowed() {
            return false;
        }
        self.socket.as_ref().is_some_and(|s| {
            s.ack(
                generation,
                binding,
                match status {
                    Status::Ok => socket::AckStatus::Ok,
                    Status::Blocked => socket::AckStatus::Blocked,
                    Status::Media => socket::AckStatus::Media,
                    Status::Rejected => socket::AckStatus::Rejected,
                },
            )
            .is_ok()
        })
    }
    fn sample(&mut self, force: bool, allow_end: bool) {
        if !self.network_allowed() {
            return;
        }
        let now = Instant::now();
        let Some(e) = &mut self.execution else {
            return;
        };
        let (Some(socket), Some(audio)) = (&self.socket, &self.audio) else {
            return;
        };
        if *socket.link.borrow() != Link::Connected(e.generation) || e.ended {
            return;
        }
        if !force && e.grant.is_none() {
            return;
        }
        let facts = audio.facts();
        let ended = allow_end
            && facts.ended
            && self.snapshot.as_ref().is_some_and(|s| s.transfer.is_none());
        if force
            || ended
            || e.sent_at
                .is_none_or(|at| now.saturating_duration_since(at) >= REPORT_INTERVAL)
        {
            let report = socket::Report {
                elapsed: facts.elapsed,
                duration: facts.duration,
                is_playing: facts.playing && !ended,
                is_buffering: false,
                is_loading: facts.loading,
                ended,
                error: match e.status {
                    Status::Blocked => MediaError::Blocked,
                    Status::Media => MediaError::Media,
                    _ => MediaError::None,
                },
                plan_id: None,
            };
            if socket
                .report(e.generation, e.binding.clone(), report)
                .is_ok()
            {
                e.sent_at = Some(now);
                e.ended = ended;
            } else {
                self.stop();
            }
        }
    }
    fn tick(&mut self) {
        let requested_epoch = self.shared_epoch.load(Ordering::SeqCst);
        if self.epoch != requested_epoch {
            let offline = self.offline
                || self
                    .account
                    .as_ref()
                    .is_some_and(|a| a.session.is_offline());
            self.reset(requested_epoch);
            self.offline = offline;
            self.login_view(false, false, "");
            return;
        }
        // This control has no mailbox backlog; consumption and lifting the
        // shared Session fence happen under its latest-request lock.
        self.apply_mode();
        if self.offline {
            if self
                .account
                .as_ref()
                .is_some_and(|a| !a.session.can_access_local())
            {
                self.expire();
                return;
            }
            if self.local.desired
                && self.media.is_none()
                && let Some(audio) = &self.audio
            {
                let facts = audio.facts();
                self.local.elapsed = facts.elapsed;
                if facts.ended {
                    let change = self.local.advance(true);
                    self.local_change(change);
                }
            }
            return;
        }
        if self
            .account
            .as_ref()
            .is_some_and(|a| a.session.is_expired())
        {
            self.expire();
            return;
        }
        if !self.network_allowed() {
            self.cancel_work();
            self.stop();
            self.socket = None;
            return;
        }
        self.events();
        // Settlement has its own credit/buffer and must drain even while the
        // link is disconnected or a new generation is connecting.
        self.drain_settlements();
        if self
            .execution
            .as_ref()
            .and_then(|e| e.grant.as_ref())
            .is_some_and(|g| !g.is_valid())
        {
            self.stop();
        }
        self.sample(false, true);
    }
    fn publish(&self) {
        let mut p = PlayerView {
            offline: self.offline,
            reconnecting: self.reconnect.is_some(),
            status: self.action_status,
            room_token: self.room_token().unwrap_or_default(),
            ..PlayerView::default()
        };
        if self.offline {
            p.output = "This desktop · Offline".into();
            p.shuffle = self.local.shuffle;
            p.repeat = self.local.repeat.wire();
            p.elapsed = self.local.elapsed;
            if let Some(entry) = self.local.current() {
                p.title = entry.track.title.clone();
                p.detail = entry.track.subtitle.clone();
                p.track = entry.track.id.clone();
                p.album = entry.track.album_id.clone();
                p.artist = entry.track.artist_id.clone();
                p.context_kind = "offline";
                p.context_name = "Saved music".into();
                p.duration = entry.track.duration;
                p.control = true;
                p.transport = true;
            }
            if let Some(audio) = &self.audio
                && self.local.desired
            {
                let facts = audio.facts();
                p.elapsed = facts.elapsed;
                p.duration = facts.duration;
                p.playing = facts.playing;
            }
            p.status = match self.local_status {
                Status::Media => "Saved audio could not be played.",
                Status::Blocked => "Audio output is unavailable on this desktop.",
                _ => p.status,
            };
            if p.reconnecting {
                p.status = "Connecting… · Working offline";
            }
            p.queue = self
                .local
                .entries
                .iter()
                .enumerate()
                .map(|(i, e)| QueueRow {
                    key: e.key.to_string().into(),
                    title: e.track.title.clone().into(),
                    subtitle: e.track.subtitle.clone().into(),
                    current: self.local.position == Some(i),
                    upcoming: self.local.position.is_some_and(|pos| i > pos),
                })
                .collect();
        } else if let Some(s) = &self.snapshot {
            let live = self
                .generation
                .is_some_and(|g| self.link() == Link::Connected(g));
            p.shuffle = s.state.shuffle;
            p.repeat = match s.state.repeat {
                socket::Repeat::Off => Repeat::Off,
                socket::Repeat::All => Repeat::All,
                socket::Repeat::One => Repeat::One,
            }
            .wire();
            p.transport = live && s.state.current().is_some();
            p.control = p.transport
                || self
                    .execution
                    .as_ref()
                    .and_then(|e| e.grant.as_ref())
                    .is_some_and(Authority::is_valid);
            p.output = s
                .devices
                .iter()
                .find(|d| d.device_id == s.selected_device_id)
                .map(|d| d.name.clone())
                .unwrap_or_else(|| "No output selected".into());
            if let Some(track) = s.state.current() {
                p.title = track.name.clone();
                p.detail = artists(track);
                p.track = track.id.clone();
                p.album = track.album_id.clone();
                p.artist = track
                    .artists
                    .iter()
                    .find_map(|a| a.id.clone())
                    .unwrap_or_default();
                if let Some(context) = track.playback_context.as_ref().or(s.state.context.as_ref())
                {
                    p.context_kind = context_kind(context.kind);
                    p.context_id = context.id.clone();
                    p.context_name = context.name.clone();
                }
            }
            p.elapsed = s.state.elapsed;
            p.duration = s.state.duration;
            if let Some((a, at)) = &self.anchor
                && a.state_version == s.state_version
                && a.output_version == s.output_version
                && a.event_id == s.state.event_id()
                && a.has_reported_playback
            {
                p.duration = a.duration;
                p.elapsed = a.elapsed;
                p.playing = live && a.is_playing && s.selected_connection_id != self.connection_id;
                p.error = a.error;
                if p.playing && !a.is_loading && !a.is_buffering && p.duration > 0.0 {
                    p.elapsed = (a.elapsed + at.elapsed().as_secs_f64().min(3.0)).min(p.duration);
                }
            }
            if self.execution.is_some()
                && let Some(audio) = &self.audio
            {
                let facts = audio.facts();
                p.playing = facts.playing;
                p.elapsed = facts.elapsed;
                if facts.duration > 0.0 {
                    p.duration = facts.duration;
                }
            }
            p.queue = s
                .state
                .queue
                .iter()
                .enumerate()
                .map(|(i, t)| QueueRow {
                    key: t.key.to_string().into(),
                    title: t.name.clone().into(),
                    subtitle: artists(t).into(),
                    current: i as i64 == s.state.position,
                    upcoming: i as i64 > s.state.position,
                })
                .collect();
            p.devices = s
                .devices
                .iter()
                .map(|d| DeviceRow {
                    id: d.device_id.clone().into(),
                    name: d.name.clone().into(),
                    selected: d.device_id == s.selected_device_id,
                    available: live && d.available,
                    can_play: d.can_play,
                })
                .collect();
            if p.status.is_empty() && s.transfer.is_some() && live {
                p.status = "Switching output…";
            }
            if p.status.is_empty() && self.pending.as_ref().is_some_and(|w| w.grant.is_some()) {
                p.status = "Loading audio…";
            }
        }
        p.publish(self.window.clone(), self.shared_epoch.clone(), self.epoch);
    }
}

#[derive(Default)]
struct PlayerView {
    offline: bool,
    reconnecting: bool,
    shuffle: bool,
    repeat: &'static str,
    status: &'static str,
    room_token: String,
    error: MediaError,
    playing: bool,
    control: bool,
    transport: bool,
    elapsed: f64,
    duration: f64,
    output: String,
    title: String,
    detail: String,
    track: String,
    album: String,
    artist: String,
    context_kind: &'static str,
    context_id: String,
    context_name: String,
    queue: Vec<QueueRow>,
    devices: Vec<DeviceRow>,
}
impl PlayerView {
    fn publish(self, weak: slint::Weak<AppWindow>, shared: Arc<AtomicU64>, epoch: u64) {
        let _ = slint::invoke_from_event_loop(move || {
            if shared.load(Ordering::SeqCst) != epoch {
                return;
            }
            let Some(ui) = weak.upgrade() else {
                return;
            };
            ui.set_offline_playback(self.offline);
            ui.set_reconnecting(self.reconnecting);
            ui.set_room_token(self.room_token.into());
            ui.set_shuffle(self.shuffle);
            ui.set_repeat_mode(
                if self.repeat.is_empty() {
                    "off"
                } else {
                    self.repeat
                }
                .into(),
            );
            ui.set_now_title(if self.title.is_empty() {
                "Nothing playing".into()
            } else {
                self.title.into()
            });
            ui.set_now_detail(self.detail.into());
            ui.set_now_track_id(self.track.into());
            ui.set_now_album_id(self.album.into());
            ui.set_now_artist_id(self.artist.into());
            ui.set_now_context_kind(self.context_kind.into());
            ui.set_now_context_id(self.context_id.into());
            ui.set_now_context_name(self.context_name.into());
            ui.set_output_name(self.output.into());
            ui.set_playing(self.playing);
            ui.set_can_control(self.control);
            ui.set_remote_controls(self.transport);
            ui.set_can_seek(self.transport && self.duration > 0.0);
            ui.set_elapsed(self.elapsed as f32);
            ui.set_duration(self.duration as f32);
            let status = if !self.status.is_empty() {
                self.status
            } else {
                match self.error {
                    MediaError::Blocked => "This output needs permission to play audio.",
                    MediaError::Media => "This output could not play the audio.",
                    MediaError::None => "",
                }
            };
            ui.set_player_status(status.into());
            ui.set_queue_status(if self.queue.is_empty() {
                "Nothing in the queue".into()
            } else {
                "".into()
            });
            ui.set_device_status(if self.offline {
                "Device control is unavailable offline".into()
            } else if self.devices.is_empty() {
                "No outputs available".into()
            } else {
                "".into()
            });
            ui.set_time_label(
                format!(
                    "{}:{:02} / {}:{:02}",
                    self.elapsed as u64 / 60,
                    self.elapsed as u64 % 60,
                    self.duration as u64 / 60,
                    self.duration as u64 % 60
                )
                .into(),
            );
            let queue = ui.get_queue_rows();
            if queue.row_count() != self.queue.len()
                || self.queue.iter().enumerate().any(|(i, row)| {
                    queue.row_data(i).is_none_or(|old| {
                        old.key != row.key
                            || old.title != row.title
                            || old.subtitle != row.subtitle
                            || old.current != row.current
                            || old.upcoming != row.upcoming
                    })
                })
            {
                ui.set_queue_rows(ModelRc::new(VecModel::from(self.queue)));
            }
            let devices = ui.get_device_rows();
            if devices.row_count() != self.devices.len()
                || self.devices.iter().enumerate().any(|(i, row)| {
                    devices.row_data(i).is_none_or(|old| {
                        old.id != row.id
                            || old.name != row.name
                            || old.selected != row.selected
                            || old.available != row.available
                            || old.can_play != row.can_play
                    })
                })
            {
                ui.set_device_rows(ModelRc::new(VecModel::from(self.devices)));
            }
        });
    }
}
async fn reconnect_attempts<F, Fut>(
    mode: &ModeControl,
    serial: u64,
    current_epoch: impl Fn() -> bool,
    mut probe: F,
) -> Option<Result<(), ApiError>>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<(), ApiError>>,
{
    let cancelled = mode.cancelled(serial);
    tokio::pin!(cancelled);
    let mut delay = PROBE_FIRST_DELAY;
    loop {
        if !current_epoch() {
            return None;
        }
        let result = tokio::select! {
            biased;
            _ = &mut cancelled => return None,
            result = tokio::time::timeout(PROBE_TIMEOUT, async { probe().await }) => result.unwrap_or(Err(ApiError {
                message: "Connection is unavailable.", status: 0, expired: false,
            })),
        };
        let transient = result
            .as_ref()
            .is_err_and(|e| !e.expired && matches!(e.status, 0 | 408 | 429 | 500..=599));
        if !transient {
            return Some(result);
        }
        tokio::select! {
            biased;
            _ = &mut cancelled => return None,
            _ = tokio::time::sleep(delay) => {}
        }
        delay = next_probe_delay(delay);
    }
}
fn next_probe_delay(delay: Duration) -> Duration {
    delay.saturating_mul(2).min(PROBE_MAX_DELAY)
}
fn settlement_batch(receiver: &mut mpsc::Receiver<socket::Event>) -> Vec<socket::Event> {
    (0..SETTLEMENT_BATCH)
        .map_while(|_| receiver.try_recv().ok())
        .collect()
}
async fn socket_signal(socket: Option<&mut socket::Socket>) {
    if let Some(socket) = socket
        && socket.link.changed().await.is_ok()
    {
        return;
    }
    std::future::pending::<()>().await;
}
fn context_kind(kind: socket::ContextKind) -> &'static str {
    match kind {
        socket::ContextKind::Album => "album",
        socket::ContextKind::Artist => "artist",
        socket::ContextKind::Playlist => "playlist",
        socket::ContextKind::Derived => "derived",
        socket::ContextKind::Likes => "likes",
    }
}
fn artists(t: &socket::Track) -> String {
    t.artists
        .iter()
        .map(|a| a.name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}
fn connection(input: &str, lan: bool) -> Result<Api, ApiError> {
    let origin = crate::api::Origin::parse(input)?;
    if origin.as_str().starts_with("http:") && !lan {
        return Err(ApiError {
            message: "Confirm trusted-LAN HTTP, or use HTTPS.",
            expired: false,
            status: 0,
        });
    }
    Api::new(origin)
}

pub fn start(
    window: slint::Weak<AppWindow>,
    epoch: Arc<AtomicU64>,
    access: watch::Sender<Option<Access>>,
    mode: ModeControl,
) -> (mpsc::Sender<Command>, std::thread::JoinHandle<()>) {
    let (tx, mut commands) = mpsc::channel(32);
    let worker = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("playback runtime");
        runtime.block_on(async move {
            let (results, mut replies) = mpsc::channel(16);
            let mut e = Engine {
                epoch: 0,
                shared_epoch: epoch,
                mode,
                access,
                results,
                window,
                device_id: uuid::Uuid::new_v4().to_string(),
                login: None,
                reconnect: None,
                logouts: vec![],
                account: None,
                offline: false,
                audio: None,
                local: LocalQueue::default(),
                local_status: Status::Ok,
                local_store: None,
                local_read_slot: Arc::new(Semaphore::new(1)),
                socket: None,
                generation: None,
                snapshot: None,
                anchor: None,
                connection_id: String::new(),
                execution: None,
                pending: None,
                media: None,
                operation: 0,
                request_id: 0,
                room_incarnation: 0,
                local_revision: 0,
                pause_fence: None,
                recent: None,
                recent_change: 0,
                action_status: "",
            };
            let mut cadence = tokio::time::interval(TICK);
            cadence.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut refresh = 0;
            loop {
                tokio::select! {
                    command = commands.recv() => {
                        if !e.command(command.unwrap_or(Command::Shutdown)) { break; }
                        e.publish();
                    }
                    Some(reply) = replies.recv() => { e.reply(reply); e.publish(); },
                    _ = socket_signal(e.socket.as_mut()) => { e.tick(); e.publish(); },
                    _ = cadence.tick() => {
                        e.tick();
                        refresh = (refresh + 1) % 5;
                        if refresh == 0 { e.publish(); }
                    }
                }
            }
            for task in e.logouts {
                let _ = task.await;
            }
        });
        runtime.shutdown_timeout(Duration::from_secs(2));
    });
    (tx, worker)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn engine() -> Engine {
        let (results, _) = mpsc::channel(2);
        let (access, _) = watch::channel(None);
        Engine {
            epoch: 7,
            shared_epoch: Arc::new(AtomicU64::new(7)),
            mode: ModeControl::default(),
            access,
            results,
            window: slint::Weak::default(),
            device_id: "synthetic".into(),
            login: None,
            reconnect: None,
            logouts: vec![],
            account: None,
            offline: false,
            audio: None,
            local: LocalQueue::default(),
            local_status: Status::Ok,
            socket: None,
            local_store: None,
            local_read_slot: Arc::new(Semaphore::new(1)),
            generation: None,
            snapshot: None,
            anchor: None,
            connection_id: String::new(),
            execution: None,
            pending: None,
            media: None,
            operation: 0,
            request_id: 0,
            room_incarnation: 0,
            local_revision: 0,
            pause_fence: None,
            recent: None,
            recent_change: 0,
            action_status: "",
        }
    }
    #[test]
    fn stale_background_play_and_unscoped_mode_change_are_discarded() {
        let mut e = engine();
        e.command(Command::Offline(true));
        assert!(!e.offline);
        e.command(Command::Offline(true).scoped(6));
        assert!(!e.offline);
        e.command(Command::Offline(true).scoped(7));
        assert!(e.offline && e.socket.is_none());
        e.shared_epoch.store(8, Ordering::SeqCst);
        e.command(Command::Offline(false).scoped(7));
        assert!(e.offline);
    }
    #[tokio::test]
    async fn offline_mode_fences_shared_session_and_never_logs_out_on_the_wire() {
        let mut e = engine();
        let api = Arc::new(connection("http://127.0.0.1:1", true).unwrap());
        let session = api.test_session();
        e.account = Some(Account {
            api,
            session: session.clone(),
        });
        e.command(Command::Offline(true).scoped(7));
        assert!(session.is_offline());
        assert!(e.access.borrow().as_ref().unwrap().offline);
        assert!(e.socket.is_none());
        e.command(Command::Shutdown);
        assert!(e.access.borrow().is_none());
        for task in e.logouts.drain(..) {
            task.await.unwrap();
        }
        assert!(session.is_expired());
    }
    #[tokio::test]
    async fn cookie_expiry_keeps_local_access_but_go_online_requires_sign_in() {
        let mut e = engine();
        let api = Arc::new(connection("http://127.0.0.1:1", true).unwrap());
        let deadline = Instant::now() + Duration::from_millis(100);
        let session = api.test_session_with_deadline(deadline);
        e.account = Some(Account {
            api,
            session: session.clone(),
        });
        e.command(Command::Offline(true).scoped(7));
        tokio::time::sleep_until(tokio::time::Instant::from_std(
            deadline + Duration::from_millis(1),
        ))
        .await;
        e.tick();
        assert!(e.audio.is_some() && e.access.borrow().as_ref().unwrap().offline);
        let access = e.access.borrow().as_ref().unwrap().clone();
        e.mode.request(&access, false);
        e.tick();
        let serial = e.reconnect.as_ref().unwrap().serial;
        e.reply(Reply::Reconnect {
            epoch: 7,
            serial,
            result: Err(ApiError {
                message: "Expired",
                expired: true,
                status: 401,
            }),
        });
        assert!(e.offline && e.account.is_none() && e.socket.is_none());
        assert!(e.access.borrow().is_none());
        assert!(session.is_offline()); // never reopened the expired internet credential
    }
    #[test]
    fn explicit_sign_in_leaves_offline_even_if_the_new_origin_is_invalid() {
        let mut e = engine();
        e.command(Command::Offline(true).scoped(7));
        e.command(Command::Logout(7));
        assert!(e.offline);
        e.shared_epoch.store(8, Ordering::SeqCst);
        e.command(Command::Login {
            epoch: 8,
            origin: "not an origin".into(),
            username: "test".into(),
            password: "test".into(),
            lan: false,
        });
        assert!(!e.offline && e.epoch == 8 && e.socket.is_none());
    }
    #[tokio::test]
    async fn shared_fences_retire_even_if_mode_or_logout_command_never_arrives() {
        let mut e = engine();
        let api = Arc::new(connection("http://127.0.0.1:1", true).unwrap());
        let session = api.test_session();
        e.account = Some(Account {
            api,
            session: session.clone(),
        });
        e.login = Some(tokio::spawn(std::future::pending()));
        session.set_offline(true); // simulate UI fence with a full command queue
        e.tick();
        assert!(e.offline && e.socket.is_none() && e.login.is_none());
        assert!(e.access.borrow().as_ref().unwrap().offline);
        e.shared_epoch.store(8, Ordering::SeqCst); // logout message was never delivered
        e.tick();
        assert_eq!(e.epoch, 8);
        assert!(e.account.is_none() && e.audio.is_none() && e.access.borrow().is_none());
        for task in e.logouts.drain(..) {
            let _ = task.await;
        }
        assert!(!session.can_access_local());
    }
    #[tokio::test]
    async fn latest_offline_choice_cannot_be_overridden_by_an_earlier_online_request() {
        let mut e = engine();
        let api = Arc::new(connection("http://127.0.0.1:1", true).unwrap());
        let session = api.test_session();
        e.account = Some(Account {
            api,
            session: session.clone(),
        });
        e.publish_access();
        let access = e.access.borrow().as_ref().unwrap().clone();
        e.mode.request(&access, true);
        e.tick();
        assert!(e.offline);
        let access = e.access.borrow().as_ref().unwrap().clone();
        e.mode.request(&access, false);
        e.tick();
        let serial = e.reconnect.as_ref().unwrap().serial;
        e.mode.request(&access, true);
        e.reply(Reply::Reconnect {
            epoch: 7,
            serial,
            result: Ok(()),
        });
        assert!(e.offline && session.is_offline());
        assert!(e.reconnect.is_none() && e.socket.is_none());
    }
    #[tokio::test]
    async fn latest_online_choice_consumes_an_unapplied_offline_fence() {
        let mut e = engine();
        let api = Arc::new(connection("http://127.0.0.1:1", true).unwrap());
        let session = api.test_session();
        e.account = Some(Account {
            api,
            session: session.clone(),
        });
        e.publish_access();
        let access = e.access.borrow().as_ref().unwrap().clone();
        e.mode.request(&access, true);
        assert!(!e.offline && session.is_offline());
        e.mode.request(&access, false);
        e.tick();
        assert!(e.offline && session.is_offline() && e.socket.is_none());
        let serial = e.reconnect.as_ref().unwrap().serial;
        e.reply(Reply::Reconnect {
            epoch: 7,
            serial,
            result: Ok(()),
        });
        assert!(!e.offline && !session.is_offline());
        // This current-thread test never yields after socket spawn.
        drop(e);
    }
    #[tokio::test]
    async fn failed_validation_keeps_local_queue_until_confirmed_same_account_return() {
        let mut e = engine();
        let api = Arc::new(connection("http://127.0.0.1:1", true).unwrap());
        let session = api.test_session();
        e.account = Some(Account {
            api,
            session: session.clone(),
        });
        e.command(Command::Offline(true).scoped(7));
        e.local.replace(
            vec![LocalTrack {
                id: "saved".into(),
                title: "Saved".into(),
                subtitle: String::new(),
                album_id: String::new(),
                artist_id: String::new(),
                duration: 20.0,
            }],
            0,
            false,
        );
        let token = e.room_token();
        let access = e.access.borrow().as_ref().unwrap().clone();
        e.mode.request(&access, false);
        e.tick();
        let serial = e.reconnect.as_ref().unwrap().serial;
        assert_eq!(e.room_token(), token);
        e.reply(Reply::Reconnect {
            epoch: 7,
            serial,
            result: Err(ApiError {
                message: "Forbidden",
                expired: false,
                status: 403,
            }),
        });
        assert_eq!(e.room_token(), token);
        assert_eq!(e.local.current().unwrap().track.id, "saved");
        assert!(session.is_offline() && e.socket.is_none());
        assert!(e.access.borrow().as_ref().unwrap().offline);
        e.mode.request(&access, false);
        e.tick();
        let serial = e.reconnect.as_ref().unwrap().serial;
        e.reply(Reply::Reconnect {
            epoch: 7,
            serial,
            result: Ok(()),
        });
        assert!(e.local.current().is_none() && !session.is_offline());
        assert!(!e.access.borrow().as_ref().unwrap().offline);
    }
    #[test]
    fn probe_delay_caps_without_a_retry_attempt_limit() {
        let delays: Vec<_> = std::iter::successors(Some(PROBE_FIRST_DELAY), |delay| {
            Some(next_probe_delay(*delay))
        })
        .take(12)
        .map(|delay| delay.as_millis())
        .collect();
        assert_eq!(
            delays,
            [
                500, 1_000, 2_000, 4_000, 8_000, 16_000, 30_000, 30_000, 30_000, 30_000, 30_000,
                30_000
            ]
        );
    }
    #[tokio::test]
    async fn stay_offline_cancels_probe_during_retry_delay() {
        let api = Arc::new(connection("http://127.0.0.1:1", true).unwrap());
        let session = api.test_session();
        let access = Access {
            epoch: 7,
            api,
            session,
            offline: true,
            recent_change: 0,
        };
        let mode = ModeControl::default();
        mode.request(&access, false);
        let mut serial = None;
        mode.apply(7, |_, id| serial = Some(id));
        let (tx, rx) = tokio::sync::oneshot::channel();
        let mut tx = Some(tx);
        let probe = reconnect_attempts(
            &mode,
            serial.unwrap(),
            || true,
            || {
                if let Some(tx) = tx.take() {
                    let _ = tx.send(());
                } else {
                    panic!("cancelled probe retried");
                }
                std::future::ready(Err(ApiError {
                    message: "Temporary",
                    expired: false,
                    status: 503,
                }))
            },
        );
        let work_offline = async {
            rx.await.unwrap();
            mode.request(&access, true);
        };
        let (result, ()) = tokio::join!(probe, work_offline);
        assert!(result.is_none());
    }
    #[test]
    fn stale_room_token_cannot_edit_a_different_displayed_queue() {
        let mut e = engine();
        let api = Arc::new(connection("http://127.0.0.1:1", true).unwrap());
        let session = api.test_session();
        e.account = Some(Account { api, session });
        e.command(Command::Offline(true).scoped(7));
        let tracks = ["a", "b", "c"]
            .into_iter()
            .map(|id| LocalTrack {
                id: id.into(),
                title: id.into(),
                subtitle: String::new(),
                album_id: String::new(),
                artist_id: String::new(),
                duration: 100.0,
            })
            .collect();
        assert_eq!(e.local.replace(tracks, 0, false), Change::Start);
        let shown = e.room_token().unwrap();
        let remove = e.local.entries[1].key;
        let move_key = e.local.entries[2].key;
        e.command(
            Command::Guarded(
                shown.clone(),
                Action::Move {
                    key: move_key,
                    after: None,
                },
            )
            .scoped(7),
        );
        assert_eq!(e.local.entries[1].key, move_key);
        e.command(Command::Guarded(shown.clone(), Action::Remove(remove)).scoped(7));
        assert_eq!(e.local.entries.len(), 3);
        e.command(Command::Guarded(e.room_token().unwrap(), Action::Remove(remove)).scoped(7));
        assert_eq!(e.local.entries.len(), 2);
        e.local.desired = false;
        e.command(Command::Guarded(shown, Action::Seek(35.0)).scoped(7));
        e.command(Command::Playback(Action::Seek(35.0)).scoped(7));
        assert_eq!(e.local.elapsed, 0.0);
        e.command(Command::Guarded(e.room_token().unwrap(), Action::Seek(35.0)).scoped(7));
        assert_eq!(e.local.elapsed, 35.0);
    }
    #[test]
    fn bounded_settlement_batch_is_feedback_only_and_can_settle_a_retired_link() {
        let mut e = engine();
        let api = Arc::new(connection("http://127.0.0.1:1", true).unwrap());
        let session = api.test_session();
        e.account = Some(Account { api, session });
        e.generation = Some(2);
        e.request_id = SETTLEMENT_BATCH as u64;
        let binding = Binding {
            instruction_id: "old-execution".into(),
            output_version: 2,
            event_id: "song".into(),
        };
        let grant = Authority::new(Instant::now() + Duration::from_secs(4));
        e.execution = Some(Execution {
            generation: 2,
            binding: binding.clone(),
            grant: Some(grant.clone()),
            status: Status::Ok,
            sent_at: None,
            ended: false,
        });
        let (tx, mut rx) = mpsc::channel(SETTLEMENT_BATCH + 1);
        for i in 1..=SETTLEMENT_BATCH {
            tx.try_send(socket::Event {
                generation: 1,
                kind: EventKind::CommandUncertain {
                    request_id: i as u64,
                },
            })
            .ok()
            .unwrap();
        }
        tx.try_send(socket::Event {
            generation: 2,
            kind: EventKind::LeaseExpired(binding),
        })
        .ok()
        .unwrap();
        let batch = settlement_batch(&mut rx);
        assert_eq!(batch.len(), SETTLEMENT_BATCH);
        for event in batch {
            e.settlement(event);
        }
        assert_eq!(e.action_status, "Playback action unconfirmed.");
        assert_eq!(rx.len(), 1);
        for event in settlement_batch(&mut rx) {
            e.settlement(event);
        }
        assert!(grant.is_valid() && e.execution.is_some());
        e.shared_epoch.store(8, Ordering::SeqCst);
        e.settlement(socket::Event {
            generation: 2,
            kind: EventKind::CommandNotSent {
                request_id: e.request_id,
                error: socket::SocketError::NotConnected,
            },
        });
        assert_eq!(e.action_status, "Playback action unconfirmed.");
    }
    #[tokio::test]
    async fn fresh_same_output_resume_admits_media_despite_older_local_pause_settlement() {
        use serde_json::{from_value, json};
        fn instruction(id: &str, wants_play: bool, sequence: u64) -> Instruction {
            from_value(json!({"id":id,"type":"execute","outputVersion":3,"eventId":"song-event",
                "elapsed":2.0,"desiredPlaying":wants_play,"lease":{"clientSequence":sequence,"durationMs":8000},
                "track":{"id":"song","name":"Song","artists":[],"albumName":"","albumId":"",
                    "duration":30.0,"key":1,"order":1,"intent":"now"}})).unwrap()
        }
        for (outcome, execution, code, receipt_first) in [
            ("accepted", "confirmed", "", true),
            ("rejected", "unchanged", "invalid", true),
            ("accepted", "superseded", "superseded", false),
        ] {
            let mut e = engine();
            let api = Arc::new(connection("http://127.0.0.1:1", true).unwrap());
            let session = api.test_session();
            e.account = Some(Account {
                api,
                session: session.clone(),
            });
            e.audio = Some(Audio::new(
                Some(session),
                false,
                e.shared_epoch.clone(),
                e.epoch,
            ));
            e.generation = Some(2);
            e.request_id = 7;
            // Inject a test output link at the already-validated socket boundary:
            // the frame, matching generation, and live grant are all required.
            let (_link, rx) = watch::channel(Link::Connected(2));
            let deadline = Instant::now() + Duration::from_secs(5);
            e.admit_instruction(
                2,
                instruction("interrupted", true, 4),
                Some(deadline),
                false,
                Some(rx.clone()),
            );
            let old_operation = e.operation;
            let old_grant = e.pending.as_ref().unwrap().grant.as_ref().unwrap().clone();
            e.pause_local();
            assert!(!old_grant.is_valid() && e.pending.is_none());
            e.admit_instruction(
                2,
                instruction("buffered-before-pause", true, 4),
                Some(deadline),
                true,
                Some(rx.clone()),
            );
            e.admit_instruction(
                2,
                instruction("interrupted", true, 4),
                Some(deadline),
                false,
                Some(rx.clone()),
            );
            assert!(e.pending.is_none());
            let receipt = socket::Event {
                generation: 2,
                kind: EventKind::Result {
                    request_id: 7,
                    result: from_value(
                        json!({"id":"7","outcome":outcome,"execution":execution,"code":code,
                    "stateVersion":9,"acceptedCount":0,"placement":"none","final":true}),
                    )
                    .unwrap(),
                },
            };
            let mut receipt = Some(receipt);
            if receipt_first {
                e.settlement(receipt.take().unwrap());
            }
            e.admit_instruction(
                2,
                instruction("other-controller-resume", true, 5),
                Some(deadline),
                false,
                Some(rx),
            );
            if let Some(receipt) = receipt {
                e.settlement(receipt);
            }
            e.reply(Reply::Media {
                epoch: 7,
                operation: old_operation,
                result: Ok(vec![1]),
            });
            let pending = e.pending.as_ref().unwrap();
            assert_eq!(pending.instruction.id, "other-controller-resume");
            assert!(pending.grant.as_ref().unwrap().is_valid() && e.pause_fence.is_none());
            e.cancel_work();
        }
    }
    #[test]
    fn canonical_http_requires_lan_consent() {
        assert!(connection("HTTP://127.0.0.1:1", false).is_err());
        assert!(connection("HTTP://127.0.0.1:1", true).is_ok());
    }
}
