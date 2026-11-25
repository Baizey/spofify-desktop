//! Explicitly downloaded audio. Linux, local filesystem, private to the OS user.
//! Access requires the account's non-revoked local grant, independent of the
//! network cookie deadline while offline. A record is never sign-in authority.
//! Downloaded bytes survive logout but become inaccessible until the same account
//! at the same canonical origin signs in again. No credentials or server paths
//! are persisted. A download grants no player/coordinator authority.

use std::{
    collections::{HashMap, HashSet},
    fmt,
    fs::File,
    future::{Future, pending},
    io::{Read, Write},
    mem::MaybeUninit,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use futures_util::{Stream, StreamExt, stream};
use ring::digest::{Context, SHA256, digest};
use rustix::{
    fd::OwnedFd,
    fs::{self, AtFlags, FileType, FlockOperation, Mode, OFlags, RawDir, RenameFlags, Stat},
    io::Errno,
    process::geteuid,
};
use serde::{Deserialize, Serialize};
use tokio::sync::{Semaphore, mpsc, oneshot, watch};

use crate::{
    api::{Api, ApiError, Item, Session},
    audio_input::AudioInput,
    storage::{self as local_storage, DIR_FLAGS},
};

const IO_CHUNK: usize = 64 * 1024;
const DOWNLOAD_DISK_STALL: Duration = Duration::from_secs(10);
// The default download store is shared by all callers in this process. Even
// an admission worker stuck before it acquires the file lock keeps this slot.
static DOWNLOAD_SLOT: Semaphore = Semaphore::const_new(1);
static DOWNLOAD_OPEN_SLOTS: Semaphore = Semaphore::const_new(2);
const PROJECTION_DISK_STALL: Duration = Duration::from_secs(10);
static PROJECTION_SLOTS: Semaphore = Semaphore::const_new(2);
// A failed payload never modifies downloads. Suppress only the exact scoped
// file identity, until the recording is replaced or this process ends.
/// Validates a *new* use of an already hashed recording. A currently audible
/// run may finish on its existing descriptor after removal.
#[derive(Debug)]
pub(crate) struct SavedProof {
    root: Arc<OwnedFd>,
    name: String,
    stat: Stat,
}

impl SavedProof {
    pub(crate) fn current(&self) -> bool {
        fs::statat(&self.root, self.name.as_str(), AtFlags::SYMLINK_NOFOLLOW)
            .is_ok_and(|after| unchanged_entry(&self.stat, &after))
    }
}
const MAX_HEADER: usize = 16 * 1024;
const MAGIC: &[u8; 8] = b"SPOFDL01";
const PREFIX_BYTES: usize = 12;
const LOCK: &str = ".download-lock";
const STAGE: &str = ".partial";
// Earlier development builds also used the v1 directory for their cache metadata.
// These are not audio records. This app never opens or deletes them.
const LEGACY_CACHE_INDEX: &str = ".playback-cache-v1";
const LEGACY_CACHE_STAGE: &str = ".playback-cache-partial";
const ROOT_DIR: &str = "spofify-downloads-v2";
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
    RecordingChanged,
    UnsupportedAudio,
    InvalidMedia,
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
            Self::RecordingChanged => {
                "This download changed. Select it again to play the current version."
            }
            Self::UnsupportedAudio => "This audio cannot be played on this device.",
            Self::InvalidMedia => "Unable to verify this track’s download.",
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

/// The v1 payload's expected digest, not a local grant or proof of playback.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Recording([u8; 32]);

impl Recording {
    #[cfg(test)]
    pub(crate) fn fixture() -> Self {
        Self([0; 32])
    }

    // Both v1 record opening and manifest parsing validate the digest syntax.
    fn from_validated_hex(value: &str) -> Self {
        debug_assert!(hex_digest(value));
        fn nibble(b: u8) -> u8 {
            match b {
                b'0'..=b'9' => b - b'0',
                b'a'..=b'f' => b - b'a' + 10,
                _ => unreachable!("validated lowercase hexadecimal"),
            }
        }
        Self(std::array::from_fn(|i| {
            let bytes = value.as_bytes();
            nibble(bytes[2 * i]) << 4 | nibble(bytes[2 * i + 1])
        }))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Download {
    pub item: Item,
    pub bytes: u64,
    pub recording: Recording,
}

/// A verified, original-format payload from the same descriptor that was hashed.
/// The caller must still fence late results and playback on session/occurrence
/// changes, and use the appropriate player admission path.
pub struct PlaybackInput {
    pub item: Item,
    pub source: AudioInput,
    pub(crate) recording: Recording,
}

pub(crate) struct OnlineMedia {
    pub source: AudioInput,
}

/// Choose authenticated account media; this does not interpret queue, output,
/// or grant state. The caller must fence the result by account epoch and the
/// exact native operation, whether awaiting Go confirmation or committed.
pub(crate) async fn online_media(
    api: &Arc<Api>,
    session: &Session,
    slot: Arc<Semaphore>,
    id: &str,
) -> Result<OnlineMedia, ApiError> {
    online_media_with_store(api, session, slot, id, OfflineStore::open_existing).await
}

async fn online_media_with_store(
    api: &Arc<Api>,
    session: &Session,
    slot: Arc<Semaphore>,
    id: &str,
    open: impl FnOnce() -> Result<Option<OfflineStore>, Error> + Send + 'static,
) -> Result<OnlineMedia, ApiError> {
    api.with_authenticated_network(session, async {
        let input = read_verified(
            api.clone(),
            session.clone(),
            slot,
            id.to_owned(),
            move || open()?.ok_or(Error::NotDownloaded),
        )
        .await;
        let source = match input {
            Ok((_, input)) => input.source,
            Err(_) => AudioInput::online(api.media(session, id).await?).await?,
        };
        Ok(OnlineMedia { source })
    })
    .await
}

pub(crate) async fn read_verified(
    api: Arc<Api>,
    session: Session,
    slot: Arc<Semaphore>,
    id: String,
    open: impl FnOnce() -> Result<OfflineStore, Error> + Send + 'static,
) -> Result<(OfflineStore, PlaybackInput), Error> {
    read_verified_inner(api, session, slot, id, None, open).await
}

/// Verify that the selected recording is on the descriptor used for playback.
pub(crate) async fn read_verified_recording(
    api: Arc<Api>,
    session: Session,
    slot: Arc<Semaphore>,
    id: String,
    recording: Recording,
    open: impl FnOnce() -> Result<OfflineStore, Error> + Send + 'static,
) -> Result<(OfflineStore, PlaybackInput), Error> {
    read_verified_inner(api, session, slot, id, Some(recording), open).await
}

/// Incremental verification renews a disk stall timer, not a track deadline.
/// A retired kernel read still owns its slot until it actually returns.
async fn read_verified_inner(
    api: Arc<Api>,
    session: Session,
    slot: Arc<Semaphore>,
    id: String,
    recording: Option<Recording>,
    open: impl FnOnce() -> Result<OfflineStore, Error> + Send + 'static,
) -> Result<(OfflineStore, PlaybackInput), Error> {
    use std::time::Duration;
    use tokio::{sync::watch, time::timeout};
    const STALL: Duration = Duration::from_secs(10);
    let mut mode = session.network_changes();
    let stamp = *mode.borrow_and_update();
    let permit = tokio::select! {
        biased;
        _ = mode.changed() => return Err(Error::Cancelled),
        permit = timeout(STALL, slot.acquire_owned()) => permit.map_err(|_| Error::Busy)?.map_err(|_| Error::Cancelled)?,
    };
    let cancellation = CancelOnDrop(Arc::new(AtomicBool::new(false)));
    let cancelled = cancellation.0.clone();
    let (progress, mut advances) = watch::channel(());
    let worker_mode = mode.clone();
    let mut worker = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let live = || {
            if cancelled.load(Ordering::Acquire) || *worker_mode.borrow() != stamp {
                return Err(Error::Cancelled);
            }
            let _ = progress.send(());
            Ok(())
        };
        live()?;
        let store = open()?;
        let input = store.read_live_recording(&api, &session, &id, recording, live)?;
        Ok((store, input))
    });
    loop {
        tokio::select! {
            biased;
            _ = mode.changed() => return Err(Error::Cancelled),
            result = &mut worker => return result.map_err(|_| Error::Storage)?,
            progress = timeout(STALL, advances.changed()) => match progress {
                Err(_) => return Err(Error::Storage),
                Ok(Err(_)) => return worker.await.map_err(|_| Error::Storage)?,
                Ok(Ok(())) => (),
            }
        }
    }
}

/// Structurally verified download metadata in a single network-mode interval.
/// Keep one handle per interval and fence late results in the caller.
/// Browsing does not read audio bytes; playback verifies the digest separately.
#[derive(Clone)]
pub(crate) struct VerifiedDownloads {
    api: Arc<Api>,
    session: Session,
    mode: watch::Receiver<u64>,
    generation: u64,
}

impl VerifiedDownloads {
    #[cfg(test)]
    pub(crate) fn new(api: Arc<Api>, session: Session) -> Result<Self, Error> {
        let mut mode = session.network_changes();
        let generation = *mode.borrow_and_update();
        if generation & 1 == 0 {
            return Err(Error::Offline);
        }
        Self::with_mode(api, session, mode, generation)
    }

    pub(crate) fn for_browsing(api: Arc<Api>, session: Session) -> Result<Self, Error> {
        let mut mode = session.network_changes();
        let generation = *mode.borrow_and_update();
        Self::with_mode(api, session, mode, generation)
    }

    fn with_mode(api: Arc<Api>, session: Session, mode: watch::Receiver<u64>, generation: u64) -> Result<Self, Error> {
        api.authenticated_account(&session)?;
        Ok(Self { api, session, mode, generation })
    }

    fn check_live(&self) -> Result<(), Error> {
        if *self.mode.borrow() != self.generation {
            return Err(Error::Cancelled);
        }
        self.api.authenticated_account(&self.session)?;
        Ok(())
    }

    /// `None` scans the active scope (Search); `Some` opens only the requested
    /// track keys. Missing, unreadable and structurally invalid entries are
    /// omitted independently. This is a snapshot, not a playback guarantee.
    pub(crate) async fn verified_records(
        &self,
        ids: Option<Vec<String>>,
    ) -> Result<HashMap<String, Download>, Error> {
        self.verified_records_with(ids, OfflineStore::open_existing)
            .await
    }

    async fn verified_records_with(
        &self,
        ids: Option<Vec<String>>,
        open: impl FnOnce() -> Result<Option<OfflineStore>, Error> + Send + 'static,
    ) -> Result<HashMap<String, Download>, Error> {
        self.check_live()?;
        let mut mode = self.mode.clone();
        let work = async {
            if ids.as_ref().is_some_and(Vec::is_empty) {
                return Ok(HashMap::new());
            }
            // A retired blocking worker still holds its slot until disk I/O
            // returns and the worker observes cancellation.
            let permit = tokio::time::timeout(PROJECTION_DISK_STALL, PROJECTION_SLOTS.acquire())
                .await
                .map_err(|_| Error::Busy)?
                .map_err(|_| Error::Cancelled)?;
            let cancellation = CancelOnDrop(Arc::new(AtomicBool::new(false)));
            let cancelled = cancellation.0.clone();
            let (progress, mut advances) = watch::channel(());
            let grant = self.clone();
            let worker = tokio::task::spawn_blocking(move || {
                let _permit = permit;
                let live = || download_checkpoint(&cancelled, &progress, || grant.check_live());
                live()?;
                let scope = Scope::new(&grant.api, &grant.session)?;
                let store = open()?;
                live()?;
                match store {
                    Some(store) => store.project_records(&scope, ids, live),
                    None => Ok(HashMap::new()),
                }
            });
            await_download_disk(&mut advances, PROJECTION_DISK_STALL, worker)
                .await?
                .map_err(|_| Error::Storage)?
        };
        let result = tokio::select! {
            biased;
            _ = mode.changed() => return Err(Error::Cancelled),
            result = work => result,
        };
        self.check_live()?;
        result
    }
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

    /// Bound even the initial path walk for download and pin operations.
    pub(crate) async fn open_for_download(
        open: impl FnOnce() -> Result<Self, Error> + Send + 'static,
    ) -> Result<Self, Error> {
        let slot = DOWNLOAD_OPEN_SLOTS.try_acquire().map_err(|_| Error::Busy)?;
        tokio::time::timeout(
            DOWNLOAD_DISK_STALL,
            tokio::task::spawn_blocking(move || {
                let _slot = slot;
                open()
            }),
        )
        .await
        .map_err(|_| Error::Storage)?
        .map_err(|_| Error::Storage)?
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
    #[cfg(test)]
    pub async fn download(&self, api: &Api, session: &Session, item: &Item) -> Result<Download, Error> {
        self.download_scoped(api, session, item, None).await
    }

    pub async fn download_for_access(&self, access: &crate::desk::Access, item: &Item) -> Result<Download, Error> {
        let epoch = access.personal.account_fence(access.epoch);
        #[cfg(not(test))]
        if epoch.is_none() { return Err(Error::Cancelled); }
        self.download_scoped(&access.api, &access.session, item, epoch).await
    }

    async fn download_scoped(
        &self,
        api: &Api,
        session: &Session,
        item: &Item,
        epoch: Option<(Arc<std::sync::atomic::AtomicU64>, u64)>,
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
        let mode_live = download_liveness(session.clone(), network.clone(), generation);
        let live = move || {
            mode_live()?;
            if epoch.as_ref().is_some_and(|(current, admitted)| current.load(Ordering::SeqCst) != *admitted) {
                return Err(Error::Cancelled);
            }
            Ok(())
        };
        in_download_mode(&mut network, async {
            let admission = self
                .admit_download(name, live.clone(), DOWNLOAD_DISK_STALL)
                .await?;
            api.authenticated_account(session)?;
            let body = api.offline_manifest(session, &track.id).await?;
            let manifest = Manifest::parse(&body, &track.id)?;
            live()?;
            let audio = api
                .offline_audio(
                    session,
                    &track.id,
                    &manifest.media_version,
                    &manifest.content_type,
                    manifest.byte_length,
                )
                .await?;
            live()?;
            let header = Header {
                scope: scope.0,
                track,
                bytes: manifest.byte_length,
                audio_hash: manifest.sha256,
            };
            self.transfer(
                admission,
                header,
                stream::try_unfold(audio, |mut audio| async {
                    Ok(audio.chunk().await?.map(|bytes| (bytes, audio)))
                }),
                live,
                DOWNLOAD_DISK_STALL,
            )
            .await
        })
        .await
    }

    async fn admit_download(
        &self,
        name: String,
        live: impl Fn() -> Result<(), Error> + Send + 'static,
        stall: Duration,
    ) -> Result<DownloadAdmission, Error> {
        let slot = DOWNLOAD_SLOT.try_acquire().map_err(|_| Error::Busy)?;
        let cancellation = CancelOnDrop(Arc::new(AtomicBool::new(false)));
        let cancelled = cancellation.0.clone();
        let (progress, mut advances) = watch::channel(());
        let store = self.clone();
        let worker = tokio::task::spawn_blocking(move || {
            let fd =
                store.admit_live(&name, || download_checkpoint(&cancelled, &progress, &live))?;
            Ok(DownloadAdmission {
                _fd: fd,
                _slot: slot,
            })
        });
        await_download_disk(&mut advances, stall, worker)
            .await?
            .map_err(|_| Error::Storage)?
    }

    async fn transfer<B: AsRef<[u8]> + Send + 'static>(
        &self,
        admission: impl Send + 'static,
        header: Header,
        chunks: impl Stream<Item = Result<B, Error>>,
        live: impl Fn() -> Result<(), Error> + Send + Sync + 'static,
        stall: Duration,
    ) -> Result<Download, Error> {
        // Acknowledge staging and each chunk before polling the network again.
        // This bounds the handoff to one chunk and lets the disk watchdog run
        // only when waiting for disk, not when the worker is waiting for HTTP.
        let (send, mut receive) = mpsc::channel::<Packet<B>>(1);
        let (ready, staged) = oneshot::channel();
        let (progress, mut advances) = watch::channel(());
        let cancellation = CancelOnDrop(Arc::new(AtomicBool::new(false)));
        let cancelled = cancellation.0.clone();
        let store = self.clone();
        let mut worker = tokio::task::spawn_blocking(move || {
            let _admission = admission;
            let live = Arc::new(live);
            let publish_progress = progress.clone();
            let live = move || download_checkpoint(&cancelled, &progress, &*live);
            live()?;
            let mut stage = store.stage_live(header, &live)?;
            if ready.send(()).is_err() {
                return Err(Error::Cancelled);
            }
            while let Some(packet) = receive.blocking_recv() {
                live()?;
                match packet {
                    Packet::Chunk(chunk, written) => {
                        stage.write(chunk.as_ref(), &live)?;
                        if written.send(()).is_err() {
                            return Err(Error::Cancelled);
                        }
                    }
                    Packet::Complete => {
                        let (partial, header) = stage.verify_and_sync(live.clone())?;
                        return store.publish(partial, header, live, || {
                            let _ = publish_progress.send(());
                        });
                    }
                }
            }
            Err(Error::Cancelled)
        });
        let transfer = async {
            if await_download_disk(&mut advances, stall, staged)
                .await?
                .is_err()
            {
                return Ok(()); // The worker has the actual storage error.
            }
            tokio::pin!(chunks);
            while let Some(chunk) = chunks.next().await {
                let chunk = chunk?;
                let (written, ack) = oneshot::channel();
                let received = await_download_disk(&mut advances, stall, async {
                    send.send(Packet::Chunk(chunk, written)).await.is_ok() && ack.await.is_ok()
                })
                .await?;
                if !received {
                    return Ok(());
                }
            }
            // A closed channel alone is never permission to publish.
            let _ = await_download_disk(&mut advances, stall, send.send(Packet::Complete)).await?;
            Ok::<_, Error>(())
        };
        tokio::select! {
            biased;
            result = &mut worker => return result.map_err(|_| Error::Storage)?,
            result = transfer => result?,
        }
        // On a transfer/network error or mode change the future drops its
        // guard and channel immediately. The worker retires with its lease.
        drop(send);
        await_download_disk(&mut advances, stall, worker)
            .await?
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
                let (_, header, _) = open_entry(&dir, name, &scope)?;
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

    #[cfg(test)]
    pub(crate) fn test_install(&self, api: &Api, session: &Session, id: &str, bytes: &[u8]) {
        tests::install(self, &Scope::new(api, session).unwrap(), id, bytes).unwrap();
    }

    /// Read and verify one test record without a caller-owned cancellation fence.
    #[cfg(test)]
    pub fn read(
        &self,
        api: &Api,
        session: &Session,
        track_id: &str,
    ) -> Result<PlaybackInput, Error> {
        self.read_live(api, session, track_id, || Ok(()))
    }

    #[cfg(test)]
    fn read_live(
        &self,
        api: &Api,
        session: &Session,
        track_id: &str,
        live: impl Fn() -> Result<(), Error>,
    ) -> Result<PlaybackInput, Error> {
        self.read_live_recording(api, session, track_id, None, live)
    }

    /// The caller's liveness fence is checked between bounded reads, including
    /// before returning the same descriptor that passed digest verification.
    fn read_live_recording(
        &self,
        api: &Api,
        session: &Session,
        track_id: &str,
        recording: Option<Recording>,
        live: impl Fn() -> Result<(), Error>,
    ) -> Result<PlaybackInput, Error> {
        let check = || {
            live()?;
            api.authenticated_account(session)?;
            Ok::<_, Error>(())
        };
        check()?;
        let scope = Scope::new(api, session)?;
        let name = scope.name(track_id)?;
        let (mut file, header, offset, stat) = {
            let dir = self.lock_dir(false)?;
            open_entry_stamped(&dir, &name, &scope)?
        }; // Only pin the name during open; the descriptor survives unlink.
        if header.track.id != track_id {
            return Err(Error::Damaged);
        }
        let actual_recording = header.recording();
        if recording.is_some_and(|expected| expected != actual_recording) {
            return Err(Error::RecordingChanged);
        }
        let mut hash = Context::new(&SHA256);
        let mut remaining = header.bytes;
        let mut buffer = [0; IO_CHUNK];
        while remaining != 0 {
            check()?;
            let chunk = &mut buffer[..remaining.min(IO_CHUNK as u64) as usize];
            if file.read_exact(chunk).is_err() {
                self.retire_damaged(&name, &stat, check);
                return Err(Error::Damaged);
            }
            hash.update(chunk);
            remaining -= chunk.len() as u64;
        }
        check()?;
        if !matches!(file.read(&mut [0]), Ok(0))
            || header.audio_hash != hex(hash.finish().as_ref())
        {
            self.retire_damaged(&name, &stat, check);
            return Err(Error::Damaged);
        }
        check()?;
        let proof = SavedProof { root: self.root.clone(), name, stat };
        if !proof.current() { return Err(Error::RecordingChanged); }
        Ok(PlaybackInput {
            item: header.track.item(),
            source: AudioInput::verified_saved(file, offset, header.bytes, proof),
            recording: actual_recording,
        })
    }

    fn retire_damaged(&self, name: &str, checked: &Stat, live: impl Fn() -> Result<(), Error>) {
        // Remove only the exact corrupt publication, never a concurrent
        // replacement. Its absence is durable and permits a clean redownload.
        let result = (|| {
            let dir = self.lock_dir(true)?;
            let removed = {
                let _publication = crate::playlist_pin_store::PUBLICATION_GATE.lock().unwrap();
                live()?;
                let same = fs::statat(&dir, name, AtFlags::SYMLINK_NOFOLLOW)
                    .is_ok_and(|current| unchanged_entry(checked, &current));
                if same { storage(fs::unlinkat(&dir, name, AtFlags::empty()))?; }
                same
            };
            if removed { storage(fs::fsync(&dir))?; }
            Ok::<_, Error>(())
        })();
        if let Err(error) = result {
            eprintln!("Unable to retire damaged download: {error}");
        }
    }

    fn project_records(
        &self,
        scope: &Scope,
        ids: Option<Vec<String>>,
        live: impl Fn() -> Result<(), Error>,
    ) -> Result<HashMap<String, Download>, Error> {
        // Do not hold the publication lock during a scan. Published records
        // are opened by descriptor and rechecked by name after the scan.
        // Use a fresh directory description so RawDir starts at offset zero.
        let dir = storage(fs::openat(
            self.root.as_ref(),
            ".",
            DIR_FLAGS,
            Mode::empty(),
        ))?;
        live()?;
        storage(local_storage::private_dir(&dir))?;
        let mut opened = Vec::new();
        let mut visit = |name: &str| -> Result<(), Error> {
            live()?;
            // A per-file error is not a failure of the scope or directory.
            if let Ok((_, header, _, stat)) = open_entry_stamped(&dir, name, scope) {
                opened.push((name.to_owned(), header.download(), stat));
            }
            Ok(())
        };
        if let Some(ids) = ids {
            let mut seen = HashSet::new();
            for id in ids {
                live()?;
                if seen.contains(&id) {
                    continue;
                }
                if let Ok(name) = scope.name(&id) {
                    visit(&name)?;
                }
                seen.insert(id);
            }
        } else {
            let mut buffer = [MaybeUninit::uninit(); 4096];
            let mut entries = RawDir::new(&dir, &mut buffer);
            loop {
                live()?;
                let Some(entry) = entries.next() else { break };
                let entry = storage(entry)?;
                if let Ok(name) = entry.file_name().to_str()
                    && scope.contains(name)
                    && valid_key(name)
                {
                    visit(name)?;
                }
            }
        }
        let mut records = HashMap::new();
        for (name, download, before) in opened {
            live()?;
            if fs::statat(&dir, name.as_str(), AtFlags::SYMLINK_NOFOLLOW)
                .is_ok_and(|after| unchanged_entry(&before, &after))
            {
                records.insert(download.item.id.clone(), download);
            }
        }
        live()?;
        Ok(records)
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

    #[cfg(test)]
    fn admit(&self, name: &str) -> Result<OwnedFd, Error> {
        self.admit_live(name, || Ok(()))
    }

    fn admit_live(
        &self,
        name: &str,
        live: impl Fn() -> Result<(), Error>,
    ) -> Result<OwnedFd, Error> {
        live()?;
        let dir = self.lock_dir(true)?;
        live()?;
        let admission = storage(fs::openat(
            &dir,
            LOCK,
            FILE_FLAGS | OFlags::RDWR | OFlags::CREATE,
            PRIVATE_FILE,
        ))?;
        live()?;
        if file_size(&storage(fs::fstat(&admission))?)? != 0 {
            return Err(Error::Damaged);
        }
        lock(&admission, FlockOperation::NonBlockingLockExclusive)?;
        live()?;
        // Recover an abandoned private staging file after a process crash.
        match fs::unlinkat(&dir, STAGE, AtFlags::empty()) {
            Ok(()) | Err(Errno::NOENT) => (),
            Err(_) => return Err(Error::Storage),
        }
        live()?;
        reject_existing(&dir, name)?;
        live()?;
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

    #[cfg(test)]
    fn stage(&self, header: Header) -> Result<StagedDownload<'_>, Error> {
        self.stage_live(header, || Ok(()))
    }

    /// Must hold download admission, but not the directory publication lock.
    fn stage_live(
        &self,
        header: Header,
        live: impl Fn() -> Result<(), Error>,
    ) -> Result<StagedDownload<'_>, Error> {
        let json = serde_json::to_vec(&header).map_err(|_| Error::Storage)?;
        if json.is_empty() || json.len() > MAX_HEADER {
            return Err(Error::InvalidTrack);
        }
        let offset = (PREFIX_BYTES + json.len()) as u64;
        if header.bytes == 0 || header.bytes > i64::MAX as u64 - offset {
            return Err(Error::InvalidMedia);
        }
        live()?;
        let fd = storage(fs::openat(
            self.root.as_ref(),
            STAGE,
            FILE_FLAGS | OFlags::RDWR | OFlags::CREATE | OFlags::EXCL,
            PRIVATE_FILE,
        ))?;
        let partial = Stage {
            dir: self.root.as_ref(),
            published: false,
        };
        live()?;
        if file_size(&storage(fs::fstat(&fd))?)? != 0 {
            return Err(Error::Damaged);
        }
        live()?;
        let mut file = File::from(fd);
        file.write_all(MAGIC)
            .and_then(|_| file.write_all(&(json.len() as u32).to_le_bytes()))
            .and_then(|_| file.write_all(&json))
            .map_err(write_error)?;
        Ok(StagedDownload {
            file,
            partial,
            offset,
            remaining: header.bytes,
            header,
            hash: Context::new(&SHA256),
        })
    }

    fn publish(
        &self,
        mut partial: Stage<'_>,
        header: Header,
        live: impl Fn() -> Result<(), Error>,
        progress: impl Fn(),
    ) -> Result<Download, Error> {
        let name = Scope(header.scope.clone()).name(&header.track.id)?;
        live()?;
        let dir = self.lock_dir(true)?;
        live()?;
        reject_existing(&dir, &name)?;
        live()?;
        {
            // Serialize only the commit point with account/mode retirement.
            // Never hold this gate while transferring, hashing, or syncing.
            let _publication = crate::playlist_pin_store::PUBLICATION_GATE.lock().unwrap();
            live()?;
            match fs::renameat_with(&dir, STAGE, &dir, name.as_str(), RenameFlags::NOREPLACE) {
                Ok(()) => partial.published = true,
                Err(Errno::EXIST) => return Err(Error::AlreadyDownloaded),
                Err(err) => return Err(storage_error(err)),
            }
        }
        progress();
        if let Err(err) = fs::fsync(&dir) {
            progress();
            let _ = fs::unlinkat(&dir, name.as_str(), AtFlags::empty());
            progress();
            let _ = fs::fsync(&dir);
            return Err(storage_error(err));
        }
        Ok(header.download())
    }
}

struct DownloadAdmission {
    _fd: OwnedFd,
    _slot: tokio::sync::SemaphorePermit<'static>,
}

async fn in_download_mode<T>(
    mode: &mut watch::Receiver<u64>,
    operation: impl Future<Output = Result<T, Error>>,
) -> Result<T, Error> {
    tokio::select! {
        biased;
        _ = mode.changed() => Err(Error::Offline),
        result = operation => result,
    }
}

fn download_liveness(
    session: Session,
    mode: watch::Receiver<u64>,
    generation: u64,
) -> impl Fn() -> Result<(), Error> + Clone + use<> {
    move || {
        check_online_liveness(&session)?;
        if *mode.borrow() != generation {
            return Err(Error::Offline);
        }
        Ok(())
    }
}

fn download_checkpoint(
    cancelled: &AtomicBool,
    progress: &watch::Sender<()>,
    live: impl Fn() -> Result<(), Error>,
) -> Result<(), Error> {
    if cancelled.load(Ordering::Acquire) {
        return Err(Error::Cancelled);
    }
    live()?;
    if cancelled.load(Ordering::Acquire) {
        return Err(Error::Cancelled);
    }
    let _ = progress.send(());
    Ok(())
}

async fn await_download_disk<T>(
    advances: &mut watch::Receiver<()>,
    stall: Duration,
    work: impl Future<Output = T>,
) -> Result<T, Error> {
    advances.borrow_and_update(); // Ignore progress from the previous phase.
    tokio::pin!(work);
    loop {
        let progress = async {
            // A closed sender does not prove that the worker has joined;
            // it may still be dropping disk resources after an error.
            if advances.changed().await.is_err() {
                pending::<()>().await;
            }
        };
        tokio::select! {
            biased;
            result = &mut work => return Ok(result),
            progress = tokio::time::timeout(stall, progress) => {
                progress.map_err(|_| Error::Storage)?;
            }
        }
    }
}

enum Packet<B> {
    Chunk(B, oneshot::Sender<()>),
    Complete,
}

struct StagedDownload<'a> {
    file: File,
    partial: Stage<'a>,
    offset: u64,
    remaining: u64,
    header: Header,
    hash: Context,
}

impl<'a> StagedDownload<'a> {
    fn write(&mut self, bytes: &[u8], live: impl Fn() -> Result<(), Error>) -> Result<(), Error> {
        if bytes.len() as u64 > self.remaining {
            return Err(Error::InvalidMedia);
        }
        for chunk in bytes.chunks(IO_CHUNK) {
            live()?;
            self.file.write_all(chunk).map_err(write_error)?;
            self.hash.update(chunk);
            self.remaining -= chunk.len() as u64;
        }
        Ok(())
    }

    fn verify_and_sync(
        self,
        live: impl Fn() -> Result<(), Error> + Send + Sync + Clone + 'static,
    ) -> Result<(Stage<'a>, Header), Error> {
        if self.remaining != 0 || hex(self.hash.finish().as_ref()) != self.header.audio_hash {
            return Err(Error::InvalidMedia);
        }
        live()?;
        let source = AudioInput::file(
            self.file.try_clone().map_err(write_error)?,
            self.offset,
            self.header.bytes,
        );
        probe_audio(&source, live)?;
        self.file.sync_all().map_err(write_error)?;
        Ok((self.partial, self.header))
    }
}

fn probe_audio(
    source: &AudioInput,
    live: impl Fn() -> Result<(), Error> + Send + Sync + Clone + 'static,
) -> Result<(), Error> {
    // Admission check only; one decoded sample does not validate every frame.
    let probe_live = live.clone();
    let supported = rodio::Decoder::builder()
        .with_data(source.reader_with_cancel(move || probe_live().is_ok()))
        .with_byte_len(source.len())
        .with_seekable(true)
        .build()
        .is_ok_and(|mut decoder| decoder.next().is_some());
    live()?;
    supported.then_some(()).ok_or(Error::UnsupportedAudio)
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
    byte_length: u64,
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
        Ok(value)
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
    fn recording(&self) -> Recording {
        Recording::from_validated_hex(&self.audio_hash)
    }

    fn download(self) -> Download {
        let recording = self.recording();
        Download {
            item: self.track.item(),
            bytes: self.bytes,
            recording,
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

fn open_entry(dir: &OwnedFd, name: &str, scope: &Scope) -> Result<(File, Header, u64), Error> {
    let (file, header, offset, _) = open_entry_stamped(dir, name, scope)?;
    Ok((file, header, offset))
}

fn open_entry_stamped(
    dir: &OwnedFd,
    name: &str,
    scope: &Scope,
) -> Result<(File, Header, u64, Stat), Error> {
    let fd = fs::openat(dir, name, FILE_FLAGS | OFlags::RDONLY, Mode::empty()).map_err(|e| {
        if e == Errno::NOENT {
            Error::NotDownloaded
        } else {
            Error::Damaged
        }
    })?;
    let stat = storage(fs::fstat(&fd))?;
    let actual = file_size(&stat)?;
    let mut file = File::from(fd);
    let mut prefix = [0; PREFIX_BYTES];
    file.read_exact(&mut prefix).map_err(|_| Error::Damaged)?;
    let len = u32::from_le_bytes([prefix[8], prefix[9], prefix[10], prefix[11]]) as usize;
    let offset = PREFIX_BYTES as u64 + len as u64;
    if &prefix[..8] != MAGIC || !(1..=MAX_HEADER).contains(&len) || actual <= offset {
        return Err(Error::Damaged);
    }
    let mut json = vec![0; len];
    file.read_exact(&mut json).map_err(|_| Error::Damaged)?;
    let header: Header = serde_json::from_slice(&json).map_err(|_| Error::Damaged)?;
    if header.scope != scope.0
        || !header.track.valid()
        || !hex_digest(&header.audio_hash)
        || actual.checked_sub(offset) != Some(header.bytes)
        || scope.name(&header.track.id).map_err(|_| Error::Damaged)? != name
    {
        return Err(Error::Damaged);
    }
    Ok((file, header, offset, stat))
}

// Verify the name still refers to the same unchanged file, even if it was
// opened early in the scan. Do not compare access times (header reads).
fn unchanged_entry(before: &Stat, after: &Stat) -> bool {
    file_size(after).is_ok()
        && before.st_dev == after.st_dev
        && before.st_ino == after.st_ino
        && before.st_size == after.st_size
        && before.st_mtime == after.st_mtime
        && before.st_mtime_nsec == after.st_mtime_nsec
        && before.st_ctime == after.st_ctime
        && before.st_ctime_nsec == after.st_ctime_nsec
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
    if file_size(s)? < PREFIX_BYTES as u64 + 2 {
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
    pub(super) fn install(
        store: &OfflineStore,
        scope: &Scope,
        id: &str,
        data: &[u8],
    ) -> Result<Download, Error> {
        let _guard = store.admit(&scope.name(id)?)?;
        // Some legacy fixtures deliberately contain non-audio payloads.
        let mut stage = store.stage(fixture_header(scope, id, data))?;
        stage.write(data, || Ok(()))?;
        stage.file.sync_all().map_err(write_error)?;
        store.publish(stage.partial, stage.header, || Ok(()), || ())
    }

    fn fixture_header(scope: &Scope, id: &str, data: &[u8]) -> Header {
        Header {
            scope: scope.0.clone(),
            track: track(id),
            bytes: data.len() as u64,
            audio_hash: hex(digest(&SHA256, data).as_ref()),
        }
    }

    fn fixture_payload(input: PlaybackInput) -> Vec<u8> {
        let mut bytes = Vec::new();
        input.source.reader().read_to_end(&mut bytes).unwrap();
        bytes
    }

    #[tokio::test]
    async fn online_admission_uses_the_verified_download_without_network() {
        let fixture = Fixture::new();
        let api = Arc::new(api("https://one.invalid"));
        let session = api.test_session();
        let store = fixture.store();
        let own = Scope::new(&api, &session).unwrap();
        install(&store, &own, "saved", b"verified local bytes").unwrap();
        let read = |id: &'static str| {
            let store = store.clone();
            online_media_with_store(
                &api,
                &session,
                Arc::new(Semaphore::new(1)),
                id,
                move || Ok(Some(store)),
            )
        };
        let chosen = read("saved").await.unwrap();
        assert!(chosen.source.is_saved());
        let mut actual = Vec::new();
        chosen.source.reader().read_to_end(&mut actual).unwrap();
        assert_eq!(actual, b"verified local bytes");
        // In isolation the engine can keep this exact verified descriptor;
        // a fresh online admission must still reopen and verify by account.
        let isolated = chosen.source.isolated().unwrap();
        let local = session.clone();
        local.set_offline(true);
        assert!(store.remove(&api, &local, "saved").unwrap());
        actual.clear();
        isolated.reader().read_to_end(&mut actual).unwrap();
        assert_eq!(actual, b"verified local bytes");
        assert!(read("saved").await.is_err(), "manual mode retires online admission");
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
            fixture_payload(reopened.read(&same, &new_session, "track-1").unwrap()),
            b"fixture bytes"
        );
        let mut altered = new_session.clone();
        altered.account_id = "other-account".into();
        assert_eq!(
            fixture_payload(reopened.read(&same, &altered, "track-1").unwrap()),
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
            fixture_payload(store.read(&api, &expired, "keep").unwrap()),
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
        let mut stage = store
            .stage(fixture_header(&scope, "track", b"temporary"))
            .unwrap();
        assert!(stale.exists());
        assert_eq!(
            stage.write(b"temporary", || Err(Error::Cancelled)),
            Err(Error::Cancelled)
        );
        drop(stage);
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
        assert!(!store.remove(&api, &session, "track").unwrap(), "damaged entry was already retired");
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

    #[tokio::test]
    async fn projection_does_not_read_sparse_library_payloads_or_replace_playback_verification() {
        let fixture = Fixture::new();
        let api = Arc::new(api("https://one.invalid"));
        let session = api.test_session();
        let scope = Scope::new(&api, &session).unwrap();
        let store = fixture.store();

        // Structurally valid but far too large for browsing to read. A wrong
        // digest additionally makes any accidental eager verification fail
        // the result assertion independently of machine performance.
        write_sparse_entry(
            &fixture.root(),
            &scope,
            "huge",
            16 * 1024_u64.pow(3),
            &"0".repeat(64),
        );
        install(&store, &scope, "changed", b"audio bytes").unwrap();
        let mut file = OpenOptions::new()
            .write(true)
            .open(fixture.root().join(scope.name("changed").unwrap()))
            .unwrap();
        file.seek(SeekFrom::End(-1)).unwrap();
        file.write_all(b"!").unwrap();
        drop(file);

        session.set_offline(true);
        let downloads = VerifiedDownloads::new(api.clone(), session.clone()).unwrap();
        let projected = tokio::time::timeout(Duration::from_secs(5), {
            let store = store.clone();
            downloads.verified_records_with(None, move || Ok(Some(store)))
        })
        .await
        .expect("browsing must not read large payloads")
        .unwrap();
        assert_eq!(projected.len(), 2);
        assert_eq!(projected["huge"].bytes, 16 * 1024_u64.pow(3));
        assert_eq!(projected["huge"].recording, Recording::fixture());
        let selected_store = store.clone();
        let result = read_verified_recording(
            api,
            session,
            Arc::new(Semaphore::new(1)),
            "changed".into(),
            projected["changed"].recording,
            move || Ok(selected_store),
        )
        .await;
        assert!(matches!(result, Err(Error::Damaged)));
        let after_failure = downloads
            .verified_records_with(None, {
                let reopened = fixture.store();
                move || Ok(Some(reopened))
            })
            .await
            .unwrap();
        assert!(!after_failure.contains_key("changed"));
        assert!(after_failure.contains_key("huge"));
        assert!(!fixture.root().join(scope.name("changed").unwrap()).exists(),
            "a new process must not depend on a memory-only list of corrupt files");
        install(&store, &scope, "changed", b"replacement").unwrap();
        let replacement = downloads
            .verified_records_with(None, move || Ok(Some(store)))
            .await
            .unwrap();
        assert!(replacement.contains_key("changed"));
    }

    #[tokio::test]
    async fn projection_skips_invalid_and_vanished_entries_without_losing_valid_tracks() {
        let fixture = Fixture::new();
        let api = Arc::new(api("https://one.invalid"));
        let session = api.test_session();
        let scope = Scope::new(&api, &session).unwrap();
        let store = fixture.store();
        let good = install(&store, &scope, "good", b"audio bytes").unwrap();
        let removed = install(&store, &scope, "remove", b"audio bytes").unwrap();
        for id in ["truncated", "unreadable"] {
            install(&store, &scope, id, b"audio bytes").unwrap();
        }
        OpenOptions::new()
            .write(true)
            .open(fixture.root().join(scope.name("truncated").unwrap()))
            .unwrap()
            .set_len(PREFIX_BYTES as u64)
            .unwrap();
        stdfs::set_permissions(
            fixture.root().join(scope.name("unreadable").unwrap()),
            stdfs::Permissions::from_mode(0o000),
        )
        .unwrap();
        symlink(
            "missing",
            fixture.root().join(scope.name("symlink").unwrap()),
        )
        .unwrap();
        for (origin, account, id) in [
            ("https://one.invalid", "other-account", "foreign-account"),
            ("https://two.invalid", &session.account_id, "foreign-origin"),
        ] {
            install(
                &store,
                &Scope::from_parts(origin, account),
                id,
                b"other scope",
            )
            .unwrap();
        }

        session.set_offline(true);
        let downloads = VerifiedDownloads::new(api.clone(), session.clone()).unwrap();
        let project = |ids| {
            let store = store.clone();
            downloads.verified_records_with(ids, move || Ok(Some(store)))
        };
        assert_eq!(
            project(None).await.unwrap(),
            HashMap::from([("good".into(), good.clone()), ("remove".into(), removed)])
        );
        assert!(store.remove(&api, &session, "remove").unwrap());
        assert_eq!(
            project(Some(
                [
                    "good",
                    "good",
                    "remove",
                    "truncated",
                    "unreadable",
                    "symlink",
                    "foreign-account",
                    "foreign-origin",
                    "..",
                ]
                .map(str::to_owned)
                .to_vec()
            ))
            .await
            .unwrap(),
            HashMap::from([("good".into(), good)])
        );
        assert!(project(Some(Vec::new())).await.unwrap().is_empty());
        assert!(
            downloads
                .verified_records_with(None, || Ok(None))
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn projection_cancels_on_mode_change_and_rejects_stale_grants() {
        let api = Arc::new(api("https://one.invalid"));
        let session = api.test_session();
        assert!(matches!(
            VerifiedDownloads::new(api.clone(), session.clone()),
            Err(Error::Offline)
        ));
        session.set_offline(true);
        let downloads = VerifiedDownloads::new(api.clone(), session.clone()).unwrap();
        let foreign_api = Arc::new(self::api("https://two.invalid"));
        assert!(matches!(
            VerifiedDownloads::new(foreign_api, session.clone()),
            Err(Error::Api(_))
        ));

        let (started, opening) = oneshot::channel();
        let (finish, finished) = std::sync::mpsc::channel();
        let stale = downloads.clone();
        let request = tokio::spawn(async move {
            stale
                .verified_records_with(None, move || {
                    started.send(()).unwrap();
                    finished.recv().unwrap();
                    Ok(None)
                })
                .await
        });
        opening.await.unwrap();
        session.set_offline(false);
        session.set_offline(true);
        let result = tokio::time::timeout(Duration::from_secs(2), request).await;
        finish.send(()).unwrap();
        assert_eq!(result.unwrap().unwrap(), Err(Error::Cancelled));
        assert_eq!(
            downloads
                .verified_records_with(None, || unreachable!())
                .await,
            Err(Error::Cancelled)
        );

        let renewed = VerifiedDownloads::new(api.clone(), session.clone()).unwrap();
        assert!(api.logout(&session).await.is_err()); // local revocation only
        assert!(
            matches!(renewed.verified_records_with(None, || unreachable!()).await, Err(Error::Api(err)) if err.expired)
        );
    }

    #[tokio::test]
    async fn selected_recording_cannot_play_a_same_id_valid_replacement() {
        let fixture = Fixture::new();
        let api = Arc::new(api("https://one.invalid"));
        let session = api.test_session();
        let scope = Scope::new(&api, &session).unwrap();
        let store = fixture.store();
        let id = "same-id";
        let mut original = wav_header(2).to_vec();
        original.extend_from_slice(&[0, 0]);
        let installed = install(&store, &scope, id, &original).unwrap();
        assert_eq!(store.list(&api, &session).unwrap(), [installed.clone()]);

        session.set_offline(true);
        let downloads = VerifiedDownloads::new(api.clone(), session.clone()).unwrap();
        let browse = || {
            let store = store.clone();
            downloads.verified_records_with(Some(vec![id.into()]), move || Ok(Some(store)))
        };
        let selected = browse().await.unwrap()[id].clone();
        assert_eq!(selected, installed);
        let open = || {
            let store = store.clone();
            move || Ok(store)
        };
        let play = |recording| {
            read_verified_recording(
                api.clone(),
                session.clone(),
                Arc::new(Semaphore::new(1)),
                id.into(),
                recording,
                open(),
            )
        };
        let (_, initial_input) = play(selected.recording).await.unwrap();
        assert_eq!(initial_input.recording, selected.recording);
        assert_eq!(fixture_payload(initial_input), original);

        assert!(store.remove(&api, &session, id).unwrap());
        let mut replacement = wav_header(2).to_vec();
        replacement.extend_from_slice(&[1, 0]);
        let mut header = fixture_header(&scope, id, &replacement);
        header.track.name = "Replacement recording".into();
        {
            let _guard = store.admit(&scope.name(id).unwrap()).unwrap();
            let mut stage = store.stage(header).unwrap();
            stage.write(&replacement, || Ok(())).unwrap();
            let (partial, header) = stage.verify_and_sync(|| Ok(())).unwrap();
            store.publish(partial, header, || Ok(()), || ()).unwrap();
        }

        assert!(matches!(
            play(selected.recording).await,
            Err(Error::RecordingChanged)
        ));
        let current = browse().await.unwrap()[id].clone();
        assert_ne!(selected.recording, current.recording);
        let (_, input) = play(current.recording).await.unwrap();
        assert_eq!(input.recording, current.recording);
        assert_eq!(input.item.name, "Replacement recording");
        assert_eq!(fixture_payload(input), replacement);
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
        assert_eq!(
            fixture_payload(store.read(&api, &session, "track-1088").unwrap()),
            [0]
        );
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
        const OLD_TRACK_CAP: usize = 32 * 1024 * 1024;
        let mut hash = Context::new(&SHA256);
        for _ in 0..(OLD_TRACK_CAP / IO_CHUNK) {
            hash.update(&[0; IO_CHUNK]);
        }
        let hash = hex(hash.finish().as_ref());
        // Exceeds the old byte quota alone; the audio payloads are sparse holes.
        for index in 0..33 {
            write_sparse_entry(
                &fixture.root(),
                if index == 0 { &foreign } else { &scope },
                &format!("track-{index}"),
                OLD_TRACK_CAP as u64,
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
    fn kernel_enospc_cleans_stage_and_releases_admission() {
        let full = match fs::open("/dev/full", FILE_FLAGS | OFlags::WRONLY, Mode::empty()) {
            Ok(fd) => fd,
            Err(Errno::NOENT) => {
                eprintln!("Skipping kernel ENOSPC fixture: /dev/full is unavailable");
                return;
            }
            Err(err) => panic!("open Linux ENOSPC fixture: {err}"),
        };
        let stat = fs::fstat(&full).unwrap();
        assert_eq!(
            FileType::from_raw_mode(stat.st_mode),
            FileType::CharacterDevice
        );
        assert_eq!(stat.st_rdev, fs::makedev(1, 7));

        let fixture = Fixture::new();
        let store = fixture.store();
        let scope = Scope::from_parts("https://one.invalid", "fixture");
        let key = scope.name("track").unwrap();
        let partial = fixture.root().join(STAGE);
        let data = b"nonempty body";
        let result = (|| {
            let _admission = store.admit(&key)?;
            let mut stage = store.stage(fixture_header(&scope, "track", data))?;
            assert!(partial.exists());
            // Inject only the write sink; the real private stage still needs cleanup.
            stage.file = File::from(full);
            stage.write(data, || Ok(()))
        })();
        assert_eq!(result, Err(Error::Full));
        assert!(!partial.exists());
        assert!(!fixture.root().join(&key).exists());
        let _next = store.admit(&key).expect("admission released after ENOSPC");
    }

    // Mono 16-bit PCM at 8 kHz. Generate only the header; transfer samples in chunks.
    fn wav_header(data_len: u32) -> [u8; 44] {
        let mut bytes = *b"RIFF\0\0\0\0WAVEfmt \x10\0\0\0\x01\0\x01\0\x40\x1f\0\0\x80\x3e\0\0\x02\0\x10\0data\0\0\0\0";
        bytes[4..8].copy_from_slice(&(36 + data_len).to_le_bytes());
        bytes[40..44].copy_from_slice(&data_len.to_le_bytes());
        bytes
    }

    #[tokio::test]
    async fn streams_large_v1_wav_and_reads_same_descriptor_after_unlink() {
        const CHUNKS: usize = 33 * 1024 * 1024 / IO_CHUNK;
        let fixture = Fixture::new();
        let api = api("https://one.invalid");
        let session = api.test_session();
        let scope = Scope::new(&api, &session).unwrap();
        let store = fixture.store();
        let key = scope.name("large").unwrap();
        let path = fixture.root().join(&key);
        let wav = wav_header((CHUNKS * IO_CHUNK) as u32);
        let len = (wav.len() + CHUNKS * IO_CHUNK) as u64;
        let sample = [0x34, 0x12];
        let mut samples = [0; IO_CHUNK];
        samples[..sample.len()].copy_from_slice(&sample);
        samples[IO_CHUNK - sample.len()..].copy_from_slice(&sample);
        let mut hash = Context::new(&SHA256);
        hash.update(&wav);
        for _ in 0..CHUNKS {
            hash.update(&samples);
        }
        let digest = hex(hash.finish().as_ref());
        let parts = std::iter::once(wav.to_vec()).chain((0..CHUNKS).map(|_| samples.to_vec()));
        let saved = store
            .transfer(
                store.admit(&key).unwrap(),
                Header {
                    scope: scope.0.clone(),
                    track: track("large"),
                    bytes: len,
                    audio_hash: digest.clone(),
                },
                stream::iter(parts.map(Ok)),
                || Ok(()),
                DOWNLOAD_DISK_STALL,
            )
            .await
            .unwrap();
        assert_eq!(saved.bytes, len);
        assert_eq!(store.list(&api, &session).unwrap(), [saved]);
        assert!(!fixture.root().join(STAGE).exists());

        // Match the original v1 keys, order, and prefix exactly, not just a new round trip.
        let expected = format!(
            concat!(
                "{{\"scope\":\"{}\",\"track\":{{\"id\":\"large\",\"name\":\"Fixture track\",",
                "\"artists\":\"Fixture artist\",\"album_name\":\"\",\"duration_ms\":10,",
                "\"track_number\":1,\"disc_number\":1}},\"bytes\":{},\"audio_hash\":\"{}\"}}"
            ),
            scope.0, len, digest
        );
        let mut persisted = File::open(&path).unwrap();
        let mut prefix = [0; PREFIX_BYTES];
        persisted.read_exact(&mut prefix).unwrap();
        assert_eq!(&prefix[..8], b"SPOFDL01");
        assert_eq!(&prefix[8..], &(expected.len() as u32).to_le_bytes());
        let mut json = vec![0; expected.len()];
        persisted.read_exact(&mut json).unwrap();
        assert_eq!(json, expected.as_bytes());
        drop(persisted);

        let pinned = store
            .lock_members(&api, &session, &["large".into()])
            .unwrap();
        assert_eq!(store.remove(&api, &session, "large"), Err(Error::Busy));
        drop(pinned);

        let checks = std::cell::Cell::new(0);
        let bump = || {
            checks.set(checks.get() + 1);
            checks.get()
        };
        assert!(matches!(
            store.read_live(&api, &session, "large", || {
                if bump() == 3 {
                    Err(Error::Cancelled)
                } else {
                    Ok(())
                }
            }),
            Err(Error::Cancelled)
        ));
        let input = store.read(&api, &session, "large").unwrap();
        checks.set(0);
        assert!(matches!(store.read_live(&api, &session, "large", || {
            if bump() == 3 {
                // Replacement during verification invalidates a new admission.
                assert!(store.remove(&api, &session, "large").unwrap());
                install(&store, &scope, "large", b"different recording").unwrap();
            }
            Ok(())
        }), Err(Error::RecordingChanged)));
        assert!(!input.source.may_start());
        // The descriptor remains readable for a run already admitted.
        assert_eq!(input.source.len(), len);
        let mut reader = input.source.reader();
        assert_eq!(reader.seek(SeekFrom::End(-2)).unwrap(), len - 2);
        let mut last = [0; 2];
        reader.read_exact(&mut last).unwrap();
        assert_eq!(last, sample);
        assert_eq!(reader.read(&mut last).unwrap(), 0);
        let mut decoder = rodio::Decoder::builder()
            .with_data(input.source.reader())
            .with_byte_len(len)
            .build()
            .unwrap();
        assert!(
            (decoder.next().unwrap() - (i16::from_le_bytes(sample) as f32 / 32768.0)).abs() < 0.001
        );
        assert_eq!(
            fixture_payload(store.read(&api, &session, "large").unwrap()),
            b"different recording"
        );
    }

    #[tokio::test]
    async fn short_long_corrupt_and_unsupported_transfers_do_not_publish() {
        let fixture = Fixture::new();
        let api = api("https://one.invalid");
        let session = api.test_session();
        let scope = Scope::new(&api, &session).unwrap();
        let store = fixture.store();
        let key = scope.name("track").unwrap();
        let wav = wav_header(2);
        let mut audio = wav.to_vec();
        audio.extend_from_slice(&[1, 0]);
        let mut altered = audio.clone();
        *altered.last_mut().unwrap() ^= 0xff;
        for (body, actual, error) in [
            (&audio[..], &audio[..45], Error::InvalidMedia),
            (&audio[..45], &audio[..], Error::InvalidMedia),
            (&audio[..], &altered[..], Error::InvalidMedia),
            (
                &b"not audio"[..],
                &b"not audio"[..],
                Error::UnsupportedAudio,
            ),
        ] {
            assert_eq!(
                store
                    .transfer(
                        store.admit(&key).unwrap(),
                        fixture_header(&scope, "track", body),
                        stream::iter([Ok(actual.to_vec())]),
                        || Ok(()),
                        DOWNLOAD_DISK_STALL,
                    )
                    .await,
                Err(error)
            );
            assert!(!fixture.root().join(STAGE).exists());
            assert!(!fixture.root().join(&key).exists());
        }
        store
            .transfer(
                store.admit(&key).unwrap(),
                fixture_header(&scope, "track", &audio),
                stream::iter([Ok(audio)]),
                || Ok(()),
                DOWNLOAD_DISK_STALL,
            )
            .await
            .unwrap();
        assert!(fixture.root().join(key).exists());
    }

    #[tokio::test]
    async fn stalled_transfer_times_out_or_cancels_without_losing_its_stage_lease() {
        for stop in [Some(Error::Storage), Some(Error::Offline), None] {
            exercise_stalled_transfer(stop).await;
        }
    }

    async fn exercise_stalled_transfer(stop: Option<Error>) {
        use std::sync::atomic::AtomicUsize;
        use tokio::{sync::Notify, time::timeout};

        const WAIT: Duration = Duration::from_secs(10);
        let fixture = Fixture::new();
        let api = api("https://one.invalid");
        let session = api.test_session();
        let scope = Scope::new(&api, &session).unwrap();
        let store = fixture.store();
        let key = scope.name("track").unwrap();
        let admission = store.admit(&key).unwrap();
        let header = fixture_header(&scope, "track", b"partial body");
        let writing = Arc::new(Notify::new());
        let worker_writing = writing.clone();
        let produced = Arc::new(AtomicUsize::new(0));
        let producer_count = produced.clone();
        let worker_count = produced.clone();
        let (resume, wait) = std::sync::mpsc::sync_channel(1);
        let wait = std::sync::Mutex::new(wait);
        let blocked = AtomicBool::new(false);
        let mut mode = session.network_changes();
        let generation = *mode.borrow_and_update();
        let live = download_liveness(session.clone(), mode.clone(), generation);
        let mut task = tokio::spawn({
            let store = store.clone();
            async move {
                in_download_mode(
                    &mut mode,
                    store.transfer(
                        admission,
                        header,
                        stream::repeat_with(move || {
                            producer_count.fetch_add(1, Ordering::Relaxed);
                            Ok(&b"partial"[..])
                        }),
                        move || {
                            // Stall on the first packet after staging, not while awaiting HTTP.
                            if worker_count.load(Ordering::Relaxed) != 0
                                && !blocked.swap(true, Ordering::Relaxed)
                            {
                                worker_writing.notify_one();
                                let _ = wait.lock().unwrap().recv_timeout(WAIT);
                            }
                            live()
                        },
                        if stop == Some(Error::Storage) {
                            Duration::from_millis(250)
                        } else {
                            WAIT
                        },
                    ),
                )
                .await
            }
        });
        timeout(WAIT, writing.notified()).await.unwrap();
        assert_eq!(produced.load(Ordering::Relaxed), 1);
        let stage = fixture.root().join(STAGE);
        assert!(stage.exists());
        assert!(store.list(&api, &session).unwrap().is_empty());
        assert!(matches!(store.admit(&key), Err(Error::Busy)));
        match stop {
            Some(err) => {
                if err == Error::Offline {
                    session.set_offline(true);
                }
                assert_eq!(
                    timeout(Duration::from_secs(2), &mut task)
                        .await
                        .unwrap()
                        .unwrap(),
                    Err(err)
                );
            }
            None => {
                task.abort();
                assert!(task.await.unwrap_err().is_cancelled());
            }
        }
        // Returning from the async operation must not release a retired disk worker's lease.
        assert!(stage.exists());
        assert!(matches!(store.admit(&key), Err(Error::Busy)));
        assert!(!fixture.root().join(&key).exists());
        resume.send(()).unwrap();
        timeout(WAIT, async {
            while stage.exists() {
                tokio::task::yield_now().await;
            }
            loop {
                match store.admit(&key) {
                    Ok(_) => break,
                    Err(Error::Busy) => tokio::task::yield_now().await,
                    other => panic!("unexpected admission result: {other:?}"),
                }
            }
        })
        .await
        .unwrap();
        assert!(!fixture.root().join(key).exists());
    }

    #[tokio::test]
    async fn stalled_admission_keeps_worker_slot_until_it_physically_returns() {
        use tokio::{sync::Notify, time::timeout};

        const WAIT: Duration = Duration::from_secs(10);
        let fixture = Fixture::new();
        let store = fixture.store();
        let key = Scope::from_parts("https://one.invalid", "fixture")
            .name("track")
            .unwrap();
        let started = Arc::new(Notify::new());
        let blocked = started.clone();
        let (resume, wait) = std::sync::mpsc::sync_channel(1);
        let task = tokio::spawn({
            let (store, key) = (store.clone(), key.clone());
            async move {
                store
                    .admit_download(
                        key,
                        move || {
                            blocked.notify_one();
                            let _ = wait.recv_timeout(WAIT);
                            Ok(())
                        },
                        Duration::from_millis(250),
                    )
                    .await
            }
        });
        timeout(WAIT, started.notified()).await.unwrap();
        assert!(matches!(
            timeout(Duration::from_secs(2), task)
                .await
                .unwrap()
                .unwrap(),
            Err(Error::Storage)
        ));
        assert!(matches!(
            store.admit_download(key.clone(), || Ok(()), WAIT).await,
            Err(Error::Busy)
        ));
        resume.send(()).unwrap();
        timeout(WAIT, async {
            loop {
                match store.admit_download(key.clone(), || Ok(()), WAIT).await {
                    Ok(_) => break,
                    Err(Error::Busy) => tokio::task::yield_now().await,
                    Err(err) => panic!("unexpected admission result: {err}"),
                }
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn saved_publication_does_not_block_network_mode_changes() {
        use tokio::{sync::Notify, time::timeout};

        const WAIT: Duration = Duration::from_secs(10);
        let fixture = Fixture::new();
        let api = api("https://one.invalid");
        let session = api.test_session();
        let scope = Scope::new(&api, &session).unwrap();
        let mut mode = session.network_changes();
        let generation = *mode.borrow_and_update();
        let live = download_liveness(session.clone(), mode, generation);
        let store = fixture.store();
        let key = scope.name("track").unwrap();
        let at_rename = Arc::new(Notify::new());
        let blocked = at_rename.clone();
        let (resume, wait) = std::sync::mpsc::sync_channel(1);
        let worker = tokio::task::spawn_blocking({
            let (store, key) = (store.clone(), key.clone());
            move || {
                let _admission = store.admit(&key)?;
                let data = b"fixture payload";
                let mut stage = store.stage(fixture_header(&scope, "track", data))?;
                stage.write(data, || Ok(()))?;
                let checks = std::cell::Cell::new(0);
                store.publish(
                    stage.partial,
                    stage.header,
                    || {
                        let check = checks.get();
                        checks.set(check + 1);
                        if check == 2 {
                            // At the final check with the directory locked, before rename.
                            blocked.notify_one();
                            let _ = wait.recv_timeout(WAIT);
                        }
                        live()
                    },
                    || (),
                )
            }
        });
        timeout(WAIT, at_rename.notified()).await.unwrap();
        timeout(
            Duration::from_secs(2),
            tokio::task::spawn_blocking(move || session.set_offline(true)),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(fixture.root().join(STAGE).exists());
        resume.send(()).unwrap();
        assert_eq!(
            timeout(WAIT, worker).await.unwrap().unwrap(),
            Err(Error::Offline)
        );
        assert!(!fixture.root().join(STAGE).exists());
        assert!(!fixture.root().join(key).exists());
    }

    #[test]
    fn validates_go_manifest_digest_mime_track_and_size() {
        let audio = b"synthetic payload";
        let hash = hex(digest(&SHA256, audio).as_ref());
        let mut body = serde_json::json!({"version": 1, "trackId":"track", "mediaVersion":format!("sha256:{hash}"),
            "sha256":hash, "format":"wav", "byteLength":audio.len(), "contentType":"audio/wav", "downloadUrl":"https://ignored.invalid/"});
        let parse =
            |body: &serde_json::Value| Manifest::parse(&serde_json::to_vec(body).unwrap(), "track");
        assert_eq!(parse(&body).unwrap().byte_length, audio.len() as u64);
        body["mediaVersion"] = format!("sha256:{}", "0".repeat(64)).into();
        assert!(matches!(parse(&body), Err(Error::InvalidMedia)));
        body["mediaVersion"] = format!("sha256:{hash}").into();
        body["trackId"] = "another".into();
        assert!(matches!(parse(&body), Err(Error::InvalidMedia)));
        body["trackId"] = "track".into();
        body["contentType"] = "image/png".into();
        assert!(matches!(parse(&body), Err(Error::InvalidMedia)));
        body["contentType"] = "audio/wav".into();
        body["byteLength"] = 0.into();
        assert!(matches!(parse(&body), Err(Error::InvalidMedia)));
        body["byteLength"] = (u32::MAX as u64 + 1).into();
        assert_eq!(parse(&body).unwrap().byte_length, 1 << 32);
    }
}
