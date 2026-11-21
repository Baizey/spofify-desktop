//! Slint has no reduced-motion setting on its winit backend. Read the host
//! desktop preference when available; never change it or block app startup.
use crate::AppWindow;
use slint::ComponentHandle;

pub fn install(ui: &AppWindow) {
    if std::env::var_os("SPOFIFY_REDUCED_MOTION").as_deref() == Some("1".as_ref()) {
        ui.set_reduced_motion(true);
        return;
    }
    #[cfg(target_os = "linux")]
    {
        let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
        let command = if desktop
            .split(':')
            .any(|name| name.eq_ignore_ascii_case("GNOME"))
        {
            Some((
                "gsettings",
                &["get", "org.gnome.desktop.interface", "enable-animations"][..],
            ))
        } else if desktop
            .split(':')
            .any(|name| name.eq_ignore_ascii_case("KDE"))
        {
            let tool = if std::env::var("KDE_SESSION_VERSION").as_deref() == Ok("5") {
                "kreadconfig5"
            } else {
                "kreadconfig6"
            };
            Some((
                tool,
                &[
                    "--file",
                    "kdeglobals",
                    "--group",
                    "KDE",
                    "--key",
                    "AnimationDurationFactor",
                ][..],
            ))
        } else {
            None
        };
        let Some((tool, args)) = command else { return };
        let window = ui.as_weak();
        std::thread::spawn(move || {
            let Ok(output) = std::process::Command::new(tool).args(args).output() else {
                return;
            };
            let value = String::from_utf8_lossy(&output.stdout);
            let reduced = output.status.success()
                && match tool {
                    "gsettings" => value.trim() == "false",
                    _ => value.trim().parse::<f32>() == Ok(0.0),
                };
            if reduced {
                let _ = window.upgrade_in_event_loop(|ui| ui.set_reduced_motion(true));
            }
        });
    }
}
