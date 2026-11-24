//! Navigation snapshots. UI indices are ephemeral: resolve an index against the
//! currently accepted `Page::rows`, then capture the typed `Row::key` and the
//! actor's route/generation/account before awaiting. Do not infer IDs from labels.
//! Mutation handoff: `Row::id` is the entity ID (track ID on occurrence rows),
//! `RowKey::ManualEntry { playlist_id, entry_id }` identifies exact editable rows,
//! and `Row::position` is a stored zero-based slot. `Page::playlist` (detail) and
//! `Row::playlist` (list/picker) provide ID, exact name, revision, counts and
//! capabilities; `PlaylistMetadata::name`, `Page::definition`, and
//! `Page::description` prefill editors. `Page::title` is display-only.
//! `Page::kind` overrides the route kind with `playlist-manual` or `playlist-rules`
//! on detail pages. `Row::playback` and `Page::playback` are server selection
//! inputs, not queue keys or local audio authority. Never display signed inputs.
//! `Row::item` is the positive `api::Item` for offline track saves.

use std::{collections::HashSet, fmt, sync::Arc};

use crate::{
    api::{
        ApiError, Item, PAGE_SIZE,
        catalog::{self, CatalogItem, CatalogKind, PageRequest},
        library::{
            self, EvaluationState, PlaylistCapabilities, PlaylistKind, PlaylistSummary,
            RuleDefinition,
        },
    },
    desk::Access,
    personal, player,
};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Route {
    History,
    Search(String),
    Album(String),
    Artist(String),
    Playlists,
    Playlist(String),
    Picker(String),
    Offline,
    SavedCollection(String),
    Appearance,
    Now,
    Queue,
}

impl Route {
    pub const fn catalog_continuity(&self) -> bool {
        matches!(self, Self::Search(_) | Self::Album(_) | Self::Artist(_))
    }

    pub const fn personal(&self) -> bool {
        matches!(
            self,
            Self::History | Self::Playlists | Self::Playlist(_) | Self::Picker(_)
        )
    }

    pub const fn kind(&self) -> &'static str {
        match self {
            Self::History => "history",
            Self::Search(_) => "search",
            Self::Album(_) => "album",
            Self::Artist(_) => "artist",
            Self::Playlists => "playlists",
            Self::Playlist(_) => "playlist",
            Self::Picker(_) => "picker",
            Self::Offline => "offline",
            Self::SavedCollection(_) => "saved-collection",
            Self::Appearance => "appearance",
            Self::Now => "now",
            Self::Queue => "queue",
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

/// A display label and its explicit, optional destination. This is separate
/// from primary row activation and playback, which can have different targets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Credit {
    pub name: String,
    pub target: Option<Route>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MusicMetadata {
    pub credits: Vec<Credit>,
    pub album: Option<Credit>,
}

impl MusicMetadata {
    fn from_catalog(item: &CatalogItem) -> Self {
        let credits = item
            .artists
            .iter()
            .filter_map(|credit| {
                let target = music_route("artist", &credit.id);
                let name = ui_text(&credit.name);
                if target.is_none() && name.is_empty() {
                    return None;
                }
                Some(Credit {
                    name: if name.is_empty() {
                        "Unknown artist".into()
                    } else {
                        name
                    },
                    target,
                })
            })
            .collect();
        // A release card already opens its own album; its metadata only links
        // the credited artists, not its own title.
        let album = (item.kind == CatalogKind::Track)
            .then(|| {
                let name = ui_text(&item.album_name);
                let target = music_route("album", &item.album_id);
                (target.is_some() || !name.is_empty()).then(|| Credit {
                    name: if name.is_empty() {
                        "Album details".into()
                    } else {
                        name
                    },
                    target,
                })
            })
            .flatten();
        Self { credits, album }
    }
}

/// Album and artist detail routes need one addressable segment. Never infer an
/// ID from a label, nor reinterpret an opaque ID as a path or query string.
pub(crate) fn music_route(kind: &str, id: &str) -> Option<Route> {
    if !catalog::segment_id(id) {
        return None;
    }
    match kind {
        "artist" => Some(Route::Artist(id.into())),
        "album" => Some(Route::Album(id.into())),
        _ => None,
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Row {
    pub kind: String,
    pub title: String,
    pub subtitle: String,
    pub detail: String,
    pub playable: bool,
    pub id: String,
    pub music: MusicMetadata,
    pub key: RowKey,
    pub route: Option<Route>,
    pub playlist: Option<PlaylistMetadata>,
    pub position: Option<u64>,
    pub item: Option<Item>,
    /// Exact v1 recording identity from the same scoped inventory record as `item`.
    pub(crate) local_recording: Option<crate::offline::Recording>,
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
    /// This page's playable occurrences come from the local download store, not the account.
    pub local_audio: bool,
    /// A local album/artist context resolves to at least one actual track.
    pub(crate) local_collection_playable: bool,
    /// Revision of the actual retained membership, separate from the latest summary.
    /// Legacy saved manifests do not record this value.
    pub(crate) playlist_membership_revision: Option<u64>,
    /// Local status for unavailable evaluation or incomplete views.
    pub status: String,
    pub(crate) playlist_evaluation_failed: bool,
    pub total: Option<u64>,
    pub music: MusicMetadata,
    pub picker_track_id: Option<String>,
    /// The durable source read with a Saved playlist detail, not its mutable status.
    pub(crate) saved_pin: Option<crate::playlist_pin_store::Target>,
    #[doc(hidden)]
    pub state: PageState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContextKind {
    Album,
    Artist,
    Playlist,
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
            Self::Derived => "derived",
        }
    }
}

impl PlaybackContext {
    #[cfg(test)]
    pub fn info_route(&self) -> Route {
        match self.kind {
            ContextKind::Album => Route::Album(self.id.clone()),
            ContextKind::Artist => Route::Artist(self.id.clone()),
            ContextKind::Playlist | ContextKind::Derived => Route::Playlist(self.id.clone()),
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
    #[cfg(test)]
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
    keys: HashSet<RowKey>,
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

/// Fetch the initial projection. Catalog continuation fetches a separate bounded
/// delta; only the browsing actor can commit it to an accepted page.
pub async fn load(access: &Access, route: &Route) -> Result<Page, ApiError> {
    let (api, session) = (&access.api, &access.session);
    if session.is_expired() {
        return Err(EXPIRED);
    }
    if route.personal() {
        // A read-only projection; navigation owns opening one playlist scope
        // and the engine owns all wire I/O. No personal HTTP request can follow.
        return Ok(access.personal.read(|status, replica| {
            (status.epoch == access.epoch)
                .then(|| personal_projection(route, replica))
                .flatten()
                .unwrap_or_else(|| Page {
                    kind: route.kind().into(),
                    status: "The account library has not loaded this page yet.".into(),
                    ..Page::default()
                })
        }));
    }
    if access.offline
        && !matches!(
            route,
            Route::Offline
                | Route::SavedCollection(_)
                | Route::Appearance
                | Route::Now
                | Route::Queue
        )
    {
        return Err(OFFLINE);
    }
    let mut page = Page {
        kind: route.kind().into(),
        state: PageState {
            route: Some(route.clone()),
            epoch: access.epoch,
            account_id: session.account_id.clone(),
            ..PageState::default()
        },
        ..Page::default()
    };

    match route {
        Route::Search(query) => {
            page.title = "Search".into();
            if query.trim().is_empty() {
                page.subtitle = "Find tracks, albums, and artists".into();
                return Ok(page);
            }
            let batch = fetch_catalog(access, route, None, 0, &page.title).await?;
            page.subtitle = count_label(batch.total, "result", "results");
            batch.commit(&mut page)?;
        }
        Route::Album(id) | Route::Artist(id) => {
            let album = matches!(route, Route::Album(_));
            let kind = if album {
                CatalogKind::Album
            } else {
                CatalogKind::Artist
            };
            let detail = api.catalog_detail(session, kind, id).await?;
            let row = catalog_row(detail.item, None);
            page.title = row.title;
            page.subtitle = if album {
                row.detail
            } else {
                join_labels(&row.subtitle, &row.detail)
            };
            page.music = row.music;
            page.playback = row.playback;
            // Detail and listing are separate transactions. Only the child
            // listing's revision fences subsequent offsets.
            fetch_catalog(access, route, None, 0, &page.title)
                .await?
                .commit(&mut page)?;
        }
        Route::History | Route::Playlists | Route::Playlist(_) | Route::Picker(_) => {
            // Only the account socket can supply these projections.
            return Err(CHANGED);
        }
        // The actor supplies local/offline and live player projections.
        Route::Offline => page.title = "Offline".into(),
        Route::SavedCollection(_) => page.title = "Saved collection".into(),
        Route::Appearance => page.title = "Appearance".into(),
        Route::Now => page.title = "Now playing".into(),
        Route::Queue => page.title = "Queue".into(),
    }
    if session.is_expired() {
        return Err(EXPIRED);
    }
    Ok(page)
}

pub struct CatalogBatch {
    rows: Vec<Row>,
    total: u64,
    expected: Option<Next>,
    next: Option<Next>,
    snapshot: Option<Snapshot>,
}

impl CatalogBatch {
    pub fn commit(self, page: &mut Page) -> Result<(), ApiError> {
        if page.state.next != self.expected || (self.expected.is_some() && self.rows.is_empty()) {
            return Err(CHANGED);
        }
        let cursor = match self.expected {
            Some(Next::Cursor(cursor)) => Some(cursor),
            _ => None,
        };
        page.append(self.rows, self.total, self.next, self.snapshot, cursor)
    }
}

pub async fn load_next(
    access: &Access,
    route: &Route,
    page: &Page,
) -> Result<CatalogBatch, ApiError> {
    if access.session.is_expired() {
        return Err(EXPIRED);
    }
    if access.offline || access.session.is_offline() {
        return Err(OFFLINE);
    }
    if page.state.route.as_ref() != Some(route)
        || page.state.epoch != access.epoch
        || page.state.account_id != access.session.account_id
        || page.state.next.is_none()
    {
        return Err(CHANGED);
    }
    fetch_catalog(
        access,
        route,
        page.state.next.as_ref(),
        page.rows.len(),
        &page.title,
    )
    .await
}

async fn fetch_catalog(
    access: &Access,
    route: &Route,
    expected: Option<&Next>,
    before: usize,
    title: &str,
) -> Result<CatalogBatch, ApiError> {
    let (api, session) = (&access.api, &access.session);
    let request = PageRequest {
        offset: match expected {
            Some(Next::Offset(n)) => *n,
            _ => 0,
        },
        limit: PAGE_SIZE,
    };
    let (items, total, next, snapshot, context) = match route {
        Route::Search(query) => {
            let result = match expected {
                Some(Next::Cursor(cursor)) => api.unified_search_next(session, cursor).await?,
                None => {
                    api.unified_search(session, query, None, request.limit)
                        .await?
                }
                _ => return Err(CHANGED),
            };
            let total = u64::from(result.result_count);
            let count = result.items.len();
            let end = before as u64 + count as u64;
            if count > request.limit as usize
                || end > total
                || result.next_cursor.is_some() != (end < total)
            {
                return Err(CHANGED);
            }
            (
                result.items,
                total,
                result.next_cursor.map(Next::Cursor),
                None,
                None,
            )
        }
        Route::Album(id) | Route::Artist(id) => {
            let album = matches!(route, Route::Album(_));
            let result = if album {
                api.album_tracks(session, id, request).await?
            } else {
                api.artist_albums(session, id, request).await?
            };
            let next = offset_next(
                request,
                result.offset,
                result.limit,
                result.items.len(),
                result.total,
            )?;
            // Artist album cards play their own albums, not the entire artist.
            let context = album.then(|| context_for(ContextKind::Album, id, title));
            (
                result.items,
                result.total,
                next,
                Some(Snapshot::Catalog(result.revision)),
                context,
            )
        }
        _ => return Err(CHANGED),
    };
    if session.is_expired() {
        return Err(EXPIRED);
    }
    Ok(CatalogBatch {
        rows: items
            .into_iter()
            .map(|item| catalog_row(item, context.as_ref()))
            .collect(),
        total,
        expected: expected.cloned(),
        next,
        snapshot,
    })
}

impl Page {
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
        let mut seen = HashSet::with_capacity(rows.len());
        if rows.iter().any(|row| {
            row.key == RowKey::None || self.state.keys.contains(&row.key) || !seen.insert(&row.key)
        }) {
            return Err(CHANGED);
        }
        self.state.keys.extend(seen.into_iter().cloned());
        self.rows.extend(rows);
        if let Some(cursor) = cursor {
            self.state.used_cursors.insert(cursor);
        }
        self.total = Some(total);
        self.state.snapshot = snapshot;
        self.state.next = next;
        self.more = self.state.next.is_some();
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

pub fn personal_projection(route: &Route, replica: &personal::Replica) -> Option<Page> {
    match route {
        Route::History => replica.snapshot().map(|s| recent(&s.recent)),
        Route::Playlists | Route::Picker(_) => replica
            .snapshot()
            .map(|s| personal_list(route, &s.playlists)),
        Route::Playlist(id) => replica
            .playlist()
            .filter(|p| &p.summary.id == id)
            .map(|p| personal_playlist(&p.summary, p.definition.as_ref(), &p.rows)),
        _ => None,
    }
}

pub(crate) fn recent(entries: &[library::HistoryEntry]) -> Page {
    Page {
        kind: "history".into(),
        title: "History".into(),
        subtitle: "Recent plays".into(),
        total: Some(entries.len() as u64),
        rows: entries.iter().cloned().map(history_row).collect(),
        ..Page::default()
    }
}

pub fn personal_list(route: &Route, summaries: &[PlaylistSummary]) -> Page {
    let picker = matches!(route, Route::Picker(_));
    let mut page = Page {
        kind: route.kind().into(),
        title: if picker {
            "Add to playlist"
        } else {
            "Playlists"
        }
        .into(),
        ..Page::default()
    };
    if let Route::Picker(track_id) = route {
        if !valid_id(track_id) {
            page.status = "This track cannot be added to a playlist.".into();
            return page;
        }
        page.picker_track_id = Some(track_id.clone());
        page.subtitle = "Choose a playlist you can edit".into();
    }
    page.rows = summaries
        .iter()
        .filter(|p| !picker || (p.kind == PlaylistKind::Manual && p.capabilities.add_tracks))
        .cloned()
        .map(|summary| {
            let subtitle = if picker {
                playlist_subtitle(&summary)
            } else {
                playlist_count(summary.track_count)
            };
            let mut row = playlist_row(summary);
            row.subtitle = subtitle;
            if picker {
                row.playable = false;
                row.playback = None;
                row.route = None;
            }
            row
        })
        .collect();
    // The wire carries no total, so 100 summaries is not proof there are more.
    if summaries.len() == 100 {
        page.status = "The account snapshot shows up to 100 playlists.".into();
    }
    page
}

pub(crate) fn unsaved_playlist(summary: &PlaylistSummary) -> Page {
    let mut page = Page::default();
    apply_playlist_summary(&mut page, summary);
    page.status = "Playlist details not saved on this device.".into();
    page
}

pub fn personal_playlist(
    summary: &PlaylistSummary,
    definition: Option<&RuleDefinition>,
    entries: &[library::ManualEntry],
) -> Page {
    let mut page = Page::default();
    let metadata = apply_playlist_summary(&mut page, summary);
    page.definition = definition.cloned();
    page.total = summary.track_count.or(Some(entries.len() as u64));
    let derived = metadata.kind == PlaylistKind::Derived;
    let ready = playlist_ready(summary);
    let allow_play = ready && summary.capabilities.play;
    let context = playlist_context(summary, &page.title);
    page.playback = playlist_selection(summary, &context);
    page.rows = entries
        .iter()
        .cloned()
        .map(|entry| {
            if derived {
                derived_row(entry, &context, allow_play)
            } else {
                manual_row(entry, &context, allow_play)
            }
        })
        .collect();
    if !ready {
        page.playlist_evaluation_failed = summary.evaluation_state == Some(EvaluationState::Error);
        page.status = match summary.evaluation_state {
            Some(EvaluationState::Error) => "Songs aren't available for this playlist right now.",
            _ => "This playlist is still updating.",
        }
        .into();
    } else if summary.available_count.is_some_and(|count| count > 0) && !allow_play {
        page.status = "Playback isn’t available for this playlist.".into();
    } else if summary.available_count == Some(0) && summary.track_count.is_some_and(|n| n > 0) {
        page.status = "No songs have available audio.".into();
    }
    page
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

pub(crate) fn track_selection(id: &str, context: Option<&Arc<PlaybackContext>>) -> PlaybackSelection {
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
    let music = MusicMetadata::from_catalog(&item);
    let artist_names = list_label(music.credits.iter().map(|credit| credit.name.as_str()));
    let mut row = Row {
        kind: kind.singular().into(),
        title,
        id: id.clone(),
        music,
        route: music_route(kind.singular(), &id),
        ..Row::default()
    };
    match kind {
        CatalogKind::Track => {
            row.key = RowKey::Track(id.clone());
            row.subtitle = artist_names;
            row.detail = if item.has_audio {
                duration_label(item.duration_ms)
            } else {
                "Unavailable".into()
            };
            row.playable = item.has_audio;
            row.playback = row.playable.then(|| track_selection(&id, context));
            if row.playable {
                row.item = Some(Item {
                    id,
                    name: row.title.clone(),
                    kind: row.kind.clone(),
                    artists: row.subtitle.clone(),
                    album_name: ui_text(&item.album_name),
                    duration_ms: item.duration_ms,
                    has_audio: true,
                    track_number: item.track_number,
                    disc_number: item.disc_number,
                });
            }
        }
        CatalogKind::Album | CatalogKind::Artist => {
            let (key, kind) = if matches!(kind, CatalogKind::Album) {
                row.subtitle = artist_names;
                (RowKey::Album(id.clone()), ContextKind::Album)
            } else {
                row.subtitle = list_label(item.genres.iter().map(String::as_str));
                (RowKey::Artist(id.clone()), ContextKind::Artist)
            };
            row.key = key;
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

fn history_row(entry: library::HistoryEntry) -> Row {
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
        row.music = MusicMetadata::default();
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
    entry: library::ManualEntry,
    context: &Arc<PlaybackContext>,
    allow_play: bool,
) -> Row {
    let mut row = catalog_row(entry.track, Some(context));
    row.key = RowKey::DerivedOccurrence {
        playlist_id: context.id.clone(),
        occurrence_id: entry.id.clone(),
    };
    row.position = Some(entry.position);
    row.playable &= allow_play && opaque(&entry.id, 4096);
    if row.playable {
        if let Some(PlaybackSelection::Track { track, .. }) = &mut row.playback {
            track.target_id = Some(context.id.clone());
            track.occurrence_id = Some(entry.id);
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

/// Detail has its own compact copy; keep the Playlists row subtitles unchanged.
pub(crate) fn playlist_detail_copy(page: &Page) -> (String, String) {
    if page.kind == "saved-collection" {
        let metadata = if page.subtitle.is_empty() {
            page.saved_pin
                .as_ref()
                .map_or_else(String::new, |target| match target.kind {
                    PlaylistKind::Manual => String::new(),
                    PlaylistKind::Derived => "Rule playlist".into(),
                })
        } else {
            page.subtitle.clone()
        };
        return (metadata, String::new());
    }
    let kind = if page.kind == "playlist-rules" {
        "Rule playlist"
    } else {
        ""
    };
    let count = if page.local_audio {
        Some(page.rows.len() as u64)
    } else {
        page.playlist.as_ref().and_then(|m| m.track_count)
    }
    .map(|n| count_label(n, "song", "songs"));
    let metadata = join_labels(kind, count.as_deref().unwrap_or(""));
    let description = ui_text(
        &page
            .description
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" "),
    );
    (metadata, description)
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

fn playlist_ready(summary: &PlaylistSummary) -> bool {
    summary.kind == PlaylistKind::Manual || summary.evaluation_state == Some(EvaluationState::Ready)
}

fn playlist_context(summary: &PlaylistSummary, name: &str) -> Arc<PlaybackContext> {
    let kind = match summary.kind {
        PlaylistKind::Manual => ContextKind::Playlist,
        PlaylistKind::Derived => ContextKind::Derived,
    };
    context_for(kind, &summary.id, name)
}

fn playlist_selection(
    summary: &PlaylistSummary,
    context: &Arc<PlaybackContext>,
) -> Option<PlaybackSelection> {
    // The current account protocol resolves the complete canonical playlist by
    // kind and ID. Display rows/count limits are not selection credentials.
    (playlist_ready(summary)
        && summary.capabilities.play
        && summary.available_count.is_some_and(|count| count > 0))
    .then(|| collection(context))
}

fn playlist_row(summary: PlaylistSummary) -> Row {
    let name = title(&summary.name, "Untitled playlist");
    let playback = playlist_selection(&summary, &playlist_context(&summary, &name));
    Row {
        kind: "playlist".into(),
        title: name,
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
pub(crate) fn ui_text(input: &str) -> String {
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

pub(crate) fn join_labels(left: &str, right: &str) -> String {
    if left.is_empty() {
        return ui_text(right);
    }
    if right.is_empty() {
        return ui_text(left);
    }
    ui_text(&format!("{left} · {right}"))
}

pub(crate) fn playlist_count(count: Option<u64>) -> String {
    count.map_or_else(String::new, |n| count_label(n, "song", "songs"))
}

pub(crate) fn count_label(n: u64, singular: &str, plural: &str) -> String {
    format!("{n} {}", if n == 1 { singular } else { plural })
}

pub(crate) fn duration_label(ms: u64) -> String {
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
            id: "track%2F音".into(),
            name: format!("Music\u{202e}\u{001b}{}", "音".repeat(200)),
            artists: vec![
                ArtistCredit {
                    id: String::new(),
                    name: "Name only".into(),
                },
                ArtistCredit {
                    id: "artist%2F1".into(),
                    name: "Artist".into(),
                },
                ArtistCredit {
                    id: "artist-2".into(),
                    name: "Guest".into(),
                },
            ],
            genres: vec![],
            album_id: "album%2F1".into(),
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

    fn history_entry(id: &str, metadata_missing: bool) -> library::HistoryEntry {
        library::HistoryEntry {
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
        assert_eq!(row.music.credits.len(), 3);
        assert_eq!(row.music.credits[0].target, None);
        assert_eq!(
            row.music.credits[1].target,
            Some(Route::Artist("artist%2F1".into()))
        );
        assert_eq!(
            row.music.credits[2].target,
            Some(Route::Artist("artist-2".into()))
        );
        assert_eq!(
            row.music.album.as_ref().unwrap().target,
            Some(Route::Album("album%2F1".into()))
        );
        assert_eq!(row.subtitle, "Name only, Artist, Guest");
        assert_eq!(row.detail, "1:01");
        assert_eq!(row.item.as_ref().unwrap().name, row.title);
        assert!(row.route.is_none());
        assert_eq!(
            selection(&row),
            json!({"kind":"track", "track":{"id":"track%2F音"}})
        );
    }

    #[test]
    fn unavailable_track_keeps_its_album_and_artist_metadata() {
        let mut item = track();
        item.has_audio = false;
        let row = catalog_row(item, None);
        assert!(!row.playable);
        assert!(row.playback.is_none());
        assert_eq!(row.detail, "Unavailable");
        assert_eq!(row.subtitle, "Name only, Artist, Guest");
        assert_eq!(
            row.music.album.unwrap().target,
            Some(Route::Album("album%2F1".into()))
        );
    }

    #[test]
    fn detail_links_do_not_guess_from_names_or_unsupported_path_ids() {
        let mut item = track();
        item.id = "track/音".into();
        item.album_id.clear();
        item.artists[1].id = "artist/1".into(); // Go's oneSegment rejects this ID.
        let row = catalog_row(item, None);
        assert!(row.route.is_none());
        assert_eq!(row.music.credits[1].name, "Artist");
        assert!(row.music.credits[1].target.is_none());
        assert_eq!(row.music.album.as_ref().unwrap().name, "Album");
        assert!(row.music.album.as_ref().unwrap().target.is_none());
        assert_eq!(selection(&row)["track"]["id"], "track/音");
    }

    #[test]
    fn rows_in_album_keep_their_collection_context() {
        let album = context_for(ContextKind::Album, "album%2F1", "Album");
        let row = catalog_row(track(), Some(&album));
        assert_eq!(
            row.playback
                .as_ref()
                .unwrap()
                .context()
                .unwrap()
                .info_route(),
            Route::Album("album%2F1".into())
        );
        assert_eq!(selection(&row)["context"]["kind"], "album");
        assert_eq!(row.detail, "1:01");
        assert_eq!(row.item.as_ref().unwrap().album_name, "Album");
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
        assert!(a.route.is_none() && b.route.is_none());
        assert_eq!(
            selection(&b),
            json!({"kind":"track", "track":{"id":"track%2F音","entryId":"entry2"},
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
    fn playlist_list_shows_only_known_song_counts_without_changing_the_picker() {
        let metadata = rule_metadata();
        let mut summary = PlaylistSummary {
            id: metadata.id,
            name: metadata.name,
            description: String::new(),
            kind: PlaylistKind::Derived,
            count_state: library::CountState::Exact,
            evaluation_state: Some(EvaluationState::Ready),
            evaluation_error_code: None,
            track_count: Some(0),
            available_count: Some(0),
            capabilities: metadata.capabilities,
            revision: metadata.revision,
            created_at: String::new(),
            updated_at: String::new(),
        };
        for (count, label) in [
            (Some(0), "0 songs"),
            (Some(1), "1 song"),
            (Some(3), "3 songs"),
            (None, ""),
        ] {
            summary.track_count = count;
            summary.available_count = count.map(|_| 0);
            summary.count_state = if count.is_none() {
                library::CountState::Pending
            } else {
                library::CountState::Exact
            };
            summary.evaluation_state = Some(if count.is_none() {
                EvaluationState::Pending
            } else {
                EvaluationState::Ready
            });
            let page = personal_list(&Route::Playlists, std::slice::from_ref(&summary));
            assert_eq!(page.rows[0].subtitle, label);
        }
        summary.kind = PlaylistKind::Manual;
        summary.evaluation_state = None;
        summary.count_state = library::CountState::Exact;
        summary.track_count = Some(3);
        summary.available_count = Some(3);
        summary.capabilities.add_tracks = true;
        let list = personal_list(&Route::Playlists, std::slice::from_ref(&summary));
        assert_eq!(list.rows[0].subtitle, "3 songs");
        let picker = personal_list(&Route::Picker("track".into()), &[summary]);
        assert_eq!(picker.rows[0].subtitle, "Playlist · 3 songs");
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

    #[test]
    fn playlist_collection_play_is_consistent_across_sizes_and_capabilities() {
        let metadata = rule_metadata();
        let mut summary = PlaylistSummary {
            id: metadata.id,
            name: "Mix".into(),
            description: String::new(),
            kind: metadata.kind,
            count_state: library::CountState::Exact,
            evaluation_state: Some(EvaluationState::Ready),
            evaluation_error_code: None,
            track_count: Some(1161),
            available_count: Some(1161),
            capabilities: metadata.capabilities,
            revision: metadata.revision,
            created_at: String::new(),
            updated_at: String::new(),
        };
        let entries = (0..1161)
            .map(|i| library::ManualEntry {
                id: format!("occurrence-{i}"),
                position: i,
                track: track(),
            })
            .collect::<Vec<_>>();
        for (kind, wire_kind) in [
            (PlaylistKind::Manual, "playlist"),
            (PlaylistKind::Derived, "derived"),
        ] {
            summary.kind = kind;
            summary.evaluation_state =
                (kind == PlaylistKind::Derived).then_some(EvaluationState::Ready);
            let list = personal_list(&Route::Playlists, &[summary.clone()]);
            let detail = personal_playlist(&summary, None, &entries);
            assert_eq!(detail.total, Some(1161));
            assert_eq!(detail.rows.len(), 1161);
            assert!(list.rows[0].playable);
            assert_eq!(list.rows[0].playback, detail.playback);
            assert_eq!(
                selection(&list.rows[0]),
                json!({"kind":"collection", "context":{
                    "kind": wire_kind, "id":"mix", "name":"Mix"
                }})
            );
            for (total, available, permitted) in [
                (0, Some(0), true),
                (1161, Some(0), true),
                (1161, None, true),
                (1161, Some(1161), false),
            ] {
                let mut blocked = summary.clone();
                blocked.track_count = Some(total);
                blocked.available_count = available;
                blocked.capabilities.play = permitted;
                let row = playlist_row(blocked.clone());
                assert!(!row.playable && row.playback.is_none());
                assert!(personal_playlist(&blocked, None, &[]).playback.is_none());
            }
        }
        for state in [
            None,
            Some(EvaluationState::Stale),
            Some(EvaluationState::Error),
        ] {
            summary.evaluation_state = state;
            assert!(!playlist_row(summary.clone()).playable);
            assert!(
                personal_playlist(&summary, None, &entries)
                    .playback
                    .is_none()
            );
        }
    }

    #[test]
    fn derived_selection_uses_server_occurrence_without_legacy_credentials() {
        let metadata = rule_metadata();
        let mut summary = PlaylistSummary {
            id: metadata.id,
            name: "Mix".into(),
            description: String::new(),
            kind: metadata.kind,
            count_state: library::CountState::Exact,
            evaluation_state: Some(EvaluationState::Ready),
            evaluation_error_code: None,
            track_count: Some(1),
            available_count: Some(1),
            capabilities: metadata.capabilities,
            revision: metadata.revision,
            created_at: String::new(),
            updated_at: String::new(),
        };
        let entries = [library::ManualEntry {
            id: "o1_source".into(),
            position: 0,
            track: track(),
        }];
        let page = personal_playlist(&summary, None, &entries);
        assert!(page.rows[0].route.is_none());
        assert_eq!(
            selection(&page.rows[0]),
            json!({"kind":"track",
            "track":{"id":"track%2F音","targetId":"mix","occurrenceId":"o1_source"},
            "context":{"kind":"derived","id":"mix","name":"Mix"}})
        );
        let context = page.playback.as_ref().unwrap().context().unwrap();
        assert_eq!(context.info_route(), Route::Playlist("mix".into()));
        summary.evaluation_state = Some(EvaluationState::Stale);
        let stale = personal_playlist(&summary, None, &entries);
        assert!(stale.playback.is_none());
        assert!(!stale.rows[0].playable);
        assert!(stale.rows[0].playback.is_none());
    }

    #[test]
    fn large_playlist_detail_keeps_play_and_edit_identity_for_all_occurrences() {
        const SONGS: u64 = 1163;
        let metadata = rule_metadata();
        let mut summary = PlaylistSummary {
            id: metadata.id,
            name: "Mix".into(),
            description: String::new(),
            kind: PlaylistKind::Manual,
            count_state: library::CountState::Exact,
            evaluation_state: None,
            evaluation_error_code: None,
            track_count: Some(SONGS),
            available_count: Some(SONGS),
            capabilities: metadata.capabilities,
            revision: metadata.revision,
            created_at: String::new(),
            updated_at: String::new(),
        };
        summary.capabilities.edit_entries = true;
        let route = Route::Playlist("mix".into());
        let entries = (0..SONGS)
            .map(|i| library::ManualEntry {
                id: format!("occurrence-{i}"),
                // Hidden rows can leave gaps in Go's stored slots.
                position: 2 * i,
                track: track(),
            })
            .collect::<Vec<_>>();
        for (kind, wire_kind) in [
            (PlaylistKind::Manual, "playlist"),
            (PlaylistKind::Derived, "derived"),
        ] {
            summary.kind = kind;
            summary.evaluation_state =
                (kind == PlaylistKind::Derived).then_some(EvaluationState::Ready);
            let page = personal_playlist(&summary, None, &entries);
            assert_eq!(page.total, Some(SONGS));
            assert_eq!(page.rows.len(), SONGS as usize);
            assert!(!page.more && page.status.is_empty());
            for (row, entry) in page.rows.iter().zip(&entries) {
                assert_eq!(row.position, Some(entry.position));
                assert!(row.playable && row.route.is_none());
                let (key, track_fields) = if kind == PlaylistKind::Manual {
                    (
                        RowKey::ManualEntry {
                            playlist_id: "mix".into(),
                            entry_id: entry.id.clone(),
                        },
                        json!({"id":"track%2F音", "entryId":entry.id}),
                    )
                } else {
                    (
                        RowKey::DerivedOccurrence {
                            playlist_id: "mix".into(),
                            occurrence_id: entry.id.clone(),
                        },
                        json!({"id":"track%2F音", "targetId":"mix", "occurrenceId":entry.id}),
                    )
                };
                assert_eq!(row.key, key);
                assert_eq!(
                    selection(row),
                    json!({"kind":"track", "track":track_fields,
                    "context":{"kind":wire_kind, "id":"mix", "name":"Mix"}})
                );
            }
            assert_eq!(
                page.playback
                    .as_ref()
                    .unwrap()
                    .context()
                    .unwrap()
                    .info_route(),
                route
            );
            if kind == PlaylistKind::Manual {
                let detail = personal::Playlist {
                    summary: summary.clone(),
                    definition: None,
                    token: "complete".into(),
                    rows: entries.clone().into(),
                    stale: false,
                };
                let order = crate::actions::PlaylistOrder::current(&route, &page, &detail).unwrap();
                for (index, anchor) in [
                    (1099, "occurrence-1100"),
                    (1100, ""),
                    (2, "occurrence-1162"),
                ] {
                    let change = order.movement(&entries[index].id, anchor).unwrap();
                    assert_eq!(
                        change.command,
                        personal::Command::MoveEntry {
                            playlist_id: "mix".into(),
                            entry_id: entries[index].id.clone(),
                            after: anchor.into(),
                        }
                    );
                }
                let change =
                    crate::actions::mutation(&route, &page, "remove", Some(1162), "").unwrap();
                assert_eq!(
                    change.command,
                    personal::Command::RemoveEntry {
                        playlist_id: "mix".into(),
                        entry_id: "occurrence-1162".into(),
                    }
                );
            } else {
                assert!(crate::actions::mutation(&route, &page, "remove", Some(1162), "").is_err());
            }
        }
    }

    #[test]
    fn history_distinguishes_repeated_plays_and_never_opens_missing_tracks() {
        let online = history_row(history_entry("event", false));
        assert_eq!(online.key, RowKey::HistoryEvent("online:event".into()));
        assert!(selection(&online).get("context").is_none());
        let mut offline = history_entry("event", false);
        offline.origin = Some("offline".into());
        let offline = history_row(offline);
        let repeated = history_row(history_entry("another-event", false));
        assert_eq!(offline.id, online.id);
        assert_eq!(repeated.id, online.id);
        assert_ne!(offline.key, online.key);
        assert_ne!(repeated.key, online.key);
        let missing = history_row(history_entry("old-event", true));
        assert!(!missing.playable && missing.item.is_none() && missing.route.is_none());
    }

    #[test]
    fn catalog_commit_is_transactional_and_requires_the_accepted_boundary() {
        let mut page = Page::default();
        let batch =
            |expected: Option<u32>, next: Option<u32>, revision, keys: &[&str]| CatalogBatch {
                rows: keys
                    .iter()
                    .map(|key| Row {
                        key: RowKey::Track((*key).into()),
                        ..Row::default()
                    })
                    .collect(),
                total: 3,
                expected: expected.map(Next::Offset),
                next: next.map(Next::Offset),
                snapshot: Some(Snapshot::Catalog(revision)),
            };
        batch(None, Some(1), 8, &["a"]).commit(&mut page).unwrap();
        let accepted = page.clone();
        for invalid in [
            batch(None, None, 8, &["b", "c"]),
            batch(Some(1), None, 9, &["b", "c"]),
            batch(Some(1), None, 8, &[]),
            batch(Some(1), None, 8, &["a", "c"]),
            batch(Some(1), None, 8, &["b", "b"]),
        ] {
            assert_eq!(invalid.commit(&mut page), Err(CHANGED));
            assert_eq!(page, accepted);
        }
        batch(Some(1), None, 8, &["b", "c"])
            .commit(&mut page)
            .unwrap();
        assert!(!page.more);
        assert_eq!(
            page.rows
                .iter()
                .map(|row| row.key.clone())
                .collect::<Vec<_>>(),
            ["a", "b", "c"].map(|key| RowKey::Track(key.into()))
        );
    }

    #[test]
    fn offset_progress_and_append_are_bounded_and_reject_overlap_or_cursor_cycles() {
        assert_eq!(CHANGED.status, 409);
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
        for start in (2..999).step_by(PAGE_SIZE as usize) {
            let end = (start + PAGE_SIZE as usize).min(999);
            let before = if start == 2 {
                "b".into()
            } else {
                start.to_string()
            };
            page.append(
                (start..end).map(row).collect(),
                999,
                (end < 999).then(|| Next::Cursor(end.to_string())),
                None,
                Some(before),
            )
            .unwrap();
            assert_eq!(page.rows.len(), end);
            assert_eq!(page.more, end < 999);
        }
        assert!(page.state.next.is_none());
    }
}
