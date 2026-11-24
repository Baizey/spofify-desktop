mod actions;
mod api;
mod api_transport;
mod artwork;
mod audio;
mod audio_input;
mod auth;
mod collections;
mod desk;
mod engine;
mod local_catalog;
mod login_store;
mod media;
mod mode;
mod motion;
#[cfg(target_os = "linux")]
mod mpris;
mod offline;
mod output_identity;
mod pages;
mod personal;
mod personal_bridge;
mod personal_cache;
mod playback_sync;
mod player;
mod playlist_browser;
#[cfg(test)]
mod playlist_observations;
mod playlist_pin_store;
mod playlist_pins;
mod playlist_view;
use api::preferences;
mod presentation;
mod quick_add;
mod replica;
mod saved;
mod search_bulk;
mod socket;
mod storage;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod update;
slint::include_modules!();
use engine::{AuthCommand, Command};
use slint::{ComponentHandle, Model};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use tokio::sync::{mpsc, watch};

fn send(tx: &mpsc::Sender<Command>, command: Command, ui: &AppWindow) {
    if tx.try_send(command).is_err() {
        if ui.get_signed_in() {
            ui.set_player_status("That action wasn't sent. Please try again in a moment.".into());
        } else {
            ui.set_busy(false);
            ui.set_error(true);
            ui.set_status("The desktop is busy. Please try again.".into());
        }
    }
}
fn scope(epoch: &AtomicU64, c: Command) -> Command {
    c.scoped(epoch.load(Ordering::SeqCst))
}
fn clear(ui: &AppWindow) {
    ui.set_query("".into());
    ui.set_browse_rows(Default::default());
    ui.set_queue_order_token("".into());
    ui.set_queue_order_writable(false);
    ui.set_queue_order_rows(Default::default());
    ui.set_queue_rows(Default::default());
    ui.set_device_rows(Default::default());
    ui.set_account_label("".into());
    ui.set_form_mode("".into());
    ui.set_form_name("".into());
    ui.set_form_value("".into());
    ui.set_form_description("".into());
    ui.set_now_track_id("".into());
    ui.set_room_token("".into());
    ui.set_queue_room_key("".into());
    ui.set_media_track_token("".into());
    ui.set_now_album_id("".into());
    ui.set_now_album_name("".into());
    ui.set_now_credits(Default::default());
    presentation::clear(ui);
    ui.set_now_title("Nothing playing".into());
    ui.set_now_detail("".into());
    ui.set_playing(false);
    ui.set_can_control(false);
    ui.set_can_seek(false);
    ui.set_manual_offline(false);
}
fn main() -> Result<(), slint::PlatformError> {
    if std::env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == "--version")
    {
        println!("Spofify Desktop {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if !cfg!(debug_assertions)
        && (std::env::var_os("SLINT_MCP_PORT").is_some()
            || std::env::var_os("SLINT_TEST_SERVER").is_some())
    {
        eprintln!("Developer inspection is unavailable in release builds.");
        std::process::exit(2);
    }
    let ui = AppWindow::new()?;
    playlist_view::install(&ui);
    motion::install(&ui);
    if cfg!(debug_assertions) && std::env::var_os("SLINT_MCP_PORT").is_some() {
        ui.set_inspection_reorder_line(std::env::var_os("SPOFIFY_INSPECT_REORDER_LINE").is_some());
    }
    if cfg!(debug_assertions)
        && std::env::var_os("SLINT_MCP_PORT").is_some()
        && let Ok(size) = std::env::var("SPOFIFY_DEV_SIZE")
        && let Some((w, h)) = size
            .split_once('x')
            .and_then(|(w, h)| Some((w.parse::<u32>().ok()?, h.parse::<u32>().ok()?)))
        && (800..=1920).contains(&w)
        && (600..=1200).contains(&h)
    {
        ui.set_inspection_width(w as f32);
        ui.set_inspection_height(h as f32);
    }
    let epoch = Arc::new(AtomicU64::new(0));
    let (access_tx, access) = watch::channel(None);
    let mode = mode::ModeControl::default();
    let (auth_tx, auth_rx) = mpsc::unbounded_channel();
    ui.set_busy(true);
    ui.set_restoring_login(true);
    preferences::install_density(&ui);
    let (master_gain, _volume_writer) = preferences::install_local_volume(&ui);
    let output = engine::OutputPreferences {
        crossfade_enabled: preferences::install_playback(&ui),
        master_gain,
    };
    let (tx, worker) = engine::start(
        ui.as_weak(),
        epoch.clone(),
        access_tx,
        mode.clone(),
        auth_rx,
        auth_tx.clone(),
        output,
    );
    let (art, pages) = watch::channel(None);
    let _covers = artwork::install(&ui, access.clone(), pages);
    let (desk, browser) = desk::start(&ui, epoch.clone(), access.clone(), tx.clone(), art);
    desk.bind(&ui, mode.clone(), access.clone());
    let _media = {
        let (tx, epoch) = (tx.clone(), epoch.clone());
        presentation::install(
            &ui,
            access.clone(),
            epoch.clone(),
            move |ui, room, snapshot, start_ms| {
                send(
                    &tx,
                    scope(
                        &epoch,
                        Command::SeekLyric {
                            room,
                            snapshot,
                            start_ms,
                        },
                    ),
                    ui,
                );
            },
        )
    };
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    let updates = update::start(&ui, mode.update_network());
    bind_auth(&ui, &auth_tx, &epoch);
    for op in ["previous", "next", "toggle"] {
        let (w, tx, epoch) = (ui.as_weak(), tx.clone(), epoch.clone());
        let callback = move || {
            if let Some(ui) = w.upgrade()
                && ui.get_signed_in()
            {
                send(
                    &tx,
                    scope(
                        &epoch,
                        match op {
                            "next" => {
                                Command::Guarded(ui.get_room_token().into(), player::Action::Next)
                            }
                            "previous" => Command::Guarded(
                                ui.get_room_token().into(),
                                player::Action::Previous,
                            ),
                            _ => Command::GuardedToggle(ui.get_room_token().into()),
                        },
                    ),
                    &ui,
                );
            }
        };
        match op {
            "previous" => ui.on_previous(callback),
            "next" => ui.on_next(callback),
            _ => ui.on_toggle_play(callback),
        }
    }
    {
        let (w, tx, epoch) = (ui.as_weak(), tx.clone(), epoch.clone());
        ui.on_seek(move |s| {
            if let Some(ui) = w.upgrade()
                && ui.get_signed_in()
            {
                send(
                    &tx,
                    scope(
                        &epoch,
                        Command::Guarded(
                            ui.get_room_token().into(),
                            player::Action::Seek(s as f64),
                        ),
                    ),
                    &ui,
                );
            }
        });
    }
    {
        let (w, tx, epoch) = (ui.as_weak(), tx.clone(), epoch.clone());
        ui.on_queue_action(move |op, key| {
            let Some(ui) = w.upgrade() else { return };
            if !ui.get_signed_in() {
                return;
            }
            let key = key.parse::<u64>().ok();
            use player::Action as A;
            let action = match op.as_str() {
                "play" | "jump" => key.map(A::Jump),
                "remove" => key.map(A::Remove),
                "clear" => Some(A::Clear),
                "close" => Some(A::Close),
                "shuffle" => Some(A::Shuffle(!ui.get_shuffle())),
                "repeat" => Some(A::CycleRepeat),
                _ => None,
            };
            if let Some(a) = action {
                send(
                    &tx,
                    scope(&epoch, Command::Guarded(ui.get_room_token().into(), a)),
                    &ui,
                );
            }
        });
    }
    {
        let (w, tx, epoch) = (ui.as_weak(), tx.clone(), epoch.clone());
        ui.on_queue_order_drop(move |basis, id, predecessor| {
            let Some(ui) = w.upgrade() else { return };
            if !ui.get_signed_in() || basis.is_empty() || basis != ui.get_queue_order_token() {
                return;
            }
            let Ok(key) = id.parse::<u64>() else { return };
            let after = if predecessor.is_empty() {
                None
            } else if let Ok(anchor) = predecessor.parse::<u64>() {
                Some(anchor)
            } else {
                return;
            };
            if tx
                .try_send(scope(
                    &epoch,
                    Command::ReorderQueue {
                        basis: basis.into(),
                        key,
                        after,
                    },
                ))
                .is_err()
            {
                ui.set_player_status("The queue order was not sent.".into());
            }
        });
    }
    {
        let (w, tx, epoch) = (ui.as_weak(), tx.clone(), epoch.clone());
        ui.on_device_action(move |op, id, name| {
            let Some(ui) = w.upgrade() else { return };
            if !ui.get_signed_in() || ui.get_manual_offline() || ui.get_reconnecting() {
                return;
            }
            use player::Action as A;
            let action = match op.as_str() {
                "select" => A::SelectDevice(id.into()),
                "rename"
                    if ui
                        .get_device_rows()
                        .iter()
                        .any(|row| row.id == id && row.selected && row.available) =>
                {
                    A::RenameDevice {
                        device_id: Some(id.into()),
                        name: name.into(),
                    }
                }
                _ => return,
            };
            send(&tx, scope(&epoch, Command::Playback(action)), &ui);
        });
    }
    #[cfg(target_os = "linux")]
    let _media_keys = mpris::attach(&ui, epoch.clone(), move |ui, command| {
        send(&tx, command, ui);
    });
    let result = ui.run();
    mode.close_updates();
    let _ = auth_tx.send(AuthCommand::Shutdown);
    if worker.join().is_err() {
        eprintln!("Audio worker closed unexpectedly.");
    }
    drop(desk);
    if browser.join().is_err() {
        eprintln!("Library worker closed unexpectedly.");
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    if let Some(updates) = updates {
        // Let a cancelled partial download drop its stage, but never hang
        // exit waiting for updater filesystem I/O.
        let _ = updates.recv_timeout(std::time::Duration::from_millis(250));
    }
    result
}
fn advance_account_epoch(epoch: &AtomicU64) -> u64 {
    // Only the final local pin rename shares this lock, never a download or fsync.
    let _publication = playlist_pin_store::PUBLICATION_GATE.lock().unwrap();
    epoch.fetch_add(1, Ordering::SeqCst).wrapping_add(1)
}

fn bind_auth(ui: &AppWindow, tx: &mpsc::UnboundedSender<AuthCommand>, epoch: &Arc<AtomicU64>) {
    {
        let (w, tx, epoch) = (ui.as_weak(), tx.clone(), epoch.clone());
        ui.on_sign_in(move || {
            let Some(ui) = w.upgrade() else { return };
            if ui.get_busy() || ui.get_signed_in() {
                return;
            }
            let command = AuthCommand::Login {
                epoch: advance_account_epoch(&epoch),
                origin: ui.get_origin().into(),
                username: ui.get_username().into(),
                password: ui.get_password().into(),
                remember: ui.get_keep_signed_in(),
            };
            ui.set_password("".into());
            ui.set_busy(true);
            ui.set_restoring_login(false);
            ui.set_status("".into());
            ui.set_error(false);
            ui.set_auth_notice("".into());
            if tx.send(command).is_err() {
                ui.set_busy(false);
                ui.set_restoring_login(false);
                ui.set_error(true);
                ui.set_status("Sign-in is unavailable. Reopen the desktop and try again.".into());
            }
        });
    }
    for kind in ["logout", "cancel", "server", "username"] {
        let (w, tx, epoch) = (ui.as_weak(), tx.clone(), epoch.clone());
        let callback = move || {
            let Some(ui) = w.upgrade() else { return };
            let editing = matches!(kind, "server" | "username");
            // Ordinary edits need no worker round trip. If an auth result is
            // queued, fence it before changing either part of the login scope.
            let interrupt = ui.get_busy() || ui.get_signed_in();
            if !editing && !interrupt {
                return;
            }
            ui.set_password("".into());
            ui.set_keep_signed_in(false);
            ui.set_auth_notice("".into());
            ui.set_status("".into());
            ui.set_error(false);
            if !interrupt {
                return;
            }
            let epoch = advance_account_epoch(&epoch);
            ui.set_signed_in(false);
            ui.set_busy(false);
            ui.set_restoring_login(false);
            clear(&ui);
            let _ = tx.send(if kind == "logout" {
                AuthCommand::Logout(epoch)
            } else {
                AuthCommand::Cancel(epoch)
            });
        };
        match kind {
            "logout" => ui.on_sign_out(callback),
            "cancel" => ui.on_cancel_sign_in(callback),
            "server" => ui.on_server_edited(callback),
            _ => ui.on_username_edited(callback),
        }
    }
}
