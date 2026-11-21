//! Durable playlist intent and its account-scoped background worker. The store
//! owns intent tokens and committed membership; this actor only schedules work
//! and projects lightweight status for the desk.

use std::{
    collections::BTreeMap,
    future::pending,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use futures_util::future::BoxFuture;
use tokio::{
    sync::{Mutex, mpsc, oneshot, watch},
    time::{Instant, sleep_until},
};

use crate::{
    api::ApiError,
    collections,
    desk::Access,
    personal_bridge::Link,
    playlist_pin_store::{Membership, Record, Store, Target, fingerprint},
};

const PROGRESS_INTERVAL: Duration = Duration::from_millis(150);
const STALE: ApiError = error("The account or connection changed. Try again.");
const OFFLINE: ApiError = error("Go online to save this playlist.");
const UNAVAILABLE: ApiError = error("Saved playlists are unavailable. Try again.");

const fn error(message: &'static str) -> ApiError {
    ApiError {
        message,
        expired: false,
        status: 0,
    }
}

type AccessRx = watch::Receiver<Option<Access>>;

#[derive(Clone, Default)]
pub(crate) struct View {
    pub(crate) epoch: Option<u64>,
    pub(crate) revision: u64,
    pub(crate) records: BTreeMap<String, Status>,
}

#[derive(Clone)]
pub(crate) struct Status {
    pub(crate) target: Target,
    pub(crate) key: String,
    pub(crate) wanted: bool,
    pub(crate) has_committed: bool,
    pub(crate) phase: Phase,
    committed: Option<Fingerprint>,
    hint: Option<&'static str>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Phase {
    Pending,
    Downloading { verified: usize, total: usize },
    Saved,
    Retry,
}

impl Status {
    pub(crate) fn label(&self) -> String {
        match (self.phase, self.has_committed) {
            (_, _) if !self.wanted => "Not saved on this device".into(),
            (Phase::Saved, _) => "Saved on this device".into(),
            (Phase::Pending, true) => "Saved · Refresh pending".into(),
            (Phase::Retry, true) => format!("Saved · {}", self.hint.unwrap_or("Refresh pending")),
            (Phase::Downloading { total: 0, .. }, true) => "Saved · Refreshing".into(),
            (Phase::Downloading { verified, total }, true) => {
                format!("Saved · Refreshing {verified} of {total}")
            }
            (Phase::Pending, false) => "Waiting to download".into(),
            (Phase::Retry, false) => self.hint.unwrap_or("Waiting to retry").into(),
            (Phase::Downloading { total: 0, .. }, false) => "Downloading".into(),
            (Phase::Downloading { verified, total }, false) => {
                format!("Downloading · {verified} of {total}")
            }
        }
    }

    /// Detail represents committed audio on the control itself. Keep only
    /// actionable state information beside it; list-row copy stays unchanged.
    pub(crate) fn detail_feedback(&self) -> String {
        match (self.phase, self.has_committed) {
            (Phase::Saved, _) => String::new(),
            (Phase::Pending, true) => "Refresh pending".into(),
            (Phase::Pending, false) => "Waiting to download".into(),
            (Phase::Retry, committed) => self
                .hint
                .unwrap_or(if committed {
                    "Refresh pending"
                } else {
                    "Waiting to retry"
                })
                .into(),
            (Phase::Downloading { verified, total }, committed) => {
                let verb = if committed {
                    "Refreshing"
                } else {
                    "Downloading"
                };
                if total == 0 {
                    verb.into()
                } else {
                    format!("{verb} {verified} of {total}")
                }
            }
        }
    }

    fn same_row(&self, other: &Self) -> bool {
        self.target == other.target
            && self.key == other.key
            && self.wanted == other.wanted
            && self.committed == other.committed
    }
}

// ApiError retains only static local messages, never URLs, response bodies
// or transport errors. Authentication results belong to the session UI.
fn retry_hint(error: ApiError) -> Option<&'static str> {
    (!error.expired && !matches!(error.status, 401 | 403)).then_some(error.message)
}

#[derive(Clone)]
pub(crate) struct Controller {
    access: AccessRx,
    epoch: Arc<AtomicU64>,
    requests: mpsc::Sender<Set>,
}

impl Controller {
    pub(crate) async fn set(
        &self,
        access: &Access,
        target: Target,
        wanted: bool,
    ) -> Result<(), ApiError> {
        let scope = Scope::capture(access, &self.epoch)?;
        scope.check_set(&self.access, wanted)?;
        let (reply, result) = oneshot::channel();
        self.requests
            .send(Set {
                scope,
                target,
                wanted,
                reply,
            })
            .await
            .map_err(|_| UNAVAILABLE)?;
        result.await.map_err(|_| UNAVAILABLE)?
    }
}

pub(crate) fn start(
    access: AccessRx,
    epoch: Arc<AtomicU64>,
) -> (Controller, watch::Receiver<View>) {
    start_with(
        access,
        epoch,
        Arc::new(Store::open_default),
        Arc::new(transfer),
    )
}

type Open = Arc<dyn Fn(&Access) -> Result<Store, ApiError> + Send + Sync>;
type Live = Arc<dyn Fn() -> Result<(), ApiError> + Send + Sync>;
type Progress = (usize, usize);
type Transfer = Arc<
    dyn Fn(
            Access,
            Target,
            watch::Sender<Progress>,
            Live,
        ) -> BoxFuture<'static, Result<Membership, ApiError>>
        + Send
        + Sync,
>;

fn transfer(
    access: Access,
    target: Target,
    report: watch::Sender<Progress>,
    live: Live,
) -> BoxFuture<'static, Result<Membership, ApiError>> {
    Box::pin(async move {
        collections::complete_playlist(
            &access,
            &target,
            |verified, total| {
                report.send_replace((verified, total));
            },
            || live(),
        )
        .await
    })
}

fn start_with(
    access: AccessRx,
    epoch: Arc<AtomicU64>,
    open: Open,
    transfer: Transfer,
) -> (Controller, watch::Receiver<View>) {
    let (requests, incoming) = mpsc::channel(8);
    let (view, updates) = watch::channel(View::default());
    tokio::spawn(run(
        access.clone(),
        epoch.clone(),
        incoming,
        view,
        Disk::new(open),
        transfer,
    ));
    (
        Controller {
            access,
            epoch,
            requests,
        },
        updates,
    )
}

#[derive(Clone)]
struct Scope {
    access: Access,
    epoch: Arc<AtomicU64>,
    network: watch::Receiver<u64>,
    generation: u64,
}

impl Scope {
    fn capture(access: &Access, epoch: &Arc<AtomicU64>) -> Result<Self, ApiError> {
        if epoch.load(Ordering::SeqCst) != access.epoch {
            return Err(STALE);
        }
        access.api.authenticated_account(&access.session)?;
        let network = access.session.network_changes();
        let generation = *network.borrow();
        Ok(Self {
            access: access.clone(),
            epoch: epoch.clone(),
            network,
            generation,
        })
    }

    fn same_account(&self, other: &Self) -> bool {
        self.access.epoch == other.access.epoch
            && Arc::ptr_eq(&self.access.api, &other.access.api)
            && self.network.same_channel(&other.network)
    }

    fn same(&self, other: &Self) -> bool {
        self.same_account(other)
            && self.generation == other.generation
            && self.access.offline == other.access.offline
    }

    fn offline(&self) -> bool {
        self.access.offline || self.generation & 1 != 0
    }

    fn current_account(&self, rx: &AccessRx) -> Result<Self, ApiError> {
        // Logout advances the shared epoch before the engine republishes Access.
        if self.epoch.load(Ordering::SeqCst) != self.access.epoch || rx.has_changed().is_err() {
            return Err(STALE);
        }
        let current = rx.borrow();
        let other = Self::capture(current.as_ref().ok_or(STALE)?, &self.epoch)?;
        if !self.same_account(&other) || self.epoch.load(Ordering::SeqCst) != self.access.epoch {
            return Err(STALE);
        }
        Ok(other)
    }

    fn check(&self, rx: &AccessRx) -> Result<(), ApiError> {
        self.same(&self.current_account(rx)?)
            .then_some(())
            .ok_or(STALE)
    }

    fn check_set(&self, rx: &AccessRx, wanted: bool) -> Result<(), ApiError> {
        if wanted {
            self.check(rx)?;
            if self.offline() {
                return Err(OFFLINE);
            }
        } else {
            // Unpin is a local mutation, valid through a mode transition.
            self.current_account(rx)?;
        }
        Ok(())
    }
}

struct Set {
    scope: Scope,
    target: Target,
    wanted: bool,
    reply: oneshot::Sender<Result<(), ApiError>>,
}

#[derive(Clone)]
struct Disk {
    gate: Arc<Mutex<()>>,
    open: Open,
}

impl Disk {
    fn new(open: Open) -> Self {
        Self {
            gate: Arc::default(),
            open,
        }
    }

    async fn run<T: Send + 'static>(
        self,
        f: impl FnOnce(&dyn Fn(&Access) -> Result<Store, ApiError>) -> Result<T, ApiError>
        + Send
        + 'static,
    ) -> Result<T, ApiError> {
        let gate = self.gate.lock_owned().await;
        tokio::task::spawn_blocking(move || {
            // A detached blocking syscall may finish after cancellation. Hold
            // the gate in this closure, not the dropped async waiter.
            let _gate = gate;
            f(&*self.open)
        })
        .await
        .map_err(|_| UNAVAILABLE)?
    }
}

#[derive(Default)]
struct Backoff {
    failures: u8,
    due: Option<Instant>,
}

impl Backoff {
    fn fail(&mut self, now: Instant) {
        let delay = (2 << self.failures.min(8)).min(300);
        self.failures = self.failures.saturating_add(1);
        self.due = Some(now + Duration::from_secs(delay));
    }

    fn ready(&self, now: Instant) -> bool {
        self.due.is_none_or(|due| due <= now)
    }
}

type Fingerprint = [u8; 32];

// Discard full memberships on the blocking worker before updating the actor.
struct Stored {
    intent: Record,
    committed: Option<Fingerprint>,
}

impl From<Record> for Stored {
    fn from(mut intent: Record) -> Self {
        let committed = intent.committed.take().as_ref().map(fingerprint);
        Self { intent, committed }
    }
}

struct Entry {
    status: Status,
    intent: Arc<Record>,
    backoff: Backoff,
    turn: u64,
}

impl Entry {
    fn scheduled(&self) -> bool {
        self.status.wanted && self.status.phase != Phase::Saved
    }
}

#[derive(Default)]
struct Rows {
    entries: BTreeMap<String, Entry>,
    turns: u64,
}

impl Rows {
    fn requeue(&mut self, key: &str) {
        self.turns += 1;
        self.entries.get_mut(key).unwrap().turn = self.turns;
    }

    fn ready(&self, now: Instant) -> Option<String> {
        self.entries
            .iter()
            .filter(|(_, e)| e.scheduled() && e.backoff.ready(now))
            .min_by_key(|(_, e)| e.turn)
            .map(|(key, _)| key.clone())
    }

    fn due(&self) -> Option<Instant> {
        self.entries
            .values()
            .filter(|e| e.scheduled())
            .filter_map(|e| e.backoff.due)
            .min()
    }

    fn reset_retries(&mut self, active: Option<&str>, refresh_saved: bool) {
        for (key, entry) in &mut self.entries {
            if active == Some(key) {
                continue;
            }
            entry.backoff = Backoff::default();
            if entry.scheduled() || (refresh_saved && entry.status.wanted) {
                entry.status.phase = Phase::Pending;
                entry.status.hint = None;
            }
        }
    }

    fn replace(&mut self, records: Vec<Stored>) {
        let mut old = std::mem::take(&mut self.entries);
        for record in records {
            let key = &record.intent.key;
            if let Some(previous) = old.remove(key) {
                self.entries.insert(key.clone(), previous);
            }
            self.put(record);
        }
    }

    fn put(&mut self, Stored { intent, committed }: Stored) {
        let has_committed = committed.is_some();
        let key = intent.key.clone();
        let status = Status {
            target: intent.target.clone(),
            key: key.clone(),
            wanted: intent.wanted,
            has_committed,
            phase: if has_committed {
                Phase::Saved
            } else {
                Phase::Pending
            },
            committed,
            hint: None,
        };
        if let Some(old) = self.entries.get_mut(&key)
            && old.intent.token == intent.token
            && old.status.committed == committed
        {
            // Idempotent Set and local reloads do not restart the same job.
            old.status = Status {
                phase: old.status.phase,
                hint: old.status.hint,
                ..status
            };
            return;
        }
        self.entries.insert(
            key.clone(),
            Entry {
                status,
                intent: Arc::new(intent),
                backoff: Backoff::default(),
                turn: 0,
            },
        );
        self.requeue(&key);
    }

    fn project(&self, epoch: Option<u64>, tx: &watch::Sender<View>) {
        tx.send_if_modified(|view| {
            let mut durable = view.epoch != epoch || view.records.len() != self.entries.len();
            let mut phase = false;
            view.epoch = epoch;
            view.records.retain(|key, _| self.entries.contains_key(key));
            for (key, entry) in &self.entries {
                let same_row = view
                    .records
                    .get(key)
                    .is_some_and(|s| s.same_row(&entry.status));
                if !same_row {
                    durable = true;
                    view.records.insert(key.clone(), entry.status.clone());
                } else if let Some(old) = view.records.get_mut(key)
                    && (old.phase != entry.status.phase || old.hint != entry.status.hint)
                {
                    phase = true;
                    old.phase = entry.status.phase;
                    old.hint = entry.status.hint;
                }
            }
            if durable {
                view.revision = view.revision.wrapping_add(1);
            }
            durable || phase
        });
    }
}

enum AttemptResult {
    Saved {
        committed: Fingerprint,
        token: String,
    },
    Reload, // Intent or committed snapshot changed outside this attempt.
    Retry {
        error: ApiError,
        reload: bool,
    },
}

struct Attempt {
    job: Arc<Record>,
    cancelled: Arc<AtomicBool>,
    progress: watch::Receiver<Progress>,
    sample: Instant,
    run: BoxFuture<'static, AttemptResult>,
}

impl Attempt {
    fn start(
        scope: Scope,
        access: AccessRx,
        disk: Disk,
        transfer: Transfer,
        job: Arc<Record>,
        expected_committed: Option<Fingerprint>,
    ) -> Self {
        let (report, progress) = watch::channel((0, 0));
        let cancelled = Arc::new(AtomicBool::new(false));
        let work = job.clone();
        let token = cancelled.clone();
        let run = Box::pin(async move {
            let online = scope.access.clone();
            let live: Live = Arc::new(move || {
                if token.load(Ordering::Acquire) {
                    return Err(STALE);
                }
                scope.check_set(&access, true)
            });
            if let Err(error) = live() {
                return AttemptResult::Retry {
                    error,
                    reload: false,
                };
            }
            let membership =
                match transfer(online.clone(), work.target.clone(), report, live.clone()).await {
                    Ok(membership) => membership,
                    Err(error) => {
                        return AttemptResult::Retry {
                            error,
                            reload: false,
                        };
                    }
                };
            match disk
                .run(move |open| {
                    live()?;
                    let store = open(&online)?;
                    let committed = fingerprint(&membership);
                    let _audio_guard = collections::lock_playlist_members(&online, &membership)?;
                    live()?;
                    Ok(store
                        .publish(&online, &work, expected_committed, membership, || live())?
                        .map(|token| (committed, token)))
                })
                .await
            {
                Ok(Some((committed, token))) => AttemptResult::Saved { committed, token },
                Ok(None) => AttemptResult::Reload,
                Err(error) => AttemptResult::Retry {
                    error,
                    reload: true,
                },
            }
        });
        Self {
            job,
            cancelled,
            progress,
            sample: Instant::now() + PROGRESS_INTERVAL,
            run,
        }
    }
}

impl Drop for Attempt {
    fn drop(&mut self) {
        // Fence already-running blocking work, then drop the network future.
        self.cancelled.store(true, Ordering::Release);
    }
}

enum StorageResult {
    Listed(Scope, Result<Vec<Stored>, ApiError>),
    Set {
        scope: Scope,
        before: Option<String>,
        result: Result<Stored, ApiError>,
        reply: oneshot::Sender<Result<(), ApiError>>,
    },
}

enum Event {
    Access,
    Reconnect(u64),
    Command(Option<Set>),
    Storage(StorageResult),
    Attempt(AttemptResult),
    Sample,
    Wake,
}

async fn wait_until(at: Option<Instant>) {
    match at {
        Some(at) => sleep_until(at).await,
        None => pending().await,
    }
}

async fn poll<T>(run: Option<&mut BoxFuture<'static, T>>) -> T {
    match run {
        Some(run) => run.await,
        None => pending().await,
    }
}

async fn network_changed(scope: Option<&Scope>) {
    if let Some(scope) = scope {
        let mut rx = scope.network.clone();
        let _ = rx.wait_for(|n| *n != scope.generation).await;
    } else {
        pending().await
    }
}

async fn reconnected(scope: Option<&Scope>, last: Option<u64>) -> u64 {
    let Some(scope) = scope.filter(|scope| !scope.offline()) else {
        return pending().await;
    };
    let personal = &scope.access.personal;
    let mut changes = personal.changes();
    loop {
        let generation = personal.read(|status, _| {
            (status.epoch == scope.access.epoch && status.link == Link::Ready)
                .then_some(status.generation)
        });
        if let Some(generation) = generation.filter(|generation| Some(*generation) != last) {
            return generation;
        }
        if changes.changed().await.is_err() {
            return pending().await;
        }
    }
}

async fn run(
    mut access: AccessRx,
    epoch: Arc<AtomicU64>,
    mut commands: mpsc::Receiver<Set>,
    view: watch::Sender<View>,
    disk: Disk,
    transfer: Transfer,
) {
    let mut scope: Option<Scope> = None;
    let mut rows = Rows::default();
    let mut attempt: Option<Attempt> = None;
    let mut storage: Option<BoxFuture<'static, StorageResult>> = None;
    let mut load = false;
    let mut load_backoff = Backoff::default();
    let mut connection = None;
    let mut refresh_on_ready = false;
    let mut receive = true;

    loop {
        let next = access
            .borrow_and_update()
            .as_ref()
            .and_then(|a| Scope::capture(a, &epoch).ok());
        if access.has_changed().is_err() {
            break;
        }
        let same = scope.as_ref().zip(next.as_ref());
        if !same.is_some_and(|(a, b)| a.same(b)) && (scope.is_some() || next.is_some()) {
            attempt = None;
            if same.is_some_and(|(a, b)| a.same_account(b)) {
                rows.reset_retries(None, false);
                refresh_on_ready = next.as_ref().is_some_and(|s| !s.offline());
            } else {
                rows = Rows::default();
                refresh_on_ready = false;
            }
            scope = next;
            load = scope.is_some();
            load_backoff = Backoff::default();
            connection = None;
        }
        let now = Instant::now();
        let mut wake = None;
        if storage.is_none()
            && attempt.is_none()
            && let Some(scope) = &scope
        {
            if load && load_backoff.ready(now) {
                let (disk, check, captured) = (disk.clone(), access.clone(), scope.clone());
                storage = Some(Box::pin(async move {
                    let operation_scope = captured.clone();
                    let result = disk
                        .run(move |open| {
                            operation_scope.check(&check)?;
                            let access = &operation_scope.access;
                            // Corrupt records are not scheduled or restored from
                            // a cached row; valid records remain independent.
                            let (good, _) = open(access)?.scan(access)?;
                            Ok(good.into_iter().map(Stored::from).collect())
                        })
                        .await;
                    StorageResult::Listed(captured, result)
                }));
            } else {
                // A global scan error backs off the scan, not known-good jobs.
                // Publish still verifies the on-disk token before committing.
                wake = load_backoff.due.filter(|_| load);
                if !scope.offline() {
                    if let Some(key) = rows.ready(now) {
                        rows.requeue(&key);
                        let entry = rows.entries.get_mut(&key).unwrap();
                        entry.status.phase = Phase::Downloading {
                            verified: 0,
                            total: 0,
                        };
                        entry.status.hint = None;
                        attempt = Some(Attempt::start(
                            scope.clone(),
                            access.clone(),
                            disk.clone(),
                            transfer.clone(),
                            entry.intent.clone(),
                            entry.status.committed,
                        ));
                    } else {
                        wake = wake.into_iter().chain(rows.due()).min();
                    }
                }
            }
        }
        rows.project(scope.as_ref().map(|s| s.access.epoch), &view);

        let sample = attempt.as_ref().map(|work| work.sample);
        let event = tokio::select! {
            biased;
            _ = access.changed() => Event::Access,
            _ = network_changed(scope.as_ref()) => Event::Access,
            cmd = commands.recv(), if storage.is_none() && receive => Event::Command(cmd),
            result = poll(storage.as_mut()) => Event::Storage(result),
            generation = reconnected(scope.as_ref(), connection) => Event::Reconnect(generation),
            _ = wait_until(sample) => Event::Sample,
            result = poll(attempt.as_mut().map(|work| &mut work.run)) => Event::Attempt(result),
            _ = wait_until(wake) => Event::Wake,
        };
        let now = Instant::now();
        match event {
            Event::Access | Event::Wake => {}
            Event::Reconnect(generation) => {
                // Wake pending HTTP work on the first ready snapshot too,
                // but refresh saved pins only after a clean reconnect.
                let refresh = std::mem::take(&mut refresh_on_ready) || connection.is_some();
                connection = Some(generation);
                rows.reset_retries(attempt.as_ref().map(|work| work.job.key.as_str()), refresh);
                load_backoff = Backoff::default();
            }
            Event::Command(None) => receive = false,
            Event::Command(Some(Set {
                scope: captured,
                target,
                wanted,
                reply,
            })) => {
                if let Err(err) = captured.check_set(&access, wanted) {
                    let _ = reply.send(Err(err));
                    continue;
                }
                let key = match collections::playlist_key(&captured.access, &target.id) {
                    Ok(key) => key,
                    Err(err) => {
                        let _ = reply.send(Err(err));
                        continue;
                    }
                };
                if !wanted && attempt.as_ref().is_some_and(|a| a.job.key == key) {
                    let work = attempt.take().unwrap();
                    if let Some(entry) = rows.entries.get_mut(&work.job.key) {
                        entry.status.phase = Phase::Pending;
                    }
                    drop(work);
                }
                let before = rows.entries.get(&key).map(|e| e.intent.token.clone());
                let (disk, check) = (disk.clone(), access.clone());
                storage = Some(Box::pin(async move {
                    let operation_scope = captured.clone();
                    let result = disk
                        .run(move |open| {
                            let live = || operation_scope.check_set(&check, wanted);
                            live()?;
                            let store = open(&operation_scope.access)?;
                            store
                                .set(&operation_scope.access, target, wanted, live)
                                .map(Stored::from)
                        })
                        .await;
                    StorageResult::Set {
                        scope: captured,
                        before,
                        result,
                        reply,
                    }
                }));
            }
            Event::Storage(StorageResult::Listed(captured, result)) => {
                storage = None;
                if captured.check(&access).is_ok() {
                    match result {
                        Ok(records) => {
                            rows.replace(records);
                            load = false;
                            load_backoff = Backoff::default();
                        }
                        Err(_) => load_backoff.fail(now),
                    }
                }
            }
            Event::Storage(StorageResult::Set {
                scope: captured,
                before,
                result,
                reply,
            }) => {
                storage = None;
                if captured.current_account(&access).is_ok() {
                    match &result {
                        Ok(stored) => {
                            let record = &stored.intent;
                            if attempt.as_ref().is_some_and(|work| {
                                work.job.key == record.key
                                    && (work.job.token != record.token || !record.wanted)
                            }) {
                                attempt = None;
                            }
                        }
                        // A storage error can be after the atomic rename.
                        Err(_) => load = true,
                    }
                }
                let _ = reply.send(result.map(|stored| {
                    if captured.current_account(&access).is_ok() {
                        // An idempotent Set can return the old token while
                        // an already-submitted publication is completing.
                        let superseded = rows.entries.get(&stored.intent.key).is_some_and(|e| {
                            before.as_ref() == Some(&stored.intent.token)
                                && e.intent.token != stored.intent.token
                        });
                        if !superseded {
                            rows.put(stored);
                            rows.project(Some(captured.access.epoch), &view);
                        }
                    }
                }));
            }
            Event::Attempt(result) => {
                let work = attempt.take().unwrap();
                if scope
                    .as_ref()
                    .is_some_and(|s| s.check_set(&access, true).is_ok())
                    && let Some(entry) = rows.entries.get_mut(&work.job.key)
                    && Arc::ptr_eq(&entry.intent, &work.job)
                {
                    match result {
                        AttemptResult::Saved { committed, token } => {
                            Arc::make_mut(&mut entry.intent).token = token;
                            entry.status.committed = Some(committed);
                            entry.status.has_committed = true;
                            entry.status.phase = Phase::Saved;
                            entry.backoff = Backoff::default();
                        }
                        AttemptResult::Reload => {
                            entry.status.phase = Phase::Pending;
                            entry.backoff.fail(now);
                            load = true;
                        }
                        AttemptResult::Retry { error, reload } => {
                            entry.status.phase = Phase::Retry;
                            entry.status.hint = retry_hint(error);
                            entry.backoff.fail(now);
                            load |= reload;
                        }
                    }
                }
            }
            Event::Sample => {
                let work = attempt.as_mut().unwrap();
                work.sample = now + PROGRESS_INTERVAL;
                if scope
                    .as_ref()
                    .is_some_and(|s| s.check_set(&access, true).is_ok())
                    && let Some(entry) = rows.entries.get_mut(&work.job.key)
                    && Arc::ptr_eq(&entry.intent, &work.job)
                {
                    let (verified, total) = *work.progress.borrow_and_update();
                    entry.status.phase = Phase::Downloading { verified, total };
                }
            }
        }
    }
    drop(attempt);
    Rows::default().project(None, &view);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{Api, Origin, library::PlaylistKind};
    use std::{fs, os::unix::fs::DirBuilderExt, path::PathBuf};
    use tokio::time::timeout;

    fn target(id: &str) -> Target {
        Target {
            id: id.into(),
            title: "Playlist".into(),
            kind: PlaylistKind::Manual,
        }
    }

    fn pending_record(key: &str) -> Record {
        Record {
            key: key.into(),
            target: target(key),
            token: "synthetic-token".into(),
            wanted: true,
            committed: None,
        }
    }

    #[test]
    fn failures_do_not_starve_later_pins_and_reconnect_wakes_retries() {
        let now = Instant::now();
        let mut rows = Rows::default();
        rows.put(pending_record("a").into());
        rows.put(pending_record("b").into());
        assert_eq!(rows.ready(now).as_deref(), Some("a"));
        rows.requeue("a");
        let a = rows.entries.get_mut("a").unwrap();
        a.backoff.fail(now);
        a.status.phase = Phase::Retry;
        assert_eq!(rows.ready(now).as_deref(), Some("b"));
        rows.entries.get_mut("b").unwrap().status.phase = Phase::Saved;
        assert_eq!(rows.ready(now), None);
        assert_eq!(rows.due(), Some(now + Duration::from_secs(2)));
        rows.reset_retries(None, false);
        assert_eq!(rows.ready(now).as_deref(), Some("a"));
        assert_eq!(rows.entries["b"].status.phase, Phase::Saved);
    }

    #[test]
    fn detail_feedback_tracks_typed_phase_without_repeating_saved_state() {
        let mut rows = Rows::default();
        rows.put(pending_record("a").into());
        let status = &mut rows.entries.get_mut("a").unwrap().status;
        assert_eq!(status.detail_feedback(), "Waiting to download");
        status.phase = Phase::Downloading {
            verified: 2,
            total: 4,
        };
        assert_eq!(status.detail_feedback(), "Downloading 2 of 4");
        status.phase = Phase::Retry;
        status.hint = Some("The library is temporarily unavailable.");
        assert_eq!(
            status.detail_feedback(),
            "The library is temporarily unavailable."
        );
        status.has_committed = true;
        status.phase = Phase::Saved;
        assert_eq!(status.detail_feedback(), "");
        assert_eq!(status.label(), "Saved on this device");
        status.phase = Phase::Pending;
        assert_eq!(status.detail_feedback(), "Refresh pending");
        status.phase = Phase::Downloading {
            verified: 0,
            total: 0,
        };
        assert_eq!(status.detail_feedback(), "Refreshing");
        status.phase = Phase::Downloading {
            verified: 2,
            total: 4,
        };
        assert_eq!(status.detail_feedback(), "Refreshing 2 of 4");
    }

    #[test]
    fn progress_and_refresh_keep_revision_and_playable_snapshot() {
        let mut record = pending_record("a");
        let mut rows = Rows::default();
        let (tx, view) = watch::channel(View::default());
        let membership = |track: &str| Membership {
            title: "Saved".into(),
            tracks: vec![track.into()],
        };
        record.committed = Some(membership("one"));
        rows.put(record.clone().into());
        rows.project(Some(3), &tx);
        let revision = view.borrow().revision;
        assert!(rows.ready(Instant::now()).is_none());
        rows.reset_retries(None, true);
        rows.project(Some(3), &tx);
        assert_eq!(
            view.borrow().records["a"].label(),
            "Saved · Refresh pending"
        );
        let a = rows.entries.get_mut("a").unwrap();
        a.status.phase = Phase::Downloading {
            verified: 2,
            total: 4,
        };
        rows.project(Some(3), &tx);
        assert_eq!(view.borrow().revision, revision);
        let a = rows.entries.get_mut("a").unwrap();
        a.status.phase = Phase::Retry;
        a.status.hint = retry_hint(error("A track is not saved on this device."));
        rows.project(Some(3), &tx);
        assert_eq!(
            view.borrow().records["a"].label(),
            "Saved · A track is not saved on this device."
        );
        assert!(view.borrow().records["a"].has_committed);
        assert_eq!(view.borrow().revision, revision);
        record.token = "rotated-token".into();
        rows.put(record.clone().into());
        rows.project(Some(3), &tx);
        assert_eq!(view.borrow().revision, revision);
        assert_eq!(view.borrow().records["a"].phase, Phase::Saved);
        record.committed = Some(membership("two"));
        rows.put(record.into());
        rows.project(Some(3), &tx);
        assert_eq!(view.borrow().revision, revision + 1);
        assert_eq!(view.borrow().records["a"].phase, Phase::Saved);
    }

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = fs::canonicalize(std::env::temp_dir())
                .unwrap()
                .join(format!("spofify-pin-worker-test-{}", uuid::Uuid::new_v4()));
            fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[tokio::test]
    async fn stale_refresh_cannot_replace_newer_membership_for_the_same_intent() {
        let root = Fixture::new();
        let api = Arc::new(Api::new(Origin::parse("https://music.invalid").unwrap()).unwrap());
        let online = Access {
            epoch: 8,
            session: api.test_session(),
            api,
            offline: false,
            personal: Default::default(),
        };
        let path = root.0.clone();
        let open: Open = Arc::new(move |access| Store::open(&path, access));
        let store = open(&online).unwrap();
        let pin = store
            .set(&online, target("refresh"), true, || Ok(()))
            .unwrap();
        // An empty synthetic snapshot never opens the installed audio store.
        let snapshot = |title: &str| Membership {
            title: title.into(),
            tracks: vec![],
        };
        let token = store
            .publish(&online, &pin, None, snapshot("Previous"), || Ok(()))
            .unwrap()
            .unwrap();
        let captured = Stored::from(store.get(&online, &pin.key).unwrap().unwrap());
        assert_eq!(captured.intent.token, token);
        assert!(captured.intent.committed.is_none());
        let basis = captured.intent.clone();
        assert_eq!(captured.committed, Some(fingerprint(&snapshot("Previous"))));

        let epoch = Arc::new(AtomicU64::new(online.epoch));
        let (_access, rx) = watch::channel(Some(online.clone()));
        let fake: Transfer =
            Arc::new(move |_, _, _, _| Box::pin(async move { Ok(snapshot("Stale candidate")) }));
        let mut attempt = Attempt::start(
            Scope::capture(&online, &epoch).unwrap(),
            rx,
            Disk::new(open),
            fake,
            Arc::new(captured.intent),
            captured.committed,
        );
        // Another process publishes identical content. The fingerprint is
        // unchanged, but the rotated token must still reject the stale attempt.
        let newer_token = store
            .publish(
                &online,
                &basis,
                captured.committed,
                snapshot("Previous"),
                || Ok(()),
            )
            .unwrap()
            .unwrap();
        assert_ne!(newer_token, token);
        let result = timeout(Duration::from_secs(5), &mut attempt.run)
            .await
            .unwrap();
        assert!(matches!(result, AttemptResult::Reload));
        let current = store.get(&online, &pin.key).unwrap().unwrap();
        assert_eq!(current.token, newer_token);
        assert_eq!(current.committed, Some(snapshot("Previous")));
    }

    struct NotifyDrop(Option<oneshot::Sender<()>>);
    impl Drop for NotifyDrop {
        fn drop(&mut self) {
            let _ = self.0.take().unwrap().send(());
        }
    }

    #[tokio::test]
    async fn persisted_intent_resumes_and_offline_or_unpin_drop_active_transfer() {
        let root = Fixture::new();
        timeout(Duration::from_secs(5), async {
            let api = Arc::new(Api::new(Origin::parse("https://music.invalid").unwrap()).unwrap());
            let online = Access {
                epoch: 7,
                session: api.test_session(),
                api,
                offline: false,
                personal: Default::default(),
            };
            let epoch = Arc::new(AtomicU64::new(online.epoch));
            let (access, rx) = watch::channel(Some(online.clone()));
            let path = root.0.clone();
            let open: Open = Arc::new(move |access| Store::open(&path, access));
            let store = open(&online).unwrap();
            let original = store
                .set(&online, target("original"), true, || Ok(()))
                .unwrap();
            let damaged = store
                .set(&online, target("damaged"), true, || Ok(()))
                .unwrap();
            let dir = fs::read_dir(&root.0)
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path();
            fs::write(dir.join(format!("{}.json", damaged.key)), b"{broken").unwrap();
            let (started, mut attempts) = mpsc::channel(4);
            let fake: Transfer = Arc::new(move |_, _, _, live| {
                let started = started.clone();
                Box::pin(async move {
                    let (done, dropped) = oneshot::channel();
                    let _notify = NotifyDrop(Some(done));
                    assert!(started.try_send((live, dropped)).is_ok());
                    pending().await
                })
            });
            let (controller, mut view) = start_with(rx, epoch.clone(), open, fake);
            let (first_live, first_dropped) = attempts.recv().await.unwrap();
            assert!(first_live().is_ok());
            assert!(view.borrow().records.contains_key(&original.key));
            assert!(!view.borrow().records.contains_key(&damaged.key));

            online.session.set_offline(true);
            let offline = Access {
                offline: true,
                ..online.clone()
            };
            access.send_replace(Some(offline.clone()));
            first_dropped.await.unwrap();
            assert!(first_live().is_err());
            assert_eq!(
                controller.set(&offline, target("other"), true).await,
                Err(OFFLINE)
            );
            // The caller's same-account online snapshot still admits local unpin.
            controller
                .set(&online, target("other"), false)
                .await
                .unwrap();
            assert!(matches!(
                attempts.try_recv(),
                Err(mpsc::error::TryRecvError::Empty)
            ));

            online.session.set_offline(false);
            access.send_replace(Some(online.clone()));
            let (second_live, second_dropped) = attempts.recv().await.unwrap();
            assert!(second_live().is_ok());

            // Title changes must not prevent immediate cancellation by key.
            let renamed = Target {
                title: "Renamed".into(),
                ..original.target.clone()
            };
            controller.set(&online, renamed, false).await.unwrap();
            second_dropped.await.unwrap();
            assert!(second_live().is_err());
            assert!(!view.borrow().records[&original.key].wanted);
            // Even a detached publisher that fails to observe cancellation
            // cannot replace the tombstone using its stale durable token.
            let membership = Membership {
                title: "Stale".into(),
                tracks: vec![],
            };
            assert_eq!(
                store
                    .publish(&online, &original, None, membership, || Ok(()))
                    .unwrap(),
                None,
            );
            let (good, unreadable) = store.scan(&online).unwrap();
            assert!(good.iter().all(|record| !record.wanted));
            assert_eq!(unreadable, [damaged.key]);

            // Logout must fence existing work even before Access is published.
            controller
                .set(&online, target("logout"), true)
                .await
                .unwrap();
            let (third_live, third_dropped) = attempts.recv().await.unwrap();
            assert!(third_live().is_ok());
            epoch.fetch_add(1, Ordering::SeqCst);
            assert_eq!(third_live(), Err(STALE));
            assert_eq!(
                controller.set(&online, target("other"), false).await,
                Err(STALE)
            );
            access.send_replace(None);
            third_dropped.await.unwrap();
            view.wait_for(|latest| latest.epoch.is_none() && latest.records.is_empty())
                .await
                .unwrap();
        })
        .await
        .unwrap();
    }
}
