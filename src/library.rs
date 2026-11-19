//! Account-owned listening history and playlists.
//! Declare as an `api.rs` child: `#[path = "library.rs"] pub mod library;`.
//!
//! Browser methods on `Api` (`&Session` first): `history(page)`,
//! `playlists(kind_filter, page)`, `playlist(id)`, `manual_entries(id, page)`,
//! `derived_entries(id, limit, cursor)`, `playlist_definition(id)`,
//! `tracked_playlist_sources(page)`, `preview_definition(definition, target, limit, cursor)`,
//! `create_playlist(name, description, definition)`,
//! `update_playlist(id, revision, name, description)`, `delete_playlist(id, revision)`,
//! `update_playlist_definition(id, revision, definition)`,
//! `add_playlist_track(id, revision, track_id)`, `remove_playlist_entry(id, revision, entry_id)`,
//! `reorder_playlist(id, revision, entry_ids)`, `copy_derived_playlist(id, revision, fence, name, description)`.
//! All offsets use `catalog::PageRequest`, revisions are `u64`, and limits are `u32`.
//! Optional arguments: `kind_filter: Option<PlaylistKind>`, `definition` on create
//! is `Option<&RuleDefinition>` (None creates manual), `target: Option<(&str, u64)>`,
//! rule cursors are `Option<&str>`.

use std::collections::HashSet;

use reqwest::{Method, StatusCode};
use serde::{Deserialize, Serialize};

use super::catalog::{
    CatalogItem, CatalogKind, PageRequest, Route, WireItem, derived_id, limit_ok, opaque_token,
    page_matches, safe_timestamp,
};
use super::{Api, ApiError, INVALID_INPUT, INVALID_RESPONSE, Session, valid_id};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PlaylistKind {
    Manual,
    Derived,
}

impl PlaylistKind {
    fn wire(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Derived => "derived",
        }
    }
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

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OffsetPage<T> {
    pub items: Vec<T>,
    pub total: u64,
    pub offset: u32,
    pub limit: u32,
}

impl<T> OffsetPage<T> {
    pub fn has_more(&self) -> bool {
        u64::from(self.offset) + (self.items.len() as u64) < self.total
    }
}

pub type PlaylistPage = OffsetPage<PlaylistSummary>;
pub type ManualEntriesPage = OffsetPage<ManualEntry>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryEntry {
    pub id: String,
    pub played_at: String,
    pub track: CatalogItem,
    pub origin: Option<String>,
    pub received_at: Option<String>,
    pub metadata_missing: bool,
}

pub type HistoryPage = OffsetPage<HistoryEntry>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManualEntry {
    /// Playlist entry identity, not track identity or a derived occurrence ID.
    pub id: String,
    /// Stored slot, not the index of the visible row in the page.
    pub position: u64,
    pub track: CatalogItem,
}

#[derive(Clone, PartialEq, Eq)]
pub struct DerivedEntry {
    pub occurrence_id: String,
    /// Opaque playback selection capability, absent for nonplayable/stale rows.
    pub selection_token: Option<String>,
    pub position: u64,
    pub track: CatalogItem,
}

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

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct PlaylistDefinition {
    pub definition: RuleDefinition,
    pub revision: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackedPlaylistSource {
    pub id: String,
    pub name: String,
    pub state: SourceState,
    pub track_count: Option<u64>,
    pub saved_at: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SourceState {
    Ready,
    NotReady,
}

impl Api {
    /// Paginated play events, including repeated plays and retired tracks.
    pub async fn history(
        &self,
        session: &Session,
        page: PageRequest,
    ) -> Result<HistoryPage, ApiError> {
        let (limit, offset) = page_params(page)?;
        let wire: OffsetPage<WireHistory> = self
            .catalog_get(
                session,
                Route::History,
                &[("limit", &limit), ("offset", &offset)],
            )
            .await?;
        wire.project(page, WireHistory::project)
    }

    pub async fn playlists(
        &self,
        session: &Session,
        kind_filter: Option<PlaylistKind>,
        page: PageRequest,
    ) -> Result<PlaylistPage, ApiError> {
        let (limit, offset) = page_params(page)?;
        let mut query = vec![("limit", limit.as_str()), ("offset", offset.as_str())];
        if let Some(kind) = kind_filter {
            query.push(("kind", kind.wire()));
        }
        let wire: PlaylistPage = self.catalog_get(session, Route::Playlists, &query).await?;
        let result = wire.project(page, |row| row.checked(None, kind_filter, None))?;
        unique(result.items.iter().map(|row| row.id.as_str()))?;
        Ok(result)
    }

    pub async fn playlist(&self, session: &Session, id: &str) -> Result<PlaylistSummary, ApiError> {
        let wire: PlaylistSummary = self.catalog_get(session, Route::Playlist(id), &[]).await?;
        wire.checked(Some(id), None, None)
    }

    pub async fn manual_entries(
        &self,
        session: &Session,
        id: &str,
        page: PageRequest,
    ) -> Result<ManualEntriesPage, ApiError> {
        let (limit, offset) = page_params(page)?;
        let wire: OffsetPage<WireManualEntry> = self
            .catalog_get(
                session,
                Route::Entries(id),
                &[("limit", &limit), ("offset", &offset)],
            )
            .await?;
        let result = wire.project(page, WireManualEntry::project)?;
        // Tracks may repeat; only the occurrence IDs/positions must be unique.
        unique(result.items.iter().map(|row| row.id.as_str()))?;
        if !result
            .items
            .windows(2)
            .all(|w| w[0].position < w[1].position)
        {
            return Err(INVALID_RESPONSE);
        }
        Ok(result)
    }

    pub async fn derived_entries(
        &self,
        session: &Session,
        id: &str,
        limit: u32,
        cursor: Option<&str>,
    ) -> Result<DerivedPage, ApiError> {
        check_rule_paging(limit, cursor)?;
        let limit_param = limit.to_string();
        let mut query = vec![("limit", limit_param.as_str())];
        if let Some(cursor) = cursor {
            query.push(("cursor", cursor));
        }
        let wire: WireDerivedPage = self
            .catalog_get(session, Route::Entries(id), &query)
            .await?;
        wire.project(limit, cursor, DerivedMode::Saved)
    }

    pub async fn playlist_definition(
        &self,
        session: &Session,
        id: &str,
    ) -> Result<PlaylistDefinition, ApiError> {
        let result: PlaylistDefinition = self
            .catalog_get(session, Route::Definition(id), &[])
            .await?;
        if !revision_ok(result.revision) || !result.definition.is_valid() {
            return Err(INVALID_RESPONSE);
        }
        Ok(result)
    }

    pub async fn tracked_playlist_sources(
        &self,
        session: &Session,
        page: PageRequest,
    ) -> Result<OffsetPage<TrackedPlaylistSource>, ApiError> {
        let (limit, offset) = page_params(page)?;
        let wire: OffsetPage<TrackedPlaylistSource> = self
            .catalog_get(
                session,
                Route::Sources,
                &[("limit", &limit), ("offset", &offset)],
            )
            .await?;
        let result = wire.project(page, |row| {
            let ready = row.state == SourceState::Ready;
            if !valid_id(&row.id)
                || !super::valid_text(&row.name, 1024)
                || ready != row.track_count.is_some()
                || ready != row.saved_at.is_some()
                || row
                    .saved_at
                    .as_deref()
                    .is_some_and(|s| !super::valid_text(s, 64))
            {
                return Err(INVALID_RESPONSE);
            }
            Ok(row)
        })?;
        unique(result.items.iter().map(|row| row.id.as_str()))?;
        Ok(result)
    }

    /// Preview never confers playback selection tokens.
    pub async fn preview_definition(
        &self,
        session: &Session,
        definition: &RuleDefinition,
        target: Option<(&str, u64)>,
        limit: u32,
        cursor: Option<&str>,
    ) -> Result<DerivedPage, ApiError> {
        check_rule_paging(limit, cursor)?;
        if !definition.is_valid()
            || target.is_some_and(|(id, revision)| !derived_id(id) || !revision_ok(revision))
        {
            return Err(INVALID_INPUT);
        }
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Request<'a> {
            definition: &'a RuleDefinition,
            #[serde(skip_serializing_if = "Option::is_none")]
            target_playlist_id: Option<&'a str>,
            #[serde(skip_serializing_if = "Option::is_none")]
            expected_revision: Option<u64>,
            limit: u32,
            #[serde(skip_serializing_if = "Option::is_none")]
            cursor: Option<&'a str>,
        }
        let target_playlist_id = target.map(|(id, _)| id);
        let expected_revision = target.map(|(_, revision)| revision);
        let request = Request {
            definition,
            target_playlist_id,
            expected_revision,
            limit,
            cursor,
        };
        let wire: WireDerivedPage = self
            .catalog_write(
                session,
                Route::Preview,
                Method::POST,
                request,
                StatusCode::OK,
            )
            .await?;
        wire.project(limit, cursor, DerivedMode::Preview(expected_revision))
    }

    pub async fn create_playlist(
        &self,
        session: &Session,
        name: &str,
        description: &str,
        definition: Option<&RuleDefinition>,
    ) -> Result<PlaylistSummary, ApiError> {
        let name = metadata(name, description)?;
        if definition.is_some_and(|definition| !definition.is_valid()) {
            return Err(INVALID_INPUT);
        }
        #[derive(Serialize)]
        struct Request<'a> {
            name: &'a str,
            description: &'a str,
            #[serde(skip_serializing_if = "Option::is_none")]
            kind: Option<&'static str>,
            #[serde(skip_serializing_if = "Option::is_none")]
            definition: Option<&'a RuleDefinition>,
        }
        let kind = if definition.is_some() {
            PlaylistKind::Derived
        } else {
            PlaylistKind::Manual
        };
        let request = Request {
            name,
            description,
            kind: definition.map(|_| kind.wire()),
            definition,
        };
        let wire: PlaylistSummary = self
            .catalog_write(
                session,
                Route::Playlists,
                Method::POST,
                request,
                StatusCode::CREATED,
            )
            .await?;
        wire.checked(None, Some(kind), None)
    }

    pub async fn update_playlist(
        &self,
        session: &Session,
        id: &str,
        revision: u64,
        name: &str,
        description: &str,
    ) -> Result<PlaylistSummary, ApiError> {
        let name = metadata(name, description)?;
        check_revision(revision)?;
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Request<'a> {
            name: &'a str,
            description: &'a str,
            expected_revision: u64,
        }
        let request = Request {
            name,
            description,
            expected_revision: revision,
        };
        self.playlist_write(
            session,
            Route::Playlist(id),
            Method::PATCH,
            request,
            id,
            None,
            revision,
        )
        .await
    }

    pub async fn delete_playlist(
        &self,
        session: &Session,
        id: &str,
        revision: u64,
    ) -> Result<(), ApiError> {
        check_revision(revision)?;
        self.catalog_delete(
            session,
            Route::Playlist(id),
            Revision {
                expected_revision: revision,
            },
        )
        .await
    }

    pub async fn update_playlist_definition(
        &self,
        session: &Session,
        id: &str,
        revision: u64,
        definition: &RuleDefinition,
    ) -> Result<PlaylistSummary, ApiError> {
        check_revision(revision)?;
        if !definition.is_valid() {
            return Err(INVALID_INPUT);
        }
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Request<'a> {
            definition: &'a RuleDefinition,
            expected_revision: u64,
        }
        let request = Request {
            definition,
            expected_revision: revision,
        };
        self.playlist_write(
            session,
            Route::Definition(id),
            Method::PUT,
            request,
            id,
            Some(PlaylistKind::Derived),
            revision,
        )
        .await
    }

    pub async fn add_playlist_track(
        &self,
        session: &Session,
        id: &str,
        revision: u64,
        track_id: &str,
    ) -> Result<PlaylistSummary, ApiError> {
        check_revision(revision)?;
        if !valid_id(track_id) {
            return Err(INVALID_INPUT);
        }
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Request<'a> {
            track_id: &'a str,
            expected_revision: u64,
        }
        let request = Request {
            track_id,
            expected_revision: revision,
        };
        self.playlist_write(
            session,
            Route::Entries(id),
            Method::POST,
            request,
            id,
            Some(PlaylistKind::Manual),
            revision,
        )
        .await
    }

    pub async fn remove_playlist_entry(
        &self,
        session: &Session,
        id: &str,
        revision: u64,
        entry_id: &str,
    ) -> Result<PlaylistSummary, ApiError> {
        check_revision(revision)?;
        let request = Revision {
            expected_revision: revision,
        };
        self.playlist_write(
            session,
            Route::Entry(id, entry_id),
            Method::DELETE,
            request,
            id,
            Some(PlaylistKind::Manual),
            revision,
        )
        .await
    }

    /// Full published membership permutation, not just the current page.
    pub async fn reorder_playlist(
        &self,
        session: &Session,
        id: &str,
        revision: u64,
        entry_ids: &[String],
    ) -> Result<PlaylistSummary, ApiError> {
        check_revision(revision)?;
        let mut seen = HashSet::new();
        if entry_ids.len() > 32_768
            || entry_ids
                .iter()
                .any(|id| !valid_id(id) || !seen.insert(id.as_str()))
        {
            return Err(INVALID_INPUT);
        }
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Request<'a> {
            entry_ids: &'a [String],
            expected_revision: u64,
        }
        let request = Request {
            entry_ids,
            expected_revision: revision,
        };
        self.playlist_write(
            session,
            Route::Order(id),
            Method::PUT,
            request,
            id,
            Some(PlaylistKind::Manual),
            revision,
        )
        .await
    }

    pub async fn copy_derived_playlist(
        &self,
        session: &Session,
        id: &str,
        revision: u64,
        fence: &str,
        name: &str,
        description: &str,
    ) -> Result<PlaylistSummary, ApiError> {
        check_revision(revision)?;
        let name = metadata(name, description)?;
        if !opaque_token(fence, 4096) {
            return Err(INVALID_INPUT);
        }
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Request<'a> {
            name: &'a str,
            description: &'a str,
            expected_revision: u64,
            fence: &'a str,
        }
        let request = Request {
            name,
            description,
            expected_revision: revision,
            fence,
        };
        let wire: PlaylistSummary = self
            .catalog_write(
                session,
                Route::Copy(id),
                Method::POST,
                request,
                StatusCode::CREATED,
            )
            .await?;
        wire.checked(None, Some(PlaylistKind::Manual), None)
    }

    async fn playlist_write(
        &self,
        session: &Session,
        route: Route<'_>,
        method: Method,
        body: impl Serialize,
        id: &str,
        kind: Option<PlaylistKind>,
        revision: u64,
    ) -> Result<PlaylistSummary, ApiError> {
        let wire: PlaylistSummary = self
            .catalog_write(session, route, method, body, StatusCode::OK)
            .await?;
        wire.checked(Some(id), kind, Some(revision))
    }
}

impl<T> OffsetPage<T> {
    fn project<U>(
        self,
        page: PageRequest,
        project: impl FnMut(T) -> Result<U, ApiError>,
    ) -> Result<OffsetPage<U>, ApiError> {
        if !page_matches(page, self.limit, self.offset, self.items.len(), self.total) {
            return Err(INVALID_RESPONSE);
        }
        Ok(OffsetPage {
            items: self
                .items
                .into_iter()
                .map(project)
                .collect::<Result<Vec<_>, _>>()?,
            total: self.total,
            offset: self.offset,
            limit: self.limit,
        })
    }
}

impl PlaylistSummary {
    fn checked(
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

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Revision {
    expected_revision: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireHistory {
    id: String,
    played_at: String,
    track: WireItem,
    origin: Option<String>,
    received_at: Option<String>,
    #[serde(default)]
    metadata_missing: bool,
}

impl WireHistory {
    fn project(self) -> Result<HistoryEntry, ApiError> {
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
struct WireManualEntry {
    id: String,
    position: u64,
    track: WireItem,
}

impl WireManualEntry {
    fn project(self) -> Result<ManualEntry, ApiError> {
        if !valid_id(&self.id) || self.position > i64::MAX as u64 {
            return Err(INVALID_RESPONSE);
        }
        Ok(ManualEntry {
            id: self.id,
            position: self.position,
            track: self.track.project(Some(CatalogKind::Track), false, false)?,
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireDerivedEntry {
    occurrence_id: String,
    selection_token: Option<String>,
    position: u64,
    track: WireItem,
}

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

#[derive(Clone, Copy)]
enum DerivedMode {
    Saved,
    Preview(Option<u64>),
}

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

impl RuleDefinition {
    /// New likes-sourced recipe using a caller-owned stable slot ID (`s_`
    /// followed by 32 lower-case hex digits).
    pub fn from_likes(slot_id: String) -> Self {
        Self {
            version: 1,
            sources: vec![RuleSource::Likes {
                slot_id,
                take: None,
            }],
            include: None,
            exclude: None,
            duplicates: RuleDuplicates::Unique,
            sort: RuleSort::SourceOrder,
            take: None,
        }
    }

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

fn slot_ok(id: &str) -> bool {
    id.strip_prefix("s_").is_some_and(|rest| {
        rest.len() == 32
            && rest
                .bytes()
                .all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f'))
    })
}

fn valid_rule_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 512 && id.trim() == id && !id.chars().any(char::is_control)
}
fn take_ok(take: Option<RuleTake>) -> bool {
    take.is_none_or(|t| (1..=i32::MAX as u32).contains(&t.count))
}
fn revision_ok(revision: u64) -> bool {
    (1..=i64::MAX as u64).contains(&revision)
}
fn check_revision(revision: u64) -> Result<(), ApiError> {
    if revision_ok(revision) {
        Ok(())
    } else {
        Err(INVALID_INPUT)
    }
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

fn page_params(page: PageRequest) -> Result<(String, String), ApiError> {
    if !limit_ok(page.limit) || page.offset > 1_000_000 {
        return Err(INVALID_INPUT);
    }
    Ok((page.limit.to_string(), page.offset.to_string()))
}

fn check_rule_paging(limit: u32, cursor: Option<&str>) -> Result<(), ApiError> {
    if !limit_ok(limit) || cursor.is_some_and(|cursor| !opaque_token(cursor, 4096)) {
        return Err(INVALID_INPUT);
    }
    Ok(())
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
        assert!(row.track.playable_item().is_none());
    }

    #[test]
    fn history_offset_preserves_repeated_plays() {
        let raw = json!({"items":[
            {"id":"event-a","playedAt":"2025-01-01T12:00:01Z","track":track()},
            {"id":"event-b","playedAt":"2025-01-01T12:00:00Z","track":track()}
        ],"total":4,"offset":2,"limit":2});
        let wire: OffsetPage<WireHistory> = serde_json::from_value(raw).unwrap();
        let result = wire
            .project(
                PageRequest {
                    offset: 2,
                    limit: 2,
                },
                WireHistory::project,
            )
            .unwrap();
        assert_eq!(result.items[0].track.id, result.items[1].track.id);
        assert_ne!(result.items[0].id, result.items[1].id);
        assert!(!result.has_more());
    }

    #[test]
    fn manual_order_is_by_occurrence_not_track() {
        let raw = json!({"items":[{"id":"entry-a","position":2,"track":track()},
            {"id":"entry-b","position":4,"track":track()}],"total":2,"offset":0,"limit":2});
        let wire: OffsetPage<WireManualEntry> = serde_json::from_value(raw).unwrap();
        let result = wire
            .project(
                PageRequest {
                    offset: 0,
                    limit: 2,
                },
                WireManualEntry::project,
            )
            .unwrap();
        assert_eq!(result.items[0].track.id, result.items[1].track.id);
        assert_ne!(result.items[0].id, result.items[1].id);
        assert_eq!(result.items[1].position, 4);
        let body = serde_json::to_value(Revision {
            expected_revision: 9,
        })
        .unwrap();
        assert_eq!(body, json!({"expectedRevision":9}));
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
