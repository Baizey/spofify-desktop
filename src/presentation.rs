//! On-demand Now Playing media. Keep the `install` guard alive through `ui.run()`.
//! The UI owns the active track ID and visibility; Access owns the account and
//! offline fence. The worker never infers a track ID from presentation text.

use crate::{
    AppWindow,
    api::{Api, ApiError, Session},
    desk::Access,
    media,
};
use slint::{ComponentHandle, ModelRc, SharedPixelBuffer, Timer, TimerMode, VecModel};
use std::{
    future::Future,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};
use tokio::sync::{oneshot, watch};

pub(crate) type AccessRx = watch::Receiver<Option<Access>>;

#[derive(Clone)]
pub(crate) struct Account {
    pub(crate) epoch: u64,
    pub(crate) api: Arc<Api>,
    pub(crate) session: Session,
    pub(crate) offline: bool,
    id: String,
    network: u64,
}

impl Account {
    pub(crate) fn current(rx: &AccessRx) -> Option<Self> {
        Self::from_access(rx.borrow().as_ref()?)
    }

    fn from_access(access: &Access) -> Option<Self> {
        let (_, id) = access.api.authenticated_account(&access.session).ok()?;
        let network = *access.session.network_changes().borrow();
        Some(Self {
            epoch: access.epoch,
            api: access.api.clone(),
            session: access.session.clone(),
            offline: access.offline || network & 1 != 0,
            id: id.to_owned(),
            network,
        })
    }

    // An offline transition does not change account identity. Re-authentication
    // always does, even when it happens to the same account in the same origin.
    pub(crate) fn same(&self, other: &Self) -> bool {
        self.epoch == other.epoch && Arc::ptr_eq(&self.api, &other.api) && self.id == other.id
    }

    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    fn same_network(&self, other: &Self) -> bool {
        self.same(other) && self.offline == other.offline && self.network == other.network
    }

    pub(crate) fn matches(&self, rx: &AccessRx) -> bool {
        rx.has_changed().is_ok() && Self::current(rx).is_some_and(|a| self.same_network(&a))
    }
}

// Recent-list notifications share Access but must not invalidate unrelated requests.
pub(crate) async fn scope_changed(rx: &mut AccessRx, previous: Option<&Account>) -> bool {
    let updated = rx.wait_for(|value| {
        let account = value.as_ref().and_then(Account::from_access);
        match (previous, account) {
            (Some(a), Some(b)) => !a.same_network(&b),
            (None, None) => false,
            _ => true,
        }
    });
    if let Some(a) = previous {
        let mut net = a.session.network_changes();
        tokio::select! {
            result = updated => result.is_ok(),
            _ = net.wait_for(|n| *n != a.network) => true,
            _ = session_ended(&a.session) => true,
        }
    } else {
        updated.await.is_ok()
    }
}

pub(crate) struct Worker {
    stop: Option<oneshot::Sender<()>>,
    join: Option<thread::JoinHandle<()>>,
}

impl Worker {
    pub(crate) fn start<F>(name: &str, make: impl FnOnce() -> F + Send + 'static) -> Self
    where
        F: Future<Output = ()> + 'static,
    {
        let (stop, cancel) = oneshot::channel();
        let join = thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                    .max_blocking_threads(2)
                    .enable_all()
                    .build()
                else {
                    return;
                };
                runtime.block_on(async move {
                    tokio::select! { biased; _ = cancel => {}, _ = make() => {} }
                });
            })
            .ok();
        Self {
            stop: Some(stop),
            join,
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

async fn session_ended(session: &Session) {
    while !session.is_expired() {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

pub(crate) const TIMEOUT: ApiError = ApiError {
    message: "The request timed out.",
    expired: false,
    status: 0,
};

pub(crate) async fn read_with_backoff<T>(
    mut read: impl AsyncFnMut() -> Result<T, ApiError>,
) -> Result<T, ApiError> {
    for delay in [250, 750] {
        match tokio::time::timeout(Duration::from_secs(5), read())
            .await
            .unwrap_or(Err(TIMEOUT))
        {
            Err(e) if !e.expired && (matches!(e.status, 0 | 408 | 429) || e.status >= 500) => {
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }
            outcome => return outcome,
        }
    }
    tokio::time::timeout(Duration::from_secs(5), read())
        .await
        .unwrap_or(Err(TIMEOUT))
}

pub(crate) struct Presentation {
    _observer: Timer,
    _worker: Worker,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Demand {
    track: String,
    visible: bool,
}

fn demand(ui: &AppWindow) -> Demand {
    Demand {
        track: ui.get_now_track_id().to_string(),
        visible: ui.get_page_kind() == "now",
    }
}

fn clear(ui: &AppWindow) {
    ui.set_cover_art(Default::default());
    ui.set_lyrics_text(Default::default());
    ui.set_waveform(Default::default());
    ui.set_media_status(Default::default());
}

/// UI-thread install. The timer observes local Slint state only; it does not
/// poll the server. A result also checks live Slint state at delivery, so the
/// observation interval can never let data from a different track publish.
pub(crate) fn install(ui: &AppWindow, access: AccessRx) -> Presentation {
    clear(ui);
    let initial = demand(ui);
    let (tx, rx) = watch::channel(initial.clone());
    let weak = ui.as_weak();
    let observer = Timer::default();
    let shown = Arc::new(Mutex::new((
        Account::current(&access),
        initial.track.clone(),
    )));
    {
        let (access, shown) = (access.clone(), shown.clone());
        let previous = std::cell::RefCell::new(initial);
        observer.start(TimerMode::Repeated, Duration::from_millis(50), move || {
            let Some(ui) = weak.upgrade() else { return };
            let next = demand(&ui);
            bind_display(&ui, &shown, Account::current(&access), &next.track);
            let mut previous = previous.borrow_mut();
            if next != *previous {
                tx.send_replace(next.clone());
                *previous = next;
            }
        });
    }
    let weak = ui.as_weak();
    let worker = Worker::start("desktop-presentation", move || run(weak, access, rx, shown));
    Presentation {
        _observer: observer,
        _worker: worker,
    }
}

type DisplayBinding = Arc<Mutex<(Option<Account>, String)>>;

fn bind_display(ui: &AppWindow, shown: &DisplayBinding, account: Option<Account>, track: &str) {
    let mut shown = shown.lock().unwrap();
    let same_account = match (&shown.0, &account) {
        (Some(a), Some(b)) => a.same(b),
        (None, None) => true,
        _ => false,
    };
    if !same_account || shown.1 != track {
        *shown = (account, track.to_owned());
        drop(shown);
        clear(ui);
    }
}

struct Projection {
    art: Option<media::Artwork>,
    lyrics: Option<String>,
    waveform: Option<media::Envelope>,
    status: &'static str,
    settled: bool,
}

async fn load(account: &Account, id: &str, expanded: bool) -> Projection {
    if !expanded {
        let art =
            read_with_backoff(async || media::artwork(&account.api, &account.session, id).await)
                .await;
        return Projection {
            settled: art.is_ok(),
            art: art.ok().flatten(),
            lyrics: None,
            waveform: None,
            status: "",
        };
    }
    let (art, lyrics, waveform) = tokio::join!(
        read_with_backoff(async || media::artwork(&account.api, &account.session, id).await),
        read_with_backoff(async || media::lyrics(&account.api, &account.session, id).await),
        read_with_backoff(async || media::waveform(&account.api, &account.session, id).await),
    );
    let settled = art.is_ok()
        && lyrics.as_ref().is_ok_and(|v| {
            !matches!(
                v.state,
                media::LyricsState::Pending | media::LyricsState::Error
            )
        })
        && waveform.as_ref().is_ok_and(|v| {
            !matches!(
                v.state,
                media::WaveformState::Queued
                    | media::WaveformState::Running
                    | media::WaveformState::Stale
                    | media::WaveformState::Error
                    | media::WaveformState::Timeout
                    | media::WaveformState::Cancelled
            )
        });
    let art = art.ok().flatten();
    let lyrics = lyrics.ok().and_then(lyric_text);
    let waveform = waveform.ok().and_then(|v| v.envelope);
    let usable = art.is_some() as usize + lyrics.is_some() as usize + waveform.is_some() as usize;
    Projection {
        art,
        lyrics,
        waveform,
        status: match usable {
            3 => "",
            0 if settled => "No cover art, lyrics, or waveform are available.",
            0 => "Now Playing media is unavailable.",
            _ => "Some Now Playing media is unavailable.",
        },
        settled,
    }
}

fn lyric_text(lyrics: media::Lyrics) -> Option<String> {
    let mut text = match lyrics.state {
        media::LyricsState::Plain => lyrics.plain_text?,
        media::LyricsState::Timed => lyrics
            .lines
            .into_iter()
            .map(|line| line.text)
            .collect::<Vec<_>>()
            .join("\n"),
        media::LyricsState::Instrumental => "Instrumental".to_owned(),
        _ => return None,
    };
    if let Some(attribution) = lyrics.attribution {
        text.push_str("\n\nLyrics source: ");
        text.push_str(&attribution.name);
    }
    Some(text)
}

async fn run(
    ui: slint::Weak<AppWindow>,
    mut access: AccessRx,
    mut wanted: watch::Receiver<Demand>,
    shown: DisplayBinding,
) {
    // This key keeps the last complete/missing response for this track while
    // the pane is hidden. No catalog-wide or on-disk media cache exists here.
    let mut satisfied: Option<(Account, String, bool, &'static str, Arc<AtomicBool>)> = None;
    loop {
        access.mark_unchanged();
        let account = Account::current(&access);
        let request = wanted.borrow_and_update().clone();
        if !satisfied
            .as_ref()
            .is_some_and(|(a, track, full, _, delivered)| {
                account.as_ref().is_some_and(|b| a.same(b))
                    && track == &request.track
                    && (*full || !request.visible)
                    && delivered.load(Ordering::Acquire)
            })
        {
            satisfied = None;
        }
        if let Some(account) = account.as_ref().filter(|_| !request.track.is_empty()) {
            let a_fence = access.clone();
            let r_fence = wanted.clone();
            if account.offline {
                publish(
                    &ui,
                    a_fence,
                    r_fence,
                    account.clone(),
                    request.clone(),
                    shown.clone(),
                    None,
                    "Offline",
                    None,
                );
            } else if satisfied.is_none() {
                // Bias invalidation before work, including an already queued access change.
                let projected = tokio::select! {
                    biased;
                    change = scope_changed(&mut access, Some(account)) => { if !change { break } continue; }
                    change = wanted.changed() => { if change.is_err() { break } continue; }
                    data = load(account, &request.track, request.visible) => data,
                };
                let status = projected.status;
                let delivered = projected.settled.then(|| Arc::new(AtomicBool::new(false)));
                if let Some(delivered) = &delivered {
                    satisfied = Some((
                        account.clone(),
                        request.track.clone(),
                        request.visible,
                        status,
                        delivered.clone(),
                    ));
                }
                publish(
                    &ui,
                    a_fence,
                    r_fence,
                    account.clone(),
                    request.clone(),
                    shown.clone(),
                    Some(projected),
                    status,
                    delivered,
                );
            } else if let Some((_, _, _, status, _)) = &satisfied {
                publish(
                    &ui,
                    a_fence,
                    r_fence,
                    account.clone(),
                    request.clone(),
                    shown.clone(),
                    None,
                    status,
                    None,
                );
            }
        }
        tokio::select! {
            change = scope_changed(&mut access, account.as_ref()) => { if !change { break } }
            change = wanted.changed() => { if change.is_err() { break } }
        }
    }
}

fn publish(
    ui: &slint::Weak<AppWindow>,
    access: AccessRx,
    wanted: watch::Receiver<Demand>,
    account: Account,
    request: Demand,
    shown: DisplayBinding,
    projected: Option<Projection>,
    status: &'static str,
    delivered: Option<Arc<AtomicBool>>,
) {
    let _ = ui.upgrade_in_event_loop(move |ui| {
        if !account.matches(&access)
            || !matches!(wanted.has_changed(), Ok(false))
            || demand(&ui) != request
        {
            return;
        }
        bind_display(&ui, &shown, Some(account), &request.track);
        let mut complete = true;
        if let Some(data) = projected {
            if let Some(art) = data.art {
                let buffer = SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                    &art.rgba, art.width, art.height,
                );
                ui.set_cover_art(slint::Image::from_rgba8(buffer));
            }
            if let Some(lyrics) = data.lyrics {
                ui.set_lyrics_text(lyrics.into());
            }
            if let Some(envelope) = data.waveform {
                let actual = Duration::try_from_secs_f32(ui.get_duration()).ok();
                if actual.is_some_and(|duration| envelope.matches_duration(duration)) {
                    let waveform = envelope
                        .bins
                        .into_iter()
                        .map(|bin| f32::from(bin) / 255.0)
                        .collect::<Vec<_>>();
                    ui.set_waveform(ModelRc::new(VecModel::from(waveform)));
                } else {
                    complete = false;
                }
            }
        }
        ui.set_media_status(if complete || !status.is_empty() {
            status.into()
        } else {
            "Waveform unavailable for this playback duration.".into()
        });
        if let Some(delivered) = delivered {
            delivered.store(complete, Ordering::Release);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Origin;

    #[test]
    fn scope_ignores_recent_updates_but_fences_even_quick_offline_roundtrips() {
        let api = Arc::new(Api::new(Origin::parse("https://example.invalid").unwrap()).unwrap());
        let session = api.test_session();
        let mut access = Access {
            epoch: 7,
            api,
            session: session.clone(),
            offline: false,
            recent_change: 0,
        };
        let (tx, rx) = watch::channel(Some(access.clone()));
        let account = Account::current(&rx).unwrap();
        access.recent_change += 1;
        tx.send_replace(Some(access.clone()));
        assert!(account.matches(&rx));
        session.set_offline(true);
        assert!(!account.matches(&rx));
        assert!(Account::current(&rx).unwrap().offline);
        session.set_offline(false);
        assert!(!account.matches(&rx));
        let resumed = Account::current(&rx).unwrap();
        assert!(account.same(&resumed));
        assert!(resumed.matches(&rx));
        access.epoch += 1;
        tx.send_replace(Some(access));
        assert!(!resumed.matches(&rx));
        assert!(!resumed.same(&Account::current(&rx).unwrap()));
    }
}
