//! Password and startup auth attempts, and the ordered queue for local login persistence.
//!
//! The engine owns admission: no attempt changes the saved profile before its
//! epoch is accepted, and a saved cookie gives no access until the server validates it.

use crate::{
    api::{Api, Origin, RememberedCredential, Session},
    login_store::{LoginProfile, LoginStore, StoreError},
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
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
const RESTORE_FAILED: &str =
    "Could not restore the saved session. Retry or sign in with your password.";

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
    pub retry: bool,
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
    scope: Option<(String, String)>,
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
    if let Some((origin, username)) = scope {
        // A Retry is never allowed to use the credential for an edited form,
        // or for a profile another process replaced with a different scope.
        if origin != profile.origin.as_str() || username != profile.username {
            return Err(Fallback::default());
        }
    } else {
        prefill(profile.origin.as_str().into(), profile.username.clone()).await;
    }
    if !profile.remembered || current.load(Ordering::SeqCst) != epoch {
        return Err(Fallback::default());
    }
    let credential = match store.restore(&profile).await {
        Ok(Some(credential)) => credential,
        Ok(None) | Err(StoreError::InvalidCredential) => return Err(stale(profile)),
        Err(e) => {
            return Err(Fallback {
                status: RESTORE_FAILED,
                notice: if e == StoreError::KeyringUnavailable {
                    KEYRING
                } else {
                    ""
                },
                retry: true,
                ..Fallback::default()
            });
        }
    };
    // A cancelled keyring lookup must never open a network session later.
    if current.load(Ordering::SeqCst) != epoch {
        return Err(Fallback::default());
    }
    let origin = profile.origin.clone();
    let api = Arc::new(Api::new(origin.clone()).map_err(|_| retry_restore())?);
    match api.resume(&credential).await {
        Ok(session) if session.username == profile.username => Ok(Success {
            api,
            session,
            origin,
            kind: Kind::Restored(profile),
        }),
        // The server is authoritative; a renamed or mismatched account needs
        // explicit sign-in before it can replace the last-good profile.
        Ok(_) => Err(stale(profile)),
        Err(e) if e.expired || e.status == 401 => Err(stale(profile)),
        Err(_) => Err(retry_restore()),
    }
}

fn retry_restore() -> Fallback {
    Fallback {
        status: RESTORE_FAILED,
        retry: true,
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
