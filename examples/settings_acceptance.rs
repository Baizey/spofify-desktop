//! Disposable native UI fixture: no authentication, network, playback, or storage workers.
//! Run only in a private display using scripts/acceptance/settings.py.
use slint::{Color, ModelRc, VecModel};

slint::include_modules!();

fn main() -> Result<(), slint::PlatformError> {
    let ui = AppWindow::new()?;
    ui.set_signed_in(true);
    ui.set_page_kind("appearance".into());
    ui.set_account_label("SyntheticSettingsUserWithLongUnbrokenName".into());
    ui.set_music_storage("12 downloads · 164.7 MB on this device".into());
    ui.set_palette_enabled(true);
    ui.set_palette_id("jade".into());
    display(&ui, 0, 1.0);

    let weak = ui.as_weak();
    ui.on_appearance_changed(move |mode, scale, _| display(&weak.unwrap(), mode, scale));
    let weak = ui.as_weak();
    ui.on_reset_appearance(move || display(&weak.unwrap(), 0, 1.0));
    let weak = ui.as_weak();
    ui.on_crossfade_changed(move |enabled| weak.unwrap().set_crossfade_enabled(enabled));
    let weak = ui.as_weak();
    ui.on_palette_selected(move |id| weak.unwrap().set_palette_id(id));
    let weak = ui.as_weak();
    ui.on_navigate(move |route| weak.unwrap().set_page_kind(route));
    let weak = ui.as_weak();
    ui.on_page_action(move |operation, request| {
        if operation == "offline" {
            weak.unwrap().set_manual_offline(request == "on");
        }
    });
    let weak = ui.as_weak();
    ui.on_sign_out(move || weak.unwrap().set_signed_in(false));

    if std::env::var_os("SETTINGS_FIXTURE_ERRORS").is_some() {
        display(&ui, 1, 1.25);
        ui.set_crossfade_status(
            "Couldn’t save Crossfade on this desktop. The setting hasn’t changed.".into(),
        );
        ui.set_palette_status("Account palettes are unavailable right now.".into());
        ui.set_palette_enabled(false);
    }
    ui.run()
}

fn display(ui: &AppWindow, mode: i32, scale: f32) {
    ui.set_appearance_mode(mode);
    ui.set_appearance_scale(scale);
    let presets = [
        (
            "jade",
            "Jade",
            [0x66e3b4, 0x08271d, 0x91bfff, 0xdfbd78],
            [0x08704e, 0xffffff, 0x2759a4, 0x77551b],
        ),
        (
            "ember",
            "Ember",
            [0xffad89, 0x31170d, 0xc4b0ff, 0xa4d7bd],
            [0xaa431b, 0xffffff, 0x654199, 0x23644a],
        ),
        (
            "violet",
            "Violet",
            [0xc8b5ff, 0x22183c, 0x6de2d5, 0xe8b4c5],
            [0x6442a5, 0xffffff, 0x0c6d67, 0x8b465e],
        ),
    ];
    let rows: Vec<_> = presets
        .into_iter()
        .map(|(id, title, dark, light)| {
            let rgba = if mode == 1 { light } else { dark };
            let [primary, on_primary, secondary, tertiary] =
                rgba.map(|value| Color::from_argb_encoded(0xff000000 | value));
            PaletteRow {
                id: id.into(),
                title: title.into(),
                primary,
                on_primary,
                secondary,
                tertiary,
            }
        })
        .collect();
    ui.set_palettes(ModelRc::new(VecModel::from(rows)));
}
