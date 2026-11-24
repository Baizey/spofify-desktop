//! Password and startup auth attempts, and the ordered queue for local login persistence.
//!
//! The engine owns admission: no attempt changes the saved profile before its
//! epoch is accepted. A legacy cookie still requires server validation; only an
//! typed local grant from the active keyring slot can authorize cold offline use.

use crate::{
    api::{Api, ApiError, Origin, RememberedLogin, Session},
    login_store::{LoginProfile, LoginStore, StagedMode, StoreError},
};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{sync::mpsc, task::JoinHandle};

const KEYRING: &str = "The system keyring is unavailable or locked.";
const BUSY: &str = "Another desktop window is using this saved sign-in.";
const PREFILL: &str = "The desktop could not read the saved sign-in details.";
const NOT_SAVED: &str = "This sign-in could not be saved. You may need to sign in next time.";
const SAVE_KEYRING: &str =
    "The system keyring is unavailable or locked. This sign-in was not remembered.";
const FORGET_KEYRING: &str = "Automatic sign-in is off. The system keyring is unavailable or locked, so its old entry could not be removed.";
const MODE_NOT_SAVED: &str = "This mode could not be saved for the next launch.";
const FORGET_FAILED: &str = "The desktop could not update its saved sign-in settings.";
const STALE: &str = "The saved session is no longer valid. Sign in again.";
const RESTORE_FAILED: &str = "Could not restore the saved session. Sign in with your password.";
const RESTORE_BACKOFF: [Duration; 2] = [Duration::from_millis(300), Duration::from_secs(1)];

pub(crate) struct Success {
    pub api: Arc<Api>,
    pub session: Session,
    pub origin: Origin,
    pub kind: Kind,
    /// The stored preference, not the transport state. A typed grant always
    /// starts with a local-only session; only auto-offline tries online by default.
    pub manual_offline: bool,
}

pub(crate) enum Kind {
    Password { remember: bool },
    Restored(LoginProfile),
}

#[derive(Default)]
pub(crate) struct Fallback {
    pub status: &'static str,
    pub notice: &'static str,
    pub discard: Option<LoginProfile>,
}

pub(crate) type Outcome = Result<Success, Fallback>;

pub(crate) async fn password(
    origin: &str,
    username: &str,
    password: &str,
    remember: bool,
) -> Outcome {
    let fail = |e: crate::api::ApiError| Fallback {
        status: e.message,
        ..Fallback::default()
    };
    let origin = Origin::parse(origin).map_err(fail)?;
    let api = Arc::new(Api::new(origin.clone()).map_err(fail)?);
    let session = if remember {
        api.login_remembered(username, password).await
    } else {
        api.login(username, password).await
    }
    .map_err(fail)?;
    Ok(Success {
        api,
        session,
        origin,
        kind: Kind::Password { remember },
        manual_offline: false,
    })
}

pub(crate) async fn restore<F>(
    store: Result<Arc<LoginStore>, StoreError>,
    epoch: u64,
    current: &AtomicU64,
    manual_offline: bool,
    online_preference_known: impl Fn(),
    prefill: impl FnOnce(String, String) -> F,
) -> Outcome
where
    F: Future<Output = ()>,
{
    restore_with_mode(
        store,
        epoch,
        current,
        Some(manual_offline),
        online_preference_known,
        prefill,
        async |store, profile| store.restore(profile).await,
    )
    .await
}

#[cfg(test)]
pub(crate) async fn restore_with<F>(
    store: Result<Arc<LoginStore>, StoreError>,
    epoch: u64,
    current: &AtomicU64,
    online_preference_known: impl Fn(),
    prefill: impl FnOnce(String, String) -> F,
    lookup: impl AsyncFn(&LoginStore, &LoginProfile) -> Result<Option<RememberedLogin>, StoreError>,
) -> Outcome
where
    F: Future<Output = ()>,
{
    restore_with_mode(store, epoch, current, None, online_preference_known, prefill, lookup).await
}

async fn restore_with_mode<F>(
    store: Result<Arc<LoginStore>, StoreError>,
    epoch: u64,
    current: &AtomicU64,
    manual_offline: Option<bool>,
    online_preference_known: impl Fn(),
    prefill: impl FnOnce(String, String) -> F,
    lookup: impl AsyncFn(&LoginStore, &LoginProfile) -> Result<Option<RememberedLogin>, StoreError>,
) -> Outcome
where F: Future<Output = ()>,
{
    let unreadable = |_| Fallback {
        notice: PREFILL,
        ..Fallback::default()
    };
    let store = store.map_err(|e| {
        if e == StoreError::Busy {
            Fallback {
                notice: BUSY,
                ..Fallback::default()
            }
        } else {
            unreadable(e)
        }
    })?;
    let profile = store.profile().map_err(unreadable)?;
    // Disk prefill alone never records Work offline. If a slot is active,
    // only its typed keyring record can resolve the startup preference.
    if manual_offline != Some(true) && profile.as_ref().is_none_or(|profile| !profile.remembered) {
        online_preference_known();
    }
    let Some(profile) = profile else {
        return Err(Fallback::default());
    };
    prefill(profile.origin.as_str().into(), profile.username.clone()).await;
    if !profile.remembered || current.load(Ordering::SeqCst) != epoch {
        return Err(Fallback::default());
    }
    let (api, session, manual_offline) =
        with_restore_backoff(epoch, current, &RESTORE_BACKOFF, || {
            restore_once_with_mode(
                lookup(&store, &profile), &profile, epoch, current,
                manual_offline, &online_preference_known,
            )
        })
        .await?;
    Ok(Success {
        api,
        session,
        origin: profile.origin.clone(),
        kind: Kind::Restored(profile),
        manual_offline,
    })
}

enum RestoreFailure {
    Transient,
    Final(Fallback),
}

#[cfg(test)]
async fn restore_once(
    lookup: impl Future<Output = Result<Option<RememberedLogin>, StoreError>>,
    profile: &LoginProfile,
    epoch: u64,
    current: &AtomicU64,
    online_preference_known: &impl Fn(),
) -> Result<(Arc<Api>, Session, bool), RestoreFailure> {
    restore_once_with_mode(lookup, profile, epoch, current, None, online_preference_known).await
}

async fn restore_once_with_mode(
    lookup: impl Future<Output = Result<Option<RememberedLogin>, StoreError>>,
    profile: &LoginProfile,
    epoch: u64,
    current: &AtomicU64,
    manual_offline: Option<bool>,
    online_preference_known: &impl Fn(),
) -> Result<(Arc<Api>, Session, bool), RestoreFailure> {
    // Re-check the original login slot on each attempt. A changed profile
    // can neither substitute its credential nor reuse the old one here.
    let credential = match lookup.await {
        Ok(Some(credential)) => credential,
        Ok(None) | Err(StoreError::InvalidCredential) => {
            return Err(RestoreFailure::Final(stale(profile.clone())));
        }
        Err(StoreError::Busy) => return Err(RestoreFailure::Transient),
        Err(StoreError::KeyringUnavailable) => {
            return Err(RestoreFailure::Final(Fallback {
                notice: KEYRING,
                ..restore_failed()
            }));
        }
        Err(_) => return Err(RestoreFailure::Final(restore_failed())),
    };
    // A cancelled keyring lookup must never open a network session later.
    if current.load(Ordering::SeqCst) != epoch {
        return Err(RestoreFailure::Final(Fallback::default()));
    }
    let credential = match credential {
        RememberedLogin::Offline(grant) => {
            let (api, session) = grant
                .admit(&profile.origin, &profile.username)
                .map_err(|_| RestoreFailure::Final(stale(profile.clone())))?;
            let manual = manual_offline.unwrap_or_else(|| grant.manual_offline());
            if !manual {
                online_preference_known();
            }
            return Ok((Arc::new(api), session, manual));
        }
        RememberedLogin::Online(_) if manual_offline == Some(true) => {
            return Err(RestoreFailure::Final(Fallback::default()));
        }
        RememberedLogin::Online(credential) => credential,
    };
    let api = Arc::new(
        Api::new(profile.origin.clone()).map_err(|_| RestoreFailure::Final(restore_failed()))?,
    );
    match api
        .resume_if(&credential, || {
            // Local origin/cookie validation is complete; the updater does not
            // need to wait for server revalidation or account admission.
            online_preference_known();
            current.load(Ordering::SeqCst) == epoch
        })
        .await
    {
        Ok(session) if session.username == profile.username => Ok((api, session, false)),
        // A renamed or mismatched account still needs explicit sign-in.
        Ok(_) => Err(RestoreFailure::Final(stale(profile.clone()))),
        Err(e) if e.expired || e.status == 401 => {
            Err(RestoreFailure::Final(stale(profile.clone())))
        }
        Err(e) if e.can_retry_restore() => Err(RestoreFailure::Transient),
        Err(_) => Err(RestoreFailure::Final(restore_failed())),
    }
}

const LOST_LOCAL: ApiError = ApiError {
    message: STALE,
    expired: true,
    status: 0,
};

/// Validate a cold local grant for an online attempt without lifting its
/// network fence. The engine owns the attempt deadline, rechecks the active
/// slot before admission, and fences the returned capability by mode and epoch.
pub(crate) async fn reconnect_local(
    store: &LoginStore,
    profile: &LoginProfile,
    local_api: &Api,
    local: &Session,
    current: impl Fn() -> bool,
) -> Result<(Arc<Api>, Session), ApiError> {
    let unavailable = ApiError {
        message: KEYRING,
        status: 0,
        expired: false,
    };
    if !current() {
        return Err(unavailable);
    }
    let (origin, account) = local_api
        .authenticated_account(local)
        .map_err(|_| LOST_LOCAL)?;
    if origin != profile.origin.as_str()
        || local.username != profile.username
        || !local.is_local_only()
    {
        return Err(LOST_LOCAL);
    }
    let found = store.restore(profile).await;
    if !current() {
        return Err(unavailable);
    }
    let grant = match found {
        Ok(Some(RememberedLogin::Offline(grant)))
            if grant.matches_principal(&profile.username, account) =>
        {
            grant
        }
        Err(StoreError::Busy) => {
            return Err(ApiError {
                status: 503,
                ..unavailable
            });
        }
        Err(StoreError::Config | StoreError::KeyringUnavailable) => return Err(unavailable),
        _ => return Err(LOST_LOCAL),
    };
    grant
        .validate(&profile.origin, &profile.username)
        .map_err(|_| LOST_LOCAL)?;
    let api = Arc::new(Api::new(profile.origin.clone())?);
    // `resume_if` requires a same-origin, same-account rolling-v1 response
    // and constructs an online session from its fresh summary, cookie and CSRF.
    let session = api
        .resume_if(RememberedLogin::Offline(grant).credential(), || {
            current() && local.can_access_local()
        })
        .await
        .map_err(|error| {
            if error.rejects_local_grant() {
                LOST_LOCAL
            } else {
                error
            }
        })?;
    if session.username != profile.username || !local.can_access_local() {
        return Err(LOST_LOCAL);
    }
    if !current() {
        return Err(unavailable);
    }
    Ok((api, session))
}

async fn with_restore_backoff<T, F>(
    epoch: u64,
    current: &AtomicU64,
    delays: &[Duration],
    mut attempt: impl FnMut() -> F,
) -> Result<T, Fallback>
where
    F: Future<Output = Result<T, RestoreFailure>>,
{
    // The engine aborts this task on Cancel, a newer sign-in, offline mode or
    // shutdown. Recheck the epoch after sleeping as well as after the attempt.
    let mut delays = delays.iter();
    loop {
        if current.load(Ordering::SeqCst) != epoch {
            return Err(Fallback::default());
        }
        let outcome = attempt().await;
        if current.load(Ordering::SeqCst) != epoch {
            return Err(Fallback::default());
        }
        match outcome {
            Ok(value) => return Ok(value),
            Err(RestoreFailure::Final(fallback)) => return Err(fallback),
            Err(RestoreFailure::Transient) => match delays.next() {
                Some(delay) => tokio::time::sleep(*delay).await,
                None => return Err(restore_failed()),
            },
        }
    }
}

fn restore_failed() -> Fallback {
    Fallback {
        status: RESTORE_FAILED,
        ..Fallback::default()
    }
}

fn stale(profile: LoginProfile) -> Fallback {
    Fallback {
        status: STALE,
        discard: Some(profile),
        ..Fallback::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::ready;

    fn typed_record(profile: &LoginProfile, format: &str) -> RememberedLogin {
        serde_json::from_value(serde_json::json!({
            "format": format,
            "username": profile.username,
            "role": "user",
            "credential": {
                "origin": profile.origin.as_str(),
                "accountId": "fixture-account",
                "cookie": "spofify_session=fixture-cookie"
            },
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn remembered_manual_startup_admits_only_local_authority() {
        let profile = LoginProfile::test_remembered();
        let current = AtomicU64::new(7);
        let (api, session, manual_offline) = restore_once(
            ready(Ok(Some(typed_record(&profile, "local-v1")))),
            &profile,
            7,
            &current,
            &|| panic!("manual mode must keep network blocked"),
        )
        .await
        .ok()
        .expect("a valid typed grant needs no server");
        assert!(manual_offline && session.is_local_only() && session.is_offline());
        assert_eq!(
            api.authenticated_account(&session).unwrap(),
            (profile.origin.as_str(), "fixture-account")
        );
    }

    #[tokio::test]
    async fn remembered_online_preference_admits_local_before_http() {
        let profile = LoginProfile::test_remembered();
        let current = AtomicU64::new(7);
        let auto = AtomicBool::new(false);
        let (api, session, manual_offline) = restore_once(
            ready(Ok(Some(typed_record(&profile, "local-online-v1")))),
            &profile,
            7,
            &current,
            &|| auto.store(true, Ordering::Relaxed),
        )
        .await
        .ok()
        .expect("an online preference must not gate local admission on HTTP");
        assert!(auto.load(Ordering::Relaxed) && !manual_offline);
        assert!(session.is_local_only() && session.is_offline());
        assert_eq!(
            api.authenticated_account(&session).unwrap().1,
            "fixture-account"
        );
        assert!(api.remembered_login(&session).is_err());
    }

    #[tokio::test]
    async fn keyring_result_after_epoch_change_cannot_admit_local_authority() {
        let profile = LoginProfile::test_remembered();
        let current = AtomicU64::new(7);
        let result = restore_once(
            async {
                current.store(8, Ordering::SeqCst);
                Ok(Some(typed_record(&profile, "local-v1")))
            },
            &profile,
            7,
            &current,
            &|| panic!("a retired epoch must not open network"),
        )
        .await;
        assert!(
            matches!(result, Err(RestoreFailure::Final(fallback)) if fallback.discard.is_none())
        );
    }

    #[tokio::test]
    async fn restore_automatically_recovers_then_stops_at_the_attempt_limit() {
        let current = AtomicU64::new(7);
        let delays = RESTORE_BACKOFF.map(|_| Duration::ZERO);
        let mut attempts = 0;
        let recovered = with_restore_backoff(7, &current, &delays, || {
            attempts += 1;
            ready(if attempts == 1 {
                Err(RestoreFailure::Transient)
            } else {
                Ok(())
            })
        })
        .await;
        assert!(recovered.is_ok());
        assert_eq!(attempts, 2);

        let mut attempts = 0;
        let failed = with_restore_backoff::<(), _>(7, &current, &delays, || {
            attempts += 1;
            ready(Err(RestoreFailure::Transient))
        })
        .await
        .err()
        .unwrap();
        assert_eq!(attempts, 3);
        assert_eq!(failed.status, RESTORE_FAILED);
        assert!(failed.discard.is_none());
    }

    #[tokio::test]
    async fn final_failure_after_a_transient_one_finishes_immediately() {
        let current = AtomicU64::new(7);
        let mut attempts = 0;
        let result = with_restore_backoff::<(), _>(7, &current, &[Duration::ZERO; 2], || {
            attempts += 1;
            ready(if attempts == 1 {
                Err(RestoreFailure::Transient)
            } else {
                Err(RestoreFailure::Final(Fallback {
                    status: STALE,
                    ..Fallback::default()
                }))
            })
        })
        .await;
        assert_eq!(result.err().unwrap().status, STALE);
        assert_eq!(attempts, 2);
    }

    #[tokio::test]
    async fn epoch_change_during_backoff_never_reissues_restore() {
        let current = AtomicU64::new(7);
        let attempted = tokio::sync::Notify::new();
        let mut attempts = 0;
        let delays = [Duration::from_millis(20)];
        let (result, ()) = tokio::join!(
            with_restore_backoff::<(), _>(7, &current, &delays, || {
                attempts += 1;
                attempted.notify_one();
                ready(Err(RestoreFailure::Transient))
            }),
            async {
                attempted.notified().await;
                current.store(8, Ordering::SeqCst);
            },
        );
        assert_eq!(result.err().unwrap().status, "");
        assert_eq!(attempts, 1);
    }
}

/// A slot identity shared with the ordered writer, not a credential. Mode
/// updates replace this only after the new active slot has been committed.
#[derive(Clone, Default)]
pub(crate) struct Retention(Arc<RetentionState>);

#[derive(Default)]
struct RetentionState {
    slot: Mutex<(Option<LoginProfile>, Option<StagedMode>)>,
    retired: AtomicBool,
}

impl Retention {
    pub(crate) fn new(profile: LoginProfile) -> Self {
        let retained = Self::default();
        retained.set(profile);
        retained
    }

    pub(crate) fn profile(&self) -> Option<LoginProfile> {
        self.0.slot.lock().unwrap().0.clone()
    }

    fn set(&self, profile: LoginProfile) {
        *self.0.slot.lock().unwrap() = (Some(profile), None);
    }

    fn staged(&self) -> Option<StagedMode> {
        self.0.slot.lock().unwrap().1.clone()
    }

    fn set_staged(&self, staged: StagedMode) {
        let mut slot = self.0.slot.lock().unwrap();
        if slot
            .0
            .as_ref()
            .is_some_and(|profile| staged.matches(profile))
        {
            slot.1 = Some(staged);
        }
    }

    pub(crate) fn retire(&self) {
        self.0.retired.store(true, Ordering::Release);
    }

    fn permits(&self, session: &Session) -> bool {
        !self.0.retired.load(Ordering::Acquire) && !session.is_revoked()
    }
}

pub(crate) enum Write {
    Record {
        origin: Origin,
        username: String,
        wanted: bool,
        login: Option<RememberedLogin>,
        retained: Retention,
        session: Session,
    },
    UpgradeLegacy(Retention, Session, RememberedLogin),
    Preference(Retention, Session, bool, Option<StagedMode>),
    Forget(Option<LoginProfile>),
    ForgetRetained(Retention),
    Barrier(tokio::sync::oneshot::Sender<()>),
}

fn matches_retained_login(login: &RememberedLogin, session: &Session) -> bool {
    matches!(login, RememberedLogin::Offline(grant)
        if grant.matches_principal(&session.username, session.remembered_account_id()))
}

// Busy occurs before a mutation acquires its flock, so repeating this case
// cannot repeat a keyring operation. Most contenders are cancelled reads.
async fn with_write_admission<T, F>(mut operation: impl FnMut() -> F) -> Result<T, StoreError>
where
    F: Future<Output = Result<T, StoreError>>,
{
    for delay in RESTORE_BACKOFF {
        match operation().await {
            Err(StoreError::Busy) => tokio::time::sleep(delay).await,
            other => return other,
        }
    }
    operation().await
}

impl Write {
    /// A preference is independent of the current session's network fence.
    /// Writes run in issue order and publication is still fenced by retention
    /// and revocation. In particular, a temporary auto-offline transport fence
    /// must not itself change what the next launch does.
    pub(crate) fn preference(
        retained: Retention,
        session: Session,
        manual_offline: bool,
        staged: Option<StagedMode>,
    ) -> Self {
        Self::Preference(retained, session, manual_offline, staged)
    }

    async fn run(self, store: Result<&LoginStore, StoreError>) -> &'static str {
        if let Self::Barrier(done) = self {
            let _ = done.send(());
            return "";
        }
        if store.is_err_and(|e| e == StoreError::Busy) {
            return BUSY;
        }
        match self {
            Self::Record {
                origin,
                username,
                wanted,
                login,
                retained,
                session,
            } => {
                let permitted = || retained.permits(&session);
                if !permitted() {
                    return "";
                }
                let login = login.filter(|login| wanted && matches_retained_login(login, &session));
                let stored = match store {
                    Ok(store) => {
                        with_write_admission(|| {
                            store.record_login(&origin, &username, login.as_ref(), &permitted)
                        })
                        .await
                    }
                    Err(e) => Err(e),
                };
                match stored {
                    Ok(_) if wanted && login.is_none() => NOT_SAVED,
                    Ok(profile) => {
                        if profile.remembered {
                            retained.set(profile);
                        }
                        ""
                    }
                    Err(_) if !permitted() => "",
                    Err(StoreError::KeyringUnavailable) if wanted => SAVE_KEYRING,
                    Err(StoreError::KeyringUnavailable) => FORGET_KEYRING,
                    Err(_) => NOT_SAVED,
                }
            }
            Self::UpgradeLegacy(retained, session, login) => {
                let permitted = || retained.permits(&session) && !session.is_local_only();
                if !permitted() {
                    return "";
                }
                let (Some(profile), Ok(store)) = (retained.profile(), store) else {
                    return NOT_SAVED;
                };
                if !matches_retained_login(&login, &session) {
                    return NOT_SAVED;
                }
                match with_write_admission(|| store.upgrade_legacy(&profile, &login, &permitted))
                    .await
                {
                    Ok(profile) => {
                        retained.set(profile);
                        ""
                    }
                    Err(_) if !permitted() => "",
                    Err(_) => NOT_SAVED,
                }
            }
            Self::Preference(retained, session, manual_offline, staged) => {
                if let Some(staged) = staged {
                    retained.set_staged(staged);
                }
                let permitted = || retained.permits(&session);
                if !permitted() {
                    return "";
                }
                let Some(profile) = retained.profile() else {
                    return "";
                };
                let Ok(store) = store else {
                    return MODE_NOT_SAVED;
                };
                if profile.username != session.username {
                    return MODE_NOT_SAVED;
                }
                // If an accepted mode change disarmed the slot before a temporary
                // keyring lock, the same admitted owner can still update that exact
                // item after unlock. A fresh process cannot adopt the cleanup marker.
                let staged = match retained.staged() {
                    Some(staged) => staged,
                    None => match with_write_admission(|| {
                        std::future::ready(store.stage_mode(&profile))
                    })
                    .await
                    {
                        Ok(staged) => staged,
                        Err(_) => return MODE_NOT_SAVED,
                    },
                };
                retained.set_staged(staged.clone());
                match with_write_admission(|| {
                    store.change_preference(
                        &profile,
                        &staged,
                        session.remembered_account_id(),
                        manual_offline,
                        &permitted,
                    )
                })
                .await
                {
                    Ok(profile) => {
                        retained.set(profile);
                        ""
                    }
                    Err(_) if !permitted() => "",
                    Err(_) => MODE_NOT_SAVED,
                }
            }
            Self::Barrier(_) => unreachable!(),
            intent @ (Self::Forget(_) | Self::ForgetRetained(_)) => {
                let profile = match intent {
                    Self::Forget(profile) => profile,
                    Self::ForgetRetained(retained) => {
                        let Some(profile) = retained.profile() else {
                            return "";
                        };
                        Some(profile)
                    }
                    _ => unreachable!(),
                };
                let forgotten = match (store, profile) {
                    (Ok(store), Some(profile)) => {
                        with_write_admission(|| store.forget_profile(&profile)).await
                    }
                    (Ok(store), None) => with_write_admission(|| store.forget()).await,
                    (Err(e), _) => Err(e),
                };
                match forgotten {
                    Ok(()) => "",
                    Err(StoreError::KeyringUnavailable) => FORGET_KEYRING,
                    Err(_) => FORGET_FAILED,
                }
            }
        }
    }
}

/// Save/forget operations are ordered independently of cancelled request tasks.
/// In particular, logout retires a slot even when an earlier save was in flight.
pub(crate) struct Writer {
    tx: mpsc::UnboundedSender<(u64, Write)>,
    worker: JoinHandle<()>,
}

impl Writer {
    pub(crate) fn new(
        store: Result<Arc<LoginStore>, StoreError>,
        publish: impl Fn(u64, &'static str) + Send + 'static,
    ) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<(u64, Write)>();
        let worker = tokio::spawn(async move {
            while let Some((epoch, write)) = rx.recv().await {
                let notice = write.run(store.as_deref().map_err(|e| *e)).await;
                if !notice.is_empty() {
                    publish(epoch, notice);
                }
            }
        });
        Self { tx, worker }
    }

    pub(crate) fn barrier(&self, epoch: u64) -> tokio::sync::oneshot::Receiver<()> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.send(epoch, Write::Barrier(tx));
        rx
    }

    pub(crate) fn sender(&self) -> mpsc::UnboundedSender<(u64, Write)> {
        self.tx.clone()
    }

    pub(crate) fn send(&self, epoch: u64, write: Write) {
        let _ = self.tx.send((epoch, write));
    }

    pub(crate) async fn finish(self) {
        drop(self.tx);
        let _ = self.worker.await;
    }
}
