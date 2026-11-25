//! Account personal read model for Go's v3 account socket. This reducer never
//! reads, sends, persists, or replays commands. The caller fences account/socket
//! epochs before reducing a frame and dispatches the returned sync immediately.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    mem::size_of,
    sync::Arc,
};

use serde::{Deserialize, Serialize};

use crate::api::{
    catalog::ArtistCredit,
    library::{
        HistoryEntry, ManualEntry, PlaylistKind, PlaylistSummary, RuleDefinition, WireHistory,
        WireManualEntry,
    },
};

const MAX_PLAYLISTS: usize = 100;
const MAX_RECENT: usize = 40;
const MAX_DETAILS: usize = 8;
// Estimated row memory for cache eviction only. Staging and the watched
// detail can exceed this budget without truncating rows.
const DETAIL_CACHE_BYTES: usize = 64 * 1024 * 1024;

/// Inner payload of `{"type":"personal","command":…}`. No account, client
/// revision, request ID or ACK. `MoveEntry.after == ""` explicitly means first.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Command {
    SyncPlaylist {
        playlist_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        known: Option<String>,
        #[serde(skip_serializing_if = "is_zero")]
        offset: usize,
        // A local request identity, never an on-the-wire token. Command
        // value equality still compares the wire fields; accepts_sync also
        // compares this allocation so an A → B → A watch can't reuse old A.
        #[serde(skip)]
        local_request: Arc<()>,
    },
    CreatePlaylist {
        name: String,
        description: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        definition: Option<RuleDefinition>,
        #[serde(skip_serializing_if = "Option::is_none")]
        track_id: Option<String>,
    },
    RenamePlaylist {
        playlist_id: String,
        name: String,
        description: String,
    },
    DeletePlaylist {
        playlist_id: String,
    },
    CopyPlaylist {
        playlist_id: String,
        name: String,
        description: String,
    },
    SetPlaylistRules {
        playlist_id: String,
        definition: RuleDefinition,
    },
    AddEntry {
        playlist_id: String,
        track_id: String,
    },
    RemoveEntry {
        playlist_id: String,
        entry_id: String,
    },
    MoveEntry {
        playlist_id: String,
        entry_id: String,
        after: String,
    },
    ClearHistory,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}
pub(crate) fn id_ok(id: &str) -> bool {
    !id.is_empty() && id.len() <= 512 && id.trim() == id && !id.chars().any(char::is_control)
}
fn token_ok(token: &str) -> bool {
    !token.is_empty() && token.len() <= 128 && token.bytes().all(|b| b.is_ascii_graphic())
}
fn metadata_ok(name: &str, description: &str) -> bool {
    let trimmed = name.trim();
    name.len() <= 400
        && !trimmed.is_empty()
        && trimmed.chars().count() <= 100
        && !trimmed.chars().any(char::is_control)
        && description.len() <= 2000
        && description.chars().count() <= 500
        && !description
            .chars()
            .any(|c| c.is_control() && c != '\n' && c != '\t')
}

impl Command {
    pub fn valid(&self) -> bool {
        match self {
            Self::SyncPlaylist {
                playlist_id,
                known,
                offset,
                ..
            } => {
                id_ok(playlist_id)
                    && known.as_deref().is_none_or(token_ok)
                    && (*offset == 0 || known.is_some())
            }
            Self::CreatePlaylist {
                name,
                description,
                definition,
                track_id,
            } => {
                metadata_ok(name, description)
                    && track_id.as_deref().is_none_or(id_ok)
                    && !(definition.is_some() && track_id.is_some())
            }
            Self::RenamePlaylist {
                playlist_id,
                name,
                description,
            }
            | Self::CopyPlaylist {
                playlist_id,
                name,
                description,
            } => id_ok(playlist_id) && metadata_ok(name, description),
            Self::DeletePlaylist { playlist_id } | Self::SetPlaylistRules { playlist_id, .. } => {
                id_ok(playlist_id)
            }
            Self::AddEntry {
                playlist_id,
                track_id,
            } => id_ok(playlist_id) && id_ok(track_id),
            Self::RemoveEntry {
                playlist_id,
                entry_id,
            } => id_ok(playlist_id) && id_ok(entry_id),
            Self::MoveEntry {
                playlist_id,
                entry_id,
                after,
            } => {
                id_ok(playlist_id)
                    && id_ok(entry_id)
                    && entry_id != after
                    && (after.is_empty() || id_ok(after))
            }
            Self::ClearHistory => true,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Snapshot {
    pub playlists: Vec<PlaylistSummary>,
    pub recent: Vec<HistoryEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Playlist {
    pub summary: PlaylistSummary,
    pub definition: Option<RuleDefinition>,
    /// The token of a complete server part set; a delta clears it.
    pub token: String,
    pub rows: Arc<Vec<ManualEntry>>,
    /// Rows are last-good rather than reconciled with the account summary.
    pub stale: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Part {
    pub summary: PlaylistSummary,
    pub definition: Option<RuleDefinition>,
    pub token: String,
    pub offset: usize,
    pub rows: Vec<ManualEntry>,
    pub more: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Change {
    pub playlist: Option<PlaylistSummary>,
    pub deleted: Option<String>,
    pub entries: Option<EntryChange>,
    pub recent: Option<RecentChange>,
    pub rows_invalid: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EntryChange {
    pub added: Option<ManualEntry>,
    pub removed: Option<String>,
    pub moved: Option<String>,
    pub after: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecentChange {
    pub removed: Vec<RecentKey>,
    pub inserted: Vec<RecentInsertion>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Deserialize)]
pub struct RecentKey {
    pub id: String,
    #[serde(default)]
    pub origin: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecentInsertion {
    pub index: usize,
    pub item: HistoryEntry,
}

/// Inner payload of `{"type":"personal","update":…}`. Go uses an object
/// with exactly one scoped field rather than an explicit variant tag.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Update {
    Snapshot(Snapshot),
    Part(Part),
    Change(Change),
    Missing { playlist_id: String },
}

pub use Command as PersonalCommand;
pub use Update as PersonalUpdate;

/// Exact successful reducer commits; never an input frame or staged rows.
#[derive(Clone, Copy)]
pub(crate) enum AcceptedPlaylist<'a> {
    Summaries(&'a [PlaylistSummary]),
    Summary {
        summary: &'a PlaylistSummary,
        invalidates_membership: bool,
    },
    Complete(&'a Playlist),
    Deleted(&'a str),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireUpdate {
    snapshot: Option<WireSnapshot>,
    part: Option<WirePart>,
    change: Option<WireChange>,
    missing: Option<WireMissing>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct WireMissing {
    playlist_id: String,
}

#[derive(Deserialize)]
struct WireSnapshot {
    playlists: Vec<PlaylistSummary>,
    recent: Vec<WireHistory>,
}

#[derive(Deserialize)]
struct WirePart {
    summary: PlaylistSummary,
    definition: Option<RuleDefinition>,
    token: String,
    offset: usize,
    rows: Vec<WireManualEntry>,
    more: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireChange {
    playlist: Option<PlaylistSummary>,
    deleted: Option<String>,
    entries: Option<WireEntryChange>,
    recent: Option<WireRecentChange>,
    #[serde(default)]
    rows_invalid: bool,
}

#[derive(Deserialize)]
struct WireEntryChange {
    added: Option<WireManualEntry>,
    removed: Option<String>,
    moved: Option<String>,
    after: Option<String>,
}

#[derive(Deserialize)]
struct WireRecentChange {
    #[serde(default)]
    removed: Vec<RecentKey>,
    #[serde(default)]
    inserted: Vec<WireRecentInsertion>,
}

#[derive(Deserialize)]
struct WireRecentInsertion {
    index: usize,
    item: WireHistory,
}

impl<'de> Deserialize<'de> for Update {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let invalid = |_| serde::de::Error::custom("invalid personal projection");
        let summary = |p: PlaylistSummary| p.checked(None, None, None).map_err(invalid);
        let wire = WireUpdate::deserialize(de)?;
        match (wire.snapshot, wire.part, wire.change, wire.missing) {
            (Some(s), None, None, None) => Ok(Self::Snapshot(Snapshot {
                playlists: s
                    .playlists
                    .into_iter()
                    .map(summary)
                    .collect::<Result<_, _>>()?,
                recent: s
                    .recent
                    .into_iter()
                    .map(|r| r.project().map_err(invalid))
                    .collect::<Result<_, _>>()?,
            })),
            (None, Some(p), None, None) => {
                let summary = summary(p.summary)?;
                let rows = p
                    .rows
                    .into_iter()
                    .map(|r| r.project_scoped(summary.kind).map_err(invalid))
                    .collect::<Result<_, _>>()?;
                Ok(Self::Part(Part {
                    summary,
                    definition: p.definition,
                    token: p.token,
                    offset: p.offset,
                    rows,
                    more: p.more,
                }))
            }
            (None, None, Some(d), None) => {
                let entries = d
                    .entries
                    .map(|e| -> Result<_, D::Error> {
                        Ok(EntryChange {
                            added: e.added.map(|r| r.project().map_err(invalid)).transpose()?,
                            removed: e.removed,
                            moved: e.moved,
                            after: e.after,
                        })
                    })
                    .transpose()?;
                let recent = d
                    .recent
                    .map(|r| -> Result<_, D::Error> {
                        Ok(RecentChange {
                            removed: r.removed,
                            inserted: r
                                .inserted
                                .into_iter()
                                .map(|i| {
                                    Ok(RecentInsertion {
                                        index: i.index,
                                        item: i.item.project().map_err(invalid)?,
                                    })
                                })
                                .collect::<Result<_, D::Error>>()?,
                        })
                    })
                    .transpose()?;
                Ok(Self::Change(Change {
                    playlist: d.playlist.map(summary).transpose()?,
                    deleted: d.deleted,
                    entries,
                    recent,
                    rows_invalid: d.rows_invalid,
                }))
            }
            (None, None, None, Some(m)) if id_ok(&m.playlist_id) => {
                Ok(Self::Missing { playlist_id: m.playlist_id })
            }
            _ => Err(serde::de::Error::custom(
                "expected exactly one personal update",
            )),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PersonalError;

impl std::fmt::Display for PersonalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Invalid account personal update")
    }
}
impl std::error::Error for PersonalError {}

#[derive(Default, PartialEq, Eq)]
enum Pull {
    #[default]
    Idle,
    /// A matching token produces no frame; never wait for an ACK.
    Probe,
    /// An optional quick-add verification read, including after navigation.
    Catchup,
    /// An initial full part or continuation is in flight.
    Required,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Membership {
    Unknown,
    Present,
    Absent,
}

struct Retained {
    playlist: Playlist,
    bytes: usize,
    // Track membership is boolean to the caller, but we must keep occurrence
    // counts so removing one copy of a song cannot remove another's evidence.
    tracks: HashMap<Box<str>, usize>,
    // Only retained while building a part set. Check each new part before any
    // of its track IDs become available as positive evidence.
    staged_entries: HashSet<Box<str>>,
}

impl Retained {
    fn add_track(&mut self, id: &str) {
        if let Some(count) = self.tracks.get_mut(id) {
            *count += 1;
        } else {
            self.tracks.insert(id.into(), 1);
        }
    }

    fn remove_track(&mut self, id: &str) {
        if let Some(count) = self.tracks.get_mut(id) {
            *count -= 1;
            if *count == 0 {
                self.tracks.remove(id);
            }
        }
    }
}

// Part sets are scoped to their resource, not the visible route. The server
// echoes a playlist ID and continuation token, not our local request identity.
#[derive(Default)]
struct ReadScope {
    observed: Option<PlaylistSummary>,
    unavailable: bool,
    staged: Option<Retained>,
    pull: Pull,
    expected_sync: Option<Command>,
    sync_started_after: Option<u64>,
    repairing: bool,
}

/// Account-owned state. Keep it across reconnects, call `disconnected` on
/// retirement, and `clear` on account change. Never pass old-epoch input.
#[derive(Default)]
pub struct Replica {
    snapshot: Option<Snapshot>,
    ready: bool,
    desired: Option<String>,
    observed: Option<PlaylistSummary>,
    unavailable: bool,
    details: VecDeque<Retained>,
    background: HashMap<String, ReadScope>,
    staged: Option<Retained>,
    pull: Pull,
    expected_sync: Option<Command>,
    sync_started_after: Option<u64>,
    repairing: bool,
}

impl Replica {
    pub fn snapshot(&self) -> Option<&Snapshot> {
        self.snapshot.as_ref()
    }

    pub fn playlist(&self) -> Option<&Playlist> {
        self.cached(self.desired.as_deref()?).map(|d| &d.playlist)
    }

    #[cfg(test)]
    pub fn ready(&self) -> bool {
        self.ready
    }

    /// Recheck a queued sync at the transport boundary: navigation, repair,
    /// or a newer continuation may already have superseded it.
    pub fn accepts_sync(&self, command: &Command) -> bool {
        let Command::SyncPlaylist { playlist_id, .. } = command else { return false; };
        let expected = if self.desired.as_deref() == Some(playlist_id) {
            self.expected_sync.as_ref()
        } else {
            self.background.get(playlist_id).and_then(|p| p.expected_sync.as_ref())
        };
        let Some(expected @ Command::SyncPlaylist { local_request: pending, .. }) = expected else { return false; };
        self.ready
            && expected == command
            && matches!(command, Command::SyncPlaylist { local_request, .. } if Arc::ptr_eq(pending, local_request))
    }

    /// Called after the transport started the exact current request, under the
    /// same admission guard. A queued or superseded request cannot admit a miss.
    pub(crate) fn started_sync(&mut self, command: &Command, after: u64) {
        debug_assert!(self.accepts_sync(command));
        let Command::SyncPlaylist { playlist_id, .. } = command else { return; };
        if self.desired.as_ref() == Some(playlist_id) {
            self.sync_started_after = Some(after);
        } else if let Some(read) = self.background.get_mut(playlist_id) {
            read.sync_started_after = Some(after);
        }
    }

    pub(crate) fn pending_sync(&self) -> Option<&Command> {
        self.expected_sync.as_ref()
    }

    pub(crate) fn pending_sync_for(&self, id: &str) -> Option<&Command> {
        if self.desired.as_deref() == Some(id) {
            self.pending_sync()
        } else {
            self.background.get(id)?.expected_sync.as_ref()
        }
    }

    /// The current account snapshot or a scoped missing reply confirmed absence.
    pub fn playlist_unavailable(&self) -> bool {
        self.unavailable
    }

    pub fn command(&self, command: Command) -> Option<Command> {
        if !self.ready || !command.valid() { return None; }
        let snapshot = self.snapshot.as_ref()?;
        let listed = |id: &str| snapshot.playlists.iter().find(|p| p.id == id);
        let allowed = match &command {
            Command::SyncPlaylist { .. } => false,
            Command::CreatePlaylist { .. } | Command::ClearHistory => true,
            Command::RenamePlaylist { playlist_id, .. } => listed(playlist_id).is_some_and(|p| p.capabilities.edit_metadata),
            Command::DeletePlaylist { playlist_id } => listed(playlist_id).is_some_and(|p| p.capabilities.delete),
            Command::CopyPlaylist { playlist_id, .. } => listed(playlist_id).is_some_and(|p| p.capabilities.copy_to_manual),
            Command::SetPlaylistRules { playlist_id, .. } => listed(playlist_id).is_some_and(|p| p.capabilities.edit_definition),
            Command::AddEntry { playlist_id, .. } => listed(playlist_id).is_some_and(|p| p.capabilities.add_tracks),
            Command::RemoveEntry { playlist_id, .. } | Command::MoveEntry { playlist_id, .. } => {
                listed(playlist_id).is_some_and(|p| p.capabilities.edit_entries)
            }
        };
        allowed.then_some(command)
    }

    pub(crate) fn watched_playlist(&self) -> Option<&str> {
        self.desired.as_deref()
    }

    pub(crate) fn scoped_summary(&self) -> Option<&PlaylistSummary> {
        let id = self.desired.as_deref()?;
        self.staged
            .as_ref()
            .or_else(|| self.cached(id).filter(|d| !d.playlist.stale))
            .map(|d| &d.playlist.summary)
    }

    pub(crate) fn membership(&self, playlist_id: &str, track_id: &str) -> Membership {
        if !self.ready || self.desired.as_deref() != Some(playlist_id) {
            return Membership::Unknown;
        }
        if let Some(stage) = &self.staged {
            return if stage.playlist.summary.kind == PlaylistKind::Manual
                && stage.tracks.contains_key(track_id)
            {
                Membership::Present
            } else {
                Membership::Unknown
            };
        }
        match self
            .cached(playlist_id)
            .filter(|d| !d.playlist.stale && d.playlist.summary.kind == PlaylistKind::Manual)
        {
            Some(detail) if detail.tracks.contains_key(track_id) => Membership::Present,
            Some(_) => Membership::Absent,
            None => Membership::Unknown,
        }
    }

    /// Catch up after an unobserved add. This is a read, not a receipt or a
    /// write retry. Leave a complete projection usable until a new part set
    /// actually arrives; a matching token might emit no frame at all.
    pub(crate) fn probe_playlist(&mut self) -> Option<Command> {
        if !self.ready || self.unavailable || self.pull == Pull::Required {
            return None;
        }
        let id = self.desired.clone()?;
        let detail = &self.cached(&id)?.playlist;
        if detail.stale {
            return None;
        }
        let known = (!detail.token.is_empty()).then(|| detail.token.clone());
        let command = self.sync(id, known, 0);
        // Even an unconditional catch-up shouldn't suppress a later repair:
        // this optional request can be dropped at local queue admission.
        self.pull = Pull::Catchup;
        Some(command)
    }

    pub(crate) fn is_catchup_sync(&self) -> bool {
        self.pull == Pull::Catchup
    }

    pub(crate) fn refresh_playlist(&mut self) -> Option<Command> {
        if !self.ready || self.unavailable {
            return None;
        }
        let id = self.desired.clone()?;
        self.reset_pull();
        self.mark_stale(&id);
        Some(self.sync(id, None, 0))
    }

    pub(crate) fn seed_detail(&mut self, playlist: Playlist) {
        let id = &playlist.summary.id;
        // Disk can supply last-good rows, never account authority. At least
        // the live summary must prove the exact revision before seeding rows.
        if !self.ready || self.summary(id) != Some(&playlist.summary) {
            return;
        }
        if self.cached(id).is_some_and(|d| {
            !d.playlist.stale || d.playlist.summary.revision > playlist.summary.revision
        }) {
            return;
        }
        if let Some(i) = self.cache_index(id) {
            self.details.remove(i);
        }
        let mut detail = Retained {
            playlist,
            bytes: 0,
            tracks: HashMap::new(),
            staged_entries: HashSet::new(),
        };
        let rows = detail.playlist.rows.clone();
        for row in rows.iter() {
            detail.bytes = detail.bytes.saturating_add(row_bytes(row));
            if detail.playlist.summary.kind == PlaylistKind::Manual {
                detail.add_track(&row.track.id);
            }
        }
        self.details.push_back(detail);
    }

    // The v3 detail protocol can answer a conditional read with silence.
    // v2 refreshes use an unconditional first part: freshness can only be
    // advanced by an explicit, complete response.
    pub(crate) fn read_full_detail(&mut self) -> Option<Command> {
        let id = self.desired.clone()?;
        if !self.ready || self.unavailable { return None; }
        self.reset_pull();
        Some(self.sync(id, None, 0))
    }

    pub(crate) fn defer_detail(&mut self) {
        self.reset_pull();
    }

    pub(crate) fn retire_background(&mut self, id: &str) {
        self.background.remove(id);
    }

    fn take_read(&mut self) -> ReadScope {
        ReadScope {
            observed: self.observed.take(),
            unavailable: std::mem::take(&mut self.unavailable),
            staged: self.staged.take(),
            pull: std::mem::take(&mut self.pull),
            expected_sync: self.expected_sync.take(),
            sync_started_after: self.sync_started_after.take(),
            repairing: std::mem::take(&mut self.repairing),
        }
    }

    fn restore_read(&mut self, scope: ReadScope) {
        self.observed = scope.observed;
        self.unavailable = scope.unavailable;
        self.staged = scope.staged;
        self.pull = scope.pull;
        self.expected_sync = scope.expected_sync;
        self.sync_started_after = scope.sync_started_after;
        self.repairing = scope.repairing;
    }

    pub(crate) fn watch_shared(&mut self, id: Option<String>) -> Option<Command> {
        let id = id.filter(|id| id_ok(id));
        if self.desired == id { return None; }
        let parked = id.as_ref().and_then(|id| self.background.remove(id));
        // Bound abandoned reads even if the peer never finishes them. This
        // does not limit the number of durably saved detail projections.
        if self.pull == Pull::Required && self.background.len() < MAX_DETAILS
            && let Some(previous) = self.desired.clone()
        {
            let read = self.take_read();
            self.background.insert(previous, read);
        }
        if let Some(parked) = parked {
            self.desired = id;
            self.restore_read(parked);
            return None;
        }
        self.watch(id)
    }

    pub fn watch(&mut self, id: Option<String>) -> Option<Command> {
        let id = id.filter(|id| id_ok(id));
        if self.desired == id {
            return None;
        }
        self.reset_pull();
        self.observed = None;
        self.unavailable = false;
        self.desired = id;
        if let Some(i) = self.desired.as_deref().and_then(|id| self.cache_index(id)) {
            let retained = self.details.remove(i).expect("located detail");
            self.details.push_back(retained);
        }
        self.evict_details();
        self.start_watch()
    }

    pub fn disconnected(&mut self) {
        self.ready = false;
        self.background.clear();
        self.reset_pull();
        self.invalidate_details();
    }

    fn invalidate_details(&mut self) {
        for d in &mut self.details {
            d.playlist.stale = true;
            d.playlist.token.clear();
        }
    }

    #[cfg(test)]
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    #[cfg(test)]
    pub fn receive(&mut self, update: Update) -> Result<Option<Command>, PersonalError> {
        self.receive_observed(update, Some(1), |_| {})
    }

    pub(crate) fn receive_observed(
        &mut self,
        update: Update,
        ingress: Option<u64>,
        mut accept: impl FnMut(AcceptedPlaylist<'_>),
    ) -> Result<Option<Command>, PersonalError> {
        if let Update::Missing { playlist_id } = &update {
            let started = if self.desired.as_ref() == Some(playlist_id) {
                self.sync_started_after
            } else {
                self.background.get(playlist_id).and_then(|read| read.sync_started_after)
            };
            // An already queued frame from a retired request cannot settle a
            // newer local request even when both requests used the same ID.
            if self.ready && started.zip(ingress).is_some_and(|(after, stamp)| stamp > after) {
                self.delete_playlist(playlist_id);
                accept(AcceptedPlaylist::Deleted(playlist_id));
            }
            return Ok(None);
        }
        let resource = match &update {
            Update::Part(p) => Some(p.summary.id.as_str()),
            Update::Change(c) => c.playlist.as_ref().map(|p| p.id.as_str()),
            _ => None,
        };
        if let Some(id) = resource.filter(|id| self.desired.as_deref() != Some(*id))
            && let Some(parked) = self.background.remove(id)
        {
            let id = id.to_owned();
            let visible = self.desired.replace(id.clone());
            let visible_read = self.take_read();
            self.restore_read(parked);
            let result = match update {
                Update::Part(p) if self.ready => self.receive_part(p, &mut accept),
                Update::Change(c) if self.ready => self.receive_change(c, &mut accept),
                _ => Ok(None),
            };
            let updated = self.take_read();
            if result.is_ok() && updated.expected_sync.is_some() { self.background.insert(id, updated); }
            self.desired = visible;
            self.restore_read(visible_read);
            return result;
        }
        match update {
            Update::Snapshot(s) => {
                let command = self.install_snapshot(s)?;
                accept(AcceptedPlaylist::Summaries(
                    &self
                        .snapshot
                        .as_ref()
                        .expect("installed snapshot")
                        .playlists,
                ));
                Ok(command)
            }
            Update::Part(p) if self.ready => self.receive_part(p, &mut accept),
            Update::Change(d) if self.ready => self.receive_change(d, &mut accept),
            _ => Ok(None),
        }
    }

    fn start_watch(&mut self) -> Option<Command> {
        let id = self.desired.clone()?;
        if !self.ready || self.unavailable {
            return None;
        }
        if self.summary(&id).is_none() {
            self.unavailable = true;
            return None;
        }
        let known = self.cached(&id)
            .filter(|d| !d.playlist.stale && !d.playlist.token.is_empty())
            .map(|d| d.playlist.token.clone());
        Some(self.sync(id, known, 0))
    }

    fn sync(&mut self, playlist_id: String, known: Option<String>, offset: usize) -> Command {
        self.pull = if offset == 0 && known.is_some() {
            Pull::Probe
        } else {
            Pull::Required
        };
        let command = Command::SyncPlaylist {
            playlist_id,
            known,
            offset,
            local_request: Arc::new(()),
        };
        self.expected_sync = Some(command.clone());
        self.sync_started_after = None;
        command
    }

    fn reset_pull(&mut self) {
        self.staged = None;
        self.pull = Pull::Idle;
        self.expected_sync = None;
        self.sync_started_after = None;
        self.repairing = false;
    }

    fn summary(&self, id: &str) -> Option<&PlaylistSummary> {
        self.snapshot
            .as_ref()?
            .playlists
            .iter()
            .find(|p| p.id == id)
            .or_else(|| self.observed.as_ref().filter(|p| p.id == id))
    }
    fn cache_index(&self, id: &str) -> Option<usize> {
        self.details
            .iter()
            .position(|d| d.playlist.summary.id == id)
    }
    fn cached(&self, id: &str) -> Option<&Retained> {
        self.details.get(self.cache_index(id)?)
    }

    fn evict_details(&mut self) {
        while self.details.len() > MAX_DETAILS
            || self
                .details
                .iter()
                .map(|d| d.bytes)
                .fold(0, usize::saturating_add)
                > DETAIL_CACHE_BYTES
        {
            let Some(i) = self
                .details
                .iter()
                .position(|d| self.desired.as_deref() != Some(d.playlist.summary.id.as_str()))
            else {
                break;
            };
            self.details.remove(i);
        }
    }

    fn install_snapshot(&mut self, mut s: Snapshot) -> Result<Option<Command>, PersonalError> {
        if s.playlists.len() > MAX_PLAYLISTS
            || s.recent.len() > MAX_RECENT
            || !unique_ids(s.playlists.iter().map(|p| p.id.as_str()))
            || !unique_recent(&s.recent)
            || self.ready && s.playlists.iter().any(|p| self.summary(&p.id).is_some_and(|old| old.revision > p.revision))
        {
            return Err(PersonalError);
        }
        let previous = self.snapshot.as_ref();
        let compatible = |id: &str, staged: Option<&Retained>, observed: Option<&PlaylistSummary>| {
            let before = previous.and_then(|s| s.playlists.iter().find(|p| p.id == id)).or(observed);
            match s.playlists.iter().find(|p| p.id == id) {
                Some(current) => staged.map_or_else(
                    || before.is_none_or(|p| p == current),
                    |part| part.playlist.summary == *current,
                ),
                None => false,
            }
        };
        self.background.retain(|id, read| compatible(id, read.staged.as_ref(), read.observed.as_ref()));
        let keep_visible_pull = self.ready && self.expected_sync.is_some()
            && self.desired.as_deref().is_some_and(|id| compatible(id, self.staged.as_ref(), self.observed.as_ref()));
        if let Some(id) = self.desired.as_deref() {
            let listed = s.playlists.iter().any(|p| p.id == id);
            self.unavailable = !listed;
        }
        if !keep_visible_pull {
            self.observed = None;
            self.reset_pull();
        }
        self.details.retain_mut(|d| {
            let Some(p) = s.playlists.iter().find(|p| p.id == d.playlist.summary.id) else {
                return false;
            };
            if p.kind != d.playlist.summary.kind {
                return false;
            }
            if *p != d.playlist.summary {
                d.playlist.stale = true;
                d.playlist.token.clear();
                d.playlist.summary = p.clone();
            }
            true
        });
        sort_summaries(&mut s.playlists);
        self.snapshot = Some(s);
        self.ready = true;
        Ok(if keep_visible_pull { None } else { self.start_watch() })
    }

    fn receive_part(
        &mut self,
        p: Part,
        accept: &mut impl FnMut(AcceptedPlaylist<'_>),
    ) -> Result<Option<Command>, PersonalError> {
        let id = p.summary.id.clone();
        if self.desired.as_deref() != Some(&id) || self.pull == Pull::Idle {
            return Ok(None);
        }
        if p.offset > 0
            && matches!(
                self.expected_sync,
                Some(Command::SyncPlaylist { offset: 0, .. })
            )
        {
            // A prior visit or invalidation may leave one response in flight.
            // It cannot start or complete the replacement requested at zero.
            return Ok(None);
        }
        let current = self.summary(&id);
        if !token_ok(&p.token)
            || p.rows.is_empty() && p.more
            || current.is_some_and(|old| p.summary.kind != old.kind)
            || (p.summary.kind == PlaylistKind::Derived) != p.definition.is_some()
        {
            return Err(PersonalError);
        }
        if current.is_some_and(|old| p.summary.revision < old.revision) {
            return self.repair_part(id);
        }
        let mut stage = if p.offset == 0 {
            if self
                .staged
                .as_ref()
                .is_some_and(|s| s.playlist.token == p.token && s.playlist.summary == p.summary)
            {
                // A conditional probe and its unconditional repair can both
                // yield the first part. A continuation is already in flight.
                return Ok(None);
            }
            Retained {
                bytes: 0,
                tracks: HashMap::new(),
                staged_entries: HashSet::new(),
                playlist: Playlist {
                    summary: p.summary,
                    definition: p.definition,
                    token: p.token,
                    rows: Arc::default(),
                    stale: false,
                },
            }
        } else {
            let Some(stage) = self.staged.take() else {
                return self.repair_part(id);
            };
            if stage.playlist.token != p.token
                || stage.playlist.summary != p.summary
                || stage.playlist.definition != p.definition
            {
                return self.repair_part(id);
            }
            if p.offset < stage.playlist.rows.len() {
                self.staged = Some(stage);
                return Ok(None);
            }
            if p.offset != stage.playlist.rows.len() {
                return self.repair_part(id);
            }
            stage
        };
        if !p.rows.windows(2).all(|r| r[0].position < r[1].position)
            || matches!((stage.playlist.rows.last(), p.rows.first()), (Some(a), Some(b)) if a.position >= b.position)
        {
            return self.repair_part(id);
        }
        // The manual summary counts the complete visible row set exactly.
        // Derived evaluation can have a different live count.
        let end = stage.playlist.rows.len().saturating_add(p.rows.len()) as u64;
        if stage.playlist.summary.kind == PlaylistKind::Manual
            && stage
                .playlist
                .summary
                .track_count
                .is_some_and(|total| end > total || p.more && end == total)
        {
            return self.repair_part(id);
        }
        for row in &p.rows {
            if row.id.is_empty() || !stage.staged_entries.insert(row.id.as_str().into()) {
                return self.repair_part(id);
            }
            if stage.playlist.summary.kind == PlaylistKind::Manual {
                stage.add_track(&row.track.id);
            }
            stage.bytes = stage.bytes.saturating_add(row_bytes(row));
        }
        Arc::make_mut(&mut stage.playlist.rows).extend(p.rows);
        if p.more {
            let next_offset = stage.playlist.rows.len();
            let token = stage.playlist.token.clone();
            // A reply to a cache probe means the server has different rows.
            // Keep last-good rows visible but non-actionable until commit.
            self.mark_stale(&id);
            self.staged = Some(stage);
            return Ok(Some(self.sync(id, Some(token), next_offset)));
        }
        if !visible_count_matches(&stage.playlist.summary, stage.playlist.rows.len()) {
            return self.repair_part(id);
        }
        stage.staged_entries = HashSet::new();
        self.reset_pull();
        self.observed = Some(stage.playlist.summary.clone());
        let snapshot = self.snapshot.as_mut().expect("ready snapshot");
        replace_summary(&mut snapshot.playlists, stage.playlist.summary.clone());
        if let Some(i) = self.cache_index(&id) {
            self.details.remove(i);
        }
        self.details.push_back(stage);
        accept(AcceptedPlaylist::Complete(
            &self.details.back().expect("installed detail").playlist,
        ));
        self.evict_details();
        Ok(None)
    }

    fn mark_stale(&mut self, id: &str) {
        if let Some(i) = self.cache_index(id) {
            self.details[i].playlist.stale = true;
            self.details[i].playlist.token.clear();
        }
    }

    fn repair_part(&mut self, id: String) -> Result<Option<Command>, PersonalError> {
        self.staged = None;
        self.mark_stale(&id);
        if self.repairing {
            return Err(PersonalError);
        }
        self.repairing = true;
        Ok(Some(self.sync(id, None, 0)))
    }

    fn delete_playlist(&mut self, id: &str) {
        self.snapshot.as_mut().expect("ready snapshot").playlists.retain(|p| p.id != id);
        self.details.retain(|d| d.playlist.summary.id != id);
        self.background.remove(id);
        if self.desired.as_deref() == Some(id) {
            self.reset_pull();
            self.observed = None;
            self.unavailable = true;
        }
    }

    fn receive_change(
        &mut self,
        d: Change,
        accept: &mut impl FnMut(AcceptedPlaylist<'_>),
    ) -> Result<Option<Command>, PersonalError> {
        let snapshot = self.snapshot.as_ref().expect("ready snapshot");
        if d.deleted.as_deref().is_some_and(|id| !id_ok(id))
            || d.playlist.is_none() && (d.entries.is_some() || d.rows_invalid)
            || d.playlist.as_ref().is_some_and(|p| {
                !id_ok(&p.id)
                    || d.deleted.as_deref() == Some(&p.id)
                    || (d.entries.is_some() && p.kind != PlaylistKind::Manual)
                    || (snapshot.playlists.len() == MAX_PLAYLISTS
                        && !snapshot.playlists.iter().any(|old| old.id == p.id)
                        && !d.deleted.as_ref().is_some_and(|id| snapshot.playlists.iter().any(|old| &old.id == id)))
                    || self
                        .summary(&p.id)
                        .is_some_and(|before| p.kind != before.kind || p.revision < before.revision)
            })
        {
            return Err(PersonalError);
        }
        let recent = d
            .recent
            .map(|e| apply_recent(&snapshot.recent, e))
            .transpose()?;
        let snapshot = self.snapshot.as_mut().expect("ready snapshot");
        if let Some(recent) = recent {
            snapshot.recent = recent;
        }
        if let Some(id) = d.deleted {
            self.delete_playlist(&id);
            accept(AcceptedPlaylist::Deleted(&id));
        }
        let Some(p) = d.playlist else {
            return Ok(None);
        };
        if self.desired.as_deref() == Some(&p.id) {
            self.observed = Some(p.clone());
            self.unavailable = false;
        }
        replace_summary(
            &mut self.snapshot.as_mut().expect("ready snapshot").playlists,
            p.clone(),
        );
        let id = p.id.clone();
        let observes_rows = d.entries.is_some();
        let mut repair = true;
        if let Some(i) = self.cache_index(&id) {
            let previous = &mut self.details[i];
            repair = d.rows_invalid
                || previous.playlist.stale
                || !apply_entries(previous, &p, d.entries);
            previous.playlist.summary = p;
            previous.playlist.token.clear();
            previous.playlist.stale = repair;
            if !repair && observes_rows {
                accept(AcceptedPlaylist::Complete(&previous.playlist));
            } else {
                accept(AcceptedPlaylist::Summary {
                    summary: &previous.playlist.summary,
                    invalidates_membership: d.rows_invalid || observes_rows,
                });
            }
            self.evict_details();
        } else {
            accept(AcceptedPlaylist::Summary {
                summary: &p,
                invalidates_membership: d.rows_invalid || observes_rows,
            });
        }
        if self.desired.as_deref() != Some(&id) {
            return Ok(None);
        }
        // Invalidate the expected continuation as well as its rows. A queued
        // request is now refused by accepts_sync; an already sent reply may
        // either restart at zero (Go's token changed) or be ignored above.
        let interrupted = self.staged.take().is_some();
        if interrupted || repair && self.pull != Pull::Required {
            self.repairing = false;
            return Ok(Some(self.sync(id, None, 0)));
        }
        Ok(None)
    }
}

fn replace_summary(summaries: &mut Vec<PlaylistSummary>, p: PlaylistSummary) {
    if let Some(current) = summaries.iter_mut().find(|s| s.id == p.id) {
        *current = p;
    } else {
        summaries.push(p);
    }
    sort_summaries(summaries);
}

pub(crate) fn sort_summaries(summaries: &mut [PlaylistSummary]) {
    // Go emits UTC RFC3339Nano; a whole second's trailing `Z` must sort
    // before any fractional value within that second.
    fn key(p: &PlaylistSummary) -> &str {
        p.updated_at.trim_end_matches('Z')
    }
    summaries.sort_by(|a, b| key(b).cmp(key(a)).then_with(|| a.id.cmp(&b.id)));
}

fn unique_ids<'a>(ids: impl Iterator<Item = &'a str>) -> bool {
    let mut seen = HashSet::new();
    ids.into_iter().all(|id| !id.is_empty() && seen.insert(id))
}

fn recent_key(item: &HistoryEntry) -> (&str, &str) {
    (&item.id, item.origin.as_deref().unwrap_or_default())
}

fn unique_recent(items: &[HistoryEntry]) -> bool {
    let mut seen = HashSet::new();
    items
        .iter()
        .all(|item| id_ok(&item.id) && seen.insert(recent_key(item)))
}

fn apply_recent(
    old: &[HistoryEntry],
    edit: RecentChange,
) -> Result<Vec<HistoryEntry>, PersonalError> {
    if edit.removed.len() > MAX_RECENT || edit.inserted.len() > MAX_RECENT {
        return Err(PersonalError);
    }
    let mut keys = HashSet::new();
    for key in &edit.removed {
        if !keys.insert((key.id.as_str(), key.origin.as_str()))
            || !old
                .iter()
                .any(|r| recent_key(r) == (key.id.as_str(), key.origin.as_str()))
        {
            return Err(PersonalError);
        }
    }
    let mut rows: Vec<_> = old
        .iter()
        .filter(|r| !keys.contains(&recent_key(r)))
        .cloned()
        .collect();
    if rows.len() + edit.inserted.len() > MAX_RECENT {
        return Err(PersonalError);
    }
    let mut previous = None;
    for i in edit.inserted {
        if i.index > rows.len() || previous.is_some_and(|p| p >= i.index) {
            return Err(PersonalError);
        }
        previous = Some(i.index);
        rows.insert(i.index, i.item);
    }
    if !unique_recent(&rows) {
        return Err(PersonalError);
    }
    Ok(rows)
}

fn visible_count_matches(summary: &PlaylistSummary, len: usize) -> bool {
    // Manual details contain precisely the visible (audio-associated) rows.
    // Derived evaluation can be stale/pending with a different live count.
    summary.kind == PlaylistKind::Derived || summary.track_count == Some(len as u64)
}

fn apply_entries(d: &mut Retained, p: &PlaylistSummary, edit: Option<EntryChange>) -> bool {
    let rows = &d.playlist.rows;
    let Some(e) = edit else {
        // Summary-only deltas can rename a playlist; a changed visible count
        // without a row operation requires a scoped replacement.
        return visible_count_matches(p, rows.len())
            && p.available_count == d.playlist.summary.available_count
            && p.track_count == d.playlist.summary.track_count;
    };
    match (
        e.added,
        e.removed,
        e.moved,
        e.after.as_deref().filter(|s| !s.is_empty()),
    ) {
        (Some(row), None, None, None) => {
            if !visible_count_matches(p, rows.len() + 1)
                || rows.iter().any(|r| r.id == row.id)
                || rows.last().is_some_and(|r| r.position >= row.position)
            {
                return false;
            }
            d.bytes = d.bytes.saturating_add(row_bytes(&row));
            d.add_track(&row.track.id);
            Arc::make_mut(&mut d.playlist.rows).push(row);
        }
        (None, Some(id), None, None) => {
            let Some(i) = rows.iter().position(|r| r.id == id) else {
                return false;
            };
            if !visible_count_matches(p, rows.len() - 1) {
                return false;
            }
            let rows = Arc::make_mut(&mut d.playlist.rows);
            let row = rows.remove(i);
            if rows.capacity() > rows.len().saturating_mul(2) {
                rows.shrink_to_fit();
            }
            d.bytes = d.bytes.saturating_sub(row_bytes(&row));
            d.remove_track(&row.track.id);
        }
        (None, None, Some(id), after) => {
            let Some(from) = rows.iter().position(|r| r.id == id) else {
                return false;
            };
            let to = match after {
                None => 0,
                Some(anchor) if anchor != id => {
                    let Some(at) = rows.iter().position(|r| r.id == anchor) else {
                        return false;
                    };
                    at + usize::from(at < from)
                }
                Some(_) => return false,
            };
            if !visible_count_matches(p, rows.len()) {
                return false;
            }
            // Go permutes the visible occurrences in their existing stored
            // slots; the slots need not be consecutive (hidden/deleted rows).
            let range = from.min(to)..=from.max(to);
            let slots: Vec<_> = rows[range.clone()].iter().map(|r| r.position).collect();
            let rows = Arc::make_mut(&mut d.playlist.rows);
            let row = rows.remove(from);
            rows.insert(to, row);
            for (row, slot) in rows[range].iter_mut().zip(slots) {
                row.position = slot;
            }
        }
        _ => return false,
    }
    true
}

fn row_bytes(row: &ManualEntry) -> usize {
    let t = &row.track;
    // Vec growth can reserve spare inline slots. Account for that headroom
    // while using the actual allocated capacities for nested metadata.
    [
        size_of::<ManualEntry>().saturating_mul(2),
        row.id.capacity(),
        t.id.capacity().saturating_mul(2), // includes index key
        size_of::<(Box<str>, usize)>().saturating_mul(2), // index headroom
        t.name.capacity(),
        t.album_id.capacity(),
        t.album_name.capacity(),
        t.album_type.capacity(),
        t.release_date.capacity(),
        t.relation.capacity(),
        t.artists
            .capacity()
            .saturating_mul(size_of::<ArtistCredit>()),
        t.genres.capacity().saturating_mul(size_of::<String>()),
    ]
    .into_iter()
    .chain(
        t.artists
            .iter()
            .flat_map(|a| [a.id.capacity(), a.name.capacity()]),
    )
    .chain(t.genres.iter().map(String::capacity))
    .fold(0, usize::saturating_add)
}

#[cfg(test)]
#[path = "personal_tests.rs"]
mod tests;
