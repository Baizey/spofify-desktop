//! Password and startup auth attempts, and the ordered queue for local login persistence.
//!
//! The engine owns admission: no attempt changes the saved profile before its
//! epoch is accepted, and a saved cookie gives no access until the server validates it.

use crate::{
    api::{Api, Origin, RememberedCredential, Session},
    login_store::{LoginProfile, LoginStore, StoreError},
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{sync::mpsc, task::JoinHandle};

const KEYRING: &str = "The system keyring is unavailable or locked.";
const PREFILL: &str = "The desktop could not read the saved sign-in details.";
const NOT_SAVED: &str = "This sign-in could not be saved. You may need to sign in next time.";
const SAVE_KEYRING: &str =
    "The system keyring is unavailable or locked. This sign-in was not remembered.";
const FORGET_KEYRING: &str = "Automatic sign-in is off. The system keyring is unavailable or locked, so its old entry could not be removed.";
const FORGET_FAILED: &str = "The desktop could not update its saved sign-in settings.";
const STALE: &str = "The saved session is no longer valid. Sign in again.";
const RESTORE_FAILED: &str = "Could not restore the saved session. Sign in with your password.";
const RESTORE_BACKOFF: [Duration; 2] = [Duration::from_millis(300), Duration::from_secs(1)];

pub(crate) struct Success {
    pub api: Arc<Api>,
    pub session: Session,
    pub origin: Origin,
    pub kind: Kind,
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
    })
}

pub(crate) async fn restore<F>(
    epoch: u64,
    current: &AtomicU64,
    prefill: impl FnOnce(String, String) -> F,
) -> Outcome
where
    F: Future<Output = ()>,
{
    let unreadable = |_| Fallback {
        notice: PREFILL,
        ..Fallback::default()
    };
    let store = LoginStore::open_default().map_err(unreadable)?;
    let profile = store.profile().map_err(unreadable)?;
    let Some(profile) = profile else {
        return Err(Fallback::default());
    };
    prefill(profile.origin.as_str().into(), profile.username.clone()).await;
    if !profile.remembered || current.load(Ordering::SeqCst) != epoch {
        return Err(Fallback::default());
    }
    let origin = profile.origin.clone();
    let api = Arc::new(Api::new(origin.clone()).map_err(|_| restore_failed())?);
    let session = with_restore_backoff(epoch, current, &RESTORE_BACKOFF, || {
        restore_once(&store, &profile, &api, epoch, current)
    })
    .await?;
    Ok(Success {
        api,
        session,
        origin,
        kind: Kind::Restored(profile),
    })
}

enum RestoreFailure {
    Transient,
    Final(Fallback),
}

async fn restore_once(
    store: &LoginStore,
    profile: &LoginProfile,
    api: &Api,
    epoch: u64,
    current: &AtomicU64,
) -> Result<Session, RestoreFailure> {
    // Re-check the original login slot on each attempt. A changed profile
    // can neither substitute its credential nor reuse the old one here.
    let credential = match store.restore(profile).await {
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
    match api.resume(&credential).await {
        Ok(session) if session.username == profile.username => Ok(session),
        // A renamed or mismatched account still needs explicit sign-in.
        Ok(_) => Err(RestoreFailure::Final(stale(profile.clone()))),
        Err(e) if e.expired || e.status == 401 => {
            Err(RestoreFailure::Final(stale(profile.clone())))
        }
        Err(e) if e.can_retry_restore() => Err(RestoreFailure::Transient),
        Err(_) => Err(RestoreFailure::Final(restore_failed())),
    }
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

pub(crate) enum Write {
    Record {
        origin: Origin,
        username: String,
        wanted: bool,
        credential: Option<RememberedCredential>,
        retained: Arc<AtomicBool>,
    },
    Forget(Option<LoginProfile>),
}

impl Write {
    async fn run(self) -> &'static str {
        let store = LoginStore::open_default();
        match self {
            Self::Record {
                origin,
                username,
                wanted,
                credential,
                retained,
            } => {
                let stored = match store {
                    Ok(store) => {
                        store
                            .record_login(&origin, &username, credential.as_ref())
                            .await
                    }
                    Err(e) => Err(e),
                };
                match stored {
                    Ok(()) if wanted && credential.is_none() => NOT_SAVED,
                    Ok(()) => {
                        retained.store(credential.is_some(), Ordering::Release);
                        ""
                    }
                    Err(StoreError::KeyringUnavailable) if wanted => SAVE_KEYRING,
                    Err(StoreError::KeyringUnavailable) => FORGET_KEYRING,
                    Err(_) => NOT_SAVED,
                }
            }
            Self::Forget(profile) => {
                let forgotten = match (store, profile) {
                    (Ok(store), Some(profile)) => store.forget_profile(&profile).await,
                    (Ok(store), None) => store.forget().await,
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
    pub(crate) fn new(publish: impl Fn(u64, &'static str) + Send + 'static) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<(u64, Write)>();
        let worker = tokio::spawn(async move {
            while let Some((epoch, write)) = rx.recv().await {
                let notice = write.run().await;
                if !notice.is_empty() {
                    publish(epoch, notice);
                }
            }
        });
        Self { tx, worker }
    }

    pub(crate) fn send(&self, epoch: u64, write: Write) {
        let _ = self.tx.send((epoch, write));
    }

    pub(crate) async fn finish(self) {
        drop(self.tx);
        let _ = self.worker.await;
    }
}
