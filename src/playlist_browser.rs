//! Downloaded collections and playlist pins. Normal playlist browsing uses personal_cache.

use crate::{
    api::{ApiError, library::PlaylistKind},
    collections,
    desk::Access,
    pages::{Page, Route, Row, RowKey},
    playlist_pin_store::{Provenance, Record, Store, Target},
    playlist_pins::Controller,
};

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

fn pin_rows(records: Vec<Record>) -> impl Iterator<Item = Row> {
    records.into_iter().filter(|r| r.wanted).map(|r| {
        let (title, status) = r.committed.as_ref().map_or(
            (r.target.title.as_str(), "Pending"),
            |m| (m.title.as_str(), "Saved"),
        );
        collections::saved_playlist_row(&r.key, title, r.target.kind, status.into())
    })
}

pub(crate) async fn saved_rows(access: &Access) -> Result<(Vec<Row>, Option<ApiError>), ApiError> {
    let disk_access = access.clone();
    let pins = disk(move || Store::open_default(&disk_access)?.scan(&disk_access)).await;
    let collections = collections::list(access).await;
    access.api.authenticated_account(&access.session)?;
    let mut warning = collections.as_ref().err().copied();
    let mut rows = collections.unwrap_or_default();
    match pins {
        Ok((records, unreadable)) => {
            if !unreadable.is_empty() {
                warning = Some(ApiError {
                    message: "Some offline playlists could not be loaded.",
                    status: 0,
                    expired: false,
                });
            }
            rows.extend(pin_rows(records));
        }
        Err(error) => warning = Some(error),
    }
    rows.sort_unstable_by(|a, b| a.title.cmp(&b.title).then_with(|| a.id.cmp(&b.id)));
    Ok((rows, warning))
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
        Page {
            title: target.title.clone(),
            kind: "saved-collection".into(),
            ..Page::default()
        }
    };
    page.saved_pin = Some(target);
    Ok(page)
}

pub(crate) async fn remove_collection(access: &Access, id: &str) -> Result<(), ApiError> {
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
        api::library::PlaylistCapabilities, pages::PlaylistMetadata,
        playlist_pin_store::Membership,
    };

    #[test]
    fn saved_pin_provenance_uses_manifest_position_not_visible_index_or_track_id() {
        for kind in [PlaylistKind::Manual, PlaylistKind::Derived] {
            let source = Provenance {
                revision: 7,
                occurrence_ids: ["last", "missing", "first"].map(str::to_owned).into(),
            };
            let mut page = Page {
                rows: [0, 2].map(|position| Row {
                    id: "same-track".into(),
                    key: RowKey::Track("same-track".into()),
                    position: Some(position),
                    ..Row::default()
                }).into(),
                ..Page::default()
            };
            apply_saved_provenance(&mut page, "playlist", kind, &source);
            assert_eq!(page.playlist_membership_revision, Some(7));
            for (row, occurrence) in page.rows.iter().zip(["last", "first"]) {
                assert_eq!(row.id, "same-track");
                assert_eq!(row.key, match kind {
                    PlaylistKind::Manual => RowKey::ManualEntry {
                        playlist_id: "playlist".into(), entry_id: occurrence.into(),
                    },
                    PlaylistKind::Derived => RowKey::DerivedOccurrence {
                        playlist_id: "playlist".into(), occurrence_id: occurrence.into(),
                    },
                });
            }
        }
    }

    #[test]
    fn summary_identifies_the_pin_target_without_playlist_detail() {
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
                        edit_metadata: false, edit_entries: false, edit_definition: false,
                        play: false, copy_to_manual: false, delete: false, add_tracks: false,
                    },
                }),
                ..Row::default()
            };
            let page = Page::default();
            let selected = target(&Route::Playlists, &page, Some(&row)).unwrap();
            assert_eq!(selected.id, id);
            assert_eq!(selected.kind, kind);
            assert!(target(&Route::Playlist(id), &page, None).is_none());
            row.id = "different-id".into();
            assert!(target(&Route::Playlists, &page, Some(&row)).is_none());
        }
    }

    #[test]
    fn saved_list_distinguishes_pending_complete_and_unpinned_playlists() {
        let pin = |key: &str, wanted, committed| Record {
            target: Target { id: key.into(), title: "Pin intent".into(), kind: PlaylistKind::Manual },
            key: key.into(), token: "test".into(), wanted, committed,
        };
        let committed = Membership {
            title: "Last complete membership".into(), tracks: vec!["track".into()], provenance: None,
        };
        let rows: Vec<_> = pin_rows(vec![pin("pending", true, None), pin("saved", true, Some(committed)), pin("retired", false, None)]).collect();
        assert_eq!(rows.len(), 2);
        assert_eq!((&*rows[0].title, &*rows[0].detail), ("Pin intent", "Pending"));
        assert_eq!((&*rows[1].title, &*rows[1].detail), ("Last complete membership", "Saved"));
    }
}
