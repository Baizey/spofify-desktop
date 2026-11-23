//! Actual AppWindow against synthetic models; no Desktop workers or services.
//! cargo build --offline --locked --example playing_indicator_motion_acceptance
//! runtime=$(mktemp -d)
//! env -u DISPLAY -u WAYLAND_DISPLAY -u XDG_CURRENT_DESKTOP -u SPOFIFY_REDUCED_MOTION \
//!   XDG_RUNTIME_DIR="$runtime" \
//!   WLR_BACKENDS=headless WLR_RENDERER=pixman WLR_HEADLESS_OUTPUTS=1 \
//!   SLINT_BACKEND=winit-software cage -d -- sh -c \
//!   'wlr-randr --output HEADLESS-1 --custom-mode 1280x720@60Hz && exec "$@"' sh \
//!   target/debug/examples/playing_indicator_motion_acceptance /tmp/playing-indicator-frames
//! rmdir "$runtime"

use image::{Rgba, RgbaImage};
use slint::platform::WindowEvent;
use slint::{
    ComponentHandle, Image, LogicalPosition, ModelRc, Rgba8Pixel, SharedPixelBuffer, Timer,
    VecModel,
};
use std::{cell::RefCell, os::unix::fs::DirBuilderExt, rc::Rc, time::Duration};

slint::include_modules!();

#[path = "../src/motion.rs"]
mod motion;

type Point = (u32, u32);
const MINT: Rgba<u8> = Rgba([0x66, 0xe3, 0xb4, 255]);
const SAMPLES: usize = 6;

fn sleeve() -> Image {
    let mut pixels = SharedPixelBuffer::<Rgba8Pixel>::new(64, 64);
    for (i, pixel) in pixels.make_mut_slice().iter_mut().enumerate() {
        *pixel = if (i % 64 + i / 64 * 2) % 27 < 8 {
            Rgba8Pixel {
                r: 231,
                g: 166,
                b: 93,
                a: 255,
            }
        } else {
            Rgba8Pixel {
                r: 44,
                g: 74,
                b: 127,
                a: 255,
            }
        };
    }
    Image::from_rgba8(pixels)
}

fn snapshot(ui: &AppWindow) -> RgbaImage {
    let pixels = ui
        .window()
        .take_snapshot()
        .expect("native software snapshot");
    assert_eq!((pixels.width(), pixels.height()), (1280, 720));
    RgbaImage::from_raw(pixels.width(), pixels.height(), pixels.as_bytes().to_vec()).unwrap()
}

// Anchor the existing 16x16 slots from the unchanged centered paused dot,
// never from the moving paint. Look only in the two leading row columns.
fn footprint(frame: &RgbaImage, xs: std::ops::Range<u32>, ys: std::ops::Range<u32>) -> Point {
    let mut origins = Vec::new();
    for y in ys {
        for x in xs.clone() {
            if ![(x, y), (x - 1, y), (x + 1, y), (x, y - 1), (x, y + 1)]
                .into_iter()
                .all(|(x, y)| *frame.get_pixel(x, y) == MINT)
            {
                continue;
            }
            let points: Vec<_> = (y - 5..=y + 5)
                .flat_map(|py| (x - 5..=x + 5).map(move |px| (px, py)))
                .filter(|&(px, py)| *frame.get_pixel(px, py) == MINT)
                .collect();
            let (left, right) = (
                points.iter().map(|p| p.0).min().unwrap(),
                points.iter().map(|p| p.0).max().unwrap(),
            );
            let (top, bottom) = (points[0].1, points.last().unwrap().1);
            if (3..=5).contains(&(right - left)) && (3..=5).contains(&(bottom - top)) {
                let origin = ((left + right + 1) / 2 - 8, (top + bottom + 1) / 2 - 8);
                if !origins.contains(&origin) {
                    origins.push(origin);
                }
            }
        }
    }
    assert_eq!(
        origins.len(),
        1,
        "expected exactly one paused dot, found {origins:?}"
    );
    origins[0]
}

fn compare(a: &RgbaImage, b: &RgbaImage, slots: &[Point], label: &str) -> Vec<usize> {
    let mut changed = vec![0; slots.len()];
    for (x, y, p) in a.enumerate_pixels() {
        if p != b.get_pixel(x, y) {
            let slot = slots
                .iter()
                .position(|&(sx, sy)| (sx..sx + 16).contains(&x) && (sy..sy + 16).contains(&y))
                .unwrap_or_else(|| panic!("{label}: unexpected pixel difference at ({x}, {y})"));
            changed[slot] += 1;
        }
    }
    changed
}

const PHASES: [(&str, bool); 8] = [
    ("01-cover-paused", false),
    ("02-cover-playing", true),
    ("03-artless-paused", false),
    ("04-artless-playing", true),
    ("05-artless-reduced", false),
    ("06-artless-inactive", false),
    ("07-artless-reactivated", true),
    ("08-queue-scrolled-out", true),
];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let output =
        std::path::PathBuf::from(std::env::args_os().nth(1).expect("new output directory"));
    std::fs::DirBuilder::new().mode(0o700).create(&output)?;
    let ui = AppWindow::new()?;
    ui.set_inspection_width(1280.);
    ui.set_inspection_height(720.);
    ui.set_signed_in(true);
    ui.set_page_kind("search".into());
    ui.set_page_key("fixture-search".into());
    ui.set_page_title("Search".into());
    ui.set_page_subtitle("1 result".into());
    ui.set_query("P(Doom)".into());
    ui.set_now_track_id("p-doom".into());
    ui.set_now_title("P(Doom)".into());
    ui.set_now_credits(ModelRc::new(VecModel::from(vec![ArtistCredit {
        id: "vera-north".into(),
        name: "Vera North".into(),
    }])));
    ui.set_can_control(true);
    ui.set_remote_controls(true);
    ui.set_duration(222.);
    ui.set_elapsed(121.);
    ui.set_queue_room_key("fixture-room".into());
    ui.set_browse_rows(ModelRc::new(VecModel::from(vec![BrowseRow {
        track_id: "p-doom".into(),
        occurrence_key: "search-hit".into(),
        title: "P(Doom)".into(),
        subtitle: "Vera North".into(),
        kind: "track".into(),
        playable: true,
        ..Default::default()
    }])));
    // Same recording in every Queue phase: only the cursor occurrence may move.
    ui.set_queue_rows(ModelRc::new(VecModel::from(
        (0..24)
            .map(|i| QueueRow {
                key: format!("queue-{i}").into(),
                track_id: "p-doom".into(),
                title: "P(Doom)".into(),
                subtitle: "Vera North".into(),
                current: i == 0,
                upcoming: i > 1,
                ..Default::default()
            })
            .collect::<Vec<_>>(),
    )));
    let art = sleeve();
    ui.set_browse_artwork(ModelRc::new(VecModel::from(vec![art.clone()])));
    ui.set_cover_art(art);
    assert!(
        !ui.get_indicator_window_active(),
        "activation must default to false"
    );
    motion::install(&ui);

    let timer = Timer::default();
    let weak = ui.as_weak();
    let complete = Rc::new(std::cell::Cell::new(false));
    let done = complete.clone();
    let capture = RefCell::new((
        0usize,
        Vec::<RgbaImage>::new(),
        Vec::<Point>::new(),
        None::<RgbaImage>,
    ));
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(173),
        move || {
            let ui = weak.unwrap();
            let (phase, frames, slots, preceding) = &mut *capture.borrow_mut();
            let Some(&(label, moving)) = PHASES.get(*phase) else {
                return;
            };
            let frame = snapshot(&ui);
            frame
                .save(output.join(format!("{label}-{}.png", frames.len())))
                .unwrap();
            frames.push(frame);
            if frames.len() < SAMPLES {
                return;
            }
            if matches!(*phase, 0 | 2) {
                *slots = vec![
                    footprint(&frames[0], 243..265, 190..281),
                    footprint(&frames[0], 1008..1027, 138..263),
                ];
            }
            let allowed = if !moving {
                &[]
            } else if *phase == 7 {
                &slots[..1]
            } else {
                &slots[..]
            };
            let mut variation = vec![0; allowed.len()];
            for frame in &frames[1..] {
                for (total, count) in variation
                    .iter_mut()
                    .zip(compare(&frames[0], frame, allowed, label))
                {
                    *total += count;
                }
            }
            assert!(
                variation.iter().all(|&n| n > 0),
                "{label}: not all visible indicators varied: {variation:?}"
            );
            if matches!(*phase, 1 | 3 | 4 | 5 | 6) {
                let allowed = if *phase == 5 { &[][..] } else { &slots[..] };
                let changed = compare(preceding.as_ref().unwrap(), &frames[0], allowed, label);
                if matches!(*phase, 1 | 3) {
                    assert!(
                        changed.iter().all(|&n| n > 0),
                        "{label}: paused dots did not change"
                    );
                }
            }
            if *phase == 7 {
                let prior = preceding.as_ref().unwrap();
                let (x, y) = slots[1];
                assert!(
                    (y..y + 16)
                        .all(|py| (x..x + 16).all(|px| *frames[0].get_pixel(px, py) != MINT))
                );
                assert!(
                    (420..500).any(|py| (980..1260)
                        .any(|px| prior.get_pixel(px, py) != frames[0].get_pixel(px, py))),
                    "wheel did not scroll Queue"
                );
            }
            println!("PASS {label}: slots={slots:?} changed-pixel totals={variation:?}");
            *preceding = Some(frames.swap_remove(0));
            frames.clear();
            *phase += 1;
            match *phase {
                1 => {
                    // Private headless Cage has no keyboard seat; exercise the
                    // same binding directly when winit does not emit Focused.
                    if !ui.get_indicator_window_active() {
                        println!("No native focus in headless Cage; injecting fixture activation");
                        ui.set_indicator_window_active(true);
                    }
                    ui.set_playing(true);
                }
                2 => {
                    ui.set_playing(false);
                    ui.set_cover_art(Image::default());
                }
                3 => ui.set_playing(true),
                4 => ui.set_reduced_motion(true),
                // Drive the gate for focus loss/return within single-client Cage.
                5 => {
                    ui.set_indicator_window_active(false);
                    ui.set_reduced_motion(false);
                }
                6 => ui.set_indicator_window_active(true),
                7 => ui.window().dispatch_event(WindowEvent::PointerScrolled {
                    position: LogicalPosition::new(1170., 475.),
                    delta_x: 0.,
                    delta_y: -720.,
                }),
                _ => {
                    done.set(true);
                    slint::quit_event_loop().unwrap();
                }
            }
        },
    );
    ui.run()?;
    assert!(complete.get(), "window closed before fixture finished");
    Ok(())
}
