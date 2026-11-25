use super::*;

#[test]
fn snapshot_omission_is_authoritative_even_at_the_account_quota() {
    let mut r = Replica::default();
    initial(&mut r, 1);
    assert_eq!(watch(&mut r, "p"), sync("p", None, 0));
    part(&mut r, raw_summary("p", 1, 1), "first", 0, vec![raw_entry("e", 0)], false);
    assert_eq!(row_ids(&r), ["e"]);
    let pending = r.refresh_playlist().unwrap();
    let playlists: Vec<_> = (0..MAX_PLAYLISTS)
        .map(|i| raw_summary(&format!("other-{i}"), 0, 1))
        .collect();
    assert_eq!(send(&mut r, json!({"snapshot":{"playlists":playlists,"recent":[]}})), None);
    assert!(r.playlist_unavailable());
    assert!(r.cached("p").is_none());
    assert!(!r.accepts_sync(&pending));
    assert_eq!(r.watch(None), None);
    assert_eq!(watch(&mut r, "p"), None);
    assert!(r.playlist_unavailable());
    assert!(r.ready());
}

#[test]
fn over_quota_delta_fails_atomically_instead_of_evicting_a_live_summary() {
    let mut r = Replica::default();
    let playlists: Vec<_> = (0..MAX_PLAYLISTS)
        .map(|i| raw_summary(&format!("mix-{i}"), 0, 1))
        .collect();
    send(&mut r, json!({"snapshot":{"playlists":playlists,"recent":[]}}));
    let before = r.snapshot().unwrap().clone();
    let new = raw_summary("new", 0, 1);
    let illegal = serde_json::from_value(json!({"change":{"playlist":new.clone()}})).unwrap();
    assert_eq!(r.receive(illegal), Err(PersonalError));
    assert_eq!(r.snapshot(), Some(&before));
    send(&mut r, json!({"change":{"deleted":"mix-0","playlist":new}}));
    assert_eq!(r.snapshot().unwrap().playlists.len(), MAX_PLAYLISTS);
    assert!(r.snapshot().unwrap().playlists.iter().any(|p| p.id == "new"));
}

#[test]
fn only_a_started_current_scoped_continuation_accepts_a_missing_reply() {
    let mut r = Replica::default();
    initial(&mut r, 2);
    let first = watch(&mut r, "p").unwrap();
    let missing = || serde_json::from_value(json!({"missing":{"playlistId":"p"}})).unwrap();
    assert_eq!(r.receive(missing()), Ok(None));
    assert!(r.accepts_sync(&first));
    r.started_sync(&first, 0);
    let next = part(&mut r, raw_summary("p", 2, 1), "staged", 0, vec![raw_entry("e", 0)], true).unwrap();
    assert_eq!(r.receive(missing()), Ok(None), "continuation not yet sent");
    r.started_sync(&next, 2);
    assert_eq!(r.receive(missing()), Ok(None), "frame was queued before the continuation");
    assert!(r.accepts_sync(&next));
    let mut deleted = false;
    r.receive_observed(missing(), Some(3), |event| {
        deleted |= matches!(event, AcceptedPlaylist::Deleted("p"));
    }).unwrap();
    assert!(deleted);
    assert!(r.playlist_unavailable());
    assert!(r.staged.is_none() && r.playlist().is_none());
    assert!(!r.accepts_sync(&next));
    assert!(r.command(Command::DeletePlaylist { playlist_id: "p".into() }).is_none());
    assert!(r.command(Command::ClearHistory).is_some());
}
