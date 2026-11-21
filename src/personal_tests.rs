use super::*;
use serde_json::{Value, json};

#[path = "personal_direct_tests.rs"]
mod direct;

fn raw_summary(id: &str, count: usize, revision: u64) -> Value {
    json!({"id": id, "name":"My list", "description":"", "kind":"manual", "countState":"exact",
        "trackCount":count,"availableCount":count,"capabilities":{"editMetadata":true,"editEntries":true,
        "editDefinition":false,"play":true,"copyToManual":false,"delete":true,"addTracks":true},
        "revision":revision,"createdAt":"2026-08-03T00:00:00Z","updatedAt":"2026-08-03T00:00:00Z"})
}

fn raw_derived_summary(id: &str, count: usize, revision: u64) -> Value {
    let mut p = raw_summary(id, count, revision);
    p["kind"] = json!("derived");
    p["evaluationState"] = json!("ready");
    p
}

fn raw_definition() -> Value {
    json!({"version":1,"sources":[{"kind":"allSaved","slotId":"source"}],"duplicates":"keep","sort":"sourceOrder"})
}

fn raw_track() -> Value {
    json!({"kind":"track","id":"song","name":"Song","artists":[{"id":"","name":"Credit"}],
        "hasAudio":true,"coverage":{"tracks":1,"inLibrary":1,"albums":1}})
}

fn raw_entry(id: &str, position: u64) -> Value {
    json!({"id":id,"position":position,"track":raw_track()})
}

fn summary(id: &str, count: usize, revision: u64) -> PlaylistSummary {
    serde_json::from_value(raw_summary(id, count, revision)).unwrap()
}

fn entry(id: &str, position: u64) -> ManualEntry {
    serde_json::from_value::<WireManualEntry>(raw_entry(id, position))
        .unwrap()
        .project()
        .unwrap()
}

fn send(r: &mut Replica, update: Value) -> Option<Command> {
    r.receive(serde_json::from_value(update).expect("valid Go wire payload"))
        .expect("valid personal update")
}

fn watch(r: &mut Replica, id: &str) -> Option<Command> {
    r.watch(Some(id.into()))
}

fn initial(r: &mut Replica, count: usize) -> Option<Command> {
    send(
        r,
        json!({"snapshot": {"playlists":[raw_summary("p", count, 1)],"recent":[]}}),
    )
}

fn sync(id: &str, known: Option<&str>, offset: usize) -> Option<Command> {
    Some(Command::SyncPlaylist {
        playlist_id: id.into(),
        known: known.map(str::to_owned),
        offset,
    })
}

fn part(
    r: &mut Replica,
    summary: Value,
    token: &str,
    offset: usize,
    rows: Vec<Value>,
    more: bool,
) -> Option<Command> {
    send(
        r,
        json!({"part":{"summary":summary,"token":token,"offset":offset,"rows":rows,"more":more}}),
    )
}

fn row_ids(r: &Replica) -> Vec<&str> {
    r.playlist()
        .unwrap()
        .rows
        .iter()
        .map(|row| row.id.as_str())
        .collect()
}

#[test]
fn initial_and_parts_are_atomic_and_direct() {
    let mut r = Replica::default();
    assert_eq!(watch(&mut r, "p"), None);
    assert!(r.snapshot().is_none());
    assert!(!r.ready());
    assert_eq!(initial(&mut r, 2), sync("p", None, 0));
    assert!(r.ready());
    assert_eq!(watch(&mut r, "p"), None);
    let p = raw_summary("p", 2, 1);
    let next = part(&mut r, p.clone(), "v1", 0, vec![raw_entry("e1", 2)], true);
    assert_eq!(next, sync("p", Some("v1"), 1));
    assert!(r.accepts_sync(next.as_ref().unwrap()));
    assert!(!r.accepts_sync(sync("p", None, 0).as_ref().unwrap()));
    assert!(r.playlist().is_none());
    assert_eq!(
        part(&mut r, p, "v1", 1, vec![raw_entry("e2", 5)], false),
        None
    );
    assert_eq!(row_ids(&r), ["e1", "e2"]);
    assert_eq!(r.playlist().unwrap().rows[1].track.id, "song");
    assert!(!r.accepts_sync(next.as_ref().unwrap()));
}

#[test]
fn a_changed_probe_keeps_cached_rows_visible_but_stale_until_the_last_part() {
    let mut r = Replica::default();
    initial(&mut r, 2);
    watch(&mut r, "p");
    let p = raw_summary("p", 2, 1);
    part(
        &mut r,
        p.clone(),
        "old",
        0,
        vec![raw_entry("e1", 2), raw_entry("e2", 5)],
        false,
    );
    r.watch(None);
    assert_eq!(watch(&mut r, "p"), sync("p", Some("old"), 0));
    assert!(!r.playlist().unwrap().stale);
    assert_eq!(
        part(
            &mut r,
            p.clone(),
            "new",
            0,
            vec![raw_entry("fresh1", 2)],
            true
        ),
        sync("p", Some("new"), 1)
    );
    assert!(r.playlist().unwrap().stale);
    assert_eq!(row_ids(&r), ["e1", "e2"]);
    part(&mut r, p, "new", 1, vec![raw_entry("fresh2", 5)], false);
    assert!(!r.playlist().unwrap().stale);
    assert_eq!(row_ids(&r), ["fresh1", "fresh2"]);
}

#[test]
fn go_can_restart_at_zero_for_a_new_token() {
    let mut r = Replica::default();
    initial(&mut r, 2);
    watch(&mut r, "p");
    part(
        &mut r,
        raw_summary("p", 2, 1),
        "old",
        0,
        vec![raw_entry("old1", 0)],
        true,
    );
    let p = raw_summary("p", 2, 2);
    assert_eq!(
        part(
            &mut r,
            p.clone(),
            "new",
            0,
            vec![raw_entry("new1", 0)],
            true
        ),
        sync("p", Some("new"), 1)
    );
    assert!(r.playlist().is_none());
    part(&mut r, p, "new", 1, vec![raw_entry("new2", 1)], false);
    assert_eq!(row_ids(&r), ["new1", "new2"]);
    assert_eq!(r.snapshot().unwrap().playlists[0].revision, 2);
}

#[test]
fn invalidation_discards_stage_keeps_last_good_and_restarts_once() {
    let mut r = Replica::default();
    watch(&mut r, "p");
    initial(&mut r, 1);
    let p = raw_summary("p", 1, 1);
    part(
        &mut r,
        p.clone(),
        "old",
        0,
        vec![raw_entry("last-good", 0)],
        false,
    );
    let q = raw_summary("p", 2, 2);
    assert_eq!(
        send(&mut r, json!({"change":{"playlist":q,"rowsInvalid":true}})),
        sync("p", None, 0)
    );
    assert_eq!(row_ids(&r), ["last-good"]);
    assert!(r.playlist().unwrap().stale);
    let queued = part(
        &mut r,
        q.clone(),
        "pending",
        0,
        vec![raw_entry("uncommitted", 0)],
        true,
    )
    .unwrap();
    assert_eq!(
        send(&mut r, json!({"change":{"playlist":q,"rowsInvalid":true}})),
        sync("p", None, 0)
    );
    assert!(!r.accepts_sync(&queued));
    assert_eq!(
        part(
            &mut r,
            q.clone(),
            "pending",
            1,
            vec![raw_entry("discard", 1)],
            false
        ),
        None
    );
    assert_eq!(row_ids(&r), ["last-good"]);
    part(
        &mut r,
        q,
        "fresh",
        0,
        vec![raw_entry("fresh1", 0), raw_entry("fresh2", 1)],
        false,
    );
    assert_eq!(row_ids(&r), ["fresh1", "fresh2"]);
    assert!(!r.playlist().unwrap().stale);
}

#[test]
fn malformed_part_gets_at_most_one_repair_and_never_publishes() {
    let mut r = Replica::default();
    initial(&mut r, 2);
    watch(&mut r, "p");
    part(
        &mut r,
        raw_summary("p", 2, 1),
        "v",
        0,
        vec![raw_entry("dup", 0)],
        true,
    );
    r.staged.as_mut().unwrap().bytes = DETAIL_CACHE_BYTES;
    assert_eq!(
        part(
            &mut r,
            raw_summary("p", 2, 1),
            "v",
            1,
            vec![raw_entry("dup", 1)],
            false
        ),
        sync("p", None, 0)
    );
    assert!(r.playlist().is_none());
    assert_eq!(
        r.receive(Update::Part(Part {
            summary: summary("p", 2, 1),
            definition: None,
            token: "v".into(),
            offset: 0,
            rows: vec![entry("incomplete", 1)],
            more: false
        })),
        Err(PersonalError)
    );
    assert!(r.playlist().is_none());
}

#[test]
fn manual_nonterminal_parts_cannot_reach_or_exceed_declared_total() {
    for rows in [
        vec![raw_entry("e2", 1)],
        vec![raw_entry("e2", 1), raw_entry("e3", 2)],
    ] {
        let mut r = Replica::default();
        initial(&mut r, 2);
        watch(&mut r, "p");
        let p = raw_summary("p", 2, 1);
        let next = part(&mut r, p.clone(), "v", 0, vec![raw_entry("e1", 0)], true).unwrap();
        let repair = part(&mut r, p, "v", 1, rows, true);
        assert_eq!(repair, sync("p", None, 0));
        assert!(!r.accepts_sync(&next));
        assert!(r.staged.is_none());
        assert!(r.playlist().is_none());
        assert_eq!(
            r.receive(Update::Part(Part {
                summary: summary("p", 2, 1),
                definition: None,
                token: "v".into(),
                offset: 0,
                rows: vec![entry("e1", 0), entry("e2", 1)],
                more: true,
            })),
            Err(PersonalError)
        );
    }
}

#[test]
fn empty_terminal_parts_work_for_empty_manual_and_derived_count_drift() {
    let mut r = Replica::default();
    initial(&mut r, 0);
    watch(&mut r, "p");
    assert_eq!(
        part(&mut r, raw_summary("p", 0, 1), "v", 0, vec![], false),
        None
    );
    assert!(r.playlist().unwrap().rows.is_empty());

    watch(&mut r, "d");
    let mut p = raw_derived_summary("d", 0, 1);
    p["countState"] = json!("stale");
    let definition = raw_definition();
    assert_eq!(
        send(
            &mut r,
            json!({"part":{"summary":p,"definition":definition,
            "token":"v","offset":0,"rows":[raw_entry("o1_row", 0)],"more":true}})
        ),
        sync("d", Some("v"), 1)
    );
    assert_eq!(
        send(
            &mut r,
            json!({"part":{"summary":p,"definition":definition,
            "token":"v","offset":1,"rows":[],"more":false}})
        ),
        None
    );
    assert_eq!(row_ids(&r), ["o1_row"]);
}

#[test]
fn large_manual_and_derived_parts_install_all_rows_atomically() {
    const TOTAL: usize = 1301;
    const PART_ROWS: usize = 64;
    for derived in [false, true] {
        let mut r = Replica::default();
        let (p, definition) = if derived {
            (raw_derived_summary("p", TOTAL, 1), raw_definition())
        } else {
            (raw_summary("p", TOTAL, 1), Value::Null)
        };
        send(&mut r, json!({"snapshot":{"playlists":[p],"recent":[]}}));
        assert_eq!(watch(&mut r, "p"), sync("p", None, 0));
        let id = |i| format!("{}{i}", if derived { "o1_" } else { "e" });
        for offset in (0..TOTAL).step_by(PART_ROWS) {
            let end = (offset + PART_ROWS).min(TOTAL);
            let rows: Vec<_> = (offset..end)
                .map(|i| raw_entry(&id(i), 2 * i as u64))
                .collect();
            let more = end < TOTAL;
            let next = send(
                &mut r,
                json!({"part":{"summary":p,"definition":definition,
                "token":"v","offset":offset,"rows":rows,"more":more}}),
            );
            if more {
                assert_eq!(next, sync("p", Some("v"), end));
                let next = next.unwrap();
                assert!(next.valid());
                assert!(r.accepts_sync(&next));
                assert!(r.playlist().is_none());
            } else {
                assert_eq!(next, None);
            }
        }
        let detail = r.playlist().unwrap();
        assert_eq!(detail.rows.len(), TOTAL);
        assert_eq!(detail.definition.is_some(), derived);
        for (i, row) in detail.rows.iter().enumerate() {
            assert_eq!(row.id, id(i));
            assert_eq!(row.position, 2 * i as u64);
        }
    }
}

#[test]
fn continuation_offset_is_not_bounded_by_cache_budget() {
    let offset = DETAIL_CACHE_BYTES / size_of::<ManualEntry>() + 1;
    assert!(sync("p", Some("v"), offset).unwrap().valid());
    assert!(!sync("p", None, offset).unwrap().valid());
    assert!(!sync("p", Some("invalid token"), offset).unwrap().valid());
    assert!(!sync("", Some("v"), offset).unwrap().valid());
}

#[test]
fn oversized_multipart_remains_complete_while_watched_and_evicts_inactive_details() {
    let mut r = Replica::default();
    let p = raw_summary("p", 3, 1);
    let old = raw_summary("old", 1, 1);
    send(
        &mut r,
        json!({"snapshot":{"playlists":[p,old],"recent":[]}}),
    );
    watch(&mut r, "old");
    part(&mut r, old, "old", 0, vec![raw_entry("old", 0)], false);
    watch(&mut r, "p");
    part(&mut r, p.clone(), "v", 0, vec![raw_entry("e1", 0)], true);
    // Simulate an expensive first part without allocating it in the test.
    r.staged.as_mut().unwrap().bytes = DETAIL_CACHE_BYTES;
    assert_eq!(
        part(&mut r, p.clone(), "v", 1, vec![raw_entry("e2", 1)], true),
        sync("p", Some("v"), 2)
    );
    assert!(r.playlist().is_none());
    assert_eq!(
        part(&mut r, p, "v", 2, vec![raw_entry("e3", 2)], false),
        None
    );
    assert_eq!(row_ids(&r), ["e1", "e2", "e3"]);
    assert!(!r.playlist().unwrap().stale);
    assert!(r.cached("old").is_none());
    assert!(r.cached("p").unwrap().bytes > DETAIL_CACHE_BYTES);
    assert_eq!(
        send(
            &mut r,
            json!({"change":{"playlist":raw_summary("p", 3, 2),"rowsInvalid":true}})
        ),
        sync("p", None, 0)
    );
    assert_eq!(row_ids(&r), ["e1", "e2", "e3"]);
    assert!(r.playlist().unwrap().stale);
    r.watch(None);
    assert!(r.cached("p").is_none());
}

#[test]
fn live_add_evicts_inactive_details_instead_of_triggering_repair() {
    let mut r = Replica::default();
    let p = raw_summary("p", 1, 1);
    let old = raw_summary("old", 1, 1);
    send(
        &mut r,
        json!({"snapshot":{"playlists":[p,old],"recent":[]}}),
    );
    watch(&mut r, "old");
    part(&mut r, old, "old", 0, vec![raw_entry("old", 0)], false);
    watch(&mut r, "p");
    part(&mut r, p, "v", 0, vec![raw_entry("e1", 0)], false);
    let old_i = r.cache_index("old").unwrap();
    r.details[old_i].bytes = DETAIL_CACHE_BYTES - r.cached("p").unwrap().bytes;
    assert_eq!(
        send(
            &mut r,
            json!({"change":{"playlist":raw_summary("p", 2, 2),"entries":{"added":raw_entry("e2", 1)}}})
        ),
        None
    );
    assert!(r.cached("old").is_none());
    assert_eq!(row_ids(&r), ["e1", "e2"]);
    assert!(!r.playlist().unwrap().stale);
}

#[test]
fn row_byte_accounting_saturates_without_rejecting_live_updates() {
    let mut r = Replica::default();
    let p = raw_summary("p", 1, 1);
    let old = raw_summary("old", 1, 1);
    send(
        &mut r,
        json!({"snapshot":{"playlists":[p,old],"recent":[]}}),
    );
    watch(&mut r, "old");
    part(&mut r, old, "old", 0, vec![raw_entry("old", 0)], false);
    watch(&mut r, "p");
    part(&mut r, p, "v", 0, vec![raw_entry("e1", 0)], false);
    let p_i = r.cache_index("p").unwrap();
    r.details[p_i].bytes = usize::MAX;
    assert_eq!(
        send(
            &mut r,
            json!({"change":{"playlist":raw_summary("p", 2, 2),"entries":{"added":raw_entry("e2", 1)}}})
        ),
        None
    );
    assert!(r.cached("old").is_none());
    assert_eq!(r.cached("p").unwrap().bytes, usize::MAX);
    assert_eq!(row_ids(&r), ["e1", "e2"]);
    r.details[0].bytes = 0;
    assert_eq!(
        send(
            &mut r,
            json!({"change":{"playlist":raw_summary("p", 1, 3),"entries":{"removed":"e1"}}})
        ),
        None
    );
    assert_eq!(r.details[0].bytes, 0);
    assert_eq!(row_ids(&r), ["e2"]);
    assert!(!r.playlist().unwrap().stale);
}

#[test]
fn navigation_retains_bounded_details_and_ignores_unwatched_parts() {
    let mut r = Replica::default();
    let lists: Vec<_> = (0..=MAX_DETAILS)
        .map(|i| raw_summary(&format!("p{i}"), 1, 1))
        .collect();
    send(&mut r, json!({"snapshot":{"playlists":lists,"recent":[]}}));
    for i in 0..=MAX_DETAILS {
        let id = format!("p{i}");
        assert_eq!(watch(&mut r, &id), sync(&id, None, 0));
        part(
            &mut r,
            lists[i].clone(),
            "v",
            0,
            vec![raw_entry(&format!("e{i}"), 0)],
            false,
        );
    }
    assert!(r.cached("p0").is_none());
    assert_eq!(r.details.len(), MAX_DETAILS);
    assert_eq!(watch(&mut r, "p1"), sync("p1", Some("v"), 0));
    assert_eq!(row_ids(&r), ["e1"]);
    let request = watch(&mut r, "p0").unwrap();
    assert_eq!(watch(&mut r, "p2"), sync("p2", Some("v"), 0));
    assert!(!r.accepts_sync(&request));
    assert_eq!(
        part(
            &mut r,
            lists[0].clone(),
            "later",
            0,
            vec![raw_entry("unwatched", 0)],
            false
        ),
        None
    );
    assert_eq!(row_ids(&r), ["e2"]);
    assert!(r.cached("p0").is_none());
}

#[test]
fn disconnect_and_repair_snapshot_keep_rows_but_force_unknown_token() {
    let mut r = Replica::default();
    watch(&mut r, "p");
    initial(&mut r, 1);
    part(
        &mut r,
        raw_summary("p", 1, 1),
        "v1",
        0,
        vec![raw_entry("old", 0)],
        false,
    );
    r.disconnected();
    assert!(!r.ready());
    assert!(r.snapshot().is_some());
    assert!(r.playlist().unwrap().stale);
    assert_eq!(row_ids(&r), ["old"]);
    assert_eq!(send(&mut r, json!({"change":{"deleted":"p"}})), None);
    assert_eq!(r.command(Command::ClearHistory), None);
    assert_eq!(initial(&mut r, 1), sync("p", None, 0));
    assert_eq!(row_ids(&r), ["old"]);
    part(
        &mut r,
        raw_summary("p", 1, 1),
        "v2",
        0,
        vec![raw_entry("new", 0)],
        false,
    );
    assert_eq!(initial(&mut r, 1), sync("p", None, 0));
    assert_eq!(row_ids(&r), ["new"]);
    send(&mut r, json!({"snapshot":{"playlists":[],"recent":[]}}));
    assert!(r.playlist().is_none());
    assert!(r.details.is_empty());
    r.clear();
    assert!(r.snapshot().is_none());
    assert!(r.desired.is_none());
}

#[test]
fn live_edits_address_occurrences_not_track_ids_and_preserve_slot_order() {
    let mut r = Replica::default();
    initial(&mut r, 1);
    watch(&mut r, "p");
    part(
        &mut r,
        raw_summary("p", 1, 1),
        "v",
        0,
        vec![raw_entry("e1", 2)],
        false,
    );
    let command = Command::AddEntry {
        playlist_id: "p".into(),
        track_id: "song".into(),
    };
    assert_eq!(r.command(command.clone()), Some(command));
    assert_eq!(row_ids(&r), ["e1"], "admission is not mutation");
    assert_eq!(
        send(
            &mut r,
            json!({"change":{"playlist":raw_summary("p", 2, 2),"entries":{"added":raw_entry("e2", 7)}}})
        ),
        None
    );
    assert_eq!(row_ids(&r), ["e1", "e2"]);
    assert_eq!(
        send(
            &mut r,
            json!({"change":{"playlist":raw_summary("p", 2, 3),"entries":{"moved":"e2"}}})
        ),
        None
    );
    assert_eq!(row_ids(&r), ["e2", "e1"]);
    assert_eq!(r.playlist().unwrap().rows[0].position, 2);
    assert_eq!(r.playlist().unwrap().rows[1].position, 7);
    assert_eq!(
        send(
            &mut r,
            json!({"change":{"playlist":raw_summary("p", 1, 4),"entries":{"removed":"e1"}}})
        ),
        None
    );
    assert_eq!(row_ids(&r), ["e2"]);
    assert_eq!(r.playlist().unwrap().token, "");
    assert_eq!(
        send(
            &mut r,
            json!({"change":{"playlist":raw_summary("p", 0, 5),"entries":{"removed":"missing"}}})
        ),
        sync("p", None, 0)
    );
    assert_eq!(row_ids(&r), ["e2"]);
    assert!(r.playlist().unwrap().stale);
    send(&mut r, json!({"change":{"deleted":"p"}}));
    assert!(r.playlist().is_none());
}

#[test]
fn recent_uses_origin_and_final_indices_and_rejects_whole_bad_delta() {
    let mut r = Replica::default();
    let item = |id, origin| json!({"id":id,"playedAt":"2026-08-03T00:00:00Z","track":raw_track(),"origin":origin});
    send(
        &mut r,
        json!({"snapshot":{"playlists":[raw_summary("p",0,1)],"recent":[item("event","online"),item("event","offline"),item("ev2","online")]}}),
    );
    send(
        &mut r,
        json!({"change":{"recent":{"removed":[{"id":"event","origin":"online"}],"inserted":[{"index":0,"item":item("new","online")},{"index":2,"item":item("third","online")}]}}}),
    );
    let before = r.snapshot().unwrap().clone();
    assert_eq!(
        before.recent.iter().map(recent_key).collect::<Vec<_>>(),
        [
            ("new", "online"),
            ("event", "offline"),
            ("third", "online"),
            ("ev2", "online")
        ]
    );
    let bad: Update = serde_json::from_value(json!({"change":{"deleted":"p","recent":{"inserted":[{"index":99,"item":item("bad","online")}]}}})).unwrap();
    assert_eq!(r.receive(bad), Err(PersonalError));
    assert_eq!(r.snapshot(), Some(&before));
    assert!(r.command(Command::ClearHistory).is_some());
    assert_eq!(r.snapshot(), Some(&before));
}

#[test]
fn recent_accepts_bounded_eviction_and_only_committed_clear() {
    let mut r = Replica::default();
    let item = |id: String| json!({"id":id,"playedAt":"2026-08-03T00:00:00Z","track":raw_track(),"origin":"online"});
    let recent: Vec<_> = (0..MAX_RECENT).map(|i| item(format!("ev{i}"))).collect();
    send(&mut r, json!({"snapshot":{"playlists":[],"recent":recent}}));
    let invalid: Update = serde_json::from_value(
        json!({"change":{"recent":{"inserted":[{"index":0,"item":item("new".into())}]}}}),
    )
    .unwrap();
    assert_eq!(r.receive(invalid), Err(PersonalError));
    send(
        &mut r,
        json!({"change":{"recent":{"removed":[{"id":format!("ev{}", MAX_RECENT - 1),"origin":"online"}],"inserted":[{"index":0,"item":item("new".into())}]}}}),
    );
    let recent = &r.snapshot().unwrap().recent;
    assert_eq!(recent.len(), MAX_RECENT);
    assert_eq!(recent[0].id, "new");
    let removed: Vec<_> = recent
        .iter()
        .map(|r| json!({"id":r.id,"origin":r.origin}))
        .collect();
    assert_eq!(
        r.command(Command::ClearHistory),
        Some(Command::ClearHistory)
    );
    assert_eq!(r.snapshot().unwrap().recent.len(), MAX_RECENT);
    send(&mut r, json!({"change":{"recent":{"removed":removed}}}));
    assert!(r.snapshot().unwrap().recent.is_empty());
}

#[test]
fn wire_fields_are_exact_and_derived_ids_are_not_manual_ids() {
    assert_eq!(
        serde_json::to_value(sync("p", None, 0).unwrap()).unwrap(),
        json!({"type":"syncPlaylist","playlistId":"p"})
    );
    assert_eq!(
        serde_json::to_value(sync("p", Some("v"), 4).unwrap()).unwrap(),
        json!({"type":"syncPlaylist","playlistId":"p","known":"v","offset":4})
    );
    let command = Command::MoveEntry {
        playlist_id: "p".into(),
        entry_id: "e".into(),
        after: String::new(),
    };
    assert_eq!(
        serde_json::to_value(command).unwrap(),
        json!({"type":"moveEntry","playlistId":"p","entryId":"e","after":""})
    );
    assert!(
        serde_json::from_value::<Update>(
            json!({"snapshot":{"playlists":[],"recent":[]},"change":{}})
        )
        .is_err()
    );
    assert!(serde_json::from_value::<Update>(json!({})).is_err());
    let p = raw_derived_summary("d", 1, 1);
    let id = format!("o1_{}", "a".repeat(1000));
    let definition = raw_definition();
    let mut r = Replica::default();
    send(&mut r, json!({"snapshot":{"playlists":[p],"recent":[]}}));
    watch(&mut r, "d");
    send(
        &mut r,
        json!({"part":{"summary":p,"definition":definition,"token":"v","offset":0,"rows":[raw_entry(&id, 0)],"more":false}}),
    );
    assert_eq!(row_ids(&r), [id.as_str()]);
    assert!(r.playlist().unwrap().definition.is_some());
}

#[test]
fn sorting_handles_whole_second_and_fractional_go_timestamps() {
    let mut a = raw_summary("a", 0, 1);
    a["updatedAt"] = json!("2026-08-03T00:00:00.1Z");
    let b = raw_summary("b", 0, 1);
    let mut r = Replica::default();
    send(&mut r, json!({"snapshot":{"playlists":[b,a],"recent":[]}}));
    assert_eq!(r.snapshot().unwrap().playlists[0].id, "a");
}
