//! Offline projection for the normal Playlists and Saved routes. Observations
//! describe the source; pin intent and saved snapshots never stand in for it.

use std::collections::{HashMap, HashSet};

use crate::{
    api::{
        ApiError,
        library::{PlaylistCapabilities, PlaylistKind},
    },
    collections::{self, LegacyPlaylist},
    desk::Access,
    offline::Download,
    pages::{self, Page, PlaylistMetadata, Route, Row, RowKey},
    playlist_observations::{self, Membership, Summary},
    playlist_pin_store::{Provenance, Record, Store, Target},
    playlist_pins::Controller,
};

const UNAVAILABLE: ApiError = ApiError {
    message: "This playlist isn't available offline.",
    status: 404,
    expired: false,
};

const MODE_CHANGED: ApiError = ApiError {
    message: "The connection mode changed.",
    status: 0,
    expired: false,
};

const READ_ONLY: PlaylistCapabilities = PlaylistCapabilities {
    edit_metadata: false,
    edit_entries: false,
    edit_definition: false,
    play: false,
    copy_to_manual: false,
    delete: false,
    add_tracks: false,
};

pub(crate) async fn offline_page(access: &Access, route: &Route) -> Result<Page, ApiError> {
    let mode = access.session.network_changes();
    let generation = *mode.borrow();
    if !access.offline || generation & 1 == 0 {
        return Err(MODE_CHANGED);
    }
    // A failed read is not evidence of absence: we cannot rule out an
    // observed deletion and must not resurrect a saved snapshot.
    let observed = playlist_observations::load(access).await?;
    let page = match route {
        Route::Playlists => {
            let disk_access = access.clone();
            let pins = disk(move || Store::open_default(&disk_access)?.scan(&disk_access)).await;
            apply_list_playability(access, &observed, pins).await?
        }
        Route::Playlist(id) => {
            if observed.is_deleted(id) {
                return Err(UNAVAILABLE);
            }
            let summary = observed.summary(id);
            let complete = observed.membership(id);
            let (pin, legacy) = if needs_compatibility(summary, complete) {
                match compatibility(access, id).await {
                    Ok(pair) => pair,
                    Err(e) if !e.expired && summary.is_some() => (None, None),
                    Err(e) => return Err(e),
                }
            } else {
                (None, None)
            };
            let (mut page, members) = detail_projection(
                id,
                Observation {
                    summary,
                    complete,
                    deleted: false,
                },
                pin.as_ref(),
                legacy.as_ref(),
            )?;
            let ids = members.ids();
            if !ids.is_empty() {
                let available = collections::browse_tracks(access, ids).await?;
                members.project(&mut page, id, &available);
            }
            page
        }
        _ => return Err(UNAVAILABLE),
    };
    access.api.authenticated_account(&access.session)?;
    if *mode.borrow() != generation {
        return Err(MODE_CHANGED);
    }
    Ok(page)
}

async fn apply_list_playability(
    access: &Access,
    observed: &playlist_observations::ObservedPlaylists,
    pins: Result<(Vec<Record>, Vec<String>), ApiError>,
) -> Result<Page, ApiError> {
    let (records, unreadable, can_lookup_legacy) = match pins {
        Ok((records, unreadable)) => (records, unreadable, true),
        Err(e) if e.expired => return Err(e),
        // A broken pin index does not invalidate genuine observations, but
        // absence of a pin tombstone cannot be established for legacy data.
        Err(_) => (Vec::new(), Vec::new(), false),
    };
    let mut page = list_projection(observed.summaries(), |id| observed.is_deleted(id), &records);
    let pins: HashMap<_, _> = records.iter().map(|r| (r.target.id.as_str(), r)).collect();
    let legacy = if can_lookup_legacy {
        let eligible: Vec<_> = page
            .rows
            .iter()
            .filter_map(|row| {
                (needs_compatibility(observed.summary(&row.id), observed.membership(&row.id))
                    && allows_legacy(&row.id, pins.get(row.id.as_str()).copied()))
                .then(|| row.id.clone())
            })
            .collect();
        match list_legacy(access, eligible, unreadable).await {
            Ok(snapshots) => snapshots,
            Err(e) if e.expired => return Err(e),
            Err(_) => HashMap::new(),
        }
    } else {
        HashMap::new()
    };
    let members: Vec<_> = page
        .rows
        .iter()
        .map(|row| {
            let id = row.id.as_str();
            source_members(
                id,
                observed.summary(id),
                observed.membership(id),
                pins.get(id).copied(),
                legacy.get(id),
            )
        })
        .collect();
    let candidates = list_candidates(&members);
    let available = match collections::browse_tracks(access, candidates).await {
        Ok(available) => available,
        Err(e) if e.expired => return Err(e),
        Err(_) => return Ok(page), // Keep normal Open actions without proven local Play.
    };
    mark_playable(&mut page, &members, &available);
    Ok(page)
}

fn list_candidates(members: &[Members<'_>]) -> HashSet<String> {
    let unique: HashSet<_> = members.iter().flat_map(Members::track_ids).collect();
    unique.into_iter().map(str::to_owned).collect()
}

fn mark_playable(page: &mut Page, members: &[Members<'_>], available: &HashMap<String, Download>) {
    for (row, membership) in page.rows.iter_mut().zip(members) {
        row.playable = membership.has_local_track(available);
    }
}

async fn list_legacy(
    access: &Access,
    eligible: Vec<String>,
    unreadable: Vec<String>,
) -> Result<HashMap<String, LegacyPlaylist>, ApiError> {
    if eligible.is_empty() {
        return Ok(HashMap::new());
    }
    let access = access.clone();
    disk(move || {
        let unreadable: HashSet<_> = unreadable.into_iter().collect();
        let mut source_by_key = HashMap::new();
        for id in eligible {
            let key = collections::playlist_key(&access, &id)?;
            if !unreadable.contains(&key) {
                source_by_key.insert(key, id);
            }
        }
        collections::legacy_playlists(&access, &source_by_key)
    })
    .await
}

fn allows_legacy(id: &str, pin: Option<&Record>) -> bool {
    pin.is_none_or(|r| r.wanted && r.target.id == id && r.committed.is_none())
}

async fn compatibility(
    access: &Access,
    id: &str,
) -> Result<(Option<Record>, Option<LegacyPlaylist>), ApiError> {
    let (access, id) = (access.clone(), id.to_owned());
    disk(move || {
        let key = collections::playlist_key(&access, &id)?;
        // A malformed/unreadable pin might be a tombstone. Do not resurrect
        // a legacy snapshot when the pin state cannot be read.
        let pin = Store::open_default(&access)?.get(&access, &key)?;
        let legacy = if allows_legacy(&id, pin.as_ref()) {
            match collections::legacy_playlist(&access, &key) {
                Ok(snapshot) => snapshot,
                Err(e) if !e.expired && pin.is_some() => None,
                Err(e) => return Err(e),
            }
        } else {
            None
        };
        Ok((pin, legacy))
    })
    .await
}

fn list_projection<'a>(
    summaries: impl Iterator<Item = &'a Summary>,
    is_deleted: impl Fn(&str) -> bool,
    pins: &[Record],
) -> Page {
    let mut known = HashSet::new();
    let mut rows: Vec<_> = summaries
        .filter(|summary| !is_deleted(&summary.id))
        .map(|summary| {
            known.insert(summary.id.as_str());
            Row {
                playlist: Some(read_only_metadata(summary)),
                detail: pages::ui_text(&summary.description),
                ..playlist_row(
                    &summary.id,
                    &summary.name,
                    summary.kind,
                    summary.track_count,
                )
            }
        })
        .collect();
    rows.extend(pins.iter().filter_map(|pin| {
        let Target { id, kind, title } = &pin.target;
        (pin.wanted && !is_deleted(id) && !known.contains(id.as_str())).then(|| {
            let title = pin.committed.as_ref().map_or(title, |m| &m.title);
            playlist_row(id, title, *kind, None)
        })
    }));
    rows.sort_unstable_by(|a, b| a.title.cmp(&b.title).then_with(|| a.id.cmp(&b.id)));
    Page {
        title: "Playlists".into(),
        kind: "playlists".into(),
        total: Some(rows.len() as u64),
        rows,
        ..Page::default()
    }
}

fn display_title(name: &str) -> String {
    let label = pages::ui_text(name);
    if label.is_empty() {
        "Untitled playlist".into()
    } else {
        label
    }
}

fn playlist_row(id: &str, name: &str, kind: PlaylistKind, count: Option<u64>) -> Row {
    let label = kind_label(kind);
    Row {
        id: id.into(),
        title: display_title(name),
        key: RowKey::Playlist(id.into()),
        route: Some(Route::Playlist(id.into())),
        kind: "playlist".into(),
        subtitle: count.map_or_else(
            || label.into(),
            |n| format!("{label} · {n} {}", if n == 1 { "song" } else { "songs" }),
        ),
        ..Row::default()
    }
}

fn read_only_metadata(summary: &Summary) -> PlaylistMetadata {
    PlaylistMetadata {
        id: summary.id.clone(),
        name: summary.name.clone(),
        kind: summary.kind,
        revision: summary.revision,
        track_count: summary.track_count,
        available_count: summary.available_count,
        capabilities: READ_ONLY,
    }
}

fn kind_label(kind: PlaylistKind) -> &'static str {
    match kind {
        PlaylistKind::Manual => "Playlist",
        PlaylistKind::Derived => "Rule playlist",
    }
}

#[derive(Default)]
struct Observation<'a> {
    summary: Option<&'a Summary>,
    complete: Option<&'a Membership>,
    deleted: bool,
}

enum Members<'a> {
    Observed(&'a Membership, PlaylistKind),
    Saved {
        tracks: &'a [String],
        provenance: Option<(&'a Provenance, PlaylistKind)>,
    },
    Unknown,
}

fn playlist_row_key(kind: PlaylistKind, playlist_id: &str, occurrence_id: &str) -> RowKey {
    match kind {
        PlaylistKind::Manual => RowKey::ManualEntry {
            playlist_id: playlist_id.into(),
            entry_id: occurrence_id.into(),
        },
        PlaylistKind::Derived => RowKey::DerivedOccurrence {
            playlist_id: playlist_id.into(),
            occurrence_id: occurrence_id.into(),
        },
    }
}

/// Local detail projection keeps original manifest positions even when unavailable
/// audio is filtered. Never look up identity by track ID or visible row index.
fn apply_saved_provenance(page: &mut Page, id: &str, kind: PlaylistKind, source: &Provenance) {
    page.playlist_membership_revision = Some(source.revision);
    for row in &mut page.rows {
        if let Some(occurrence_id) = row
            .position
            .and_then(|position| usize::try_from(position).ok())
            .and_then(|position| source.occurrence_ids.get(position))
        {
            row.key = playlist_row_key(kind, id, occurrence_id);
        }
    }
}

impl Members<'_> {
    fn track_ids(&self) -> impl Iterator<Item = &str> {
        let (observed, saved) = match self {
            Self::Observed(members, _) => (members.rows.as_slice(), &[][..]),
            Self::Saved { tracks, .. } => (&[][..], *tracks),
            Self::Unknown => (&[][..], &[][..]),
        };
        observed
            .iter()
            .map(|r| r.track_id.as_str())
            .chain(saved.iter().map(String::as_str))
    }

    fn ids(&self) -> HashSet<String> {
        self.track_ids().map(str::to_owned).collect()
    }

    fn has_local_track(&self, available: &HashMap<String, Download>) -> bool {
        self.track_ids()
            .any(|id| collections::local_track(available, id).is_some())
    }

    fn project(&self, page: &mut Page, id: &str, available: &HashMap<String, Download>) {
        page.rows = match self {
            Self::Observed(members, kind) => members
                .rows
                .iter()
                .filter_map(|entry| {
                    let download = collections::local_track(available, &entry.track_id)?;
                    let key = playlist_row_key(*kind, id, &entry.occurrence_id);
                    Some(collections::local_track_row(download, key, entry.position))
                })
                .collect(),
            Self::Saved { tracks, .. } => tracks
                .iter()
                .enumerate()
                .filter_map(|(position, id)| {
                    let download = collections::local_track(available, id)?;
                    Some(collections::local_track_row(
                        download,
                        RowKey::Track(id.clone()),
                        position as u64,
                    ))
                })
                .collect(),
            Self::Unknown => Vec::new(),
        };
        if let Self::Saved {
            provenance: Some((source, kind)),
            ..
        } = self
        {
            apply_saved_provenance(page, id, *kind, source);
        }
    }

    fn revision(&self) -> Option<u64> {
        match self {
            Self::Observed(members, _) => Some(members.revision),
            Self::Saved { provenance, .. } => provenance.map(|(source, _)| source.revision),
            Self::Unknown => None,
        }
    }
}

fn needs_compatibility(summary: Option<&Summary>, complete: Option<&Membership>) -> bool {
    !summary.is_some_and(|s| known_empty(s) || complete.is_some_and(|m| usable_membership(s, m)))
}

fn known_empty(summary: &Summary) -> bool {
    summary.kind == PlaylistKind::Manual && summary.track_count == Some(0)
}

fn usable_membership(summary: &Summary, membership: &Membership) -> bool {
    if membership.revision > summary.revision {
        return false;
    }
    if known_empty(summary) {
        return membership.revision == summary.revision && membership.rows.is_empty();
    }
    // A Rules page with no rows does not establish empty membership if its
    // current observed count is unknown or positive.
    summary.kind != PlaylistKind::Derived
        || membership.revision < summary.revision
        || !membership.rows.is_empty()
        || summary.track_count == Some(0)
}

fn source_members<'a>(
    id: &str,
    summary: Option<&'a Summary>,
    complete: Option<&'a Membership>,
    pin: Option<&'a Record>,
    legacy: Option<&'a LegacyPlaylist>,
) -> Members<'a> {
    let summary = summary.filter(|s| s.id == id);
    let legacy = legacy.filter(|_| allows_legacy(id, pin));
    let pin = pin.filter(|r| r.wanted && r.target.id == id);
    let kind = summary
        .map(|s| s.kind)
        .or_else(|| pin.map(|p| p.target.kind));
    if let Some(s) = summary {
        if let Some(membership) = complete.filter(|m| usable_membership(s, m)) {
            // Last-good membership retains its own genuine revision.
            return Members::Observed(membership, s.kind);
        }
        // Newer exact empty Manual membership supersedes older saved versions.
        if known_empty(s) {
            return Members::Unknown;
        }
    }
    if let Some(pin) = pin.filter(|p| Some(p.target.kind) == kind) {
        if let Some(membership) = &pin.committed {
            return Members::Saved {
                tracks: &membership.tracks,
                provenance: membership
                    .provenance
                    .as_ref()
                    .map(|source| (source, pin.target.kind)),
            };
        }
    }
    if let Some(snapshot) = legacy.filter(|p| kind.is_none_or(|k| k == p.kind)) {
        return Members::Saved {
            tracks: &snapshot.tracks,
            provenance: None,
        };
    }
    Members::Unknown
}

fn detail_projection<'a>(
    id: &str,
    observed: Observation<'a>,
    pin: Option<&'a Record>,
    legacy: Option<&'a LegacyPlaylist>,
) -> Result<(Page, Members<'a>), ApiError> {
    if observed.deleted {
        return Err(UNAVAILABLE);
    }
    let summary = observed.summary.filter(|s| s.id == id);
    let members = source_members(id, summary, observed.complete, pin, legacy);
    // A pin tombstone (or unexpected identity) must not resurrect a snapshot
    // or its title. It does not erase a genuine source observation.
    let legacy = legacy.filter(|_| allows_legacy(id, pin));
    let pin = pin.filter(|r| r.wanted && r.target.id == id);
    let (name, kind) = if let Some(s) = summary {
        (&s.name, s.kind)
    } else if let Some(pin) = pin {
        let title = pin
            .committed
            .as_ref()
            .map_or(&pin.target.title, |m| &m.title);
        (title, pin.target.kind)
    } else if let Some(legacy) = legacy {
        (&legacy.title, legacy.kind)
    } else {
        return Ok((
            Page {
                kind: "playlist".into(),
                title: "Playlist".into(),
                ..Page::default()
            },
            Members::Unknown,
        ));
    };

    let total = match (summary, &members) {
        (Some(s), _) => s.track_count,
        (None, Members::Saved { tracks, .. }) => Some(tracks.len() as u64),
        _ => None,
    };
    let page = Page {
        title: display_title(name),
        kind: match kind {
            PlaylistKind::Manual => "playlist-manual",
            PlaylistKind::Derived => "playlist-rules",
        }
        .into(),
        playlist: summary.map(read_only_metadata),
        playlist_membership_revision: members.revision(),
        description: summary.map_or_else(String::new, |s| s.description.clone()),
        total,
        ..Page::default()
    };
    Ok((page, members))
}

pub(crate) fn target(route: &Route, page: &Page, row: Option<&Row>) -> Option<Target> {
    let meta = match (route, row) {
        (Route::Playlists, Some(r)) if matches!(&r.route, Some(Route::Playlist(id)) if id == &r.id) => {
            r.playlist.as_ref().filter(|m| m.id == r.id)
        }
        (Route::Playlist(id), None) => page.playlist.as_ref().filter(|m| &m.id == id),
        _ => None,
    }?;
    Some(Target {
        id: meta.id.clone(),
        title: meta.name.clone(),
        kind: meta.kind,
    })
}

// The playable snapshot may be old while the pin is still pending. Leave it
// visible on the same route until a complete replacement is published.
fn merge(legacy: Vec<Row>, pins: Vec<Record>) -> Vec<Row> {
    let mut result = Vec::new();
    let mut pins: HashMap<_, _> = pins.into_iter().map(|r| (r.key.clone(), r)).collect();
    for row in legacy {
        match pins.get(&row.id) {
            Some(r) if !r.wanted || r.committed.is_some() => (),
            _ => {
                pins.remove(&row.id);
                result.push(row);
            }
        }
    }
    result.extend(pins.into_values().filter(|r| r.wanted).map(|r| {
        let (title, status) = r
            .committed
            .as_ref()
            .map_or((r.target.title.as_str(), "Pending"), |m| {
                (m.title.as_str(), "Saved")
            });
        collections::saved_playlist_row(&r.key, title, r.target.kind, status.into())
    }));
    result.sort_unstable_by(|a, b| a.title.cmp(&b.title).then_with(|| a.id.cmp(&b.id)));
    result
}

pub(crate) async fn saved_rows(access: &Access) -> Result<(Vec<Row>, Option<ApiError>), ApiError> {
    let disk_access = access.clone();
    let pins = disk(move || Store::open_default(&disk_access)?.scan(&disk_access)).await;
    let legacy = collections::list(access, pins.is_ok()).await;
    access.api.authenticated_account(&access.session)?;
    let mut warning = legacy.as_ref().err().copied();
    let mut legacy = legacy.unwrap_or_default();
    match pins {
        Ok((records, unreadable)) => {
            if !unreadable.is_empty() {
                let unreadable: std::collections::HashSet<_> = unreadable.into_iter().collect();
                // A broken pin might be a tombstone; do not resurrect the
                // matching legacy row. Other downloads remain usable.
                legacy.retain(|row| !unreadable.contains(&row.id));
                warning = Some(ApiError {
                    message: "Some offline playlists could not be loaded.",
                    status: 0,
                    expired: false,
                });
            }
            Ok((merge(legacy, records), warning))
        }
        Err(err) => Ok((legacy, Some(err))),
    }
}

pub(crate) async fn detail(access: &Access, id: &str) -> Result<Page, ApiError> {
    let Some(record) = record(access, id).await? else {
        return collections::detail(access, id).await;
    };
    if !record.wanted {
        return Err(ApiError {
            message: "This playlist is not saved on this device.",
            status: 404,
            expired: false,
        });
    }
    let target = record.target;
    let mut page = if let Some(member) = record.committed {
        let mut page =
            collections::saved_playlist_page(access, member.title, target.kind, member.tracks)
                .await?;
        if let Some(source) = member.provenance {
            apply_saved_provenance(&mut page, &target.id, target.kind, &source);
        }
        page
    } else {
        match collections::detail(access, id).await {
            Err(e) if e.status == 404 => Page {
                title: target.title.clone(),
                kind: "saved-collection".into(),
                ..Page::default()
            },
            other => other?,
        }
    };
    page.saved_pin = Some(target);
    Ok(page)
}

pub(crate) async fn remove_legacy(access: &Access, id: &str) -> Result<(), ApiError> {
    if record(access, id).await?.is_some() {
        return Err(ApiError {
            message: "This saved collection changed. Close the confirmation and open it again.",
            expired: false,
            status: 409,
        });
    }
    collections::remove(access, id).await
}

pub(crate) async fn remove(
    access: &Access,
    controller: &Controller,
    id: &str,
) -> Result<(), ApiError> {
    if let Some(r) = record(access, id).await? {
        if r.wanted {
            controller.set(access, r.target, false).await?;
        }
        return Ok(());
    }
    collections::remove(access, id).await
}

async fn record(access: &Access, id: &str) -> Result<Option<Record>, ApiError> {
    let (access, id) = (access.clone(), id.to_owned());
    disk(move || Store::open_default(&access)?.get(&access, &id)).await
}

async fn disk<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, ApiError> + Send + 'static,
) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(f).await.map_err(|_| ApiError {
        message: "Unable to access saved playlists.",
        status: 0,
        expired: false,
    })?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        api::{Item, library::EvaluationState},
        playlist_observations::Occurrence,
        playlist_pin_store::Membership as PinMembership,
    };

    fn summary(id: &str, kind: PlaylistKind) -> Summary {
        Summary {
            id: id.into(),
            name: "Observed title".into(),
            description: "Observed description".into(),
            kind,
            evaluation_state: (kind == PlaylistKind::Derived).then_some(EvaluationState::Ready),
            track_count: Some(3),
            available_count: Some(3),
            revision: 7,
            updated_at: "2025-01-01T00:00:00Z".into(),
        }
    }

    fn occurrence(key: &str, id: &str, position: u64) -> Occurrence {
        Occurrence {
            occurrence_id: key.into(),
            position,
            track_id: id.into(),
        }
    }

    fn local_download(id: &str) -> Download {
        collections::synthetic_download(Item {
            id: id.into(),
            name: "Local title".into(),
            kind: "track".into(),
            artists: "Local artists".into(),
            album_name: "Local album".into(),
            has_audio: true,
            duration_ms: 123_456,
            track_number: 3,
            disc_number: 4,
        })
    }

    #[test]
    fn observed_occurrence_identity_order_and_local_metadata_survive_filtering() {
        for kind in [PlaylistKind::Manual, PlaylistKind::Derived] {
            let summary = summary("source", kind);
            let complete = Membership {
                revision: summary.revision,
                rows: vec![
                    occurrence("first", "one", 0),
                    occurrence("middle", "missing", 1),
                    occurrence("repeat", "one", 2),
                ],
            };
            assert!(!needs_compatibility(Some(&summary), Some(&complete)));
            let pin = pin("key", true, true);
            let (mut page, members) = detail_projection(
                &summary.id,
                Observation {
                    summary: Some(&summary),
                    complete: Some(&complete),
                    deleted: false,
                },
                Some(&pin),
                None,
            )
            .unwrap();
            assert_eq!(
                members.ids(),
                HashSet::from(["one".into(), "missing".into()])
            );
            let downloaded = local_download("one");
            members.project(
                &mut page,
                &summary.id,
                &HashMap::from([
                    ("one".into(), downloaded.clone()),
                    ("missing".into(), local_download("unrelated")),
                ]),
            );
            assert_eq!(page.title, "Observed title");
            assert_eq!(page.total, Some(3));
            assert_eq!(page.playlist_membership_revision, Some(7));
            assert!(page.status.is_empty());
            assert!(page.playback.is_none());
            assert_eq!(page.playlist.as_ref().unwrap().capabilities, READ_ONLY);
            assert_eq!(page.rows.len(), 2);
            for (row, (position, key)) in page.rows.iter().zip([(0, "first"), (2, "repeat")]) {
                assert_eq!(row.position, Some(position));
                assert_eq!(row.id, "one");
                assert_eq!(
                    row.key,
                    match kind {
                        PlaylistKind::Manual => RowKey::ManualEntry {
                            playlist_id: "source".into(),
                            entry_id: key.into()
                        },
                        PlaylistKind::Derived => RowKey::DerivedOccurrence {
                            playlist_id: "source".into(),
                            occurrence_id: key.into()
                        },
                    }
                );
                assert_eq!(row.item.as_ref(), Some(&downloaded.item));
                assert_eq!(row.local_recording.as_ref(), Some(&downloaded.recording));
                assert_eq!(row.title, "Local title");
                assert_eq!(row.subtitle, "Local artists");
                assert_eq!(row.detail, "2:03");
                assert_eq!(
                    row.music.album.as_ref(),
                    Some(&pages::Credit {
                        name: "Local album".into(),
                        target: None,
                    })
                );
                assert!(row.music.credits.is_empty());
                assert!(row.playable && row.playback.is_none());
            }
        }
    }

    #[test]
    fn saved_pin_provenance_uses_manifest_position_not_visible_index_or_track_id() {
        for kind in [PlaylistKind::Manual, PlaylistKind::Derived] {
            let identity = |suffix| match kind {
                PlaylistKind::Manual => format!("entry_{suffix}"),
                PlaylistKind::Derived => format!("o1_{suffix}"),
            };
            let mut pin = pin("saved-route-key", true, true);
            pin.target.kind = kind;
            let member = pin.committed.as_mut().unwrap();
            member.tracks = vec!["one".into(), "missing".into(), "one".into()];
            let available = HashMap::from([("one".into(), local_download("one"))]);
            let (mut legacy, members) =
                detail_projection("source", Observation::default(), Some(&pin), None).unwrap();
            members.project(&mut legacy, "source", &available);
            assert!(legacy.playlist_membership_revision.is_none());
            assert_eq!(legacy.total, Some(3));
            assert_eq!(legacy.rows.len(), 2);
            assert!(
                legacy
                    .rows
                    .iter()
                    .all(|row| row.key == RowKey::Track("one".into()))
            );

            let source = Provenance {
                revision: 7,
                occurrence_ids: vec![identity("last"), identity("middle"), identity("first")],
            };
            // Saved detail receives generic rows/positions from collections.
            let mut saved = Page {
                saved_pin: Some(pin.target.clone()),
                ..legacy
            };
            apply_saved_provenance(&mut saved, &pin.target.id, kind, &source);
            pin.committed.as_mut().unwrap().provenance = Some(source);
            let (mut fallback, members) =
                detail_projection("source", Observation::default(), Some(&pin), None).unwrap();
            members.project(&mut fallback, "source", &available);
            assert_eq!(saved.playlist_membership_revision, Some(7));
            assert_eq!(fallback.playlist_membership_revision, Some(7));
            assert_eq!(saved.rows, fallback.rows);
            for (row, (position, suffix)) in saved.rows.iter().zip([(0, "last"), (2, "first")]) {
                let key = match kind {
                    PlaylistKind::Manual => RowKey::ManualEntry {
                        playlist_id: "source".into(),
                        entry_id: identity(suffix),
                    },
                    PlaylistKind::Derived => RowKey::DerivedOccurrence {
                        playlist_id: "source".into(),
                        occurrence_id: identity(suffix),
                    },
                };
                assert_eq!(row.key, key);
                assert_eq!(row.position, Some(position));
                assert_eq!(row.id, "one");
                assert_eq!(row.local_recording, Some(available["one"].recording));
            }
        }
    }

    #[test]
    fn newer_summary_keeps_its_title_and_revision_over_last_good_members() {
        let older = summary("source", PlaylistKind::Derived);
        let complete = Membership {
            revision: older.revision,
            rows: vec![occurrence("prior", "one", 0)],
        };
        let latest = Summary {
            revision: 8,
            name: "New title".into(),
            evaluation_state: Some(EvaluationState::Error),
            track_count: None,
            available_count: None,
            ..older
        };
        let (mut page, members) = detail_projection(
            "source",
            Observation {
                summary: Some(&latest),
                complete: Some(&complete),
                deleted: false,
            },
            None,
            None,
        )
        .unwrap();
        members.project(
            &mut page,
            "source",
            &HashMap::from([("one".into(), local_download("one"))]),
        );
        assert_eq!(page.title, "New title");
        assert_eq!(page.playlist.as_ref().unwrap().revision, 8);
        assert!(page.playlist.as_ref().unwrap().track_count.is_none());
        assert_eq!(page.rows.len(), 1);
        assert_eq!(page.playlist_membership_revision, Some(7));
        assert!(page.status.is_empty());
    }

    #[test]
    fn empty_rules_parts_without_an_empty_count_do_not_shadow_committed_membership() {
        let latest = Summary {
            evaluation_state: Some(EvaluationState::Pending),
            track_count: None,
            available_count: None,
            ..summary("source", PlaylistKind::Derived)
        };
        let empty = Membership {
            revision: latest.revision,
            rows: Vec::new(),
        };
        assert!(needs_compatibility(Some(&latest), Some(&empty)));
        let mut pin = pin("key", true, true);
        pin.target.kind = PlaylistKind::Derived;
        let (page, members) = detail_projection(
            "source",
            Observation {
                summary: Some(&latest),
                complete: Some(&empty),
                deleted: false,
            },
            Some(&pin),
            None,
        )
        .unwrap();
        assert_eq!(members.ids(), HashSet::from(["one".into()]));
        assert!(page.playlist_membership_revision.is_none());
    }

    #[test]
    fn exact_newer_empty_membership_wins_over_older_observed_and_saved_versions() {
        let latest = Summary {
            revision: 8,
            track_count: Some(0),
            available_count: Some(0),
            ..summary("source", PlaylistKind::Manual)
        };
        let complete = Membership {
            revision: 7,
            rows: vec![occurrence("prior", "one", 0)],
        };
        let pin = pin("key", true, true);
        for complete in [None, Some(&complete)] {
            assert!(!needs_compatibility(Some(&latest), complete));
            let (page, members) = detail_projection(
                "source",
                Observation {
                    summary: Some(&latest),
                    complete,
                    deleted: false,
                },
                Some(&pin),
                None,
            )
            .unwrap();
            assert_eq!(page.total, Some(0));
            assert!(members.ids().is_empty());
        }
    }

    #[test]
    fn list_keeps_observed_summaries_without_audio_or_pin_membership() {
        let summary = summary("source", PlaylistKind::Manual);
        let separate = Summary {
            id: "another-source".into(),
            name: "Another playlist".into(),
            ..summary.clone()
        };
        let page = list_projection(
            [&summary, &separate].into_iter(),
            |_| false,
            &[pin("key", true, true)],
        );
        assert_eq!(page.total, Some(2));
        for (row, summary) in page.rows.iter().zip([&separate, &summary]) {
            assert_eq!(row.id, summary.id);
            assert_eq!(row.title, summary.name);
            assert_eq!(row.route, Some(Route::Playlist(summary.id.clone())));
            assert_eq!(row.playlist.as_ref(), Some(&read_only_metadata(summary)));
            assert!(!row.playable && row.playback.is_none());
        }
    }

    #[test]
    fn list_play_matches_exact_local_members_using_one_candidate_union() {
        let observed = summary("observed", PlaylistKind::Manual);
        let legacy_summary = summary("legacy", PlaylistKind::Manual);
        let no_local = summary("missing", PlaylistKind::Manual);
        let empty = Summary {
            track_count: Some(0),
            available_count: Some(0),
            ..summary("empty", PlaylistKind::Manual)
        };
        let complete = Membership {
            revision: observed.revision,
            rows: vec![occurrence("a", "one", 0), occurrence("b", "one", 1)],
        };
        let unavailable = Membership {
            revision: no_local.revision,
            rows: vec![occurrence("c", "other", 0)],
        };
        let legacy = LegacyPlaylist {
            title: "Snapshot".into(),
            kind: PlaylistKind::Manual,
            tracks: vec!["two".into()],
        };
        let mut committed = pin("key", true, true);
        committed.target.id = "committed".into();
        let mut pending = pin("key2", true, false);
        pending.target.id = "pending".into();
        let pins = [committed, pending];
        let mut page = list_projection(
            [&observed, &legacy_summary, &no_local, &empty].into_iter(),
            |_| false,
            &pins,
        );
        let members: Vec<_> = page
            .rows
            .iter()
            .map(|row| match row.id.as_str() {
                "committed" => source_members(&row.id, None, None, Some(&pins[0]), None),
                "pending" => source_members(&row.id, None, None, Some(&pins[1]), None),
                "observed" => source_members(
                    &row.id,
                    Some(&observed),
                    Some(&complete),
                    None,
                    Some(&legacy),
                ),
                "legacy" => {
                    source_members(&row.id, Some(&legacy_summary), None, None, Some(&legacy))
                }
                "missing" => source_members(
                    &row.id,
                    Some(&no_local),
                    Some(&unavailable),
                    None,
                    Some(&legacy),
                ),
                "empty" => source_members(&row.id, Some(&empty), None, None, Some(&legacy)),
                _ => panic!("unexpected row"),
            })
            .collect();
        assert_eq!(
            list_candidates(&members),
            HashSet::from(["one".into(), "two".into(), "other".into()])
        );
        mark_playable(
            &mut page,
            &members,
            &HashMap::from([
                ("one".into(), local_download("one")),
                ("two".into(), local_download("two")),
                ("other".into(), local_download("unrelated")),
            ]),
        );
        for row in &page.rows {
            assert_eq!(
                row.playable,
                matches!(row.id.as_str(), "committed" | "observed" | "legacy")
            );
            assert_eq!(row.route, Some(Route::Playlist(row.id.clone())));
            assert!(row.playback.is_none());
        }
    }

    #[test]
    fn unobserved_direct_playlist_has_an_ordinary_empty_detail() {
        let (page, members) =
            detail_projection("unknown", Observation::default(), None, None).unwrap();
        assert_eq!(page.kind, "playlist");
        assert_eq!(page.title, "Playlist");
        assert!(page.rows.is_empty());
        assert!(page.status.is_empty());
        assert!(page.playlist.is_none());
        assert!(page.total.is_none());
        assert!(members.ids().is_empty());
    }

    #[test]
    fn observed_deletion_and_pin_tombstone_have_distinct_authority() {
        let known = summary("source", PlaylistKind::Manual);
        let ready = pin("key", true, true);
        let legacy = LegacyPlaylist {
            title: "Legacy title".into(),
            kind: known.kind,
            tracks: vec!["two".into()],
        };
        assert!(matches!(
            detail_projection(
                "source",
                Observation {
                    summary: Some(&known),
                    deleted: true,
                    ..Observation::default()
                },
                Some(&ready),
                Some(&legacy)
            ),
            Err(UNAVAILABLE)
        ));
        assert!(
            list_projection([&known].into_iter(), |id| id == "source", &[ready])
                .rows
                .is_empty()
        );
        let tombstone = pin("key", false, false);
        let (page, members) = detail_projection(
            "source",
            Observation::default(),
            Some(&tombstone),
            Some(&legacy),
        )
        .unwrap();
        assert_eq!(page.kind, "playlist");
        assert_eq!(page.title, "Playlist");
        assert!(members.ids().is_empty());
        let (page, members) = detail_projection(
            "source",
            Observation {
                summary: Some(&known),
                ..Observation::default()
            },
            Some(&tombstone),
            Some(&legacy),
        )
        .unwrap();
        assert_eq!(page.title, "Observed title");
        assert!(members.ids().is_empty());
    }

    #[test]
    fn pending_pin_is_a_source_target_not_membership() {
        let pending = pin("key", true, false);
        let list = list_projection(
            std::iter::empty(),
            |_| false,
            std::slice::from_ref(&pending),
        );
        assert_eq!(list.rows.len(), 1);
        assert_eq!(list.rows[0].route, Some(Route::Playlist("source".into())));
        assert!(list.rows[0].playlist.is_none());
        let (mut page, members) =
            detail_projection("source", Observation::default(), Some(&pending), None).unwrap();
        members.project(
            &mut page,
            "source",
            &HashMap::from([("one".into(), local_download("one"))]),
        );
        assert_eq!(page.kind, "playlist-manual");
        assert_eq!(page.title, "Pinned title");
        assert!(page.playlist.is_none());
        assert!(page.playlist_membership_revision.is_none());
        assert!(page.total.is_none());
        assert!(page.rows.is_empty());
        let legacy = LegacyPlaylist {
            title: "Legacy title".into(),
            kind: pending.target.kind,
            tracks: vec!["two".into()],
        };
        let (mut page, members) = detail_projection(
            "source",
            Observation::default(),
            Some(&pending),
            Some(&legacy),
        )
        .unwrap();
        assert_eq!(members.ids(), HashSet::from(["two".into()]));
        let downloaded = HashMap::from([
            ("one".into(), local_download("one")),
            ("two".into(), local_download("two")),
        ]);
        members.project(&mut page, "source", &downloaded);
        assert_eq!(page.rows.len(), 1);
        assert_eq!(
            page.rows[0].local_recording.as_ref(),
            Some(&downloaded["two"].recording)
        );
        let ready = pin("key", true, true);
        let known = summary("source", PlaylistKind::Manual);
        let (mut page, members) = detail_projection(
            "source",
            Observation {
                summary: Some(&known),
                ..Observation::default()
            },
            Some(&ready),
            Some(&legacy),
        )
        .unwrap();
        assert_eq!(members.ids(), HashSet::from(["one".into()]));
        members.project(&mut page, "source", &downloaded);
        assert_eq!(page.rows.len(), 1);
        assert_eq!(
            page.rows[0].local_recording.as_ref(),
            Some(&downloaded["one"].recording)
        );
        assert_eq!(page.playlist.unwrap().revision, known.revision);
        assert!(page.playlist_membership_revision.is_none());
    }

    #[test]
    fn summary_can_pin_even_without_any_playlist_detail() {
        for kind in [PlaylistKind::Manual, PlaylistKind::Derived] {
            let id = "playlist-id".to_owned();
            let mut row = Row {
                id: id.clone(),
                route: Some(Route::Playlist(id.clone())),
                playlist: Some(PlaylistMetadata {
                    id: id.clone(),
                    name: "Large playlist".into(),
                    kind,
                    revision: 7,
                    track_count: Some(1_000_000),
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
                }),
                ..Row::default()
            };
            let view_without_detail = Page::default();
            let selected = target(&Route::Playlists, &view_without_detail, Some(&row)).unwrap();
            assert_eq!(selected.id, id);
            assert_eq!(selected.kind, kind);
            assert!(target(&Route::Playlist(id), &view_without_detail, None).is_none());
            row.id = "different-id".into();
            assert!(target(&Route::Playlists, &view_without_detail, Some(&row)).is_none());
        }
    }

    fn legacy(key: &str) -> Row {
        collections::saved_playlist_row(
            key,
            "Older snapshot",
            PlaylistKind::Manual,
            "1 of 3 songs available".into(),
        )
    }
    fn pin(key: &str, wanted: bool, ready: bool) -> Record {
        Record {
            target: Target {
                id: "source".into(),
                title: "Pinned title".into(),
                kind: PlaylistKind::Manual,
            },
            key: key.into(),
            token: "token".into(),
            wanted,
            committed: ready.then(|| PinMembership {
                title: "Complete title".into(),
                tracks: vec!["one".into()],
                provenance: None,
            }),
        }
    }
    #[test]
    fn pending_pins_keep_legacy_playable_and_duplicates_are_replaced_only_when_complete() {
        let legacy = vec![legacy("overlap"), legacy("unrelated"), legacy("unpinned")];
        let pins = vec![
            pin("overlap", true, false),
            pin("unpinned", false, false),
            pin("new", true, false),
        ];
        let rows = merge(legacy.clone(), pins);
        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows.iter().find(|r| r.id == "overlap").unwrap().detail,
            "1 of 3 songs available"
        );
        assert!(rows.iter().any(|r| r.id == "new" && r.detail == "Pending"));
        assert!(rows.iter().all(|r| r.id != "unpinned"));
        let ready = merge(legacy, vec![pin("overlap", true, true)]);
        let rows: Vec<_> = ready.iter().filter(|r| r.id == "overlap").collect();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].title, "Complete title");
    }
}
