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
    a.quick_filter(EPOCH, true);
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
            assert!(!known.visible);
            let missing = v.row("missing");
            assert_eq!(
                missing.membership,
                if end < total {
                    Membership::Unknown
                } else {
                    Membership::Absent
                }
            );
            assert!(missing.visible);
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
    a.quick_filter(EPOCH, true);
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
        assert!(song.pending && song.visible && !song.can_add);
        assert_eq!(song.membership, Membership::Absent);
        assert_eq!(v.notice, None);
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
        assert!(!song.pending && !song.visible && !song.can_add);
        assert_eq!(song.membership, Membership::Present);
        assert_eq!(v.notice, None); // Plain membership; no success/receipt state exists.
    });
    a.read(|_, r| assert!(r.playlist().is_some_and(|p| p.stale && p.rows.is_empty())));
}

#[test]
fn quick_add_dropped_no_diff_and_reconnect_never_rearm_old_reader_attempt() {
    let (hub, a) = ready();
    empty_a(&hub, &a);
    let t = stage(&a, "a", "song");
    dispatch(&hub, &a, &t);
    elapse(&a, 6);
    assert!(a.quick_stage(EPOCH, 2, "a", "song").is_none());
    a.quick_view(EPOCH, |v| {
        let song = v.row("song");
        assert!(!song.pending && song.visible && !song.can_add);
        assert_eq!(v.notice, Some(Outcome::Unconfirmed));
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
        assert_eq!(v.row("song").membership, Membership::Present);
        assert!(!v.row("song").pending && v.notice.is_none());
    });
    receive(
        &hub,
        GENERATION,
        json!({"change":{"playlist":summary("a", 0, 3), "entries":{"removed":"foreign"}}}),
    );
    a.quick_view(EPOCH, |v| {
        let song = v.row("song");
        assert_eq!(song.membership, Membership::Absent);
        assert!(song.visible && !song.pending && !song.can_add);
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
    elapse(&a, 6);
    a.quick_view(EPOCH, |v| {
        assert!(v.target.unwrap().id == "b" && v.notice.is_none())
    });
    a.quick_select(EPOCH, Some("a"));
    a.quick_view(EPOCH, |v| assert_eq!(v.notice, Some(Outcome::Unconfirmed)));
    a.quick_select(EPOCH, None);
    a.quick_view(EPOCH, |v| assert!(v.target.is_none() && v.notice.is_none()));
}

#[test]
fn quick_add_attempt_budget_does_not_evict_a_hold_or_affect_explicit_add() {
    let (hub, a) = ready();
    empty_a(&hub, &a);
    let refused = stage(&a, "a", "not-sent");
    assert!(!hub.admit_quick_add(&refused, || false));
    let still_eligible = stage(&a, "a", "not-sent");
    a.quick_cancel(&still_eligible);
    for index in 0..4096 {
        let ticket = stage(&a, "a", &format!("song-{index}"));
        dispatch(&hub, &a, &ticket);
        elapse(&a, 6);
    }
    a.quick_view(EPOCH, |v| {
        let row = v.row("new-song");
        assert!(v.session_limit && row.visible && !row.can_add);
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
    a.quick_filter(EPOCH, true);
    let held = stage(&a, "a", "song");
    assert!(hub.admit_quick_add(&held, || true));
    assert!(a.quick_select(EPOCH, Some("b")).is_some());
    assert!(!a.start_quick_add(&held, || panic!("old target reached socket")));
    a.quick_view(EPOCH, |v| {
        assert_eq!(v.target.unwrap().id, "b");
        assert!(!v.hide);
        assert_eq!(v.notice, None); // The unsent ticket was for A.
    });
    part(&hub, GENERATION, summary("b", 0, 1), "b", 0, vec![], false);
    a.quick_filter(EPOCH, true);
    let rejected = stage(&a, "b", "song");
    let mut no_add = summary("b", 0, 2);
    no_add["capabilities"]["addTracks"] = json!(false);
    receive(&hub, GENERATION, json!({"change":{"playlist":no_add}}));
    assert!(!hub.admit_quick_add(&rejected, || panic!("revoked rights reached socket")));
    a.quick_view(EPOCH, |v| assert!(v.target.is_none() && !v.hide));
    assert!(a.quick_select(EPOCH, Some("b")).is_none());
    empty_a(&hub, &a);
    a.quick_filter(EPOCH, true);
    let unsent = stage(&a, "a", "song");
    assert!(a.quick_watch(EPOCH, false, Some("b".into())).is_some());
    assert!(!hub.admit_quick_add(&unsent, || panic!("off-Search ticket")));
    assert!(a.quick_watch(EPOCH, true, None).is_some());
    a.quick_view(EPOCH, |v| assert!(v.hide && v.target.unwrap().id == "a"));
    let leaving = stage(&a, "a", "song");
    hub.reset(EPOCH + 1);
    assert!(!hub.admit_quick_add(&leaving, || panic!("old account ticket")));
    for epoch in [EPOCH, EPOCH + 1] {
        a.quick_view(epoch, |v| {
            assert!(v.target.is_none() && !v.hide && v.notice.is_none())
        });
    }
}

#[test]
fn quick_add_selects_current_editable_detail_beyond_the_summary_cap() {
    let hub = PersonalHub::default();
    let a = hub.access();
    hub.begin(EPOCH, GENERATION);
    let summaries: Vec<_> = (0..100)
        .map(|i| summary(&format!("p{i:03}"), 0, 1))
        .collect();
    lists(&hub, GENERATION, summaries.clone());
    assert!(a.quick_select(EPOCH, Some("z-outside")).is_none());
    assert!(
        a.quick_watch(EPOCH, false, Some("z-outside".into()))
            .is_some()
    );
    part(
        &hub,
        GENERATION,
        summary("z-outside", 0, 1),
        "outside",
        0,
        vec![],
        false,
    );
    assert_eq!(a.quick_targets(EPOCH).len(), 101);
    assert!(a.quick_select(EPOCH, Some("z-outside")).is_none()); // Off Search: no second watch.
    assert!(a.quick_watch(EPOCH, true, None).is_none()); // Same exact watched route.
    a.quick_filter(EPOCH, true);
    hub.disconnect(EPOCH, GENERATION);
    hub.begin(EPOCH, GENERATION + 1);
    assert!(lists(&hub, GENERATION + 1, summaries).is_some());
    a.quick_view(EPOCH, |v| {
        assert_eq!(v.target.unwrap().id, "z-outside");
        assert!(v.hide);
        assert_eq!(v.row("song").membership, Membership::Unknown);
    });
    part(
        &hub,
        GENERATION + 1,
        summary("z-outside", 0, 1),
        "outside-2",
        0,
        vec![],
        false,
    );
    assert!(a.quick_stage(EPOCH, 4, "z-outside", "song").is_some());
}

#[test]
fn quick_add_capped_target_disconnects_are_bounded_until_scoped_completion() {
    let hub = PersonalHub::default();
    let a = hub.access();
    hub.begin(EPOCH, GENERATION);
    let summaries: Vec<_> = (0..100)
        .map(|i| summary(&format!("p{i:03}"), 0, 1))
        .collect();
    lists(&hub, GENERATION, summaries.clone());
    let target = "z-outside";
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
    a.quick_filter(EPOCH, true);
    hub.disconnect(EPOCH, GENERATION);

    for generation in GENERATION + 1..=GENERATION + 2 {
        hub.begin(EPOCH, generation);
        let mut snapshot = summaries.clone();
        if generation == GENERATION + 2 {
            // A summary—even the exact stored identity and revision—is not
            // a validated scoped result and cannot replenish the budget.
            snapshot[99] = summary(target, 2, 2);
        }
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
        hub.disconnect(EPOCH, generation); // Go closes on an invalid scoped request; no error frame.
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
            assert!(v.read_stalled && v.hide && v.target.is_some_and(|p| p.id == target));
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
    a.quick_filter(EPOCH, true);
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
            assert!(v.row(track).visible);
        }
    });
    elapse(&a, 7);
    assert!(!hub.authorizes(EPOCH, GENERATION, &repair.unwrap()));
    a.quick_view(EPOCH, |v| assert!(v.read_stalled && v.hide));
    assert!(a.quick_watch(EPOCH, false, Some("b".into())).is_some());
    assert!(a.quick_watch(EPOCH, true, None).is_some());
}
