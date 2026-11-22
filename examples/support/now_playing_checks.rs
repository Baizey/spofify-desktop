//! Scale-1 native input/paint checks without Slint MCP or a loopback listener.
use std::{
    cell::RefCell,
    path::{Path, PathBuf},
    time::Duration,
};

use image::{Rgba, RgbaImage};
use slint::platform::{Key, PointerEventButton, WindowEvent};
use slint::{ComponentHandle, LogicalPosition, SharedString, Timer};

use crate::{AppWindow, FIRST, click, delivery, frame, save_png};

const DOCK_TOP: u32 = 1080; // DockLayout's wide, standard-density 100px lane.
const TIMELINE: Rect = [525, 1133, 1300, 1156];
const DOCK_PLAY: (f32, f32) = (912.0, 1110.0);
const FOCUS: Rgba<u8> = Rgba([223, 189, 120, 255]);
const ACCENT: Rgba<u8> = Rgba([102, 227, 180, 255]);
const MUTED: Rgba<u8> = Rgba([180, 189, 203, 255]);
const DIVIDER: Rgba<u8> = Rgba([48, 54, 56, 255]);
const LYRICS: (f32, f32) = (1382.0, 1121.0);
const QUEUE: (f32, f32) = (1421.0, 1121.0);
const QUEUE_CLOSE: (f32, f32) = (1626.0, 29.0);

type Rect = [u32; 4]; // Left/top inclusive, right/bottom exclusive.

pub(super) fn close_queue(ui: &AppWindow) {
    click(ui, QUEUE_CLOSE.0, QUEUE_CLOSE.1);
}

fn event_key(ui: &AppWindow, key: impl Into<slint::SharedString>, pressed: bool) {
    let text = key.into();
    ui.window().dispatch_event(if pressed {
        WindowEvent::KeyPressed { text }
    } else {
        WindowEvent::KeyReleased { text }
    });
}

fn press(ui: &AppWindow, key: impl Into<slint::SharedString>) {
    let key = key.into();
    event_key(ui, key.clone(), true);
    event_key(ui, key, false);
}

fn mask_bounds(image: &RgbaImage, mask: impl Fn(u32, u32, &Rgba<u8>) -> bool) -> Option<Rect> {
    let mut r = [image.width(), image.height(), 0, 0];
    for (x, y, _) in image.enumerate_pixels().filter(|(x, y, p)| mask(*x, *y, p)) {
        r = [r[0].min(x), r[1].min(y), r[2].max(x + 1), r[3].max(y + 1)];
    }
    (r[0] < r[2]).then_some(r)
}

fn diff(a: &RgbaImage, b: &RgbaImage) -> Option<Rect> {
    mask_bounds(a, |x, y, p| b.get_pixel(x, y) != p)
}

fn horizontal_run(
    image: &RgbaImage,
    [left, top, right, bottom]: Rect,
    ink: impl Fn(Rgba<u8>) -> bool,
) -> Rect {
    let mut best = [0; 4];
    for y in top..bottom {
        let mut start = left;
        for x in left..=right {
            if x < right && ink(*image.get_pixel(x, y)) {
                continue;
            }
            if x - start > best[2] - best[0] {
                best = [start, y, x, y + 1];
            }
            start = x + 1;
        }
    }
    best
}

fn reader_ink(image: &RgbaImage) -> Rect {
    // Locate the compact focus outline and the editorial reader's divider.
    let ring = mask_bounds(image, |_, y, p| (120..DOCK_TOP).contains(&y) && *p == FOCUS)
        .expect("Lyrics shortcut did not paint the reader's keyboard focus ring");
    assert!(
        ring[3] - ring[1] < 50,
        "unexpected reader focus ink: {ring:?}"
    );
    let rule = horizontal_run(
        image,
        [0, ring[1].saturating_sub(28), image.width(), ring[1]],
        |p| p == DIVIDER,
    );
    assert!(
        rule[0].abs_diff(ring[0]) < 6 && rule[2] - rule[0] > 400,
        "missing Lyrics divider: {rule:?}, ring {ring:?}"
    );
    [
        rule[0].saturating_sub(1),
        ring[3],
        (rule[2] + 1).min(image.width()),
        DOCK_TOP,
    ]
}

fn only_reader(before: &RgbaImage, after: &RgbaImage, reader: Rect, action: &str) {
    let [l, t, r, b] =
        diff(before, after).unwrap_or_else(|| panic!("{action}: no reader movement"));
    assert!(
        l >= reader[0] && t >= reader[1] && r <= reader[2] && b <= reader[3],
        "{action}: changed pixels {:?} outside lyrics {reader:?}",
        [l, t, r, b]
    );
}

fn ink_points(rect: Rect) -> impl Iterator<Item = (u32, u32)> {
    let [l, t, r, b] = rect;
    (t..b).flat_map(move |y| (l..r).map(move |x| (x, y)))
}

fn has_color(image: &RgbaImage, rect: Rect, color: Rgba<u8>) -> bool {
    ink_points(rect).any(|(x, y)| *image.get_pixel(x, y) == color)
}

fn lines(image: &RgbaImage, [left, top, right, bottom]: Rect) -> Vec<Rect> {
    // Ignore tiny disconnected antialiasing fragments and muted credit/status.
    let mut rows: Vec<Rect> = Vec::new();
    for y in top..bottom {
        let xs: Vec<_> = (left..right)
            .filter(|&x| image.get_pixel(x, y).0[..3].iter().all(|&c| c >= 215))
            .collect();
        if xs.len() < 5 {
            continue;
        }
        let (l, r) = (xs[0], xs[xs.len() - 1] + 1);
        if let Some(last) = rows.last_mut().filter(|row| y <= row[3] + 3) {
            *last = [last[0].min(l), last[1], last[2].max(r), y + 1];
        } else {
            rows.push([l, y, r, y + 1]);
        }
    }
    rows
}

fn authored_timed(image: &RgbaImage, reader: Rect) -> Vec<Rect> {
    let rows = lines(image, reader);
    assert!(
        rows.len() >= 10,
        "missing first two stanzas and overflow lines: {rows:?}"
    );
    let pitch = |i: usize| rows[i + 1][1] - rows[i][1];
    let step = pitch(0);
    assert!(
        step >= 20 && pitch(3).abs_diff(2 * step) <= 2 && pitch(7).abs_diff(2 * step) <= 2,
        "timed authored stanza spacing was lost: {rows:?}"
    );
    assert!(
        [rows[2], rows[7]]
            .iter()
            .all(|row| (680..970).contains(&(row[2] - row[0]))),
        "the two ordinary long authored lines did not render intact"
    );
    assert!(
        pitch(8).abs_diff(step) <= 2
            && rows[8][2] - rows[8][0] > (reader[2] - reader[0]) * 3 / 4
            && rows[9][2] - rows[9][0] < 450,
        "the overflow timed line is missing, clipped too early, or soft-wrapped"
    );
    rows
}

fn dock_timeline(ui: &AppWindow, base: &Path) {
    ui.set_elapsed(60.0);
    let samples = ui.get_waveform();
    ui.set_waveform(Default::default());
    let fallback = frame(ui);
    ui.set_waveform(samples.clone());
    let resting = frame(ui);
    let in_timeline =
        |x, y| (TIMELINE[0]..TIMELINE[2]).contains(&x) && (TIMELINE[1]..TIMELINE[3]).contains(&y);
    assert!(ink_points(TIMELINE).any(|(x, y)| resting.get_pixel(x, y) != fallback.get_pixel(x, y)));
    assert!(
        resting
            .enumerate_pixels()
            .all(|(x, y, p)| in_timeline(x, y) || p == fallback.get_pixel(x, y)),
        "arrival of waveform samples moved or repainted outside the seek rail"
    );
    shot(base, "dock-waveform", &resting);
    for _ in 0..28 {
        press(ui, Key::Tab);
        let focused = frame(ui);
        if !has_color(&focused, TIMELINE, FOCUS) {
            continue;
        }
        assert!(
            ink_points(TIMELINE).all(|(x, y)| {
                (*focused.get_pixel(x, y) == FOCUS) == (*resting.get_pixel(x, y) == ACCENT)
            }),
            "focus must recolor only the active waveform and thumb, without an outline or geometry shift"
        );
        shot(base, "dock-waveform-focus", &focused);
        press(ui, Key::RightArrow);
        assert!(
            (ui.get_elapsed() - 61.0).abs() < 0.01,
            "focused dock Right did not seek"
        );
        ui.set_waveform(Default::default());
        press(ui, Key::RightArrow);
        ui.set_waveform(samples);
        press(ui, Key::RightArrow);
        assert!(
            (ui.get_elapsed() - 63.0).abs() < 0.01,
            "replacing waveform paint lost seek focus"
        );
        click(ui, 925.0, 1144.0);
        assert!(
            (107.0..140.0).contains(&ui.get_elapsed()),
            "dock pointer did not seek near the center"
        );
        println!(
            "PASS dock waveform arrival and Tab change only paint; focus survives; arrow and pointer seek"
        );
        return;
    }
    panic!("Tab did not reach the dock waveform");
}

fn has_status(image: &RgbaImage, reader: Rect) -> bool {
    // At caption size the software rasterizer may not emit an exactly opaque
    // muted-color pixel, especially for a fallback font.
    ink_points(reader)
        .filter(|&(x, y)| {
            let [r, g, b, _] = image.get_pixel(x, y).0;
            (110..=180).contains(&r) && r <= g && g <= b && b - r <= 30
        })
        .take(25)
        .count()
        == 25
}

fn transitions(ui: &AppWindow, base: &Path) {
    ui.invoke_next();
    assert_eq!(ui.get_now_track_id().as_str(), "plain");
    assert_eq!(
        ui.get_lyrics_track_id().as_str(),
        "timed",
        "fixture must leave outgoing lyrics stale until delivery"
    );
    let loading = frame(ui);
    let reader = reader_ink(&loading);
    let unexpected = lines(&loading, reader);
    assert!(
        unexpected.is_empty() && has_status(&loading, reader),
        "loading did not hide the old timed verses and paint status: ink={unexpected:?}, reader={reader:?}"
    );
    delivery(ui, true);
    let plain = frame(ui);
    shot(base, "plain-lyrics", &plain);
    let reader = reader_ink(&plain);
    let rows = lines(&plain, reader);
    let width = reader[2] - reader[0];
    assert_eq!(
        rows.len(),
        5,
        "plain lyrics changed authored line breaks: {rows:?}"
    );
    assert!(
        (reader[0] + 6..reader[0] + 30).contains(&rows[0][0]),
        "track change kept the old horizontal pan: {rows:?}"
    );
    let pitch = rows[1][1] - rows[0][1];
    assert!(
        (rows[2][1] - rows[1][1]).abs_diff(2 * pitch) <= 2
            && (rows[4][1] - rows[3][1]).abs_diff(2 * pitch) <= 2,
        "plain authored stanza gaps disappeared: {rows:?}"
    );
    assert!(
        rows[4][2] - rows[4][0] > width * 3 / 4 && !has_color(&plain, reader, ACCENT),
        "plain long line is clipped too early or gained timed paint"
    );
    press(ui, Key::RightArrow);
    only_reader(&plain, &frame(ui), reader, "plain Right");
    ui.invoke_next();
    assert_eq!(ui.get_now_track_id().as_str(), "missing");
    let next_loading = frame(ui);
    let reader = reader_ink(&next_loading);
    assert!(
        lines(&next_loading, reader).is_empty() && has_status(&next_loading, reader),
        "loading showed stale plain text or lacked status"
    );
    delivery(ui, true);
    let missing = frame(ui);
    assert!(
        lines(&missing, reader).is_empty() && has_status(&missing, reader),
        "No Text did not replace the loading status"
    );
    only_reader(&next_loading, &missing, reader, "loading to No Text");
    println!("PASS timed → loading → plain → loading → No Text, authored plain lines and overflow");
}

fn shot(base: &Path, suffix: &str, picture: &RgbaImage) {
    let mut name = base
        .file_stem()
        .expect("fixture PNG filename")
        .to_os_string();
    name.push(format!("-{suffix}.png"));
    save_png(&base.with_file_name(name), picture);
}

fn thumb(image: &RgbaImage, reader: Rect) -> Rect {
    let [l, _, r, b] = reader;
    let bar = horizontal_run(image, [l, b - 75, r, b - 1], |p| {
        let [r, g, b, _] = p.0;
        (100..145).contains(&r)
            && (5..17).contains(&g.saturating_sub(r))
            && (5..17).contains(&b.saturating_sub(g))
    });
    assert!(
        (40..(r - l) * 3 / 4).contains(&(bar[2] - bar[0])),
        "no visible proportional pan thumb: {bar:?}"
    );
    bar
}

fn overflow_playhead(ui: &AppWindow, base: &Path, reader: Rect, row: Rect) {
    ui.set_elapsed(3.0 * (FIRST.len() + 1) as f32);
    let marker = |frame: &RgbaImage| {
        let region = [reader[0], row[1] - 8, reader[0] + 12, row[3] + 8];
        mask_bounds(frame, |x, y, p| {
            (region[0]..region[2]).contains(&x)
                && (region[1]..region[3]).contains(&y)
                && *p == ACCENT
        })
        .expect("long current line lacks its leading marker")
    };
    let before = frame(ui);
    let anchored = marker(&before);
    let resting_thumb = thumb(&before, reader);
    assert!(!ui.get_playing());
    shot(base, "active-long-before", &before);

    for _ in 0..5 {
        press(ui, Key::RightArrow);
    }
    let panned = frame(ui);
    only_reader(&before, &panned, reader, "active long line keyboard pan");
    assert_eq!(
        marker(&panned),
        anchored,
        "keyboard pan moved the active marker"
    );
    assert!(
        thumb(&panned, reader)[0] > resting_thumb[0] + 20,
        "keyboard pan did not advance the thumb"
    );
    shot(base, "active-long-keyboard", &panned);
    press(ui, Key::Home);

    let start = LogicalPosition::new(
        (resting_thumb[0] + resting_thumb[2]) as f32 / 2.0,
        resting_thumb[1] as f32,
    );
    let target = LogicalPosition::new(start.x + (reader[2] - reader[0]) as f32 * 0.42, start.y);
    let window = ui.window();
    window.dispatch_event(WindowEvent::PointerMoved { position: start });
    window.dispatch_event(WindowEvent::PointerPressed {
        position: start,
        button: PointerEventButton::Left,
    });
    window.dispatch_event(WindowEvent::PointerMoved { position: target });
    window.dispatch_event(WindowEvent::PointerReleased {
        position: target,
        button: PointerEventButton::Left,
    });
    window.dispatch_event(WindowEvent::PointerMoved {
        position: LogicalPosition::new(5.0, 5.0),
    });
    let dragged = frame(ui);
    only_reader(&before, &dragged, reader, "active long line pointer drag");
    assert_eq!(
        marker(&dragged),
        anchored,
        "thumb drag moved the active marker"
    );
    assert!(
        thumb(&dragged, reader)[0] > resting_thumb[0] + 300,
        "thumb drag did not advance the thumb"
    );
    assert_ne!(
        diff(&panned, &dragged),
        None,
        "drag failed to reveal a different part of the long line"
    );
    shot(base, "active-long-drag", &dragged);
    press(ui, Key::Home);
    assert_eq!(
        diff(&before, &frame(ui)),
        None,
        "Home after drag did not restore the active verse"
    );
    println!("PASS proportional thumb follows keyboard/drag; paused long-line marker is fixed");
}

fn reading(ui: &AppWindow, base: &Path) {
    let focused = frame(ui);
    let reader = reader_ink(&focused);
    let rows = authored_timed(&focused, reader);
    assert!(
        has_color(
            &focused,
            [reader[0], rows[1][1] - 3, reader[2], rows[1][3] + 3],
            ACCENT
        ),
        "paused line never acquired current paint"
    );
    let elapsed = ui.get_elapsed();
    press(ui, Key::RightArrow);
    let panned = frame(ui);
    only_reader(&focused, &panned, reader, "timed Right");
    assert!(
        lines(&panned, reader)[0][2] + 12 < rows[0][2],
        "right pan did not translate the first line left"
    );
    press(ui, Key::LeftArrow);
    assert_eq!(
        diff(&focused, &frame(ui)),
        None,
        "Left did not restore the original frame"
    );
    press(ui, Key::PageDown);
    only_reader(&focused, &frame(ui), reader, "PageDown");
    press(ui, Key::Home);
    assert_eq!(
        diff(&focused, &frame(ui)),
        None,
        "Home did not restore the original frame"
    );
    let position = LogicalPosition::new((reader[0] + 80) as f32, (reader[1] + 80) as f32);
    ui.window().dispatch_event(WindowEvent::PointerScrolled {
        position,
        delta_x: 0.0,
        delta_y: -400.0,
    });
    only_reader(&focused, &frame(ui), reader, "reader wheel");
    press(ui, Key::Home);
    press(ui, Key::End);
    let end = frame(ui);
    only_reader(&focused, &end, reader, "End");
    assert!(
        has_color(&end, reader, MUTED),
        "End did not reveal the independent timed attribution"
    );
    press(ui, Key::Home);
    assert_eq!(
        ui.get_elapsed(),
        elapsed,
        "reading arrows or PageDown sought playback"
    );
    assert!(!ui.get_playing(), "reading input toggled paused playback");
    ui.set_elapsed(15.0); // authored blank, between line four and the next stanza
    let blank = frame(ui);
    assert!(
        !has_color(&blank, reader, ACCENT),
        "the active blank acquired a highlight"
    );
    assert_eq!(
        lines(&blank, reader),
        rows,
        "clock update displaced or remounted authored baselines"
    );
    println!(
        "PASS only lyrics pan and scroll; entire dock/deck stable; timed lines don't wrap and active blank stays unpainted"
    );
    overflow_playhead(ui, base, reader, rows[8]);

    event_key(ui, Key::Control, true);
    press(ui, "l");
    event_key(ui, Key::Control, false);
    press(ui, "x");
    assert_eq!(
        ui.get_query().as_str(),
        "x",
        "Ctrl+L did not transfer focus to search"
    );
    ui.set_query("".into());
    assert_eq!(
        ui.get_page_kind().as_str(),
        "now",
        "focusing search opened another page"
    );
}

pub(super) fn run(
    ui: &AppWindow,
    evidence: PathBuf,
    actions: &RefCell<Vec<(SharedString, SharedString)>>,
    finish: impl FnOnce(RgbaImage) + 'static,
) {
    close_queue(ui);
    click(ui, 48.0, 1121.0); // Dock cover: “Open Now Playing” from History.
    assert_eq!(
        ui.get_page_kind().as_str(),
        "now",
        "dock cover did not open Now Playing"
    );
    assert!(ui.get_can_back());
    let opened = frame(ui);
    press(ui, Key::RightArrow);
    assert_eq!(
        diff(&opened, &frame(ui)),
        None,
        "ordinary Now Playing entry stole lyrics arrow focus"
    );
    let elapsed = ui.get_elapsed();
    click(ui, 1460.0, 214.0); // The old in-page waveform lane.
    click(ui, 1426.0, 280.0); // The old in-page Play button.
    assert_eq!(
        ui.get_elapsed(),
        elapsed,
        "obsolete in-page pointer target sought playback"
    );
    assert!(
        !ui.get_playing(),
        "obsolete in-page pointer target toggled playback"
    );
    click(ui, DOCK_PLAY.0, DOCK_PLAY.1);
    assert!(
        ui.get_playing(),
        "dock Play did not toggle after opening Now Playing"
    );
    press(ui, Key::Space);
    assert!(
        !ui.get_playing(),
        "keyboard activation of dock Pause did not toggle playback"
    );
    println!(
        "PASS sole dock transport: former in-page targets inert, pointer Play and keyboard Pause work"
    );
    click(ui, 520.0, 222.0); // Artist credit.
    click(ui, 490.0, 254.0); // Album.
    click(ui, 710.0, 254.0); // Playlist source.
    assert_eq!(
        actions
            .borrow()
            .iter()
            .map(|(kind, id)| (kind.as_str(), id.as_str()))
            .collect::<Vec<_>>(),
        [
            ("artist", "lantern-house-artist"),
            ("album", "lantern-house-album"),
            ("context-playlist", "lantern-house-playlist")
        ],
        "Now Playing identity links did not route to their entities"
    );
    println!("PASS artist, album, and playback-source links dispatch their entity actions");
    click(ui, QUEUE.0, QUEUE.1);
    let queue_open = frame(ui);
    assert!(
        (1300..1640)
            .any(|x| (15..400).any(|y| opened.get_pixel(x, y) != queue_open.get_pixel(x, y))),
        "queue action did not paint its pane"
    );
    shot(&evidence, "queue", &queue_open);
    close_queue(ui);
    click(ui, LYRICS.0, LYRICS.1);
    let weak = ui.as_weak();
    Timer::single_shot(Duration::from_millis(40), move || {
        let ui = weak.unwrap();
        reading(&ui, &evidence);
        dock_timeline(&ui, &evidence);
        click(&ui, LYRICS.0, LYRICS.1);
        let weak = ui.as_weak();
        Timer::single_shot(Duration::from_millis(40), move || {
            let ui = weak.unwrap();
            press(&ui, Key::RightArrow); // Switch tracks with a nonzero horizontal pan.
            transitions(&ui, &evidence);
            press(&ui, Key::Escape);
            let weak = ui.as_weak();
            Timer::single_shot(Duration::from_millis(50), move || {
                let ui = weak.unwrap();
                assert_eq!(
                    ui.get_page_kind().as_str(),
                    "history",
                    "Back did not restore History"
                );
                assert!(
                    !ui.get_can_back(),
                    "History still has a phantom navigation step"
                );
                assert!(
                    has_color(&frame(&ui), [1364, 1100, 1401, 1141], FOCUS),
                    "Back failed to restore keyboard focus to dock Lyrics"
                );
                println!(
                    "PASS dock Open Now Playing, Queue toggle, Lyrics focus, Ctrl+L and Back to History"
                );
                finish(opened);
            });
        });
    });
}
