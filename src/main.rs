mod actions;
mod api;
mod artwork;
mod audio;
mod auth;
mod collections;
mod desk;
mod engine;
mod login_store;
mod media;
mod mode;
#[cfg(target_os = "linux")]
mod mpris;
mod offline;
mod pages;
mod personal;
mod personal_bridge;
mod player;
use api::preferences;
mod presentation;
mod replica;
mod saved;
mod socket;
mod storage;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod update;
slint::include_modules!();
use engine::{AuthCommand, Command};
use slint::{ComponentHandle, Model};
use std::{
    cell::RefCell,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
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
    ui.set_queue_rows(Default::default());
    ui.set_device_rows(Default::default());
    ui.set_account_label("".into());
    ui.set_form_mode("".into());
    ui.set_form_name("".into());
    ui.set_form_value("".into());
    ui.set_form_description("".into());
    ui.set_now_track_id("".into());
    ui.set_now_album_id("".into());
    ui.set_now_album_name("".into());
    ui.set_next_title("".into());
    ui.set_next_subtitle("".into());
    ui.set_now_credits(Default::default());
    ui.set_cover_art(Default::default());
    ui.set_waveform(Default::default());
    ui.set_lyrics_text("".into());
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
    let crossfade = preferences::install_playback(&ui);
    let (tx, worker) = engine::start(
        ui.as_weak(),
        epoch.clone(),
        access_tx,
        mode.clone(),
        auth_rx,
        auth_tx.clone(),
        crossfade,
    );
    let (art, pages) = watch::channel(None);
    let _covers = artwork::install(&ui, access.clone(), pages);
    let (desk, browser) = desk::start(&ui, epoch.clone(), access.clone(), tx.clone(), art);
    desk.bind(&ui, mode, access.clone());
    let _media = presentation::install(&ui, access.clone());
    let _prefs = preferences::install(&ui, access.clone());
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    update::start(access);
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
                        if op == "toggle" {
                            Command::Toggle
                        } else {
                            Command::Transport(op)
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
            let model = ui.get_queue_rows();
            let rows = upcoming_keys(model.iter());
            use player::Action as A;
            let action = match op.as_str() {
                "play" | "jump" => key.map(A::Jump),
                "remove" => key.map(A::Remove),
                "up" | "down" => key.and_then(|k| {
                    move_after(&rows, k, op == "down").map(|after| A::Move { key: k, after })
                }),
                "clear" => Some(A::Clear),
                "close" => Some(A::Close),
                "shuffle" => Some(A::Shuffle(!ui.get_shuffle())),
                "repeat" => Some(A::Repeat(match ui.get_repeat_mode().as_str() {
                    "off" => player::Repeat::All,
                    "all" => player::Repeat::One,
                    _ => player::Repeat::Off,
                })),
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
        let renaming = Rc::new(RefCell::new(None::<(u64, String)>));
        ui.on_device_action(move |op, value| {
            let Some(ui) = w.upgrade() else { return };
            if !ui.get_signed_in() {
                return;
            }
            let e = epoch.load(Ordering::SeqCst);
            use player::Action as A;
            let action = match op.as_str() {
                "select" => A::SelectDevice(value.into()),
                "deselect" => A::DeselectDevice,
                "prepare-rename" => {
                    renaming.replace(Some((e, value.into())));
                    return;
                }
                "rename" => {
                    let Some((old, id)) = renaming.take() else {
                        return;
                    };
                    if old != e {
                        return;
                    }
                    ui.set_form_mode("".into());
                    A::RenameDevice {
                        device_id: Some(id),
                        name: value.into(),
                    }
                }
                _ => return,
            };
            send(&tx, scope(&epoch, Command::Playback(action)), &ui);
        });
    }
    #[cfg(target_os = "linux")]
    let _media_keys = mpris::attach(&ui, epoch.clone());
    let result = ui.run();
    let _ = auth_tx.send(AuthCommand::Shutdown);
    if worker.join().is_err() {
        eprintln!("Audio worker closed unexpectedly.");
    }
    drop(desk);
    if browser.join().is_err() {
        eprintln!("Library worker closed unexpectedly.");
    }
    result
}
fn bind_auth(ui: &AppWindow, tx: &mpsc::UnboundedSender<AuthCommand>, epoch: &Arc<AtomicU64>) {
    for retry in [false, true] {
        let (w, tx, epoch) = (ui.as_weak(), tx.clone(), epoch.clone());
        let callback = move || {
            let Some(ui) = w.upgrade() else { return };
            if ui.get_busy() || ui.get_signed_in() || (retry && !ui.get_can_retry_session()) {
                return;
            }
            let epoch = epoch.fetch_add(1, Ordering::SeqCst).wrapping_add(1);
            let origin = ui.get_origin().into();
            let username = ui.get_username().into();
            let command = if retry {
                AuthCommand::Retry {
                    epoch,
                    origin,
                    username,
                }
            } else {
                AuthCommand::Login {
                    epoch,
                    origin,
                    username,
                    password: ui.get_password().into(),
                    remember: ui.get_keep_signed_in(),
                }
            };
            ui.set_password("".into());
            ui.set_busy(true);
            ui.set_restoring_login(retry);
            ui.set_can_retry_session(false);
            ui.set_status("".into());
            ui.set_error(false);
            ui.set_auth_notice("".into());
            if tx.send(command).is_err() {
                ui.set_busy(false);
                ui.set_restoring_login(false);
                ui.set_error(true);
                ui.set_status("Sign-in is unavailable. Reopen the desktop and try again.".into());
            }
        };
        if retry {
            ui.on_retry_session(callback);
        } else {
            ui.on_sign_in(callback);
        }
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
            ui.set_can_retry_session(false);
            ui.set_auth_notice("".into());
            ui.set_status("".into());
            ui.set_error(false);
            if !interrupt {
                return;
            }
            let epoch = epoch.fetch_add(1, Ordering::SeqCst).wrapping_add(1);
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
fn upcoming_keys(rows: impl Iterator<Item = QueueRow>) -> Vec<u64> {
    rows.filter(|r| r.upcoming)
        .filter_map(|r| r.key.parse().ok())
        .collect()
}
/// Convert a one-step upcoming move to the server's stable predecessor key.
fn move_after(rows: &[u64], key: u64, down: bool) -> Option<Option<u64>> {
    let i = rows.iter().position(|k| *k == key)?;
    if down {
        Some(Some(*rows.get(i + 1)?))
    } else if i == 0 {
        None
    } else {
        Some(i.checked_sub(2).map(|j| rows[j]))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn queue_move_uses_identity_not_position() {
        let rows = [
            (3, false, false),
            (7, true, false),
            (12, false, true),
            (41, false, true),
        ]
        .into_iter()
        .map(|(k, current, upcoming)| QueueRow {
            key: k.to_string().into(),
            current,
            upcoming,
            ..Default::default()
        });
        let keys = upcoming_keys(rows);
        assert_eq!(keys, vec![12, 41]);
        assert_eq!(move_after(&keys, 41, false), Some(None));
        assert_eq!(move_after(&keys, 12, false), None);
        assert_eq!(move_after(&keys, 41, true), None);
        let q = [91, 10, 42];
        assert_eq!(move_after(&q, 10, false), Some(None));
        assert_eq!(move_after(&q, 10, true), Some(Some(42)));
        assert_eq!(move_after(&q, 42, true), None);
        assert_eq!(move_after(&q, 63, false), None);
    }
}
