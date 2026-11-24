//! Desktop playback actor: independent local playback and the v3 account room.
#[cfg(test)]
#[path = "engine_cross_native_tests.rs"]
mod cross_native_tests;
#[path = "engine_crossfade.rs"]
mod crossfade;
#[path = "engine_native_v3.rs"]
mod native;
#[path = "engine_online.rs"]
mod online;

use crate::{
    AppWindow, ArtistCredit, DeviceRow, QueueRow,
    api::{Api, ApiError, Session},
    audio::{Audio, MasterGain},
    auth::{self, Kind, Retention, Success, Write, Writer},
    desk::Access,
    login_store::{LoginProfile, LoginStore, StoreError},
    mode::ModeControl,
    offline::{self, OfflineStore, PlaybackInput},
    output_identity::Installation,
    personal::Command as PersonalCommand,
    personal_bridge::{PersonalHub, Status as PersonalStatus},
    personal_cache::Listening,
    player::{self, Action, Authority, Change, LocalQueue, LocalTrack, Status},
    quick_add::Ticket as QuickAddTicket,
    socket::{self, Link},
};
use online::Online;
use slint::{Model, ModelRc, VecModel};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    sync::{Semaphore, mpsc, watch},
    task::JoinHandle,
    time::Instant,
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
    Accept(u64),
    Cancel(u64),
    GoOnline(u64),
    Logout(u64),
    Shutdown,
}

pub enum Command {
    Scoped(u64, Box<Command>),
    Playback(Action),
    Personal {
        epoch: u64,
        generation: u64,
        command: PersonalCommand,
    },
    QuickAdd(QuickAddTicket),
    ReorderPlaylist {
        observed: PersonalStatus,
        mode_stamp: u64,
        command: PersonalCommand,
    },
    Guarded(String, Action),
    SeekLyric {
        room: String,
        snapshot: crate::presentation::LyricSnapshot,
        start_ms: i32,
    },
    ReorderQueue {
        basis: String,
        key: u64,
        after: Option<u64>,
    },
    GuardedToggle(String),
    #[cfg(test)]
    Offline(bool),
    PlaySaved {
        mode_stamp: u64,
        queue: Vec<LocalTrack>,
        index: usize,
        input: PlaybackInput,
        store: OfflineStore,
    },
    QueueSaved {
        mode_stamp: u64,
        queue: Vec<LocalTrack>,
        store: OfflineStore,
    },
}
impl Command {
    pub fn scoped(self, epoch: u64) -> Self {
        Self::Scoped(epoch, Box::new(self))
    }
}

const TICK: Duration = Duration::from_millis(25);
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const AUTH_ATTEMPTS: u8 = 4;
const LYRIC_SEEK_LEAD_MS: i32 = 250;

fn lyric_seek_before_ms(duration: f64) -> i32 {
    if !crate::player::seconds(duration) || duration == 0.0 {
        return 0;
    }
    // Exclusive upper bound; compare in seconds to avoid rounding the authored
    // timestamp or accepting a line exactly at the end of the recording.
    let floor_ms = (duration * 1_000.0) as i32;
    floor_ms + i32::from(f64::from(floor_ms) / 1_000.0 < duration)
}

fn lyric_seek_seconds(start_ms: i32, before_ms: i32) -> Option<f64> {
    (0..before_ms)
        .contains(&start_ms)
        .then(|| f64::from(start_ms.saturating_sub(LYRIC_SEEK_LEAD_MS).max(0)) / 1_000.0)
}

struct Account {
    api: Arc<Api>,
    session: Session,
}
enum Rejoined {
    Existing,
    Cold(Account, LoginProfile),
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
        result: Result<Rejoined, ApiError>,
    },
    Media {
        epoch: u64,
        request: native::MediaRequest,
        source: Option<crate::audio_input::AudioInput>,
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
    #[cfg(test)]
    manual_mode_path: Option<std::path::PathBuf>,
    access: watch::Sender<Option<Access>>,
    results: mpsc::Sender<Reply>,
    window: slint::Weak<AppWindow>,
    device_id: String,
    login: Option<JoinHandle<()>>,
    auth_ready: Option<(u64, Success)>,
    auth_tx: Option<mpsc::UnboundedSender<AuthCommand>>,
    auth_writer: Option<Writer>,
    login_store: Result<Arc<LoginStore>, StoreError>,
    retained: Option<Retention>,
    reconnect: Option<Reconnect>,
    recover_at: Option<Instant>,
    listening: Listening,
    saved_skip: usize,
    logouts: Vec<JoinHandle<()>>,
    account: Option<Account>,
    offline: bool,
    audio: Option<Audio>,
    master_gain: MasterGain,
    local: LocalQueue,
    local_store: Option<OfflineStore>,
    crossfade_enabled: Arc<AtomicBool>,
    local_lookahead: Option<crossfade::Lookahead<(u64, u64)>>,
    local_read_slot: Arc<Semaphore>,
    local_preview_slot: Arc<Semaphore>,
    local_status: Status,
    socket: Option<socket::Socket>,
    online: Online,
    personal: PersonalHub,
    media: Option<JoinHandle<()>>,
    operation: u64,
    room_incarnation: u64,
    local_revision: u64,
    local_queue_instance: u64,
    action_status: &'static str,
    // Declared after the socket and audio: their teardown precedes unlock.
    installation: Option<Installation>,
}
impl Engine {
    fn current(&self) -> bool {
        self.epoch == self.shared_epoch.load(Ordering::SeqCst)
    }
    fn private_audio(&self) -> bool {
        self.local_store.is_some() || self.online.detached_owner
    }
    fn local_transport(&self) -> bool {
        self.private_audio() || self.online_generation().is_none()
    }
    fn room_token(&self) -> Option<String> {
        if !self.current() || !self.account.as_ref()?.session.can_access_local() {
            return None;
        }
        if self.local_transport() {
            if self.local_store.is_some() || !self.online.detached {
                return Some(format!(
                    "local:{}:{}:{}",
                    self.room_incarnation,
                    self.local_queue_instance,
                    self.local.occurrence.as_deref().unwrap_or_default()
                ));
            }
            let cursor = self.online.native.cursor()?;
            return Some(format!(
                "isolated:{}:{}",
                self.room_incarnation,
                cursor.occurrence.as_deref().unwrap_or_default()
            ));
        }
        let basis = self.online.projection.basis(&socket::Action::Next)?;
        Some(format!(
            "{}:{}:{}:{}:{}",
            self.room_incarnation,
            basis.session_id,
            basis.generation,
            basis.occurrence.as_deref().unwrap_or_default(),
            basis.successor_key.unwrap_or_default()
        ))
    }
    fn lyric_seek_before_ms(&self) -> i32 {
        if self.local_transport() {
            return 0;
        }
        let Some(state) = self.online.visible() else {
            return 0;
        };
        let mut cap = lyric_seek_before_ms(state.current().map_or(0.0, |row| row.duration));
        if self.online.is_owner(&self.device_id)
            && let Some(audio) = &self.audio
        {
            let decoded = lyric_seek_before_ms(audio.facts().duration);
            if decoded > 0 {
                cap = cap.min(decoded);
            }
        }
        cap
    }
    fn can_queue_saved(&self) -> bool {
        self.current()
            && self.local_transport()
            && !self.online.detached
            && self
                .account
                .as_ref()
                .is_some_and(|a| a.session.can_access_local())
    }
    fn queue_order_token(&self) -> Option<String> {
        let order = if self.local_transport() && !self.online.detached {
            let upcoming = &self.local.entries[self.local.upcoming_start()..];
            (upcoming.len() > 1).then_some(self.local_revision)?
        } else {
            use std::hash::{Hash, Hasher};
            let upcoming = self.online_upcoming()?;
            if upcoming.len() < 2 {
                return None;
            }
            let mut hash = std::collections::hash_map::DefaultHasher::new();
            for row in upcoming {
                row.key.hash(&mut hash);
            }
            hash.finish()
        };
        let mode = *self.account.as_ref()?.session.network_changes().borrow();
        Some(format!("{}:{order}:{mode}", self.room_token()?))
    }
    fn online_upcoming(&self) -> Option<&[socket::Track]> {
        let (state, key) = if self.online.detached {
            (
                self.online.native.state()?,
                self.online.native.cursor()?.key,
            )
        } else {
            let state = self.online.visible()?;
            (state, state.cursor.key)
        };
        let current = state.queue.iter().position(|r| r.key == key)?;
        Some(&state.queue[current + 1..])
    }

    fn reorder_queue(&mut self, basis: &str, key: u64, after: Option<u64>) {
        // The normal command drain is bounded. Do not write against the
        // projection if newer events still await validation.
        if !self.offline && self.socket.as_ref().is_some_and(|s| !s.events.is_empty()) {
            return;
        }
        let local = self.local_transport() && !self.online.detached;
        if self.queue_order_token().as_deref() != Some(basis) {
            return;
        }
        let valid = if local {
            queue_destination(
                self.local
                    .entries
                    .iter()
                    .skip(self.local.upcoming_start())
                    .map(|r| r.key),
                key,
                after,
            )
        } else {
            self.online_upcoming()
                .is_some_and(|rows| queue_destination(rows.iter().map(|r| r.key), key, after))
        };
        if valid {
            let Some(mode_stamp) = basis
                .rsplit(':')
                .next()
                .and_then(|stamp| stamp.parse::<u64>().ok())
            else {
                return;
            };
            let action = Action::Move { key, after };
            if local {
                // Ordering doesn't sample or alter the running Saved output.
                let Some(account) = &self.account else { return };
                let mode = account.session.network_changes();
                let mode = mode.borrow();
                if *mode != mode_stamp {
                    return;
                }
                let change = self.local.action(action);
                self.local_change(change);
            } else if self
                .account
                .as_ref()
                .is_some_and(|a| *a.session.network_changes().borrow() == mode_stamp)
            {
                self.action(action);
            }
        }
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
        let access = self.account.as_ref().map(|a| Access {
            epoch: self.epoch,
            api: a.api.clone(),
            session: a.session.clone(),
            offline: self.online_generation().is_none(),
            personal: self.personal.access(),
        });
        self.personal
            .bind_cache(access.as_ref(), &self.shared_epoch);
        self.access.send_replace(access);
    }
    fn login_view(&self, busy: bool, error: bool, status: &'static str) {
        let (weak, shared, epoch) = (self.window.clone(), self.shared_epoch.clone(), self.epoch);
        let signed = self.account.is_some();
        let manual_offline = self.offline;
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
                ui.set_manual_offline(manual_offline);
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
        if a.session.is_local_only() {
            a.session.retire_local_grant();
            return;
        }
        self.logouts.push(tokio::spawn(async move {
            // Even offline, logout revokes every clone without creating a request.
            let _ = a.api.logout(&a.session).await;
        }));
    }
    fn reset(&mut self, epoch: u64) {
        self.cancel_reconnect();
        self.recover_at = None;
        self.cancel_login();
        self.clear_output();
        if let Some(retained) = self.retained.take() {
            retained.retire();
        }
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
        // Consume a mode click before sealing the remembered session, even
        // if the window closed before the next playback tick.
        if self.current() {
            self.apply_mode();
        }
        // Finish the accepted last-good projection before retiring this account.
        self.personal.flush_cache().await;
        let epoch = crate::advance_account_epoch(&self.shared_epoch);
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
            if let Some(profile) = retained.and_then(|keep| keep.profile()) {
                // Do not remotely log out a normally retained login. Check for
                // a denial through the final local retirement, after writes drain.
                a.session.set_offline(true);
                if a.session.retire_local_grant()
                    && let Ok(store) = &self.login_store
                {
                    let _ = store.forget_profile(&profile).await;
                }
            } else {
                self.revoke(a);
            }
        }
        for task in self.logouts.drain(..) {
            let _ = task.await;
        }
    }
    fn clear_output(&mut self) {
        self.room_incarnation = self.room_incarnation.wrapping_add(1);
        self.local_revision = 0;
        self.cancel_work();
        self.stop();
        self.audio = None;
        self.disconnect_personal();
        self.socket = None;
        self.online = Online::default();
        self.local = LocalQueue::default();
        self.listening = Listening::default();
        self.saved_skip = 0;
        self.local_store = None;
        self.local_status = Status::Ok;
        self.action_status = "";
    }
    fn connect(&mut self) {
        if !self.network_allowed() || self.socket.is_some() || self.online.superseded {
            return;
        }
        let Some(a) = &self.account else {
            return;
        };
        self.socket = socket::spawn(
            a.api.clone(),
            a.session.clone(),
            socket::DeviceIdentity {
                device_id: self.device_id.clone(),
                name: "Spofify Desktop".into(),
                can_play: self.audio.is_some(),
            },
        )
        .ok();
    }
    fn apply_mode(&mut self) {
        let mode = self.mode.clone();
        mode.apply(self.epoch, |desired, serial| {
            self.cancel_reconnect();
            self.recover_at = None;
            if desired {
                self.set_offline(true);
                self.save_mode(true);
            } else {
                if self.offline && !self.save_mode(false) { return; }
                self.offline = false;
                self.online.superseded = false;
                if self.account.as_ref().is_some_and(|a| a.session.is_offline()) {
                    self.start_reconnect(serial);
                } else {
                    self.connect();
                }
            }
        });
        if !self.offline && self.reconnect.is_none() && self.recover_at.is_none()
            && self.account.as_ref().is_some_and(|a| a.session.is_offline() && !a.session.is_local_only())
        {
            self.automatic_fallback();
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
        let cold = session.is_local_only();
        let retained = self.retained.clone();
        let profile = retained.as_ref().and_then(Retention::profile);
        let writer = self.auth_writer.as_ref().map(Writer::sender);
        let prior_writes = self
            .auth_writer
            .as_ref()
            .map(|writer| writer.barrier(epoch));
        let store = self.login_store.clone();
        self.action_status = "";
        let task = tokio::spawn(async move {
            // The manual preference write must settle before reading the cold
            // grant. Keep the network fence closed until validation succeeds.
            if let Some(prior_writes) = prior_writes {
                tokio::select! {
                    biased;
                    _ = mode.cancelled(serial) => return,
                    done = prior_writes => if done.is_err() { return },
                }
            }
            let result = if cold {
                let (Some(profile), Ok(store)) = (profile, store) else {
                    return;
                };
                let result = reconnect_attempts(
                    &mode,
                    serial,
                    || current.load(Ordering::SeqCst) == epoch,
                    || {
                        auth::reconnect_local(&store, &profile, &api, &session, || {
                            current.load(Ordering::SeqCst) == epoch && mode.can_probe(epoch, serial)
                        })
                    },
                )
                .await;
                if result
                    .as_ref()
                    .is_some_and(|r| r.as_ref().is_err_and(|e| e.expired || e.status == 401))
                {
                    // A definite denial outlives a queued mode/epoch change. Only
                    // retire the exact attempted slot, never its successor.
                    session.retire_local_grant();
                    let _ = store.retire(Some(&profile));
                    if let Some(writer) = writer {
                        let _ = writer.send((epoch, Write::Forget(Some(profile.clone()))));
                    }
                }
                result.map(|r| {
                    r.map(|(api, session)| Rejoined::Cold(Account { api, session }, profile))
                })
            } else {
                let result = reconnect_attempts(
                    &mode,
                    serial,
                    || current.load(Ordering::SeqCst) == epoch,
                    || api.validate_reconnect(&session),
                )
                .await;
                // The probe itself fences an authoritative revocation. A local
                // deadline, failed request or malformed response is not a denial.
                if result.as_ref().is_some_and(Result::is_err)
                    && session.is_revoked()
                    && let Some(retained) = retained
                {
                    retained.retire();
                    if let (Ok(store), Some(profile)) = (store, retained.profile()) {
                        let _ = store.retire(Some(&profile));
                    }
                    if let Some(writer) = writer {
                        let _ = writer.send((epoch, Write::ForgetRetained(retained)));
                    }
                }
                result.map(|r| r.map(|()| Rejoined::Existing))
            };
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
    fn finish_reconnect(&mut self, epoch: u64, serial: u64, result: Result<Rejoined, ApiError>) {
        if epoch != self.epoch
            || !self.current()
            || self
                .reconnect
                .as_ref()
                .is_none_or(|probe| probe.serial != serial)
        {
            return;
        }
        self.cancel_reconnect();
        match result {
            Ok(rejoined) => {
                let mode = self.mode.clone();
                mode.if_current_online(epoch, serial, || {
                    if !self.current() {
                        return;
                    }
                    self.recover_at = None;
                    if let Rejoined::Cold(account, profile) = rejoined {
                        let profile = self.retained.as_ref().and_then(Retention::profile).unwrap_or(profile);
                        if !self.slot_active(&profile) {
                            self.expire();
                            return;
                        }
                        if let Some(audio) = &mut self.audio {
                            audio.rebind_account_session(account.session.clone());
                        }
                        if let Some(local) = self.account.replace(account) {
                            self.revoke(local);
                        }
                    }
                    self.set_offline(false);
                });
            }
            Err(e)
                if e.status == 401
                    || self
                        .account
                        .as_ref()
                        .is_some_and(|a| a.session.is_revoked()) =>
            {
                self.expire();
            }
            Err(_) => self.automatic_fallback(),
        }
    }
    fn automatic_fallback(&mut self) {
        if self.offline || self.account.is_none() {
            return;
        }
        if let Some(a) = &self.account {
            a.session.set_offline(true);
        }
        self.cancel_login();
        self.leave_link();
        self.socket = None;
        self.recover_at = Some(Instant::now() + Duration::from_secs(15));
        self.publish_access();
    }
    fn set_offline(&mut self, offline: bool) {
        self.offline = offline;
        if self.audio.is_none()
            && let Some(a) = &self.account
        {
            self.audio = Some(Audio::new_account_scoped(
                a.session.clone(),
                self.shared_epoch.clone(),
                self.epoch,
                self.master_gain.clone(),
            ));
        }
        if offline {
            self.cancel_login();
            self.retire_pending(false);
            self.mode.block_updates();
            if let Some(a) = &self.account {
                a.session.set_offline(true);
            }
            self.leave_link();
            self.socket = None;
            for task in &self.logouts {
                task.abort();
            }
        } else if let Some(a) = &self.account {
            if a.session.is_expired() {
                self.expire();
                return;
            }
            a.session.set_offline(false);
            self.mode.allow_updates(self.epoch, &self.shared_epoch);
        }
        self.publish_access();
        if !offline {
            self.connect();
        }
    }
    fn save_mode(&mut self, offline: bool) -> bool {
        // Device preference only: a remembered identity is neither needed
        // nor changed by toggling this network gate.
        #[cfg(not(test))]
        let result = crate::api::preferences::save_manual_offline(offline);
        #[cfg(test)]
        let result = self.manual_mode_path.as_deref().map_or(Ok(()), |path| {
            crate::api::preferences::save_manual_mode_at(path, offline)
        });
        if result.is_ok() { return true; }
        self.action_status = "Offline Mode preference couldn't be saved.";
        false
    }
    fn auth_transport_expired(&mut self) {
        let Some(session) = self.account.as_ref().map(|a| a.session.clone()) else {
            return;
        };
        if session.is_revoked() {
            self.expire();
            return;
        }
        if self.offline || self.reconnect.is_some() {
            return;
        }
        // A local cookie deadline is not a server denial. Keep the local grant
        // and decoder while rechecking the prior credential separately.
        session.set_offline(true);
        self.leave_link();
        self.socket = None;
        self.publish_access();
        if let Some(serial) = self.mode.begin_automatic(self.epoch) {
            self.start_reconnect(serial);
        }
    }
    fn expire(&mut self) {
        let offline = self.offline;
        if let Some(retained) = self.retained.take() {
            retained.retire();
            if let Some(profile) = retained.profile()
                && let Ok(store) = &self.login_store
            {
                let _ = store.retire(Some(&profile));
            }
            self.write_auth(self.epoch, Write::ForgetRetained(retained));
        }
        self.reset(self.epoch);
        self.offline = offline;
        self.login_view(false, true, "Session expired. Sign in again.");
    }
    fn slot_active(&self, profile: &LoginProfile) -> bool {
        self.login_store
            .as_deref()
            .map_err(|e| *e)
            .and_then(|store| store.is_active(profile))
            .unwrap_or(false)
    }
    fn write_auth(&self, epoch: u64, write: Write) {
        if let Some(writer) = &self.auth_writer {
            writer.send(epoch, write);
        }
    }
    fn start_restore(&mut self) {
        #[cfg(not(test))]
        let manual_offline = crate::api::preferences::manual_offline();
        #[cfg(test)]
        let manual_offline = self.manual_mode_path.as_deref().is_some_and(crate::api::preferences::manual_mode_at);
        self.offline = manual_offline;
        if manual_offline { self.mode.block_updates(); }
        let (epoch, current, results) =
            (self.epoch, self.shared_epoch.clone(), self.results.clone());
        let store = self.login_store.clone();
        let mode = self.mode.clone();
        self.login = Some(tokio::spawn(async move {
            let prefill = results.clone();
            let outcome = auth::restore(
                store,
                epoch,
                &current,
                manual_offline,
                || mode.resolve_update_startup(epoch, &current),
                move |origin, username| async move {
                    let _ = prefill.send(Reply::Prefill(epoch, origin, username)).await;
                },
            )
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
            AuthCommand::GoOnline(epoch) if epoch == self.epoch && self.current() && self.account.is_none() && self.login.is_none() => {
                if self.save_mode(false) {
                    self.offline = false;
                    self.mode.allow_updates(epoch, &self.shared_epoch);
                    self.login_view(false, false, "");
                } else {
                    self.login_view(false, true, self.action_status);
                }
            }
            AuthCommand::Logout(epoch) => {
                // This intent must not get skipped if another auth intent is
                // already queued. The persistence worker orders it with saves.
                let offline = self.explicit_offline();
                self.retire_pending(true);
                if let Some(retained) = &self.retained {
                    retained.retire();
                }
                if let Ok(store) = &self.login_store {
                    let _ = store.retire(None);
                }
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
            AuthCommand::Login {
                epoch,
                origin,
                username,
                password,
                remember,
            } if epoch == self.shared_epoch.load(Ordering::SeqCst) => {
                self.retire_pending(false);
                if self.offline {
                    self.reset(epoch);
                    self.offline = true;
                    self.login_view(false, false, "Go online to sign in.");
                    return true;
                }
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
            Command::QuickAdd(ticket) => {
                self.apply_mode();
                if !self.offline && self.current() {
                    self.drain_online_events(None);
                }
                self.send_quick_add(ticket);
            }
            Command::ReorderPlaylist {
                observed,
                mode_stamp,
                command,
            } if observed.epoch == self.epoch && self.current() => {
                self.apply_mode();
                if !self.offline {
                    self.drain_online_events(None);
                }
                if !self.send_playlist_order(observed, mode_stamp, &command) {
                    self.personal.refused(observed.epoch, observed.generation);
                }
            }
            Command::Scoped(epoch, command) if epoch == self.epoch && self.current() => {
                self.apply_mode();
                self.scoped(*command)
            }
            _ => {}
        }
    }
    fn scoped(&mut self, command: Command) {
        // A guarded local transport event was issued against the displayed
        // offline visit. Serialize it ahead of an opening snapshot queued
        // concurrently; any resulting cursor is eligible for claim repair,
        // never reinterpreted as a new command sent to another owner.
        let local_transport = self.local_transport()
            && match &command {
                Command::GuardedToggle(token)
                | Command::Guarded(
                    token,
                    Action::Pause
                    | Action::Resume
                    | Action::Seek(_)
                    | Action::Next
                    | Action::Previous
                    | Action::Jump(_),
                ) => !token.is_empty() && self.room_token().as_deref() == Some(token),
                _ => false,
            };
        if !local_transport {
            self.drain_online_events(None);
        }
        match command {
            #[cfg(test)]
            Command::Offline(true) => self.set_offline(true),
            Command::PlaySaved {
                mode_stamp,
                queue,
                index,
                input,
                store,
            } if self.audio.is_some()
                && self.online_generation().is_none()
                && self.account.as_ref().is_some_and(|a| {
                    *a.session.network_changes().borrow() == mode_stamp
                        && a.session.can_access_local()
                }) =>
            {
                if !queue.get(index).is_some_and(|item| {
                    item.id == input.item.id && item.recording == Some(input.recording)
                }) || queue.iter().any(|item| item.recording.is_none())
                    || input.source.len() == 0
                {
                    return;
                }
                if self.local.replace(queue, index, false) == Change::Start {
                    self.online.detached = false;
                    self.online.detached_owner = false;
                    self.saved_skip = 0;
                    self.cancel_work();
                    self.stop();
                    self.local_store = Some(store);
                    self.local_queue_instance = self.local_queue_instance.wrapping_add(1);
                    self.local_revision = self.local_revision.wrapping_add(1);
                    self.start_saved(Some(input));
                }
            }
            Command::QueueSaved {
                mode_stamp,
                queue,
                store,
            } if self.can_queue_saved()
                && self
                    .account
                    .as_ref()
                    .is_some_and(|a| *a.session.network_changes().borrow() == mode_stamp) =>
            {
                if queue.iter().all(|item| item.recording.is_some())
                    && self.local.append(queue) == Change::Edit
                {
                    self.local_store = Some(store);
                    self.local_change(Change::Edit);
                }
            }
            Command::ReorderQueue { basis, key, after } => self.reorder_queue(&basis, key, after),
            Command::Guarded(token, action) => {
                let current = self.room_token();
                // Next's successor guard must not discard a rapid explicit
                // repeat change merely because it changed what Next means.
                let same_room = self.online_generation().is_some()
                    && matches!(
                        action,
                        Action::CycleRepeat | Action::Repeat(_) | Action::Shuffle(_)
                    )
                    && token
                        .rsplit_once(':')
                        .zip(current.as_deref().and_then(|t| t.rsplit_once(':')))
                        .is_some_and(|((before, _), (now, _))| before == now);
                if !token.is_empty() && (same_room || current.as_deref() == Some(token.as_str())) {
                    match action {
                        Action::Pause if !self.wants_pause() => (),
                        Action::Resume if self.wants_pause() => (),
                        _ => self.action(action),
                    }
                }
            }
            Command::SeekLyric {
                room,
                snapshot,
                start_ms,
            } => {
                // A bounded drain may have left newer cursor events waiting.
                // Never submit a timestamp for a cursor we cannot revalidate.
                if (self.local_store.is_some()
                    || self.socket.as_ref().is_some_and(|s| s.events.is_empty()))
                    && self.room_token().as_deref() == Some(room.as_str())
                    && let Some(seconds) = lyric_seek_seconds(start_ms, self.lyric_seek_before_ms())
                    && snapshot.is_current()
                {
                    self.action(Action::Seek(seconds));
                }
            }
            Command::Playback(action) if !action.depends_on_visible_queue() => self.action(action),
            Command::GuardedToggle(token)
                if !token.is_empty() && self.room_token().as_deref() == Some(token.as_str()) =>
            {
                self.toggle();
            }
            _ => {}
        }
        if local_transport {
            self.drain_online_events(None);
        }
    }
    fn wants_pause(&self) -> bool {
        if !self.local_transport() {
            self.online.wants_pause()
        } else if self.local_store.is_some() {
            self.local.desired
        } else {
            self.online.detached
                && self
                    .online
                    .native
                    .cursor()
                    .is_some_and(|c| c.desired_playing)
        }
    }
    fn retire_private_audio(&mut self) {
        self.cancel_work();
        self.stop();
        self.online.detached = false;
        self.online.detached_owner = false;
        self.local = LocalQueue::default();
        self.local_store = None;
    }
    fn toggle(&mut self) {
        self.action(if self.wants_pause() {
            Action::Pause
        } else {
            Action::Resume
        });
    }
    fn action(&mut self, action: Action) {
        let explicit_online = self.online_generation().is_some() && matches!(action,
            Action::Play { .. } | Action::Enqueue { .. } | Action::SelectDevice(_) | Action::RenameDevice { .. });
        if explicit_online && self.private_audio() && !matches!(action, Action::Enqueue { .. } | Action::RenameDevice { .. }) {
            self.retire_private_audio();
        }
        if explicit_online || !self.local_transport() {
            if !self.online.reconciling_claim()
                || matches!(
                    action,
                    Action::SelectDevice(_) | Action::RenameDevice { .. }
                )
            {
                self.online_action(action);
            }
            return;
        }
        if self.local_store.is_some() || (!self.online.detached && !self.local.entries.is_empty()) {
            self.saved_skip = 0;
            if self.local.desired
                && self.local.current().is_some()
                && let Some(audio) = &self.audio
            {
                self.local.elapsed = audio.facts().elapsed;
            }
            let change = self.local.action(action);
            self.local_change(change);
        } else if self.online.detached {
            self.detached_action(action);
        }
    }
    fn record_local_listening(&mut self) {
        use crate::api::{catalog::{CatalogItem, CatalogKind, Coverage}, library::HistoryEntry};
        let Some(audio) = &self.audio else { return };
        let (event_id, origin, id, title, album_id, album_name, artists, duration) = if self.local_store.is_some() {
            let (Some(entry), Some(visit)) = (self.local.current(), self.local.occurrence.as_ref()) else { return };
            let track = &entry.track;
            let artists = track.music.credits.iter().map(|c| crate::api::catalog::ArtistCredit {
                id: match &c.target { Some(crate::pages::Route::Artist(id)) => id.clone(), _ => String::new() },
                name: c.name.clone(),
            }).collect();
            let album_id = track.music.album.as_ref().and_then(|c| match &c.target { Some(crate::pages::Route::Album(id)) => Some(id.clone()), _ => None }).unwrap_or_default();
            (visit.clone(), "offline", track.id.clone(), track.title.clone(), album_id, track.album_name.clone(), artists, track.duration)
        } else if self.online.detached {
            let (Some(state), Some(cursor)) = (self.online.native.state(), self.online.native.cursor()) else { return };
            let Some(visit) = &cursor.occurrence else { return };
            let Some(track) = state.queue.iter().find(|row| row.key == cursor.key) else { return };
            // Match the server event for the same already-shared visit.
            let mut hash = ring::digest::Context::new(&ring::digest::SHA256);
            hash.update(self.online.projection.session_id().unwrap_or_default().as_bytes());
            hash.update(b"\0");
            hash.update(visit.as_bytes());
            let digest: String = hash.finish().as_ref().iter().map(|b| format!("{b:02x}")).collect();
            let artists = track.artists.iter().map(|a| crate::api::catalog::ArtistCredit {
                id: a.id.clone().unwrap_or_default(), name: a.name.clone(),
            }).collect();
            (format!("v3:{digest}"), "online", track.id.clone(), track.name.clone(), track.album_id.clone(), track.album_name.clone(), artists, track.duration)
        } else {
            self.listening.suspend();
            return;
        };
        let facts = audio.facts();
        let sample = audio.run_identity().map(|run| (run, (facts.elapsed * 1_000.0) as u64));
        if self.listening.observe(&event_id, sample) {
            self.personal.record_local(self.epoch, HistoryEntry {
                id: event_id,
                played_at: time::OffsetDateTime::now_utc().format(&time::format_description::well_known::Rfc3339).expect("UTC timestamp"),
                origin: Some(origin.into()), received_at: None, metadata_missing: false,
                track: CatalogItem {
                    kind: CatalogKind::Track, id, name: title, artists, genres: vec![], album_id, album_name,
                    album_type: String::new(), release_date: String::new(), relation: String::new(),
                    duration_ms: (duration * 1_000.0) as u64, disc_number: 0, track_number: 0,
                    has_audio: true, coverage: Coverage::default(),
                }
            });
        }
    }

    fn tick_saved(&mut self) {
        if self.local.desired && self.media.is_none() && self.audio.is_some() {
            let audio = self.audio.as_mut().unwrap();
            if let Some(status) = audio.failure() {
                self.local.elapsed = audio.facts().elapsed;
                self.saved_failure(status);
                return;
            }
            if audio.has_started() {
                self.saved_skip = 0;
            }
            self.tick_saved_crossfade();
            let facts = self.audio.as_ref().unwrap().facts();
            self.local.elapsed = facts.elapsed;
            if facts.ended {
                self.finish_saved_track();
            }
        }
    }
    fn saved_failure(&mut self, status: Status) {
        if self.saved_skip > 0 {
            self.saved_skip -= 1;
            if self.local.advance(false) == Change::Start {
                self.local_change(Change::Start);
                return;
            }
        }
        self.saved_skip = 0;
        self.local.desired = false;
        self.local_status = status;
        self.stop_local();
    }

    fn local_change(&mut self, change: Change) {
        if change != Change::Invalid {
            self.local_revision = self.local_revision.wrapping_add(1);
        }
        match change {
            Change::Start => {
                self.cancel_work();
                self.stop_local();
                self.start_saved(None);
            }
            Change::Stop => {
                self.cancel_work();
                self.stop_local();
                self.local_status = Status::Ok;
            }
            Change::Clear => {
                self.cancel_work();
                self.local_lookahead = None;
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
            if input.item.id != id || input.source.len() == 0 {
                self.local_status = Status::Media;
                self.local.desired = false;
                return;
            }
            audio.discard_cache();
            self.local_status = audio.play(
                id,
                input.source,
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
        let load = read_saved(
            account,
            store,
            self.local_read_slot.clone(),
            id,
            entry.track.recording,
        );
        let results = self.results.clone();
        let (epoch, operation) = (self.epoch, self.operation);
        self.media = Some(tokio::spawn(async move {
            let input = load.await;
            let _ = results
                .send(Reply::OfflineMedia {
                    epoch,
                    operation,
                    input,
                })
                .await;
        }));
    }
    fn tick_saved_crossfade(&mut self) {
        let target = if self.crossfade_enabled.load(Ordering::Acquire) {
            self.local.current().zip(self.local.crossfade_successor())
        } else {
            None
        };
        let Some(audio) = &mut self.audio else { return };
        if self
            .local_lookahead
            .as_ref()
            .is_some_and(|look| target.is_none_or(|(from, to)| look.key != (from.key, to.key)))
        {
            self.local_lookahead = None;
            audio.cancel_crossfade();
        }
        audio.maintain_crossfade();
        let Some((from, to)) = target else { return };
        let Some(remaining) = audio.crossfade_remaining() else {
            return;
        };
        if remaining > crate::audio::CROSSFADE_SECONDS {
            return;
        }
        // Read saved audio only as its fade window opens, so speculative
        // verification doesn't hold a removed/replaced download for long.
        if self.local_lookahead.is_none() {
            let (Some(account), Some(store)) = (&self.account, self.local_store.clone()) else {
                return;
            };
            let id = to.track.id.clone();
            // A retired speculative read can still be inside blocking I/O;
            // it must not occupy the slot needed to play, seek, or resume.
            let load = read_saved(
                account,
                store,
                self.local_preview_slot.clone(),
                id.clone(),
                to.track.recording,
            );
            self.local_lookahead =
                Some(crossfade::Lookahead::new((from.key, to.key), async move {
                    let input = load.await?;
                    (input.item.id == id && input.source.len() != 0).then_some(input.source)
                }));
        }
        if let Some(look) = &mut self.local_lookahead
            && let Some(source) = look.take_source()
            && audio.start_crossfade(to.track.id.clone(), source, Authority::offline())
        {
            look.started(());
        }
    }
    fn finish_saved_track(&mut self) {
        let overlap = self
            .local_lookahead
            .take()
            .and_then(crossfade::Lookahead::into_playing);
        let change = self.local.advance(true);
        self.saved_skip = if change == Change::Start { self.local.entries.len() - 1 } else { 0 };
        if change == Change::Start
            && self.crossfade_enabled.load(Ordering::Acquire)
            && overlap.is_some()
            && let Some(audio) = &mut self.audio
            && audio.promote_crossfade()
        {
            audio.discard_cache();
            self.local.elapsed = audio.facts().elapsed;
            self.local_revision = self.local_revision.wrapping_add(1);
            self.local_status = Status::Ok;
            self.action_status = "";
        } else {
            self.local_change(change);
        }
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
                ui.set_busy(false);
                ui.set_restoring_login(false);
                ui.set_signed_in(true);
            }
        });
    }
    fn accept_login(&mut self, epoch: u64) {
        if !self.current()
            || epoch != self.epoch
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
                manual_offline,
            },
        ) = self.auth_ready.take().unwrap();
        let cold = session.is_local_only();
        if cold && !matches!(&kind, Kind::Restored(profile) if self.slot_active(profile)) {
            self.login_view(
                false,
                true,
                "The saved session is no longer available. Sign in again.",
            );
            return;
        }
        // All sign-in paths announce capability, not a selection or transport
        // command. Only Go may elect a vacant output or rebind its same device.
        if self.installation.is_none() {
            self.installation = Installation::open().ok();
        }
        self.device_id = self
            .installation
            .as_ref()
            .and_then(|installation| {
                let (origin, principal) = api.authenticated_account(&session).ok()?;
                Some(installation.device_id(origin, principal))
            })
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        self.retained = match kind {
            Kind::Restored(profile) => {
                let retained = Retention::new(profile);
                if !cold && let Ok(login) = api.remembered_login(&session) {
                    self.write_auth(
                        epoch,
                        Write::UpgradeLegacy(retained.clone(), session.clone(), login),
                    );
                }
                Some(retained)
            }
            Kind::Password { remember } => {
                // Account replacement cannot leave an old cold slot active
                // while keyring deletion / publication is pending.
                if let Ok(store) = &self.login_store
                    && store.retire(None).is_err()
                    && store.profile().ok().flatten().is_some_and(|p| p.remembered)
                {
                    self.revoke(Account { api, session });
                    self.login_view(false, true, "Could not retire the previous saved sign-in.");
                    return;
                }
                let retained = Retention::default();
                let login = remember
                    .then(|| api.remembered_login(&session).ok())
                    .flatten();
                self.write_auth(
                    epoch,
                    Write::Record {
                        origin,
                        username: session.username.clone(),
                        wanted: remember,
                        login,
                        retained: retained.clone(),
                        session: session.clone(),
                    },
                );
                remember.then_some(retained)
            }
        };
        self.audio = Some(Audio::new_account_scoped(
            session.clone(),
            self.shared_epoch.clone(),
            self.epoch,
            self.master_gain.clone(),
        ));
        self.account = Some(Account { api, session });
        if manual_offline {
            self.set_offline(true);
        } else {
            self.publish_access();
            if cold {
                if let Some(serial) = self.mode.begin_automatic(epoch) {
                    self.start_reconnect(serial);
                }
            } else {
                self.mode.allow_updates(epoch, &self.shared_epoch);
                self.connect();
            }
        }
    }
    fn finish_login(&self, fallback: auth::Fallback) {
        if let Some(profile) = fallback.discard {
            self.write_auth(self.epoch, Write::Forget(Some(profile)));
        }
        let (weak, shared, epoch) = (self.window.clone(), self.shared_epoch.clone(), self.epoch);
        let manual_offline = self.offline;
        let _ = slint::invoke_from_event_loop(move || {
            if shared.load(Ordering::SeqCst) == epoch
                && let Some(ui) = weak.upgrade()
            {
                ui.set_manual_offline(manual_offline);
                ui.set_busy(false);
                ui.set_restoring_login(false);
                ui.set_error(!fallback.status.is_empty());
                ui.set_status(fallback.status.into());
                ui.set_auth_notice(fallback.notice.into());
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
                if epoch == self.epoch && self.login.is_some() =>
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
                if epoch == self.epoch && self.login.is_some() =>
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
                && self.local_store.is_some()
                && self.local.desired =>
            {
                self.media = None;
                if let Some(input) = input {
                    self.start_saved(Some(input));
                } else {
                    self.saved_failure(Status::Load);
                }
            }
            Reply::Media {
                epoch,
                request,
                source,
            } if self.epoch == epoch => self.complete_online(request, source),
            _ => {}
        }
    }
    fn link(&self) -> Link {
        self.socket
            .as_ref()
            .map_or(Link::Closed, |s| *s.link.borrow())
    }
    fn cancel_work(&mut self) {
        self.operation = self.operation.wrapping_add(1);
        if let Some(task) = self.media.take() {
            task.abort();
        }
    }
    fn stop_local(&mut self) {
        self.local_lookahead = None;
        if let Some(audio) = &mut self.audio {
            audio.stop();
        }
    }
    fn stop(&mut self) {
        self.online.cancel_loads();
        self.online.native = native::NativeV3::default();
        self.stop_local();
    }
    fn tick(&mut self) {
        self.tick_with_event(None);
    }
    fn explicit_offline(&self) -> bool {
        self.offline
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
        if self
            .account
            .as_ref()
            .is_some_and(|a| !a.session.can_access_local())
        {
            self.auth_transport_expired();
            if self.account.is_none() {
                return;
            }
        }
        if !self.offline && self.reconnect.is_none()
            && self.recover_at.is_some_and(|at| Instant::now() >= at)
        {
            self.recover_at = None;
            if let Some(serial) = self.mode.begin_automatic(self.epoch) {
                self.start_reconnect(serial);
            }
        }
        self.record_local_listening();
        self.tick_online(event);
    }
    fn publish(&self) {
        let mut p = PlayerView {
            offline: self.offline,
            local_playback: self.local_transport(),
            reconnecting: self.reconnect.is_some()
                || (!self.offline
                    && !self.online.superseded
                    && self.account.is_some()
                    && self.online_generation().is_none()),
            saved_queue_enabled: self.can_queue_saved(),
            status: self.action_status,
            room_token: self.room_token().unwrap_or_default(),
            queue_order_token: self.queue_order_token().unwrap_or_default(),
            lyric_seek_before_ms: self.lyric_seek_before_ms(),
            ..PlayerView::default()
        };
        self.project_online(&mut p);
        let saved = p.local_playback && !self.online.detached;
        if saved {
            p.media_track_token.clear();
            p.output = "This desktop".into();
            p.status = self.action_status;
            p.playing = false;
            p.album.clear();
            p.credits.clear();
            p.context_id.clear();
            p.context_kind = "";
            p.context_name.clear();
            p.shuffle = self.local.shuffle;
            p.repeat = self.local.repeat.wire();
            p.wants_pause = self.local.desired;
            p.elapsed = self.local.elapsed;
            if let Some(entry) = self.local.current() {
                p.media_track_token = format!(
                    "local:{}:{}:{}",
                    self.room_incarnation,
                    self.local_queue_instance,
                    self.local.occurrence.as_deref().unwrap_or_default()
                );
                p.title = entry.track.title.clone();
                p.detail = entry.track.subtitle.clone();
                p.track = entry.track.id.clone();
                p.album_name = entry.track.album_name.clone();
                p.album = local_album(&entry.track);
                p.credits = local_credits(&entry.track);
                if let Some(context) = &entry.track.context {
                    p.context_kind = context.kind;
                    p.context_id.clone_from(&context.id);
                    p.context_name.clone_from(&context.name);
                }
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
                Status::Load => "Saved audio could not be loaded.",
                Status::Media => "This desktop could not decode or seek the saved audio.",
                Status::Blocked => "Audio output is unavailable on this desktop.",
                _ => p.status,
            };
            p.queue = self
                .local
                .entries
                .iter()
                .enumerate()
                .map(|(i, e)| QueueEntryView {
                    key: e.key.to_string(),
                    track_id: e.track.id.clone(),
                    title: e.track.title.clone(),
                    subtitle: e.track.subtitle.clone(),
                    album_name: e.track.album_name.clone(),
                    album_id: local_album(&e.track),
                    credits: local_credits(&e.track),
                    current: self.local.position == Some(i),
                    upcoming: i >= self.local.upcoming_start(),
                })
                .collect();
        }
        // Room identity is independent of queue revision/cursor changes. The
        // local queue can be replaced within the same authenticated room.
        if !p.queue.is_empty() {
            if saved {
                p.queue_room_key = format!(
                    "local:{}:{}",
                    self.room_incarnation, self.local_queue_instance
                );
            } else if self.online.detached {
                p.queue_room_key = format!("isolated:{}", self.room_incarnation);
            } else if let Some(generation) = self.online_generation() {
                p.queue_room_key = format!("online:{}:{generation}", self.room_incarnation);
            }
        }
        p.queue_order_writable = !p.queue_order_token.is_empty();
        p.publish(self.window.clone(), self.shared_epoch.clone(), self.epoch);
    }
}

// Validate occurrence keys and reject a same-slot drop against the live region.
// An empty predecessor is the start of Up Next, never the start of the room.
fn queue_destination(keys: impl Iterator<Item = u64>, key: u64, after: Option<u64>) -> bool {
    if !player::key(key) || after.is_some_and(|a| !player::key(a) || a == key) {
        return false;
    }
    let mut previous = None;
    let mut moving = None;
    let mut anchor_exists = after.is_none();
    for entry in keys {
        if Some(entry) == after {
            anchor_exists = true;
        }
        if entry == key {
            moving = Some(previous);
        }
        previous = Some(entry);
    }
    anchor_exists && moving.is_some_and(|original| original != after)
}

fn queue_order_rows(queue: &[QueueEntryView]) -> Vec<crate::ReorderRow> {
    let mut rows = Vec::with_capacity(queue.len());
    for (phase, section) in [(0, "NOW PLAYING"), (1, "UP NEXT"), (2, "PLAYED")] {
        let mut first = true;
        for row in queue.iter().filter(|r| {
            (if r.current {
                0
            } else if r.upcoming {
                1
            } else {
                2
            }) == phase
        }) {
            rows.push(crate::ReorderRow {
                key: row.key.as_str().into(),
                title: row.title.as_str().into(),
                subtitle: row.subtitle.as_str().into(),
                reorderable: phase == 1,
                section: if first { section.into() } else { "".into() },
            });
            first = false;
        }
    }
    rows
}

#[derive(Debug, PartialEq, Eq)]
struct CreditView {
    id: String,
    name: String,
}

fn local_album(track: &LocalTrack) -> String {
    match track.music.album.as_ref().and_then(|c| c.target.as_ref()) {
        Some(crate::pages::Route::Album(id)) => album_route_id(id).into(),
        _ => String::new(),
    }
}
fn local_credits(track: &LocalTrack) -> Vec<CreditView> {
    track
        .music
        .credits
        .iter()
        .map(|c| CreditView {
            id: match &c.target {
                Some(crate::pages::Route::Artist(id)) => id.clone(),
                _ => String::new(),
            },
            name: c.name.clone(),
        })
        .collect()
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
    track_id: String,
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
            && self.track_id == row.track_id.as_str()
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
            track_id: self.track_id.into(),
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
    local_playback: bool,
    reconnecting: bool,
    saved_queue_enabled: bool,
    shuffle: bool,
    repeat: &'static str,
    status: &'static str,
    room_token: String,
    queue_room_key: String,
    queue_order_token: String,
    queue_order_writable: bool,
    queue_order_awaiting_echo: bool,
    media_track_token: String,
    lyric_seek_before_ms: i32,
    playing: bool,
    wants_pause: bool,
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
            ui.set_manual_offline(self.offline);
            ui.set_local_playback(self.local_playback);
            ui.set_reconnecting(self.reconnecting);
            ui.set_saved_queue_enabled(self.saved_queue_enabled);
            ui.set_room_token(self.room_token.into());
            ui.set_queue_room_key(self.queue_room_key.into());
            ui.set_media_track_token(self.media_track_token.into());
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
            ui.set_wants_pause(self.wants_pause);
            ui.set_can_control(self.control);
            ui.set_remote_controls(self.transport);
            ui.set_can_seek(self.transport && self.duration > 0.0);
            ui.set_lyric_seek_before_ms(self.lyric_seek_before_ms);
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
            let queue = ui.get_queue_rows();
            if queue.row_count() != self.queue.len()
                || !self
                    .queue
                    .iter()
                    .zip(queue.iter())
                    .all(|(entry, row)| entry.matches(&row))
            {
                ui.set_queue_order_rows(ModelRc::new(VecModel::from(queue_order_rows(
                    &self.queue,
                ))));
                ui.set_queue_rows(ModelRc::new(VecModel::from(
                    self.queue
                        .into_iter()
                        .map(QueueEntryView::into_row)
                        .collect::<Vec<_>>(),
                )));
            }
            ui.set_queue_order_token(self.queue_order_token.into());
            ui.set_queue_order_writable(self.queue_order_writable);
            ui.set_queue_order_awaiting_echo(self.queue_order_awaiting_echo);
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
fn read_saved(
    account: &Account,
    store: OfflineStore,
    slot: Arc<Semaphore>,
    id: String,
    recording: Option<offline::Recording>,
) -> impl std::future::Future<Output = Option<PlaybackInput>> + use<> {
    let (api, session) = (account.api.clone(), account.session.clone());
    async move {
        let (_, input) =
            offline::read_verified_recording(api, session, slot, id, recording?, move || Ok(store))
                .await
                .ok()?;
        Some(input)
    }
}

async fn reconnect_attempts<T, F, Fut>(
    mode: &ModeControl,
    serial: u64,
    current_epoch: impl Fn() -> bool,
    mut probe: F,
) -> Option<Result<T, ApiError>>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, ApiError>>,
{
    let cancelled = mode.cancelled(serial);
    tokio::pin!(cancelled);
    for attempt in 1..=AUTH_ATTEMPTS {
        if !current_epoch() {
            return None;
        }
        let result = tokio::select! {
            biased;
            _ = &mut cancelled => return None,
            result = tokio::time::timeout(PROBE_TIMEOUT, async { probe().await }) => result.unwrap_or(Err(ApiError {
                message: "Connection is unavailable.", status: 408, expired: false,
            })),
        };
        if attempt == AUTH_ATTEMPTS || !result.as_ref().is_err_and(|e| e.can_retry_restore()) {
            return Some(result);
        }
        tokio::select! {
            biased;
            _ = &mut cancelled => return None,
            _ = tokio::time::sleep(Duration::from_millis(250 << (attempt - 1))) => {},
        }
    }
    unreachable!("authentication attempts must be positive")
}
async fn socket_signal(socket: Option<&mut socket::Socket>) -> Option<socket::Event> {
    if let Some(socket) = socket {
        tokio::select! {
            Ok(_) = socket.link.changed() => return None,
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

fn album_route_id(id: &str) -> &str {
    if crate::api::catalog::segment_id(id) {
        id
    } else {
        ""
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

pub(crate) struct OutputPreferences {
    pub(crate) crossfade_enabled: Arc<AtomicBool>,
    pub(crate) master_gain: MasterGain,
}

pub fn start(
    window: slint::Weak<AppWindow>,
    epoch: Arc<AtomicU64>,
    access: watch::Sender<Option<Access>>,
    mode: ModeControl,
    mut auth_intents: mpsc::UnboundedReceiver<AuthCommand>,
    auth_tx: mpsc::UnboundedSender<AuthCommand>,
    output: OutputPreferences,
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
            let login_store = LoginStore::open_default().map(Arc::new);
            let writer = Writer::new(login_store.clone(), move |epoch, notice| {
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
                #[cfg(test)]
                manual_mode_path: None,
                access,
                results,
                window,
                device_id: uuid::Uuid::new_v4().to_string(),
                login: None,
                auth_ready: None,
                auth_tx: Some(auth_tx),
                auth_writer: Some(writer),
                login_store,
                retained: None,
                reconnect: None,
                recover_at: None,
                listening: Listening::default(),
                saved_skip: 0,
                logouts: vec![],
                account: None,
                offline: false,
                audio: None,
                master_gain: output.master_gain,
                local: LocalQueue::default(),
                local_status: Status::Ok,
                local_store: None,
                crossfade_enabled: output.crossfade_enabled,
                local_lookahead: None,
                local_read_slot: Arc::new(Semaphore::new(1)),
                local_preview_slot: Arc::new(Semaphore::new(1)),
                socket: None,
                online: Online::default(),
                personal: PersonalHub::default(),
                media: None,
                operation: 0,
                room_incarnation: 0,
                local_revision: 0,
                local_queue_instance: 0,
                action_status: "",
                installation: None,
            };
            e.start_restore();
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
    use std::time::Instant;

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
    fn player_album_links_preserve_the_exact_addressable_id() {
        assert_eq!(album_route_id("album%2F原"), "album%2F原");
        for invalid in ["", "..", "bad/segment", "bad\\segment", " album"] {
            assert_eq!(album_route_id(invalid), "");
        }
        let mut track = LocalTrack {
            album_name: "The display name is not an ID".into(),
            ..Default::default()
        };
        assert_eq!(local_album(&track), "");
        track.music.album = Some(crate::pages::Credit {
            name: String::new(),
            target: Some(crate::pages::Route::Album("album%2F原".into())),
        });
        assert_eq!(local_album(&track), "album%2F原");
    }

    #[test]
    fn queue_projection_detects_track_identity_album_and_credit_changes() {
        let entry = || QueueEntryView {
            key: "7".into(),
            track_id: "recording-1".into(),
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
        next.track_id = "recording-2".into();
        assert!(!next.matches(&row));
        let mut next = entry();
        next.credits[0].id = "artist/2".into();
        assert!(!next.matches(&row));
        let mut next = entry();
        next.album_id = "album/2".into();
        assert!(!next.matches(&row));
        let mut next = entry();
        next.album_name = "New album name".into();
        assert!(!next.matches(&row));
    }

    #[tokio::test]
    async fn saved_queue_drop_uses_exact_upcoming_occurrences_and_rejects_stale_basis() {
        let mut e = engine();
        let api = Arc::new(connection("http://127.0.0.1:1").unwrap());
        e.account = Some(Account {
            session: api.test_session(),
            api,
        });
        e.command(Command::Offline(true).scoped(7));
        let track = LocalTrack {
            id: "same-song".into(),
            title: "Fixture".into(),
            subtitle: String::new(),
            album_name: String::new(),
            duration: 25.0,
            ..Default::default()
        };
        assert_eq!(e.local.replace(vec![track; 4], 0, false), Change::Start);
        let keys = |e: &Engine| {
            e.local
                .entries
                .iter()
                .map(|row| row.key)
                .collect::<Vec<_>>()
        };
        let before = keys(&e);
        let basis = e.queue_order_token().unwrap();
        e.local.elapsed = 4.5;
        assert_eq!(e.queue_order_token().as_deref(), Some(basis.as_str()));
        let drop = |basis: &str, key, after| {
            Command::ReorderQueue {
                basis: basis.into(),
                key,
                after,
            }
            .scoped(7)
        };
        for (key, after) in [
            (before[0], Some(before[2])),
            (before[2], Some(before[0])),
            (before[2], Some(before[1])),
            (before[1], None),
            (before[3], Some(u64::MAX)),
        ] {
            e.command(drop(&basis, key, after));
            assert_eq!(keys(&e), before);
            assert_eq!(e.queue_order_token().as_deref(), Some(basis.as_str()));
        }
        e.command(drop(&basis, before[3], None));
        assert_eq!(keys(&e), [before[0], before[3], before[1], before[2]]);
        assert_eq!(e.local.elapsed, 4.5);
        e.command(drop(&basis, before[3], Some(before[2])));
        assert_eq!(keys(&e), [before[0], before[3], before[1], before[2]]);
        let updated = e.queue_order_token().unwrap();
        e.command(drop(&updated, before[3], Some(before[2])));
        assert_eq!(keys(&e), before);
    }

    pub(super) fn engine() -> Engine {
        let (results, _) = mpsc::channel(2);
        let (access, _) = watch::channel(None);
        Engine {
            epoch: 7,
            shared_epoch: Arc::new(AtomicU64::new(7)),
            mode: ModeControl::default(),
            manual_mode_path: None,
            access,
            results,
            window: slint::Weak::default(),
            device_id: "synthetic".into(),
            login: None,
            auth_ready: None,
            auth_tx: None,
            auth_writer: None,
            login_store: Err(StoreError::Config),
            retained: None,
            reconnect: None,
            recover_at: None,
            listening: Listening::default(),
            saved_skip: 0,
            logouts: vec![],
            account: None,
            offline: false,
            audio: None,
            master_gain: MasterGain::default(),
            local: LocalQueue::default(),
            local_status: Status::Ok,
            socket: None,
            local_store: None,
            crossfade_enabled: Arc::default(),
            local_lookahead: None,
            local_read_slot: Arc::new(Semaphore::new(1)),
            local_preview_slot: Arc::new(Semaphore::new(1)),
            online: Online::default(),
            personal: PersonalHub::default(),
            media: None,
            operation: 0,
            room_incarnation: 0,
            local_revision: 0,
            local_queue_instance: 0,
            action_status: "",
            installation: None,
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
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn guarded_media_cannot_retarget_a_new_run_mode_or_account() {
        let mut e = engine();
        let api = Arc::new(connection("http://127.0.0.1:1").unwrap());
        e.account = Some(Account {
            session: api.test_session(),
            api,
        });
        e.command(Command::Offline(true).scoped(7));
        assert!(e.offline);
        let track = LocalTrack {
            id: "same-song".into(),
            title: "Fixture".into(),
            subtitle: String::new(),
            album_name: String::new(),
            duration: 25.0,
            ..Default::default()
        };
        assert_eq!(
            e.local.replace(vec![track.clone(), track], 0, false),
            Change::Start
        );
        let first_run = e.room_token().unwrap();
        e.command(Command::Guarded(first_run.clone(), Action::Seek(4.0)).scoped(7));
        assert_eq!(e.local.elapsed, 4.0);
        let sought_run = e.room_token().unwrap();
        e.command(Command::Guarded(sought_run.clone(), Action::Next).scoped(7));
        assert_eq!(e.local.position, Some(1));
        e.command(Command::Guarded(sought_run, Action::Seek(12.0)).scoped(7));
        assert_eq!(e.local.elapsed, 0.0);

        let current = e.room_token().unwrap();
        e.command(Command::Guarded("retired-online:3".into(), Action::Seek(12.0)).scoped(7));
        assert_eq!(e.local.elapsed, 0.0);
        let intended = e.local.desired;
        e.command(Command::GuardedToggle(first_run).scoped(7));
        assert_eq!(e.local.desired, intended);
        e.shared_epoch.store(8, Ordering::SeqCst);
        e.command(Command::Guarded(current, Action::Seek(12.0)).scoped(7));
        assert_eq!(e.local.elapsed, 0.0);
    }
    #[test]
    fn lyric_seek_leads_by_exact_milliseconds_and_never_clamps_late_lines() {
        let before = lyric_seek_before_ms(36_000.003);
        assert_eq!(lyric_seek_seconds(36_000_002, before), Some(35_999.752));
        assert_eq!(lyric_seek_seconds(36_000_003, before), None);
        assert_eq!(lyric_seek_seconds(100, before), Some(0.0));
        assert_eq!(lyric_seek_seconds(250, before), Some(0.0));
        assert_eq!(lyric_seek_seconds(251, before), Some(0.001));
        assert_eq!(lyric_seek_seconds(-1, before), None);
        for duration in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert_eq!(lyric_seek_seconds(0, lyric_seek_before_ms(duration)), None);
        }
    }

    #[tokio::test]
    async fn lyric_seek_cannot_submit_to_an_isolated_saved_queue() {
        use crate::presentation::LyricSnapshot;
        use std::{fs, os::unix::fs::DirBuilderExt, path::PathBuf};

        struct Fixture(PathBuf);
        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        let dir = Fixture(
            std::env::temp_dir().join(format!("spofify-lyric-test-{}", uuid::Uuid::new_v4())),
        );
        fs::DirBuilder::new().mode(0o700).create(&dir.0).unwrap();
        let mut e = engine();
        let api = Arc::new(connection("http://127.0.0.1:1").unwrap());
        e.account = Some(Account {
            session: api.test_session(),
            api,
        });
        e.local_store = Some(OfflineStore::open(&dir.0).unwrap());
        let track = LocalTrack {
            id: "same-recording".into(),
            duration: 12.0,
            ..Default::default()
        };
        assert_eq!(
            e.local.replace(vec![track.clone(), track], 0, false),
            Change::Start
        );
        e.action(Action::Pause);
        let revision = Arc::new(AtomicU64::new(1));
        let seek = |room: &str, observed, start_ms| {
            Command::SeekLyric {
                room: room.into(),
                snapshot: LyricSnapshot::capture(&revision, observed),
                start_ms,
            }
            .scoped(7)
        };
        let first = e.room_token().unwrap();
        e.command(seek(&first, 1, 2345));
        assert_eq!(e.local.elapsed, 0.0);
        assert!(!e.local.desired && e.socket.is_none() && e.media.is_none());

        let before_next = e.room_token().unwrap();
        // Natural repeat/advance has a different room revision even if the
        // same recording remains eligible and the old lyrics are retained.
        e.command(Command::Guarded(before_next.clone(), Action::Next).scoped(7));
        assert_eq!(e.local.position, Some(1));
        e.action(Action::Pause);
        e.command(seek(&before_next, 1, 9000));
        assert_eq!(e.local.elapsed, 0.0);

        let second = e.room_token().unwrap();
        let stale_model = seek(&second, 1, 9000);
        revision.store(2, Ordering::SeqCst);
        e.command(stale_model);
        e.command(seek(&second, 2, 12000));
        assert_eq!(e.local.elapsed, 0.0);
        assert_eq!(e.room_token().as_deref(), Some(second.as_str()));
        e.command(seek(&second, 2, 7));
        assert_eq!(e.local.elapsed, 0.0);
        assert_eq!(e.room_token().as_deref(), Some(second.as_str()));
        assert!(!e.local.desired);
        let before_account_change = e.room_token().unwrap();
        e.shared_epoch.store(8, Ordering::SeqCst);
        e.command(seek(&before_account_change, 2, 2000));
        assert_eq!(e.local.elapsed, 0.0);
    }

    #[tokio::test]
    async fn saved_queue_is_admitted_online_only_for_the_current_mode_generation() {
        let dir = std::env::temp_dir().join(format!("spofify-saved-test-{}", uuid::Uuid::new_v4()));
        use std::{fs, os::unix::fs::DirBuilderExt};
        fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
        struct Fixture(std::path::PathBuf);
        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        let fixture = Fixture(dir);
        let store = OfflineStore::open(&fixture.0).unwrap();
        let mut e = engine();
        let api = Arc::new(connection("http://127.0.0.1:1").unwrap());
        let session = api.test_session();
        e.audio = Some(Audio::new(
            Some(session.clone()),
            false,
            e.shared_epoch.clone(),
            e.epoch,
            e.master_gain.clone(),
        ));
        let (mixer, _samples) = rodio::mixer::mixer(1, 8000);
        e.audio
            .as_mut()
            .unwrap()
            .use_test_streaming_output(mixer, 1, 8000);
        e.account = Some(Account {
            api,
            session: session.clone(),
        });
        let item = crate::api::Item {
            id: "saved".into(),
            name: "Saved fixture".into(),
            kind: "track".into(),
            artists: String::new(),
            album_name: String::new(),
            duration_ms: 10000,
            has_audio: true,
            track_number: 1,
            disc_number: 1,
        };
        let command = |mode_stamp| {
            Command::PlaySaved {
                mode_stamp,
                queue: vec![LocalTrack {
                    id: item.id.clone(),
                    title: item.name.clone(),
                    duration: 10.0,
                    recording: Some(offline::Recording::fixture()),
                    ..Default::default()
                }],
                index: 0,
                // Invalid audio still admits the selected queue, then fails decoding.
                input: PlaybackInput {
                    item: item.clone(),
                    recording: offline::Recording::fixture(),
                    source: crate::audio_input::AudioInput::from(Arc::from([1])),
                },
                store: store.clone(),
            }
            .scoped(7)
        };
        let stamp = *session.network_changes().borrow();
        e.command(command(stamp.wrapping_add(2)));
        assert!(e.local.current().is_none());
        e.command(command(stamp));
        assert!(e.network_allowed());
        assert_eq!(e.local.current().unwrap().track.id, "saved");
        assert!(e.room_token().unwrap().starts_with("local:"));
        tokio::time::timeout(Duration::from_secs(2), async {
            while e.local_status == Status::Ok {
                e.tick_saved();
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(e.local_status, Status::Media);
        e.action(Action::Close);
        assert!(e.local_store.is_none());
        assert!(e.network_allowed());
    }
    #[tokio::test]
    async fn queue_saved_is_fenced_and_idle_without_native_work() {
        use std::{fs, os::unix::fs::DirBuilderExt};
        let dir = fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!("spofify-queue-test-{}", uuid::Uuid::new_v4()));
        fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
        let store = OfflineStore::open(&dir).unwrap();
        let mut e = engine();
        let api = Arc::new(connection("http://127.0.0.1:1").unwrap());
        let session = api.test_session();
        e.account = Some(Account {
            api,
            session: session.clone(),
        });
        assert!(e.can_queue_saved()); // before any account socket is admitted
        e.online.detached = true;
        assert!(!e.can_queue_saved()); // do not strand the retained native run
        e.online.detached = false;
        e.local_store = Some(store.clone());
        assert!(e.can_queue_saved());
        e.command(Command::Offline(true).scoped(7));
        let stamp = *session.network_changes().borrow();
        let add = |stamp| {
            Command::QueueSaved {
                mode_stamp: stamp,
                queue: vec![
                    LocalTrack {
                        id: "one".into(),
                        title: "One".into(),
                        duration: 2.0,
                        recording: Some(offline::Recording::fixture()),
                        ..Default::default()
                    };
                    2
                ],
                store: store.clone(),
            }
            .scoped(7)
        };
        e.command(add(stamp.wrapping_add(2)));
        assert!(e.local.entries.is_empty());
        e.command(add(stamp));
        assert_eq!(e.local.entries.len(), 2);
        assert_ne!(e.local.entries[0].key, e.local.entries[1].key);
        assert!(!e.local.desired && e.local.current().is_none() && e.media.is_none());
        let claim = e.saved_claim().unwrap();
        assert_eq!(claim.cursor.key, e.local.entries[0].key);
        assert!(claim.cursor.occurrence.is_some() && !claim.cursor.desired_playing);
        assert_eq!(claim.queue.len(), 2);
        assert!(e.socket.is_none() && e.queue_order_token().is_some());
        drop(e);
        fs::remove_dir_all(dir).unwrap();
    }
    #[tokio::test]
    async fn manual_saved_next_does_not_wait_for_a_retired_preview_read() {
        use std::{fs, os::unix::fs::DirBuilderExt, path::PathBuf};
        struct Fixture(PathBuf);
        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        let dir = Fixture(
            fs::canonicalize(std::env::temp_dir())
                .unwrap()
                .join(format!("spofify-crossfade-test-{}", uuid::Uuid::new_v4())),
        );
        fs::DirBuilder::new().mode(0o700).create(&dir.0).unwrap();
        let mut e = engine();
        let api = Arc::new(connection("http://127.0.0.1:1").unwrap());
        let session = api.test_session();
        e.account = Some(Account { api, session });
        e.command(Command::Offline(true).scoped(7));
        e.local_store = Some(OfflineStore::open(&dir.0).unwrap());
        let tracks = ["old", "new"]
            .into_iter()
            .map(|id| LocalTrack {
                id: id.into(),
                title: id.into(),
                subtitle: String::new(),
                album_name: String::new(),
                duration: 25.0,
                ..Default::default()
            })
            .collect();
        assert_eq!(e.local.replace(tracks, 0, false), Change::Start);
        let from = e.local.current().unwrap().key;
        let to = e.local.crossfade_successor().unwrap().key;
        e.local_lookahead = Some(crossfade::Lookahead::new(
            (from, to),
            std::future::pending(),
        ));
        // A blocking preview read can keep its permit after its receiver dies.
        let _preview_read = e.local_preview_slot.clone().acquire_owned().await.unwrap();
        let (results, mut replies) = mpsc::channel(1);
        e.results = results;
        e.action(Action::Next);
        assert!(e.local_lookahead.is_none());
        assert_eq!(e.local.current().unwrap().key, to);
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), replies.recv()).await,
            Ok(Some(Reply::OfflineMedia { input: None, .. }))
        ));
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
        e.retained = Some(Retention::new(LoginProfile::test_remembered()));
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
                manual_offline: false,
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
        assert!(!e.offline && e.account.is_none() && e.socket.is_none());
        assert!(e.access.borrow().is_none());
        assert!(session.is_offline()); // never reopened the expired internet credential
    }
    #[tokio::test]
    async fn local_cookie_deadline_does_not_revoke_retained_account_or_queue() {
        let mut e = engine();
        let api = Arc::new(connection("http://127.0.0.1:1").unwrap());
        let session = api.test_session_with_deadline(Instant::now() - Duration::from_secs(1));
        e.account = Some(Account {
            api,
            session: session.clone(),
        });
        e.retained = Some(Retention::new(LoginProfile::test_remembered()));
        assert_eq!(
            e.local.replace(
                vec![LocalTrack {
                    id: "saved".into(),
                    ..Default::default()
                }],
                0,
                false
            ),
            Change::Start
        );
        assert!(!session.can_access_local());
        e.tick();
        assert!(session.is_offline() && session.can_access_local() && !session.is_revoked());
        assert!(e.retained.as_ref().unwrap().profile().is_some());
        assert_eq!(e.local.current().unwrap().track.id, "saved");
        assert!(e.access.borrow().as_ref().unwrap().offline);
        assert!(e.reconnect.is_some());
        e.cancel_reconnect();
    }
    #[tokio::test]
    async fn explicit_sign_in_requires_going_online_first() {
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
        assert!(e.offline && e.epoch == 8 && e.socket.is_none() && e.login.is_none());
        e.auth_command(AuthCommand::GoOnline(8));
        assert!(!e.offline);
    }

    #[tokio::test]
    async fn manual_preference_survives_no_remember_sign_in_and_quit_without_granting_access() {
        let root = std::env::temp_dir().canonicalize().unwrap().join(uuid::Uuid::new_v4().to_string());
        let path = root.join("spofify-desktop/offline-mode-v2.json");
        let mut e = engine();
        e.manual_mode_path = Some(path.clone());
        let origin = crate::api::Origin::parse("http://127.0.0.1:1").unwrap();
        let api = Arc::new(Api::new(origin.clone()).unwrap());
        let session = api.test_session();
        e.auth_ready = Some((7, Success {
            api, session: session.clone(), origin, kind: Kind::Password { remember: false }, manual_offline: false,
        }));
        e.accept_login(7);
        assert!(e.account.is_some() && e.retained.is_none());
        let access = e.access.borrow().as_ref().unwrap().clone();
        e.mode.request(&access, true);
        e.apply_mode();
        assert!(session.is_offline() && e.socket.is_none());
        assert!(crate::api::preferences::manual_mode_at(&path));
        e.shutdown().await;

        let mut reopened = engine();
        reopened.manual_mode_path = Some(path.clone());
        reopened.start_restore();
        let task = reopened.login.take().unwrap();
        task.await.unwrap();
        assert!(reopened.offline && reopened.account.is_none() && reopened.access.borrow().is_none());
        assert!(reopened.socket.is_none() && reopened.reconnect.is_none());
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        assert!(*reopened.mode.update_network().borrow() == crate::mode::UpdateNetwork::Offline);
        reopened.auth_command(AuthCommand::GoOnline(7));
        assert!(!reopened.offline && !crate::api::preferences::manual_mode_at(&path));
        assert!(reopened.account.is_none());
        std::fs::remove_dir_all(root).unwrap();
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
        assert!(!e.offline && e.socket.is_none() && e.login.is_none());
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
            result: Ok(Rejoined::Existing),
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
        assert!(!e.offline && session.is_offline() && e.socket.is_none());
        let serial = e.reconnect.as_ref().unwrap().serial;
        e.reply(Reply::Reconnect {
            epoch: 7,
            serial,
            result: Ok(Rejoined::Existing),
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
                ..Default::default()
            }],
            0,
            false,
        );
        e.retained = Some(Retention::new(LoginProfile::test_remembered()));
        let token = e.room_token();
        let access = e.access.borrow().as_ref().unwrap().clone();
        for (status, expired) in [(403, false), (0, false), (0, true)] {
            e.mode.request(&access, false);
            e.tick();
            let serial = e.reconnect.as_ref().unwrap().serial;
            assert_eq!(e.room_token(), token);
            e.reply(Reply::Reconnect {
                epoch: 7,
                serial,
                result: Err(ApiError {
                    message: "No valid network session",
                    expired,
                    status,
                }),
            });
            assert_eq!(e.room_token(), token);
            assert_eq!(e.local.current().unwrap().track.id, "saved");
            assert!(session.is_offline() && session.can_access_local() && e.socket.is_none());
            assert!(!e.offline && e.recover_at.is_some());
            assert!(e.retained.as_ref().unwrap().profile().is_some());
            assert!(e.access.borrow().as_ref().unwrap().offline);
        }
        // The synthetic capability has no real keyring slot to stage.
        e.retained = None;
        e.mode.request(&access, false);
        e.tick();
        let serial = e.reconnect.as_ref().unwrap().serial;
        e.reply(Reply::Reconnect {
            epoch: 7,
            serial,
            result: Ok(Rejoined::Existing),
        });
        assert_eq!(e.local.current().unwrap().track.id, "saved");
        assert!(!session.is_offline());
        assert!(
            e.access.borrow().as_ref().unwrap().offline,
            "local remains visible until a playback snapshot"
        );
    }
    #[tokio::test]
    async fn auth_recheck_has_four_bounded_attempts() {
        let mode = ModeControl::default();
        let serial = mode.begin_automatic(7).unwrap();
        let mut count = 0;
        let result = reconnect_attempts(
            &mode,
            serial,
            || true,
            || {
                count += 1;
                std::future::ready(Err::<(), _>(ApiError {
                    message: "Temporary",
                    expired: false,
                    status: 503,
                }))
            },
        )
        .await;
        assert!(result.unwrap().is_err());
        assert_eq!(count, AUTH_ATTEMPTS);
    }
    #[tokio::test]
    async fn stay_offline_cancels_inflight_auth_probe() {
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
                std::future::pending::<Result<(), ApiError>>()
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
