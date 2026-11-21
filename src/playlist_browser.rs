//! Saved playlist projection. New pins take precedence over matching legacy
//! snapshots, but never mutate the legacy index or remove audio.

use std::collections::HashMap;

use crate::{
    api::ApiError,
    collections,
    desk::Access,
    pages::{Page, Route, Row},
    playlist_pin_store::{Record, Store, Target},
    playlist_pins::Controller,
};

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
        let access = access.clone();
        let kind = target.kind;
        disk(move || collections::saved_playlist_page(&access, member.title, kind, member.tracks))
            .await?
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
        api::library::{PlaylistCapabilities, PlaylistKind},
        pages::PlaylistMetadata,
        playlist_pin_store::Membership,
    };

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
            committed: ready.then(|| Membership {
                title: "Complete title".into(),
                tracks: vec!["one".into()],
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
