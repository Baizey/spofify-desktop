//! Desktop browsing controller. A single owner accepts navigation and writes;
//! cancellable reads populate bounded account-scoped snapshots off the UI thread.
use crate::{
    AppWindow, ArtistCredit, BrowseRow, PlaylistActions, ReorderRow, actions,
    api::{Api, ApiError, Session},
    engine::Command,
    pages::{self, Page, Route},
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
}
struct Stamp {
    epoch: Arc<AtomicU64>,
    shown: Arc<AtomicU64>,
}
#[derive(Clone)]
pub struct Desk {
    tx: mpsc::Sender<Input>,
    stamp: Arc<Stamp>,
}
impl Desk {
    fn submit(&self, ui: &AppWindow, intent: Intent) {
        if self
            .tx
            .try_send(Input {
                epoch: self.stamp.epoch.load(Ordering::SeqCst),
                generation: self.stamp.shown.load(Ordering::SeqCst),
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
                let offline=val=="on";let current=access.borrow().clone(); if let Some(a)=current && a.epoch==d.stamp.epoch.load(Ordering::SeqCst){
                    mode.request(&a,offline);if offline {ui.set_manual_offline(true);} // Until confirmed, local playback and navigation stay offline.
                }return;
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
    SearchDownloads(Result<Page, ApiError>),
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
        "playlist" | "derived"
            if crate::api::catalog::segment_id(id) && !matches!(id, "sources" | "preview") =>
        {
            Some(Route::Playlist(id.into()))
        }
        _ => None,
    }
}

fn metadata_route(kind: &str, id: &str, offline: bool) -> Option<Route> {
    if offline || !matches!(kind, "album" | "artist") {
        return None;
    }
    pages::music_route(kind, id)
}

fn projected_music(credit: Option<&pages::Credit>, offline: bool, artist: bool) -> (&str, &str) {
    let Some(credit) = credit else {
        return ("", "");
    };
    let id = match (&credit.target, offline, artist) {
        (Some(Route::Artist(id)), false, true) | (Some(Route::Album(id)), false, false) => id,
        _ => "",
    };
    (id, &credit.name)
}

fn artist_credits(music: &pages::MusicMetadata, offline: bool) -> ModelRc<ArtistCredit> {
    ModelRc::new(VecModel::from(
        music
            .credits
            .iter()
            .map(|credit| {
                let (id, name) = projected_music(Some(credit), offline, true);
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
fn row_open_target<'a>(route: &Route, row: &'a pages::Row, offline: bool) -> Option<&'a Route> {
    // Track rows play only through an explicit Play intent, even in Saved.
    row.route.as_ref().filter(|target| {
        row.kind != "track" && *target != route && (!offline || saved_route(target))
    })
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
    if p.kind == "picker" || p.kind == "saved-collection" {
        return v;
    }
    if p.kind == "offline" {
        v.push(if pinned { "unpin-playlist" } else { "remove" });
        return v;
    }
    if p.kind == "playlists" {
        if let Some(m) = playlist_list_target(r) {
            let can_use_account = !offline && personal_actions;
            if can_use_account && r.playable && r.playback.is_some() {
                v.push("queue");
            }
            v.extend(playlist_pin_action(pinned, can_use_account));
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
    match route {
        Route::Playlist(id) if let Some(m) = page.playlist.as_ref().filter(|m| &m.id == id) => {
            let playback = can_use_account && page.playback.is_some();
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
        Route::SavedCollection(_) if page.kind == "saved-collection" => PlaylistActions {
            play: page.rows.iter().any(|r| r.playable),
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

// Only the playlist list and Saved rows need the keyed pin state. Playlist
// detail uses its own status line so the track model stays stable as it saves.
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
            Some(Route::Playlist(id)) if row.playlist.is_some() => {
                self.sources.get(id.as_str()).copied()
            }
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
        if models {
            self.generation += 1;
        }
        let order = self.refresh_playlist_order();
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
        let message = if !self.message.is_empty() {
            self.message.clone()
        } else if let Some(status) = pin.page(&route) {
            if p.status.is_empty() || p.status == "Pending" {
                status.label()
            } else {
                format!("{} · {}", status.label(), p.status)
            }
        } else {
            p.status.clone()
        };
        let (busy, error, back) = (
            self.busy && !self.silent_read,
            self.error,
            self.trail.can_back(),
        );
        let (account, offline) = self
            .access
            .as_ref()
            .map(|a| (a.session.username.clone(), a.offline))
            .unwrap_or_default();

        let local_playback =
            saved_route(&self.route) || offline && matches!(self.route, Route::Search(_));
        let personal_actions = !self.route.personal() || self.personal_ready;
        let write = self.writing;
        let art = self.art.clone();
        let route = self.route.clone();
        let request = self.access.as_ref().map(|a| crate::artwork::Request {
            epoch: a.epoch,
            generation: token,
            route: self.route.clone(),
            page: p.clone(),
        });
        self.scoped_ui(move |ui| {
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
            ui.set_page_status(message.into());
            ui.set_page_busy(busy);
            ui.set_page_error(error);
            ui.set_can_back(back);
            ui.set_has_more(p.more && !offline);
            ui.set_form_busy(write);
            if models {
                ui.set_browse_model_key(token.to_string().into());
                let pin = PinDisplay::new(&pin_view, access.as_ref());
                ui.set_page_credits(artist_credits(&p.music, offline));
                ui.set_browse_rows(ModelRc::new(VecModel::from(
                    p.rows
                        .iter()
                        .map(|r| {
                            let (album_id, album_name) =
                                projected_music(r.music.album.as_ref(), offline, false);
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
                                title: r.title.as_str().into(),
                                subtitle: pin_subtitle(r, status),
                                playlist_pinned: status.is_some(),
                                detail: r.detail.as_str().into(),
                                can_open: if matches!(route, Route::Picker(_)) {
                                    !offline && personal_actions
                                } else {
                                    row_open_target(&route, r, offline).is_some()
                                },
                                playable: r.playable
                                    && personal_actions
                                    && (!offline || local_playback),
                                credits: artist_credits(&r.music, offline),
                                album_id: album_id.into(),
                                album_name: album_name.into(),
                            }
                        })
                        .collect::<Vec<_>>(),
                )));
                let pinned = pin.page(&route).is_some();
                let mut actions = Vec::new();
                if kind == "offline" && p.rows.iter().any(|r| r.playable) {
                    actions.push("play");
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
                    match kind.as_str() {
                        "playlists" => actions.extend(["new-manual", "new-rules"]),
                        "picker" => actions.push("new-manual"),
                        _ => {}
                    }
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
            if saved_route(&self.route) {
                if self.writing {
                    self.saved_reload_due = true;
                } else {
                    self.visit(Navigation::Untracked(self.route.clone()), false);
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
        let offline = self.access.as_ref().is_some_and(|a| a.offline);
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
        if old.map(|r| r.label()) != new.map(|r| r.label()) {
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
            self.visit(Navigation::Untracked(INITIAL_ROUTE), false);
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
            // Downloads remain the offline landing page, but don't abandon
            // a downloaded collection or a query that can continue locally.
            let jump_to_downloads =
                offline && !saved_route(&self.route) && !matches!(self.route, Route::Search(_));
            self.visit(
                if jump_to_downloads {
                    Navigation::Follow(Route::Offline)
                } else {
                    Navigation::Untracked(self.route.clone())
                },
                false,
            );
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
                    self.visit(Navigation::Untracked(next), false);
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
            let (message, error) = if !online {
                ("Work online to refresh your account library.", false)
            } else if unavailable {
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

    fn visit(&mut self, navigation: Navigation, more: bool) {
        if self.writing {
            return;
        }
        let back = matches!(navigation, Navigation::Back);
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
        if next.personal() {
            let previous = std::mem::replace(&mut self.route, next);
            if previous != self.route {
                self.page = Arc::default();
            }
            self.personal_changed(true);
            return;
        }
        let offline_search =
            self.access.as_ref().is_some_and(|a| a.offline) && matches!(next, Route::Search(_));
        let cached = if offline_search {
            None
        } else {
            self.cache.get(&next)
        };
        let have = cached.is_some();
        let fresh = cached.as_ref().is_some_and(|(_, f)| *f);
        let prior_page =
            std::mem::replace(&mut self.page, cached.map_or_else(Arc::default, |(p, _)| p));
        self.route = next.clone();
        let Some(a) = self.access.clone().filter(Access::usable) else {
            self.render(true);
            return;
        };
        if offline_search && let Route::Search(q) = &next {
            if q.is_empty() {
                self.page = Arc::new(pages::search_downloads(&Page::default(), q));
            } else if let Some((downloads, true)) = self.cache.get(&Route::Offline) {
                self.page = Arc::new(pages::search_downloads(&downloads, q));
            } else {
                self.page = Arc::new(pages::search_downloads(&Page::default(), q));
                let tx = self.results.clone();
                let serial = self.serial;
                self.busy = true;
                self.task = Some(tokio::spawn(async move {
                    // Coalesce typing before opening the bounded local indexes.
                    tokio::time::sleep(SEARCH_DELAY).await;
                    let result = Update::SearchDownloads(downloads_page(&a).await);
                    let _ = tx.send(Delivery { serial, result }).await;
                }));
            }
            self.render(true);
            return;
        }
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
        if a.offline && !saved_route(&next) {
            self.message = if have {
                "Showing the items from your last visit."
            } else {
                "Work online to open this collection."
            }
            .into();
            self.render(true);
            return;
        }
        if fresh && !more {
            self.render(true);
            return;
        }
        let prior = if more { Some(self.page.clone()) } else { None };
        let depth = prior.as_ref().map_or(0, |page| {
            (page.rows.len() + crate::api::PAGE_SIZE as usize).min(pages::MAX_ROWS)
        });
        let tx = self.results.clone();
        let serial = self.serial;
        self.busy = true;
        self.silent_read = have && !more;
        self.render(changed || !have || !Arc::ptr_eq(&prior_page, &self.page));
        self.task = Some(tokio::spawn(async move {
            if matches!(next, Route::Search(_)) && !more {
                tokio::time::sleep(SEARCH_DELAY).await;
            }
            let mut rebuilding = false;
            for attempt in 0..=4 {
                if !a.usable() {
                    return;
                }
                let mut result = match &next {
                    Route::Offline => downloads_page(&a).await,
                    Route::SavedCollection(id) => playlist_browser::detail(&a, id).await,
                    _ if rebuilding => rebuild_for(&a, &next, depth).await,
                    _ => pages::load(&a, &next, prior.as_deref()).await,
                };
                if !rebuilding
                    && prior.is_some()
                    && result.as_ref().is_err_and(pages::continuation_changed)
                {
                    rebuilding = true;
                    result = rebuild_for(&a, &next, depth).await;
                }
                let retry = attempt < 4 && result.as_ref().err().is_some_and(recoverable);
                if retry {
                    let result = match result {
                        Ok(p) => Update::Read(Ok(p), true),
                        Err(_) => Update::ReadGap,
                    };
                    if tx.send(Delivery { serial, result }).await.is_err() {
                        return;
                    }
                    tokio::time::sleep(Duration::from_secs(1 << attempt)).await;
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
    fn input(&mut self, i: Input) {
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
                    self.visit(navigation, false);
                }
                return;
            }
            Intent::Search(q) => {
                if q.len() <= 1000 {
                    self.visit(Navigation::Follow(Route::Search(q.trim().into())), false);
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
                            self.visit(
                                Navigation::Follow(Route::Playlist(editor.id.clone())),
                                false,
                            );
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
        if op == "more" {
            if page.more && self.task.is_none() && !a.offline {
                self.visit(Navigation::Untracked(route), true);
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
        if route.personal()
            && !self.personal_ready
            && !(unpin
                || matches!(op.as_str(), "album" | "artist")
                || op == "open" && !matches!(route, Route::Picker(_)))
        {
            self.message = "Wait for the current account library before making changes.".into();
            self.render(false);
            return;
        }
        if let Some(kind) = op.strip_prefix("context-") {
            if !offline && let Some(target) = context_route(kind, &val) {
                self.visit(Navigation::Follow(target), false);
            }
            return;
        }
        if matches!(op.as_str(), "album" | "artist") {
            if let Some(target) =
                metadata_route(&op, &val, offline).filter(|target| *target != route)
            {
                self.visit(Navigation::Follow(target), false);
            }
            return;
        }
        let saved_page = saved_route(&route);
        let local_results = saved_page || a.offline && matches!(route, Route::Search(_));
        if op == "open" && !matches!(route, Route::Picker(_)) {
            if let Some(target) = row
                .as_ref()
                .and_then(|r| row_open_target(&route, r, offline))
            {
                self.visit(Navigation::Follow(target.clone()), false);
            }
            return;
        }
        if op == "add" && !matches!(route, Route::Picker(_)) {
            if let Some(r) = row
                && !offline
            {
                self.visit(Navigation::Follow(Route::Picker(r.id)), false);
            }
            return;
        }
        let local_play = local_results && op == "play";
        if matches!(op.as_str(), "play" | "queue") && row.as_ref().is_some_and(|r| !r.playable) {
            return;
        }
        if matches!(op.as_str(), "play" | "queue") && !local_play {
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
        if a.offline && !local_play && !remove_saved {
            return;
        }
        if val.len() > 48 * 1024 {
            self.message = "The playlist description or rules are too long.".into();
            self.render(false);
            return;
        }
        if !local_play && !save && !remove_saved {
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
        self.message = if local_play {
            "Opening saved audio…"
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
        self.task = Some(tokio::spawn(async move {
            let result = if local_play {
                let i = idx
                    .or_else(|| page.rows.iter().position(|r| r.playable))
                    .unwrap_or(0);
                Update::Local(saved::play(&a, &page, i).await)
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
        if !matches!(&d.result, Update::Read(_, true) | Update::ReadGap) {
            self.task = None;
        }
        self.busy = false;
        self.writing = false;
        let unpinning = std::mem::take(&mut self.unpinning);
        self.error = false;
        self.message.clear();
        match d.result {
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
            Update::SearchDownloads(result) => match result {
                Ok(downloads) => {
                    if let Route::Search(query) = &self.route {
                        self.page = Arc::new(pages::search_downloads(&downloads, query));
                        self.cache.put(Route::Offline, Arc::new(downloads));
                        self.render(true);
                    }
                }
                Err(e) => {
                    self.error = true;
                    self.message = readable(&e);
                    self.render(false);
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
                self.visit(
                    Navigation::Untracked(route.unwrap_or_else(|| self.route.clone())),
                    false,
                );
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
                    self.visit(Navigation::Untracked(Route::Offline), false);
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
                    self.visit(Navigation::Follow(Route::Offline), false);
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
                if std::mem::take(&mut self.saved_reload_due) && saved_route(&self.route) {
                    self.visit(Navigation::Untracked(self.route.clone()), false);
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

/// A continuation is scoped to a point-in-time result set. If it expires, start
/// at the first page and follow only its new cursors until the requested depth.
async fn rebuild_for(a: &Access, r: &Route, depth: usize) -> Result<Page, ApiError> {
    rebuild(depth, |old: Option<Page>| async move {
        pages::load(a, r, old.as_ref()).await
    })
    .await
}
async fn rebuild<F, Fut>(depth: usize, mut fetch: F) -> Result<Page, ApiError>
where
    F: FnMut(Option<Page>) -> Fut,
    Fut: Future<Output = Result<Page, ApiError>>,
{
    let mut page = fetch(None).await?;
    for _ in 0..pages::MAX_ROWS {
        if !page.more || page.rows.len() >= depth {
            break;
        }
        let count = page.rows.len();
        let next = fetch(Some(page)).await?;
        if next.rows.len() <= count {
            return Err(unavailable("The library could not load the next items."));
        }
        page = next;
    }
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

    #[tokio::test]
    async fn expired_continuation_rebuilds_previous_depth_plus_one_new_page() {
        let mut offsets = Vec::new();
        let p = rebuild(60, |old: Option<Page>| {
            let mut p = old.unwrap_or_default();
            let n = p.rows.len();
            offsets.push(n);
            p.rows.resize_with(n + 20, Default::default);
            p.more = true;
            std::future::ready(Ok(p))
        })
        .await
        .unwrap();
        assert_eq!(offsets, [0, 20, 40]);
        assert_eq!(p.rows.len(), 60);
    }
    #[tokio::test]
    async fn rebuild_stops_at_actual_end_and_rejects_stuck_page() {
        let p = rebuild(60, |_| std::future::ready(Ok(Page::default())))
            .await
            .unwrap();
        assert!(p.rows.is_empty());
        assert!(
            rebuild(60, |_| std::future::ready(Ok(Page {
                more: true,
                ..Default::default()
            })))
            .await
            .is_err()
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
    fn metadata_targets_are_explicit_online_routes_only() {
        assert_eq!(
            metadata_route("artist", "ar%2F", false),
            Some(Route::Artist("ar%2F".into()))
        );
        assert_eq!(
            metadata_route("album", "al", false),
            Some(Route::Album("al".into()))
        );
        for bad in ["", " ", "a/b", "a\\b", ".."] {
            assert_eq!(metadata_route("artist", bad, false), None);
            assert_eq!(metadata_route("album", bad, false), None);
        }
        assert_eq!(metadata_route("artist", "ar", true), None);
        let credit = pages::Credit {
            name: "Artist".into(),
            target: Some(Route::Artist("ar".into())),
        };
        assert_eq!(
            projected_music(Some(&credit), false, true),
            ("ar", "Artist")
        );
        assert_eq!(projected_music(Some(&credit), true, true), ("", "Artist"));
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
        for offline in [false, true] {
            assert_eq!(row_open_target(&Route::Offline, &track, offline), None);
            assert_eq!(row_open_target(&target, &track, offline), None);
            assert_eq!(
                row_open_target(&Route::Offline, &collection, offline),
                Some(&target)
            );
        }
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
        assert!(legacy.play && !legacy.queue);
        assert_eq!(legacy.remove_action, "remove");
        assert!(
            playlist_detail_actions(&route, &Page::default(), false, false)
                .remove_action
                .is_empty()
        );
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
    let (results, mut updates) = mpsc::channel(8);
    let shown = Arc::new(AtomicU64::new(0));
    let desk = Desk {
        tx,
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
            let mut b=Browser {ui:window,epoch,shown,engine,access:None,route:INITIAL_ROUTE,page_keys:RandomState::new(),trail:Trail::default(),cache:Replica::default(),page:Arc::default(),generation:0,serial:0,busy:false,silent_read:false,personal_ready:false,personal_due:None,personal_view:None,playlist_order:None,playlist_order_model_generation:None,playlist_order_serial:0,unconfirmed_orders:HashMap::new(),pending:None,edit_basis:None,row_editor:None,writing:false,message:String::new(),error:false,task:None,results,art,pins,pin_view:playlist_pins::View::default(),saved_reload_due:false,unpinning:false};
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
                }
            }
            b.cancel();
        });
    });
    (desk, worker)
}
