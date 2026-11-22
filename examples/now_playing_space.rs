//! Native Now Playing layout fixture: real AppWindow without engine, audio, or network.
//!
//! cargo build --offline --locked --example now_playing_space
//! runtime=$(mktemp -d /dev/shm/now-playing.XXXXXX)
//! env -u DISPLAY -u WAYLAND_DISPLAY -u WAYLAND_SOCKET -u SLINT_MCP_PORT XDG_RUNTIME_DIR="$runtime" \
//!   WLR_BACKENDS=headless WLR_RENDERER=pixman WLR_HEADLESS_OUTPUTS=1 \
//!   SLINT_BACKEND=winit-software cage -d -- sh -c \
//!   'wlr-randr --output HEADLESS-1 --custom-mode 1660x1180@60Hz && exec "$@"' sh \
//!   target/debug/examples/now_playing_space /tmp/now-playing-paused.png
//! rmdir "$runtime"
//!
//! Supply a new PNG path each time. Add `--overflow` for the stress line or
//! `--plain` for plain lyrics or `--queue-open` to keep the Queue visible.
//! `--check` exercises native input, implies overflow mode, and saves the
//! opening frame and suffixed interaction frames next to the supplied path.

#[path = "support/now_playing_checks.rs"]
mod now_playing_checks;
#[path = "support/now_playing_sleeve.rs"]
mod now_playing_sleeve;

use std::{
    fs::File,
    io::BufWriter,
    path::{Path, PathBuf},
    time::Duration,
};

use image::{ImageFormat, RgbaImage};
use slint::platform::{PointerEventButton, WindowEvent};
use slint::{ComponentHandle, LogicalPosition, Model, ModelRc, SharedString, Timer, VecModel};

slint::include_modules!();

const TIMED: &str = "The Last Train Out of the City Before the Snow Came";
const PLAIN: &str = "The Harbour at Four";
const MISSING: &str = "An Empty Platform";
const ARTIST: &str = "Lena Vale & the Signal Fires";
const ALBUM: &str = "A Map of Borrowed Light";
const SOURCE: &str = "Lyrics source: Lantern House session booklet";
const PLAIN_SOURCE: &str = "Lyrics source: Handwritten sleeve notes";
const FIRST: &[&str] = &[
    "At the corner where the night bus turns",
    "You drew a little window in the frost",
    "And every time the station lights went dark, we leaned closer to the glass",
    "We counted what the city hadn't lost",
    "",
    "The river kept a silver seam",
    "Under the bridges and the sleeping streets",
    "I held your letter up against the glass",
    "To see which words the morning wouldn't keep when the first train came in",
    "",
];
const REFRAIN: &[&str] = &[
    "Leave a light on in the hall",
    "Fold the map along the river",
    "If I miss the last train home",
    "Let the night run on forever",
    "",
    "The wires hummed above our heads",
    "Snow came softly through the station roof",
    "Someone on the stairs began to sing",
    "I thought for half a breath that it was you",
    "",
];
const LONG: &str = "If you ever take that long road back across the sleeping city in the hour before the bakery lights come on, leave every little radio in every empty room tuned to the weather and listen for the words we were too young to say out loud";
const PLAIN_VERSE: &str = "The harbour was empty at four\nI left my coat on the rail\n\nYou watched the pale ships turn\nThen left without saying farewell";

fn model<T: Clone + 'static>(rows: Vec<T>) -> ModelRc<T> {
    ModelRc::new(VecModel::from(rows))
}

fn delivery(ui: &AppWindow, overflow: bool) {
    ui.set_lyrics_track_id(ui.get_now_track_id());
    ui.set_lyrics_status("".into());
    ui.set_lyrics_text("".into());
    ui.set_lyrics_attribution("".into());
    ui.set_lyrics_lines(ModelRc::default());
    match ui.get_now_track_id().as_str() {
        "timed" => {
            let words = FIRST
                .iter()
                .copied()
                .chain(overflow.then_some(LONG))
                .chain(REFRAIN.iter().copied().cycle().take(50));
            ui.set_lyrics_lines(model(
                words
                    .enumerate()
                    .map(|(i, text)| TimedLyricLine {
                        start_ms: (i as i32 + 1) * 3_000,
                        text: text.into(),
                    })
                    .collect(),
            ));
            ui.set_lyrics_attribution(SOURCE.into());
        }
        "plain" => {
            let text = if overflow {
                format!("{PLAIN_VERSE}\n\n{LONG}").into()
            } else {
                PLAIN_VERSE.into()
            };
            ui.set_lyrics_text(text);
            ui.set_lyrics_attribution(PLAIN_SOURCE.into());
        }
        "missing" => ui.set_lyrics_status("No lyrics for this track.".into()),
        id => panic!("unexpected fixture track {id}"),
    }
}

fn select(ui: &AppWindow, selected: usize) {
    let recordings = [("timed", TIMED), ("plain", PLAIN), ("missing", MISSING)];
    let (id, title) = recordings[selected];
    let credit = model(vec![ArtistCredit {
        id: "lantern-house-artist".into(),
        name: ARTIST.into(),
    }]);
    // Leave the outgoing lyrics bound to their old ID until the driver releases
    // delivery. AppWindow must hide them during the new track's loading state.
    ui.set_now_track_id(id.into());
    ui.set_now_title(title.into());
    ui.set_now_credits(credit.clone());
    ui.set_cover_art(now_playing_sleeve::sleeve(selected as u32));
    ui.set_elapsed(if selected == 0 { 7.0 } else { 0.0 });
    ui.set_playing(false);
    ui.set_next_title(recordings.get(selected + 1).map_or("", |row| row.1).into());
    ui.set_next_subtitle(ARTIST.into());
    ui.set_queue_rows(model(
        recordings
            .into_iter()
            .enumerate()
            .map(|(i, (id, title))| QueueRow {
                key: (i + 1).to_string().into(),
                track_id: id.into(),
                title: title.into(),
                subtitle: "".into(),
                credits: credit.clone(),
                album_id: "lantern-house-album".into(),
                album_name: ALBUM.into(),
                current: i == selected,
                upcoming: i > selected,
            })
            .collect(),
    ));
}

fn click(ui: &AppWindow, x: f32, y: f32) {
    let window = ui.window();
    let position = LogicalPosition::new(x, y);
    window.dispatch_event(WindowEvent::PointerMoved { position });
    window.dispatch_event(WindowEvent::PointerPressed {
        position,
        button: PointerEventButton::Left,
    });
    window.dispatch_event(WindowEvent::PointerReleased {
        position,
        button: PointerEventButton::Left,
    });
    window.dispatch_event(WindowEvent::PointerMoved {
        position: LogicalPosition::new(5.0, 5.0),
    });
}

fn frame(ui: &AppWindow) -> RgbaImage {
    let pixels = ui
        .window()
        .take_snapshot()
        .expect("native software snapshot");
    assert_eq!((pixels.width(), pixels.height()), (1660, 1180));
    RgbaImage::from_raw(pixels.width(), pixels.height(), pixels.as_bytes().to_vec()).unwrap()
}

fn save_png(output: &Path, png: &RgbaImage) {
    let file = File::create_new(output).expect("new fixture output path");
    png.write_to(&mut BufWriter::new(file), ImageFormat::Png)
        .expect("write fixture PNG");
    println!("PASS 1660×1180 native frame: {}", output.display());
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let output = args
        .next()
        .map(PathBuf::from)
        .ok_or("expected a new PNG path")?;
    let mut overflow = false;
    let mut plain = false;
    let mut checks = false;
    let mut queue_open = false;
    for arg in args {
        match arg.to_str() {
            Some("--overflow") => overflow = true,
            Some("--plain") => plain = true,
            Some("--check") => checks = true,
            Some("--queue-open") => queue_open = true,
            _ => return Err(format!("unknown Now Playing fixture argument: {arg:?}").into()),
        }
    }
    if checks && (plain || queue_open) {
        return Err("--check cannot be combined with --plain or --queue-open".into());
    }
    overflow |= checks;
    let ui = AppWindow::new()?;
    ui.set_inspection_width(1660.0);
    ui.set_inspection_height(1180.0);
    ui.set_signed_in(true);
    ui.set_page_kind("history".into());
    ui.set_can_control(true);
    ui.set_remote_controls(true);
    ui.set_can_seek(true);
    ui.set_output_name("This desktop".into());
    ui.set_now_album_id("lantern-house-album".into());
    ui.set_now_album_name(ALBUM.into());
    ui.set_now_context_kind("playlist".into());
    ui.set_now_context_id("lantern-house-playlist".into());
    ui.set_now_context_name("Night trains".into());
    ui.set_duration(248.0);
    ui.set_local_volume(0.65);
    ui.set_waveform(model(
        (0..240)
            .map(|i| {
                let phase = i as f32 / 239.0;
                let envelope = 0.28 + 0.47 * (phase * 11.0).sin().abs();
                envelope * (0.35 + 0.65 * (i * 13 % 23) as f32 / 23.0)
            })
            .collect(),
    ));
    select(&ui, usize::from(plain));
    delivery(&ui, overflow);

    // Whole seconds are enough for the fixture timeline; exact lyric selection,
    // parsing, and automatic recovery are exercised by synced_lyrics.py.
    ui.on_resolve_active_lyric(|lines, elapsed, _| {
        lines
            .iter()
            .take_while(|line| line.start_ms as f32 <= elapsed * 1000.0)
            .count() as i32
            - 1
    });
    let weak = ui.as_weak();
    ui.on_navigate(move |route: SharedString| {
        let ui = weak.unwrap();
        if route == "back" {
            // Model the real controller publishing Back after the UI callback.
            let weak = ui.as_weak();
            Timer::single_shot(Duration::from_millis(20), move || {
                let ui = weak.unwrap();
                ui.set_can_back(false);
                ui.set_page_kind("history".into());
            });
        } else {
            ui.set_can_back(route == "now");
            ui.set_page_kind(route);
        }
    });
    let weak = ui.as_weak();
    ui.on_toggle_play(move || {
        let ui = weak.unwrap();
        ui.set_playing(!ui.get_playing());
    });
    let weak = ui.as_weak();
    ui.on_seek(move |position| weak.unwrap().set_elapsed(position));
    let weak = ui.as_weak();
    ui.on_next(move || {
        let ui = weak.unwrap();
        match ui.get_now_track_id().as_str() {
            "timed" => select(&ui, 1),
            "plain" => select(&ui, 2),
            _ => (),
        }
    });
    if !checks {
        ui.set_page_kind("now".into());
        ui.set_can_back(true);
    }
    let weak = ui.as_weak();
    Timer::single_shot(Duration::from_millis(300), move || {
        let ui = weak.unwrap();
        let evidence = output.clone();
        let finish = move |png: RgbaImage| {
            save_png(&output, &png);
            slint::quit_event_loop().unwrap();
        };
        if checks {
            now_playing_checks::run(&ui, evidence, finish);
        } else {
            if !queue_open {
                // Expose the full central design at the requested size.
                now_playing_checks::close_queue(&ui);
            }
            finish(frame(&ui));
        }
    });
    ui.run()?;
    Ok(())
}
