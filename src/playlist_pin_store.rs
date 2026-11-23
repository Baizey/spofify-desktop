//! Private playlist intent and its last successfully published membership.
//! Neither a pin nor its tombstone owns audio or modifies legacy collections.

use std::{
    collections::HashSet,
    fs::File,
    io::{BufRead, BufReader, BufWriter, ErrorKind, Read, Write},
    mem::MaybeUninit,
};

use ring::{
    digest::{Context, SHA256},
    rand::{SecureRandom, SystemRandom},
};
use rustix::{
    fd::OwnedFd,
    fs::{self, AtFlags, FileType, FlockOperation, Mode, OFlags, RawDir, Stat},
    io::Errno,
    process::geteuid,
};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{
    api::{ApiError, library::PlaylistKind},
    collections::playlist_key,
    desk::Access,
    storage::{self, DIR_FLAGS},
};

/// Serializes the final publication check and rename with external epoch/network changes.
pub(crate) static PUBLICATION_GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());

const ROOT: &str = "spofify-playlist-pins-v1";
const STAGE: &str = ".partial";
const VERSION: u8 = 1;
const PRIVATE_FILE: Mode = Mode::RUSR.union(Mode::WUSR);
const FILE_FLAGS: OFlags = OFlags::NOFOLLOW
    .union(OFlags::CLOEXEC)
    .union(OFlags::NONBLOCK);

const fn error(message: &'static str) -> ApiError {
    ApiError {
        message,
        expired: false,
        status: 0,
    }
}
const STORAGE: ApiError = error("Unable to access private playlist pin storage.");
const DAMAGED: ApiError = error("The playlist pin data is invalid.");
const INVALID: ApiError = error("The playlist pin contains invalid playlist information.");
const NO_SPACE: ApiError = error("Not enough space on this device to update playlist pins.");
const WRONG_SCOPE: ApiError =
    error("This playlist pin store belongs to a different account or server.");
const NOT_DURABLE: ApiError =
    error("The playlist pin change was applied, but could not be confirmed on disk.");
const NOT_DURABLE_NO_SPACE: ApiError = error(
    "The playlist pin change was applied, but there was not enough space to confirm it on disk.",
);

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Target {
    pub(crate) id: String,
    pub(crate) title: String,
    #[serde(serialize_with = "serialize_kind")]
    pub(crate) kind: PlaylistKind,
}

fn serialize_kind<S: Serializer>(kind: &PlaylistKind, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(match kind {
        PlaylistKind::Manual => "manual",
        PlaylistKind::Derived => "derived",
    })
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Membership {
    pub(crate) title: String,
    pub(crate) tracks: Vec<String>,
    /// Absent in older pins. Never infer identities from track IDs or positions.
    #[serde(
        default,
        deserialize_with = "deserialize_provenance",
        skip_serializing_if = "Option::is_none"
    )]
    pub(crate) provenance: Option<Provenance>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Provenance {
    pub(crate) revision: u64,
    /// Exact manifest identity in the same order as `Membership::tracks`.
    pub(crate) occurrence_ids: Vec<String>,
}

fn deserialize_provenance<'de, D: Deserializer<'de>>(
    value: D,
) -> Result<Option<Provenance>, D::Error> {
    Provenance::deserialize(value).map(Some)
}

pub(crate) fn fingerprint(membership: &Membership) -> [u8; 32] {
    let mut digest = Context::new(&SHA256);
    if let Some(provenance) = &membership.provenance {
        digest.update(b"spofify-pin-provenance\0");
        digest.update(&provenance.revision.to_be_bytes());
        digest.update(&(provenance.occurrence_ids.len() as u64).to_be_bytes());
        for id in &provenance.occurrence_ids {
            digest.update(&(id.len() as u64).to_be_bytes());
            digest.update(id.as_bytes());
        }
    }
    for field in std::iter::once(&membership.title).chain(&membership.tracks) {
        digest.update(&(field.len() as u64).to_be_bytes());
        digest.update(field.as_bytes());
    }
    digest.finish().as_ref().try_into().expect("SHA-256 length")
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Record {
    pub(crate) target: Target,
    pub(crate) key: String,
    pub(crate) token: String,
    pub(crate) wanted: bool,
    #[serde(deserialize_with = "Deserialize::deserialize")]
    pub(crate) committed: Option<Membership>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DiskRecord {
    version: u8,
    scope: String,
    record: Record,
}

#[derive(Clone)]
pub(crate) struct Store {
    scope: String,
    #[cfg(test)]
    fixture: Option<std::sync::Arc<OwnedFd>>,
}

impl Store {
    /// Bind to an authenticated account without creating or reading a data directory.
    pub(crate) fn open_default(access: &Access) -> Result<Self, ApiError> {
        Ok(Self {
            scope: scope(access)?,
            #[cfg(test)]
            fixture: None,
        })
    }

    /// Use only the explicit fixture path; never resolve XDG or HOME in a fixture.
    #[cfg(test)]
    pub(crate) fn open(path: &std::path::Path, access: &Access) -> Result<Self, ApiError> {
        let mut store = Self::open_default(access)?;
        store.fixture = Some(std::sync::Arc::new(storage_result(storage::open_private(
            path,
        ))?));
        store.check_scope(access)?;
        Ok(store)
    }

    #[cfg(test)]
    fn list(&self, access: &Access) -> Result<Vec<Record>, ApiError> {
        let (records, unreadable) = self.scan(access)?;
        if !unreadable.is_empty() {
            return Err(DAMAGED);
        }
        Ok(records)
    }

    /// Return valid records and the exact keys of unreadable records. Those
    /// keys must not fall back to legacy data: a damaged record may be a tombstone.
    /// Each record is independently atomic, without taking the mutation lock.
    pub(crate) fn scan(&self, access: &Access) -> Result<(Vec<Record>, Vec<String>), ApiError> {
        self.check_scope(access)?;
        let mut records = Vec::new();
        let mut unreadable = Vec::new();
        if let Some(dir) = self.scope_dir(false)? {
            let mut buffer = [MaybeUninit::uninit(); 4096];
            let mut entries = RawDir::new(&dir, &mut buffer);
            while let Some(entry) = entries.next() {
                let entry = storage_result(entry)?;
                let name = entry.file_name().to_str().map_err(|_| DAMAGED)?;
                if matches!(name, "." | ".." | STAGE) {
                    continue;
                }
                let key = name
                    .strip_suffix(".json")
                    .filter(|key| hex_id(key, 64))
                    .ok_or(DAMAGED)?;
                match self.read(&dir, access, name, key) {
                    Ok(Some(record)) => records.push(record),
                    _ => {
                        // Do not turn a revoked session into per-record corruption.
                        self.check_scope(access)?;
                        unreadable.push(key.to_owned());
                    }
                }
            }
        }
        unreadable.sort_unstable();
        unreadable.dedup();
        records.sort_unstable_by(|a, b| a.key.cmp(&b.key));
        // Some filesystems can revisit a name replaced during a directory scan.
        records.dedup_by(|a, b| a.key == b.key);
        records.retain(|record| unreadable.binary_search(&record.key).is_err());
        self.check_scope(access)?;
        Ok((records, unreadable))
    }

    /// Look up one legacy-compatible key without scanning other memberships.
    pub(crate) fn get(&self, access: &Access, key: &str) -> Result<Option<Record>, ApiError> {
        self.check_scope(access)?;
        // A malformed Saved route key cannot name a local record.
        if !hex_id(key, 64) {
            return Ok(None);
        }
        let name = record_name(key)?;
        let record = match self.scope_dir(false)? {
            Some(dir) => self.read(&dir, access, &name, key)?,
            None => None,
        };
        self.check_scope(access)?;
        Ok(record)
    }

    pub(crate) fn set(
        &self,
        access: &Access,
        target: Target,
        wanted: bool,
        live: impl Fn() -> Result<(), ApiError>,
    ) -> Result<Record, ApiError> {
        self.check_scope(access)?;
        live()?;
        if !valid_target(&target) {
            return Err(INVALID);
        }
        let key = playlist_key(access, &target.id)?;
        let name = record_name(&key)?;
        let dir = self.scope_dir(true)?.ok_or(STORAGE)?;
        lock(&dir)?;
        self.check_scope(access)?;
        live()?;
        let current = self.read(&dir, access, &name, &key)?;
        if let Some(current) = current {
            // Check the identity in addition to its hash before updating the record.
            if current.target.id != target.id {
                return Err(DAMAGED);
            }
            if wanted && current.wanted && current.target.kind == target.kind {
                live()?;
                self.check_scope(access)?;
                return Ok(current);
            }
        }
        // Even when there was no previous v1 pin, an unpin must suppress a
        // matching legacy snapshot. A later repin gets a new publication token.
        let record = Record {
            target,
            key,
            token: new_token()?,
            wanted,
            committed: None,
        };
        self.commit(&dir, access, &name, record, live)
    }

    pub(crate) fn publish(
        &self,
        access: &Access,
        record: &Record,
        expected_committed: Option<[u8; 32]>,
        membership: Membership,
        live: impl Fn() -> Result<(), ApiError>,
    ) -> Result<Option<String>, ApiError> {
        self.check_scope(access)?;
        if !record.wanted {
            return Ok(None);
        }
        if !valid_target(&record.target)
            || !hex_id(&record.token, 32)
            || playlist_key(access, &record.target.id)? != record.key
        {
            return Err(INVALID);
        }
        let name = record_name(&record.key)?;
        let Some(dir) = self.scope_dir(false)? else {
            self.check_scope(access)?;
            return Ok(None);
        };
        lock(&dir)?;
        self.check_scope(access)?;
        let current = self.read(&dir, access, &name, &record.key)?;
        self.check_scope(access)?;
        let Some(mut current) = current.filter(|current| {
            current.wanted
                && current.token == record.token
                && current.target.id == record.target.id
                && current.target.kind == record.target.kind
                // Workers for the same intent must also share the captured basis.
                && current.committed.as_ref().map(fingerprint) == expected_committed
        }) else {
            return Ok(None);
        };
        if membership.provenance.is_none() || !valid_membership(&membership, current.target.kind) {
            return Err(INVALID);
        }
        // Advance the CAS even if the refreshed membership has identical content.
        loop {
            let token = new_token()?;
            if token != current.token {
                current.token = token;
                break;
            }
        }
        current.committed = Some(membership);
        let published = self.commit(&dir, access, &name, current, live)?;
        Ok(Some(published.token))
    }

    fn check_scope(&self, access: &Access) -> Result<(), ApiError> {
        if scope(access)? != self.scope {
            return Err(WRONG_SCOPE);
        }
        Ok(())
    }

    fn scope_dir(&self, create: bool) -> Result<Option<OwnedFd>, ApiError> {
        #[cfg(test)]
        if let Some(root) = &self.fixture {
            storage_result(storage::private_dir(root))?;
            return open_scope(root, &self.scope, create);
        }
        let root = match if create {
            storage::open_default(ROOT)
        } else {
            storage::open_default_existing(ROOT)
        } {
            Ok(root) => root,
            Err(Errno::NOENT) if !create => return Ok(None),
            Err(err) => return Err(storage_error(err)),
        };
        if create {
            // The root may also have been created by this operation. Persist
            // that directory entry in the data base before publishing records.
            let base = storage_result(fs::openat(&root, "..", DIR_FLAGS, Mode::empty()))?;
            storage_result(fs::fsync(&base))?;
        }
        open_scope(&root, &self.scope, create)
    }

    fn read(
        &self,
        dir: &OwnedFd,
        access: &Access,
        name: &str,
        key: &str,
    ) -> Result<Option<Record>, ApiError> {
        let fd = match fs::openat(dir, name, FILE_FLAGS | OFlags::RDONLY, Mode::empty()) {
            Ok(fd) => fd,
            Err(Errno::NOENT) => return Ok(None),
            Err(Errno::LOOP) => return Err(DAMAGED),
            Err(err) => return Err(storage_error(err)),
        };
        let len = private_len(&storage_result(fs::fstat(&fd))?)?;
        // Bound parsing by the opened inode's measured length, not a product
        // quota. Avoid reading an entire encoded playlist into a second buffer.
        let mut file = BufReader::new(File::from(fd));
        let mut bounded = (&mut file).take(len);
        let disk: DiskRecord = serde_json::from_reader(&mut bounded)
            .map_err(|err| err.io_error_kind().map_or(DAMAGED, io_error))?;
        if bounded.limit() != 0
            || !file
                .fill_buf()
                .map_err(|err| io_error(err.kind()))?
                .is_empty()
        {
            return Err(DAMAGED);
        }
        let record = disk.record;
        if disk.version != VERSION
            || disk.scope != self.scope
            || record.key != key
            || !hex_id(&record.token, 32)
            || !valid_target(&record.target)
            || (!record.wanted && record.committed.is_some())
            || record
                .committed
                .as_ref()
                .is_some_and(|m| !valid_membership(m, record.target.kind))
            || playlist_key(access, &record.target.id)? != key
        {
            return Err(DAMAGED);
        }
        Ok(Some(record))
    }

    /// Called under this account directory's exclusive flock, only for local I/O.
    fn commit(
        &self,
        dir: &OwnedFd,
        access: &Access,
        name: &str,
        record: Record,
        live: impl Fn() -> Result<(), ApiError>,
    ) -> Result<Record, ApiError> {
        self.check_scope(access)?;
        match fs::statat(dir, STAGE, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => {
                // An interrupted writer may leave its private stage behind.
                private_len(&stat)?;
                storage_result(fs::unlinkat(dir, STAGE, AtFlags::empty()))?;
            }
            Err(Errno::NOENT) => (),
            Err(err) => return Err(storage_error(err)),
        }
        let fd = storage_result(fs::openat(
            dir,
            STAGE,
            FILE_FLAGS | OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL,
            PRIVATE_FILE,
        ))?;
        let mut stage = Stage {
            dir,
            published: false,
        };
        if private_len(&storage_result(fs::fstat(&fd))?)? != 0 {
            return Err(DAMAGED);
        }
        let disk = DiskRecord {
            version: VERSION,
            scope: self.scope.clone(),
            record,
        };
        let file = File::from(fd);
        {
            let mut writer = BufWriter::new(&file);
            serde_json::to_writer(&mut writer, &disk)
                .map_err(|err| err.io_error_kind().map_or(STORAGE, io_error))?;
            writer.flush().map_err(|err| io_error(err.kind()))?;
        }
        file.sync_all().map_err(|err| io_error(err.kind()))?;
        {
            let _publication = PUBLICATION_GATE.lock().map_err(|_| STORAGE)?;
            live()?;
            self.check_scope(access)?;
            storage_result(fs::renameat(dir, STAGE, dir, name))?;
        }
        stage.published = true;
        // Rename is already visible. Don't imply a sync error rolled it back.
        fs::fsync(dir).map_err(|err| match err {
            Errno::NOSPC | Errno::DQUOT => NOT_DURABLE_NO_SPACE,
            _ => NOT_DURABLE,
        })?;
        Ok(disk.record)
    }
}

fn open_scope(root: &OwnedFd, scope: &str, create: bool) -> Result<Option<OwnedFd>, ApiError> {
    let dir = match fs::openat(root, scope, DIR_FLAGS, Mode::empty()) {
        Ok(dir) => dir,
        Err(Errno::NOENT) if create => {
            match fs::mkdirat(root, scope, Mode::RWXU) {
                Ok(()) | Err(Errno::EXIST) => (),
                Err(err) => return Err(storage_error(err)),
            }
            storage_result(fs::openat(root, scope, DIR_FLAGS, Mode::empty()))?
        }
        Err(Errno::NOENT) => return Ok(None),
        Err(err) => return Err(storage_error(err)),
    };
    storage_result(storage::private_dir(&dir))?;
    // Persist the scope entry even if another process just created it.
    if create {
        storage_result(fs::fsync(root))?;
    }
    Ok(Some(dir))
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

fn lock(dir: &OwnedFd) -> Result<(), ApiError> {
    // Each operation opens its own directory file description. Dropping it
    // releases the flock; no persistent lock file is needed.
    loop {
        match fs::flock(dir, FlockOperation::LockExclusive) {
            Ok(()) => return storage_result(storage::private_dir(dir)),
            Err(Errno::INTR) => (),
            Err(err) => return Err(storage_error(err)),
        }
    }
}

fn private_len(stat: &Stat) -> Result<u64, ApiError> {
    // A reader of the old inode after atomic rename can legitimately see nlink=0.
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile
        || stat.st_uid != geteuid().as_raw()
        || stat.st_mode & 0o7777 != 0o600
        || stat.st_nlink > 1
    {
        return Err(DAMAGED);
    }
    u64::try_from(stat.st_size).map_err(|_| DAMAGED)
}

fn storage_result<T>(result: rustix::io::Result<T>) -> Result<T, ApiError> {
    result.map_err(storage_error)
}

fn storage_error(err: Errno) -> ApiError {
    match err {
        Errno::NOSPC | Errno::DQUOT => NO_SPACE,
        _ => STORAGE,
    }
}

fn io_error(kind: ErrorKind) -> ApiError {
    match kind {
        ErrorKind::StorageFull | ErrorKind::QuotaExceeded => NO_SPACE,
        _ => STORAGE,
    }
}

fn scope(access: &Access) -> Result<String, ApiError> {
    let (origin, account) = access.api.authenticated_account(&access.session)?;
    Ok(scope_hash(origin, account))
}

fn scope_hash(origin: &str, account: &str) -> String {
    let mut hash = Context::new(&SHA256);
    hash.update(b"spofify-playlist-pin-scope-v1\0");
    for field in [origin, account] {
        hash.update(&(field.len() as u64).to_be_bytes());
        hash.update(field.as_bytes());
    }
    hex(hash.finish().as_ref())
}

fn new_token() -> Result<String, ApiError> {
    let mut random = [0; 16];
    SystemRandom::new().fill(&mut random).map_err(|_| STORAGE)?;
    Ok(hex(&random))
}

fn record_name(key: &str) -> Result<String, ApiError> {
    if !hex_id(key, 64) {
        return Err(INVALID);
    }
    Ok(format!("{key}.json"))
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

fn hex_id(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn valid_target(target: &Target) -> bool {
    // Match the playlist manifest route's catalog::derived_id rule, for both kinds.
    !target.id.is_empty()
        && target.id.len() <= 128
        && target
            .id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
        && valid_title(&target.title)
}

fn valid_membership(membership: &Membership, kind: PlaylistKind) -> bool {
    valid_title(&membership.title)
        && membership.tracks.iter().all(|id| {
            // Track IDs use the offline track route's broader opaque ID rule.
            !id.is_empty()
                && id.len() <= 512
                && id.trim() == id
                && !matches!(id.as_str(), "." | "..")
                && !id.chars().any(char::is_control)
        })
        && membership.provenance.as_ref().is_none_or(|provenance| {
            let max_id_bytes = match kind {
                PlaylistKind::Manual => 512,
                PlaylistKind::Derived => 4096,
            };
            let mut seen = HashSet::new();
            (1..=9_007_199_254_740_991).contains(&provenance.revision)
                && provenance.occurrence_ids.len() == membership.tracks.len()
                && provenance.occurrence_ids.iter().all(|id| {
                    !id.is_empty()
                        && id.len() <= max_id_bytes
                        && id.bytes().all(|byte| byte.is_ascii_graphic())
                        && seen.insert(id.as_str())
                })
        })
}

fn valid_title(value: &str) -> bool {
    !value.trim().is_empty() && value.len() <= 1024 && !value.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{Api, Origin};
    use std::{
        fs as stdfs,
        os::unix::fs::{DirBuilderExt, PermissionsExt, symlink},
        path::PathBuf,
        sync::Arc,
    };

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = stdfs::canonicalize(std::env::temp_dir())
                .unwrap()
                .join(format!(
                    "spofify-playlist-pin-test-{}",
                    uuid::Uuid::new_v4()
                ));
            stdfs::DirBuilder::new().mode(0o700).create(&path).unwrap();
            Self(path)
        }

        fn open(&self, access: &Access) -> Store {
            Store::open(&self.0, access).unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            stdfs::remove_dir_all(&self.0).unwrap();
        }
    }

    fn account(origin: &str) -> Access {
        let api = Arc::new(Api::new(Origin::parse(origin).unwrap()).unwrap());
        let session = api.test_session();
        session.set_offline(true);
        Access {
            epoch: 0,
            api,
            session,
            offline: true,
            personal: crate::personal_bridge::PersonalAccess::default(),
        }
    }

    fn target() -> Target {
        Target {
            id: "fixture-List_1".into(),
            title: "Fixture playlist".into(),
            kind: PlaylistKind::Manual,
        }
    }

    fn membership(tracks: &[&str]) -> Membership {
        Membership {
            title: "Published title".into(),
            tracks: tracks.iter().map(|id| (*id).into()).collect(),
            provenance: Some(Provenance {
                revision: 7,
                occurrence_ids: (0..tracks.len()).map(|i| format!("entry_{i}")).collect(),
            }),
        }
    }

    #[test]
    fn old_pin_is_read_without_rewrite_and_refresh_retains_exact_duplicate_rows() {
        let fixture = Fixture::new();
        let access = account("https://fixture.invalid");
        let store = fixture.open(&access);
        let pin = store
            .set(
                &access,
                Target {
                    kind: PlaylistKind::Derived,
                    ..target()
                },
                true,
                || Ok(()),
            )
            .unwrap();
        let path = fixture
            .0
            .join(&store.scope)
            .join(record_name(&pin.key).unwrap());
        let mut old: serde_json::Value =
            serde_json::from_slice(&stdfs::read(&path).unwrap()).unwrap();
        old["record"]["committed"] = serde_json::json!({
            "title": "Published title",
            "tracks": ["same", "same"],
        });
        let bytes = serde_json::to_vec(&old).unwrap();
        stdfs::write(&path, &bytes).unwrap();
        let store = fixture.open(&access);
        let record = store.get(&access, &pin.key).unwrap().unwrap();
        let old_members = record.committed.as_ref().unwrap();
        assert_eq!(old_members.tracks, ["same", "same"]);
        assert_eq!(old_members.provenance, None);
        assert_eq!(store.list(&access).unwrap(), [record.clone()]);
        assert_eq!(stdfs::read(&path).unwrap(), bytes);
        let basis = Some(fingerprint(old_members));
        assert_eq!(
            store.publish(&access, &record, basis, old_members.clone(), || Ok(())),
            Err(INVALID)
        );
        let refreshed = Membership {
            provenance: Some(Provenance {
                revision: 12,
                occurrence_ids: vec!["o1_first".into(), "o1_second".into()],
            }),
            ..old_members.clone()
        };
        assert_ne!(Some(fingerprint(&refreshed)), basis);
        let token = store
            .publish(&access, &record, basis, refreshed.clone(), || Ok(()))
            .unwrap()
            .unwrap();
        let reloaded = fixture
            .open(&access)
            .get(&access, &pin.key)
            .unwrap()
            .unwrap();
        assert_eq!(reloaded.token, token);
        assert_eq!(reloaded.committed, Some(refreshed));
    }

    #[test]
    fn malformed_provenance_cannot_publish_or_read_as_legacy() {
        let fixture = Fixture::new();
        let access = account("https://fixture.invalid");
        let store = fixture.open(&access);
        let pin = store.set(&access, target(), true, || Ok(())).unwrap();
        let path = fixture
            .0
            .join(&store.scope)
            .join(record_name(&pin.key).unwrap());
        let original = stdfs::read(&path).unwrap();
        for (revision, ids) in [
            (0, vec!["first", "second"]),
            (7, vec!["first"]),
            (7, vec!["first", "first"]),
            (7, vec!["", "second"]),
        ] {
            let damaged = Membership {
                provenance: Some(Provenance {
                    revision,
                    occurrence_ids: ids.into_iter().map(str::to_owned).collect(),
                }),
                ..membership(&["same", "same"])
            };
            assert_eq!(
                store.publish(&access, &pin, None, damaged.clone(), || Ok(())),
                Err(INVALID)
            );
            let mut disk: DiskRecord = serde_json::from_slice(&original).unwrap();
            disk.record.committed = Some(damaged);
            stdfs::write(&path, serde_json::to_vec(&disk).unwrap()).unwrap();
            assert_eq!(store.get(&access, &pin.key), Err(DAMAGED));
            stdfs::write(&path, &original).unwrap();
        }
        let mut disk: serde_json::Value = serde_json::from_slice(&original).unwrap();
        disk["record"]["committed"] = serde_json::json!({
            "title": "Published title",
            "tracks": ["same"],
            "provenance": null,
        });
        stdfs::write(&path, serde_json::to_vec(&disk).unwrap()).unwrap();
        assert_eq!(store.get(&access, &pin.key), Err(DAMAGED));
    }

    #[tokio::test]
    async fn persistence_is_scoped_and_ignores_display_identity() {
        let fixture = Fixture::new();
        let first = account("https://fixture.invalid");
        let other = account("https://other.invalid");
        let store = fixture.open(&first);
        assert!(store.list(&first).unwrap().is_empty());
        assert_eq!(stdfs::read_dir(&fixture.0).unwrap().count(), 0);
        let pin = store.set(&first, target(), true, || Ok(())).unwrap();
        assert_eq!(pin.key, playlist_key(&first, &pin.target.id).unwrap());
        let saved = membership(&["two", "one", "two"]);
        let token = store
            .publish(&first, &pin, None, saved.clone(), || Ok(()))
            .unwrap()
            .unwrap();
        assert_ne!(token, pin.token);
        assert_eq!(store.list(&other), Err(WRONG_SCOPE));
        assert_eq!(
            store.set(&other, target(), false, || Ok(())),
            Err(WRONG_SCOPE)
        );
        assert_eq!(
            store.publish(&other, &pin, None, saved.clone(), || Ok(())),
            Err(WRONG_SCOPE)
        );
        assert!(fixture.open(&other).list(&other).unwrap().is_empty());
        let other_pin = fixture
            .open(&other)
            .set(&other, target(), true, || Ok(()))
            .unwrap();
        assert_ne!(pin.key, other_pin.key);
        assert_ne!(
            scope_hash("https://fixture.invalid", "synthetic"),
            scope_hash("https://fixture.invalid", "other-account")
        );

        // Offline logout revokes locally without any HTTP; a new session for
        // the same account can still open exactly the committed record.
        assert!(first.api.logout(&first.session).await.is_err());
        assert!(store.list(&first).unwrap_err().expired);
        drop(store);
        let mut same = account("https://FIXTURE.invalid:443");
        same.session.account_id = "untrusted display field".into();
        let store = fixture.open(&same);
        let expected = Record {
            token,
            committed: Some(saved),
            ..pin
        };
        assert_eq!(store.list(&same).unwrap(), [expected.clone()]);
        assert_eq!(store.get(&same, &expected.key), Ok(Some(expected.clone())));
        assert_eq!(store.get(&same, &other_pin.key), Ok(None));
        assert_eq!(store.get(&same, "../index.json"), Ok(None));
        assert_eq!(store.get(&other, &expected.key), Err(WRONG_SCOPE));
        assert_eq!(
            store.set(&same, target(), true, || Ok(())).unwrap(),
            expected
        );
        assert_eq!(fixture.open(&other).list(&other).unwrap(), [other_pin]);
        let dir = fixture.0.join(&store.scope);
        let path = dir.join(record_name(&expected.key).unwrap());
        assert_eq!(
            stdfs::metadata(dir).unwrap().permissions().mode() & 0o7777,
            0o700
        );
        assert_eq!(
            stdfs::metadata(&path).unwrap().permissions().mode() & 0o7777,
            0o600
        );
        let json = stdfs::read_to_string(path).unwrap();
        assert!(!json.contains("synthetic") && !json.contains("fixture.invalid"));
    }

    #[test]
    fn unpin_tombstone_and_repin_supersede_old_publishers() {
        let fixture = Fixture::new();
        let access = account("https://fixture.invalid");
        let store = fixture.open(&access);
        let old = store.set(&access, target(), true, || Ok(())).unwrap();
        assert!(
            store
                .publish(&access, &old, None, membership(&["old"]), || Ok(()))
                .unwrap()
                .is_some()
        );
        let old = store.get(&access, &old.key).unwrap().unwrap();
        let old_basis = old.committed.as_ref().map(fingerprint);
        let tombstone = store.set(&access, target(), false, || Ok(())).unwrap();
        assert!(!tombstone.wanted && tombstone.committed.is_none());
        assert_eq!(tombstone.key, old.key);
        assert_ne!(tombstone.token, old.token);
        let store = fixture.open(&access);
        let unexpected_live = || panic!("a superseded publish must not try to commit");
        assert_eq!(store.list(&access).unwrap(), [tombstone.clone()]);
        assert_eq!(
            store.publish(&access, &old, old_basis, membership(&[]), unexpected_live),
            Ok(None)
        );
        assert_eq!(
            store.publish(&access, &tombstone, None, membership(&[]), unexpected_live),
            Ok(None)
        );
        let new = store.set(&access, target(), true, || Ok(())).unwrap();
        assert!(new.wanted && new.committed.is_none());
        assert_eq!(new.key, old.key);
        assert_ne!(new.token, old.token);
        assert_ne!(new.token, tombstone.token);
        assert_eq!(
            store.publish(
                &access,
                &old,
                old_basis,
                membership(&["late"]),
                unexpected_live
            ),
            Ok(None)
        );
        assert_eq!(store.list(&access).unwrap(), [new.clone()]);
        assert!(
            store
                .publish(&access, &new, None, membership(&[]), || Ok(()))
                .unwrap()
                .is_some()
        );
        let new = store.get(&access, &new.key).unwrap().unwrap();

        let changed = store
            .set(
                &access,
                Target {
                    kind: PlaylistKind::Derived,
                    ..target()
                },
                true,
                || Ok(()),
            )
            .unwrap();
        assert_eq!(changed.key, new.key);
        assert_ne!(changed.token, new.token);
        assert!(changed.committed.is_none());
        assert_eq!(
            store.publish(
                &access,
                &new,
                Some(fingerprint(&membership(&[]))),
                membership(&["old kind"]),
                unexpected_live
            ),
            Ok(None)
        );
    }

    #[test]
    fn prepublication_failure_preserves_previous_membership() {
        let fixture = Fixture::new();
        let access = account("https://fixture.invalid");
        let store = fixture.open(&access);
        let pin = store.set(&access, target(), true, || Ok(())).unwrap();
        let saved = Membership {
            title: "Large membership".into(),
            provenance: Some(Provenance {
                revision: 7,
                occurrence_ids: (0..9_000).map(|i| format!("entry_{i}")).collect(),
            }),
            tracks: ["a".repeat(512), "b".repeat(512), "a".repeat(512)]
                .into_iter()
                .cycle()
                .take(9_000)
                .collect(),
        };
        let token = store
            .publish(&access, &pin, None, saved.clone(), || Ok(()))
            .unwrap()
            .unwrap();
        let stage = fixture.0.join(&store.scope).join(STAGE);
        let mut captured = store.get(&access, &pin.key).unwrap().unwrap();
        assert_eq!(captured.token, token);
        let basis = captured.committed.take().as_ref().map(fingerprint);
        assert_eq!(basis, Some(fingerprint(&saved)));
        assert_eq!(
            store.publish(&access, &captured, None, membership(&["stale"]), || panic!(
                "same token but outdated membership basis"
            )),
            Ok(None)
        );
        const CANCELLED: ApiError = error("Fixture publication cancelled.");
        assert_eq!(
            store.publish(&access, &captured, basis, membership(&["new"]), || {
                assert!(stage.is_file());
                Err(CANCELLED)
            }),
            Err(CANCELLED)
        );
        assert!(!stage.exists());
        let store = fixture.open(&access);
        assert_eq!(
            store.list(&access).unwrap(),
            [Record {
                token,
                committed: Some(saved),
                ..pin
            }]
        );
        let refreshed = membership(&["new"]);
        assert!(
            store
                .publish(&access, &captured, basis, refreshed.clone(), || Ok(()))
                .unwrap()
                .is_some()
        );
        assert_eq!(
            store
                .get(&access, &captured.key)
                .unwrap()
                .unwrap()
                .committed,
            Some(refreshed)
        );
        assert_eq!(
            store.publish(&access, &captured, basis, membership(&["late"]), || panic!(
                "outdated captured fingerprint"
            )),
            Ok(None)
        );
    }

    #[test]
    fn identical_publication_advances_cas_and_rejects_older_worker() {
        let fixture = Fixture::new();
        let access = account("https://fixture.invalid");
        let store = fixture.open(&access);
        let pin = store.set(&access, target(), true, || Ok(())).unwrap();
        let saved = membership(&["same", "same"]);
        let first_token = store
            .publish(&access, &pin, None, saved.clone(), || Ok(()))
            .unwrap()
            .unwrap();
        let mut captured = store.get(&access, &pin.key).unwrap().unwrap();
        let basis = captured.committed.take().as_ref().map(fingerprint);
        assert_eq!(captured.token, first_token);
        let next_token = store
            .publish(&access, &captured, basis, saved.clone(), || Ok(()))
            .unwrap()
            .unwrap();
        assert_ne!(next_token, first_token);
        let committed = store.get(&access, &pin.key).unwrap().unwrap();
        assert_eq!(committed.token, next_token);
        assert_eq!(committed.committed, Some(saved));
        assert_eq!(
            store.publish(
                &access,
                &captured,
                basis,
                membership(&["stale"]),
                || panic!("same fingerprint but stale token")
            ),
            Ok(None)
        );
    }

    #[test]
    fn stale_set_cannot_rename_a_staged_tombstone() {
        let fixture = Fixture::new();
        let access = account("https://fixture.invalid");
        let store = fixture.open(&access);
        let pin = store.set(&access, target(), true, || Ok(())).unwrap();
        let stage = fixture.0.join(&store.scope).join(STAGE);
        const STALE: ApiError = error("Fixture operation superseded.");
        let checks = std::cell::Cell::new(0);
        assert_eq!(
            store.set(&access, target(), false, || {
                checks.set(checks.get() + 1);
                if stage.is_file() { Err(STALE) } else { Ok(()) }
            }),
            Err(STALE)
        );
        assert!(checks.get() >= 3);
        assert!(!stage.exists());
        assert_eq!(store.get(&access, &pin.key), Ok(Some(pin.clone())));
        checks.set(0);
        assert_eq!(
            store.set(&access, target(), true, || {
                checks.set(checks.get() + 1);
                if checks.get() == 3 {
                    Err(STALE)
                } else {
                    Ok(())
                }
            }),
            Err(STALE)
        );
        assert_eq!(store.get(&access, &pin.key), Ok(Some(pin)));
    }

    #[test]
    fn scan_reports_the_key_of_a_corrupt_tombstone_and_retains_healthy_records() {
        let fixture = Fixture::new();
        let access = account("https://fixture.invalid");
        let store = fixture.open(&access);
        let healthy = store.set(&access, target(), true, || Ok(())).unwrap();
        let corrupt = store
            .set(
                &access,
                Target {
                    id: "to-hide".into(),
                    ..target()
                },
                false,
                || Ok(()),
            )
            .unwrap();
        let dir = fixture.0.join(&store.scope);
        stdfs::write(dir.join(record_name(&corrupt.key).unwrap()), b"broken JSON").unwrap();
        assert_eq!(
            store.scan(&access),
            Ok((vec![healthy.clone()], vec![corrupt.key.clone()]))
        );
        assert_eq!(store.get(&access, &healthy.key), Ok(Some(healthy)));
        assert_eq!(store.get(&access, &corrupt.key), Err(DAMAGED));
        assert_eq!(store.list(&access), Err(DAMAGED));
        stdfs::write(dir.join("not-a-key.json"), b"broken JSON").unwrap();
        assert_eq!(store.scan(&access), Err(DAMAGED));
    }

    #[test]
    fn rejects_unsafe_layout_and_inconsistent_disk_records() {
        let fixture = Fixture::new();
        let access = account("https://fixture.invalid");
        let store = fixture.open(&access);
        for id in [
            "",
            ".",
            "..",
            "dir/list",
            "a\\b",
            "p%2f",
            "white space",
            "é",
        ] {
            assert_eq!(
                store.set(
                    &access,
                    Target {
                        id: id.into(),
                        ..target()
                    },
                    true,
                    || Ok(()),
                ),
                Err(INVALID)
            );
        }
        assert_eq!(
            store.set(
                &access,
                Target {
                    title: "bad\nname".into(),
                    ..target()
                },
                true,
                || Ok(()),
            ),
            Err(INVALID)
        );
        let pin = store.set(&access, target(), true, || Ok(())).unwrap();
        let dir = fixture.0.join(&store.scope);
        let path = dir.join(record_name(&pin.key).unwrap());
        let original = stdfs::read(&path).unwrap();
        let mut wrong: DiskRecord = serde_json::from_slice(&original).unwrap();
        wrong.scope = scope_hash("https://fixture.invalid", "another-account");
        stdfs::write(&path, serde_json::to_vec(&wrong).unwrap()).unwrap();
        assert_eq!(store.list(&access), Err(DAMAGED));
        wrong.scope = store.scope.clone();
        wrong.record.target.id = "unrelated".into();
        stdfs::write(&path, serde_json::to_vec(&wrong).unwrap()).unwrap();
        assert_eq!(store.list(&access), Err(DAMAGED));
        stdfs::write(&path, &original).unwrap();
        stdfs::set_permissions(&path, stdfs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(store.list(&access), Err(DAMAGED));
        stdfs::set_permissions(&path, stdfs::Permissions::from_mode(0o600)).unwrap();
        let outside = fixture.0.join("record");
        stdfs::hard_link(&path, &outside).unwrap();
        assert_eq!(store.list(&access), Err(DAMAGED));
        stdfs::remove_file(&path).unwrap();
        symlink(&outside, &path).unwrap();
        assert_eq!(store.list(&access), Err(DAMAGED));
        assert_eq!(store.set(&access, target(), false, || Ok(())), Err(DAMAGED));
        assert_eq!(stdfs::read(&outside).unwrap(), original);
        stdfs::remove_file(&path).unwrap();
        stdfs::remove_dir(&dir).unwrap();
        symlink(&fixture.0, &dir).unwrap();
        assert!(store.list(&access).is_err());
        assert!(store.set(&access, target(), true, || Ok(())).is_err());
    }
}
