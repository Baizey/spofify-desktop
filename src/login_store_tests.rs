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

fn origin() -> Origin {
    Origin::parse("HTTPS://Music.Example:443/").unwrap()
}

async fn cancel_after_first_poll(future: impl Future<Output = Result<()>>) {
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
            .unwrap_err(),
        StoreError::KeyringUnavailable
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
            .await,
        Err(StoreError::Config)
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
