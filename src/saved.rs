//! Track-level adapter for the Saved screen; disk operations run off the desk actor.

use std::sync::{Arc, OnceLock};
use tokio::sync::Semaphore;

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
const MODE_CHANGED: ApiError = ApiError {
    message: "The playback mode changed.",
    expired: false,
    status: 0,
};
static SELECTED_READ_SLOT: OnceLock<Arc<Semaphore>> = OnceLock::new();

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
            Full => "There is no space for this download. Free disk space and try again.",
            Busy => "The download store is busy. Try again shortly.",
            Storage => STORAGE.message,
            Damaged => "The downloaded file or store layout is invalid.",
            UnsupportedAudio => "This audio cannot be played on this device.",
            InvalidMedia => "Unable to verify this track’s download.",
            Offline => "Downloads are unavailable while offline.",
            Cancelled => "The download was cancelled.",
        },
        expired: false,
        status: 0,
    }
}

pub async fn storage_summary(access: &Access) -> Result<String, ApiError> {
    let (api, session) = (access.api.clone(), access.session.clone());
    let (count, bytes) = tokio::task::spawn_blocking(move || {
        let rows = match OfflineStore::open_existing().map_err(error)? {
            Some(store) => store.list(&api, &session).map_err(error)?,
            None => Vec::new(),
        };
        Ok::<_, ApiError>((rows.len(), rows.iter().map(|row| row.bytes).sum::<u64>()))
    })
    .await
    .map_err(|_| STORAGE)??;
    Ok(storage_label(count, bytes))
}

fn storage_label(count: usize, bytes: u64) -> String {
    let size = if bytes >= 1024 * 1024 {
        format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    };
    format!(
        "{size} · {count} {}",
        if count == 1 {
            "downloaded song"
        } else {
            "downloaded songs"
        }
    )
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
    let mut network = access.session.network_changes();
    if access.offline || *network.borrow_and_update() & 1 != 0 {
        return Err(error(offline::Error::Offline));
    }
    access.api.authenticated_account(&access.session)?;
    tokio::select! {
        biased;
        _ = network.changed() => Err(error(offline::Error::Offline)),
        result = async {
            let store = OfflineStore::open_for_download(OfflineStore::open_default).await.map_err(error)?;
            match store.download(&access.api, &access.session, item).await {
                Ok(_) | Err(offline::Error::AlreadyDownloaded) => Ok(()),
                Err(err) => Err(error(err)),
            }
        } => result,
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
    access.api.authenticated_account(&access.session)?;
    let mut network = access.session.network_changes();
    let mode_stamp = *network.borrow_and_update();
    let (mut queue, index) = queue_selection(page, index)?;
    let track_id = queue[index].id.clone();
    let slot = SELECTED_READ_SLOT
        .get_or_init(|| Arc::new(Semaphore::new(1)))
        .clone();
    let (store, input) = tokio::select! {
        biased;
        _ = network.changed() => return Err(MODE_CHANGED),
        result = offline::read_verified(access.api.clone(), access.session.clone(), slot, track_id, OfflineStore::open_default) => result.map_err(error)?,
    };
    access.api.authenticated_account(&access.session)?;
    if *network.borrow() != mode_stamp || access.offline != access.session.is_offline() {
        return Err(MODE_CHANGED);
    }
    // Prefer the presentation metadata paired with the verified local bytes.
    // Other occurrences carry only identities/labels and load lazily in the engine.
    queue[index] = input.item.clone();
    Ok(Command::PlaySaved {
        mode_stamp,
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
    fn storage_label_reports_only_downloads() {
        assert_eq!(storage_label(0, 0), "0 B · 0 downloaded songs");
        assert_eq!(storage_label(1, 2048), "2.0 KiB · 1 downloaded song");
        assert_eq!(
            storage_label(3, 2 * 1024 * 1024),
            "2.0 MiB · 3 downloaded songs"
        );
    }

    #[test]
    fn large_saved_view_can_play_an_occurrence_after_the_old_queue_limit() {
        let mut rows = vec![Row::default()];
        rows.extend((0..1161).map(|i| row("repeat", i != 1000)));
        let page = Page {
            rows,
            ..Page::default()
        };
        let (queue, selected) = queue_selection(&page, 1161).unwrap();
        assert_eq!(queue.len(), 1160);
        assert_eq!(selected, 1159);
        assert_eq!(queue[selected].id, "repeat");
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
