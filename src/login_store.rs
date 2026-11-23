//! Last successful login prefill plus an opt-in Secret Service credential.
//!
//! Disk stores only the prefill and an opaque random slot. `cleanup` is a
//! write-ahead and retirement marker, never a restore capability. Both a new
//! slot and an old slot being retired are durably marked before any keyring
//! mutation. Only a confirmed store can publish an `active` slot. Deletion is
//! local-first: an unavailable keyring leaves a non-restorable cleanup marker.

use std::{
    env, fmt,
    fs::{self as stdfs, File},
    io::{ErrorKind, Read, Write},
    path::{Component, Path, PathBuf},
};

use rustix::{
    fd::OwnedFd,
    fs::{self, AtFlags, FileType, FlockOperation, Mode, OFlags, Stat},
    io::Errno,
    process::geteuid,
};
use serde::{Deserialize, Serialize};

use crate::{
    api::{Origin, RememberedLogin},
    storage::{DIR_FLAGS, private_dir},
};

const ROOT: &str = "spofify-desktop-login-v1";
const METADATA: &str = "login.json";
const LOCK: &str = ".login.lock";
const STAGE: &str = ".login.stage";
const MAX_METADATA: usize = 8192;
const MAX_SECRET: usize = 16 * 1024;
const FILE_MODE: Mode = Mode::RUSR.union(Mode::WUSR);
const FILE_FLAGS: OFlags = OFlags::CLOEXEC
    .union(OFlags::NOFOLLOW)
    .union(OFlags::NONBLOCK);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StoreError {
    Config,
    Busy,
    InvalidInput,
    KeyringUnavailable,
    InvalidCredential,
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Config => "Unable to access the private login settings.",
            Self::Busy => "The login settings are busy. Try again.",
            Self::InvalidInput => "Unable to save these login settings.",
            Self::KeyringUnavailable => "The system keyring is unavailable or locked.",
            Self::InvalidCredential => "The saved login credential is invalid.",
        })
    }
}

impl std::error::Error for StoreError {}

type Result<T> = std::result::Result<T, StoreError>;
type Permit<'a> = &'a (dyn Fn() -> bool + Sync);

/// Prefill is usable without contacting either the server or the keyring.
#[derive(Clone)]
pub(crate) struct LoginProfile {
    pub(crate) origin: Origin,
    pub(crate) username: String,
    pub(crate) remembered: bool,
    active_slot: Option<Slot>,
}

#[cfg(test)]
impl LoginProfile {
    pub(crate) fn test_remembered() -> Self {
        Self {
            origin: Origin::parse("http://127.0.0.1:1").unwrap(),
            username: "synthetic".into(),
            remembered: true,
            active_slot: Some(Slot::new().unwrap()),
        }
    }
}

pub(crate) struct LoginStore {
    dir: OwnedFd,
}

/// The old active slot was durably disarmed before admitting online authority.
#[derive(Clone)]
pub(crate) struct StagedMode(LoginProfile);

impl StagedMode {
    pub(crate) fn matches(&self, profile: &LoginProfile) -> bool {
        self.0.active_slot == profile.active_slot
            && self.0.origin.as_str() == profile.origin.as_str()
            && self.0.username == profile.username
    }
}

impl LoginStore {
    pub(crate) fn open_default() -> Result<Self> {
        let absolute = |name: &str| {
            env::var_os(name)
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
        };
        let base = absolute("XDG_CONFIG_HOME")
            .or_else(|| absolute("HOME").map(|home| home.join(".config")))
            .ok_or(StoreError::Config)?;
        let dir = config_dir(&base)?;
        // A single running Desktop owns remembered authority for this config
        // root. Lock the directory inode (not a removable lock pathname) so
        // another process cannot retire an active grant behind its owner.
        lock_exclusive(&dir)?;
        Ok(Self { dir })
    }

    pub(crate) fn profile(&self) -> Result<Option<LoginProfile>> {
        self.read()?.map(|meta| meta.profile()).transpose()
    }

    /// Read only the currently active item. Only the versioned LocalGrant variant
    /// (never the disk prefill or a legacy bearer) can be admitted offline.
    pub(crate) async fn restore(&self, expected: &LoginProfile) -> Result<Option<RememberedLogin>> {
        self.restore_with(&SecretService, expected)
            .await?
            .map(|bytes| serde_json::from_slice(&bytes).map_err(|_| StoreError::InvalidCredential))
            .transpose()
    }

    /// Call only for a successful, current login. `None` records prefill and
    /// disables any previous remembered login. Prefill persists even if the
    /// keyring step subsequently fails; if enable fails there is no new active slot.
    pub(crate) async fn record_login(
        &self,
        origin: &Origin,
        username: &str,
        login: Option<&RememberedLogin>,
        permitted: Permit<'_>,
    ) -> Result<LoginProfile> {
        if let Some(login) = login {
            let RememberedLogin::Offline(grant) = login else {
                return Err(StoreError::InvalidInput);
            };
            grant
                .validate(origin, username)
                .map_err(|_| StoreError::InvalidInput)?;
        }
        let blob = login
            .map(serde_json::to_vec)
            .transpose()
            .map_err(|_| StoreError::InvalidInput)?;
        self.record_with_permit(&SecretService, origin, username, blob.as_deref(), permitted)
            .await
    }

    /// Re-check admission after the async keyring restore and UI acceptance.
    pub(crate) fn is_active(&self, expected: &LoginProfile) -> Result<bool> {
        let _lock = self.lock()?;
        config(fs::fsync(&self.dir))?;
        Ok(self
            .read()?
            .is_some_and(|meta| meta.active_matches(expected)))
    }

    pub(crate) fn stage_mode(&self, expected: &LoginProfile) -> Result<StagedMode> {
        self.stage_active_mode(Some(expected))?
            .ok_or(StoreError::InvalidCredential)
    }

    /// A missing active slot is not interchangeable with a mismatched slot.
    /// The latter may still hold a cold grant after a failed publication sync.
    pub(crate) fn stage_active_mode(
        &self,
        expected: Option<&LoginProfile>,
    ) -> Result<Option<StagedMode>> {
        let _lock = self.lock()?;
        let Some(mut old) = self.read()?.filter(|meta| meta.active.is_some()) else {
            config(fs::fsync(&self.dir))?;
            return Ok(None);
        };
        let expected = expected
            .filter(|expected| old.active_matches(expected))
            .ok_or(StoreError::InvalidCredential)?;
        old.cleanup = old.active.take();
        self.commit(&old)?;
        Ok(Some(StagedMode(expected.clone())))
    }

    pub(crate) fn retire(&self, expected: Option<&LoginProfile>) -> Result<()> {
        let _lock = self.lock()?;
        self.retire_active(expected).map(|_| ())
    }

    /// Change the preference already bound to this account. A cold local
    /// session has no bearer or deadline to export: the exact staged keyring
    /// record is read and its format is the only grant field changed.
    pub(crate) async fn change_preference(
        &self,
        expected: &LoginProfile,
        staged: &StagedMode,
        account: &str,
        manual_offline: bool,
        permitted: Permit<'_>,
    ) -> Result<LoginProfile> {
        self.replace_remembered(
            &SecretService,
            expected,
            Some(staged),
            |current| preferred(current, expected, account, manual_offline),
            permitted,
        )
        .await
    }

    /// Upgrade only the existing legacy bearer after the engine accepted its
    /// HTTP-validated session. Never substitute a different cookie or account.
    pub(crate) async fn upgrade_legacy(
        &self,
        expected: &LoginProfile,
        login: &RememberedLogin,
        permitted: Permit<'_>,
    ) -> Result<LoginProfile> {
        self.replace_remembered(
            &SecretService,
            expected,
            None,
            |current| upgraded(current, expected, login),
            permitted,
        )
        .await
    }

    async fn replace_remembered(
        &self,
        keyring: &impl Keyring,
        expected: &LoginProfile,
        staged: Option<&StagedMode>,
        update: impl FnOnce(RememberedLogin) -> Result<RememberedLogin>,
        permitted: Permit<'_>,
    ) -> Result<LoginProfile> {
        let _lock = self.lock()?;
        if !permitted() {
            return Err(StoreError::InvalidCredential);
        }
        let mut old = self.read()?.ok_or(StoreError::InvalidCredential)?;
        let slot = match staged {
            Some(StagedMode(staged))
                if old.active.is_none()
                    && old.cleanup == expected.active_slot
                    && expected.active_slot == staged.active_slot
                    && old.matches(expected)
                    && old.matches(staged) =>
            {
                old.cleanup
            }
            None if old.active_matches(expected) => old.active,
            _ => None,
        }
        .ok_or(StoreError::InvalidCredential)?;
        config(fs::fsync(&self.dir))?;
        let current = keyring
            .lookup(slot)
            .await?
            .filter(|b| valid_secret(b))
            .ok_or(StoreError::InvalidCredential)?;
        let current =
            serde_json::from_slice(&current).map_err(|_| StoreError::InvalidCredential)?;
        let replacement = update(current)?;
        let blob = serde_json::to_vec(&replacement).map_err(|_| StoreError::InvalidInput)?;
        if !valid_secret(&blob) {
            return Err(StoreError::InvalidInput);
        }
        if !permitted() {
            return Err(StoreError::InvalidCredential);
        }
        // An upgrade validates the still-active legacy record before disarming
        // it. Other writes can supply their prior exact, durably disarmed slot.
        if old.active.is_some() {
            old.cleanup = old.active.take();
            self.commit(&old)?;
        }
        self.finish_cleanup(keyring, &mut old).await?;
        self.publish_keyring(keyring, old, Some(&blob), permitted)
            .await
    }

    /// Disable restore and remove the exact keyring item (keep prefill). An
    /// unavailable keyring leaves cleanup pending; calling `forget` again retries.
    pub(crate) async fn forget(&self) -> Result<()> {
        self.forget_with(&SecretService, None).await
    }

    /// Conditional cleanup after a definitive rejection of a restore candidate;
    /// a stale candidate cannot retire a more recent login.
    pub(crate) async fn forget_profile(&self, expected: &LoginProfile) -> Result<()> {
        self.forget_with(&SecretService, Some(expected)).await
    }

    async fn restore_with(
        &self,
        keyring: &impl Keyring,
        expected: &LoginProfile,
    ) -> Result<Option<Vec<u8>>> {
        if !expected.remembered || expected.active_slot.is_none() {
            return Ok(None);
        }
        let _lock = self.lock()?;
        let Some(meta) = self.read()? else {
            return Ok(None);
        };
        if !meta.active_matches(expected) {
            return Ok(None);
        }
        let slot = meta.active.unwrap();
        // Do not use state from a rename whose directory sync may have failed.
        config(fs::fsync(&self.dir))?;
        let found = keyring.lookup(slot).await?;
        if found.as_ref().is_some_and(|bytes| !valid_secret(bytes)) {
            return Err(StoreError::InvalidCredential);
        }
        Ok(found)
    }

    async fn record_with_permit(
        &self,
        keyring: &impl Keyring,
        origin: &Origin,
        username: &str,
        blob: Option<&[u8]>,
        permitted: Permit<'_>,
    ) -> Result<LoginProfile> {
        if !valid_username(username) || blob.is_some_and(|b| !valid_secret(b)) {
            return Err(StoreError::InvalidInput);
        }
        let _lock = self.lock()?;
        let old = self.read()?;
        let mut meta = Metadata {
            version: 1,
            origin: origin.as_str().into(),
            username: username.into(),
            active: None,
            cleanup: old.and_then(|meta| meta.active.or(meta.cleanup)),
        };
        // Always commit the latest prefill and disarm the old slot, even if
        // cleanup needs to wait for the keyring. Don't accumulate stale slots.
        self.commit(&meta)?;
        self.finish_cleanup(keyring, &mut meta).await?;
        self.publish_keyring(keyring, meta, blob, permitted).await
    }

    async fn publish_keyring(
        &self,
        keyring: &impl Keyring,
        mut meta: Metadata,
        blob: Option<&[u8]>,
        permitted: Permit<'_>,
    ) -> Result<LoginProfile> {
        if let Some(blob) = blob {
            let slot = Slot::new()?;
            meta.cleanup = Some(slot);
            self.commit(&meta)?;
            keyring.store(slot, blob).await?;
            meta.cleanup = None;
            meta.active = Some(slot);
            if let Err(error) = self.commit_guarded(&meta, Some(permitted)) {
                // If rename succeeded but the directory sync failed, explicitly
                // disarm the potentially visible new slot before reporting failure.
                if error != StoreError::InvalidCredential {
                    meta.cleanup = meta.active.take();
                    let _ = self.commit(&meta);
                }
                return Err(error);
            }
        }
        meta.profile()
    }

    async fn forget_with(
        &self,
        keyring: &impl Keyring,
        expected: Option<&LoginProfile>,
    ) -> Result<()> {
        let _lock = self.lock()?;
        if let Some(mut meta) = self.retire_active(expected)? {
            self.finish_cleanup(keyring, &mut meta).await?;
        }
        Ok(())
    }

    fn retire_active(&self, expected: Option<&LoginProfile>) -> Result<Option<Metadata>> {
        let Some(mut meta) = self.read()? else {
            return Ok(None);
        };
        if expected.is_some_and(|expected| {
            !expected.remembered
                || !meta.matches(expected)
                || meta.active.or(meta.cleanup) != expected.active_slot
        }) {
            return Ok(None);
        }
        if let Some(slot) = meta.active.take() {
            meta.cleanup = Some(slot);
            self.commit(&meta)?;
        }
        Ok(Some(meta))
    }

    async fn finish_cleanup(&self, keyring: &impl Keyring, meta: &mut Metadata) -> Result<()> {
        if let Some(slot) = meta.cleanup {
            config(fs::fsync(&self.dir))?;
            keyring.clear(slot).await?;
            meta.cleanup = None;
            self.commit(meta)?;
        }
        Ok(())
    }

    fn lock(&self) -> Result<OwnedFd> {
        config(private_dir(&self.dir))?;
        let fd = config(fs::openat(
            &self.dir,
            LOCK,
            FILE_FLAGS | OFlags::RDWR | OFlags::CREATE,
            FILE_MODE,
        ))?;
        // Separate open-file description per admission, even on the same store.
        if private_len(&config(fs::fstat(&fd))?, false)? != 0 {
            return Err(StoreError::Config);
        }
        lock_exclusive(&fd)?;
        Ok(fd)
    }

    fn read(&self) -> Result<Option<Metadata>> {
        config(private_dir(&self.dir))?;
        let fd = match fs::openat(
            &self.dir,
            METADATA,
            FILE_FLAGS | OFlags::RDONLY,
            Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(Errno::NOENT) => return Ok(None),
            Err(_) => return Err(StoreError::Config),
        };
        let len = private_len(&config(fs::fstat(&fd))?, true)?;
        if !(1..=MAX_METADATA as u64).contains(&len) {
            return Err(StoreError::Config);
        }
        let mut bytes = Vec::with_capacity(len as usize);
        File::from(fd)
            .take(len + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| StoreError::Config)?;
        if bytes.len() as u64 != len {
            return Err(StoreError::Config);
        }
        let meta: Metadata = serde_json::from_slice(&bytes).map_err(|_| StoreError::Config)?;
        meta.validate()?;
        Ok(Some(meta))
    }

    /// Requires admission. The abandoned stage holds metadata only. Never open
    /// it by following links. Sync data before publication and the directory
    /// after publication, before the operation can touch the keyring.
    fn commit(&self, meta: &Metadata) -> Result<()> {
        self.commit_guarded(meta, None)
    }

    fn commit_guarded(&self, meta: &Metadata, permitted: Option<Permit<'_>>) -> Result<()> {
        let bytes = serde_json::to_vec(meta).map_err(|_| StoreError::Config)?;
        if bytes.len() > MAX_METADATA {
            return Err(StoreError::Config);
        }
        match fs::unlinkat(&self.dir, STAGE, AtFlags::empty()) {
            Ok(()) | Err(Errno::NOENT) => (),
            Err(_) => return Err(StoreError::Config),
        }
        let fd = config(fs::openat(
            &self.dir,
            STAGE,
            FILE_FLAGS | OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL,
            FILE_MODE,
        ))?;
        let temp = Stage(&self.dir);
        config(fs::fchmod(&fd, FILE_MODE))?;
        let mut file = File::from(fd);
        file.write_all(&bytes)
            .and_then(|()| file.sync_all())
            .map_err(|_| StoreError::Config)?;
        {
            // Known revocation and active-slot publication share the rename
            // fence; never hold it across a keyring operation or sync.
            let _publication =
                permitted.map(|_| crate::playlist_pin_store::PUBLICATION_GATE.lock().unwrap());
            if permitted.is_some_and(|allowed| !allowed()) {
                return Err(StoreError::InvalidCredential);
            }
            config(fs::renameat(&self.dir, STAGE, &self.dir, METADATA))?;
        }
        drop(temp);
        config(fs::fsync(&self.dir))
    }
}

fn preferred(
    current: RememberedLogin,
    expected: &LoginProfile,
    account: &str,
    manual_offline: bool,
) -> Result<RememberedLogin> {
    let RememberedLogin::Offline(mut grant) = current else {
        return Err(StoreError::InvalidCredential);
    };
    grant
        .validate(&expected.origin, &expected.username)
        .map_err(|_| StoreError::InvalidCredential)?;
    if !grant.matches_principal(&expected.username, account) {
        return Err(StoreError::InvalidCredential);
    }
    grant.set_manual_offline(manual_offline);
    Ok(RememberedLogin::Offline(grant))
}

fn upgraded(
    current: RememberedLogin,
    expected: &LoginProfile,
    login: &RememberedLogin,
) -> Result<RememberedLogin> {
    let (RememberedLogin::Online(legacy), RememberedLogin::Offline(grant)) = (current, login)
    else {
        return Err(StoreError::InvalidCredential);
    };
    grant
        .validate(&expected.origin, &expected.username)
        .map_err(|_| StoreError::InvalidCredential)?;
    if grant.manual_offline() || &legacy != login.credential() {
        return Err(StoreError::InvalidCredential);
    }
    // The server-validated candidate is in memory; never derive a grant from
    // an unvalidated bearer. Clone only this exact candidate for publication.
    Ok(login.clone())
}

struct Stage<'a>(&'a OwnedFd);
impl Drop for Stage<'_> {
    fn drop(&mut self) {
        let _ = fs::unlinkat(self.0, STAGE, AtFlags::empty());
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Metadata {
    version: u8,
    origin: String,
    username: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    active: Option<Slot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cleanup: Option<Slot>,
}

impl Metadata {
    fn validate(&self) -> Result<()> {
        if self.version != 1
            || !valid_username(&self.username)
            || (self.active.is_some() && self.cleanup.is_some())
        {
            return Err(StoreError::Config);
        }
        self.origin()?;
        Ok(())
    }

    fn origin(&self) -> Result<Origin> {
        let origin = Origin::parse(&self.origin).map_err(|_| StoreError::Config)?;
        if origin.as_str() != self.origin {
            return Err(StoreError::Config);
        }
        Ok(origin)
    }

    fn profile(self) -> Result<LoginProfile> {
        Ok(LoginProfile {
            origin: self.origin()?,
            username: self.username,
            remembered: self.active.is_some(),
            active_slot: self.active,
        })
    }

    fn active_matches(&self, expected: &LoginProfile) -> bool {
        self.active.is_some() && self.active == expected.active_slot && self.matches(expected)
    }

    fn matches(&self, expected: &LoginProfile) -> bool {
        self.origin == expected.origin.as_str() && self.username == expected.username
    }
}

#[derive(Clone, Copy, Eq, PartialEq, Deserialize, Serialize)]
#[serde(try_from = "String", into = "String")]
struct Slot(uuid::Uuid);

impl Slot {
    fn new() -> Result<Self> {
        // Fail closed if system randomness is unavailable instead of panicking.
        use ring::rand::{SecureRandom, SystemRandom};
        let mut raw = [0; 16];
        SystemRandom::new()
            .fill(&mut raw)
            .map_err(|_| StoreError::Config)?;
        Ok(Self(uuid::Builder::from_random_bytes(raw).into_uuid()))
    }
}

impl TryFrom<String> for Slot {
    type Error = &'static str;
    fn try_from(encoded: String) -> std::result::Result<Self, Self::Error> {
        let id = uuid::Uuid::parse_str(&encoded).map_err(|_| "invalid login slot")?;
        if id.get_version_num() != 4
            || id.get_variant() != uuid::Variant::RFC4122
            || id.simple().to_string() != encoded
        {
            return Err("invalid login slot");
        }
        Ok(Self(id))
    }
}

impl From<Slot> for String {
    fn from(slot: Slot) -> Self {
        slot.0.simple().to_string()
    }
}

fn valid_username(value: &str) -> bool {
    !value.trim().is_empty() && value.len() <= 256 && !value.chars().any(char::is_control)
}

fn valid_secret(bytes: &[u8]) -> bool {
    (1..=MAX_SECRET).contains(&bytes.len())
}

fn lock_exclusive(fd: &OwnedFd) -> Result<()> {
    fs::flock(fd, FlockOperation::NonBlockingLockExclusive).map_err(|e| {
        if e == Errno::WOULDBLOCK {
            StoreError::Busy
        } else {
            StoreError::Config
        }
    })
}

fn config<T>(result: rustix::io::Result<T>) -> Result<T> {
    result.map_err(|_| StoreError::Config)
}

fn private_len(stat: &Stat, allow_unlinked: bool) -> Result<u64> {
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile
        || stat.st_uid != geteuid().as_raw()
        || stat.st_mode & 0o7777 != 0o600
        || stat.st_nlink > 1
        || (!allow_unlinked && stat.st_nlink == 0)
    {
        return Err(StoreError::Config);
    }
    u64::try_from(stat.st_size).map_err(|_| StoreError::Config)
}

/// Respect aliases in the user-selected XDG base (e.g. /home -> /var/home),
/// but never in the app root or entries. Missing suffixes and the app leaf
/// are opened relative to descriptors, and parents synced before use.
fn config_dir(base: &Path) -> Result<OwnedFd> {
    if !absolute_clean(base) {
        return Err(StoreError::Config);
    }
    let mut ancestor = base;
    let mut missing = Vec::new();
    let resolved = loop {
        match stdfs::canonicalize(ancestor) {
            Ok(path) if path != Path::new("/") => break path,
            Err(e) if e.kind() == ErrorKind::NotFound => {
                missing.push(ancestor.file_name().ok_or(StoreError::Config)?);
                ancestor = ancestor.parent().ok_or(StoreError::Config)?;
            }
            _ => return Err(StoreError::Config),
        }
    };
    let mut dir = config(fs::open("/", DIR_FLAGS, Mode::empty()))?;
    for part in resolved.components().skip(1) {
        let Component::Normal(name) = part else {
            return Err(StoreError::Config);
        };
        dir = config(fs::openat(&dir, name, DIR_FLAGS, Mode::empty()))?;
    }
    for name in missing.into_iter().rev() {
        dir = config_child(&dir, name)?;
    }
    let root = config_child(&dir, ROOT.as_ref())?;
    config(private_dir(&root))?;
    Ok(root)
}

fn absolute_clean(path: &Path) -> bool {
    let mut parts = path.components();
    parts.next() == Some(Component::RootDir)
        && parts.all(|part| matches!(part, Component::Normal(_)))
}

fn config_child(dir: &OwnedFd, name: &std::ffi::OsStr) -> Result<OwnedFd> {
    let stat = config(fs::fstat(dir))?;
    if stat.st_uid != geteuid().as_raw() || stat.st_mode & 0o022 != 0 {
        return Err(StoreError::Config);
    }
    let next = match fs::openat(dir, name, DIR_FLAGS, Mode::empty()) {
        Err(Errno::NOENT) => {
            match fs::mkdirat(dir, name, Mode::RWXU) {
                Ok(()) | Err(Errno::EXIST) => (),
                Err(_) => return Err(StoreError::Config),
            }
            config(fs::openat(dir, name, DIR_FLAGS, Mode::empty()))?
        }
        other => config(other)?,
    };
    config(fs::fsync(dir))?;
    Ok(next)
}

trait Keyring {
    async fn lookup(&self, slot: Slot) -> Result<Option<Vec<u8>>>;
    async fn store(&self, slot: Slot, blob: &[u8]) -> Result<()>;
    async fn clear(&self, slot: Slot) -> Result<()>;
}

struct SecretService;

#[cfg(target_os = "linux")]
#[path = "login_store_secret_service.rs"]
mod secret_service;

#[cfg(not(target_os = "linux"))]
impl Keyring for SecretService {
    async fn lookup(&self, _: Slot) -> Result<Option<Vec<u8>>> {
        Err(StoreError::KeyringUnavailable)
    }
    async fn store(&self, _: Slot, _: &[u8]) -> Result<()> {
        Err(StoreError::KeyringUnavailable)
    }
    async fn clear(&self, _: Slot) -> Result<()> {
        Err(StoreError::KeyringUnavailable)
    }
}

#[cfg(test)]
#[path = "login_store_tests.rs"]
mod tests;
