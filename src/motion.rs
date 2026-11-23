//! Slint has no reduced-motion setting on its winit backend. Read the host
//! desktop preference when available; never change it or block app startup.
use crate::AppWindow;
use slint::ComponentHandle;
use slint::winit_030::{EventResult, WinitWindowAccessor, winit::event::WindowEvent};

pub fn install(ui: &AppWindow) {
    // Wayland doesn't expose a minimized/occluded event through winit. Use
    // activation as a conservative gate, also stopping on native occlusion
    // where available. Never query window state from the indicator's timer.
    let weak = ui.as_weak();
    let (mut focused, mut occluded) = (false, false);
    ui.window().on_winit_window_event(move |_, event| {
        match event {
            WindowEvent::Focused(active) => focused = *active,
            WindowEvent::Occluded(hidden) => occluded = *hidden,
            _ => return EventResult::Propagate,
        }
        if let Some(ui) = weak.upgrade() {
            ui.set_indicator_window_active(focused && !occluded);
        }
        EventResult::Propagate
    });

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
        // Keep the continuously playing glyph static until the preference
        // is known; don't change existing reorder transitions while waiting.
        ui.set_indicator_preference_pending(true);
        let window = ui.as_weak();
        std::thread::spawn(move || {
            let reduced = std::process::Command::new(tool)
                .args(args)
                .output()
                .is_ok_and(|output| {
                    let value = String::from_utf8_lossy(&output.stdout);
                    output.status.success()
                        && match tool {
                            "gsettings" => value.trim() == "false",
                            _ => value.trim().parse::<f32>() == Ok(0.0),
                        }
                });
            let _ = window.upgrade_in_event_loop(move |ui| {
                if reduced {
                    ui.set_reduced_motion(true);
                }
                ui.set_indicator_preference_pending(false);
            });
        });
    }
}
