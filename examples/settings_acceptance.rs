//! Disposable native UI fixture: no authentication, network, playback, or storage workers.
//! Run only in a private display using scripts/acceptance/settings.py.
slint::include_modules!();

fn main() -> Result<(), slint::PlatformError> {
    let ui = AppWindow::new()?;
    ui.set_signed_in(true);
    ui.set_page_kind("settings".into());
    ui.set_account_label("SyntheticSettingsUserWithLongUnbrokenName".into());
    ui.set_music_storage("12 downloads · 164.7 MB on this device".into());
    let errors = std::env::var_os("SETTINGS_FIXTURE_ERRORS").is_some();
    ui.set_density_scale(if errors { 1.25 } else { 1.0 });

    let weak = ui.as_weak();
    ui.on_density_changed(move |scale| weak.unwrap().set_density_scale(scale));
    let weak = ui.as_weak();
    ui.on_crossfade_changed(move |enabled| {
        let ui = weak.unwrap();
        if errors {
            ui.set_crossfade_status(
                "Couldn’t save Crossfade on this desktop. The setting hasn’t changed.".into(),
            );
        } else {
            ui.set_crossfade_enabled(enabled);
        }
    });
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
    ui.run()
}
