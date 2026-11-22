//! On-demand Now Playing media. Keep the `install` guard alive through `ui.run()`.
//! The UI owns the active track ID and visibility; Access owns the account and
//! offline fence. The worker never infers a track ID from presentation text.

use crate::{
    AppWindow, TimedLyricLine,
    api::{Api, ApiError, Session},
    desk::Access,
    media,
};
use slint::{ComponentHandle, Model, ModelRc, SharedPixelBuffer, Timer, TimerMode, VecModel};
use std::{
    future::Future,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
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

async fn read_with_backoff<T>(
    mut read: impl AsyncFnMut() -> Result<T, ApiError>,
    should_retry: impl Fn(ApiError) -> bool,
) -> Result<T, ApiError> {
    for delay in [250, 750] {
        match tokio::time::timeout(Duration::from_secs(5), read())
            .await
            .unwrap_or(Err(TIMEOUT))
        {
            Err(e) if !e.expired && should_retry(e) => {
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }
            outcome => return outcome,
        }
    }
    tokio::time::timeout(Duration::from_secs(5), read())
        .await
        .unwrap_or(Err(TIMEOUT))
}

// The dock is much narrower than the service's envelope. Keep evenly timed
// peak buckets rather than overlapping thousands of individual bar items.
fn timeline_peaks(bins: &[u8]) -> Vec<f32> {
    let count = bins.len().min(128);
    (0..count)
        .map(|i| {
            let start = i * bins.len() / count;
            let end = (i + 1) * bins.len() / count;
            f32::from(bins[start..end].iter().copied().max().unwrap_or(0)) / 255.0
        })
        .collect()
}

fn retry_artwork_or_waveform(error: ApiError) -> bool {
    matches!(error.status, 0 | 408 | 429) || error.status >= 500
}

fn retry_lyrics_read(error: ApiError) -> bool {
    // Status zero also covers permanent local failures and invalid responses.
    // Don't guess at a server's rate-limit cooldown (429).
    error == TIMEOUT
        || error.is_retryable_lyric_transport_failure()
        || crate::api_transport::is_transient_status(error.status)
}

pub(crate) struct Presentation {
    _observer: Timer,
    _worker: Worker,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Demand {
    track: String,
    visible: bool,
    display_revision: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LyricDemand {
    view: Demand,
    occurrence: String,
    open_generation: i32,
    available: bool,
}

impl LyricDemand {
    fn from_ui(ui: &AppWindow, view: Demand) -> Self {
        let available = ui.get_lyrics_track_id() == view.track
            && (!ui.get_lyrics_text().is_empty() || ui.get_lyrics_lines().row_count() != 0);
        Self {
            view,
            occurrence: ui.get_media_track_token().to_string(),
            open_generation: ui.get_lyrics_open_generation(),
            available,
        }
    }

    fn needs_read(&self) -> bool {
        self.view.visible && !self.view.track.is_empty() && !self.available
    }

    fn matches(&self, ui: &AppWindow) -> bool {
        ui.get_page_kind() == "now"
            && ui.get_now_track_id() == self.view.track
            && ui.get_media_track_token() == self.occurrence
            && ui.get_lyrics_open_generation() == self.open_generation
    }
}

fn demand(ui: &AppWindow, display_revision: u64) -> Demand {
    Demand {
        track: ui.get_now_track_id().to_string(),
        visible: ui.get_page_kind() == "now",
        display_revision,
    }
}

fn update_demand<T: PartialEq>(tx: &watch::Sender<T>, next: T) -> bool {
    tx.send_if_modified(|current| {
        if *current == next {
            return false;
        }
        *current = next;
        true
    })
}

pub(crate) fn clear(ui: &AppWindow) {
    ui.set_cover_art(Default::default());
    ui.set_lyrics_track_id(Default::default());
    ui.set_lyrics_text(Default::default());
    ui.set_lyrics_lines(Default::default());
    ui.set_lyrics_attribution(Default::default());
    ui.set_lyrics_status(Default::default());
    ui.set_waveform(Default::default());
    ui.set_media_status(Default::default());
}

/// UI-thread install. The timer observes local Slint state only; it does not
/// poll the server. A result also checks live Slint state at delivery, so the
/// observation interval can never let data from a different track publish.
pub(crate) fn install(ui: &AppWindow, access: AccessRx, epoch: Arc<AtomicU64>) -> Presentation {
    clear(ui);
    ui.on_resolve_active_lyric(displayed_lyric_line);
    let initial = demand(ui, 0);
    let (tx, rx) = watch::channel(initial.clone());
    let (lyrics_tx, lyrics_rx) = watch::channel(LyricDemand::from_ui(ui, initial.clone()));
    let weak = ui.as_weak();
    let observer = Timer::default();
    let shown = Arc::new(Mutex::new(DisplayedScope {
        account: Account::current(&access),
        track: initial.track.clone(),
        revision: initial.display_revision,
    }));
    {
        let (access, shown) = (access.clone(), shown.clone());
        observer.start(TimerMode::Repeated, Duration::from_millis(50), move || {
            let Some(ui) = weak.upgrade() else { return };
            let mut next = demand(&ui, 0);
            next.display_revision =
                bind_display(&ui, &shown, Account::current(&access), &next.track);
            let lyrics = LyricDemand::from_ui(&ui, next.clone());
            let reading = lyrics.needs_read();
            if update_demand(&lyrics_tx, lyrics) && reading {
                ui.set_lyrics_status(Default::default());
            }
            update_demand(&tx, next);
        });
    }
    let weak = ui.as_weak();
    let worker = Worker::start("desktop-presentation", move || async move {
        tokio::join!(
            run_media(weak.clone(), access.clone(), rx, shown.clone()),
            run_lyrics(weak, access, lyrics_rx, shown, epoch),
        );
    });
    Presentation {
        _observer: observer,
        _worker: worker,
    }
}

struct DisplayedScope {
    account: Option<Account>,
    track: String,
    revision: u64,
}

type DisplayBinding = Arc<Mutex<DisplayedScope>>;

impl DisplayedScope {
    fn rebind(&mut self, account: Option<Account>, track: &str) -> bool {
        let same_scope = match (&self.account, &account) {
            (Some(a), Some(b)) => a.same_network(b),
            (None, None) => true,
            _ => false,
        };
        if same_scope && self.track == track {
            return false;
        }
        self.account = account;
        self.track = track.to_owned();
        self.revision = self.revision.wrapping_add(1);
        true
    }
}

// A worker waking on Access must not start reads under a new scope using
// demand that the UI published for the preceding account or network.
fn demand_is_bound(shown: &DisplayBinding, request: &Demand, account: &Account) -> bool {
    let shown = shown.lock().unwrap();
    shown.revision == request.display_revision
        && shown
            .account
            .as_ref()
            .is_some_and(|a| a.same_network(account))
}

fn bind_display(
    ui: &AppWindow,
    shown: &DisplayBinding,
    account: Option<Account>,
    track: &str,
) -> u64 {
    let mut shown = shown.lock().unwrap();
    let changed = shown.rebind(account, track);
    let revision = shown.revision;
    drop(shown);
    if changed {
        clear(ui);
    }
    revision
}

struct Projection {
    art: Option<media::Artwork>,
    waveform: Option<media::Envelope>,
    status: &'static str,
    settled: bool,
}

async fn load_media(account: &Account, id: &str, expanded: bool) -> Projection {
    if !expanded {
        let art = read_with_backoff(
            async || media::artwork(&account.api, &account.session, id).await,
            retry_artwork_or_waveform,
        )
        .await;
        return Projection {
            settled: art.is_ok(),
            art: art.ok().flatten(),
            waveform: None,
            status: "",
        };
    }
    let (art, waveform) = tokio::join!(
        read_with_backoff(
            async || media::artwork(&account.api, &account.session, id).await,
            retry_artwork_or_waveform,
        ),
        read_with_backoff(
            async || media::waveform(&account.api, &account.session, id).await,
            retry_artwork_or_waveform,
        ),
    );
    let settled = art.is_ok()
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
    let waveform = waveform.ok().and_then(|v| v.envelope);
    let usable = art.is_some() as usize + waveform.is_some() as usize;
    Projection {
        art,
        waveform,
        status: match usable {
            2 => "",
            0 if settled => "No cover art or waveform is available.",
            0 => "Now Playing media is unavailable.",
            _ => "Some Now Playing media is unavailable.",
        },
        settled,
    }
}

#[derive(Default)]
struct LyricProjection {
    text: Option<String>,
    lines: Vec<media::LyricLine>,
    attribution: String,
    status: &'static str,
}

fn project_lyrics(result: Result<media::Lyrics, ApiError>) -> LyricProjection {
    use media::LyricsState::*;
    let mut projected = LyricProjection::default();
    projected.status = match result.as_ref().map(|lyrics| lyrics.state) {
        Ok(NotFound) | Err(ApiError { status: 404, .. }) => "No lyrics for this track.",
        Ok(Pending) => "Lyrics aren’t ready.",
        Ok(Plain | Timed | Instrumental) => "",
        Ok(Unavailable | Error) | Err(_) => "Lyrics unavailable.",
    };
    if let Ok(lyrics) = result {
        match lyrics.state {
            Timed => projected.lines = lyrics.lines,
            Plain => projected.text = lyrics.plain_text.map(lyric_display_text),
            Instrumental => projected.text = Some("Instrumental".into()),
            _ => return projected,
        }
        if let Some(attribution) = lyrics.attribution {
            projected.attribution = format!("Lyrics source: {}", attribution.name);
        }
    }
    projected
}

// Unpainted trailing spaces must not widen the viewport or erase hard breaks.
fn lyric_display_text(text: String) -> String {
    fn trim_padding(line: &str) -> &str {
        line.trim_end_matches(|ch: char| {
            ch.is_whitespace() && !matches!(ch, '\u{85}' | '\u{2028}' | '\u{2029}')
        })
    }
    if text
        .split('\n')
        .any(|line| trim_padding(line).len() != line.len())
    {
        text.split('\n')
            .map(trim_padding)
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        text
    }
}

fn lyric_model(lines: Vec<media::LyricLine>) -> ModelRc<TimedLyricLine> {
    let lines = lines
        .into_iter()
        .map(|line| TimedLyricLine {
            start_ms: line.start_ms as i32, // bounded by parse_lyrics
            text: lyric_display_text(line.text).into(),
        })
        .collect::<Vec<_>>();
    ModelRc::new(VecModel::from(lines))
}

// Called from the reader's binding with the same projection as the seek bar.
// The immutable row model is constructed only on delivery, not on clock ticks.
fn displayed_lyric_line(lines: ModelRc<TimedLyricLine>, elapsed: f32, duration: f32) -> i32 {
    let Some(last) = media::active_lyric_line(
        lines.row_count(),
        |i| u64::try_from(lines.row_data(i)?.start_ms).ok(),
        elapsed,
        duration,
    ) else {
        return -1;
    };
    let Some(latest) = lines.row_data(last) else {
        return -1;
    };
    // A stanza break still ends the previous timestamp, but is never sung.
    // Preserve a genuine lyric if the same timestamp also has an empty row.
    let timestamp = latest.start_ms;
    std::iter::once(latest)
        .chain((0..last).rev().map_while(|i| lines.row_data(i)))
        .take_while(|line| line.start_ms == timestamp)
        .position(|line| !line.text.trim().is_empty())
        .map_or(-1, |offset| (last - offset) as i32)
}

async fn run_lyrics(
    ui: slint::Weak<AppWindow>,
    mut access: AccessRx,
    mut wanted: watch::Receiver<LyricDemand>,
    shown: DisplayBinding,
    epoch: Arc<AtomicU64>,
) {
    loop {
        access.mark_unchanged();
        let account = Account::current(&access);
        let request = wanted.borrow_and_update().clone();
        if let Some(account) = account.as_ref().filter(|a| {
            !a.offline && request.needs_read() && demand_is_bound(&shown, &request.view, a)
        }) {
            let authority = access.clone();
            let result = tokio::select! {
                biased;
                change = scope_changed(&mut access, Some(account)) => { if !change { break } continue; }
                change = wanted.changed() => { if change.is_err() { break } continue; }
                result = read_with_backoff(
                    async || {
                        // The observer may not yet have seen a route or occurrence
                        // change right before this backoff elapsed. Check live UI
                        // state before issuing each GET, not just when publishing.
                        let (tx, live) = oneshot::channel();
                        let demand = request.clone();
                        let _ = ui.upgrade_in_event_loop(move |ui| {
                            let _ = tx.send(demand.matches(&ui));
                        });
                        if live.await != Ok(true)
                            || epoch.load(Ordering::SeqCst) != account.epoch
                            || !account.matches(&authority)
                        {
                            return Ok(None);
                        }
                        media::lyrics(&account.api, &account.session, &request.view.track)
                            .await
                            .map(Some)
                    },
                    retry_lyrics_read,
                ) => result,
            };
            if let Some(result) = result.transpose() {
                Delivery {
                    ui: ui.clone(),
                    access: access.clone(),
                    account: account.clone(),
                    shown: shown.clone(),
                }
                .lyrics(request, project_lyrics(result), epoch.clone());
            }
        }
        // A bounded read per demand, not polling. Valid DTOs (including pending
        // and error) are snapshots, not progress signals for a client-owned job.
        tokio::select! {
            change = scope_changed(&mut access, account.as_ref()) => { if !change { break } }
            change = wanted.changed() => { if change.is_err() { break } }
        }
    }
}

async fn run_media(
    ui: slint::Weak<AppWindow>,
    mut access: AccessRx,
    mut wanted: watch::Receiver<Demand>,
    shown: DisplayBinding,
) {
    // Keep the last complete artwork/waveform response for the current display.
    // Lyrics have their own demand and can publish or retry independently.
    let mut satisfied: Option<(Account, Demand, &'static str, Arc<AtomicBool>)> = None;
    loop {
        access.mark_unchanged();
        let account = Account::current(&access);
        let request = wanted.borrow_and_update().clone();
        if !satisfied.as_ref().is_some_and(|(a, loaded, _, delivered)| {
            account.as_ref().is_some_and(|b| a.same_network(b))
                && loaded.track == request.track
                && loaded.display_revision == request.display_revision
                && (loaded.visible || !request.visible)
                && delivered.load(Ordering::Acquire)
        }) {
            satisfied = None;
        }
        if let Some(account) = account
            .as_ref()
            .filter(|a| !request.track.is_empty() && demand_is_bound(&shown, &request, a))
        {
            let delivery = Delivery {
                ui: ui.clone(),
                access: access.clone(),
                account: account.clone(),
                shown: shown.clone(),
            };
            if account.offline {
                delivery.media(request, wanted.clone(), None, "Offline", None);
            } else if satisfied.is_none() {
                // Bias invalidation before work, including an already queued access change.
                let projected = tokio::select! {
                    biased;
                    change = scope_changed(&mut access, Some(account)) => { if !change { break } continue; }
                    change = wanted.changed() => { if change.is_err() { break } continue; }
                    data = load_media(account, &request.track, request.visible) => data,
                };
                let status = projected.status;
                let delivered = projected.settled.then(|| Arc::new(AtomicBool::new(false)));
                if let Some(delivered) = &delivered {
                    satisfied = Some((account.clone(), request.clone(), status, delivered.clone()));
                }
                delivery.media(request, wanted.clone(), Some(projected), status, delivered);
            } else if let Some((_, _, status, _)) = &satisfied {
                delivery.media(request, wanted.clone(), None, status, None);
            }
        }
        tokio::select! {
            change = scope_changed(&mut access, account.as_ref()) => { if !change { break } }
            change = wanted.changed() => { if change.is_err() { break } }
        }
    }
}

struct Delivery {
    ui: slint::Weak<AppWindow>,
    access: AccessRx,
    account: Account,
    shown: DisplayBinding,
}

impl Delivery {
    fn update(
        self,
        request: Demand,
        current: impl FnOnce(&AppWindow) -> bool + Send + 'static,
        apply: impl FnOnce(AppWindow, String) + Send + 'static,
    ) {
        let Self {
            ui,
            access,
            account,
            shown,
        } = self;
        let _ = ui.upgrade_in_event_loop(move |ui| {
            if !account.matches(&access) {
                return;
            }
            // Record a live track change even if the timer hasn't observed it.
            let live = ui.get_now_track_id();
            let revision = bind_display(&ui, &shown, Some(account), live.as_str());
            if live.as_str() == request.track
                && revision == request.display_revision
                && current(&ui)
            {
                apply(ui, request.track);
            }
        });
    }

    fn lyrics(self, request: LyricDemand, lyrics: LyricProjection, epoch: Arc<AtomicU64>) {
        let account_epoch = self.account.epoch;
        self.update(
            request.view.clone(),
            move |ui| epoch.load(Ordering::SeqCst) == account_epoch && request.matches(ui),
            move |ui, track| {
                ui.set_lyrics_text(lyrics.text.unwrap_or_default().into());
                ui.set_lyrics_lines(lyric_model(lyrics.lines));
                ui.set_lyrics_attribution(lyrics.attribution.into());
                ui.set_lyrics_status(lyrics.status.into());
                ui.set_lyrics_track_id(track.into());
            },
        );
    }

    fn media(
        self,
        request: Demand,
        wanted: watch::Receiver<Demand>,
        projected: Option<Projection>,
        status: &'static str,
        delivered: Option<Arc<AtomicBool>>,
    ) {
        let visible = request.visible;
        self.update(
            request,
            move |ui| {
                matches!(wanted.has_changed(), Ok(false))
                    && (ui.get_page_kind() == "now") == visible
            },
            move |ui, _| {
                let mut complete = true;
                if let Some(data) = projected {
                    if let Some(art) = data.art {
                        let buffer = SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                            &art.rgba, art.width, art.height,
                        );
                        ui.set_cover_art(slint::Image::from_rgba8(buffer));
                    }
                    if let Some(envelope) = data.waveform {
                        let actual = Duration::try_from_secs_f32(ui.get_duration()).ok();
                        if actual.is_some_and(|duration| envelope.matches_duration(duration)) {
                            ui.set_waveform(ModelRc::new(VecModel::from(timeline_peaks(
                                &envelope.bins,
                            ))));
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
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Origin;

    #[test]
    fn dock_waveform_keeps_peaks_at_both_edges_and_bounds_bar_count() {
        assert!(timeline_peaks(&[]).is_empty());
        assert_eq!(timeline_peaks(&[255, 0]), [1.0, 0.0]);
        let mut bins = vec![0; 2047];
        bins[0] = 255;
        bins[1015] = 128;
        bins[2046] = 64;
        let peaks = timeline_peaks(&bins);
        assert_eq!(peaks.len(), 128);
        assert_eq!(peaks[0], 1.0);
        assert_eq!(peaks[63], 128.0 / 255.0);
        assert_eq!(peaks[127], 64.0 / 255.0);
        assert_eq!(peaks.iter().filter(|&&peak| peak != 0.0).count(), 3);
    }

    #[test]
    fn a_display_rebind_invalidates_receipts_even_when_the_track_returns() {
        let mut scope = DisplayedScope {
            account: None,
            track: "first".into(),
            revision: 0,
        };
        assert!(scope.rebind(None, "second"));
        assert!(scope.rebind(None, "first"));
        assert!(!scope.rebind(None, "first"));
        assert_eq!(scope.revision, 2);
    }

    #[test]
    fn lyric_demand_only_revalidates_negatives_on_a_new_request() {
        let mut negative = LyricDemand {
            view: Demand {
                track: "same-track".into(),
                visible: true,
                display_revision: 1,
            },
            occurrence: "online:1:chain:1".into(),
            open_generation: 1,
            available: false,
        };
        let (tx, mut rx) = watch::channel(negative.clone());
        assert!(negative.needs_read());
        assert!(!update_demand(&tx, negative.clone())); // clock/observer tick
        negative.open_generation += 1; // reopen
        assert!(update_demand(&tx, negative.clone()));
        assert!(rx.borrow_and_update().needs_read());
        negative.occurrence = "online:1:chain:2".into();
        assert!(update_demand(&tx, negative.clone()));
        assert!(rx.borrow_and_update().needs_read());
        let mut accepted = negative;
        accepted.available = true;
        accepted.occurrence = "online:1:chain:3".into();
        accepted.open_generation += 1;
        assert!(!accepted.needs_read());
    }

    #[tokio::test]
    async fn lyric_reads_retry_only_transient_failures_with_a_fixed_budget() {
        let mut calls = 0;
        let unavailable = ApiError {
            status: 503,
            ..TIMEOUT
        };
        let result = read_with_backoff(
            async || {
                calls += 1;
                Err::<(), _>(unavailable)
            },
            retry_lyrics_read,
        )
        .await;
        assert_eq!(result, Err(unavailable));
        assert_eq!(calls, 3);

        for status in [0, 400, 401, 403, 404, 429, 501, 505, 511] {
            let failure = ApiError {
                message: "Permanent test failure",
                status,
                expired: status == 401,
            };
            let mut calls = 0;
            let result = read_with_backoff(
                async || {
                    calls += 1;
                    Err::<(), _>(failure)
                },
                retry_lyrics_read,
            )
            .await;
            assert_eq!(result, Err(failure));
            assert_eq!(calls, 1, "status {status}");
        }
    }

    #[test]
    fn lyrics_keep_missing_pending_and_failure_distinct() {
        let server_lyrics = |state| media::Lyrics {
            track_id: "track".into(),
            recording_version: "v1".into(),
            source_generation: "1".into(),
            state,
            plain_text: None,
            lines: Vec::new(),
            reason: None,
            provenance: None,
            attribution: None,
        };
        use media::LyricsState::*;
        assert_eq!(project_lyrics(Err(TIMEOUT)).status, "Lyrics unavailable.");
        assert_eq!(
            project_lyrics(Err(ApiError {
                status: 404,
                ..TIMEOUT
            }))
            .status,
            "No lyrics for this track."
        );
        for (state, expected) in [
            (Error, "Lyrics unavailable."),
            (Unavailable, "Lyrics unavailable."),
            (Pending, "Lyrics aren’t ready."),
            (NotFound, "No lyrics for this track."),
        ] {
            let result = project_lyrics(Ok(server_lyrics(state)));
            assert!(result.text.is_none());
            assert_eq!(result.status, expected);
            assert!(result.lines.is_empty());
        }
        let result = project_lyrics(Ok(server_lyrics(Instrumental)));
        assert_eq!(result.text.as_deref(), Some("Instrumental"));
        assert!(result.status.is_empty());
    }

    #[test]
    fn lyrics_project_clock_changes_and_replace_timed_rows_with_plain() {
        let mut lyrics = media::Lyrics {
            track_id: "track".into(),
            recording_version: "v1".into(),
            source_generation: "local-v1".into(),
            state: media::LyricsState::Timed,
            plain_text: Some("Alternate transcription".into()),
            lines: ["First   ", " \t", "Second", ""]
                .into_iter()
                .zip([500, 1000, 1500, 1500])
                .map(|(text, start_ms)| media::LyricLine {
                    start_ms,
                    text: text.into(),
                })
                .collect(),
            reason: None,
            provenance: None,
            attribution: Some(media::Attribution {
                name: "Fixture".into(),
                ownership: media::AttributionOwnership::Library,
                url: None,
            }),
        };
        let timed = project_lyrics(Ok(lyrics.clone()));
        assert!(timed.text.is_none());
        assert_eq!(timed.attribution, "Lyrics source: Fixture");
        let rows = lyric_model(timed.lines);
        for (position, active) in [
            (0.0, -1),
            (0.5, 0),
            (1.0, -1),
            (1.25, -1),
            (1.5, 2),
            (0.75, 0),
            (0.0, -1),
        ] {
            assert_eq!(displayed_lyric_line(rows.clone(), position, 2.0), active);
        }
        assert_eq!(rows.row_data(0).unwrap().text, "First");
        assert_eq!(rows.row_data(1).unwrap().text, "");
        assert_eq!(rows.row_data(2).unwrap().text, "Second");

        lyrics.state = media::LyricsState::Plain;
        lyrics.lines.clear();
        lyrics.plain_text = Some("Plain verse\t \n \n  Second stanza\n".into());
        let plain = project_lyrics(Ok(lyrics));
        assert_eq!(
            plain.text.as_deref(),
            Some("Plain verse\n\n  Second stanza\n")
        );
        assert_eq!(plain.attribution, "Lyrics source: Fixture");
        assert_eq!(displayed_lyric_line(lyric_model(plain.lines), 1.5, 2.0), -1);
    }

    #[test]
    fn scope_ignores_recent_updates_but_fences_even_quick_offline_roundtrips() {
        let api = Arc::new(Api::new(Origin::parse("https://example.invalid").unwrap()).unwrap());
        let session = api.test_session();
        let mut access = Access {
            epoch: 7,
            api,
            session: session.clone(),
            offline: false,
            personal: Default::default(),
        };
        let (tx, rx) = watch::channel(Some(access.clone()));
        let account = Account::current(&rx).unwrap();
        access.personal = Default::default();
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
