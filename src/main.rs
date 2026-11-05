mod api;
mod audio;
mod engine;
mod player;
slint::include_modules!();
use slint::ComponentHandle;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

fn send(tx: &tokio::sync::mpsc::Sender<engine::Command>, command: engine::Command, ui: &AppWindow) {
    if tx.try_send(command).is_err() {
        ui.set_busy(false);
        ui.set_error(true);
        ui.set_status("Desktop is busy. Please try again.".into());
    }
}
fn main() -> Result<(), slint::PlatformError> {
    // Inspection has no listener by default. Production builds refuse even an explicit opt-in.
    if !cfg!(debug_assertions)
        && (std::env::var_os("SLINT_MCP_PORT").is_some()
            || std::env::var_os("SLINT_TEST_SERVER").is_some())
    {
        eprintln!("Developer inspection is unavailable in release builds.");
        std::process::exit(2);
    }
    let ui = AppWindow::new()?;
    // Optional native acceptance size; never active in release or ordinary launches.
    if cfg!(debug_assertions) && std::env::var_os("SLINT_MCP_PORT").is_some() {
        if let Ok(size) = std::env::var("SPOFIFY_DEV_SIZE") {
            if let Some((w, h)) = size
                .split_once('x')
                .and_then(|(w, h)| Some((w.parse::<u32>().ok()?, h.parse::<u32>().ok()?)))
            {
                if (800..=1600).contains(&w) && (600..=1200).contains(&h) {
                    ui.set_inspection_width(w as f32);
                    ui.set_inspection_height(h as f32);
                }
            }
        }
    }
    let epoch = Arc::new(AtomicU64::new(0));
    let search = Arc::new(AtomicU64::new(0));
    let (tx, worker) = engine::start(ui.as_weak(), epoch.clone(), search.clone());
    {
        let (weak, tx, epoch) = (ui.as_weak(), tx.clone(), epoch.clone());
        ui.on_sign_in(move || {
            if let Some(ui) = weak.upgrade() {
                if ui.get_busy() {
                    return;
                }
                let current = epoch.fetch_add(1, Ordering::SeqCst) + 1;
                let command = engine::Command::Login {
                    epoch: current,
                    origin: ui.get_origin().to_string(),
                    username: ui.get_username().to_string(),
                    password: ui.get_password().to_string(),
                    lan: ui.get_lan_consent(),
                };
                ui.set_password("".into());
                ui.set_status("".into());
                ui.set_busy(true);
                send(&tx, command, &ui);
            }
        });
    }
    {
        let (weak, tx, epoch, search) = (ui.as_weak(), tx.clone(), epoch.clone(), search.clone());
        ui.on_sign_out(move || {
            if let Some(ui) = weak.upgrade() {
                let current = epoch.fetch_add(1, Ordering::SeqCst) + 1;
                search.fetch_add(1, Ordering::SeqCst);
                ui.set_password("".into());
                ui.set_username("".into());
                ui.set_query("".into());
                ui.set_signed_in(false);
                ui.set_busy(false);
                ui.set_rows(Default::default());
                send(&tx, engine::Command::Logout(current), &ui);
            }
        });
    }
    {
        let (weak, tx, epoch, search) = (ui.as_weak(), tx.clone(), epoch.clone(), search.clone());
        ui.on_server_edited(move || {
            if let Some(ui) = weak.upgrade() {
                let current = epoch.fetch_add(1, Ordering::SeqCst) + 1;
                search.fetch_add(1, Ordering::SeqCst);
                ui.set_password("".into());
                ui.set_lan_consent(false);
                ui.set_busy(false);
                ui.set_status("".into());
                send(&tx, engine::Command::Logout(current), &ui);
            }
        });
    }
    for retry in [false, true] {
        let (weak, tx, epoch, search) = (ui.as_weak(), tx.clone(), epoch.clone(), search.clone());
        let callback = move || {
            if let Some(ui) = weak.upgrade() {
                let sequence = search.fetch_add(1, Ordering::SeqCst) + 1;
                ui.set_rows(Default::default());
                ui.set_status("".into());
                ui.set_busy(!ui.get_query().trim().is_empty());
                send(
                    &tx,
                    engine::Command::Search {
                        epoch: epoch.load(Ordering::SeqCst),
                        sequence,
                        query: ui.get_query().to_string(),
                    },
                    &ui,
                );
            }
        };
        if retry {
            ui.on_retry(callback)
        } else {
            ui.on_search_edited(callback)
        }
    }
    {
        let (weak, tx) = (ui.as_weak(), tx.clone());
        ui.on_play_track(move |index| {
            if let Some(ui) = weak.upgrade() {
                if index >= 0 {
                    send(&tx, engine::Command::Play(index as usize), &ui);
                }
            }
        });
    }
    {
        let (weak, tx) = (ui.as_weak(), tx.clone());
        ui.on_toggle_play(move || {
            if let Some(ui) = weak.upgrade() {
                send(&tx, engine::Command::Toggle, &ui);
            }
        });
    }
    {
        let (weak, tx) = (ui.as_weak(), tx.clone());
        ui.on_stop(move || {
            if let Some(ui) = weak.upgrade() {
                send(&tx, engine::Command::Stop, &ui);
            }
        });
    }
    let result = ui.run();
    epoch.fetch_add(1, Ordering::SeqCst); // output source is fenced before worker/network shutdown
    let _ = tx.blocking_send(engine::Command::Shutdown);
    if worker.join().is_err() {
        eprintln!("Desktop worker shut down unexpectedly.");
    }
    result
}
