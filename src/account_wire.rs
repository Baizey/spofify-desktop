//! Typed playback v3 frames. The Personal lane owns its independent schema.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub(crate) const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;
pub(crate) const FIRST_PREVIEW_KEY: u64 = 1 << 52;

pub(super) fn text(s: &str, limit: usize) -> bool {
    s.len() <= limit && !s.trim().is_empty() && !s.chars().any(char::is_control)
}

pub(super) fn name(s: &str) -> bool {
    text(s, 256)
        && (1..=64).contains(
            &s.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}')
                .chars()
                .count(),
        )
}

fn id(s: &str) -> bool {
    text(s, 128)
}

fn catalog_id(s: &str) -> bool {
    // Library IDs are opaque; their rules differ from socket/device IDs.
    !s.is_empty() && s.len() <= 512 && !s.contains('\0')
}

pub(super) fn counter(n: u64) -> bool {
    n <= MAX_SAFE_INTEGER
}

fn published_key(key: u64) -> bool {
    (1..FIRST_PREVIEW_KEY).contains(&key)
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

// Optional protocol fields may be absent but cannot be JSON null.
fn present<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(d).map(Some)
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Cursor {
    pub(crate) key: u64,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub(crate) occurrence: Option<String>,
    pub(crate) position_ms: u64,
    pub(crate) desired_playing: bool,
}

impl Cursor {
    pub(super) fn valid(&self) -> bool {
        counter(self.position_ms)
            && (if self.key == 0 {
                self.occurrence.is_none() && !self.desired_playing && self.position_ms == 0
            } else {
                published_key(self.key) && self.occurrence.as_deref().is_some_and(id)
            })
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum Repeat {
    #[default]
    Off,
    All,
    One,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum Intent {
    #[default]
    Low,
    High,
    Next,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum ContextKind {
    Album,
    Artist,
    Playlist,
    Likes,
    Derived,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Context {
    pub(crate) kind: ContextKind,
    pub(crate) id: String,
    #[serde(default)]
    pub(crate) name: String,
}

impl Context {
    fn valid(&self) -> bool {
        catalog_id(&self.id)
            && !self.name.contains('\0')
            && (self.kind != ContextKind::Likes || self.id == "liked-songs")
    }
}

fn valid_provenance(
    context: Option<&Context>,
    entry: Option<&str>,
    target: Option<&str>,
    occurrence: Option<&str>,
) -> bool {
    [entry, target, occurrence]
        .into_iter()
        .flatten()
        .all(catalog_id)
        && match context {
            Some(c) if c.kind == ContextKind::Playlist => {
                entry.is_some() && target.is_none() && occurrence.is_none()
            }
            Some(c) if c.kind == ContextKind::Derived => {
                entry.is_none() && target == Some(c.id.as_str()) && occurrence.is_some()
            }
            _ => entry.is_none() && target.is_none() && occurrence.is_none(),
        }
}

/// A queue row can include server catalog provenance not permitted in a selection.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PlaybackContext {
    #[serde(flatten)]
    pub(crate) context: Context,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub(crate) revision: u64,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub(crate) fence: Option<String>,
}

impl std::ops::Deref for PlaybackContext {
    type Target = Context;

    fn deref(&self) -> &Context {
        &self.context
    }
}

impl From<Context> for PlaybackContext {
    fn from(context: Context) -> Self {
        Self {
            context,
            revision: 0,
            fence: None,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TrackRef {
    pub(crate) id: String,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub(crate) entry_id: Option<String>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub(crate) target_id: Option<String>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub(crate) occurrence_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub(crate) enum Selection {
    Track {
        track: TrackRef,
        #[serde(
            default,
            deserialize_with = "present",
            skip_serializing_if = "Option::is_none"
        )]
        context: Option<Context>,
    },
    Collection {
        context: Context,
    },
}

impl Selection {
    fn valid(&self) -> bool {
        match self {
            Self::Track { track, context } => {
                catalog_id(&track.id)
                    && context.as_ref().is_none_or(Context::valid)
                    && valid_provenance(
                        context.as_ref(),
                        track.entry_id.as_deref(),
                        track.target_id.as_deref(),
                        track.occurrence_id.as_deref(),
                    )
            }
            Self::Collection { context } => context.valid(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub(crate) enum Action {
    Play {
        selection: Selection,
        #[serde(
            default,
            deserialize_with = "present",
            skip_serializing_if = "Option::is_none"
        )]
        shuffle: Option<bool>,
        #[serde(
            default,
            deserialize_with = "present",
            skip_serializing_if = "Option::is_none"
        )]
        seed: Option<u32>,
    },
    Add {
        selection: Selection,
        placement: Intent,
    },
    Pause,
    Resume,
    Next,
    Previous,
    Close,
    Clear,
    Remove {
        key: u64,
    },
    Jump {
        key: u64,
    },
    Move {
        key: u64,
        after: u64,
    },
    Repeat {
        repeat: Repeat,
    },
    Shuffle {
        shuffle: bool,
        #[serde(
            default,
            deserialize_with = "present",
            skip_serializing_if = "Option::is_none"
        )]
        seed: Option<u32>,
    },
    Seek {
        position_ms: u64,
    },
    Select {
        device_id: String,
    },
}

impl Action {
    fn valid(&self) -> bool {
        match self {
            Self::Play { selection, .. } | Self::Add { selection, .. } => selection.valid(),
            Self::Remove { key } | Self::Jump { key } => published_key(*key),
            Self::Move { key, after } => {
                published_key(*key) && (*after == 0 || published_key(*after))
            }
            Self::Seek { position_ms } => counter(*position_ms),
            Self::Select { device_id } => id(device_id),
            _ => true,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CommandBasis {
    pub(crate) session_id: String,
    pub(crate) generation: u64,
    pub(crate) revision: u64,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub(crate) occurrence: Option<String>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub(crate) key: Option<u64>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub(crate) successor_key: Option<u64>,
}

impl CommandBasis {
    fn valid(&self) -> bool {
        id(&self.session_id)
            && counter(self.generation)
            && counter(self.revision)
            && self.occurrence.as_deref().is_none_or(id)
            && !(self.occurrence.is_some() && self.key == Some(0))
            && self.key.is_none_or(counter)
            && self.successor_key.is_none_or(counter)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ReattachBasis {
    pub(crate) session_id: String,
    pub(crate) generation: u64,
    pub(crate) revision: u64,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub(crate) occurrence: Option<String>,
}

impl ReattachBasis {
    pub(super) fn valid(&self) -> bool {
        id(&self.session_id)
            && counter(self.generation)
            && counter(self.revision)
            && self.occurrence.as_deref().is_none_or(id)
    }
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(tag = "type", rename = "command", rename_all = "camelCase")]
pub(crate) struct Command {
    pub(crate) id: String,
    pub(crate) issued_at_ms: u64,
    pub(crate) basis: CommandBasis,
    pub(crate) action: Action,
}

impl<'de> Deserialize<'de> for Command {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(
            tag = "type",
            rename_all = "camelCase",
            rename_all_fields = "camelCase"
        )]
        enum Tagged {
            Command {
                id: String,
                issued_at_ms: u64,
                basis: CommandBasis,
                action: Action,
            },
        }
        let Tagged::Command {
            id,
            issued_at_ms,
            basis,
            action,
        } = Tagged::deserialize(d)?;
        Ok(Self {
            id,
            issued_at_ms,
            basis,
            action,
        })
    }
}

impl Command {
    pub(super) fn valid(&self) -> bool {
        command_id(&self.id)
            && counter(self.issued_at_ms)
            && self.basis.valid()
            && self.action.valid()
    }
}

pub(super) fn command_id(s: &str) -> bool {
    id(s)
        && s.rsplit_once(':').is_some_and(|(connection, index)| {
            id(connection)
                && !index.starts_with('0')
                && !index.is_empty()
                && index.bytes().all(|byte| byte.is_ascii_digit())
                && index.parse::<u64>().is_ok_and(counter)
        })
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Artist {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) id: Option<String>,
    #[serde(default)]
    pub(crate) name: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Track {
    pub(crate) id: String,
    #[serde(default)]
    pub(crate) name: String,
    #[serde(default, deserialize_with = "nullable_default")]
    pub(crate) artists: Vec<Artist>,
    #[serde(default)]
    pub(crate) album_name: String,
    #[serde(default)]
    pub(crate) album_id: String,
    #[serde(default)]
    pub(crate) duration: f64,
    pub(crate) key: u64,
    pub(crate) order: u64,
    pub(crate) intent: Intent,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) entry_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) target_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) occurrence_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) playback_context: Option<PlaybackContext>,
}

fn nullable_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

fn valid_rows(rows: &[Track]) -> bool {
    let mut keys = HashSet::with_capacity(rows.len());
    rows.iter().all(|row| {
        published_key(row.key) && counter(row.order) && !row.id.is_empty() && keys.insert(row.key)
    })
}

impl Track {
    fn valid_import(&self) -> bool {
        published_key(self.order)
            && catalog_id(&self.id)
            && (self.album_id.is_empty() || catalog_id(&self.album_id))
            && (0.0..=(MAX_SAFE_INTEGER as f64 / 1000.0)).contains(&self.duration)
            && [&self.name, &self.album_name]
                .into_iter()
                .all(|s| !s.contains('\0'))
            && self
                .artists
                .iter()
                .all(|a| !a.name.contains('\0') && a.id.as_deref().is_none_or(catalog_id))
            && self.playback_context.as_ref().is_none_or(|c| {
                c.context.valid()
                    && counter(c.revision)
                    && c.fence.as_deref().is_none_or(catalog_id)
            })
            && valid_provenance(
                self.playback_context.as_deref(),
                self.entry_id.as_deref(),
                self.target_id.as_deref(),
                self.occurrence_id.as_deref(),
            )
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct State {
    #[serde(default, skip_serializing_if = "is_zero")]
    pub(crate) collection_first: u64,
    pub(crate) queue: Vec<Track>,
    pub(crate) cursor: Cursor,
    pub(crate) repeat: Repeat,
    pub(crate) shuffle: bool,
}

impl State {
    pub(crate) fn current(&self) -> Option<&Track> {
        self.queue.iter().find(|row| row.key == self.cursor.key)
    }

    fn valid(&self) -> bool {
        self.cursor.valid()
            && valid_rows(&self.queue)
            && (if self.queue.is_empty() {
                self.cursor.key == 0
            } else {
                self.current().is_some()
            })
            && (self.collection_first == 0
                || self.queue.iter().any(|r| r.key == self.collection_first))
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Claim {
    pub(crate) local: Option<State>,
    pub(crate) reattach_basis: Option<ReattachBasis>,
}

impl Claim {
    pub(super) fn local(&self) -> Option<&State> {
        self.local.as_ref().filter(|p| !p.queue.is_empty())
    }

    pub(super) fn valid(&self) -> bool {
        self.reattach_basis
            .as_ref()
            .is_none_or(ReattachBasis::valid)
            && self.local().is_none_or(|local| {
                let mut orders = HashSet::with_capacity(local.queue.len());
                local.valid()
                    && local
                        .queue
                        .iter()
                        .all(|r| r.valid_import() && orders.insert(r.order))
            })
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum QueueEditKind {
    Replace,
    Insert,
    Remove,
    Order,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct QueueEdit {
    pub(crate) kind: QueueEditKind,
    #[serde(default)]
    pub(crate) collection_first: u64,
    #[serde(default)]
    pub(crate) rows: Vec<Track>,
    #[serde(default)]
    pub(crate) rebase_after: u64,
    #[serde(default)]
    pub(crate) before: u64,
    #[serde(default)]
    pub(crate) keys: Vec<u64>,
}

impl QueueEdit {
    // Missing anchors or traversal gaps are relative to the canonical replica.
    fn valid(&self) -> bool {
        if !valid_rows(&self.rows)
            || (self.collection_first != 0
                && !self.rows.iter().any(|r| r.key == self.collection_first))
        {
            return false;
        }
        let mut keys = HashSet::with_capacity(self.keys.len());
        if !self
            .keys
            .iter()
            .all(|&k| published_key(k) && keys.insert(k))
        {
            return false;
        }
        match self.kind {
            QueueEditKind::Replace => {
                self.keys.is_empty() && self.before == 0 && self.rebase_after == 0
            }
            QueueEditKind::Insert => {
                !self.rows.is_empty()
                    && self.keys.is_empty()
                    && self.rebase_after == 0
                    && (self.before == 0 || published_key(self.before))
            }
            QueueEditKind::Remove => {
                self.rows.is_empty()
                    && !self.keys.is_empty()
                    && self.before == 0
                    && self.rebase_after == 0
            }
            QueueEditKind::Order => {
                self.rows.is_empty()
                    && self.before == 0
                    && (self.rebase_after == 0 || keys.contains(&self.rebase_after))
            }
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PlayerDelta {
    #[serde(default, deserialize_with = "present")]
    pub(crate) queue: Option<QueueEdit>,
    #[serde(default, deserialize_with = "present")]
    pub(crate) cursor: Option<Cursor>,
    #[serde(default, deserialize_with = "present")]
    pub(crate) repeat: Option<Repeat>,
    #[serde(default, deserialize_with = "present")]
    pub(crate) shuffle: Option<bool>,
}

impl PlayerDelta {
    fn valid(&self) -> bool {
        self.queue.as_ref().is_none_or(QueueEdit::valid)
            && self.cursor.as_ref().is_none_or(Cursor::valid)
            && (self.queue.is_some()
                || self.cursor.is_some()
                || self.repeat.is_some()
                || self.shuffle.is_some())
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Device {
    pub(crate) device_id: String,
    pub(crate) name: String,
    pub(crate) can_play: bool,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Owner {
    pub(crate) generation: u64,
    #[serde(default, deserialize_with = "present")]
    pub(crate) device_id: Option<String>,
}

fn valid_roster(owner: &Owner, devices: &[Device]) -> bool {
    let mut ids = HashSet::new();
    // A detached owner is deliberately still named but missing from the roster.
    counter(owner.generation)
        && owner.device_id.as_deref().is_none_or(id)
        && devices.len() <= 32
        && devices
            .iter()
            .all(|d| id(&d.device_id) && ids.insert(&d.device_id))
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub(crate) struct Ownership {
    pub(crate) owner: Owner,
    pub(crate) devices: Vec<Device>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Snapshot {
    pub(crate) session_id: String,
    pub(crate) connection_id: String,
    pub(crate) revision: u64,
    pub(crate) owner: Owner,
    pub(crate) devices: Vec<Device>,
    pub(crate) player: State,
    #[serde(default, deserialize_with = "present")]
    pub(crate) observation: Option<Observation>,
    pub(crate) server_time_ms: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub(crate) struct StateUpdate {
    pub(crate) generation: u64,
    pub(crate) revision: u64,
    pub(crate) delta: PlayerDelta,
    #[serde(default, deserialize_with = "present")]
    pub(crate) command: Option<Command>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub(crate) struct Execute {
    pub(crate) generation: u64,
    pub(crate) revision: u64,
    pub(crate) command: Command,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum ObservationStatus {
    Loading,
    Buffering,
    Playing,
    Paused,
    Stopped,
    Failed,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Observation {
    pub(crate) key: u64,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub(crate) occurrence: Option<String>,
    pub(crate) position_ms: u64,
    pub(crate) sampled_at_ms: u64,
    pub(crate) status: ObservationStatus,
}

impl Observation {
    fn valid(&self) -> bool {
        counter(self.position_ms)
            && counter(self.sampled_at_ms)
            && (if self.key == 0 {
                self.occurrence.is_none()
                    && self.status == ObservationStatus::Stopped
                    && self.position_ms == 0
            } else {
                published_key(self.key) && self.occurrence.as_deref().is_some_and(id)
            })
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub(crate) struct ObservationUpdate {
    pub(crate) generation: u64,
    pub(crate) revision: u64,
    pub(crate) observation: Observation,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct Offer {
    pub(crate) id: String,
    pub(crate) generation: u64,
}

impl Offer {
    pub(super) fn valid(&self) -> bool {
        command_id(&self.id) && counter(self.generation)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum ResultStatus {
    Accepted,
    Applied,
    Satisfied,
    Stale,
    Failed,
    Unavailable,
}

impl ResultStatus {
    pub(crate) fn is_terminal(self) -> bool {
        self != Self::Accepted
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub(crate) struct CommandResult {
    pub(crate) id: String,
    pub(crate) status: ResultStatus,
    pub(crate) revision: u64,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Advance {
    pub(crate) from_occurrence: String,
    pub(crate) from_key: u64,
    pub(crate) to_key: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) to_occurrence: Option<String>,
    pub(crate) ended_position_ms: u64,
}

impl Advance {
    fn valid(&self) -> bool {
        id(&self.from_occurrence)
            && published_key(self.from_key)
            && counter(self.ended_position_ms)
            && (if self.to_key == 0 {
                self.to_occurrence.is_none()
            } else {
                published_key(self.to_key)
                    && self
                        .to_occurrence
                        .as_deref()
                        .is_some_and(|s| id(s) && s != self.from_occurrence)
            })
    }
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum SettlementStatus {
    Applied,
    Satisfied,
    Stale,
    Failed,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub(crate) struct Settlement {
    pub(crate) id: String,
    pub(crate) status: SettlementStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) cursor: Option<Cursor>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Report {
    pub(crate) generation: u64,
    pub(crate) revision: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) clock_offset_ms: Option<i64>,
    pub(crate) observation: Observation,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) advance: Option<Advance>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) settlements: Vec<Settlement>,
}

impl Report {
    pub(super) fn valid(&self) -> bool {
        let mut ids = HashSet::new();
        counter(self.generation)
            && counter(self.revision)
            && self
                .clock_offset_ms
                .is_none_or(|ms| counter(ms.unsigned_abs()))
            && self.observation.valid()
            && self.advance.as_ref().is_none_or(Advance::valid)
            && self.settlements.len() <= 64
            && self.settlements.iter().all(|s| {
                command_id(&s.id)
                    && ids.insert(&s.id)
                    && s.cursor
                        .as_ref()
                        .is_none_or(|c| s.status == SettlementStatus::Applied && c.valid())
            })
    }
}

#[derive(Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub(super) enum Incoming {
    Snapshot(Snapshot),
    State(StateUpdate),
    Execute(Execute),
    Observation(ObservationUpdate),
    Ownership(Ownership),
    Offer(Offer),
    Result(CommandResult),
    Pong {
        client_time_ms: u64,
        server_time_ms: u64,
    },
    Personal {
        update: Value,
    },
}

impl Incoming {
    pub(super) fn valid(&self, device: &str, bulk: bool) -> bool {
        match self {
            Self::Snapshot(s) => {
                id(&s.session_id)
                    && id(&s.connection_id)
                    && counter(s.revision)
                    && counter(s.server_time_ms)
                    && s.player.valid()
                    && valid_roster(&s.owner, &s.devices)
                    && s.observation.as_ref().is_none_or(Observation::valid)
                    && s.devices
                        .iter()
                        .any(|d| d.device_id == device && (!bulk || !d.can_play))
                    && (!bulk || s.owner.device_id.as_deref() != Some(device))
            }
            Self::State(s) => {
                counter(s.generation)
                    && counter(s.revision)
                    && s.delta.valid()
                    && s.command.as_ref().is_none_or(Command::valid)
            }
            Self::Execute(e) => {
                counter(e.generation)
                    && counter(e.revision)
                    && e.command.valid()
                    && matches!(e.command.action, Action::Next | Action::Seek { .. })
            }
            Self::Observation(o) => {
                counter(o.generation) && counter(o.revision) && o.observation.valid()
            }
            Self::Ownership(o) => valid_roster(&o.owner, &o.devices),
            Self::Offer(o) => o.valid(),
            Self::Result(r) => command_id(&r.id) && counter(r.revision),
            Self::Pong {
                client_time_ms,
                server_time_ms,
            } => counter(*client_time_ms) && counter(*server_time_ms),
            Self::Personal { update } => update.as_object().is_some_and(|o| {
                let mut known = ["snapshot", "change", "part"]
                    .into_iter()
                    .filter_map(|key| o.get(key));
                known.next().is_some_and(Value::is_object) && known.next().is_none()
            }),
        }
    }
}

#[derive(Serialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub(super) enum Outgoing<'a, P: ?Sized> {
    Hello {
        ticket: &'a str,
        name: &'a str,
        device_id: &'a str,
        can_play: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        local: Option<&'a State>,
        #[serde(skip_serializing_if = "Option::is_none")]
        reattach_basis: Option<&'a ReattachBasis>,
    },
    ReconcileClaim {
        id: &'a str,
        basis: &'a ReattachBasis,
        cursor: &'a Cursor,
    },
    Refresh {
        generation: u64,
    },
    Accept {
        id: &'a str,
        generation: u64,
    },
    Rename {
        device_id: &'a str,
        name: &'a str,
    },
    Report {
        sequence: u64,
        #[serde(flatten)]
        report: &'a Report,
    },
    Ping {
        client_time_ms: u64,
    },
    Personal {
        command: &'a P,
    },
    Sync,
}
