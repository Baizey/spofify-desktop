//! Durable last-good personal projections, not credentials or playback grants.
//! Only the personal reducer can publish a snapshot; one worker per account
//! epoch writes accepted state in order and coalesces superseded disk work.
use std::{
    collections::HashMap,
    fs::File,
    io::{self, BufReader, BufWriter, Write},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use rustix::{
    fd::OwnedFd,
    fs::{self, AtFlags, FileType, Mode, OFlags},
    io::Errno,
    process::geteuid,
};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::{
    api::library::{HistoryEntry, PlaylistSummary},
    desk::Access,
    pages::{self, Page, Route},
    personal::{AcceptedPlaylist, Playlist, Replica, Snapshot},
    playlist_pin_store::PUBLICATION_GATE,
    storage,
};

const ROOT: &str = "spofify-personal-v2";
const FILE_MODE: Mode = Mode::RUSR.union(Mode::WUSR);
const FLAGS: OFlags = OFlags::NOFOLLOW
    .union(OFlags::CLOEXEC)
    .union(OFlags::NONBLOCK);
pub(crate) const FRESHNESS: Duration = Duration::from_secs(5 * 60);

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct Cache {
    pub snapshot: Option<Arc<Snapshot>>,
    pub snapshot_at: u64,
    pub details: HashMap<String, Detail>,
    pub local_recent: Arc<Vec<HistoryEntry>>,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Detail {
    pub playlist: Playlist,
    pub refreshed_at: u64,
}

impl Detail {
    pub fn fresh(&self, now: u64, ttl: Duration) -> bool {
        !self.playlist.stale
            && self.refreshed_at != 0
            && now
                .checked_sub(self.refreshed_at)
                .is_some_and(|elapsed| elapsed < ttl.as_millis() as u64)
    }
}

impl Cache {
    pub fn projection(&self, route: &Route) -> Option<Page> {
        match route {
            Route::History if self.snapshot.is_some() || !self.local_recent.is_empty() => Some(
                pages::recent(&self.recent(self.snapshot.as_ref().map_or(&[], |s| &s.recent))),
            ),
            Route::Playlists | Route::Picker(_) => Some(pages::personal_list(
                route,
                &self.snapshot.as_ref()?.playlists,
            )),
            Route::Playlist(id) => {
                let summary = self
                    .snapshot
                    .as_ref()
                    .and_then(|s| s.playlists.iter().find(|p| p.id == *id));
                if let Some(detail) = self.details.get(id) {
                    let p = &detail.playlist;
                    Some(pages::personal_playlist(
                        &p.summary,
                        p.definition.as_ref(),
                        &p.rows,
                    ))
                } else {
                    Some(pages::unsaved_playlist(summary?))
                }
            }
            _ => None,
        }
    }

    pub fn accept(&mut self, event: AcceptedPlaylist<'_>, refreshed_at: Option<u64>) {
        match event {
            AcceptedPlaylist::Summaries(list) => {
                self.reconcile(list, Replica::summaries_capped(list))
            }
            AcceptedPlaylist::Summary {
                summary,
                invalidates_membership,
            } => {
                if let Some(detail) = self.details.get_mut(&summary.id) {
                    detail.playlist.summary = summary.clone();
                    detail.playlist.stale |= invalidates_membership;
                }
            }
            AcceptedPlaylist::Complete(playlist) => {
                let id = playlist.summary.id.clone();
                let refreshed_at = refreshed_at
                    .unwrap_or_else(|| self.details.get(&id).map_or(0, |d| d.refreshed_at));
                self.details.insert(
                    id,
                    Detail {
                        playlist: playlist.clone(),
                        refreshed_at,
                    },
                );
            }
            AcceptedPlaylist::Deleted(id) => {
                self.details.remove(id);
            }
        }
    }

    fn reconcile(&mut self, list: &[PlaylistSummary], capped: bool) {
        self.details.retain(|id, detail| {
            let Some(summary) = list.iter().find(|summary| summary.id == *id) else {
                return capped;
            };
            if detail.playlist.summary.kind != summary.kind {
                return false;
            }
            if detail.playlist.summary != *summary {
                detail.playlist.stale = true;
                detail.playlist.summary = summary.clone();
            }
            true
        });
    }

    pub fn recent(&self, online: &[HistoryEntry]) -> Vec<HistoryEntry> {
        use time::{OffsetDateTime, format_description::well_known::Rfc3339};
        let mut entries: Vec<_> = self
            .local_recent
            .iter()
            .filter(|local| {
                !online
                    .iter()
                    .any(|entry| entry.id == local.id && entry.origin == local.origin)
            })
            .chain(online)
            .collect();
        let timestamp =
            |entry: &HistoryEntry| OffsetDateTime::parse(&entry.played_at, &Rfc3339).ok();
        entries.sort_by_cached_key(|entry| std::cmp::Reverse(timestamp(entry)));
        entries.into_iter().take(40).cloned().collect()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Scope(String);
impl Scope {
    pub fn new(access: &Access) -> Option<Self> {
        let (origin, account) = access.api.authenticated_account(&access.session).ok()?;
        let mut hash = ring::digest::Context::new(&ring::digest::SHA256);
        hash.update(origin.as_bytes());
        hash.update(&[0]);
        hash.update(account.as_bytes());
        Some(Self(
            hash.finish()
                .as_ref()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
        ))
    }

    pub fn load(&self) -> Cache {
        let result = storage::open_default_existing(ROOT)
            .map_err(io::Error::from)
            .and_then(|root| self.read(&root));
        match result {
            Ok(cache) => cache,
            Err(error) => {
                if error.kind() != io::ErrorKind::NotFound {
                    eprintln!("Unable to read the account library cache: {error}");
                }
                Cache::default()
            }
        }
    }

    fn read(&self, root: &OwnedFd) -> io::Result<Cache> {
        let fd = fs::openat(root, &self.0, FLAGS | OFlags::RDONLY, Mode::empty())?;
        let stat = fs::fstat(&fd)?;
        if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile
            || stat.st_uid != geteuid().as_raw()
            || stat.st_mode & 0o7777 != 0o600
        {
            return Err(Errno::PERM.into());
        }
        Ok(serde_json::from_reader(BufReader::new(File::from(fd)))?)
    }

    fn write(&self, root: &OwnedFd, cache: &Cache, live: impl Fn() -> bool) -> io::Result<()> {
        if !live() {
            return Ok(());
        }
        let stage = format!(".{}", uuid::Uuid::new_v4());
        let fd = fs::openat(
            root,
            &stage,
            FLAGS | OFlags::CREATE | OFlags::EXCL | OFlags::WRONLY,
            FILE_MODE,
        )?;
        let result = (|| {
            let mut file = BufWriter::new(File::from(fd));
            serde_json::to_writer(&mut file, cache)?;
            file.flush()?;
            file.get_ref().sync_all()?;
            {
                let _publication = PUBLICATION_GATE.lock().unwrap();
                if !live() {
                    return Ok(());
                }
                fs::renameat(root, &stage, root, &self.0)?;
            }
            fs::fsync(root)?;
            Ok(())
        })();
        let _ = fs::unlinkat(root, &stage, AtFlags::empty());
        result
    }
}

#[derive(Clone)]
pub(crate) struct Writer {
    live: Arc<AtomicBool>,
    updates: watch::Sender<Option<(u64, Cache)>>,
    written: watch::Receiver<u64>,
}
impl Writer {
    pub fn start(scope: Scope, epoch: u64, current: Arc<AtomicU64>) -> Self {
        let (updates, mut rx) = watch::channel::<Option<(u64, Cache)>>(None);
        let (done, written) = watch::channel(0);
        let live = Arc::new(AtomicBool::new(true));
        let running = live.clone();
        tokio::spawn(async move {
            while rx.changed().await.is_ok() {
                let Some((serial, cache)) = rx.borrow_and_update().clone() else {
                    continue;
                };
                let (scope, current, running) = (scope.clone(), current.clone(), running.clone());
                let result = tokio::task::spawn_blocking(move || {
                    let live = || {
                        running.load(Ordering::SeqCst) && current.load(Ordering::SeqCst) == epoch
                    };
                    if !live() {
                        return Ok(());
                    }
                    scope.write(&storage::open_default(ROOT)?, &cache, live)
                })
                .await;
                if !matches!(result, Ok(Ok(()))) {
                    eprintln!("Unable to persist the account library cache");
                }
                done.send_replace(serial);
            }
        });
        Self {
            live,
            updates,
            written,
        }
    }
    pub fn retire(&self) {
        let _publication = PUBLICATION_GATE.lock().unwrap();
        self.live.store(false, Ordering::SeqCst);
    }
    pub fn publish(&self, cache: &Cache) {
        self.updates.send_modify(|state| {
            let serial = state.as_ref().map_or(1, |(last, _)| last + 1);
            *state = Some((serial, cache.clone()));
        });
    }
    pub async fn flush(mut self) {
        let target = self
            .updates
            .borrow()
            .as_ref()
            .map_or(0, |(serial, _)| *serial);
        let _ = self.written.wait_for(|serial| *serial >= target).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn play(id: &str, played_at: &str) -> HistoryEntry {
        HistoryEntry {
            id: id.into(),
            played_at: played_at.into(),
            origin: None,
            received_at: None,
            track: serde_json::from_value(serde_json::json!({
                "kind":"track", "id":"same-track", "name":"Song", "artists":[], "genres":[],
                "album_id":"", "album_name":"", "album_type":"", "release_date":"", "relation":"",
                "duration_ms":0, "disc_number":0, "track_number":0, "has_audio":false, "coverage":{"tracks":0,"inLibrary":0,"albums":0}
            }))
            .unwrap(),
            metadata_missing: true,
        }
    }

    #[test]
    fn history_matches_event_identity_not_track_identity() {
        let first = play("event-1", "2026-06-10T10:00:00Z");
        let second = play("event-2", "2026-06-10T10:01:00.001Z");
        let cache = Cache {
            local_recent: Arc::new(vec![first.clone(), second.clone()]),
            ..Cache::default()
        };
        assert_eq!(
            cache.recent(std::slice::from_ref(&first)),
            vec![second, first]
        );
    }

    #[test]
    fn retired_write_cannot_replace_last_good_or_leak_to_another_scope() {
        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(uuid::Uuid::new_v4().to_string());
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&root)
            .unwrap();
        let dir = storage::open_private(&root).unwrap();
        let scope = Scope("first-account".into());
        let other = Scope("other-account".into());
        let mut cache = Cache {
            snapshot_at: 42,
            ..Cache::default()
        };
        scope.write(&dir, &cache, || true).unwrap();
        cache.snapshot_at = 99;
        let checked = Cell::new(false);
        scope
            .write(&dir, &cache, || !checked.replace(true))
            .unwrap();
        assert_eq!(scope.read(&dir).unwrap().snapshot_at, 42);
        assert_eq!(
            other.read(&dir).err().unwrap().kind(),
            io::ErrorKind::NotFound
        );
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }
}
