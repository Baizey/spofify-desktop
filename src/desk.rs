//! Desktop browsing controller. A single owner accepts navigation and writes;
//! cancellable reads populate bounded account-scoped snapshots off the UI thread.
use crate::{
    AppWindow, ArtistCredit, BrowseRow, actions,
    api::{Api, ApiError, Session},
    engine::Command,
    pages::{self, Page, Route},
    personal_bridge::{Effect, Link, Mutation, PersonalAccess, Status},
    player,
    replica::{Replica, Trail},
    saved,
};
use slint::{ComponentHandle, ModelRc, VecModel};
use std::{
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
    pending: Option<Pending>,
    edit_basis: Option<(String, String, u64)>, // form, playlist, original revision
    writing: bool,
    message: String,
    error: bool,
    task: Option<JoinHandle<()>>,
    results: mpsc::Sender<Delivery>,
    art: watch::Sender<Option<crate::artwork::Request>>,
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
        Route::Devices => "Devices",
        Route::Album(_) => "Album",
        Route::Artist(_) => "Artist",
        Route::Playlist(_) => "Playlist",
        Route::SavedCollection(_) => "Saved collection",
        Route::Track(_) => "Track",
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
            "devices" => Self::Follow(Route::Devices),
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
fn strings(v: Vec<&str>) -> ModelRc<slint::SharedString> {
    ModelRc::new(VecModel::from(
        v.into_iter()
            .map(slint::SharedString::from)
            .collect::<Vec<_>>(),
    ))
}
fn row_actions(p: &Page, r: &pages::Row, i: usize, offline: bool) -> Vec<&'static str> {
    let mut v = Vec::new();
    if p.kind == "picker" {
        return v;
    }
    if matches!(p.kind.as_str(), "offline" | "saved-collection") {
        if p.kind == "offline" {
            v.push("remove");
        }
        return v;
    }
    if offline {
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
        if i > 0 {
            v.push("up");
        }
        if i + 1 < p.rows.len() {
            v.push("down");
        }
    }
    v
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
    fn render(&mut self, models: bool) {
        if models {
            self.generation += 1;
        }
        let token = self.generation;
        let shown = self.shown.clone();
        let p = self.page.clone();
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
        let request = self.access.as_ref().map(|a| crate::artwork::Request {
            epoch: a.epoch,
            generation: token,
            route: self.route.clone(),
            page: p.clone(),
        });
        self.scoped_ui(move |ui| {
            ui.set_account_label(account.into());
            ui.set_manual_offline(offline);
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
                let (album_id, album_name) =
                    projected_music(p.music.album.as_ref(), offline, false);
                ui.set_page_credits(artist_credits(&p.music, offline));
                ui.set_page_album_id(album_id.into());
                ui.set_page_album_name(album_name.into());
                ui.set_browse_rows(ModelRc::new(VecModel::from(
                    p.rows
                        .iter()
                        .enumerate()
                        .map(|(i, r)| {
                            let (album_id, album_name) =
                                projected_music(r.music.album.as_ref(), offline, false);
                            BrowseRow {
                                section: Default::default(),
                                actions: if personal_actions {
                                    strings(row_actions(&p, r, i, offline))
                                } else {
                                    strings(Vec::new())
                                },
                                kind: r.kind.as_str().into(),
                                title: r.title.as_str().into(),
                                subtitle: r.subtitle.as_str().into(),
                                detail: r.detail.as_str().into(),
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
                let mut actions = Vec::new();
                if kind == "saved-collection" {
                    actions.push("remove");
                }
                if matches!(kind.as_str(), "saved-collection" | "offline")
                    && p.rows.iter().any(|r| r.playable)
                {
                    actions.push("play");
                }
                if !offline && personal_actions {
                    if p.playback.is_some() {
                        actions.extend(["play", "queue"]);
                    }
                    if matches!(
                        kind.as_str(),
                        "album" | "artist" | "playlist-manual" | "playlist-rules"
                    ) {
                        actions.push("save");
                    }
                    match kind.as_str() {
                        "playlists" => actions.extend(["new-manual", "new-rules"]),
                        "picker" => actions.push("new-manual"),
                        _ => {}
                    }
                    if let Some(m) = &p.playlist {
                        let c = &m.capabilities;
                        if c.edit_metadata {
                            actions.push("rename");
                        }
                        if c.edit_definition {
                            actions.push("rules");
                        }
                        if c.delete {
                            actions.push("delete");
                        }
                    }
                }
                let (options, primary) = actions
                    .into_iter()
                    .partition(|a| matches!(*a, "rename" | "rules" | "delete"));
                ui.set_page_actions(strings(primary));
                ui.set_page_options(strings(options));

                shown.store(token, Ordering::SeqCst);
                ui.set_page_artwork(Default::default());
                ui.set_browse_artwork(Default::default());
                art.send_replace(request);
            }
        });
    }
    fn reset(&mut self, access: Option<Access>) {
        self.cancel();
        self.personal_due = None;
        self.personal_ready = false;
        self.personal_view = None;
        self.pending = None;
        self.edit_basis = None;
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
            let interrupted = self.pending.take().is_some();
            self.cancel();
            self.personal_due = None;
            self.personal_view = None;
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
            if interrupted {
                self.message =
                    "You went offline before the playlist change could be confirmed.".into();
                self.error = true;
                self.render(false);
            }
        }
    }
    fn personal_write_error(&mut self, message: String) {
        self.pending = None;
        self.writing = false;
        self.busy = false;
        self.error = true;
        self.message = message.clone();
        self.scoped_ui(move |ui| ui.set_form_error(message.into()));
        self.render(false);
    }

    fn personal_mutation(&mut self, access: &Access, mutation: Mutation) {
        let before = access.personal.read(|status, _| status);
        if access.offline
            || access.session.is_offline()
            || before.epoch != access.epoch
            || before.link != Link::Ready
        {
            self.personal_write_error("You're not connected to your account library.".into());
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
        self.pending = Some(Pending {
            effect: mutation.effect,
            before,
            due: tokio::time::Instant::now() + PERSONAL_WRITE_TIMEOUT,
        });
        self.writing = true;
        self.busy = true;
        self.error = false;
        self.message = "Waiting to see the change in your account library…".into();
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
        let (status, next, detail_live, unavailable, limited) =
            a.personal.read(|status, replica| {
                let detail_live = replica.playlist().is_some_and(|p| !p.stale);
                let page = (status.epoch == a.epoch)
                    .then(|| pages::personal_projection(&route, replica))
                    .flatten();
                (
                    status,
                    page,
                    detail_live,
                    matches!(route, Route::Playlist(_)) && replica.playlist_unavailable(),
                    matches!(route, Route::Playlist(_)) && replica.playlist_limited(),
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
                self.personal_write_error("The connection was interrupted. The change was not confirmed; check the current library before retrying.".into());
                self.render(render_models);
                return;
            }
            if status.refused != before.refused {
                self.personal_write_error("The connection could not take the change. Check the current library before retrying.".into());
                self.render(render_models);
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
                self.message = "The matching change is now visible in your account library.".into();
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
            && !unavailable
            && !limited;
        if !waiting || refused || (visiting && self.error) {
            self.personal_due = None;
        } else if transitioned || visiting {
            self.personal_due = Some(tokio::time::Instant::now() + PERSONAL_READ_TIMEOUT);
        }
        self.busy = self.personal_due.is_some() && self.page.rows.is_empty();
        if !(visiting && self.error) {
            let (message, error) = if !online {
                ("Work online to refresh your account library.", false)
            } else if limited {
                (
                    "This playlist is too large to load here. Previously loaded songs may still be shown.",
                    true,
                )
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
    }

    fn personal_timeout(&mut self) {
        let now = tokio::time::Instant::now();
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.due <= now)
        {
            self.personal_write_error("The server did not confirm this change. Check the current library before retrying.".into());
        }
        if self.personal_due.is_some_and(|due| due <= now) {
            self.personal_due = None;
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
        if matches!(next, Route::Now | Route::Queue | Route::Devices)
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
                    Route::SavedCollection(id) => crate::collections::detail(&a, id).await,
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
        let (mut op, idx, val) = match i.intent {
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
            _ => return,
        };
        let row = idx.and_then(|i| self.page.rows.get(i)).cloned();
        let route = self.route.clone();
        let page = self.page.clone();
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
            self.edit_basis = page
                .playlist
                .as_ref()
                .map(|p| (mode.to_owned(), p.id.clone(), p.revision));
            if matches!(mode, "delete" | "remove" | "device-rename") {
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
        if matches!(op.as_str(), "rename" | "rules" | "delete")
            && !draft_matches(&self.edit_basis, &page, &op)
        {
            self.scoped_ui(|ui| {
                ui.set_form_error(
                    "This playlist changed. Close the editor and open it again before saving."
                        .into(),
                )
            });
            return;
        }
        // Explicit Work offline closes the session gate immediately, before
        // the access watch necessarily reports the mode change to the browser.
        let offline = a.offline || a.session.is_offline();
        if route.personal()
            && !self.personal_ready
            && !matches!(op.as_str(), "open" | "album" | "artist" | "track")
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
        if matches!(op.as_str(), "album" | "artist" | "track") {
            let target = if op == "track" && !offline && idx.is_none() {
                pages::music_route("track", &val) // Now Playing's explicit track link.
            } else {
                metadata_route(&op, &val, offline)
            };
            if let Some(target) = target.filter(|target| *target != route) {
                self.visit(Navigation::Follow(target), false);
            }
            return;
        }
        let saved_page = saved_route(&route);
        let local_results = saved_page || a.offline && matches!(route, Route::Search(_));
        if op == "open" && !matches!(route, Route::Picker(_)) {
            let target = row.as_ref().and_then(|r| {
                r.route.clone().or_else(|| {
                    // Download rows have local playback, not a local Track
                    // detail. While online they can still open their stored
                    // track identity on this account's server.
                    if saved_page
                        && !offline
                        && let pages::RowKey::Track(id) = &r.key
                    {
                        pages::music_route("track", id)
                    } else {
                        None
                    }
                })
            });
            if let Some(target) = target {
                if (!offline || saved_route(&target)) && target != route {
                    self.visit(Navigation::Follow(target), false);
                }
                return;
            }
            if !local_results || !row.as_ref().is_some_and(|r| r.playable) {
                return;
            }
            op = "play".into(); // Saved tracks have no separate offline detail.
        }
        if op == "add" && !matches!(route, Route::Picker(_)) {
            if let Some(r) = row
                && !a.offline
            {
                self.visit(Navigation::Follow(Route::Picker(r.id)), false);
            }
            return;
        }
        let local_play = local_results && op == "play";
        if matches!(op.as_str(), "play" | "queue") && !local_play {
            if a.offline {
                return;
            }
            let selection = if let Some(r) = row {
                if !r.playable {
                    return;
                }
                r.playback
            } else {
                page.playback.clone()
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
        let remove_saved = op == "remove" && saved_page;
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
                Ok(mutation) => self.personal_mutation(&a, mutation),
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
                        crate::collections::remove(&a, id)
                            .await
                            .map(|_| Some(Route::Offline)),
                    )
                } else {
                    let res = if let Some(r) = row.as_ref() {
                        if remove_saved && matches!(r.route, Some(Route::SavedCollection(_))) {
                            crate::collections::remove(&a, &r.id).await
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
            Update::Changed(Err(e)) => {
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
                self.render(false);
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
    let mut groups = crate::collections::list(access).await?;
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
            album.clone(),
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
            album.clone(),
            Route::Track("track".into()),
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
        assert!(row_actions(&p, &r, 0, true).is_empty());
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
            let mut b=Browser {ui:window,epoch,shown,engine,access:None,route:INITIAL_ROUTE,page_keys:RandomState::new(),trail:Trail::default(),cache:Replica::default(),page:Arc::default(),generation:0,serial:0,busy:false,silent_read:false,personal_ready:false,personal_due:None,personal_view:None,pending:None,edit_basis:None,writing:false,message:String::new(),error:false,task:None,results,art};
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
                    Some(u)=updates.recv()=>b.result(u),
                    input=inputs.recv()=>{let Some(i)=input else{break;};b.input(i);},
                }
            }
            b.cancel();
        });
    });
    (desk, worker)
}
