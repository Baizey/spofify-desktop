//! Private native Search presentation fallback. No TCP or account workers.
//!
//! cargo build --offline --locked --example catalog_continuity_acceptance
//! Run the example with one new output-directory argument inside a private Cage
//! headless Wayland session, with SLINT_BACKEND=winit-software and no MCP port.
//! Add `--deep` after the output directory to exercise 523 rows with faster
//! intermediate appends and native PNGs at the 500/520 request boundaries.
//! Emits native PNGs and events.log. Uses real WindowEvent dispatch, not model
//! writes for user input. The synthetic callbacks stand in for the browser
//! actor; this does not validate HTTP, cancellation, or retry policy.

use slint::platform::{Key, WindowEvent};
use slint::{
    ComponentHandle, LogicalPosition, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, Timer,
    VecModel,
};
use std::cell::{Cell, RefCell};
use std::fs::{self, File};
use std::io::Write;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

slint::include_modules!();

const PAGE: usize = 20;
const TOTAL: usize = 62;
const DEEP_TOTAL: usize = 523;
const TARGET: i32 = PAGE as i32; // First row of the first append.
const SEARCH: &str = "catalog-fixture-search";
const W: f32 = 1200.0;
const H: f32 = 720.0;
type Frame = SharedPixelBuffer<Rgba8Pixel>;
type Outcome<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn check(ok: bool, message: impl Into<String>) -> Outcome {
    if ok {
        Ok(())
    } else {
        Err(message.into().into())
    }
}

fn rows(from: usize, to: usize) -> impl Iterator<Item = BrowseRow> {
    (from..to).map(|row| BrowseRow {
        kind: "artist".into(),
        title: format!("Synthetic Ensemble {:03}", row + 1).into(),
        subtitle: "Fixture artist".into(),
        detail: "No audio".into(),
        can_open: true,
        ..Default::default()
    })
}

#[derive(Clone, Copy, Debug)]
struct Position {
    y: f32,
    anchor: i32,
    offset: f32,
    row: i32,
}

impl Position {
    fn read(ui: &AppWindow) -> Self {
        let y = ui.get_catalog_scroll_y();
        Self {
            y,
            anchor: ui.get_catalog_anchor_row(),
            offset: y + ui.get_catalog_anchor_y(),
            row: ui.get_catalog_focused_row(),
        }
    }

    fn same_anchor(self, other: Self) -> bool {
        self.anchor >= 0
            && self.anchor == other.anchor
            && (self.offset - other.offset).abs() < 0.1
            && (self.y - other.y).abs() < 0.1
    }
}

struct Fixture {
    model: Rc<VecModel<BrowseRow>>,
    total: usize,
    demands: RefCell<Vec<usize>>,
    visit: Cell<Option<Position>>,
    opens: Cell<usize>,
    backs: Cell<usize>,
    fault: RefCell<Option<String>>,
    journal: RefCell<File>,
    started: Instant,
    output: PathBuf,
}

impl Fixture {
    fn log(&self, event: impl std::fmt::Display) {
        println!("{event}");
        if let Err(error) = writeln!(
            self.journal.borrow_mut(),
            "{:06} {event}",
            self.started.elapsed().as_millis()
        ) {
            self.fault.replace(Some(error.to_string()));
        }
    }

    fn records_pixels(&self, offset: usize) -> bool {
        matches!(offset, 20 | 40 | 60 | 500 | 520)
    }

    fn search(&self, ui: &AppWindow, restore: Option<Position>) {
        ui.set_page_kind("search".into());
        ui.set_page_key(SEARCH.into());
        ui.set_page_title("Search".into());
        ui.set_page_subtitle(format!("{} results", self.total).into());
        ui.set_can_back(false);
        ui.set_browse_model_key(SEARCH.into());
        ui.set_browse_rows(self.model.clone().into());
        if let Some(pos) = restore {
            ui.set_catalog_return_key(SEARCH.into());
            ui.set_catalog_return_y(pos.y);
            ui.set_catalog_return_anchor(pos.anchor);
            ui.set_catalog_return_offset(pos.offset);
            ui.set_catalog_return_row(pos.row);
            ui.set_catalog_return_part("".into());
            ui.set_catalog_return_credit("".into());
            ui.set_catalog_return_serial(ui.get_catalog_return_serial() + 1);
        }
    }

    fn bind(self: &Rc<Self>, ui: &AppWindow) {
        let f = self.clone();
        let weak = ui.as_weak();
        ui.on_catalog_demand(move |key| {
            let ui = weak.unwrap();
            let from = f.model.row_count();
            if ui.get_page_kind() != "search"
                || from >= f.total
                || key != format!("search-{from}").as_str()
                || f.demands.borrow().contains(&from)
            {
                f.fault
                    .replace(Some(format!("unexpected catalog demand {key} at {from}")));
                return;
            }
            f.demands.borrow_mut().push(from);
            f.log(format_args!("demand offset={from} key={key}"));
            ui.set_catalog_request_key("".into());
            ui.set_catalog_loading(true);
            let weak = weak.clone();
            let f = f.clone();
            let delay_ms = if f.records_pixels(from) { 750 } else { 130 };
            Timer::single_shot(Duration::from_millis(delay_ms), move || {
                let ui = weak.unwrap();
                let to = (from + PAGE).min(f.total);
                f.model.extend(rows(from, to));
                ui.set_catalog_loading(false);
                ui.set_catalog_request_key(if to < f.total {
                    format!("search-{to}").into()
                } else {
                    "".into()
                });
                f.log(format_args!("append {from}..{to} in place"));
            });
        });
        let f = self.clone();
        let weak = ui.as_weak();
        ui.on_row_action(move |op, index| {
            let ui = weak.unwrap();
            let row_origin = ui.get_catalog_row_navigation();
            f.log(format_args!(
                "row-action {op} index={index} row_origin={row_origin}"
            ));
            if op != "open" || index != TARGET || ui.get_page_kind() != "search" || !row_origin {
                f.fault.replace(Some(format!(
                    "unexpected row action {op} {index}, row_origin={row_origin}"
                )));
                return;
            }
            f.visit.set(Some(Position::read(&ui)));
            f.opens.set(f.opens.get() + 1);
            ui.set_page_kind("artist".into());
            ui.set_page_key("catalog-fixture-artist".into());
            ui.set_page_title(f.model.row_data(index as usize).unwrap().title);
            ui.set_page_subtitle("Synthetic artist detail".into());
            ui.set_browse_model_key("catalog-fixture-artist".into());
            ui.set_browse_rows(ModelRc::default());
            ui.set_can_back(true);
        });
        let f = self.clone();
        let weak = ui.as_weak();
        ui.on_navigate(move |route| {
            let ui = weak.unwrap();
            let row_origin = ui.get_catalog_row_navigation();
            f.log(format_args!("navigate {route} row_origin={row_origin}"));
            if route != "back" || f.visit.get().is_none() || row_origin {
                f.fault.replace(Some(format!(
                    "unexpected navigation {route}, row_origin={row_origin}"
                )));
                return;
            }
            f.backs.set(f.backs.get() + 1);
            f.search(&ui, f.visit.take());
        });
    }

    fn frame(&self, ui: &AppWindow, name: &str) -> Outcome<Frame> {
        let frame = ui.window().take_snapshot()?;
        check(
            frame.width() >= 1000 && frame.height() >= 600,
            "unexpected native snapshot dimensions",
        )?;
        save(&self.output, name, &frame)?;
        self.log(format_args!(
            "frame {name} {}x{}",
            frame.width(),
            frame.height()
        ));
        Ok(frame)
    }
}

fn save(output: &Path, name: &str, frame: &Frame) -> Outcome {
    image::save_buffer(
        output.join(format!("{name}.png")),
        frame.as_bytes(),
        frame.width(),
        frame.height(),
        image::ColorType::Rgba8,
    )?;
    Ok(())
}

fn same_catalog_pixels(a: &Frame, b: &Frame) -> bool {
    if a.width() != b.width() || a.height() != b.height() {
        return false;
    }
    // Compare a broad interior area, excluding toolbar, scrollbar, and the
    // loading caption below the accepted rows. Both buffers are native RGBA.
    let stride = (a.width() * 4) as usize;
    let width = a.width() as usize;
    let left = (width / 5) * 4;
    let right = (width * 2 / 3) * 4;
    (a.height() as usize / 4..a.height() as usize * 3 / 4).all(|y| {
        let s = y * stride;
        a.as_bytes()[s + left..s + right] == b.as_bytes()[s + left..s + right]
    })
}

fn focus_edges(frame: &Frame) -> Vec<usize> {
    // A body focus outline spans the middle of the catalog, unlike lyric,
    // rail, input or text accents. Match the real Theme.tertiary paint color.
    let w = frame.width() as usize;
    let stride = w * 4;
    (frame.height() as usize / 5..frame.height() as usize)
        .filter(|y| {
            let start = y * stride;
            frame.as_bytes()[start + (w / 3) * 4..start + (w * 2 / 3) * 4]
                .chunks_exact(4)
                .filter(|p| p[..3] == [0xdf, 0xbd, 0x78])
                .count()
                > w / 4
        })
        .collect()
}

fn wheel(f: &Fixture, ui: &AppWindow, delta_y: f32) {
    f.log(format_args!("input wheel delta_y={delta_y}"));
    ui.window().dispatch_event(WindowEvent::PointerScrolled {
        position: LogicalPosition::new(W / 2.0, H / 2.0),
        delta_x: 0.0,
        delta_y,
    });
    ui.window().dispatch_event(WindowEvent::PointerMoved {
        position: LogicalPosition::new(4.0, 4.0),
    });
}

fn key(ui: &AppWindow, key: Key) {
    ui.window()
        .dispatch_event(WindowEvent::KeyPressed { text: key.into() });
    ui.window()
        .dispatch_event(WindowEvent::KeyReleased { text: key.into() });
}

#[derive(Debug)]
enum Stage {
    Start,
    Held(usize),
    Appended(usize),
    Exhaustion,
    Tab,
    Activate,
    Artist,
    Back,
    Reopened,
}

struct Journey {
    stage: Stage,
    entered: Instant,
    before: Option<(Position, Option<Frame>)>,
    tabs: usize,
}

impl Journey {
    fn go(&mut self, stage: Stage) {
        self.stage = stage;
        self.entered = Instant::now();
    }

    fn step(&mut self, f: &Fixture, ui: &AppWindow) -> Outcome<bool> {
        if let Some(error) = f.fault.take() {
            return Err(error.into());
        }
        let elapsed = self.entered.elapsed();
        check(
            elapsed < Duration::from_secs(8),
            format!("timed out at {:?}: {:?}", self.stage, Position::read(ui)),
        )?;
        match self.stage {
            Stage::Start if elapsed >= Duration::from_millis(350) => {
                check(
                    f.demands.borrow().is_empty(),
                    "initial viewport loaded more without demand",
                )?;
                f.frame(ui, "01-search-first-page")?;
                wheel(f, ui, -20_000.0);
                self.go(Stage::Held(PAGE));
            }
            Stage::Held(from) if ui.get_catalog_loading() => {
                let p = Position::read(ui);
                check(
                    p.y < -100.0 && p.anchor >= 0,
                    format!("wheel did not reach an anchor: {p:?}"),
                )?;
                check(
                    f.model.row_count() == from,
                    "delayed page arrived before held frame",
                )?;
                let frame = f
                    .records_pixels(from)
                    .then(|| f.frame(ui, &format!("02-search-held-{from:03}")))
                    .transpose()?;
                self.before = Some((p, frame));
                self.go(Stage::Appended(from));
            }
            Stage::Appended(from) if f.model.row_count() > from => {
                let (before, held) = self.before.take().unwrap();
                let after = Position::read(ui);
                check(
                    before.same_anchor(after),
                    format!("append moved native anchor: {before:?} -> {after:?}"),
                )?;
                if let Some(held) = held {
                    let frame = f.frame(ui, &format!("03-search-appended-{from:03}"))?;
                    check(
                        same_catalog_pixels(&held, &frame),
                        "append changed the old visible rows' pixels",
                    )?;
                }
                f.log(format_args!(
                    "PASS append {from} preserved viewport and native anchor {after:?}"
                ));
                wheel(f, ui, -20_000.0);
                self.go(if f.model.row_count() < f.total {
                    Stage::Held(f.model.row_count())
                } else {
                    Stage::Exhaustion
                });
            }
            Stage::Exhaustion if elapsed >= Duration::from_millis(300) => {
                check(
                    f.demands
                        .borrow()
                        .iter()
                        .copied()
                        .eq((PAGE..f.total).step_by(PAGE)),
                    "demand count or offsets changed",
                )?;
                f.frame(ui, "04-search-exhausted")?;
                wheel(f, ui, 100_000.0);
                check(
                    ui.get_catalog_scroll_y().abs() <= 1.0,
                    "wheel did not return to the top for the keyboard journey",
                )?;
                self.go(Stage::Tab);
            }
            Stage::Tab => {
                if ui.get_catalog_focused_row() == TARGET {
                    self.go(Stage::Activate);
                } else {
                    self.tabs += 1;
                    check(self.tabs <= 100, "Tab never reached the first appended row")?;
                    f.log(format_args!("input Tab #{}", self.tabs));
                    key(ui, Key::Tab);
                }
            }
            Stage::Activate => {
                let pos = Position::read(ui);
                let frame = f.frame(ui, "05-keyboard-first-appended-row")?;
                check(
                    !focus_edges(&frame).is_empty(),
                    "focused row has no gold outline in native pixels",
                )?;
                self.before = Some((pos, Some(frame)));
                f.log("input Enter");
                key(ui, Key::Return);
                self.go(Stage::Artist);
            }
            Stage::Artist if f.opens.get() == 1 => {
                check(
                    ui.get_page_kind() == "artist",
                    "Enter did not open artist detail",
                )?;
                f.frame(ui, "06-keyboard-opened-artist")?;
                f.log("input Escape (Back)");
                key(ui, Key::Escape);
                self.go(Stage::Back);
            }
            Stage::Back if elapsed >= Duration::from_millis(140) && f.backs.get() == 1 => {
                let (before, original) = self.before.take().unwrap();
                let original = original.unwrap();
                let pos = Position::read(ui);
                let frame = f.frame(ui, "07-keyboard-back-restored")?;
                check(
                    ui.get_page_kind() == "search" && before.same_anchor(pos) && pos.row == TARGET,
                    format!(
                        "Back did not restore Search anchor and focused row: {before:?} -> {pos:?}"
                    ),
                )?;
                check(
                    focus_edges(&original) == focus_edges(&frame),
                    "Back moved or lost the native focus outline",
                )?;
                check(
                    same_catalog_pixels(&original, &frame),
                    "Back changed the restored visible rows' pixels",
                )?;
                f.log(format_args!(
                    "PASS Back restored focus={TARGET} and native anchor {pos:?}"
                ));
                f.log("input Enter (without retabbing after Back)");
                key(ui, Key::Return);
                self.go(Stage::Reopened);
            }
            Stage::Reopened if f.opens.get() == 2 => {
                f.frame(ui, "08-keyboard-reopened-artist")?;
                f.log(format_args!(
                    "PASS native Search continuation and Back; rows={} demands={} opens=2 backs=1",
                    f.total,
                    f.demands.borrow().len()
                ));
                return Ok(true);
            }
            _ => {}
        }
        Ok(false)
    }
}

fn main() -> Outcome {
    let mut args = std::env::args_os().skip(1);
    let output = PathBuf::from(
        args.next()
            .ok_or("provide one new private output directory")?,
    );
    let total = match args.next() {
        None => TOTAL,
        Some(flag) if flag == "--deep" => DEEP_TOTAL,
        Some(_) => return Err("expected optional --deep after output directory".into()),
    };
    check(args.next().is_none(), "too many arguments")?;
    fs::DirBuilder::new().mode(0o700).create(&output)?;
    let ui = AppWindow::new()?;
    ui.set_inspection_width(W);
    ui.set_inspection_height(H);
    ui.set_reduced_motion(true);
    ui.set_signed_in(true);
    ui.set_account_label("Disposable catalog fixture".into());
    ui.set_query("synthetic ensembles".into());
    let fixture = Rc::new(Fixture {
        model: Rc::new(VecModel::from(rows(0, PAGE).collect::<Vec<_>>())),
        total,
        demands: Default::default(),
        visit: Default::default(),
        opens: Default::default(),
        backs: Default::default(),
        fault: Default::default(),
        started: Instant::now(),
        journal: RefCell::new(File::create(output.join("events.log"))?),
        output,
    });
    fixture.search(&ui, None);
    fixture.bind(&ui);
    ui.set_catalog_request_key(format!("search-{PAGE}").into());

    let timer = Timer::default();
    let weak = ui.as_weak();
    let journey = RefCell::new(Journey {
        stage: Stage::Start,
        entered: Instant::now(),
        before: None,
        tabs: 0,
    });
    let result = Rc::new(RefCell::new(None));
    let completed = result.clone();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(32),
        move || {
            if completed.borrow().is_some() {
                return;
            }
            let ui = weak.unwrap();
            match journey.borrow_mut().step(&fixture, &ui) {
                Ok(false) => return,
                Ok(true) => {
                    completed.replace(Some(Ok(())));
                }
                Err(error) => {
                    fixture.log(format_args!("FAIL {error}"));
                    let _ = fixture.frame(&ui, "failure");
                    completed.replace(Some(Err(error.to_string())));
                }
            }
            let _ = slint::quit_event_loop();
        },
    );
    ui.run()?;
    timer.stop();
    result
        .take()
        .ok_or("native window closed before the scenario finished")?
        .map_err(Into::into)
}
