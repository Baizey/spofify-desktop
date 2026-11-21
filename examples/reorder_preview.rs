//! Disposable native pixel/event fixture. Uses the real AppWindow and its input
//! dispatch on an owned display, but no network, audio, login or user data.
use slint::platform::{Key, PointerEventButton::Left, WindowEvent};
use slint::{ComponentHandle, LogicalPosition, Model, ModelRc, SharedString, Timer};
use std::{cell::RefCell, path::PathBuf, rc::Rc, time::Duration};
slint::include_modules!();

#[path = "../src/playlist_view.rs"]
mod playlist_view;

fn model<T: Clone + 'static>(rows: Vec<T>) -> ModelRc<T> {
    ModelRc::new(slint::VecModel::from(rows))
}

fn song(index: usize) -> SharedString {
    if index == 1 || index == 3 {
        "Repeated song".into()
    } else {
        format!("Track {index:03}").into()
    }
}

fn commit(rows: ModelRc<ReorderRow>, key: &str, after: &str) -> ModelRc<ReorderRow> {
    let mut entries: Vec<_> = rows.iter().collect();
    let from = entries.iter().position(|row| row.key == key).unwrap();
    let entry = entries.remove(from);
    let slot = if after.is_empty() {
        entries.iter().position(|row| row.reorderable).unwrap()
    } else {
        entries.iter().position(|row| row.key == after).unwrap() + 1
    };
    entries.insert(slot, entry);
    let mut upcoming = false;
    for row in &mut entries {
        if row.reorderable && (row.section == "UP NEXT" || !upcoming) {
            if row.key.starts_with("queue-") {
                row.section = if upcoming { "" } else { "UP NEXT" }.into();
            }
            upcoming = true;
        }
    }
    model(entries)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let script = std::fs::read_to_string(args.next().expect("event script path"))?;
    let output = PathBuf::from(args.next().expect("new output folder"));
    std::fs::create_dir(&output)?;
    let line = args.next().as_deref() == Some("line");
    let ui = AppWindow::new()?;
    playlist_view::install(&ui);
    ui.set_inspection_width(1280.0);
    ui.set_inspection_height(720.0);
    ui.set_inspection_reorder_line(line);
    ui.set_signed_in(true);
    ui.set_account_label("Disposable UI fixture".into());
    ui.set_page_kind("playlist-manual".into());
    ui.set_page_key("manual-fixture".into());
    ui.set_page_title("Synthetic Manual Duplicates".into());
    ui.set_playlist_order_scope("playlist-r1".into());
    ui.set_queue_order_token("queue-r1".into());
    ui.set_queue_order_writable(true);
    ui.set_now_title("Current song".into());
    ui.set_now_track_id("current".into());
    ui.set_can_control(true);
    ui.set_remote_controls(true);
    ui.set_browse_rows(model(
        (0..124)
            .map(|i| BrowseRow {
                kind: "track".into(),
                title: song(i),
                subtitle: "Fixture artist".into(),
                playable: true,
                track_id: format!("track-{}", if i == 3 { 1 } else { i }).into(),
                actions: model(vec!["queue".into(), "remove".into()]),
                ..BrowseRow::default()
            })
            .collect(),
    ));
    ui.set_playlist_order_rows(model(
        (0..124)
            .map(|i| ReorderRow {
                key: format!("entry-{i:03}").into(),
                title: song(i),
                subtitle: "Fixture artist".into(),
                reorderable: true,
                section: "".into(),
            })
            .collect(),
    ));
    ui.set_queue_rows(model(
        (0..13)
            .map(|i| QueueRow {
                key: format!("queue-{i:03}").into(),
                title: if i == 0 {
                    "Current song".into()
                } else {
                    song(i)
                },
                subtitle: "Fixture artist".into(),
                track_id: if i == 0 {
                    "current".into()
                } else {
                    format!("track-{}", if i == 3 { 1 } else { i }).into()
                },
                current: i == 0,
                upcoming: i > 0,
                ..QueueRow::default()
            })
            .collect(),
    ));
    ui.set_queue_order_rows(model(
        (0..13)
            .map(|i| ReorderRow {
                key: format!("queue-{i:03}").into(),
                title: if i == 0 {
                    "Current song".into()
                } else {
                    song(i)
                },
                subtitle: "Fixture artist".into(),
                reorderable: i > 0,
                section: match i {
                    0 => "NOW PLAYING",
                    1 => "UP NEXT",
                    _ => "",
                }
                .into(),
            })
            .collect(),
    ));
    let window = ui.as_weak();
    ui.on_queue_order_drop(move |scope, key, after| {
        println!("drop queue {scope} {key} {after:?}");
        window.unwrap().set_queue_order_writable(false);
        let window = window.clone();
        Timer::single_shot(Duration::from_millis(140), move || {
            let ui = window.unwrap();
            let order = commit(ui.get_queue_order_rows(), &key, &after);
            let prior = ui.get_queue_rows();
            ui.set_queue_rows(model(
                order
                    .iter()
                    .map(|entry| prior.iter().find(|r| r.key == entry.key).unwrap())
                    .collect(),
            ));
            ui.set_queue_order_rows(order);
            ui.set_queue_order_token(format!("{scope}+1").into());
            ui.set_queue_order_writable(true);
        });
    });
    let window = ui.as_weak();
    ui.on_playlist_order_drop(move |scope, key, after| {
        println!("drop playlist {scope} {key} {after:?}");
        window.unwrap().set_form_busy(true);
        let window = window.clone();
        Timer::single_shot(Duration::from_millis(140), move || {
            let ui = window.unwrap();
            let order = commit(ui.get_playlist_order_rows(), &key, &after);
            let prior = ui.get_browse_rows();
            let old = ui.get_playlist_order_rows();
            ui.set_browse_rows(model(
                order
                    .iter()
                    .map(|entry| {
                        let index = old.iter().position(|r| r.key == entry.key).unwrap();
                        prior.row_data(index).unwrap()
                    })
                    .collect(),
            ));
            ui.set_playlist_order_rows(order);
            ui.set_playlist_order_scope(format!("{scope}+1").into());
            ui.set_form_busy(false);
        });
    });
    ui.on_row_action(|op, index| println!("catalog {op} {index}"));
    ui.on_queue_action(|op, key| println!("queue {op} {key}"));

    let instructions: Vec<_> = script
        .lines()
        .filter(|line| !line.trim().starts_with('#'))
        .filter_map(|line| {
            let (delay, action) = line.trim().split_once(' ')?;
            let (count, action) = action
                .strip_prefix("repeat ")
                .map_or((1, action), |repeat| {
                    let (count, action) = repeat.split_once(' ').expect("repeat count and event");
                    (count.parse().expect("repeat count"), action)
                });
            Some(std::iter::repeat_n(
                (
                    delay.parse().expect("delay in milliseconds"),
                    action.to_owned(),
                ),
                count,
            ))
        })
        .flatten()
        .collect();
    let mut instructions = instructions.into_iter();
    let mut next = instructions.next();
    let timer = Rc::new(Timer::default());
    let running = timer.clone();
    let window = ui.as_weak();
    let action = RefCell::new(move || {
        let ui = window.unwrap();
        let (_, line) = next.take().unwrap();
        println!("event {line}");
        let mut fields = line.split_whitespace();
        match fields.next().unwrap() {
            verb @ ("down" | "move" | "up" | "scroll") => {
                let x = fields.next().unwrap().parse().unwrap();
                let y = fields.next().unwrap().parse().unwrap();
                let position = LogicalPosition::new(x, y);
                let event = match verb {
                    "down" => WindowEvent::PointerPressed {
                        position,
                        button: Left,
                    },
                    "up" => WindowEvent::PointerReleased {
                        position,
                        button: Left,
                    },
                    "scroll" => WindowEvent::PointerScrolled {
                        position,
                        delta_x: 0.0,
                        delta_y: fields.next().unwrap().parse().unwrap(),
                    },
                    _ => WindowEvent::PointerMoved { position },
                };
                ui.window().dispatch_event(event);
            }
            "key" => {
                let text: SharedString = match fields.next().unwrap() {
                    "up" => Key::UpArrow.into(),
                    "down" => Key::DownArrow.into(),
                    "home" => Key::Home.into(),
                    "end" => Key::End.into(),
                    "tab" => Key::Tab.into(),
                    "enter" => Key::Return.into(),
                    "escape" => Key::Escape.into(),
                    "space" => " ".into(),
                    key => panic!("unknown key {key}"),
                };
                ui.window()
                    .dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
                ui.window()
                    .dispatch_event(WindowEvent::KeyReleased { text });
            }
            "frame" => {
                let buffer = ui.window().take_snapshot().expect("native frame");
                image::save_buffer(
                    output.join(format!("{}.png", fields.next().unwrap())),
                    buffer.as_bytes(),
                    buffer.width(),
                    buffer.height(),
                    image::ColorType::Rgba8,
                )
                .expect("save frame");
            }
            "revise" => ui.set_playlist_order_scope("external-r2".into()),
            "progress" => {
                ui.set_elapsed(10.0);
                ui.set_now_detail("Updated playback facts".into());
            }
            "quit" => {
                slint::quit_event_loop().unwrap();
            }
            verb => panic!("unknown fixture command {verb}"),
        }
        next = instructions.next();
        next.as_ref().map(|(delay, _)| *delay)
    });
    fn schedule(timer: Rc<Timer>, delay: u64, action: Rc<RefCell<dyn FnMut() -> Option<u64>>>) {
        let running = timer.clone();
        timer.start(
            slint::TimerMode::SingleShot,
            Duration::from_millis(delay),
            move || {
                let following = action.borrow_mut()();
                if let Some(delay) = following {
                    schedule(running.clone(), delay, action.clone());
                }
            },
        );
    }
    schedule(running, 250, Rc::new(action));
    ui.run()?;
    Ok(())
}
