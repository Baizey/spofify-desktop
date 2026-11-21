//! Disposable AppWindow fixture: no account, network, audio, or profile workers.
use slint::{Image, ModelRc, Rgba8Pixel, SharedPixelBuffer, VecModel};

slint::include_modules!();

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

    let weak = ui.as_weak();
    ui.on_navigate(move |route| {
        let ui = weak.unwrap();
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
