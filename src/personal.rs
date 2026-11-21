//! Account personal read model for Go's v3 account socket. This reducer never
//! reads, sends, persists, or replays commands. The caller fences account/socket
//! epochs before reducing a frame and dispatches the returned sync immediately.

use std::{
    collections::{HashSet, VecDeque},
    mem::size_of,
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
fn id_ok(id: &str) -> bool {
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub playlists: Vec<PlaylistSummary>,
    pub recent: Vec<HistoryEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Playlist {
    pub summary: PlaylistSummary,
    pub definition: Option<RuleDefinition>,
    /// The token of a complete server part set; a delta clears it.
    pub token: String,
    pub rows: Vec<ManualEntry>,
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
}

pub use Command as PersonalCommand;
pub use Update as PersonalUpdate;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireUpdate {
    snapshot: Option<WireSnapshot>,
    part: Option<WirePart>,
    change: Option<WireChange>,
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
        match (wire.snapshot, wire.part, wire.change) {
            (Some(s), None, None) => Ok(Self::Snapshot(Snapshot {
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
            (None, Some(p), None) => {
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
            (None, None, Some(d)) => {
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
    /// An initial full part or continuation is in flight.
    Required,
}

struct Retained {
    playlist: Playlist,
    bytes: usize,
}

/// Account-owned state. Keep it across reconnects, call `disconnected` on
/// retirement, and `clear` on account change. Never pass old-epoch input.
#[derive(Default)]
pub struct Replica {
    snapshot: Option<Snapshot>,
    ready: bool,
    desired: Option<String>,
    // A scoped direct read can observe a playlist outside the summary bound.
    observed: Option<PlaylistSummary>,
    unconfirmed_direct: bool,
    unavailable: bool,
    details: VecDeque<Retained>,
    staged: Option<Retained>,
    pull: Pull,
    expected_sync: Option<Command>,
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
        self.ready && self.expected_sync.as_ref() == Some(command)
    }

    /// The watched playlist was deleted, or the transport ended while an
    /// unconfirmed direct lookup was outstanding. Go has no not-found frame;
    /// this is a display state, not proof that the account lacks the playlist.
    pub fn playlist_unavailable(&self) -> bool {
        self.unavailable
    }

    pub fn command(&self, command: Command) -> Option<Command> {
        (self.ready && !matches!(command, Command::SyncPlaylist { .. }) && command.valid())
            .then_some(command)
    }

    pub fn watch(&mut self, id: Option<String>) -> Option<Command> {
        let id = id.filter(|id| id_ok(id));
        if self.desired == id {
            return None;
        }
        self.reset_pull();
        self.observed = None;
        self.unconfirmed_direct = false;
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
        self.unavailable |= self.unconfirmed_direct;
        self.unconfirmed_direct = false;
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

    pub fn receive(&mut self, update: Update) -> Result<Option<Command>, PersonalError> {
        match update {
            Update::Snapshot(s) => self.install_snapshot(s),
            Update::Part(p) if self.ready => self.receive_part(p),
            Update::Change(d) if self.ready => self.receive_change(d),
            _ => Ok(None),
        }
    }

    fn start_watch(&mut self) -> Option<Command> {
        let id = self.desired.clone()?;
        if !self.ready || self.unavailable {
            return None;
        }
        self.unconfirmed_direct = self.summary(&id).is_none();
        // An unlisted route needs a positive reply, not a conditional
        // request whose silence wouldn't confirm that the scope was read.
        let known = self
            .cached(&id)
            .filter(|d| {
                !self.unconfirmed_direct && !d.playlist.stale && !d.playlist.token.is_empty()
            })
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
        };
        self.expected_sync = Some(command.clone());
        command
    }

    fn reset_pull(&mut self) {
        self.staged = None;
        self.pull = Pull::Idle;
        self.expected_sync = None;
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
        {
            return Err(PersonalError);
        }
        if self.ready {
            self.invalidate_details();
        }
        let capped = s.playlists.len() == MAX_PLAYLISTS;
        if let Some(id) = self.desired.as_deref() {
            let listed = s.playlists.iter().any(|p| p.id == id);
            self.unavailable = !listed
                && (self.unavailable
                    || self.unconfirmed_direct
                    || (!capped && (self.summary(id).is_some() || self.cached(id).is_some())));
        }
        self.observed = None;
        self.unconfirmed_direct = false;
        self.details.retain_mut(|d| {
            let Some(p) = s.playlists.iter().find(|p| p.id == d.playlist.summary.id) else {
                return capped;
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
        sort(&mut s.playlists);
        self.snapshot = Some(s);
        self.reset_pull();
        self.ready = true;
        Ok(self.start_watch())
    }

    fn receive_part(&mut self, p: Part) -> Result<Option<Command>, PersonalError> {
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
        // A part proves the ID exists, even when the list couldn't advertise
        // it. Later disconnects can safely rehydrate this watched scope.
        self.unconfirmed_direct = false;
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
                playlist: Playlist {
                    summary: p.summary,
                    definition: p.definition,
                    token: p.token,
                    rows: Vec::new(),
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
        stage.bytes = p
            .rows
            .iter()
            .map(row_bytes)
            .fold(stage.bytes, usize::saturating_add);
        stage.playlist.rows.extend(p.rows);
        if p.more {
            let next_offset = stage.playlist.rows.len();
            let token = stage.playlist.token.clone();
            // A reply to a cache probe means the server has different rows.
            // Keep last-good rows visible but non-actionable until commit.
            self.mark_stale(&id);
            self.staged = Some(stage);
            return Ok(Some(self.sync(id, Some(token), next_offset)));
        }
        if !unique_ids(stage.playlist.rows.iter().map(|r| r.id.as_str()))
            || !visible_count_matches(&stage.playlist.summary, stage.playlist.rows.len())
        {
            return self.repair_part(id);
        }
        self.reset_pull();
        self.observed = Some(stage.playlist.summary.clone());
        let snapshot = self.snapshot.as_mut().expect("ready snapshot");
        replace_summary(&mut snapshot.playlists, stage.playlist.summary.clone());
        if let Some(i) = self.cache_index(&id) {
            self.details.remove(i);
        }
        self.details.push_back(stage);
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

    fn receive_change(&mut self, d: Change) -> Result<Option<Command>, PersonalError> {
        let snapshot = self.snapshot.as_ref().expect("ready snapshot");
        if d.deleted.as_deref().is_some_and(|id| !id_ok(id))
            || d.playlist.is_none() && (d.entries.is_some() || d.rows_invalid)
            || d.playlist.as_ref().is_some_and(|p| {
                !id_ok(&p.id)
                    || d.deleted.as_deref() == Some(&p.id)
                    || (d.entries.is_some() && p.kind != PlaylistKind::Manual)
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
            snapshot.playlists.retain(|p| p.id != id);
            self.details.retain(|d| d.playlist.summary.id != id);
            if self.desired.as_deref() == Some(&id) {
                self.reset_pull();
                self.observed = None;
                self.unconfirmed_direct = false;
                self.unavailable = true;
            }
        }
        let Some(p) = d.playlist else {
            return Ok(None);
        };
        if self.desired.as_deref() == Some(&p.id) {
            self.observed = Some(p.clone());
            self.unconfirmed_direct = false;
            self.unavailable = false;
        }
        replace_summary(
            &mut self.snapshot.as_mut().expect("ready snapshot").playlists,
            p.clone(),
        );
        let id = p.id.clone();
        let mut repair = true;
        if let Some(i) = self.cache_index(&id) {
            let previous = &mut self.details[i];
            repair = d.rows_invalid
                || previous.playlist.stale
                || !apply_entries(previous, &p, d.entries);
            previous.playlist.summary = p;
            previous.playlist.token.clear();
            previous.playlist.stale = repair;
            self.evict_details();
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
    sort(summaries);
    summaries.truncate(MAX_PLAYLISTS);
}

fn sort(summaries: &mut [PlaylistSummary]) {
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
    let rows = &mut d.playlist.rows;
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
            rows.push(row);
        }
        (None, Some(id), None, None) => {
            let Some(i) = rows.iter().position(|r| r.id == id) else {
                return false;
            };
            if !visible_count_matches(p, rows.len() - 1) {
                return false;
            }
            d.bytes = d.bytes.saturating_sub(row_bytes(&rows.remove(i)));
            if rows.capacity() > rows.len().saturating_mul(2) {
                rows.shrink_to_fit();
            }
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
        t.id.capacity(),
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
