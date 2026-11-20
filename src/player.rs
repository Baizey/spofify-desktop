//! Playback intents and a local-only queue. The online server owns its queue;
//! commands contain canonical selection identifiers, never client media or paths.
use crate::socket;
use serde::{Deserialize, Serialize};
#[cfg(test)]
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

/// Irreversible per-run revocation, independent of the room's output grant.
/// A socket loss can leave an already-audible run permitted, but cannot admit a
/// source which has never produced a sample. Offline output has its own scope.
#[derive(Clone)]
pub struct Authority {
    revoked: Arc<AtomicBool>,
    output: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
    start: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
}
impl Authority {
    pub(crate) fn online(
        output: impl Fn() -> bool + Send + Sync + 'static,
        start: impl Fn() -> bool + Send + Sync + 'static,
    ) -> Self {
        Self {
            revoked: Arc::default(),
            output: Some(Arc::new(output)),
            start: Some(Arc::new(start)),
        }
    }
    pub fn offline() -> Self {
        Self {
            revoked: Arc::default(),
            output: None,
            start: None,
        }
    }
    pub fn revoke(&self) {
        self.revoked.store(true, Ordering::Release);
    }
    pub fn is_valid(&self) -> bool {
        !self.revoked.load(Ordering::Acquire) && self.output.as_ref().is_none_or(|f| f())
    }
    pub fn may_start(&self) -> bool {
        self.is_valid() && self.start.as_ref().is_none_or(|f| f())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Ok,
    Blocked,
    Media,
    Rejected,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Repeat {
    #[default]
    Off,
    All,
    One,
}
impl Repeat {
    pub fn wire(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::All => "all",
            Self::One => "one",
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Placement {
    Low,
    High,
    Next,
}

/// Catalog identity as exposed by the desk controller.
#[derive(Clone, Serialize)]
pub struct Context {
    pub kind: String,
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fence: Option<String>,
}
impl Context {
    pub fn new(kind: impl Into<String>, id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            id: id.into(),
            name: name.into(),
            revision: None,
            fence: None,
        }
    }
}

#[derive(Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackRef {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub occurrence_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selection_token: Option<String>,
}

#[derive(Clone, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Selection {
    Track {
        track: TrackRef,
        #[serde(skip_serializing_if = "Option::is_none")]
        context: Option<Context>,
    },
    Collection {
        context: Context,
    },
    Occurrence {
        key: u64,
    },
}
impl Selection {
    pub fn track(id: impl Into<String>) -> Self {
        Self::Track {
            track: TrackRef {
                id: id.into(),
                ..TrackRef::default()
            },
            context: None,
        }
    }
    pub fn collection(context: Context) -> Self {
        Self::Collection { context }
    }
}

pub enum Action {
    Play {
        selection: Selection,
        shuffle: bool,
    },
    Enqueue {
        selection: Selection,
        placement: Placement,
    },
    Pause,
    Resume,
    Next,
    Previous,
    Seek(f64),
    Jump(u64),
    Remove(u64),
    Move {
        key: u64,
        after: Option<u64>,
    },
    Clear,
    Close,
    Shuffle(bool),
    Repeat(Repeat),
    SelectDevice(String),
    DeselectDevice,
    RenameDevice {
        device_id: Option<String>,
        name: String,
    },
}

pub(crate) fn key(k: u64) -> bool {
    (1..(1 << 52)).contains(&k)
}
fn text(s: &str, max: usize) -> bool {
    s.len() <= max && !s.chars().any(char::is_control)
}
fn id(s: &str, max: usize) -> bool {
    !s.trim().is_empty() && text(s, max)
}
pub fn seconds(n: f64) -> bool {
    (0.0..=604800.0).contains(&n)
}
impl Context {
    fn valid(&self) -> bool {
        matches!(
            self.kind.as_str(),
            "album" | "artist" | "playlist" | "likes" | "derived"
        ) && id(&self.id, 512)
            && text(&self.name, 512)
    }
}
impl Selection {
    fn valid(&self) -> bool {
        match self {
            Self::Occurrence { key: k } => key(*k),
            Self::Collection { context } => context.valid(),
            Self::Track { track, context } => {
                id(&track.id, 512)
                    && context.as_ref().is_none_or(Context::valid)
                    && match context.as_ref().map(|c| c.kind.as_str()) {
                        Some("derived") => {
                            track.entry_id.is_none()
                                && track.target_id.as_ref() == context.as_ref().map(|c| &c.id)
                                && track.occurrence_id.as_ref().is_some_and(|v| id(v, 4096))
                        }
                        kind => {
                            track.target_id.is_none()
                                && track.occurrence_id.is_none()
                                && track.selection_token.is_none()
                                && if kind == Some("playlist") {
                                    track.entry_id.as_ref().is_some_and(|v| id(v, 512))
                                } else {
                                    track.entry_id.is_none()
                                }
                        }
                    }
            }
        }
    }
}
impl Selection {
    fn into_wire(self) -> socket::Selection {
        // Browse fences and signed preview tokens are HTTP-only; the account
        // resolver looks up the actual occurrence using current stored state.
        fn context(c: Context) -> socket::Context {
            socket::Context {
                kind: match c.kind.as_str() {
                    "album" => socket::ContextKind::Album,
                    "artist" => socket::ContextKind::Artist,
                    "playlist" => socket::ContextKind::Playlist,
                    "likes" => socket::ContextKind::Likes,
                    "derived" => socket::ContextKind::Derived,
                    _ => unreachable!("validated context kind"),
                },
                id: c.id,
                name: c.name,
            }
        }
        match self {
            Self::Track {
                track,
                context: source,
            } => socket::Selection::Track {
                track: socket::TrackRef {
                    id: track.id,
                    entry_id: track.entry_id,
                    target_id: track.target_id,
                    occurrence_id: track.occurrence_id,
                },
                context: source.map(context),
            },
            Self::Collection { context: source } => socket::Selection::Collection {
                context: context(source),
            },
            Self::Occurrence { .. } => unreachable!("occurrences are addressed by jump"),
        }
    }
}
impl Action {
    pub fn depends_on_visible_queue(&self) -> bool {
        matches!(
            self,
            Self::Seek(_)
                | Self::Jump(_)
                | Self::Remove(_)
                | Self::Move { .. }
                | Self::Clear
                | Self::Play {
                    selection: Selection::Occurrence { .. },
                    ..
                }
                | Self::Enqueue {
                    selection: Selection::Occurrence { .. },
                    ..
                }
        )
    }
    pub(crate) fn wire(self, own_device: &str) -> Option<socket::Command> {
        use socket::{Command as C, Intent};
        Some(match self {
            Self::Play {
                selection: Selection::Occurrence { key: k },
                ..
            } => {
                if !key(k) {
                    return None;
                }
                C::Jump { key: k }
            }
            Self::Play { selection, shuffle } if selection.valid() => C::Play {
                selection: selection.into_wire(),
                shuffle: Some(shuffle),
                seed: None,
            },
            Self::Enqueue {
                selection,
                placement,
            } if selection.valid() && !matches!(selection, Selection::Occurrence { .. }) => {
                C::Add {
                    selection: selection.into_wire(),
                    placement: match placement {
                        Placement::Low => Intent::Low,
                        Placement::High => Intent::High,
                        Placement::Next => Intent::Next,
                    },
                }
            }
            Self::Pause => C::Pause,
            Self::Resume => C::Resume,
            Self::Next => C::Next,
            Self::Previous => C::Previous,
            Self::Clear => C::Clear,
            Self::Close => C::Close,
            Self::Seek(value) if seconds(value) => C::Seek { value },
            Self::Jump(k) if key(k) => C::Jump { key: k },
            Self::Remove(k) if key(k) => C::Remove { key: k },
            Self::Move { key: k, after } if key(k) && after.is_none_or(|a| key(a) && a != k) => {
                C::Move {
                    key: k,
                    after: after.unwrap_or(0),
                }
            }
            Self::Shuffle(shuffle) => C::Shuffle {
                shuffle,
                seed: None,
            },
            Self::Repeat(repeat) => C::Repeat {
                repeat: match repeat {
                    Repeat::Off => socket::Repeat::Off,
                    Repeat::All => socket::Repeat::All,
                    Repeat::One => socket::Repeat::One,
                },
            },
            Self::SelectDevice(device_id) if id(&device_id, 128) => C::Select { device_id },
            Self::DeselectDevice => C::Release,
            Self::RenameDevice { device_id, name } => {
                let device_id = device_id.as_deref().unwrap_or(own_device);
                let name = name.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}');
                if !id(device_id, 128) || !id(name, 256) || name.chars().count() > 64 {
                    return None;
                }
                C::Rename {
                    device_id: device_id.into(),
                    name: name.into(),
                }
            }
            _ => return None,
        })
    }
}

/// Presentation metadata for an occurrence in the local-only queue. Engine
/// accepts Command::PlayOffline { queue: Vec<Item>, index, input: PlaybackInput,
/// store: OfflineStore }. The optional crossfade may also verify one successor.
#[derive(Clone)]
pub struct LocalTrack {
    pub id: String,
    pub title: String,
    pub subtitle: String,
    pub album_name: String,
    pub duration: f64,
}
#[derive(Clone)]
pub struct LocalEntry {
    pub key: u64,
    pub track: LocalTrack,
    order: usize,
}
#[derive(Default)]
pub struct LocalQueue {
    pub entries: Vec<LocalEntry>,
    pub position: Option<usize>,
    pub shuffle: bool,
    pub repeat: Repeat,
    pub desired: bool,
    pub elapsed: f64,
    next_key: u64,
    pinned: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    Edit,
    Start,
    Stop,
    Clear,
    Invalid,
}
impl LocalQueue {
    pub fn current(&self) -> Option<&LocalEntry> {
        self.position.and_then(|p| self.entries.get(p))
    }
    pub fn replace(&mut self, tracks: Vec<LocalTrack>, selected: usize, shuffle: bool) -> Change {
        if tracks.len() > crate::offline::MAX_DOWNLOADS
            || selected >= tracks.len()
            || !key(self.next_key + tracks.len() as u64)
            || !tracks.iter().all(|t| {
                id(&t.id, 512)
                    && text(&t.title, 1024)
                    && text(&t.subtitle, 4096)
                    && text(&t.album_name, 1024)
                    && seconds(t.duration)
            })
        {
            return Change::Invalid;
        }
        let mut entries = tracks
            .into_iter()
            .enumerate()
            .map(|(order, track)| {
                self.next_key += 1;
                LocalEntry {
                    key: self.next_key,
                    track,
                    order,
                }
            })
            .collect::<Vec<_>>();
        if shuffle {
            shuffle_entries(&mut entries);
        } else {
            entries.rotate_left(selected);
            for (i, entry) in entries.iter_mut().enumerate() {
                entry.order = i;
            }
        }
        self.entries = entries;
        self.shuffle = shuffle;
        self.pinned = false;
        self.select(0)
    }
    fn select(&mut self, position: usize) -> Change {
        self.position = Some(position);
        self.desired = true;
        self.elapsed = 0.0;
        Change::Start
    }
    fn next_position(&self, natural: bool) -> Option<usize> {
        let p = self.position?;
        if natural && self.repeat == Repeat::One {
            Some(p)
        } else if p + 1 < self.entries.len() {
            Some(p + 1)
        } else if self.repeat == Repeat::All {
            Some(0)
        } else {
            None
        }
    }
    pub fn crossfade_successor(&self) -> Option<&LocalEntry> {
        let to = self.next_position(true)?;
        // Repeat-one (or repeat-all with only one entry) replays from zero.
        (Some(to) != self.position).then(|| &self.entries[to])
    }
    pub fn advance(&mut self, natural: bool) -> Change {
        if let Some(to) = self.next_position(natural) {
            self.select(to)
        } else if self.position.is_some() {
            self.desired = false;
            Change::Stop
        } else {
            Change::Invalid
        }
    }
    pub fn action(&mut self, action: Action) -> Change {
        let current = self.current().map(|e| e.track.duration);
        match action {
            Action::Pause => {
                self.desired = false;
                Change::Stop
            }
            Action::Resume if current.is_some() => {
                if current.is_some_and(|d| d > 0.0 && self.elapsed >= d) {
                    self.elapsed = 0.0;
                }
                self.desired = true;
                Change::Start
            }
            Action::Seek(elapsed) if seconds(elapsed) && current.is_some() => {
                self.elapsed = current
                    .filter(|d| *d > 0.0)
                    .map_or(elapsed, |d| elapsed.min(d));
                if self.desired {
                    Change::Start
                } else {
                    Change::Stop
                }
            }
            Action::Next => self.advance(false),
            Action::Previous if self.position.is_some() => {
                let p = self.position.unwrap();
                self.select(if self.elapsed > 3.0 {
                    p
                } else if p > 0 {
                    p - 1
                } else if self.repeat == Repeat::All {
                    self.entries.len() - 1
                } else {
                    0
                })
            }
            Action::Jump(key) => self
                .entries
                .iter()
                .position(|e| e.key == key)
                .map_or(Change::Invalid, |i| self.select(i)),
            Action::Remove(key) => {
                let Some(i) = self.upcoming(key) else {
                    return Change::Invalid;
                };
                self.entries.remove(i);
                Change::Edit
            }
            Action::Move { key, after } => {
                let Some(from) = self.upcoming(key) else {
                    return Change::Invalid;
                };
                let anchor = match after {
                    Some(a) if a != key => self.upcoming(a),
                    None => self.position,
                    _ => None,
                };
                let Some(anchor) = anchor else {
                    return Change::Invalid;
                };
                let to = if from < anchor { anchor } else { anchor + 1 };
                if from != to {
                    let entry = self.entries.remove(from);
                    self.entries.insert(to, entry);
                    for (i, entry) in self.entries.iter_mut().enumerate() {
                        entry.order = i;
                    }
                    self.shuffle = false;
                    self.pinned = true;
                }
                Change::Edit
            }
            Action::Clear => {
                self.entries.truncate(self.position.map_or(0, |p| p + 1));
                Change::Edit
            }
            Action::Close => {
                let repeat = self.repeat;
                let next_key = self.next_key;
                *self = Self {
                    repeat,
                    next_key,
                    ..Self::default()
                };
                Change::Clear
            }
            Action::Shuffle(on) => {
                if on != self.shuffle && !self.pinned {
                    let after = self.position.map_or(0, |p| p + 1);
                    if on {
                        shuffle_entries(&mut self.entries[after..]);
                    } else {
                        self.entries[after..].sort_by_key(|e| e.order);
                    }
                }
                self.shuffle = on;
                Change::Edit
            }
            Action::Repeat(repeat) => {
                self.repeat = repeat;
                Change::Edit
            }
            _ => Change::Invalid,
        }
    }
    fn upcoming(&self, key: u64) -> Option<usize> {
        let pos = self.position?;
        self.entries
            .iter()
            .position(|e| e.key == key)
            .filter(|i| *i > pos)
    }
}
fn shuffle_entries(entries: &mut [LocalEntry]) {
    // A v4 UUID supplies random bits without adding another RNG dependency.
    // Rejection sampling avoids modulo bias; the queue is bounded to 1000.
    for i in (1..entries.len()).rev() {
        let size = i as u32 + 1;
        let limit = u32::MAX - (u32::MAX % size);
        let j = loop {
            let uuid = uuid::Uuid::new_v4();
            let bytes = uuid.as_bytes();
            let n = u32::from_ne_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
            if n < limit {
                break (n % size) as usize;
            }
        };
        entries.swap(i, j);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn track(id: &str) -> LocalTrack {
        LocalTrack {
            id: id.into(),
            title: id.into(),
            subtitle: "Synthetic".into(),
            album_name: "".into(),
            duration: 60.0,
        }
    }
    #[test]
    fn account_choices_are_canonical_and_move_uses_zero_anchor() {
        let selection = Selection::Track {
            track: TrackRef {
                id: "song".into(),
                entry_id: Some("second".into()),
                ..TrackRef::default()
            },
            context: Some(Context::new("playlist", "list", "Synthetic")),
        };
        let wire = Action::Play {
            selection,
            shuffle: false,
        }
        .wire("desktop")
        .unwrap();
        assert_eq!(
            serde_json::to_value(wire).unwrap(),
            json!({"type":"play","shuffle":false,"selection":{"kind":"track",
            "track":{"id":"song","entryId":"second"},"context":{"kind":"playlist","id":"list","name":"Synthetic"}}})
        );
        let wire = Action::Move {
            key: 7,
            after: None,
        }
        .wire("desktop")
        .unwrap();
        assert_eq!(
            serde_json::to_value(wire).unwrap(),
            json!({"type":"move","key":7,"after":0})
        );
        assert!(
            (Action::Move {
                key: 7,
                after: Some(7)
            })
            .wire("desktop")
            .is_none()
        );
        assert!(Action::Seek(f64::NAN).wire("desktop").is_none());
        assert!(Action::Jump(1 << 52).wire("desktop").is_none());
    }
    #[test]
    fn duplicate_occurrences_rotate_without_history_and_repeat_one_only_on_end() {
        let mut q = LocalQueue::default();
        assert_eq!(
            q.replace(vec![track("same"), track("mid"), track("same")], 2, false),
            Change::Start
        );
        assert_eq!(q.position, Some(0));
        let initial = q.current().unwrap().key;
        assert_eq!(q.entries[1].track.id, "same");
        assert_ne!(initial, q.entries[1].key);
        q.action(Action::Repeat(Repeat::One));
        q.advance(true);
        assert_eq!(q.current().unwrap().key, initial);
        q.advance(false);
        assert_eq!(q.position, Some(1));
        let old = q.entries[2].key;
        assert_eq!(q.action(Action::Remove(initial)), Change::Invalid);
        assert_eq!(q.action(Action::Remove(old)), Change::Edit);
        q.action(Action::Close);
        q.replace(vec![track("new")], 0, false);
        assert!(q.current().unwrap().key > initial);
        assert_eq!(q.repeat, Repeat::One);
    }
    #[test]
    fn saved_crossfade_uses_natural_occurrences_but_not_self_repeat() {
        let mut q = LocalQueue::default();
        q.replace(vec![track("same"), track("same")], 0, false);
        assert_eq!(q.crossfade_successor().unwrap().key, q.entries[1].key);
        q.repeat = Repeat::One;
        assert!(q.crossfade_successor().is_none());
        q.repeat = Repeat::Off;
        q.advance(true);
        assert!(q.crossfade_successor().is_none());
        q.repeat = Repeat::All;
        assert_eq!(q.crossfade_successor().unwrap().key, q.entries[0].key);
        q.advance(true);
        q.action(Action::Clear);
        assert!(q.crossfade_successor().is_none());
    }
    #[test]
    fn shuffle_and_move_preserve_current_occurrence_and_position() {
        let mut q = LocalQueue::default();
        q.replace(
            vec![track("a"), track("b"), track("c"), track("d")],
            1,
            false,
        );
        let keys = q.entries.iter().map(|e| e.key).collect::<Vec<_>>();
        q.elapsed = 12.0;
        q.action(Action::Shuffle(true));
        q.action(Action::Shuffle(false));
        assert_eq!(q.entries.iter().map(|e| e.key).collect::<Vec<_>>(), keys);
        q.action(Action::Move {
            key: keys[3],
            after: None,
        });
        assert_eq!(q.entries[1].key, keys[3]);
        assert_eq!(q.current().unwrap().key, keys[0]);
        assert_eq!(q.elapsed, 12.0);
    }
    #[test]
    fn derived_occurrence_is_preserved_without_sending_browse_credentials() {
        let mut context = Context::new("derived", "mix", "Mix");
        context.revision = Some(7);
        context.fence = Some("browse-fence".into());
        let selection = Selection::Track {
            track: TrackRef {
                id: "song".into(),
                target_id: Some("mix".into()),
                occurrence_id: Some("second".into()),
                selection_token: Some("browse-token".into()),
                ..TrackRef::default()
            },
            context: Some(context),
        };
        let wire = Action::Play {
            selection,
            shuffle: false,
        }
        .wire("desktop")
        .unwrap();
        assert_eq!(
            serde_json::to_value(wire).unwrap()["selection"],
            json!({"kind":"track",
            "track":{"id":"song","targetId":"mix","occurrenceId":"second"},
            "context":{"kind":"derived","id":"mix","name":"Mix"}})
        );
    }
}
