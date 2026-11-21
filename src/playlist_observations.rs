//! Last positively observed playlist metadata and occurrence membership. This
//! does not resolve tracks, confer write/play authority, or change any saves.
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    fs::File,
    io::{BufReader, BufWriter, Read, Write},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use ring::digest::{Context, SHA256};
use rustix::{
    fd::OwnedFd,
    fs::{self, AtFlags, FileType, FlockOperation, Mode, OFlags},
    io::Errno,
    process::geteuid,
};
use serde::{Deserialize, Serialize, Serializer};
use tokio::sync::{Notify, mpsc, oneshot, watch};

use crate::{
    api::{
        Api, ApiError, Session,
        library::{EvaluationState, ManualEntry, PlaylistKind, PlaylistSummary},
    },
    desk::Access,
    personal::AcceptedPlaylist,
    playlist_pin_store::PUBLICATION_GATE,
    storage::{self, DIR_FLAGS},
};

const ROOT: &str = "spofify-playlist-observations-v1";
const VERSION: u8 = 1;
const FILE_FLAGS: OFlags = OFlags::NOFOLLOW
    .union(OFlags::CLOEXEC)
    .union(OFlags::NONBLOCK);
const PRIVATE_FILE: Mode = Mode::RUSR.union(Mode::WUSR);
const WRITER_CAPACITY: usize = 64;
const READ_WAIT: Duration = Duration::from_secs(2);
const FLUSH_WAIT: Duration = Duration::from_millis(500);

const fn error(message: &'static str) -> ApiError {
    ApiError {
        message,
        expired: false,
        status: 0,
    }
}
const STORAGE: ApiError = error("Unable to access offline playlist observations.");
const INVALID: ApiError = error("Offline playlist observations are invalid.");
const NOT_CURRENT: ApiError = error("The account or connection mode changed.");
const NOT_DURABLE: ApiError =
    error("Offline playlist observations could not be confirmed on disk.");
const OVERLOADED: ApiError = error("Offline playlist changes could not be saved.");
const BUSY: ApiError = error("Offline playlist storage is busy.");

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Summary {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) description: String,
    #[serde(serialize_with = "serialize_kind")]
    pub(crate) kind: PlaylistKind,
    #[serde(
        serialize_with = "serialize_evaluation",
        deserialize_with = "Deserialize::deserialize"
    )]
    pub(crate) evaluation_state: Option<EvaluationState>,
    pub(crate) revision: u64,
    #[serde(deserialize_with = "Deserialize::deserialize")]
    pub(crate) track_count: Option<u64>,
    #[serde(deserialize_with = "Deserialize::deserialize")]
    pub(crate) available_count: Option<u64>,
    pub(crate) updated_at: String,
}

fn serialize_kind<S: Serializer>(kind: &PlaylistKind, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(match kind {
        PlaylistKind::Manual => "manual",
        PlaylistKind::Derived => "derived",
    })
}

fn serialize_evaluation<S: Serializer>(
    state: &Option<EvaluationState>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    state
        .map(|state| match state {
            EvaluationState::Pending => "pending",
            EvaluationState::Ready => "ready",
            EvaluationState::Stale => "stale",
            EvaluationState::Error => "error",
        })
        .serialize(serializer)
}

impl Summary {
    fn ready_membership(&self) -> bool {
        self.kind == PlaylistKind::Manual || self.evaluation_state == Some(EvaluationState::Ready)
    }
}

impl From<&PlaylistSummary> for Summary {
    fn from(p: &PlaylistSummary) -> Self {
        Self {
            id: p.id.clone(),
            name: p.name.trim().to_owned(),
            description: p.description.clone(),
            kind: p.kind,
            evaluation_state: p.evaluation_state,
            revision: p.revision,
            track_count: p.track_count,
            available_count: p.available_count,
            updated_at: p.updated_at.clone(),
        }
    }
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Occurrence {
    pub(crate) occurrence_id: String,
    pub(crate) position: u64,
    pub(crate) track_id: String,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Membership {
    pub(crate) revision: u64,
    pub(crate) rows: Vec<Occurrence>,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "lowercase", deny_unknown_fields)]
enum Record {
    Known {
        summary: Summary,
        #[serde(deserialize_with = "Deserialize::deserialize")]
        membership: Option<Membership>,
        membership_current: bool,
    },
    Deleted {},
}

#[derive(Default, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct ObservedPlaylists(BTreeMap<String, Record>);

impl ObservedPlaylists {
    pub(crate) fn summaries(&self) -> impl Iterator<Item = &Summary> {
        self.0.values().filter_map(|record| match record {
            Record::Known { summary, .. } => Some(summary),
            Record::Deleted { .. } => None,
        })
    }

    pub(crate) fn summary(&self, id: &str) -> Option<&Summary> {
        match self.0.get(id)? {
            Record::Known { summary, .. } => Some(summary),
            Record::Deleted { .. } => None,
        }
    }

    pub(crate) fn membership(&self, id: &str) -> Option<&Membership> {
        match self.0.get(id)? {
            Record::Known { membership, .. } => membership.as_ref(),
            Record::Deleted { .. } => None,
        }
    }

    pub(crate) fn is_deleted(&self, id: &str) -> bool {
        matches!(self.0.get(id), Some(Record::Deleted { .. }))
    }

    /// An invalidation is sticky until a fresh complete detail. A later
    /// ready summary alone does not prove its rows match that evaluation.
    pub(crate) fn membership_is_current(&self, id: &str) -> bool {
        matches!(
            self.0.get(id),
            Some(Record::Known {
                membership_current: true,
                ..
            })
        )
    }

    fn apply(&mut self, observation: &Observation) -> bool {
        match observation {
            Observation::Summaries(summaries) => {
                // Snapshot omission (of any length) is not evidence of deletion.
                summaries.iter().fold(false, |changed, summary| {
                    self.observe(summary, None, false) | changed
                })
            }
            Observation::Summary(summary, invalidates) => self.observe(summary, None, *invalidates),
            Observation::Complete(summary, rows) => self.observe(summary, Some(rows), false),
            Observation::Deleted(id) if self.is_deleted(id) => false,
            Observation::Deleted(id) => {
                self.0.insert(id.clone(), Record::Deleted {});
                true
            }
        }
    }

    fn observe(
        &mut self,
        summary: &Summary,
        rows: Option<&[ManualEntry]>,
        invalidates: bool,
    ) -> bool {
        let old = self.0.get(&summary.id);
        if match old {
            Some(Record::Known { summary: old, .. }) => {
                old.revision > summary.revision || old.kind != summary.kind
            }
            // A playlist ID is immutable. No positive frame, regardless of
            // revision or freshness, proves a different incarnation of it.
            Some(Record::Deleted { .. }) => true,
            None => false,
        } {
            return false;
        }
        // A terminal Rules page may be pending, failed or stale. It supplies
        // evaluation metadata, but it is not a complete evaluated membership.
        let rows = rows.filter(|_| summary.ready_membership());
        let current = rows.is_some()
            || (!invalidates
                && summary.ready_membership()
                && matches!(old, Some(Record::Known { summary: last, membership_current: true, .. })
                if last.revision == summary.revision));
        let membership = rows.map(|rows| Membership {
            revision: summary.revision,
            rows: rows
                .iter()
                .map(|row| Occurrence {
                    occurrence_id: row.id.clone(),
                    position: row.position,
                    track_id: row.track.id.clone(),
                })
                .collect(),
        });
        if let Some(Record::Known {
            summary: previous,
            membership: last,
            membership_current,
        }) = old
            && previous == summary
            && *membership_current == current
            && (membership.is_none() || membership == *last)
        {
            return false;
        }
        let previous = self.0.remove(&summary.id);
        let membership = membership.or_else(|| match previous {
            Some(Record::Known { membership, .. }) => membership,
            _ => None,
        });
        self.0.insert(
            summary.id.clone(),
            Record::Known {
                summary: summary.clone(),
                membership,
                membership_current: current,
            },
        );
        true
    }

    fn valid(&self) -> bool {
        self.0.iter().all(|(id, record)| match record {
            Record::Deleted { .. } => id_ok(id),
            Record::Known {
                summary: p,
                membership,
                membership_current,
            } => {
                id == &p.id
                    && id.len() <= 128
                    && !id.is_empty()
                    && id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
                    && name_ok(&p.name)
                    && p.description.len() <= 2000
                    && p.description.chars().count() <= 500
                    && !p
                        .description
                        .chars()
                        .any(|c| c.is_control() && c != '\n' && c != '\t')
                    && p.updated_at.len() <= 35
                    && cookie::time::OffsetDateTime::parse(
                        &p.updated_at,
                        &cookie::time::format_description::well_known::Rfc3339,
                    )
                    .is_ok()
                    && revision_ok(p.revision)
                    && (p.kind != PlaylistKind::Manual || p.evaluation_state.is_none())
                    && (!membership_current
                        || (p.ready_membership()
                            && membership
                                .as_ref()
                                .is_some_and(|m| m.revision == p.revision)))
                    && match (p.track_count, p.available_count) {
                        (Some(total), Some(available)) => available <= total,
                        (None, None) => p.kind == PlaylistKind::Derived,
                        _ => false,
                    }
                    && membership.as_ref().is_none_or(|m| {
                        revision_ok(m.revision)
                            && m.revision <= p.revision
                            && m.rows.windows(2).all(|r| r[0].position < r[1].position)
                            && {
                                let mut occurrences = HashSet::new();
                                m.rows.iter().all(|r| {
                                    occurrence_ok(p.kind, &r.occurrence_id)
                                        && r.position <= i64::MAX as u64
                                        && id_ok(&r.track_id)
                                        && occurrences.insert(r.occurrence_id.as_str())
                                })
                            }
                    })
            }
        })
    }
}

fn id_ok(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 512
        && id.trim() == id
        && !matches!(id, "." | "..")
        && !id.chars().any(char::is_control)
}
fn occurrence_ok(kind: PlaylistKind, id: &str) -> bool {
    match kind {
        PlaylistKind::Manual => id_ok(id),
        PlaylistKind::Derived => {
            id.starts_with("o1_") && id.len() <= 4096 && id.bytes().all(|b| b.is_ascii_graphic())
        }
    }
}
fn name_ok(name: &str) -> bool {
    let name = name.trim();
    !name.is_empty()
        && name.len() <= 400
        && name.chars().count() <= 100
        && !name.chars().any(char::is_control)
}
fn revision_ok(revision: u64) -> bool {
    (1..=i64::MAX as u64).contains(&revision)
}

// Hold shared reducer rows until the disk worker can project only the three
// occurrence fields. Completed playlists aren't copied under the engine lock.
enum Observation {
    Summaries(Vec<Summary>),
    Summary(Summary, bool),
    Complete(Summary, Arc<Vec<ManualEntry>>),
    Deleted(String),
}

type LoadResult = Result<Option<ObservedPlaylists>, ApiError>;
enum Request {
    Store(Observation),
    Barrier {
        read: bool,
        reply: oneshot::Sender<LoadResult>,
    },
}

struct Grant {
    api: Arc<Api>,
    session: Session,
    epoch: u64,
    current_epoch: Arc<AtomicU64>,
    mode: watch::Receiver<u64>,
}

impl Grant {
    fn account(&self) -> Result<(&str, &str), ApiError> {
        if self.current_epoch.load(Ordering::SeqCst) != self.epoch {
            return Err(NOT_CURRENT);
        }
        self.api.authenticated_account(&self.session)
    }

    fn matches(&self, access: &Access) -> bool {
        self.epoch == access.epoch
            && Arc::ptr_eq(&self.api, &access.api)
            && self.mode.same_channel(&access.session.network_changes())
    }

    fn mode(&self) -> u64 {
        *self.mode.borrow()
    }

    fn check(&self, mode: u64) -> Result<(), ApiError> {
        self.account()?;
        if self.mode() != mode {
            return Err(NOT_CURRENT);
        }
        Ok(())
    }
}

#[derive(Default)]
struct Tombstones {
    // Only immutable IDs, coalesced. The engine inserts one ID and the
    // worker swaps the set out; no disk or projection under this lock.
    ids: Mutex<BTreeSet<String>>,
    changed: Notify,
}

#[derive(Clone)]
pub(crate) struct Writer {
    tx: mpsc::Sender<Request>,
    grant: Arc<Grant>,
    overloaded: Arc<AtomicBool>,
    tombstones: Arc<Tombstones>,
}

impl Writer {
    pub(crate) fn start(access: &Access, current_epoch: Arc<AtomicU64>) -> Result<Self, ApiError> {
        Self::start_in(access, current_epoch, Root::Default)
    }

    fn start_in(
        access: &Access,
        current_epoch: Arc<AtomicU64>,
        root: Root,
    ) -> Result<Self, ApiError> {
        let grant = Arc::new(Grant {
            api: access.api.clone(),
            session: access.session.clone(),
            epoch: access.epoch,
            current_epoch,
            mode: access.session.network_changes(),
        });
        let (origin, account) = grant.account()?;
        let store = Store {
            root,
            scope: scope_hash(origin, account),
        };
        let (tx, rx) = mpsc::channel(WRITER_CAPACITY);
        let writer = Self {
            tx,
            grant,
            overloaded: Arc::default(),
            tombstones: Arc::default(),
        };
        tokio::spawn(run(
            rx,
            store,
            writer.grant.clone(),
            writer.overloaded.clone(),
            writer.tombstones.clone(),
        ));
        Ok(writer)
    }

    pub(crate) fn matches(&self, access: &Access) -> bool {
        self.grant.matches(access)
    }

    pub(crate) fn online_mode(&self) -> Option<u64> {
        let mode = self.grant.mode();
        (mode & 1 == 0 && self.grant.check(mode).is_ok()).then_some(mode)
    }

    pub(crate) fn observe(&self, mode: u64, event: AcceptedPlaylist<'_>) {
        if mode & 1 != 0 || self.grant.check(mode).is_err() {
            return;
        }
        if let AcceptedPlaylist::Deleted(id) = event {
            self.tombstones.ids.lock().unwrap().insert(id.to_owned());
            self.tombstones.changed.notify_one();
            return;
        }
        if self.overloaded.load(Ordering::Acquire) {
            return;
        }
        let observation = match event {
            AcceptedPlaylist::Summaries(s) if s.is_empty() => return,
            AcceptedPlaylist::Summaries(s) => {
                Observation::Summaries(s.iter().map(Summary::from).collect())
            }
            AcceptedPlaylist::Summary {
                summary,
                invalidates_membership,
            } => Observation::Summary(summary.into(), invalidates_membership),
            AcceptedPlaylist::Complete(p) => {
                let summary = Summary::from(&p.summary);
                if summary.ready_membership() {
                    Observation::Complete(summary, p.rows.clone())
                } else {
                    Observation::Summary(summary, true)
                }
            }
            AcceptedPlaylist::Deleted(_) => unreachable!("deletes use the priority set"),
        };
        if let Err(mpsc::error::TrySendError::Full(_)) =
            self.tx.try_send(Request::Store(observation))
        {
            // The durable tombstones still advance, but the prefix of
            // positive observations isn't complete evidence for this grant.
            self.overloaded.store(true, Ordering::Release);
        }
    }

    async fn barrier(&self, read: bool) -> LoadResult {
        if read && self.overloaded.load(Ordering::Acquire) {
            return Err(OVERLOADED);
        }
        let wait = if read { READ_WAIT } else { FLUSH_WAIT };
        let result = tokio::time::timeout(wait, async {
            let (reply, rx) = oneshot::channel();
            self.tx
                .send(Request::Barrier { read, reply })
                .await
                .map_err(|_| STORAGE)?;
            rx.await.map_err(|_| STORAGE)?
        })
        .await;
        let result = result.map_err(|_| BUSY)??;
        if self.overloaded.load(Ordering::Acquire) {
            return Err(OVERLOADED);
        }
        Ok(result)
    }

    pub(crate) async fn flush(&self) -> Result<(), ApiError> {
        self.barrier(false).await.map(|_| ())
    }
}

pub(crate) async fn load(access: &Access) -> Result<ObservedPlaylists, ApiError> {
    let writer = access
        .personal
        .playlist_observations(access)
        .ok_or(NOT_CURRENT)?;
    let mode = writer.grant.mode();
    if (mode & 1 != 0) != access.offline {
        return Err(NOT_CURRENT);
    }
    writer.grant.check(mode)?;
    // The writer is FIFO: reading immediately after going offline still sees
    // previously accepted observations, rather than racing a queued deletion.
    let result = writer.barrier(true).await;
    writer.grant.check(mode)?;
    if access.personal.playlist_observations(access).is_none() {
        return Err(NOT_CURRENT);
    }
    result?.ok_or(STORAGE)
}

async fn run(
    mut rx: mpsc::Receiver<Request>,
    store: Store,
    grant: Arc<Grant>,
    overloaded: Arc<AtomicBool>,
    tombstones: Arc<Tombstones>,
) {
    let mut pending = Vec::new();
    let mut pending_deletes = BTreeSet::new();
    let mut write_fault = None;
    loop {
        let (mut next, closed) = tokio::select! {
            biased;
            _ = tombstones.changed.notified() => (None, false),
            request = rx.recv() => {
                let closed = request.is_none();
                (request, closed)
            }
        };
        let mut barrier = None;
        // Batch ready observations but don't delay a browsing request behind
        // any later frames. The batch size is not a membership limit.
        for _ in pending.len()..WRITER_CAPACITY {
            match next.take().or_else(|| rx.try_recv().ok()) {
                Some(Request::Store(event)) => {
                    if write_fault.is_none() {
                        pending.push(event);
                    }
                }
                Some(Request::Barrier { read, reply }) => {
                    barrier = Some((read, reply));
                    break;
                }
                None => break,
            }
        }
        // Drain after selecting a barrier: earlier accepted deletes have
        // already reached this set. Publish them separately so a failed or
        // saturated positive batch never blocks their durable update.
        let deletes = std::mem::take(&mut *tombstones.ids.lock().unwrap());
        pending_deletes.extend(deletes);
        if !pending_deletes.is_empty() {
            let batch = pending_deletes
                .iter()
                .cloned()
                .map(Observation::Deleted)
                .collect();
            if let Err(err) = persist(&store, &grant, batch, false).await.1 {
                if let Some((_, reply)) = barrier {
                    let _ = reply.send(Err(err));
                }
                if closed {
                    return;
                }
                if pending.len() == WRITER_CAPACITY {
                    // Don't let a failed batch prevent accepting a later barrier.
                    write_fault = Some(err);
                    pending.clear();
                }
                continue;
            }
            pending_deletes.clear();
        }
        if closed {
            return;
        }
        if let Some(err) = write_fault {
            if let Some((_, reply)) = barrier {
                let _ = reply.send(Err(err));
            }
            continue;
        }
        // A cancelled/stalled browser request needn't parse the store later.
        let read = !overloaded.load(Ordering::Acquire)
            && barrier
                .as_ref()
                .is_some_and(|(read, reply)| *read && !reply.is_closed());
        let result;
        (pending, result) = persist(&store, &grant, pending, read).await;
        match &result {
            Ok(_) => pending.clear(),
            // A mode fence or cookie deadline need not end local access if
            // the same grant switches to offline. Retain pending evidence.
            Err(err)
                if (*err == NOT_CURRENT || err.expired)
                    && pending.len() < WRITER_CAPACITY
                    && !overloaded.load(Ordering::Acquire) =>
            {
                ()
            }
            Err(err) if !pending.is_empty() => {
                // Report lost positive evidence for this grant; the worker
                // still handles priority deletes.
                write_fault = Some(*err);
                eprintln!("{err}");
                pending.clear();
            }
            Err(_) => (),
        }
        if let Some((_, reply)) = barrier {
            let _ = reply.send(result);
        }
    }
}

async fn persist(
    store: &Store,
    grant: &Arc<Grant>,
    mut pending: Vec<Observation>,
    read: bool,
) -> (Vec<Observation>, LoadResult) {
    if pending.is_empty() && !read {
        return (pending, Ok(None));
    }
    let mut result = Err(STORAGE);
    for ms in [0, 25, 75] {
        if ms > 0 {
            tokio::time::sleep(Duration::from_millis(ms)).await;
        }
        let (store, grant) = (store.clone(), grant.clone());
        let outcome = tokio::task::spawn_blocking(move || {
            let result = store.update(&grant, &pending, read);
            (pending, result)
        })
        .await;
        let Ok((batch, attempt)) = outcome else {
            return (Vec::new(), Err(STORAGE));
        };
        pending = batch;
        result = attempt;
        if !matches!(result, Err(STORAGE | NOT_DURABLE)) {
            break;
        }
    }
    (pending, result)
}

#[derive(Clone)]
enum Root {
    Default,
    #[cfg(test)]
    Fixture(std::path::PathBuf),
}

impl Root {
    fn open(&self, create: bool) -> Result<Option<OwnedFd>, ApiError> {
        let result = match self {
            Self::Default if create => storage::open_default(ROOT),
            Self::Default => storage::open_default_existing(ROOT),
            #[cfg(test)]
            Self::Fixture(path) => storage::open_private(path),
        };
        match result {
            Ok(root) => Ok(Some(root)),
            Err(Errno::NOENT) if !create => Ok(None),
            Err(_) => Err(STORAGE),
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Disk {
    version: u8,
    scope: String,
    playlists: ObservedPlaylists,
}

#[derive(Clone)]
struct Store {
    root: Root,
    scope: String,
}

impl Store {
    fn update(
        &self,
        grant: &Grant,
        pending: &[Observation],
        read: bool,
    ) -> Result<Option<ObservedPlaylists>, ApiError> {
        // A mode switch invalidates this publication attempt, not the already
        // accepted account-local evidence. Retry with a fresh local mode fence.
        for _ in 0..2 {
            let mode = grant.mode();
            let result = self.update_in_mode(grant, mode, pending, read);
            if grant.mode() == mode || result.is_ok() {
                return result;
            }
        }
        Err(NOT_CURRENT)
    }

    fn update_in_mode(
        &self,
        grant: &Grant,
        mode: u64,
        pending: &[Observation],
        read: bool,
    ) -> Result<Option<ObservedPlaylists>, ApiError> {
        grant.check(mode)?;
        if pending.is_empty() && !read {
            return Ok(None);
        }
        let Some(dir) = self.root.open(!pending.is_empty())? else {
            return Ok(read.then(ObservedPlaylists::default));
        };
        if !pending.is_empty() {
            fs::flock(&dir, FlockOperation::NonBlockingLockExclusive).map_err(|_| STORAGE)?;
            // A newly created storage root must itself survive a restart.
            let base = fs::openat(&dir, "..", DIR_FLAGS, Mode::empty()).map_err(|_| STORAGE)?;
            fs::fsync(&base).map_err(|_| STORAGE)?;
        }
        let name = format!("{}.json", self.scope);
        let mut disk = self.read(&dir, &name)?;
        let changed = pending.iter().fold(false, |changed, event| {
            disk.playlists.apply(event) | changed
        });
        if changed {
            if !disk.playlists.valid() {
                return Err(INVALID);
            }
            self.write(&dir, &name, &disk, grant, mode)?;
        } else if !pending.is_empty() {
            // This can be a retry after a visible rename whose sync failed.
            fs::fsync(&dir).map_err(|_| NOT_DURABLE)?;
        }
        Ok(read.then_some(disk.playlists))
    }

    fn read(&self, dir: &OwnedFd, name: &str) -> Result<Disk, ApiError> {
        let fd = match fs::openat(dir, name, FILE_FLAGS | OFlags::RDONLY, Mode::empty()) {
            Ok(fd) => fd,
            Err(Errno::NOENT) => {
                return Ok(Disk {
                    version: VERSION,
                    scope: self.scope.clone(),
                    playlists: ObservedPlaylists::default(),
                });
            }
            Err(Errno::LOOP) => return Err(INVALID),
            Err(_) => return Err(STORAGE),
        };
        let stat = fs::fstat(&fd).map_err(|_| STORAGE)?;
        if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile
            || stat.st_uid != geteuid().as_raw()
            || stat.st_mode & 0o7777 != 0o600
            || stat.st_nlink > 1
            || stat.st_size < 0
        {
            return Err(INVALID);
        }
        let mut file = BufReader::new(File::from(fd)).take(stat.st_size as u64);
        let disk: Disk = serde_json::from_reader(&mut file)
            .map_err(|err| if err.is_io() { STORAGE } else { INVALID })?;
        if file.limit() != 0
            || file.into_inner().read(&mut [0]).map_err(|_| STORAGE)? != 0
            || disk.version != VERSION
            || disk.scope != self.scope
            || !disk.playlists.valid()
        {
            return Err(INVALID);
        }
        Ok(disk)
    }

    fn write(
        &self,
        dir: &OwnedFd,
        name: &str,
        disk: &Disk,
        grant: &Grant,
        mode: u64,
    ) -> Result<(), ApiError> {
        // The exclusive writer lock covers cleanup of an interrupted stage.
        let stage_name = format!(".{}.partial", self.scope);
        match fs::unlinkat(dir, &stage_name, AtFlags::empty()) {
            Ok(()) | Err(Errno::NOENT) => (),
            Err(_) => return Err(STORAGE),
        }
        let fd = fs::openat(
            dir,
            &stage_name,
            FILE_FLAGS | OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL,
            PRIVATE_FILE,
        )
        .map_err(|_| STORAGE)?;
        let stage = Stage {
            dir,
            name: stage_name,
        };
        let mut writer = BufWriter::new(File::from(fd));
        serde_json::to_writer(&mut writer, disk).map_err(|_| STORAGE)?;
        writer.flush().map_err(|_| STORAGE)?;
        writer.get_ref().sync_all().map_err(|_| STORAGE)?;
        {
            let _publication = PUBLICATION_GATE.lock().map_err(|_| STORAGE)?;
            grant.check(mode)?;
            fs::renameat(dir, &stage.name, dir, name).map_err(|_| STORAGE)?;
        }
        fs::fsync(dir).map_err(|_| NOT_DURABLE)
    }
}

struct Stage<'a> {
    dir: &'a OwnedFd,
    name: String,
}
impl Drop for Stage<'_> {
    fn drop(&mut self) {
        let _ = fs::unlinkat(self.dir, &self.name, AtFlags::empty());
    }
}

fn scope_hash(origin: &str, account: &str) -> String {
    use std::fmt::Write as _;
    let mut hash = Context::new(&SHA256);
    hash.update(b"spofify-playlist-observations-v1\0");
    for field in [origin, account] {
        hash.update(&(field.len() as u64).to_be_bytes());
        hash.update(field.as_bytes());
    }
    let mut hex = String::with_capacity(64);
    for byte in hash.finish().as_ref() {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{api::Origin, personal_bridge::PersonalHub};
    use serde_json::{Value, json};
    use std::{fs as stdfs, os::unix::fs::DirBuilderExt, path::PathBuf};

    const ORIGIN: &str = "https://observations.invalid";
    const GENERATION: u64 = 3;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = stdfs::canonicalize(std::env::temp_dir())
                .unwrap()
                .join(format!("spofify-observation-test-{}", uuid::Uuid::new_v4()));
            stdfs::DirBuilder::new().mode(0o700).create(&path).unwrap();
            Self(path)
        }

        fn attach(
            &self,
            hub: &PersonalHub,
            origin: &str,
            epoch: Arc<AtomicU64>,
            offline: bool,
        ) -> Access {
            let api = Arc::new(Api::new(Origin::parse(origin).unwrap()).unwrap());
            let session = api.test_session();
            session.set_offline(offline);
            let access = Access {
                epoch: epoch.load(Ordering::SeqCst),
                api,
                session,
                offline,
                personal: hub.access(),
            };
            let writer = Writer::start_in(&access, epoch, Root::Fixture(self.0.clone())).unwrap();
            hub.set_observation_writer(access.epoch, writer);
            access
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            stdfs::remove_dir_all(&self.0).unwrap();
        }
    }

    fn stall(access: &Access, hub: &PersonalHub) -> mpsc::Receiver<Request> {
        let original = access.personal.playlist_observations(access).unwrap();
        let (tx, rx) = mpsc::channel(WRITER_CAPACITY);
        hub.set_observation_writer(
            access.epoch,
            Writer {
                tx,
                grant: original.grant,
                overloaded: Arc::default(),
                tombstones: Arc::default(),
            },
        );
        rx
    }

    fn summary(id: &str, count: usize, revision: u64, kind: &str) -> Value {
        json!({"id":id,"name":"Observed","description":"","kind":kind,"countState":"exact",
            "evaluationState": (kind == "derived").then_some("ready"),
            "trackCount":count,"availableCount":count,"revision":revision,
            "createdAt":"2026-01-01T00:00:00Z","updatedAt":"2026-01-01T00:00:00Z",
            "capabilities":{"editMetadata":true,"editEntries":true,"editDefinition":false,
                "play":true,"copyToManual":false,"delete":true,"addTracks":true}})
    }

    fn row(id: &str, position: u64) -> Value {
        json!({"id":id,"position":position,"track":{"kind":"track","id":"duplicate-song",
            "name":"Not cached","artists":[],"hasAudio":true,
            "coverage":{"tracks":1,"inLibrary":1,"albums":1}}})
    }

    fn part(summary: Value, offset: usize, rows: Vec<Value>, more: bool, token: &str) -> Value {
        let definition = if summary["kind"] == "derived" {
            json!({"version":1,"sources":[{"kind":"allSaved","slotId":"source"}],
                "duplicates":"keep","sort":"sourceOrder"})
        } else {
            Value::Null
        };
        json!({"part":{"summary":summary,"definition":definition,"offset":offset,
            "token":token,"rows":rows,"more":more}})
    }

    fn send(hub: &PersonalHub, access: &Access, value: Value) {
        hub.receive(
            access.epoch,
            GENERATION,
            serde_json::from_value(value).unwrap(),
        )
        .unwrap();
    }

    fn snapshot(hub: &PersonalHub, access: &Access, playlists: Vec<Value>) {
        send(
            hub,
            access,
            json!({"snapshot":{"playlists":playlists,"recent":[]}}),
        );
    }

    #[tokio::test]
    async fn accepted_deletions_survive_saturation_flush_and_relaunch() {
        let fixture = Fixture::new();
        let hub = PersonalHub::default();
        let epoch = Arc::new(AtomicU64::new(7));
        let mut access = fixture.attach(&hub, ORIGIN, epoch.clone(), false);
        hub.begin(access.epoch, GENERATION);
        snapshot(&hub, &access, vec![summary("manual", 0, 1, "manual")]);
        assert!(load(&access).await.unwrap().summary("manual").is_some());
        let paused = stall(&access, &hub);
        let writer = access.personal.playlist_observations(&access).unwrap();
        let mode = writer.online_mode().unwrap();
        let p: PlaylistSummary = serde_json::from_value(summary("other", 0, 1, "manual")).unwrap();
        for _ in 0..WRITER_CAPACITY {
            writer.observe(mode, AcceptedPlaylist::Summaries(std::slice::from_ref(&p)));
        }
        // Queue is precisely full: a deletion must not depend on admission
        // even before a positive observation sets the overloaded flag.
        assert_eq!(writer.tx.capacity(), 0);
        send(&hub, &access, json!({"change":{"deleted":"manual"}}));
        assert!(!writer.overloaded.load(Ordering::Acquire));
        writer.observe(mode, AcceptedPlaylist::Summaries(std::slice::from_ref(&p)));
        send(&hub, &access, json!({"change":{"deleted":"unknown"}}));
        send(&hub, &access, json!({"change":{"deleted":"unknown"}}));
        assert_eq!(writer.tombstones.ids.lock().unwrap().len(), 2);
        access.session.set_offline(true);
        access.offline = true;
        // The disk still has the earlier positive, but normal offline browse
        // must not return it (or use a saved fallback) after losing evidence.
        assert_eq!(
            crate::playlist_browser::offline_page(&access, &crate::pages::Route::Playlists)
                .await
                .err(),
            Some(OVERLOADED)
        );
        tokio::spawn(run(
            paused,
            Store {
                root: Root::Fixture(fixture.0.clone()),
                scope: scope_hash(ORIGIN, "synthetic"),
            },
            writer.grant.clone(),
            writer.overloaded.clone(),
            writer.tombstones.clone(),
        ));
        // Partial positive observations are still reported honestly, but the
        // awaited barrier durably publishes the independently queued deletes.
        assert_eq!(writer.flush().await, Err(OVERLOADED));
        epoch.store(8, Ordering::SeqCst);
        let next_hub = PersonalHub::default();
        let next = fixture.attach(&next_hub, ORIGIN, epoch, true);
        let reopened = load(&next).await.unwrap();
        assert!(reopened.is_deleted("manual") && reopened.is_deleted("unknown"));
        assert!(reopened.summary("manual").is_none());
    }

    #[tokio::test]
    async fn stalled_worker_has_real_offline_browse_and_flush_timeouts() {
        let fixture = Fixture::new();
        let hub = PersonalHub::default();
        let access = fixture.attach(&hub, ORIGIN, Arc::new(AtomicU64::new(7)), true);
        let _stalled = stall(&access, &hub);
        let writer = access.personal.playlist_observations(&access).unwrap();
        let within = |delay| delay + Duration::from_secs(1);
        assert_eq!(
            tokio::time::timeout(within(FLUSH_WAIT), writer.flush())
                .await
                .unwrap(),
            Err(BUSY)
        );
        let result = tokio::time::timeout(
            within(READ_WAIT),
            crate::playlist_browser::offline_page(&access, &crate::pages::Route::Playlists),
        )
        .await
        .unwrap();
        assert_eq!(result.err(), Some(BUSY));
    }

    #[tokio::test]
    async fn stores_only_completed_accepted_occurrences_and_preserves_last_good_revision() {
        let fixture = Fixture::new();
        let hub = PersonalHub::default();
        let access = fixture.attach(&hub, ORIGIN, Arc::new(AtomicU64::new(7)), false);
        hub.begin(access.epoch, GENERATION);
        let p = summary("manual", 2, 3, "manual");
        snapshot(&hub, &access, vec![p.clone()]);
        assert!(
            access
                .personal
                .watch_playlist(access.epoch, Some("manual".into()))
                .is_some()
        );
        send(
            &hub,
            &access,
            part(p.clone(), 0, vec![row("one", 4)], true, "not-for-storage"),
        );
        assert!(load(&access).await.unwrap().membership("manual").is_none());
        send(
            &hub,
            &access,
            part(p, 1, vec![row("two", 9)], false, "not-for-storage"),
        );
        let first = load(&access).await.unwrap();
        let m = first.membership("manual").unwrap();
        assert_eq!(m.revision, 3);
        assert_eq!(
            m.rows
                .iter()
                .map(|row| (
                    row.occurrence_id.as_str(),
                    row.position,
                    row.track_id.as_str()
                ))
                .collect::<Vec<_>>(),
            [("one", 4, "duplicate-song"), ("two", 9, "duplicate-song")]
        );

        let p4 = summary("manual", 2, 4, "manual");
        send(
            &hub,
            &access,
            json!({"change":{"playlist":p4,"entries":{"moved":"two","after":""}}}),
        );
        let moved = load(&access).await.unwrap();
        let m = moved.membership("manual").unwrap();
        assert_eq!(m.revision, 4);
        assert_eq!(
            (m.rows[0].occurrence_id.as_str(), m.rows[0].position),
            ("two", 4)
        );
        assert_eq!(
            (m.rows[1].occurrence_id.as_str(), m.rows[1].position),
            ("one", 9)
        );

        let p5 = summary("manual", 2, 5, "manual");
        send(
            &hub,
            &access,
            json!({"change":{"playlist":p5,"rowsInvalid":true}}),
        );
        send(
            &hub,
            &access,
            part(p5, 0, vec![row("replacement", 0)], true, "interrupted"),
        );
        let stale = load(&access).await.unwrap();
        assert_eq!(stale.summary("manual").unwrap().revision, 5);
        assert_eq!(stale.membership("manual"), Some(m));

        // Neither a wrong socket generation nor an invalid accepted-generation
        // frame supplies additional observations.
        let frame =
            json!({"snapshot":{"playlists":[summary("other", 0, 1, "manual")],"recent":[]}});
        hub.receive(
            access.epoch,
            GENERATION - 1,
            serde_json::from_value(frame).unwrap(),
        )
        .unwrap();
        let bad = summary("bad", 0, 1, "manual");
        let frame = json!({"snapshot":{"playlists":[bad.clone(),bad],"recent":[]}});
        assert!(
            hub.receive(
                access.epoch,
                GENERATION,
                serde_json::from_value(frame).unwrap()
            )
            .is_err()
        );
        let result = load(&access).await.unwrap();
        assert!(result.summary("other").is_none() && result.summary("bad").is_none());
        let file = fixture
            .0
            .join(format!("{}.json", scope_hash(ORIGIN, "synthetic")));
        let file = stdfs::read_to_string(file).unwrap();
        for secret_or_authority in [
            "csrf",
            "cookie",
            "capabilities",
            "editMetadata",
            "not-for-storage",
            "Not cached",
        ] {
            assert!(!file.contains(secret_or_authority));
        }
    }

    #[tokio::test]
    async fn preserves_a_large_unlisted_rules_detail_across_omission_mode_change_and_relaunch() {
        let fixture = Fixture::new();
        let hub = PersonalHub::default();
        let epoch = Arc::new(AtomicU64::new(7));
        let mut access = fixture.attach(&hub, ORIGIN, epoch.clone(), false);
        hub.begin(access.epoch, GENERATION);
        snapshot(&hub, &access, vec![]);
        assert!(
            access
                .personal
                .watch_playlist(access.epoch, Some("rules".into()))
                .is_some()
        );
        // A Rules materialization need not match the live summary count.
        let p = summary("rules", 2, 11, "derived");
        let total = 8_193;
        let rows = (0..total).map(|i| row(&format!("o1_{i}"), 3 * i)).collect();
        send(&hub, &access, part(p, 0, rows, false, "opaque"));
        snapshot(
            &hub,
            &access,
            (0..100)
                .map(|i| summary(&format!("other{i}"), 0, 1, "manual"))
                .collect(),
        );
        snapshot(&hub, &access, vec![]);
        access.session.set_offline(true);
        // The engine hasn't applied the new mode yet; any late online frame
        // belonging to the old socket/mode must not reach the observation store.
        send(&hub, &access, json!({"change":{"deleted":"rules"}}));
        assert!(load(&access).await.is_err());
        access.offline = true;
        let result = load(&access).await.unwrap();
        assert_eq!(result.summaries().count(), 101);
        assert!(!result.is_deleted("rules"));
        assert_eq!(
            result.membership("rules").unwrap().rows.len(),
            total as usize
        );

        epoch.store(8, Ordering::SeqCst);
        hub.reset(8);
        assert!(load(&access).await.is_err());
        let next_hub = PersonalHub::default();
        let next = fixture.attach(&next_hub, ORIGIN, epoch.clone(), true);
        let reopened = load(&next).await.unwrap();
        let m = reopened.membership("rules").unwrap();
        assert_eq!(m.revision, 11);
        assert_eq!(m.rows[8192].occurrence_id, "o1_8192");
        assert_eq!(m.rows[8192].position, 3 * 8192);
        assert_eq!(m.rows[8192].track_id, "duplicate-song");
        let foreign_hub = PersonalHub::default();
        let foreign = fixture.attach(&foreign_hub, "https://other.invalid", epoch, true);
        assert_eq!(load(&foreign).await.unwrap().summaries().count(), 0);
        let mut wrong_grant = next.clone();
        wrong_grant.session = next.api.test_session();
        wrong_grant.session.set_offline(true);
        assert!(load(&wrong_grant).await.is_err());
        assert_ne!(
            scope_hash(ORIGIN, "synthetic"),
            scope_hash(ORIGIN, "another-account")
        );
    }

    #[tokio::test]
    async fn rules_only_replaces_membership_on_a_ready_complete_detail() {
        let fixture = Fixture::new();
        let hub = PersonalHub::default();
        let epoch = Arc::new(AtomicU64::new(7));
        let access = fixture.attach(&hub, ORIGIN, epoch.clone(), false);
        hub.begin(access.epoch, GENERATION);
        let ready = summary("rules", 1, 5, "derived");
        let mut pending = ready.clone();
        pending["evaluationState"] = json!("pending");
        pending["countState"] = json!("pending");
        pending["trackCount"] = Value::Null;
        pending["availableCount"] = Value::Null;
        snapshot(&hub, &access, vec![pending.clone()]);
        access
            .personal
            .watch_playlist(access.epoch, Some("rules".into()));
        send(&hub, &access, part(pending, 0, vec![], false, "pending"));
        let observed = load(&access).await.unwrap();
        assert_eq!(
            observed.summary("rules").unwrap().evaluation_state,
            Some(EvaluationState::Pending)
        );
        assert!(observed.membership("rules").is_none());

        access.personal.refresh_playlist(access.epoch);
        send(
            &hub,
            &access,
            part(ready.clone(), 0, vec![row("o1_ready", 4)], false, "ready"),
        );
        let good = load(&access).await.unwrap();
        assert!(good.membership_is_current("rules"));
        let membership = good.membership("rules").unwrap();
        assert_eq!(membership.revision, 5);

        // Same-revision invalidation preserves rows but sticks even if a
        // snapshot knows the evaluation is ready before we pull its detail.
        send(
            &hub,
            &access,
            json!({"change":{"playlist":ready,"rowsInvalid":true}}),
        );
        let invalidated = load(&access).await.unwrap();
        assert_eq!(invalidated.membership("rules"), Some(membership));
        assert!(!invalidated.membership_is_current("rules"));
        let mut failed = ready.clone();
        failed["evaluationState"] = json!("error");
        send(&hub, &access, part(failed, 0, vec![], false, "failed"));
        let failed = load(&access).await.unwrap();
        assert_eq!(
            failed.summary("rules").unwrap().evaluation_state,
            Some(EvaluationState::Error)
        );
        assert_eq!(failed.membership("rules"), Some(membership));
        assert!(!failed.membership_is_current("rules"));
        snapshot(&hub, &access, vec![ready.clone()]);
        let summary_only = load(&access).await.unwrap();
        assert_eq!(
            summary_only.summary("rules").unwrap().evaluation_state,
            Some(EvaluationState::Ready)
        );
        assert_eq!(summary_only.membership("rules"), Some(membership));
        assert!(!summary_only.membership_is_current("rules"));

        let mut stale = summary("rules", 1, 6, "derived");
        stale["evaluationState"] = json!("stale");
        stale["countState"] = json!("stale");
        send(
            &hub,
            &access,
            part(stale, 0, vec![row("o1_not_current", 99)], false, "stale"),
        );
        let stale = load(&access).await.unwrap();
        assert_eq!(stale.summary("rules").unwrap().revision, 6);
        assert_eq!(
            stale.summary("rules").unwrap().evaluation_state,
            Some(EvaluationState::Stale)
        );
        assert_eq!(stale.membership("rules"), Some(membership));
        assert!(!stale.membership_is_current("rules"));

        epoch.store(8, Ordering::SeqCst);
        let next_hub = PersonalHub::default();
        let next = fixture.attach(&next_hub, ORIGIN, epoch, false);
        let reopened = load(&next).await.unwrap();
        assert_eq!(reopened.membership("rules"), Some(membership));
        assert!(!reopened.membership_is_current("rules"));
        next_hub.begin(next.epoch, GENERATION);
        snapshot(&next_hub, &next, vec![]);
        next.personal
            .watch_playlist(next.epoch, Some("rules".into()));
        let replacement = summary("rules", 1, 6, "derived");
        send(
            &next_hub,
            &next,
            part(replacement, 0, vec![row("o1_new", 12)], false, "new"),
        );
        let result = load(&next).await.unwrap();
        assert!(result.membership_is_current("rules"));
        assert_eq!(result.membership("rules").unwrap().revision, 6);
        assert_eq!(
            result.membership("rules").unwrap().rows[0].occurrence_id,
            "o1_new"
        );
    }

    #[tokio::test]
    async fn deletions_are_sticky_for_unknown_and_known_ids_across_delayed_writers() {
        let fixture = Fixture::new();
        let hub = PersonalHub::default();
        let epoch = Arc::new(AtomicU64::new(7));
        let access = fixture.attach(&hub, ORIGIN, epoch.clone(), false);
        hub.begin(access.epoch, GENERATION);
        let p = summary("manual", 0, 2, "manual");
        snapshot(&hub, &access, vec![p.clone()]);
        access
            .personal
            .watch_playlist(access.epoch, Some("manual".into()));
        send(&hub, &access, part(p.clone(), 0, vec![], false, "empty"));
        assert_eq!(
            load(&access)
                .await
                .unwrap()
                .membership("manual")
                .unwrap()
                .rows
                .len(),
            0
        );
        // An independent writer for the same principal/server may be delayed
        // with frames not seen by this process, and with a higher revision.
        let delayed_hub = PersonalHub::default();
        let delayed = fixture.attach(&delayed_hub, ORIGIN, Arc::new(AtomicU64::new(14)), false);
        delayed_hub.begin(delayed.epoch, GENERATION);
        snapshot(&delayed_hub, &delayed, vec![]);
        delayed
            .personal
            .watch_playlist(delayed.epoch, Some("manual".into()));

        send(&hub, &access, json!({"change":{"deleted":"manual"}}));
        send(&hub, &access, json!({"change":{"deleted":"not-observed"}}));
        snapshot(&hub, &access, vec![p]);
        let result = load(&access).await.unwrap();
        assert!(result.is_deleted("manual") && result.is_deleted("not-observed"));
        assert!(result.membership("manual").is_none());

        send(
            &delayed_hub,
            &delayed,
            part(
                summary("manual", 1, 999, "manual"),
                0,
                vec![row("delayed", 3)],
                false,
                "later",
            ),
        );
        snapshot(
            &delayed_hub,
            &delayed,
            vec![
                summary("not-observed", 1, 999, "manual"),
                summary("different-id", 1, 1, "manual"),
            ],
        );
        let result = load(&delayed).await.unwrap();
        assert!(result.is_deleted("manual") && result.is_deleted("not-observed"));
        assert!(result.summary("manual").is_none() && result.membership("manual").is_none());
        assert!(result.summary("not-observed").is_none());
        assert!(result.summary("different-id").is_some());

        epoch.store(8, Ordering::SeqCst);
        let next_hub = PersonalHub::default();
        let next = fixture.attach(&next_hub, ORIGIN, epoch, true);
        let reopened = load(&next).await.unwrap();
        assert!(reopened.is_deleted("manual") && reopened.is_deleted("not-observed"));
        assert_eq!(
            reopened
                .summaries()
                .map(|p| p.id.as_str())
                .collect::<Vec<_>>(),
            ["different-id"]
        );
    }
}
