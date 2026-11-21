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
            link: link_tx,
            authority: authority_tx,
            events: event_tx,
        },
        link,
        authority,
        events,
    )
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
    let newer = pulses.next(time + Duration::from_secs(1)).unwrap();
    assert_eq!(pulses.accept(92, 8000, "handle", time), None);
    assert_eq!(pulses.accept(newer, 15_001, "handle", time), None);
    assert_eq!(pulses.accept(newer, 8000, "", time), None);
    assert_eq!(
        pulses.accept(newer, 15_000, "handle", time + Duration::from_secs(3)),
        Some(time + Duration::from_secs(16))
    );
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
            at: Instant::now(),
            raw: encode(&Command::Next).unwrap(),
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
                at: Instant::now(),
                raw: encode(&Command::Pause).unwrap(),
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
