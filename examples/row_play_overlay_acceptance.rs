//! Disposable native AppWindow fixture. No app workers, account, network, or store.
//!
//! cargo build --offline --locked --example row_play_overlay_acceptance
//! runtime=$(mktemp -d)
//! env -u DISPLAY -u WAYLAND_DISPLAY XDG_RUNTIME_DIR="$runtime" \
//!   WLR_BACKENDS=headless WLR_RENDERER=pixman WLR_HEADLESS_OUTPUTS=1 \
//!   SLINT_BACKEND=winit-software cage -d -- \
//!   target/debug/examples/row_play_overlay_acceptance /tmp/row-play-capture
//! rmdir "$runtime"
//!
//! Supply a new output directory for each run. Frames are native RGBA snapshots
//! of the same 1280×720 layout; use matching PNG names for design comparisons.
//! Add `--capture-only` after the directory to skip the callback/focus journey.

use slint::platform::{Key, PointerEventButton, WindowEvent};
use slint::{
    ComponentHandle, Image, LogicalPosition, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, Timer,
    VecModel,
};
use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    fs::{self, File},
    io::Write,
    os::unix::fs::DirBuilderExt,
    path::PathBuf,
    rc::Rc,
    time::Duration,
};

slint::include_modules!();

#[path = "../src/playlist_view.rs"]
mod playlist_view;

const WIDTH: f32 = 1280.;
const HEIGHT: f32 = 720.;
const PLAYLIST_COUNT: usize = 1_207;
const SEARCH_ARTIST: i32 = 12;
const OUTSIDE: Point = (4., 4.);
// Logical anchors measured in the 1280×720 native frames. Use the body clear
// of metadata for selection and hover; Play and menu explicitly hit their slots.
const SEARCH_BODY: Point = (400., 241.);
const SEARCH_PLAY: Point = (244., 269.);
const SEARCH_FALLBACK: Point = (244., 437.);
const SEARCH_MENU: Point = (928., 269.);
const UPCOMING_HOVER: Point = (1065., 322.);
const QUEUE_BODY: Point = (1120., 303.);
const QUEUE_MENU: Point = (1245., 322.);
const RULES_BODY: Point = (400., 400.);
const RULES_PLAY: Point = (240., 419.);
const ALBUM_PLAY: Point = (240., 460.);
const SAVED_PLAY: Point = (244., 259.);
type Point = (f32, f32);
type Outcome<T = ()> = Result<T, Box<dyn std::error::Error>>;
type Frame = SharedPixelBuffer<Rgba8Pixel>;

fn check(ok: bool, message: impl Into<String>) -> Outcome {
    if ok {
        Ok(())
    } else {
        Err(message.into().into())
    }
}

fn model<T: Clone + 'static>(rows: Vec<T>) -> ModelRc<T> {
    ModelRc::new(VecModel::from(rows))
}

fn ops(names: &[&str]) -> ModelRc<slint::SharedString> {
    model(names.iter().map(|name| (*name).into()).collect())
}

fn credits() -> ModelRc<ArtistCredit> {
    model(vec![ArtistCredit {
        id: "vera".into(),
        name: "Vera North".into(),
    }])
}

/// Make small, colorful record sleeves in memory. No file, font, or service lookup.
fn sleeve(index: u32) -> Image {
    let palettes = [
        ([40, 65, 133], [250, 179, 88]),
        ([27, 98, 82], [214, 153, 183]),
        ([102, 38, 75], [235, 119, 100]),
    ];
    let (base, ink) = palettes[index as usize % palettes.len()];
    let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(128, 128);
    for (i, p) in buffer.make_mut_slice().iter_mut().enumerate() {
        let (x, y) = ((i % 128) as i32, (i / 128) as i32);
        let (dx, dy) = (x - 57, y - 61);
        let r2 = dx * dx + dy * dy;
        let [r, g, b] = if r2 < 1_500 && (r2 / 94) % 3 != 1 || (x + 2 * y) % 43 < 5 {
            ink
        } else {
            base
        };
        *p = Rgba8Pixel { r, g, b, a: 255 };
    }
    Image::from_rgba8(buffer)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Page {
    Search,
    Playlists,
    Rules,
    Album,
    History,
    Saved,
    Artist,
}

impl Page {
    fn kind(self) -> &'static str {
        match self {
            Self::Search => "search",
            Self::Playlists => "playlists",
            Self::Rules => "playlist-rules",
            Self::Album => "album",
            Self::History => "history",
            Self::Saved => "saved-collection",
            Self::Artist => "artist",
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::Search => "Search",
            Self::Playlists => "Playlists",
            Self::Rules => "Late Night Signals",
            Self::Album => "Glass Coast",
            Self::History => "History",
            Self::Saved => "Evening Downloads",
            Self::Artist => "Vera North",
        }
    }

    fn key(self) -> String {
        format!("row-play-fixture-{}", self.kind())
    }

    fn rows(self) -> Vec<BrowseRow> {
        match self {
            Self::Search => (0..48)
                .map(|i| match i {
                    1 => entity("album", "Glass Coast", "Vera North · 2022", "album-glass"),
                    3 | SEARCH_ARTIST => entity("artist", "Vera North", "", "artist-vera"),
                    _ => track(self, i),
                })
                .collect(),
            Self::Playlists => vec![
                BrowseRow {
                    section: "RULE PLAYLISTS".into(),
                    actions: ops(&["rules", "pin-playlist", "delete"]),
                    ..entity(
                        "playlist-rules",
                        "Late Night Signals",
                        "1,207 songs",
                        "rules-late",
                    )
                },
                BrowseRow {
                    section: "MANUAL PLAYLISTS".into(),
                    actions: ops(&["rename", "pin-playlist"]),
                    ..entity(
                        "playlist-manual",
                        "Early Morning Radio",
                        "8 songs",
                        "manual-radio",
                    )
                },
            ],
            Self::Rules => (0..PLAYLIST_COUNT as i32).map(|i| track(self, i)).collect(),
            Self::Album | Self::History | Self::Saved => (0..8).map(|i| track(self, i)).collect(),
            Self::Artist => vec![entity("album", "Glass Coast", "2022", "album-glass")],
        }
    }
}

fn entity(kind: &str, title: &str, subtitle: &str, identity: &str) -> BrowseRow {
    BrowseRow {
        kind: kind.into(),
        title: title.into(),
        subtitle: subtitle.into(),
        occurrence_key: identity.into(),
        can_open: true,
        playable: kind != "artist",
        ..BrowseRow::default()
    }
}

fn track(page: Page, i: i32) -> BrowseRow {
    let playable = i != 4;
    let title = match i {
        0 | 2 => "Night Ferry".into(),
        1 => "Amber Lines".into(),
        4 => "Unlisted Signal".into(),
        _ => format!("Harbor Recording {:04}", i + 1),
    };
    BrowseRow {
        kind: "track".into(),
        title: title.into(),
        track_id: match i {
            0 | 2 => "night-ferry".into(),
            1 => "amber-lines".into(),
            _ => format!("signal-{i}").into(),
        },
        occurrence_key: format!("{}-{i}", page.kind()).into(),
        subtitle: if i == 2 {
            "Vera North".into()
        } else {
            "".into()
        },
        credits: if i == 2 {
            ModelRc::default()
        } else {
            credits()
        },
        album_id: "glass-coast".into(),
        album_name: if page == Page::Saved {
            ""
        } else {
            "Glass Coast"
        }
        .into(),
        detail: match (page, playable) {
            (Page::Saved, false) => "Not saved on this device",
            (_, false) => "Unavailable",
            (Page::History, true) => "Today · 21:43",
            _ => "3:42",
        }
        .into(),
        playable,
        actions: if playable && page != Page::Saved {
            ops(&["queue", "add", "save"])
        } else {
            ModelRc::default()
        },
        ..BrowseRow::default()
    }
}

fn queue() -> ModelRc<QueueRow> {
    let row = |key: &str, title: &str, track: &str, current, upcoming| QueueRow {
        key: key.into(),
        title: title.into(),
        track_id: track.into(),
        credits: credits(),
        album_id: "glass-coast".into(),
        album_name: "Glass Coast".into(),
        current,
        upcoming,
        ..QueueRow::default()
    };
    model(vec![
        row("q-played", "Night Ferry", "night-ferry", false, false),
        row("q-current", "Amber Lines", "amber-lines", true, false),
        row("q-next-1", "Night Ferry", "night-ferry", false, true),
        row("q-next-2", "Night Ferry", "night-ferry", false, true),
    ])
}

fn long_queue() -> ModelRc<QueueRow> {
    let rows = queue();
    let mut upcoming = rows.row_data(2).unwrap();
    upcoming.title = "Night Ferry Across the Open Sea (Extended Mix)".into();
    upcoming.credits = model(vec![
        ArtistCredit {
            id: "vera".into(),
            name: "Vera North".into(),
        },
        ArtistCredit {
            id: "marin".into(),
            name: "Marin James and the Harbourside Ensemble".into(),
        },
    ]);
    upcoming.album_name = "Glass Coast: Expanded Anniversary Edition".into();
    rows.set_row_data(2, upcoming);
    rows
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Receipt {
    Row(String, i32, String),
    Queue(String, String),
    QueueOrder(String, String, String),
    Entity(String, String),
    Navigate(String),
}

#[derive(Clone, Copy, Debug)]
struct CatalogPosition {
    y: f32,
    anchor: i32,
    offset: f32,
    row: i32,
}

impl CatalogPosition {
    fn read(ui: &AppWindow) -> Self {
        let y = ui.get_catalog_scroll_y();
        Self {
            y,
            anchor: ui.get_catalog_anchor_row(),
            offset: y + ui.get_catalog_anchor_y(),
            row: ui.get_catalog_focused_row(),
        }
    }
}

struct Fixture {
    output: PathBuf,
    log: RefCell<File>,
    art: Image,
    receipts: RefCell<Vec<Receipt>>,
    generation: Cell<u32>,
    visit: Cell<Option<CatalogPosition>>,
    follow_artist: Cell<bool>,
}

impl Fixture {
    fn log(&self, line: impl std::fmt::Display) {
        println!("{line}");
        let _ = writeln!(&mut *self.log.borrow_mut(), "{line}");
    }

    fn show(&self, ui: &AppWindow, page: Page) {
        move_to(ui, OUTSIDE);
        ui.set_manual_offline(page == Page::Saved);
        ui.set_page_kind(page.kind().into());
        ui.set_page_key(page.key().into());
        ui.set_page_title(page.title().into());
        ui.set_page_subtitle(
            match page {
                Page::Search => "48 results",
                Page::Album => "2022 · 8 tracks",
                Page::Artist => "",
                _ => "",
            }
            .into(),
        );
        ui.set_page_artwork(
            if matches!(page, Page::Rules | Page::Album | Page::Saved | Page::Artist) {
                sleeve(if page == Page::Rules { 1 } else { 2 })
            } else {
                Image::default()
            },
        );
        ui.set_can_back(page == Page::Artist);
        ui.set_playlist_metadata(
            match page {
                Page::Rules => "Rule playlist · 1,207 songs",
                Page::Saved => "Playlist · 8 songs",
                _ => "",
            }
            .into(),
        );
        ui.set_playlist_actions(PlaylistActions {
            play: matches!(page, Page::Rules | Page::Saved),
            queue: page == Page::Rules,
            rules: page == Page::Rules,
            offline_action: match page {
                Page::Rules => "pin-playlist",
                Page::Saved => "unpin-playlist",
                _ => "",
            }
            .into(),
            ..PlaylistActions::default()
        });
        ui.set_playlist_offline_state(PlaylistOfflineState {
            saved: page == Page::Saved,
            ..Default::default()
        });
        self.bump_model(ui);
        ui.set_browse_artwork(model(match page {
            Page::Search => vec![self.art.clone(), sleeve(1), Image::default(), sleeve(2)],
            Page::Playlists | Page::History => vec![sleeve(1)],
            _ => vec![],
        }));
        ui.set_browse_rows(model(page.rows()));
        self.log(format_args!("page {}", page.kind()));
    }

    fn bump_model(&self, ui: &AppWindow) {
        self.generation.set(self.generation.get() + 1);
        ui.set_browse_model_key(self.generation.get().to_string().into());
    }

    fn replace_first_track(&self, ui: &AppWindow, occurrence: &str) -> Outcome {
        // Update in place: replacing the delegate could independently cancel Play.
        self.bump_model(ui);
        let model = ui.get_browse_rows();
        let mut replacement = model.row_data(0).ok_or("missing first track")?;
        replacement.title = "New Passage".into();
        replacement.occurrence_key = occurrence.into();
        replacement.track_id = occurrence.into();
        model.set_row_data(0, replacement);
        Ok(())
    }

    fn bind(self: &Rc<Self>, ui: &AppWindow) {
        let f = self.clone();
        let weak = ui.as_weak();
        ui.on_row_action(move |op, index| {
            let ui = weak.unwrap();
            let occurrence = (index >= 0)
                .then(|| ui.get_browse_rows().row_data(index as usize))
                .flatten()
                .map_or("<missing>".into(), |row| row.occurrence_key.to_string());
            f.record(Receipt::Row(op.to_string(), index, occurrence));
            if f.follow_artist.get()
                && op == "open"
                && index == SEARCH_ARTIST
                && ui.get_catalog_row_navigation()
                && ui.get_page_kind() == "search"
            {
                f.visit.set(Some(CatalogPosition::read(&ui)));
                f.show(&ui, Page::Artist);
            }
        });
        let f = self.clone();
        ui.on_queue_action(move |op, key| {
            f.record(Receipt::Queue(op.to_string(), key.to_string()))
        });
        let f = self.clone();
        ui.on_queue_order_drop(move |basis, key, after| {
            f.record(Receipt::QueueOrder(
                basis.to_string(),
                key.to_string(),
                after.to_string(),
            ))
        });
        let f = self.clone();
        ui.on_page_action(move |op, id| f.record(Receipt::Entity(op.to_string(), id.to_string())));
        let f = self.clone();
        let weak = ui.as_weak();
        ui.on_navigate(move |route| {
            let ui = weak.unwrap();
            f.record(Receipt::Navigate(route.to_string()));
            if route == "back" && ui.get_page_kind() == "artist" {
                if let Some(pos) = f.visit.get() {
                    f.show(&ui, Page::Search);
                    ui.set_catalog_return_key(Page::Search.key().into());
                    ui.set_catalog_return_y(pos.y);
                    ui.set_catalog_return_anchor(pos.anchor);
                    ui.set_catalog_return_offset(pos.offset);
                    ui.set_catalog_return_row(pos.row);
                    ui.set_catalog_return_part("".into());
                    ui.set_catalog_return_credit("".into());
                    ui.set_catalog_return_serial(ui.get_catalog_return_serial() + 1);
                }
            }
        });
    }

    fn record(&self, receipt: Receipt) {
        self.log(format_args!("callback {receipt:?}"));
        self.receipts.borrow_mut().push(receipt);
    }

    fn expect(&self, label: &str, expected: &[Receipt]) -> Outcome {
        let actual = self.receipts.take();
        check(
            actual == expected,
            format!("expected {expected:?}, got {actual:?}"),
        )?;
        self.log(format_args!("PASS {label}"));
        Ok(())
    }

    fn snapshot(&self, ui: &AppWindow) -> Outcome<Frame> {
        let frame = ui.window().take_snapshot()?;
        check(
            frame.width() == WIDTH as u32 && frame.height() == HEIGHT as u32,
            format!(
                "expected {WIDTH}×{HEIGHT} native pixels, got {}×{}",
                frame.width(),
                frame.height()
            ),
        )?;
        Ok(frame)
    }

    fn frame(&self, ui: &AppWindow, name: &str) -> Outcome {
        let frame = self.snapshot(ui)?;
        image::save_buffer(
            self.output.join(format!("{name}.png")),
            frame.as_bytes(),
            frame.width(),
            frame.height(),
            image::ColorType::Rgba8,
        )?;
        self.log(format_args!("frame {name}"));
        Ok(())
    }
}

fn position((x, y): Point) -> LogicalPosition {
    LogicalPosition::new(x, y)
}
fn move_to(ui: &AppWindow, p: Point) {
    ui.window().dispatch_event(WindowEvent::PointerMoved {
        position: position(p),
    });
}
fn button(ui: &AppWindow, p: Point, down: bool) {
    let position = position(p);
    let button = PointerEventButton::Left;
    ui.window().dispatch_event(if down {
        WindowEvent::PointerPressed { position, button }
    } else {
        WindowEvent::PointerReleased { position, button }
    });
}
fn click(ui: &AppWindow, p: Point) {
    move_to(ui, p);
    button(ui, p, true);
    button(ui, p, false);
}
fn key(ui: &AppWindow, k: Key) {
    ui.window()
        .dispatch_event(WindowEvent::KeyPressed { text: k.into() });
    ui.window()
        .dispatch_event(WindowEvent::KeyReleased { text: k.into() });
}
fn wheel(ui: &AppWindow, p: Point, delta_y: f32) {
    ui.window().dispatch_event(WindowEvent::PointerScrolled {
        position: position(p),
        delta_x: 0.,
        delta_y,
    });
    move_to(ui, OUTSIDE);
}

type Action = Box<dyn FnOnce(&Fixture, &AppWindow) -> Outcome>;
struct Script(VecDeque<(&'static str, Action)>);

impl Script {
    fn new() -> Self {
        Self(VecDeque::new())
    }
    fn step(
        &mut self,
        name: &'static str,
        action: impl FnOnce(&Fixture, &AppWindow) -> Outcome + 'static,
    ) {
        self.0.push_back((name, Box::new(action)));
    }
    fn show(&mut self, page: Page) {
        self.step("show page", move |f, ui| {
            f.show(ui, page);
            Ok(())
        });
    }
    fn shot(&mut self, name: &'static str) {
        self.step(name, move |f, ui| f.frame(ui, name));
    }
    fn hover(&mut self, p: Point) {
        self.step("hover", move |_, ui| {
            move_to(ui, p);
            Ok(())
        });
    }
    fn click(&mut self, p: Point) {
        self.step("click", move |_, ui| {
            click(ui, p);
            Ok(())
        });
    }
    fn key(&mut self, k: Key) {
        self.step("key", move |_, ui| {
            key(ui, k);
            Ok(())
        });
    }
    fn wheel(&mut self, dy: f32) {
        self.step("wheel", move |_, ui| {
            wheel(ui, (710., 465.), dy);
            Ok(())
        });
    }
}

fn captures(script: &mut Script) {
    script.shot("01-search-normal");
    script.hover(SEARCH_BODY);
    script.shot("02-search-art-hover");
    script.hover((400., SEARCH_FALLBACK.1));
    script.shot("03-search-fallback-hover");
    script.hover((400., 353.));
    script.shot("03a-search-album-hover");
    script.hover((400., 513.));
    script.shot("03b-search-artist-hover");
    script.hover(OUTSIDE);
    script.shot("05-queue-current-art-and-played");
    script.hover(UPCOMING_HOVER);
    script.shot("06-queue-upcoming-hover");
    script.hover(OUTSIDE);
    script.step("show current without an active output", |_, ui| {
        ui.set_playing(false);
        Ok(())
    });
    script.shot("07a-queue-current-not-playing");
    script.step("restore active current", |_, ui| {
        ui.set_playing(true);
        Ok(())
    });
    script.step("show current marker", |_, ui| {
        ui.set_cover_art(Image::default());
        Ok(())
    });
    script.shot("08-queue-current-marker");
    script.step("show long upcoming metadata", |f, ui| {
        ui.set_cover_art(f.art.clone());
        ui.set_queue_rows(long_queue());
        Ok(())
    });
    script.shot("08a-queue-long-metadata-rest");
    script.hover(UPCOMING_HOVER);
    script.shot("08b-queue-long-metadata-hover");
    script.step("restore regular metadata", |_, ui| {
        ui.set_queue_rows(queue());
        Ok(())
    });
    script.click(QUEUE_MENU);
    script.shot("07-queue-upcoming-menu");
    script.key(Key::Escape);
    script.hover(OUTSIDE);
    script.shot("07b-queue-menu-escape");
    // The whole face comes before artist and album links, then overflow.
    for _ in 0..3 {
        script.key(Key::Backtab);
    }
    script.shot("07c-queue-keyboard");
    script.click(SEARCH_MENU);
    script.shot("04-search-menu");
    script.key(Key::Escape);
    script.show(Page::Playlists);
    script.shot("09-playlists-rule-and-manual");
    script.hover((410., 225.));
    script.shot("09a-playlists-rule-artwork-hover");
    script.hover((410., 314.));
    script.shot("09b-playlists-manual-fallback-hover");
    script.show(Page::Rules);
    script.shot("10-rules-numbered");
    script.hover(RULES_BODY);
    script.shot("10a-rules-numbered-hover");
    script.wheel(-40_000.);
    script.shot("11-rules-deep-scroll");
    script.show(Page::Album);
    script.shot("12-album-numbered");
    script.hover((400., ALBUM_PLAY.1));
    script.shot("12a-album-numbered-hover");
    script.show(Page::History);
    script.shot("13-history");
    script.hover((400., 308.));
    script.shot("13a-history-current-hover");
    script.show(Page::Saved);
    script.shot("14-saved-offline");
    script.hover((400., SAVED_PLAY.1));
    script.shot("14a-saved-offline-hover");
}

fn row(op: &str, page: Page, index: i32) -> Receipt {
    Receipt::Row(op.into(), index, format!("{}-{index}", page.kind()))
}

fn queue_receipt(op: &str, key: &str) -> Receipt {
    Receipt::Queue(op.into(), key.into())
}

/// A small control's ring has gold pixels across its vertical center.
/// The row outline never intersects this inner check region.
fn play_ring(frame: &Frame, (x, y): Point) -> bool {
    let stride = frame.width() as usize * 4;
    let (x, y) = (x as usize, y as usize);
    let (left, right) = (x - 21, x + 21);
    (y - 12..=y + 12)
        .flat_map(|y| {
            frame.as_bytes()[y * stride + left * 4..y * stride + right * 4].chunks_exact(4)
        })
        .filter(|p| p[..3] == [0xdf, 0xbd, 0x78])
        .count()
        >= 12
}

fn tab_to_play(f: &Fixture, ui: &AppWindow, p: Point) -> Outcome {
    move_to(ui, OUTSIDE);
    for n in 1..=6 {
        key(ui, Key::Tab);
        if play_ring(&f.snapshot(ui)?, p) {
            f.log(format_args!(
                "native Tab #{n} reached leading Play at {p:?}"
            ));
            return Ok(());
        }
    }
    Err(format!("Tab traversal never reached visible focus on Play at {p:?}").into())
}

impl Script {
    fn expect(&mut self, label: &'static str, receipts: Vec<Receipt>) {
        self.step(label, move |f, _| f.expect(label, &receipts));
    }
}

fn queue_body_ring(frame: &Frame) -> bool {
    let stride = frame.width() as usize * 4;
    (285..290)
        .flat_map(|y| {
            frame.as_bytes()[y * stride + 1000 * 4..y * stride + 1240 * 4].chunks_exact(4)
        })
        .filter(|pixel| pixel[..3] == [0xdf, 0xbd, 0x78])
        .count()
        > 200
}

fn queue_upcoming_ink(frame: &Frame) -> Vec<(usize, usize)> {
    let stride = frame.width() as usize * 4;
    (293..313)
        .flat_map(|y| (996..1140).map(move |x| (x, y)))
        .filter(|&(x, y)| {
            frame.as_bytes()[y * stride + x * 4..y * stride + x * 4 + 3] == [0xf4, 0xf6, 0xfa]
        })
        .collect()
}

fn assertions(s: &mut Script) {
    s.expect("capture interactions emit no actions", vec![]);
    s.show(Page::Search);
    s.step(
        "ordinary Queue hover keeps text fixed and clears on exit",
        |f, ui| {
            let normal = f.snapshot(ui)?;
            move_to(ui, UPCOMING_HOVER);
            let hovered = f.snapshot(ui)?;
            move_to(ui, OUTSIDE);
            let after = f.snapshot(ui)?;
            let ink = queue_upcoming_ink(&normal);
            let stride = WIDTH as usize * 4;
            let row = |y| y * stride + 976 * 4..y * stride + 1264 * 4;
            check(
                !ink.is_empty() && ink == queue_upcoming_ink(&hovered),
                "Queue hover moved upcoming title ink",
            )?;
            check(
                (285..358).all(|y| normal.as_bytes()[row(y)] == after.as_bytes()[row(y)])
                    && (285..358).any(|y| normal.as_bytes()[row(y)] != hovered.as_bytes()[row(y)]),
                "Queue hover did not reveal paint and then restore the unchanged row",
            )
        },
    );
    s.click(SEARCH_PLAY);
    s.expect(
        "one Play click selects the artwork occurrence",
        vec![row("play", Page::Search, 0)],
    );
    s.click(SEARCH_FALLBACK);
    s.click(SEARCH_FALLBACK);
    s.expect(
        "Play double-click emits once for the second copy",
        vec![row("play", Page::Search, 2)],
    );

    s.click(SEARCH_BODY);
    s.expect("track body selects without playing", vec![]);
    s.step("body selection is visible", |f, ui| {
        let frame = f.snapshot(ui)?;
        let x = 450;
        let y = 223;
        let offset = (y * frame.width() as usize + x) * 4;
        check(
            frame.as_bytes()[offset..offset + 3] == [0xdf, 0xbd, 0x78],
            "the clicked row has no body-focus outline",
        )
    });
    s.shot("15-search-body-selected");
    s.key(Key::Return);
    s.expect(
        "Enter on track body plays",
        vec![row("play", Page::Search, 0)],
    );
    s.key(Key::Space);
    s.expect(
        "Space on track body plays",
        vec![row("play", Page::Search, 0)],
    );
    s.click(SEARCH_BODY);
    s.click(SEARCH_BODY);
    s.expect(
        "track body double-click plays once",
        vec![row("play", Page::Search, 0)],
    );
    s.step("focus artwork Play by Tab", |f, ui| {
        tab_to_play(f, ui, SEARCH_PLAY)
    });
    s.shot("16-search-keyboard-play");
    s.key(Key::Return);
    s.expect(
        "Enter on independently focused Play",
        vec![row("play", Page::Search, 0)],
    );
    s.key(Key::Space);
    s.expect(
        "Space on independently focused Play",
        vec![row("play", Page::Search, 0)],
    );

    s.click((312., 279.));
    s.expect(
        "artist link avoids row Play",
        vec![Receipt::Entity("artist".into(), "vera".into())],
    );
    s.click((312., 299.));
    s.expect(
        "album link avoids row Play",
        vec![Receipt::Entity("album".into(), "glass-coast".into())],
    );
    s.click(SEARCH_MENU);
    s.key(Key::Return);
    s.expect(
        "overflow menu routes its own operation",
        vec![row("queue", Page::Search, 0)],
    );
    s.click((400., 333.));
    let album = Receipt::Row("open".into(), 1, "album-glass".into());
    s.expect(
        "album row body opens instead of playing",
        vec![album.clone()],
    );
    s.key(Key::Return);
    s.expect("Enter on album body opens", vec![album.clone()]);
    s.key(Key::Space);
    s.expect("Space on album body opens", vec![album]);
    s.click((244., 353.));
    s.expect(
        "album artwork Play is distinct from opening the album",
        vec![Receipt::Row("play".into(), 1, "album-glass".into())],
    );
    s.click((244., 513.));
    s.expect(
        "unplayable artist art still opens the artist",
        vec![Receipt::Row("open".into(), 3, "artist-vera".into())],
    );

    s.click((244., 588.));
    s.key(Key::Return);
    s.key(Key::Space);
    s.expect(
        "unavailable track cannot play by pointer, Enter or Space",
        vec![],
    );
    s.click((986., 218.));
    s.click((1244., 219.));
    s.click((986., 500.));
    s.expect(
        "current and Played queue occurrences have no Play slot",
        vec![],
    );
    s.click((977.5, 302.));
    s.expect("pane inset has no Queue Play strip", vec![]);
    s.click(QUEUE_MENU);
    s.key(Key::Escape);
    s.hover(OUTSIDE);
    s.step("Escape restores Queue overflow keyboard focus", |f, ui| {
        check(
            play_ring(&f.snapshot(ui)?, QUEUE_MENU),
            "queue menu trigger did not regain focus",
        )
    });
    s.key(Key::Return);
    s.key(Key::Return);
    s.expect(
        "menu Play names the first duplicate",
        vec![queue_receipt("play", "q-next-1")],
    );
    s.click((QUEUE_MENU.0, 396.));
    s.key(Key::End);
    s.key(Key::Return);
    s.expect(
        "menu Remove names the second duplicate",
        vec![queue_receipt("remove", "q-next-2")],
    );
    s.click(QUEUE_MENU);
    s.key(Key::Escape);
    for _ in 0..4 {
        s.key(Key::Backtab);
    }
    s.key(Key::Tab);
    s.step(
        "a single Tab from the preceding link focuses the entire upcoming row",
        |f, ui| {
            check(
                queue_body_ring(&f.snapshot(ui)?),
                "unclipped Queue row focus outline missing",
            )
        },
    );
    s.shot("16b-queue-keyboard-play");
    s.key(Key::Space);
    s.key(Key::Return);
    s.expect(
        "Space and Enter play the focused occurrence",
        vec![queue_receipt("play", "q-next-1"); 2],
    );
    s.key(Key::Tab);
    s.key(Key::Return);
    s.expect(
        "Tab moves to artist link, not a duplicate Play stop",
        vec![Receipt::Entity("artist".into(), "vera".into())],
    );
    s.key(Key::Tab);
    s.key(Key::Return);
    s.expect(
        "album link retains its own keyboard action",
        vec![Receipt::Entity("album".into(), "glass-coast".into())],
    );
    s.click((1012., 323.));
    s.expect(
        "pointer opens artist link without Play",
        vec![Receipt::Entity("artist".into(), "vera".into())],
    );
    s.click((1012., 343.));
    s.expect(
        "pointer opens album link without Play",
        vec![Receipt::Entity("album".into(), "glass-coast".into())],
    );
    s.step("show overflowing credit list", |_, ui| {
        ui.set_queue_rows(long_queue());
        Ok(())
    });
    s.click(QUEUE_MENU);
    s.key(Key::Escape);
    s.hover(OUTSIDE);
    s.key(Key::Backtab);
    s.key(Key::Backtab);
    s.shot("16f-queue-overflowed-credit-focus");
    s.key(Key::Return);
    s.expect(
        "keyboard reveals and activates the second artist credit",
        vec![Receipt::Entity("artist".into(), "marin".into())],
    );
    s.step("restore short credits", |_, ui| {
        ui.set_queue_rows(queue());
        Ok(())
    });
    s.click(QUEUE_BODY);
    s.expect("single body click does not play", vec![]);
    s.step("double-click duplicate row body", |_, ui| {
        let second = (QUEUE_BODY.0, 377.);
        click(ui, second);
        click(ui, second);
        Ok(())
    });
    s.expect(
        "double-click plays the second duplicate exactly once",
        vec![queue_receipt("play", "q-next-2")],
    );
    s.step("replace focused Queue key in place", |_, ui| {
        let rows = ui.get_queue_rows();
        let mut next = rows.row_data(3).ok_or("missing focused upcoming row")?;
        next.key = "q-next-replacement".into();
        rows.set_row_data(3, next);
        Ok(())
    });
    s.key(Key::Space);
    s.key(Key::Return);
    s.expect(
        "stale body focus cannot play the replacement occurrence",
        vec![],
    );
    s.click(QUEUE_MENU);
    s.step("remove the occurrence with an open menu", |_, ui| {
        ui.set_queue_rows(model(
            queue().iter().filter(|row| row.key != "q-next-1").collect(),
        ));
        Ok(())
    });
    s.key(Key::Return);
    s.expect(
        "stale popup cannot act after its occurrence was removed",
        vec![],
    );
    s.key(Key::Escape);
    s.click(QUEUE_MENU);
    s.key(Key::Return);
    s.expect(
        "fresh popup targets the remaining duplicate",
        vec![queue_receipt("play", "q-next-2")],
    );
    s.step("restore rows before a playback transition", |_, ui| {
        ui.set_queue_rows(queue());
        Ok(())
    });
    s.click(QUEUE_BODY);
    s.key(Key::Return);
    s.expect(
        "focused occurrence starts playing",
        vec![queue_receipt("play", "q-next-1")],
    );
    s.step(
        "publish that occurrence as current, retaining its duplicate",
        |_, ui| {
            let rows = queue();
            let mut previous = rows.row_data(1).unwrap();
            previous.current = false;
            rows.set_row_data(1, previous);
            let mut current = rows.row_data(2).unwrap();
            current.current = true;
            current.upcoming = false;
            rows.set_row_data(2, current);
            ui.set_now_track_id("night-ferry".into());
            ui.set_cover_art(Image::default());
            ui.set_queue_rows(rows);
            Ok(())
        },
    );
    s.shot("16h-queue-play-transition");
    s.key(Key::Return);
    s.key(Key::Space);
    s.expect(
        "former upcoming focus does not play the new current or remaining duplicate",
        vec![],
    );
    s.step("disable playback in offline view", |f, ui| {
        ui.set_now_track_id("amber-lines".into());
        ui.set_cover_art(f.art.clone());
        ui.set_queue_rows(queue());
        ui.set_manual_offline(true);
        Ok(())
    });
    s.click(QUEUE_BODY);
    s.click(QUEUE_BODY);
    s.key(Key::Space);
    s.click(QUEUE_MENU);
    s.expect(
        "offline without a local queue cannot play or remove",
        vec![],
    );
    s.step("enable idle local Queue while still offline", |_, ui| {
        ui.set_local_playback(true);
        ui.set_playing(false);
        ui.set_now_track_id("".into());
        ui.set_queue_rows(model(queue().iter().filter(|row| row.upcoming).collect()));
        Ok(())
    });
    s.shot("16g-queue-idle-local");
    let idle_menu = (QUEUE_MENU.0, 218.);
    s.click(idle_menu);
    s.key(Key::Return);
    s.expect(
        "idle local Queue retains Play",
        vec![queue_receipt("play", "q-next-1")],
    );
    s.click(idle_menu);
    s.key(Key::End);
    s.key(Key::Return);
    s.expect(
        "idle local Queue retains Remove",
        vec![queue_receipt("remove", "q-next-1")],
    );
    s.step("restore online Queue", |_, ui| {
        ui.set_local_playback(false);
        ui.set_manual_offline(false);
        ui.set_playing(true);
        ui.set_now_track_id("amber-lines".into());
        ui.set_queue_rows(queue());
        Ok(())
    });
    s.step("populate a scrollable Queue", |_, ui| {
        let mut rows: Vec<_> = queue().iter().collect();
        rows.extend((0..30).map(|i| QueueRow {
            key: format!("q-wheel-{i}").into(),
            title: format!("Queue Recording {:02}", i + 1).into(),
            track_id: format!("wheel-track-{i}").into(),
            upcoming: true,
            ..QueueRow::default()
        }));
        ui.set_queue_rows(model(rows));
        Ok(())
    });
    s.click(QUEUE_BODY);
    s.key(Key::Return);
    s.expect(
        "long Queue body owns focus before wheel",
        vec![queue_receipt("play", "q-next-1")],
    );
    s.step("wheel from the leading Queue inset", |f, ui| {
        let before = f.snapshot(ui)?;
        check(
            queue_body_ring(&before),
            "row does not own focus before wheel",
        )?;
        wheel(ui, (978., QUEUE_BODY.1), -40_000.);
        let after = f.snapshot(ui)?;
        check(
            !queue_body_ring(&after) && queue_upcoming_ink(&before) != queue_upcoming_ink(&after),
            "Queue did not release focus and scroll through the leading inset",
        )
    });
    s.key(Key::Return);
    s.key(Key::Space);
    s.expect(
        "wheel and keys do not activate clipped Play in the unchanged Queue",
        vec![],
    );
    s.shot("16c-queue-scrolled-from-pane-inset");
    s.step("restore Queue top", |_, ui| {
        wheel(ui, UPCOMING_HOVER, 40_000.);
        Ok(())
    });
    s.click(QUEUE_BODY);
    s.key(Key::Return);
    s.expect(
        "first upcoming occurrence plays before partial scroll",
        vec![queue_receipt("play", "q-next-1")],
    );
    s.step("partially clip the focused Queue row", |_, ui| {
        wheel(ui, UPCOMING_HOVER, -156.);
        Ok(())
    });
    s.shot("16e-queue-partly-clipped");
    s.key(Key::Return);
    s.key(Key::Space);
    s.expect(
        "partly clipped Queue face does not handle old focus",
        vec![],
    );
    s.step("restore original Queue", |_, ui| {
        wheel(ui, (1065., 322.), 40_000.);
        ui.set_queue_rows(queue());
        Ok(())
    });

    s.step("prepare reorder view", |_, ui| {
        ui.set_queue_order_token("queue-order-1".into());
        ui.set_queue_order_writable(true);
        ui.set_queue_order_rows(model(vec![
            ReorderRow {
                key: "q-current".into(),
                title: "Amber Lines".into(),
                section: "Now playing".into(),
                ..Default::default()
            },
            ReorderRow {
                key: "q-next-1".into(),
                title: "Night Ferry".into(),
                reorderable: true,
                ..Default::default()
            },
            ReorderRow {
                key: "q-next-2".into(),
                title: "Night Ferry".into(),
                reorderable: true,
                ..Default::default()
            },
        ]));
        Ok(())
    });
    s.click((1180., 30.));
    s.key(Key::Space);
    s.key(Key::DownArrow);
    s.key(Key::Space);
    s.expect(
        "reorder sends only the moved occurrence and predecessor",
        vec![Receipt::QueueOrder(
            "queue-order-1".into(),
            "q-next-1".into(),
            "q-next-2".into(),
        )],
    );
    s.shot("16d-queue-reorder");
    s.click((1180., 30.));
    s.step("leave reorder fixture", |_, ui| {
        ui.set_queue_order_token("".into());
        ui.set_queue_order_rows(ModelRc::default());
        Ok(())
    });

    s.step("press Play before model change", |_, ui| {
        move_to(ui, SEARCH_PLAY);
        button(ui, SEARCH_PLAY, true);
        Ok(())
    });
    s.step("change model key while Play is pressed", |f, ui| {
        f.replace_first_track(ui, "replacement-occurrence")
    });
    s.step("release stale Play", |_, ui| {
        button(ui, SEARCH_PLAY, false);
        Ok(())
    });
    s.expect("stale Play release cannot activate replacement", vec![]);
    s.hover(OUTSIDE);
    s.shot("17-search-model-key-replaced");
    s.hover(SEARCH_BODY);
    s.click(SEARCH_PLAY);
    s.expect(
        "replacement responds to a fresh press",
        vec![Receipt::Row(
            "play".into(),
            0,
            "replacement-occurrence".into(),
        )],
    );

    s.click(SEARCH_BODY);
    s.hover(SEARCH_PLAY);
    s.step("press again before a second model change", |_, ui| {
        button(ui, SEARCH_PLAY, true);
        Ok(())
    });
    s.step("publish replacement while this press is held", |f, ui| {
        f.replace_first_track(ui, "immediate-fresh-occurrence")
    });
    s.step(
        "reject stale release then immediately press and release again",
        |_, ui| {
            button(ui, SEARCH_PLAY, false);
            // Keep both releases within Slint's double-click interval.
            click(ui, SEARCH_PLAY);
            Ok(())
        },
    );
    s.expect(
        "a fresh Play after a rejected release is not swallowed as a double",
        vec![Receipt::Row(
            "play".into(),
            0,
            "immediate-fresh-occurrence".into(),
        )],
    );

    s.click(SEARCH_BODY);
    s.hover(SEARCH_PLAY);
    s.step(
        "release Play and update model before the deferred activation",
        |f, ui| {
            click(ui, SEARCH_PLAY);
            f.replace_first_track(ui, "after-deferred-occurrence")
        },
    );
    s.expect("a pending Play never activates a replacement model", vec![]);

    s.show(Page::Playlists);
    s.click((244., 225.));
    s.expect(
        "rule playlist cover plays its exact row",
        vec![Receipt::Row("play".into(), 0, "rules-late".into())],
    );
    s.click((410., 215.));
    s.expect(
        "rule playlist title opens rather than playing",
        vec![Receipt::Row("open".into(), 0, "rules-late".into())],
    );
    s.show(Page::Album);
    s.click(ALBUM_PLAY);
    s.expect(
        "numbered album Play selects duplicate #3, not #1",
        vec![row("play", Page::Album, 2)],
    );
    s.show(Page::Rules);
    s.click(RULES_PLAY);
    s.expect(
        "numbered playlist Play selects duplicate #3, not #1",
        vec![row("play", Page::Rules, 2)],
    );
    s.click(RULES_BODY);
    s.step("focus numbered Play by Tab", |f, ui| {
        tab_to_play(f, ui, RULES_PLAY)
    });
    s.shot("18-rules-keyboard-play-before-scroll");
    s.wheel(-40_000.);
    s.step("evict focused playlist Play", |_, ui| {
        let mounted = ui.get_visible_playlist_indices();
        check(
            mounted.row_count() < 20 && mounted.row_data(0).unwrap_or(0) > 50,
            "wheel did not evict the focused playlist row",
        )
    });
    s.shot("19-rules-focus-evicted-after-scroll");
    s.key(Key::Return);
    s.key(Key::Space);
    s.expect("evicted Play never activates from old focus", vec![]);
    s.key(Key::End);
    s.key(Key::Space);
    s.expect(
        "playlist keyboard focus hands off and reaches absolute last row",
        vec![row("play", Page::Rules, PLAYLIST_COUNT as i32 - 1)],
    );
    s.shot("20-rules-keyboard-last-row");

    s.show(Page::Saved);
    s.click(SAVED_PLAY);
    s.expect(
        "stored recording still plays in offline Saved",
        vec![row("play", Page::Saved, 0)],
    );
    s.click((244., 490.));
    s.click(QUEUE_BODY);
    s.expect("unsaved track and offline queue are gated", vec![]);

    s.show(Page::Search);
    s.click(SEARCH_BODY);
    s.step("focus Search Play before scroll", |f, ui| {
        tab_to_play(f, ui, SEARCH_PLAY)
    });
    // The leading control clips out, but part of the row is still on screen.
    // The overscan delegate must not retain a live keyboard action here.
    s.wheel(-240.);
    s.step(
        "evict catalog Play before crossing the overscan boundary",
        |_, ui| {
            let pos = CatalogPosition::read(ui);
            check(
                (pos.y + 240.).abs() < 1. && pos.row == -1,
                format!("clipped, still-mounted Play kept focus: {pos:?}"),
            )
        },
    );
    s.key(Key::Return);
    s.key(Key::Space);
    s.expect(
        "clipped catalog Play does not handle keys within overscan",
        vec![],
    );
    s.wheel(40_000.);
    s.click(SEARCH_BODY);
    s.step("focus Search Play before deep scroll", |f, ui| {
        tab_to_play(f, ui, SEARCH_PLAY)
    });
    s.wheel(-40_000.);
    s.step("evict catalog Play pinned by the focus anchor", |_, ui| {
        let pos = CatalogPosition::read(ui);
        check(
            pos.y < -1_000. && pos.row == -1,
            format!("offscreen catalog Play kept focus: {pos:?}"),
        )
    });
    s.key(Key::Return);
    s.key(Key::Space);
    s.expect("evicted catalog Play no longer handles keys", vec![]);
    s.shot("20a-search-play-evicted-after-scroll");
    s.wheel(40_000.);
    s.step("Tab to artist below initial viewport", |f, ui| {
        f.follow_artist.set(true);
        for _ in 0..100 {
            key(ui, Key::Tab);
            if ui.get_catalog_focused_row() == SEARCH_ARTIST {
                let pos = CatalogPosition::read(ui);
                return check(
                    pos.y < -100. && pos.anchor >= 0,
                    format!("artist focus must scroll Search: {pos:?}"),
                );
            }
        }
        Err("Tab did not reach the target artist in Search".into())
    });
    s.shot("21-search-keyboard-artist-before-open");
    s.key(Key::Return);
    let artist_open = Receipt::Row("open".into(), SEARCH_ARTIST, "artist-vera".into());
    s.expect(
        "keyboard opens scrolled artist result",
        vec![artist_open.clone()],
    );
    s.shot("22-keyboard-opened-artist");
    s.key(Key::Escape);
    s.expect(
        "Escape navigates Back",
        vec![Receipt::Navigate("back".into())],
    );
    s.shot("23-search-keyboard-back-restored");
    s.step(
        "confirm catalog scroll and focused row restored",
        |f, ui| {
            let before = f.visit.get().ok_or("missing artist visit")?;
            let after = CatalogPosition::read(ui);
            check(
                ui.get_page_kind() == "search"
                    && before.anchor == after.anchor
                    && (before.offset - after.offset).abs() < 1.
                    && (before.y - after.y).abs() < 1.
                    && after.row == SEARCH_ARTIST,
                format!("Back moved Search viewport or focus: {before:?} -> {after:?}"),
            )
        },
    );
    s.key(Key::Return);
    s.expect(
        "restored live artist focus opens without retabbing",
        vec![artist_open],
    );
}

fn main() -> Outcome {
    let mut args = std::env::args_os().skip(1);
    let output = PathBuf::from(args.next().ok_or("provide a new output directory")?);
    let capture_only = match args.next() {
        None => false,
        Some(flag) if flag == "--capture-only" => true,
        Some(_) => {
            return Err("expected optional --capture-only after the output directory".into());
        }
    };
    check(args.next().is_none(), "too many arguments")?;
    fs::DirBuilder::new().mode(0o700).create(&output)?;
    let ui = AppWindow::new()?;
    ui.set_inspection_width(WIDTH);
    ui.set_inspection_height(HEIGHT);
    ui.set_reduced_motion(true);
    ui.set_signed_in(true);
    ui.set_account_label("Fixture listener".into());
    ui.set_query("Vera North".into());
    ui.set_now_track_id("amber-lines".into());
    ui.set_now_title("Amber Lines".into());
    ui.set_now_credits(credits());
    ui.set_now_album_id("glass-coast".into());
    ui.set_now_album_name("Glass Coast".into());
    ui.set_playing(true);
    ui.set_can_control(true);
    ui.set_remote_controls(true);
    ui.set_can_seek(true);
    ui.set_elapsed(121.);
    ui.set_duration(222.);
    ui.set_local_volume(0.68);
    ui.set_output_name("Fixture output".into());
    ui.set_queue_rows(queue());
    let art = sleeve(0);
    ui.set_cover_art(art.clone());
    let fixture = Rc::new(Fixture {
        log: RefCell::new(File::create(output.join("events.log"))?),
        output,
        art,
        receipts: Default::default(),
        generation: Default::default(),
        visit: Default::default(),
        follow_artist: Default::default(),
    });
    playlist_view::install(&ui);
    fixture.bind(&ui);
    fixture.show(&ui, Page::Search);
    let mut script = Script::new();
    captures(&mut script);
    if !capture_only {
        assertions(&mut script);
    }

    let timer = Timer::default();
    let weak = ui.as_weak();
    let complete = Rc::new(RefCell::new(None));
    let result = complete.clone();
    let script = RefCell::new(script);
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(120),
        move || {
            if result.borrow().is_some() {
                return;
            }
            let ui = weak.unwrap();
            let step = script.borrow_mut().0.pop_front();
            match step {
                Some((name, action)) => match action(&fixture, &ui) {
                    Ok(()) => return,
                    Err(error) => {
                        let error = format!("{name}: {error}");
                        fixture.log(format_args!("FAIL {error}"));
                        let _ = fixture.frame(&ui, "failure");
                        result.replace(Some(Err(error)));
                    }
                },
                None => {
                    fixture.log("PASS native row Play fixture");
                    result.replace(Some(Ok(())));
                }
            }
            let _ = slint::quit_event_loop();
        },
    );
    ui.run()?;
    timer.stop();
    complete
        .take()
        .ok_or("native window closed before the fixture finished")?
        .map_err(Into::into)
}
