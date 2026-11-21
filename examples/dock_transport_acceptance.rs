//! Disposable native dock fixture. Projects playback states without the engine, audio, or an account.
use std::{path::Path, time::Duration};

use slint::{ModelRc, Timer, TimerMode, VecModel};

slint::include_modules!();

fn main() -> Result<(), slint::PlatformError> {
    let ui = AppWindow::new()?;
    ui.set_page_kind("history".into());
    ui.set_signed_in(true);
    ui.set_can_control(true);
    ui.set_remote_controls(true);
    ui.set_can_seek(true);
    ui.set_output_name("No output selected".into());
    ui.set_now_track_id("fixture-track".into());
    ui.set_now_title("Synthetic Tone 001".into());
    ui.set_now_credits(ModelRc::new(VecModel::from(vec![ArtistCredit {
        id: "ensemble".into(),
        name: "Synthetic ensemble".into(),
    }])));
    ui.set_now_album_id("fixture-album".into());
    ui.set_now_album_name("Synthetic album".into());
    ui.set_duration(193.0);
    ui.set_elapsed(46.0);
    ui.set_local_volume(0.65);

    const REQUEST: &str = "Connecting this output…";
    const LOADING: &str = "Loading audio…";
    let weak = ui.as_weak();
    ui.on_toggle_play(move || {
        let ui = weak.unwrap();
        if ui.get_playing() {
            ui.set_playing(false);
            ui.set_elapsed(46.0);
            ui.set_output_name("No output selected".into());
        } else if ui.get_player_status().is_empty() {
            ui.set_player_status(REQUEST.into());
        }
    });

    // The driver releases each phase in the fixture's private working directory
    // only after observing it. CI speed cannot cause a phase to be skipped.
    let timer = Timer::default();
    let weak = ui.as_weak();
    timer.start(TimerMode::Repeated, Duration::from_millis(16), move || {
        let ui = weak.unwrap();
        match ui.get_player_status().as_str() {
            REQUEST if consume("buffer") => {
                ui.set_output_name("This desktop".into());
                ui.set_player_status(LOADING.into());
            }
            LOADING if consume("known-duration") => ui.set_duration(193.0),
            LOADING if consume("play") => {
                ui.set_playing(true);
                ui.set_player_status("".into());
            }
            "" if !ui.get_playing() && consume("unknown-duration") => ui.set_duration(0.0),
            "" if !ui.get_playing() && consume("dense") => ui.set_density_scale(1.25),
            "" if ui.get_playing() && consume("tick") => ui.set_elapsed(61.0),
            _ => {}
        }
    });
    ui.run()
}

fn consume(gate: &str) -> bool {
    std::fs::remove_file(Path::new(gate)).is_ok()
}
