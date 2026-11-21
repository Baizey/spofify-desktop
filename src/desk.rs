//! Desktop browsing controller. A single owner accepts navigation and writes;
//! cancellable reads populate bounded account-scoped snapshots off the UI thread.
use crate::{
    AppWindow, ArtistCredit, BrowseRow, PlaylistActions, PlaylistOfflineState,
    PlaylistSourceRestore, ReorderRow, actions,
    api::{Api, ApiError, Session},
    engine::Command,
    pages::{self, Page, Route, RowKey},
    personal_bridge::{Effect, Link, Mutation, PersonalAccess, Status},
    player, playlist_browser, playlist_pins,
    replica::{Replica, Trail},
    saved,
};
use slint::{ComponentHandle, Model, ModelRc, VecModel};
use std::{
    collections::HashMap,
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
}
enum Intent {
    Navigate(String),
    Search(String),
    Row(String, usize),
    Page(String, String),
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
}

struct CatalogPosition {
    y: f32,
    anchor: i32,
    anchor_offset: f32,
    row: i32,
    part: String,
    credit: String,
}

#[derive(Default)]
struct CatalogContinuation {
    loading: bool,
    stopped: bool,
    status: String,
}

struct CatalogVisit {
    page: Arc<Page>,
    position: CatalogPosition,
    continuation: CatalogContinuation,
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
                    anchor_offset,
                    row: row.unwrap_or(-1),
                    part: row.map_or_else(String::new, |_| ui.get_catalog_focused_part().into()),
                    credit: row
                        .map_or_else(String::new, |_| ui.get_catalog_focused_credit().into()),
                }
            });
        if self
            .tx
            .try_send(Input {
                epoch: self.stamp.epoch.load(Ordering::SeqCst),
                generation: self.stamp.shown.load(Ordering::SeqCst),
                catalog_position: position,
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
    LocalRead(u64, Result<Page, ApiError>, Option<CatalogPosition>),
    Storage(Result<String, ApiError>),
    Changed(Result<Option<Route>, ApiError>),
    Local(Result<Command, ApiError>),
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
    edit_basis: Option<(String, String, u64)>, // form, playlist, original revision
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
        Route::Offline => "Saved on this device",
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
                trail.visit(current, &next);
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
        (Some(Route::Artist(id)), true) | (Some(Route::Album(id)), false) => id,
        _ => "",
    };
    (id, &credit.name)
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
    fn matches(&self, route: &Route, page: &Page) -> bool {
        matches!(route, Route::Playlist(id) if id == &self.id)
            && page.playlist.as_ref().is_some_and(|m| {
                m.id == self.id && m.revision == self.revision && can_edit_playlist(m, self.op)
            })
    }
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
            if !pinned && can_use_account {
                v.push("pin-playlist");
            }
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
                rules: can_use_account
                    && page.definition.is_some()
                    && can_edit_playlist(m, "rules"),
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
        let _ = slint::invoke_from_event_loop(move || {
            if shared.load(Ordering::SeqCst) == epoch
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
        let catalog_request_key = self.catalog_request_key();
        let catalog_status = self.continuation.status.clone();
        let catalog_loading = self.continuation.loading;
        let catalog_restore = self.catalog_restore.take();
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
            let switching_playlist_source = models
                && matches!(route, Route::Playlist(_))
                && ui.get_page_key().as_str() == key
                && ui.get_browse_local_audio() != p.local_audio;
            let position = switching_playlist_source.then(|| {
                let focused = ui.get_playlist_focus_index();
                let occurrence = usize::try_from(focused)
                    .ok()
                    .and_then(|i| ui.get_browse_rows().row_data(i))
                    .map(|row| row.occurrence_key)
                    .filter(|key| !key.is_empty());
                (ui.get_catalog_scroll_y(), focused, occurrence)
            });
            // Invalidate a captured gesture before changing its view, then publish the
            // new scope after both row models and the shown generation are installed.
            if order.is_some() {
                ui.set_playlist_order_scope(Default::default());
            }
            ui.set_account_label(account.into());
            ui.set_manual_offline(offline);
            if kind == "now" && ui.get_page_kind() != "now" {
                // Record reopening with the route, before the media observer runs.
                ui.set_lyrics_open_generation(ui.get_lyrics_open_generation().wrapping_add(1));
            }
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
                        let (album_id, album_name) = projected_music(r.music.album.as_ref(), false);
                        let status = if matches!(kind.as_str(), "playlists" | "offline") {
                            pin.row(r)
                        } else {
                            None
                        };
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
                            occurrence_key: match &r.key {
                                RowKey::ManualEntry { entry_id, .. } => entry_id.as_str().into(),
                                RowKey::DerivedOccurrence { occurrence_id, .. } => {
                                    occurrence_id.as_str().into()
                                }
                                RowKey::Track(_)
                                    if matches!(
                                        route,
                                        Route::Playlist(_) | Route::SavedCollection(_)
                                    ) =>
                                {
                                    r.position
                                        .map(|pos| format!("saved:{pos}:{}", r.id).into())
                                        .unwrap_or_default()
                                }
                                _ => Default::default(),
                            },
                            title: r.title.as_str().into(),
                            subtitle: pin_subtitle(r, status),
                            playlist_pinned: status.is_some(),
                            detail: r.detail.as_str().into(),
                            can_open: if matches!(route, Route::Picker(_)) {
                                !offline && personal_actions
                            } else {
                                row_open_target(&route, r).is_some()
                            },
                            playable: r.playable && can_play_rows,
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
                    let restore = position.map(|(y, before, occurrence)| {
                        let after = occurrence
                            .and_then(|key| rows.iter().position(|r| r.occurrence_key == key))
                            .and_then(|i| i32::try_from(i).ok())
                            .unwrap_or(-1);
                        (y, before, after)
                    });
                    ui.set_browse_model_key(token.to_string().into());
                    ui.set_browse_local_audio(p.local_audio);
                    ui.set_page_credits(artist_credits(&p.music));
                    ui.set_browse_rows(ModelRc::new(VecModel::from(rows)));
                    if let Some((y, before, after)) = restore {
                        ui.set_playlist_source_restore(PlaylistSourceRestore {
                            y,
                            before,
                            after,
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
        });
    }
    fn pins_changed(&mut self, view: playlist_pins::View) {
        let old = std::mem::replace(&mut self.pin_view, view);
        if self.access.as_ref().map(|a| a.epoch) != self.pin_view.epoch {
            return;
        }
        let structural = old.epoch != self.pin_view.epoch || old.revision != self.pin_view.revision;
        if structural {
            self.cache.saved_changed();
            if saved_route(&self.route)
                || self.page.local_audio
                    && matches!(self.route, Route::Playlists | Route::Playlist(_))
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
                    (actions.is_some() || old.map(|r| r.label()) != new.map(|r| r.label()))
                        .then(|| (i, pin_subtitle(row, new), actions))
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
                for (index, text, actions) in changes {
                    if let Some(mut row) = rows.row_data(index) {
                        row.subtitle = text;
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
    }
    fn reset(&mut self, access: Option<Access>) {
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
            .is_some_and(|(a, b)| a.epoch == b.epoch && Arc::ptr_eq(&a.api, &b.api));
        if !same {
            self.reset(next);
            return;
        }
        let old = self.access.replace(next.unwrap()).unwrap();
        let a = self.access.as_ref().unwrap();
        let offline = a.offline;
        if old.offline != offline {
            self.catalog_visits.clear();
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
                || self.edit_basis.as_ref().is_some_and(|(_, id, revision)| {
                    replica.playlist().is_some_and(|detail| {
                        !detail.stale
                            && detail.summary.id == *id
                            && detail.summary.revision == *revision
                    })
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
        if self
            .engine
            .try_send(Command::Personal {
                epoch: access.epoch,
                generation: before.generation,
                command: mutation.command,
            })
            .is_err()
        {
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
            return;
        }
        let Some(a) = self.access.clone().filter(Access::usable) else {
            self.render(visiting);
            return;
        };
        if a.local() {
            return; // Never republish server playback or rows into a local projection.
        }
        let route = self.route.clone();
        let (status, next, detail_live, unavailable) = a.personal.read(|status, replica| {
            let detail_live = replica.playlist().is_some_and(|p| !p.stale);
            let page = (status.epoch == a.epoch)
                .then(|| pages::personal_projection(&route, replica))
                .flatten();
            (
                status,
                page,
                detail_live,
                matches!(route, Route::Playlist(_)) && replica.playlist_unavailable(),
            )
        });
        let prior = self.personal_view.replace(status);
        let online = !a.offline && !a.session.is_offline();
        let prior_ready = self.personal_ready;
        self.personal_ready = online
            && status.epoch == a.epoch
            && status.link == Link::Ready
            && next.is_some()
            && (!matches!(route, Route::Playlist(_)) || detail_live);
        let models_changed = if let Some(next) = next {
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
            if let Some(observed) = a.personal.observed(before, &pending.effect) {
                self.pending = None;
                self.writing = false;
                self.busy = false;
                self.edit_basis = None;
                self.scoped_ui(clear_form_owned);
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
        let waiting = online
            && matches!(status.link, Link::Waiting | Link::Ready)
            && !self.personal_ready
            && !unavailable;
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
                    "The playlist request was not sent. Open it again to retry.",
                    true,
                )
            } else if self.personal_ready {
                ("", false)
            } else if status.link == Link::Gap {
                (
                    "The account update was interrupted. Waiting for a fresh snapshot.",
                    true,
                )
            } else if status.link == Link::Disconnected {
                (
                    "The account library is disconnected. Reconnect to refresh it.",
                    true,
                )
            } else if self.personal_due.is_some() {
                ("Waiting for the account library…", false)
            } else {
                (
                    "The account library did not finish loading. Open this page again to retry.",
                    true,
                )
            };
            self.message = message.into();
            self.error = error;
        }
        self.render(render_models);
        if self.personal_ready {
            self.finish_row_editor();
        } else if unavailable || self.personal_due.is_none() {
            self.row_editor = None;
        }
    }

    fn personal_timeout(&mut self) {
        let now = tokio::time::Instant::now();
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

    fn visit(&mut self, navigation: Navigation) {
        if self.writing {
            return;
        }
        let back = matches!(navigation, Navigation::Back);
        let follow = matches!(navigation, Navigation::Follow(_));
        let previous_top = self.trail.top_id();
        let Some(next) = navigation.resolve(
            &self.route,
            &mut self.trail,
            self.task.is_some(),
            self.error,
        ) else {
            return;
        };
        self.row_editor = None;
        let changed = next != self.route;
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
            && self.route.catalog_continuity()
            && !self.page.rows.is_empty()
            && let Some(position) = self.catalog_position.take()
        {
            self.catalog_visits.insert(
                entry,
                CatalogVisit {
                    page: self.page.clone(),
                    position,
                    continuation: std::mem::take(&mut self.continuation),
                },
            );
        }
        let returned =
            if back || (follow && previous_top.is_some_and(|id| !self.trail.contains_id(id))) {
                previous_top.and_then(|id| self.catalog_visits.remove(&id))
            } else {
                None
            };
        self.catalog_visits
            .retain(|id, _| self.trail.contains_id(*id));
        self.continuation = CatalogContinuation::default();
        let retry_personal = !changed && self.error && matches!(next, Route::Playlist(_));
        if back && let Route::Search(q) = &next {
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
            if let Some((generation, command)) = a.personal.watch_playlist(a.epoch, wanted)
                && !a.offline
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
                self.message = "The playlist request was not sent. Open it again to retry.".into();
            }
        }
        self.personal_due = None;
        self.personal_ready = false;
        self.personal_view = None;
        if let Some(a) = self.access.clone().filter(Access::local)
            && !matches!(next, Route::Now | Route::Queue | Route::Appearance)
        {
            self.visit_local(a, next, returned.map(|visit| visit.position));
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
        if let Some(visit) = returned
            && !visit.page.local_audio
            && self.access.as_ref().is_some_and(|a| !a.local())
        {
            self.route = next;
            self.page = visit.page;
            self.continuation = visit.continuation;
            self.catalog_restore = Some(visit.position);
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
        self.task = Some(tokio::spawn(async move {
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
                .map(|mut page| {
                    page.local_audio = saved_route(&next);
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
    fn visit_local(&mut self, access: Access, next: Route, restore: Option<CatalogPosition>) {
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
        if !retain_view {
            self.page = Arc::new(Page {
                local_audio: true,
                ..Page::default()
            });
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
        self.render(!retain_view);
        self.task = Some(tokio::spawn(async move {
            if matches!(next, Route::Search(_)) && !current {
                tokio::time::sleep(SEARCH_DELAY).await;
            }
            let known = known
                .iter()
                .map(|(route, page)| (route, page.as_ref()))
                .collect::<Vec<_>>();
            let result = match &next {
                Route::Playlists | Route::Playlist(_) => {
                    playlist_browser::offline_page(&access, &next).await
                }
                Route::Offline => downloads_page(&access).await,
                Route::SavedCollection(id) => playlist_browser::detail(&access, id).await,
                Route::History | Route::Search(_) | Route::Album(_) | Route::Artist(_) => {
                    saved::inventory(&access).await.map(|records| {
                        catalog_page(&next, &records, &known, history.as_ref(), false)
                    })
                }
                _ => Ok(Page::default()),
            }
            .map(|mut page| {
                page.local_audio = true;
                if matches!(
                    next,
                    Route::Playlist(_) | Route::SavedCollection(_) | Route::Offline
                ) {
                    let items = page
                        .rows
                        .iter()
                        .filter_map(|row| row.item.as_ref())
                        .map(|item| (item.id.clone(), item.clone()))
                        .collect();
                    crate::local_catalog::attach_track_links(
                        &mut page.rows,
                        &items,
                        &known,
                        history.as_ref(),
                    );
                }
                page
            });
            let _ = tx
                .send(Delivery {
                    serial,
                    result: Update::LocalRead(mode, result, restore),
                })
                .await;
        }));
    }

    fn finish_row_editor(&mut self) {
        if self.personal_ready
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
        let draft = if op == "delete" {
            Ok(actions::Draft::default())
        } else {
            actions::prepare(&self.route, &self.page, op)
        };
        match draft {
            Ok(draft) => {
                self.edit_basis = Some((op.to_owned(), playlist.id.clone(), playlist.revision));
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
            i.catalog_position.take()
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
        let offline = a.offline || a.session.is_offline();
        if let Some(mode) = op.strip_prefix("edit-") {
            if !offline && self.personal_ready && PLAYLIST_EDITS.contains(&mode) {
                match (&route, row.as_ref()) {
                    (Route::Playlists, Some(row)) => {
                        if let Some(editor) = RowEditor::select(row, mode) {
                            self.visit(Navigation::Follow(Route::Playlist(editor.id.clone())));
                            if self.personal_ready || self.personal_due.is_some() {
                                self.row_editor = Some(editor);
                                self.finish_row_editor();
                            }
                        }
                    }
                    (Route::Playlist(_), None) => self.open_playlist_editor(mode),
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
        let local_action = page.local_audio
            && (saved_route(&route) || offline)
            && matches!(op.as_str(), "play" | "queue");
        if route.personal()
            && !self.personal_ready
            && !(unpin
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
                self.visit(Navigation::Follow(target));
            }
            return;
        }
        let saved_page = saved_route(&route);
        if op == "open" && !matches!(route, Route::Picker(_)) {
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
            return;
        }
        if matches!(op.as_str(), "play" | "queue") && !local_action {
            if offline {
                return;
            }
            let selection = match row {
                Some(r) => r.playback,
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
                let result = Update::PinChanged(pins.set(&a, target, wanted).await);
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
                        // This confirmation was for a legacy snapshot. A modern
                        // pin is never removed through this fallback path.
                        playlist_browser::remove_legacy(&a, id)
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
    fn result(&mut self, d: Delivery) {
        if d.serial != self.serial
            || !self
                .access
                .as_ref()
                .is_some_and(|a| a.epoch == self.epoch.load(Ordering::SeqCst))
        {
            return;
        }
        if let Update::LocalRead(mode, _, _) = &d.result
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
            Update::CatalogAppend(..) => unreachable!("handled above"),
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
            Update::LocalRead(_, result, restore) => match result {
                Ok(page) => {
                    let changed = *self.page != page;
                    self.page = Arc::new(page);
                    self.catalog_restore = restore;
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
            Update::Local(Ok(command)) => {
                self.playback(command);
                if std::mem::take(&mut self.saved_reload_due) && self.page.local_audio {
                    self.visit(Navigation::Untracked(self.route.clone()));
                } else {
                    self.render(false);
                }
            }
            Update::Local(Err(e)) => {
                self.message = readable(&e);
                self.error = true;
                self.render(false);
            }
        }
    }
}
const SEARCH_DELAY: Duration = Duration::from_millis(240);

fn retained_history(access: &Access) -> Option<Page> {
    access.personal.read(|status, replica| {
        (status.epoch == access.epoch)
            .then(|| pages::personal_projection(&Route::History, replica))
            .flatten()
    })
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
        Route::Playlist(_) => playlist_browser::offline_page(access, route).await?,
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
    fn step(route: &mut Route, trail: &mut Trail, navigation: Navigation) {
        *route = navigation.resolve(route, trail, false, false).unwrap();
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
                Navigation::Back.resolve(&route, &mut trail, false, false),
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
            .resolve(&Route::Appearance, &mut trail, false, false)
            .unwrap();
        assert!(trail.can_back());
        let root = Navigation::named("offline").unwrap();
        assert_eq!(root.resolve(&saved, &mut trail, true, false), Some(saved));
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
        let basis = Some(("rename".to_string(), "p".to_string(), 7));
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
    fn playlist_list_menu_offers_only_actions_for_that_row_and_mode() {
        let list = pages::personal_list(&Route::Playlists, &[editable_playlist()]);
        let row = &list.rows[0];
        assert_eq!(
            row_actions(&list, row, false, true, false),
            ["queue", "pin-playlist", "rename", "rules", "delete"]
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
        assert_eq!(
            row_actions(&list, &list.rows[0], false, true, false),
            ["pin-playlist"]
        );
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
        let pinned = playlist_detail_actions(&route, &detail, true, true);
        assert!(pinned.rules);
        assert_eq!(pinned.offline_action, "unpin-playlist");
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
        let legacy = playlist_detail_actions(&route, &saved, false, false);
        assert!(legacy.play && legacy.queue);
        assert_eq!(legacy.remove_action, "remove");
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
fn draft_matches(b: &Option<(String, String, u64)>, p: &Page, mode: &str) -> bool {
    b.as_ref()
        .zip(p.playlist.as_ref())
        .is_some_and(|((op, id, rev), m)| op == mode && id == &m.id && *rev == m.revision)
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
            let mut b=Browser {ui:window,epoch,shown,engine,access:None,route:INITIAL_ROUTE,page_keys:RandomState::new(),trail:Trail::default(),cache:Replica::default(),page:Arc::default(),catalog_visits:HashMap::new(),catalog_position:None,catalog_restore:None,continuation:CatalogContinuation::default(),generation:0,serial:0,busy:false,silent_read:false,personal_ready:false,personal_due:None,personal_view:None,playlist_order:None,playlist_order_model_generation:None,playlist_order_serial:0,unconfirmed_orders:HashMap::new(),pending:None,edit_basis:None,row_editor:None,writing:false,message:String::new(),error:false,task:None,results,art,pins,pin_view:playlist_pins::View::default(),saved_reload_due:false,unpinning:false};
            let initial = access.borrow_and_update().clone();
            let mut personal_updates = initial.as_ref().map(|a| a.personal.changes());
            b.access_changed(initial);
            loop {
                let due=b.personal_due.into_iter().chain(b.pending.as_ref().map(|p| p.due)).min();
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
