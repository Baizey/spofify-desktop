//! Desktop browsing controller. A single owner accepts navigation and writes;
//! cancellable reads populate bounded account-scoped snapshots off the UI thread.
use crate::{
    AppWindow, ArtistCredit, BrowseRow, PlaylistActions, PlaylistListOffline, PlaylistOfflineMark,
    PlaylistOfflineState, PlaylistSourceRestore, ReorderRow, SearchAddChoice, SearchAddRow,
    SearchAddSelection, actions,
    api::{Api, ApiError, Session, library::PlaylistKind},
    engine::Command,
    pages::{self, Page, Route, RowKey},
    personal::PersonalCommand,
    personal_bridge::{Effect, Link, Mutation, PersonalAccess, Status},
    player, playlist_browser, playlist_pins, quick_add,
    replica::{Replica, Trail},
    saved,
};
use slint::{ComponentHandle, Model, ModelRc, VecModel};
use std::{
    collections::{HashMap, HashSet},
    hash::{BuildHasher, RandomState},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::Duration,
};
use tokio::{
    sync::{mpsc, watch},
    task::JoinHandle,
};

#[derive(Clone)]
pub struct Access {
    pub epoch: u64,
    pub api: Arc<Api>,
    pub session: Session,
    pub offline: bool,
    pub personal: PersonalAccess,
}
impl Access {
    fn usable(&self) -> bool {
        self.api.authenticated_account(&self.session).is_ok()
    }

    fn local(&self) -> bool {
        self.offline || self.session.is_offline()
    }

    fn same_browse_account(&self, next: &Self) -> bool {
        self.epoch == next.epoch
            && (Arc::ptr_eq(&self.api, &next.api)
                || self.offline
                    && self.session.is_local_only()
                    && !next.local()
                    && !next.session.is_local_only()
                    && self.api.bound_account(&self.session).is_ok_and(|old| {
                        next.api
                            .authenticated_account(&next.session)
                            .is_ok_and(|new| old == new)
                    }))
    }
}
enum Intent {
    Navigate(String),
    Search(String),
    Row(String, usize),
    Page(String, String),
    ChooseQuickTarget {
        scope: String,
        id: String,
    },
    ToggleQuickSkip {
        scope: String,
    },
    CancelQuickBulk {
        scope: String,
    },
    QuickAdd {
        scope: String,
        target: String,
        kind: String,
        source: String,
        index: usize,
    },
    PlaylistOrderDrop {
        scope: String,
        grabbed: String,
        after: String,
    },
}
struct Input {
    epoch: u64,
    generation: u64,
    intent: Intent,
    catalog_position: Option<CatalogPosition>,
    title_origin: Option<TitleOrigin>,
}

struct CatalogPosition {
    y: f32,
    anchor: i32,
    anchor_key: Option<RowKey>,
    anchor_offset: f32,
    row: i32,
    row_key: Option<RowKey>,
    part: String,
    credit: String,
}

impl CatalogPosition {
    fn capture_keys(&mut self, rows: &[pages::Row]) {
        let key = |index| {
            usize::try_from(index)
                .ok()
                .and_then(|i| rows.get(i))
                .filter(|row| row.key != RowKey::None)
                .map(|row| row.key.clone())
        };
        self.anchor_key = key(self.anchor);
        self.row_key = key(self.row);
    }

    fn resolve_keys(&mut self, rows: &[pages::Row]) {
        let index = |key: &Option<RowKey>| {
            key.as_ref()
                .and_then(|key| unique_row_index(rows.iter().map(|row| &row.key == key)))
                .unwrap_or(-1)
        };
        self.anchor = index(&self.anchor_key);
        self.row = index(&self.row_key);
    }
}

fn matching_title_control(control: &str, album: &str) -> bool {
    !album.is_empty() && control.strip_prefix("album:") == Some(album)
}

fn playlist_focus_destination(rows: &[BrowseRow], occurrence: &str, control: &str) -> Option<i32> {
    if occurrence.is_empty() {
        return None;
    }
    let index = unique_row_index(rows.iter().map(|row| row.occurrence_key == occurrence))?;
    // Enter on a title navigates, whereas Enter on the track body may play.
    // If the control changed identity, hand focus to the list, not the body.
    (control.is_empty() || matching_title_control(control, &rows[index as usize].album_id))
        .then_some(index)
}

fn unique_row_index(matches: impl Iterator<Item = bool>) -> Option<i32> {
    let mut indices = matches
        .enumerate()
        .filter_map(|(i, matches)| matches.then_some(i));
    let first = indices.next()?;
    if indices.next().is_some() {
        return None;
    }
    i32::try_from(first).ok()
}

#[derive(Debug, PartialEq, Eq)]
enum TitleSource {
    Queue { room: String, track: String },
    Dock,
    Playlist,
}

#[derive(Debug, PartialEq, Eq)]
struct TitleOrigin {
    source: TitleSource,
    key: String,
    album: String,
}

impl TitleOrigin {
    fn capture(ui: &AppWindow, album: &str) -> Option<Self> {
        let key = ui.get_title_source_key();
        if key.is_empty() || !crate::api::catalog::segment_id(album) {
            return None;
        }
        let source = match ui.get_title_source().as_str() {
            "queue" => {
                let room = ui.get_queue_room_key();
                if room.is_empty() {
                    return None;
                }
                let track = ui
                    .get_queue_rows()
                    .iter()
                    .find(|row| row.key == key && row.album_id == album)?
                    .track_id;
                TitleSource::Queue {
                    room: room.into(),
                    track: track.into(),
                }
            }
            "dock" if ui.get_now_track_id() == key && ui.get_now_album_id() == album => {
                TitleSource::Dock
            }
            "playlist" => TitleSource::Playlist,
            _ => return None,
        };
        Some(Self {
            source,
            key: key.into(),
            album: album.into(),
        })
    }

    fn playlist_index(&self, rows: &[pages::Row], route: &Route) -> Option<i32> {
        if !matches!(route, Route::Playlist(_) | Route::SavedCollection(_)) {
            return None;
        }
        let index = unique_row_index(
            rows.iter()
                .map(|row| playlist_occurrence_key(row, route).as_ref() == self.key),
        )?;
        (track_album(&rows[index as usize]).0 == self.album).then_some(index)
    }

    fn publish(self, ui: &AppWindow, playlist_row: i32) {
        let (source, room, track) = match &self.source {
            TitleSource::Queue { room, track } => {
                if ui.get_queue_room_key() != room.as_str()
                    || !ui.get_queue_rows().iter().any(|row| {
                        row.key == self.key.as_str()
                            && row.track_id == track.as_str()
                            && row.album_id == self.album.as_str()
                    })
                {
                    return;
                }
                ("queue", room.as_str(), track.as_str())
            }
            TitleSource::Dock => {
                if ui.get_now_track_id() != self.key.as_str()
                    || ui.get_now_album_id() != self.album.as_str()
                {
                    return;
                }
                ("dock", "", "")
            }
            TitleSource::Playlist => ("playlist", "", ""),
        };
        ui.set_title_return_source(source.into());
        ui.set_title_return_room(room.into());
        ui.set_title_return_track(track.into());
        ui.set_title_return_key(self.key.into());
        ui.set_title_return_album(self.album.into());
        ui.set_title_return_row(playlist_row);
        ui.set_title_return_serial(ui.get_title_return_serial().wrapping_add(1));
    }
}

#[derive(Default)]
struct CatalogContinuation {
    loading: bool,
    stopped: bool,
    status: String,
}

#[derive(Default)]
struct QuickProjection {
    selection: SearchAddSelection,
    choices: Vec<SearchAddChoice>,
    scope: slint::SharedString,
    bulk_scope: slint::SharedString,
    status: slint::SharedString,
    status_error: bool,
    blocked: slint::SharedString,
    skip: bool,
    bulk_active: bool,
    rows: Vec<SearchAddRow>,
}

fn focus_add_songs_search(ui: &AppWindow) {
    ui.set_search_add_open_serial(ui.get_search_add_open_serial().wrapping_add(1));
}

impl QuickProjection {
    fn publish(self, ui: &AppWindow) {
        if ui.get_search_add_scope() != self.scope {
            ui.set_search_add_scope(self.scope);
        }
        if ui.get_search_add_bulk_scope() != self.bulk_scope {
            ui.set_search_add_bulk_scope(self.bulk_scope);
        }
        if ui.get_search_add_selection() != self.selection {
            ui.set_search_add_selection(self.selection);
        }
        if !ui
            .get_search_add_choices()
            .iter()
            .eq(self.choices.iter().cloned())
        {
            ui.set_search_add_choices(ModelRc::new(VecModel::from(self.choices)));
        }
        let rows = ui.get_search_add_rows();
        if !rows.iter().eq(self.rows.iter().cloned()) {
            if let Some(model) = rows.as_any().downcast_ref::<VecModel<SearchAddRow>>()
                && model.row_count() == self.rows.len()
            {
                // Membership and bulk progress must not remount the ranked list.
                for (index, row) in self.rows.into_iter().enumerate() {
                    if model.row_data(index).as_ref() != Some(&row) {
                        model.set_row_data(index, row);
                    }
                }
            } else {
                ui.set_search_add_rows(ModelRc::new(VecModel::from(self.rows)));
            }
        }
        ui.set_search_add_skip(self.skip);
        ui.set_search_add_status(self.status);
        ui.set_search_add_status_error(self.status_error);
        ui.set_search_add_bulk_active(self.bulk_active);
        ui.set_search_add_blocked(self.blocked);
    }
}

struct CatalogVisit {
    page: Option<Arc<Page>>,
    position: CatalogPosition,
    continuation: CatalogContinuation,
}

impl CatalogVisit {
    fn retire_snapshot(&mut self) {
        // Positions are route-scoped typed identities, not remote rows.
        self.page = None;
        self.continuation = CatalogContinuation::default();
    }
}

#[derive(Clone)]
struct CatalogDemand {
    epoch: u64,
    generation: u64,
    key: String,
}

struct Stamp {
    epoch: Arc<AtomicU64>,
    shown: Arc<AtomicU64>,
}
#[derive(Clone)]
pub struct Desk {
    tx: mpsc::Sender<Input>,
    catalog_demand: watch::Sender<Option<CatalogDemand>>,
    stamp: Arc<Stamp>,
}
impl Desk {
    fn submit(&self, ui: &AppWindow, intent: Intent) {
        let position =
            matches!(ui.get_page_kind().as_str(), "search" | "album" | "artist").then(|| {
                let y = ui.get_catalog_scroll_y();
                let anchor_offset = ui.get_catalog_anchor_y() + y;
                let row = ui
                    .get_catalog_row_navigation()
                    .then(|| ui.get_catalog_focused_row());
                CatalogPosition {
                    y,
                    anchor: if anchor_offset <= 0.0 {
                        ui.get_catalog_anchor_row()
                    } else {
                        -1
                    },
                    anchor_key: None,
                    anchor_offset,
                    row: row.unwrap_or(-1),
                    row_key: None,
                    part: row.map_or_else(String::new, |_| ui.get_catalog_focused_part().into()),
                    credit: row
                        .map_or_else(String::new, |_| ui.get_catalog_focused_credit().into()),
                }
            });
        let title_origin = match &intent {
            Intent::Page(op, album) if op == "album" => TitleOrigin::capture(ui, album),
            _ => None,
        };
        if self
            .tx
            .try_send(Input {
                epoch: self.stamp.epoch.load(Ordering::SeqCst),
                generation: self.stamp.shown.load(Ordering::SeqCst),
                catalog_position: position,
                title_origin,
                intent,
            })
            .is_err()
        {
            ui.set_page_status("Please finish the current action first.".into());
        }
    }
    pub fn bind(
        &self,
        ui: &AppWindow,
        mode: crate::mode::ModeControl,
        access: watch::Receiver<Option<Access>>,
    ) {
        let (d, w) = (self.clone(), ui.as_weak());
        ui.on_navigate(move |s| {
            if let Some(ui) = w.upgrade() {
                d.submit(&ui, Intent::Navigate(s.into()));
            }
        });
        let (d, w) = (self.clone(), ui.as_weak());
        ui.on_search_edited(move || {
            if let Some(ui) = w.upgrade() {
                d.submit(&ui, Intent::Search(ui.get_query().to_string()));
            }
        });
        let (d, w) = (self.clone(), ui.as_weak());
        ui.on_choose_search_add_destination(move |id| {
            if let Some(ui) = w.upgrade() {
                d.submit(
                    &ui,
                    Intent::ChooseQuickTarget {
                        scope: ui.get_search_add_scope().into(),
                        id: id.into(),
                    },
                );
            }
        });
        let (d, w) = (self.clone(), ui.as_weak());
        ui.on_toggle_search_add_skip(move || {
            if let Some(ui) = w.upgrade() {
                d.submit(
                    &ui,
                    Intent::ToggleQuickSkip {
                        scope: ui.get_search_add_scope().into(),
                    },
                );
            }
        });
        let (d, w) = (self.clone(), ui.as_weak());
        ui.on_cancel_search_bulk(move || {
            if let Some(ui) = w.upgrade() {
                d.submit(
                    &ui,
                    Intent::CancelQuickBulk {
                        scope: ui.get_search_add_bulk_scope().into(),
                    },
                );
            }
        });
        let (d, w) = (self.clone(), ui.as_weak());
        ui.on_quick_add_search_result(move |target, kind, source, index| {
            if let Some(ui) = w.upgrade()
                && let Ok(index) = usize::try_from(index)
            {
                let scope = if kind.as_str() == "track" {
                    ui.get_search_add_scope()
                } else {
                    ui.get_search_add_bulk_scope()
                };
                d.submit(
                    &ui,
                    Intent::QuickAdd {
                        scope: scope.into(),
                        target: target.into(),
                        kind: kind.into(),
                        source: source.into(),
                        index,
                    },
                );
            }
        });
        let (d, w) = (self.clone(), ui.as_weak());
        ui.on_row_action(move |op, i| {
            if let Some(ui) = w.upgrade()
                && i >= 0
            {
                d.submit(&ui, Intent::Row(op.into(), i as usize));
            }
        });
        let d = self.clone();
        ui.on_catalog_demand(move |key| {
            d.catalog_demand.send_replace(Some(CatalogDemand {
                epoch: d.stamp.epoch.load(Ordering::SeqCst),
                generation: d.stamp.shown.load(Ordering::SeqCst),
                key: key.into(),
            }));
        });
        let (d, w) = (self.clone(), ui.as_weak());
        ui.on_playlist_order_drop(move |scope, grabbed, after| {
            if let Some(ui) = w.upgrade() {
                d.submit(
                    &ui,
                    Intent::PlaylistOrderDrop {
                        scope: scope.into(),
                        grabbed: grabbed.into(),
                        after: after.into(),
                    },
                );
            }
        });
        let (d, w) = (self.clone(), ui.as_weak());
        ui.on_page_action(move|op,value|{let Some(ui)=w.upgrade()else{return};let op=op.to_string();let mut val=value.to_string();
            if op=="offline" {
                // Fence new requests immediately, before the engine drops its active transports.
                let offline = val == "on";
                if mode.request_current(&access, d.stamp.epoch.load(Ordering::SeqCst), offline) && offline {
                    ui.set_manual_offline(true);
                }
                return;
            }
            if matches!(op.as_str(),"new-manual"|"new-rules"|"rename"|"rules"|"copy") {
                val=serde_json::json!({"name":ui.get_form_name().as_str(),"description":ui.get_form_description().as_str(),"definition":ui.get_form_value().as_str()}).to_string();
            }
            d.submit(&ui,Intent::Page(op,val));
        });
    }
}
enum Update {
    Read(Result<Page, ApiError>, bool), // true when the task will retry
    ReadGap,
    CatalogAppend(u64, Option<Result<pages::CatalogBatch, ApiError>>),
    LocalRead(u64, Result<Page, ApiError>),
    Availability(u64, Result<Page, ApiError>),
    VerifyUnavailable(u64, Vec<String>),
    Storage(Result<String, ApiError>),
    Changed(Result<Option<Route>, ApiError>),
    Local(String, Result<Command, ApiError>),
    CollectionSaved(Result<crate::collections::SaveSummary, ApiError>),
    PinChanged(Result<(), ApiError>),
}
struct Delivery {
    serial: u64,
    result: Update,
}
struct Pending {
    effect: Effect,
    before: Status,
    due: tokio::time::Instant,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PlaylistOrderBasis {
    page_generation: u64,
    network_mode: u64,
    // Includes the authority generation, interruption and observed change sequence.
    status: Status,
}
impl PlaylistOrderBasis {
    fn current(
        epoch: u64,
        page_generation: u64,
        network_mode: u64,
        status: Status,
    ) -> Option<Self> {
        (network_mode & 1 == 0 && status.epoch == epoch && status.link == Link::Ready).then_some(
            Self {
                page_generation,
                network_mode,
                status,
            },
        )
    }
}

struct PlaylistOrderScope {
    key: String,
    basis: PlaylistOrderBasis,
}
struct UnconfirmedOrder {
    epoch: u64,
    socket_generation: u64,
    revision: u64,
}
impl UnconfirmedOrder {
    fn blocks(&self, status: Status, revision: u64) -> bool {
        // A terminal part may belong to a read sent before the move. A new
        // detail stamp alone is not evidence that the playlist advanced.
        status.epoch == self.epoch
            && status.generation <= self.socket_generation
            && revision <= self.revision
    }
}
struct PlaylistOrderPublication {
    scope: String,
    rows: Option<Vec<ReorderRow>>, // None keeps the current row model on a scope-only update.
}

const INITIAL_ROUTE: Route = Route::History;
const PERSONAL_READ_TIMEOUT: Duration = Duration::from_secs(10);
const PERSONAL_WRITE_TIMEOUT: Duration = Duration::from_secs(12);
struct Browser {
    ui: slint::Weak<AppWindow>,
    epoch: Arc<AtomicU64>,
    shown: Arc<AtomicU64>,
    engine: mpsc::Sender<Command>,
    access: Option<Access>,
    route: Route,
    page_keys: RandomState,
    trail: Trail,
    cache: Replica,
    page: Arc<Page>,
    catalog_visits: HashMap<u64, CatalogVisit>,
    catalog_position: Option<CatalogPosition>,
    catalog_restore: Option<CatalogPosition>,
    title_visits: HashMap<u64, TitleOrigin>,
    title_restore: Option<TitleOrigin>,
    add_songs_visits: HashSet<u64>,
    add_songs_restore: bool,
    add_songs_open: bool,
    continuation: CatalogContinuation,
    generation: u64,
    serial: u64,
    busy: bool,
    silent_read: bool,
    personal_ready: bool,
    personal_due: Option<tokio::time::Instant>,
    personal_view: Option<Status>,
    playlist_order: Option<PlaylistOrderScope>,
    playlist_order_model_generation: Option<u64>,
    playlist_order_serial: u64,
    unconfirmed_orders: HashMap<String, UnconfirmedOrder>,
    pending: Option<Pending>,
    edit_basis: Option<EditBasis>,
    row_editor: Option<RowEditor>,
    writing: bool,
    message: String,
    error: bool,
    task: Option<JoinHandle<()>>,
    results: mpsc::Sender<Delivery>,
    art: watch::Sender<Option<crate::artwork::Request>>,
    pins: playlist_pins::Controller,
    pin_view: playlist_pins::View,
    saved_reload_due: bool,
    unpinning: bool,
}
/// Opaque, account-session-local identity for the shell's scroll position.
/// Hash the exact typed route, not its display title or refresh generation, and
/// never serialize the raw query or server entity ID into the scroll key.
fn page_key(route: &Route, keys: Option<&RandomState>) -> String {
    keys.map_or_else(String::new, |keys| {
        format!("p-{:016x}", keys.hash_one(route))
    })
}

fn label(route: &Route) -> &'static str {
    match route {
        Route::History => "History",
        Route::Search(_) => "Search",
        Route::Playlists => "Playlists",
        Route::Picker(_) => "Add to a playlist",
        Route::Offline => "Downloads",
        Route::Appearance => "Settings",
        Route::Now => "Now playing",
        Route::Queue => "Queue",
        Route::Album(_) => "Album",
        Route::Artist(_) => "Artist",
        Route::Playlist(_) => "Playlist",
        Route::SavedCollection(_) => "Saved collection",
    }
}
enum Navigation {
    Section(Route),
    Follow(Route),
    AddSongs,
    Untracked(Route), // Refresh or land after a mutation without adding a Back entry.
    Back,
}

impl Navigation {
    fn named(name: &str) -> Option<Self> {
        Some(match name {
            "history" => Self::Section(Route::History),
            "playlists" => Self::Section(Route::Playlists),
            "offline" => Self::Section(Route::Offline),
            "appearance" | "settings" => Self::Section(Route::Appearance),
            "search" => Self::Follow(Route::Search(String::new())),
            "now" => Self::Follow(Route::Now),
            "queue" => Self::Follow(Route::Queue),
            "back" => Self::Back,
            _ => return None,
        })
    }

    fn resolve(
        self,
        current: &Route,
        trail: &mut Trail,
        reading: bool,
        failed: bool,
        preserve_title_source: bool,
    ) -> Option<Route> {
        match self {
            Self::Section(next) => {
                // A selected rail item can still be showing a detail. Decide
                // from the actual route, and clear Back even on a same-route
                // click if this section was reached through another action.
                if !trail.can_back() && repeat_navigation(current, &next, reading, failed) {
                    return None;
                }
                trail.clear();
                Some(next)
            }
            Self::Follow(next) => {
                if repeat_navigation(current, &next, reading, failed) {
                    return None;
                }
                if preserve_title_source {
                    trail.visit_from_title(current, &next);
                } else {
                    trail.visit(current, &next);
                }
                Some(next)
            }
            Self::AddSongs => {
                let next = Route::Search(String::new());
                trail.visit_from_title(current, &next);
                Some(next)
            }
            Self::Untracked(next) => Some(next),
            Self::Back => trail.pop(),
        }
    }
}
fn context_route(kind: &str, id: &str) -> Option<Route> {
    // A remote player may still have a likes context, but it has no desktop page.
    match kind {
        "album" | "artist" => pages::music_route(kind, id),
        "saved" if id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit()) => {
            Some(Route::SavedCollection(id.into()))
        }
        "playlist" | "derived"
            if crate::api::catalog::segment_id(id) && !matches!(id, "sources" | "preview") =>
        {
            Some(Route::Playlist(id.into()))
        }
        _ => None,
    }
}

fn projected_music(credit: Option<&pages::Credit>, artist: bool) -> (&str, &str) {
    let Some(credit) = credit else {
        return ("", "");
    };
    let id = match (&credit.target, artist) {
        (Some(Route::Artist(id)), true) | (Some(Route::Album(id)), false)
            if crate::api::catalog::segment_id(id) =>
        {
            id
        }
        _ => "",
    };
    (id, &credit.name)
}

fn track_album(row: &pages::Row) -> (&str, &str) {
    if row.kind != "track" {
        return ("", "");
    }
    projected_music(row.music.album.as_ref(), false)
}

fn search_track_unavailable(route: &Route, row: &pages::Row) -> bool {
    // Search's catalog row uses hasAudio; local search only projects verified recordings.
    matches!(route, Route::Search(_)) && row.kind == "track" && !row.playable
}

fn quick_add_kind(row: &pages::Row) -> Option<&str> {
    if row.id.is_empty() {
        return None;
    }
    match (row.kind.as_str(), &row.key, &row.route) {
        ("track", RowKey::Track(id), _) if row.playable && *id == row.id => Some("track"),
        (kind @ "album", RowKey::Album(id), Some(Route::Album(route)))
        | (kind @ "artist", RowKey::Artist(id), Some(Route::Artist(route)))
            if *id == row.id && *route == row.id =>
        {
            Some(kind)
        }
        _ => None,
    }
}

fn quick_track_state(view: &quick_add::View<'_>, id: &str) -> &'static str {
    let track = view.row(id);
    if track.pending {
        "pending"
    } else if track.membership == quick_add::Membership::Present {
        "member"
    } else if track.outcome == Some(quick_add::Outcome::Unconfirmed) {
        "unconfirmed"
    } else if track.can_add && track.outcome == Some(quick_add::Outcome::NotSent) {
        "not-sent"
    } else if track.can_add {
        "ready"
    } else if view.at_capacity && track.membership == quick_add::Membership::Absent {
        "at-capacity"
    } else {
        "unavailable"
    }
}

fn add_songs_eligible(route: &Route, page: &Page) -> bool {
    matches!(route, Route::Playlist(id) if page.playlist.as_ref().is_some_and(|playlist|
        playlist.id == *id && playlist.kind == PlaylistKind::Manual
        && playlist.capabilities.edit_entries && playlist.capabilities.add_tracks))
}

fn playlist_occurrence_key<'a>(row: &'a pages::Row, route: &Route) -> std::borrow::Cow<'a, str> {
    use std::borrow::Cow;
    match &row.key {
        RowKey::ManualEntry { entry_id, .. } => Cow::Borrowed(entry_id),
        RowKey::DerivedOccurrence { occurrence_id, .. } => Cow::Borrowed(occurrence_id),
        RowKey::Track(_) if matches!(route, Route::Playlist(_) | Route::SavedCollection(_)) => row
            .position
            .map(|pos| format!("saved:{pos}:{}", row.id))
            .map_or(Cow::Borrowed(""), Cow::Owned),
        _ => Cow::Borrowed(""),
    }
}

fn artist_credits(music: &pages::MusicMetadata) -> ModelRc<ArtistCredit> {
    ModelRc::new(VecModel::from(
        music
            .credits
            .iter()
            .map(|credit| {
                let (id, name) = projected_music(Some(credit), true);
                ArtistCredit {
                    id: id.into(),
                    name: name.into(),
                }
            })
            .collect::<Vec<_>>(),
    ))
}
fn repeat_navigation(current: &Route, next: &Route, reading: bool, failed: bool) -> bool {
    current == next && (reading || (matches!(next, Route::Search(_)) && !failed))
}
fn saved_route(route: &Route) -> bool {
    matches!(route, Route::Offline | Route::SavedCollection(_))
}
fn row_open_target<'a>(route: &Route, row: &'a pages::Row) -> Option<&'a Route> {
    // Track rows play only through an explicit Play intent, even in Saved.
    row.route
        .as_ref()
        .filter(|target| row.kind != "track" && *target != route)
}
fn strings(v: Vec<&str>) -> ModelRc<slint::SharedString> {
    ModelRc::new(VecModel::from(
        v.into_iter()
            .map(slint::SharedString::from)
            .collect::<Vec<_>>(),
    ))
}
fn playlist_pin_action(pinned: bool, can_pin: bool) -> Option<&'static str> {
    if pinned {
        Some("unpin-playlist")
    } else if can_pin {
        Some("pin-playlist")
    } else {
        None
    }
}
fn playlist_list_target(row: &pages::Row) -> Option<&pages::PlaylistMetadata> {
    row.playlist.as_ref().filter(|m| {
        m.id == row.id && matches!(&row.route, Some(Route::Playlist(id)) if id == &m.id)
    })
}

const PLAYLIST_EDITS: [&str; 3] = ["rename", "rules", "delete"];
const STALE_PLAYLIST_FORM: &str = "This playlist changed. Close the editor and open it again.";

fn can_edit_playlist(m: &pages::PlaylistMetadata, op: &str) -> bool {
    m.revision > 0
        && match op {
            "rename" => m.capabilities.edit_metadata,
            "rules" => {
                m.kind == crate::api::library::PlaylistKind::Derived
                    && m.capabilities.edit_definition
            }
            "delete" => m.capabilities.delete,
            _ => false,
        }
}

struct EditBasis {
    op: String,
    id: String,
    revision: u64,
    rules: Option<(u64, crate::personal::RulesBasis)>, // socket generation and exact prior definition
}

struct RowEditor {
    op: &'static str,
    id: String,
    revision: u64,
}
impl RowEditor {
    fn select(row: &pages::Row, op: &str) -> Option<Self> {
        let m = playlist_list_target(row)?;
        let op = PLAYLIST_EDITS.into_iter().find(|&action| action == op)?;
        can_edit_playlist(m, op).then(|| Self {
            op,
            id: m.id.clone(),
            revision: m.revision,
        })
    }
    fn ready(&self, page: &Page, rows_ready: bool) -> bool {
        if self.op == "rules" { page.live_rules } else { rows_ready }
    }

    fn matches(&self, route: &Route, page: &Page) -> bool {
        matches!(route, Route::Playlist(id) if id == &self.id)
            && page.playlist.as_ref().is_some_and(|m| {
                m.id == self.id && m.revision == self.revision && can_edit_playlist(m, self.op)
            })
    }
}

fn search_entity_actions(
    page: &Page,
    row: &pages::Row,
    offline: bool,
) -> Option<&'static [&'static str]> {
    let (details, id): (&[_], _) = match (row.kind.as_str(), &row.route) {
        ("album", Some(Route::Album(id))) if id == &row.id => (&["open-album"], id),
        ("artist", Some(Route::Artist(id))) if id == &row.id => (&["open-artist"], id),
        _ => return None,
    };
    if !crate::api::catalog::segment_id(id) {
        return None;
    }
    // Online rows carry the same canonical collection selection as their
    // detail pages. Offline rows deliberately have no wire selection; their
    // playable flag comes from the verified local catalog instead.
    let playable = row.playable
        && if offline {
            page.local_audio
        } else {
            !page.local_audio
                && matches!(&row.playback, Some(pages::PlaybackSelection::Collection { context })
                    if context.kind.as_str() == row.kind && &context.id == id)
        };
    Some(if playable {
        &["play", "queue"]
    } else {
        details
    })
}

fn row_actions(
    p: &Page,
    r: &pages::Row,
    offline: bool,
    personal_actions: bool,
    pinned: bool,
) -> Vec<&'static str> {
    let mut v = Vec::new();
    if p.kind == "picker" {
        return v;
    }
    if p.kind == "search" && matches!(r.kind.as_str(), "album" | "artist") {
        return search_entity_actions(p, r, offline)
            .unwrap_or_default()
            .to_vec();
    }
    if offline && p.local_audio && r.playable {
        v.push("queue");
    }
    if p.kind == "saved-collection" {
        return v;
    }
    if p.kind == "offline" {
        v.push(if pinned { "unpin-playlist" } else { "remove" });
        return v;
    }
    if p.kind == "playlists" {
        if pinned && matches!(&r.route, Some(Route::Playlist(id)) if id == &r.id) {
            v.push("unpin-playlist");
        }
        if let Some(m) = playlist_list_target(r) {
            let can_use_account = !offline && personal_actions;
            if can_use_account && r.playable && r.playback.is_some() {
                v.insert(0, "queue");
            }
            // Download the playlist from its detail page, after the complete
            // projection is available for offline browsing.
            if can_use_account {
                v.extend(
                    PLAYLIST_EDITS
                        .into_iter()
                        .filter(|op| can_edit_playlist(m, op)),
                );
            }
        }
        return v;
    }
    if offline || !personal_actions {
        return v;
    }
    if r.playback.is_some() && r.playable {
        v.push("queue");
    }
    if r.item.is_some() {
        v.push("add");
        if r.playable {
            v.push("save");
        }
    }
    if matches!(r.key, pages::RowKey::ManualEntry { .. })
        && p.playlist
            .as_ref()
            .is_some_and(|m| m.capabilities.edit_entries)
    {
        v.push("remove");
    }
    v
}
fn playlist_detail_actions(
    route: &Route,
    page: &Page,
    pinned: bool,
    can_use_account: bool,
) -> PlaylistActions {
    let local_play = page.local_audio && page.rows.iter().any(|r| r.playable);
    match route {
        Route::Playlist(id) if let Some(m) = page.playlist.as_ref().filter(|m| &m.id == id) => {
            let playback = local_play || can_use_account && page.playback.is_some();
            PlaylistActions {
                play: playback,
                queue: playback,
                offline_action: playlist_pin_action(pinned, can_use_account)
                    .unwrap_or("")
                    .into(),
                rename: can_use_account && can_edit_playlist(m, "rename"),
                rules: !page.local_audio && page.live_rules && can_edit_playlist(m, "rules"),
                remove_action: if can_use_account && can_edit_playlist(m, "delete") {
                    "delete".into()
                } else {
                    Default::default()
                },
            }
        }
        Route::Playlist(_) if page.local_audio => PlaylistActions {
            play: local_play,
            queue: local_play,
            offline_action: playlist_pin_action(pinned, false).unwrap_or("").into(),
            ..Default::default()
        },
        Route::SavedCollection(_) if page.kind == "saved-collection" => PlaylistActions {
            play: local_play,
            queue: local_play,
            remove_action: if page.saved_pin.is_some() {
                "unpin-playlist"
            } else {
                "remove"
            }
            .into(),
            ..Default::default()
        },
        _ => PlaylistActions::default(),
    }
}

fn pin_subtitle(row: &pages::Row, status: Option<&playlist_pins::Status>) -> slint::SharedString {
    match status {
        Some(status) if !row.subtitle.is_empty() => {
            format!("{} · {}", row.subtitle, status.label()).into()
        }
        Some(status) => status.label().into(),
        None => row.subtitle.as_str().into(),
    }
}

fn playlist_list_offline(status: Option<&playlist_pins::Status>) -> PlaylistListOffline {
    use playlist_pins::Phase;

    let Some(status) = status.filter(|status| status.wanted) else {
        return PlaylistListOffline::default();
    };
    let (mark, percent) = match status.phase {
        Phase::Saved => (PlaylistOfflineMark::Saved, String::new()),
        Phase::Pending => (PlaylistOfflineMark::Pending, String::new()),
        Phase::Retry => (PlaylistOfflineMark::Retry, String::new()),
        Phase::Downloading { verified, total } => (
            PlaylistOfflineMark::Downloading,
            if total == 0 {
                String::new()
            } else {
                format!("{}%", verified.min(total) as u128 * 100 / total as u128)
            },
        ),
    };
    let description = match status.phase {
        Phase::Retry if status.has_committed => {
            format!(
                "Saved · Offline refresh interrupted: {}",
                status.detail_feedback()
            )
        }
        Phase::Retry => format!("Offline download interrupted: {}", status.detail_feedback()),
        _ => status.label(),
    };
    PlaylistListOffline {
        mark,
        retained: status.has_committed && status.phase != Phase::Saved,
        percent: percent.into(),
        description: description.into(),
    }
}

fn pin_row_copy(
    route: &Route,
    row: &pages::Row,
    status: Option<&playlist_pins::Status>,
) -> (slint::SharedString, PlaylistListOffline) {
    match route {
        Route::Playlists => (row.subtitle.as_str().into(), playlist_list_offline(status)),
        Route::Offline => (pin_subtitle(row, status), PlaylistListOffline::default()),
        _ => (row.subtitle.as_str().into(), PlaylistListOffline::default()),
    }
}

fn playlist_detail_offline(
    route: &Route,
    page: &Page,
    pin: Option<&playlist_pins::Status>,
) -> PlaylistOfflineState {
    // The committed pin or a loaded Saved snapshot represents usable local
    // membership, not merely the intent to save. Keep removal authority separate.
    let saved_snapshot = matches!(route, Route::SavedCollection(_))
        && page.kind == "saved-collection"
        && page.total.is_some();
    PlaylistOfflineState {
        saved: saved_snapshot || pin.is_some_and(|status| status.has_committed),
        feedback: pin.map_or_else(
            || {
                if matches!(route, Route::SavedCollection(_))
                    && page.saved_pin.is_some()
                    && !saved_snapshot
                {
                    "Waiting to download".into()
                } else {
                    Default::default()
                }
            },
            |status| status.detail_feedback().into(),
        ),
    }
}

// Detail pin state is published independently of the track model and notices.
struct PinDisplay<'a> {
    view: Option<&'a playlist_pins::View>,
    sources: HashMap<&'a str, &'a playlist_pins::Status>,
}
impl<'a> PinDisplay<'a> {
    fn new(view: &'a playlist_pins::View, access: Option<&Access>) -> Self {
        let view = access.filter(|a| Some(a.epoch) == view.epoch).map(|_| view);
        Self {
            sources: view
                .into_iter()
                .flat_map(|v| v.records.values())
                .filter(|r| r.wanted)
                .map(|r| (r.target.id.as_str(), r))
                .collect(),
            view,
        }
    }
    fn row(&self, row: &pages::Row) -> Option<&'a playlist_pins::Status> {
        match &row.route {
            Some(Route::Playlist(id)) if id == &row.id => self.sources.get(id.as_str()).copied(),
            Some(Route::SavedCollection(key)) => self.view?.records.get(key).filter(|r| r.wanted),
            _ => None,
        }
    }
    fn page(&self, route: &Route) -> Option<&'a playlist_pins::Status> {
        match route {
            Route::Playlist(id) => self.sources.get(id.as_str()).copied(),
            Route::SavedCollection(key) => self.view?.records.get(key).filter(|r| r.wanted),
            _ => None,
        }
    }
}
fn unavailable(message: &'static str) -> ApiError {
    ApiError {
        message,
        expired: false,
        status: 0,
    }
}
fn readable(e: &ApiError) -> String {
    match e.status {
        401 => "Sign in again to continue.".into(),
        403 => "This account does not have access to this item.".into(),
        404 => "This item is no longer available.".into(),
        409 | 410 | 412 => {
            "The collection changed. Check the current items before making another change.".into()
        }
        s if s >= 500 => "The library is temporarily unavailable.".into(),
        _ if e.message.contains("incompatible") || e.message.contains("response") => {
            "This item could not be opened.".into()
        }
        _ => e.message.into(),
    }
}
const READ_RETRIES: u32 = 4;
fn read_retry<T>(attempt: u32, result: &Result<T, ApiError>) -> bool {
    attempt < READ_RETRIES && result.as_ref().err().is_some_and(recoverable)
}
fn read_backoff(attempt: u32) -> Duration {
    Duration::from_secs(1 << attempt)
}
fn recoverable(e: &ApiError) -> bool {
    !e.expired
        && (matches!(e.status, 408 | 429 | 500..=599)
            || e.status == 0
                && (e.message.contains("reach")
                    || e.message.contains("connection")
                    || e.message.contains("temporarily")))
}
impl Browser {
    fn cancel(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
        self.serial += 1;
        self.busy = false;
        self.silent_read = false;
        // Same-view cancellation (for example, a save starting) must not
        // re-arm observer demand for the same in-flight boundary.
        self.continuation.stopped |= std::mem::take(&mut self.continuation.loading);
        self.writing = false;
        self.unpinning = false;
    }
    fn scoped_ui(&self, f: impl FnOnce(AppWindow) + Send + 'static) {
        let ui = self.ui.clone();
        let shared = self.epoch.clone();
        let epoch = self
            .access
            .as_ref()
            .map_or_else(|| shared.load(Ordering::SeqCst), |a| a.epoch);
        let session = self.access.as_ref().map(|a| a.session.clone());
        let _ = slint::invoke_from_event_loop(move || {
            if shared.load(Ordering::SeqCst) == epoch
                && session.as_ref().is_none_or(|s| !s.is_revoked())
                && let Some(ui) = ui.upgrade()
            {
                f(ui);
            }
        });
    }
    fn order_timeout_blocks(&self, status: Status, id: &str, revision: u64) -> bool {
        self.unconfirmed_orders
            .get(id)
            .is_some_and(|unconfirmed| unconfirmed.blocks(status, revision))
    }

    fn refresh_playlist_order(&mut self) -> Option<PlaylistOrderPublication> {
        let current = self.access.as_ref().and_then(|a| {
            if !matches!(self.route, Route::Playlist(_))
                || !self.personal_ready
                || a.offline
                || !a.usable()
            {
                return None;
            }
            let mode = *a.session.network_changes().borrow();
            a.personal.read(|status, replica| {
                // This page must come from the same observation as its authority stamp.
                if self.personal_view != Some(status) {
                    return None;
                }
                let basis = PlaylistOrderBasis::current(a.epoch, self.generation, mode, status)?;
                let detail = replica.playlist()?;
                let order = actions::PlaylistOrder::current(&self.route, &self.page, detail)?;
                let blocked =
                    self.order_timeout_blocks(status, &detail.summary.id, detail.summary.revision);
                Some((basis, order, blocked))
            })
        });
        let before = self.playlist_order.as_ref().map(|scope| scope.basis);
        // Preserve the just-dropped gesture's readback preview without granting a
        // new scope during a write. Admission still rejects input while writing.
        let after = current
            .as_ref()
            .filter(|(basis, _, blocked)| {
                !blocked
                    && (!self.writing
                        || (before == Some(*basis)
                            && self.pending.as_ref().is_some_and(|pending| {
                                pending.before == basis.status
                                    && matches!(&pending.effect, Effect::Move { .. })
                            })))
            })
            .map(|(basis, _, _)| *basis);
        let model_generation = current.as_ref().map(|(basis, _, _)| basis.page_generation);
        let models_changed = self.playlist_order_model_generation != model_generation;
        if before == after && !models_changed {
            return None;
        }
        let rows = models_changed.then(|| {
            current.map_or_else(Vec::new, |(_, order, _)| {
                order
                    .rows()
                    .map(|(key, row)| ReorderRow {
                        key: key.into(),
                        title: row.title.as_str().into(),
                        subtitle: row.subtitle.as_str().into(),
                        reorderable: true,
                        section: Default::default(),
                    })
                    .collect()
            })
        });
        self.playlist_order_model_generation = model_generation;
        self.playlist_order = after.map(|basis| {
            self.playlist_order_serial += 1;
            PlaylistOrderScope {
                key: format!("order-{:016x}", self.playlist_order_serial),
                basis,
            }
        });
        let scope = self
            .playlist_order
            .as_ref()
            .map(|scope| scope.key.clone())
            .unwrap_or_default();
        Some(PlaylistOrderPublication { scope, rows })
    }

    fn quick_request(&mut self, a: &Access, request: Option<(u64, PersonalCommand)>) {
        if let Some((generation, command)) = request
            && !a.local()
        {
            // Bounded by the shared scoped-read deadline. A busy/disconnected
            // transport never authorizes absence or automatically retries an add.
            let _ = self.engine.try_send(Command::Personal {
                epoch: a.epoch,
                generation,
                command,
            });
        }
    }

    fn quick_projection(&self) -> QuickProjection {
        let Some(a) = self.access.as_ref().filter(|a| a.usable()) else {
            return QuickProjection::default();
        };
        if !matches!(self.route, Route::Search(_)) {
            return QuickProjection::default();
        }
        let offline = a.local();
        let mode = *a.session.network_changes().borrow();
        let targets = if offline {
            Vec::new()
        } else {
            a.personal.quick_targets(a.epoch)
        };
        let mut labels = HashMap::new();
        for target in &targets {
            *labels.entry(&target.name).or_insert(0) += 1;
        }
        let choices: Vec<_> = targets
            .iter()
            .map(|target| SearchAddChoice {
                id: target.id.as_str().into(),
                name: target.name.as_str().into(),
                detail: if labels[&target.name] > 1 {
                    target.description.as_str().into()
                } else {
                    Default::default()
                },
            })
            .collect();
        a.personal.quick_view(a.epoch, |view| {
            let selection = view
                .target
                .map(|target| {
                    let index = choices
                        .iter()
                        .position(|choice| choice.id.as_str() == target.id);
                    SearchAddSelection {
                        id: target.id.as_str().into(),
                        name: target.name.as_str().into(),
                        detail: index.map(|i| choices[i].detail.clone()).unwrap_or_default(),
                        option_index: index.and_then(|i| i32::try_from(i + 1).ok()).unwrap_or(0),
                    }
                })
                .unwrap_or_default();
            let bulk = view.bulk.filter(|_| !offline);
            let bulk_active = bulk.is_some_and(|status| status.active);
            let mut rows = Vec::new();
            if view.target.is_some() && !offline {
                rows = self
                    .page
                    .rows
                    .iter()
                    .map(|row| {
                        let Some(kind) = quick_add_kind(row) else {
                            return SearchAddRow::default();
                        };
                        let state = if kind == "track" {
                            quick_track_state(&view, &row.id)
                        } else if bulk.is_some_and(|status| {
                            status.active && status.kind == kind && status.source == row.id
                        }) {
                            "pending"
                        } else if view.bulk_ready {
                            "ready"
                        } else {
                            "unavailable"
                        };
                        SearchAddRow {
                            id: row.id.as_str().into(),
                            state: state.into(),
                        }
                    })
                    .collect();
            }
            let (status, status_error) = if let Some(bulk) =
                bulk.filter(|status| status.active || !status.message.is_empty())
            {
                (
                    if bulk.message.is_empty() {
                        "Adding songs…"
                    } else {
                        bulk.message.as_str()
                    },
                    false,
                )
            } else if !offline && view.read_retired {
                ("Couldn’t check playlist membership.", true)
            } else if !offline && view.session_limit && view.target.is_some() {
                (
                    "Quick add is unavailable for this session. Use ⋯ to add songs.",
                    true,
                )
            } else {
                ("", false)
            };
            QuickProjection {
                scope: view.scope(mode).into(),
                bulk_scope: view.bulk_scope(mode).into(),
                selection,
                choices,
                skip: view.skip,
                bulk_active,
                rows,
                status: status.into(),
                status_error,
                blocked: if offline {
                    "Offline".into()
                } else if view.read_retired && view.target.is_some() {
                    "Unavailable".into()
                } else if view.read_stalled && view.target.is_some() {
                    "Checking playlist".into()
                } else {
                    Default::default()
                },
            }
        })
    }

    fn render(&mut self, models: bool) {
        self.render_rows(models, None);
    }

    fn catalog_request_key(&self) -> String {
        if self.route.catalog_continuity()
            && self.page.more
            && !self.continuation.stopped
            && !self.busy
            && self.task.is_none()
            && self
                .access
                .as_ref()
                .is_some_and(|a| !a.offline && !a.session.is_offline())
        {
            format!("{}:{}", self.generation, self.serial)
        } else {
            String::new()
        }
    }

    fn render_rows(&mut self, models: bool, append_from: Option<usize>) {
        if models {
            self.generation += 1;
        }
        let order = self.refresh_playlist_order();
        let quick = self.quick_projection();
        let playlist_add_songs = self.personal_ready
            && !self.writing
            && self.access.as_ref().is_some_and(|a| !a.local())
            && add_songs_eligible(&self.route, &self.page);
        let add_songs_open = std::mem::take(&mut self.add_songs_open);
        let add_songs_return = (!self.busy || self.page.playlist.is_some())
            && std::mem::take(&mut self.add_songs_restore);
        let catalog_request_key = self.catalog_request_key();
        let catalog_status = self.continuation.status.clone();
        let catalog_loading = self.continuation.loading;
        // After a mode change only the position survived. Wait for the
        // projected local/online model rather than resolving a loading shell.
        let catalog_restore = if self.busy {
            None
        } else {
            self.catalog_restore.take().map(|mut position| {
                position.resolve_keys(&self.page.rows);
                position
            })
        };
        // The playlist route can publish a loading shell before it publishes
        // the current membership. Resolve the occurrence against that membership.
        let title_restore = if self
            .title_restore
            .as_ref()
            .is_some_and(|origin| origin.source != TitleSource::Playlist || !self.busy)
        {
            self.title_restore.take().map(|origin| {
                let row = if origin.source == TitleSource::Playlist {
                    origin
                        .playlist_index(&self.page.rows, &self.route)
                        .unwrap_or(-1)
                } else {
                    -1
                };
                (origin, row)
            })
        } else {
            None
        };
        let token = self.generation;
        let shown = self.shown.clone();
        let p = self.page.clone();
        let route = self.route.clone();
        let access = self.access.clone();
        let pin_view = self.pin_view.clone();
        let pin = PinDisplay::new(&pin_view, access.as_ref());
        let kind = if p.kind.is_empty() {
            self.route.kind().into()
        } else {
            p.kind.clone()
        };
        let key = page_key(&self.route, self.access.as_ref().map(|_| &self.page_keys));
        let title = if p.title.is_empty() {
            label(&self.route).into()
        } else {
            p.title.clone()
        };
        let message = if self.message.is_empty() {
            p.status.clone()
        } else {
            self.message.clone()
        };
        let playlist_offline_state = playlist_detail_offline(&route, &p, pin.page(&route));
        let (playlist_metadata, playlist_description) =
            if matches!(route, Route::Playlist(_) | Route::SavedCollection(_)) {
                pages::playlist_detail_copy(&p)
            } else {
                Default::default()
            };
        let (busy, error, back) = (
            self.busy && !self.silent_read,
            self.error || (self.message.is_empty() && p.playlist_evaluation_failed),
            self.trail.can_back(),
        );
        let (account, offline) = self
            .access
            .as_ref()
            .map(|a| (a.session.username.clone(), a.local()))
            .unwrap_or_default();
        let can_play_rows = if p.local_audio {
            offline || saved_route(&route)
        } else {
            !offline && (!route.personal() || self.personal_ready)
        };
        let personal_actions = !self.route.personal() || self.personal_ready;
        let write = self.writing;
        let order_pending = self
            .pending
            .as_ref()
            .is_some_and(|pending| matches!(pending.effect, Effect::Move { .. }));
        let art = self.art.clone();
        let route = self.route.clone();
        let request = self.access.as_ref().map(|a| crate::artwork::Request {
            epoch: a.epoch,
            generation: token,
            route: self.route.clone(),
            page: p.clone(),
        });
        self.scoped_ui(move |ui| {
            let replacing_playlist = models
                && matches!(route, Route::Playlist(_) | Route::SavedCollection(_))
                && ui.get_page_key().as_str() == key;
            let switching_playlist_source = replacing_playlist
                && matches!(route, Route::Playlist(_))
                && ui.get_browse_local_audio() != p.local_audio;
            let control = ui.get_playlist_focus_control();
            let title_occurrence = ui.get_playlist_focus_occurrence();
            let restoring_title =
                replacing_playlist && !title_occurrence.is_empty() && control.starts_with("album:");
            let position = (switching_playlist_source || restoring_title).then(|| {
                let mut focused = ui.get_playlist_focus_index();
                let displayed = ui.get_browse_rows();
                let occurrence = if restoring_title {
                    let existing = unique_row_index(
                        displayed
                            .iter()
                            .map(|row| row.occurrence_key == title_occurrence),
                    );
                    if let Some(index) = existing {
                        focused = index;
                    }
                    existing
                        .and_then(|i| displayed.row_data(i as usize))
                        .filter(|row| matching_title_control(&control, &row.album_id))
                        .map(|row| row.occurrence_key)
                } else {
                    usize::try_from(focused)
                        .ok()
                        .and_then(|i| displayed.row_data(i))
                        .map(|row| row.occurrence_key)
                        .filter(|key| !key.is_empty())
                };
                (ui.get_catalog_scroll_y(), focused, occurrence, control)
            });
            // Invalidate a captured gesture before changing its view, then publish the
            // new scope after both row models and the shown generation are installed.
            if order.is_some() {
                ui.set_playlist_order_scope(Default::default());
            }
            ui.set_account_label(account.into());
            if kind == "now" && ui.get_page_kind() != "now" {
                // Record reopening with the route, before the media observer runs.
                ui.set_lyrics_open_generation(ui.get_lyrics_open_generation().wrapping_add(1));
            }
            ui.set_page_album_id(match &route {
                Route::Album(id) => id.as_str().into(),
                _ => Default::default(),
            });
            ui.set_page_kind(kind.clone().into());
            ui.set_page_key(key.into());
            ui.set_page_title(title.into());
            ui.set_page_subtitle(p.subtitle.clone().into());
            ui.set_playlist_metadata(playlist_metadata.into());
            ui.set_playlist_description(playlist_description.into());
            ui.set_playlist_offline_state(playlist_offline_state);
            ui.set_page_status(message.into());
            ui.set_page_busy(busy);
            ui.set_page_error(error);
            ui.set_account_actions_available(!offline && personal_actions);
            ui.set_can_back(back);
            ui.set_catalog_request_key(catalog_request_key.into());
            ui.set_catalog_continuation_status(catalog_status.into());
            ui.set_catalog_loading(catalog_loading);
            ui.set_form_busy(write);
            let pin = PinDisplay::new(&pin_view, access.as_ref());
            if models || append_from.is_some() {
                let rows = p
                    .rows
                    .iter()
                    .skip(append_from.unwrap_or(0))
                    .map(|r| {
                        let (album_id, album_name) = track_album(r);
                        let status = if matches!(route, Route::Playlists | Route::Offline) {
                            pin.row(r)
                        } else {
                            None
                        };
                        let (subtitle, playlist_list_offline) = pin_row_copy(&route, r, status);
                        BrowseRow {
                            section: Default::default(),
                            actions: strings(row_actions(
                                &p,
                                r,
                                offline,
                                personal_actions,
                                status.is_some(),
                            )),
                            kind: r.kind.as_str().into(),
                            track_id: if r.kind == "track" { r.id.as_str() } else { "" }.into(),
                            occurrence_key: playlist_occurrence_key(r, &route).as_ref().into(),
                            title: r.title.as_str().into(),
                            subtitle,
                            playlist_list_offline,
                            playlist_pinned: status.is_some(),
                            detail: r.detail.as_str().into(),
                            can_open: if matches!(route, Route::Picker(_)) {
                                !offline && personal_actions
                            } else {
                                row_open_target(&route, r).is_some()
                            },
                            playable: r.playable && can_play_rows,
                            track_unavailable: search_track_unavailable(&route, r),
                            credits: artist_credits(&r.music),
                            album_id: album_id.into(),
                            album_name: album_name.into(),
                        }
                    })
                    .collect::<Vec<_>>();
                if let Some(start) = append_from {
                    let displayed = ui.get_browse_rows();
                    if let Some(model) = displayed.as_any().downcast_ref::<VecModel<BrowseRow>>()
                        && model.row_count() == start
                        && shown.load(Ordering::SeqCst) == token
                    {
                        model.extend(rows);
                    }
                } else {
                    let restore = position.map(|(y, before, occurrence, control)| {
                        let occurrence = occurrence.unwrap_or_default();
                        let after = playlist_focus_destination(&rows, &occurrence, &control);
                        let control = after.map_or_else(Default::default, |_| control);
                        (y, before, after.unwrap_or(-1), control, occurrence)
                    });
                    ui.set_browse_model_key(token.to_string().into());
                    ui.set_browse_local_audio(p.local_audio);
                    ui.set_page_credits(artist_credits(&p.music));
                    ui.set_browse_rows(ModelRc::new(VecModel::from(rows)));
                    if let Some((y, before, after, control, occurrence)) = restore {
                        ui.set_playlist_source_restore(PlaylistSourceRestore {
                            y,
                            before,
                            after,
                            control,
                            occurrence,
                            serial: ui.get_playlist_source_restore().serial.wrapping_add(1),
                        });
                    }
                }
            }
            if models {
                let pinned = pin.page(&route).is_some();
                let mut actions = Vec::new();
                if p.local_audio
                    && matches!(route, Route::Offline | Route::Album(_) | Route::Artist(_))
                    && (p.local_collection_playable || p.rows.iter().any(|r| r.playable))
                {
                    actions.push("play");
                    actions.push("queue");
                }
                if !offline && personal_actions {
                    if !matches!(route, Route::Playlist(_) | Route::SavedCollection(_))
                        && p.playback.is_some()
                    {
                        actions.extend(["play", "queue"]);
                    }
                    if matches!(kind.as_str(), "album" | "artist") {
                        actions.push("save");
                    }
                    if kind == "picker" {
                        actions.push("new-manual");
                    }
                }
                if kind == "playlists" {
                    actions.extend(["new-manual", "new-rules"]);
                }
                ui.set_page_actions(strings(actions));
                ui.set_playlist_actions(playlist_detail_actions(
                    &route,
                    &p,
                    pinned,
                    !offline && personal_actions,
                ));

                shown.store(token, Ordering::SeqCst);
                ui.set_page_artwork(Default::default());
                ui.set_browse_artwork(Default::default());
                art.send_replace(request);
            }
            if let Some(order) = order {
                if let Some(rows) = order.rows {
                    ui.set_playlist_order_rows(ModelRc::new(VecModel::from(rows)));
                }
                ui.set_playlist_order_scope(order.scope.into());
            }
            quick.publish(&ui);
            ui.set_playlist_add_songs(playlist_add_songs);
            if add_songs_open {
                focus_add_songs_search(&ui);
            }
            if add_songs_return {
                ui.set_search_add_return_serial(ui.get_search_add_return_serial().wrapping_add(1));
            }
            // Keep the preview until the confirmed rows and scope are published,
            // not until a separate form-completion callback clears form-busy.
            ui.set_playlist_order_pending(order_pending);
            if let Some(position) = catalog_restore {
                ui.set_catalog_return_key(ui.get_page_key());
                ui.set_catalog_return_y(position.y);
                ui.set_catalog_return_anchor(position.anchor);
                ui.set_catalog_return_offset(position.anchor_offset);
                ui.set_catalog_return_row(position.row);
                ui.set_catalog_return_part(position.part.into());
                ui.set_catalog_return_credit(position.credit.into());
                ui.set_catalog_return_serial(ui.get_catalog_return_serial().wrapping_add(1));
            }
            if let Some((origin, row)) = title_restore {
                origin.publish(&ui, row);
            }
        });
    }
    fn pins_changed(&mut self, view: playlist_pins::View) {
        let old = std::mem::replace(&mut self.pin_view, view);
        if self.access.as_ref().map(|a| a.epoch) != self.pin_view.epoch {
            return;
        }
        let structural = old.epoch != self.pin_view.epoch || old.revision != self.pin_view.revision;
        let refresh_local_list =
            structural && self.page.local_audio && self.route == Route::Playlists;
        if structural {
            self.cache.saved_changed();
            if saved_route(&self.route)
                || self.page.local_audio && matches!(self.route, Route::Playlist(_))
            {
                if self.writing {
                    self.saved_reload_due = true;
                } else {
                    self.visit(Navigation::Untracked(self.route.clone()));
                }
                return;
            }
        }
        if !matches!(self.route, Route::Playlists | Route::Playlist(_)) && !saved_route(&self.route)
        {
            return;
        }
        // Update the status and, only when intent changes, the menu actions in
        // place. Do not remount the trigger or reload artwork on a pin update.
        let prev = PinDisplay::new(&old, self.access.as_ref());
        let next = PinDisplay::new(&self.pin_view, self.access.as_ref());
        let offline = self.access.as_ref().is_some_and(Access::local);
        let personal_actions = !self.route.personal() || self.personal_ready;
        let changes: Vec<_> = if matches!(self.route, Route::Playlists | Route::Offline) {
            self.page
                .rows
                .iter()
                .enumerate()
                .filter_map(|(i, row)| {
                    let (old, new) = (prev.row(row), next.row(row));
                    let actions = (old.is_some() != new.is_some()).then(|| {
                        let pinned = new.is_some();
                        (
                            row_actions(&self.page, row, offline, personal_actions, pinned),
                            pinned,
                        )
                    });
                    let copy = pin_row_copy(&self.route, row, new);
                    (actions.is_some() || pin_row_copy(&self.route, row, old) != copy)
                        .then_some((i, copy, actions))
                })
                .collect()
        } else {
            Vec::new()
        };
        let (old, new) = (prev.page(&self.route), next.page(&self.route));
        let detail_actions = (old.is_some() != new.is_some()).then(|| {
            playlist_detail_actions(
                &self.route,
                &self.page,
                new.is_some(),
                !offline && personal_actions,
            )
        });
        if !changes.is_empty() || detail_actions.is_some() {
            let generation = self.generation;
            let shown = self.shown.clone();
            self.scoped_ui(move |ui| {
                if shown.load(Ordering::SeqCst) != generation {
                    return;
                }
                if let Some(actions) = detail_actions {
                    ui.set_playlist_actions(actions);
                }
                let rows = ui.get_browse_rows();
                for (index, (subtitle, offline), actions) in changes {
                    if let Some(mut row) = rows.row_data(index) {
                        row.subtitle = subtitle;
                        row.playlist_list_offline = offline;
                        if let Some((actions, pinned)) = actions {
                            row.actions = strings(actions);
                            row.playlist_pinned = pinned;
                        }
                        rows.set_row_data(index, row);
                    }
                }
            });
        }
        let detail_state = |status: Option<&playlist_pins::Status>| {
            let state = playlist_detail_offline(&self.route, &self.page, status);
            (state.saved, state.feedback)
        };
        if detail_state(old) != detail_state(new) {
            self.render(false);
        }
        // A pin can change the badge and menu without changing the offline
        // projection (e.g. local audio from a different source remains). Patch
        // surviving rows before re-reading membership and playability.
        if refresh_local_list {
            if self.writing {
                self.saved_reload_due = true;
            } else {
                self.visit(Navigation::Untracked(self.route.clone()));
            }
        }
    }
    fn reset(&mut self, access: Option<Access>) {
        self.abort_search_bulk();
        self.cancel();
        self.saved_reload_due = false;
        self.personal_due = None;
        self.personal_ready = false;
        self.personal_view = None;
        self.pending = None;
        self.unconfirmed_orders.clear();
        self.edit_basis = None;
        self.row_editor = None;
        self.cache = Replica::default();
        self.catalog_visits.clear();
        self.catalog_restore = None;
        self.title_visits.clear();
        self.add_songs_visits.clear();
        self.add_songs_restore = false;
        self.add_songs_open = false;
        self.title_restore = None;
        self.continuation = CatalogContinuation::default();
        self.trail = Trail::default();
        self.page_keys = RandomState::new();
        self.route = INITIAL_ROUTE;
        self.page = Arc::default();
        self.message.clear();
        self.error = false;
        self.access = access;
        self.scoped_ui(|ui| {
            ui.set_query("".into());
            clear_form(&ui);
        });
        if self.access.is_some() {
            self.visit(Navigation::Untracked(INITIAL_ROUTE));
        } else {
            self.render(true);
        }
    }
    fn access_changed(&mut self, next: Option<Access>) {
        let same = self
            .access
            .as_ref()
            .zip(next.as_ref())
            .is_some_and(|(a, b)| a.same_browse_account(b));
        if !same {
            self.reset(next);
            return;
        }
        let old = self.access.replace(next.unwrap()).unwrap();
        let a = self.access.as_ref().unwrap();
        let offline = a.offline;
        if old.offline != offline {
            for visit in self.catalog_visits.values_mut() {
                visit.retire_snapshot();
            }
            if self.unpinning {
                // Durable unpin needs the same local account grant, not the
                // previous online connection. Finish before refreshing Saved.
                self.personal_due = None;
                self.personal_view = None;
                self.personal_ready = false;
                self.render(true);
                return;
            }
            let interrupted = self.pending.take();
            self.cancel();
            self.personal_due = None;
            self.personal_view = None;
            self.edit_basis = None;
            self.scoped_ui(|ui| clear_form(&ui));
            // A source change is not navigation: retain the exact destination and Back trail.
            self.visit(Navigation::Untracked(self.route.clone()));
            if let Some(pending) = interrupted {
                self.message = if matches!(pending.effect, Effect::Move { .. }) {
                    "You went offline before the playlist order change was confirmed."
                } else {
                    "You went offline before the playlist change could be confirmed."
                }
                .into();
                self.error = true;
                self.render(false);
            }
        }
    }
    fn personal_write_error(&mut self, message: String) {
        self.personal_write_error_render(message, false);
    }

    fn personal_write_error_render(&mut self, message: String, models: bool) {
        self.pending = None;
        self.writing = false;
        self.busy = false;
        self.error = true;
        self.message = message.clone();
        self.scoped_ui(move |ui| ui.set_form_error(message.into()));
        self.render(models);
    }

    fn personal_mutation(&mut self, access: &Access, mutation: Mutation, op: &str) {
        let (before, draft_current) = access.personal.read(|status, replica| {
            let draft_current = !PLAYLIST_EDITS.contains(&op)
                || self.edit_basis.as_ref().is_some_and(|basis| {
                    if op == "rules" {
                        basis.rules.as_ref().is_some_and(|(generation, prior)| {
                            status.generation == *generation
                                && replica.watched_playlist() == Some(&basis.id)
                                && replica.editable_rules(&basis.id) == Some(prior)
                        })
                    } else {
                        replica.playlist().is_some_and(|detail| {
                            !detail.stale && detail.summary.id == basis.id && detail.summary.revision == basis.revision
                        })
                    }
                });
            (status, draft_current)
        });
        if access.offline
            || access.session.is_offline()
            || before.epoch != access.epoch
            || before.link != Link::Ready
        {
            self.personal_write_error("You're not connected to your account library.".into());
            return;
        }
        if !draft_current {
            self.scoped_ui(|ui| ui.set_form_error(STALE_PLAYLIST_FORM.into()));
            return;
        }
        let command = if op == "rules" {
            let Some((_, prior)) = self.edit_basis.as_ref().and_then(|basis| basis.rules.clone()) else { return; };
            Command::EditPlaylistRules { observed: before, prior, command: mutation.command }
        } else {
            Command::Personal { epoch: access.epoch, generation: before.generation, command: mutation.command }
        };
        if self.engine.try_send(command).is_err() {
            self.personal_write_error("The change was not sent. Please try again.".into());
            return;
        }
        self.personal_sent(before, mutation.effect);
    }

    fn playlist_order_drop(&mut self, access: &Access, scope: &str, grabbed: &str, after: &str) {
        let Some(published) = self.playlist_order.as_ref().filter(|p| p.key == scope) else {
            return;
        };
        if !self.personal_ready || self.writing || access.offline {
            return;
        }
        // Copy the stamp before locking the replica. Engine checks it again.
        let network_mode = *access.session.network_changes().borrow();
        let sent = access.personal.read(|status, replica| {
            // The watch notification can lag behind the authoritative replica. Validate
            // both occurrences here; Engine rechecks the observed stamp at socket
            // admission. Go has no expected-revision field for server-side CAS.
            let detail = replica.playlist()?;
            if self.order_timeout_blocks(status, &detail.summary.id, detail.summary.revision) {
                return None;
            }
            let order = actions::PlaylistOrder::current(&self.route, &self.page, detail)?;
            let Mutation { command, effect } = order.movement(grabbed, after)?;
            if Some(published.basis)
                != PlaylistOrderBasis::current(access.epoch, self.generation, network_mode, status)
                || self.epoch.load(Ordering::SeqCst) != access.epoch
            {
                return None;
            }
            Some((
                status,
                effect,
                self.engine
                    .try_send(Command::ReorderPlaylist {
                        observed: status,
                        mode_stamp: network_mode,
                        command,
                    })
                    .is_ok(),
            ))
        });
        match sent {
            Some((before, effect, true)) => self.personal_sent(before, effect),
            Some((_, _, false)) => {
                self.personal_write_error("The playlist order change was not sent.".into())
            }
            None => {} // A stale, invalid, or same-slot drop makes no change.
        }
    }

    fn personal_sent(&mut self, before: Status, effect: Effect) {
        self.pending = Some(Pending {
            effect,
            before,
            due: tokio::time::Instant::now() + PERSONAL_WRITE_TIMEOUT,
        });
        self.writing = true;
        self.busy = true;
        self.error = false;
        self.message = "Saving changes…".into();
        self.scoped_ui(|ui| ui.set_form_error("".into()));
        self.render(false);
    }

    fn personal_changed(&mut self, visiting: bool) {
        if !self.route.personal() {
            if matches!(self.route, Route::Search(_)) {
                self.render(false);
            }
            return;
        }
        let Some(a) = self.access.clone().filter(Access::usable) else {
            self.render(visiting);
            return;
        };
        if a.local() {
            // The history overlay can change as a local visit qualifies.
            if self.route == Route::History {
                self.visit(Navigation::Untracked(Route::History));
            }
            return;
        }
        let route = self.route.clone();
        let crate::personal_bridge::BrowseView { status, page, live, detail_live, detail_nonready, unavailable, confirmed } =
            a.personal.browse_view(a.epoch, &route, self.pending.as_ref().map(|p| (p.before, &p.effect)));
        let prior = self.personal_view.replace(status);
        let online = !a.offline && !a.session.is_offline();
        let prior_ready = self.personal_ready;
        self.personal_ready = online
            && status.epoch == a.epoch
            && status.link == Link::Ready
            && live
            && (!matches!(route, Route::Playlist(_)) || detail_live);
        let models_changed = if let Some(next) = page {
            if *self.page == next {
                false
            } else {
                self.page = Arc::new(next);
                true
            }
        } else if unavailable && self.page.playlist.is_some() {
            self.page = Arc::default();
            true
        } else {
            false
        };
        if self.personal_ready
            && matches!(route, Route::Playlist(_))
            && (visiting || models_changed || !prior_ready)
        {
            self.cache.put(route.clone(), self.page.clone());
        }
        let render_models = visiting || models_changed || prior_ready != self.personal_ready;
        if let Some(pending) = self.pending.as_ref() {
            let before = pending.before;
            if !online
                || status.epoch != before.epoch
                || status.generation != before.generation
                || status.interruption != before.interruption
                || status.link != Link::Ready
            {
                let message = if matches!(&pending.effect, Effect::Move { .. }) {
                    "The connection was interrupted before the playlist order change was confirmed."
                } else {
                    "The connection was interrupted. The change was not confirmed; check the current library before retrying."
                };
                self.personal_write_error_render(message.into(), render_models);
                return;
            }
            if status.refused != before.refused {
                let message = if matches!(&pending.effect, Effect::Move { .. }) {
                    "The playlist order change was not sent."
                } else {
                    "The connection could not take the change. Check the current library before retrying."
                };
                self.personal_write_error_render(message.into(), render_models);
                return;
            }
            if let Some(observed) = confirmed {
                let ordering = matches!(pending.effect, Effect::Move { .. });
                self.pending = None;
                self.writing = false;
                self.busy = false;
                self.edit_basis = None;
                if !ordering { self.scoped_ui(clear_form_owned); }
                if let Some(next) = observed {
                    self.trail.landed(&next);
                    self.visit(Navigation::Untracked(next));
                }
                self.error = false;
                self.message.clear();
                self.render(true);
                return;
            }
            self.render(render_models);
            return;
        }
        // Do not overwrite a local download's separate progress or completion.
        if self.writing || (!visiting && self.personal_ready && self.error && prior_ready) {
            self.render(render_models);
            return;
        }
        let refused = prior.is_some_and(|old| {
            old.generation == status.generation && old.refused != status.refused
        });
        let transitioned = prior.is_none_or(|old| {
            old.link != status.link
                || old.generation != status.generation
                || old.interruption != status.interruption
        }) || prior_ready;
        let waiting_for_editor = self.row_editor.as_ref().is_some_and(|editor| !editor.ready(&self.page, self.personal_ready));
        let waiting = online
            && matches!(status.link, Link::Waiting | Link::Ready)
            && !unavailable
            && ((!self.personal_ready && !detail_nonready) || waiting_for_editor);
        // Time out a stalled part, not a long playlist making progress.
        let progressing = matches!(route, Route::Playlist(_))
            && prior.is_some_and(|old| old.playlist_progress != status.playlist_progress);
        if !waiting || refused || (visiting && self.error) {
            self.personal_due = None;
        } else if transitioned || visiting || progressing {
            self.personal_due = Some(tokio::time::Instant::now() + PERSONAL_READ_TIMEOUT);
        }
        self.busy = self.personal_due.is_some() && self.page.rows.is_empty();
        if !(visiting && self.error) {
            let (message, error) = if unavailable {
                (
                    "This playlist could not be opened. It may have been removed.",
                    true,
                )
            } else if refused && !self.personal_ready {
                (
                    "The playlist couldn't refresh.",
                    true,
                )
            } else if self.personal_ready || detail_nonready {
                ("", false) // Allow the accepted evaluation status to describe the retained rows.
            } else if status.link == Link::Gap {
                (
                    "The account update was interrupted. Waiting for a fresh snapshot.",
                    true,
                )
            } else if status.link == Link::Disconnected {
                (
                    "The account library is offline.",
                    true,
                )
            } else if self.personal_due.is_some() {
                ("Waiting for the account library…", false)
            } else {
                (
                    "The account library couldn't refresh.",
                    true,
                )
            };
            self.message = message.into();
            self.error = error;
        }
        self.render(render_models);
        self.finish_row_editor();
        if unavailable || self.personal_due.is_none() {
            self.row_editor = None;
        }
    }

    fn personal_timeout(&mut self) {
        let now = tokio::time::Instant::now();
        if let Some(a) = self.access.clone()
            && a.personal
                .quick_view(a.epoch, |view| view.due.is_some_and(|due| due <= now))
        {
            let request = a.personal.quick_tick(a.epoch);
            self.quick_request(&a, request);
            if matches!(self.route, Route::Search(_)) {
                self.render(false);
            }
        }
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.due <= now)
        {
            // The timer may win a select against a ready personal watch. Apply the
            // latest observation before treating the write as unconfirmed.
            self.personal_changed(false);
            if let Some(pending) = self.pending.as_ref().filter(|pending| pending.due <= now) {
                if let Effect::Move { id, revision, .. } = &pending.effect {
                    let observed = self.personal_view.unwrap_or(pending.before);
                    let revision = self
                        .page
                        .playlist
                        .as_ref()
                        .filter(|playlist| playlist.id == *id)
                        .map_or(*revision, |playlist| playlist.revision.max(*revision));
                    self.unconfirmed_orders.insert(
                        id.clone(),
                        UnconfirmedOrder {
                            epoch: observed.epoch,
                            socket_generation: observed.generation,
                            revision,
                        },
                    );
                    // Read-only recovery: never resend the move. Refresh marks the
                    // old detail stale before attempting to queue a fresh read.
                    if let Some(a) = self.access.as_ref()
                        && let Some((generation, command)) = a.personal.refresh_playlist(a.epoch)
                        && !a.offline
                        && !a.session.is_offline()
                    {
                        let _ = self.engine.try_send(Command::Personal {
                            epoch: a.epoch,
                            generation,
                            command,
                        });
                    }
                    self.personal_write_error(
                        "The server did not confirm the playlist order change.".into(),
                    );
                    self.personal_changed(false);
                } else {
                    self.personal_write_error("The server did not confirm this change. Check the current library before retrying.".into());
                }
            }
        }
        if self.personal_due.is_some_and(|due| due <= now) {
            self.personal_due = None;
            self.row_editor = None;
            self.personal_changed(false);
        }
    }

    fn demand_catalog(&mut self, demand: CatalogDemand) {
        if demand.generation == self.generation
            && !demand.key.is_empty()
            && demand.key == self.catalog_request_key()
            && !self.writing
            && let Some(access) = self
                .access
                .clone()
                .filter(|a| a.epoch == demand.epoch && a.usable())
        {
            self.continue_catalog(access);
        }
    }

    fn continue_catalog(&mut self, access: Access) {
        self.serial += 1;
        let serial = self.serial;
        let route = self.route.clone();
        let prior = self.page.clone();
        let tx = self.results.clone();
        self.busy = true;
        self.silent_read = true;
        self.continuation.loading = true;
        let network = access.session.network_changes();
        let mode = *network.borrow();
        self.task = Some(tokio::spawn(async move {
            let mut result = None;
            for attempt in 0..=READ_RETRIES {
                if mode & 1 != 0 || *network.borrow() != mode || !access.usable() {
                    break;
                }
                let reply = pages::load_next(&access, &route, &prior).await;
                if *network.borrow() != mode {
                    break;
                }
                if !read_retry(attempt, &reply) {
                    result = Some(reply);
                    break;
                }
                tokio::time::sleep(read_backoff(attempt)).await;
            }
            drop(prior);
            let _ = tx
                .send(Delivery {
                    serial,
                    result: Update::CatalogAppend(mode, result),
                })
                .await;
        }));
        self.render(false);
    }

    fn abort_search_bulk(&self) {
        if matches!(self.route, Route::Search(_))
            && let Some(a) = self.access.as_ref()
        {
            a.personal.quick_abort_bulk(a.epoch);
        }
    }

    fn visit(&mut self, navigation: Navigation) {
        self.visit_from(navigation, None);
    }

    fn visit_from(&mut self, navigation: Navigation, title_origin: Option<TitleOrigin>) {
        if self.writing {
            return;
        }
        let back = matches!(navigation, Navigation::Back);
        let follow = matches!(navigation, Navigation::Follow(_));
        let add_songs = matches!(navigation, Navigation::AddSongs);
        let previous_top = self.trail.top_id();
        let search_title = matches!((&self.route, &navigation), (Route::Search(_), Navigation::Follow(Route::Album(id)))
        if self.catalog_position.as_ref().is_some_and(|position| {
            position.part == "album" && position.credit == *id
                && matches!(position.row_key, Some(RowKey::Track(_)))
        }));
        let Some(next) = navigation.resolve(
            &self.route,
            &mut self.trail,
            self.task.is_some(),
            self.error,
            title_origin.is_some() || search_title,
        ) else {
            return;
        };
        self.row_editor = None;
        let changed = next != self.route;
        if changed {
            // A new query is a new source scope even if the destination stays selected.
            self.abort_search_bulk();
        }
        if changed
            && self.route.catalog_continuity()
            && !self.page.local_audio
            && !self.error
            && !self.page.kind.is_empty()
        {
            self.cache.put(self.route.clone(), self.page.clone());
        }
        self.continuation.loading = false;
        if let Some(entry) = self
            .trail
            .top_id()
            .filter(|id| previous_top.is_none_or(|previous| *id > previous))
        {
            if let Some(origin) = title_origin {
                self.title_visits.insert(entry, origin);
            }
            if add_songs {
                self.add_songs_visits.insert(entry);
            }
            if self.route.catalog_continuity()
                && !self.page.rows.is_empty()
                && let Some(position) = self.catalog_position.take()
            {
                self.catalog_visits.insert(
                    entry,
                    CatalogVisit {
                        page: Some(self.page.clone()),
                        position,
                        continuation: std::mem::take(&mut self.continuation),
                    },
                );
            }
        }
        let returning =
            back || (follow && previous_top.is_some_and(|id| !self.trail.contains_id(id)));
        let returned = if returning {
            previous_top.and_then(|id| self.catalog_visits.remove(&id))
        } else {
            None
        };
        if returning {
            self.title_restore = previous_top.and_then(|id| self.title_visits.remove(&id));
            self.add_songs_restore =
                previous_top.is_some_and(|id| self.add_songs_visits.remove(&id));
        } else if changed {
            self.title_restore = None;
            self.add_songs_restore = false;
        }
        self.add_songs_open = add_songs;
        self.add_songs_visits
            .retain(|id| self.trail.contains_id(*id));
        self.catalog_visits
            .retain(|id, _| self.trail.contains_id(*id));
        self.title_visits
            .retain(|id, _| self.trail.contains_id(*id));
        self.continuation = CatalogContinuation::default();
        if changed {
            self.catalog_restore = None;
        }
        let restored_snapshot = returned.and_then(|visit| {
            self.catalog_restore = Some(visit.position);
            visit.page.map(|page| (page, visit.continuation))
        });
        let retry_personal = !changed && self.error && matches!(next, Route::Playlist(_));
        if (back || add_songs)
            && let Route::Search(q) = &next
        {
            let q = q.clone();
            self.scoped_ui(move |ui| ui.set_query(q.into()));
        } else if changed
            && matches!(self.route, Route::Search(_))
            && !matches!(next, Route::Search(_))
        {
            self.scoped_ui(|ui| ui.set_query("".into()));
        }
        self.cancel();
        self.message.clear();
        self.error = false;
        if changed {
            self.edit_basis = None;
            self.scoped_ui(|ui| clear_form(&ui));
        }
        if let Some(a) = self.access.clone().filter(Access::usable) {
            let wanted = match &next {
                Route::Playlist(id) => Some(id.clone()),
                _ => None,
            };
            if retry_personal {
                a.personal.watch_playlist(a.epoch, None);
            }
            if let Some((generation, command)) = a.personal.quick_watch(
                a.epoch,
                matches!(next, Route::Search(_)) && !a.local(),
                wanted,
            ) && !a.offline
                && !a.session.is_offline()
                && self
                    .engine
                    .try_send(Command::Personal {
                        epoch: a.epoch,
                        generation,
                        command,
                    })
                    .is_err()
            {
                self.error = true;
                self.message = "The playlist couldn't refresh.".into();
            }
        }
        self.personal_due = None;
        self.personal_ready = false;
        self.personal_view = None;
        if let Some(a) = self.access.clone().filter(Access::local)
            && !matches!(next, Route::Now | Route::Queue | Route::Appearance)
        {
            self.visit_local(a, next);
            return;
        }
        if next.personal() {
            let previous = std::mem::replace(&mut self.route, next);
            if previous != self.route {
                self.page = Arc::default();
            }
            self.personal_changed(true);
            return;
        }
        if let Some((page, continuation)) = restored_snapshot
            && !page.local_audio
            && self.access.as_ref().is_some_and(|a| !a.local())
        {
            self.route = next;
            self.page = page;
            self.continuation = continuation;
            self.render(true);
            return;
        }
        let cached = self.cache.get(&next);
        let have = cached.is_some();
        let fresh = cached.as_ref().is_some_and(|(_, f)| *f);
        let prior_page =
            std::mem::replace(&mut self.page, cached.map_or_else(Arc::default, |(p, _)| p));
        self.route = next.clone();
        let Some(a) = self.access.clone().filter(Access::usable) else {
            self.render(true);
            return;
        };
        if next == Route::Appearance {
            self.scoped_ui(|ui| ui.set_music_storage("Checking downloads…".into()));
            let tx = self.results.clone();
            let serial = self.serial;
            self.task = Some(tokio::spawn(async move {
                let result = Update::Storage(saved::storage_summary(&a).await);
                let _ = tx.send(Delivery { serial, result }).await;
            }));
            self.render(true);
            return;
        }
        if matches!(next, Route::Now | Route::Queue)
            || matches!(&next, Route::Search(q) if q.is_empty())
        {
            self.render(true);
            return;
        }
        if fresh {
            self.render(true);
            return;
        }
        let tx = self.results.clone();
        let serial = self.serial;
        self.busy = true;
        self.silent_read = have;
        self.render(changed || !have || !Arc::ptr_eq(&prior_page, &self.page));
        let known = if saved_route(&next) { self.cache.catalog_pages() } else { Vec::new() };
        let previous = self.page.clone();
        self.task = Some(tokio::spawn(async move {
            let history = saved_route(&next).then(|| retained_history(&a)).flatten();
            let known: Vec<_> = known.iter().map(|(route, page)| (route, page.as_ref())).collect();
            if matches!(next, Route::Search(_)) {
                tokio::time::sleep(SEARCH_DELAY).await;
            }
            for attempt in 0..=READ_RETRIES {
                if !a.usable() {
                    return;
                }
                let result = match &next {
                    Route::Offline => downloads_page(&a).await,
                    Route::SavedCollection(id) => playlist_browser::detail(&a, id).await,
                    _ => pages::load(&a, &next).await,
                }
                .map(|page| if saved_route(&next) {
                    project_local_page(&next, page, &known, history.as_ref(), Some(&previous))
                } else {
                    page
                });
                let retry = read_retry(attempt, &result);
                if retry {
                    let result = match result {
                        Ok(p) => Update::Read(Ok(p), true),
                        Err(_) => Update::ReadGap,
                    };
                    if tx.send(Delivery { serial, result }).await.is_err() {
                        return;
                    }
                    tokio::time::sleep(read_backoff(attempt)).await;
                } else {
                    let _ = tx
                        .send(Delivery {
                            serial,
                            result: Update::Read(result, false),
                        })
                        .await;
                    return;
                }
            }
        }));
    }
    fn visit_local(&mut self, access: Access, next: Route) {
        let serial = self.serial;
        let mode = *access.session.network_changes().borrow();
        let current = self.route == next;
        let previous = current.then(|| self.page.clone());
        let mut known = self.cache.catalog_pages();
        if next.catalog_continuity()
            && let Some(page) = &previous
            && !page.local_audio
        {
            known.push((next.clone(), page.clone()));
        }
        let history = retained_history(&access);
        self.route = next.clone();
        let retain_view = current
            && !self.page.kind.is_empty()
            && !matches!(&next, Route::Search(query) if query.trim().is_empty());
        let personal = matches!(next, Route::Playlists | Route::Playlist(_) | Route::History | Route::Picker(_));
        let retire_admissions = retain_view && personal && !self.page.local_audio;
        if !retain_view {
            let mut page = access.personal.cached_page(access.epoch, &next).unwrap_or_default();
            reset_local_admissions(&mut page);
            self.page = Arc::new(page);
        } else if retire_admissions {
            reset_local_admissions(Arc::make_mut(&mut self.page));
        }
        // The old source is already fenced. Keep its last painted view only
        // until the local projection is ready, then publish once rather than
        // collapsing the content extent and losing scroll/focus in between.
        if matches!(&next, Route::Search(query) if query.trim().is_empty())
            || matches!(next, Route::Picker(_))
        {
            self.render(true);
            return;
        }
        let tx = self.results.clone();
        self.busy = true;
        self.silent_read = retain_view;
        self.render(!retain_view || retire_admissions);
        self.task = Some(tokio::spawn(async move {
            if matches!(next, Route::Search(_)) && !current {
                tokio::time::sleep(SEARCH_DELAY).await;
            }
            let known = known
                .iter()
                .map(|(route, page)| (route, page.as_ref()))
                .collect::<Vec<_>>();
            let result = match &next {
                Route::Playlists | Route::Playlist(_) | Route::History => {
                    cached_local_page(&access, &next).await
                }
                Route::Offline => downloads_page(&access).await,
                Route::SavedCollection(id) => playlist_browser::detail(&access, id).await,
                Route::Search(_) | Route::Album(_) | Route::Artist(_) => {
                    saved::inventory(&access).await.map(|records| {
                        catalog_page(&next, &records, &known, history.as_ref(), false)
                    })
                }
                _ => Ok(Page::default()),
            }
            .map(|page| project_local_page(&next, page, &known, history.as_ref(), previous.as_deref()));
            let _ = tx
                .send(Delivery {
                    serial,
                    result: Update::LocalRead(mode, result),
                })
                .await;
        }));
    }

    fn finish_row_editor(&mut self) {
        if self.row_editor.as_ref().is_some_and(|editor| editor.ready(&self.page, self.personal_ready))
            && let Some(editor) = self.row_editor.take()
        {
            if editor.matches(&self.route, &self.page) {
                self.open_playlist_editor(editor.op);
            } else {
                self.message = "This playlist changed. Review it before editing.".into();
                self.error = true;
                self.render(false);
            }
        }
    }

    fn open_playlist_editor(&mut self, op: &str) {
        let Route::Playlist(id) = &self.route else {
            return;
        };
        let Some(playlist) = self
            .page
            .playlist
            .as_ref()
            .filter(|m| &m.id == id && can_edit_playlist(m, op))
        else {
            return;
        };
        let rules = if op == "rules" {
            if !self.page.live_rules { return; }
            let Some(access) = self.access.as_ref().filter(|a| !a.local()) else { return; };
            let Some(rules) = access.personal.read(|status, replica| {
                let basis = replica.editable_rules(id)?;
                (status.epoch == access.epoch && status.link == Link::Ready
                    && replica.watched_playlist() == Some(id)
                    && basis.revision == playlist.revision && self.page.definition.as_ref() == Some(&basis.definition))
                    .then(|| (status.generation, basis.clone()))
            }) else { return; };
            Some(rules)
        } else {
            None
        };
        let draft = if op == "delete" {
            Ok(actions::Draft::default())
        } else {
            actions::prepare(&self.route, &self.page, op)
        };
        match draft {
            Ok(draft) => {
                self.edit_basis = Some(EditBasis { op: op.to_owned(), id: playlist.id.clone(), revision: playlist.revision, rules });
                let key = page_key(&self.route, Some(&self.page_keys));
                let generation = self.generation.to_string();
                let mode = op.to_owned();
                self.scoped_ui(move |ui| {
                    if ui.get_browse_model_key() != generation.as_str() {
                        return;
                    }
                    ui.invoke_show_playlist_editor(
                        key.into(),
                        mode.into(),
                        draft.name.into(),
                        draft.description.into(),
                        draft.definition.into(),
                        actions::RULES_HELP.into(),
                    );
                });
            }
            Err(e) => {
                self.message = readable(&e);
                self.error = true;
                self.render(false);
            }
        }
    }

    fn playback(&mut self, c: Command) {
        if let Some(a) = &self.access
            && self.engine.try_send(c.scoped(a.epoch)).is_err()
        {
            self.message = "That action was not sent. Please try again.".into();
            self.render(false);
        }
    }
    fn input(&mut self, mut i: Input) {
        self.catalog_position = if i.generation == self.generation {
            i.catalog_position.take().map(|mut position| {
                position.capture_keys(&self.page.rows);
                position
            })
        } else {
            None
        };
        self.handle_input(i);
        self.catalog_position = None;
    }

    fn handle_input(&mut self, i: Input) {
        let Some(a) = self
            .access
            .clone()
            .filter(|a| a.epoch == i.epoch && a.usable())
        else {
            return;
        };
        if self.writing {
            return;
        }
        let offline = a.local();
        let (op, idx, val) = match i.intent {
            Intent::Navigate(s) => {
                if let Some(navigation) = Navigation::named(&s) {
                    self.visit(navigation);
                }
                return;
            }
            Intent::Search(q) => {
                if q.len() <= 1000 {
                    self.visit(Navigation::Follow(Route::Search(q.trim().into())));
                }
                return;
            }
            Intent::ChooseQuickTarget { scope, id }
                if i.generation == self.generation && matches!(self.route, Route::Search(_)) =>
            {
                let mode = *a.session.network_changes().borrow();
                if (id.is_empty() || !offline)
                    && a.personal
                        .quick_view(a.epoch, |view| view.scope(mode) == scope)
                {
                    let request = a
                        .personal
                        .quick_select(a.epoch, (!id.is_empty()).then_some(id.as_str()));
                    self.quick_request(&a, request);
                    self.render(false);
                }
                return;
            }
            Intent::ToggleQuickSkip { scope }
                if i.generation == self.generation
                    && matches!(self.route, Route::Search(_))
                    && !offline =>
            {
                let mode = *a.session.network_changes().borrow();
                if let Some(skip) = a.personal.quick_view(a.epoch, |view| {
                    (view.scope(mode) == scope).then_some(!view.skip)
                }) {
                    a.personal.quick_skip(a.epoch, skip);
                    self.render(false);
                }
                return;
            }
            Intent::CancelQuickBulk { scope }
                if i.generation == self.generation && matches!(self.route, Route::Search(_)) =>
            {
                let mode = *a.session.network_changes().borrow();
                a.personal.quick_abort_bulk_scoped(a.epoch, mode, &scope);
                self.render(false);
                return;
            }
            Intent::QuickAdd {
                scope,
                target,
                kind,
                source,
                index,
            } if i.generation == self.generation && !offline => {
                if matches!(self.route, Route::Search(_))
                    && self.page.rows.get(index).is_some_and(|row| {
                        quick_add_kind(row) == Some(kind.as_str()) && row.id == source
                    })
                {
                    let mode = *a.session.network_changes().borrow();
                    if kind == "track" {
                        if let Some(ticket) = a
                            .personal
                            .quick_stage_scoped(a.epoch, mode, &scope, &target, &source)
                            && self
                                .engine
                                .try_send(Command::QuickAdd(ticket.clone()))
                                .is_err()
                        {
                            a.personal.quick_cancel(&ticket);
                        }
                    } else {
                        crate::search_bulk::start(
                            &a,
                            self.epoch.clone(),
                            mode,
                            &scope,
                            &target,
                            &kind,
                            &source,
                        );
                    }
                    self.render(false);
                }
                return;
            }
            Intent::Row(op, idx)
                if i.generation == self.generation && idx < self.page.rows.len() =>
            {
                (op, Some(idx), String::new())
            }
            Intent::Page(op, val) if i.generation == self.generation => (op, None, val),
            Intent::PlaylistOrderDrop {
                scope,
                grabbed,
                after,
            } if i.generation == self.generation => {
                self.playlist_order_drop(&a, &scope, &grabbed, &after);
                return;
            }
            _ => return,
        };
        self.row_editor = None;
        let row = idx.and_then(|i| self.page.rows.get(i)).cloned();
        let route = self.route.clone();
        let page = self.page.clone();
        // Work offline closes the session gate before the access watch arrives.
        if op == "add-songs" {
            if !offline
                && self.personal_ready
                && idx.is_none()
                && let Route::Playlist(id) = &route
                && add_songs_eligible(&route, &page)
            {
                let request = a.personal.quick_select(a.epoch, Some(id));
                self.quick_request(&a, request);
                if a.personal.quick_view(a.epoch, |view| {
                    view.target.is_some_and(|target| target.id == *id)
                }) {
                    self.visit(Navigation::AddSongs);
                }
            }
            return;
        }
        if let Some(mode) = op.strip_prefix("edit-") {
            if !offline && PLAYLIST_EDITS.contains(&mode) {
                match (&route, row.as_ref()) {
                    (Route::Playlists, Some(row)) if self.personal_ready => {
                        if let Some(editor) = RowEditor::select(row, mode) {
                            self.visit(Navigation::Follow(Route::Playlist(editor.id.clone())));
                            if editor.op == "rules" && !editor.ready(&self.page, self.personal_ready)
                                && self.personal_due.is_none()
                            {
                                self.personal_due = Some(tokio::time::Instant::now() + PERSONAL_READ_TIMEOUT);
                            }
                            if editor.ready(&self.page, self.personal_ready) || self.personal_due.is_some() {
                                self.row_editor = Some(editor);
                                self.finish_row_editor();
                            }
                        }
                    }
                    (Route::Playlist(_), None)
                        if self.personal_ready || mode == "rules" && page.live_rules => self.open_playlist_editor(mode),
                    _ => {}
                }
            }
            return;
        }
        if op.starts_with("cancel-form") {
            self.edit_basis = None;
            return;
        }
        if let Some(mode) = op.strip_prefix("prepare-") {
            self.edit_basis = None;
            if !matches!(mode, "new-manual" | "new-rules") {
                return;
            }
            let draft = actions::prepare(&route, &page, mode);
            let mode = mode.to_owned();
            self.scoped_ui(move |ui| {
                if ui.get_form_mode() != mode.as_str() {
                    return;
                }
                match draft {
                    Ok(d) => {
                        ui.set_form_error("".into());
                        ui.set_form_name(d.name.into());
                        ui.set_form_description(d.description.into());
                        ui.set_form_value(d.definition.into());
                        if matches!(mode.as_str(), "rules" | "new-rules") {
                            ui.set_form_help(actions::RULES_HELP.into());
                        }
                    }
                    Err(e) => ui.set_form_error(readable(&e).into()),
                }
            });
            return;
        }
        if PLAYLIST_EDITS.contains(&op.as_str())
            && (idx.is_some() || !draft_matches(&self.edit_basis, &page, &op))
        {
            self.scoped_ui(|ui| ui.set_form_error(STALE_PLAYLIST_FORM.into()));
            return;
        }
        let pin = if matches!(op.as_str(), "pin-playlist" | "unpin-playlist") {
            let pins = PinDisplay::new(&self.pin_view, Some(&a));
            if matches!(route, Route::SavedCollection(_)) && idx.is_none() {
                page.saved_pin.clone()
            } else {
                match (&route, row.as_ref()) {
                    (Route::Playlists | Route::Offline, Some(row)) => pins.row(row),
                    (Route::Playlist(_), None) => pins.page(&route),
                    _ => None,
                }
                .map(|pin| pin.target.clone())
            }
        } else {
            None
        };
        let unpin = op == "unpin-playlist" && pin.is_some();
        if op == "unpin-playlist" && !unpin {
            if idx.is_none() {
                self.scoped_ui(|ui| ui.set_form_error("This playlist is no longer saved.".into()));
            }
            return;
        }
        if offline && !page.local_audio && !saved_route(&route) {
            return; // The previous online projection may still be painted, but is inert.
        }
        let saved_action = saved_route(&route) && matches!(op.as_str(), "play" | "queue");
        let local_action = offline && (saved_action || page.local_audio && matches!(op.as_str(), "play" | "queue"));
        if route.personal()
            && !self.personal_ready
            && !(unpin
                || op == "rules" && page.live_rules
                || local_action
                || matches!(op.as_str(), "album" | "artist")
                || op == "open" && !matches!(route, Route::Picker(_)))
        {
            // A now-unavailable control may have left a click in flight. This
            // is neither an account mutation nor a reason to add offline copy.
            return;
        }
        if let Some(kind) = op.strip_prefix("context-") {
            if let Some(target) = context_route(kind, &val) {
                self.visit(Navigation::Follow(target));
            }
            return;
        }
        if matches!(op.as_str(), "album" | "artist") {
            if let Some(target) = pages::music_route(&op, &val).filter(|target| *target != route) {
                let origin = i.title_origin.filter(|origin| {
                    origin.source != TitleSource::Playlist
                        || origin.playlist_index(&page.rows, &route).is_some()
                });
                self.visit_from(Navigation::Follow(target), origin);
            }
            return;
        }
        let saved_page = saved_route(&route);
        let entity_details = matches!(op.as_str(), "open-album" | "open-artist");
        match (&route, row.as_ref()) {
            (Route::Search(_), Some(r)) if matches!(r.kind.as_str(), "album" | "artist") => {
                if !search_entity_actions(&page, r, offline)
                    .is_some_and(|actions| op == "open" || actions.contains(&op.as_str()))
                {
                    return;
                }
            }
            _ if entity_details => return,
            _ => {}
        }
        if (op == "open" || entity_details) && !matches!(route, Route::Picker(_)) {
            if let Some(target) = row.as_ref().and_then(|r| row_open_target(&route, r)) {
                self.visit(Navigation::Follow(target.clone()));
            }
            return;
        }
        if op == "add" && !matches!(route, Route::Picker(_)) {
            if let Some(r) = row
                && !offline
            {
                self.visit(Navigation::Follow(Route::Picker(r.id)));
            }
            return;
        }
        if matches!(op.as_str(), "play" | "queue") && row.as_ref().is_some_and(|r| !r.playable) {
            self.message = if offline { "This track isn't downloaded." } else { "This track cannot be played." }.into();
            self.render(false);
            return;
        }
        if matches!(op.as_str(), "play" | "queue") && !local_action {
            if offline {
                return;
            }
            let selection = match row {
                Some(r) => r.playback.or_else(|| (saved_action && r.kind == "track").then(|| pages::track_selection(&r.id, None))),
                None => page.playback.clone(),
            };
            if let Some(s) = selection {
                let selection = s.into();
                self.playback(Command::Playback(if op == "play" {
                    player::Action::Play {
                        selection,
                        shuffle: false,
                    }
                } else {
                    player::Action::Enqueue { selection }
                }));
            }
            return;
        }
        if op == "pin-playlist" || unpin {
            let wanted = !unpin;
            let target = if wanted && !offline && pin.is_none() {
                playlist_browser::target(&route, &page, row.as_ref())
                    .filter(|target| a.personal.can_download_playlist(a.epoch, &target.id))
            } else if unpin {
                pin
            } else {
                None
            };
            let Some(target) = target else { return };
            let pins = self.pins.clone();
            self.cancel();
            self.writing = true;
            self.unpinning = unpin;
            self.error = false;
            self.message.clear();
            self.render(false);
            let (tx, serial) = (self.results.clone(), self.serial);
            self.task = Some(tokio::spawn(async move {
                let saved = !wanted || a.personal.save_playlist_details(a.epoch, &target.id).await;
                let result = Update::PinChanged(if saved {
                    pins.set(&a, target, wanted).await
                } else {
                    Err(unavailable("Playlist details could not be saved on this device."))
                });
                let _ = tx.send(Delivery { serial, result }).await;
            }));
            return;
        }
        let remove_saved = op == "remove" && saved_page;
        if remove_saved && idx.is_none() && page.saved_pin.is_some() {
            self.scoped_ui(|ui| {
                ui.set_form_error(
                    "This saved collection changed. Close the confirmation and open it again."
                        .into(),
                )
            });
            return;
        }
        let save = op == "save";
        if offline && !local_action && !remove_saved {
            return;
        }
        if val.len() > 48 * 1024 {
            self.message = "The playlist description or rules are too long.".into();
            self.render(false);
            return;
        }
        if !local_action && !save && !remove_saved {
            let op = if op == "open" && matches!(route, Route::Picker(_)) {
                "add"
            } else {
                &op
            };
            match actions::mutation(&route, &page, op, idx, &val) {
                Ok(mutation) => self.personal_mutation(&a, mutation, op),
                Err(e) => self.personal_write_error(readable(&e)),
            }
            return;
        }
        self.cancel();
        self.writing = true;
        self.busy = true;
        self.error = false;
        self.silent_read = local_action;
        self.message = if local_action {
            ""
        } else if save {
            "Saving audio on this device…"
        } else {
            "Applying change…"
        }
        .into();
        self.render(false);
        let tx = self.results.clone();
        let serial = self.serial;
        let pins = self.pins.clone();
        let local_target = local_action
            .then(|| {
                row.as_ref()
                    .and_then(|r| row_open_target(&route, r))
                    .cloned()
            })
            .flatten();
        let mut known = self.cache.catalog_pages();
        if local_target.is_some() && route.catalog_continuity() && !page.local_audio {
            known.push((route.clone(), page.clone()));
        }
        self.task = Some(tokio::spawn(async move {
            let result = if local_action {
                Update::Local(
                    row.as_ref().map_or_else(String::new, |row| row.id.clone()),
                    local_playback(&a, &route, &page, idx, local_target, &known, op == "queue")
                        .await,
                )
            } else if save && row.is_none() {
                Update::CollectionSaved(crate::collections::save(&a, &route, &page).await)
            } else if save || remove_saved {
                if remove_saved
                    && let Route::SavedCollection(id) = &route
                    && row.is_none()
                {
                    Update::Changed(
                        // A catalog collection confirmation cannot remove a pin.
                        playlist_browser::remove_collection(&a, id)
                            .await
                            .map(|_| Some(Route::Offline)),
                    )
                } else {
                    let res = if let Some(r) = row.as_ref() {
                        if remove_saved && matches!(r.route, Some(Route::SavedCollection(_))) {
                            playlist_browser::remove(&a, &pins, &r.id).await
                        } else {
                            if save {
                                saved::save(&a, r).await
                            } else {
                                saved::remove(&a, r).await
                            }
                        }
                    } else {
                        Err(unavailable("Choose a track to save or remove."))
                    };
                    Update::Changed(res.map(|_| None))
                }
            } else {
                unreachable!("personal mutations are sent through the account socket")
            };
            let _ = tx.send(Delivery { serial, result }).await;
        }));
    }
    fn check_availability(&mut self) {
        if self.task.is_some() || self.writing { return; }
        let Some(access) = self.access.clone().filter(Access::usable) else { return; };
        let route = self.route.clone();
        if !(route == Route::Offline || access.local() && matches!(route, Route::Playlist(_) | Route::History)) { return; }
        let mode = *access.session.network_changes().borrow();
        let (serial, tx) = (self.serial, self.results.clone());
        let known = self.cache.catalog_pages();
        let previous = self.page.clone();
        self.task = Some(tokio::spawn(async move {
            let history = retained_history(&access);
            let known: Vec<_> = known.iter().map(|(route, page)| (route, page.as_ref())).collect();
            let page = if route == Route::Offline {
                downloads_page(&access).await
            } else {
                cached_local_page(&access, &route).await
            }.map(|page| project_local_page(&route, page, &known, history.as_ref(), Some(&previous)));
            let _ = tx.send(Delivery { serial, result: Update::Availability(mode, page) }).await;
        }));
    }

    fn verify_unavailable(&mut self, id: String) {
        let Some(access) = self.access.clone().filter(Access::usable) else { return; };
        let ids: HashSet<_> = self.page.rows.iter().filter(|r| r.local_recording.is_some())
            .filter(|r| id.is_empty() || r.id == id).map(|r| r.id.clone()).collect();
        if ids.is_empty() { self.check_availability(); return; }
        let mode = *access.session.network_changes().borrow();
        let (serial, tx) = (self.serial, self.results.clone());
        self.task = Some(tokio::spawn(async move {
            let missing = match crate::collections::browse_tracks(&access, ids.clone()).await {
                Ok(records) => ids.into_iter().filter(|id| !records.contains_key(id)).collect(),
                Err(_) => Vec::new(),
            };
            let _ = tx.send(Delivery { serial, result: Update::VerifyUnavailable(mode, missing) }).await;
        }));
    }

    fn result(&mut self, d: Delivery) {
        if d.serial != self.serial
            || !self
                .access
                .as_ref()
                .is_some_and(|a| a.epoch == self.epoch.load(Ordering::SeqCst))
        {
            return;
        }
        if let Update::LocalRead(mode, _) = &d.result
            && !self.access.as_ref().is_some_and(|a| {
                a.local() && a.usable() && *a.session.network_changes().borrow() == *mode
            })
        {
            return;
        }
        if let Update::Read(..) | Update::ReadGap = &d.result
            && !saved_route(&self.route)
            && self.access.as_ref().is_some_and(Access::local)
        {
            return;
        }
        if let Update::VerifyUnavailable(mode, ids) = d.result {
            self.task = None;
            if !ids.is_empty() && self.access.as_ref().is_some_and(|a| a.usable() && *a.session.network_changes().borrow() == mode) {
                let page = Arc::make_mut(&mut self.page);
                if self.route == Route::Offline {
                    page.rows.retain(|r| !ids.contains(&r.id));
                    page.total = Some(page.rows.len() as u64);
                } else {
                    for row in page.rows.iter_mut().filter(|r| ids.contains(&r.id)) {
                        row.local_recording = None;
                        row.playable = false;
                        row.detail = pages::join_labels(&row.detail, "Not downloaded");
                    }
                    page.local_collection_playable &= page.rows.iter().any(|r| r.playable);
                }
                self.render(true);
                self.check_availability();
            }
            return;
        }
        if let Update::Availability(mode, result) = d.result {
            self.task = None;
            if self.access.as_ref().is_some_and(|a| a.usable() && *a.session.network_changes().borrow() == mode)
                && let Ok(page) = result && *self.page != page
            {
                self.cache.saved_changed();
                self.page = Arc::new(page);
                self.render(true);
            }
            return;
        }
        if let Update::CatalogAppend(mode, result) = d.result {
            self.task = None;
            self.busy = false;
            self.continuation.loading = false;
            let online = self.access.as_ref().is_some_and(|a| {
                !a.offline
                    && !a.session.is_expired()
                    && *a.session.network_changes().borrow() == mode
            });
            let start = self.page.rows.len();
            let failure = online && result.is_some();
            if online
                && let Some(Ok(batch)) = result
                && batch.commit(Arc::make_mut(&mut self.page)).is_ok()
            {
                self.render_rows(false, Some(start));
            } else {
                // Never rebuild or reenter the observer on a failed or cancelled
                // continuation. The accepted snapshot and its controls survive.
                self.continuation.stopped = true;
                if failure {
                    self.continuation.status = "More results couldn’t be loaded.".into();
                }
                self.render(false);
            }
            return;
        }
        if !matches!(&d.result, Update::Read(_, true) | Update::ReadGap) {
            self.task = None;
        }
        self.busy = false;
        self.silent_read = false;
        self.writing = false;
        let unpinning = std::mem::take(&mut self.unpinning);
        self.error = false;
        self.message.clear();
        match d.result {
            Update::CatalogAppend(..) | Update::Availability(..) | Update::VerifyUnavailable(..) => unreachable!("handled above"),
            Update::Read(Ok(page), _) => {
                let changed = *self.page != page;
                self.page = Arc::new(page);
                self.cache.put(self.route.clone(), self.page.clone());
                self.render(changed);
            }
            Update::Storage(result) => {
                let summary = result.unwrap_or_else(|_| "Storage unavailable".into());
                self.scoped_ui(move |ui| ui.set_music_storage(summary.into()));
            }
            Update::LocalRead(_, result) => match result {
                Ok(page) => {
                    let changed = *self.page != page;
                    self.page = Arc::new(page);
                    self.render(changed);
                }
                Err(e) => {
                    let retiring_remote = !self.page.local_audio;
                    if retiring_remote {
                        let page = Arc::make_mut(&mut self.page);
                        page.rows.clear();
                        page.local_audio = true;
                        page.playback = None;
                        page.local_collection_playable = false;
                    }
                    self.error = true;
                    self.message = readable(&e);
                    self.render(retiring_remote);
                }
            },
            Update::ReadGap => {
                self.message = if self.page.rows.is_empty() {
                    "Waiting for the library…"
                } else {
                    "Connection interrupted. Showing your last view."
                }
                .into();
                self.render(false);
            }
            Update::Read(Err(e), _) => {
                self.continuation.stopped = true;
                self.error = true;
                self.message = readable(&e);
                self.render(false);
            }
            Update::Changed(Ok(route)) => {
                self.scoped_ui(|ui| clear_form(&ui));
                self.cache.saved_changed();
                if route.is_none()
                    && !matches!(self.route, Route::Offline | Route::SavedCollection(_))
                {
                    self.message = "Saved on this device.".into();
                    self.render(false);
                    return;
                }
                if let Some(next) = &route {
                    self.trail.landed(next);
                }
                self.visit(Navigation::Untracked(
                    route.unwrap_or_else(|| self.route.clone()),
                ));
            }
            Update::CollectionSaved(result) => {
                self.cache.saved_changed();
                match result {
                    Ok(summary) => {
                        self.message = if summary.present == summary.total {
                            format!("Saved all {} tracks on this device.", summary.total)
                        } else {
                            format!(
                                "Saved {} of {} tracks. Some audio is unavailable.",
                                summary.present, summary.total
                            )
                        };
                    }
                    Err(e) => {
                        self.message = readable(&e);
                        self.error = true;
                    }
                }
                self.render(false);
            }
            Update::PinChanged(result) => {
                if unpinning {
                    let error = result.as_ref().err().map(readable);
                    self.scoped_ui(move |ui| {
                        if ui.get_form_mode() == "remove-saved"
                            && ui.get_form_return() == "unpin-playlist"
                        {
                            if let Some(error) = error {
                                ui.set_form_error(error.into());
                            } else {
                                clear_form(&ui);
                            }
                        }
                    });
                }
                let landed_offline = self.access.as_ref().is_some_and(|a| a.offline);
                if unpinning && saved_route(&self.route) && result.is_ok() {
                    self.cache.saved_changed();
                    self.saved_reload_due = false;
                    if matches!(self.route, Route::SavedCollection(_)) {
                        self.trail.landed(&Route::Offline);
                    }
                    self.visit(Navigation::Untracked(Route::Offline));
                    // The original popup trigger may no longer exist.
                    self.scoped_ui(|ui| {
                        ui.set_saved_pin_focus_serial(
                            ui.get_saved_pin_focus_serial().wrapping_add(1),
                        )
                    });
                    return;
                }
                if unpinning && self.route == Route::Playlists && result.is_ok() {
                    self.scoped_ui(|ui| {
                        ui.set_saved_pin_focus_serial(
                            ui.get_saved_pin_focus_serial().wrapping_add(1),
                        );
                    });
                }
                if unpinning && landed_offline && !saved_route(&self.route) {
                    self.visit(Navigation::Untracked(self.route.clone()));
                }
                if let Err(e) = result {
                    self.message = readable(&e);
                    self.error = true;
                }
                self.render(false);
            }
            Update::Changed(Err(e)) => {
                // A removal can be visible even if the final disk sync failed.
                self.cache.saved_changed();
                let msg = readable(&e);
                self.scoped_ui({
                    let m = msg.clone();
                    move |ui| ui.set_form_error(m.into())
                });
                self.error = true;
                self.message = msg;
                self.render(false);
            }
            Update::Local(_, Ok(command)) => {
                self.playback(command);
                if std::mem::take(&mut self.saved_reload_due) && self.page.local_audio {
                    self.visit(Navigation::Untracked(self.route.clone()));
                } else {
                    self.render(false);
                }
            }
            Update::Local(id, Err(e)) => {
                self.message = readable(&e);
                self.error = true;
                self.cache.saved_changed();
                self.render(false);
                self.verify_unavailable(id);
            }
        }
    }
}
const SEARCH_DELAY: Duration = Duration::from_millis(240);

fn retained_history(access: &Access) -> Option<Page> {
    access.personal.cached_page(access.epoch, &Route::History)
}

async fn cached_local_page(access: &Access, route: &Route) -> Result<Page, ApiError> {
    let page = access.personal.cached_page(access.epoch, route).unwrap_or_else(|| match route {
        Route::Playlists => pages::personal_list(route, &[]),
        Route::History => pages::recent(&[]),
        _ => Page { title: "Playlist".into(), status: "Playlist details not saved on this device.".into(), ..Page::default() },
    });
    let ids = page.rows.iter().filter(|row| row.item.is_some()).map(|row| row.id.clone()).collect();
    apply_download_availability(page, crate::collections::browse_tracks(access, ids).await)
}

fn reset_local_admissions(page: &mut Page) {
    page.local_audio = true;
    page.live_rules = false;
    page.playback = None;
    page.local_collection_playable = false;
    for row in &mut page.rows {
        row.playable = false;
        row.local_recording = None;
    }
}

fn apply_download_availability(
    mut page: Page,
    records: Result<HashMap<String, crate::offline::Download>, ApiError>,
) -> Result<Page, ApiError> {
    reset_local_admissions(&mut page);
    let records = match records {
        Ok(records) => records,
        Err(error) if error.expired => return Err(error),
        Err(_) => {
            // Availability is independent of cached metadata. Do not turn a
            // failed verification into either an empty view or a missing file.
            page.status = "Download availability couldn’t be checked.".into();
            return Ok(page);
        }
    };
    for row in page.rows.iter_mut().filter(|row| row.item.is_some()) {
        row.local_recording = records.get(&row.id).map(|record| record.recording);
        row.playable = row.local_recording.is_some();
        if !row.playable {
            row.detail = pages::join_labels(&row.detail, "Not downloaded");
        }
    }
    Ok(page)
}

async fn local_playback(
    access: &Access,
    route: &Route,
    page: &Page,
    index: Option<usize>,
    target: Option<Route>,
    known: &[(Route, Arc<Page>)],
    enqueue: bool,
) -> Result<Command, ApiError> {
    if target.is_none() && !matches!(route, Route::Artist(_)) {
        return saved::play(access, route, page, index, enqueue).await;
    }
    let route = target.as_ref().unwrap_or(route);
    let page = match route {
        Route::Playlist(_) => cached_local_page(access, route).await?,
        Route::SavedCollection(id) => playlist_browser::detail(access, id).await?,
        Route::Album(_) | Route::Artist(_) => {
            let records = saved::inventory(access).await?;
            let recent = retained_history(access);
            let known = known
                .iter()
                .map(|(route, page)| (route, page.as_ref()))
                .collect::<Vec<_>>();
            catalog_page(route, &records, &known, recent.as_ref(), true)
        }
        _ => return Err(unavailable("No songs are available on this device.")),
    };
    saved::play(access, route, &page, None, enqueue).await
}

fn catalog_page(
    route: &Route,
    records: &HashMap<String, crate::offline::Download>,
    known: &[(&Route, &Page)],
    recent: Option<&Page>,
    resolve_artist_tracks: bool,
) -> Page {
    let items = records
        .iter()
        .map(|(id, record)| (id.clone(), record.item.clone()))
        .collect();
    let mut page = crate::local_catalog::project(route, &items, known, recent).unwrap_or_default();
    if matches!(route, Route::Search(q) if !q.trim().is_empty()) {
        page.subtitle = pages::count_label(page.rows.len() as u64, "result", "results");
    }
    if resolve_artist_tracks && let Route::Artist(id) = route {
        page.rows = crate::local_catalog::artist_tracks(id, &items, known, recent);
    }
    for row in page.rows.iter_mut().filter(|row| row.item.is_some()) {
        row.local_recording = records.get(&row.id).map(|record| record.recording);
    }
    page
}

fn project_local_page(
    route: &Route,
    mut page: Page,
    known: &[(&Route, &Page)],
    history: Option<&Page>,
    previous: Option<&Page>,
) -> Page {
    page.local_audio = true;
    if !matches!(route, Route::Playlist(_) | Route::SavedCollection(_) | Route::Offline) {
        return page;
    }
    if let Some(previous) = previous.filter(|p| p.local_audio) {
        let rows: HashMap<_, _> = previous.rows.iter().filter(|r| r.item.is_some()).map(|r| (&r.key, r)).collect();
        for row in &mut page.rows {
            if let Some(old) = rows.get(&row.key).filter(|old| old.item == row.item) {
                // A verified inventory row has no typed catalog IDs. Keep
                // already resolved links if the catalog source was evicted.
                row.music = old.music.clone();
            }
        }
    }
    let items = page.rows.iter().filter_map(|row| row.item.as_ref())
        .map(|item| (item.id.clone(), item.clone())).collect();
    crate::local_catalog::attach_track_links(&mut page.rows, &items, known, history);
    page
}

async fn downloads_page(access: &Access) -> Result<Page, ApiError> {
    let mut page = saved::page(access).await?;
    let (mut groups, warning) = playlist_browser::saved_rows(access).await?;
    if let Some(error) = warning {
        page.status = readable(&error);
    }
    groups.append(&mut page.rows);
    page.total = Some(groups.len() as u64);
    page.rows = groups;
    Ok(page)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playlist_list_offline_marks_keep_intent_progress_errors_and_commits_distinct() {
        use playlist_pins::{Phase, Status};
        let project = |phase, committed, hint| {
            playlist_list_offline(Some(&Status::fixture(phase, committed, hint)))
        };
        assert_eq!(playlist_list_offline(None), PlaylistListOffline::default());
        let mut removed = Status::fixture(Phase::Saved, true, None);
        removed.wanted = false;
        assert_eq!(
            playlist_list_offline(Some(&removed)),
            PlaylistListOffline::default()
        );

        let saved = project(Phase::Saved, true, None);
        assert_eq!(saved.mark, PlaylistOfflineMark::Saved);
        assert!(!saved.retained);
        assert_eq!(saved.description.as_str(), "Saved on this device");

        for committed in [false, true] {
            let pending = project(Phase::Pending, committed, None);
            assert_eq!(pending.mark, PlaylistOfflineMark::Pending);
            assert_eq!(pending.retained, committed);
            assert!(pending.description.contains(if committed {
                "Refresh pending"
            } else {
                "Waiting to download"
            }));
            let starting = project(
                Phase::Downloading {
                    verified: 0,
                    total: 0,
                },
                committed,
                None,
            );
            assert_eq!(starting.mark, PlaylistOfflineMark::Downloading);
            assert!(starting.percent.is_empty());
            let progress = project(
                Phase::Downloading {
                    verified: 1,
                    total: 3,
                },
                committed,
                None,
            );
            assert_eq!(progress.mark, PlaylistOfflineMark::Downloading);
            assert_eq!(progress.percent.as_str(), "33%");
            assert!(progress.description.contains("1 of 3"));
            assert_eq!(progress.retained, committed);
            let complete = project(
                Phase::Downloading {
                    verified: usize::MAX,
                    total: usize::MAX,
                },
                committed,
                None,
            );
            assert_eq!(complete.mark, PlaylistOfflineMark::Downloading);
            assert_eq!(complete.percent.as_str(), "100%");
            let interrupted = project(
                Phase::Retry,
                committed,
                Some("A track is not saved on this device."),
            );
            assert_eq!(interrupted.mark, PlaylistOfflineMark::Retry);
            assert_eq!(interrupted.retained, committed);
            assert!(interrupted.description.contains("A track is not saved"));
            assert_ne!(interrupted.description, saved.description);
        }
    }

    #[test]
    fn playlist_list_status_is_separate_from_count_and_does_not_change_saved_route() {
        let row = pages::Row {
            subtitle: "1 song".into(),
            ..Default::default()
        };
        let status = playlist_pins::Status::fixture(playlist_pins::Phase::Pending, false, None);
        let (count, mark) = pin_row_copy(&Route::Playlists, &row, Some(&status));
        assert_eq!(count.as_str(), "1 song");
        assert_eq!(mark.mark, PlaylistOfflineMark::Pending);
        let (saved_subtitle, saved_mark) = pin_row_copy(&Route::Offline, &row, Some(&status));
        assert_eq!(saved_subtitle.as_str(), "1 song · Waiting to download");
        assert_eq!(saved_mark, PlaylistListOffline::default());
    }

    #[test]
    fn playlist_order_basis_preserves_only_the_original_online_authority() {
        use crate::{
            personal::{Change, PersonalUpdate, Snapshot},
            personal_bridge::PersonalHub,
        };
        let hub = PersonalHub::default();
        let access = hub.access();
        let snapshot = || {
            PersonalUpdate::Snapshot(Snapshot {
                playlists: Vec::new(),
                recent: Vec::new(),
            })
        };
        hub.begin(1, 2);
        hub.receive(1, 2, snapshot()).unwrap();
        let status = access.read(|status, _| status);
        let basis = PlaylistOrderBasis::current(1, 3, 0, status).unwrap();
        assert_eq!(Some(basis), PlaylistOrderBasis::current(1, 3, 0, status));
        assert_ne!(Some(basis), PlaylistOrderBasis::current(1, 4, 0, status));
        assert_eq!(None, PlaylistOrderBasis::current(7, 3, 0, status));
        assert_eq!(None, PlaylistOrderBasis::current(1, 3, 1, status));
        assert_ne!(Some(basis), PlaylistOrderBasis::current(1, 3, 2, status));

        hub.receive(1, 2, PersonalUpdate::Change(Change::default()))
            .unwrap();
        let newest = access.read(|status, _| status);
        assert_ne!(Some(basis), PlaylistOrderBasis::current(1, 3, 0, newest));
        let renewed = PlaylistOrderBasis::current(1, 3, 0, newest).unwrap();
        hub.gap(1, 2);
        assert_eq!(
            None,
            access.read(|status, _| PlaylistOrderBasis::current(1, 3, 0, status))
        );
        hub.receive(1, 2, snapshot()).unwrap();
        assert_ne!(
            Some(renewed),
            access.read(|status, _| PlaylistOrderBasis::current(1, 3, 0, status))
        );

        hub.begin(1, 5);
        hub.receive(1, 5, snapshot()).unwrap();
        let new_socket = access.read(|status, _| PlaylistOrderBasis::current(1, 3, 0, status));
        assert!(new_socket.is_some());
        assert_ne!(Some(renewed), new_socket);
    }

    #[test]
    fn unconfirmed_order_stays_blocked_until_fresh_playlist_authority() {
        use crate::{
            personal::{Change, PersonalUpdate, Snapshot},
            personal_bridge::PersonalHub,
        };
        let hub = PersonalHub::default();
        let access = hub.access();
        hub.begin(1, 2);
        let snapshot = || {
            PersonalUpdate::Snapshot(Snapshot {
                playlists: Vec::new(),
                recent: Vec::new(),
            })
        };
        hub.receive(1, 2, snapshot()).unwrap();
        let _ = access.watch_playlist(1, Some("manual".into()));
        let before = access.read(|status, _| status);
        let fence = UnconfirmedOrder {
            epoch: before.epoch,
            socket_generation: before.generation,
            revision: 10,
        };
        assert!(fence.blocks(before, 10));
        hub.receive(1, 2, PersonalUpdate::Change(Change::default()))
            .unwrap();
        assert!(access.read(|status, _| fence.blocks(status, 10)));
        hub.receive(1, 2, snapshot()).unwrap();
        let updated = access.read(|status, _| status);
        assert_ne!(before.playlist_detail, updated.playlist_detail);
        assert!(fence.blocks(updated, 9));
        assert!(fence.blocks(updated, 10));
        assert!(!fence.blocks(updated, 11));
        hub.begin(1, 3);
        hub.receive(1, 3, snapshot()).unwrap();
        assert!(!access.read(|status, _| fence.blocks(status, 10)));
    }

    #[test]
    fn catalog_retry_is_bounded_and_only_for_transient_errors() {
        for status in [400, 401, 404, 409, 410, 412, 408, 429, 503] {
            let reply: Result<(), _> = Err(ApiError {
                message: "",
                status,
                expired: status == 401,
            });
            assert_eq!(read_retry(0, &reply), matches!(status, 408 | 429 | 503));
            assert!(!read_retry(READ_RETRIES, &reply));
        }
        assert_eq!(
            (0..READ_RETRIES)
                .map(|i| read_backoff(i).as_secs())
                .sum::<u64>(),
            15
        );
    }
    #[test]
    fn same_query_from_edit_and_enter_is_coalesced_except_for_retry() {
        let query = Route::Search("track".into());
        assert!(repeat_navigation(&query, &query, true, false));
        assert!(repeat_navigation(&query, &query, false, false));
        assert!(!repeat_navigation(&query, &query, false, true));
        assert!(!repeat_navigation(
            &query,
            &Route::Search("another".into()),
            true,
            false
        ));
        assert!(!repeat_navigation(&query, &Route::History, true, false));
    }
    #[test]
    fn only_current_context_screens_are_navigable() {
        assert_eq!(
            context_route("album", "al"),
            Some(Route::Album("al".into()))
        );
        assert_eq!(
            context_route("artist", "ar"),
            Some(Route::Artist("ar".into()))
        );
        for kind in ["playlist", "derived"] {
            assert_eq!(context_route(kind, "p"), Some(Route::Playlist("p".into())));
        }
        assert_eq!(context_route("likes", "liked-songs"), None);
        assert_eq!(context_route("playlist", ""), None);
    }
    #[test]
    fn metadata_targets_are_explicit_even_in_device_views() {
        for bad in ["", " ", "a/b", "a\\b", ".."] {
            assert_eq!(pages::music_route("artist", bad), None);
            assert_eq!(pages::music_route("album", bad), None);
        }
        let credit = pages::Credit {
            name: "Artist".into(),
            target: Some(Route::Artist("ar".into())),
        };
        assert_eq!(projected_music(Some(&credit), true), ("ar", "Artist"));
        assert_eq!(projected_music(Some(&credit), false), ("", "Artist"));
    }
    #[test]
    fn title_target_comes_only_from_an_addressable_track_album() {
        let mut row = pages::Row {
            kind: "track".into(),
            music: pages::MusicMetadata {
                album: Some(pages::Credit {
                    name: "Genuine display name".into(),
                    target: Some(Route::Album("album%2F1".into())),
                }),
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(track_album(&row), ("album%2F1", "Genuine display name"));
        let album = row.music.album.as_mut().unwrap();
        album.target = Some(Route::Album("bad/id".into()));
        assert_eq!(track_album(&row), ("", "Genuine display name"));
        row.music.album.as_mut().unwrap().target = None;
        assert_eq!(track_album(&row), ("", "Genuine display name"));
        row.kind = "album".into();
        assert_eq!(track_album(&row), ("", ""));
    }

    #[test]
    fn search_track_unavailable_distinguishes_media_from_play_controls() {
        let route = Route::Search("fixture".into());
        let mut row = pages::Row {
            kind: "track".into(),
            ..Default::default()
        };
        assert!(search_track_unavailable(&route, &row));
        assert!(!search_track_unavailable(
            &Route::Playlist("list".into()),
            &row
        ));
        row.kind = "album".into();
        assert!(!search_track_unavailable(&route, &row));
        row.kind = "track".into();
        row.playable = true;
        assert!(!search_track_unavailable(&route, &row));
    }

    #[test]
    fn search_quick_add_requires_a_typed_catalog_source() {
        let mut row = pages::Row {
            kind: "album".into(),
            id: "source".into(),
            key: RowKey::Album("source".into()),
            route: Some(Route::Album("source".into())),
            ..Default::default()
        };
        // Search coverage is a hint; bulk reads the authoritative collection.
        assert!(!row.playable);
        assert_eq!(quick_add_kind(&row), Some("album"));
        row.route = Some(Route::Album("wrong-source".into()));
        assert_eq!(quick_add_kind(&row), None);
        row.kind = "artist".into();
        row.route = Some(Route::Artist("source".into()));
        assert_eq!(quick_add_kind(&row), None);
        row.key = RowKey::Artist("source".into());
        assert_eq!(quick_add_kind(&row), Some("artist"));
        row.kind = "track".into();
        row.key = RowKey::Track("source".into());
        row.route = None;
        assert_eq!(quick_add_kind(&row), None);
        row.playable = true;
        assert_eq!(quick_add_kind(&row), Some("track"));
    }

    #[test]
    fn playlist_title_return_resolves_the_exact_occurrence_after_a_reorder() {
        let route = Route::Playlist("list".into());
        let occurrence = |entry_id: &str, album: &str| pages::Row {
            kind: "track".into(),
            id: "repeated-song".into(),
            key: RowKey::ManualEntry {
                playlist_id: "list".into(),
                entry_id: entry_id.into(),
            },
            music: pages::MusicMetadata {
                album: Some(pages::Credit {
                    name: "Same display name".into(),
                    target: Some(Route::Album(album.into())),
                }),
                ..Default::default()
            },
            ..Default::default()
        };
        let origin = TitleOrigin {
            source: TitleSource::Playlist,
            key: "entry-b".into(),
            album: "exact-album".into(),
        };
        let a = occurrence("entry-a", "exact-album");
        let b = occurrence("entry-b", "exact-album");
        assert_eq!(
            origin.playlist_index(&[a.clone(), b.clone()], &route),
            Some(1)
        );
        assert_eq!(
            origin.playlist_index(&[b.clone(), a.clone()], &route),
            Some(0)
        );
        assert_eq!(origin.playlist_index(&[a], &route), None);
        assert_eq!(origin.playlist_index(&[b.clone(), b.clone()], &route), None);
        assert_eq!(
            origin.playlist_index(&[b, occurrence("entry-b", "new-album")], &route),
            None
        );
        assert_eq!(
            origin.playlist_index(&[occurrence("entry-b", "new-album")], &route),
            None
        );
    }

    #[test]
    fn replacement_does_not_transfer_title_focus_to_a_playable_body() {
        let row = |album: &str| BrowseRow {
            occurrence_key: "entry".into(),
            album_id: album.into(),
            playable: true,
            ..Default::default()
        };
        assert_eq!(
            playlist_focus_destination(&[row("album%2F1")], "entry", "album:album%2F1"),
            Some(0)
        );
        for album in ["", "new-album"] {
            assert_eq!(
                playlist_focus_destination(&[row(album)], "entry", "album:album%2F1"),
                None
            );
            assert_eq!(
                playlist_focus_destination(&[row(album)], "entry", ""),
                Some(0)
            );
        }
    }

    #[test]
    fn catalog_return_remaps_focus_and_scroll_anchor_by_typed_row_key() {
        let row = |key| pages::Row {
            key,
            ..Default::default()
        };
        let track = row(RowKey::Track("same-id".into()));
        let album = row(RowKey::Album("same-id".into()));
        let mut position = CatalogPosition {
            y: -150.0,
            anchor: 0,
            anchor_key: None,
            anchor_offset: -5.0,
            row: 1,
            row_key: None,
            part: "album".into(),
            credit: "exact-title-album".into(),
        };
        position.capture_keys(&[album.clone(), track.clone()]);
        let mut visit = CatalogVisit {
            page: Some(Arc::new(Page {
                rows: vec![album.clone(), track.clone()],
                ..Default::default()
            })),
            position,
            continuation: CatalogContinuation {
                loading: true,
                ..Default::default()
            },
        };
        visit.retire_snapshot();
        assert!(visit.page.is_none() && !visit.continuation.loading);
        let mut position = visit.position;
        position.resolve_keys(&[track, album.clone()]);
        assert_eq!((position.anchor, position.row), (1, 0));
        assert_eq!(
            (position.part.as_str(), position.credit.as_str()),
            ("album", "exact-title-album")
        );
        position.resolve_keys(&[album]);
        assert_eq!((position.anchor, position.row), (0, -1));
    }

    #[test]
    fn downloads_poll_keeps_enriched_navigation_and_does_not_replace_an_unchanged_view() {
        let track = |id: &str| {
            let item = crate::api::Item {
                id: id.into(), name: id.into(), kind: "track".into(),
                artists: "Artist".into(), album_name: "Album".into(), has_audio: true,
                duration_ms: 1000, disc_number: 1, track_number: 1,
            };
            pages::Row {
                id: id.into(), key: RowKey::Track(id.into()), kind: "track".into(),
                item: Some(item), playable: true, ..pages::Row::default()
            }
        };
        let lost = track("deleted");
        let keep = track("survives");
        let mut enriched = keep.clone();
        enriched.music.album = Some(pages::Credit { name: "Album".into(), target: Some(Route::Album("album-id".into())) });
        enriched.music.credits.push(pages::Credit { name: "Artist".into(), target: Some(Route::Artist("artist-id".into())) });
        let old_page = project_local_page(&Route::Offline, Page {
            kind: "offline".into(), total: Some(2), rows: vec![lost.clone(), enriched.clone()], ..Page::default()
        }, &[], None, None);
        let raw = Page { rows: vec![lost, keep.clone()], ..old_page.clone() };
        let unchanged = project_local_page(&Route::Offline, raw, &[], None, Some(&old_page));
        assert!(unchanged == old_page, "no-op poll must not replace the focused row model");
        let raw = Page { rows: vec![keep.clone()], total: Some(1), ..old_page.clone() };
        let changed = project_local_page(&Route::Offline, raw, &[], None, Some(&old_page));
        assert_eq!(changed.rows.len(), 1);
        assert_eq!(changed.rows[0].key, keep.key);
        assert_eq!(track_album(&changed.rows[0]), ("album-id", "Album"));
        assert_eq!(changed.rows[0].music.credits, enriched.music.credits);
    }

    fn step(route: &mut Route, trail: &mut Trail, navigation: Navigation) {
        *route = navigation
            .resolve(route, trail, false, false, false)
            .unwrap();
    }

    #[test]
    fn peer_rail_clicks_start_roots_with_no_back() {
        let mut route = INITIAL_ROUTE;
        let mut trail = Trail::default();
        assert_eq!(route, Route::History);
        assert!(!trail.can_back());
        step(
            &mut route,
            &mut trail,
            Navigation::Follow(Route::Album("from-history".into())),
        );
        step(&mut route, &mut trail, Navigation::Back);
        assert_eq!(route, Route::History);
        assert!(!trail.can_back());

        for (target, expected) in [
            ("playlists", Route::Playlists),
            ("offline", Route::Offline),
            ("history", Route::History),
            ("playlists", Route::Playlists),
            ("offline", Route::Offline),
            ("settings", Route::Appearance),
        ] {
            step(&mut route, &mut trail, Navigation::named(target).unwrap());
            assert_eq!(route, expected);
            assert!(!trail.can_back(), "{target} must not offer Back");
            assert_eq!(
                Navigation::Back.resolve(&route, &mut trail, false, false, false),
                None
            );
        }
    }

    #[test]
    fn selecting_the_active_rail_section_from_detail_discards_prior_routes() {
        for (target, root, detail) in [
            (
                "playlists",
                Route::Playlists,
                Route::Playlist("list".into()),
            ),
            (
                "offline",
                Route::Offline,
                Route::SavedCollection("saved".into()),
            ),
        ] {
            let mut route = root.clone();
            let mut trail = Trail::default();
            step(&mut route, &mut trail, Navigation::Follow(detail.clone()));
            assert_eq!(route, detail);
            assert!(trail.can_back());
            step(&mut route, &mut trail, Navigation::named(target).unwrap());
            assert_eq!(route, root);
            assert!(!trail.can_back());
        }
    }

    #[test]
    fn reselecting_a_root_reached_by_following_a_link_clears_back_even_while_reading() {
        let mut trail = Trail::default();
        let saved = Navigation::Follow(Route::Offline)
            .resolve(&Route::Appearance, &mut trail, false, false, false)
            .unwrap();
        assert!(trail.can_back());
        let root = Navigation::named("offline").unwrap();
        assert_eq!(
            root.resolve(&saved, &mut trail, true, false, false),
            Some(saved)
        );
        assert!(!trail.can_back());
    }

    #[test]
    fn back_restores_entity_and_ranked_search_sources_without_inserting_history() {
        let keys = RandomState::new();
        let playlist = Route::Playlist("list".into());
        let query = Route::Search("a ranked query".into());
        let artist = Route::Artist("artist".into());
        let album = Route::Album("album".into());
        let sources = [
            artist.clone(),
            query.clone(),
            playlist.clone(),
            Route::Playlists,
        ]
        .map(|r| (page_key(&r, Some(&keys)), r));
        let mut route = INITIAL_ROUTE;
        let mut trail = Trail::default();
        step(
            &mut route,
            &mut trail,
            Navigation::named("playlists").unwrap(),
        );
        for next in [
            playlist.clone(),
            Route::Search("a ranked".into()),
            query.clone(),
            artist.clone(),
            album,
        ] {
            step(&mut route, &mut trail, Navigation::Follow(next));
        }
        for (key, source) in sources {
            step(&mut route, &mut trail, Navigation::named("back").unwrap());
            assert_eq!(route, source);
            assert_eq!(page_key(&route, Some(&keys)), key);
        }
        assert!(!trail.can_back());
    }

    #[test]
    fn add_songs_enters_regular_search_and_keeps_the_initiating_manual_route() {
        let mut route = Route::Playlists;
        let mut trail = Trail::default();
        let detail = Route::Playlist("list".into());
        let search = Route::Search(String::new());
        step(&mut route, &mut trail, Navigation::Follow(detail.clone()));
        step(&mut route, &mut trail, Navigation::AddSongs);
        assert_eq!(route, search);
        let shortcut = trail.top_id();
        step(
            &mut route,
            &mut trail,
            Navigation::Follow(Route::Search("night train".into())),
        );
        assert_eq!(trail.top_id(), shortcut);
        let album = Route::Album("night-train".into());
        step(&mut route, &mut trail, Navigation::Follow(album));
        step(&mut route, &mut trail, Navigation::Back);
        assert_eq!(route, Route::Search("night train".into()));
        step(&mut route, &mut trail, Navigation::Back);
        assert_eq!(route, detail);
        step(&mut route, &mut trail, Navigation::Back);
        assert_eq!(route, Route::Playlists);
        assert!(!trail.can_back());
    }

    #[test]
    fn page_keys_do_not_expose_query_or_entity_identity() {
        let keys = RandomState::new();
        let private = "synthetic private query/id";
        let query = Route::Search(private.into());
        let key = page_key(&query, Some(&keys));
        assert!(key.starts_with("p-"));
        assert_eq!(key.len(), 18);
        assert!(!key.contains(private));
        assert_ne!(
            key,
            page_key(&Route::Search(private.to_uppercase()), Some(&keys))
        );
        assert_ne!(key, page_key(&Route::Playlist(private.into()), Some(&keys)));

        assert_ne!(page_key(&query, Some(&RandomState::new())), key);
        assert!(page_key(&Route::History, None).is_empty());
    }
    #[test]
    fn catalog_indexes_are_not_named_destinations() {
        assert!(Navigation::named("albums").is_none());
        assert!(Navigation::named("artists").is_none());
        assert!(matches!(
            Navigation::named("search"),
            Some(Navigation::Follow(Route::Search(q))) if q.is_empty()
        ));
    }
    #[test]
    fn stale_form_cannot_borrow_new_playlist_revision() {
        let mut m = pages::PlaylistMetadata {
            id: "p".into(),
            name: "Name".into(),
            kind: crate::api::library::PlaylistKind::Manual,
            revision: 7,
            track_count: None,
            available_count: None,
            capabilities: crate::api::library::PlaylistCapabilities {
                edit_metadata: true,
                edit_entries: true,
                edit_definition: false,
                play: true,
                copy_to_manual: false,
                delete: true,
                add_tracks: true,
            },
        };
        let basis = Some(EditBasis { op: "rename".into(), id: "p".into(), revision: 7, rules: None });
        let mut p = Page {
            playlist: Some(m.clone()),
            ..Default::default()
        };
        assert!(draft_matches(&basis, &p, "rename"));
        m.revision += 1;
        p.playlist = Some(m);
        assert!(!draft_matches(&basis, &p, "rename"));
        assert!(!draft_matches(&basis, &p, "delete"));
    }
    #[test]
    fn track_bodies_have_no_destination_but_saved_collection_cards_still_open() {
        let track = pages::Row {
            kind: "track".into(),
            playable: true,
            ..Default::default()
        };
        let target = Route::SavedCollection("collection".into());
        let collection = pages::Row {
            kind: "saved-collection".into(),
            route: Some(target.clone()),
            ..Default::default()
        };
        assert_eq!(row_open_target(&Route::Offline, &track), None);
        assert_eq!(row_open_target(&target, &track), None);
        assert_eq!(row_open_target(&Route::Offline, &collection), Some(&target));
        let playlist = Route::Playlist("playlist".into());
        let row = pages::Row {
            kind: "playlist".into(),
            route: Some(playlist.clone()),
            ..Default::default()
        };
        assert_eq!(row_open_target(&Route::Playlists, &row), Some(&playlist));
    }
    #[test]
    fn offline_does_not_offer_online_mutations() {
        let p = Page {
            kind: "album".into(),
            ..Default::default()
        };
        let r = pages::Row {
            id: "t".into(),
            playable: true,
            ..Default::default()
        };
        assert!(row_actions(&p, &r, true, true, false).is_empty());
    }

    fn editable_playlist() -> crate::api::library::PlaylistSummary {
        use crate::api::library::{
            CountState, EvaluationState, PlaylistCapabilities, PlaylistKind, PlaylistSummary,
        };
        PlaylistSummary {
            id: "mix".into(),
            name: "Mix".into(),
            description: "The actual description".into(),
            kind: PlaylistKind::Derived,
            revision: 1,
            track_count: Some(3),
            available_count: Some(2),
            count_state: CountState::Exact,
            evaluation_state: Some(EvaluationState::Ready),
            evaluation_error_code: None,
            created_at: String::new(),
            updated_at: String::new(),
            capabilities: PlaylistCapabilities {
                edit_metadata: true,
                edit_entries: false,
                edit_definition: true,
                play: true,
                copy_to_manual: false,
                delete: true,
                add_tracks: false,
            },
        }
    }

    #[test]
    fn search_entity_menu_uses_only_addressable_and_playable_catalog_targets() {
        let mut page = Page {
            kind: "search".into(),
            ..Page::default()
        };
        for (kind, route, open) in [
            ("album", Route::Album("same".into()), "open-album"),
            ("artist", Route::Artist("same".into()), "open-artist"),
        ] {
            let mut row = pages::Row {
                id: "same".into(),
                route: Some(route),
                kind: kind.into(),
                playable: true,
                ..pages::Row::default()
            };
            assert_eq!(row_actions(&page, &row, false, true, false), [open]);
            assert_eq!(row_actions(&page, &row, true, false, false), [open]);
            page.local_audio = true;
            assert_eq!(
                row_actions(&page, &row, true, false, false),
                ["play", "queue"]
            );
            assert_eq!(row_actions(&page, &row, false, true, false), [open]);
            row.playable = false;
            assert_eq!(row_actions(&page, &row, true, false, false), [open]);
            row.route = Some(Route::Album("unrelated".into()));
            assert!(row_actions(&page, &row, true, false, false).is_empty());
            page.local_audio = false;
        }
    }

    #[test]
    fn failed_download_lookup_keeps_cached_playlist_and_history_occurrences() {
        use crate::api::library::{HistoryEntry, ManualEntry};
        let track: crate::api::catalog::CatalogItem = serde_json::from_value(serde_json::json!({
            "kind":"track", "id":"same-track", "name":"Song", "artists":[], "genres":[],
            "album_id":"album", "album_name":"Album", "album_type":"", "release_date":"", "relation":"",
            "duration_ms":60000, "disc_number":0, "track_number":0, "has_audio":true,
            "coverage":{"tracks":1,"inLibrary":1,"albums":1}
        })).unwrap();
        let playlist = pages::personal_playlist(&editable_playlist(), None, &[
            ManualEntry { id: "entry-a".into(), position: 0, track: track.clone() },
            ManualEntry { id: "entry-b".into(), position: 1, track: track.clone() },
        ]);
        let history = pages::recent(&[
            HistoryEntry { id: "play-a".into(), played_at: "2026-06-10T10:00:00Z".into(), track: track.clone(),
                origin: None, received_at: None, metadata_missing: false },
            HistoryEntry { id: "play-b".into(), played_at: "2026-06-10T10:01:00Z".into(), track,
                origin: None, received_at: None, metadata_missing: false },
        ]);
        let failed = ApiError { message: "disk unavailable", status: 0, expired: false };
        for page in [playlist, history] {
            let unknown = apply_download_availability(page.clone(), Err(failed)).unwrap();
            let missing = apply_download_availability(page.clone(), Ok(HashMap::new())).unwrap();
            assert_eq!(unknown.title, page.title);
            assert_eq!((unknown.rows.len(), unknown.total), (page.rows.len(), page.total));
            assert!(!unknown.status.is_empty());
            assert!(unknown.local_audio && unknown.playback.is_none());
            for ((unknown, missing), original) in unknown.rows.iter().zip(&missing.rows).zip(&page.rows) {
                assert_eq!((&unknown.key, &unknown.title, &unknown.detail), (&original.key, &original.title, &original.detail));
                assert!(!unknown.playable && unknown.local_recording.is_none());
                assert!(missing.detail.contains("Not downloaded"));
            }
        }
    }

    #[test]
    fn playlist_list_menu_offers_only_actions_for_that_row_and_mode() {
        let list = pages::personal_list(&Route::Playlists, &[editable_playlist()]);
        let row = &list.rows[0];
        assert_eq!(
            row_actions(&list, row, false, true, false),
            ["queue", "rename", "rules", "delete"]
        );
        assert_eq!(
            row_actions(&list, row, false, true, true),
            ["queue", "unpin-playlist", "rename", "rules", "delete"]
        );
        assert!(row_actions(&list, row, false, false, false).is_empty());
        assert_eq!(
            row_actions(&list, row, true, false, true),
            ["unpin-playlist"]
        );

        let mut unavailable = editable_playlist();
        unavailable.capabilities.play = false;
        unavailable.capabilities.edit_metadata = false;
        unavailable.capabilities.edit_definition = false;
        unavailable.capabilities.delete = false;
        let list = pages::personal_list(&Route::Playlists, &[unavailable]);
        assert!(row_actions(&list, &list.rows[0], false, true, false).is_empty());
    }

    #[test]
    fn detail_actions_separate_local_removal_from_capability_gated_controls() {
        let summary = editable_playlist();
        let mut detail = pages::personal_playlist(&summary, None, &[]);
        let route = Route::Playlist(summary.id);
        let online = playlist_detail_actions(&route, &detail, false, true);
        assert!(online.play && online.queue && online.rename && !online.rules);
        assert_eq!(online.offline_action, "pin-playlist");
        assert_eq!(online.remove_action, "delete");
        detail.definition = Some(
            serde_json::from_value(serde_json::json!({
                "version": 1, "sources": [{"kind": "allSaved", "slotId": "s"}],
                "sort": "sourceOrder", "duplicates": "unique"
            }))
            .unwrap(),
        );
        detail.live_rules = true;
        let pinned = playlist_detail_actions(&route, &detail, true, true);
        assert!(pinned.rules);
        assert_eq!(pinned.offline_action, "unpin-playlist");
        detail.live_rules = false;
        let disconnected = playlist_detail_actions(&route, &detail, true, false);
        assert!(
            !disconnected.play
                && !disconnected.queue
                && !disconnected.rename
                && !disconnected.rules
        );
        assert_eq!(disconnected.offline_action, "unpin-playlist");
        assert!(disconnected.remove_action.is_empty());

        let mut saved = Page {
            kind: "saved-collection".into(),
            local_audio: true,
            ..Default::default()
        };
        let route = Route::SavedCollection("key".into());
        saved.saved_pin = Some(crate::playlist_pin_store::Target {
            id: "mix".into(),
            title: "Mix".into(),
            kind: crate::api::library::PlaylistKind::Derived,
        });
        // The detail's source determines removal even if the status stream lags.
        let pending = playlist_detail_actions(&route, &saved, false, false);
        assert!(!pending.play && !pending.queue);
        assert_eq!(pending.remove_action, "unpin-playlist");
        saved.saved_pin = None;
        saved.rows.push(pages::Row {
            playable: true,
            ..Default::default()
        });
        let collection = playlist_detail_actions(&route, &saved, false, false);
        assert!(collection.play && collection.queue);
        assert_eq!(collection.remove_action, "remove");
        assert!(
            playlist_detail_actions(&route, &Page::default(), false, false)
                .remove_action
                .is_empty()
        );
    }

    #[test]
    fn detail_copy_preserves_rules_counts_and_validated_editor_values() {
        use crate::api::library::{EvaluationState, PlaylistKind};

        let mut summary = editable_playlist();
        summary.name = "Late rotation".into();
        summary.description = "One\nline\tthen another".into();
        assert_eq!(
            pages::playlist_detail_copy(&pages::personal_playlist(&summary, None, &[])),
            (
                "Rule playlist · 3 songs".into(),
                "One line then another".into()
            )
        );
        summary.track_count = None;
        summary.evaluation_state = Some(EvaluationState::Error);
        let detail = pages::personal_playlist(&summary, None, &[]);
        assert_eq!(pages::playlist_detail_copy(&detail).0, "Rule playlist");
        assert_eq!(detail.description, summary.description);
        assert!(detail.status.contains("aren't available"));
        assert!(detail.playlist_evaluation_failed);
        summary.kind = PlaylistKind::Manual;
        summary.track_count = Some(1);
        let detail = pages::personal_playlist(&summary, None, &[]);
        assert_eq!(pages::playlist_detail_copy(&detail).0, "1 song");
        assert_eq!(detail.subtitle, "Playlist · 1 song");
    }

    #[test]
    fn saved_detail_projects_pending_intent_separately_from_local_snapshot() {
        let route = Route::SavedCollection("fixture".into());
        let mut page = Page {
            kind: "saved-collection".into(),
            saved_pin: Some(crate::playlist_pin_store::Target {
                id: "mix".into(),
                title: "Mix".into(),
                kind: crate::api::library::PlaylistKind::Derived,
            }),
            ..Default::default()
        };
        let pending = playlist_detail_offline(&route, &page, None);
        assert!(!pending.saved);
        assert_eq!(pending.feedback, "Waiting to download");
        assert_eq!(pages::playlist_detail_copy(&page).0, "Rule playlist");
        page.total = Some(3);
        page.subtitle = "Rule playlist · 2 of 3 songs".into();
        page.status = "1 song not available on this device.".into();
        let saved = playlist_detail_offline(&route, &page, None);
        assert!(saved.saved);
        assert_eq!(saved.feedback, "");
        assert_eq!(pages::playlist_detail_copy(&page).0, page.subtitle);
        assert_eq!(page.status, "1 song not available on this device.");
        assert_eq!(
            playlist_detail_actions(&route, &page, false, false).remove_action,
            "unpin-playlist"
        );
        page.saved_pin = None;
        assert!(playlist_detail_offline(&route, &page, None).saved);
        assert_eq!(
            playlist_detail_actions(&route, &page, false, false).remove_action,
            "remove"
        );
        assert!(!playlist_detail_offline(&Route::Playlist("mix".into()), &page, None).saved);
    }

    #[test]
    fn direct_and_list_rule_editors_use_live_definition_without_ready_rows() {
        use crate::personal::{Change, Part, PersonalCommand, PersonalUpdate, Snapshot};
        let hub = crate::personal_bridge::PersonalHub::default();
        let personal = hub.access();
        let mut summary = editable_playlist();
        let id = summary.id.clone();
        let route = Route::Playlist(id.clone());
        let definition = |slot: &str| -> crate::api::library::RuleDefinition {
            serde_json::from_value(serde_json::json!({
                "version":1,"sources":[{"kind":"allSaved","slotId":slot}],
                "duplicates":"keep","sort":"sourceOrder"
            })).unwrap()
        };
        let old = definition("old");
        let current = definition("current");
        let accepted = |summary, definition| PersonalUpdate::Part(Part {
            summary, definition: Some(definition), token: "definition".into(), offset: 0, rows: vec![], more: false,
        });
        summary.track_count = Some(0);
        summary.available_count = Some(0);
        hub.begin(3, 17);
        hub.receive(3, 17, PersonalUpdate::Snapshot(Snapshot { playlists: vec![summary.clone()], recent: vec![] })).unwrap();
        personal.watch_playlist(3, Some(id.clone())).unwrap();
        hub.receive(3, 17, accepted(summary.clone(), old.clone())).unwrap();
        let (before_status, before) = personal.read(|status, replica| (status, replica.editable_rules(&id).unwrap().clone()));
        personal.watch_playlist(3, None);

        summary.revision += 1;
        summary.evaluation_state = Some(crate::api::library::EvaluationState::Error);
        summary.count_state = crate::api::library::CountState::Pending;
        summary.track_count = None;
        summary.available_count = None;
        assert!(hub.receive(3, 17, PersonalUpdate::Change(Change {
            playlist: Some(summary.clone()), rows_invalid: true, ..Change::default()
        })).unwrap().is_none());
        let list = personal.browse_view(3, &Route::Playlists, None).page.unwrap();
        assert!(row_actions(&list, &list.rows[0], false, true, false).contains(&"rules"));
        let ticket = RowEditor::select(&list.rows[0], "rules").unwrap();
        let (_, read) = personal.watch_playlist(3, Some(ticket.id.clone())).unwrap();
        let pending = personal.browse_view(3, &route, None).page.unwrap();
        assert!(ticket.matches(&route, &pending));
        assert!(!ticket.ready(&pending, false));
        assert!(pending.definition.is_none());
        assert!(!playlist_detail_actions(&route, &pending, false, false).rules);
        assert!(personal.start_playlist_sync(3, 17, &read, || true));
        hub.receive(3, 17, accepted(summary.clone(), current.clone())).unwrap();

        let view = personal.browse_view(3, &route, None);
        assert!(!view.detail_live && view.detail_nonready);
        let page = view.page.unwrap();
        let actions = playlist_detail_actions(&route, &page, false, false);
        assert!(actions.rules && !actions.play && !actions.queue && actions.offline_action.is_empty());
        assert!(ticket.ready(&page, false) && ticket.matches(&route, &page));
        assert_eq!(page.definition.as_ref(), Some(&current));
        assert_ne!(actions::prepare(&route, &page, "rules").unwrap().definition, "");
        let prior = personal.read(|_, replica| replica.editable_rules(&id).unwrap().clone());
        let command = PersonalCommand::SetPlaylistRules { playlist_id: id.clone(), definition: old };
        let copy = PersonalCommand::CopyPlaylist { playlist_id: id.clone(), name: "Copy".into(), description: String::new() };
        assert!(!hub.authorizes(3, 17, &copy));
        assert!(!personal.can_download_playlist(3, &id));
        assert!(!hub.admit_rules_edit(3, 17, &before, &command, || panic!("cached stale definition reached transport")));
        assert!(hub.admit_rules_edit(3, 17, &prior, &command, || true));
        let effect = Effect::Rules { id: id.clone(), definition: current, revision: before.revision };
        assert_eq!(personal.observed(before_status, &effect), Some(None), "a verified definition can confirm a write even before its rows are ready");
        hub.disconnect(3, 17);
        assert!(!hub.admit_rules_edit(3, 17, &prior, &command, || panic!("retired binding reached transport")));
        hub.begin(3, 18);
        hub.receive(3, 18, PersonalUpdate::Snapshot(Snapshot { playlists: vec![summary], recent: vec![] })).unwrap();
        assert!(!personal.browse_view(3, &route, None).page.unwrap().live_rules,
            "matching metadata on a replacement binding does not verify a definition");
    }

    #[test]
    fn row_edit_tickets_must_match_the_loaded_playlist_revision_and_capability() {
        let mut summary = editable_playlist();
        let list = pages::personal_list(&Route::Playlists, &[summary.clone()]);
        let row = &list.rows[0];
        let ticket = RowEditor::select(row, "rules").unwrap();
        let route = Route::Playlist(summary.id.clone());
        let detail = pages::personal_playlist(&summary, None, &[]);
        assert!(ticket.matches(&route, &detail));
        assert!(!ticket.matches(&Route::Playlist("other".into()), &detail));
        assert!(!ticket.matches(&Route::Playlists, &detail));
        summary.capabilities.edit_definition = false;
        assert!(!ticket.matches(&route, &pages::personal_playlist(&summary, None, &[])));
        summary.capabilities.edit_definition = true;
        summary.revision += 1;
        assert!(!ticket.matches(&route, &pages::personal_playlist(&summary, None, &[])));
        let mut wrong = row.clone();
        wrong.route = Some(Route::Playlist("other".into()));
        assert!(RowEditor::select(&wrong, "delete").is_none());
    }
}
fn draft_matches(b: &Option<EditBasis>, p: &Page, mode: &str) -> bool {
    b.as_ref().zip(p.playlist.as_ref()).is_some_and(|(basis, m)| {
        basis.op == mode && basis.id == m.id && basis.revision == m.revision
            && (mode != "rules" || p.live_rules && basis.rules.as_ref().is_some_and(|(_, prior)| {
                p.definition.as_ref() == Some(&prior.definition)
            }))
    })
}
fn clear_form_owned(ui: AppWindow) {
    clear_form(&ui);
}
fn clear_form(ui: &AppWindow) {
    ui.set_form_mode("".into());
    ui.set_form_name("".into());
    ui.set_form_description("".into());
    ui.set_form_value("".into());
    ui.set_form_help("".into());
    ui.set_form_error("".into());
    ui.set_form_busy(false);
}
pub fn start(
    ui: &AppWindow,
    epoch: Arc<AtomicU64>,
    mut access: watch::Receiver<Option<Access>>,
    engine: mpsc::Sender<Command>,
    art: watch::Sender<Option<crate::artwork::Request>>,
) -> (Desk, thread::JoinHandle<()>) {
    let (tx, mut inputs) = mpsc::channel(48);
    let (catalog_demand, mut demands) = watch::channel(None);
    let (results, mut updates) = mpsc::channel(8);
    let shown = Arc::new(AtomicU64::new(0));
    let desk = Desk {
        tx,
        catalog_demand,
        stamp: Arc::new(Stamp {
            epoch: epoch.clone(),
            shown: shown.clone(),
        }),
    };
    let window = ui.as_weak();
    let worker = thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("browser runtime");
        runtime.block_on(async move {
            let (pins, mut pin_updates) = playlist_pins::start(access.clone(), epoch.clone());
            let mut b=Browser {ui:window,epoch,shown,engine,access:None,route:INITIAL_ROUTE,page_keys:RandomState::new(),trail:Trail::default(),cache:Replica::default(),page:Arc::default(),catalog_visits:HashMap::new(),catalog_position:None,catalog_restore:None,title_visits:HashMap::new(),title_restore:None,add_songs_visits:HashSet::new(),add_songs_restore:false,add_songs_open:false,continuation:CatalogContinuation::default(),generation:0,serial:0,busy:false,silent_read:false,personal_ready:false,personal_due:None,personal_view:None,playlist_order:None,playlist_order_model_generation:None,playlist_order_serial:0,unconfirmed_orders:HashMap::new(),pending:None,edit_basis:None,row_editor:None,writing:false,message:String::new(),error:false,task:None,results,art,pins,pin_view:playlist_pins::View::default(),saved_reload_due:false,unpinning:false};
            let initial = access.borrow_and_update().clone();
            let mut personal_updates = initial.as_ref().map(|a| a.personal.changes());
            b.access_changed(initial);
            let mut availability = tokio::time::interval(Duration::from_secs(2));
            availability.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                let quick_due = b.access.as_ref().and_then(|a| a.personal.quick_view(a.epoch, |view| view.due));
                let due=b.personal_due.into_iter().chain(b.pending.as_ref().map(|p| p.due)).chain(quick_due).min();
                tokio::select! { biased;
                    change=access.changed()=>{
                        if change.is_err(){break;}
                        let next=access.borrow_and_update().clone();
                        if b.access.as_ref().map(|a| a.epoch) != next.as_ref().map(|a| a.epoch) {
                            personal_updates = next.as_ref().map(|a| a.personal.changes());
                        }
                        b.access_changed(next);
                    },
                    _=async {match due{Some(t)=>tokio::time::sleep_until(t).await,None=>std::future::pending().await}}=>b.personal_timeout(),
                    _=async { match &mut personal_updates { Some(w) => { let _ = w.changed().await; }, None => std::future::pending().await } }=>b.personal_changed(false),
                    Ok(())=pin_updates.changed()=>b.pins_changed(pin_updates.borrow_and_update().clone()),
                    Some(u)=updates.recv()=>b.result(u),
                    _=availability.tick()=>b.check_availability(),
                    input=inputs.recv()=>{let Some(i)=input else{break;};b.input(i);},
                    Ok(())=demands.changed()=>{
                        let demand = demands.borrow_and_update().clone();
                        if let Some(demand) = demand { b.demand_catalog(demand); }
                    },
                }
            }
            b.cancel();
        });
    });
    (desk, worker)
}
