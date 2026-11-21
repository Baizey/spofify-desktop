use super::*;
use crate::socket::{ContextKind, PlayerDelta};
use serde_json::json;

fn candidate(command: &Command, state: &State, link: watch::Receiver<Link>) -> Option<Start> {
    Start::new(
        command,
        state,
        1,
        link,
        watch::channel(None).1,
        Arc::default(),
    )
}

fn context() -> Context {
    Context {
        kind: ContextKind::Playlist,
        id: "playlist".into(),
        name: "Old name".into(),
    }
}
fn play(entry: &str) -> Command {
    Command::Play {
        selection: Selection::Track {
            track: TrackRef {
                id: "duplicate".into(),
                entry_id: Some(entry.into()),
                ..TrackRef::default()
            },
            context: Some(context()),
        },
        shuffle: Some(false),
        seed: None,
    }
}
fn paused() -> State {
    serde_json::from_value(json!({
        "queue": [{"key":8,"order":8,"id":"duplicate","entryId":"second","intent":"low"}],
        "cursor":{"key":8,"chain":"previous","visit":2,"run":7,"elapsed":12.25,"desired":false,"playing":false},
        "repeat":"off","shuffle":false
    })).unwrap()
}
fn receipt(entry: &str) -> (PlayerDelta, State) {
    let cursor = Cursor {
        key: 10,
        chain: "next".into(),
        desired: true,
        ..Cursor::default()
    };
    let delta: PlayerDelta = serde_json::from_value(json!({
        "source":"this-output", "cursor":cursor, "queue":{"kind":"replace","rows":[
            {"key":10,"order":10,"id":"duplicate","entryId":entry,"intent":"low",
             "playbackContext":{"kind":"playlist","id":"playlist","name":"Renamed"}}
        ]}
    }))
    .unwrap();
    let state = State {
        cursor,
        queue: delta.queue.as_ref().unwrap().rows.clone(),
        ..State::default()
    };
    (delta, state)
}

#[test]
fn same_recording_is_not_an_ack_for_a_different_occurrence() {
    let (_link, receiver) = watch::channel(Link::Connected(1));
    let base = paused();
    let mut start = candidate(&play("second"), &base, receiver).unwrap();
    assert_eq!(start.id, "duplicate");
    let (delta, state) = receipt("first");
    assert!(!start.observe(&delta, &state, true, false));
    let (delta, state) = receipt("second");
    assert!(
        !start.observe(&delta, &state, false, false),
        "another controller's exact selection is not a receipt"
    );
    assert!(start.observe(&delta, &state, true, false));
    assert_eq!(start.canonical.as_ref(), Some(&state.cursor));
}

#[test]
fn play_add_and_denial_do_not_become_a_commit() {
    let (_link, receiver) = watch::channel(Link::Connected(1));
    let base = paused();
    let mut start = candidate(&play("second"), &base, receiver).unwrap();
    let (mut insert, _) = receipt("second");
    insert.cursor = None;
    insert.queue.as_mut().unwrap().kind = QueueEditKind::Insert;
    assert!(start.observe(&insert, &base, true, false));
    assert!(start.canonical.is_none());
    let denial = PlayerDelta {
        source: Some("this-output".into()),
        cursor: Some(base.cursor.clone()),
        ..Default::default()
    };
    assert!(!start.observe(&denial, &base, true, false));
}

#[test]
fn a_distinct_ordered_receipt_recovers_after_double_play() {
    let mut receipts = Receipts::default();
    let (first, second) = (play("first"), play("second"));
    receipts.sent(&first, None);
    assert!(!receipts.may_start(&first, 0));
    receipts.sent(&first, None);
    let (delta, state) = receipt("first");
    receipts.received(&delta, &state);
    assert!(
        !receipts.may_start(&first, 0),
        "the source tag alone cannot disambiguate identical pending plays"
    );
    assert!(receipts.may_start(&second, 0));
    receipts.sent(&second, None);
    let (delta, state) = receipt("second");
    receipts.received(&delta, &state);
    assert!(receipts.may_start(&first, 0) && receipts.may_start(&second, 0));
}

#[test]
fn resume_requires_the_same_paused_occurrence_position_and_next_run() {
    let (_link, receiver) = watch::channel(Link::Connected(1));
    let base = paused();
    let mut start = candidate(&Command::Resume, &base, receiver).unwrap();
    assert_eq!(start.elapsed, 12.25);
    let mut state = base.clone();
    state.cursor.run += 1;
    let mut update = PlayerDelta {
        cursor: Some(state.cursor.clone()),
        ..Default::default()
    };
    assert!(start.observe(&update, &state, false, true));
    assert!(start.output_pulse.is_some());
    start.output_confirmed();
    state.cursor.run += 1;
    state.cursor.desired = true;
    update.cursor = Some(state.cursor.clone());
    let mut wrong = state.clone();
    wrong.cursor.elapsed += 1.0;
    assert!(!start.observe(&update, &wrong, false, false));
    assert!(start.observe(&update, &state, false, false));
    assert_eq!(start.canonical, Some(state.cursor));
}

#[test]
fn overlapping_resumes_do_not_borrow_an_earlier_unlabeled_run() {
    let mut receipts = Receipts::default();
    assert!(receipts.may_start(&Command::Resume, 0));
    receipts.sent(&Command::Resume, Some(6));
    assert!(!receipts.may_start(&Command::Resume, 5));
    assert!(receipts.may_start(&Command::Resume, 6));
}

#[test]
fn start_requires_an_exact_non_shuffled_choice_or_known_resume() {
    let (_link, receiver) = watch::channel(Link::Connected(1));
    let mut shuffle = play("second");
    let Command::Play { shuffle: flag, .. } = &mut shuffle else {
        unreachable!()
    };
    *flag = Some(true);
    let collection = Command::Play {
        selection: Selection::Collection { context: context() },
        shuffle: None,
        seed: None,
    };
    for command in [&shuffle, &collection, &Command::Next] {
        assert!(candidate(command, &paused(), receiver.clone()).is_none());
    }
    assert!(candidate(&Command::Resume, &State::default(), receiver).is_none());
}

#[test]
fn speculative_source_cannot_gain_disconnect_grace_or_survive_deadline() {
    let (link, receiver) = watch::channel(Link::Connected(1));
    let mut start = candidate(&play("second"), &State::default(), receiver).unwrap();
    let source = start.permit.source();
    assert!(source.may_start());
    link.send_replace(Link::Connected(2));
    assert!(!source.is_valid() && !start.permit.commit(Authority::offline()));
    link.send_replace(Link::Connected(1));
    start.permit.deadline = Instant::now();
    let expired = start.permit.source();
    assert!(!expired.is_valid() && !start.permit.commit(Authority::offline()));
}

#[test]
fn a_known_grant_deadline_fences_pcm_before_the_socket_polls_expiry() {
    let (_link, receiver) = watch::channel(Link::Connected(1));
    let (grants, grant) = watch::channel(None);
    let start = Start::new(
        &play("second"),
        &State::default(),
        1,
        receiver,
        grant,
        Arc::default(),
    )
    .unwrap();
    let source = start.permit.source();
    assert!(source.may_start());
    grants.send_replace(Some(OutputAuthority {
        generation: 1,
        deadline: Instant::now(),
        attached: true,
        resume_through: 0,
    }));
    assert!(!source.is_valid());
}

#[test]
fn commit_rebinds_the_same_source_to_ordinary_output_authority() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let (link, receiver) = watch::channel(Link::Connected(1));
    let start = candidate(&play("second"), &State::default(), receiver).unwrap();
    let source = start.permit.source();
    let committed = Arc::new(AtomicBool::new(true));
    let bound = Authority::online(
        {
            let committed = committed.clone();
            move || committed.load(Ordering::Acquire)
        },
        || true,
    );
    assert!(source.may_start());
    assert!(start.permit.commit(bound));
    link.send_replace(Link::Disconnected);
    assert!(source.is_valid());
    committed.store(false, Ordering::Release);
    assert!(!source.is_valid());
}
