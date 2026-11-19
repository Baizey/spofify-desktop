//! Projections of Go 08bb99e's account socket messages. Display metadata is not
//! authentication or an execution fence; tolerate additional display fields.

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

fn published_key(key: u64) -> bool {
    (1..FIRST_PREVIEW_KEY).contains(&key)
}

fn seconds(value: f64) -> bool {
    (0.0..=9_223_372_036.854_776).contains(&value)
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Cursor {
    pub(crate) key: u64,
    pub(crate) chain: String,
    pub(crate) visit: u64,
    pub(crate) run: u64,
    pub(crate) elapsed: f64,
    pub(crate) desired: bool,
    pub(crate) playing: bool,
}

impl Cursor {
    pub(crate) fn same_run(&self, other: &Self) -> bool {
        self.key == other.key
            && self.chain == other.chain
            && self.visit == other.visit
            && self.run == other.run
    }

    pub(super) fn valid(&self) -> bool {
        ((published_key(self.key) && text(&self.chain, 128))
            || (self.key == 0
                && self.chain.is_empty()
                && self.visit == 0
                && !self.desired
                && !self.playing))
            && self.visit <= MAX_SAFE_INTEGER
            && self.run <= MAX_SAFE_INTEGER
            && seconds(self.elapsed)
    }

    // A preview is never a wire reattach cursor, even if it reuses a published row key.
    pub(super) fn reattachable(&self) -> bool {
        self.valid() && published_key(self.key) && !self.chain.starts_with("local:")
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

#[derive(Clone, Debug, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TrackRef {
    pub(crate) id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) entry_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) target_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) occurrence_id: Option<String>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub(crate) enum Selection {
    Track {
        track: TrackRef,
        #[serde(skip_serializing_if = "Option::is_none")]
        context: Option<Context>,
    },
    Collection {
        context: Context,
    },
}

impl Selection {
    pub(crate) fn track(id: String) -> Self {
        Self::Track {
            track: TrackRef {
                id,
                ..TrackRef::default()
            },
            context: None,
        }
    }

    fn valid(&self) -> bool {
        match self {
            Self::Track { track, context } => {
                !track.id.is_empty() && context.as_ref().is_none_or(|c| !c.id.is_empty())
            }
            Self::Collection { context } => !context.id.is_empty(),
        }
    }
}

/// Choices are top-level wire frames; there are no command IDs or receipts.
#[derive(Clone, Debug, Serialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub(crate) enum Command {
    Play {
        selection: Selection,
        #[serde(skip_serializing_if = "Option::is_none")]
        shuffle: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
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
    Release,
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
        #[serde(skip_serializing_if = "Option::is_none")]
        seed: Option<u32>,
    },
    Seek {
        value: f64,
    },
    Select {
        device_id: String,
    },
    Rename {
        device_id: String,
        name: String,
    },
}

impl Command {
    pub(super) fn valid(&self) -> bool {
        match self {
            Self::Play { selection, .. } | Self::Add { selection, .. } => selection.valid(),
            Self::Remove { key } | Self::Jump { key } => published_key(*key),
            Self::Move { key, after } => {
                published_key(*key) && (*after == 0 || published_key(*after))
            }
            Self::Seek { value } => seconds(*value),
            Self::Select { device_id } => text(device_id, 128),
            Self::Rename {
                device_id,
                name: display_name,
            } => text(device_id, 128) && name(display_name),
            _ => true,
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum FactError {
    Blocked,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Fact {
    pub(crate) key: u64,
    pub(crate) chain: String,
    pub(crate) visit: u64,
    pub(crate) run: u64,
    pub(crate) elapsed: f64,
    pub(crate) playing: bool,
    pub(crate) buffering: bool,
    pub(crate) ended: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) landed: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<FactError>,
}

impl Fact {
    pub(super) fn valid(&self) -> bool {
        published_key(self.key)
            && text(&self.chain, 128)
            && !self.chain.starts_with("local:")
            && self.visit <= MAX_SAFE_INTEGER
            && self.run <= MAX_SAFE_INTEGER
            && seconds(self.elapsed)
            && self.ended == self.landed.is_some()
            && self.landed.is_none_or(|key| key == 0 || published_key(key))
            && (!self.ended || !self.playing && !self.buffering)
            && (self.error.is_none() || !self.playing && !self.ended)
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Artist {
    pub(crate) id: Option<String>,
    #[serde(default)]
    pub(crate) name: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
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
    pub(crate) entry_id: Option<String>,
    pub(crate) target_id: Option<String>,
    pub(crate) occurrence_id: Option<String>,
    pub(crate) playback_context: Option<Context>,
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
        published_key(row.key)
            && row.order <= MAX_SAFE_INTEGER
            && !row.id.is_empty()
            && keys.insert(row.key)
    })
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct State {
    #[serde(default)]
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
                || self
                    .queue
                    .iter()
                    .any(|row| row.key == self.collection_first))
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
    // Only frame-local checks. Missing anchors or traversal gaps refer to the
    // receiver's canonical replica, which is repaired with snapshot/sync.
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
    pub(crate) source: Option<String>,
    #[serde(default)]
    pub(crate) observed: bool,
    pub(crate) queue: Option<QueueEdit>,
    pub(crate) cursor: Option<Cursor>,
    pub(crate) repeat: Option<Repeat>,
    pub(crate) shuffle: Option<bool>,
}

impl PlayerDelta {
    fn valid(&self) -> bool {
        self.source.as_deref().is_none_or(|s| text(s, 128))
            && self.queue.as_ref().is_none_or(QueueEdit::valid)
            && self.cursor.as_ref().is_none_or(Cursor::valid)
            && (!self.observed || self.cursor.is_some())
            && (self.queue.is_some()
                || self.cursor.is_some()
                || self.repeat.is_some()
                || self.shuffle.is_some())
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Device {
    pub(crate) id: String,
    pub(crate) device_id: String,
    pub(crate) name: String,
    pub(crate) can_play: bool,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub(crate) struct Outputs {
    pub(crate) owner: String,
    pub(crate) devices: Vec<Device>,
}

impl Outputs {
    fn valid(&self) -> bool {
        // A detached owner is deliberately still named but missing from the roster.
        let mut connections = HashSet::new();
        (self.owner.is_empty() || text(&self.owner, 128))
            && self.devices.len() <= 32
            && self
                .devices
                .iter()
                .all(|d| text(&d.id, 128) && text(&d.device_id, 128) && connections.insert(&d.id))
    }
}

#[derive(Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub(super) enum Incoming {
    Snapshot {
        connection_id: String,
        player: State,
        #[serde(flatten)]
        outputs: Outputs,
    },
    Player {
        player: PlayerDelta,
    },
    Outputs {
        #[serde(flatten)]
        outputs: Outputs,
    },
    Grant {
        basis: u64,
        ms: u64,
        handle: String,
    },
    Personal {
        update: Value,
    },
}

impl Incoming {
    pub(super) fn valid(&self, device: &str) -> bool {
        match self {
            Self::Snapshot {
                connection_id,
                player,
                outputs,
            } => {
                player.valid()
                    && outputs.valid()
                    && outputs
                        .devices
                        .iter()
                        .any(|d| &d.id == connection_id && d.device_id == device)
            }
            Self::Player { player } => player.valid(),
            Self::Outputs { outputs } => outputs.valid(),
            // Expired or unmatched grants are ignored by the presence state machine.
            Self::Grant { .. } => true,
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
    },
    Presence {
        basis: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        handle: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        base: Option<&'a Cursor>,
        #[serde(skip_serializing_if = "Option::is_none")]
        actual: Option<&'a Cursor>,
    },
    Fact {
        #[serde(flatten)]
        fact: &'a Fact,
    },
    Personal {
        command: &'a P,
    },
    Sync,
}
