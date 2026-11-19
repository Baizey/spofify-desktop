//! Navigation snapshots. UI indices are ephemeral: resolve an index against the
//! currently accepted `Page::rows`, then capture the typed `Row::key` and the
//! actor's route/generation/account before awaiting. Do not infer IDs from labels.
//! Mutation handoff: `Row::id` is the entity ID (track ID on occurrence rows),
//! `RowKey::ManualEntry { playlist_id, entry_id }` identifies exact editable rows,
//! and `Row::position` is a stored zero-based slot. `Page::playlist` (detail) and
//! `Row::playlist` (list/picker) provide ID, exact name, revision, counts and
//! capabilities; `PlaylistMetadata::name`, `Page::definition`, and
//! `Page::description` prefill editors. `Page::title` is display-only.
//! `Page::pending_rules()` flags
//! a saved rules playlist awaiting evaluation.
//! `Page::kind` overrides the route kind with `playlist-manual` or `playlist-rules`
//! on detail pages. `Row::playback` and `Page::playback` are server selection
//! inputs, not queue keys or local audio authority. Never display signed inputs.
//! `Row::item` is the positive `api::Item` for offline track saves. `Page::partial`
//! marks an incomplete Home snapshot. `Page::retain_home_shelves` carries over
//! only failed shelves from the previous snapshot of the same account.

use std::{collections::HashSet, fmt, sync::Arc};

use crate::{
    api::{
        ApiError, Item, PAGE_SIZE,
        catalog::{self, CatalogItem, CatalogKind, PageRequest, ReleaseGroup},
        library::{
            self, EvaluationState, PlaylistCapabilities, PlaylistKind, PlaylistSummary,
            RuleDefinition,
        },
    },
    desk::Access,
    player,
};

pub const MAX_ROWS: usize = 500;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Route {
    Home,
    History,
    Search(String),
    Track(String),
    Album(String),
    Artist(String),
    Albums,
    Artists,
    Liked,
    Playlists,
    Playlist(String),
    Picker(String),
    Offline,
    SavedCollection(String),
    Appearance,
    Now,
    Queue,
    Devices,
}

impl Route {
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Home => "home",
            Self::History => "history",
            Self::Search(_) => "search",
            Self::Track(_) => "track",
            Self::Album(_) => "album",
            Self::Artist(_) => "artist",
            Self::Albums => "albums",
            Self::Artists => "artists",
            Self::Liked => "liked",
            Self::Playlists => "playlists",
            Self::Playlist(_) => "playlist",
            Self::Picker(_) => "picker",
            Self::Offline => "offline",
            Self::SavedCollection(_) => "saved-collection",
            Self::Appearance => "appearance",
            Self::Now => "now",
            Self::Queue => "queue",
            Self::Devices => "devices",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub enum RowKey {
    #[default]
    None,
    Track(String),
    Album(String),
    Artist(String),
    Playlist(String),
    /// Origin-qualified event key. Online and offline IDs are separate namespaces.
    HistoryEvent(String),
    ManualEntry {
        playlist_id: String,
        entry_id: String,
    },
    DerivedOccurrence {
        playlist_id: String,
        occurrence_id: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlaylistMetadata {
    pub id: String,
    /// Exact API-validated name for editor prefill, not a display label.
    pub name: String,
    pub kind: PlaylistKind,
    pub revision: u64,
    pub track_count: Option<u64>,
    pub available_count: Option<u64>,
    pub capabilities: PlaylistCapabilities,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Row {
    pub kind: String,
    pub title: String,
    pub subtitle: String,
    pub detail: String,
    pub playable: bool,
    pub id: String,
    pub album_id: String,
    pub artist_id: String,
    pub key: RowKey,
    pub route: Option<Route>,
    pub playlist: Option<PlaylistMetadata>,
    pub position: Option<u64>,
    /// `Some(true)` for confirmed liked rows; `None` means not read.
    pub liked: Option<bool>,
    pub item: Option<Item>,
    pub playback: Option<PlaybackSelection>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Page {
    pub title: String,
    pub subtitle: String,
    pub rows: Vec<Row>,
    pub more: bool,
    pub kind: String,
    pub description: String,
    pub definition: Option<RuleDefinition>,
    pub playlist: Option<PlaylistMetadata>,
    pub playback: Option<PlaybackSelection>,
    /// Local status for partial Home, unavailable rule evaluation, or the cap.
    pub status: String,
    pub partial: bool,
    pub total: Option<u64>,
    pub album_id: String,
    pub artist_id: String,
    pub picker_track_id: Option<String>,
    #[doc(hidden)]
    pub state: PageState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContextKind {
    Album,
    Artist,
    Playlist,
    Likes,
    Derived,
}

#[derive(Clone, PartialEq, Eq)]
pub struct PlaybackContext {
    pub kind: ContextKind,
    pub id: String,
    pub name: String,
    pub revision: Option<u64>,
    fence: Option<String>,
}

impl ContextKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Album => "album",
            Self::Artist => "artist",
            Self::Playlist => "playlist",
            Self::Likes => "likes",
            Self::Derived => "derived",
        }
    }
}

impl PlaybackContext {
    pub fn info_route(&self) -> Route {
        match self.kind {
            ContextKind::Album => Route::Album(self.id.clone()),
            ContextKind::Artist => Route::Artist(self.id.clone()),
            ContextKind::Playlist | ContextKind::Derived => Route::Playlist(self.id.clone()),
            ContextKind::Likes => Route::Liked,
        }
    }

    fn player_context(&self) -> player::Context {
        player::Context {
            kind: self.kind.as_str().into(),
            id: self.id.clone(),
            name: self.name.clone(),
            revision: self.revision,
            fence: self.fence.clone(),
        }
    }
}

impl fmt::Debug for PlaybackContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PlaybackContext")
            .field("kind", &self.kind)
            .field("id", &self.id)
            .field("revision", &self.revision)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Default, PartialEq, Eq)]
pub struct TrackSelection {
    pub id: String,
    pub entry_id: Option<String>,
    pub target_id: Option<String>,
    pub occurrence_id: Option<String>,
    selection_token: Option<String>,
}

impl fmt::Debug for TrackSelection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TrackSelection")
            .field("id", &self.id)
            .field("entry_id", &self.entry_id)
            .field("target_id", &self.target_id)
            .finish_non_exhaustive()
    }
}

/// Convert only when submitting an explicit playback intent to the player.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlaybackSelection {
    Track {
        track: TrackSelection,
        context: Option<Arc<PlaybackContext>>,
    },
    Collection {
        context: Arc<PlaybackContext>,
    },
}

impl PlaybackSelection {
    pub fn context(&self) -> Option<&PlaybackContext> {
        match self {
            Self::Track { context, .. } => context.as_deref(),
            Self::Collection { context } => Some(context),
        }
    }
}

impl From<PlaybackSelection> for player::Selection {
    fn from(selection: PlaybackSelection) -> Self {
        match selection {
            PlaybackSelection::Track { track, context } => Self::Track {
                track: player::TrackRef {
                    id: track.id,
                    entry_id: track.entry_id,
                    target_id: track.target_id,
                    occurrence_id: track.occurrence_id,
                    selection_token: track.selection_token,
                },
                context: context.as_deref().map(PlaybackContext::player_context),
            },
            PlaybackSelection::Collection { context } => Self::collection(context.player_context()),
        }
    }
}

#[derive(Clone, Default, PartialEq, Eq)]
pub struct PageState {
    route: Option<Route>,
    epoch: u64,
    account_id: String,
    next: Option<Next>,
    snapshot: Option<Snapshot>,
    used_cursors: HashSet<String>,
    home: Option<HomeFailures>,
    pending_rules: bool,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct HomeFailures {
    recent: bool,
    playlists: bool,
    likes: bool,
}

impl HomeFailures {
    fn any(self) -> bool {
        self.recent || self.playlists || self.likes
    }

    fn contains(self, row: &Row) -> bool {
        match HomeShelf::of(row) {
            HomeShelf::Recent => self.recent,
            HomeShelf::Playlists => self.playlists,
            HomeShelf::Likes => self.likes,
            HomeShelf::Browse => false,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum HomeShelf {
    Recent,
    Playlists,
    Likes,
    Browse,
}

impl HomeShelf {
    fn of(row: &Row) -> Self {
        match row.key {
            RowKey::HistoryEvent(_) => Self::Recent,
            RowKey::Playlist(_) => Self::Playlists,
            RowKey::Track(_) => Self::Likes,
            _ => Self::Browse,
        }
    }
}

impl fmt::Debug for PageState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PageState").finish_non_exhaustive()
    }
}

#[derive(Clone, PartialEq, Eq)]
enum Next {
    Offset(u32),
    Cursor(String),
}

#[derive(Clone, PartialEq, Eq)]
enum Snapshot {
    Catalog(u64),
    Manual(u64),
    Derived {
        revision: u64,
        fence: String,
        available: u64,
    },
}

const CHANGED: ApiError = ApiError {
    message: "This list has changed.",
    expired: false,
    status: 409,
};
const EXPIRED: ApiError = ApiError {
    message: "Your session has expired. Sign in again.",
    expired: true,
    status: 401,
};
const OFFLINE: ApiError = ApiError {
    message: "Go online to load this collection.",
    expired: false,
    status: 0,
};
const MAX_SAFE_REVISION: u64 = 9_007_199_254_740_991;

/// A continuation lost its snapshot. The actor may retain the old projection
/// while scheduling a fresh first page instead of retrying the same cursor.
pub fn continuation_changed(error: &ApiError) -> bool {
    !error.expired && matches!(error.status, 409 | 410 | 412)
}

/// Fetch one bounded segment. The actor owns the retained account replica, decides
/// when to call this, and must discard results for superseded navigation/access.
/// An append failure returns no replacement, so the accepted page can stay visible.
pub async fn load(access: &Access, route: &Route, old: Option<&Page>) -> Result<Page, ApiError> {
    let (api, session) = (&access.api, &access.session);
    if session.is_expired() {
        return Err(EXPIRED);
    }
    if access.offline
        && !matches!(
            route,
            Route::Offline
                | Route::SavedCollection(_)
                | Route::Appearance
                | Route::Now
                | Route::Queue
                | Route::Devices
        )
    {
        return Err(OFFLINE);
    }
    let mut page = match old {
        Some(old) => {
            if old.state.route.as_ref() != Some(route)
                || old.state.epoch != access.epoch
                || old.state.account_id != session.account_id
            {
                return Err(CHANGED);
            }
            if !old.more || old.rows.len() >= MAX_ROWS || old.state.next.is_none() {
                return Ok(old.clone());
            }
            old.clone()
        }
        None => Page {
            kind: route.kind().into(),
            state: PageState {
                route: Some(route.clone()),
                epoch: access.epoch,
                account_id: session.account_id.clone(),
                ..PageState::default()
            },
            ..Page::default()
        },
    };
    let limit = PAGE_SIZE.min((MAX_ROWS - page.rows.len()) as u32);
    let request = PageRequest {
        offset: match page.state.next {
            Some(Next::Offset(n)) => n,
            _ => 0,
        },
        limit,
    };
    let cursor = match &page.state.next {
        Some(Next::Cursor(value)) => Some(value.clone()),
        _ => None,
    };

    match route {
        Route::Home => project_home(&mut page, api.home(session).await?)?,
        Route::History => {
            page.title = "Listening history".into();
            let result = api.history(session, request).await?;
            let next = offset_next(
                request,
                result.offset,
                result.limit,
                result.items.len(),
                result.total,
            )?;
            let rows = result.items.into_iter().map(history_row).collect();
            page.append(rows, result.total, next, None, None)?;
            page.subtitle = count_label(result.total, "play", "plays");
        }
        Route::Search(query) => {
            page.title = "Search".into();
            if query.trim().is_empty() {
                page.subtitle = "Find tracks, albums, and artists".into();
                return Ok(page);
            }
            let result = if let Some(cursor) = &cursor {
                api.unified_search_next(session, cursor).await?
            } else {
                api.unified_search(session, query, None, request.limit)
                    .await?
            };
            let total = u64::from(result.result_count);
            let end = (page.rows.len() + result.items.len()) as u64;
            if end > total || result.next_cursor.is_some() != (end < total) {
                return Err(CHANGED);
            }
            let rows = result
                .items
                .into_iter()
                .map(|item| catalog_row(item, None))
                .collect();
            page.append(
                rows,
                total,
                result.next_cursor.map(Next::Cursor),
                None,
                cursor,
            )?;
            page.subtitle = count_label(total, "result", "results");
        }
        Route::Track(id) => {
            let detail = api.catalog_detail(session, CatalogKind::Track, id).await?;
            let row = catalog_row(detail.item, None);
            page.title = row.title.clone();
            page.subtitle = join_labels(&row.subtitle, &row.detail);
            page.album_id = row.album_id.clone();
            page.artist_id = row.artist_id.clone();
            page.playback = row.playback.clone();
            page.rows.push(row);
        }
        Route::Album(id) | Route::Artist(id) => {
            let album = matches!(route, Route::Album(_));
            let kind = if album {
                CatalogKind::Album
            } else {
                CatalogKind::Artist
            };
            if old.is_none() {
                let detail = api.catalog_detail(session, kind, id).await?;
                let row = catalog_row(detail.item, None);
                page.title = row.title;
                page.subtitle = join_labels(&row.subtitle, &row.detail);
                page.album_id = row.album_id;
                page.artist_id = row.artist_id;
                page.state.snapshot = Some(Snapshot::Catalog(detail.revision));
                page.playback = row.playback;
            }
            let result = if album {
                api.album_tracks(session, id, request).await?
            } else {
                api.artist_albums(session, id, ReleaseGroup::All, request)
                    .await?
            };
            // Artist album cards play their own albums, never the entire artist.
            let context = if album {
                Some(context_for(ContextKind::Album, id, &page.title))
            } else {
                None
            };
            project_catalog(&mut page, request, result, context)?;
        }
        Route::Albums | Route::Artists => {
            let (kind, title) = if matches!(route, Route::Albums) {
                (CatalogKind::Album, "Albums")
            } else {
                (CatalogKind::Artist, "Artists")
            };
            page.title = title.into();
            let result = api.catalog(session, kind, request).await?;
            page.subtitle = count_label(result.total, kind.singular(), kind.plural());
            project_catalog(&mut page, request, result, None)?;
        }
        Route::Liked => {
            let result = api.likes(session, request).await?;
            page.title = "Liked songs".into();
            page.subtitle = count_label(result.total, "song", "songs");
            let context = context_for(ContextKind::Likes, "liked-songs", &page.title);
            page.playback = (result.available_total > 0).then(|| collection(&context));
            let next = offset_next(
                request,
                result.offset,
                result.limit,
                result.items.len(),
                result.total,
            )?;
            let rows = result
                .items
                .into_iter()
                .map(|entry| liked_row(entry, &context))
                .collect();
            page.append(rows, result.total, next, None, None)?;
        }
        Route::Playlists | Route::Picker(_) => {
            let picker = if let Route::Picker(track_id) = route {
                if !valid_id(track_id) {
                    return Err(CHANGED);
                }
                page.picker_track_id = Some(track_id.clone());
                true
            } else {
                false
            };
            page.title = if picker {
                "Add to playlist"
            } else {
                "Playlists"
            }
            .into();
            if picker {
                page.subtitle = "Choose a playlist you can edit".into();
            }
            let result = api
                .playlists(session, picker.then_some(PlaylistKind::Manual), request)
                .await?;
            let next = offset_next(
                request,
                result.offset,
                result.limit,
                result.items.len(),
                result.total,
            )?;
            let rows = result
                .items
                .into_iter()
                .filter_map(|summary| {
                    if picker
                        && (summary.kind != PlaylistKind::Manual
                            || !summary.capabilities.add_tracks)
                    {
                        return None;
                    }
                    let mut row = playlist_row(summary);
                    if picker {
                        row.playable = false;
                        row.playback = None;
                        row.route = None;
                    }
                    Some(row)
                })
                .collect();
            page.append(rows, result.total, next, None, None)?;
        }
        Route::Playlist(id) => load_playlist(access, &mut page, id, request, cursor).await?,
        // The actor supplies local/offline and live player projections.
        Route::Offline => page.title = "Offline".into(),
        Route::SavedCollection(_) => page.title = "Saved collection".into(),
        Route::Appearance => page.title = "Appearance".into(),
        Route::Now => page.title = "Now playing".into(),
        Route::Queue => page.title = "Queue".into(),
        Route::Devices => page.title = "Devices".into(),
    }
    if session.is_expired() {
        return Err(EXPIRED);
    }
    Ok(page)
}

impl Page {
    pub fn section(&self, index: usize) -> &'static str {
        if self.kind != "home" {
            return "";
        }
        let Some(row) = self.rows.get(index) else {
            return "";
        };
        let shelf = HomeShelf::of(row);
        if index > 0 && HomeShelf::of(&self.rows[index - 1]) == shelf {
            return "";
        }
        match shelf {
            HomeShelf::Recent => "RECENTLY PLAYED",
            HomeShelf::Playlists => "YOUR PLAYLISTS",
            HomeShelf::Likes => "LIKED SONGS",
            HomeShelf::Browse => "",
        }
    }
    /// Only pending or stale evaluations warrant a bounded automatic recheck.
    pub fn pending_rules(&self) -> bool {
        self.state.pending_rules
    }

    /// Carry over failed Home shelves from the same account and epoch; a
    /// successful empty shelf is authoritative and is never restored.
    pub fn retain_home_shelves(&mut self, last: &Page) {
        let Some(failed) = self.state.home else {
            return;
        };
        if !self.partial
            || last.state.home.is_none()
            || self.state.epoch != last.state.epoch
            || self.state.account_id != last.state.account_id
        {
            return;
        }
        self.rows.retain(|row| !failed.contains(row));
        self.rows
            .extend(last.rows.iter().filter(|row| failed.contains(row)).cloned());
        self.rows.sort_by_key(HomeShelf::of);
        self.rows.truncate(MAX_ROWS);
    }

    /// Commit only validated progress. Stable keys deliberately distinguish repeated
    /// songs in History and in playlists, while disallowing overlapping windows.
    fn append(
        &mut self,
        rows: Vec<Row>,
        total: u64,
        next: Option<Next>,
        snapshot: Option<Snapshot>,
        cursor: Option<String>,
    ) -> Result<(), ApiError> {
        if self.total.is_some_and(|previous| previous != total)
            || self
                .state
                .snapshot
                .as_ref()
                .is_some_and(|previous| Some(previous) != snapshot.as_ref())
        {
            return Err(CHANGED);
        }
        if let Some(Next::Cursor(next)) = &next {
            if rows.is_empty()
                || Some(next) == cursor.as_ref()
                || self.state.used_cursors.contains(next)
            {
                return Err(CHANGED);
            }
        }
        let mut seen: HashSet<&RowKey> = self.rows.iter().map(|row| &row.key).collect();
        if rows
            .iter()
            .any(|row| row.key == RowKey::None || !seen.insert(&row.key))
        {
            return Err(CHANGED);
        }
        let remaining = MAX_ROWS.saturating_sub(self.rows.len());
        let capped = rows.len() > remaining || (rows.len() == remaining && next.is_some());
        self.rows.extend(rows.into_iter().take(remaining));
        if let Some(cursor) = cursor {
            self.state.used_cursors.insert(cursor);
        }
        self.total = Some(total);
        self.state.snapshot = snapshot;
        self.state.next = if capped { None } else { next };
        self.more = self.state.next.is_some();
        if capped {
            self.status = "Showing the first 500 items.".into();
        }
        Ok(())
    }
}

fn offset_next(
    request: PageRequest,
    offset: u32,
    limit: u32,
    count: usize,
    total: u64,
) -> Result<Option<Next>, ApiError> {
    if offset != request.offset || limit != request.limit || count > limit as usize {
        return Err(CHANGED);
    }
    let end = u64::from(offset) + count as u64;
    if end >= total && (end == total || count == 0) {
        return Ok(None);
    }
    if count == 0 || end > total {
        return Err(CHANGED);
    }
    Ok(Some(Next::Offset(u32::try_from(end).map_err(|_| CHANGED)?)))
}

fn project_catalog(
    page: &mut Page,
    request: PageRequest,
    result: catalog::CatalogPage,
    context: Option<Arc<PlaybackContext>>,
) -> Result<(), ApiError> {
    let next = offset_next(
        request,
        result.offset,
        result.limit,
        result.items.len(),
        result.total,
    )?;
    let rows = result
        .items
        .into_iter()
        .map(|item| catalog_row(item, context.as_ref()))
        .collect();
    page.append(
        rows,
        result.total,
        next,
        Some(Snapshot::Catalog(result.revision)),
        None,
    )
}

async fn load_playlist(
    access: &Access,
    page: &mut Page,
    id: &str,
    request: PageRequest,
    cursor: Option<String>,
) -> Result<(), ApiError> {
    let (api, session) = (&access.api, &access.session);
    let metadata = if let Some(metadata) = &page.playlist {
        metadata.clone()
    } else {
        let summary = api.playlist(session, id).await?;
        let metadata = apply_playlist_summary(page, &summary);
        if metadata.kind == PlaylistKind::Derived && metadata.capabilities.edit_definition {
            let definition = api.playlist_definition(session, id).await?;
            if definition.revision != metadata.revision {
                return Err(CHANGED);
            }
            page.definition = Some(definition.definition);
        }
        metadata
    };
    match metadata.kind {
        PlaylistKind::Manual => {
            let result = api.manual_entries(session, id, request).await?;
            // Offset pages don't expose a revision. Confirm only this bounded read,
            // never load a full remote collection just to render or start playback.
            let confirmed = api.playlist(session, id).await?;
            if confirmed.revision != metadata.revision
                || confirmed.kind != metadata.kind
                || confirmed
                    .track_count
                    .is_some_and(|count| count != result.total)
            {
                return Err(CHANGED);
            }
            let next = offset_next(
                request,
                result.offset,
                result.limit,
                result.items.len(),
                result.total,
            )?;
            let context = context_for(ContextKind::Playlist, id, &page.title);
            page.playback = (metadata.capabilities.play
                && metadata.available_count.unwrap_or(0) > 0)
                .then(|| collection(&context));
            let rows = result
                .items
                .into_iter()
                .map(|entry| manual_row(entry, &context, metadata.capabilities.play))
                .collect();
            page.append(
                rows,
                result.total,
                next,
                Some(Snapshot::Manual(metadata.revision)),
                None,
            )
        }
        PlaylistKind::Derived => {
            let result = api
                .derived_entries(session, id, request.limit, cursor.as_deref())
                .await?;
            project_derived(page, &metadata, result, cursor)
        }
    }
}

fn apply_playlist_summary(page: &mut Page, summary: &PlaylistSummary) -> PlaylistMetadata {
    page.kind = match summary.kind {
        PlaylistKind::Manual => "playlist-manual",
        PlaylistKind::Derived => "playlist-rules",
    }
    .into();
    page.subtitle = playlist_subtitle(summary);
    page.title = title(&summary.name, "Untitled playlist");
    // Keep the API-validated editor values intact and separate from the labels.
    page.description.clone_from(&summary.description);
    let metadata = playlist_metadata(summary);
    page.playlist = Some(metadata.clone());
    metadata
}

fn project_derived(
    page: &mut Page,
    metadata: &PlaylistMetadata,
    result: library::DerivedPage,
    cursor: Option<String>,
) -> Result<(), ApiError> {
    if result.revision.is_some_and(|r| r != metadata.revision) {
        return Err(CHANGED);
    }
    page.state.pending_rules = matches!(
        result.evaluation_state,
        Some(EvaluationState::Pending | EvaluationState::Stale)
    );
    let ready = result.complete && result.evaluation_state == Some(EvaluationState::Ready);
    let context = if ready {
        let revision = result
            .revision
            .filter(|rev| (1..=MAX_SAFE_REVISION).contains(rev))
            .ok_or(CHANGED)?;
        if !opaque(&result.fence, 4096)
            || result
                .items
                .iter()
                .enumerate()
                .any(|(index, item)| item.position != (page.rows.len() + index) as u64)
            || (result.next_cursor.is_some()
                != (((page.rows.len() + result.items.len()) as u64) < result.total))
        {
            return Err(CHANGED);
        }
        Some(Arc::new(PlaybackContext {
            kind: ContextKind::Derived,
            id: metadata.id.clone(),
            name: page.title.clone(),
            revision: Some(revision),
            fence: Some(result.fence.clone()),
        }))
    } else {
        if cursor.is_some() {
            return Err(CHANGED);
        }
        page.status = match result.evaluation_state {
            Some(EvaluationState::Error) => "Songs aren't available for this playlist right now.",
            _ => "This playlist is still updating.",
        }
        .into();
        None
    };
    let snapshot = context.as_ref().map(|_| Snapshot::Derived {
        revision: metadata.revision,
        fence: result.fence,
        available: result.available_total,
    });
    let next = if ready {
        result.next_cursor.map(Next::Cursor)
    } else {
        None
    };
    let playable_context =
        context.filter(|_| metadata.capabilities.play && result.available_total > 0);
    page.playback = playable_context.as_ref().map(collection);
    let rows = result
        .items
        .into_iter()
        .map(|entry| derived_row(entry, &metadata.id, playable_context.as_ref()))
        .collect();
    page.append(rows, result.total, next, snapshot, cursor)
}

fn project_home(page: &mut Page, home: library::Home) -> Result<(), ApiError> {
    // Home has independent bounded reads, not a dedicated aggregate endpoint.
    let library::Home {
        recent,
        playlists,
        likes,
    } = home;
    let failures = [
        recent.as_ref().err(),
        playlists.as_ref().err(),
        likes.as_ref().err(),
    ];
    if let Some(expired) = failures.into_iter().flatten().find(|error| error.expired) {
        return Err(*expired);
    }
    if let [Some(recent_error), Some(_), Some(_)] = failures {
        return Err(*recent_error);
    }
    page.title = "Home".into();
    page.subtitle = "Recent listening, playlists, and liked songs".into();
    let failures = HomeFailures {
        recent: failures[0].is_some(),
        playlists: failures[1].is_some(),
        likes: failures[2].is_some(),
    };
    page.partial = failures.any();
    page.state.home = Some(failures);
    if page.partial {
        page.status = "Some of your music isn't available right now.".into();
    }
    if let Ok(recent) = recent {
        if recent.items.is_empty() {
            page.subtitle = "No recent plays yet. Explore your music below.".into();
        }
        page.rows.extend(recent.items.into_iter().map(history_row));
    }
    if let Ok(playlists) = playlists {
        page.rows
            .extend(playlists.items.into_iter().map(playlist_row));
    }
    if let Ok(likes) = likes {
        let context = context_for(ContextKind::Likes, "liked-songs", "Liked songs");
        page.rows.extend(
            likes
                .items
                .into_iter()
                .map(|entry| liked_row(entry, &context)),
        );
    }
    page.rows.truncate(MAX_ROWS);
    Ok(())
}

fn context_for(kind: ContextKind, id: &str, name: &str) -> Arc<PlaybackContext> {
    Arc::new(PlaybackContext {
        kind,
        id: id.into(),
        name: ui_text(name),
        revision: None,
        fence: None,
    })
}

fn collection(context: &Arc<PlaybackContext>) -> PlaybackSelection {
    PlaybackSelection::Collection {
        context: Arc::clone(context),
    }
}

fn track_selection(id: &str, context: Option<&Arc<PlaybackContext>>) -> PlaybackSelection {
    PlaybackSelection::Track {
        track: TrackSelection {
            id: id.into(),
            ..TrackSelection::default()
        },
        context: context.cloned(),
    }
}

fn catalog_row(item: CatalogItem, context: Option<&Arc<PlaybackContext>>) -> Row {
    let id = item.id.clone();
    let kind = item.kind;
    let title = title(
        &item.name,
        match kind {
            CatalogKind::Track => "Untitled track",
            CatalogKind::Album => "Untitled album",
            CatalogKind::Artist => "Unknown artist",
        },
    );
    let artist_names = list_label(item.artists.iter().map(|credit| credit.name.as_str()));
    let artist_id = if kind == CatalogKind::Artist {
        id.clone()
    } else {
        item.artists
            .iter()
            .find(|credit| valid_id(&credit.id))
            .map(|credit| credit.id.clone())
            .unwrap_or_default()
    };
    let album_id = if kind == CatalogKind::Album {
        id.clone()
    } else {
        item.album_id.clone()
    };
    let mut row = Row {
        kind: kind.singular().into(),
        title,
        id: id.clone(),
        album_id,
        artist_id,
        ..Row::default()
    };
    match kind {
        CatalogKind::Track => {
            row.key = RowKey::Track(id.clone());
            row.route = Some(Route::Track(id.clone()));
            row.subtitle = artist_names;
            let album = ui_text(&item.album_name);
            let availability = if item.has_audio {
                duration_label(item.duration_ms)
            } else {
                "Unavailable".into()
            };
            row.detail = join_labels(&album, &availability);
            row.playable = item.has_audio;
            row.playback = row.playable.then(|| track_selection(&id, context));
            if row.playable {
                row.item = Some(Item {
                    id,
                    name: row.title.clone(),
                    kind: row.kind.clone(),
                    artists: row.subtitle.clone(),
                    album_name: album,
                    duration_ms: item.duration_ms,
                    has_audio: true,
                    track_number: item.track_number,
                    disc_number: item.disc_number,
                });
            }
        }
        CatalogKind::Album | CatalogKind::Artist => {
            let (key, route, kind) = if matches!(kind, CatalogKind::Album) {
                row.subtitle = artist_names;
                (
                    RowKey::Album(id.clone()),
                    Route::Album(id.clone()),
                    ContextKind::Album,
                )
            } else {
                row.subtitle = list_label(item.genres.iter().map(String::as_str));
                (
                    RowKey::Artist(id.clone()),
                    Route::Artist(id.clone()),
                    ContextKind::Artist,
                )
            };
            row.key = key;
            row.route = Some(route);
            let year = item
                .release_date
                .get(..4)
                .filter(|year| year.bytes().all(|b| b.is_ascii_digit()))
                .unwrap_or_default();
            let tracks = count_label(
                item.coverage.in_library,
                "available song",
                "available songs",
            );
            row.detail = join_labels(year, &tracks);
            row.playable = item.coverage.in_library > 0;
            row.playback = row
                .playable
                .then(|| collection(&context_for(kind, &id, &row.title)));
        }
    }
    row
}

fn history_row(entry: library::RecentEntry) -> Row {
    let mut row = catalog_row(entry.track, None);
    let origin = if entry.origin.as_deref() == Some("offline") {
        "offline"
    } else {
        "online"
    };
    row.key = RowKey::HistoryEvent(format!("{origin}:{}", entry.id));
    if entry.metadata_missing {
        row.title = "Unavailable track".into();
        row.id.clear();
        row.album_id.clear();
        row.artist_id.clear();
        row.subtitle.clear();
        row.detail.clear();
        row.playable = false;
        row.playback = None;
        row.item = None;
        row.route = None;
    }
    if let Some(date) = entry
        .played_at
        .get(..10)
        .filter(|d| d.bytes().all(|b| b.is_ascii_digit() || b == b'-'))
    {
        row.detail = join_labels(date, &row.detail);
    }
    row
}

fn liked_row(entry: library::LikeEntry, context: &Arc<PlaybackContext>) -> Row {
    let mut row = catalog_row(entry.track, Some(context));
    row.liked = Some(true);
    row
}

fn manual_row(
    entry: library::ManualEntry,
    context: &Arc<PlaybackContext>,
    allow_play: bool,
) -> Row {
    let mut row = catalog_row(entry.track, Some(context));
    row.key = RowKey::ManualEntry {
        playlist_id: context.id.clone(),
        entry_id: entry.id.clone(),
    };
    row.position = Some(entry.position);
    if !allow_play {
        row.playable = false;
        row.playback = None;
    }
    if let Some(PlaybackSelection::Track { track, .. }) = &mut row.playback {
        track.entry_id = Some(entry.id);
    }
    row
}

fn derived_row(
    entry: library::DerivedEntry,
    id: &str,
    context: Option<&Arc<PlaybackContext>>,
) -> Row {
    let mut row = catalog_row(entry.track, context);
    row.key = RowKey::DerivedOccurrence {
        playlist_id: id.into(),
        occurrence_id: entry.occurrence_id.clone(),
    };
    row.position = Some(entry.position);
    let token = entry.selection_token.filter(|value| opaque(value, 8192));
    row.playable &= context.is_some() && token.is_some() && opaque(&entry.occurrence_id, 4096);
    if row.playable {
        if let Some(PlaybackSelection::Track { track, .. }) = &mut row.playback {
            track.target_id = Some(id.into());
            track.occurrence_id = Some(entry.occurrence_id);
            track.selection_token = token;
        }
    } else {
        row.playback = None;
    }
    row
}

fn playlist_metadata(summary: &PlaylistSummary) -> PlaylistMetadata {
    PlaylistMetadata {
        id: summary.id.clone(),
        name: summary.name.clone(),
        kind: summary.kind,
        revision: summary.revision,
        track_count: summary.track_count,
        available_count: summary.available_count,
        capabilities: summary.capabilities.clone(),
    }
}

fn playlist_subtitle(summary: &PlaylistSummary) -> String {
    let kind = match summary.kind {
        PlaylistKind::Manual => "Playlist",
        PlaylistKind::Derived => "Rule playlist",
    };
    let counts = summary.track_count.map(|n| count_label(n, "song", "songs"));
    join_labels(
        kind,
        counts
            .as_deref()
            .unwrap_or(if summary.kind == PlaylistKind::Derived {
                "Updating"
            } else {
                ""
            }),
    )
}

fn playlist_row(summary: PlaylistSummary) -> Row {
    let name = title(&summary.name, "Untitled playlist");
    // A derived card doesn't carry a fresh evaluation; allow opening it, not
    // guessing a collection fence or starting an asynchronous Play lookup.
    let playback = (summary.kind == PlaylistKind::Manual
        && summary.capabilities.play
        && summary.available_count.unwrap_or(0) > 0)
        .then(|| collection(&context_for(ContextKind::Playlist, &summary.id, &name)));
    Row {
        kind: "playlist".into(),
        title: name,
        subtitle: playlist_subtitle(&summary),
        detail: ui_text(&summary.description),
        id: summary.id.clone(),
        key: RowKey::Playlist(summary.id.clone()),
        route: Some(Route::Playlist(summary.id.clone())),
        playlist: Some(playlist_metadata(&summary)),
        playable: playback.is_some(),
        playback,
        ..Row::default()
    }
}

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 512
        && value.trim() == value
        && !value.chars().any(char::is_control)
        && !matches!(value, "." | "..")
}

fn opaque(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
}

// Use plain, byte-bounded labels. Strip terminal controls and direction
// overrides/isolate markers without changing the opaque identifiers we send.
fn ui_text(input: &str) -> String {
    const TEXT_BYTES: usize = 256;
    let mut text = String::new();
    for ch in input.trim().chars() {
        if ch.is_control()
            || matches!(ch, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        {
            continue;
        }
        if text.len() + ch.len_utf8() > TEXT_BYTES {
            break;
        }
        text.push(ch);
    }
    text.truncate(text.trim_end().len());
    text
}

fn title(value: &str, fallback: &str) -> String {
    let text = ui_text(value);
    if text.trim().is_empty() {
        fallback.into()
    } else {
        text
    }
}

fn list_label<'a>(items: impl Iterator<Item = &'a str>) -> String {
    let joined = items
        .take(8)
        .map(ui_text)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(", ");
    ui_text(&joined)
}

fn join_labels(left: &str, right: &str) -> String {
    if left.is_empty() {
        return ui_text(right);
    }
    if right.is_empty() {
        return ui_text(left);
    }
    ui_text(&format!("{left} · {right}"))
}

fn count_label(n: u64, singular: &str, plural: &str) -> String {
    format!("{n} {}", if n == 1 { singular } else { plural })
}

fn duration_label(ms: u64) -> String {
    if ms == 0 {
        return String::new();
    }
    let s = ms / 1000;
    let h = s / 3600;
    if h > 0 {
        format!("{h}:{:02}:{:02}", (s % 3600) / 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use catalog::{ArtistCredit, Coverage};
    use serde_json::json;

    fn track() -> CatalogItem {
        CatalogItem {
            kind: CatalogKind::Track,
            id: "track/音".into(),
            name: format!("Music\u{202e}\u{001b}{}", "音".repeat(200)),
            artists: vec![
                ArtistCredit {
                    id: String::new(),
                    name: "Name only".into(),
                },
                ArtistCredit {
                    id: "artist/1".into(),
                    name: "Artist".into(),
                },
            ],
            genres: vec![],
            album_id: "album/1".into(),
            album_name: "Album".into(),
            album_type: String::new(),
            release_date: String::new(),
            relation: String::new(),
            duration_ms: 61_234,
            disc_number: 1,
            track_number: 4,
            has_audio: true,
            coverage: Coverage::default(),
        }
    }

    fn selection(row: &Row) -> serde_json::Value {
        serde_json::to_value(player::Selection::from(row.playback.clone().unwrap())).unwrap()
    }

    fn recent(id: &str, metadata_missing: bool) -> library::RecentEntry {
        library::RecentEntry {
            id: id.into(),
            played_at: "2026-05-01T00:00:00Z".into(),
            track: track(),
            origin: None,
            received_at: None,
            metadata_missing,
        }
    }

    #[test]
    fn labels_are_bounded_but_selection_ids_are_exact() {
        let row = catalog_row(track(), None);
        assert!(row.title.len() <= 256);
        assert!(!row.title.contains(['\u{202e}', '\u{001b}']));
        assert_eq!(row.artist_id, "artist/1");
        assert_eq!(row.album_id, "album/1");
        assert_eq!(row.subtitle, "Name only, Artist");
        assert_eq!(row.detail, "Album · 1:01");
        assert_eq!(row.item.as_ref().unwrap().name, row.title);
        assert_eq!(row.route, Some(Route::Track("track/音".into())));
        assert_eq!(
            selection(&row),
            json!({"kind":"track", "track":{"id":"track/音"}})
        );
    }

    #[test]
    fn rows_in_album_and_liked_views_keep_their_collection_context() {
        let album = context_for(ContextKind::Album, "album/1", "Album");
        let row = catalog_row(track(), Some(&album));
        assert_eq!(
            row.playback
                .as_ref()
                .unwrap()
                .context()
                .unwrap()
                .info_route(),
            Route::Album("album/1".into())
        );
        assert_eq!(selection(&row)["context"]["kind"], "album");
        let likes = context_for(ContextKind::Likes, "liked-songs", "Liked songs");
        let row = liked_row(
            library::LikeEntry {
                track: track(),
                liked_at: "2026-05-01T00:00:00Z".into(),
            },
            &likes,
        );
        assert_eq!(row.liked, Some(true));
        assert_eq!(selection(&row)["context"]["id"], "liked-songs");
    }

    #[test]
    fn manual_duplicates_are_exact_contextual_occurrences() {
        let context = context_for(ContextKind::Playlist, "mix", "Mix");
        let a = manual_row(
            library::ManualEntry {
                id: "entry1".into(),
                position: 0,
                track: track(),
            },
            &context,
            true,
        );
        let b = manual_row(
            library::ManualEntry {
                id: "entry2".into(),
                position: 1,
                track: track(),
            },
            &context,
            true,
        );
        assert_eq!(a.id, b.id);
        assert_ne!(a.key, b.key);
        assert_eq!(b.position, Some(1));
        assert_eq!(
            selection(&b),
            json!({"kind":"track", "track":{"id":"track/音","entryId":"entry2"},
            "context":{"kind":"playlist","id":"mix","name":"Mix"}})
        );
        let mut page = Page::default();
        assert_eq!(page.append(vec![a, b], 2, None, None, None), Ok(()));
    }

    fn rule_metadata() -> PlaylistMetadata {
        PlaylistMetadata {
            id: "mix".into(),
            name: "Mix".into(),
            kind: PlaylistKind::Derived,
            revision: 12,
            track_count: Some(2),
            available_count: Some(2),
            capabilities: PlaylistCapabilities {
                edit_metadata: true,
                edit_entries: false,
                edit_definition: true,
                play: true,
                copy_to_manual: true,
                delete: true,
                add_tracks: false,
            },
        }
    }

    #[test]
    fn playlist_editor_keeps_all_eighty_emoji_in_the_validated_name() {
        let metadata = rule_metadata();
        let name = "🎵".repeat(80);
        let summary = PlaylistSummary {
            id: metadata.id,
            name: name.clone(),
            description: "Line one\nLine two".into(),
            kind: metadata.kind,
            count_state: library::CountState::Exact,
            evaluation_state: Some(EvaluationState::Ready),
            evaluation_error_code: None,
            track_count: metadata.track_count,
            available_count: metadata.available_count,
            capabilities: metadata.capabilities,
            revision: metadata.revision,
            created_at: "2026-05-01T00:00:00Z".into(),
            updated_at: "2026-05-01T00:00:00Z".into(),
        };
        let mut page = Page::default();
        let detail = apply_playlist_summary(&mut page, &summary);
        assert_eq!(detail.name, name);
        assert_eq!(page.playlist.as_ref().unwrap().name.chars().count(), 80);
        assert_eq!(page.title, "🎵".repeat(64));
        assert_eq!(page.description, summary.description);
        let row = playlist_row(summary);
        assert_eq!(row.title, page.title);
        assert_eq!(row.playlist.unwrap().name, name);
    }

    fn rule_page(pos: u64, state: EvaluationState, fence: &str) -> library::DerivedPage {
        library::DerivedPage {
            items: vec![library::DerivedEntry {
                occurrence_id: format!("source/{pos}"),
                selection_token: Some("signed-secret".into()),
                position: pos,
                track: track(),
            }],
            total: 2,
            available_total: 2,
            limit: 1,
            next_cursor: (pos == 0).then(|| "cursor-secret".into()),
            revision: Some(12),
            fence: fence.into(),
            complete: state == EvaluationState::Ready,
            evaluation_state: Some(state),
            evaluation_error_code: None,
        }
    }

    #[test]
    fn only_pending_and_stale_rules_request_an_automatic_recheck() {
        assert!(!Page::default().pending_rules());
        for (state, pending) in [
            (EvaluationState::Pending, true),
            (EvaluationState::Stale, true),
            (EvaluationState::Ready, false),
            (EvaluationState::Error, false),
        ] {
            let mut page = Page::default();
            project_derived(
                &mut page,
                &rule_metadata(),
                rule_page(0, state, "fence"),
                None,
            )
            .unwrap();
            assert_eq!(page.pending_rules(), pending, "{state:?}");
        }
    }

    #[test]
    fn derived_context_is_fenced_playable_only_when_ready_and_redacted() {
        let mut page = Page {
            title: "Mix".into(),
            ..Page::default()
        };
        let metadata = rule_metadata();
        project_derived(
            &mut page,
            &metadata,
            rule_page(0, EvaluationState::Ready, "signed-fence"),
            None,
        )
        .unwrap();
        assert_eq!(
            selection(&page.rows[0]),
            json!({"kind":"track",
            "track":{"id":"track/音","targetId":"mix","occurrenceId":"source/0","selectionToken":"signed-secret"},
            "context":{"kind":"derived","id":"mix","name":"Mix","revision":12,"fence":"signed-fence"}})
        );
        let context = page.playback.as_ref().unwrap().context().unwrap();
        assert_eq!(context.info_route(), Route::Playlist("mix".into()));
        for secret in ["signed-secret", "signed-fence", "cursor-secret"] {
            assert!(!format!("{page:?}").contains(secret));
        }
        assert_eq!(
            project_derived(
                &mut page,
                &metadata,
                rule_page(1, EvaluationState::Ready, "other-fence"),
                Some("cursor-secret".into())
            ),
            Err(CHANGED)
        );
        assert_eq!(page.rows.len(), 1);

        let mut stale = Page::default();
        project_derived(
            &mut stale,
            &metadata,
            rule_page(0, EvaluationState::Stale, "signed-fence"),
            None,
        )
        .unwrap();
        assert!(!stale.more);
        assert!(stale.playback.is_none());
        assert!(!stale.rows[0].playable);
        assert!(stale.rows[0].playback.is_none());
    }

    #[test]
    fn home_reports_partial_reads_but_never_discards_successful_shelves() {
        let mut page = Page::default();
        let row = recent("event", false);
        project_home(
            &mut page,
            library::Home {
                recent: Ok(library::OffsetPage {
                    items: vec![row],
                    total: 1,
                    offset: 0,
                    limit: 8,
                }),
                playlists: Err(CHANGED),
                likes: Err(CHANGED),
            },
        )
        .unwrap();
        assert!(page.partial);
        assert_eq!(
            page.rows[0].key,
            RowKey::HistoryEvent("online:event".into())
        );
        assert!(selection(&page.rows[0]).get("context").is_none());
        let mut offline = recent("event", false);
        offline.origin = Some("offline".into());
        assert_ne!(history_row(offline).key, page.rows[0].key);
        let missing = history_row(recent("old-event", true));
        assert!(!missing.playable && missing.item.is_none() && missing.route.is_none());
        assert_eq!(
            project_home(
                &mut Page::default(),
                library::Home {
                    recent: Err(CHANGED),
                    playlists: Err(EXPIRED),
                    likes: Err(CHANGED),
                }
            ),
            Err(EXPIRED)
        );
    }

    #[test]
    fn partial_home_retains_only_failed_shelves_and_only_for_the_same_scope() {
        fn shelf<T>(items: Vec<T>) -> library::OffsetPage<T> {
            library::OffsetPage {
                total: items.len() as u64,
                items,
                offset: 0,
                limit: 8,
            }
        }
        fn likes(items: Vec<library::LikeEntry>) -> library::LikesPage {
            let count = items.len() as u64;
            library::LikesPage {
                items,
                total: count,
                available_total: count,
                offset: 0,
                limit: 8,
            }
        }
        let mut previous = Page::default();
        project_home(
            &mut previous,
            library::Home {
                recent: Ok(shelf(vec![recent("retained-event", false)])),
                playlists: Ok(shelf(vec![])),
                likes: Ok(likes(vec![library::LikeEntry {
                    track: track(),
                    liked_at: "2026-05-01T00:00:00Z".into(),
                }])),
            },
        )
        .unwrap();
        previous.state.account_id = "first".into();
        let mut next = Page::default();
        project_home(
            &mut next,
            library::Home {
                recent: Err(CHANGED),
                playlists: Ok(shelf(vec![])),
                likes: Ok(likes(vec![])),
            },
        )
        .unwrap();
        assert!(next.partial);
        next.state.account_id = "second".into();
        next.retain_home_shelves(&previous);
        assert!(next.rows.is_empty());
        next.state.account_id = "first".into();
        next.retain_home_shelves(&previous);
        next.retain_home_shelves(&previous);
        assert_eq!(next.rows.len(), 1); // Only a real listening event, no fabricated shelf entries.
        assert_eq!(
            next.rows[0].key,
            RowKey::HistoryEvent("online:retained-event".into())
        );
        assert!(next.rows.iter().all(|r| r.liked.is_none()));
    }

    #[test]
    fn offset_progress_and_append_are_bounded_and_reject_overlap_or_cursor_cycles() {
        assert_eq!(CHANGED.status, 409);
        assert!(continuation_changed(&CHANGED));
        let request = PageRequest {
            offset: 20,
            limit: 20,
        };
        assert!(matches!(
            offset_next(request, 20, 20, 20, 99),
            Ok(Some(Next::Offset(40)))
        ));
        assert!(matches!(offset_next(request, 20, 20, 0, 99), Err(CHANGED)));
        assert!(matches!(offset_next(request, 21, 20, 20, 99), Err(CHANGED)));
        let row = |n: usize| Row {
            key: RowKey::Track(n.to_string()),
            ..Row::default()
        };
        let mut page = Page::default();
        page.append(
            vec![row(0)],
            999,
            Some(Next::Cursor("a".into())),
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            page.append(vec![row(0)], 999, None, None, None),
            Err(CHANGED)
        );
        page.append(
            vec![row(1)],
            999,
            Some(Next::Cursor("b".into())),
            None,
            Some("a".into()),
        )
        .unwrap();
        assert_eq!(
            page.append(
                vec![row(2)],
                999,
                Some(Next::Cursor("a".into())),
                None,
                Some("b".into())
            ),
            Err(CHANGED)
        );
        page.append(
            (2..510).map(row).collect(),
            999,
            Some(Next::Cursor("c".into())),
            None,
            Some("b".into()),
        )
        .unwrap();
        assert_eq!(page.rows.len(), MAX_ROWS);
        assert!(!page.more);
        assert!(page.state.next.is_none());
    }
}
