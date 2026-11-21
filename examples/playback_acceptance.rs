//! Disposable AppWindow fixture: no account, network, audio, or profile workers.
use slint::{Image, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, Timer, TimerMode, VecModel};
use std::{
    fs::{self, File},
    io::Write,
    path::PathBuf,
    rc::Rc,
    time::Duration,
};

slint::include_modules!();

fn picker_fixture(ui: &AppWindow) -> Option<(Rc<File>, Timer)> {
    let journal_path = PathBuf::from(std::env::var_os("SPOFIFY_PICKER_FIXTURE_JOURNAL")?);
    let journal =
        Rc::new(File::create(&journal_path).expect("create disposable device picker journal"));
    let row = |id: &str, name: &str, selected, available, can_play| DeviceRow {
        id: id.into(),
        name: name.into(),
        selected,
        available,
        can_play,
    };
    let mut rows = vec![
        row("desktop-1", "Desktop speakers", true, true, true),
        row("remote-7", "Studio receiver", false, true, true),
        row("remote-12", "Travel speaker", false, false, true),
        row("viewer-6", "Library browser", false, true, false),
    ];
    rows.extend((1..=8).map(|i| {
        row(
            &format!("extra-{i}"),
            &format!("Room output {i:02}"),
            false,
            true,
            true,
        )
    }));
    load_roster(ui, &rows[..4]);

    let weak = ui.as_weak();
    let events = journal.clone();
    ui.on_device_action(move |op, id, name| {
        let ui = weak.unwrap();
        let mut rows: Vec<_> = ui.get_device_rows().iter().collect();
        match op.as_str() {
            "select" => {
                writeln!(&*events, "device\t{op}\t{id}").expect("record selection");
                for row in &mut rows {
                    row.selected = row.id == id && row.available && row.can_play;
                }
            }
            "rename" => {
                writeln!(&*events, "device\t{op}\t{id}\t{name}").expect("record rename");
                if let Some(chosen) = rows
                    .iter_mut()
                    .find(|row| row.id == id && row.selected && row.available)
                {
                    chosen.name = name;
                }
            }
            _ => panic!("unexpected disposable picker callback: {op}"),
        }
        load_roster(&ui, &rows);
    });

    let control = journal_path.with_extension("control");
    let timer = Timer::default();
    let weak = ui.as_weak();
    timer.start(TimerMode::Repeated, Duration::from_millis(50), move || {
        let Ok(command) = fs::read_to_string(&control) else {
            return;
        };
        let ui = weak.unwrap();
        match command.trim() {
            "change-page-key" => ui.set_page_key("fixture-new-history-context".into()),
            "drop-dock" => ui.set_now_track_id("".into()),
            "restore-dock" => ui.set_now_track_id("one".into()),
            "now" => {
                ui.set_page_kind("now".into());
                ui.set_can_back(true);
            }
            "history" => {
                ui.set_page_kind("history".into());
                ui.set_can_back(false);
            }
            "empty" => load_roster(&ui, &[]),
            "one" => load_roster(&ui, &rows[..1]),
            "mixed" => load_roster(&ui, &rows[..4]),
            "long" => load_roster(&ui, &rows),
            "long-names" => {
                let mut roster = rows[..4].to_vec();
                for device in &mut roster[..2] {
                    device.name =
                        format!("{} — {}", device.name, "extended label ".repeat(12)).into();
                }
                load_roster(&ui, &roster);
            }
            "selected-unavailable" => {
                let mut roster = rows[..4].to_vec();
                roster[0].available = false;
                load_roster(&ui, &roster);
            }
            "density-max" => ui.set_density_scale(1.25),
            "offline" => ui.set_manual_offline(true),
            "reconnect" => ui.set_reconnecting(true),
            "online" => {
                ui.set_manual_offline(false);
                ui.set_reconnecting(false);
            }
            "disconnect" => {
                let rows = rows[..4]
                    .iter()
                    .cloned()
                    .map(|mut row| {
                        row.available = false;
                        row.selected = false;
                        row
                    })
                    .collect::<Vec<_>>();
                load_roster(&ui, &rows);
                ui.set_output_name("Output disconnected".into());
            }
            _ => panic!("unknown disposable picker control: {command:?}"),
        }
        fs::remove_file(&control).expect("acknowledge disposable picker control");
    });
    Some((journal, timer))
}

fn load_roster(ui: &AppWindow, rows: &[DeviceRow]) {
    let output = rows.iter().find(|row| row.selected);
    ui.set_output_name(output.map_or_else(|| "No output selected".into(), |row| row.name.clone()));
    // Match the engine's snapshot replacement without claiming a real output.
    ui.set_device_rows(ModelRc::new(VecModel::from(rows.to_vec())));
}

fn main() -> Result<(), slint::PlatformError> {
    let ui = AppWindow::new()?;
    ui.set_page_kind("history".into());
    ui.set_signed_in(true);
    ui.set_can_control(true);
    ui.set_remote_controls(true);
    ui.set_can_seek(true);
    ui.set_playing(true);
    ui.set_now_track_id("one".into());
    ui.set_now_title("Synthetic Tone 001".into());
    ui.set_now_album_id("one".into());
    ui.set_now_album_name("Synthetic album".into());
    ui.set_duration(193.0);
    ui.set_elapsed(46.0);
    ui.set_local_volume(0.65);
    ui.set_local_muted(true);
    let mut artwork = SharedPixelBuffer::<Rgba8Pixel>::new(48, 48);
    artwork
        .make_mut_slice()
        .fill(Rgba8Pixel::new(75, 53, 94, 255));
    ui.set_cover_art(Image::from_rgba8(artwork));

    let credits = ModelRc::new(VecModel::from(vec![ArtistCredit {
        id: "ensemble".into(),
        name: "Synthetic ensemble".into(),
    }]));
    ui.set_now_credits(credits.clone());
    let rows = (0..3)
        .map(|i| QueueRow {
            key: i.to_string().into(),
            track_id: if i == 1 { "one" } else { "another" }.into(),
            title: format!("Queue recording {i}").into(),
            subtitle: if i == 2 { "Synthetic ensemble" } else { "" }.into(),
            credits: if i == 2 {
                ModelRc::default()
            } else {
                credits.clone()
            },
            album_id: "one".into(),
            album_name: "Synthetic album".into(),
            current: i == 1,
            upcoming: i == 2,
        })
        .collect::<Vec<_>>();
    ui.set_queue_rows(ModelRc::new(VecModel::from(rows)));

    let (picker_journal, _picker_control) = picker_fixture(&ui).unzip();
    let weak = ui.as_weak();
    ui.on_navigate(move |route| {
        let ui = weak.unwrap();
        if let Some(events) = &picker_journal {
            writeln!(&**events, "navigate\t{route}").expect("record navigation");
        }
        ui.set_can_back(route == "now");
        ui.set_page_kind(if route == "back" {
            "history".into()
        } else {
            route
        });
    });
    let weak = ui.as_weak();
    ui.on_seek(move |elapsed| weak.unwrap().set_elapsed(elapsed.round()));
    let weak = ui.as_weak();
    ui.on_local_volume_change(move |volume| {
        let ui = weak.unwrap();
        ui.set_local_volume(volume);
        ui.set_local_muted(false);
    });
    let weak = ui.as_weak();
    ui.on_previous(move || weak.unwrap().set_duration(0.0));
    let weak = ui.as_weak();
    ui.on_next(move || {
        let ui = weak.unwrap();
        ui.set_now_title(
            if ui.get_duration() > 0.0 && ui.get_now_title() == "Synthetic Tone 002" {
                "Synthetic Tone 002 — with a longer title".into()
            } else {
                "Synthetic Tone 002".into()
            },
        );
        ui.set_duration(193.0);
    });
    let weak = ui.as_weak();
    ui.on_toggle_play(move || {
        let ui = weak.unwrap();
        ui.set_playing(!ui.get_playing());
    });
    ui.run()
}
