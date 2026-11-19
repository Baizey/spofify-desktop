//! Desktop browsing controller. A single owner accepts navigation and writes;
//! cancellable reads populate bounded account-scoped snapshots off the UI thread.
use crate::{
    AppWindow, BrowseRow, actions,
    api::{Api, ApiError, Session},
    engine::Command,
    pages::{self, Page, Route},
    player,
    replica::{Replica, Trail},
    saved,
};
use slint::{ComponentHandle, ModelRc, VecModel};
use std::{
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
    pub recent_change: u64,
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
    Read(Result<Page, ApiError>),
    ReadGap,
    Changed(Result<Option<Route>, ApiError>, bool),
    Local(Result<Command, ApiError>),
    CollectionSaved(Result<crate::collections::SaveSummary, ApiError>),
}
struct Delivery {
    serial: u64,
    result: Update,
}
struct Browser {
    ui: slint::Weak<AppWindow>,
    epoch: Arc<AtomicU64>,
    shown: Arc<AtomicU64>,
    engine: mpsc::Sender<Command>,
    access: Option<Access>,
    route: Route,
    trail: Trail,
    cache: Replica,
    page: Arc<Page>,
    generation: u64,
    serial: u64,
    busy: bool,
    silent_read: bool,
    recent_due: Option<tokio::time::Instant>,
    edit_basis: Option<(String, String, u64)>, // form, playlist, original revision
    writing: bool,
    message: String,
    error: bool,
    task: Option<JoinHandle<()>>,
    results: mpsc::Sender<Delivery>,
    art: watch::Sender<Option<crate::artwork::Request>>,
}
fn label(route: &Route) -> &'static str {
    match route {
        Route::Home => "Home",
        Route::History => "Recently played",
        Route::Search(_) => "Search",
        Route::Liked => "Liked music",
        Route::Playlists => "Playlists",
        Route::Picker(_) => "Add to a playlist",
        Route::Offline => "Saved on this device",
        Route::Appearance => "Settings",
        Route::Now => "Now playing",
        Route::Queue => "Queue",
        Route::Devices => "Devices",
        Route::Albums => "Albums",
        Route::Artists => "Artists",
        Route::Album(_) => "Album",
        Route::Artist(_) => "Artist",
        Route::Playlist(_) => "Playlist",
        Route::SavedCollection(_) => "Saved collection",
        Route::Track(_) => "Track",
    }
}
fn named_route(name: &str) -> Option<Route> {
    Some(match name {
        "home" => Route::Home,
        "history" => Route::History,
        "search" => Route::Search(String::new()),
        "liked" => Route::Liked,
        "playlists" => Route::Playlists,
        "offline" => Route::Offline,
        "appearance" | "settings" => Route::Appearance,
        "now" => Route::Now,
        "queue" => Route::Queue,
        "devices" => Route::Devices,
        "albums" => Route::Albums,
        "artists" => Route::Artists,
        _ => return None,
    })
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
    if !r.album_id.is_empty() {
        v.push("album");
    }
    if !r.artist_id.is_empty() {
        v.push("artist");
    }
    if offline {
        return v;
    }
    if r.playback.is_some() && r.playable {
        v.push("queue");
    }
    if r.item.is_some() {
        v.extend([
            if r.liked == Some(true) {
                "unlike"
            } else {
                "like"
            },
            "add",
        ]);
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
        if i + 1 < p.rows.len() || p.more {
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
            ui.set_page_title(title.into());
            ui.set_page_subtitle(p.subtitle.clone().into());
            ui.set_page_status(message.into());
            ui.set_page_busy(busy);
            ui.set_page_error(error);
            ui.set_can_back(back);
            ui.set_has_more(p.more && !offline);
            ui.set_form_busy(write);
            if models {
                ui.set_browse_rows(ModelRc::new(VecModel::from(
                    p.rows
                        .iter()
                        .enumerate()
                        .map(|(i, r)| BrowseRow {
                            section: p.section(i).into(),
                            actions: strings(row_actions(&p, r, i, offline)),
                            kind: r.kind.as_str().into(),
                            title: r.title.as_str().into(),
                            subtitle: r.subtitle.as_str().into(),
                            detail: r.detail.as_str().into(),
                            playable: r.playable
                                && offline
                                    == matches!(kind.as_str(), "offline" | "saved-collection"),
                        })
                        .collect::<Vec<_>>(),
                )));
                let mut actions = Vec::new();
                if kind == "saved-collection" {
                    actions.push("remove");
                }
                if offline
                    && matches!(kind.as_str(), "saved-collection" | "offline")
                    && p.rows.iter().any(|r| r.playable)
                {
                    actions.push("play");
                }
                if !offline {
                    if matches!(
                        kind.as_str(),
                        "album" | "artist" | "playlist-manual" | "playlist-rules" | "liked"
                    ) {
                        actions.push("save");
                    }
                    if p.playback.is_some() {
                        actions.extend(["play", "queue"]);
                    }
                    if kind == "playlists" {
                        actions.extend(["new-manual", "new-rules"]);
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
                ui.set_page_actions(strings(actions));

                shown.store(token, Ordering::SeqCst);
                ui.set_page_artwork(Default::default());
                ui.set_browse_artwork(Default::default());
                art.send_replace(request);
            }
        });
    }
    fn reset(&mut self, access: Option<Access>) {
        self.cancel();
        self.recent_due = None;
        self.edit_basis = None;
        self.cache = Replica::default();
        self.trail = Trail::default();
        self.route = Route::Home;
        self.page = Arc::default();
        self.message.clear();
        self.error = false;
        self.access = access;
        self.scoped_ui(|ui| clear_form(&ui));
        if self.access.is_some() {
            self.visit(Route::Home, false, false);
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
        let recent = a.recent_change != old.recent_change;
        if recent {
            // Playback progress is a hint, never a committed history record. Let
            // the server settle and combine nearby hints into one background read.
            self.cache.recent_changed();
            self.recent_due = Some(tokio::time::Instant::now() + Duration::from_secs(2));
        }
        if old.offline != offline {
            self.cancel();
            self.scoped_ui(|ui| clear_form(&ui));
            self.visit(
                if offline {
                    Route::Offline
                } else {
                    self.route.clone()
                },
                offline,
                false,
            );
        }
    }
    fn recent_hint(&mut self) {
        self.recent_due = None;
        if self.access.as_ref().is_some_and(|a| !a.offline)
            && !self.writing
            && !self.busy
            && matches!(self.route, Route::Home | Route::History)
        {
            self.cache.recent_changed();
            self.visit(self.route.clone(), false, false);
        }
    }
    fn visit(&mut self, next: Route, remember: bool, more: bool) {
        if self.writing {
            return;
        }
        let changed = next != self.route;
        if remember {
            self.trail.visit(&self.route, &next);
        }
        self.cancel();
        self.message.clear();
        self.error = false;
        if changed {
            self.edit_basis = None;
            self.scoped_ui(|ui| clear_form(&ui));
        }
        let cached = self.cache.get(&next);
        let have = cached.is_some();
        let fresh = cached.as_ref().is_some_and(|(_, f)| *f);
        self.page = cached.map_or_else(Arc::default, |(p, _)| p);
        self.route = next.clone();
        let Some(a) = self.access.clone().filter(Access::usable) else {
            self.render(true);
            return;
        };
        if matches!(
            next,
            Route::Now | Route::Queue | Route::Devices | Route::Appearance
        ) || matches!(&next,Route::Search(q) if q.trim().is_empty())
        {
            self.render(true);
            return;
        }
        if a.offline && !matches!(next, Route::Offline | Route::SavedCollection(_)) {
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
        self.render(changed || !have);
        self.task = Some(tokio::spawn(async move {
            if matches!(next, Route::Search(_)) && !more {
                tokio::time::sleep(Duration::from_millis(240)).await;
            }
            let mut rebuilding = false;
            for attempt in 0..=4 {
                if !a.usable() {
                    return;
                }
                let mut result = match &next {
                    Route::Offline => {
                        async {
                            let mut p = saved::page(&a).await?;
                            let mut groups = crate::collections::list(&a).await?;
                            groups.append(&mut p.rows);
                            p.rows = groups;
                            Ok(p)
                        }
                        .await
                    }
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
                let retry = attempt < 4
                    && (result.as_ref().err().is_some_and(recoverable)
                        || result
                            .as_ref()
                            .is_ok_and(|p| p.partial || p.pending_rules()));
                if retry {
                    let result = match result {
                        Ok(p) => Update::Read(Ok(p)),
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
                            result: Update::Read(result),
                        })
                        .await;
                    return;
                }
            }
        }));
    }
    fn playback(&mut self, c: Command) {
        if let Some(a) = &self.access
            && self
                .engine
                .try_send(Command::Scoped(a.epoch, Box::new(c)))
                .is_err()
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
                if s == "back" {
                    if let Some(r) = self.trail.pop() {
                        if let Route::Search(q) = &r {
                            let q = q.clone();
                            self.scoped_ui(move |ui| ui.set_query(q.into()));
                        }
                        self.visit(r, false, false);
                    }
                } else if let Some(r) = named_route(&s) {
                    if r == Route::Search(String::new()) {
                        self.scoped_ui(|ui| ui.set_query("".into()));
                    }
                    self.visit(r, true, false);
                }
                return;
            }
            Intent::Search(q) => {
                if q.len() <= 1000 {
                    self.visit(Route::Search(q), true, false);
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
            if page.more && !self.busy && !a.offline {
                self.visit(route, false, true);
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
        if let Some(kind) = op.strip_prefix("context-") {
            if val.is_empty() {
                return;
            }
            let target = match kind {
                "likes" => Route::Liked,
                "album" => Route::Album(val),
                "artist" => Route::Artist(val),
                "playlist" | "derived" => Route::Playlist(val),
                _ => return,
            };
            self.visit(target, true, false);
            return;
        }
        if matches!(op.as_str(), "album" | "artist" | "track") {
            let id = row.as_ref().map_or(val, |r| {
                if op == "artist" {
                    r.artist_id.clone()
                } else if op == "album" {
                    r.album_id.clone()
                } else {
                    r.id.clone()
                }
            });
            if !id.is_empty() {
                self.visit(
                    match op.as_str() {
                        "artist" => Route::Artist(id),
                        "album" => Route::Album(id),
                        _ => Route::Track(id),
                    },
                    true,
                    false,
                );
            }
            return;
        }
        if op == "open" && !matches!(route, Route::Picker(_)) {
            if let Some(r) = row.as_ref().and_then(|r| r.route.clone()) {
                self.visit(r, true, false);
            } else if let Some(r) = &row
                && !r.id.is_empty()
            {
                self.visit(Route::Track(r.id.clone()), true, false);
            }
            return;
        }
        if op == "add" && !matches!(route, Route::Picker(_)) {
            if let Some(r) = row
                && !a.offline
            {
                self.visit(Route::Picker(r.id), true, false);
            }
            return;
        }
        let saved_page = matches!(route, Route::Offline | Route::SavedCollection(_));
        let local_play = saved_page && a.offline && op == "play";
        if saved_page && !a.offline && op == "play" {
            self.message = "Choose Work offline to play the saved queue on this computer.".into();
            self.render(false);
            return;
        }
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
                    player::Action::Enqueue {
                        selection,
                        placement: player::Placement::Low,
                    }
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
                        true,
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
                    Update::Changed(res.map(|_| None), true)
                }
            } else {
                let op = if op == "open" && matches!(route, Route::Picker(_)) {
                    "add"
                } else {
                    &op
                };
                Update::Changed(
                    actions::mutate(&a, &route, &page, op, idx, &val).await,
                    false,
                )
            };
            let _ = tx.send(Delivery { serial, result }).await;
        }));
    }
    fn result(&mut self, d: Delivery) {
        if d.serial != self.serial {
            return;
        }
        self.busy = false;
        self.writing = false;
        self.error = false;
        self.message.clear();
        match d.result {
            Update::Read(Ok(mut page)) => {
                let partial = page.partial;
                if partial {
                    page.retain_home_shelves(&self.page);
                }
                let changed = *self.page != page;
                self.page = Arc::new(page);
                self.cache.put(self.route.clone(), self.page.clone());
                if partial {
                    self.cache.recent_changed();
                    self.message = "Some items aren't available right now.".into();
                }
                self.render(changed);
            }
            Update::ReadGap => {
                self.message = if self.page.rows.is_empty() {
                    "Waiting for the library…"
                } else {
                    "Connection interrupted. Showing your last view."
                }
                .into();
                self.render(false);
            }
            Update::Read(Err(e)) => {
                self.error = true;
                self.message = readable(&e);
                self.render(false);
            }
            Update::Changed(Ok(route), saved) => {
                self.scoped_ui(|ui| clear_form(&ui));
                if saved {
                    self.cache.saved_changed();
                    if route.is_none()
                        && !matches!(self.route, Route::Offline | Route::SavedCollection(_))
                    {
                        self.message = "Saved on this device.".into();
                        self.render(false);
                        return;
                    }
                } else {
                    self.cache.collection_changed();
                }
                self.visit(route.unwrap_or_else(|| self.route.clone()), false, false);
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
            Update::Changed(Err(e), _) => {
                let msg = readable(&e);
                self.scoped_ui({
                    let m = msg.clone();
                    move |ui| ui.set_form_error(m.into())
                });
                if matches!(e.status, 409 | 410 | 412) || recoverable(&e) {
                    // Outcome may be unknown after a lost response. Re-read; never resubmit the write.
                    self.cache.collection_changed();
                    self.visit(self.route.clone(), false, false);
                }
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
    fn only_recent_entries_can_start_the_recent_shelf() {
        use pages::{Row, RowKey};
        let mut p = Page {
            kind: "home".into(),
            ..Default::default()
        };
        p.rows = [
            RowKey::Playlist("p".into()),
            RowKey::Playlist("q".into()),
            RowKey::Track("liked".into()),
        ]
        .into_iter()
        .map(|key| Row {
            key,
            ..Default::default()
        })
        .collect();
        assert_eq!(p.section(0), "YOUR PLAYLISTS");
        assert_eq!(p.section(1), "");
        assert_eq!(p.section(2), "LIKED SONGS");
        p.rows.insert(
            0,
            Row {
                key: RowKey::HistoryEvent("play".into()),
                ..Default::default()
            },
        );
        assert_eq!(p.section(0), "RECENTLY PLAYED");
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
            album_id: "a".into(),
            ..Default::default()
        };
        assert_eq!(row_actions(&p, &r, 0, true), ["album"]);
    }
}
fn draft_matches(b: &Option<(String, String, u64)>, p: &Page, mode: &str) -> bool {
    b.as_ref()
        .zip(p.playlist.as_ref())
        .is_some_and(|((op, id, rev), m)| op == mode && id == &m.id && *rev == m.revision)
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
            let mut b=Browser {ui:window,epoch,shown,engine,access:None,route:Route::Home,trail:Trail::default(),cache:Replica::default(),page:Arc::default(),generation:0,serial:0,busy:false,silent_read:false,recent_due:None,edit_basis:None,writing:false,message:String::new(),error:false,task:None,results,art};
            loop {
                let due=b.recent_due;
                tokio::select! { biased;
                    change=access.changed()=>{if change.is_err(){break;}let next=access.borrow_and_update().clone();b.access_changed(next);},
                    Some(u)=updates.recv()=>b.result(u),
                    input=inputs.recv()=>{let Some(i)=input else{break;};b.input(i);},
                    _=async {match due{Some(t)=>tokio::time::sleep_until(t).await,None=>std::future::pending().await}}=>b.recent_hint(),
                }
            }
            b.cancel();
        });
    });
    (desk, worker)
}
