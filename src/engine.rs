//! Single account actor. Network operations are cancellable tasks, never UI-thread work.
use crate::{
    AppWindow, TrackRow,
    api::{Api, ApiError, Item, Origin, Page, Session},
    audio::Audio,
    player::{self, Endpoint, Player},
};
use serde_json::json;
use slint::{Model, ModelRc, VecModel};
use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{sync::mpsc, task::JoinHandle};

pub enum Command {
    Login {
        epoch: u64,
        origin: String,
        username: String,
        password: String,
        lan: bool,
    },
    Logout(u64),
    Search {
        epoch: u64,
        sequence: u64,
        query: String,
    },
    Play(usize),
    Toggle,
    Stop,
    Shutdown,
}
enum ResultMessage {
    Login(u64, Arc<Api>, Result<Session, ApiError>),
    Search(u64, u64, Result<Page, ApiError>),
    Media(u64, u64, Result<Vec<u8>, ApiError>),
    Protocol(u64, player::Request, Result<serde_json::Value, ApiError>),
    Reconcile(u64, Result<serde_json::Value, ApiError>),
}
#[derive(Default)]
struct View {
    signed: bool,
    busy: bool,
    error: bool,
    retry: bool,
    status: String,
    label: String,
    rows: Vec<Item>,
    now_title: String,
    now_detail: String,
    player_status: String,
    playing: bool,
    controls: bool,
    can_stop: bool,
}
struct Account {
    api: Arc<Api>,
    session: Session,
    audio: Audio,
    player: Player,
}
struct Engine {
    epoch: u64,
    shared_epoch: Arc<AtomicU64>,
    search_sequence: u64,
    shared_search: Arc<AtomicU64>,
    view: View,
    account: Option<Account>,
    pending: Option<Item>,
    pending_authority: Option<player::Authority>,
    ready: Option<Vec<u8>>,
    play_sequence: u64,
    wants_claim: bool,
    login: Option<JoinHandle<()>>,
    search: Option<JoinHandle<()>>,
    media: Option<JoinHandle<()>>,
    protocol: Option<JoinHandle<()>>,
    logouts: Vec<JoinHandle<()>>,
    results: mpsc::Sender<ResultMessage>,
    window: slint::Weak<AppWindow>,
    next_poll: Instant,
}
fn connection(input: &str, lan: bool) -> Result<Api, ApiError> {
    let origin = Origin::parse(input)?;
    if origin.as_str().starts_with("http:") && !lan {
        return Err(ApiError {
            message: "Confirm trusted-LAN HTTP, or use HTTPS.",
            expired: false,
            status: 0,
        });
    }
    Api::new(origin)
}

fn abort(task: &mut Option<JoinHandle<()>>, draining: &mut Vec<JoinHandle<()>>) {
    if let Some(task) = task.take() {
        task.abort();
        draining.push(task);
    }
}
impl Engine {
    fn publish(&self) {
        let weak = self.window.clone();
        let epoch = self.epoch;
        let shared = self.shared_epoch.clone();
        let seq = self.search_sequence;
        let shared_search = self.shared_search.clone();
        let signed = self.view.signed;
        let busy = self.view.busy;
        let error = self.view.error;
        let retry = self.view.retry;
        let status = self.view.status.clone();
        let label = self.view.label.clone();
        let rows = self.view.rows.clone();
        let title = self.view.now_title.clone();
        let detail = self.view.now_detail.clone();
        let player_status = self.view.player_status.clone();
        let playing = self.view.playing;
        let controls = self.view.controls;
        let can_stop = self.view.can_stop;
        let _ = slint::invoke_from_event_loop(move || {
            if shared.load(Ordering::SeqCst) != epoch {
                return;
            }
            if let Some(ui) = weak.upgrade() {
                ui.set_signed_in(signed);
                if shared_search.load(Ordering::SeqCst) == seq || !signed {
                    ui.set_busy(busy);
                    ui.set_error(error);
                    ui.set_retry_visible(retry);
                    ui.set_status(status.into());
                    ui.set_result_label(label.into());
                    let next_rows = rows
                        .into_iter()
                        .map(|r| TrackRow {
                            title: r.name.into(),
                            subtitle: format!("{} · {}", r.artists, r.album_name).into(),
                            detail: format!(
                                "{}:{:02}",
                                r.duration_ms / 60000,
                                (r.duration_ms / 1000) % 60
                            )
                            .into(),
                            playable: r.has_audio,
                        })
                        .collect::<Vec<_>>();
                    // Preserve row widgets and keyboard focus across player refreshes.
                    let current = ui.get_rows();
                    if current.row_count() != next_rows.len()
                        || next_rows.iter().enumerate().any(|(i, row)| {
                            current.row_data(i).is_none_or(|old| {
                                old.title != row.title
                                    || old.subtitle != row.subtitle
                                    || old.detail != row.detail
                                    || old.playable != row.playable
                            })
                        })
                    {
                        ui.set_rows(ModelRc::new(VecModel::from(next_rows)));
                    }
                }
                ui.set_now_title(if title.is_empty() {
                    "Nothing playing".into()
                } else {
                    title.into()
                });
                ui.set_now_detail(detail.into());
                ui.set_player_status(player_status.into());
                ui.set_playing(playing);
                ui.set_can_control(controls);
                ui.set_can_stop(can_stop);
            }
        });
    }
    fn reset(&mut self, epoch: u64) {
        abort(&mut self.login, &mut self.logouts);
        abort(&mut self.search, &mut self.logouts);
        abort(&mut self.media, &mut self.logouts);
        abort(&mut self.protocol, &mut self.logouts);
        if let Some(mut a) = self.account.take() {
            a.player.shutdown(&mut a.audio);
            a.audio.clear();
            // Bounded logout requests are drained on normal shutdown. No automatic retries.
            self.logouts.push(tokio::spawn(async move {
                if !a.player.device_id().is_empty() {
                    let _ = a
                        .api
                        .player(
                            &a.session,
                            json!({"action":"leave", "deviceId":a.player.device_id()}),
                        )
                        .await;
                }
                let _ = a.api.logout(&a.session).await;
            }));
        }
        self.logouts.retain(|t| !t.is_finished());
        self.epoch = epoch;
        self.search_sequence = self.shared_search.load(Ordering::SeqCst);
        self.pending = None;
        self.pending_authority = None;
        self.ready = None;
        self.wants_claim = false;
        self.view = View::default();
    }
    fn api_error(&mut self, e: ApiError, player: bool) {
        if e.expired {
            self.reset(self.epoch);
            self.view.error = true;
            self.view.status = "Session expired. Sign in again.".into();
        } else if player {
            self.view.player_status = e.message.into();
            self.pending = None;
            self.ready = None;
            self.wants_claim = false;
        } else {
            self.view.busy = false;
            self.view.error = true;
            self.view.status = e.message.into();
            self.view.retry = self.account.is_some();
        }
    }
    async fn command(&mut self, c: Command) -> bool {
        match c {
            Command::Shutdown => {
                self.reset(self.shared_epoch.load(Ordering::SeqCst));
                return false;
            }
            Command::Logout(epoch) => {
                self.reset(epoch);
                self.view.status = "Signed out on this desktop.".into();
            }
            Command::Login {
                epoch,
                origin,
                username,
                password,
                lan,
            } => {
                self.reset(epoch);
                match connection(&origin, lan) {
                    Ok(api) => {
                        let api = Arc::new(api);
                        let tx = self.results.clone();
                        self.view.busy = true;
                        self.login = Some(tokio::spawn(async move {
                            let result = api.login(&username, &password).await;
                            drop(password);
                            let _ = tx.send(ResultMessage::Login(epoch, api, result)).await;
                        }));
                    }
                    Err(e) => self.api_error(e, false),
                }
            }
            Command::Search {
                epoch,
                sequence,
                query,
            } if epoch == self.epoch => {
                self.search_sequence = sequence;
                abort(&mut self.search, &mut self.logouts);
                self.view.rows.clear();
                self.view.status.clear();
                self.view.error = false;
                self.view.retry = false;
                if query.trim().is_empty() {
                    self.view.busy = false;
                    self.view.label = "Search to begin. Nothing is preloaded.".into();
                } else if let Some(a) = &self.account {
                    self.view.busy = true;
                    self.view.label.clear();
                    let (api, session, tx) =
                        (a.api.clone(), a.session.clone(), self.results.clone());
                    self.search = Some(tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_millis(350)).await;
                        let result = api.search(&session, &query).await;
                        let _ = tx
                            .send(ResultMessage::Search(epoch, sequence, result))
                            .await;
                    }));
                }
            }
            Command::Play(index) => {
                if self.view.busy
                    || self.search_sequence != self.shared_search.load(Ordering::SeqCst)
                {
                    return true;
                }
                if let Some(item) = self.view.rows.get(index).filter(|i| i.has_audio).cloned() {
                    abort(&mut self.media, &mut self.logouts);
                    self.ready = None;
                    self.play_sequence += 1;
                    self.pending = Some(item);
                    self.pending_authority = None;
                    self.wants_claim = true;
                    self.view.can_stop = true;
                    self.view.player_status = "Requesting local playback permission…".into();
                    self.next_poll = Instant::now();
                }
            }
            Command::Toggle => {
                self.play_sequence += 1;
                abort(&mut self.media, &mut self.logouts);
                self.pending = None;
                self.ready = None;
                self.wants_claim = false;
                if let Some(a) = &mut self.account {
                    let kind = if a.audio.snapshot()["playing"] == true {
                        "pause"
                    } else {
                        "resume"
                    };
                    if kind == "pause" {
                        // Silence immediately even while a poll is in flight.
                        a.audio.stop();
                    }
                    match a
                        .player
                        .local(json!({"type":kind}), &mut a.audio, Instant::now())
                    {
                        Ok(player::Status::Ok) => {
                            self.view.player_status = if kind == "pause" {
                                "Paused"
                            } else {
                                "Playing locally"
                            }
                            .into()
                        }
                        Ok(_) => {
                            self.view.player_status =
                                "Unable to resume. Select the track again.".into()
                        }
                        Err(e) => self.view.player_status = e.to_string(),
                    }
                }
            }
            Command::Stop => {
                self.play_sequence += 1;
                abort(&mut self.media, &mut self.logouts);
                self.pending = None;
                self.ready = None;
                self.wants_claim = false;
                if let Some(a) = &mut self.account {
                    let _ = a
                        .player
                        .local(json!({"type":"clear"}), &mut a.audio, Instant::now());
                    a.audio.clear();
                }
                self.view.now_title.clear();
                self.view.now_detail.clear();
                self.view.player_status = "Stopped".into();
            }
            _ => {}
        }
        self.publish();
        true
    }
    fn result(&mut self, r: ResultMessage) {
        let epoch = match &r {
            ResultMessage::Login(e, ..)
            | ResultMessage::Search(e, ..)
            | ResultMessage::Media(e, ..)
            | ResultMessage::Protocol(e, ..)
            | ResultMessage::Reconcile(e, ..) => *e,
        };
        if epoch != self.epoch || epoch != self.shared_epoch.load(Ordering::SeqCst) {
            return;
        }
        match r {
            ResultMessage::Login(epoch, api, Ok(session))
                if epoch == self.epoch && epoch == self.shared_epoch.load(Ordering::SeqCst) =>
            {
                self.login = None;
                self.view.signed = true;
                self.view.busy = false;
                self.view.label = "Search to begin. Nothing is preloaded.".into();
                let audio = Audio::new(session.clone(), self.shared_epoch.clone(), epoch);
                self.account = Some(Account {
                    api,
                    session,
                    audio,
                    player: Player::new(),
                });
            }
            ResultMessage::Login(epoch, _, Err(e)) if epoch == self.epoch => {
                self.login = None;
                self.api_error(e, false);
            }
            ResultMessage::Search(epoch, seq, result)
                if epoch == self.epoch
                    && seq == self.search_sequence
                    && seq == self.shared_search.load(Ordering::SeqCst) =>
            {
                self.search = None;
                self.view.busy = false;
                match result {
                    Ok(page) => {
                        self.view.label =
                            format!("{} shown · {} matches", page.items.len(), page.total);
                        self.view.status = if page.items.is_empty() {
                            "No tracks found. Try another search.".into()
                        } else {
                            String::new()
                        };
                        self.view.rows = page.items;
                    }
                    Err(e) => self.api_error(e, false),
                }
            }
            ResultMessage::Media(epoch, seq, result)
                if epoch == self.epoch && seq == self.play_sequence =>
            {
                self.media = None;
                match result {
                    Ok(bytes) => self.ready = Some(bytes),
                    Err(e) => self.api_error(e, true),
                }
            }
            ResultMessage::Reconcile(epoch, result) if epoch == self.epoch => {
                self.protocol = None;
                match result {
                    Ok(value) => {
                        if let Some(a) = &mut self.account {
                            if let Err(e) = a.player.reconcile(value, &mut a.audio, Instant::now())
                            {
                                self.view.player_status = e.to_string();
                            }
                        }
                    }
                    Err(e) => self.api_error(e, true),
                }
                self.next_poll = Instant::now() + player::POLL_INTERVAL;
            }
            ResultMessage::Protocol(epoch, request, result) if epoch == self.epoch => {
                self.protocol = None;
                match result {
                    Ok(value) => {
                        if let Some(a) = &mut self.account {
                            if let Err(e) =
                                a.player
                                    .consume(&request, value, &mut a.audio, Instant::now())
                            {
                                self.view.player_status = e.to_string();
                                self.pending = None;
                                self.ready = None;
                                self.wants_claim = false;
                            }
                            self.next_poll = Instant::now() + player::POLL_INTERVAL;
                        }
                    }
                    Err(e) => {
                        if let Some(a) = &mut self.account {
                            a.player.failed(&mut a.audio);
                        }
                        let conflict = e.status == 409;
                        let missing_device = e.status == 404 && request.body["action"] == "poll";
                        self.api_error(e, true);
                        if missing_device && let Some(a) = &mut self.account {
                            // A definite missing registration is not an uncertain write.
                            // Retire its authority; only a NEW explicit Play may register.
                            a.audio.clear();
                            a.player = Player::new();
                            self.pending_authority = None;
                            self.view.now_title.clear();
                            self.view.now_detail.clear();
                            self.view.player_status =
                                "Desktop registration expired. Select the track again.".into();
                        }
                        abort(&mut self.media, &mut self.logouts);
                        self.play_sequence += 1;
                        if conflict && let Some(a) = &self.account {
                            let (api, session, tx) =
                                (a.api.clone(), a.session.clone(), self.results.clone());
                            self.protocol = Some(tokio::spawn(async move {
                                let result = api.player_view(&session).await;
                                let _ = tx.send(ResultMessage::Reconcile(epoch, result)).await;
                            }));
                        }
                        self.next_poll = Instant::now() + player::POLL_INTERVAL;
                    }
                }
            }
            _ => {} // late work cannot reinstall sessions, search results or playback
        }
        self.publish();
    }
    fn tick(&mut self) {
        if self.epoch != self.shared_epoch.load(Ordering::SeqCst) {
            return;
        }
        if self
            .account
            .as_ref()
            .is_some_and(|a| a.session.is_expired())
        {
            self.reset(self.epoch);
            self.view.error = true;
            self.view.status = "Session expired. Sign in again.".into();
            self.publish();
            return;
        }
        let Some(a) = &mut self.account else {
            return;
        };
        let now = Instant::now();
        a.player.tick(&mut a.audio, now);
        if self.pending.is_some()
            && self
                .pending_authority
                .as_ref()
                .is_some_and(|permit| !permit.is_valid())
        {
            abort(&mut self.media, &mut self.logouts);
            self.play_sequence += 1;
            self.pending = None;
            self.ready = None;
            self.pending_authority = None;
            self.wants_claim = false;
            self.view.player_status = "Playback permission lost. Select the track again.".into();
        }
        let state = a.audio.snapshot();
        self.view.playing = state["playing"] == true;
        self.view.controls =
            state["queue"].as_array().is_some_and(|q| !q.is_empty()) && a.player.is_owner(now);
        self.view.can_stop =
            self.pending.is_some() || state["queue"].as_array().is_some_and(|q| !q.is_empty());
        if self.pending.is_none() && a.player.device_id().is_empty() {
            return;
        }
        if self.protocol.is_some() {
            return;
        }
        if let Some(item) = self.pending.clone() {
            if a.player.is_owner(now) && !a.player.pending_acks() {
                self.wants_claim = false;
                if let Some(bytes) = self.ready.take() {
                    a.audio.preload(item.id.clone(), bytes);
                    let track = json!({"id":item.id,"name":item.name,"artists":[{"id":"","name":item.artists}],"albumName":item.album_name,"albumId":"","duration":item.duration_ms as f64/1000.0});
                    let result = player::single_track(&track, self.play_sequence)
                        .and_then(|s| a.player.play(s, &mut a.audio, now));
                    self.view.now_title = item.name;
                    self.view.now_detail = item.artists;
                    self.view.player_status = match result {
                        Ok(player::Status::Ok) => "Playing locally".into(),
                        Ok(player::Status::Blocked) => {
                            "Audio output unavailable on this desktop.".into()
                        }
                        Ok(_) => "Unable to decode this track.".into(),
                        Err(e) => e.to_string(),
                    };
                    self.pending = None;
                } else if self.media.is_none() {
                    let (api, session, tx, epoch, seq) = (
                        a.api.clone(),
                        a.session.clone(),
                        self.results.clone(),
                        self.epoch,
                        self.play_sequence,
                    );
                    self.pending_authority = a.player.authority(now);
                    self.view.player_status = "Loading audio into memory…".into();
                    self.media = Some(tokio::spawn(async move {
                        let result = api.media(&session, &item.id).await;
                        let _ = tx.send(ResultMessage::Media(epoch, seq, result)).await;
                    }));
                }
            } else if !a.player.active_device_id().is_empty()
                && a.player.active_device_id() != a.player.device_id()
            {
                abort(&mut self.media, &mut self.logouts);
                self.play_sequence += 1;
                self.ready = None;
                self.pending = None;
                self.wants_claim = false;
                self.view.player_status="Another output is selected. Stop/release it there first; desktop will not take over.".into();
            }
        }
        if now >= self.next_poll {
            let request = if a.player.device_id().is_empty() {
                a.player.register("Spofify Desktop", now)
            } else if self.wants_claim && a.player.active_device_id().is_empty() {
                self.wants_claim = false;
                a.player.claim(&uuid::Uuid::new_v4().to_string(), now)
            } else {
                a.player.poll(&mut a.audio, now)
            };
            match request {
                Ok(request) => {
                    let (api, session, tx, epoch) = (
                        a.api.clone(),
                        a.session.clone(),
                        self.results.clone(),
                        self.epoch,
                    );
                    self.protocol = Some(tokio::spawn(async move {
                        let result = api.player(&session, request.body.clone()).await;
                        let _ = tx
                            .send(ResultMessage::Protocol(epoch, request, result))
                            .await;
                    }));
                }
                Err(e) => {
                    self.view.player_status = e.to_string();
                    self.pending = None;
                    self.wants_claim = false;
                }
            }
            self.next_poll = now + player::POLL_INTERVAL;
        }
    }
}

pub fn start(
    window: slint::Weak<AppWindow>,
    epoch: Arc<AtomicU64>,
    search: Arc<AtomicU64>,
) -> (mpsc::Sender<Command>, std::thread::JoinHandle<()>) {
    let (tx, mut rx) = mpsc::channel(32);
    let thread = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("network runtime");
        runtime.block_on(async move {
            let(results,mut replies)=mpsc::channel(16);
            let mut engine=Engine{epoch:0,shared_epoch:epoch,search_sequence:0,shared_search:search,view:View::default(),account:None,pending:None,pending_authority:None,ready:None,play_sequence:0,wants_claim:false,login:None,search:None,media:None,protocol:None,logouts:vec![],results,window,next_poll:Instant::now()};
            let mut timer=tokio::time::interval(Duration::from_millis(50));let mut refresh=0;
            loop {tokio::select!{
                command=rx.recv()=>match command {Some(c)=>if !engine.command(c).await{break},None=>{engine.reset(engine.shared_epoch.load(Ordering::SeqCst));break}},
                result=replies.recv()=>if let Some(r)=result{engine.result(r)},
                _=timer.tick()=>{engine.tick();refresh+=1;if refresh%5==0{engine.publish();}}
            }}
            for task in engine.logouts {let _=task.await;}
        });
        runtime.shutdown_timeout(Duration::from_secs(2));
    });
    (tx, thread)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn engine() -> (Engine, mpsc::Receiver<ResultMessage>) {
        let (results, rx) = mpsc::channel(16);
        (
            Engine {
                epoch: 1,
                shared_epoch: Arc::new(AtomicU64::new(1)),
                search_sequence: 2,
                shared_search: Arc::new(AtomicU64::new(2)),
                view: View::default(),
                account: None,
                pending: None,
                pending_authority: None,
                ready: None,
                play_sequence: 3,
                wants_claim: false,
                login: None,
                search: None,
                media: None,
                protocol: None,
                logouts: vec![],
                results,
                window: slint::Weak::default(),
                next_poll: Instant::now(),
            },
            rx,
        )
    }
    fn page(name: &str) -> Page {
        Page {
            items: vec![Item {
                id: name.into(),
                name: name.into(),
                kind: "track".into(),
                artists: "Fixture".into(),
                album_name: "Fixture".into(),
                duration_ms: 1000,
                has_audio: true,
                track_number: 1,
                disc_number: 1,
            }],
            total: 1,
            offset: 0,
            limit: 20,
        }
    }
    #[test]
    fn canonical_http_requires_consent() {
        for origin in [
            "http://127.0.0.1:1234",
            "HTTP://127.0.0.1:1234",
            "HtTp://192.168.1.2",
        ] {
            assert!(connection(origin, false).is_err());
            assert!(connection(origin, true).is_ok());
        }
        assert!(connection("https://example.com", false).is_ok());
    }
    #[test]
    fn actual_engine_latest_query_and_session_win() {
        let (mut e, _) = engine();
        e.result(ResultMessage::Search(1, 1, Ok(page("old"))));
        assert!(e.view.rows.is_empty());
        e.result(ResultMessage::Search(1, 2, Ok(page("current"))));
        assert_eq!(e.view.rows[0].name, "current");
        e.shared_search.store(3, Ordering::SeqCst);
        e.result(ResultMessage::Search(1, 2, Ok(page("late"))));
        assert_eq!(e.view.rows[0].name, "current");
        e.shared_epoch.store(2, Ordering::SeqCst);
        e.result(ResultMessage::Search(1, 3, Ok(page("old session"))));
        e.result(ResultMessage::Media(1, 3, Ok(vec![1, 2, 3])));
        assert!(e.ready.is_none());
        let api = Arc::new(connection("http://127.0.0.1:1", true).unwrap());
        e.result(ResultMessage::Login(1, api.clone(), Ok(api.test_session())));
        assert!(e.account.is_none());
    }
    #[tokio::test]
    async fn abort_is_drained_and_empty_query_cancels_real_debounce() {
        let (mut e, mut rx) = engine();
        let api = Arc::new(connection("http://127.0.0.1:1", true).unwrap());
        let session = api.test_session();
        e.account = Some(Account {
            api,
            audio: Audio::new(session.clone(), e.shared_epoch.clone(), 1),
            session,
            player: Player::new(),
        });
        e.command(Command::Search {
            epoch: 1,
            sequence: 3,
            query: "never sent".into(),
        })
        .await;
        assert!(e.search.is_some());
        e.command(Command::Search {
            epoch: 1,
            sequence: 4,
            query: "  ".into(),
        })
        .await;
        assert!(e.search.is_none());
        assert!(!e.view.busy);
        for task in e.logouts.drain(..) {
            assert!(task.await.unwrap_err().is_cancelled());
        }
        assert!(rx.try_recv().is_err());
    }
    #[tokio::test]
    async fn missing_device_retires_authority_and_waits_for_new_explicit_play() {
        let (mut e, _) = engine();
        let api = Arc::new(connection("http://127.0.0.1:1", true).unwrap());
        let session = api.test_session();
        let mut a = Account {
            api,
            audio: Audio::new(session.clone(), e.shared_epoch.clone(), 1),
            session,
            player: Player::new(),
        };
        let now = Instant::now();
        let request = a.player.register("Synthetic", now).unwrap();
        a.player.consume(&request, json!({"deviceId":"old", "devices":[{"deviceId":"old"}],"activeDeviceId":"old","epoch":1,"leaseMs":8000,"state":null,"stateSeq":0,"transfer":null,"directives":[]}), &mut a.audio, now).unwrap();
        let permit = a.player.authority(now).unwrap();
        let poll = a.player.poll(&mut a.audio, now).unwrap();
        e.account = Some(a);
        e.pending = Some(page("track").items.remove(0));
        e.pending_authority = Some(permit.clone());
        e.media = Some(tokio::spawn(std::future::pending()));
        e.result(ResultMessage::Protocol(
            1,
            poll,
            Err(ApiError {
                status: 404,
                expired: false,
                message: "missing",
            }),
        ));
        assert!(!permit.is_valid());
        assert!(e.pending.is_none() && e.media.is_none());
        assert!(e.account.as_ref().unwrap().player.device_id().is_empty());
        e.tick();
        assert!(e.protocol.is_none());
        e.view.rows = page("track").items;
        e.command(Command::Play(0)).await;
        assert!(e.pending.is_some());
        let a = e.account.as_mut().unwrap();
        assert_eq!(
            a.player.register("Synthetic", now).unwrap().body["action"],
            "register"
        );
        for task in e.logouts {
            assert!(task.await.unwrap_err().is_cancelled());
        }
    }

    #[tokio::test]
    async fn stop_cancels_pending_media_and_stale_bytes_do_not_start() {
        let (mut e, _) = engine();
        e.pending = Some(page("track").items.remove(0));
        e.media = Some(tokio::spawn(std::future::pending()));
        e.command(Command::Stop).await;
        assert!(e.pending.is_none());
        assert!(e.media.is_none());
        e.result(ResultMessage::Media(1, 3, Ok(vec![1])));
        assert!(e.ready.is_none());
        for task in e.logouts {
            assert!(task.await.unwrap_err().is_cancelled());
        }
    }
}
