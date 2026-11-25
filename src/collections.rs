//! Explicit, ordered collection downloads. Membership is local metadata, not
//! authority to use audio: the track store still owns admission and playback.
//! Removing a collection never removes audio, including audio saved by a failed
//! or cancelled collection save. Individual downloads can be removed separately.

use std::{
    collections::{HashMap, HashSet},
    fs::File,
    hash::{BuildHasher, RandomState},
    io::{BufRead, BufReader, BufWriter, ErrorKind, Read, Write},
    os::unix::fs::FileExt,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use ring::digest::{Context, SHA256};
use rustix::{
    fd::OwnedFd,
    fs::{self, AtFlags, FileType, FlockOperation, Mode, OFlags, RenameFlags, Stat},
    io::Errno,
    process::geteuid,
};
use serde::{Deserialize, Serialize};

use crate::{
    api::{ApiError, Item, PAGE_SIZE, library::PlaylistKind},
    desk::Access,
    offline::{self, Download, OfflineStore},
    pages::{self, Credit, MusicMetadata, Page, Route, Row, RowKey},
    playlist_pin_store::{Membership, Provenance, Target as PinTarget},
    storage as local_storage,
};

// Bound each response, not the total number of membership occurrences. Audio
// storage and playback limits are independent of collection metadata.
const MAX_PAGE_BYTES: usize = 2 * 1024 * 1024;
const ROOT: &str = "spofify-collections-v2";
const INDEX: &str = "index.json";
const STAGE: &str = ".partial";
const ADMISSION: &str = ".save-lock";
const PRIVATE_FILE: Mode = Mode::RUSR.union(Mode::WUSR);
const FILE_FLAGS: OFlags = OFlags::NOFOLLOW
    .union(OFlags::CLOEXEC)
    .union(OFlags::NONBLOCK);

const fn error(message: &'static str) -> ApiError {
    ApiError {
        message,
        expired: false,
        status: 0,
    }
}
const STORAGE: ApiError = error("Unable to access private collection storage.");
const DAMAGED: ApiError = error("The saved collection index is invalid.");
const BUSY: ApiError = error("Another collection operation is in progress. Try again shortly.");
const NO_SPACE: ApiError = error("Not enough space on this device to update saved collections.");
const NOT_DURABLE: ApiError =
    error("The change was applied, but could not be confirmed on disk. Reopen Saved to check.");
const INVALID: ApiError = error("The server returned an incompatible collection manifest.");
const UNSUPPORTED: ApiError = error("This page cannot be saved as a collection.");
const EMPTY: ApiError = error("No tracks in this collection could be saved.");
const UNRECORDED: ApiError = error("This playlist contains songs without recorded audio.");
const OFFLINE: ApiError = error("Work online to save a collection.");
const CANCELLED: ApiError = error("The collection save was cancelled.");
const CHANGED: ApiError = ApiError {
    message: "The collection changed. Open it again before saving.",
    expired: false,
    status: 409,
};
const NOT_FOUND: ApiError = ApiError {
    message: "This saved collection is not on this device.",
    expired: false,
    status: 404,
};

pub async fn list(access: &Access) -> Result<Vec<Row>, ApiError> {
    let disk_access = access.clone();
    let entries = blocking(move || {
        let scope = scope(&disk_access)?;
        let index = IndexDir::read_existing()?;
        let mut entries: Vec<_> = index
            .in_scope(&scope)
            .filter(|entry| entry.kind.is_catalog_collection())
            .collect();
        entries.sort_unstable_by(|a, b| a.title.cmp(&b.title).then_with(|| a.key.cmp(&b.key)));
        Ok(entries)
    })
    .await?;
    let ids = entries
        .iter()
        .flat_map(|entry| entry.tracks.iter().cloned())
        .collect();
    let available = browse_tracks(access, ids).await?;
    check_live(access)?;
    Ok(entries
        .into_iter()
        .map(|entry| list_row(entry, &available))
        .collect())
}

pub async fn detail(access: &Access, id: &str) -> Result<Page, ApiError> {
    if !hex_digest(id) {
        return Err(NOT_FOUND);
    }
    let disk_access = access.clone();
    let id = id.to_owned();
    let entry = blocking(move || {
        let scope = scope(&disk_access)?;
        IndexDir::read_existing()?
            .in_scope(&scope)
            .find(|e| e.key == id && e.kind.is_catalog_collection())
            .ok_or(NOT_FOUND)
    })
    .await?;
    let available = browse_tracks(access, entry.tracks.iter().cloned().collect()).await?;
    check_live(access)?;
    Ok(entry.detail_page(&available))
}

fn available_tracks(
    access: &Access,
    store: &OfflineStore,
) -> Result<HashMap<String, Download>, ApiError> {
    // Local headers supply metadata and identity; full byte verification is
    // deferred to playback.
    Ok(store
        .list(&access.api, &access.session)
        .map_err(audio_error)?
        .into_iter()
        .map(|d| (d.item.id.clone(), d))
        .collect())
}

pub(crate) fn saved_playlist_row(
    key: &str,
    title: &str,
    kind: PlaylistKind,
    status: String,
) -> Row {
    collection_row(key.into(), title.into(), kind.into(), status)
}

/// Project the playable subset of the saved membership without mutating the
/// original ordered occurrences.
pub(crate) async fn saved_playlist_page(
    access: &Access,
    title: String,
    kind: PlaylistKind,
    tracks: Vec<String>,
) -> Result<Page, ApiError> {
    check_live(access)?;
    let available = browse_tracks(access, tracks.iter().cloned().collect()).await?;
    check_live(access)?;
    Ok(detail_page(title, kind.into(), &tracks, &available))
}

/// Scoped structural inventory for browsing only; playback reopens and
/// verifies the recording. Never hash audio payloads to populate a page.
pub(crate) async fn browse_tracks(
    access: &Access,
    ids: HashSet<String>,
) -> Result<HashMap<String, Download>, ApiError> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    offline::VerifiedDownloads::for_browsing(access.api.clone(), access.session.clone())
        .map_err(audio_error)?
        .verified_records(Some(ids.into_iter().collect()))
        .await
        .map_err(audio_error)
}

fn list_row(entry: StoredCollection, available: &HashMap<String, Download>) -> Row {
    let detail = entry.summary(available).label();
    collection_row(entry.key, entry.title, entry.kind, detail)
}

fn collection_row(key: String, title: String, kind: Kind, detail: String) -> Row {
    Row {
        kind: "saved-collection".into(),
        id: key.clone(),
        route: Some(Route::SavedCollection(key)),
        title,
        subtitle: kind.label().into(),
        detail,
        ..Row::default()
    }
}

fn detail_page(
    title: String,
    kind: Kind,
    tracks: &[String],
    available: &HashMap<String, Download>,
) -> Page {
    let summary = SaveSummary::from_tracks(tracks, available);
    let rows = tracks
        .iter()
        .enumerate()
        .filter_map(|(position, id)| {
            let download = local_track(available, id)?;
            Some(local_track_row(
                download,
                RowKey::Track(id.clone()),
                position as u64,
            ))
        })
        .collect();
    Page {
        title,
        subtitle: format!("{} · {}", kind.label(), summary.label()),
        kind: "saved-collection".into(),
        total: Some(summary.total as u64),
        rows,
        ..Page::default()
    }
}

pub(crate) fn local_track<'a>(
    available: &'a HashMap<String, Download>,
    id: &str,
) -> Option<&'a Download> {
    available.get(id).filter(|download| {
        let item = &download.item;
        item.id == id && item.kind == "track" && item.has_audio
    })
}

pub(crate) fn local_track_row(download: &Download, key: RowKey, position: u64) -> Row {
    let item = &download.item;
    let album_name = pages::ui_text(&item.album_name);
    Row {
        kind: "track".into(),
        id: item.id.clone(),
        title: item.name.clone(),
        subtitle: item.artists.clone(),
        detail: pages::duration_label(item.duration_ms),
        music: MusicMetadata {
            album: (!album_name.is_empty()).then_some(Credit {
                name: album_name,
                target: None,
            }),
            ..MusicMetadata::default()
        },
        key,
        position: Some(position),
        playable: true,
        item: Some(item.clone()),
        local_recording: Some(download.recording),
        ..Row::default()
    }
}

#[cfg(test)]
pub(crate) fn synthetic_download(item: Item) -> Download {
    Download {
        item,
        bytes: 123,
        recording: offline::Recording::fixture(),
    }
}

pub async fn remove(access: &Access, id: &str) -> Result<(), ApiError> {
    if !hex_digest(id) {
        return Err(NOT_FOUND);
    }
    let access = access.clone();
    let id = id.to_owned();
    let cancel = Cancellation::new();
    let cancelled = cancel.0.clone();
    blocking(move || {
        let scope = scope(&access)?;
        let dir = IndexDir::open_default()?;
        let _admission = dir.admit()?;
        let mut index = dir.read()?;
        let removed = index.remove_entry(&scope, &id);
        let live = || {
            check_cancelled(&cancelled)?;
            check_live(&access)
        };
        live()?;
        if !removed {
            return Ok(());
        }
        dir.commit(&index, live)
    })
    .await
}

/// A point-in-time count of collection slots saved on this device. Repeated
/// tracks count once per occurrence, including in a partial save.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SaveSummary {
    pub present: usize,
    pub total: usize,
}
impl SaveSummary {
    fn from_tracks(tracks: &[String], available: &HashMap<String, Download>) -> Self {
        Self {
            present: tracks
                .iter()
                .filter(|id| local_track(available, id).is_some())
                .count(),
            total: tracks.len(),
        }
    }

    fn label(self) -> String {
        format!("{} of {} available", self.present, song_count(self.total))
    }
}

pub async fn save(access: &Access, route: &Route, page: &Page) -> Result<SaveSummary, ApiError> {
    if !matches!(route, Route::Album(_) | Route::Artist(_)) {
        return Err(UNSUPPORTED);
    }
    if access.offline {
        return Err(OFFLINE);
    }
    let mut network = access.session.network_changes();
    let generation = *network.borrow_and_update();
    if generation & 1 != 0 {
        return Err(OFFLINE);
    }
    check_live(access)?;
    let target = Target::new(route, page)?;
    let open = {
        let access = access.clone();
        blocking(move || {
            let scope = scope(&access)?;
            let dir = IndexDir::open_default()?;
            let admission = dir.admit()?;
            let index = dir.read()?;
            Ok((scope, dir, admission, index))
        })
    };
    let cancel = Cancellation::new();
    let cancelled = cancel.0.clone();
    tokio::select! {
        biased;
        _ = network.changed() => Err(OFFLINE),
        result = async {
            let (scope, dir, admission, mut index) = open.await?;
            // Validate every manifest page before starting any audio I/O.
            let Snapshot { kind, title, tracks } = snapshot(access, target).await?;
            if tracks.is_empty() { return Err(EMPTY); }
            let store = OfflineStore::open_for_download(OfflineStore::open_default).await.map_err(audio_error)?;
            {
                let mut seen = HashSet::new();
                for item in &tracks {
                    if !item.has_audio || !seen.insert(&item.id) { continue; }
                    check_live(access)?;
                    if let Err(err) = store.download_for_access(access, item).await {
                        handle_track_error(err)?;
                    }
                }
            }
            let key = collection_key(&scope, target.domain(), target.id());
            let entry = StoredCollection {
                scope, key, kind, title,
                tracks: tracks.into_iter().map(|t| t.id).collect(),
            };
            let access = access.clone();
            let changes = access.session.network_changes();
            blocking(move || {
                let _admission = admission;
                // Count existing downloads as well, and don't count an earlier
                // success if the track has already been removed.
                let summary = entry.summary(&available_tracks(&access, &store)?);
                if summary.present == 0 { return Err(EMPTY); }
                index.replace_entry(entry);
                dir.commit(&index, || {
                    check_cancelled(&cancelled)?;
                    check_live(&access)?;
                    if *changes.borrow() != generation { return Err(OFFLINE); }
                    Ok(())
                })?;
                Ok(summary)
            }).await
        } => result,
    }
}

/// Transfer a complete fenced snapshot. Only the caller can publish the
/// membership; verified audio survives a failed or cancelled pin.
pub(crate) async fn complete_playlist(
    access: &Access,
    target: &PinTarget,
    progress: impl FnMut(usize, usize) + Send,
    live: impl Fn() -> Result<(), ApiError> + Send + Sync,
) -> Result<Membership, ApiError> {
    if access.offline {
        return Err(OFFLINE);
    }
    if !valid_id(&target.id) {
        return Err(UNSUPPORTED);
    }
    let mut network = access.session.network_changes();
    let generation = *network.borrow_and_update();
    if generation & 1 != 0 {
        return Err(OFFLINE);
    }
    let fence = network.clone();
    let live = || {
        live()?;
        if *fence.borrow() != generation {
            return Err(OFFLINE);
        }
        check_live(access)
    };
    let target = Target::Playlist {
        id: &target.id,
        kind: target.kind,
        revision: None,
    };
    tokio::select! {
        biased;
        _ = network.changed() => Err(OFFLINE),
        result = async {
            let state = fetch_manifest::<PlaylistTracks>(access, target, &live).await?;
            state.complete(access, progress, &live, OfflineStore::open_default).await
        } => result,
    }
}

/// Call on a blocking worker. Check that required entries still exist under
/// the audio directory's shared lock. Keep the guard through the pin store's
/// commit and do not take an exclusive audio lock while holding it.
pub(crate) fn lock_playlist_members(
    access: &Access,
    membership: &Membership,
) -> Result<Option<OwnedFd>, ApiError> {
    lock_playlist_members_with(access, membership, OfflineStore::open_existing)
}

fn lock_playlist_members_with(
    access: &Access,
    membership: &Membership,
    open: impl FnOnce() -> Result<Option<OfflineStore>, offline::Error>,
) -> Result<Option<OwnedFd>, ApiError> {
    check_live(access)?;
    let guard = if membership.tracks.is_empty() {
        None
    } else {
        let store = open()
            .map_err(audio_error)?
            .ok_or_else(|| audio_error(offline::Error::NotDownloaded))?;
        Some(
            store
                .lock_members(&access.api, &access.session, &membership.tracks)
                .map_err(audio_error)?,
        )
    };
    check_live(access)?;
    Ok(guard)
}

async fn verify_track(
    store: &OfflineStore,
    access: &Access,
    id: &str,
) -> Result<(), offline::Error> {
    static PIN_VERIFY_SLOT: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();
    let slot = PIN_VERIFY_SLOT
        .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(1)))
        .clone();
    let store = store.clone();
    offline::read_verified(
        access.api.clone(),
        access.session.clone(),
        slot,
        id.to_owned(),
        move || Ok(store),
    )
    .await
    .map(|_| ())
}

fn handle_track_error(err: offline::Error) -> Result<(), ApiError> {
    match err {
        offline::Error::AlreadyDownloaded
        | offline::Error::NotDownloaded
        | offline::Error::UnsupportedAudio => Ok(()),
        // Skip only item-level absence or unsupported representations, not
        // authentication, transport, integrity, quota or consistency failures.
        offline::Error::Api(e) if !e.expired && matches!(e.status, 404 | 410 | 413 | 415) => Ok(()),
        err => Err(audio_error(err)),
    }
}

#[derive(Clone, Copy)]
enum Target<'a> {
    Album(&'a str),
    Artist(&'a str),
    Playlist {
        id: &'a str,
        kind: PlaylistKind,
        revision: Option<u64>,
    },
}
impl<'a> Target<'a> {
    fn new(route: &'a Route, page: &Page) -> Result<Self, ApiError> {
        match route {
            Route::Album(id) if valid_id(id) => Ok(Self::Album(id)),
            Route::Artist(id) if valid_id(id) => Ok(Self::Artist(id)),
            Route::Playlist(id) if valid_id(id) => {
                // The account socket supplies the accepted basis; only the
                // genuine offline manifest grants a consistent download view.
                let playlist = page
                    .playlist
                    .as_ref()
                    .filter(|p| p.id == *id)
                    .ok_or(CHANGED)?;
                Ok(Self::Playlist {
                    id,
                    kind: playlist.kind,
                    revision: Some(playlist.revision),
                })
            }
            _ => Err(UNSUPPORTED),
        }
    }
    fn domain(self) -> &'static str {
        match self {
            Self::Album(_) => "albums",
            Self::Artist(_) => "artists",
            Self::Playlist { .. } => "playlists",
        }
    }
    fn id(self) -> &'a str {
        match self {
            Self::Album(id) | Self::Artist(id) | Self::Playlist { id, .. } => id,
        }
    }
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
enum Kind {
    Album,
    Artist,
    Manual,
    Derived,
}
impl Kind {
    fn is_catalog_collection(self) -> bool {
        matches!(self, Self::Album | Self::Artist)
    }

    fn label(self) -> &'static str {
        match self {
            Self::Album => "Album",
            Self::Artist => "Artist",
            Self::Manual => "Playlist",
            Self::Derived => "Rule playlist",
        }
    }
}

impl From<PlaylistKind> for Kind {
    fn from(kind: PlaylistKind) -> Self {
        match kind {
            PlaylistKind::Manual => Self::Manual,
            PlaylistKind::Derived => Self::Derived,
        }
    }
}

struct Snapshot {
    kind: Kind,
    title: String,
    tracks: Vec<Item>,
}

async fn snapshot(access: &Access, target: Target<'_>) -> Result<Snapshot, ApiError> {
    fetch_manifest::<Vec<Item>>(access, target, || Ok(()))
        .await?
        .finish()
}

async fn fetch_manifest<T: ManifestTracks>(
    access: &Access,
    target: Target<'_>,
    live: impl Fn() -> Result<(), ApiError>,
) -> Result<ManifestState<T>, ApiError> {
    let mut state = ManifestState::default();
    let mut cursor = None;
    loop {
        live()?;
        let wire = access
            .api
            .offline_collection_manifest(
                &access.session,
                target.domain(),
                target.id(),
                cursor.as_deref(),
            )
            .await?;
        live()?;
        cursor = state.append(target, &wire)?;
        if cursor.is_none() {
            return Ok(state);
        }
    }
}

// Include cancelled workers until an in-flight filesystem call returns. The
// spool and its permit are always owned by either the operation or its worker.
static SEARCH_SPOOL_SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);
const SEARCH_SLOT_WAIT: Duration = Duration::from_secs(10);
const SEARCH_STORAGE: ApiError = error("Unable to stage songs for quick add.");
const SEARCH_NO_SPACE: ApiError = error("Not enough space to stage songs for quick add.");
const SEARCH_CANCELLED: ApiError = error("Adding songs was cancelled.");
const SEARCH_OFFLINE: ApiError = error("Work online to add songs from search.");

/// Read the entire album or artist manifest before the caller can start any
/// writes. Personal writes can invalidate the manifest fence between pages.
pub(crate) async fn spool_search_source(
    access: &Access,
    kind: &str,
    id: &str,
    live: impl Fn() -> bool,
) -> Result<SearchSourceSpool, ApiError> {
    let target = match (kind, valid_id(id)) {
        ("album", true) => Target::Album(id),
        ("artist", true) => Target::Artist(id),
        _ => return Err(INVALID),
    };
    let mut network = access.session.network_changes();
    let generation = *network.borrow_and_update();
    if access.offline || generation & 1 != 0 {
        return Err(SEARCH_OFFLINE);
    }
    let fence = network.clone();
    let check = || {
        if !live() {
            return Err(SEARCH_CANCELLED);
        }
        if *fence.borrow() != generation {
            return Err(SEARCH_OFFLINE);
        }
        check_live(access)
    };
    check()?;
    let cancel = Cancellation::new();
    let cancelled = cancel.0.clone();
    tokio::select! {
        biased;
        _ = network.changed() => Err(SEARCH_OFFLINE),
        // Also observe guard changes while awaiting a network/disk operation,
        // even when the caller has no separate cancellation future.
        error = async {
            loop {
                tokio::time::sleep(Duration::from_millis(50)).await;
                if let Err(error) = check() { break error; }
            }
        } => Err(error),
        result = async {
            let permit = tokio::time::timeout(SEARCH_SLOT_WAIT, SEARCH_SPOOL_SLOTS.acquire())
                .await.map_err(|_| SEARCH_STORAGE)?.map_err(|_| SEARCH_STORAGE)?;
            let domain = target.domain();
            let id = target.id().to_owned();
            let mut source = search_disk(move || {
                let dir = local_storage::open_default("spofify-search-spool").map_err(search_errno)?;
                SearchStaging::open(dir, domain, id, permit, cancelled)
            }).await?;
            let mut cursor: Option<String> = None;
            loop {
                check()?;
                let body = access.api.offline_collection_manifest(
                    &access.session, target.domain(), target.id(), cursor.as_deref(),
                ).await?;
                check()?;
                (source, cursor) = search_disk(move || {
                    let cursor = source.append(&body)?;
                    Ok((source, cursor))
                }).await?;
                check()?;
                if cursor.is_none() { break; }
            }
            let spool = search_disk(move || source.finish()).await?;
            check()?;
            Ok(spool)
        } => result,
    }
}

async fn search_disk<T: Send + 'static>(
    operation: impl FnOnce() -> Result<T, ApiError> + Send + 'static,
) -> Result<T, ApiError> {
    // Index growth and final allocation checks can depend on source size.
    // Keep them cancellable without setting an implicit source-size limit.
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|_| SEARCH_STORAGE)?
}

pub(crate) struct SearchSourceSpool {
    total: u64,
    replay: Option<SearchReplay>,
}

impl SearchSourceSpool {
    pub(crate) fn total(&self) -> u64 {
        self.total
    }

    /// Read up to `limit` track IDs in server order, never more than one
    /// manifest page. An empty positive-sized batch means end of source.
    pub(crate) async fn next_batch(&mut self, limit: usize) -> Result<Vec<String>, ApiError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut replay = self.replay.take().ok_or(SEARCH_STORAGE)?;
        if replay.remaining == 0 {
            self.replay = Some(replay);
            return Ok(Vec::new());
        }
        let (replay, ids) = search_disk(move || {
            let ids = replay.read_batch(limit.min(PAGE_SIZE as usize))?;
            Ok((replay, ids))
        })
        .await?;
        self.replay = Some(replay);
        Ok(ids)
    }
}

struct SearchReplay {
    file: BufReader<File>,
    remaining: u64,
    _permit: tokio::sync::SemaphorePermit<'static>,
}

impl SearchReplay {
    fn read_batch(&mut self, limit: usize) -> Result<Vec<String>, ApiError> {
        let mut ids = Vec::with_capacity(limit);
        let mut key = [0; SEARCH_KEY_CAPACITY];
        while self.remaining > 0 && ids.len() < limit {
            let mut header = [0; 3];
            self.file.read_exact(&mut header).map_err(search_io)?;
            let len = search_key_len(header)?;
            self.file.read_exact(&mut key[..len]).map_err(search_io)?;
            match header[0] {
                SEARCH_TRACK => {
                    let id = std::str::from_utf8(&key[..len]).map_err(|_| SEARCH_STORAGE)?;
                    if !valid_id(id) {
                        return Err(SEARCH_STORAGE);
                    }
                    ids.push(id.to_owned());
                    self.remaining -= 1;
                }
                SEARCH_CURSOR | SEARCH_OCCURRENCE => (),
                _ => return Err(SEARCH_STORAGE),
            }
        }
        Ok(ids)
    }
}

struct SearchStaging {
    domain: &'static str,
    id: String,
    progress: ManifestProgress,
    keys: SearchKeys,
    permit: tokio::sync::SemaphorePermit<'static>,
}

impl SearchStaging {
    fn open(
        dir: OwnedFd,
        domain: &'static str,
        id: String,
        permit: tokio::sync::SemaphorePermit<'static>,
        cancelled: Arc<AtomicBool>,
    ) -> Result<Self, ApiError> {
        search_live(&cancelled)?;
        let records = anonymous_search_file(&dir)?;
        search_live(&cancelled)?;
        let slots = SearchSlots::open(&dir, 64)?;
        Ok(Self {
            domain,
            id,
            progress: ManifestProgress::default(),
            keys: SearchKeys {
                dir,
                records,
                end: 0,
                slots,
                entries: 0,
                hash: RandomState::new(),
                cancelled,
            },
            permit,
        })
    }

    fn append(&mut self, wire: &[u8]) -> Result<Option<String>, ApiError> {
        self.keys.live()?;
        let target = match self.domain {
            "albums" => Target::Album(&self.id),
            "artists" => Target::Artist(&self.id),
            _ => return Err(INVALID),
        };
        self.progress.append(target, wire, &mut self.keys)
    }

    fn finish(self) -> Result<SearchSourceSpool, ApiError> {
        let (_, _, total) = self.progress.finish()?;
        self.keys.live()?;
        // Catch delayed allocation errors before handing replay to a writer.
        self.keys.records.sync_data().map_err(search_io)?;
        Ok(SearchSourceSpool {
            total,
            replay: Some(SearchReplay {
                file: BufReader::new(self.keys.records),
                remaining: total,
                _permit: self.permit,
            }),
        })
    }
}

fn anonymous_search_file(dir: &OwnedFd) -> Result<File, ApiError> {
    let flags = OFlags::RDWR | OFlags::CLOEXEC;
    let fd = match fs::openat(
        dir,
        ".",
        flags | OFlags::TMPFILE | OFlags::EXCL,
        PRIVATE_FILE,
    ) {
        Ok(fd) => fd,
        Err(Errno::OPNOTSUPP | Errno::INVAL) => {
            // Same fallback as audio_input: unlink the private name before
            // writing any catalog data.
            let name = uuid::Uuid::new_v4().to_string();
            let fd = fs::openat(
                dir,
                name.as_str(),
                flags | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW,
                PRIVATE_FILE,
            )
            .map_err(search_errno)?;
            fs::unlinkat(dir, name.as_str(), AtFlags::empty()).map_err(search_errno)?;
            fd
        }
        Err(err) => return Err(search_errno(err)),
    };
    let stat = fs::fstat(&fd).map_err(search_errno)?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile
        || stat.st_uid != geteuid().as_raw()
        || stat.st_mode & (0o7777 & !0o600) != 0
        || stat.st_nlink != 0
    {
        return Err(SEARCH_STORAGE);
    }
    Ok(File::from(fd))
}

const SEARCH_TRACK: u8 = 0;
const SEARCH_CURSOR: u8 = 1;
const SEARCH_OCCURRENCE: u8 = 2;
const SEARCH_KEY_CAPACITY: usize = 4096;
const SEARCH_SLOT_BYTES: u64 = 16;

fn search_errno(err: Errno) -> ApiError {
    match err {
        Errno::NOSPC | Errno::DQUOT | Errno::FBIG => SEARCH_NO_SPACE,
        _ => SEARCH_STORAGE,
    }
}

fn search_io(err: std::io::Error) -> ApiError {
    err.raw_os_error()
        .map(Errno::from_raw_os_error)
        .map_or(SEARCH_STORAGE, search_errno)
}

fn search_key_len(header: [u8; 3]) -> Result<usize, ApiError> {
    let len = usize::from(u16::from_le_bytes([header[1], header[2]]));
    (1..=SEARCH_KEY_CAPACITY)
        .contains(&len)
        .then_some(len)
        .ok_or(SEARCH_STORAGE)
}

/// An append-only key log plus an anonymous, growable open-addressed index.
/// A slot stores a keyed hash and an exact key's log offset + 1 (zero is
/// empty). Hash collisions compare original bytes; no RAM set scales with
/// source size. Growth streams the old index into its replacement.
struct SearchKeys {
    dir: OwnedFd,
    records: File,
    end: u64,
    slots: SearchSlots,
    entries: u64,
    hash: RandomState,
    cancelled: Arc<AtomicBool>,
}

struct SearchSlots {
    file: File,
    capacity: u64,
}

impl SearchSlots {
    fn open(dir: &OwnedFd, capacity: u64) -> Result<Self, ApiError> {
        let file = anonymous_search_file(dir)?;
        file.set_len(
            capacity
                .checked_mul(SEARCH_SLOT_BYTES)
                .ok_or(SEARCH_NO_SPACE)?,
        )
        .map_err(search_io)?;
        Ok(Self { file, capacity })
    }

    fn find(
        &self,
        hash: u64,
        mut matches: impl FnMut(u64) -> Result<bool, ApiError>,
        live: impl Fn() -> Result<(), ApiError>,
    ) -> Result<u64, ApiError> {
        let mask = self.capacity - 1;
        let mut index = hash & mask;
        for _ in 0..self.capacity {
            live()?;
            let slot = index * SEARCH_SLOT_BYTES;
            let mut bytes = [0; SEARCH_SLOT_BYTES as usize];
            self.file
                .read_exact_at(&mut bytes, slot)
                .map_err(search_io)?;
            let (key, old_hash) = Self::decode(bytes);
            if key == 0 {
                return Ok(slot);
            }
            if old_hash == hash && matches(key - 1)? {
                return Err(INVALID);
            }
            index = (index + 1) & mask;
        }
        Err(SEARCH_STORAGE)
    }

    fn decode(bytes: [u8; SEARCH_SLOT_BYTES as usize]) -> (u64, u64) {
        let (key, hash) = bytes.split_at(8);
        (
            u64::from_le_bytes(key.try_into().unwrap()),
            u64::from_le_bytes(hash.try_into().unwrap()),
        )
    }

    fn write(&self, slot: u64, key: u64, hash: u64) -> Result<(), ApiError> {
        let mut bytes = [0; SEARCH_SLOT_BYTES as usize];
        bytes[..8].copy_from_slice(&key.to_le_bytes());
        bytes[8..].copy_from_slice(&hash.to_le_bytes());
        self.file.write_all_at(&bytes, slot).map_err(search_io)
    }
}

fn search_live(cancelled: &AtomicBool) -> Result<(), ApiError> {
    if cancelled.load(Ordering::Acquire) {
        Err(SEARCH_CANCELLED)
    } else {
        Ok(())
    }
}

impl SearchKeys {
    fn live(&self) -> Result<(), ApiError> {
        search_live(&self.cancelled)
    }

    fn grow(&mut self) -> Result<(), ApiError> {
        let capacity = self.slots.capacity.checked_mul(2).ok_or(SEARCH_NO_SPACE)?;
        let next = SearchSlots::open(&self.dir, capacity)?;
        // Only positional I/O accesses slots, so the old stream starts at 0.
        let mut old = BufReader::new(&self.slots.file);
        for _ in 0..self.slots.capacity {
            self.live()?;
            let mut bytes = [0; SEARCH_SLOT_BYTES as usize];
            old.read_exact(&mut bytes).map_err(search_io)?;
            let (key, hash) = SearchSlots::decode(bytes);
            if key != 0 {
                let slot = next.find(hash, |_| Ok(false), || self.live())?;
                next.write(slot, key, hash)?;
            }
        }
        self.slots = next;
        Ok(())
    }

    fn insert(&mut self, tag: u8, value: &str) -> Result<(), ApiError> {
        if self.entries == self.slots.capacity / 2 {
            self.grow()?;
        }
        let hash = self.hash.hash_one((tag, value));
        let len = value.len();
        let slot = self.slots.find(
            hash,
            |offset| {
                let mut header = [0; 3];
                self.records
                    .read_exact_at(&mut header, offset)
                    .map_err(search_io)?;
                if header[0] != tag || search_key_len(header)? != len {
                    return Ok(false);
                }
                let mut key = [0; SEARCH_KEY_CAPACITY];
                self.records
                    .read_exact_at(&mut key[..len], offset + 3)
                    .map_err(search_io)?;
                Ok(key[..len] == *value.as_bytes())
            },
            || self.live(),
        )?;
        let end = self
            .end
            .checked_add(3 + len as u64)
            .ok_or(SEARCH_NO_SPACE)?;
        let length = u16::try_from(len).map_err(|_| INVALID)?.to_le_bytes();
        self.records
            .write_all_at(&[tag, length[0], length[1]], self.end)
            .map_err(search_io)?;
        self.records
            .write_all_at(value.as_bytes(), self.end + 3)
            .map_err(search_io)?;
        self.slots.write(slot, self.end + 1, hash)?;
        self.end = end;
        self.entries += 1;
        Ok(())
    }
}

impl ManifestSink for SearchKeys {
    fn counts(&self, total: u64, recorded: u64) -> Result<(), ApiError> {
        (total == recorded).then_some(()).ok_or(INVALID)
    }

    fn cursor(&mut self, cursor: &str) -> Result<(), ApiError> {
        let base64url = |part: &str| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        };
        if !cursor
            .strip_prefix("ofc1_")
            .and_then(|token| token.split_once('.'))
            .is_some_and(|(claim, signature)| base64url(claim) && base64url(signature))
        {
            return Err(INVALID);
        }
        self.insert(SEARCH_CURSOR, cursor)
    }

    fn occurrence(&mut self, id: String, track: Item) -> Result<(), ApiError> {
        if !track.has_audio {
            return Err(INVALID);
        }
        self.insert(SEARCH_OCCURRENCE, &id)?;
        self.insert(SEARCH_TRACK, &track.id)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireManifest {
    version: u8,
    playlist: Option<Header>,
    collection: Option<Header>,
    fence: String,
    items: Vec<WireOccurrence>,
    total: u64,
    recorded_audio_total: u64,
    limit: u32,
    #[serde(deserialize_with = "Deserialize::deserialize")]
    next_cursor: Option<String>,
}
#[derive(Deserialize, PartialEq, Eq)]
struct Header {
    id: String,
    kind: String,
    name: String,
    revision: Option<u64>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireOccurrence {
    occurrence_id: String,
    position: u64,
    track: WireTrack,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireTrack {
    id: String,
    name: String,
    kind: String,
    has_audio: bool,
    #[serde(default)]
    artists: Vec<WireArtist>,
    #[serde(default)]
    album_name: String,
    #[serde(default)]
    duration_ms: u64,
    #[serde(default)]
    track_number: u32,
    #[serde(default)]
    disc_number: u32,
}
#[derive(Deserialize)]
struct WireArtist {
    name: String,
}

impl WireTrack {
    fn project(self) -> Result<Item, ApiError> {
        if !valid_id(&self.id)
            || self.kind != "track"
            || !valid_title(&self.name)
            || self.artists.len() > 32
            || self.artists.iter().any(|a| !valid_title(&a.name))
            || (!self.album_name.is_empty() && !valid_title(&self.album_name))
            || self.duration_ms > 7 * 24 * 60 * 60 * 1000
            || self.track_number > 100_000
            || self.disc_number > 100_000
        {
            return Err(INVALID);
        }
        let artists = self
            .artists
            .iter()
            .map(|a| a.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        if artists.len() > 4096 {
            return Err(INVALID);
        }
        Ok(Item {
            id: self.id,
            name: self.name,
            kind: self.kind,
            has_audio: self.has_audio,
            artists,
            album_name: self.album_name,
            duration_ms: self.duration_ms,
            track_number: self.track_number,
            disc_number: self.disc_number,
        })
    }
}

#[derive(Default)]
struct ManifestState<T = Vec<Item>> {
    progress: ManifestProgress,
    memory: MemoryManifest<T>,
}

#[derive(Default)]
struct MemoryManifest<T> {
    tracks: T,
    occurrences: HashSet<String>,
    cursors: HashSet<String>,
}

#[derive(Default)]
struct ManifestProgress {
    metadata: Option<(Header, String, u64, u64, Kind)>,
    position: u64,
    recorded: u64,
}

trait ManifestSink {
    fn counts(&self, _total: u64, _recorded: u64) -> Result<(), ApiError> {
        Ok(())
    }
    fn cursor(&mut self, cursor: &str) -> Result<(), ApiError>;
    fn occurrence(&mut self, id: String, track: Item) -> Result<(), ApiError>;
}

trait ManifestTracks: Default {
    fn push_track(&mut self, occurrence_id: &str, item: Item) -> Result<(), ApiError>;
}

impl ManifestTracks for Vec<Item> {
    fn push_track(&mut self, _: &str, item: Item) -> Result<(), ApiError> {
        self.push(item);
        Ok(())
    }
}

impl<T: ManifestTracks> ManifestSink for MemoryManifest<T> {
    fn cursor(&mut self, cursor: &str) -> Result<(), ApiError> {
        self.cursors
            .insert(cursor.into())
            .then_some(())
            .ok_or(INVALID)
    }
    fn occurrence(&mut self, id: String, track: Item) -> Result<(), ApiError> {
        if self.occurrences.contains(id.as_str()) {
            return Err(INVALID);
        }
        self.tracks.push_track(&id, track)?;
        self.occurrences.insert(id);
        Ok(())
    }
}

impl<T: ManifestTracks> ManifestState<T> {
    fn append(&mut self, target: Target<'_>, body: &[u8]) -> Result<Option<String>, ApiError> {
        self.progress.append(target, body, &mut self.memory)
    }
    fn finish_parts(self) -> Result<(Header, Kind, T), ApiError> {
        let (header, kind, _) = self.progress.finish()?;
        Ok((header, kind, self.memory.tracks))
    }
}

impl ManifestProgress {
    fn append(
        &mut self,
        target: Target<'_>,
        body: &[u8],
        sink: &mut impl ManifestSink,
    ) -> Result<Option<String>, ApiError> {
        if body.len() > MAX_PAGE_BYTES {
            return Err(INVALID);
        }
        let wire: WireManifest = serde_json::from_slice(body).map_err(|_| INVALID)?;
        let (header, kind, prefix) = match (target, wire.collection, wire.playlist) {
            (Target::Album(_), Some(header), None)
                if header.kind == "album" && header.revision.is_none() =>
            {
                (header, Kind::Album, "ofc1_")
            }
            (Target::Artist(_), Some(header), None)
                if header.kind == "artist" && header.revision.is_none() =>
            {
                (header, Kind::Artist, "ofc1_")
            }
            (Target::Playlist { kind, revision, .. }, None, Some(header)) => {
                let playlist_kind = match header.kind.as_str() {
                    "manual" => PlaylistKind::Manual,
                    "derived" => PlaylistKind::Derived,
                    _ => return Err(INVALID),
                };
                let actual = header
                    .revision
                    .filter(|r| (1..=9_007_199_254_740_991).contains(r))
                    .ok_or(INVALID)?;
                if playlist_kind != kind || revision.is_some_and(|expected| expected != actual) {
                    return Err(CHANGED);
                }
                (header, playlist_kind.into(), "of1_")
            }
            _ => return Err(INVALID),
        };
        if wire.version != 1
            || wire.limit != PAGE_SIZE
            || wire.recorded_audio_total > wire.total
            || header.id != target.id()
            || !valid_title(&header.name)
            || !wire.fence.strip_prefix("f1_").is_some_and(hex_digest)
        {
            return Err(INVALID);
        }
        if let Some((old, fence, total, recorded, _)) = &self.metadata {
            if *old != header
                || *fence != wire.fence
                || *total != wire.total
                || *recorded != wire.recorded_audio_total
            {
                return Err(CHANGED);
            }
        }
        sink.counts(wire.total, wire.recorded_audio_total)?;
        let offset = self.position;
        let remaining = wire.total.checked_sub(offset).ok_or(INVALID)?;
        let count = wire.items.len() as u64;
        if count != remaining.min(u64::from(PAGE_SIZE))
            || wire.next_cursor.is_some() != (count < remaining)
        {
            return Err(INVALID);
        }
        if let Some(next) = &wire.next_cursor {
            if !next.starts_with(prefix) || !opaque(next) {
                return Err(INVALID);
            }
            sink.cursor(next)?;
        }
        for (i, occurrence) in wire.items.into_iter().enumerate() {
            if occurrence.position != offset + i as u64 || !opaque(&occurrence.occurrence_id) {
                return Err(INVALID);
            }
            let item = occurrence.track.project()?;
            let recorded = u64::from(item.has_audio);
            sink.occurrence(occurrence.occurrence_id, item)?;
            self.position += 1;
            self.recorded += recorded;
        }
        self.metadata = Some((
            header,
            wire.fence,
            wire.total,
            wire.recorded_audio_total,
            kind,
        ));
        Ok(wire.next_cursor)
    }
    fn finish(self) -> Result<(Header, Kind, u64), ApiError> {
        let (header, _, total, recorded, kind) = self.metadata.ok_or(INVALID)?;
        if total != self.position || recorded != self.recorded {
            return Err(INVALID);
        }
        Ok((header, kind, total))
    }
}

impl ManifestState {
    fn finish(self) -> Result<Snapshot, ApiError> {
        let (header, kind, tracks) = self.finish_parts()?;
        Ok(Snapshot {
            kind,
            title: header.name,
            tracks,
        })
    }
}

#[derive(Default)]
struct PlaylistTracks {
    order: Vec<(usize, String)>,
    unique: Vec<(Item, usize)>,
    by_id: HashMap<String, usize>,
}
impl ManifestTracks for PlaylistTracks {
    fn push_track(&mut self, occurrence_id: &str, item: Item) -> Result<(), ApiError> {
        let index = if let Some(&i) = self.by_id.get(&item.id) {
            let (previous, count) = &mut self.unique[i];
            if *previous != item {
                return Err(INVALID);
            }
            *count += 1;
            i
        } else {
            let i = self.unique.len();
            self.by_id.insert(item.id.clone(), i);
            self.unique.push((item, 1));
            i
        };
        self.order.push((index, occurrence_id.into()));
        Ok(())
    }
}

impl ManifestState<PlaylistTracks> {
    async fn complete(
        self,
        access: &Access,
        mut progress: impl FnMut(usize, usize),
        live: impl Fn() -> Result<(), ApiError>,
        open: impl FnOnce() -> Result<OfflineStore, offline::Error> + Send + 'static,
    ) -> Result<Membership, ApiError> {
        let all_recorded = self.progress.position == self.progress.recorded;
        let (
            header,
            _,
            PlaylistTracks {
                order,
                unique,
                by_id,
            },
        ) = self.finish_parts()?;
        let revision = header.revision.ok_or(INVALID)?;
        drop(by_id);
        live()?;
        let total = order.len();
        progress(0, total);
        live()?;
        if !all_recorded {
            return Err(UNRECORDED);
        }
        if total != 0 {
            let store = OfflineStore::open_for_download(open)
                .await
                .map_err(audio_error)?;
            let mut verified = 0;
            for (item, count) in &unique {
                live()?;
                let local = verify_track(&store, access, &item.id).await;
                live()?;
                match local {
                    Ok(()) => (),
                    Err(offline::Error::NotDownloaded) => {
                        // Only absence permits a transfer. A winner of the
                        // no-overwrite download race must still pass the read.
                        let download = store.download_for_access(access, item).await;
                        live()?;
                        match download {
                            Ok(_) => (), // Download already verified, probed and committed.
                            Err(offline::Error::AlreadyDownloaded) => {
                                verify_track(&store, access, &item.id)
                                    .await
                                    .map_err(audio_error)?;
                            }
                            Err(err) => return Err(audio_error(err)),
                        }
                    }
                    Err(err) => return Err(audio_error(err)),
                }
                live()?;
                verified += *count;
                progress(verified, total);
            }
        }
        let (tracks, occurrence_ids) = order
            .into_iter()
            .map(|(index, occurrence_id)| (unique[index].0.id.clone(), occurrence_id))
            .unzip();
        live()?;
        Ok(Membership {
            title: header.name,
            tracks,
            provenance: Some(Provenance {
                revision,
                occurrence_ids,
            }),
        })
    }
}

// The private index stores only each collection's disjoint account/key
// hashes, display title/kind and ordered opaque track IDs. No session, server
// paths, cursor/fence or playback capabilities are serializable here.
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredCollection {
    scope: String,
    key: String,
    kind: Kind,
    title: String,
    tracks: Vec<String>,
}
impl StoredCollection {
    fn summary(&self, available: &HashMap<String, Download>) -> SaveSummary {
        SaveSummary::from_tracks(&self.tracks, available)
    }

    fn detail_page(self, available: &HashMap<String, Download>) -> Page {
        detail_page(self.title, self.kind, &self.tracks, available)
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Index {
    version: u8,
    entries: Vec<StoredCollection>,
}
impl Index {
    fn in_scope(self, scope: &str) -> impl Iterator<Item = StoredCollection> {
        self.entries
            .into_iter()
            .filter(move |entry| entry.scope == scope)
    }
    fn empty() -> Self {
        Self {
            version: 1,
            entries: Vec::new(),
        }
    }
    fn remove_entry(&mut self, scope: &str, key: &str) -> bool {
        let before = self.entries.len();
        self.entries
            .retain(|entry| entry.scope != scope || entry.key != key);
        self.entries.len() != before
    }
    fn replace_entry(&mut self, entry: StoredCollection) {
        self.remove_entry(&entry.scope, &entry.key);
        self.entries.push(entry);
    }
    fn validate(&self) -> Result<(), ApiError> {
        let mut seen = HashSet::new();
        if self.version != 1
            || self.entries.iter().any(|e| {
                !hex_digest(&e.scope)
                    || !hex_digest(&e.key)
                    || !valid_title(&e.title)
                    || e.tracks.is_empty()
                    || e.tracks.iter().any(|id| !valid_id(id))
                    || !seen.insert((&e.scope, &e.key))
            })
        {
            return Err(DAMAGED);
        }
        Ok(())
    }
}

struct IndexDir(OwnedFd);
impl IndexDir {
    fn open_default() -> Result<Self, ApiError> {
        storage(local_storage::open_default(ROOT)).map(Self)
    }
    fn read_existing() -> Result<Index, ApiError> {
        match local_storage::open_default_existing(ROOT) {
            Ok(dir) => Self(dir).read(),
            Err(Errno::NOENT) => Ok(Index::empty()),
            Err(err) => storage(Err(err)),
        }
    }
    #[cfg(test)]
    fn open(path: &std::path::Path) -> Result<Self, ApiError> {
        storage(local_storage::open_private(path)).map(Self)
    }
    fn admit(&self) -> Result<OwnedFd, ApiError> {
        let fd = storage(fs::openat(
            &self.0,
            ADMISSION,
            FILE_FLAGS | OFlags::RDWR | OFlags::CREATE,
            PRIVATE_FILE,
        ))?;
        if private_len(&storage(fs::fstat(&fd))?)? != 0 {
            return Err(DAMAGED);
        }
        lock(&fd)?;
        // Recover an abandoned private stage only under admission.
        match fs::unlinkat(&self.0, STAGE, AtFlags::empty()) {
            Ok(()) | Err(Errno::NOENT) => Ok(fd),
            Err(_) => Err(STORAGE),
        }
    }
    fn read(&self) -> Result<Index, ApiError> {
        // Atomic rename means an independently opened reader sees the entire
        // old or new index, so reads never wait on sequential audio downloads.
        let fd = match fs::openat(&self.0, INDEX, FILE_FLAGS | OFlags::RDONLY, Mode::empty()) {
            Ok(fd) => fd,
            Err(Errno::NOENT) => return Ok(Index::empty()),
            Err(_) => return Err(DAMAGED),
        };
        let len = private_len(&storage(fs::fstat(&fd))?)?;
        // Stream the opened inode without allocating a second copy of the
        // index or reading beyond its measured length into the parser.
        let mut file = BufReader::new(File::from(fd));
        let mut bounded = (&mut file).take(len);
        let value: Index = serde_json::from_reader(&mut bounded).map_err(|_| DAMAGED)?;
        if bounded.limit() != 0 || !file.fill_buf().map_err(|_| DAMAGED)?.is_empty() {
            return Err(DAMAGED);
        }
        value.validate()?;
        Ok(value)
    }
    /// Called while holding this directory's exclusive mutation admission.
    fn commit(
        &self,
        index: &Index,
        live: impl Fn() -> Result<(), ApiError>,
    ) -> Result<(), ApiError> {
        live()?;
        index.validate()?;
        let fd = storage(fs::openat(
            &self.0,
            STAGE,
            FILE_FLAGS | OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL,
            PRIVATE_FILE,
        ))?;
        let mut stage = Stage {
            dir: &self.0,
            published: false,
        };
        // A restrictive umask must not replace a readable index with one the
        // reader would reject. The stage guard cleans up on this failure too.
        if private_len(&storage(fs::fstat(&fd))?)? != 0 {
            return Err(DAMAGED);
        }
        let file = File::from(fd);
        {
            let mut writer = BufWriter::new(&file);
            serde_json::to_writer(&mut writer, index)
                .map_err(|err| err.io_error_kind().map_or(STORAGE, write_error))?;
            writer.flush().map_err(|err| write_error(err.kind()))?;
        }
        file.sync_all().map_err(|err| write_error(err.kind()))?;
        live()?;
        storage(fs::renameat_with(
            &self.0,
            STAGE,
            &self.0,
            INDEX,
            RenameFlags::empty(),
        ))?;
        stage.published = true;
        // The new index is already visible. A sync failure here leaves crash
        // durability uncertain; it does not imply that publication failed.
        fs::fsync(&self.0).map_err(|_| NOT_DURABLE)
    }
}

struct Stage<'a> {
    dir: &'a OwnedFd,
    published: bool,
}
impl Drop for Stage<'_> {
    fn drop(&mut self) {
        if !self.published {
            let _ = fs::unlinkat(self.dir, STAGE, AtFlags::empty());
        }
    }
}

fn private_len(stat: &Stat) -> Result<u64, ApiError> {
    // An open reader on the previous index can have nlink == 0 after replacement.
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile
        || stat.st_uid != geteuid().as_raw()
        || stat.st_nlink > 1
        || stat.st_mode & 0o7777 != 0o600
    {
        return Err(DAMAGED);
    }
    u64::try_from(stat.st_size).map_err(|_| DAMAGED)
}
fn lock(fd: &OwnedFd) -> Result<(), ApiError> {
    fs::flock(fd, FlockOperation::NonBlockingLockExclusive).map_err(|e| {
        if e == Errno::WOULDBLOCK {
            BUSY
        } else {
            STORAGE
        }
    })
}
fn storage<T>(r: rustix::io::Result<T>) -> Result<T, ApiError> {
    r.map_err(|err| match err {
        Errno::NOSPC | Errno::DQUOT => NO_SPACE,
        _ => STORAGE,
    })
}
fn write_error(kind: ErrorKind) -> ApiError {
    match kind {
        ErrorKind::StorageFull | ErrorKind::QuotaExceeded => NO_SPACE,
        _ => STORAGE,
    }
}
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, ApiError> + Send + 'static,
) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(f).await.map_err(|_| STORAGE)?
}

struct Cancellation(Arc<AtomicBool>);
impl Cancellation {
    fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }
}
impl Drop for Cancellation {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}
fn check_cancelled(cancelled: &AtomicBool) -> Result<(), ApiError> {
    if cancelled.load(Ordering::Acquire) {
        Err(CANCELLED)
    } else {
        Ok(())
    }
}
fn check_live(access: &Access) -> Result<(), ApiError> {
    access
        .api
        .authenticated_account(&access.session)
        .map(|_| ())
}
fn scope(access: &Access) -> Result<String, ApiError> {
    let (origin, principal) = access.api.authenticated_account(&access.session)?;
    Ok(hash_fields(
        b"spofify-collection-scope-v1",
        &[origin, principal],
    ))
}
pub(crate) fn playlist_key(access: &Access, id: &str) -> Result<String, ApiError> {
    if !valid_id(id) {
        return Err(UNSUPPORTED);
    }
    Ok(collection_key(&scope(access)?, "playlists", id))
}

fn collection_key(scope: &str, domain: &str, id: &str) -> String {
    hash_fields(b"spofify-collection-key-v1", &[scope, domain, id])
}

fn hash_fields(domain: &[u8], parts: &[&str]) -> String {
    use std::fmt::Write as _;
    let mut hash = Context::new(&SHA256);
    hash.update(domain);
    for part in parts {
        hash.update(&(part.len() as u64).to_be_bytes());
        hash.update(part.as_bytes());
    }
    let mut hex = String::with_capacity(64);
    for byte in hash.finish().as_ref() {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}
fn hex_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f'))
}
fn opaque(value: &str) -> bool {
    !value.is_empty() && value.len() <= 4096 && value.bytes().all(|b| b.is_ascii_graphic())
}
fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 512
        && value.trim() == value
        && !matches!(value, "." | "..")
        && !value.chars().any(char::is_control)
}
fn valid_title(value: &str) -> bool {
    !value.trim().is_empty() && value.len() <= 1024 && !value.chars().any(char::is_control)
}
fn song_count(count: usize) -> String {
    format!("{count} {}", if count == 1 { "song" } else { "songs" })
}
fn audio_error(e: offline::Error) -> ApiError {
    use offline::Error::*;
    match e {
        Api(e) => e,
        InvalidTrack => error("This collection contains an invalid track."),
        NotDownloaded => error("A track is not saved on this device."),
        AlreadyDownloaded => error("This track is already saved."),
        Full => error("There is no space for this download. Free disk space and try again."),
        Busy => error("The download store is busy. Try again shortly."),
        Storage => error("Unable to access private download storage."),
        Damaged => error("The downloaded file or store layout is invalid."),
        RecordingChanged => error("This download changed."),
        UnsupportedAudio => error("This audio cannot be played on this device."),
        InvalidMedia => error("Unable to verify a track’s download."),
        Offline => OFFLINE,
        Cancelled => CANCELLED,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{Api, Origin};
    use serde_json::{Value, json};
    use std::{
        fs as stdfs,
        os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
        path::PathBuf,
    };

    fn track(id: &str) -> Value {
        json!({"id": id, "name": format!("Name {id}"), "kind": "track", "hasAudio": true,
            "artists": [{"id": "ar", "name": "Fixture artist"}], "libraryPath": "/not/retained"})
    }

    fn wire_page(
        kind: &str,
        total: usize,
        range: std::ops::Range<usize>,
        cursor: Option<&str>,
    ) -> Value {
        let items: Vec<_> = range
            .map(|i| {
                json!({"occurrenceId": format!("entry_{i}"), "position": i,
            "track": track(if matches!(i, 0 | 20) { "repeat" } else { "track" })})
            })
            .collect();
        let mut body = json!({"version": 1, "items": items, "fence": format!("f1_{}", "a".repeat(64)),
            "total": total, "recordedAudioTotal": total, "limit": PAGE_SIZE, "nextCursor": cursor});
        if matches!(kind, "album" | "artist") {
            body["collection"] =
                json!({"id": "fixture", "kind": kind, "name": "Fixture collection"});
        } else {
            body["playlist"] = json!({"id": "fixture", "kind": kind, "name": "Fixture playlist", "description": "ignored", "revision": 7});
        }
        body
    }

    fn append(
        state: &mut ManifestState,
        target: Target<'_>,
        body: Value,
    ) -> Result<Option<String>, ApiError> {
        state.append(target, &serde_json::to_vec(&body).unwrap())
    }

    #[test]
    fn artist_route_uses_artist_manifest_and_checks_identity() {
        let os_route = Route::Artist("fixture".into());
        let target = Target::new(&os_route, &Page::default()).unwrap();
        assert_eq!(target.domain(), "artists");
        assert_eq!(target.id(), "fixture");
        let mut state = ManifestState::default();
        assert_eq!(
            append(&mut state, target, wire_page("album", 1, 0..1, None)),
            Err(INVALID)
        );
        let mut confused = wire_page("artist", 1, 0..1, None);
        confused["collection"]["id"] = json!("wrong");
        assert_eq!(append(&mut state, target, confused), Err(INVALID));
        assert_eq!(
            append(&mut state, target, wire_page("artist", 1, 0..1, None)),
            Ok(None)
        );
        let snapshot = state.finish().unwrap();
        assert!(matches!(snapshot.kind, Kind::Artist));
        assert_eq!(snapshot.tracks[0].artists, "Fixture artist");
    }

    #[test]
    fn fenced_playlist_pages_preserve_occurrences_and_reject_malformed_or_oversize_input() {
        for (kind, playlist_kind) in [
            ("manual", PlaylistKind::Manual),
            ("derived", PlaylistKind::Derived),
        ] {
            let target = Target::Playlist {
                id: "fixture",
                kind: playlist_kind,
                revision: Some(7),
            };
            let page_size = PAGE_SIZE as usize;
            let total = 2 * page_size + 1;
            let first = wire_page(kind, total, 0..page_size, Some("of1_first.signature"));
            let second = wire_page(
                kind,
                total,
                page_size..2 * page_size,
                Some("of1_second.signature"),
            );
            let last = wire_page(kind, total, 2 * page_size..total, None);
            let mut state = ManifestState::default();
            let mut body = serde_json::to_vec(&first).unwrap();
            body.resize(MAX_PAGE_BYTES + 1, b' ');
            assert_eq!(state.append(target, &body), Err(INVALID));
            body.pop();
            assert_eq!(
                state.append(target, &body),
                Ok(Some("of1_first.signature".into()))
            );

            let mut stale = second.clone();
            stale["fence"] = json!(format!("f1_{}", "b".repeat(64)));
            assert_eq!(append(&mut state, target, stale), Err(CHANGED));
            let mut repeated_cursor = second.clone();
            repeated_cursor["nextCursor"] = first["nextCursor"].clone();
            assert_eq!(append(&mut state, target, repeated_cursor), Err(INVALID));
            let mut oversized_page = second.clone();
            oversized_page["items"].as_array_mut().unwrap().push(json!({
                "occurrenceId": "extra", "position": 2 * page_size, "track": track("track")
            }));
            assert_eq!(append(&mut state, target, oversized_page), Err(INVALID));
            assert!(append(&mut state, target, second).unwrap().is_some());

            let mut duplicate = last.clone();
            duplicate["items"][0]["occurrenceId"] = json!("entry_0");
            assert_eq!(append(&mut state, target, duplicate), Err(INVALID));
            let mut missing_cursor = last.clone();
            missing_cursor.as_object_mut().unwrap().remove("nextCursor");
            assert_eq!(append(&mut state, target, missing_cursor), Err(INVALID));
            assert_eq!(append(&mut state, target, last), Ok(None));
            let snapshot = state.finish().unwrap();
            assert_eq!(snapshot.tracks.len(), total);
            assert_eq!(snapshot.tracks[0], snapshot.tracks[20]);
        }
    }

    #[test]
    fn large_playlist_manifests_preserve_every_occurrence() {
        const TOTAL: usize = 1161;
        for (kind, playlist_kind) in [
            ("manual", PlaylistKind::Manual),
            ("derived", PlaylistKind::Derived),
        ] {
            let target = Target::Playlist {
                id: "fixture",
                kind: playlist_kind,
                revision: Some(7),
            };
            let mut state = ManifestState::default();
            let page_size = PAGE_SIZE as usize;
            for start in (0..TOTAL).step_by(page_size) {
                let end = (start + page_size).min(TOTAL);
                let cursor = format!("of1_page_{end}.signature");
                let next = (end < TOTAL).then_some(cursor);
                assert_eq!(
                    append(
                        &mut state,
                        target,
                        wire_page(kind, TOTAL, start..end, next.as_deref()),
                    ),
                    Ok(next)
                );
            }
            let snapshot = state.finish().unwrap();
            assert_eq!(snapshot.tracks.len(), TOTAL);
            for (i, item) in snapshot.tracks.iter().enumerate() {
                assert_eq!(
                    item.id,
                    if matches!(i, 0 | 20) {
                        "repeat"
                    } else {
                        "track"
                    }
                );
            }
        }
    }

    #[test]
    fn unavailable_manifest_members_and_per_track_failures_are_partial() {
        let target = Target::Playlist {
            id: "fixture",
            kind: PlaylistKind::Manual,
            revision: Some(7),
        };
        let mut body = wire_page("manual", 3, 0..3, None);
        body["recordedAudioTotal"] = json!(1);
        body["items"][0]["track"] = track("unavailable");
        body["items"][0]["track"]["hasAudio"] = json!(false);
        body["items"][2]["track"] = body["items"][0]["track"].clone();
        let mut state = ManifestState::default();
        assert_eq!(append(&mut state, target, body), Ok(None));
        let snapshot = state.finish().unwrap();
        assert_eq!(
            snapshot
                .tracks
                .iter()
                .map(|t| (t.id.as_str(), t.has_audio))
                .collect::<Vec<_>>(),
            [
                ("unavailable", false),
                ("track", true),
                ("unavailable", false)
            ]
        );
        for status in [404, 410, 413, 415] {
            assert_eq!(
                handle_track_error(offline::Error::Api(ApiError {
                    status,
                    ..NOT_FOUND
                })),
                Ok(())
            );
        }
        assert_eq!(handle_track_error(offline::Error::UnsupportedAudio), Ok(()));
        for err in [
            offline::Error::Api(CHANGED),
            offline::Error::InvalidMedia,
            offline::Error::RecordingChanged,
            offline::Error::Full,
        ] {
            assert_eq!(handle_track_error(err), Err(audio_error(err)));
        }
    }

    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            let path = stdfs::canonicalize(std::env::temp_dir())
                .unwrap()
                .join(format!("spofify-collections-test-{}", uuid::Uuid::new_v4()));
            stdfs::DirBuilder::new().mode(0o700).create(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            stdfs::remove_dir_all(&self.0).unwrap();
        }
    }

    fn search_stage(dir: &Scratch, domain: &'static str, cancel: &Cancellation) -> SearchStaging {
        static SLOT: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);
        SearchStaging::open(
            local_storage::open_private(&dir.0).unwrap(),
            domain,
            "fixture".into(),
            SLOT.try_acquire().unwrap(),
            cancel.0.clone(),
        )
        .unwrap()
    }

    fn search_wire(
        kind: &str,
        total: usize,
        range: std::ops::Range<usize>,
        cursor: Option<&str>,
    ) -> Value {
        let mut page = wire_page(kind, total, range.clone(), cursor);
        for (row, position) in page["items"].as_array_mut().unwrap().iter_mut().zip(range) {
            row["track"] = track(&format!("song:{position}/音"));
        }
        page
    }

    fn append_search(stage: &mut SearchStaging, page: &Value) -> Result<Option<String>, ApiError> {
        stage.append(&serde_json::to_vec(page).unwrap())
    }

    #[tokio::test]
    async fn search_source_spool_is_anonymous_and_replays_exact_ids_in_bounded_batches() {
        let dir = Scratch::new();
        let cancel = Cancellation::new();
        let total = 1161;
        let size = PAGE_SIZE as usize;
        for (kind, domain) in [("artist", "artists"), ("album", "albums")] {
            let mut stage = search_stage(&dir, domain, &cancel);
            for start in (0..total).step_by(size) {
                let end = (start + size).min(total);
                let cursor = (end < total).then(|| format!("ofc1_next_{end}.signature"));
                let page = search_wire(kind, total, start..end, cursor.as_deref());
                assert_eq!(append_search(&mut stage, &page), Ok(cursor));
            }
            assert!(stage.keys.slots.capacity > 64);
            for file in [&stage.keys.records, &stage.keys.slots.file] {
                let stat = fs::fstat(file).unwrap();
                assert_eq!(stat.st_nlink, 0);
                assert_eq!(stat.st_mode & (0o7777 & !0o600), 0);
            }
            let mut spool = stage.finish().unwrap();
            assert_eq!(spool.total(), total as u64);
            let first = spool.next_batch(1).await.unwrap();
            assert_eq!(first, ["song:0/音"]);
            let mut position = first.len();
            while position != total {
                let batch = spool.next_batch(usize::MAX).await.unwrap();
                assert_eq!(batch.len(), size.min(total - position));
                for id in batch {
                    assert_eq!(id, format!("song:{position}/音"));
                    position += 1;
                }
            }
            assert!(spool.next_batch(1).await.unwrap().is_empty());
        }
        assert_eq!(stdfs::read_dir(&dir.0).unwrap().count(), 0);
    }

    #[test]
    fn search_source_spool_rejects_cross_page_drift_and_repeats() {
        let dir = Scratch::new();
        let cancel = Cancellation::new();
        let size = PAGE_SIZE as usize;
        let total = size * 2 + 1;
        let first = search_wire("album", total, 0..size, Some("ofc1_first.signature"));
        let second = search_wire(
            "album",
            total,
            size..size * 2,
            Some("ofc1_second.signature"),
        );
        let changes = [
            (json!(format!("f1_{}", "b".repeat(64))), "/fence", CHANGED),
            (json!("Renamed album"), "/collection/name", CHANGED),
            (json!(total - 1), "/recordedAudioTotal", CHANGED),
            (json!("artist"), "/collection/kind", INVALID),
            (json!("wrong"), "/collection/id", INVALID),
            (json!(size + 1), "/items/0/position", INVALID),
            (json!(false), "/items/0/track/hasAudio", INVALID),
            (
                first["items"][0]["occurrenceId"].clone(),
                "/items/0/occurrenceId",
                INVALID,
            ),
            (
                first["items"][0]["track"]["id"].clone(),
                "/items/1/track/id",
                INVALID,
            ),
            (first["nextCursor"].clone(), "/nextCursor", INVALID),
            (json!("ofc1_.signature"), "/nextCursor", INVALID),
            (json!("ofc1_more..dots"), "/nextCursor", INVALID),
            (Value::Null, "/nextCursor", INVALID),
        ];
        for (value, pointer, expected) in changes {
            let mut stage = search_stage(&dir, "albums", &cancel);
            assert!(append_search(&mut stage, &first).unwrap().is_some());
            let mut bad = second.clone();
            *bad.pointer_mut(pointer).unwrap() = value;
            assert_eq!(append_search(&mut stage, &bad), Err(expected), "{pointer}");
            assert_eq!(stage.finish().err(), Some(INVALID), "{pointer}");
        }
        assert_eq!(stdfs::read_dir(&dir.0).unwrap().count(), 0);
    }

    #[test]
    fn search_source_checks_recorded_count_final_cursor_and_cancellation() {
        let dir = Scratch::new();
        let cancel = Cancellation::new();
        let mut empty = search_wire("artist", 0, 0..0, None);
        let mut stage = search_stage(&dir, "artists", &cancel);
        assert_eq!(append_search(&mut stage, &empty), Ok(None));
        assert_eq!(stage.finish().unwrap().total(), 0);

        empty.as_object_mut().unwrap().remove("nextCursor");
        let mut stage = search_stage(&dir, "artists", &cancel);
        assert_eq!(append_search(&mut stage, &empty), Err(INVALID));

        let mut page = search_wire("artist", 1, 0..1, None);
        page["recordedAudioTotal"] = json!(0);
        assert_eq!(append_search(&mut stage, &page), Err(INVALID));
        page["recordedAudioTotal"] = json!(1);
        assert_eq!(append_search(&mut stage, &page), Ok(None));
        drop(cancel);
        assert_eq!(stage.finish().err(), Some(SEARCH_CANCELLED));
        assert_eq!(stdfs::read_dir(&dir.0).unwrap().count(), 0);
    }

    fn account(origin: &str) -> Access {
        let api = Arc::new(Api::new(Origin::parse(origin).unwrap()).unwrap());
        let session = api.test_session();
        Access {
            epoch: 0,
            api,
            session,
            offline: false,
            personal: crate::personal_bridge::PersonalAccess::default(),
        }
    }

    fn pin_target(kind: PlaylistKind) -> Target<'static> {
        Target::Playlist {
            id: "fixture",
            kind,
            revision: None,
        }
    }

    fn append_pin(
        state: &mut ManifestState<PlaylistTracks>,
        kind: PlaylistKind,
        body: Value,
    ) -> Result<Option<String>, ApiError> {
        state.append(pin_target(kind), &serde_json::to_vec(&body).unwrap())
    }

    #[test]
    fn queued_playlist_needs_no_online_revision_but_fences_kind_and_manifest_revision() {
        let mut state = ManifestState::default();
        let total = PAGE_SIZE as usize + 1;
        let first = wire_page("derived", total, 0..total - 1, Some("of1_next.signature"));
        let last = wire_page("derived", total, total - 1..total, None);
        assert_eq!(
            append_pin(&mut state, PlaylistKind::Manual, first.clone()),
            Err(CHANGED)
        );
        let mut invalid = first.clone();
        invalid["playlist"]["revision"] = json!(0);
        assert_eq!(
            append_pin(&mut state, PlaylistKind::Derived, invalid),
            Err(INVALID)
        );
        assert!(
            append_pin(&mut state, PlaylistKind::Derived, first)
                .unwrap()
                .is_some()
        );
        let mut changed_revision = last.clone();
        changed_revision["playlist"]["revision"] = json!(8);
        assert_eq!(
            append_pin(&mut state, PlaylistKind::Derived, changed_revision),
            Err(CHANGED)
        );
        assert_eq!(
            append_pin(&mut state, PlaylistKind::Derived, last),
            Ok(None)
        );
        assert!(state.finish_parts().is_ok());
    }

    #[test]
    fn queued_playlist_rejects_conflicting_track_metadata_across_pages() {
        let total = PAGE_SIZE as usize + 1;
        let first = wire_page("manual", total, 0..total - 1, Some("of1_next.signature"));
        let mut last = wire_page("manual", total, total - 1..total, None);
        last["items"][0]["track"] = first["items"][0]["track"].clone();
        last["items"][0]["track"]["name"] = json!("Conflicting name");
        let mut state = ManifestState::default();
        assert!(
            append_pin(&mut state, PlaylistKind::Manual, first)
                .unwrap()
                .is_some()
        );
        assert_eq!(
            append_pin(&mut state, PlaylistKind::Manual, last),
            Err(INVALID)
        );
        assert_eq!(state.finish_parts().err(), Some(INVALID));
    }

    #[tokio::test]
    async fn empty_pin_needs_no_audio_store_and_unrecorded_occurrences_do_not_transfer() {
        let access = account("https://fixture.invalid");
        let mut state = ManifestState::default();
        append_pin(
            &mut state,
            PlaylistKind::Manual,
            wire_page("manual", 0, 0..0, None),
        )
        .unwrap();
        let mut milestones = Vec::new();
        let membership = state
            .complete(
                &access,
                |n, total| milestones.push((n, total)),
                || Ok(()),
                || panic!("empty playlist must not open the audio store"),
            )
            .await
            .unwrap();
        assert!(membership.tracks.is_empty());
        assert_eq!(
            membership.provenance,
            Some(Provenance {
                revision: 7,
                occurrence_ids: Vec::new()
            })
        );
        assert_eq!(milestones, [(0, 0)]);
        assert!(
            lock_playlist_members_with(&access, &membership, || {
                panic!("empty playlist must not open the audio store at commit")
            })
            .unwrap()
            .is_none()
        );
        let page = saved_playlist_page(
            &access,
            membership.title,
            PlaylistKind::Manual,
            membership.tracks,
        )
        .await
        .unwrap();
        assert_eq!(page.total, Some(0));
        assert!(page.rows.is_empty());

        let mut body = wire_page("manual", 3, 0..3, None);
        body["items"][0]["track"]["hasAudio"] = json!(false);
        body["items"][2]["track"] = body["items"][0]["track"].clone();
        let mut state = ManifestState::default();
        append_pin(&mut state, PlaylistKind::Manual, body.clone()).unwrap();
        let result = state
            .complete(&access, |_, _| {}, || Ok(()), || panic!("wrong wire count"))
            .await;
        assert_eq!(result.err(), Some(INVALID));
        body["recordedAudioTotal"] = json!(1);
        let mut state = ManifestState::default();
        append_pin(&mut state, PlaylistKind::Manual, body).unwrap();
        milestones.clear();
        let result = state
            .complete(
                &access,
                |n, total| milestones.push((n, total)),
                || Ok(()),
                || panic!("every occurrence must be recorded before any audio I/O"),
            )
            .await;
        assert_eq!(result.err(), Some(UNRECORDED));
        assert_eq!(milestones, [(0, 3)]);
    }

    // Write the v1 audio record format directly for network-free digest tests.
    fn install_audio(root: &std::path::Path, access: &Access, id: &str, audio: &[u8]) -> PathBuf {
        fn digest(bytes: &[u8]) -> String {
            ring::digest::digest(&SHA256, bytes)
                .as_ref()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect()
        }
        let (origin, principal) = access.api.authenticated_account(&access.session).unwrap();
        let scope = hash_fields(b"spofify-download-scope-v1\0", &[origin, principal]);
        let path = root.join(format!("{scope}.{}", digest(id.as_bytes())));
        let head = serde_json::to_vec(&json!({"scope": scope,
            "track": {"id": id, "name": id, "artists": "", "album_name": "",
                "duration_ms": 0, "track_number": 0, "disc_number": 0},
            "bytes": audio.len(), "audio_hash": digest(audio),
        }))
        .unwrap();
        let mut file = stdfs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        file.write_all(b"SPOFDL01").unwrap();
        file.write_all(&(head.len() as u32).to_le_bytes()).unwrap();
        file.write_all(&head).unwrap();
        file.write_all(audio).unwrap();
        path
    }

    #[tokio::test]
    async fn queued_transfer_verifies_unique_files_and_preserves_large_ordered_membership() {
        const TOTAL: usize = 1161;
        let access = account("https://fixture.invalid");
        access.session.set_offline(true); // Even a fixture failure cannot trigger HTTP.
        let scratch = Scratch::new();
        let root = scratch.0.join("audio");
        let store = OfflineStore::open(&root).unwrap();
        install_audio(&root, &access, "repeat", b"verified fixture audio");
        install_audio(&root, &access, "track", b"different fixture audio");
        let mut state = ManifestState::default();
        let page_size = PAGE_SIZE as usize;
        for start in (0..TOTAL).step_by(page_size) {
            let end = (start + page_size).min(TOTAL);
            let next = (end < TOTAL).then(|| format!("of1_page_{end}.signature"));
            let mut page = wire_page("derived", TOTAL, start..end, next.as_deref());
            for (position, item) in (start..end).zip(page["items"].as_array_mut().unwrap()) {
                // Use IDs deliberately unrelated to track or forward position.
                item["occurrenceId"] = json!(format!("o1_fixture_{}", TOTAL - position));
            }
            assert_eq!(
                append_pin(&mut state, PlaylistKind::Derived, page),
                Ok(next)
            );
        }
        assert_eq!(state.memory.tracks.unique.len(), 2);
        let mut milestones = Vec::new();
        let test_store = store.clone();
        let membership = state
            .complete(
                &access,
                |n, total| milestones.push((n, total)),
                || Ok(()),
                move || Ok(test_store),
            )
            .await
            .unwrap();
        assert_eq!(membership.title, "Fixture playlist");
        assert_eq!(membership.tracks.len(), TOTAL);
        let provenance = membership.provenance.as_ref().unwrap();
        assert_eq!(provenance.revision, 7);
        assert_eq!(provenance.occurrence_ids.len(), TOTAL);
        for (index, (id, occurrence_id)) in membership
            .tracks
            .iter()
            .zip(&provenance.occurrence_ids)
            .enumerate()
        {
            assert_eq!(
                id,
                if matches!(index, 0 | 20) {
                    "repeat"
                } else {
                    "track"
                }
            );
            assert_eq!(occurrence_id, &format!("o1_fixture_{}", TOTAL - index));
        }
        assert_eq!(milestones, [(0, TOTAL), (2, TOTAL), (TOTAL, TOTAL)]);

        let key = playlist_key(&access, "fixture").unwrap();
        assert_eq!(
            key,
            "054c3638d07b51addb399dcad039d58d44ab9a4d7dd9a2e209cf5fa7729a7055"
        );
        let row = saved_playlist_row(
            &key,
            &membership.title,
            PlaylistKind::Derived,
            "Saved".into(),
        );
        assert_eq!(row.route, Some(Route::SavedCollection(key)));
        assert_eq!(row.subtitle, "Rule playlist");
        assert_eq!(row.detail, "Saved");
        let guard =
            lock_playlist_members_with(&access, &membership, || Ok(Some(store.clone()))).unwrap();
        assert_eq!(
            store.remove(&access.api, &access.session, "track"),
            Err(offline::Error::Busy)
        );
        drop(guard);
        assert!(store.remove(&access.api, &access.session, "track").unwrap());
        assert_eq!(
            lock_playlist_members_with(&access, &membership, || Ok(Some(store.clone()))).err(),
            Some(audio_error(offline::Error::NotDownloaded))
        );
    }

    #[tokio::test]
    async fn a_digest_failure_never_advances_progress_and_removes_only_damaged_audio() {
        let access = account("https://fixture.invalid");
        access.session.set_offline(true);
        let scratch = Scratch::new();
        let root = scratch.0.join("audio");
        let store = OfflineStore::open(&root).unwrap();
        let healthy = install_audio(&root, &access, "repeat", b"good");
        let healthy_bytes = stdfs::read(&healthy).unwrap();
        let path = install_audio(&root, &access, "track", b"four");
        let mut damaged = stdfs::read(&path).unwrap();
        *damaged.last_mut().unwrap() = b'!';
        stdfs::write(&path, &damaged).unwrap();
        assert_eq!(store.list(&access.api, &access.session).unwrap().len(), 2);

        let mut body = wire_page("manual", 3, 0..3, None);
        body["items"][2]["track"] = body["items"][0]["track"].clone();
        let mut state = ManifestState::default();
        append_pin(&mut state, PlaylistKind::Manual, body).unwrap();
        let mut milestones = Vec::new();
        let result = state
            .complete(
                &access,
                |n, total| milestones.push((n, total)),
                || Ok(()),
                move || Ok(store),
            )
            .await;
        assert_eq!(result.err(), Some(audio_error(offline::Error::Damaged)));
        assert_eq!(milestones, [(0, 3), (2, 3)]);
        assert!(!path.exists());
        assert_eq!(stdfs::read(healthy).unwrap(), healthy_bytes);
        let reopened = OfflineStore::open(&root).unwrap();
        let downloads = reopened.list(&access.api, &access.session).unwrap();
        assert_eq!(downloads.len(), 1);
        assert_eq!(downloads[0].item.id, "repeat");
    }

    fn write_index_fixture(path: &std::path::Path, fixture: &Value) {
        let file = stdfs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path.join(INDEX))
            .unwrap();
        serde_json::to_writer(file, fixture).unwrap();
    }

    #[test]
    fn large_catalog_collection_reopens_projects_updates_and_removes() {
        const TOTAL: usize = 1161;
        let scratch = Scratch::new();
        let path = scratch.0.join(ROOT);
        let disk = IndexDir::open(&path).unwrap();
        let _admission = disk.admit().unwrap();
        let principal = "a".repeat(64);
        let foreign = "b".repeat(64);
        let key = collection_key(&principal, "artists", "known-source");
        let sibling_key = collection_key(&principal, "albums", "known-source");
        let ids: Vec<_> = ["saved", "missing", "saved", "another-missing"]
            .into_iter()
            .cycle()
            .take(TOTAL)
            .collect();
        // Exercise the exact on-disk format without using the serializer.
        let fixture = json!({"version": 1, "entries": [
            {"scope": principal, "key": sibling_key, "kind": "album", "title": "Album",
                "tracks": ["saved", "missing", "saved"]},
            {"scope": foreign, "key": key, "kind": "artist", "title": "Other account",
                "tracks": vec!["foreign"; 40]},
            {"scope": principal, "key": key, "kind": "artist", "title": "Saved artist",
                "tracks": ids},
        ]});
        write_index_fixture(&path, &fixture);
        let untouched = stdfs::read(path.join(INDEX)).unwrap();
        let entry = disk
            .read()
            .unwrap()
            .in_scope(&principal)
            .find(|e| e.key == key)
            .unwrap();
        assert_eq!(entry.title, "Saved artist");
        assert_eq!(entry.tracks.iter().map(String::as_str).collect::<Vec<_>>(), ids);
        assert_eq!(stdfs::read(path.join(INDEX)).unwrap(), untouched);
        let saved = serde_json::from_value::<WireTrack>(track("saved"))
            .unwrap()
            .project()
            .unwrap();
        let available = HashMap::from([("saved".into(), synthetic_download(saved))]);
        assert_eq!(
            list_row(entry.clone(), &available).detail,
            "581 of 1161 songs available"
        );
        let empty = entry.clone().detail_page(&HashMap::new());
        assert_eq!(empty.total, Some(TOTAL as u64));
        assert!(empty.rows.is_empty() && empty.status.is_empty());
        let page = entry.detail_page(&available);
        assert_eq!(page.total, Some(TOTAL as u64));
        assert!(page.status.is_empty());
        assert_eq!(page.rows.len(), 581);
        for (row, (position, id)) in page
            .rows
            .iter()
            .zip(ids.into_iter().enumerate().filter(|(_, id)| *id == "saved"))
        {
            assert_eq!(row.position, Some(position as u64));
            assert_eq!(row.id, id);
            assert!(row.playable && row.item.is_some());
            assert_eq!(row.local_recording.as_ref(), Some(&available[id].recording));
            assert!(row.playback.is_none());
        }

        let mut index = disk.read().unwrap();
        index.replace_entry(StoredCollection {
            scope: principal.clone(),
            key: key.clone(),
            kind: Kind::Artist,
            title: "Updated artist".into(),
            tracks: vec!["saved".into(); TOTAL + 1],
        });
        disk.commit(&index, || Ok(())).unwrap();
        let mut index = disk.read().unwrap();
        assert_eq!(index.entries.len(), 3);
        let updated = index
            .entries
            .iter()
            .find(|e| e.scope == principal && e.key == key)
            .unwrap();
        assert_eq!(updated.title, "Updated artist");
        assert_eq!(updated.tracks.len(), TOTAL + 1);
        assert!(index.remove_entry(&principal, &key));
        disk.commit(&index, || Ok(())).unwrap();

        let local = disk
            .read()
            .unwrap()
            .in_scope(&principal)
            .collect::<Vec<_>>();
        assert_eq!(local.len(), 1);
        assert_eq!(local[0].key, sibling_key);
        assert_eq!(local[0].tracks, ["saved", "missing", "saved"]);
        let other = disk.read().unwrap().in_scope(&foreign).collect::<Vec<_>>();
        assert_eq!(other.len(), 1);
        assert_eq!(other[0].key, key);
        assert_eq!(other[0].tracks, vec!["foreign"; 40]);
    }

    #[test]
    fn large_index_remains_readable_after_removal() {
        let scratch = Scratch::new();
        let path = scratch.0.join(ROOT);
        let disk = IndexDir::open(&path).unwrap();
        let _admission = disk.admit().unwrap();
        let scope = "a".repeat(64);
        let mut index = Index::empty();
        for i in 0..68 {
            index.entries.push(StoredCollection {
                scope: scope.clone(),
                key: format!("{i:064x}"),
                kind: Kind::Album,
                title: format!("Album {i}"),
                tracks: vec!["x".repeat(512); 128],
            });
        }
        disk.commit(&index, || Ok(())).unwrap();
        let mut index = disk.read().unwrap();
        assert_eq!(index.entries.len(), 68);
        let removed_key = index.entries[0].key.clone();
        assert!(index.remove_entry(&scope, &removed_key));
        disk.commit(&index, || Ok(())).unwrap();
        // The remaining index still exceeds both former product quotas.
        assert!(stdfs::metadata(path.join(INDEX)).unwrap().len() > 4 * 1024 * 1024);
        assert_eq!(disk.read().unwrap().in_scope(&scope).count(), 67);
    }

    #[test]
    fn index_rejects_malformed_data_without_discarding_it() {
        let scratch = Scratch::new();
        let path = scratch.0.join(ROOT);
        let disk = IndexDir::open(&path).unwrap();
        let _admission = disk.admit().unwrap();
        let index_path = path.join(INDEX);
        drop(
            stdfs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&index_path)
                .unwrap(),
        );
        let record = json!({"scope": "a".repeat(64), "key": "b".repeat(64),
            "kind": "manual", "title": "Fixture", "tracks": ["same", "same"]});
        let valid = json!({"version": 1, "entries": [record.clone()]});
        let mut overlong_id = valid.clone();
        overlong_id["entries"][0]["tracks"][1] = json!("x".repeat(513));
        let mut duplicate_key = valid.clone();
        duplicate_key["entries"]
            .as_array_mut()
            .unwrap()
            .push(record);
        let mut unknown_field = valid.clone();
        unknown_field["entries"][0]["audioPath"] = json!("/not/allowed");
        let mut empty_album = valid.clone();
        empty_album["entries"][0]["kind"] = json!("album");
        empty_album["entries"][0]["tracks"] = json!([]);
        let mut trailing_json = serde_json::to_vec(&valid).unwrap();
        trailing_json.extend_from_slice(b" {}");
        for invalid in [
            serde_json::to_vec(&overlong_id).unwrap(),
            serde_json::to_vec(&duplicate_key).unwrap(),
            serde_json::to_vec(&unknown_field).unwrap(),
            serde_json::to_vec(&empty_album).unwrap(),
            trailing_json,
        ] {
            stdfs::write(&index_path, &invalid).unwrap();
            assert_eq!(disk.read().err(), Some(DAMAGED));
            assert_eq!(stdfs::read(&index_path).unwrap(), invalid);
        }
    }

    #[tokio::test]
    async fn private_index_survives_session_replacement_and_offline_mode() {
        let scratch = Scratch::new();
        let path = scratch.0.join(ROOT);
        let disk = IndexDir::open(&path).unwrap();
        let admission = disk.admit().unwrap();
        assert_eq!(disk.admit().unwrap_err(), BUSY);
        let first = account("https://fixture.invalid");
        let principal = scope(&first).unwrap();
        let key = hash_fields(b"key", &[&principal, "fixture"]);
        let mut index = Index::empty();
        index.entries.push(StoredCollection {
            scope: principal.clone(),
            key: key.clone(),
            title: "Fixture".into(),
            kind: Kind::Album,
            tracks: vec!["two".into(), "one".into(), "two".into()],
        });
        disk.commit(&index, || check_live(&first)).unwrap();
        assert_eq!(
            stdfs::metadata(path.join(INDEX))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert!(
            !stdfs::read_to_string(path.join(INDEX))
                .unwrap()
                .contains("synthetic")
        );
        index.entries[0].tracks.push("unpublished".into());
        assert_eq!(
            disk.commit(&index, || if path.join(STAGE).exists() {
                Err(CANCELLED)
            } else {
                Ok(())
            }),
            Err(CANCELLED)
        );
        assert!(!path.join(STAGE).exists());
        drop(admission);
        drop(disk);
        first.session.set_offline(true);
        assert!(first.api.logout(&first.session).await.is_err()); // Local revocation, no HTTP.
        assert!(scope(&first).unwrap_err().expired);

        let mut next = account("https://FIXTURE.invalid:443");
        next.session.account_id = "mutable display field".into();
        next.session.set_offline(true);
        let disk = IndexDir::open(&path).unwrap();
        let mut visible = disk
            .read()
            .unwrap()
            .in_scope(&scope(&next).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(visible.len(), 1);
        let entry = visible.remove(0);
        assert_eq!(entry.tracks, ["two", "one", "two"]);
        let fixture_item = |id: &str| {
            serde_json::from_value::<WireTrack>(track(id))
                .unwrap()
                .project()
                .unwrap()
        };
        let mut available = HashMap::from([
            ("two".into(), synthetic_download(fixture_item("two"))),
            ("one".into(), synthetic_download(fixture_item("one"))),
        ]);
        assert_eq!(
            list_row(entry.clone(), &available).detail,
            "3 of 3 songs available"
        );
        let full = entry.clone().detail_page(&available);
        assert!(full.status.is_empty());
        assert!(
            full.rows
                .iter()
                .all(|row| { row.local_recording.as_ref() == Some(&available[&row.id].recording) })
        );
        available.remove("two"); // A single removal invalidates both duplicate slots.
        assert_eq!(
            entry.summary(&available),
            SaveSummary {
                present: 1,
                total: 3
            }
        );
        assert_eq!(
            list_row(entry.clone(), &available).detail,
            "1 of 3 songs available"
        );
        let page = entry.detail_page(&available);
        assert_eq!(page.subtitle, "Album · 1 of 3 songs available");
        assert_eq!(page.total, Some(3));
        assert_eq!(
            page.rows
                .iter()
                .map(|r| (r.position, r.id.as_str(), r.playable))
                .collect::<Vec<_>>(),
            [(Some(1), "one", true)]
        );
        assert!(page.rows.iter().all(|r| r.playback.is_none()
            && r.playable == r.item.is_some()
            && r.local_recording.as_ref() == Some(&available[&r.id].recording)));
        assert!(page.status.is_empty());

        let foreign = account("https://other.invalid");
        let foreign_scope = scope(&foreign).unwrap();
        let mut index = disk.read().unwrap();
        assert!(!index.remove_entry(&foreign_scope, &key));
        assert_eq!(disk.read().unwrap().in_scope(&foreign_scope).count(), 0);
        assert!(index.remove_entry(&scope(&next).unwrap(), &key));
        let _admission = disk.admit().unwrap();
        disk.commit(&index, || check_live(&next)).unwrap();
        assert_eq!(disk.read().unwrap().in_scope(&principal).count(), 0);
    }
}
