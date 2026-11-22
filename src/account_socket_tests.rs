use super::wire::FIRST_PREVIEW_KEY;
use super::*;
use serde_json::{json, to_value};

const DEVICE: &str = "desk-fixture";
const OLD: &str = "old-connection";
const NEW: &str = "new-connection";

fn cursor_json() -> Value {
    json!({"key":1,"chain":"fixture-chain","visit":0,"run":1,"elapsed":3.0,"desired":true,"playing":false})
}

fn snapshot(connection: &str, owner: &str) -> Value {
    json!({
        "type":"snapshot", "connectionId":connection, "owner":owner,
        "devices":[{"id":connection,"deviceId":DEVICE,"name":"Desk","canPlay":true}],
        "player":{"queue":[{"id":"track:1","name":"display".repeat(280),"artists":null,
            "albumName":"Album","albumId":"album:1","duration":181.0,
            "key":1,"order":1,"intent":"low","displayFuture":true,
            "playbackContext":{"kind":"album","id":"album:1","name":"Album","revision":9,"fence":"opaque"}}],
            "cursor":cursor_json(),"repeat":"off","shuffle":false,"collectionFirst":1}
    })
}

fn signals() -> (
    Signals,
    watch::Receiver<Link>,
    watch::Receiver<Option<OutputAuthority>>,
    mpsc::Receiver<Event>,
) {
    let (link_tx, link) = watch::channel(Link::Connecting);
    let (authority_tx, authority) = watch::channel(None);
    let (event_tx, events) = mpsc::channel(EVENT_CAPACITY);
    (
        Signals {
            order_gate: Arc::default(),
            personal_applied: watch::channel(0).0,
            link: link_tx,
            authority: authority_tx,
            direct_start: Arc::default(),
            events: event_tx,
        },
        link,
        authority,
        events,
    )
}

fn quick_playlist(id: &str) -> Value {
    json!({"id":id, "name":"Mix", "description":"", "kind":"manual", "countState":"exact",
        "trackCount":0, "availableCount":0, "revision":1,
        "createdAt":"2026-08-03T00:00:00Z", "updatedAt":"2026-08-03T00:00:00Z",
        "capabilities":{"editMetadata":true,"editEntries":true,"editDefinition":false,
            "play":true,"copyToManual":false,"delete":true,"addTracks":true}})
}

#[tokio::test]
async fn lyric_seek_writer_drops_retired_occurrences_snapshots_and_accounts() {
    let (signals, _, _, _events) = signals();
    let (actual, _) = watch::channel(None);
    let mut output = OutputState {
        lease: None,
        actual,
    };
    let mut connection = begin(NEW, "", &mut output, &signals);
    let generation = connection.generation;
    let epoch = Arc::new(AtomicU64::new(7));
    let lyrics = Arc::new(AtomicU64::new(1));
    let api = Api::new(crate::api::Origin::parse("http://127.0.0.1:1").unwrap()).unwrap();
    let (mut socket, mut peer) = Socket::test_peer(api.test_session(), generation);
    socket.order_gate = signals.order_gate.clone();
    let mut old_visit = connection.canonical.clone();
    old_visit.visit = old_visit.visit.wrapping_add(1);
    let mut observed_progress = connection.canonical.clone();
    observed_progress.elapsed += 0.5; // a genuine observation is not a new occurrence
    let scope = |cursor, observed, account| LyricSeekScope {
        cursor,
        snapshot: LyricSnapshot::capture(&lyrics, observed),
        epoch: account,
        current_epoch: epoch.clone(),
    };
    socket
        .seek_lyric(generation, 5.0, scope(connection.canonical.clone(), 1, 7))
        .unwrap();
    // A new queue snapshot can retire the same cursor's duration/recording.
    signals.order_gate.lock().unwrap().queue += 1;
    for (value, basis) in [
        (1.0, scope(old_visit, 1, 7)),
        (2.0, scope(connection.canonical.clone(), 0, 7)),
        (3.0, scope(connection.canonical.clone(), 1, 6)),
        (4.021, scope(observed_progress, 1, 7)),
    ] {
        socket.seek_lyric(generation, value, basis).unwrap();
    }
    let (client, server) = tokio::io::duplex(4096);
    let mut ws = WebSocketStream::from_raw_socket(client, Role::Client, None).await;
    let mut server = WebSocketStream::from_raw_socket(server, Role::Server, None).await;
    let script = async move {
        assert_eq!(read_json(&mut server).await["type"], "presence");
        assert_eq!(
            read_json(&mut server).await,
            json!({"type":"seek","value":4.021})
        );
    };
    let (ended, ()) = time::timeout(Duration::from_secs(2), async {
        tokio::join!(
            connection.drive(&mut ws, DEVICE, &mut peer.outbound, &mut output, &signals),
            script
        )
    })
    .await
    .unwrap();
    assert_eq!(ended, Err(End::Transport));
}

#[tokio::test]
async fn quick_add_writer_discards_stale_scoped_reads_before_age_and_keeps_account_socket() {
    use crate::personal_bridge::PersonalHub;

    let (signals, _, _, _events) = signals();
    let (actual, _) = watch::channel(None);
    let mut output = OutputState {
        lease: None,
        actual,
    };
    let mut connection = begin(NEW, "", &mut output, &signals);
    let generation = connection.generation;
    let epoch = Arc::new(AtomicU64::new(7));
    let api = Api::new(crate::api::Origin::parse("http://127.0.0.1:1").unwrap()).unwrap();
    let (socket, mut peer) = Socket::test_peer(api.test_session(), generation);
    let hub = PersonalHub::default();
    let personal = hub.access();
    hub.begin(7, generation);
    hub.receive(
        7,
        generation,
        serde_json::from_value(
            json!({"snapshot":{"playlists":[quick_playlist("a"),quick_playlist("b")],"recent":[]}}),
        )
        .unwrap(),
    )
    .unwrap();
    personal.quick_watch(7, true, None);
    let (_, retired) = personal.quick_select(7, Some("a")).unwrap();
    let stage = |command| {
        socket
            .personal_playlist_sync(7, generation, command, personal.clone(), epoch.clone())
            .is_ok()
    };
    for _ in 0..2 {
        hub.send_playlist_sync(7, generation, &retired, || stage(&retired));
    }
    let mut aged = peer.outbound.try_recv().unwrap();
    aged.at -= MAX_QUEUE_AGE + Duration::from_millis(1);
    let fresh = peer.outbound.try_recv().unwrap();
    personal.quick_select(7, Some("b"));
    let (_, replacement) = personal.quick_select(7, Some("a")).unwrap();
    assert!(
        socket
            .personal(generation, &PersonalCommand::ClearHistory)
            .is_ok()
    );
    hub.send_playlist_sync(7, generation, &replacement, || stage(&replacement));
    let normal = peer.outbound.try_recv().unwrap();
    let latest = peer.outbound.try_recv().unwrap();
    assert_eq!(
        fresh.raw, latest.raw,
        "local identity must not change the wire"
    );
    let (queue, mut input) = mpsc::channel(OUTBOUND_CAPACITY);
    for frame in [aged, fresh, normal, latest] {
        queue.try_send(frame).ok().unwrap();
    }

    let (client, server) = tokio::io::duplex(4096);
    let mut ws = WebSocketStream::from_raw_socket(client, Role::Client, None).await;
    let mut server = WebSocketStream::from_raw_socket(server, Role::Server, None).await;
    let script = async move {
        assert_eq!(read_json(&mut server).await["type"], "presence");
        assert_eq!(
            read_json(&mut server).await,
            json!({"type":"personal","command":{"type":"clearHistory"}})
        );
        assert_eq!(
            read_json(&mut server).await,
            json!({"type":"personal","command":{"type":"syncPlaylist","playlistId":"a"}})
        );
    };
    let (ended, ()) = time::timeout(Duration::from_secs(2), async {
        tokio::join!(
            connection.drive(&mut ws, DEVICE, &mut input, &mut output, &signals),
            script
        )
    })
    .await
    .unwrap();
    assert_eq!(ended, Err(End::Transport)); // Fixture peer closed after both actual frames.
}

#[tokio::test]
async fn quick_add_writer_fences_buffered_ticket_before_start_send() {
    use crate::{personal_bridge::PersonalHub, quick_add::Outcome};

    for blocked_by in [
        "selection",
        "account",
        "mode",
        "expiry",
        "wire",
        "present",
        "unrelated",
        "",
    ] {
        let (signals, link, authority, events) = signals();
        let (outbound, mut written) = mpsc::channel(2);
        let (actual, _) = watch::channel(None);
        let api = Api::new(crate::api::Origin::parse("http://127.0.0.1:1").unwrap()).unwrap();
        let session = api.test_session();
        let socket = Socket {
            link,
            authority,
            direct_start: signals.direct_start.clone(),
            events,
            outbound,
            actual,
            resume_sequence: AtomicU64::new(0),
            order_gate: signals.order_gate.clone(),
            personal_applied: signals.personal_applied.clone(),
            session: session.clone(),
            task: tokio::spawn(pending()),
        };
        signals.link.send_replace(Link::Connected(13));
        let epoch = Arc::new(AtomicU64::new(7));
        let hub = PersonalHub::default();
        let a = hub.access();
        let list = quick_playlist("target");
        hub.begin(7, 13);
        hub.receive(
            7,
            13,
            serde_json::from_value(json!({"snapshot":{"playlists":[list.clone()],"recent":[]}}))
                .unwrap(),
        )
        .unwrap();
        a.quick_watch(7, true, None);
        a.quick_select(7, Some("target"));
        hub.receive(
            7,
            13,
            serde_json::from_value(
                json!({"part":{"summary":list,"token":"t","offset":0,"rows":[],"more":false}}),
            )
            .unwrap(),
        )
        .unwrap();
        let mode_stamp = *session.network_changes().borrow();
        let ticket = a.quick_stage(7, mode_stamp, "target", "song").unwrap();
        assert!(hub.admit_quick_add(&ticket, || {
            socket
                .personal_quick_add(a.clone(), ticket.clone(), epoch.clone())
                .is_ok()
        }));
        let frame = written.try_recv().unwrap();
        let mut socket = socket;
        match blocked_by {
            "selection" => {
                a.quick_select(7, None);
            }
            "account" => {
                let _publication = PUBLICATION_GATE.lock().unwrap();
                epoch.store(8, Ordering::SeqCst);
            }
            "mode" => {
                session.set_offline(true);
            }
            "expiry" => {
                session.retire_local_grant();
            }
            "wire" | "present" | "unrelated" => {
                let value = if blocked_by == "present" {
                    let mut summary = quick_playlist("target");
                    summary["trackCount"] = json!(1);
                    summary["availableCount"] = json!(1);
                    summary["revision"] = json!(2);
                    json!({"change":{"playlist":summary,"entries":{"added":{"id":"occurrence","position":0,
                        "track":{"kind":"track","id":"song","name":"Song","hasAudio":true,
                            "coverage":{"tracks":1,"inLibrary":1,"albums":1}}}}}})
                } else {
                    json!({"change":{}})
                };
                signals
                    .event(13, EventKind::Personal(value), false)
                    .unwrap();
            }
            "" => {}
            _ => unreachable!(),
        }
        let (client, server) = tokio::io::duplex(4096);
        let mut ws = WebSocketStream::from_raw_socket(client, Role::Client, None).await;
        let mut server = WebSocketStream::from_raw_socket(server, Role::Server, None).await;
        let ScopedWrite::Quick(quick) = *frame.scoped.unwrap() else {
            panic!("quick write retains its guard")
        };
        let writer = async {
            let deadline = frame.at + MAX_QUEUE_AGE;
            if quick_ready(&signals, Some(deadline)).await {
                send_quick(&mut ws, frame.raw, quick, deadline).await
            } else {
                quick.personal.quick_cancel(&quick.ticket);
                Ok(())
            }
        };
        let apply = async {
            if matches!(blocked_by, "present" | "unrelated") {
                // The socket has published but not yet applied an update. Once
                // applied, only its exact membership can block this write.
                tokio::task::yield_now().await;
                let update = socket.events.try_recv().unwrap();
                let EventKind::Personal(value) = update.kind else {
                    unreachable!()
                };
                hub.receive(7, 13, serde_json::from_value(value).unwrap())
                    .unwrap();
                socket.mark_personal_applied(update.personal_stamp.unwrap());
            }
        };
        let (result, ()) = tokio::join!(writer, apply);
        result.unwrap();
        drop(ws);
        if matches!(blocked_by, "" | "unrelated") {
            assert_eq!(
                read_json(&mut server).await,
                json!({"type":"personal","command":{"type":"addEntry","playlistId":"target","trackId":"song"}})
            );
            hub.disconnect(7, 13);
            a.quick_view(7, |v| {
                assert_eq!(v.row("song").outcome, Some(Outcome::Unconfirmed))
            });
        } else {
            assert!(
                !matches!(server.next().await, Some(Ok(Message::Text(_)))),
                "stale ticket sent: {blocked_by}"
            );
            a.quick_view(7, |v| {
                assert_eq!(
                    v.row("song").outcome,
                    (!matches!(blocked_by, "selection" | "present")).then_some(Outcome::NotSent)
                );
            });
        }
    }
}

#[tokio::test]
async fn quick_add_writer_parks_unapplied_ingress_without_blocking_normal_writes() {
    use crate::personal_bridge::PersonalHub;

    let (signals, link, authority, events) = signals();
    let (actual, mut output) = output_state();
    let mut connection = begin(NEW, "", &mut output, &signals);
    let generation = connection.generation;
    let api = Api::new(crate::api::Origin::parse("http://127.0.0.1:1").unwrap()).unwrap();
    let session = api.test_session();
    let (outbound, mut input) = mpsc::channel(OUTBOUND_CAPACITY);
    let mut socket = Socket {
        link,
        authority,
        direct_start: signals.direct_start.clone(),
        events,
        outbound,
        actual,
        resume_sequence: AtomicU64::new(0),
        order_gate: signals.order_gate.clone(),
        personal_applied: signals.personal_applied.clone(),
        session: session.clone(),
        task: tokio::spawn(pending()),
    };
    assert!(matches!(
        socket.events.try_recv().unwrap().kind,
        EventKind::Snapshot { .. }
    ));

    let epoch = 7;
    let hub = PersonalHub::default();
    let personal = hub.access();
    let list = quick_playlist("target");
    let receive = |update| {
        hub.receive(epoch, generation, serde_json::from_value(update).unwrap())
            .unwrap();
    };
    hub.begin(epoch, generation);
    receive(json!({"snapshot":{"playlists":[list.clone()],"recent":[]}}));
    personal.quick_watch(epoch, true, None);
    personal.quick_select(epoch, Some("target")).unwrap();
    receive(json!({"part":{"summary":list,"token":"t","offset":0,"rows":[],"more":false}}));
    let mode = *session.network_changes().borrow();
    let ticket = personal.quick_stage(epoch, mode, "target", "song").unwrap();
    assert!(hub.admit_quick_add(&ticket, || {
        socket
            .personal_quick_add(
                personal.clone(),
                ticket.clone(),
                Arc::new(AtomicU64::new(epoch)),
            )
            .is_ok()
    }));
    signals
        .event(generation, EventKind::Personal(json!({"change":{}})), false)
        .unwrap();
    socket
        .personal(generation, &PersonalCommand::ClearHistory)
        .unwrap();

    let (client, server) = tokio::io::duplex(4096);
    let mut ws = WebSocketStream::from_raw_socket(client, Role::Client, None).await;
    let mut server = WebSocketStream::from_raw_socket(server, Role::Server, None).await;
    let script = async {
        assert_eq!(read_json(&mut server).await["type"], "presence");
        // Only ClearHistory can pass before the actor applies the published
        // event, despite being behind Quick Add in the outbound queue.
        assert_eq!(
            read_json(&mut server).await,
            json!({"type":"personal","command":{"type":"clearHistory"}})
        );
        let event = socket.events.try_recv().unwrap();
        let EventKind::Personal(update) = event.kind else {
            panic!("personal ingress")
        };
        let stamp = event.personal_stamp.unwrap();
        assert_ne!(*signals.personal_applied.borrow(), stamp);
        receive(update);
        socket.mark_personal_applied(stamp);
        assert_eq!(
            read_json(&mut server).await,
            json!({"type":"personal","command":{"type":"addEntry","playlistId":"target","trackId":"song"}})
        );
        assert!(
            time::timeout(Duration::from_millis(50), server.next())
                .await
                .is_err()
        );
        drop(server);
    };
    let (ended, ()) = tokio::join!(
        connection.drive(&mut ws, DEVICE, &mut input, &mut output, &signals),
        script
    );
    assert_eq!(ended, Err(End::Transport));
}

#[tokio::test]
async fn order_admission_rejects_published_updates_and_retired_mode() {
    let (signals, link, authority, events) = signals();
    let (outbound, mut written) = mpsc::channel(2);
    let (actual, _) = watch::channel(None);
    let api = Api::new(crate::api::Origin::parse("http://127.0.0.1:1").unwrap()).unwrap();
    let session = api.test_session();
    let mut socket = Socket {
        link,
        authority,
        direct_start: signals.direct_start.clone(),
        events,
        outbound,
        actual,
        resume_sequence: AtomicU64::new(0),
        order_gate: signals.order_gate.clone(),
        personal_applied: signals.personal_applied.clone(),
        session: session.clone(),
        task: tokio::spawn(pending()),
    };
    let command = || Command::Move { key: 9, after: 0 };
    signals
        .event(2, EventKind::Outputs(Outputs::default()), true)
        .unwrap();
    assert_eq!(socket.reorder(2, 0, command()), Err(SocketError::Busy));
    assert_eq!(
        socket.personal_order(2, 0, &json!({"type":"moveEntry"})),
        Err(SocketError::Busy)
    );
    assert!(written.try_recv().is_err());
    socket.events.try_recv().unwrap();
    session.set_offline(true);
    session.set_offline(false);
    assert_eq!(
        socket.reorder(2, 0, command()),
        Err(SocketError::NotConnected)
    );
    assert_eq!(
        socket.personal_order(2, 0, &json!({"type":"moveEntry"})),
        Err(SocketError::NotConnected)
    );
    assert!(written.try_recv().is_err());
    assert_eq!(socket.reorder(2, 2, command()), Ok(()));
    assert_eq!(
        serde_json::from_str::<Value>(&written.try_recv().unwrap().raw).unwrap(),
        json!({"type":"move","key":9,"after":0})
    );
}

fn output_state() -> (watch::Sender<ActualSample>, OutputState) {
    let (actual, _) = watch::channel(None);
    (
        actual.clone(),
        OutputState {
            lease: None,
            actual,
        },
    )
}

fn begin(connection: &str, owner: &str, output: &mut OutputState, signals: &Signals) -> Connection {
    let first = decode(&snapshot(connection, owner).to_string(), DEVICE).unwrap();
    Connection::begin(first, output, signals).ok().unwrap()
}

#[test]
fn exact_command_hello_fact_and_personal_frames() {
    let hello = Outgoing::<()>::Hello {
        ticket: "one-use",
        device_id: DEVICE,
        name: "Desk",
        can_play: true,
    };
    assert_eq!(
        to_value(hello).unwrap(),
        json!({"type":"hello","ticket":"one-use","name":"Desk","deviceId":DEVICE,"canPlay":true})
    );
    let ctx: Context = serde_json::from_value(
        json!({"kind":"derived","id":"pl:1","name":"Rules","revision":9,"fence":"never upstream"}),
    )
    .unwrap();
    let selection = Selection::Track {
        track: TrackRef {
            id: "track:1".into(),
            target_id: Some("pl:1".into()),
            occurrence_id: Some("occ:1".into()),
            entry_id: None,
        },
        context: Some(ctx),
    };
    let play = Command::Play {
        selection: selection.clone(),
        shuffle: Some(true),
        seed: Some(u32::MAX),
    };
    assert!(play.valid());
    assert_eq!(
        to_value(play).unwrap(),
        json!({"type":"play","shuffle":true,"seed":u32::MAX,
        "selection":{"kind":"track","track":{"id":"track:1","targetId":"pl:1","occurrenceId":"occ:1"},
            "context":{"kind":"derived","id":"pl:1","name":"Rules"}}})
    );
    assert_eq!(
        to_value(Command::Add {
            selection,
            placement: Intent::Next
        })
        .unwrap()["placement"],
        "next"
    );
    assert_eq!(
        to_value(Command::Move { key: 1, after: 0 }).unwrap(),
        json!({"type":"move","key":1,"after":0})
    );
    assert!(
        !Command::Jump {
            key: FIRST_PREVIEW_KEY
        }
        .valid()
    );
    assert!(!Command::Seek { value: f64::NAN }.valid());
    assert_eq!(
        to_value(Command::Release).unwrap(),
        json!({"type":"release"})
    );
    assert_eq!(
        to_value(Outgoing::<()>::Sync).unwrap(),
        json!({"type":"sync"})
    );
    let mut fact = Fact {
        key: 1,
        chain: "fixture-chain".into(),
        visit: 0,
        run: 1,
        elapsed: 173.0,
        playing: false,
        buffering: false,
        ended: true,
        landed: Some(0),
        error: None,
    };
    assert!(fact.valid());
    assert_eq!(
        to_value(Outgoing::<()>::Fact { fact: &fact }).unwrap(),
        json!({"type":"fact","key":1,"chain":"fixture-chain","visit":0,"run":1,"elapsed":173.0,"playing":false,"buffering":false,"ended":true,"landed":0})
    );
    fact.landed = None;
    assert!(!fact.valid());
    fact.ended = false;
    fact.error = Some(FactError::Blocked);
    assert!(fact.valid());
    assert_eq!(
        to_value(Outgoing::<()>::Fact { fact: &fact }).unwrap()["error"],
        "blocked"
    );
    let personal = json!({"type":"syncPlaylist","playlistId":"pl:1","known":"token","offset":40});
    assert_eq!(
        to_value(Outgoing::Personal { command: &personal }).unwrap(),
        json!({"type":"personal","command":personal})
    );
    let oversized = json!({"type":"createPlaylist","name":"x".repeat(MAX_OUTBOUND)});
    assert_eq!(
        encode(&Outgoing::Personal {
            command: &oversized
        }),
        Err(SocketError::Invalid)
    );
}

#[test]
fn boundary_accepts_actual_positive_display_and_go_omissions() {
    let Incoming::Snapshot {
        connection_id,
        player,
        outputs,
    } = decode(&snapshot(NEW, OLD).to_string(), DEVICE).unwrap()
    else {
        panic!("snapshot")
    };
    assert_eq!(connection_id, NEW);
    assert_eq!(outputs.owner, OLD); // detached owner does not appear in roster
    let track = player.current().unwrap();
    assert_eq!(track.key, 1);
    assert!(track.name.len() > 512);
    assert!(track.artists.is_empty());
    assert_eq!(
        track.playback_context.as_ref().unwrap().kind,
        ContextKind::Album
    );
    let closed = json!({"type":"player","player":{"queue":{"kind":"replace"},"cursor":Cursor::default(),"shuffle":false}});
    let Incoming::Player { player } = decode(&closed.to_string(), DEVICE).unwrap() else {
        panic!("player")
    };
    assert!(player.queue.unwrap().rows.is_empty());
    assert_eq!(player.cursor.unwrap().key, 0);
    assert_eq!(player.shuffle, Some(false));
    // server's SetOutput increments Run even when no row is selected.
    assert!(decode(r#"{"type":"player","player":{"cursor":{"key":0,"chain":"","visit":0,"run":2,"elapsed":0,"desired":false,"playing":false}}}"#, DEVICE).is_ok());
    for update in [
        json!({"snapshot":{"playlists":[],"recent":[]}}),
        json!({"change":{}}),
        json!({"part":{"token":"state","offset":0,"rows":[],"more":false}}),
    ] {
        assert!(
            decode(
                &json!({"type":"personal","update":update}).to_string(),
                DEVICE
            )
            .is_ok()
        );
    }
}

#[test]
fn incompatible_frames_and_structural_gaps_are_rejected() {
    for raw in [
        r#"{"type":"error","error":{"code":"unavailable"}}"#,
        r#"{"type":"result","result":{"final":true}}"#,
        r#"{"type":"personal","update":{"snapshot":{},"part":{}}}"#,
        r#"{"type":"personal","update":{"snapshot":null}}"#,
        r#"{"type":"player","player":{"queue":{"kind":"order","keys":[1,1]}}}"#,
        r#"{"type":"player","player":{"queue":{"kind":"order","keys":[1],"rebaseAfter":2}}}"#,
        r#"{"type":"player","player":{"queue":{"kind":"bogus"}}}"#,
        r#"{"type":"player","player":{"shuffle":true,"shuffle":false}}"#,
    ] {
        assert!(matches!(decode(raw, DEVICE), Err(End::Protocol)), "{raw}");
    }
    assert!(matches!(
        decode(&snapshot(NEW, OLD).to_string(), "wrong-device"),
        Err(End::Protocol)
    ));
    let mut wrong_cursor = snapshot(NEW, OLD);
    wrong_cursor["player"]["cursor"]["key"] = 2.into();
    assert!(matches!(
        decode(&wrong_cursor.to_string(), DEVICE),
        Err(End::Protocol)
    ));
}

#[test]
fn grant_must_match_a_sent_basis_and_cannot_remake_or_extend_it() {
    let time = Instant::now();
    let mut pulses = Pulses::default();
    let older = pulses.next(time).unwrap();
    pulses.written_resume = 3;
    let newer = pulses.next(time + Duration::from_secs(1)).unwrap();
    pulses.written_resume = 7; // written after this presence; not covered by its grant
    assert_eq!(pulses.accept(92, 8000, "handle", time), None);
    assert_eq!(pulses.accept(newer, 15_001, "handle", time), None);
    assert_eq!(pulses.accept(newer, 8000, "", time), None);
    assert_eq!(
        pulses.accept(newer, 15_000, "handle", time + Duration::from_secs(3)),
        Some(time + Duration::from_secs(16))
    );
    assert_eq!(pulses.seen_resume, 3);
    assert_eq!(pulses.accept(older, 8000, "handle", time), None);
    assert_eq!(pulses.accept(newer, 8000, "handle", time), None);
    let fresh = pulses.next(time + Duration::from_secs(2)).unwrap();
    assert_eq!(
        pulses.accept(fresh, 8000, "handle", time + Duration::from_secs(10)),
        None
    );
}

#[test]
fn event_queue_gap_demands_new_generation_and_fresh_snapshot() {
    let (signals, link, _, mut events) = signals();
    let (_, mut output) = output_state();
    let mut conn = begin(OLD, OLD, &mut output, &signals);
    let old_gen = conn.generation;
    for _ in 1..EVENT_CAPACITY {
        let frame = decode(r#"{"type":"player","player":{"shuffle":false}}"#, DEVICE).unwrap();
        assert_eq!(conn.receive(frame, &mut output, &signals), Ok(false));
    }
    let frame = decode(r#"{"type":"player","player":{"repeat":"all"}}"#, DEVICE).unwrap();
    assert_eq!(
        conn.receive(frame, &mut output, &signals),
        Err(End::Backpressure)
    );
    // Reconnect cannot announce an incarnation without space for its snapshot.
    signals.link.send_replace(Link::Disconnected);
    let next = decode(&snapshot(NEW, "").to_string(), DEVICE).unwrap();
    assert!(matches!(
        Connection::begin(next, &mut output, &signals),
        Err(End::Backpressure)
    ));
    assert_eq!(*link.borrow(), Link::Disconnected);
    assert!(matches!(
        events.try_recv().unwrap().kind,
        EventKind::Snapshot { .. }
    ));
    while let Ok(event) = events.try_recv() {
        assert_eq!(event.generation, old_gen);
    }
    let mut conn = begin(NEW, "", &mut output, &signals);
    assert!(conn.generation > old_gen);
    assert_eq!(*link.borrow(), Link::Connected(conn.generation));
    let event = events.try_recv().unwrap();
    assert_eq!(event.generation, conn.generation);
    assert!(
        matches!(event.kind, EventKind::Snapshot { connection_id, .. } if connection_id == NEW)
    );
    // A sync snapshot is a reset within the same generation, never a new connection.
    assert_eq!(
        conn.receive(
            decode(&snapshot(NEW, "").to_string(), DEVICE).unwrap(),
            &mut output,
            &signals
        ),
        Ok(false)
    );
    assert_eq!(events.try_recv().unwrap().generation, conn.generation);
    assert_eq!(
        conn.receive(
            decode(&snapshot(OLD, "").to_string(), DEVICE).unwrap(),
            &mut output,
            &signals
        ),
        Err(End::Protocol)
    );
}

async fn read_json(peer: &mut WebSocketStream<tokio::io::DuplexStream>) -> Value {
    let msg = time::timeout(Duration::from_secs(2), peer.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    serde_json::from_str(msg.to_text().unwrap()).unwrap()
}

async fn send_json(peer: &mut WebSocketStream<tokio::io::DuplexStream>, data: Value) {
    peer.send(Message::Text(data.to_string().into()))
        .await
        .unwrap();
}

#[tokio::test]
async fn fresh_process_reattach_waits_for_its_own_grant_and_exports_no_cursor() {
    let (signals, _, authority, _events) = signals();
    let (_, mut output) = output_state();
    let mut conn = begin(NEW, OLD, &mut output, &signals);
    let (client, peer) = tokio::io::duplex(1024);
    let mut client = WebSocketStream::from_raw_socket(client, Role::Client, None).await;
    let mut peer = WebSocketStream::from_raw_socket(peer, Role::Server, None).await;
    conn.pulse(&mut client, &output).await.unwrap();
    let pulse = read_json(&mut peer).await;
    // A fresh process never fabricates proof of the old handle or actual
    // cursor. Going through the same device's grace is Go's decision.
    assert_eq!(pulse, json!({"type":"presence","basis":1}));
    let grant = || Incoming::Grant {
        basis: 1,
        ms: 8000,
        handle: "server-handle".into(),
    };
    conn.receive(grant(), &mut output, &signals).unwrap();
    assert!(authority.borrow().is_none());
    conn.receive(
        decode(&snapshot(NEW, NEW).to_string(), DEVICE).unwrap(),
        &mut output,
        &signals,
    )
    .unwrap();
    assert!(authority.borrow().is_none());
    conn.receive(grant(), &mut output, &signals).unwrap();
    assert!(
        authority
            .borrow()
            .is_some_and(|a| a.attached && a.generation == conn.generation)
    );
    assert_eq!(
        conn.canonical,
        serde_json::from_value(cursor_json()).unwrap()
    );
}

#[tokio::test]
async fn framed_reconnect_imports_latest_samples_but_never_replays_old_choices() {
    let (signals, link, authority, mut events) = signals();
    let (actual, mut output) = output_state();
    let mut old = begin(OLD, OLD, &mut output, &signals);
    events.try_recv().unwrap();
    let old_basis = old.pulses.next(Instant::now()).unwrap();
    assert_eq!(
        old.receive(
            Incoming::Grant {
                basis: old_basis,
                ms: 8000,
                handle: "old-handle".into()
            },
            &mut output,
            &signals
        ),
        Ok(false)
    );
    let gen1 = old.generation;
    let base = old.canonical.clone();
    output.detach(gen1, base.clone(), &signals);
    assert!(!authority.borrow().unwrap().attached);
    let sample = Cursor {
        elapsed: 6.5,
        playing: true,
        ..base.clone()
    };
    actual.send_replace(Some((gen1, sample.clone())));

    let mut next = begin(NEW, OLD, &mut output, &signals);
    let gen2 = next.generation;
    assert_ne!(gen1, gen2);
    assert_eq!(*link.borrow(), Link::Connected(gen2));
    let (input_tx, mut input) = mpsc::channel(OUTBOUND_CAPACITY);
    // Retain a stale frame to exercise the writer's final generation fence
    // independently of the normal queue drain on disconnect.
    input_tx
        .try_send(WriteFrame {
            generation: gen1,
            class: Class::Choice,
            resume: None,
            at: Instant::now(),
            raw: encode(&Command::Next).unwrap(),
            scoped: None,
        })
        .ok()
        .unwrap();
    let (client, server) = tokio::io::duplex(8192);
    let mut ws = WebSocketStream::from_raw_socket(client, Role::Client, None).await;
    let mut server = WebSocketStream::from_raw_socket(server, Role::Server, None).await;
    let mut server_authority = authority.clone();
    let attached_input = input_tx.clone();
    let server_script = async move {
        let pulse = read_json(&mut server).await;
        assert_eq!(pulse["type"], "presence");
        assert_eq!(pulse["handle"], "old-handle");
        assert_eq!(pulse["base"], to_value(&base).unwrap());
        assert_eq!(pulse["actual"], to_value(&sample).unwrap());
        let mut outputs = snapshot(NEW, NEW);
        outputs.as_object_mut().unwrap().remove("player");
        outputs.as_object_mut().unwrap().remove("connectionId");
        outputs["type"] = "outputs".into();
        send_json(
            &mut server,
            json!({"type":"player","player":{"cursor":sample}}),
        )
        .await;
        send_json(&mut server, outputs).await;
        send_json(
            &mut server,
            json!({"type":"grant","basis":pulse["basis"],"ms":8000,"handle":"rotated-handle"}),
        )
        .await;
        // A matched grant from this connection is the explicit handshake
        // barrier; do not schedule the valid command by timing or task order.
        drop(
            time::timeout(
                Duration::from_secs(2),
                server_authority
                    .wait_for(|a| a.is_some_and(|a| a.generation == gen2 && a.attached)),
            )
            .await
            .unwrap()
            .unwrap(),
        );
        attached_input
            .try_send(WriteFrame {
                generation: gen2,
                class: Class::Choice,
                resume: None,
                at: Instant::now(),
                raw: encode(&Command::Pause).unwrap(),
                scoped: None,
            })
            .ok()
            .unwrap();
        // The first choice must be Pause, never the queued old-generation Next.
        assert_eq!(read_json(&mut server).await, json!({"type":"pause"}));
        send_json(
            &mut server,
            json!({"type":"personal","update":{"snapshot":{"playlists":[],"recent":[]}}}),
        )
        .await;
        // Dropping raw transport matches Go's CloseNow (no WS close frame).
    };
    let (ended, ()) = tokio::join!(
        next.drive(&mut ws, DEVICE, &mut input, &mut output, &signals),
        server_script
    );
    assert_eq!(ended, Err(End::Transport));
    assert_eq!(authority.borrow().unwrap().generation, gen2);
    assert!(authority.borrow().unwrap().attached);
    assert_eq!(output.lease.as_ref().unwrap().handle, "rotated-handle");
    let kinds: Vec<_> = std::iter::from_fn(|| events.try_recv().ok())
        .map(|e| e.kind)
        .collect();
    assert!(matches!(
        &kinds[..],
        [
            EventKind::Snapshot { .. },
            EventKind::Player(_),
            EventKind::Outputs(_),
            EventKind::Personal(_)
        ]
    ));
    // A deliberately empty owner revokes; release is not fabricated by transport.
    let msg = Incoming::Outputs {
        outputs: Outputs::default(),
    };
    assert_eq!(next.receive(msg, &mut output, &signals), Ok(false));
    assert!(authority.borrow().is_none());
}

#[test]
fn a_wire_output_correction_fences_direct_pcm_before_the_actor_consumes_it() {
    let (signals, _, _, mut events) = signals();
    let (_, mut output) = output_state();
    let mut conn = begin(NEW, "", &mut output, &signals);
    let _ = events.try_recv().unwrap();
    let direct = signals.direct_start.stamp().unwrap();
    let outputs = |owner: &str, competitor: bool| {
        let mut devices = vec![json!({"id":NEW,"deviceId":DEVICE,"name":"Desk","canPlay":true})];
        if competitor {
            devices.push(json!({"id":"phone","deviceId":"phone","name":"Phone","canPlay":true}));
        }
        decode(
            &json!({"type":"outputs","owner":owner,"devices":devices}).to_string(),
            DEVICE,
        )
        .unwrap()
    };
    conn.receive(outputs(NEW, false), &mut output, &signals)
        .ok()
        .unwrap();
    assert_eq!(
        signals.direct_start.stamp(),
        Some(direct),
        "vacancy election preserves the sink"
    );
    let _ = events.try_recv().unwrap();
    conn.receive(outputs("", true), &mut output, &signals)
        .ok()
        .unwrap();
    assert_ne!(signals.direct_start.stamp(), Some(direct));
    assert_eq!(
        events.len(),
        1,
        "native fence did not depend on actor consuming Outputs"
    );
    let recent = signals.direct_start.stamp().unwrap();
    signals.direct_start.set_expired(true);
    assert_eq!(signals.direct_start.stamp(), None);
    signals.direct_start.set_expired(false);
    assert_ne!(
        signals.direct_start.stamp(),
        Some(recent),
        "a renewal does not resurrect stopped speculative PCM"
    );
}

#[test]
fn local_release_fences_a_grant_already_in_flight() {
    let (signals, _link, authority, _events) = signals();
    let (_, mut output) = output_state();
    let mut conn = begin(OLD, OLD, &mut output, &signals);
    let accepted = conn.pulses.next(Instant::now()).unwrap();
    let inflight = conn.pulses.next(Instant::now()).unwrap();
    let grant = |basis| Incoming::Grant {
        basis,
        ms: 8000,
        handle: "private-handle".into(),
    };
    conn.receive(grant(accepted), &mut output, &signals)
        .unwrap();
    assert!(authority.borrow().is_some());
    conn.release(&mut output, &signals);
    conn.receive(grant(inflight), &mut output, &signals)
        .unwrap();
    assert!(authority.borrow().is_none());
    assert!(conn.releasing);
    conn.receive(
        Incoming::Outputs {
            outputs: Outputs::default(),
        },
        &mut output,
        &signals,
    )
    .unwrap();
    assert!(!conn.releasing);
}

#[test]
fn detached_handle_without_an_exportable_cursor_never_imports_stale_data() {
    let (signals, _link, authority, _events) = signals();
    let (actual, mut output) = output_state();
    let mut old = begin(OLD, OLD, &mut output, &signals);
    let basis = old.pulses.next(Instant::now()).unwrap();
    old.receive(
        Incoming::Grant {
            basis,
            ms: 8000,
            handle: "ticket-like-secret".into(),
        },
        &mut output,
        &signals,
    )
    .unwrap();
    let mut unrelated = old.canonical.clone();
    unrelated.chain = "unrelated".into();
    actual.send_replace(Some((old.generation, unrelated)));
    output.detach(old.generation, old.canonical, &signals);
    assert!(matches!(output.attach_frame(OLD), Some((_, None))));
    assert!(output.attach_frame(NEW).is_none());
    output.lease.as_mut().unwrap().deadline = Instant::now();
    output.expire(&signals);
    assert!(authority.borrow().is_none());
    assert!(actual.borrow().is_none());
}
