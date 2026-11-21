//! Native AppWindow projection fixture. No account, store, network, or playback workers.
use slint::{Model, ModelRc, Timer, VecModel};
use std::{
    cell::{Cell, RefCell},
    fs::{File, OpenOptions},
    io::Write,
    rc::Rc,
    time::Duration,
};

slint::include_modules!();

#[path = "../src/playlist_view.rs"]
mod playlist_view;

const COUNT: usize = 1207;
const DUPLICATES: [usize; 5] = [99, 100, 1099, 1100, COUNT - 1];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Playlists,
    Manual,
    Rules,
    Offline,
    Saved,
    Album,
}

impl Page {
    fn kind(self) -> &'static str {
        match self {
            Self::Playlists => "playlists",
            Self::Manual => "playlist-manual",
            Self::Rules => "playlist-rules",
            Self::Offline => "offline",
            Self::Saved => "saved-collection",
            Self::Album => "album",
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::Playlists => "Playlists",
            Self::Manual => "Synthetic Manual",
            Self::Rules => "Synthetic Rules",
            Self::Offline => "Saved offline",
            Self::Saved => "Synthetic Saved",
            Self::Album => "Synthetic Album",
        }
    }

    fn is_detail(self) -> bool {
        matches!(self, Self::Manual | Self::Rules | Self::Saved)
    }
}

fn model<T: Clone + 'static>(rows: Vec<T>) -> ModelRc<T> {
    ModelRc::new(VecModel::from(rows))
}

fn track(page: Page, index: usize) -> BrowseRow {
    let duplicate = DUPLICATES.contains(&index);
    let playable = duplicate || index % 47 != 3 && index != 98;
    let title = if duplicate {
        "Repeated recording".into()
    } else if !playable && page == Page::Saved {
        "Unavailable song".into()
    } else {
        format!("Recording {:04}", index + 1).into()
    };
    BrowseRow {
        kind: "track".into(),
        occurrence_key: format!("{}:{index}", page.kind()).into(),
        title,
        track_id: if duplicate {
            "shared-recording".into()
        } else {
            format!("track-{index}").into()
        },
        subtitle: match (page, playable) {
            (_, false) => "",
            (Page::Rules, true) => "Rules Artist",
            (Page::Saved, true) => "Saved Artist",
            _ => "Fixture Artist",
        }
        .into(),
        detail: match (page, playable) {
            (Page::Saved, true) => "Synthetic Album",
            (Page::Saved, false) => "Not saved on this device",
            (_, true) => "3:24",
            (_, false) => "Unavailable",
        }
        .into(),
        album_id: if playable && page != Page::Saved {
            "fixture-album"
        } else {
            ""
        }
        .into(),
        album_name: if playable && page != Page::Saved {
            "Synthetic Album"
        } else {
            ""
        }
        .into(),
        playable,
        actions: model(if page == Page::Saved || !playable {
            vec![]
        } else if page == Page::Manual {
            vec!["queue".into(), "add".into(), "remove".into()]
        } else {
            vec!["queue".into(), "add".into()]
        }),
        ..BrowseRow::default()
    }
}

fn root_row(page: Page) -> BrowseRow {
    BrowseRow {
        kind: if page == Page::Saved {
            "saved-collection"
        } else {
            page.kind()
        }
        .into(),
        title: page.title().into(),
        subtitle: format!("{COUNT} songs").into(),
        can_open: true,
        ..BrowseRow::default()
    }
}

fn show(ui: &AppWindow, page: Page, has_parent: bool, generation: usize) {
    // Publish routes and models in the same order as the application.
    ui.set_playlist_order_scope("".into());
    ui.set_manual_offline(matches!(page, Page::Offline | Page::Saved));
    ui.set_page_kind(page.kind().into());
    ui.set_page_key(page.kind().into());
    ui.set_page_title(page.title().into());
    ui.set_page_subtitle(if page.is_detail() {
        format!("{COUNT} songs").into()
    } else {
        "".into()
    });
    ui.set_can_back(has_parent);
    ui.set_page_busy(false);
    ui.set_browse_model_key(generation.to_string().into());
    let rows = match page {
        Page::Playlists => vec![root_row(Page::Manual), root_row(Page::Rules)],
        Page::Offline => vec![root_row(Page::Saved)],
        Page::Album => vec![track(Page::Saved, 0)],
        _ => (0..COUNT).map(|index| track(page, index)).collect(),
    };
    ui.set_playlist_order_rows(model(if page == Page::Manual {
        rows.iter()
            .enumerate()
            .map(|(index, row)| ReorderRow {
                key: format!("entry-{index}").into(),
                title: row.title.clone(),
                subtitle: row.subtitle.clone(),
                reorderable: true,
                ..ReorderRow::default()
            })
            .collect()
    } else {
        vec![]
    }));
    ui.set_browse_rows(model(rows));
    ui.set_playlist_actions(PlaylistActions {
        play: page.is_detail(),
        queue: matches!(page, Page::Manual | Page::Rules),
        rename: page == Page::Manual,
        rules: page == Page::Rules,
        ..PlaylistActions::default()
    });
    ui.set_playlist_order_scope(
        if page == Page::Manual {
            "manual-r1"
        } else {
            ""
        }
        .into(),
    );
}

fn show_manual_loading(ui: &AppWindow, generation: usize) {
    ui.set_playlist_order_scope("".into());
    ui.set_page_kind("playlist".into());
    ui.set_page_key(Page::Manual.kind().into());
    ui.set_page_title("Playlist".into());
    ui.set_page_subtitle("".into());
    ui.set_can_back(true);
    ui.set_page_busy(true);
    ui.set_browse_model_key(generation.to_string().into());
    ui.set_browse_rows(Default::default());
    ui.set_playlist_actions(Default::default());
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path =
        std::env::var_os("SPOFIFY_PLAYLIST_FIXTURE_JOURNAL").expect("disposable journal path");
    let journal = Rc::new(OpenOptions::new().write(true).create_new(true).open(path)?);
    let ui = AppWindow::new()?;
    playlist_view::install(&ui);
    ui.set_inspection_width(1280.0);
    ui.set_inspection_height(685.0);
    ui.set_signed_in(true);
    ui.set_account_label("Disposable UI fixture".into());
    let generation = Rc::new(Cell::new(0));
    show(&ui, Page::Playlists, false, generation.get());
    let stack = Rc::new(RefCell::new(vec![Page::Playlists]));

    let window = ui.as_weak();
    let routes = stack.clone();
    let events = journal.clone();
    let version = generation.clone();
    ui.on_navigate(move |target| {
        let ui = window.unwrap();
        let mut routes = routes.borrow_mut();
        let before = *routes.last().unwrap();
        record(
            &events,
            format_args!("navigate\t{}\t{target}", before.kind()),
        );
        match target.as_str() {
            "back" if routes.len() > 1 => {
                routes.pop();
            }
            "playlists" => *routes = vec![Page::Playlists],
            "offline" => *routes = vec![Page::Offline],
            _ => return,
        }
        let next = *routes.last().unwrap();
        let token = version.get() + 1;
        version.set(token);
        if before == Page::Album && next == Page::Manual {
            // Back can publish an empty loading route before the snapshot.
            // Keep the destination key stable; abandon stale replies.
            show_manual_loading(&ui, token);
            let version = version.clone();
            let window = window.clone();
            Timer::single_shot(Duration::from_millis(900), move || {
                if version.get() == token {
                    version.set(token + 1);
                    show(&window.unwrap(), next, true, version.get());
                }
            });
        } else {
            show(&ui, next, routes.len() > 1, token);
        }
    });
    let window = ui.as_weak();
    let routes = stack.clone();
    let events = journal.clone();
    let version = generation.clone();
    ui.on_row_action(move |op, index| {
        let ui = window.unwrap();
        let mut routes = routes.borrow_mut();
        let page = *routes.last().unwrap();
        let row = usize::try_from(index)
            .ok()
            .and_then(|i| ui.get_browse_rows().row_data(i));
        let id = row
            .as_ref()
            .map_or("!out-of-bounds", |row| row.track_id.as_str());
        record(
            &events,
            format_args!("row\t{}\t{op}\t{index}\t{id}", page.kind()),
        );
        let destination = match (page, op.as_str(), index) {
            (Page::Playlists, "open", 0) => Page::Manual,
            (Page::Playlists, "open", 1) => Page::Rules,
            (Page::Offline, "open", 0) => Page::Saved,
            _ => return,
        };
        routes.push(destination);
        version.set(version.get() + 1);
        show(&ui, destination, true, version.get());
    });
    let window = ui.as_weak();
    let events = journal.clone();
    ui.on_page_action(move |op, id| {
        let mut stack = stack.borrow_mut();
        let page = *stack.last().unwrap();
        record(&events, format_args!("page\t{}\t{op}\t{id}", page.kind()));
        if op == "album" && id == "fixture-album" && matches!(page, Page::Manual | Page::Rules) {
            stack.push(Page::Album);
            generation.set(generation.get() + 1);
            show(&window.unwrap(), Page::Album, true, generation.get());
        }
    });
    ui.on_playlist_order_drop(move |scope, key, after| {
        record(&journal, format_args!("order\t{scope}\t{key}\t{after}"));
    });
    ui.run()?;
    Ok(())
}

fn record(file: &File, event: std::fmt::Arguments<'_>) {
    writeln!(&*file, "{event}").expect("record native callback");
}
