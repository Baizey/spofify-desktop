//! Device-only views of the ordinary catalog routes. Local items supply track
//! identity, display metadata and playability. Accepted online pages can only
//! establish exact catalog relationships and display labels, never playback.

use std::collections::{HashMap, HashSet};

use crate::{
    api::{Item, catalog::segment_id, library::PlaylistKind},
    pages::{
        Credit, MusicMetadata, Page, Route, Row, RowKey, duration_label, join_labels, ui_text,
    },
};

/// `accepted` contains only current-session accepted online pages (catalog or
/// playlist detail), with preferred pages last (e.g. an extended snapshot).
/// `recent` is the current accepted personal History view, if any. The caller
/// owns session fences and local item verification. The result has no server
/// selections, continuation state, or mutations.
pub(crate) fn project(
    route: &Route,
    verified: &HashMap<String, Item>,
    accepted: &[(&Route, &Page)],
    recent: Option<&Page>,
) -> Option<Page> {
    if !route.catalog_continuity() && *route != Route::History {
        return None;
    }
    let recent = accepted_recent(recent);
    let known = Known::new(verified, accepted, recent);
    let (title, music, rows): (_, _, Vec<Row>) = match route {
        Route::Search(query) => (
            "Search".into(),
            MusicMetadata::default(),
            known.search(verified, query),
        ),
        Route::History => (
            "History".into(),
            MusicMetadata::default(),
            recent
                .into_iter()
                .flat_map(|page| &page.rows)
                .filter_map(|event| {
                    if event.kind != "track" || !matches!(event.key, RowKey::HistoryEvent(_)) {
                        return None;
                    }
                    let item = local_track(verified, &event.id)?;
                    let mut row = known.track_row(item);
                    row.key = event.key.clone();
                    // Recent already projects the date ahead of duration. Retain
                    // the date, not the server track's duration or availability.
                    if let Some(date) = recent_date(&event.detail) {
                        row.detail = join_labels(date, &row.detail);
                    }
                    Some(row)
                })
                .collect(),
        ),
        Route::Album(id) => {
            let album = known.albums.get(id.as_str());
            // An explicit track album ID can differ from the album listing
            // that supplied it. Expose that evidence in this detail view only;
            // don't change Artist cards, membership, or playback ordering.
            let supplemental = known.tracks.iter().filter_map(|(&track_id, links)| {
                let (credit_id, credit) = links.album_credit?;
                (credit_id == id.as_str() && links.title_album() == Some(id.as_str()))
                    .then_some((track_id, credit))
            });
            let title = album
                .and_then(|album| album.display.name())
                .or_else(|| {
                    supplemental
                        .clone()
                        .filter_map(|(_, credit)| {
                            (!credit.name.is_empty() && credit.name != "Album details")
                                .then_some(credit.name.as_str())
                        })
                        .min()
                })
                .unwrap_or("Album");
            let music = album
                .map(|album| known.album_music(album))
                .unwrap_or_default();
            let rows = known.album_detail_tracks(album, verified, supplemental.map(|(id, _)| id));
            (ui_text(title), music, rows)
        }
        Route::Artist(id) => {
            let artist = known.artists.get(id.as_str());
            let title = artist
                .and_then(|artist| artist.display.name())
                .unwrap_or("Artist");
            let rows = artist
                .into_iter()
                .flat_map(|artist| known.artist_albums(artist))
                .filter_map(|id| known.album_card(id))
                .collect();
            (ui_text(title), MusicMetadata::default(), rows)
        }
        _ => unreachable!("only ordinary catalog routes project locally"),
    };
    let local_collection_playable =
        matches!(route, Route::Album(_) | Route::Artist(_)) && !rows.is_empty();
    Some(Page {
        kind: route.kind().into(),
        title,
        total: Some(rows.len() as u64),
        rows,
        music,
        local_audio: true,
        local_collection_playable,
        ..Page::default()
    })
}

/// Artist playback uses the same locally known albums and track ordering as
/// browsing, without reconstructing the local catalog for every album.
pub(crate) fn artist_tracks(
    artist_id: &str,
    verified: &HashMap<String, Item>,
    accepted: &[(&Route, &Page)],
    recent: Option<&Page>,
) -> Vec<Row> {
    let known = Known::new(verified, accepted, accepted_recent(recent));
    known
        .artists
        .get(artist_id)
        .into_iter()
        .flat_map(|artist| known.artist_albums(artist))
        .filter_map(|id| known.albums.get(id))
        .flat_map(|album| known.album_tracks(album, verified))
        .collect()
}

/// Attach exact music links to local playlist or Saved rows. The caller
/// supplies accepted online evidence and current local items, as for `project`.
/// Membership, recording metadata, and playability are left untouched. If live
/// evidence lacks an album link or artist credits, retain that part of the
/// caller's already projected metadata, including valid typed links.
pub(crate) fn attach_track_links(
    rows: &mut [Row],
    verified: &HashMap<String, Item>,
    accepted: &[(&Route, &Page)],
    recent: Option<&Page>,
) {
    let known = Known::new(verified, accepted, accepted_recent(recent));
    for row in rows
        .iter_mut()
        .filter(|row| row.playable && row.kind == "track")
    {
        let Some(item) = local_track(verified, &row.id) else {
            continue;
        };
        let mut music = known.track_music(item);
        if !music.credits.iter().any(|credit| credit.target.is_some()) {
            music.credits = std::mem::take(&mut row.music.credits);
            for credit in &mut music.credits {
                if !matches!(&credit.target, Some(Route::Artist(id)) if segment_id(id)) {
                    credit.target = None;
                }
            }
        }
        if music
            .album
            .as_ref()
            .is_none_or(|credit| credit.target.is_none())
            && let Some(mut existing) = row.music.album.take()
        {
            let ambiguous = known
                .tracks
                .get(item.id.as_str())
                .is_some_and(TrackLinks::ambiguous_title_album);
            if ambiguous || !matches!(&existing.target, Some(Route::Album(id)) if segment_id(id)) {
                existing.target = None;
            }
            match &mut music.album {
                Some(credit) => credit.target = existing.target,
                None => music.album = Some(existing),
            }
        }
        row.music = music;
    }
}

fn accepted_recent(recent: Option<&Page>) -> Option<&Page> {
    recent.filter(|page| page.kind == "history" && !page.local_audio)
}

fn recent_date(detail: &str) -> Option<&str> {
    let date = detail.get(..10)?;
    let rest = detail.get(10..)?;
    let valid = (rest.is_empty() || rest.starts_with(" · "))
        && date.bytes().enumerate().all(|(i, b)| {
            if i == 4 || i == 7 {
                b == b'-'
            } else {
                b.is_ascii_digit()
            }
        });
    valid.then_some(date)
}

fn local_track<'a>(verified: &'a HashMap<String, Item>, id: &str) -> Option<&'a Item> {
    verified
        .get(id)
        .filter(|item| !id.is_empty() && item.id == id && item.kind == "track" && item.has_audio)
}

#[derive(Default)]
struct Display<'a> {
    detail: Option<&'a Page>,
    card: Option<&'a Row>,
    hint: Option<&'a str>,
}

impl<'a> Display<'a> {
    fn name(&self) -> Option<&'a str> {
        [
            self.detail.map(|p| p.title.as_str()),
            self.card.map(|r| r.title.as_str()),
            self.hint,
        ]
        .into_iter()
        .flatten()
        .find(|label| !label.is_empty())
    }

    fn music(&self) -> Option<&'a MusicMetadata> {
        self.detail
            .map(|page| &page.music)
            .or_else(|| self.card.map(|row| &row.music))
    }
}

#[derive(Default)]
struct Album<'a> {
    display: Display<'a>,
    members: HashSet<&'a str>,
    order: Vec<&'a str>,
}

#[derive(Default)]
struct Artist<'a> {
    display: Display<'a>,
    albums: HashSet<&'a str>,
    order: Vec<&'a str>,
    has_track: bool,
}

#[derive(Default)]
struct TrackLinks<'a> {
    detail_album: Option<&'a str>,
    conflicting_detail_album: bool,
    album_credit: Option<(&'a str, &'a Credit)>,
    conflicting_album_credit: bool,
    artists: HashMap<&'a str, &'a Credit>,
    display_credits: Option<&'a [Credit]>,
}

impl<'a> TrackLinks<'a> {
    fn album(&self) -> Option<&'a str> {
        // Keep existing collection membership and Artist playback semantics.
        self.detail_album
            .or_else(|| self.album_credit.map(|(id, _)| id))
    }

    fn ambiguous_title_album(&self) -> bool {
        self.conflicting_album_credit
            || (self.album_credit.is_none() && self.conflicting_detail_album)
    }

    fn title_album(&self) -> Option<&'a str> {
        // Explicit track metadata wins only for the title link. Equally
        // authoritative but incompatible IDs cannot establish a destination.
        if self.ambiguous_title_album() {
            return None;
        }
        self.album_credit.map(|(id, _)| id).or(self.detail_album)
    }
}

#[derive(Default)]
struct Known<'a> {
    albums: HashMap<&'a str, Album<'a>>,
    artists: HashMap<&'a str, Artist<'a>>,
    tracks: HashMap<&'a str, TrackLinks<'a>>,
}

impl<'a> Known<'a> {
    fn new(
        verified: &HashMap<String, Item>,
        accepted: &[(&'a Route, &'a Page)],
        recent: Option<&'a Page>,
    ) -> Self {
        let mut known = Self::default();
        let mut seen = HashSet::new();
        for &(route, page) in accepted.iter().rev() {
            if page.local_audio {
                continue;
            }
            if let Route::Playlist(id) = route {
                let Some(playlist) = page
                    .playlist
                    .as_ref()
                    .filter(|p| !id.is_empty() && p.id == *id)
                else {
                    continue;
                };
                let kind = match playlist.kind {
                    PlaylistKind::Manual => "playlist-manual",
                    PlaylistKind::Derived => "playlist-rules",
                };
                if page.kind != kind || !seen.insert(route) {
                    continue;
                }
                for row in &page.rows {
                    let parent = match (&row.key, playlist.kind) {
                        (RowKey::ManualEntry { playlist_id, .. }, PlaylistKind::Manual)
                        | (RowKey::DerivedOccurrence { playlist_id, .. }, PlaylistKind::Derived) => {
                            playlist_id
                        }
                        _ => continue,
                    };
                    if parent == id {
                        known.add_track(verified, row, None);
                    }
                }
                continue;
            }
            if page.kind != route.kind() || !seen.insert(route) {
                continue;
            }
            match route {
                Route::Album(id) if segment_id(id) => {
                    let album = known.albums.entry(id).or_default();
                    if album.display.detail.is_none() {
                        album.display.detail = Some(page);
                        for row in page
                            .rows
                            .iter()
                            .filter(|row| is_track(row) && local_track(verified, &row.id).is_some())
                        {
                            if album.members.insert(&row.id) {
                                album.order.push(&row.id);
                            }
                        }
                    }
                    for row in page.rows.iter().filter(|row| is_track(row)) {
                        known.add_track(verified, row, Some(id));
                    }
                }
                Route::Artist(id) if segment_id(id) => {
                    let artist = known.artists.entry(id).or_default();
                    artist.display.detail.get_or_insert(page);
                    for row in &page.rows {
                        if let Some(Route::Album(album_id)) = card_route(row) {
                            if artist.albums.insert(album_id) {
                                artist.order.push(album_id);
                            }
                            known
                                .albums
                                .entry(album_id)
                                .or_default()
                                .display
                                .card
                                .get_or_insert(row);
                        }
                    }
                }
                Route::Search(_) => {
                    for row in &page.rows {
                        match card_route(row) {
                            Some(Route::Album(id)) => {
                                known
                                    .albums
                                    .entry(id)
                                    .or_default()
                                    .display
                                    .card
                                    .get_or_insert(row);
                            }
                            Some(Route::Artist(id)) => {
                                known
                                    .artists
                                    .entry(id)
                                    .or_default()
                                    .display
                                    .card
                                    .get_or_insert(row);
                            }
                            _ if is_track(row) => known.add_track(verified, row, None),
                            _ => (),
                        }
                    }
                }
                _ => (),
            }
        }
        if let Some(page) = recent {
            for row in page
                .rows
                .iter()
                .filter(|row| matches!(row.key, RowKey::HistoryEvent(_)))
            {
                known.add_track(verified, row, None);
            }
        }
        for (&track_id, links) in &known.tracks {
            let album = links.album();
            if let Some(id) = album {
                let release = known.albums.entry(id).or_default();
                release.members.insert(track_id);
                if let Some((_, credit)) =
                    links.album_credit.filter(|(credit_id, _)| *credit_id == id)
                {
                    set_hint(&mut release.display, &credit.name, "Album details");
                }
            }
            for (&id, credit) in &links.artists {
                let artist = known.artists.entry(id).or_default();
                artist.has_track = true;
                set_hint(&mut artist.display, &credit.name, "Unknown artist");
                if let Some(album) = album {
                    artist.albums.insert(album);
                }
            }
        }
        for (&album_id, album) in &known.albums {
            if album.members.is_empty() {
                continue;
            }
            for (artist_id, credit) in album.display.music().into_iter().flat_map(artist_credits) {
                let artist = known.artists.entry(artist_id).or_default();
                artist.albums.insert(album_id);
                set_hint(&mut artist.display, &credit.name, "Unknown artist");
            }
        }
        known
    }

    fn add_track(
        &mut self,
        verified: &HashMap<String, Item>,
        row: &'a Row,
        album: Option<&'a str>,
    ) {
        if row.kind != "track" || local_track(verified, &row.id).is_none() {
            return;
        }
        let links = self.tracks.entry(&row.id).or_default();
        if let Some(album) = album {
            if links.detail_album.is_some_and(|known| known != album) {
                links.conflicting_detail_album = true;
            } else {
                links.detail_album = Some(album);
            }
        }
        if let Some((id, credit)) =
            row.music
                .album
                .as_ref()
                .and_then(|credit| match &credit.target {
                    Some(Route::Album(id)) if segment_id(id) => Some((id.as_str(), credit)),
                    _ => None,
                })
        {
            if let Some((accepted, _)) = links.album_credit {
                links.conflicting_album_credit |= accepted != id;
            } else {
                links.album_credit = Some((id, credit));
            }
        }
        for (id, credit) in artist_credits(&row.music) {
            links.artists.entry(id).or_insert(credit);
        }
        // A v1 item stores flattened display text, not credit identities.
        // Explicit catalog credits supplement it by exact track ID, never name.
        if links.display_credits.is_none() && artist_credits(&row.music).next().is_some() {
            links.display_credits = Some(&row.music.credits);
        }
    }

    fn track_music(&self, item: &Item) -> MusicMetadata {
        let links = self.tracks.get(item.id.as_str());
        let album = links.and_then(TrackLinks::title_album);
        let name = ui_text(&item.album_name);
        let name = if name.is_empty() {
            album
                .and_then(|id| {
                    self.albums
                        .get(id)
                        .and_then(|album| album.display.name())
                        .or_else(|| {
                            links?
                                .album_credit
                                .filter(|(credit_id, _)| *credit_id == id)
                                .map(|(_, credit)| credit.name.as_str())
                        })
                })
                .map(ui_text)
                .unwrap_or_default()
        } else {
            name
        };
        let album = (album.is_some() || !name.is_empty()).then(|| Credit {
            name,
            target: album.map(|id| Route::Album(id.into())),
        });
        let credits = links
            .and_then(|links| links.display_credits)
            .map(|credits| self.local_credits(credits))
            .unwrap_or_default();
        MusicMetadata { credits, album }
    }

    fn track_row(&self, item: &Item) -> Row {
        Row {
            kind: "track".into(),
            title: ui_text(&item.name),
            subtitle: ui_text(&item.artists),
            detail: duration_label(item.duration_ms),
            playable: true,
            id: item.id.clone(),
            key: RowKey::Track(item.id.clone()),
            item: Some(item.clone()),
            music: self.track_music(item),
            ..Row::default()
        }
    }

    fn local_credits(&self, credits: &[Credit]) -> Vec<Credit> {
        credits
            .iter()
            .cloned()
            .map(|mut credit| {
                if !matches!(&credit.target, Some(Route::Artist(id)) if self.artist_visible(id)) {
                    credit.target = None;
                }
                credit
            })
            .collect()
    }

    fn album_music(&self, album: &Album<'_>) -> MusicMetadata {
        let credits = album
            .display
            .music()
            .map(|music| self.local_credits(&music.credits))
            .unwrap_or_default();
        MusicMetadata {
            credits,
            album: None,
        }
    }

    fn album_tracks(&self, album: &Album<'_>, verified: &HashMap<String, Item>) -> Vec<Row> {
        self.album_detail_tracks(Some(album), verified, std::iter::empty())
    }

    fn album_detail_tracks(
        &self,
        album: Option<&Album<'a>>,
        verified: &HashMap<String, Item>,
        supplemental: impl Iterator<Item = &'a str>,
    ) -> Vec<Row> {
        let order = album.map_or(&[][..], |album| album.order.as_slice());
        let mut seen: HashSet<_> = order.iter().copied().collect();
        let mut tail: Vec<_> = album
            .into_iter()
            .flat_map(|album| album.members.iter().copied())
            .chain(supplemental)
            .filter(|id| seen.insert(*id))
            .filter_map(|id| local_track(verified, id))
            .collect();
        // Keep the accepted detail order. For other exact members use genuine
        // numbers, without implying a release position for unnumbered tracks.
        tail.sort_unstable_by_key(|item| {
            (
                item.track_number == 0,
                item.disc_number,
                item.track_number,
                item.id.as_str(),
            )
        });
        order
            .iter()
            .filter_map(|id| local_track(verified, id))
            .chain(tail)
            .map(|item| self.track_row(item))
            .collect()
    }

    fn artist_albums(&self, artist: &Artist<'a>) -> Vec<&'a str> {
        let visible = |id: &str| self.playable_album(id).is_some();
        let ordered: HashSet<_> = artist.order.iter().copied().collect();
        let mut tail: Vec<_> = artist
            .albums
            .difference(&ordered)
            .copied()
            .filter(|id| visible(id))
            .collect();
        tail.sort_by_cached_key(|id| {
            (
                self.albums[id]
                    .display
                    .name()
                    .unwrap_or_default()
                    .to_lowercase(),
                *id,
            )
        });
        artist
            .order
            .iter()
            .copied()
            .filter(|id| visible(id))
            .chain(tail)
            .collect()
    }

    // Artist playback uses the same visible albums as the ordinary detail.
    fn playable_album(&self, id: &str) -> Option<&Album<'a>> {
        self.albums
            .get(id)
            .filter(|album| !album.members.is_empty() && album.display.name().is_some())
    }

    fn album_card(&self, id: &str) -> Option<Row> {
        let album = self.playable_album(id)?;
        let music = self.album_music(album);
        let subtitle = album
            .display
            .card
            .map(|row| row.subtitle.clone())
            .unwrap_or_else(|| credit_line(&music.credits));
        Some(Row {
            kind: "album".into(),
            title: ui_text(album.display.name()?),
            subtitle,
            id: id.into(),
            music,
            playable: true,
            key: RowKey::Album(id.into()),
            route: Some(Route::Album(id.into())),
            ..Row::default()
        })
    }

    fn artist_visible(&self, id: &str) -> bool {
        self.artists.get(id).is_some_and(|artist| {
            artist.has_track
                || artist.albums.iter().any(|id| {
                    self.albums
                        .get(id)
                        .is_some_and(|album| !album.members.is_empty())
                })
        })
    }

    fn artist_card(&self, id: &str) -> Option<Row> {
        if !self.artist_visible(id) {
            return None;
        }
        let artist = self.artists.get(id)?;
        Some(Row {
            kind: "artist".into(),
            title: ui_text(artist.display.name()?),
            subtitle: artist
                .display
                .card
                .map(|row| row.subtitle.clone())
                .unwrap_or_default(),
            id: id.into(),
            playable: artist
                .albums
                .iter()
                .any(|id| self.playable_album(id).is_some()),
            key: RowKey::Artist(id.into()),
            route: Some(Route::Artist(id.into())),
            ..Row::default()
        })
    }

    fn search(&self, verified: &HashMap<String, Item>, query: &str) -> Vec<Row> {
        // Match the ordinary search input bound; never reinterpret a partial query.
        if query.chars().count() > 200
            || query.chars().any(char::is_control)
            || query.trim().is_empty()
        {
            return Vec::new();
        }
        let query = query.to_lowercase();
        let matches = |fields: [&str; 3]| {
            let text = fields.join(" ").to_lowercase();
            query.split_whitespace().all(|term| text.contains(term))
        };
        let mut rows: Vec<_> = verified
            .keys()
            .filter_map(|id| local_track(verified, id))
            .filter(|item| matches([&item.name, &item.artists, &item.album_name]))
            .map(|item| self.track_row(item))
            .collect();
        rows.extend(
            self.albums
                .keys()
                .filter_map(|id| self.album_card(id))
                .chain(self.artists.keys().filter_map(|id| self.artist_card(id)))
                .filter(|row| matches([&row.title, &row.subtitle, ""])),
        );
        sort_rows(&mut rows);
        rows
    }
}

fn is_track(row: &Row) -> bool {
    row.kind == "track" && matches!(&row.key, RowKey::Track(id) if *id == row.id)
}

fn card_route(row: &Row) -> Option<&Route> {
    match (row.route.as_ref()?, &row.key, row.kind.as_str()) {
        (route @ Route::Album(id), RowKey::Album(key), "album")
        | (route @ Route::Artist(id), RowKey::Artist(key), "artist")
            if id == key && *id == row.id && segment_id(id) =>
        {
            Some(route)
        }
        _ => None,
    }
}

fn artist_credits(music: &MusicMetadata) -> impl Iterator<Item = (&str, &Credit)> {
    music
        .credits
        .iter()
        .filter_map(|credit| match &credit.target {
            Some(Route::Artist(id)) if segment_id(id) => Some((id.as_str(), credit)),
            _ => None,
        })
}

fn credit_line(credits: &[Credit]) -> String {
    ui_text(
        &credits
            .iter()
            .take(8)
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>()
            .join(", "),
    )
}

fn set_hint<'a>(display: &mut Display<'a>, name: &'a str, placeholder: &str) {
    if !name.is_empty() && name != placeholder && display.hint.is_none_or(|current| name < current)
    {
        display.hint = Some(name);
    }
}

fn sort_rows(rows: &mut [Row]) {
    rows.sort_by_cached_key(|row| (row.title.to_lowercase(), row.kind.clone(), row.id.clone()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pages::{PlaybackSelection, TrackSelection};

    fn recording(id: &str, number: u32) -> Item {
        Item {
            id: id.into(),
            name: format!("Recording {id}"),
            kind: "track".into(),
            artists: "Fixture Duo".into(),
            album_name: "Fixture Release".into(),
            duration_ms: 64_000,
            has_audio: true,
            track_number: number,
            disc_number: 1,
        }
    }

    fn verified(items: impl IntoIterator<Item = Item>) -> HashMap<String, Item> {
        items
            .into_iter()
            .map(|item| (item.id.clone(), item))
            .collect()
    }

    fn known_track(id: &str, album: Option<&str>, artist: Option<&str>) -> Row {
        Row {
            kind: "track".into(),
            id: id.into(),
            title: "Different server title".into(),
            subtitle: "Different server credit".into(),
            detail: "5:12".into(),
            key: RowKey::Track(id.into()),
            music: MusicMetadata {
                album: album.map(|id| Credit {
                    target: Some(Route::Album(id.into())),
                    name: "Fixture Release".into(),
                }),
                credits: artist
                    .into_iter()
                    .map(|id| Credit {
                        target: Some(Route::Artist(id.into())),
                        name: "Fixture Duo".into(),
                    })
                    .collect(),
            },
            position: Some(42),
            playback: Some(selection(id)),
            ..Row::default()
        }
    }

    fn selection(id: &str) -> PlaybackSelection {
        PlaybackSelection::Track {
            track: {
                let mut track = TrackSelection::default();
                track.id = id.into();
                track
            },
            context: None,
        }
    }

    fn card(route: Route, name: &str) -> Row {
        let (id, key) = match &route {
            Route::Album(id) => (id.clone(), RowKey::Album(id.clone())),
            Route::Artist(id) => (id.clone(), RowKey::Artist(id.clone())),
            _ => unreachable!(),
        };
        Row {
            kind: route.kind().into(),
            title: name.into(),
            detail: "100 available songs".into(),
            playback: Some(selection(&id)),
            playable: true,
            id,
            key,
            route: Some(route),
            ..Row::default()
        }
    }

    fn accepted(route: &Route, title: &str, rows: Vec<Row>) -> Page {
        Page {
            kind: route.kind().into(),
            title: title.into(),
            subtitle: "Server-only count".into(),
            description: "Server-only metadata".into(),
            rows,
            more: true,
            total: Some(100),
            playback: Some(selection("server")),
            ..Page::default()
        }
    }

    fn ids(page: &Page) -> Vec<&str> {
        page.rows.iter().map(|row| row.id.as_str()).collect()
    }

    fn assert_local(page: &Page) {
        assert!(
            page.local_audio && page.playback.is_none() && page.playlist.is_none() && !page.more
        );
        assert!(page.subtitle.is_empty() && page.description.is_empty());
        assert_eq!(page.state, Default::default());
        assert_eq!(page.total, Some(page.rows.len() as u64));
        assert!(
            page.rows.iter().all(|row| row.playback.is_none()
                && row.playlist.is_none()
                && row.position.is_none())
        );
    }

    #[test]
    fn search_uses_local_labels_but_never_joins_on_them() {
        let album = Route::Album("same-release-label".into());
        let artist = Route::Artist("same-artist-label".into());
        let route = Route::Search("fIXTuRE".into());
        let online = accepted(
            &route,
            "Search",
            vec![
                card(album.clone(), "Fixture Release"),
                card(artist.clone(), "Fixture Duo"),
                known_track(
                    "not-on-device",
                    Some("same-release-label"),
                    Some("same-artist-label"),
                ),
            ],
        );
        let item = recording("local-id", 5);
        let mut verified = verified([item.clone()]);
        verified.insert("incorrect-map-key".into(), recording("misidentified", 1));
        let pages = [(&route, &online)];
        let search = project(&route, &verified, &pages, None).unwrap();
        assert_eq!(search.kind, "search");
        assert_eq!(search.title, "Search");
        assert_eq!(ids(&search), ["local-id"]);
        let row = &search.rows[0];
        assert_eq!(row.item.as_ref(), Some(&item));
        assert_eq!(row.key, RowKey::Track(item.id));
        assert!(row.playable);
        assert_eq!(row.music.album.as_ref().unwrap().name, "Fixture Release");
        assert!(row.music.album.as_ref().unwrap().target.is_none());
        assert!(row.music.credits.is_empty());
        assert_local(&search);
        for detail in [album, artist] {
            assert!(
                project(&detail, &verified, &pages, None)
                    .unwrap()
                    .rows
                    .is_empty()
            );
        }
        assert!(project(&Route::Offline, &verified, &pages, None).is_none());
    }

    #[test]
    fn exact_track_links_supply_real_cards_and_keep_recording_metadata() {
        let route = Route::Search("fixture".into());
        let online = accepted(
            &route,
            "Search",
            vec![known_track("download", Some("release"), Some("artist"))],
        );
        let pages = [(&route, &online)];
        let mut item = recording("download", 1);
        item.artists = "Original local credit, Guest".into();
        item.album_name = "Local release label".into();
        let verified = verified([item.clone()]);
        let search = project(&route, &verified, &pages, None).unwrap();
        assert_eq!(ids(&search), ["artist", "release"]);
        assert_eq!(search.rows[0].route, Some(Route::Artist("artist".into())));
        assert_eq!(search.rows[1].route, Some(Route::Album("release".into())));
        assert!(search.rows.iter().all(|r| r.playable && r.item.is_none()));
        assert!(!search.local_collection_playable);
        assert_local(&search);
        let album = project(&Route::Album("release".into()), &verified, &pages, None).unwrap();
        assert!(album.local_collection_playable);
        let row = &album.rows[0];
        assert_eq!(row.title, item.name);
        assert_eq!(row.subtitle, item.artists);
        assert_eq!(row.detail, "1:04");
        assert_eq!(row.music.album.as_ref().unwrap().name, item.album_name);
        assert_eq!(
            row.music.album.as_ref().unwrap().target,
            Some(Route::Album("release".into()))
        );
        assert_eq!(
            row.music.credits[0].target,
            Some(Route::Artist("artist".into()))
        );
        let artist = project(&Route::Artist("artist".into()), &verified, &pages, None).unwrap();
        assert_eq!(ids(&artist), ["release"]);
        assert_eq!(artist.rows[0].kind, "album");
        assert!(artist.local_collection_playable && artist.rows[0].playable);
        assert_local(&artist);
    }

    #[test]
    fn exact_track_album_id_survives_without_a_label_and_wins_over_membership() {
        let q = Route::Search("fixture".into());
        let a = Route::Album("listing-a".into());
        let b = Route::Album("listing-b".into());
        let in_a = accepted(&a, "First listing", vec![known_track("song", None, None)]);
        let in_b = accepted(&b, "Second listing", vec![known_track("song", None, None)]);
        let mut item = recording("song", 1);
        item.album_name.clear();
        let verified = verified([item.clone()]);
        let ambiguous = Known::new(&verified, &[(&a, &in_a), (&b, &in_b)], None);
        assert!(ambiguous.track_music(&item).album.is_none());
        assert_eq!(ambiguous.tracks["song"].album(), Some("listing-b"));
        let mut exact = known_track("song", Some("album%2F1"), Some("artist"));
        exact.music.album.as_mut().unwrap().name.clear();
        let exact_inside_a = accepted(&a, "First listing", vec![exact.clone()]);
        let search = accepted(&q, "Search", vec![exact]);
        let with_exact = Known::new(&verified, &[(&a, &in_a), (&b, &in_b), (&q, &search)], None);
        assert_eq!(
            with_exact.track_music(&item).album,
            Some(Credit {
                name: String::new(),
                target: Some(Route::Album("album%2F1".into())),
            })
        );
        assert_eq!(with_exact.tracks["song"].album(), Some("listing-b"));
        assert_eq!(
            with_exact.artist_albums(&with_exact.artists["artist"]),
            ["listing-b"]
        );

        let explicit_id = Route::Album("album%2F1".into());
        let detail = project(&explicit_id, &verified, &[(&a, &exact_inside_a)], None).unwrap();
        assert_eq!(ids(&detail), ["song"]);
        assert!(detail.local_collection_playable);

        let other_query = Route::Search("other".into());
        let conflicting = accepted(
            &other_query,
            "Search",
            vec![known_track("song", Some("other-release"), None)],
        );
        for pages in [
            [(&q, &search), (&other_query, &conflicting)],
            [(&other_query, &conflicting), (&q, &search)],
        ] {
            let known = Known::new(&verified, &pages, None);
            assert!(known.track_music(&item).album.is_none());
            let mut labelled = item.clone();
            labelled.album_name = "Saved album label".into();
            assert_eq!(
                known.track_music(&labelled).album,
                Some(Credit {
                    name: "Saved album label".into(),
                    target: None
                })
            );
        }
    }

    #[test]
    fn credited_artist_without_a_resolvable_album_remains_openable_only() {
        let search = Route::Search("fixture duo".into());
        let artist = Route::Artist("artist".into());
        let online = accepted(
            &search,
            "Search",
            vec![known_track("download", None, Some("artist"))],
        );
        let verified = verified([recording("download", 1)]);
        let pages = [(&search, &online)];
        let results = project(&search, &verified, &pages, None).unwrap();
        let card = results
            .rows
            .iter()
            .find(|row| row.route.as_ref() == Some(&artist))
            .unwrap();
        assert!(!card.playable && card.playback.is_none());
        let detail = project(&artist, &verified, &pages, None).unwrap();
        assert!(!detail.local_collection_playable && detail.rows.is_empty());
        assert!(artist_tracks("artist", &verified, &pages, None).is_empty());
    }

    #[test]
    fn detail_order_wins_then_other_exact_members_use_real_numeric_tags() {
        let album = Route::Album("release".into());
        let artist = Route::Artist("performer".into());
        let q = Route::Search("anything".into());
        let release = accepted(
            &album,
            "Known release",
            vec![
                known_track("second", None, None),
                known_track("not-on-device", None, None),
                known_track("first", None, None),
            ],
        );
        let performer = accepted(
            &artist,
            "Known performer",
            vec![
                card(Route::Album("unavailable".into()), "Unavailable release"),
                card(album.clone(), "An older release label"),
            ],
        );
        let online_search = accepted(
            &q,
            "Search",
            vec![
                known_track("extra10", Some("release"), None),
                known_track("extra2", Some("release"), None),
                known_track("unnumbered", Some("release"), None),
            ],
        );
        let verified = verified([
            recording("extra10", 10),
            recording("extra2", 2),
            recording("unnumbered", 0),
            recording("first", 1),
            recording("second", 2),
            recording("no-exact-link", 3),
        ]);
        let pages = [
            (&q, &online_search),
            (&artist, &performer),
            (&album, &release),
        ];
        let detail = project(&album, &verified, &pages, None).unwrap();
        assert_eq!(detail.title, "Known release");
        assert_eq!(
            ids(&detail),
            ["second", "first", "extra2", "extra10", "unnumbered"]
        );
        assert!(
            detail
                .rows
                .iter()
                .all(|row| row.playable && row.item.as_ref() == verified.get(&row.id))
        );
        assert_local(&detail);
        let artists = project(&artist, &verified, &pages, None).unwrap();
        assert_eq!(artists.title, "Known performer");
        assert_eq!(ids(&artists), ["release"]);
        assert_eq!(artists.rows[0].title, "Known release");
        assert!(artists.rows[0].detail.is_empty());
        assert_local(&artists);
        assert_eq!(
            artist_tracks("performer", &verified, &pages, None),
            detail.rows
        );
    }

    #[test]
    fn playlist_evidence_enriches_exact_occurrences_but_not_matching_labels() {
        use crate::{api::library::PlaylistCapabilities, pages::PlaylistMetadata};

        let route = Route::Playlist("mix".into());
        let search = Route::Search("fixture".into());
        let mut item = recording("linked", 1);
        item.artists = "Local credit".into();
        let same_label = Item {
            id: "other".into(),
            ..item.clone()
        };
        let verified = verified([item.clone(), same_label.clone()]);
        for (kind, detail_kind) in [
            (PlaylistKind::Manual, "playlist-manual"),
            (PlaylistKind::Derived, "playlist-rules"),
        ] {
            let key = |playlist_id: &str| match kind {
                PlaylistKind::Manual => RowKey::ManualEntry {
                    playlist_id: playlist_id.into(),
                    entry_id: "entry".into(),
                },
                PlaylistKind::Derived => RowKey::DerivedOccurrence {
                    playlist_id: playlist_id.into(),
                    occurrence_id: "occurrence".into(),
                },
            };
            let mut page = accepted(
                &route,
                "Fixture playlist",
                vec![
                    Row {
                        key: key("mix"),
                        ..known_track("linked", Some("actual-album"), Some("actual-artist"))
                    },
                    Row {
                        key: key("wrong-parent"),
                        ..known_track("other", Some("bad-album"), Some("bad-artist"))
                    },
                ],
            );
            page.kind = detail_kind.into();
            page.playlist = Some(PlaylistMetadata {
                id: "mix".into(),
                name: "Fixture playlist".into(),
                kind,
                revision: 1,
                track_count: None,
                available_count: None,
                capabilities: PlaylistCapabilities {
                    edit_metadata: false,
                    edit_entries: false,
                    edit_definition: false,
                    play: false,
                    copy_to_manual: false,
                    delete: false,
                    add_tracks: false,
                },
            });
            let saved_row = |item: &Item| Row {
                key: RowKey::Track(item.id.clone()),
                kind: "track".into(),
                id: item.id.clone(),
                title: item.name.clone(),
                subtitle: item.artists.clone(),
                position: Some(3),
                playable: true,
                item: Some(item.clone()),
                ..Row::default()
            };
            let original = [saved_row(&item), saved_row(&same_label)];
            let mut rows = original.clone();
            attach_track_links(&mut rows, &verified, &[(&route, &page)], None);
            assert_eq!(
                rows[0].music.album.as_ref().unwrap().target,
                Some(Route::Album("actual-album".into()))
            );
            assert_eq!(
                rows[0].music.credits[0].target,
                Some(Route::Artist("actual-artist".into()))
            );
            assert!(rows[1].music.album.as_ref().unwrap().target.is_none());
            assert!(rows[1].music.credits.is_empty());
            for row in &mut rows {
                row.music = MusicMetadata::default();
            }
            assert_eq!(rows, original);

            let projected = project(&search, &verified, &[(&route, &page)], None).unwrap();
            assert!(
                projected
                    .rows
                    .iter()
                    .any(|row| row.key == RowKey::Album("actual-album".into()))
            );
            assert!(
                !projected
                    .rows
                    .iter()
                    .any(|row| row.key == RowKey::Album("bad-album".into()))
            );

            let mut wrong_id = page.clone();
            wrong_id.playlist.as_mut().unwrap().id = "wrong".into();
            let mut wrong_kind = page.clone();
            wrong_kind.kind = "playlist".into();
            let mut local = page.clone();
            local.local_audio = true;
            for bad in [&wrong_id, &wrong_kind, &local] {
                let projected = project(&search, &verified, &[(&route, bad)], None).unwrap();
                assert!(projected.rows.iter().all(|row| row.kind == "track"));
            }
        }
    }

    #[test]
    fn enrichment_keeps_existing_typed_links_without_corresponding_live_evidence() {
        let search = Route::Search("fixture".into());
        let item = recording("track", 1);
        let verified = verified([item.clone()]);
        let stored = MusicMetadata {
            credits: vec![
                Credit {
                    name: "Name-only guest".into(),
                    target: None,
                },
                Credit {
                    name: "Saved credit".into(),
                    target: Some(Route::Artist("saved-artist".into())),
                },
            ],
            album: Some(Credit {
                name: item.album_name,
                target: Some(Route::Album("saved-album".into())),
            }),
        };
        let mut row = Row {
            music: stored.clone(),
            playable: true,
            ..known_track("track", None, None)
        };
        attach_track_links(std::slice::from_mut(&mut row), &verified, &[], None);
        assert_eq!(row.music, stored);

        let album = accepted(
            &search,
            "Search",
            vec![known_track("track", Some("live-album"), None)],
        );
        attach_track_links(
            std::slice::from_mut(&mut row),
            &verified,
            &[(&search, &album)],
            None,
        );
        assert_eq!(
            row.music.album.as_ref().unwrap().target,
            Some(Route::Album("live-album".into()))
        );
        assert_eq!(row.music.credits, stored.credits);

        let artist = accepted(
            &search,
            "Search",
            vec![known_track("track", None, Some("live-artist"))],
        );
        row.music = stored;
        attach_track_links(
            std::slice::from_mut(&mut row),
            &verified,
            &[(&search, &artist)],
            None,
        );
        assert_eq!(
            row.music.credits[0].target,
            Some(Route::Artist("live-artist".into()))
        );
        assert_eq!(
            row.music.album.as_ref().unwrap().target,
            Some(Route::Album("saved-album".into()))
        );
    }

    #[test]
    fn history_replays_only_verified_recent_occurrences_in_existing_order() {
        let event = |id: &str, key: &str, date: &str| Row {
            key: RowKey::HistoryEvent(key.into()),
            detail: format!("{date} · Unavailable"),
            ..known_track(id, None, None)
        };
        let route = Route::History;
        let recent = accepted(
            &route,
            "History",
            vec![
                event("a", "online:two", "2026-10-02"),
                event("absent", "online:three", "2026-10-01"),
                event("a", "offline:two", "2026-09-30"),
                known_track("not-an-event", None, None),
            ],
        );
        let a = recording("a", 1);
        let verified = verified([a.clone(), recording("not-an-event", 1)]);
        let page = project(&route, &verified, &[], Some(&recent)).unwrap();
        assert_eq!(ids(&page), ["a", "a"]);
        assert_eq!(page.rows[0].key, RowKey::HistoryEvent("online:two".into()));
        assert_eq!(page.rows[1].key, RowKey::HistoryEvent("offline:two".into()));
        assert_eq!(page.rows[0].detail, "2026-10-02 · 1:04");
        assert_eq!(page.rows[1].detail, "2026-09-30 · 1:04");
        assert!(
            page.rows
                .iter()
                .all(|row| row.playable && row.item.as_ref() == Some(&a))
        );
        assert_local(&page);
        assert!(
            project(&route, &verified, &[], None)
                .unwrap()
                .rows
                .is_empty()
        );
    }
}
