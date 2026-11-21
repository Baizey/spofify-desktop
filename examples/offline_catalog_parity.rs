//! Disposable native test bridge for mode changes while a catalog route has focus.
//!
//! Production exposes the network-mode control on Settings only. This example
//! adds a private, one-way trigger for the same page action, without navigating
//! through Settings, mocking Desk state, or putting controls in the app.
#![allow(dead_code)] // App modules expose additional commands not used by this fixture.

#[path = "../src/actions.rs"]
mod actions;
#[path = "../src/api.rs"]
mod api;
#[path = "../src/api_transport.rs"]
mod api_transport;
#[path = "../src/artwork.rs"]
mod artwork;
#[path = "../src/audio.rs"]
mod audio;
#[path = "../src/audio_input.rs"]
mod audio_input;
#[path = "../src/auth.rs"]
mod auth;
#[path = "../src/collections.rs"]
mod collections;
#[path = "../src/desk.rs"]
mod desk;
#[path = "../src/engine.rs"]
mod engine;
#[path = "../src/local_catalog.rs"]
mod local_catalog;
#[path = "../src/login_store.rs"]
mod login_store;
#[path = "../src/media.rs"]
mod media;
#[path = "../src/mode.rs"]
mod mode;
#[path = "../src/motion.rs"]
mod motion;
#[path = "../src/offline.rs"]
mod offline;
#[path = "../src/output_identity.rs"]
mod output_identity;
#[path = "../src/pages.rs"]
mod pages;
#[path = "../src/personal.rs"]
mod personal;
#[path = "../src/personal_bridge.rs"]
mod personal_bridge;
#[path = "../src/player.rs"]
mod player;
#[path = "../src/playlist_browser.rs"]
mod playlist_browser;
#[path = "../src/playlist_observations.rs"]
mod playlist_observations;
#[path = "../src/playlist_pin_store.rs"]
mod playlist_pin_store;
#[path = "../src/playlist_pins.rs"]
mod playlist_pins;
#[path = "../src/playlist_view.rs"]
mod playlist_view;
#[path = "../src/presentation.rs"]
mod presentation;
#[path = "../src/replica.rs"]
mod replica;
#[path = "../src/saved.rs"]
mod saved;
#[path = "../src/socket.rs"]
mod socket;
#[path = "../src/storage.rs"]
mod storage;

slint::include_modules!();
use api::preferences;
use engine::AuthCommand;
use slint::{ComponentHandle, Timer, TimerMode};
use std::{
    env, fs,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::sync::{mpsc, watch};

fn advance_account_epoch(epoch: &AtomicU64) -> u64 {
    let _publication = playlist_pin_store::PUBLICATION_GATE.lock().unwrap();
    epoch.fetch_add(1, Ordering::SeqCst).wrapping_add(1)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let origin = env::var("SPOFIFY_OFFLINE_PARITY_WIRE_ORIGIN")?;
    let port = origin
        .strip_prefix("http://127.0.0.1:")
        .and_then(|value| value.parse::<u16>().ok());
    assert!(
        port.is_some_and(|port| port != 0),
        "owned fixture loopback only"
    );
    let private = env::current_dir()?;
    for var in [
        "HOME",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_RUNTIME_DIR",
    ] {
        assert!(
            std::path::PathBuf::from(env::var_os(var).expect("private profile"))
                .starts_with(&private)
        );
    }
    let trigger = private.join("parity-mode-intent");
    let ui = AppWindow::new()?;
    playlist_view::install(&ui);
    motion::install(&ui);
    preferences::install_density(&ui);
    let (master_gain, _writer) = preferences::install_local_volume(&ui);
    let output = engine::OutputPreferences {
        crossfade_enabled: preferences::install_playback(&ui),
        master_gain,
    };
    let epoch = Arc::new(AtomicU64::new(0));
    let (access_tx, access) = watch::channel(None);
    let mode = mode::ModeControl::default();
    let (auth_tx, auth_rx) = mpsc::unbounded_channel();
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
    let (desk, browser) = desk::start(&ui, epoch.clone(), access.clone(), tx, art);
    desk.bind(&ui, mode, access.clone());
    let _media = presentation::install(&ui, access, epoch.clone());

    let window = ui.as_weak();
    let login = auth_tx.clone();
    ui.on_sign_in(move || {
        let ui = window.unwrap();
        if ui.get_busy()
            || ui.get_signed_in()
            || ui.get_origin() != origin
            || ui.get_username() != "synthetic-wire"
        {
            return;
        }
        let next = advance_account_epoch(&epoch);
        let password = ui.get_password().into();
        ui.set_password("".into());
        ui.set_busy(true);
        ui.set_status("".into());
        let _ = login.send(AuthCommand::Login {
            epoch: next,
            origin: origin.clone(),
            username: "synthetic-wire".into(),
            password,
            remember: false,
        });
    });

    let timer = Timer::default();
    let window = ui.as_weak();
    timer.start(TimerMode::Repeated, Duration::from_millis(50), move || {
        if fs::read_to_string(&trigger).as_deref().ok() == Some("work offline\n")
            && fs::remove_file(&trigger).is_ok()
            && let Some(ui) = window.upgrade()
            && ui.get_signed_in()
        {
            ui.invoke_page_action("offline".into(), "on".into());
        }
    });
    let result = ui.run();
    timer.stop();
    let _ = auth_tx.send(AuthCommand::Shutdown);
    worker.join().expect("audio worker");
    drop(desk);
    browser.join().expect("catalog worker");
    result?;
    Ok(())
}
