//! Account personal wire projections.

#[cfg(test)]
use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use super::catalog::{
    CatalogItem, CatalogKind, WireItem, derived_id, opaque_token, safe_timestamp,
};
use super::{ApiError, INVALID_INPUT, INVALID_RESPONSE, valid_id};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PlaylistKind {
    Manual,
    Derived,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CountState {
    Exact,
    Stale,
    Pending,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EvaluationState {
    Pending,
    Ready,
    Stale,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaylistCapabilities {
    pub edit_metadata: bool,
    pub edit_entries: bool,
    pub edit_definition: bool,
    pub play: bool,
    pub copy_to_manual: bool,
    pub delete: bool,
    pub add_tracks: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaylistSummary {
    pub id: String,
    pub name: String,
    pub description: String,
    pub kind: PlaylistKind,
    pub count_state: CountState,
    pub evaluation_state: Option<EvaluationState>,
    pub evaluation_error_code: Option<String>,
    pub track_count: Option<u64>,
    pub available_count: Option<u64>,
    pub capabilities: PlaylistCapabilities,
    pub revision: u64,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryEntry {
    pub id: String,
    pub played_at: String,
    pub track: CatalogItem,
    pub origin: Option<String>,
    pub received_at: Option<String>,
    pub metadata_missing: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManualEntry {
    /// Playlist entry identity, not track identity or a derived occurrence ID.
    pub id: String,
    /// Stored slot, not the index of the visible row in the page.
    pub position: u64,
    pub track: CatalogItem,
}

#[cfg(test)]
#[derive(Clone, PartialEq, Eq)]
pub struct DerivedEntry {
    pub occurrence_id: String,
    /// Opaque playback selection capability, absent for nonplayable/stale rows.
    pub selection_token: Option<String>,
    pub position: u64,
    pub track: CatalogItem,
}

#[cfg(test)]
#[derive(Clone, PartialEq, Eq)]
pub struct DerivedPage {
    pub items: Vec<DerivedEntry>,
    pub total: u64,
    pub available_total: u64,
    pub limit: u32,
    pub next_cursor: Option<String>,
    pub revision: Option<u64>,
    pub fence: String,
    /// Evaluation completeness, not pagination completeness.
    pub complete: bool,
    pub evaluation_state: Option<EvaluationState>,
    pub evaluation_error_code: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RuleDefinition {
    pub version: u32,
    pub sources: Vec<RuleSource>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include: Option<RulePredicate>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exclude: Option<RulePredicate>,
    pub duplicates: RuleDuplicates,
    pub sort: RuleSort,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub take: Option<RuleTake>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum RuleSource {
    AllSaved {
        slot_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        take: Option<RuleTake>,
    },
    // Kept for externally authored definitions, independent of the retired UI.
    Likes {
        slot_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        take: Option<RuleTake>,
    },
    Playlist {
        slot_id: String,
        playlist_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        take: Option<RuleTake>,
    },
    TrackedPlaylist {
        slot_id: String,
        playlist_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        take: Option<RuleTake>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RuleDuplicates {
    Keep,
    Unique,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RuleSort {
    SourceOrder,
    Title,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleTake {
    pub side: RuleSide,
    pub count: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuleSide {
    First,
    Last,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "op",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum RulePredicate {
    All { args: Vec<RulePredicate> },
    Any { args: Vec<RulePredicate> },
    Not { arg: Box<RulePredicate> },
    Genre { value: String },
    GenreKnown { value: bool },
    Artist { artist_id: String },
    Album { album_id: String },
    Availability { value: RuleAvailability },
    Membership { source: RuleMembership },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuleAvailability {
    Available,
    Unavailable,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum RuleMembership {
    Likes,
    Playlist { playlist_id: String },
    TrackedPlaylist { playlist_id: String },
}

impl PlaylistSummary {
    pub(crate) fn checked(
        self,
        id: Option<&str>,
        kind: Option<PlaylistKind>,
        revision: Option<u64>,
    ) -> Result<Self, ApiError> {
        if !derived_id(&self.id)
            || id.is_some_and(|id| id != self.id)
            || kind.is_some_and(|kind| kind != self.kind)
            || !revision_ok(self.revision)
            || revision.is_some_and(|r| self.revision < r)
            || metadata(&self.name, &self.description).is_err()
            || !safe_timestamp(&self.created_at)
            || !safe_timestamp(&self.updated_at)
            || !valid_error_code(self.evaluation_error_code.as_deref())
            || match (self.count_state, self.track_count, self.available_count) {
                (CountState::Pending, None, None) => false,
                (CountState::Exact | CountState::Stale, Some(total), Some(available)) => {
                    available > total
                }
                _ => true,
            }
            || (self.kind == PlaylistKind::Manual
                && (self.count_state != CountState::Exact || self.evaluation_state.is_some()))
        {
            return Err(INVALID_RESPONSE);
        }
        Ok(self)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WireHistory {
    id: String,
    played_at: String,
    track: WireItem,
    origin: Option<String>,
    received_at: Option<String>,
    #[serde(default)]
    metadata_missing: bool,
}

impl WireHistory {
    pub(crate) fn project(self) -> Result<HistoryEntry, ApiError> {
        if !valid_id(&self.id)
            || !safe_timestamp(&self.played_at)
            || self
                .origin
                .as_deref()
                .is_some_and(|origin| !matches!(origin, "online" | "offline"))
            || self
                .received_at
                .as_deref()
                .is_some_and(|at| !safe_timestamp(at))
        {
            return Err(INVALID_RESPONSE);
        }
        Ok(HistoryEntry {
            id: self.id,
            played_at: self.played_at,
            track: self
                .track
                .project(Some(CatalogKind::Track), false, self.metadata_missing)?,
            origin: self.origin,
            received_at: self.received_at,
            metadata_missing: self.metadata_missing,
        })
    }
}

#[derive(Deserialize)]
pub(crate) struct WireManualEntry {
    id: String,
    position: u64,
    track: WireItem,
}

impl WireManualEntry {
    pub(crate) fn project(self) -> Result<ManualEntry, ApiError> {
        self.project_scoped(PlaylistKind::Manual)
    }

    pub(crate) fn project_scoped(self, kind: PlaylistKind) -> Result<ManualEntry, ApiError> {
        let valid = match kind {
            PlaylistKind::Manual => valid_id(&self.id),
            PlaylistKind::Derived => self.id.starts_with("o1_") && opaque_token(&self.id, 4096),
        };
        if !valid || self.position > i64::MAX as u64 {
            return Err(INVALID_RESPONSE);
        }
        Ok(ManualEntry {
            id: self.id,
            position: self.position,
            track: self.track.project(Some(CatalogKind::Track), false, false)?,
        })
    }
}

#[cfg(test)]
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireDerivedEntry {
    occurrence_id: String,
    selection_token: Option<String>,
    position: u64,
    track: WireItem,
}

#[cfg(test)]
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireDerivedPage {
    items: Vec<WireDerivedEntry>,
    total: u64,
    available_total: u64,
    limit: u32,
    next_cursor: Option<String>,
    revision: Option<u64>,
    fence: String,
    complete: bool,
    evaluation_state: Option<EvaluationState>,
    evaluation_error_code: Option<String>,
}

#[cfg(test)]
#[derive(Clone, Copy)]
enum DerivedMode {
    Saved,
    Preview(Option<u64>),
}

#[cfg(test)]
impl WireDerivedPage {
    fn project(
        self,
        limit: u32,
        cursor: Option<&str>,
        mode: DerivedMode,
    ) -> Result<DerivedPage, ApiError> {
        if self.limit != limit
            || self.items.len() > limit as usize
            || self.items.len() as u64 > self.total
            || self.available_total > self.total
            || self.revision.is_some_and(|r| !revision_ok(r))
            || self.next_cursor.as_deref().is_some_and(|next| {
                !opaque_token(next, 4096)
                    || Some(next) == cursor
                    || !self.complete
                    || self.items.is_empty()
            })
            || (self.complete && !opaque_token(&self.fence, 4096))
            || (!self.complete && !self.fence.is_empty())
            || !valid_error_code(self.evaluation_error_code.as_deref())
            || match mode {
                DerivedMode::Saved => {
                    self.revision.is_none()
                        || self.evaluation_state.is_none()
                        || self.complete != (self.evaluation_state == Some(EvaluationState::Ready))
                }
                DerivedMode::Preview(revision) => !self.complete || self.revision != revision,
            }
        {
            return Err(INVALID_RESPONSE);
        }
        let mut items = Vec::with_capacity(self.items.len());
        for item in self.items {
            if !opaque_token(&item.occurrence_id, 4096)
                || item.position > i64::MAX as u64
                || items
                    .last()
                    .is_some_and(|row: &DerivedEntry| row.position >= item.position)
            {
                return Err(INVALID_RESPONSE);
            }
            let needs_token = self.complete && matches!(mode, DerivedMode::Saved);
            if item.selection_token.is_some() != needs_token
                || item
                    .selection_token
                    .as_deref()
                    .is_some_and(|s| !opaque_token(s, 8192))
            {
                return Err(INVALID_RESPONSE);
            }
            items.push(DerivedEntry {
                occurrence_id: item.occurrence_id,
                position: item.position,
                selection_token: item.selection_token,
                track: item.track.project(Some(CatalogKind::Track), false, false)?,
            });
        }
        unique(items.iter().map(|row| row.occurrence_id.as_str()))?;
        Ok(DerivedPage {
            items,
            total: self.total,
            available_total: self.available_total,
            limit: self.limit,
            next_cursor: self.next_cursor,
            revision: self.revision,
            fence: self.fence,
            complete: self.complete,
            evaluation_state: self.evaluation_state,
            evaluation_error_code: self.evaluation_error_code,
        })
    }
}

#[cfg(test)]
impl RuleDefinition {
    /// Local structural limits only. Go owns normalization, ownership checks,
    /// cycle detection, and publication admission.
    pub fn is_valid(&self) -> bool {
        if self.version != 1 || !(1..=4).contains(&self.sources.len()) || !take_ok(self.take) {
            return false;
        }
        let mut slots = HashSet::new();
        for source in &self.sources {
            let (slot, take, playlist) = match source {
                RuleSource::AllSaved { slot_id, take } | RuleSource::Likes { slot_id, take } => {
                    (slot_id, *take, None)
                }
                RuleSource::Playlist {
                    slot_id,
                    take,
                    playlist_id,
                }
                | RuleSource::TrackedPlaylist {
                    slot_id,
                    take,
                    playlist_id,
                } => (slot_id, *take, Some(playlist_id)),
            };
            if !slot_ok(slot)
                || !slots.insert(slot)
                || !take_ok(take)
                || playlist.is_some_and(|id| !valid_rule_id(id))
            {
                return false;
            }
        }
        let mut leaves = 0;
        [&self.include, &self.exclude]
            .into_iter()
            .flatten()
            .all(|p| p.valid_at(1, &mut leaves))
    }
}

#[cfg(test)]
impl RulePredicate {
    fn valid_at(&self, depth: u8, leaves: &mut u8) -> bool {
        if depth > 4 {
            return false;
        }
        match self {
            Self::All { args } | Self::Any { args } => {
                return (1..=32).contains(&args.len())
                    && args.iter().all(|arg| arg.valid_at(depth + 1, leaves));
            }
            Self::Not { arg } => return arg.valid_at(depth + 1, leaves),
            _ => (),
        }
        *leaves += 1;
        *leaves <= 32
            && match self {
                Self::Genre { value } => super::valid_text(value, 1000) && !value.contains(','),
                Self::Artist { artist_id } => valid_rule_id(artist_id),
                Self::Album { album_id } => valid_rule_id(album_id),
                Self::Membership {
                    source:
                        RuleMembership::Playlist { playlist_id }
                        | RuleMembership::TrackedPlaylist { playlist_id },
                } => valid_rule_id(playlist_id),
                _ => true,
            }
    }
}

#[cfg(test)]
fn slot_ok(id: &str) -> bool {
    id.strip_prefix("s_").is_some_and(|rest| {
        rest.len() == 32
            && rest
                .bytes()
                .all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f'))
    })
}

#[cfg(test)]
fn valid_rule_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 512 && id.trim() == id && !id.chars().any(char::is_control)
}
#[cfg(test)]
fn take_ok(take: Option<RuleTake>) -> bool {
    take.is_none_or(|t| (1..=i32::MAX as u32).contains(&t.count))
}
fn revision_ok(revision: u64) -> bool {
    (1..=i64::MAX as u64).contains(&revision)
}
fn valid_error_code(code: Option<&str>) -> bool {
    code.is_none_or(|code| {
        !code.is_empty()
            && code.len() <= 64
            && code
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
    })
}

fn metadata<'a>(name: &'a str, description: &str) -> Result<&'a str, ApiError> {
    let name = name.trim();
    if !super::valid_text(name, 400)
        || name.chars().count() > 100
        || description.len() > 2000
        || description.chars().count() > 500
        || description
            .chars()
            .any(|c| c.is_control() && !matches!(c, '\n' | '\t'))
    {
        return Err(INVALID_INPUT);
    }
    Ok(name)
}

#[cfg(test)]
fn unique<'a>(ids: impl Iterator<Item = &'a str>) -> Result<(), ApiError> {
    let mut seen = HashSet::new();
    for id in ids {
        if !seen.insert(id) {
            return Err(INVALID_RESPONSE);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn track() -> Value {
        json!({"kind":"track","id":"example-track","name":"Example song","hasAudio":true,
            "artists":[{"id":"artist-a","name":"Artist A"}],"albumId":"album-a",
            "coverage":{"tracks":1,"inLibrary":1,"albums":0}})
    }

    #[test]
    fn recent_preserves_missing_identity_without_granting_playback() {
        let raw = json!({"id":"synthetic-play","playedAt":"2025-01-01T12:00:00Z",
            "track":{"id":"retired-track","kind":"track","hasAudio":false,
                "coverage":{"tracks":1,"inLibrary":0,"albums":0}},"metadataMissing":true});
        let wire: WireHistory = serde_json::from_value(raw).unwrap();
        let row = wire.project().unwrap();
        assert_eq!(row.track.id, "retired-track");
        assert!(row.metadata_missing);
        assert!(!row.track.has_audio);
    }

    #[test]
    fn rule_shape_matches_go_tagged_unions_and_depth_limit() {
        let encoded = json!({"version":1,
            "sources":[{"slotId":"s_0123456789abcdef0123456789abcdef","kind":"trackedPlaylist","playlistId":"source-a","take":{"side":"last","count":7}}],
            "include":{"op":"all","args":[{"op":"artist","artistId":"artist-a"}, {"op":"genreKnown","value":false}]},
            "exclude":{"op":"membership","source":{"kind":"likes"}},
            "duplicates":"unique","sort":"sourceOrder"});
        let mut rule: RuleDefinition = serde_json::from_value(encoded.clone()).unwrap();
        assert!(rule.is_valid());
        assert_eq!(serde_json::to_value(&rule).unwrap(), encoded);
        let mut too_deep = RulePredicate::Album {
            album_id: "album-a".into(),
        };
        for _ in 0..4 {
            too_deep = RulePredicate::Not {
                arg: Box::new(too_deep),
            };
        }
        rule.include = Some(too_deep);
        assert!(!rule.is_valid());
        let mut misplaced = encoded;
        misplaced["sources"][0]["kind"] = json!("likes");
        assert!(serde_json::from_value::<RuleDefinition>(misplaced).is_err());
    }

    #[test]
    fn saved_candidates_carry_selection_capabilities_but_preview_never_does() {
        let mut raw = json!({"items":[{"occurrenceId":"o1_example","selectionToken":"q1_synthetic","position":0,"track":track()}],
            "total":2,"availableTotal":2,"limit":1,"nextCursor":"c1_next","revision":7,"fence":"f1_fence",
            "complete":true,"evaluationState":"ready"});
        let wire = |raw: &Value| serde_json::from_value::<WireDerivedPage>(raw.clone()).unwrap();
        let saved = wire(&raw).project(1, None, DerivedMode::Saved).unwrap();
        assert_eq!(saved.items[0].occurrence_id, "o1_example");
        assert_eq!(
            saved.items[0].selection_token.as_deref(),
            Some("q1_synthetic")
        );
        assert_eq!(saved.next_cursor.as_deref(), Some("c1_next"));
        assert!(
            wire(&raw)
                .project(1, Some("c1_next"), DerivedMode::Saved)
                .is_err()
        );
        assert!(
            wire(&raw)
                .project(1, None, DerivedMode::Preview(Some(7)))
                .is_err()
        );
        raw["items"][0]["selectionToken"] = Value::Null;
        raw["revision"] = Value::Null;
        raw.as_object_mut().unwrap().remove("evaluationState");
        assert!(
            wire(&raw)
                .project(1, None, DerivedMode::Preview(None))
                .is_ok()
        );
        raw["revision"] = json!(7);
        raw["evaluationState"] = json!("stale");
        raw["complete"] = json!(false);
        raw["fence"] = json!("");
        raw["nextCursor"] = Value::Null;
        let stale = wire(&raw).project(1, None, DerivedMode::Saved).unwrap();
        assert!(!stale.complete);
        assert!(stale.items[0].selection_token.is_none());
    }
}
