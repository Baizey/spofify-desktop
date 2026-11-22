//! Disposable native AppWindow fixture: no account, network, audio, or stores.
//! Run with the software backend in a private scale-1 Cage session; pass a new
//! directory for before/after PNGs and set the headless output to 1280×685.
use image::{Rgba, RgbaImage};
use slint::platform::{Key, PointerEventButton::Left, WindowEvent};
use slint::{ComponentHandle, LogicalPosition, ModelRc, Timer, VecModel};
use std::{path::Path, time::Duration};

slint::include_modules!();

type Crop = [u32; 4]; // left, top, right, bottom (exclusive)
const DOCK: Crop = [391, 637, 939, 665];
const VOLUME: Crop = [1146, 605, 1258, 641];
const MUTE_BUTTON: Crop = [1107, 606, 1145, 645];
const PRIMARY: Rgba<u8> = Rgba([0x66, 0xe3, 0xb4, 255]);
const FOCUS: Rgba<u8> = Rgba([0xdf, 0xbd, 0x78, 255]);
const MUTED: Rgba<u8> = Rgba([0xb4, 0xbd, 0xcb, 255]);

fn press(ui: &AppWindow, key: Key) {
    ui.window()
        .dispatch_event(WindowEvent::KeyPressed { text: key.into() });
    ui.window()
        .dispatch_event(WindowEvent::KeyReleased { text: key.into() });
}

fn frame(ui: &AppWindow) -> RgbaImage {
    let pixels = ui
        .window()
        .take_snapshot()
        .expect("native software snapshot");
    assert_eq!((pixels.width(), pixels.height()), (1280, 685));
    RgbaImage::from_raw(pixels.width(), pixels.height(), pixels.as_bytes().to_vec()).unwrap()
}

fn points([x, y, right, bottom]: Crop) -> impl Iterator<Item = (u32, u32)> {
    (y..bottom).flat_map(move |y| (x..right).map(move |x| (x, y)))
}

fn ink(image: &RgbaImage, crop: Crop, color: Rgba<u8>) -> Vec<(u32, u32)> {
    points(crop)
        .filter(|&(x, y)| *image.get_pixel(x, y) == color)
        .collect()
}

fn focus(
    ui: &AppWindow,
    frames: &Path,
    label: &str,
    crop: Crop,
    resting: Rgba<u8>,
    zero: bool,
) -> RgbaImage {
    let before = frame(ui);
    assert!(
        ink(&before, crop, FOCUS).is_empty(),
        "{label}: already focused"
    );
    before
        .save(frames.join(format!("{label}-before.png")))
        .unwrap();
    let expected = ink(&before, crop, resting);
    assert!(!expected.is_empty(), "{label}: no resting accent");
    let left = expected.iter().map(|p| p.0).min().unwrap();
    let right = expected.iter().map(|p| p.0).max().unwrap();
    let top = expected.first().unwrap().1;
    let bottom = expected.last().unwrap().1;
    // Track-only ink is 3px tall; the thumb remains taller, including at zero.
    assert!(bottom - top >= 6, "{label}: resting thumb missing");
    assert_eq!(right - left < 10, zero, "{label}: resting fill geometry");
    for _ in 0..32 {
        press(ui, Key::Tab);
        let after = frame(ui);
        let painted = ink(&after, crop, FOCUS);
        if painted.is_empty() {
            continue;
        }
        after.save(frames.join(format!("{label}-tab.png"))).unwrap();
        assert!(
            painted == expected,
            "{label}: Tab must recolor the exact active bar/thumb pixels"
        );
        assert!(
            points(crop).all(|(x, y)| {
                // Allow one extra pixel of antialiasing at the accent's edges.
                (x >= left - 1 && x <= right + 1 && y >= top - 1 && y <= bottom + 1)
                    || before.get_pixel(x, y) == after.get_pixel(x, y)
            }),
            "{label}: Tab painted outside the accent (outline or layout shift)"
        );
        println!(
            "PASS {label}: {} Tab focus pixels, unchanged geometry and no outline",
            painted.len()
        );
        return after;
    }
    panic!("{label}: Tab never painted the focus color on the slider");
}

fn drag(ui: &AppWindow, frames: &Path, label: &str, crop: Crop, x: f32, y: f32, delta: f32) {
    let start = LogicalPosition::new(x, y);
    let end = LogicalPosition::new(x + delta, y);
    let window = ui.window();
    // Start unfocused so pointer focus is exercised independently of Tab.
    press(ui, Key::Tab);
    window.dispatch_event(WindowEvent::PointerMoved { position: start });
    window.dispatch_event(WindowEvent::PointerPressed {
        position: start,
        button: Left,
    });
    window.dispatch_event(WindowEvent::PointerMoved { position: end });
    let held = frame(ui);
    held.save(frames.join(format!("{label}-drag.png"))).unwrap();
    assert!(
        ink(&held, crop, FOCUS)
            .iter()
            .all(|&(_, py)| (py as f32 - y).abs() <= 11.0),
        "{label}: pointer drag painted an outline"
    );
    window.dispatch_event(WindowEvent::PointerReleased {
        position: end,
        button: Left,
    });
}

fn check_timeline(ui: &AppWindow, frames: &Path, label: &str, crop: Crop, x: f32, y: f32) {
    ui.set_elapsed(96.0);
    focus(ui, frames, label, crop, PRIMARY, false);
    press(ui, Key::RightArrow);
    assert!(
        (ui.get_elapsed() - 97.0).abs() < 0.01,
        "{label}: Right did not commit a seek"
    );
    drag(ui, frames, label, crop, x, y, 50.0);
    assert!(
        ui.get_elapsed() > 107.0,
        "{label}: drag did not commit a seek"
    );
    press(ui, Key::Tab);
    println!("PASS {label}: arrow and drag seek");
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let frames = std::env::args_os().nth(1).expect("new frame directory");
    let frames = std::path::PathBuf::from(frames);
    std::fs::create_dir(&frames)?;
    let ui = AppWindow::new()?;
    ui.set_inspection_width(1280.0);
    ui.set_inspection_height(685.0);
    ui.set_page_kind("now".into());
    ui.set_signed_in(true);
    ui.set_can_control(true);
    ui.set_remote_controls(true);
    ui.set_can_seek(true);
    ui.set_now_track_id("fixture-track".into());
    ui.set_now_title("Synthetic track".into());
    ui.set_duration(240.0);
    ui.set_elapsed(96.0);
    ui.set_local_volume(0.65);
    ui.set_waveform(ModelRc::new(VecModel::from(
        (0..72)
            .map(|i| 0.25 + (i * 7 % 11) as f32 / 15.0)
            .collect::<Vec<_>>(),
    )));
    let weak = ui.as_weak();
    ui.on_seek(move |position| weak.unwrap().set_elapsed(position));
    let weak = ui.as_weak();
    // Match Desktop: adjusting the level does not change the separate mute flag.
    ui.on_local_volume_change(move |level| weak.unwrap().set_local_volume(level));
    let weak = ui.as_weak();
    Timer::single_shot(Duration::from_millis(300), move || {
        let ui = weak.unwrap();
        check_timeline(&ui, &frames, "now-dock-waveform", DOCK, 613.0, 649.0);
        ui.set_page_kind("history".into());
        check_timeline(&ui, &frames, "dock", DOCK, 613.0, 649.0);
        let unmuted = focus(&ui, &frames, "volume", VOLUME, PRIMARY, false);
        press(&ui, Key::RightArrow);
        assert!(
            (ui.get_local_volume() - 0.7).abs() < 0.001,
            "volume: Right did not step by 5%"
        );
        drag(&ui, &frames, "volume", VOLUME, 1222.0, 623.0, -40.0);
        assert!(
            ui.get_local_volume() < 0.50,
            "volume: drag did not change the level"
        );
        press(&ui, Key::Tab);
        println!("PASS volume: arrow and drag adjustment");
        ui.set_local_volume(0.65);
        ui.set_local_muted(true);
        let muted = focus(&ui, &frames, "muted-volume", VOLUME, MUTED, false);
        assert!(ui.get_local_muted());
        assert!(
            points(MUTE_BUTTON).any(|(x, y)| unmuted.get_pixel(x, y) != muted.get_pixel(x, y)),
            "muted volume lost its separate visible mute indicator"
        );
        press(&ui, Key::Tab);
        ui.set_local_volume(0.0);
        ui.set_local_muted(false);
        focus(&ui, &frames, "zero-volume", VOLUME, PRIMARY, true);
        assert!(!ui.get_local_muted());
        press(&ui, Key::Tab);
        ui.set_local_muted(true);
        focus(&ui, &frames, "muted-zero-volume", VOLUME, MUTED, true);
        press(&ui, Key::Tab);
        ui.set_elapsed(96.0);
        ui.set_can_seek(false);
        let disabled = frame(&ui);
        disabled.save(frames.join("disabled-dock.png")).unwrap();
        assert_eq!(
            *disabled.get_pixel(612, 649),
            MUTED,
            "disabled seek thumb lost its muted styling"
        );
        for _ in 0..25 {
            press(&ui, Key::Tab);
            assert!(
                ink(&frame(&ui), DOCK, FOCUS).is_empty(),
                "disabled seek took focus"
            );
        }
        println!("PASS disabled dock timeline: muted thumb and skipped by Tab");
        slint::quit_event_loop().unwrap();
    });
    ui.run()?;
    Ok(())
}
