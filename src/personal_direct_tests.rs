use super::*;

#[test]
fn direct_watch_not_in_snapshot_gets_scoped_parts_and_no_retry_loop() {
    let mut r = Replica::default();
    assert_eq!(watch(&mut r, "direct"), None);
    assert_eq!(initial(&mut r, 0), sync("direct", None, 0));
    r.disconnected();
    assert!(r.playlist_unavailable());
    assert_eq!(
        initial(&mut r, 0),
        None,
        "missing direct ID must not reconnect-loop"
    );
    assert_eq!(
        watch(&mut r, "direct"),
        None,
        "unchanged selection is not an explicit retry"
    );
    r.watch(None);
    assert_eq!(watch(&mut r, "direct"), sync("direct", None, 0));
    let p = raw_summary("direct", 1, 1);
    assert_eq!(
        part(&mut r, p, "v", 0, vec![raw_entry("e", 0)], false),
        None
    );
    assert_eq!(row_ids(&r), ["e"]);
    assert!(!r.playlist_unavailable());
    send(&mut r, json!({"change":{"deleted":"direct"}}));
    assert!(r.playlist_unavailable());
    assert!(r.playlist().is_none());
}

#[test]
fn summary_cap_does_not_block_parts_or_new_deltas() {
    let mut r = Replica::default();
    let lists: Vec<_> = (0..MAX_PLAYLISTS)
        .map(|i| raw_summary(&format!("p{i:03}"), 0, 1))
        .collect();
    let snapshot = json!({"snapshot":{"playlists":lists,"recent":[]}});
    send(&mut r, snapshot.clone());
    assert_eq!(watch(&mut r, "z-outside"), sync("z-outside", None, 0));
    let p = raw_summary("z-outside", 1, 1);
    part(&mut r, p, "v", 0, vec![raw_entry("e", 0)], false);
    assert_eq!(row_ids(&r), ["e"]);
    assert_eq!(r.snapshot().unwrap().playlists.len(), MAX_PLAYLISTS);
    assert!(
        !r.snapshot()
            .unwrap()
            .playlists
            .iter()
            .any(|p| p.id == "z-outside")
    );
    let mut newer = raw_summary("new", 0, 1);
    newer["updatedAt"] = json!("2026-08-03T00:00:01Z");
    send(&mut r, json!({"change":{"playlist":newer}}));
    assert_eq!(r.snapshot().unwrap().playlists[0].id, "new");
    assert_eq!(r.snapshot().unwrap().playlists.len(), MAX_PLAYLISTS);
    assert_eq!(row_ids(&r), ["e"]);
    r.watch(None);
    assert_eq!(watch(&mut r, "z-outside"), sync("z-outside", None, 0));
    // The cached token cannot confirm an unlisted direct lookup by silence.
    assert_eq!(row_ids(&r), ["e"]);
    let p = raw_summary("z-outside", 2, 2);
    assert_eq!(
        part(&mut r, p, "v2", 0, vec![raw_entry("new", 0)], true),
        sync("z-outside", Some("v2"), 1)
    );
    r.disconnected();
    assert!(!r.playlist_unavailable());
    assert_eq!(send(&mut r, snapshot), sync("z-outside", None, 0));
    assert_eq!(row_ids(&r), ["e"]);
}
