//! Catalog cover art for accepted desk snapshots. Desk publishes its watch value
//! (and clears both image properties) in the same UI callback as the new models.
//! An empty image lets Slint render its monogram; there is no fallback URL.

use std::{
    collections::VecDeque,
    io::Cursor,
    sync::{Arc, Mutex, Weak},
    time::{Duration, Instant},
};

use futures_util::{StreamExt, stream};
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader, Limits};
use slint::{ComponentHandle, Image, ModelRc, Rgba8Pixel, SharedPixelBuffer, VecModel};
use tokio::sync::{Semaphore, watch};

use crate::{
    AppWindow,
    api::catalog::CatalogKind,
    desk::Access,
    pages::{Page, Route, Row, RowKey},
    presentation::{Account, Worker},
};

const WINDOW: usize = 12;
const MAX_IN_FLIGHT: usize = 3;
const MAX_BODY: usize = 2 * 1024 * 1024;
const MAX_PIXELS: u64 = 4 * 1024 * 1024;
const MAX_DIMENSION: u32 = 4096;
const THUMBNAIL: u32 = 256;
const CACHE_BYTES: usize = 6 * 1024 * 1024;
const CACHE_ENTRIES: usize = 64;
const COVER_TTL: Duration = Duration::from_secs(600);
const MISSING_TTL: Duration = Duration::from_secs(180);

#[derive(Clone)]
pub(crate) struct Request {
    pub epoch: u64,
    pub generation: u64,
    pub route: Route,
    pub page: Arc<Page>,
}

type Requests = watch::Receiver<Option<Request>>;
type AccessRx = watch::Receiver<Option<Access>>;

impl Request {
    fn current(&self, requests: &Requests) -> bool {
        requests.has_changed().is_ok()
            && requests.borrow().as_ref().is_some_and(|next| {
                self.epoch == next.epoch
                    && self.generation == next.generation
                    && self.route == next.route
                    && Arc::ptr_eq(&self.page, &next.page)
            })
    }
}

pub(crate) struct Handle {
    // Drop in this order: queued UI callbacks lose authority before worker shutdown.
    _lifetime: Arc<()>,
    _worker: Worker,
}

/// Install on the UI thread; keep the handle alive through the Slint event loop.
/// Main owns the watch channel and desk owns the accepted-page sender.
pub(crate) fn install(ui: &AppWindow, access: AccessRx, requests: Requests) -> Handle {
    clear(ui);
    let lifetime = Arc::new(());
    let publisher = Publisher {
        ui: ui.as_weak(),
        lifetime: Arc::downgrade(&lifetime),
        access: access.clone(),
        requests: requests.clone(),
        pending: Arc::default(),
    };
    let worker = Worker::start("desktop-catalog-artwork", move || async move {
        // A decode already executing can't be interrupted. Its owned permit
        // prevents canceled generations from piling up more blocking decodes.
        let decode = Arc::new(Semaphore::new(1));
        run(
            access,
            requests,
            |update| publisher.post(update),
            async |account, key| fetch(account, key, &decode).await,
        )
        .await;
    });
    Handle {
        _lifetime: lifetime,
        _worker: worker,
    }
}

fn clear(ui: &AppWindow) {
    ui.set_page_artwork(Image::default());
    ui.set_browse_artwork(ModelRc::default());
}

/// A textual fallback, never a network key. Use the kind glyph if there is no
/// usable initial (e.g. an empty title or one consisting solely of symbols).
#[cfg(test)]
pub(crate) fn monogram(title: &str) -> String {
    title
        .split_whitespace()
        .filter_map(|word| word.chars().find(|c| c.is_alphanumeric()))
        .take(2)
        .filter_map(|c| c.to_uppercase().next())
        .collect()
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Key(CatalogKind, String);

impl Key {
    fn for_route(route: &Route) -> Option<Self> {
        match route {
            Route::Album(id) => Self::new(CatalogKind::Album, id),
            Route::Artist(id) => Self::new(CatalogKind::Artist, id),
            _ => None,
        }
    }

    fn for_row(row: &Row) -> Option<Self> {
        match &row.key {
            RowKey::Album(id) => Self::new(CatalogKind::Album, id),
            RowKey::Artist(id) => Self::new(CatalogKind::Artist, id),
            RowKey::Track(id) => Self::new(CatalogKind::Track, id),
            // The row key identifies a playback occurrence, not its recording.
            // Unavailable history entries have no recording ID.
            RowKey::HistoryEvent(_)
            | RowKey::ManualEntry { .. }
            | RowKey::DerivedOccurrence { .. }
                if crate::api::catalog::segment_id(&row.id) =>
            {
                Self::new(CatalogKind::Track, &row.id)
            }
            _ => None,
        }
    }

    fn new(kind: CatalogKind, id: &str) -> Option<Self> {
        // The transport validates and encodes this segment. Do not normalize IDs here.
        (!id.is_empty()).then(|| Self(kind, id.to_owned()))
    }
}

struct Targets {
    hero: Option<Key>,
    rows: Vec<Option<Key>>,
}

impl Targets {
    fn new(request: &Request) -> Self {
        Self {
            hero: Key::for_route(&request.route),
            rows: request
                .page
                .rows
                .iter()
                .take(WINDOW)
                .map(Key::for_row)
                .collect(),
        }
    }

    fn keys(&self) -> Vec<Key> {
        let mut unique = Vec::with_capacity(1 + self.rows.len());
        for key in self.hero.iter().chain(self.rows.iter().flatten()) {
            if !unique.contains(key) {
                unique.push(key.clone());
            }
        }
        unique
    }

    fn place(&self, frame: &mut Frame, key: &Key, cover: Pixels) {
        if self.hero.as_ref() == Some(key) {
            frame.hero = Some(cover.clone());
        }
        for (slot, target) in frame.rows.iter_mut().zip(&self.rows) {
            if target.as_ref() == Some(key) {
                *slot = Some(cover.clone());
            }
        }
    }
}

// Share immutable pixel buffers across the cache, mail slot and UI without
// copying them each time a different row finishes loading. Only the UI makes Images.
type Pixels = SharedPixelBuffer<Rgba8Pixel>;

#[derive(Clone)]
enum Cached {
    Cover(Pixels),
    Missing,
}

impl Cached {
    fn bytes(&self) -> usize {
        match self {
            Self::Cover(pixels) => pixels.as_bytes().len(),
            Self::Missing => 0,
        }
    }

    fn ttl(&self) -> Duration {
        match self {
            Self::Cover(_) => COVER_TTL,
            Self::Missing => MISSING_TTL,
        }
    }
}

struct Entry {
    key: Key,
    value: Cached,
    expires: Instant,
}

#[derive(Default)]
struct Cache {
    owner: Option<Account>,
    lru: VecDeque<Entry>,
    bytes: usize,
}

impl Cache {
    fn scope(&mut self, account: Option<&Account>) {
        if !self
            .owner
            .as_ref()
            .zip(account)
            .is_some_and(|(a, b)| a.same(b))
        {
            self.lru.clear();
            self.bytes = 0;
        }
        self.owner = account.cloned();
    }

    fn get(&mut self, key: &Key, now: Instant) -> Option<Cached> {
        let position = self.lru.iter().position(|entry| &entry.key == key)?;
        let entry = self.lru.remove(position)?;
        if now >= entry.expires {
            self.bytes -= entry.value.bytes();
            return None;
        }
        let value = entry.value.clone();
        self.lru.push_front(entry);
        Some(value)
    }

    fn insert(&mut self, key: Key, value: Cached, now: Instant) {
        if let Some(position) = self.lru.iter().position(|entry| entry.key == key)
            && let Some(entry) = self.lru.remove(position)
        {
            self.bytes -= entry.value.bytes();
        }
        self.bytes += value.bytes();
        self.lru.push_front(Entry {
            key,
            expires: now + value.ttl(),
            value,
        });
        while self.bytes > CACHE_BYTES || self.lru.len() > CACHE_ENTRIES {
            let entry = self.lru.pop_back().expect("cache bounds track its entries");
            self.bytes -= entry.value.bytes();
        }
    }
}

async fn fetch(account: &Account, key: Key, decode: &Arc<Semaphore>) -> Option<Cached> {
    if account.offline || account.session.is_offline() {
        return None;
    }
    let payload = account
        .api
        .catalog_artwork(&account.session, key.0, &key.1)
        .await
        .ok()?;
    let Some((mime, body)) = payload else {
        return Some(Cached::Missing);
    };
    let permit = decode.clone().acquire_owned().await.ok()?;
    let image = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        decode_artwork(&mime, &body)
    })
    .await
    .ok()?;
    // A definite 404 or an unusable image is negatively cached; auth failures,
    // canceled operations, and transient transport errors are not.
    Some(image.map_or(Cached::Missing, Cached::Cover))
}

fn decode_artwork(mime: &str, body: &[u8]) -> Option<Pixels> {
    if body.is_empty() || body.len() > MAX_BODY {
        return None;
    }
    let format = match mime {
        "image/png" => ImageFormat::Png,
        "image/jpeg" => ImageFormat::Jpeg,
        _ => return None,
    };
    let mut reader = ImageReader::with_format(Cursor::new(body), format);
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    limits.max_alloc = Some(48 * 1024 * 1024);
    reader.limits(limits);
    let decoder = reader.into_decoder().ok()?;
    let (width, height) = decoder.dimensions();
    if width == 0 || height == 0 || u64::from(width) * u64::from(height) > MAX_PIXELS {
        return None;
    }
    let image = DynamicImage::from_decoder(decoder).ok()?;
    let thumb = if width > THUMBNAIL || height > THUMBNAIL {
        image.thumbnail(THUMBNAIL, THUMBNAIL)
    } else {
        image
    }
    .into_rgba8();
    Some(Pixels::clone_from_slice(
        thumb.as_raw(),
        thumb.width(),
        thumb.height(),
    ))
}

#[derive(Clone, Default)]
struct Frame {
    hero: Option<Pixels>,
    rows: Vec<Option<Pixels>>,
}

struct Delivery {
    account: Account,
    request: Request,
    frame: Frame,
}

impl Delivery {
    fn current(&self, access: &AccessRx, requests: &Requests) -> bool {
        self.account.matches(access) && self.request.current(requests)
    }
}

// An invalidation has no frame. `None` for the outer Option is an empty mailbox.
enum Update {
    Show(Delivery),
    Invalidate,
}

struct Publisher {
    ui: slint::Weak<AppWindow>,
    lifetime: Weak<()>,
    access: AccessRx,
    requests: Requests,
    pending: Arc<Mutex<Option<Update>>>,
}

impl Publisher {
    fn post(&self, update: Update) {
        // Latest frame wins, with at most one queued Slint callback. A stalled
        // event loop cannot accumulate an unbounded queue of old cover pixels.
        if self.pending.lock().unwrap().replace(update).is_some() {
            return;
        }
        let (ui, lifetime) = (self.ui.clone(), self.lifetime.clone());
        let (access, requests) = (self.access.clone(), self.requests.clone());
        let pending = self.pending.clone();
        if slint::invoke_from_event_loop(move || {
            let update = pending.lock().unwrap().take();
            if lifetime.upgrade().is_none() {
                return;
            }
            let Some(ui) = ui.upgrade() else { return };
            match update {
                Some(Update::Show(delivery))
                    if ui.get_signed_in() && delivery.current(&access, &requests) =>
                {
                    let image = |cover: Option<Pixels>| {
                        cover.map_or_else(Image::default, Image::from_rgba8)
                    };
                    let hero = image(delivery.frame.hero.clone());
                    let rows = delivery
                        .frame
                        .rows
                        .iter()
                        .cloned()
                        .map(image)
                        .collect::<Vec<_>>();
                    // Recheck after materializing UI buffers, just before publishing.
                    if delivery.current(&access, &requests) {
                        ui.set_page_artwork(hero);
                        ui.set_browse_artwork(ModelRc::new(VecModel::from(rows)));
                    }
                }
                Some(Update::Invalidate) if !ui.get_signed_in() || !active(&access, &requests) => {
                    clear(&ui)
                }
                _ => {}
            }
        })
        .is_err()
        {
            self.pending.lock().unwrap().take();
        }
    }
}

fn active(access: &AccessRx, requests: &Requests) -> bool {
    access.has_changed().is_ok()
        && requests.has_changed().is_ok()
        && Account::current(access)
            .zip(requests.borrow().as_ref().map(|request| request.epoch))
            .is_some_and(|(account, epoch)| account.epoch == epoch)
}

async fn scope_changed(access: &mut AccessRx, account: Option<&Account>) -> bool {
    let mut network = account.map(|a| a.session.network_changes());
    loop {
        access.mark_unchanged();
        if access.has_changed().is_err() {
            return false;
        }
        let unchanged = match account {
            Some(account) => account.matches(access),
            None => Account::current(access).is_none(),
        };
        if !unchanged {
            return true;
        }
        if let Some(network) = &mut network {
            tokio::select! {
                _ = access.changed() => (),
                _ = network.changed() => return true,
                // Check revocation via Account::matches: an offline session
                // can still access local data beyond the network cookie deadline.
                _ = tokio::time::sleep(Duration::from_millis(100)) => (),
            }
        } else if access.changed().await.is_err() {
            return false;
        }
    }
}

async fn changed(
    access: &mut AccessRx,
    requests: &mut Requests,
    account: Option<&Account>,
) -> bool {
    tokio::select! {
        biased;
        open = scope_changed(access, account) => open,
        update = requests.changed() => update.is_ok(),
    }
}

async fn run(
    mut access: AccessRx,
    mut requests: Requests,
    emit: impl Fn(Update),
    fetch: impl AsyncFn(&Account, Key) -> Option<Cached>,
) {
    let mut cache = Cache::default();
    'pages: loop {
        if access.has_changed().is_err() || requests.has_changed().is_err() {
            break;
        }
        let account = Account::current(&access);
        let request = requests.borrow_and_update().clone();
        cache.scope(account.as_ref());
        if let Some((account, request)) = account
            .as_ref()
            .zip(request)
            .filter(|(account, request)| account.epoch == request.epoch)
        {
            let targets = Targets::new(&request);
            let mut frame = Frame {
                rows: vec![None; targets.rows.len()],
                ..Frame::default()
            };
            let mut missing = Vec::new();
            for key in targets.keys() {
                match cache.get(&key, Instant::now()) {
                    Some(Cached::Cover(pixels)) => targets.place(&mut frame, &key, pixels),
                    Some(Cached::Missing) => (),
                    None => missing.push(key),
                }
            }
            let show = |frame: &Frame| {
                emit(Update::Show(Delivery {
                    account: account.clone(),
                    request: request.clone(),
                    frame: frame.clone(),
                }))
            };
            show(&frame);
            if !account.offline {
                let fetch = &fetch;
                let mut results = stream::iter(missing.into_iter().map(|key| async move {
                    let value = fetch(account, key.clone()).await;
                    (key, value)
                }))
                .buffer_unordered(MAX_IN_FLIGHT);
                loop {
                    let result = tokio::select! {
                        biased;
                        open = changed(&mut access, &mut requests, Some(account)) => {
                            if !open { break 'pages; }
                            continue 'pages;
                        }
                        result = results.next() => result,
                    };
                    let Some((key, value)) = result else { break };
                    if !account.matches(&access) || !request.current(&requests) {
                        continue 'pages;
                    }
                    if let Some(value) = value {
                        cache.insert(key.clone(), value.clone(), Instant::now());
                        if let Cached::Cover(pixels) = value {
                            targets.place(&mut frame, &key, pixels);
                            show(&frame);
                        }
                    }
                }
            }
        } else {
            emit(Update::Invalidate);
        }
        if !changed(&mut access, &mut requests, account.as_ref()).await {
            break;
        }
    }
    emit(Update::Invalidate);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{Api, Origin};
    use image::{ImageBuffer, Rgb};
    use tokio::sync::mpsc;

    fn request(epoch: u64, generation: u64, route: Route, rows: Vec<Row>) -> Request {
        Request {
            epoch,
            generation,
            route,
            page: Arc::new(Page {
                rows,
                ..Page::default()
            }),
        }
    }

    fn access(epoch: u64) -> Access {
        let api = Arc::new(Api::new(Origin::parse("https://artwork.invalid").unwrap()).unwrap());
        Access {
            epoch,
            session: api.test_session(),
            api,
            offline: false,
            personal: Default::default(),
        }
    }

    #[test]
    fn plans_typed_targets_and_only_the_first_twelve_row_slots() {
        let mut rows = vec![
            Row {
                key: RowKey::Album("shared".into()),
                ..Row::default()
            },
            Row {
                key: RowKey::Artist("shared".into()),
                ..Row::default()
            },
            Row {
                id: "track%2Fid".into(),
                key: RowKey::HistoryEvent("event".into()),
                ..Row::default()
            },
            Row {
                kind: "track".into(),
                title: "Not an ID".into(),
                key: RowKey::HistoryEvent("metadata-missing".into()),
                ..Row::default()
            },
        ];
        rows.resize(WINDOW, Row::default());
        rows.push(Row {
            key: RowKey::Track("offscreen".into()),
            ..Row::default()
        });
        let targets = Targets::new(&request(1, 1, Route::Album("shared".into()), rows));
        assert_eq!(targets.rows.len(), WINDOW);
        let [album, artist, track] = targets.keys().try_into().unwrap();
        assert_eq!(album, Key(CatalogKind::Album, "shared".into()));
        assert_eq!(artist, Key(CatalogKind::Artist, "shared".into()));
        assert_eq!(track, Key(CatalogKind::Track, "track%2Fid".into()));
        assert!(targets.rows[3..].iter().all(Option::is_none));
        let mut frame = Frame {
            rows: vec![None; WINDOW],
            ..Frame::default()
        };
        targets.place(&mut frame, &album, Pixels::new(1, 1));
        assert!(frame.hero.is_some() && frame.rows[0].is_some());
        assert!(frame.rows[1..].iter().all(Option::is_none));
        assert!(Key::for_route(&Route::Playlist("shared".into())).is_none());
    }

    #[test]
    fn both_cache_limits_ttl_and_authenticated_owner_are_enforced() {
        let (tx, rx) = watch::channel(Some(access(1)));
        let account = Account::current(&rx).unwrap();
        let mut cache = Cache::default();
        cache.scope(Some(&account));
        let key = |id: usize| Key(CatalogKind::Album, id.to_string());
        let now = Instant::now();
        for id in 0..=CACHE_ENTRIES {
            cache.insert(key(id), Cached::Missing, now);
        }
        assert_eq!(cache.lru.len(), CACHE_ENTRIES);
        assert!(cache.get(&key(0), now).is_none());
        assert!(matches!(cache.get(&key(1), now), Some(Cached::Missing)));
        assert!(cache.get(&key(1), now + MISSING_TTL).is_none());
        let cover = Pixels::new(THUMBNAIL, THUMBNAIL);
        let cover_capacity = CACHE_BYTES / cover.as_bytes().len();
        for id in 0..=cover_capacity {
            cache.insert(key(id), Cached::Cover(cover.clone()), now);
        }
        assert_eq!(cache.bytes, CACHE_BYTES);
        assert!(cache.get(&key(0), now).is_none());
        assert!(matches!(
            cache.get(&key(cover_capacity), now),
            Some(Cached::Cover(_))
        ));
        assert!(cache.get(&key(cover_capacity), now + COVER_TTL).is_none());

        // Offline keeps the authenticated cache, but originates no network fetches.
        tx.send_modify(|a| a.as_mut().unwrap().offline = true);
        cache.scope(Account::current(&rx).as_ref());
        assert!(cache.get(&key(1), now).is_some());
        // Even if the epoch were accidentally reused, another API instance clears it.
        tx.send_replace(Some(access(1)));
        cache.scope(Account::current(&rx).as_ref());
        assert_eq!((cache.lru.len(), cache.bytes), (0, 0));
        cache.insert(key(1), Cached::Missing, now);
        tx.send_replace(None);
        cache.scope(Account::current(&rx).as_ref());
        assert!(cache.get(&key(1), now).is_none());
    }

    #[test]
    fn decoder_validates_format_and_limits_and_bounds_cached_pixels() {
        fn encoded(width: u32, height: u32, format: ImageFormat) -> Vec<u8> {
            let mut data = Cursor::new(Vec::new());
            ImageBuffer::from_pixel(width, height, Rgb([52u8, 62, 72]))
                .write_to(&mut data, format)
                .unwrap();
            data.into_inner()
        }
        let png = encoded(512, 256, ImageFormat::Png);
        let pixels = decode_artwork("image/png", &png).unwrap();
        assert_eq!((pixels.width(), pixels.height()), (256, 128));
        assert_eq!(&pixels.as_bytes()[..4], &[52, 62, 72, 255]);
        assert!(decode_artwork("image/jpeg", &png).is_none());
        assert!(decode_artwork("image/svg+xml", &png).is_none());
        assert!(decode_artwork("image/png", &png[..24]).is_none());
        assert!(decode_artwork("image/png", &vec![0; MAX_BODY + 1]).is_none());
        let wide = encoded(MAX_DIMENSION + 1, 1, ImageFormat::Png);
        assert!(decode_artwork("image/png", &wide).is_none());
        let jpeg = encoded(2, 1, ImageFormat::Jpeg);
        let pixels = decode_artwork("image/jpeg", &jpeg).unwrap();
        assert_eq!((pixels.width(), pixels.height()), (2, 1));
    }

    #[test]
    fn monograms_are_display_only_and_preserve_unicode_initials() {
        assert_eq!(monogram("  ‘Massive Attack’  "), "MA");
        assert_eq!(monogram("élan 東京"), "É東");
        assert_eq!(monogram("♫ …"), "");
    }

    #[tokio::test]
    async fn a_missing_hero_is_not_fetched_again_when_it_becomes_a_browse_row() {
        let (_access, access_rx) = watch::channel(Some(access(1)));
        let (tx, rx) = watch::channel(Some(request(1, 1, Route::Album("absent".into()), vec![])));
        let (fetched, mut calls) = mpsc::unbounded_channel();
        let (published, mut updates) = mpsc::unbounded_channel();
        let drive = run(
            access_rx,
            rx,
            |update| {
                let _ = published.send(update);
            },
            async |_, Key(_, id)| {
                let missing = id == "absent";
                fetched.send(id).unwrap();
                Some(if missing {
                    Cached::Missing
                } else {
                    Cached::Cover(Pixels::new(1, 1))
                })
            },
        );
        let exercise = async {
            assert_eq!(calls.recv().await.as_deref(), Some("absent"));
            tx.send_replace(Some(request(
                1,
                2,
                Route::Search("album".into()),
                vec![
                    Row {
                        key: RowKey::Album("absent".into()),
                        ..Row::default()
                    },
                    Row {
                        key: RowKey::Album("present".into()),
                        ..Row::default()
                    },
                ],
            )));
            assert_eq!(calls.recv().await.as_deref(), Some("present"));
            loop {
                if let Some(Update::Show(delivery)) = updates.recv().await
                    && delivery.request.generation == 2
                    && delivery.frame.rows[1].is_some()
                {
                    assert!(delivery.frame.rows[0].is_none());
                    break;
                }
            }
            assert!(calls.try_recv().is_err());
            drop(tx);
        };
        tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(drive, exercise);
        })
        .await
        .expect("artwork worker did not settle the page");
    }

    #[derive(Debug, PartialEq, Eq)]
    enum FetchEvent {
        Start(String),
        Cancel(String),
    }

    struct CancelProbe(mpsc::UnboundedSender<FetchEvent>, String);
    impl Drop for CancelProbe {
        fn drop(&mut self) {
            self.0.send(FetchEvent::Cancel(self.1.clone())).unwrap();
        }
    }

    #[tokio::test]
    async fn changes_cancel_inflight_work_and_the_session_flag_blocks_offline_fetches() {
        let a = access(1);
        let session = a.session.clone();
        let (access_tx, access_rx) = watch::channel(Some(a));
        let for_album = |epoch, generation, id: &str| {
            request(epoch, generation, Route::Album(id.into()), vec![])
        };
        let (tx, rx) = watch::channel(Some(for_album(1, 1, "stalled-old")));
        let (fetch_tx, mut fetch_rx) = mpsc::unbounded_channel();
        let (updates_tx, mut updates_rx) = mpsc::unbounded_channel();
        let drive = run(
            access_rx.clone(),
            rx.clone(),
            |update| {
                let _ = updates_tx.send(update);
            },
            async |_, Key(_, id)| {
                fetch_tx.send(FetchEvent::Start(id.clone())).unwrap();
                if id.starts_with("stalled") {
                    let _probe = CancelProbe(fetch_tx.clone(), id);
                    std::future::pending().await
                } else {
                    Some(Cached::Cover(Pixels::new(1, 1)))
                }
            },
        );
        let exercise = async {
            assert_eq!(
                fetch_rx.recv().await,
                Some(FetchEvent::Start("stalled-old".into()))
            );
            let Some(Update::Show(old)) = updates_rx.recv().await else {
                panic!("missing initial frame")
            };
            assert!(old.current(&access_rx, &rx));
            tx.send_replace(Some(for_album(1, 2, "stalled-current")));
            assert!(!old.current(&access_rx, &rx));
            assert_eq!(
                fetch_rx.recv().await,
                Some(FetchEvent::Cancel("stalled-old".into()))
            );
            assert_eq!(
                fetch_rx.recv().await,
                Some(FetchEvent::Start("stalled-current".into()))
            );
            let Some(Update::Show(online)) = updates_rx.recv().await else {
                panic!("missing online frame")
            };
            session.set_offline(true); // Desk's immediate fence, before Access updates.
            assert!(!online.current(&access_rx, &rx));
            assert_eq!(
                fetch_rx.recv().await,
                Some(FetchEvent::Cancel("stalled-current".into()))
            );
            tx.send_replace(Some(for_album(1, 3, "cover")));
            loop {
                if let Some(Update::Show(frame)) = updates_rx.recv().await
                    && frame.request.generation == 3
                {
                    assert!(frame.frame.hero.is_none());
                    break;
                }
            }
            assert!(fetch_rx.try_recv().is_err());
            session.set_offline(false);
            assert_eq!(
                fetch_rx.recv().await,
                Some(FetchEvent::Start("cover".into()))
            );
            let delivered = loop {
                if let Some(Update::Show(frame)) = updates_rx.recv().await
                    && frame.frame.hero.is_some()
                {
                    break frame;
                }
            };
            assert!(delivered.current(&access_rx, &rx));
            access_tx.send_replace(Some(access(2)));
            assert!(!delivered.current(&access_rx, &rx));
            tx.send_replace(Some(for_album(2, 4, "cover")));
            assert_eq!(
                fetch_rx.recv().await,
                Some(FetchEvent::Start("cover".into()))
            );
            drop(tx);
        };
        tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(drive, exercise);
        })
        .await
        .expect("artwork worker did not honor a fence");
    }
}
