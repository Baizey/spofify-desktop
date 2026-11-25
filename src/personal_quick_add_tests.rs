use super::*;
use crate::quick_add::{Membership, Outcome};
use serde_json::{Value, json};
use std::time::Duration;

const EPOCH: u64 = 7;
const GENERATION: u64 = 11;

fn summary(id: &str, count: usize, revision: u64) -> Value {
    json!({"id":id, "name":id, "description":"", "kind":"manual", "countState":"exact",
        "trackCount":count, "availableCount":count, "revision":revision,
        "createdAt":"2026-08-03T00:00:00Z", "updatedAt":"2026-08-03T00:00:00Z",
        "capabilities":{"editMetadata":true,"editEntries":true,"editDefinition":false,
            "play":true,"copyToManual":false,"delete":true,"addTracks":true}})
}

fn entry(id: &str, track: &str, pos: usize) -> Value {
    json!({"id":id,"position":pos,"track":{"kind":"track","id":track,"name":"Song",
        "artists":[],"hasAudio":true,"coverage":{"tracks":1,"inLibrary":1,"albums":1}}})
}

fn receive(hub: &PersonalHub, generation: u64, update: Value) -> Option<PersonalCommand> {
    hub.receive(EPOCH, generation, serde_json::from_value(update).unwrap())
        .unwrap()
}

fn lists(hub: &PersonalHub, generation: u64, summaries: Vec<Value>) -> Option<PersonalCommand> {
    receive(
        hub,
        generation,
        json!({"snapshot":{"playlists":summaries,"recent":[]}}),
    )
}

fn part(
    hub: &PersonalHub,
    generation: u64,
    s: Value,
    token: &str,
    offset: usize,
    rows: Vec<Value>,
    more: bool,
) -> Option<PersonalCommand> {
    receive(
        hub,
        generation,
        json!({"part":{"summary":s,"token":token,"offset":offset,"rows":rows,"more":more}}),
    )
}

fn ready() -> (PersonalHub, PersonalAccess) {
    let hub = PersonalHub::default();
    let access = hub.access();
    hub.begin(EPOCH, GENERATION);
    lists(
        &hub,
        GENERATION,
        vec![summary("a", 0, 1), summary("b", 0, 1)],
    );
    assert!(access.quick_watch(EPOCH, true, None).is_none());
    (hub, access)
}

fn empty_a(hub: &PersonalHub, access: &PersonalAccess) {
    assert!(access.quick_select(EPOCH, Some("a")).is_some());
    part(
        hub,
        GENERATION,
        summary("a", 0, 1),
        "empty",
        0,
        vec![],
        false,
    );
}

fn stage(access: &PersonalAccess, id: &str, track: &str) -> Ticket {
    access
        .quick_stage(EPOCH, 2, id, track)
        .expect("test write can stage")
}

fn dispatch(hub: &PersonalHub, access: &PersonalAccess, ticket: &Ticket) {
    assert!(hub.admit_quick_add(ticket, || true));
    assert!(access.start_quick_add(ticket, || true));
}

fn begin_bulk(access: &PersonalAccess, target: &str, kind: &str) -> Option<BulkTicket> {
    let scope = access.quick_view(EPOCH, |v| v.bulk_scope(2));
    access.quick_begin_bulk(EPOCH, 2, &scope, target, kind, "source")
}

#[test]
fn bulk_add_uses_scoped_absence_off_adds_one_even_when_present() {
    let (hub, a) = ready();
    a.quick_select(EPOCH, Some("a"));
    let current = summary("a", 2, 2);
    let existing = entry("exists", "song", 0);
    part(
        &hub,
        GENERATION,
        current.clone(),
        "full",
        0,
        vec![existing.clone()],
        true,
    );
    assert!(begin_bulk(&a, "a", "album").is_none());
    part(
        &hub,
        GENERATION,
        current,
        "full",
        1,
        vec![entry("last", "tail", 1)],
        false,
    );
    let skip = begin_bulk(&a, "a", "album").unwrap();
    assert_eq!(
        a.quick_bulk_send(&skip, "song", || panic!("already present")),
        BulkWrite::Skipped
    );
    assert!(begin_bulk(&a, "a", "artist").is_none());
    assert!(a.quick_stage(EPOCH, 2, "a", "missing").is_none());
    a.quick_skip(EPOCH, false);
    assert_eq!(
        a.quick_bulk_send(&skip, "tail", || panic!("settings changed operation")),
        BulkWrite::Skipped
    );
    a.quick_finish_bulk(&skip, BulkEnd::Done);
    a.quick_view(EPOCH, |v| {
        assert_eq!(v.bulk.unwrap().message, "2 already in playlist")
    });
    let allow = begin_bulk(&a, "a", "artist").unwrap();
    assert_eq!(
        a.quick_bulk_send(&allow, "song", || true),
        BulkWrite::Started
    );
    a.quick_view(EPOCH, |v| {
        let row = v.row("song");
        assert_eq!(row.membership, Membership::Present);
        assert!(!row.pending && !row.can_add);
    });
    assert!(!a.quick_bulk_ack(&allow, 2)); // Cannot be the post-write marker.
    assert!(a.quick_bulk_ack(&allow, 3));
    receive(
        &hub,
        GENERATION,
        json!({"change": {
            "playlist": summary("a", 3, 3),
            "entries": {"added": entry("added", "song", 2)}
        }}),
    );
    a.quick_bulk_checkpoint(&allow);
    a.quick_finish_bulk(&allow, BulkEnd::Done);
    a.quick_select(EPOCH, Some("b"));
    a.quick_view(EPOCH, |v| assert!(v.skip));
}

#[test]
fn bulk_add_shared_membership_never_releases_window_or_resends_started_tracks() {
    use crate::quick_add::BULK_WINDOW;
    let (hub, a) = ready();
    empty_a(&hub, &a);
    let ticket = begin_bulk(&a, "a", "artist").unwrap();
    for i in 0..BULK_WINDOW {
        let track = format!("song-{i}");
        assert_eq!(
            a.quick_bulk_send(&ticket, &track, || true),
            BulkWrite::Started
        );
    }
    receive(
        &hub,
        GENERATION,
        json!({"change": {
            "playlist": summary("a", 1, 2),
            "entries": {"added": entry("foreign", "song-0", 0)}
        }}),
    );
    assert_eq!(
        a.quick_bulk_send(&ticket, "new", || panic!(
            "window drained by shared broadcast"
        )),
        BulkWrite::Stop
    );
    assert!(a.quick_stage(EPOCH, 2, "a", "song-0").is_none());
    a.quick_abort_bulk(EPOCH);
    assert!(!a.quick_bulk_current(&ticket));
    assert!(begin_bulk(&a, "a", "album").is_none());
    a.quick_view(EPOCH, |v| {
        assert_eq!(v.row("song-0").membership, Membership::Present);
        assert_eq!(v.row("song-1").outcome, Some(Outcome::Unconfirmed));
        assert!(v.row("unrelated").can_add);
    });
    assert_eq!(
        a.quick_bulk_send(&ticket, "new", || panic!("cancelled write")),
        BulkWrite::Stop
    );
}

#[test]
fn bulk_add_stale_launch_cancel_and_old_account_completion_cannot_touch_the_next_bulk() {
    let (hub, a) = ready();
    empty_a(&hub, &a);
    let before = a.quick_view(EPOCH, |v| v.bulk_scope(2));
    let first = a
        .quick_begin_bulk(EPOCH, 2, &before, "a", "album", "some-source")
        .unwrap();
    let during = a.quick_view(EPOCH, |v| v.bulk_scope(2));
    a.quick_abort_bulk_scoped(EPOCH, 2, &before);
    assert!(a.quick_bulk_current(&first));
    a.quick_abort_bulk_scoped(EPOCH, 2, &during);
    assert!(!a.quick_bulk_current(&first));
    assert!(
        a.quick_begin_bulk(EPOCH, 2, &before, "a", "album", "some-source")
            .is_none()
    );

    let epoch = EPOCH + 1;
    hub.begin(epoch, GENERATION + 1);
    let read = |raw| {
        hub.receive(epoch, GENERATION + 1, serde_json::from_value(raw).unwrap())
            .unwrap()
    };
    read(json!({"snapshot": {"playlists": [summary("a", 0, 1)], "recent": []}}));
    a.quick_watch(epoch, true, None);
    a.quick_select(epoch, Some("a"));
    read(
        json!({"part": {"summary": summary("a", 0, 1), "token": "again", "offset": 0, "rows": [], "more": false}}),
    );
    let scope = a.quick_view(epoch, |v| v.bulk_scope(2));
    let current = a
        .quick_begin_bulk(epoch, 2, &scope, "a", "album", "some-source")
        .unwrap();
    a.quick_finish_bulk(&first, BulkEnd::Done);
    assert!(a.quick_bulk_current(&current));
}

#[test]
fn bulk_add_published_account_epoch_fences_the_wire_before_hub_retirement() {
    let (hub, a) = ready();
    empty_a(&hub, &a);
    let ticket = begin_bulk(&a, "a", "album").unwrap();
    let epoch = AtomicU64::new(EPOCH);
    {
        let _publication = crate::playlist_pin_store::PUBLICATION_GATE.lock().unwrap();
        epoch.store(EPOCH + 1, Ordering::SeqCst);
    }
    assert!(
        a.quick_bulk_current(&ticket),
        "Engine has not retired the old hub"
    );
    let _publication = crate::playlist_pin_store::PUBLICATION_GATE.lock().unwrap();
    assert_eq!(
        a.quick_bulk_send_ordered(&ticket, "song", None, &epoch, || {
            panic!("obsolete account reached start_send")
        }),
        BulkWrite::Stop
    );
    assert!(
        !a.quick_bulk_ack(&ticket, 2),
        "no ambiguous write was started"
    );
}

fn elapse(access: &PersonalAccess, seconds: u64) {
    access
        .state
        .lock()
        .unwrap()
        .quick_tick(Instant::now() + Duration::from_secs(seconds));
}

#[test]
fn quick_add_partial_large_membership_is_exact_boolean_not_committed_rows() {
    let (hub, a) = ready();
    let total = 12_000;
    let s = summary("a", total, 2);
    assert!(a.quick_select(EPOCH, Some("a")).is_some());
    for offset in (0..total).step_by(500) {
        let end = offset + 500;
        let next = part(
            &hub,
            GENERATION,
            s.clone(),
            "large",
            offset,
            (offset..end)
                .map(|i| {
                    entry(
                        &format!("e{i}"),
                        if i % 2 == 0 { "duplicate" } else { "other" },
                        i,
                    )
                })
                .collect(),
            end < total,
        );
        if end < total {
            assert!(
                matches!(next, Some(PersonalCommand::SyncPlaylist {offset, ..}) if offset == end)
            );
        }
        a.quick_view(EPOCH, |v| {
            let known = v.row("duplicate");
            assert_eq!(known.membership, Membership::Present);
            assert!(!known.can_add);
            let missing = v.row("missing");
            assert_eq!(
                missing.membership,
                if end < total {
                    Membership::Unknown
                } else {
                    Membership::Absent
                }
            );
            assert_eq!(missing.can_add, end == total);
        });
        a.read(|_, r| {
            assert_eq!(
                r.playlist().map(|p| p.rows.len()),
                (end == total).then_some(total)
            )
        });
    }
    receive(
        &hub,
        GENERATION,
        json!({"change":{"playlist":summary("a", total-1, 3),"entries":{"removed":"e0"}}}),
    );
    a.quick_view(EPOCH, |v| {
        assert_eq!(v.row("duplicate").membership, Membership::Present)
    });
}

#[test]
fn quick_add_foreign_same_song_resolves_membership_but_other_facts_do_not() {
    let (hub, a) = ready();
    empty_a(&hub, &a);
    let t = stage(&a, "a", "song");
    dispatch(&hub, &a, &t);
    assert!(a.quick_stage(EPOCH, 2, "a", "song").is_none());
    receive(&hub, GENERATION, json!({"change":{}}));
    receive(
        &hub,
        GENERATION,
        json!({"change":{"playlist":summary("b", 1, 2),"entries":{"added":entry("foreign-b", "song", 0)}}}),
    );
    receive(
        &hub,
        GENERATION,
        json!({"change":{"playlist":summary("a", 0, 2)}}),
    );
    hub.refused(EPOCH, GENERATION); // Neither ordinary refusal nor a revision is this ticket.
    a.quick_view(EPOCH, |v| {
        let song = v.row("song");
        assert!(song.pending && !song.can_add);
        assert_eq!(song.membership, Membership::Absent);
        assert_eq!(song.outcome, None);
    });
    receive(
        &hub,
        GENERATION,
        json!({"change":{"playlist":summary("a", 2, 3),"rowsInvalid":true}}),
    );
    part(
        &hub,
        GENERATION,
        summary("a", 2, 3),
        "foreign",
        0,
        vec![entry("different-client", "song", 0)],
        true,
    );
    a.quick_view(EPOCH, |v| {
        let song = v.row("song");
        assert!(!song.pending && !song.can_add);
        assert_eq!(song.membership, Membership::Present);
        assert_eq!(song.outcome, None); // Plain membership; no success/receipt state exists.
    });
    a.read(|_, r| assert!(r.playlist().is_some_and(|p| p.stale && p.rows.is_empty())));
}

#[test]
fn quick_add_three_distinct_writes_start_and_observe_out_of_order() {
    let (hub, a) = ready();
    empty_a(&hub, &a);
    let first = stage(&a, "a", "first");
    dispatch(&hub, &a, &first);
    let second = stage(&a, "a", "second");
    let third = stage(&a, "a", "third");
    assert!(a.quick_stage(EPOCH, 2, "a", "third").is_none());
    assert!(hub.admit_quick_add(&second, || true));
    assert!(a.quick_stage(EPOCH, 2, "a", "second").is_none());
    assert!(hub.admit_quick_add(&third, || true));
    assert!(a.start_quick_add(&third, || true));
    assert!(a.start_quick_add(&second, || true));
    a.quick_view(EPOCH, |v| {
        for song in ["first", "second", "third"] {
            let row = v.row(song);
            assert_eq!(row.membership, Membership::Absent);
            assert!(row.pending && !row.can_add && row.outcome.is_none());
        }
        assert!(v.row("unused").can_add && !v.at_capacity);
    });
    for (index, song) in ["third", "second"].into_iter().enumerate() {
        receive(
            &hub,
            GENERATION,
            json!({"change": {"playlist": summary("a", index + 1, index as u64 + 2),
                "entries": {"added": entry(song, song, index)}}}),
        );
        a.quick_view(EPOCH, |v| {
            let row = v.row(song);
            assert_eq!(row.membership, Membership::Present);
            assert!(!row.pending && !row.can_add && row.outcome.is_none());
            assert!(v.row("first").pending);
            assert_eq!(v.row("second").pending, index == 0);
        });
    }
    elapse(&a, 31);
    a.quick_view(EPOCH, |v| {
        assert_eq!(v.row("first").outcome, Some(Outcome::Unconfirmed));
        assert!(v.row("second").outcome.is_none());
        assert!(v.row("third").outcome.is_none());
    });
    receive(
        &hub,
        GENERATION,
        json!({"change": {"playlist": summary("a", 3, 4),
            "entries": {"added": entry("foreign-first", "first", 2)}}}),
    );
    a.quick_view(EPOCH, |v| {
        let row = v.row("first");
        assert_eq!(row.membership, Membership::Present);
        assert!(!row.pending && !row.can_add && row.outcome.is_none());
    });
}

#[test]
fn quick_add_live_capacity_covers_all_phases_and_releases_one_slot_at_a_time() {
    let (hub, a) = ready();
    empty_a(&hub, &a);
    let tickets: Vec<_> = (0..8).map(|i| stage(&a, "a", &format!("s{i}"))).collect();
    dispatch(&hub, &a, &tickets[0]);
    assert!(hub.admit_quick_add(&tickets[1], || true));
    a.quick_view(EPOCH, |v| {
        assert!(v.at_capacity && !v.session_limit && !v.row("extra").can_add);
        for i in 0..8 {
            assert!(v.row(&format!("s{i}")).pending);
        }
    });
    assert!(a.quick_stage(EPOCH, 2, "a", "extra").is_none());
    assert!(a.quick_stage(EPOCH, 2, "a", "s0").is_none());
    assert!(a.quick_stage(EPOCH, 2, "a", "s1").is_none());
    assert!(a.quick_stage(EPOCH, 2, "a", "s2").is_none());
    assert!(!hub.admit_quick_add(&tickets[2], || false));
    a.quick_view(EPOCH, |v| {
        assert!(!v.at_capacity && v.row("extra").can_add);
        assert_eq!(v.row("s2").outcome, Some(Outcome::NotSent));
        assert!(v.row("s2").can_add && !v.row("s2").pending);
        assert!(v.row("s0").pending && v.row("s1").pending && v.row("s3").pending);
    });
    let replacement = stage(&a, "a", "extra");
    assert!(a.quick_stage(EPOCH, 2, "a", "s2").is_none());
    receive(
        &hub,
        GENERATION,
        json!({"change": {"playlist": summary("a", 1, 2),
            "entries": {"added": entry("foreign-queued", "s1", 0)}}}),
    );
    assert!(!a.start_quick_add(&tickets[1], || panic!("already present")));
    a.quick_view(EPOCH, |v| {
        assert!(!v.at_capacity && v.row("extra").pending);
        assert_eq!(v.row("s1").membership, Membership::Present);
        assert!(v.row("s1").outcome.is_none());
        assert_eq!(v.row("s2").outcome, Some(Outcome::NotSent));
    });
    let restaged = stage(&a, "a", "s2");
    assert!(!hub.admit_quick_add(&tickets[2], || panic!("stale ticket for restaged row")));
    a.quick_cancel(&tickets[2]);
    a.quick_view(EPOCH, |v| {
        assert!(v.at_capacity && v.row("s2").pending && v.row("s2").outcome.is_none());
    });
    assert!(hub.admit_quick_add(&restaged, || true));
    assert!(hub.admit_quick_add(&replacement, || true));
    assert!(a.start_quick_add(&replacement, || true));
}

#[test]
fn quick_add_observation_window_begins_at_actual_start_and_deadlines_are_independent() {
    let (hub, a) = ready();
    empty_a(&hub, &a);
    let mut state = a.state.lock().unwrap();
    let State {
        status,
        replica,
        quick,
        ..
    } = &mut *state;
    let status = *status;
    let now = Instant::now();
    let after = |s| now + Duration::from_secs(s);
    let early = quick.stage(status, replica, 2, "a", "early", now).unwrap();
    let late = quick
        .stage(status, replica, 2, "a", "late", after(1))
        .unwrap();
    assert!(quick.admit(status, replica, &early, now, || true));
    assert!(quick.admit(status, replica, &late, after(2), || true));
    assert!(quick.start(status, replica, &late, after(5), || true));
    assert_eq!(quick.view(true, replica, status).due, Some(after(13)));
    assert_eq!(
        quick.view(true, replica, status).row("early").outcome,
        Some(Outcome::NotSent)
    );
    assert!(!quick.start(status, replica, &early, after(5), || panic!("expired")));
    assert!(quick.take_probe_due(after(13)));
    assert_eq!(quick.view(true, replica, status).due, Some(after(35)));
    assert!(!quick.tick(after(34)));
    assert!(quick.view(true, replica, status).row("late").pending);
    assert!(!quick.tick(after(35)));
    let view = quick.view(true, replica, status);
    assert!(view.due.is_none());
    assert_eq!(view.row("late").outcome, Some(Outcome::Unconfirmed));
    assert_eq!(view.row("early").outcome, Some(Outcome::NotSent));
    assert!(view.row("early").can_add && !view.row("late").can_add);
}

#[test]
fn quick_add_dispatch_rechecks_exact_absence_during_multipart_repair() {
    let (hub, a) = ready();
    empty_a(&hub, &a);
    let started = stage(&a, "a", "started");
    dispatch(&hub, &a, &started);
    let queued = stage(&a, "a", "queued");
    assert!(hub.admit_quick_add(&queued, || true));
    let staged = stage(&a, "a", "staged");
    let updated = summary("a", 2, 2);
    receive(
        &hub,
        GENERATION,
        json!({"change":{"playlist":updated, "rowsInvalid":true}}),
    );
    part(
        &hub,
        GENERATION,
        updated.clone(),
        "repair",
        0,
        vec![entry("e0", "other", 0)],
        true,
    );
    a.quick_view(EPOCH, |v| {
        for song in ["started", "queued", "staged"] {
            let row = v.row(song);
            assert_eq!(row.membership, Membership::Unknown);
            assert!(row.pending && !row.can_add);
        }
    });
    assert!(!hub.admit_quick_add(&staged, || panic!("unknown is not absent at admission")));
    assert!(!a.start_quick_add(&queued, || panic!("unknown is not absent at socket start")));
    part(
        &hub,
        GENERATION,
        updated,
        "repair",
        1,
        vec![entry("e1", "another", 1)],
        false,
    );
    a.quick_view(EPOCH, |v| {
        assert!(v.row("started").pending);
        for song in ["queued", "staged"] {
            let row = v.row(song);
            assert_eq!(row.membership, Membership::Absent);
            assert_eq!(row.outcome, Some(Outcome::NotSent));
            assert!(row.can_add && !row.pending);
        }
    });
}

#[test]
fn quick_add_dropped_no_diff_and_reconnect_never_rearm_old_reader_attempt() {
    let (hub, a) = ready();
    empty_a(&hub, &a);
    let t = stage(&a, "a", "song");
    dispatch(&hub, &a, &t);
    elapse(&a, 29);
    a.quick_view(EPOCH, |v| assert!(v.row("song").pending));
    elapse(&a, 31);
    assert!(a.quick_stage(EPOCH, 2, "a", "song").is_none());
    a.quick_view(EPOCH, |v| {
        let song = v.row("song");
        assert!(!song.pending && !song.can_add);
        assert_eq!(song.outcome, Some(Outcome::Unconfirmed));
        assert_eq!(v.row("another-song").outcome, None);
        assert!(v.row("another-song").can_add);
    });
    assert!(a.refresh_playlist(EPOCH).is_some());
    part(
        &hub,
        GENERATION,
        summary("a", 0, 1),
        "same-generation",
        0,
        vec![],
        false,
    );
    assert!(a.quick_stage(EPOCH, 2, "a", "song").is_none());
    assert!(!a.start_quick_add(&t, || panic!("must not replay")));
    hub.disconnect(EPOCH, GENERATION);
    hub.begin(EPOCH, GENERATION + 1);
    assert!(lists(&hub, GENERATION + 1, vec![summary("a", 0, 1)]).is_some());
    part(
        &hub,
        GENERATION + 1,
        summary("a", 0, 1),
        "fresh-generation",
        0,
        vec![],
        false,
    );
    assert!(a.quick_stage(EPOCH, 4, "a", "song").is_none());
    assert!(!hub.admit_quick_add(&t, || panic!("old socket ticket")));
    assert!(
        hub.authorizes(EPOCH, GENERATION + 1, &t.command()),
        "normal explicit Add is unaffected"
    );
    hub.begin(EPOCH + 1, GENERATION + 2);
    let send = |value| {
        hub.receive(
            EPOCH + 1,
            GENERATION + 2,
            serde_json::from_value(value).unwrap(),
        )
        .unwrap()
    };
    let s = summary("a", 0, 1);
    send(json!({"snapshot":{"playlists":[s.clone()],"recent":[]}}));
    a.quick_watch(EPOCH + 1, true, None);
    assert!(a.quick_select(EPOCH + 1, Some("a")).is_some());
    send(json!({"part":{"summary":s,"token":"new-account","offset":0,"rows":[],"more":false}}));
    assert!(a.quick_stage(EPOCH + 1, 4, "a", "song").is_some());
}

#[test]
fn quick_add_link_loss_distinguishes_started_and_unsent_rows() {
    let (hub, a) = ready();
    empty_a(&hub, &a);
    let began = stage(&a, "a", "began");
    dispatch(&hub, &a, &began);
    let queued = stage(&a, "a", "queued");
    assert!(hub.admit_quick_add(&queued, || true));
    let staged = stage(&a, "a", "staged");
    hub.disconnect(EPOCH, GENERATION);
    assert!(!a.start_quick_add(&queued, || panic!("old link started queued write")));
    assert!(!hub.admit_quick_add(&staged, || panic!("old link admitted staged write")));
    hub.begin(EPOCH, GENERATION + 1);
    lists(&hub, GENERATION + 1, vec![summary("a", 0, 1)]);
    part(
        &hub,
        GENERATION + 1,
        summary("a", 0, 1),
        "no-diff",
        0,
        vec![],
        false,
    );
    a.quick_view(EPOCH, |v| {
        let began = v.row("began");
        assert_eq!(began.membership, Membership::Absent);
        assert_eq!(began.outcome, Some(Outcome::Unconfirmed));
        assert!(!began.pending && !began.can_add);
        for song in ["queued", "staged"] {
            let row = v.row(song);
            assert_eq!(row.membership, Membership::Absent);
            assert_eq!(row.outcome, Some(Outcome::NotSent));
            assert!(!row.pending && row.can_add);
        }
        assert_eq!(v.row("another-song").outcome, None);
    });
    assert!(a.quick_stage(EPOCH, 4, "a", "began").is_none());
    assert!(a.quick_stage(EPOCH, 4, "a", "queued").is_some());
}

#[test]
fn quick_add_foreign_present_then_removed_does_not_rearm_a_started_add() {
    let (hub, a) = ready();
    empty_a(&hub, &a);
    let delayed = stage(&a, "a", "song");
    dispatch(&hub, &a, &delayed);
    receive(
        &hub,
        GENERATION,
        json!({"change":{"playlist":summary("a", 1, 2), "entries":{"added":entry("foreign", "song", 0)}}}),
    );
    a.quick_view(EPOCH, |v| {
        let song = v.row("song");
        assert_eq!(song.membership, Membership::Present);
        assert!(!song.pending && song.outcome.is_none());
    });
    receive(
        &hub,
        GENERATION,
        json!({"change":{"playlist":summary("a", 0, 3), "entries":{"removed":"foreign"}}}),
    );
    a.quick_view(EPOCH, |v| {
        let song = v.row("song");
        assert_eq!(song.membership, Membership::Absent);
        assert!(!song.pending && !song.can_add);
        assert_eq!(song.outcome, Some(Outcome::Unconfirmed));
        assert!(v.row("another-song").can_add);
    });
    assert!(a.quick_stage(EPOCH, 2, "a", "song").is_none());
    assert!(hub.authorizes(EPOCH, GENERATION, &delayed.command()));
}

#[test]
fn quick_add_late_outcome_is_projected_only_for_its_exact_target() {
    let (hub, a) = ready();
    empty_a(&hub, &a);
    let delayed = stage(&a, "a", "song");
    dispatch(&hub, &a, &delayed);
    a.quick_select(EPOCH, Some("b"));
    part(
        &hub,
        GENERATION,
        summary("b", 0, 1),
        "empty-b",
        0,
        vec![],
        false,
    );
    let other = stage(&a, "b", "song");
    dispatch(&hub, &a, &other);
    elapse(&a, 31);
    receive(
        &hub,
        GENERATION,
        json!({"change":{"playlist":summary("b", 1, 2), "entries":{"added":entry("foreign-b", "song", 0)}}}),
    );
    a.quick_view(EPOCH, |v| {
        assert_eq!(v.target.unwrap().id, "b");
        let row = v.row("song");
        assert_eq!(row.membership, Membership::Present);
        assert!(row.outcome.is_none() && !row.pending);
    });
    a.quick_select(EPOCH, Some("a"));
    a.quick_view(EPOCH, |v| {
        assert_eq!(v.row("song").outcome, Some(Outcome::Unconfirmed))
    });
    a.quick_select(EPOCH, None);
    a.quick_view(EPOCH, |v| {
        assert!(v.target.is_none() && v.row("song").outcome.is_none())
    });
}

#[test]
fn quick_add_attempt_budget_does_not_evict_a_hold_or_affect_explicit_add() {
    let (hub, a) = ready();
    empty_a(&hub, &a);
    let refused = stage(&a, "a", "not-sent");
    assert!(!hub.admit_quick_add(&refused, || false));
    let still_eligible = stage(&a, "a", "not-sent");
    a.quick_cancel(&still_eligible);
    for index in 0..4093 {
        let ticket = stage(&a, "a", &format!("song-{index}"));
        dispatch(&hub, &a, &ticket);
        elapse(&a, 31);
    }
    let started = stage(&a, "a", "last-started");
    dispatch(&hub, &a, &started);
    let queued = stage(&a, "a", "last-queued");
    assert!(hub.admit_quick_add(&queued, || true));
    let staged = stage(&a, "a", "last-staged");
    assert!(a.quick_stage(EPOCH, 2, "a", "no-slot").is_none());
    a.quick_view(EPOCH, |v| {
        assert!(v.at_capacity && !v.session_limit);
        assert!(v.row("last-staged").pending && !v.row("no-slot").can_add);
    });
    a.quick_cancel(&staged);
    a.quick_view(EPOCH, |v| {
        assert!(!v.at_capacity && v.row("no-slot").can_add)
    });
    let replacement = stage(&a, "a", "replacement");
    assert!(!hub.admit_quick_add(&staged, || panic!("stale reservation")));
    assert!(a.start_quick_add(&queued, || true));
    dispatch(&hub, &a, &replacement);
    elapse(&a, 31);
    a.quick_view(EPOCH, |v| {
        let row = v.row("new-song");
        assert!(v.session_limit && !v.at_capacity && !row.can_add);
        assert_eq!(v.row("song-0").outcome, Some(Outcome::Unconfirmed));
    });
    for song in ["song-0", "new-song"] {
        assert!(a.quick_stage(EPOCH, 2, "a", song).is_none());
        assert!(hub.authorizes(
            EPOCH,
            GENERATION,
            &PersonalCommand::AddEntry {
                playlist_id: "a".into(),
                track_id: song.into(),
            }
        ));
    }
}

#[test]
fn quick_add_view_scope_fences_no_target_target_round_trip_and_mode() {
    let (hub, a) = ready();
    let unselected = a.quick_view(EPOCH, |v| v.scope(2));
    empty_a(&hub, &a);
    let first = a.quick_view(EPOCH, |v| v.scope(2));
    assert_ne!(unselected, first);
    assert!(
        a.quick_stage_scoped(EPOCH, 2, &unselected, "a", "song")
            .is_none()
    );
    a.quick_select(EPOCH, Some("b"));
    a.quick_select(EPOCH, Some("a"));
    let back = a.quick_view(EPOCH, |v| v.scope(2));
    assert_ne!(first, back);
    assert!(
        a.quick_stage_scoped(EPOCH, 2, &first, "a", "song")
            .is_none()
    );
    assert!(a.quick_stage_scoped(EPOCH, 4, &back, "a", "song").is_none());
    assert!(a.quick_stage_scoped(EPOCH, 2, &back, "a", "song").is_some());
}

#[test]
fn quick_add_obsolete_reads_never_refuse_a_normal_write() {
    let (hub, a) = ready();
    let (generation, stale) = a.quick_select(EPOCH, Some("a")).unwrap();
    a.quick_select(EPOCH, Some("b"));
    let (_, current) = a.quick_select(EPOCH, Some("a")).unwrap();
    assert_eq!(
        serde_json::to_value(&stale).unwrap(),
        serde_json::to_value(&current).unwrap()
    );
    let refused = || a.read(|status, _| status.refused);
    let before = refused();
    hub.send_playlist_sync(EPOCH, generation, &stale, || {
        panic!("old A read admitted after A → B → A")
    });
    hub.send_playlist_sync(EPOCH, generation, &current, || false);
    assert_eq!(refused(), before);
    let (_, detail) = a.quick_watch(EPOCH, false, Some("b".into())).unwrap();
    hub.send_playlist_sync(EPOCH, generation, &current, || {
        panic!("obsolete Search read admitted off Search")
    });
    assert_eq!(refused(), before);
    hub.send_playlist_sync(EPOCH, generation, &detail, || false);
    assert_eq!(refused(), before + 1);
}

#[test]
fn quick_add_held_unsent_target_rights_route_and_account_are_fenced() {
    let (hub, a) = ready();
    empty_a(&hub, &a);
    let held = stage(&a, "a", "song");
    assert!(hub.admit_quick_add(&held, || true));
    assert!(a.quick_select(EPOCH, Some("b")).is_some());
    assert!(!a.start_quick_add(&held, || panic!("old target reached socket")));
    a.quick_view(EPOCH, |v| {
        assert_eq!(v.target.unwrap().id, "b");
        assert_eq!(v.row("song").outcome, None); // The unsent ticket was for A.
    });
    part(&hub, GENERATION, summary("b", 0, 1), "b", 0, vec![], false);
    let rejected = stage(&a, "b", "song");
    let mut no_add = summary("b", 0, 2);
    no_add["capabilities"]["addTracks"] = json!(false);
    receive(&hub, GENERATION, json!({"change":{"playlist":no_add}}));
    assert!(!hub.admit_quick_add(&rejected, || panic!("revoked rights reached socket")));
    a.quick_view(EPOCH, |v| assert!(v.target.is_none()));
    assert!(a.quick_select(EPOCH, Some("b")).is_none());
    empty_a(&hub, &a);
    let unsent = stage(&a, "a", "song");
    assert!(a.quick_watch(EPOCH, false, Some("b".into())).is_some());
    assert!(!hub.admit_quick_add(&unsent, || panic!("off-Search ticket")));
    assert!(a.quick_watch(EPOCH, true, None).is_some());
    a.quick_view(EPOCH, |v| assert_eq!(v.target.unwrap().id, "a"));
    let leaving = stage(&a, "a", "song");
    hub.reset(EPOCH + 1);
    assert!(!hub.admit_quick_add(&leaving, || panic!("old account ticket")));
    for epoch in [EPOCH, EPOCH + 1] {
        a.quick_view(epoch, |v| {
            assert!(v.target.is_none() && v.row("song").outcome.is_none())
        });
    }
}

#[test]
fn quick_add_retires_target_omitted_by_full_account_snapshot() {
    let (hub, a) = ready();
    empty_a(&hub, &a);
    let held = stage(&a, "a", "song");
    let summaries = (0..100).map(|i| summary(&format!("p{i:03}"), 0, 1)).collect();
    assert!(lists(&hub, GENERATION, summaries).is_none());
    assert_eq!(a.quick_targets(EPOCH).len(), 100);
    assert!(a.quick_select(EPOCH, Some("a")).is_none());
    assert!(!hub.admit_quick_add(&held, || panic!("removed target reached socket")));
    a.quick_view(EPOCH, |v| assert!(v.target.is_none()));
    assert!(a.quick_watch(EPOCH, false, Some("a".into())).is_none());
}

#[test]
fn quick_add_target_disconnects_are_bounded_until_scoped_completion() {
    let hub = PersonalHub::default();
    let a = hub.access();
    hub.begin(EPOCH, GENERATION);
    let target = "target";
    let summaries = vec![summary(target, 0, 1)];
    lists(&hub, GENERATION, summaries.clone());
    assert!(a.quick_watch(EPOCH, false, Some(target.into())).is_some());
    part(
        &hub,
        GENERATION,
        summary(target, 0, 1),
        "verified",
        0,
        vec![],
        false,
    );
    a.quick_select(EPOCH, Some(target));
    a.quick_watch(EPOCH, true, None);
    hub.disconnect(EPOCH, GENERATION);

    for generation in GENERATION + 1..=GENERATION + 2 {
        hub.begin(EPOCH, generation);
        let mut snapshot = summaries.clone();
        // A summary with the exact identity and revision does not prove
        // complete membership and cannot replenish the read budget.
        snapshot[0] = summary(target, 2, 2);
        let request = lists(&hub, generation, snapshot).expect("the bounded reconnect attempt");
        assert!(a.start_playlist_sync(EPOCH, generation, &request, || true));
        if generation == GENERATION + 1 {
            let continuation = part(
                &hub,
                generation,
                summary(target, 2, 2),
                "validated-prefix",
                0,
                vec![entry("foreign", "song", 0)],
                true,
            )
            .unwrap();
            assert!(matches!(
                continuation,
                PersonalCommand::SyncPlaylist { offset: 1, .. }
            ));
            assert!(a.start_playlist_sync(EPOCH, generation, &continuation, || true));
        }
        hub.disconnect(EPOCH, generation); // Connection loss proves nothing about membership.
    }

    for generation in GENERATION + 3..=GENERATION + 5 {
        hub.begin(EPOCH, generation);
        assert!(lists(&hub, generation, summaries.clone()).is_none());
        a.read(|status, r| {
            assert_eq!(status.link, Link::Ready);
            assert!(
                r.watched_playlist().is_none(),
                "retire only the failed quick watch"
            );
        });
        a.quick_view(EPOCH, |v| {
            assert!(v.read_stalled && v.target.is_some_and(|p| p.id == target));
            assert_eq!(v.row("song").membership, Membership::Unknown);
            assert!(!v.row("song").can_add);
        });
        assert!(hub.authorizes(EPOCH, generation, &PersonalCommand::ClearHistory));
        assert!(receive(&hub, generation, json!({"change":{}})).is_none());
        hub.disconnect(EPOCH, generation);
    }

    let generation = GENERATION + 6;
    hub.begin(EPOCH, generation);
    assert!(lists(&hub, generation, summaries).is_none());
    a.quick_watch(EPOCH, false, Some(target.into())).unwrap(); // An explicit detail visit may validate it.
    part(
        &hub,
        generation,
        summary(target, 0, 3),
        "verified-again",
        0,
        vec![],
        false,
    );
    a.quick_watch(EPOCH, true, None);
    a.quick_view(EPOCH, |v| assert!(!v.read_stalled && v.row("song").can_add));
}

#[test]
fn quick_add_stalled_or_invalid_partial_read_never_proves_absence() {
    let (hub, a) = ready();
    assert!(a.quick_select(EPOCH, Some("a")).is_some());
    let s = summary("a", 3, 2);
    let initial = part(
        &hub,
        GENERATION,
        s.clone(),
        "broken",
        0,
        vec![entry("same-entry", "old-positive", 0)],
        true,
    );
    assert!(matches!(
        initial,
        Some(PersonalCommand::SyncPlaylist { offset: 1, .. })
    ));
    let repair = part(
        &hub,
        GENERATION,
        s,
        "broken",
        1,
        vec![entry("same-entry", "never-proof", 1)],
        true,
    );
    assert!(matches!(
        repair,
        Some(PersonalCommand::SyncPlaylist {
            offset: 0,
            known: None,
            ..
        })
    ));
    a.quick_view(EPOCH, |v| {
        for track in ["old-positive", "never-proof"] {
            assert_eq!(v.row(track).membership, Membership::Unknown);
            assert!(!v.row(track).can_add);
        }
    });
    elapse(&a, 21);
    let repair = repair.unwrap();
    assert!(
        hub.authorizes(EPOCH, GENERATION, &repair),
        "a slow read can still repair the scope"
    );
    a.quick_view(EPOCH, |v| assert!(v.read_stalled));
    elapse(&a, 121);
    assert!(!hub.authorizes(EPOCH, GENERATION, &repair));
    assert!(a.quick_watch(EPOCH, false, Some("b".into())).is_some());
    assert!(a.quick_watch(EPOCH, true, None).is_some());
}

#[test]
fn quick_add_lost_broadcast_catches_up_once_without_inventing_a_receipt() {
    let (hub, a) = ready();
    empty_a(&hub, &a);
    for song in ["observed-by-read", "silent"] {
        let t = stage(&a, "a", song);
        dispatch(&hub, &a, &t);
    }
    let now = Instant::now();
    let timer = |elapsed| {
        a.state
            .lock()
            .unwrap()
            .run_quick_timer(now + Duration::from_secs(elapsed))
    };
    assert!(timer(7).is_none());
    let probe = timer(9).unwrap();
    assert_eq!(
        serde_json::to_value(&probe).unwrap(),
        json!({"type":"syncPlaylist","playlistId":"a","known":"empty"})
    );
    assert!(a.start_playlist_sync(EPOCH, GENERATION, &probe, || true));
    assert!(
        timer(10).is_none(),
        "one read coalesces both started intents"
    );
    a.quick_view(EPOCH, |v| {
        assert!(
            v.row("third").can_add,
            "a conditional probe does not invalidate all absence"
        );
        assert!(v.row("observed-by-read").pending && v.row("silent").pending);
    });
    // No broadcast (e.g. a lost tail projection). Only scoped rows can restore
    // membership. Even this proves only current presence, not who added it.
    part(
        &hub,
        GENERATION,
        summary("a", 1, 2),
        "after-probe",
        0,
        vec![entry("e", "observed-by-read", 0)],
        false,
    );
    assert!(timer(31).is_none());
    a.quick_view(EPOCH, |v| {
        assert_eq!(v.row("observed-by-read").membership, Membership::Present);
        assert_eq!(v.row("observed-by-read").outcome, None);
        assert_eq!(v.row("silent").outcome, Some(Outcome::Unconfirmed));
        assert!(v.row("third").can_add);
    });
    assert!(a.quick_stage(EPOCH, 2, "a", "silent").is_none());
}

#[test]
fn quick_add_silent_matching_probe_never_marks_presence_or_rearms() {
    let (hub, a) = ready();
    empty_a(&hub, &a);
    let t = stage(&a, "a", "song");
    dispatch(&hub, &a, &t);
    let now = Instant::now();
    let timer = |elapsed| {
        a.state
            .lock()
            .unwrap()
            .run_quick_timer(now + Duration::from_secs(elapsed))
    };
    let probe = timer(9).unwrap();
    assert!(a.start_playlist_sync(EPOCH, GENERATION, &probe, || true));
    // Go emits no frame when known equals the projection token.
    assert!(timer(31).is_none());
    a.quick_view(EPOCH, |v| {
        let row = v.row("song");
        assert!(!row.pending && !row.can_add && !v.read_stalled);
        assert_eq!(row.outcome, Some(Outcome::Unconfirmed));
        assert!(v.row("elsewhere").can_add);
    });
    assert!(!a.start_quick_add(&t, || panic!("must not replay")));
}

#[test]
fn quick_add_catchup_stays_optional_when_detail_inherits_the_watch() {
    for tokenless in [false, true] {
        let (hub, a) = ready();
        empty_a(&hub, &a);
        let ticket = stage(&a, "a", "pending");
        dispatch(&hub, &a, &ticket);
        if tokenless {
            receive(
                &hub,
                GENERATION,
                json!({"change":{"playlist":summary("a", 1, 2),
                "entries":{"added":entry("other", "other", 0)}}}),
            );
        }
        let probe = a
            .state
            .lock()
            .unwrap()
            .run_quick_timer(Instant::now() + Duration::from_secs(9))
            .unwrap();
        assert!(
            matches!(probe, PersonalCommand::SyncPlaylist { ref known, .. } if known.is_none() == tokenless)
        );
        assert!(a.quick_watch(EPOCH, false, Some("a".into())).is_none());
        assert_eq!(
            a.inspect_playlist_sync(EPOCH, GENERATION, &probe, |optional| optional),
            Some(true)
        );
        let before = a.read(|status, _| status.refused);
        hub.send_playlist_sync(EPOCH, GENERATION, &probe, || false);
        assert_eq!(a.read(|status, _| status.refused), before);
    }
}

#[test]
fn quick_add_hard_read_cutoff_does_not_depend_on_browser_timer_order() {
    let (hub, a) = ready();
    let summary = summary("a", 1, 2);
    let command = {
        let mut state = a.state.lock().unwrap();
        state
            .quick
            .select(Some(serde_json::from_value(summary.clone()).unwrap()));
        let command = state.quick_watch().1.unwrap();
        state.quick_read(
            Some(&command),
            false,
            Instant::now() - Duration::from_secs(121),
        );
        command
    };
    assert!(hub.authorizes(EPOCH, GENERATION, &command));
    // Ingress wins the wakeup race against the browser timer. The abandoned
    // generation still cannot become authoritative after its hard cutoff.
    part(
        &hub,
        GENERATION,
        summary,
        "expired",
        0,
        vec![entry("e", "late", 0)],
        false,
    );
    assert!(!hub.authorizes(EPOCH, GENERATION, &command));
    a.quick_view(EPOCH, |v| {
        assert!(v.read_retired && v.read_stalled);
        assert_eq!(v.row("late").membership, Membership::Unknown);
    });
}

#[test]
fn quick_add_large_repair_ignores_old_continuation_and_accepts_late_final_part() {
    let (hub, a) = ready();
    empty_a(&hub, &a);
    let ticket = stage(&a, "a", "exact-tail");
    dispatch(&hub, &a, &ticket);
    let first_gen = summary("a", 1501, 2);
    let fresh = summary("a", 1502, 3);
    // Go budgets each frame (including the envelope), not a fixed row count.
    let chunk = |s: &Value, token: &str, offset: usize| {
        let total = s["trackCount"].as_u64().unwrap() as usize;
        let empty = json!({"type":"personal","update":{"part":{"summary":s,"token":token,
            "offset":offset,"rows":[],"more":false}}});
        let mut remaining = (256 << 10) - serde_json::to_vec(&empty).unwrap().len();
        let mut rows = Vec::new();
        for i in offset..total {
            let track = if i == 1501 { "exact-tail" } else { "unrelated" };
            let mut row = entry(&format!("e{i}"), track, i);
            row["track"]["name"] = json!("x".repeat(300 + i % 130));
            let size = serde_json::to_vec(&row).unwrap().len() + usize::from(!rows.is_empty());
            if size > remaining {
                break;
            }
            remaining -= size;
            rows.push(row);
        }
        assert!(!rows.is_empty());
        let end = offset + rows.len();
        (rows, end, end < total)
    };
    let (old_rows, old_end, old_more) = chunk(&first_gen, "before-peer-edit", 0);
    assert!(old_more);
    receive(
        &hub,
        GENERATION,
        json!({"change":{"playlist":first_gen,"rowsInvalid":true}}),
    );
    let old_next = part(
        &hub,
        GENERATION,
        first_gen.clone(),
        "before-peer-edit",
        0,
        old_rows,
        old_more,
    )
    .unwrap();
    // An unsolicited invalidation makes the previous token/offset obsolete;
    // Go may restart an already-sent continuation at offset zero.
    let repair = receive(
        &hub,
        GENERATION,
        json!({"change":{"playlist":fresh,"rowsInvalid":true}}),
    )
    .unwrap();
    assert!(!hub.authorizes(EPOCH, GENERATION, &old_next));
    assert!(hub.authorizes(EPOCH, GENERATION, &repair));
    let (stale_rows, _, stale_more) = chunk(&first_gen, "before-peer-edit", old_end);
    assert!(
        part(
            &hub,
            GENERATION,
            first_gen,
            "before-peer-edit",
            old_end,
            stale_rows,
            stale_more
        )
        .is_none()
    );
    let mut offset = 0;
    let mut chunks = 0;
    let (final_rows, next) = loop {
        let (rows, end, more) = chunk(&fresh, "after-peer-edit", offset);
        if !more {
            break (
                rows,
                a.read(|_, replica| replica.pending_sync().unwrap().clone()),
            );
        }
        let continuation = part(
            &hub,
            GENERATION,
            fresh.clone(),
            "after-peer-edit",
            offset,
            rows,
            true,
        )
        .unwrap();
        assert!(a.start_playlist_sync(EPOCH, GENERATION, &continuation, || true));
        offset = end;
        chunks += 1;
    };
    assert!(chunks >= 2);
    elapse(&a, 21);
    a.quick_view(EPOCH, |v| {
        let tail = v.row("exact-tail");
        assert!(v.read_stalled && tail.pending && !tail.can_add);
        assert_eq!(tail.membership, Membership::Unknown);
    });
    assert!(
        hub.authorizes(EPOCH, GENERATION, &next),
        "soft deadline leaves the pull scoped"
    );
    assert!(
        part(
            &hub,
            GENERATION,
            fresh,
            "after-peer-edit",
            offset,
            final_rows,
            false
        )
        .is_none()
    );
    a.quick_view(EPOCH, |v| {
        let tail = v.row("exact-tail");
        assert_eq!(tail.membership, Membership::Present);
        assert!(!tail.pending && !tail.can_add && !v.read_stalled);
        assert!(v.row("not-a-member").can_add);
    });
}
