//! Track-level adapter for the Saved screen; disk operations run off the desk actor.

use crate::{
    api::ApiError,
    desk::Access,
    engine::Command,
    offline::{self, OfflineStore},
    pages::{Page, Row, RowKey},
};

const STORAGE: ApiError = ApiError {
    message: "Unable to access private download storage.",
    expired: false,
    status: 0,
};
const INVALID_TRACK: ApiError = ApiError {
    message: "This item is not a saved track.",
    expired: false,
    status: 0,
};

fn error(err: offline::Error) -> ApiError {
    use offline::Error::*;
    if let Api(err) = err {
        return if err.status == 409 {
            ApiError {
                message: "The recording changed during the download and was not saved.",
                ..err
            }
        } else {
            err
        };
    }
    ApiError {
        message: match err {
            Api(_) => unreachable!(),
            InvalidTrack => "This item cannot be downloaded.",
            NotDownloaded => "This track has not been downloaded.",
            AlreadyDownloaded => "This track is already downloaded.",
            Full => "The download store is full. Remove a download and try again.",
            Busy => "The download store is busy. Try again shortly.",
            Storage => STORAGE.message,
            Damaged => "The downloaded file or store layout is invalid.",
            UnsupportedAudio => "This audio cannot be played on this device.",
            InvalidMedia => "Unable to verify this track’s download.",
            TrackTooLarge => "This track exceeds the 32 MiB download limit.",
            Offline => "Downloads are unavailable while offline.",
            Cancelled => "The download was cancelled.",
        },
        expired: false,
        status: 0,
    }
}

pub async fn page(access: &Access) -> Result<Page, ApiError> {
    let api = access.api.clone();
    let session = access.session.clone();
    let saved =
        tokio::task::spawn_blocking(move || OfflineStore::open_default()?.list(&api, &session))
            .await
            .map_err(|_| STORAGE)?
            .map_err(error)?;
    let rows = saved
        .into_iter()
        .map(|download| {
            let item = download.item;
            Row {
                kind: "track".into(),
                title: item.name.clone(),
                subtitle: item.artists.clone(),
                detail: item.album_name.clone(),
                playable: true,
                id: item.id.clone(),
                key: RowKey::Track(item.id.clone()),
                item: Some(item),
                ..Row::default()
            }
        })
        .collect::<Vec<_>>();
    Ok(Page {
        title: "Saved".into(),
        kind: "offline".into(),
        total: Some(rows.len() as u64),
        rows,
        ..Page::default()
    })
}

pub async fn save(access: &Access, row: &Row) -> Result<(), ApiError> {
    let item = row
        .item
        .as_ref()
        .filter(|item| item.id == row.id)
        .ok_or(INVALID_TRACK)?;
    if access.offline || access.session.is_offline() {
        return Err(error(offline::Error::Offline));
    }
    let store = tokio::task::spawn_blocking(OfflineStore::open_default)
        .await
        .map_err(|_| STORAGE)?
        .map_err(error)?;
    match store.download(&access.api, &access.session, item).await {
        Ok(_) | Err(offline::Error::AlreadyDownloaded) => Ok(()),
        Err(err) => Err(error(err)),
    }
}

pub async fn remove(access: &Access, row: &Row) -> Result<(), ApiError> {
    let item = row
        .item
        .as_ref()
        .filter(|item| item.id == row.id)
        .ok_or(INVALID_TRACK)?;
    let api = access.api.clone();
    let session = access.session.clone();
    let id = item.id.clone();
    tokio::task::spawn_blocking(move || OfflineStore::open_default()?.remove(&api, &session, &id))
        .await
        .map_err(|_| STORAGE)?
        .map_err(error)?;
    Ok(())
}

pub async fn play(access: &Access, page: &Page, index: usize) -> Result<Command, ApiError> {
    if !access.offline || !access.session.is_offline() {
        return Err(ApiError {
            message: "Go offline to play this saved queue.",
            expired: false,
            status: 0,
        });
    }
    access.api.authenticated_account(&access.session)?;
    let network = access.session.network_changes();
    let (mut queue, index) = queue_selection(page, index)?;
    let track_id = queue[index].id.clone();
    let api = access.api.clone();
    let session = access.session.clone();
    let (store, input) = tokio::task::spawn_blocking(move || {
        let store = OfflineStore::open_default()?;
        let input = store.read(&api, &session, &track_id)?;
        Ok::<_, offline::Error>((store, input))
    })
    .await
    .map_err(|_| STORAGE)?
    .map_err(error)?;
    access.api.authenticated_account(&access.session)?;
    if network.has_changed().unwrap_or(true) || !access.session.is_offline() {
        return Err(ApiError {
            message: "The playback mode changed.",
            expired: false,
            status: 0,
        });
    }
    // Prefer the presentation metadata paired with the verified local bytes.
    // Other occurrences carry only identities/labels and load lazily in the engine.
    queue[index] = input.item.clone();
    Ok(Command::PlayOffline {
        queue,
        index,
        input,
        store,
    })
}

fn queue_selection(
    page: &Page,
    visible_index: usize,
) -> Result<(Vec<crate::api::Item>, usize), ApiError> {
    let mut queue = Vec::new();
    let mut selected = None;
    for (visible, row) in page.rows.iter().enumerate() {
        let Some(item) = row.item.as_ref().filter(|item| {
            row.playable && row.id == item.id && item.kind == "track" && item.has_audio
        }) else {
            continue;
        };
        if queue.len() >= offline::MAX_DOWNLOADS {
            return Err(ApiError {
                message: "This saved queue has too many entries.",
                expired: false,
                status: 0,
            });
        }
        if visible == visible_index {
            selected = Some(queue.len());
        }
        queue.push(item.clone());
    }
    Ok((queue, selected.ok_or(INVALID_TRACK)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Item;

    fn row(id: &str, playable: bool) -> Row {
        Row {
            id: id.into(),
            playable,
            item: Some(Item {
                id: id.into(),
                name: id.into(),
                kind: "track".into(),
                artists: String::new(),
                album_name: String::new(),
                duration_ms: 0,
                has_audio: true,
                track_number: 0,
                disc_number: 0,
            }),
            ..Row::default()
        }
    }

    #[test]
    fn saved_queue_keeps_manifest_order_and_selects_the_second_duplicate() {
        let page = Page {
            rows: vec![
                Row::default(), // Saved collection row, with no track item.
                row("before", true),
                row("duplicate", true),
                row("missing", false),
                row("between", true),
                row("duplicate", true),
                row("after", true),
            ],
            ..Page::default()
        };
        // Pure projection of the saved snapshot, with no API or network access.
        let (queue, selected) = queue_selection(&page, 5).unwrap();
        assert_eq!(selected, 3);
        assert_eq!(queue[selected].id, "duplicate");
        assert_eq!(
            queue.into_iter().map(|item| item.id).collect::<Vec<_>>(),
            ["before", "duplicate", "between", "duplicate", "after"]
        );
        assert_eq!(queue_selection(&page, 2).unwrap().1, 1);
        assert!(queue_selection(&page, 0).is_err());
        assert!(queue_selection(&page, 3).is_err());
    }
}
