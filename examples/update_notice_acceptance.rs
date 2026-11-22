//! Synthetic native presentation check: no account, storage, server, audio or updater.
//!
//! Build with `cargo build --offline --locked --example update_notice_acceptance`.
//! Run the binary with a new private output directory in a private native display
//! (e.g. headless Cage with `SLINT_BACKEND=winit-software`). Set the headless
//! output to 1200x700 using `wlr-randr --output HEADLESS-1 --custom-mode 1200x700@60Hz`
//! before launching the fixture. Repeat with a 480x700 output and `--narrow`
//! after the output path. The fixture emits PNGs and events.log.
//! It uses WindowEvent input against the real AppWindow; only the backend state
//! (notice arrival, playback, sign-in result and offline transition) is synthetic.

use slint::platform::{Key, PointerEventButton, WindowEvent, WindowEventDispatchResult};
use slint::{ComponentHandle, LogicalPosition, Rgba8Pixel, SharedPixelBuffer, SharedString, Timer};
use std::{
    cell::{Cell, RefCell},
    fs::{DirBuilder, File},
    io::Write,
    os::unix::fs::DirBuilderExt,
    path::PathBuf,
    rc::Rc,
    time::Duration,
};

slint::include_modules!();

const NOTICE: &str = "Update ready. Restart Spofify to use it.";
const HEIGHT: f32 = 700.0;
type Frame = SharedPixelBuffer<Rgba8Pixel>;
type Outcome<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn check(ok: bool, text: &str) -> Outcome {
    if ok { Ok(()) } else { Err(text.into()) }
}

fn key(ui: &AppWindow, text: impl Into<SharedString>) {
    let text = text.into();
    ui.window()
        .dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
    ui.window()
        .dispatch_event(WindowEvent::KeyReleased { text });
}

fn mouse(ui: &AppWindow, x: f32, y: f32) -> bool {
    let position = LogicalPosition::new(x, y);
    let button = PointerEventButton::Left;
    [
        WindowEvent::PointerMoved { position },
        WindowEvent::PointerPressed { position, button },
        WindowEvent::PointerReleased { position, button },
    ]
    .map(|event| ui.window().dispatch_event_with_result(event).unwrap())
    .into_iter()
    .all(|result| result == WindowEventDispatchResult::Rejected)
}

fn banner_pixels(frame: &Frame, scale: f32) -> &[u8] {
    // Text and padding above the divider, away from the rest of the window.
    let rows = (26.0 * scale) as usize;
    &frame.as_bytes()[..rows * frame.width() as usize * 4]
}

struct Fixture {
    output: PathBuf,
    log: File,
    width: f32,
    sign_ins: Rc<Cell<u32>>,
    toggles: Rc<Cell<u32>>,
    before_pointer: Option<Frame>,
}

impl Fixture {
    fn record(&mut self, note: &str) -> Outcome {
        println!("{note}");
        writeln!(self.log, "{note}")?;
        Ok(())
    }

    fn frame(&mut self, ui: &AppWindow, name: &str, visible: bool) -> Outcome<Frame> {
        let frame = ui.window().take_snapshot()?;
        let scale = ui.window().scale_factor();
        check(
            frame.width().abs_diff((self.width * scale).round() as u32) <= 1
                && frame.height().abs_diff((HEIGHT * scale).round() as u32) <= 1,
            "unexpected native window dimensions",
        )?;
        image::save_buffer(
            self.output.join(format!("{name}.png")),
            frame.as_bytes(),
            frame.width(),
            frame.height(),
            image::ColorType::Rgba8,
        )?;
        self.record(&format!(
            "frame {name}: {}×{}",
            frame.width(),
            frame.height()
        ))?;
        let stride = frame.width() as usize * 4;
        let across = |color: [u8; 3], y: usize| {
            frame.as_bytes()[y * stride..(y + 1) * stride]
                .chunks_exact(4)
                .all(|p| p[..3] == color)
        };
        let divider = ((25.0 * scale) as usize..(42.0 * scale) as usize)
            .any(|y| across([0x30, 0x36, 0x38], y));
        check(divider == visible, "unexpected full-width banner divider")?;
        if visible {
            check(
                banner_pixels(&frame, scale)
                    .chunks_exact(4)
                    .any(|p| p[0] > 120),
                "banner text did not render",
            )?;
        }
        Ok(frame)
    }

    fn step(&mut self, ui: &AppWindow, stage: u8) -> Outcome<bool> {
        match stage {
            0 => {
                self.frame(ui, "01-sign-in-hidden", false)?;
                key(ui, Key::Tab);
                key(ui, "/");
                check(
                    ui.get_origin().ends_with('/'),
                    "first Tab did not focus server address",
                )?;
                ui.set_update_notice(NOTICE.into());
            }
            1 => {
                let image = self.frame(ui, "02-sign-in-arrival", true)?;
                key(ui, "a");
                check(
                    ui.get_origin().ends_with("/a"),
                    "notice arrival moved sign-in focus",
                )?;
                key(ui, Key::Tab);
                key(ui, "b");
                check(
                    ui.get_username().starts_with('b') || ui.get_username().ends_with('b'),
                    "Tab after notice did not reach username",
                )?;
                for x in [16.0, self.width / 2.0, self.width - 16.0] {
                    check(mouse(ui, x, 15.0), "banner consumed a pointer event")?;
                }
                self.before_pointer = Some(image);
            }
            2 => {
                let after = self.frame(ui, "03-sign-in-after-pointer", true)?;
                check(
                    banner_pixels(&after, ui.window().scale_factor())
                        == banner_pixels(
                            self.before_pointer.as_ref().unwrap(),
                            ui.window().scale_factor(),
                        ),
                    "hover or click changed banner pixels",
                )?;
                check(self.sign_ins.get() == 0, "banner click activated sign-in")?;
                key(ui, "c");
                check(
                    ui.get_username().contains("bc"),
                    "banner click moved keyboard focus",
                )?;
                self.record("PASS notice arrival preserved server focus; Tab reached username; banner pointer events were rejected and did not change pixels or focus")?;
                key(ui, Key::Tab); // Show password
                key(ui, Key::Tab); // Password field
                key(ui, Key::Return);
                check(
                    self.sign_ins.get() == 1 && ui.get_signed_in(),
                    "Return did not invoke synthetic sign-in",
                )?;
            }
            3 => {
                self.frame(ui, "04-playing-notice-and-auth-notice", true)?;
                check(ui.get_playing(), "synthetic playback did not start")?;
                ui.set_auth_notice("".into());
                ui.set_update_notice("".into());
            }
            4 => {
                self.frame(ui, "05-playing-hidden", false)?;
                let rail = if self.width < 820.0 { 164.0 } else { 194.0 };
                mouse(ui, rail + 85.0, 29.0);
                key(ui, "a");
                check(ui.get_query() == "a", "pointer did not focus search")?;
                ui.set_update_notice(NOTICE.into());
            }
            5 => {
                let before = self.frame(ui, "06-playing-arrival", true)?;
                key(ui, "b");
                check(ui.get_query() == "ab", "notice arrival moved search focus")?;
                check(
                    mouse(ui, 24.0, 15.0),
                    "shell banner consumed a pointer event",
                )?;
                self.before_pointer = Some(before);
            }
            6 => {
                let after = self.frame(ui, "07-playing-after-pointer", true)?;
                check(
                    banner_pixels(&after, ui.window().scale_factor())
                        == banner_pixels(
                            self.before_pointer.as_ref().unwrap(),
                            ui.window().scale_factor(),
                        ),
                    "shell banner showed a hover or click response",
                )?;
                key(ui, "c");
                check(
                    ui.get_query() == "abc",
                    "shell banner click moved search focus",
                )?;
                check(
                    ui.get_playing() && self.toggles.get() == 0,
                    "banner interacted with playback",
                )?;
                self.record(
                    "PASS playback and search focus survived notice arrival and pointer probe",
                )?;
                ui.set_form_mode("new-manual".into());
                ui.set_form_name("Synthetic draft".into());
            }
            7 => {
                self.frame(ui, "08-playing-editor", true)?;
                ui.set_update_notice("".into());
            }
            8 => {
                self.frame(ui, "09-playing-editor-hidden", false)?;
                check(
                    ui.get_form_mode() == "new-manual" && ui.get_form_name() == "Synthetic draft",
                    "editor draft changed",
                )?;
                ui.set_form_mode("".into());
                ui.set_update_notice(NOTICE.into());
                ui.set_manual_offline(true);
                ui.set_local_playback(true);
                ui.set_remote_controls(false);
                ui.set_page_kind("offline".into());
            }
            9 => {
                self.frame(ui, "10-offline-playing", true)?;
                self.record("PASS native notice remains visible during offline playback; editor draft survived notice removal")?;
                return Ok(true);
            }
            _ => unreachable!(),
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
    let width = match args.next() {
        None => 1200.0,
        Some(flag) if flag == "--narrow" => 480.0,
        Some(_) => return Err("expected optional --narrow after output directory".into()),
    };
    check(args.next().is_none(), "too many arguments")?;
    DirBuilder::new().mode(0o700).create(&output)?;
    let ui = AppWindow::new()?;
    ui.set_inspection_width(width);
    ui.set_inspection_height(HEIGHT);
    ui.set_reduced_motion(true);
    ui.set_origin("https://fixture.invalid".into());
    ui.set_username("fixture".into());
    ui.set_password("synthetic".into());
    ui.set_auth_notice("Synthetic session notice".into());
    let sign_ins = Rc::new(Cell::new(0));
    let toggles = Rc::new(Cell::new(0));
    let count = sign_ins.clone();
    let weak = ui.as_weak();
    ui.on_sign_in(move || {
        count.set(count.get() + 1);
        let ui = weak.unwrap();
        ui.set_signed_in(true);
        ui.set_account_label("Fixture account".into());
        ui.set_now_title("Synthetic Night Drive".into());
        ui.set_now_detail("Fixture Ensemble".into());
        ui.set_now_track_id("fixture-track".into());
        ui.set_output_name("Fixture output".into());
        ui.set_playing(true);
        ui.set_can_control(true);
        ui.set_can_seek(true);
        ui.set_remote_controls(true);
        ui.set_elapsed(54.0);
        ui.set_duration(189.0);
    });
    let count = toggles.clone();
    ui.on_toggle_play(move || count.set(count.get() + 1));
    let mut fixture = Fixture {
        log: File::create(output.join("events.log"))?,
        output,
        width,
        sign_ins,
        toggles,
        before_pointer: None,
    };
    let timer = Timer::default();
    let weak = ui.as_weak();
    let result = Rc::new(RefCell::new(None));
    let completed = result.clone();
    let mut stage = 0;
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(220),
        move || {
            let ui = weak.unwrap();
            match fixture.step(&ui, stage) {
                Ok(false) => {
                    stage += 1;
                    return;
                }
                Ok(true) => completed.replace(Some(Ok(()))),
                Err(error) => {
                    let _ = fixture.record(&format!("FAIL {error}"));
                    completed.replace(Some(Err(error.to_string())))
                }
            };
            let _ = slint::quit_event_loop();
        },
    );
    ui.run()?;
    timer.stop();
    result
        .take()
        .ok_or("native window closed before scenario finished")?
        .map_err(Into::into)
}
