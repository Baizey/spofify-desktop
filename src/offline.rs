//! Explicitly downloaded audio. Linux, local filesystem, private to the OS user.
//! Explicit Offline uses the non-revoked local grant from the current sign-in,
//! independently of the network cookie deadline. Nothing grants offline sign-in.
//! Downloaded bytes survive logout but become inaccessible until the same account
//! at the same canonical origin signs in again. No credentials or server paths
//! are persisted. A download grants no player/coordinator authority.

use std::{
    fmt,
    fs::File,
    io::{Read, Write},
    mem::MaybeUninit,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use ring::digest::{Context, SHA256, digest};
use rustix::{
    fd::OwnedFd,
    fs::{self, AtFlags, FileType, FlockOperation, Mode, OFlags, RawDir, RenameFlags, Stat},
    io::Errno,
    process::geteuid,
};
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;

use crate::{
    api::{Api, ApiError, Item, Session},
    storage::{self as local_storage, DIR_FLAGS},
};

pub const MAX_TRACK_BYTES: usize = 32 * 1024 * 1024;
const MAX_HEADER: usize = 16 * 1024;
const MAGIC: &[u8; 8] = b"SPOFDL01";
const PREFIX_BYTES: usize = 12;
const LOCK: &str = ".download-lock";
const STAGE: &str = ".partial";
// Earlier development builds also used the v1 directory for their cache metadata.
// These are not audio records. This app never opens or deletes them.
const LEGACY_CACHE_INDEX: &str = ".playback-cache-v1";
const LEGACY_CACHE_STAGE: &str = ".playback-cache-partial";
const ROOT_DIR: &str = "spofify-downloads-v1";
const PRIVATE_FILE: Mode = Mode::RUSR.union(Mode::WUSR);
const FILE_FLAGS: OFlags = OFlags::NOFOLLOW
    .union(OFlags::CLOEXEC)
    .union(OFlags::NONBLOCK);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Api(ApiError),
    InvalidTrack,
    NotDownloaded,
    AlreadyDownloaded,
    Full,
    Busy,
    Storage,
    Damaged,
    UnsupportedAudio,
    InvalidMedia,
    TrackTooLarge,
    Offline,
    Cancelled,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Api(err) => err.message,
            Self::InvalidTrack => "This item cannot be downloaded.",
            Self::NotDownloaded => "This track has not been downloaded.",
            Self::AlreadyDownloaded => "This track is already downloaded.",
            Self::Full => "There is no space for this download. Free disk space and try again.",
            Self::Busy => "The download store is busy. Try again shortly.",
            Self::Storage => "Unable to access private download storage.",
            Self::Damaged => "The downloaded file or store layout is invalid.",
            Self::UnsupportedAudio => "This audio cannot be played on this device.",
            Self::InvalidMedia => "Unable to verify this track’s download.",
            Self::TrackTooLarge => "This track exceeds the 32 MiB download limit.",
            Self::Offline => "Downloads are unavailable while offline.",
            Self::Cancelled => "The download was cancelled.",
        })
    }
}
impl std::error::Error for Error {}
impl From<ApiError> for Error {
    fn from(err: ApiError) -> Self {
        Self::Api(err)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Download {
    pub item: Item,
    pub bytes: u64,
}

/// Same bounded original-format bytes as `Api::media`, for use by the existing
/// audio endpoint. The caller must still fence late results and playback on
/// session/occurrence changes, and use the appropriate player admission path.
pub struct PlaybackInput {
    pub item: Item,
    pub bytes: Vec<u8>,
}

pub(crate) struct OnlineMedia {
    pub bytes: Arc<[u8]>,
    /// Do not add a second, RAM-backed copy to the online stream cache.
    pub downloaded: bool,
}

/// Choose bytes only after the caller has admitted online local output. This
/// does not interpret queue or output state. The caller must still fence the
/// result by account epoch, occurrence, and coordinator authority.
pub(crate) async fn online_media(
    api: &Arc<Api>,
    session: &Session,
    slot: Arc<Semaphore>,
    id: &str,
    cached: Option<Arc<[u8]>>,
) -> Result<OnlineMedia, ApiError> {
    online_media_with_store(api, session, slot, id, cached, OfflineStore::open_existing).await
}

async fn online_media_with_store(
    api: &Arc<Api>,
    session: &Session,
    slot: Arc<Semaphore>,
    id: &str,
    cached: Option<Arc<[u8]>>,
    open: impl FnOnce() -> Result<Option<OfflineStore>, Error> + Send + 'static,
) -> Result<OnlineMedia, ApiError> {
    let network = session.network_changes();
    let stamp = *network.borrow();
    api.with_authenticated_network(session, async {
        let permit = slot.acquire_owned().await.ok();
        if let Some(permit) = permit {
            let (api, session, id) = (api.clone(), session.clone(), id.to_owned());
            let local = tokio::task::spawn_blocking(move || {
                // Hold the same worker slot as Saved playback, even if the
                // awaiting online task is dropped or the occurrence changes.
                let _permit = permit;
                if *network.borrow() != stamp || stamp & 1 != 0 || session.is_expired() {
                    return None;
                }
                let input = open().ok()??.read(&api, &session, &id).ok()?;
                Some(Arc::from(input.bytes))
            })
            .await
            .ok()
            .flatten();
            if let Some(bytes) = local {
                return Ok(OnlineMedia {
                    bytes,
                    downloaded: true,
                });
            }
        }
        let bytes = match cached {
            Some(bytes) => bytes,
            None => api.media(session, id).await?.into(),
        };
        Ok(OnlineMedia {
            bytes,
            downloaded: false,
        })
    })
    .await
}

#[derive(Clone)]
pub struct OfflineStore {
    root: Arc<OwnedFd>,
}

impl OfflineStore {
    /// Use absolute XDG_DATA_HOME or HOME/.local/share; only the data base
    /// may resolve aliases. Never fall back to a shared temp directory.
    pub fn open_default() -> Result<Self, Error> {
        Ok(Self {
            root: Arc::new(storage(local_storage::open_default(ROOT_DIR))?),
        })
    }

    pub(crate) fn open_existing() -> Result<Option<Self>, Error> {
        match local_storage::open_default_existing(ROOT_DIR) {
            Ok(root) => Ok(Some(Self {
                root: Arc::new(root),
            })),
            Err(Errno::NOENT) => Ok(None),
            Err(_) => Err(Error::Storage),
        }
    }

    /// Open a private fixture by explicit path, without any alias resolution.
    #[cfg(test)]
    pub(crate) fn open(root: &std::path::Path) -> Result<Self, Error> {
        Ok(Self {
            root: Arc::new(storage(local_storage::open_private(root))?),
        })
    }

    /// Manual operation only. No prefetch, eviction or coordinator state mutation.
    /// Only one download per store is admitted at a time, including its fetch.
    pub async fn download(
        &self,
        api: &Api,
        session: &Session,
        item: &Item,
    ) -> Result<Download, Error> {
        let scope = Scope::new(api, session)?;
        let mut network = session.network_changes();
        let generation = *network.borrow_and_update();
        if generation & 1 != 0 {
            return Err(Error::Offline);
        }
        Track::validate_item(item)?;
        let track = Track::from(item);
        let name = scope.name(&track.id)?;
        let store = self.clone();
        let admission = tokio::task::spawn_blocking(move || store.admit(&name))
            .await
            .map_err(|_| Error::Storage)??;
        api.authenticated_account(session)?;
        let prepare = async {
            let body = api.offline_manifest(session, &track.id).await?;
            let manifest = Manifest::parse(&body, &track.id)?;
            if *session.network_changes().borrow() != generation {
                return Err(Error::Offline);
            }
            let audio = api
                .offline_audio(
                    session,
                    &track.id,
                    &manifest.media_version,
                    &manifest.content_type,
                    manifest.byte_length,
                )
                .await?;
            Ok::<_, Error>((manifest, audio))
        };
        let (manifest, audio) = tokio::select! {
            biased;
            _ = network.changed() => return Err(Error::Offline),
            result = prepare => result?,
        };
        api.authenticated_account(session)?;
        let store = self.clone();
        let session = session.clone();
        let cancellation = CancelOnDrop(Arc::new(AtomicBool::new(false)));
        let cancelled = cancellation.0.clone();
        tokio::task::spawn_blocking(move || {
            let _admission = admission;
            let live = || {
                if cancelled.load(Ordering::Acquire) {
                    return Err(Error::Cancelled);
                }
                if *network.borrow() != generation {
                    return Err(Error::Offline);
                }
                check_online_liveness(&session)
            };
            live()?;
            manifest.verify(&audio)?;
            let audio = probe_audio(audio)?;
            store.commit(&scope, track, audio.as_ref(), live)
        })
        .await
        .map_err(|_| Error::Storage)?
    }

    /// Disk I/O only; use a blocking worker when calling from an async/UI actor.
    pub fn list(&self, api: &Api, session: &Session) -> Result<Vec<Download>, Error> {
        let scope = Scope::new(api, session)?;
        let dir = self.lock_dir(false)?;
        let mut downloads = Vec::new();
        self.scan(&dir, |name| {
            if scope.contains(name) {
                api.authenticated_account(session)?;
                let (_, header) = open_entry(&dir, name, &scope)?;
                downloads.push(header.download());
            }
            Ok(())
        })?;
        api.authenticated_account(session)?;
        downloads.sort_unstable_by(|a, b| {
            a.item
                .name
                .cmp(&b.item.name)
                .then_with(|| a.item.id.cmp(&b.item.id))
        });
        Ok(downloads)
    }

    /// Read and verify one track. Does not fall back to a network request.
    pub fn read(
        &self,
        api: &Api,
        session: &Session,
        track_id: &str,
    ) -> Result<PlaybackInput, Error> {
        let scope = Scope::new(api, session)?;
        let dir = self.lock_dir(false)?;
        let (mut file, header) = open_entry(&dir, &scope.name(track_id)?, &scope)?;
        if header.track.id != track_id {
            return Err(Error::Damaged);
        }
        let mut bytes = vec![0; header.bytes as usize];
        for chunk in bytes.chunks_mut(64 * 1024) {
            api.authenticated_account(session)?;
            file.read_exact(chunk).map_err(|_| Error::Damaged)?;
        }
        let mut end = [0];
        if file.read(&mut end).map_err(|_| Error::Damaged)? != 0
            || header.audio_hash != hex(digest(&SHA256, &bytes).as_ref())
        {
            return Err(Error::Damaged);
        }
        api.authenticated_account(session)?;
        Ok(PlaybackInput {
            item: header.track.item(),
            bytes,
        })
    }

    /// Hold the shared directory lock across playlist publication. Each
    /// required recording was already verified during transfer; no other
    /// cooperating process can remove an entry between this check and commit.
    pub(crate) fn lock_members(
        &self,
        api: &Api,
        session: &Session,
        ids: &[String],
    ) -> Result<OwnedFd, Error> {
        let scope = Scope::new(api, session)?;
        let dir = self.lock_dir(false)?;
        let mut seen = std::collections::HashSet::new();
        for id in ids {
            if seen.insert(id) {
                api.authenticated_account(session)?;
                open_entry(&dir, &scope.name(id)?, &scope)?;
            }
        }
        api.authenticated_account(session)?;
        Ok(dir)
    }

    /// Remove only the requested key in the active account/origin namespace.
    /// Unlink by descriptor; even a damaged entry is never dereferenced here.
    /// Returns false if it was already absent.
    pub fn remove(&self, api: &Api, session: &Session, track_id: &str) -> Result<bool, Error> {
        let scope = Scope::new(api, session)?;
        let name = scope.name(track_id)?;
        let dir = self.lock_dir(true)?;
        api.authenticated_account(session)?;
        match fs::unlinkat(&dir, name.as_str(), AtFlags::empty()) {
            Ok(()) => {
                storage(fs::fsync(&dir))?;
                Ok(true)
            }
            Err(Errno::NOENT) => Ok(false),
            Err(_) => Err(Error::Storage),
        }
    }

    fn lock_dir(&self, exclusive: bool) -> Result<OwnedFd, Error> {
        // A distinct open-file description per flock (not dup/Arc sharing).
        let dir = storage(fs::openat(
            self.root.as_ref(),
            ".",
            DIR_FLAGS,
            Mode::empty(),
        ))?;
        storage(local_storage::private_dir(&dir))?;
        lock(
            &dir,
            if exclusive {
                FlockOperation::NonBlockingLockExclusive
            } else {
                FlockOperation::NonBlockingLockShared
            },
        )?;
        Ok(dir)
    }

    fn admit(&self, name: &str) -> Result<OwnedFd, Error> {
        let dir = self.lock_dir(true)?;
        let admission = storage(fs::openat(
            &dir,
            LOCK,
            FILE_FLAGS | OFlags::RDWR | OFlags::CREATE,
            PRIVATE_FILE,
        ))?;
        if file_size(&storage(fs::fstat(&admission))?)? != 0 {
            return Err(Error::Damaged);
        }
        lock(&admission, FlockOperation::NonBlockingLockExclusive)?;
        // Recover an abandoned private staging file after a process crash.
        match fs::unlinkat(&dir, STAGE, AtFlags::empty()) {
            Ok(()) | Err(Errno::NOENT) => (),
            Err(_) => return Err(Error::Storage),
        }
        reject_existing(&dir, name)?;
        Ok(admission)
    }

    fn scan(
        &self,
        dir: &OwnedFd,
        mut entry: impl FnMut(&str) -> Result<(), Error>,
    ) -> Result<(), Error> {
        let mut buffer = [MaybeUninit::uninit(); 4096];
        let mut iterator = RawDir::new(dir, &mut buffer);
        while let Some(next) = iterator.next() {
            let next = storage(next)?;
            let name = next.file_name().to_str().map_err(|_| Error::Damaged)?;
            if matches!(
                name,
                "." | ".." | LOCK | STAGE | LEGACY_CACHE_INDEX | LEGACY_CACHE_STAGE
            ) {
                continue;
            }
            if !valid_key(name) {
                return Err(Error::Damaged);
            }
            validate_entry_stat(&storage(fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW))?)?;
            entry(name)?;
        }
        Ok(())
    }

    /// Must hold download admission. The staging file is not part of any listing.
    fn commit(
        &self,
        scope: &Scope,
        track: Track,
        bytes: &[u8],
        live: impl Fn() -> Result<(), Error>,
    ) -> Result<Download, Error> {
        if bytes.is_empty() || bytes.len() > MAX_TRACK_BYTES {
            return Err(Error::UnsupportedAudio);
        }
        let header = Header {
            scope: scope.0.clone(),
            track,
            bytes: bytes.len() as u64,
            audio_hash: hex(digest(&SHA256, bytes).as_ref()),
        };
        let name = scope.name(&header.track.id)?;
        let json = serde_json::to_vec(&header).map_err(|_| Error::Storage)?;
        if json.is_empty() || json.len() > MAX_HEADER {
            return Err(Error::InvalidTrack);
        }
        let dir = self.lock_dir(true)?;
        live()?;
        reject_existing(&dir, &name)?;
        let file = storage(fs::openat(
            &dir,
            STAGE,
            FILE_FLAGS | OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL,
            PRIVATE_FILE,
        ))?;
        let mut stage = Stage {
            dir: &dir,
            published: false,
        };
        if file_size(&storage(fs::fstat(&file))?)? != 0 {
            return Err(Error::Damaged);
        }
        let mut file = File::from(file);
        file.write_all(MAGIC)
            .and_then(|_| file.write_all(&(json.len() as u32).to_le_bytes()))
            .and_then(|_| file.write_all(&json))
            .map_err(write_error)?;
        for chunk in bytes.chunks(64 * 1024) {
            live()?;
            file.write_all(chunk).map_err(write_error)?;
        }
        file.sync_all().map_err(write_error)?;
        live()?;
        // Atomic no-overwrite publication; this is the commit point.
        match fs::renameat_with(&dir, STAGE, &dir, name.as_str(), RenameFlags::NOREPLACE) {
            Ok(()) => stage.published = true,
            Err(Errno::EXIST) => return Err(Error::AlreadyDownloaded),
            Err(err) => return Err(storage_error(err)),
        }
        if let Err(err) = fs::fsync(&dir) {
            let _ = fs::unlinkat(&dir, name.as_str(), AtFlags::empty());
            let _ = fs::fsync(&dir);
            return Err(storage_error(err));
        }
        Ok(header.download())
    }
}

/// Give Rodio an owned, `'static` reader without cloning the audio allocation.
#[derive(Clone)]
struct ProbeBuffer(Arc<Vec<u8>>);

impl AsRef<[u8]> for ProbeBuffer {
    fn as_ref(&self) -> &[u8] {
        self.0.as_slice()
    }
}

fn probe_audio(bytes: Vec<u8>) -> Result<ProbeBuffer, Error> {
    let bytes = ProbeBuffer(Arc::new(bytes));
    // Admission check only; one decoded sample does not validate every frame.
    let mut decoder = rodio::Decoder::try_from(std::io::Cursor::new(bytes.clone()))
        .map_err(|_| Error::UnsupportedAudio)?;
    decoder.next().ok_or(Error::UnsupportedAudio)?;
    Ok(bytes)
}

struct CancelOnDrop(Arc<AtomicBool>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
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

struct Scope(String);
impl Scope {
    fn new(api: &Api, session: &Session) -> Result<Self, Error> {
        let (origin, account) = api.authenticated_account(session)?;
        Ok(Self::from_parts(origin, account))
    }

    fn from_parts(origin: &str, account: &str) -> Self {
        let mut key = Context::new(&SHA256);
        key.update(b"spofify-download-scope-v1\0");
        for field in [origin, account] {
            key.update(&(field.len() as u64).to_be_bytes());
            key.update(field.as_bytes());
        }
        Self(hex(key.finish().as_ref()))
    }

    fn name(&self, track_id: &str) -> Result<String, Error> {
        if !valid_id(track_id) {
            return Err(Error::InvalidTrack);
        }
        Ok(format!(
            "{}.{}",
            self.0,
            hex(digest(&SHA256, track_id.as_bytes()).as_ref())
        ))
    }

    fn contains(&self, name: &str) -> bool {
        name.split_once('.')
            .is_some_and(|(scope, _)| scope == self.0)
    }
}

// `/media` prepares an immutable transfer identity. Ignore `downloadUrl`:
// construct the fixed authenticated route from the typed ID and digest instead.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Manifest {
    version: u8,
    track_id: String,
    media_version: String,
    sha256: String,
    byte_length: usize,
    format: String,
    content_type: String,
}
impl Manifest {
    fn parse(body: &[u8], track_id: &str) -> Result<Self, Error> {
        if body.len() > 16 * 1024 {
            return Err(Error::InvalidMedia);
        }
        let value: Self = serde_json::from_slice(body).map_err(|_| Error::InvalidMedia)?;
        let expected_mime = match value.format.as_str() {
            "mp3" => "audio/mpeg",
            "flac" => "audio/flac",
            "m4a" => "audio/mp4",
            "aac" => "audio/aac",
            "ogg" | "opus" => "audio/ogg",
            "wav" => "audio/wav",
            "aif" | "aiff" => "audio/aiff",
            "webm" => "audio/webm",
            _ => return Err(Error::UnsupportedAudio),
        };
        if value.version != 1
            || value.track_id != track_id
            || value.content_type != expected_mime
            || !hex_digest(&value.sha256)
            || value.media_version.strip_prefix("sha256:") != Some(value.sha256.as_str())
            || value.byte_length == 0
        {
            return Err(Error::InvalidMedia);
        }
        if value.byte_length > MAX_TRACK_BYTES {
            return Err(Error::TrackTooLarge);
        }
        Ok(value)
    }

    fn verify(&self, bytes: &[u8]) -> Result<(), Error> {
        if self.byte_length != bytes.len() || self.sha256 != hex(digest(&SHA256, bytes).as_ref()) {
            return Err(Error::InvalidMedia);
        }
        Ok(())
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Header {
    scope: String,
    track: Track,
    bytes: u64,
    audio_hash: String,
}
impl Header {
    fn download(self) -> Download {
        Download {
            item: self.track.item(),
            bytes: self.bytes,
        }
    }
}

// Explicit persistence allowlist: never serialize an API response, session or path.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Track {
    id: String,
    name: String,
    artists: String,
    album_name: String,
    duration_ms: u64,
    track_number: u32,
    disc_number: u32,
}
impl From<&Item> for Track {
    fn from(item: &Item) -> Self {
        Self {
            id: item.id.clone(),
            name: item.name.clone(),
            artists: item.artists.clone(),
            album_name: item.album_name.clone(),
            duration_ms: item.duration_ms,
            track_number: item.track_number,
            disc_number: item.disc_number,
        }
    }
}
impl Track {
    fn item(self) -> Item {
        Item {
            id: self.id,
            name: self.name,
            kind: "track".into(),
            artists: self.artists,
            album_name: self.album_name,
            duration_ms: self.duration_ms,
            has_audio: true,
            track_number: self.track_number,
            disc_number: self.disc_number,
        }
    }

    fn validate_item(item: &Item) -> Result<(), Error> {
        if item.kind != "track"
            || !item.has_audio
            || !valid_track_fields(
                &item.id,
                &item.name,
                &item.artists,
                &item.album_name,
                item.duration_ms,
                item.track_number,
                item.disc_number,
            )
        {
            return Err(Error::InvalidTrack);
        }
        Ok(())
    }

    fn valid(&self) -> bool {
        valid_track_fields(
            &self.id,
            &self.name,
            &self.artists,
            &self.album_name,
            self.duration_ms,
            self.track_number,
            self.disc_number,
        )
    }
}

fn open_entry(dir: &OwnedFd, name: &str, scope: &Scope) -> Result<(File, Header), Error> {
    let fd = fs::openat(dir, name, FILE_FLAGS | OFlags::RDONLY, Mode::empty()).map_err(|e| {
        if e == Errno::NOENT {
            Error::NotDownloaded
        } else {
            Error::Damaged
        }
    })?;
    let actual = file_size(&storage(fs::fstat(&fd))?)?;
    let mut file = File::from(fd);
    let mut prefix = [0; PREFIX_BYTES];
    file.read_exact(&mut prefix).map_err(|_| Error::Damaged)?;
    let len = u32::from_le_bytes([prefix[8], prefix[9], prefix[10], prefix[11]]) as usize;
    if &prefix[..8] != MAGIC
        || !(1..=MAX_HEADER).contains(&len)
        || actual > max_entry_bytes()
        || actual <= (PREFIX_BYTES + len) as u64
    {
        return Err(Error::Damaged);
    }
    let mut json = vec![0; len];
    file.read_exact(&mut json).map_err(|_| Error::Damaged)?;
    let header: Header = serde_json::from_slice(&json).map_err(|_| Error::Damaged)?;
    if header.scope != scope.0
        || !header.track.valid()
        || !hex_digest(&header.audio_hash)
        || !(1..=MAX_TRACK_BYTES as u64).contains(&header.bytes)
        || actual != (PREFIX_BYTES + len) as u64 + header.bytes
        || scope.name(&header.track.id).map_err(|_| Error::Damaged)? != name
    {
        return Err(Error::Damaged);
    }
    Ok((file, header))
}

fn max_entry_bytes() -> u64 {
    (PREFIX_BYTES + MAX_HEADER + MAX_TRACK_BYTES) as u64
}

// Admission only needs the candidate key. Real filesystem writes enforce
// capacity; walking the entire library for each new save would be quadratic.
fn reject_existing(dir: &OwnedFd, name: &str) -> Result<(), Error> {
    match fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => {
            validate_entry_stat(&stat)?;
            Err(Error::AlreadyDownloaded)
        }
        Err(Errno::NOENT) => Ok(()),
        Err(err) => Err(storage_error(err)),
    }
}

fn validate_entry_stat(s: &Stat) -> Result<(), Error> {
    if !(PREFIX_BYTES as u64 + 2..=max_entry_bytes()).contains(&file_size(s)?) {
        return Err(Error::Damaged);
    }
    Ok(())
}

fn file_size(s: &Stat) -> Result<u64, Error> {
    if FileType::from_raw_mode(s.st_mode) != FileType::RegularFile
        || s.st_uid != geteuid().as_raw()
        || s.st_nlink != 1
        || (s.st_mode & 0o7777) != 0o600
    {
        return Err(Error::Damaged);
    }
    u64::try_from(s.st_size).map_err(|_| Error::Damaged)
}

fn storage<T>(result: rustix::io::Result<T>) -> Result<T, Error> {
    result.map_err(storage_error)
}
fn storage_error(err: Errno) -> Error {
    match err {
        Errno::NOSPC | Errno::DQUOT => Error::Full,
        _ => Error::Storage,
    }
}
fn write_error(err: std::io::Error) -> Error {
    err.raw_os_error()
        .map(Errno::from_raw_os_error)
        .map_or(Error::Storage, storage_error)
}
fn lock(fd: &OwnedFd, operation: FlockOperation) -> Result<(), Error> {
    fs::flock(fd, operation).map_err(|err| {
        if err == Errno::WOULDBLOCK {
            Error::Busy
        } else {
            Error::Storage
        }
    })
}
fn check_online_liveness(session: &Session) -> Result<(), Error> {
    if session.is_expired() {
        return Err(ApiError {
            message: "Your session has expired. Sign in again.",
            expired: true,
            status: 401,
        }
        .into());
    }
    Ok(())
}
fn valid_track_fields(
    id: &str,
    name: &str,
    artist: &str,
    album: &str,
    ms: u64,
    track: u32,
    disc: u32,
) -> bool {
    valid_id(id)
        && !name.trim().is_empty()
        && valid_text(name, 1024)
        && valid_text(artist, 4096)
        && valid_text(album, 1024)
        && (album.is_empty() || !album.trim().is_empty())
        && ms <= 7 * 24 * 60 * 60 * 1000
        && track <= 100_000
        && disc <= 100_000
}
fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.trim() == id && !matches!(id, "." | "..") && valid_text(id, 512)
}
fn valid_text(text: &str, cap: usize) -> bool {
    text.len() <= cap && !text.chars().any(char::is_control)
}
fn valid_key(name: &str) -> bool {
    name.split_once('.')
        .is_some_and(|(a, b)| hex_digest(a) && hex_digest(b))
}
fn hex_digest(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}
fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8] = b"0123456789abcdef";
    let mut result = String::with_capacity(2 * bytes.len());
    for &byte in bytes {
        result.push(HEX[(byte >> 4) as usize] as char);
        result.push(HEX[(byte & 15) as usize] as char);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Origin;
    use std::{
        env,
        fs::{self as stdfs, DirBuilder, OpenOptions},
        io::{Seek, SeekFrom},
        os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt, symlink},
        path::PathBuf,
    };

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            // Honor TMPDIR: some sandbox-mounted /tmp filesystems do not
            // support atomic RENAME_NOREPLACE or absolute symlinks.
            let base = stdfs::canonicalize(env::temp_dir()).expect("resolve the test TMPDIR");
            let path = base.join(format!("spofify-store-test-{}", uuid::Uuid::new_v4()));
            DirBuilder::new().mode(0o700).create(&path).unwrap();
            Self(path)
        }
        fn root(&self) -> PathBuf {
            self.0.join("saved")
        }
        fn store(&self) -> OfflineStore {
            OfflineStore::open(&self.root()).unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            stdfs::remove_dir_all(&self.0).unwrap();
        }
    }

    fn api(url: &str) -> Api {
        Api::new(Origin::parse(url).unwrap()).unwrap()
    }
    fn track(id: &str) -> Track {
        Track {
            id: id.into(),
            name: "Fixture track".into(),
            artists: "Fixture artist".into(),
            album_name: String::new(),
            duration_ms: 10,
            track_number: 1,
            disc_number: 1,
        }
    }
    fn install(
        store: &OfflineStore,
        scope: &Scope,
        id: &str,
        data: &[u8],
    ) -> Result<Download, Error> {
        let _guard = store.admit(&scope.name(id)?)?;
        store.commit(scope, track(id), data, || Ok(()))
    }

    #[tokio::test]
    async fn verified_download_wins_over_stream_reuse_while_online() {
        let fixture = Fixture::new();
        let api = Arc::new(api("https://one.invalid"));
        let session = api.test_session();
        let store = fixture.store();
        let own = Scope::new(&api, &session).unwrap();
        let foreign = Scope::from_parts("https://one.invalid", "other-account");
        install(&store, &own, "saved", b"verified local bytes").unwrap();
        install(&store, &foreign, "foreign", b"other user data").unwrap();
        let read = |id: &'static str| {
            let store = store.clone();
            online_media_with_store(
                &api,
                &session,
                Arc::new(Semaphore::new(1)),
                id,
                Some(Arc::from(&b"reusable stream"[..])),
                move || Ok(Some(store)),
            )
        };
        let chosen = read("saved").await.unwrap();
        assert!(chosen.downloaded);
        assert_eq!(&*chosen.bytes, b"verified local bytes");
        for id in ["foreign", "missing"] {
            let chosen = read(id).await.unwrap();
            assert!(!chosen.downloaded);
            assert_eq!(&*chosen.bytes, b"reusable stream");
        }
        session.set_offline(true);
        assert!(read("saved").await.is_err());
    }

    #[test]
    fn persistent_entries_require_same_authenticated_origin_and_account() {
        let fixture = Fixture::new();
        let first = api("https://one.invalid");
        let session = first.test_session();
        let scope = Scope::new(&first, &session).unwrap();
        let other = Scope::from_parts("https://one.invalid", "other-account");
        let store = fixture.store();
        install(&store, &scope, "track-1", b"fixture bytes").unwrap();
        install(&store, &other, "track-1", b"other account payload").unwrap();
        assert!(matches!(
            install(&store, &scope, "track-1", b"replacement"),
            Err(Error::AlreadyDownloaded)
        ));
        drop(store);
        let reopened = fixture.store();
        let same = api("https://ONE.invalid:443");
        let new_session = same.test_session();
        new_session.set_offline(true);
        assert_eq!(reopened.list(&same, &new_session).unwrap().len(), 1);
        assert_eq!(
            reopened.read(&same, &new_session, "track-1").unwrap().bytes,
            b"fixture bytes"
        );
        let mut altered = new_session.clone();
        altered.account_id = "other-account".into();
        assert_eq!(
            reopened.read(&same, &altered, "track-1").unwrap().bytes,
            b"fixture bytes"
        );
        let foreign = api("https://two.invalid");
        assert!(
            reopened
                .list(&foreign, &foreign.test_session())
                .unwrap()
                .is_empty()
        );
        assert!(matches!(
            reopened.list(&foreign, &new_session),
            Err(Error::Api(_))
        ));
        assert!(reopened.remove(&same, &new_session, "track-1").unwrap());
        assert!(!reopened.remove(&same, &new_session, "track-1").unwrap());
        assert!(reopened.list(&same, &new_session).unwrap().is_empty());
        assert!(fixture.root().join(other.name("track-1").unwrap()).exists());
    }

    #[tokio::test]
    async fn explicit_offline_blocks_download_but_local_logout_revokes_reads() {
        let fixture = Fixture::new();
        let store = fixture.store();
        let api = api("https://one.invalid");
        let session = api.test_session();
        let scope = Scope::new(&api, &session).unwrap();
        install(&store, &scope, "track", b"verified fixture").unwrap();
        session.set_offline(true);
        assert!(matches!(
            store.download(&api, &session, &track("new").item()).await,
            Err(Error::Offline)
        ));
        assert!(store.read(&api, &session, "track").is_ok());
        // The API revokes locally before checking whether it may send logout.
        assert!(api.logout(&session).await.is_err());
        assert!(matches!(store.list(&api, &session), Err(Error::Api(err)) if err.expired));
        assert!(matches!(store.read(&api, &session, "track"), Err(Error::Api(err)) if err.expired));
        assert!(
            matches!(store.remove(&api, &session, "track"), Err(Error::Api(err)) if err.expired)
        );
    }

    #[tokio::test]
    async fn explicit_offline_local_grant_survives_cookie_expiry_but_not_logout() {
        let fixture = Fixture::new();
        let store = fixture.store();
        let api = api("https://one.invalid");
        let provision = api.test_session();
        let scope = Scope::new(&api, &provision).unwrap();
        install(&store, &scope, "keep", b"verified fixture").unwrap();
        install(&store, &scope, "remove", b"second fixture").unwrap();

        let expired = api.test_session_with_deadline(std::time::Instant::now());
        assert!(expired.is_expired());
        assert!(matches!(store.list(&api, &expired), Err(Error::Api(err)) if err.expired));
        expired.set_offline(true);
        assert!(expired.is_expired()); // The network cookie is still expired.
        assert_eq!(store.list(&api, &expired).unwrap().len(), 2);
        assert_eq!(
            store.read(&api, &expired, "keep").unwrap().bytes,
            b"verified fixture"
        );
        assert!(store.remove(&api, &expired, "remove").unwrap());
        assert_eq!(store.list(&api, &expired).unwrap().len(), 1);
        assert!(matches!(
            store.download(&api, &expired, &track("new").item()).await,
            Err(Error::Offline)
        ));

        // Logout revokes the shared local grant without making a request.
        assert!(api.logout(&expired.clone()).await.is_err());
        assert!(matches!(store.read(&api, &expired, "keep"), Err(Error::Api(err)) if err.expired));
        assert!(
            matches!(store.remove(&api, &expired, "keep"), Err(Error::Api(err)) if err.expired)
        );
        assert!(store.read(&api, &provision, "keep").is_ok());
    }

    #[test]
    fn rejects_symlink_hardlink_fifo_and_world_readable_root() {
        let fixture = Fixture::new();
        let target = fixture.0.join("outside");
        stdfs::write(&target, b"do not touch").unwrap();
        let store = fixture.store();
        assert_eq!(
            stdfs::metadata(fixture.root())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        let api = api("https://one.invalid");
        let session = api.test_session();
        let scope = Scope::new(&api, &session).unwrap();
        let file = fixture.root().join(scope.name("damaged").unwrap());
        symlink(&target, &file).unwrap();
        assert!(matches!(
            store.read(&api, &session, "damaged"),
            Err(Error::Damaged)
        ));
        assert!(store.remove(&api, &session, "damaged").unwrap());
        assert_eq!(stdfs::read(&target).unwrap(), b"do not touch");
        stdfs::set_permissions(&target, stdfs::Permissions::from_mode(0o600)).unwrap();
        stdfs::hard_link(&target, &file).unwrap();
        assert!(matches!(
            store.read(&api, &session, "damaged"),
            Err(Error::Damaged)
        ));
        assert!(store.remove(&api, &session, "damaged").unwrap());
        let dir = store.lock_dir(true).unwrap();
        fs::mknodat(
            &dir,
            scope.name("damaged").unwrap().as_str(),
            FileType::Fifo,
            PRIVATE_FILE,
            0,
        )
        .unwrap();
        drop(dir);
        assert!(matches!(
            store.read(&api, &session, "damaged"),
            Err(Error::Damaged)
        ));
        assert!(store.remove(&api, &session, "damaged").unwrap());
        let alias = fixture.0.join("alias");
        symlink(fixture.root(), &alias).unwrap();
        assert!(matches!(OfflineStore::open(&alias), Err(Error::Storage)));
        stdfs::set_permissions(fixture.root(), stdfs::Permissions::from_mode(0o755)).unwrap();
        assert!(matches!(
            OfflineStore::open(&fixture.root()),
            Err(Error::Storage)
        ));
    }

    #[test]
    fn cancellation_duplicate_admission_and_corruption_fail_closed() {
        let fixture = Fixture::new();
        let store = fixture.store();
        let api = api("https://one.invalid");
        let session = api.test_session();
        let scope = Scope::new(&api, &session).unwrap();
        let stale = fixture.root().join(STAGE);
        stdfs::write(&stale, b"stale").unwrap();
        let key = scope.name("track").unwrap();
        let permit = store.admit(&key).unwrap();
        assert!(!stale.exists());
        assert!(matches!(fixture.store().admit(&key), Err(Error::Busy)));
        let calls = std::cell::Cell::new(0);
        let result = store.commit(&scope, track("track"), b"temporary", || {
            calls.set(calls.get() + 1);
            if calls.get() == 3 {
                Err(Error::Cancelled)
            } else {
                Ok(())
            }
        });
        assert!(matches!(result, Err(Error::Cancelled)));
        assert!(!stale.exists());
        assert!(!fixture.root().join(&key).exists());
        drop(permit);
        install(&store, &scope, "track", b"original").unwrap();
        let path = fixture.root().join(key);
        assert_eq!(
            stdfs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let mut file = OpenOptions::new().write(true).open(path).unwrap();
        file.seek(SeekFrom::End(-1)).unwrap();
        file.write_all(b"!").unwrap();
        drop(file);
        assert!(matches!(
            store.read(&api, &session, "track"),
            Err(Error::Damaged)
        ));
        assert!(store.remove(&api, &session, "track").unwrap());
    }

    // Simulate files left by a build without the old release's global quotas.
    fn write_sparse_entry(root: &std::path::Path, scope: &Scope, id: &str, bytes: u64, hash: &str) {
        let json = serde_json::to_vec(&Header {
            scope: scope.0.clone(),
            track: track(id),
            bytes,
            audio_hash: hash.into(),
        })
        .unwrap();
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(root.join(scope.name(id).unwrap()))
            .unwrap();
        file.write_all(MAGIC).unwrap();
        file.write_all(&(json.len() as u32).to_le_bytes()).unwrap();
        file.write_all(&json).unwrap();
        file.set_len((PREFIX_BYTES + json.len()) as u64 + bytes)
            .unwrap();
    }

    #[test]
    fn large_v1_store_lists_reads_removes_and_admits_new_downloads() {
        const SAVED_COUNT: usize = 1089;
        let fixture = Fixture::new();
        let api = api("https://one.invalid");
        let session = api.test_session();
        let scope = Scope::new(&api, &session).unwrap();
        let store = fixture.store();
        let hash = hex(digest(&SHA256, &[0]).as_ref());
        for index in 0..SAVED_COUNT {
            write_sparse_entry(&fixture.root(), &scope, &format!("track-{index}"), 1, &hash);
        }
        for name in [LEGACY_CACHE_INDEX, LEGACY_CACHE_STAGE] {
            stdfs::write(fixture.root().join(name), b"opaque legacy data").unwrap();
        }
        // Exceeds the old count bound alone.
        assert_eq!(store.list(&api, &session).unwrap().len(), SAVED_COUNT);
        assert_eq!(store.read(&api, &session, "track-1088").unwrap().bytes, [0]);
        assert!(matches!(
            install(&store, &scope, "track-1088", b"never overwrite"),
            Err(Error::AlreadyDownloaded)
        ));
        install(&store, &scope, "new", b"new audio").unwrap();
        assert!(store.remove(&api, &session, "track-1088").unwrap());
        assert_eq!(store.list(&api, &session).unwrap().len(), SAVED_COUNT);
        for name in [LEGACY_CACHE_INDEX, LEGACY_CACHE_STAGE] {
            assert_eq!(
                stdfs::read(fixture.root().join(name)).unwrap(),
                b"opaque legacy data"
            );
        }

        // A large store still checks the candidate and validates layout on list.
        symlink("new", fixture.root().join(scope.name("unsafe").unwrap())).unwrap();
        assert_eq!(
            store.admit(&scope.name("unsafe").unwrap()).unwrap_err(),
            Error::Damaged
        );
        assert_eq!(store.list(&api, &session), Err(Error::Damaged));
    }

    #[test]
    fn v1_store_over_old_byte_quota_lists_by_header_without_loading_audio() {
        let fixture = Fixture::new();
        let api = api("https://one.invalid");
        let session = api.test_session();
        let scope = Scope::new(&api, &session).unwrap();
        let foreign = Scope::from_parts("https://one.invalid", "other-account");
        let store = fixture.store();
        let mut hash = Context::new(&SHA256);
        for _ in 0..(MAX_TRACK_BYTES / (64 * 1024)) {
            hash.update(&[0; 64 * 1024]);
        }
        let hash = hex(hash.finish().as_ref());
        // Exceeds the old byte quota alone; the audio payloads are sparse holes.
        for index in 0..33 {
            write_sparse_entry(
                &fixture.root(),
                if index == 0 { &foreign } else { &scope },
                &format!("track-{index}"),
                MAX_TRACK_BYTES as u64,
                &hash,
            );
        }
        let listed = store.list(&api, &session).unwrap();
        assert_eq!(listed.len(), 32);
        assert_eq!(
            listed.iter().map(|d| d.bytes).sum::<u64>(),
            1024 * 1024 * 1024
        );
        assert!(matches!(
            store.read(&api, &session, "track-0"),
            Err(Error::NotDownloaded)
        ));
        install(&store, &scope, "new", b"new audio").unwrap();
    }

    #[test]
    fn full_is_reported_only_for_filesystem_space_or_quota_errors() {
        for error in [Errno::NOSPC, Errno::DQUOT] {
            assert_eq!(storage_error(error), Error::Full);
            assert_eq!(
                write_error(std::io::Error::from_raw_os_error(error.raw_os_error())),
                Error::Full
            );
        }
        assert_eq!(storage_error(Errno::ACCESS), Error::Storage);
    }

    #[test]
    fn probes_real_wav_without_copying_or_changing_the_bytes() {
        // Mono 16-bit PCM at 8 kHz, two samples.
        let wav = b"RIFF\x28\0\0\0WAVEfmt \x10\0\0\0\x01\0\x01\0\x40\x1f\0\0\x80\x3e\0\0\x02\0\x10\0data\x04\0\0\0\x01\0\xff\xff";
        let owned = wav.to_vec();
        let ptr = owned.as_ptr();
        let probed = probe_audio(owned).unwrap();
        assert_eq!(probed.as_ref(), wav);
        assert_eq!(probed.as_ref().as_ptr(), ptr);
        assert!(matches!(
            probe_audio(b"not audio".to_vec()),
            Err(Error::UnsupportedAudio)
        ));
    }

    #[test]
    fn validates_go_manifest_digest_mime_track_and_size() {
        let audio = b"synthetic payload";
        let hash = hex(digest(&SHA256, audio).as_ref());
        let mut body = serde_json::json!({"version": 1, "trackId":"track", "mediaVersion":format!("sha256:{hash}"),
            "sha256":hash, "format":"wav", "byteLength":audio.len(), "contentType":"audio/wav", "downloadUrl":"https://ignored.invalid/"});
        let parse =
            |body: &serde_json::Value| Manifest::parse(&serde_json::to_vec(body).unwrap(), "track");
        let manifest = parse(&body).unwrap();
        assert!(manifest.verify(audio).is_ok());
        assert_eq!(
            manifest.verify(&audio[..audio.len() - 1]),
            Err(Error::InvalidMedia)
        );
        assert_eq!(
            manifest.verify(b"modified payload!"),
            Err(Error::InvalidMedia)
        );
        body["trackId"] = "another".into();
        assert!(matches!(parse(&body), Err(Error::InvalidMedia)));
        body["trackId"] = "track".into();
        body["contentType"] = "image/png".into();
        assert!(matches!(parse(&body), Err(Error::InvalidMedia)));
        body["contentType"] = "audio/wav".into();
        body["byteLength"] = (MAX_TRACK_BYTES + 1).into();
        assert!(matches!(parse(&body), Err(Error::TrackTooLarge)));
    }
}
