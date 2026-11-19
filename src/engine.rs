//! Account-state.v1 controller, output executor, and independent saved queue.
#[path = "engine_online.rs"]
mod online;
#[path = "engine_output.rs"]
mod output;

use crate::{
    AppWindow, ArtistCredit, DeviceRow, QueueRow,
    api::{Api, ApiError, Item, Session},
    audio::Audio,
    auth::{self, Kind, Success, Write, Writer},
    desk::Access,
    mode::ModeControl,
    offline::{self, OfflineStore, PlaybackInput},
    personal::Command as PersonalCommand,
    personal_bridge::PersonalHub,
    player::{Action, Authority, Change, LocalQueue, LocalTrack, Status},
    socket::{self, Link},
};
use online::Online;
use slint::{Model, ModelRc, VecModel};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    sync::{Semaphore, mpsc, watch},
    task::JoinHandle,
};

// Auth intents must not compete for admission with the bounded playback queue.
pub enum AuthCommand {
    Login {
        epoch: u64,
        origin: String,
        username: String,
        password: String,
        remember: bool,
    },
    Retry {
        epoch: u64,
        origin: String,
        username: String,
    },
    Accept(u64),
    Cancel(u64),
    Logout(u64),
    Shutdown,
}

pub enum Command {
    Scoped(u64, Box<Command>),
    Toggle,
    Transport(&'static str),
    Playback(Action),
    Personal {
        epoch: u64,
        generation: u64,
        command: PersonalCommand,
    },
    Guarded(String, Action),
    #[cfg(test)]
    Offline(bool),
    PlayOffline {
        queue: Vec<Item>,
        index: usize,
        input: PlaybackInput,
        store: OfflineStore,
    },
}
impl Command {
    pub fn scoped(self, epoch: u64) -> Self {
        Self::Scoped(epoch, Box::new(self))
    }
}

const REPORT_INTERVAL: Duration = Duration::from_secs(2);
const LOAD_BUDGET: Duration = Duration::from_secs(5);
const TICK: Duration = Duration::from_millis(25);
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const PROBE_FIRST_DELAY: Duration = Duration::from_millis(500);
const PROBE_MAX_DELAY: Duration = Duration::from_secs(30);
const PROMOTION_TIMEOUT: Duration = Duration::from_secs(10);

struct OutputPromotion {
    deadline: Instant,
    command: socket::Command,
    resume_key: Option<u64>,
    // Explicit Play can arrive before the first passive room snapshot.
    awaiting_room: bool,
}

struct Account {
    api: Arc<Api>,
    session: Session,
}
struct Reconnect {
    serial: u64,
    task: JoinHandle<()>,
}
enum Reply {
    Prefill(u64, String, String),
    Login(u64, auth::Outcome),
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
    auth_ready: Option<(u64, Success)>,
    auth_tx: Option<mpsc::UnboundedSender<AuthCommand>>,
    auth_writer: Option<Writer>,
    retained: Option<(Arc<AtomicBool>, Option<crate::login_store::LoginProfile>)>,
    reconnect: Option<Reconnect>,
    logouts: Vec<JoinHandle<()>>,
    account: Option<Account>,
    offline: bool,
    passive_output: bool,
    output_promotion: Option<OutputPromotion>,
    audio: Option<Audio>,
    local: LocalQueue,
    local_store: Option<OfflineStore>,
    local_read_slot: Arc<Semaphore>,
    local_status: Status,
    socket: Option<socket::Socket>,
    online: Online,
    personal: PersonalHub,
    media: Option<JoinHandle<()>>,
    operation: u64,
    room_incarnation: u64,
    local_revision: u64,
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
        self.online_generation()?;
        self.online
            .state
            .as_ref()
            .map(|_| format!("{}:{}", self.room_incarnation, self.online.revision))
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
                personal: self.personal.access(),
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
                    ui.set_restoring_login(false);
                    ui.set_error(error);
                    ui.set_status(status.into());
                }
            }
        });
    }
    fn cancel_login(&mut self) {
        if let Some(task) = self.login.take() {
            task.abort();
            self.logouts.push(task);
        }
    }
    fn revoke(&mut self, a: Account) {
        self.logouts.push(tokio::spawn(async move {
            // Even offline, logout revokes every clone without creating a request.
            let _ = a.api.logout(&a.session).await;
        }));
    }
    fn reset(&mut self, epoch: u64) {
        self.cancel_reconnect();
        self.cancel_login();
        self.clear_output();
        self.passive_output = false;
        self.retained = None;
        if let Some(a) = self.account.take() {
            self.revoke(a);
        }
        self.logouts.retain(|task| !task.is_finished());
        self.epoch = epoch;
        self.offline = false;
        self.personal.reset(epoch);
        self.publish_access();
    }
    async fn shutdown(&mut self) {
        let epoch = self
            .shared_epoch
            .fetch_add(1, Ordering::SeqCst)
            .wrapping_add(1);
        let account = self.account.take();
        let retained = self.retained.take();
        self.retire_pending(false);
        self.reset(epoch);
        if let Some(writer) = self.auth_writer.take() {
            // Complete accepted save/logout intent before deciding whether to
            // revoke the server session. A failed opt-in is not remembered.
            writer.finish().await;
        }
        if let Some(a) = account {
            if retained.is_some_and(|(keep, _)| keep.load(Ordering::Acquire)) {
                // Api::logout will only revoke local clones under this fence;
                // the keyring credential remains usable on the next launch.
                a.session.set_offline(true);
            }
            self.revoke(a);
        }
        for task in self.logouts.drain(..) {
            let _ = task.await;
        }
    }
    fn clear_output(&mut self) {
        if self.output_promotion.take().is_some() {
            self.passive_output = true;
        }
        self.room_incarnation = self.room_incarnation.wrapping_add(1);
        self.local_revision = 0;
        self.cancel_work();
        self.stop();
        self.audio = None;
        self.disconnect_personal();
        self.socket = None;
        self.online = Online::default();
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
                    can_play: !self.passive_output,
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
        // The native source and API consult this same immediate mode fence.
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
        self.cancel_login();
        self.retire_pending(false);
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
        if let Some((_, profile)) = self.retained.take() {
            self.write_auth(self.epoch, Write::Forget(profile));
        }
        self.reset(self.epoch);
        self.offline = offline;
        self.login_view(false, true, "Session expired. Sign in again.");
    }
    fn write_auth(&self, epoch: u64, write: Write) {
        if let Some(writer) = &self.auth_writer {
            writer.send(epoch, write);
        }
    }
    fn start_restore(&mut self, scope: Option<(String, String)>) {
        let (epoch, current, results) =
            (self.epoch, self.shared_epoch.clone(), self.results.clone());
        self.login = Some(tokio::spawn(async move {
            let prefill = results.clone();
            let outcome =
                auth::restore(scope, epoch, &current, move |origin, username| async move {
                    let _ = prefill.send(Reply::Prefill(epoch, origin, username)).await;
                })
                .await;
            let _ = results.send(Reply::Login(epoch, outcome)).await;
        }));
    }
    fn retire_pending(&mut self, logout: bool) {
        if let Some((
            _,
            Success {
                api, session, kind, ..
            },
        )) = self.auth_ready.take()
            && (logout || matches!(kind, Kind::Password { .. }))
        {
            self.revoke(Account { api, session });
        }
    }
    fn auth_command(&mut self, command: AuthCommand) -> bool {
        match command {
            AuthCommand::Shutdown => return false,
            AuthCommand::Accept(epoch) => self.accept_login(epoch),
            AuthCommand::Logout(epoch) => {
                // This intent must not get skipped if another auth intent is
                // already queued. The persistence worker orders it with saves.
                let offline = self.explicit_offline();
                self.retire_pending(true);
                self.write_auth(epoch, Write::Forget(None));
                if epoch == self.shared_epoch.load(Ordering::SeqCst) {
                    self.reset(epoch);
                    self.offline = offline;
                    self.login_view(false, false, "Signed out on this desktop.");
                }
            }
            AuthCommand::Cancel(epoch) if epoch == self.shared_epoch.load(Ordering::SeqCst) => {
                self.retire_pending(false);
                let offline = self.explicit_offline();
                self.reset(epoch);
                self.offline = offline;
                self.login_view(false, false, "Sign-in canceled.");
            }
            AuthCommand::Retry {
                epoch,
                origin,
                username,
            } if epoch == self.shared_epoch.load(Ordering::SeqCst) => {
                self.retire_pending(false);
                self.reset(epoch);
                self.start_restore(Some((origin, username)));
            }
            AuthCommand::Login {
                epoch,
                origin,
                username,
                password,
                remember,
            } if epoch == self.shared_epoch.load(Ordering::SeqCst) => {
                self.retire_pending(false);
                // Explicit sign-in goes online; retire the old account first.
                self.reset(epoch);
                let (current, results) = (self.shared_epoch.clone(), self.results.clone());
                self.login = Some(tokio::spawn(async move {
                    if current.load(Ordering::SeqCst) != epoch {
                        return;
                    }
                    let outcome = auth::password(&origin, &username, &password, remember).await;
                    drop(password);
                    let _ = results.send(Reply::Login(epoch, outcome)).await;
                }));
            }
            _ => {}
        }
        true
    }
    fn command(&mut self, command: Command) {
        match command {
            Command::Personal {
                epoch,
                generation,
                command,
            } => {
                self.apply_mode();
                self.send_personal(epoch, generation, &command);
            }
            Command::Scoped(epoch, command) if epoch == self.epoch && self.current() => {
                self.apply_mode();
                self.scoped(*command)
            }
            _ => {}
        }
    }
    fn scoped(&mut self, command: Command) {
        if !self.offline {
            // Resolve buffered canonical changes before comparing a displayed
            // queue token or deciding whether the transport button is Pause.
            self.drain_online_events(None);
        }
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
                        album_name: item.album_name,
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
                    self.action(action);
                }
            }
            Command::Playback(action) if !action.depends_on_visible_queue() => self.action(action),
            Command::Toggle => {
                let desired = if self.offline {
                    self.local.desired
                } else {
                    self.online.wants_pause()
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
        self.online_action(action);
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
    fn prepare_login(&mut self, success: Success) {
        let (weak, shared, epoch) = (self.window.clone(), self.shared_epoch.clone(), self.epoch);
        let Some(auth_tx) = self.auth_tx.clone() else {
            return;
        };
        let origin = success.origin.as_str().to_owned();
        let username = success.session.username.clone();
        self.auth_ready = Some((epoch, success));
        // UI admission serializes completion with Cancel and edits. An engine
        // result by itself has no authority to publish access or save a login.
        let _ = slint::invoke_from_event_loop(move || {
            if shared.load(Ordering::SeqCst) == epoch
                && let Some(ui) = weak.upgrade()
                && auth_tx.send(AuthCommand::Accept(epoch)).is_ok()
            {
                ui.set_origin(origin.into());
                ui.set_username(username.into());
                ui.set_password("".into());
                ui.set_keep_signed_in(false);
                ui.set_can_retry_session(false);
                ui.set_busy(false);
                ui.set_restoring_login(false);
                ui.set_signed_in(true);
            }
        });
    }
    fn accept_login(&mut self, epoch: u64) {
        if !self.current()
            || epoch != self.epoch
            || self.offline
            || self
                .auth_ready
                .as_ref()
                .is_none_or(|(ready, _)| *ready != epoch)
        {
            return;
        }
        let (
            _,
            Success {
                api,
                session,
                origin,
                kind,
            },
        ) = self.auth_ready.take().unwrap();
        self.passive_output = matches!(&kind, Kind::Restored(_));
        self.retained = match kind {
            Kind::Restored(profile) => Some((Arc::new(AtomicBool::new(true)), Some(profile))),
            Kind::Password { remember } => {
                let retained = Arc::new(AtomicBool::new(false));
                let credential = remember
                    .then(|| api.remembered_credential(&session).ok())
                    .flatten();
                self.write_auth(
                    epoch,
                    Write::Record {
                        origin,
                        username: session.username.clone(),
                        wanted: remember,
                        credential,
                        retained: retained.clone(),
                    },
                );
                remember.then_some((retained, None))
            }
        };
        self.account = Some(Account { api, session });
        // A restored account joins as a fresh, passive controller. No output
        // capability, ownership choice or playback action is sent on restore.
        self.device_id = uuid::Uuid::new_v4().to_string();
        self.publish_access();
        self.connect();
    }
    fn finish_login(&self, fallback: auth::Fallback) {
        if let Some(profile) = fallback.discard {
            self.write_auth(self.epoch, Write::Forget(Some(profile)));
        }
        let (weak, shared, epoch) = (self.window.clone(), self.shared_epoch.clone(), self.epoch);
        let _ = slint::invoke_from_event_loop(move || {
            if shared.load(Ordering::SeqCst) == epoch
                && let Some(ui) = weak.upgrade()
            {
                ui.set_busy(false);
                ui.set_restoring_login(false);
                ui.set_error(!fallback.status.is_empty());
                ui.set_status(fallback.status.into());
                ui.set_auth_notice(fallback.notice.into());
                ui.set_can_retry_session(fallback.retry);
            }
        });
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
            Reply::Prefill(epoch, origin, username)
                if epoch == self.epoch && !self.offline && self.login.is_some() =>
            {
                let (weak, shared) = (self.window.clone(), self.shared_epoch.clone());
                let _ = slint::invoke_from_event_loop(move || {
                    if shared.load(Ordering::SeqCst) == epoch
                        && let Some(ui) = weak.upgrade()
                    {
                        ui.set_origin(origin.into());
                        ui.set_username(username.into());
                    }
                });
            }
            Reply::Login(epoch, outcome)
                if epoch == self.epoch && !self.offline && self.login.is_some() =>
            {
                self.login = None;
                match outcome {
                    Ok(success) => self.prepare_login(success),
                    Err(fallback) => self.finish_login(fallback),
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
                self.complete_online(result);
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
        self.online.work = None;
    }
    fn stop(&mut self) {
        self.online.execution = None;
        if let Some(audio) = &mut self.audio {
            audio.stop();
        }
    }
    fn tick(&mut self) {
        self.tick_with_event(None);
    }
    fn explicit_offline(&self) -> bool {
        self.offline
            || self
                .account
                .as_ref()
                .is_some_and(|a| a.session.is_offline())
    }
    fn tick_with_event(&mut self, event: Option<socket::Event>) {
        let requested_epoch = self.shared_epoch.load(Ordering::SeqCst);
        if self.epoch != requested_epoch {
            let offline = self.explicit_offline();
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
        self.tick_online(event);
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
                p.album_name = entry.track.album_name.clone();
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
                .map(|(i, e)| QueueEntryView {
                    key: e.key.to_string(),
                    title: e.track.title.clone(),
                    subtitle: e.track.subtitle.clone(),
                    album_name: e.track.album_name.clone(),
                    current: self.local.position == Some(i),
                    upcoming: self.local.position.is_some_and(|pos| i > pos),
                    ..QueueEntryView::default()
                })
                .collect();
        } else {
            self.project_online(&mut p);
        }
        p.publish(self.window.clone(), self.shared_epoch.clone(), self.epoch);
    }
}

#[derive(Debug, PartialEq, Eq)]
struct CreditView {
    id: String,
    name: String,
}

fn credits_match(credits: &[CreditView], model: &ModelRc<ArtistCredit>) -> bool {
    model.row_count() == credits.len()
        && credits
            .iter()
            .zip(model.iter())
            .all(|(credit, row)| credit.id == row.id.as_str() && credit.name == row.name.as_str())
}

fn credit_model(credits: Vec<CreditView>) -> ModelRc<ArtistCredit> {
    if credits.is_empty() {
        return ModelRc::default();
    }
    ModelRc::new(VecModel::from(
        credits
            .into_iter()
            .map(|a| ArtistCredit {
                id: a.id.into(),
                name: a.name.into(),
            })
            .collect::<Vec<_>>(),
    ))
}

#[derive(Default)]
struct QueueEntryView {
    key: String,
    title: String,
    subtitle: String,
    credits: Vec<CreditView>,
    album_id: String,
    album_name: String,
    current: bool,
    upcoming: bool,
}

impl QueueEntryView {
    fn matches(&self, row: &QueueRow) -> bool {
        self.key == row.key.as_str()
            && self.title == row.title.as_str()
            && self.subtitle == row.subtitle.as_str()
            && self.album_id == row.album_id.as_str()
            && self.album_name == row.album_name.as_str()
            && self.current == row.current
            && self.upcoming == row.upcoming
            && credits_match(&self.credits, &row.credits)
    }

    fn into_row(self) -> QueueRow {
        QueueRow {
            key: self.key.into(),
            title: self.title.into(),
            subtitle: self.subtitle.into(),
            credits: credit_model(self.credits),
            album_id: self.album_id.into(),
            album_name: self.album_name.into(),
            current: self.current,
            upcoming: self.upcoming,
        }
    }
}

// Only owned, Send data is captured for the event-loop hop. Construct the
// nested Slint models on the UI thread, and only if their content changed.
#[derive(Default)]
struct PlayerView {
    offline: bool,
    reconnecting: bool,
    shuffle: bool,
    repeat: &'static str,
    status: &'static str,
    room_token: String,
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
    album_name: String,
    credits: Vec<CreditView>,
    context_kind: &'static str,
    context_id: String,
    context_name: String,
    queue: Vec<QueueEntryView>,
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
            ui.set_now_album_name(self.album_name.into());
            if !credits_match(&self.credits, &ui.get_now_credits()) {
                ui.set_now_credits(credit_model(self.credits));
            }
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
            ui.set_player_status(self.status.into());
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
            let upcoming = self.queue.iter().find(|r| r.upcoming);
            ui.set_next_title(
                upcoming
                    .map(|r| r.title.as_str().into())
                    .unwrap_or_default(),
            );
            ui.set_next_subtitle(
                upcoming
                    .map(|r| r.subtitle.as_str().into())
                    .unwrap_or_default(),
            );
            let queue = ui.get_queue_rows();
            if queue.row_count() != self.queue.len()
                || !self
                    .queue
                    .iter()
                    .zip(queue.iter())
                    .all(|(entry, row)| entry.matches(&row))
            {
                ui.set_queue_rows(ModelRc::new(VecModel::from(
                    self.queue
                        .into_iter()
                        .map(QueueEntryView::into_row)
                        .collect::<Vec<_>>(),
                )));
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
async fn socket_signal(socket: Option<&mut socket::Socket>) -> Option<socket::Event> {
    if let Some(socket) = socket {
        tokio::select! {
            Ok(_) = socket.link.changed() => return None,
            Ok(_) = socket.authority.changed() => return None,
            Some(event) = socket.events.recv() => return Some(event),
            else => {}
        }
    }
    std::future::pending().await
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
fn metadata_id(id: &str) -> &str {
    // The socket allows blank metadata. Only expose IDs the catalog can open.
    if id.trim() != id || matches!(id, "." | "..") {
        ""
    } else {
        id
    }
}
fn credit_views(artists: &[socket::Artist]) -> Vec<CreditView> {
    artists
        .iter()
        .filter_map(|a| {
            let id = metadata_id(a.id.as_deref().unwrap_or_default());
            let name = a.name.trim();
            (!id.is_empty() || !name.is_empty()).then(|| CreditView {
                id: id.into(),
                name: name.into(),
            })
        })
        .collect()
}
fn artist_subtitle(credits: &[CreditView]) -> String {
    credits
        .iter()
        .map(|a| a.name.as_str())
        .filter(|name| !name.is_empty())
        .collect::<Vec<_>>()
        .join(", ")
}
#[cfg(test)]
fn connection(input: &str) -> Result<Api, ApiError> {
    Api::new(crate::api::Origin::parse(input)?)
}

pub fn start(
    window: slint::Weak<AppWindow>,
    epoch: Arc<AtomicU64>,
    access: watch::Sender<Option<Access>>,
    mode: ModeControl,
    mut auth_intents: mpsc::UnboundedReceiver<AuthCommand>,
    auth_tx: mpsc::UnboundedSender<AuthCommand>,
) -> (mpsc::Sender<Command>, std::thread::JoinHandle<()>) {
    let (tx, mut commands) = mpsc::channel(32);
    let worker = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("playback runtime");
        runtime.block_on(async move {
            let (results, mut replies) = mpsc::channel(16);
            let (weak, shared) = (window.clone(), epoch.clone());
            let writer = Writer::new(move |epoch, notice| {
                let (weak, shared) = (weak.clone(), shared.clone());
                let _ = slint::invoke_from_event_loop(move || {
                    if shared.load(Ordering::SeqCst) == epoch
                        && let Some(ui) = weak.upgrade()
                    {
                        ui.set_auth_notice(notice.into());
                    }
                });
            });
            let mut e = Engine {
                epoch: 0,
                shared_epoch: epoch,
                mode,
                access,
                results,
                window,
                device_id: uuid::Uuid::new_v4().to_string(),
                login: None,
                auth_ready: None,
                auth_tx: Some(auth_tx),
                auth_writer: Some(writer),
                retained: None,
                reconnect: None,
                logouts: vec![],
                account: None,
                offline: false,
                passive_output: false,
                output_promotion: None,
                audio: None,
                local: LocalQueue::default(),
                local_status: Status::Ok,
                local_store: None,
                local_read_slot: Arc::new(Semaphore::new(1)),
                socket: None,
                online: Online::default(),
                personal: PersonalHub::default(),
                media: None,
                operation: 0,
                room_incarnation: 0,
                local_revision: 0,
                action_status: "",
            };
            e.start_restore(None);
            let mut cadence = tokio::time::interval(TICK);
            cadence.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut refresh = 0;
            loop {
                tokio::select! {
                    intent = auth_intents.recv() => {
                        if !e.auth_command(intent.unwrap_or(AuthCommand::Shutdown)) { break; }
                        e.publish();
                    }
                    command = commands.recv() => {
                        let Some(command) = command else { break; };
                        e.command(command);
                        e.publish();
                    }
                    Some(reply) = replies.recv() => { e.reply(reply); e.publish(); },
                    event = socket_signal(e.socket.as_mut()) => { e.tick_with_event(event); e.publish(); },
                    _ = cadence.tick() => {
                        e.tick();
                        refresh = (refresh + 1) % 5;
                        if refresh == 0 { e.publish(); }
                    }
                }
            }
            e.shutdown().await;
        });
        runtime.shutdown_timeout(Duration::from_secs(2));
    });
    (tx, worker)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credit_projection_keeps_order_and_never_invents_ids() {
        let credits = credit_views(&[
            socket::Artist {
                id: Some("artist/1".into()),
                name: "First".into(),
            },
            socket::Artist {
                id: None,
                name: "Name only".into(),
            },
            socket::Artist {
                id: Some(" ".into()),
                name: "Also name only".into(),
            },
            socket::Artist {
                id: Some("artist/2".into()),
                name: "Last".into(),
            },
        ]);
        assert_eq!(
            credits.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(),
            ["artist/1", "", "", "artist/2"],
        );
        assert_eq!(
            artist_subtitle(&credits),
            "First, Name only, Also name only, Last"
        );
    }

    #[test]
    fn queue_projection_detects_album_and_credit_only_changes() {
        let entry = || QueueEntryView {
            key: "7".into(),
            title: "Track".into(),
            subtitle: "Artist".into(),
            credits: vec![CreditView {
                id: "artist/1".into(),
                name: "Artist".into(),
            }],
            album_id: "album/1".into(),
            album_name: "Album".into(),
            upcoming: true,
            ..QueueEntryView::default()
        };
        let row = entry().into_row();
        let mut next = entry();
        assert!(next.matches(&row));
        next.credits[0].id = "artist/2".into();
        assert!(!next.matches(&row));
        let mut next = entry();
        next.album_id = "album/2".into();
        assert!(!next.matches(&row));
        let mut next = entry();
        next.album_name = "New album name".into();
        assert!(!next.matches(&row));
    }

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
            auth_ready: None,
            auth_tx: None,
            auth_writer: None,
            retained: None,
            reconnect: None,
            logouts: vec![],
            account: None,
            offline: false,
            passive_output: false,
            output_promotion: None,
            audio: None,
            local: LocalQueue::default(),
            local_status: Status::Ok,
            socket: None,
            local_store: None,
            local_read_slot: Arc::new(Semaphore::new(1)),
            online: Online::default(),
            personal: PersonalHub::default(),
            media: None,
            operation: 0,
            room_incarnation: 0,
            local_revision: 0,
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
        let api = Arc::new(connection("http://127.0.0.1:1").unwrap());
        let session = api.test_session();
        e.account = Some(Account {
            api,
            session: session.clone(),
        });
        e.command(Command::Offline(true).scoped(7));
        assert!(session.is_offline());
        assert!(e.access.borrow().as_ref().unwrap().offline);
        assert!(e.socket.is_none());
        e.shutdown().await;
        assert!(e.access.borrow().is_none());
        assert!(session.is_expired());
    }
    #[tokio::test]
    async fn remembered_shutdown_does_not_revoke_on_server() {
        let mut e = engine();
        let api = Arc::new(connection("http://127.0.0.1:1").unwrap());
        let session = api.test_session();
        e.account = Some(Account {
            api,
            session: session.clone(),
        });
        e.retained = Some((Arc::new(AtomicBool::new(true)), None));
        e.publish_access();
        e.shutdown().await;
        assert!(e.access.borrow().is_none());
        assert!(session.is_offline()); // Api::logout must not reach the server
        assert!(!session.can_access_local());
    }
    #[tokio::test]
    async fn old_auth_completion_cannot_publish_access_after_cancel() {
        let mut e = engine();
        let origin = crate::api::Origin::parse("http://127.0.0.1:1").unwrap();
        let api = Arc::new(Api::new(origin.clone()).unwrap());
        let session = api.test_session();
        session.set_offline(true); // synthetic capability; cancellation cannot contact a server
        e.auth_ready = Some((
            7,
            Success {
                api,
                session: session.clone(),
                origin,
                kind: Kind::Password { remember: true },
            },
        ));
        e.shared_epoch.store(8, Ordering::SeqCst);
        e.auth_command(AuthCommand::Accept(7));
        assert!(e.account.is_none() && e.auth_ready.is_some());
        e.auth_command(AuthCommand::Cancel(8));
        e.auth_command(AuthCommand::Accept(7));
        assert!(e.auth_ready.is_none() && e.access.borrow().is_none());
        assert!(e.retained.is_none() && e.socket.is_none());
        e.shutdown().await;
        assert!(!session.can_access_local());
    }
    #[tokio::test]
    async fn cookie_expiry_keeps_local_access_but_go_online_requires_sign_in() {
        let mut e = engine();
        let api = Arc::new(connection("http://127.0.0.1:1").unwrap());
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
    #[tokio::test]
    async fn explicit_sign_in_leaves_offline_even_if_the_new_origin_is_invalid() {
        let mut e = engine();
        e.command(Command::Offline(true).scoped(7));
        e.auth_command(AuthCommand::Logout(7));
        assert!(e.offline);
        e.shared_epoch.store(8, Ordering::SeqCst);
        e.auth_command(AuthCommand::Login {
            epoch: 8,
            origin: "not an origin".into(),
            username: "test".into(),
            password: "test".into(),
            remember: false,
        });
        assert!(!e.offline && e.epoch == 8 && e.socket.is_none());
    }
    #[tokio::test]
    async fn shared_fences_retire_even_if_mode_or_logout_command_never_arrives() {
        let mut e = engine();
        let api = Arc::new(connection("http://127.0.0.1:1").unwrap());
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
        let api = Arc::new(connection("http://127.0.0.1:1").unwrap());
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
        let api = Arc::new(connection("http://127.0.0.1:1").unwrap());
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
        let api = Arc::new(connection("http://127.0.0.1:1").unwrap());
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
                album_name: String::new(),
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
        let api = Arc::new(connection("http://127.0.0.1:1").unwrap());
        let session = api.test_session();
        let access = Access {
            epoch: 7,
            api,
            session,
            offline: true,
            personal: PersonalHub::default().access(),
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
}
