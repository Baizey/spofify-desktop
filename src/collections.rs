//! Explicit, ordered collection downloads. Membership is local metadata, not
//! authority to use audio: the track store still owns admission and playback.
//! Removing a collection never removes audio, including audio saved by a failed
//! or cancelled collection save. Individual downloads can be removed separately.

use std::{
    collections::{HashMap, HashSet},
    fs::File,
    io::{Read, Write},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
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
    offline::{self, OfflineStore},
    pages::{Page, Route, Row, RowKey},
    storage as local_storage,
};

// Bound both the number of requests and the number of playback occurrences,
// including repeats. Audio bytes have separate limits enforced by OfflineStore.
const MAX_OCCURRENCES: usize = offline::MAX_DOWNLOADS;
const MAX_COLLECTIONS: usize = 64;
const MAX_PAGE_BYTES: usize = 2 * 1024 * 1024;
const MAX_INDEX_BYTES: usize = 4 * 1024 * 1024;
const ROOT: &str = "spofify-collections-v1";
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
const FULL: ApiError =
    error("The saved collection index is full. Remove a collection and try again.");
const INVALID: ApiError = error("The server returned an incompatible collection manifest.");
const TOO_LARGE: ApiError = error(
    "This collection exceeds the offline collection song limit. Save individual tracks instead.",
);
const UNSUPPORTED: ApiError = error("This page cannot be saved as a collection.");
const EMPTY: ApiError = error("No tracks in this collection could be saved.");
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
    let access = access.clone();
    blocking(move || {
        let scope = scope(&access)?;
        let index = IndexDir::open_default()?.read()?;
        let mut entries: Vec<_> = index.in_scope(&scope).collect();
        entries.sort_unstable_by(|a, b| a.title.cmp(&b.title).then_with(|| a.key.cmp(&b.key)));
        let available = if entries.is_empty() {
            HashMap::new()
        } else {
            let store = OfflineStore::open_default().map_err(audio_error)?;
            available_tracks(&access, &store)?
        };
        check_live(&access)?;
        Ok(entries
            .into_iter()
            .map(|entry| list_row(entry, &available))
            .collect())
    })
    .await
}

pub async fn detail(access: &Access, id: &str) -> Result<Page, ApiError> {
    if !hex_digest(id) {
        return Err(NOT_FOUND);
    }
    let access = access.clone();
    let id = id.to_owned();
    blocking(move || {
        let scope = scope(&access)?;
        let entry = IndexDir::open_default()?
            .read()?
            .in_scope(&scope)
            .find(|e| e.key == id)
            .ok_or(NOT_FOUND)?;
        let store = OfflineStore::open_default().map_err(audio_error)?;
        let available = available_tracks(&access, &store)?;
        check_live(&access)?;
        Ok(detail_page(entry, &available))
    })
    .await
}

fn available_tracks(
    access: &Access,
    store: &OfflineStore,
) -> Result<HashMap<String, Item>, ApiError> {
    // Local track headers supply safe metadata; full byte verification is
    // deferred to OfflineStore::read when the user initiates playback.
    Ok(store
        .list(&access.api, &access.session)
        .map_err(audio_error)?
        .into_iter()
        .map(|d| (d.item.id.clone(), d.item))
        .collect())
}

fn list_row(entry: StoredCollection, available: &HashMap<String, Item>) -> Row {
    let detail = entry.summary(available).label();
    Row {
        kind: "saved-collection".into(),
        id: entry.key.clone(),
        route: Some(Route::SavedCollection(entry.key)),
        title: entry.title,
        subtitle: entry.kind.label().into(),
        detail,
        ..Row::default()
    }
}

fn detail_page(entry: StoredCollection, available: &HashMap<String, Item>) -> Page {
    let summary = entry.summary(available);
    let rows = entry
        .tracks
        .iter()
        .enumerate()
        .map(|(position, id)| {
            let mut row = Row {
                kind: "track".into(),
                id: id.clone(),
                key: RowKey::Track(id.clone()),
                position: Some(position as u64),
                ..Row::default()
            };
            if let Some(item) = available.get(id) {
                row.title.clone_from(&item.name);
                row.subtitle.clone_from(&item.artists);
                row.detail.clone_from(&item.album_name);
                row.playable = true;
                row.item = Some(item.clone());
            } else {
                row.title = "Unavailable song".into();
                row.detail = "Not saved on this device".into();
            }
            row
        })
        .collect();
    let missing = summary.total - summary.present;
    Page {
        title: entry.title,
        subtitle: format!("{} · {}", entry.kind.label(), summary.label()),
        kind: "saved-collection".into(),
        total: Some(summary.total as u64),
        status: if missing > 0 {
            format!("{} not available on this device.", song_count(missing))
        } else {
            String::new()
        },
        rows,
        ..Page::default()
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
        dir.commit(&index.encode()?, live)
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
    fn label(self) -> String {
        format!("{} of {} available", self.present, song_count(self.total))
    }
}

pub async fn save(access: &Access, route: &Route, page: &Page) -> Result<SaveSummary, ApiError> {
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
            let store = OfflineStore::open_default().map_err(audio_error)?;
            Ok((scope, dir, admission, index, store))
        })
    };
    let cancel = Cancellation::new();
    let cancelled = cancel.0.clone();
    tokio::select! {
        biased;
        _ = network.changed() => Err(OFFLINE),
        result = async {
            let (scope, dir, admission, mut index, store) = open.await?;
            let snapshot = snapshot(access, target).await?;
            if snapshot.tracks.is_empty() { return Err(EMPTY); }
            let key = hash_fields(b"spofify-collection-key-v1", &[&scope, target.domain(), target.id()]);
            index.remove_entry(&scope, &key);
            index.entries.push(StoredCollection {
                scope, key, kind: snapshot.kind, title: snapshot.title,
                tracks: snapshot.tracks.iter().map(|t| t.id.clone()).collect(),
            });
            // Preflight the index limits and all manifest pages before audio I/O.
            let bytes = index.encode()?;
            let entry = index.entries.pop().ok_or(DAMAGED)?;
            let mut seen = HashSet::new();
            for item in &snapshot.tracks {
                if !item.has_audio || !seen.insert(&item.id) { continue; }
                check_live(access)?;
                if let Err(err) = store.download(&access.api, &access.session, item).await {
                    handle_track_error(err)?;
                }
            }
            let access = access.clone();
            let changes = access.session.network_changes();
            blocking(move || {
                let _admission = admission;
                // Count existing downloads as well, and don't count an earlier
                // success if the track has already been removed.
                let summary = entry.summary(&available_tracks(&access, &store)?);
                if summary.present == 0 { return Err(EMPTY); }
                dir.commit(&bytes, || {
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

fn handle_track_error(err: offline::Error) -> Result<(), ApiError> {
    match err {
        offline::Error::AlreadyDownloaded
        | offline::Error::NotDownloaded
        | offline::Error::UnsupportedAudio
        | offline::Error::TrackTooLarge => Ok(()),
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
        expected: Option<(PlaylistKind, u64)>,
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
                    expected: Some((playlist.kind, playlist.revision)),
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
    // Read legacy offline downloads without invalidating the rest of the index.
    Likes,
}
impl Kind {
    fn label(self) -> &'static str {
        match self {
            Self::Album => "Album",
            Self::Artist => "Artist",
            Self::Manual => "Playlist",
            Self::Derived => "Rule playlist",
            Self::Likes => "Liked songs",
        }
    }
}

struct Snapshot {
    kind: Kind,
    title: String,
    tracks: Vec<Item>,
}

async fn snapshot(access: &Access, target: Target<'_>) -> Result<Snapshot, ApiError> {
    let mut state = ManifestState::default();
    let mut cursor = None;
    loop {
        let wire = access
            .api
            .offline_collection_manifest(
                &access.session,
                target.domain(),
                target.id(),
                cursor.as_deref(),
            )
            .await?;
        cursor = state.append(target, &wire)?;
        if cursor.is_none() {
            return state.finish();
        }
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
struct ManifestState {
    metadata: Option<(Header, String, u64, u64, Kind)>,
    tracks: Vec<Item>,
    occurrences: HashSet<String>,
    cursors: HashSet<String>,
}
impl ManifestState {
    fn append(&mut self, target: Target<'_>, body: &[u8]) -> Result<Option<String>, ApiError> {
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
            (Target::Playlist { expected, .. }, None, Some(header)) => {
                let (kind, playlist_kind) = match header.kind.as_str() {
                    "manual" => (Kind::Manual, PlaylistKind::Manual),
                    "derived" => (Kind::Derived, PlaylistKind::Derived),
                    _ => return Err(INVALID),
                };
                let revision = header
                    .revision
                    .filter(|r| (1..=9_007_199_254_740_991).contains(r))
                    .ok_or(INVALID)?;
                if expected.is_some_and(|value| value != (playlist_kind, revision)) {
                    return Err(CHANGED);
                }
                (header, kind, "of1_")
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
        if wire.total > MAX_OCCURRENCES as u64 {
            return Err(TOO_LARGE);
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
        let offset = self.tracks.len() as u64;
        let remaining = wire.total.checked_sub(offset).ok_or(INVALID)?;
        let count = wire.items.len() as u64;
        if count != remaining.min(u64::from(PAGE_SIZE))
            || wire.next_cursor.is_some() != (count < remaining)
        {
            return Err(INVALID);
        }
        if let Some(next) = &wire.next_cursor {
            if !next.starts_with(prefix) || !opaque(next) || !self.cursors.insert(next.clone()) {
                return Err(INVALID);
            }
        }
        for (i, occurrence) in wire.items.into_iter().enumerate() {
            if occurrence.position != offset + i as u64
                || !opaque(&occurrence.occurrence_id)
                || !self.occurrences.insert(occurrence.occurrence_id)
            {
                return Err(INVALID);
            }
            self.tracks.push(occurrence.track.project()?);
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
    fn finish(self) -> Result<Snapshot, ApiError> {
        let (header, _, total, recorded, kind) = self.metadata.ok_or(INVALID)?;
        if total != self.tracks.len() as u64
            || recorded != self.tracks.iter().filter(|t| t.has_audio).count() as u64
        {
            return Err(INVALID);
        }
        Ok(Snapshot {
            kind,
            title: header.name,
            tracks: self.tracks,
        })
    }
}

// One small private index; a collection stores only its disjoint account/key
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
    fn summary(&self, available: &HashMap<String, Item>) -> SaveSummary {
        SaveSummary {
            present: self
                .tracks
                .iter()
                .filter(|id| available.contains_key(*id))
                .count(),
            total: self.tracks.len(),
        }
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
    fn validate(&self) -> Result<(), ApiError> {
        let mut seen = HashSet::new();
        if self.entries.len() > MAX_COLLECTIONS
            || self.version != 1
            || self.entries.iter().any(|e| {
                !hex_digest(&e.scope)
                    || !hex_digest(&e.key)
                    || !valid_title(&e.title)
                    || e.tracks.is_empty()
                    || e.tracks.len() > MAX_OCCURRENCES
                    || e.tracks.iter().any(|id| !valid_id(id))
                    || !seen.insert((&e.scope, &e.key))
            })
        {
            return Err(DAMAGED);
        }
        Ok(())
    }
    fn encode(&self) -> Result<Vec<u8>, ApiError> {
        if self.entries.len() > MAX_COLLECTIONS {
            return Err(FULL);
        }
        self.validate()?;
        let json = serde_json::to_vec(self).map_err(|_| STORAGE)?;
        if json.len() > MAX_INDEX_BYTES {
            return Err(FULL);
        }
        Ok(json)
    }
}

struct IndexDir(OwnedFd);
impl IndexDir {
    fn open_default() -> Result<Self, ApiError> {
        storage(local_storage::open_default(ROOT)).map(Self)
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
        if !(1..=MAX_INDEX_BYTES as u64).contains(&len) {
            return Err(DAMAGED);
        }
        let mut json = Vec::with_capacity(len as usize);
        File::from(fd)
            .take(len + 1)
            .read_to_end(&mut json)
            .map_err(|_| DAMAGED)?;
        if json.len() as u64 != len {
            return Err(DAMAGED);
        }
        let value: Index = serde_json::from_slice(&json).map_err(|_| DAMAGED)?;
        value.validate()?;
        Ok(value)
    }
    /// Called while holding this directory's exclusive mutation admission.
    fn commit(
        &self,
        bytes: &[u8],
        live: impl Fn() -> Result<(), ApiError>,
    ) -> Result<(), ApiError> {
        live()?;
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
        let mut file = File::from(fd);
        file.write_all(bytes).map_err(|_| STORAGE)?;
        file.sync_all().map_err(|_| STORAGE)?;
        live()?;
        storage(fs::renameat_with(
            &self.0,
            STAGE,
            &self.0,
            INDEX,
            RenameFlags::empty(),
        ))?;
        stage.published = true;
        storage(fs::fsync(&self.0))
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
    r.map_err(|_| STORAGE)
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
        Full => error("The download store is full. Remove a download and try again."),
        Busy => error("The download store is busy. Try again shortly."),
        Storage => error("Unable to access private download storage."),
        Damaged => error("The downloaded file or store layout is invalid."),
        UnsupportedAudio => error("This audio cannot be played on this device."),
        InvalidMedia => error("Unable to verify a track’s download."),
        TrackTooLarge => error("A track exceeds the 32 MiB download limit."),
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
        os::unix::fs::{DirBuilderExt, PermissionsExt},
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
                expected: Some((playlist_kind, 7)),
            };
            let first = wire_page(kind, 21, 0..20, Some("of1_demo.signature"));
            let last = wire_page(kind, 21, 20..21, None);
            let mut state = ManifestState::default();
            assert!(append(&mut state, target, first.clone()).unwrap().is_some());
            let mut stale = last.clone();
            stale["fence"] = json!(format!("f1_{}", "b".repeat(64)));
            assert_eq!(append(&mut state, target, stale), Err(CHANGED));
            let mut duplicate = last.clone();
            duplicate["items"][0]["occurrenceId"] = json!("entry_0");
            assert_eq!(append(&mut state, target, duplicate), Err(INVALID));
            let mut missing_cursor = last.clone();
            missing_cursor.as_object_mut().unwrap().remove("nextCursor");
            assert_eq!(append(&mut state, target, missing_cursor), Err(INVALID));
            assert_eq!(append(&mut state, target, last), Ok(None));
            let snapshot = state.finish().unwrap();
            assert_eq!(snapshot.tracks.len(), 21);
            assert_eq!(snapshot.tracks[0], snapshot.tracks[20]);
            assert_eq!(
                append(
                    &mut ManifestState::default(),
                    target,
                    wire_page(kind, MAX_OCCURRENCES + 1, 0..20, Some("of1_demo.signature"))
                ),
                Err(TOO_LARGE)
            );
        }
    }

    #[test]
    fn unavailable_manifest_members_and_per_track_failures_are_partial() {
        let target = Target::Playlist {
            id: "fixture",
            expected: Some((PlaylistKind::Manual, 7)),
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
        for err in [
            offline::Error::UnsupportedAudio,
            offline::Error::TrackTooLarge,
        ] {
            assert_eq!(handle_track_error(err), Ok(()));
        }
        for err in [
            offline::Error::Api(CHANGED),
            offline::Error::InvalidMedia,
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
        let bytes = index.encode().unwrap();
        disk.commit(&bytes, || check_live(&first)).unwrap();
        assert_eq!(
            stdfs::metadata(path.join(INDEX))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert!(
            !String::from_utf8(bytes.clone())
                .unwrap()
                .contains("synthetic")
        );
        assert_eq!(
            disk.commit(&bytes, || if path.join(STAGE).exists() {
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
            ("two".into(), fixture_item("two")),
            ("one".into(), fixture_item("one")),
        ]);
        assert_eq!(
            list_row(entry.clone(), &available).detail,
            "3 of 3 songs available"
        );
        assert!(detail_page(entry.clone(), &available).status.is_empty());
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
        let page = detail_page(entry, &available);
        assert_eq!(page.subtitle, "Album · 1 of 3 songs available");
        assert_eq!(page.total, Some(3));
        assert_eq!(
            page.rows
                .iter()
                .map(|r| (r.position, r.id.as_str(), r.playable))
                .collect::<Vec<_>>(),
            [
                (Some(0), "two", false),
                (Some(1), "one", true),
                (Some(2), "two", false)
            ]
        );
        assert!(
            page.rows
                .iter()
                .all(|r| r.playback.is_none() && r.playable == r.item.is_some())
        );
        assert_eq!(page.status, "2 songs not available on this device.");

        let foreign = account("https://other.invalid");
        let foreign_scope = scope(&foreign).unwrap();
        let mut index = disk.read().unwrap();
        assert!(!index.remove_entry(&foreign_scope, &key));
        assert_eq!(disk.read().unwrap().in_scope(&foreign_scope).count(), 0);
        assert!(index.remove_entry(&scope(&next).unwrap(), &key));
        let _admission = disk.admit().unwrap();
        disk.commit(&index.encode().unwrap(), || check_live(&next))
            .unwrap();
        assert_eq!(disk.read().unwrap().in_scope(&principal).count(), 0);
    }
}
