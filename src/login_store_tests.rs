use super::*;
use std::{
    cell::{Cell, RefCell},
    ffi::OsStr,
    future::poll_fn,
    os::unix::fs::{DirBuilderExt, PermissionsExt, symlink},
    task::Poll,
};

const SYNTHETIC_SECRET: &[u8] = br#"{"origin":"https://music.example","accountId":"synthetic-account","cookie":"synthetic-cookie"}"#;

struct Fixture {
    base: PathBuf,
    store: LoginStore,
}

impl Fixture {
    fn new() -> Self {
        let base = stdfs::canonicalize(env::temp_dir())
            .unwrap()
            .join(format!("spofify-login-store-test-{}", uuid::Uuid::new_v4()));
        stdfs::DirBuilder::new().mode(0o700).create(&base).unwrap();
        let store = LoginStore {
            dir: config_dir(&base).unwrap(),
        };
        Self { base, store }
    }

    fn reopen(&self) -> LoginStore {
        LoginStore {
            dir: config_dir(&self.base).unwrap(),
        }
    }
    fn path(&self, name: &str) -> PathBuf {
        self.base.join(ROOT).join(name)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        stdfs::remove_dir_all(&self.base).unwrap();
    }
}

#[derive(Clone, Copy, Default)]
enum Behavior {
    #[default]
    Available,
    Unavailable,
    InterruptStore,
    InterruptDelete,
}

#[derive(Default)]
struct FakeKeyring {
    behavior: Cell<Behavior>,
    items: RefCell<Vec<(Slot, Vec<u8>)>>,
    calls: Cell<(usize, usize, usize)>,
}

impl Keyring for FakeKeyring {
    async fn lookup(&self, slot: Slot) -> Result<Option<Vec<u8>>> {
        let (lookups, writes, deletes) = self.calls.get();
        self.calls.set((lookups + 1, writes, deletes));
        if matches!(self.behavior.get(), Behavior::Unavailable) {
            return Err(StoreError::KeyringUnavailable);
        }
        Ok(self
            .items
            .borrow()
            .iter()
            .find(|(id, _)| *id == slot)
            .map(|(_, blob)| blob.clone()))
    }

    async fn store(&self, slot: Slot, blob: &[u8]) -> Result<()> {
        let (lookups, writes, deletes) = self.calls.get();
        self.calls.set((lookups, writes + 1, deletes));
        if matches!(self.behavior.get(), Behavior::Unavailable) {
            return Err(StoreError::KeyringUnavailable);
        }
        self.items.borrow_mut().push((slot, blob.to_vec()));
        if matches!(self.behavior.get(), Behavior::InterruptStore) {
            std::future::pending::<()>().await;
        }
        Ok(())
    }

    async fn clear(&self, slot: Slot) -> Result<()> {
        let (lookups, writes, deletes) = self.calls.get();
        self.calls.set((lookups, writes, deletes + 1));
        if matches!(self.behavior.get(), Behavior::Unavailable) {
            return Err(StoreError::KeyringUnavailable);
        }
        self.items.borrow_mut().retain(|(id, _)| *id != slot);
        if matches!(self.behavior.get(), Behavior::InterruptDelete) {
            std::future::pending::<()>().await;
        }
        Ok(())
    }
}

impl LoginStore {
    async fn record_with(
        &self,
        keyring: &impl Keyring,
        origin: &Origin,
        username: &str,
        blob: Option<&[u8]>,
    ) -> Result<LoginProfile> {
        self.record_with_permit(keyring, origin, username, blob, &|| true)
            .await
    }
    async fn change_mode_with(
        &self,
        keyring: &impl Keyring,
        profile: &LoginProfile,
        login: &RememberedLogin,
        staged: Option<&StagedMode>,
    ) -> Result<LoginProfile> {
        let staged = match staged {
            Some(staged) => staged.clone(),
            None => self.stage_mode(profile)?,
        };
        self.change_mode_with_permit(keyring, profile, login, &staged, &|| true)
            .await
    }
}

fn origin() -> Origin {
    Origin::parse("HTTPS://Music.Example:443/").unwrap()
}

async fn cancel_after_first_poll<T>(future: impl Future<Output = Result<T>>) {
    let mut future = std::pin::pin!(future);
    poll_fn(|cx| {
        assert!(future.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
}

#[tokio::test]
async fn prefill_does_not_touch_keyring_and_contains_only_canonical_identity() {
    let fixture = Fixture::new();
    let store = &fixture.store;
    let keyring = FakeKeyring::default();
    keyring.behavior.set(Behavior::Unavailable);
    assert!(store.profile().unwrap().is_none());
    store
        .record_with(&keyring, &origin(), "Listener", None)
        .await
        .unwrap();
    let profile = fixture.reopen().profile().unwrap().unwrap();
    assert_eq!(profile.origin.as_str(), "https://music.example");
    assert_eq!(profile.username, "Listener");
    assert!(!profile.remembered);
    assert!(
        store
            .restore_with(&keyring, &profile)
            .await
            .unwrap()
            .is_none()
    );
    store.forget_with(&keyring, None).await.unwrap();
    assert_eq!(keyring.calls.get(), (0, 0, 0));
    let bytes = stdfs::read(fixture.path(METADATA)).unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        json,
        serde_json::json!({ "version": 1, "origin": "https://music.example", "username": "Listener" })
    );
    assert_eq!(
        stdfs::metadata(fixture.path(METADATA))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[tokio::test]
async fn remember_replaces_the_slot_and_rejects_stale_restore_and_cleanup() {
    let fixture = Fixture::new();
    let store = &fixture.store;
    let keyring = FakeKeyring::default();
    let secret: RememberedCredential = serde_json::from_slice(SYNTHETIC_SECRET).unwrap();
    let encoded = serde_json::to_vec(&secret).unwrap();
    store
        .record_with(&keyring, &origin(), "listener", Some(&encoded))
        .await
        .unwrap();
    let old = store.profile().unwrap().unwrap();
    assert!(old.remembered);
    assert_eq!(
        store.restore_with(&keyring, &old).await.unwrap().unwrap(),
        encoded
    );
    let json: serde_json::Value =
        serde_json::from_slice(&stdfs::read(fixture.path(METADATA)).unwrap()).unwrap();
    assert_eq!(
        json,
        serde_json::json!({ "version": 1, "origin": "https://music.example", "username": "listener", "active": String::from(old.active_slot.unwrap()) })
    );
    store
        .record_with(&keyring, &origin(), "listener", Some(SYNTHETIC_SECRET))
        .await
        .unwrap();
    let new = fixture.reopen().profile().unwrap().unwrap();
    assert!(new.remembered && old.active_slot != new.active_slot);
    assert!(store.restore_with(&keyring, &old).await.unwrap().is_none());
    store.forget_with(&keyring, Some(&old)).await.unwrap();
    assert!(store.profile().unwrap().unwrap().remembered);
    assert_eq!(keyring.items.borrow().len(), 1);
    keyring.behavior.set(Behavior::Unavailable);
    assert_eq!(
        store.restore_with(&keyring, &new).await.unwrap_err(),
        StoreError::KeyringUnavailable
    );
    assert!(store.profile().unwrap().unwrap().remembered);
    keyring.behavior.set(Behavior::Available);
    keyring.items.borrow_mut().clear();
    assert!(store.restore_with(&keyring, &new).await.unwrap().is_none());
}

#[tokio::test]
async fn unavailable_delete_disarms_restore_and_keeps_prefill_and_retry_marker() {
    let fixture = Fixture::new();
    let store = &fixture.store;
    let keyring = FakeKeyring::default();
    store
        .record_with(&keyring, &origin(), "listener", Some(SYNTHETIC_SECRET))
        .await
        .unwrap();
    let previous = store.profile().unwrap().unwrap();
    keyring.behavior.set(Behavior::Unavailable);
    assert_eq!(
        store.forget_with(&keyring, None).await.unwrap_err(),
        StoreError::KeyringUnavailable
    );
    let reopened = fixture.reopen();
    let prefill = reopened.profile().unwrap().unwrap();
    assert_eq!(prefill.username, "listener");
    assert!(!prefill.remembered);
    assert!(
        reopened
            .restore_with(&keyring, &previous)
            .await
            .unwrap()
            .is_none()
    );
    assert!(reopened.read().unwrap().unwrap().cleanup == previous.active_slot);
    assert_eq!(keyring.calls.get(), (0, 1, 1));
    keyring.behavior.set(Behavior::Available);
    reopened.forget_with(&keyring, None).await.unwrap();
    assert!(reopened.read().unwrap().unwrap().cleanup.is_none());
    assert!(keyring.items.borrow().is_empty());
}

#[tokio::test]
async fn interrupted_keyring_mutations_leave_only_a_cleanup_marker() {
    let fixture = Fixture::new();
    let store = &fixture.store;
    let keyring = FakeKeyring::default();
    keyring.behavior.set(Behavior::InterruptStore);
    cancel_after_first_poll(store.record_with(
        &keyring,
        &origin(),
        "listener",
        Some(SYNTHETIC_SECRET),
    ))
    .await;
    let reopened = fixture.reopen();
    let meta = reopened.read().unwrap().unwrap();
    assert!(meta.active.is_none() && meta.cleanup.is_some());
    let prefill = reopened.profile().unwrap().unwrap();
    assert!(
        reopened
            .restore_with(&keyring, &prefill)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(keyring.items.borrow().len(), 1);
    keyring.behavior.set(Behavior::InterruptDelete);
    cancel_after_first_poll(reopened.forget_with(&keyring, None)).await;
    let reopened = fixture.reopen();
    assert!(reopened.read().unwrap().unwrap().cleanup == meta.cleanup);
    assert!(keyring.items.borrow().is_empty());
    keyring.behavior.set(Behavior::Available);
    reopened.forget_with(&keyring, None).await.unwrap();
    assert!(reopened.read().unwrap().unwrap().cleanup.is_none());
}

#[tokio::test]
async fn new_successful_nonremembered_login_disarms_previous_even_when_keyring_is_down() {
    let fixture = Fixture::new();
    let store = &fixture.store;
    let keyring = FakeKeyring::default();
    store
        .record_with(&keyring, &origin(), "listener", Some(SYNTHETIC_SECRET))
        .await
        .unwrap();
    keyring.behavior.set(Behavior::Unavailable);
    let second = Origin::parse("https://other.example").unwrap();
    assert_eq!(
        store
            .record_with(&keyring, &second, "new listener", None)
            .await
            .err(),
        Some(StoreError::KeyringUnavailable)
    );
    let profile = store.profile().unwrap().unwrap();
    assert_eq!(profile.origin.as_str(), second.as_str());
    assert_eq!(profile.username, "new listener");
    assert!(!profile.remembered);
}

#[tokio::test]
async fn rejects_aliases_and_insecure_or_invalid_metadata_before_keyring_access() {
    let fixture = Fixture::new();
    let store = &fixture.store;
    let keyring = FakeKeyring::default();
    let external = fixture.base.join("external");
    stdfs::write(&external, b"external").unwrap();
    symlink(&external, fixture.path(METADATA)).unwrap();
    assert_eq!(store.profile().err(), Some(StoreError::Config));
    assert_eq!(
        store
            .record_with(&keyring, &origin(), "listener", Some(SYNTHETIC_SECRET))
            .await
            .err(),
        Some(StoreError::Config)
    );
    assert_eq!(stdfs::read(&external).unwrap(), b"external");
    stdfs::remove_file(fixture.path(METADATA)).unwrap();
    store
        .record_with(&keyring, &origin(), "listener", None)
        .await
        .unwrap();
    stdfs::set_permissions(fixture.path(METADATA), stdfs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(store.profile().err(), Some(StoreError::Config));
    stdfs::set_permissions(fixture.path(METADATA), stdfs::Permissions::from_mode(0o600)).unwrap();
    for bad in [
        br#"{"version":1,"origin":"https://MUSIC.example","username":"listener"}"#.as_slice(),
        br#"{"version":1,"origin":"https://music.example","username":"listener","cookie":"anything"}"#,
        br#"{"version":1,"origin":"https://music.example","username":"listener","active":"not-a-slot"}"#,
    ] {
        stdfs::write(fixture.path(METADATA), bad).unwrap();
        assert_eq!(store.profile().err(), Some(StoreError::Config));
    }
    assert_eq!(keyring.calls.get(), (0, 0, 0));
    let base = fixture.base.join("alias-base");
    stdfs::DirBuilder::new().mode(0o700).create(&base).unwrap();
    symlink(fixture.base.join(ROOT), base.join(ROOT)).unwrap();
    assert_eq!(config_dir(&base).err(), Some(StoreError::Config));
    assert_eq!(
        config_child(&fixture.store.dir, OsStr::new("no-such-parent/entry")).err(),
        Some(StoreError::Config)
    );
}

const ONLINE_LOGIN: &[u8] = br#"{"origin":"https://music.example","accountId":"synthetic-account","cookie":"__Host-spofify_session=synthetic-cookie"}"#;
const LOCAL_LOGIN: &[u8] = br#"{"format":"local-v1","username":"listener","role":"user","credential":{"origin":"https://music.example","accountId":"synthetic-account","cookie":"__Host-spofify_session=synthetic-cookie"}}"#;

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[tokio::test]
async fn startup_update_permission_uses_the_saved_mode_before_account_admission() {
    use crate::{
        auth,
        mode::{ModeControl, UpdateNetwork},
    };
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };

    let wrong_origin = br#"{"origin":"https://wrong.example","accountId":"synthetic-account","cookie":"__Host-spofify_session=synthetic-cookie"}"#;
    let invalid_cookie = br#"{"origin":"https://music.example","accountId":"synthetic-account","cookie":"unrecognized=synthetic-cookie"}"#;
    for (saved_record, unavailable, expected_online) in [
        (None, false, true),
        (Some(ONLINE_LOGIN), false, true),
        (Some(LOCAL_LOGIN), false, false),
        (Some(LOCAL_LOGIN), true, false),
        (Some(wrong_origin.as_slice()), false, false),
        (Some(invalid_cookie.as_slice()), false, false),
    ] {
        let fixture = Fixture::new();
        let store = &fixture.store;
        let keyring = FakeKeyring::default();
        if let Some(blob) = saved_record {
            store
                .record_with(&keyring, &origin(), "listener", Some(blob))
                .await
                .unwrap();
        }
        if unavailable {
            keyring.behavior.set(Behavior::Unavailable);
        }
        let mode = ModeControl::default();
        let network = mode.update_network();
        let current = AtomicU64::new(1);
        let outcome = auth::restore_with(
            Ok(Arc::new(fixture.reopen())),
            1,
            &current,
            || {
                mode.resolve_update_startup(1, &current);
                // Cancel account restoration at this point. The gate must
                // already be open with no HTTP call or account admission.
                current.store(2, Ordering::SeqCst);
            },
            |_, _| std::future::ready(()),
            async |store, profile| {
                store
                    .restore_with(&keyring, profile)
                    .await?
                    .map(|bytes| {
                        serde_json::from_slice(&bytes).map_err(|_| StoreError::InvalidCredential)
                    })
                    .transpose()
            },
        )
        .await;
        if saved_record == Some(LOCAL_LOGIN) && !unavailable {
            assert!(outcome.ok().unwrap().session.is_local_only());
        } else {
            assert!(outcome.is_err());
        }
        assert_eq!(keyring.calls.get().0, usize::from(saved_record.is_some()));
        assert_eq!(
            matches!(*network.borrow(), UpdateNetwork::Online),
            expected_online
        );
    }
}

#[tokio::test]
async fn explicit_mode_rotates_only_the_active_slot_and_keeps_disk_metadata_non_authoritative() {
    let fixture = Fixture::new();
    let store = &fixture.store;
    let keyring = FakeKeyring::default();
    let online = store
        .record_with(&keyring, &origin(), "listener", Some(ONLINE_LOGIN))
        .await
        .unwrap();
    let offline: RememberedLogin = serde_json::from_slice(LOCAL_LOGIN).unwrap();
    let local = store
        .change_mode_with(&keyring, &online, &offline, None)
        .await
        .unwrap();
    assert!(!store.is_active(&online).unwrap());
    assert!(store.is_active(&local).unwrap());
    assert_eq!(
        store.stage_active_mode(Some(&online)).err(),
        Some(StoreError::InvalidCredential)
    );
    assert_eq!(
        store.stage_active_mode(None).err(),
        Some(StoreError::InvalidCredential)
    );
    assert!(
        store
            .restore_with(&keyring, &online)
            .await
            .unwrap()
            .is_none()
    );
    let blob = store.restore_with(&keyring, &local).await.unwrap().unwrap();
    assert!(matches!(
        serde_json::from_slice(&blob),
        Ok(RememberedLogin::Offline(_))
    ));
    assert!(
        store
            .change_mode_with(&keyring, &online, &offline, None)
            .await
            .is_err()
    );
    let disk: serde_json::Value =
        serde_json::from_slice(&stdfs::read(fixture.path(METADATA)).unwrap()).unwrap();
    assert_eq!(
        disk,
        serde_json::json!({"version": 1, "origin": origin().as_str(), "username": "listener", "active": String::from(local.active_slot.unwrap())})
    );
    let staged = store.stage_mode(&local).unwrap();
    assert!(!fixture.reopen().is_active(&local).unwrap());
    assert!(store.stage_active_mode(Some(&local)).unwrap().is_none());
    let online = store
        .change_mode_with(
            &keyring,
            &local,
            &serde_json::from_slice(ONLINE_LOGIN).unwrap(),
            Some(&staged),
        )
        .await
        .unwrap();
    assert!(
        store
            .restore_with(&keyring, &local)
            .await
            .unwrap()
            .is_none()
    );
    let blob = store
        .restore_with(&keyring, &online)
        .await
        .unwrap()
        .unwrap();
    assert!(serde_json::from_slice::<RememberedCredential>(&blob).is_ok());
    assert_eq!(keyring.items.borrow().len(), 1);
    store.forget_with(&keyring, Some(&online)).await.unwrap();
    assert!(
        store
            .restore_with(&keyring, &online)
            .await
            .unwrap()
            .is_none()
    );
    assert!(keyring.items.borrow().is_empty());
}

#[tokio::test]
async fn interrupted_or_missing_slot_cannot_fall_back_to_its_old_mode_or_create_a_grant() {
    for behavior in [
        Some(Behavior::Unavailable),
        Some(Behavior::InterruptStore),
        None,
    ] {
        let fixture = Fixture::new();
        let store = &fixture.store;
        let keyring = FakeKeyring::default();
        let initial = store
            .record_with(&keyring, &origin(), "listener", Some(ONLINE_LOGIN))
            .await
            .unwrap();
        let offline = serde_json::from_slice(LOCAL_LOGIN).unwrap();
        match behavior {
            Some(Behavior::InterruptStore) => {
                keyring.behavior.set(Behavior::InterruptStore);
                cancel_after_first_poll(store.change_mode_with(&keyring, &initial, &offline, None))
                    .await;
            }
            behavior => {
                if let Some(behavior) = behavior {
                    keyring.behavior.set(behavior);
                } else {
                    keyring.items.borrow_mut().clear();
                }
                assert!(
                    store
                        .change_mode_with(&keyring, &initial, &offline, None)
                        .await
                        .is_err()
                );
                assert_eq!(keyring.calls.get().1, 1); // no new keyring payload
            }
        }
        let reopened = fixture.reopen();
        assert!(!reopened.profile().unwrap().unwrap().remembered);
        assert!(reopened.read().unwrap().unwrap().cleanup.is_some());
        assert!(
            reopened
                .restore_with(&keyring, &initial)
                .await
                .unwrap()
                .is_none()
        );
    }
}

#[tokio::test]
async fn the_same_admitted_owner_can_retry_its_stage_after_keyring_unlock() {
    let fixture = Fixture::new();
    let store = &fixture.store;
    let keyring = FakeKeyring::default();
    let online = store
        .record_with(&keyring, &origin(), "listener", Some(ONLINE_LOGIN))
        .await
        .unwrap();
    let offline = serde_json::from_slice(LOCAL_LOGIN).unwrap();
    let stage = store.stage_mode(&online).unwrap();
    keyring.behavior.set(Behavior::Unavailable);
    assert_eq!(
        store
            .change_mode_with(&keyring, &online, &offline, Some(&stage))
            .await
            .err(),
        Some(StoreError::KeyringUnavailable)
    );
    assert!(!fixture.reopen().is_active(&online).unwrap());
    keyring.behavior.set(Behavior::Available);
    assert!(
        store
            .change_mode_with(&keyring, &online, &offline, None)
            .await
            .is_err()
    );
    let local = store
        .change_mode_with(&keyring, &online, &offline, Some(&stage))
        .await
        .unwrap();
    assert!(store.is_active(&local).unwrap());
}

#[tokio::test]
async fn revoked_admission_can_never_publish_a_pending_keyring_write() {
    let fixture = Fixture::new();
    let store = &fixture.store;
    let keyring = FakeKeyring::default();
    let online = store
        .record_with(&keyring, &origin(), "listener", Some(ONLINE_LOGIN))
        .await
        .unwrap();
    let offline = serde_json::from_slice(LOCAL_LOGIN).unwrap();
    let staged = store.stage_mode(&online).unwrap();
    assert_eq!(
        store
            .change_mode_with_permit(&keyring, &online, &offline, &staged, &|| false)
            .await
            .err(),
        Some(StoreError::InvalidCredential)
    );
    let meta = fixture.reopen().read().unwrap().unwrap();
    assert!(meta.active.is_none() && meta.cleanup.is_some());
    assert_eq!(keyring.items.borrow().len(), 1); // Stored keyring item remains inaccessible.
    assert!(!store.is_active(&online).unwrap());
}

#[test]
fn installation_ownership_is_exclusive_for_the_config_directory() {
    let fixture = Fixture::new();
    let contender = config_dir(&fixture.base).unwrap();
    let owner = config_dir(&fixture.base).unwrap();
    lock_exclusive(&owner).unwrap();
    assert_eq!(lock_exclusive(&contender), Err(StoreError::Busy));
    drop(owner);
    lock_exclusive(&contender).unwrap();
}

#[tokio::test]
async fn serializes_mutations_without_blocking_prefill_read() {
    let fixture = Fixture::new();
    let store = &fixture.store;
    let keyring = FakeKeyring::default();
    store
        .record_with(&keyring, &origin(), "listener", None)
        .await
        .unwrap();
    let lock = store.lock().unwrap();
    let contender = fixture.reopen();
    assert_eq!(
        contender.forget_with(&keyring, None).await.unwrap_err(),
        StoreError::Busy
    );
    assert_eq!(contender.profile().unwrap().unwrap().username, "listener");
    drop(lock);
    contender.forget_with(&keyring, None).await.unwrap();
}
