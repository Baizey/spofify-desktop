use super::*;
use serde_json::{json, to_value};
use std::sync::atomic::AtomicUsize;
use tokio::io::DuplexStream;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, frame::coding::CloseCode};

const DEVICE: &str = "desk-fixture";
const ROOM: &str = "fixture-room";
const OLD: &str = "old-connection";
const NEW: &str = "new-connection";
const OWNER_GENERATION: u64 = 12;

type Peer = WebSocketStream<DuplexStream>;

fn cursor_json() -> Value {
    json!({"key":1,"occurrence":"fixture-visit","positionMs":3000,"desiredPlaying":true})
}

fn snapshot(connection: &str, owner: Option<&str>) -> Value {
    let owner = owner.map_or_else(
        || json!({"generation":OWNER_GENERATION}),
        |id| json!({"generation":OWNER_GENERATION,"deviceId":id}),
    );
    json!({
        "type":"snapshot", "sessionId":ROOM, "connectionId":connection, "revision":8,
        "owner":owner, "serverTimeMs":1000,
        "devices":[{"deviceId":DEVICE,"name":"Desk","canPlay":true}],
        "player":{"queue":[{"id":"track:1","name":"display".repeat(280),"artists":null,
            "albumName":"Album","albumId":"album:1","duration":181.0,
            "key":1,"order":1,"intent":"low","displayFuture":true,
            "playbackContext":{"kind":"album","id":"album:1","name":"Album","revision":9,"fence":"opaque"}}],
            "cursor":cursor_json(),"repeat":"off","shuffle":false,"collectionFirst":1}
    })
}

fn initial(id: &str, owner: Option<&str>) -> Incoming {
    decode(&snapshot(id, owner).to_string(), DEVICE).unwrap()
}

fn basis() -> CommandBasis {
    CommandBasis {
        session_id: ROOM.into(),
        generation: OWNER_GENERATION,
        revision: 8,
        occurrence: Some("fixture-visit".into()),
        key: Some(1),
        successor_key: None,
    }
}

fn device() -> DeviceIdentity {
    DeviceIdentity {
        device_id: DEVICE.into(),
        name: "Desk".into(),
        can_play: true,
    }
}

fn fixture() -> (Socket, Signals, mpsc::Receiver<WriteFrame>) {
    let api = Api::new(crate::api::Origin::parse("http://127.0.0.1:1").unwrap()).unwrap();
    let session = api.test_session();
    let (link_tx, link) = watch::channel(Link::Connecting);
    let (owner_tx, last_owner) = watch::channel(None);
    let (event_tx, events) = mpsc::channel(EVENT_CAPACITY);
    let (outbound, input) = mpsc::channel(OUTBOUND_CAPACITY);
    let signals = Signals {
        order_gate: Arc::default(),
        personal_applied: watch::channel(0).0,
        link: link_tx,
        last_owner: owner_tx,
        binding: Arc::default(),
        events: event_tx,
    };
    let socket = Socket {
        link,
        events,
        last_owner,
        outbound,
        order_gate: signals.order_gate.clone(),
        personal_applied: signals.personal_applied.clone(),
        binding: signals.binding.clone(),
        session,
        task: tokio::spawn(pending()),
    };
    (socket, signals, input)
}

async fn peers() -> (Peer, Peer) {
    let (client, server) = tokio::io::duplex(4096);
    let client = WebSocketStream::from_raw_socket(client, Role::Client, None).await;
    let server = WebSocketStream::from_raw_socket(server, Role::Server, None).await;
    (client, server)
}

async fn read_raw_json(peer: &mut Peer) -> Value {
    let msg = peer.next().await.unwrap().unwrap();
    serde_json::from_str(msg.to_text().unwrap()).unwrap()
}

async fn read_json(peer: &mut Peer) -> Value {
    loop {
        let frame = read_raw_json(peer).await;
        if frame["type"] != "ping" {
            return frame;
        }
    }
}

async fn send_json(peer: &mut Peer, data: Value) {
    peer.send(Message::Text(data.to_string().into()))
        .await
        .unwrap();
}

fn quick_playlist(id: &str) -> Value {
    json!({"id":id, "name":"Mix", "description":"", "kind":"manual", "countState":"exact",
        "trackCount":0, "availableCount":0, "revision":1,
        "createdAt":"2026-08-03T00:00:00Z", "updatedAt":"2026-08-03T00:00:00Z",
        "capabilities":{"editMetadata":true,"editEntries":true,"editDefinition":false,
            "play":true,"copyToManual":false,"delete":true,"addTracks":true}})
}

#[test]
fn hello_preserves_detached_cursor_and_catalog_provenance_and_omits_empty_claim() {
    let mut d = device();
    let ticket = "a".repeat(64);
    let read = |raw: String| serde_json::from_str::<Value>(&raw).unwrap();
    let plain =
        json!({"type":"hello","ticket":ticket,"deviceId":DEVICE,"name":"Desk","canPlay":true});
    let mut claim = Claim {
        local: Some(State::default()),
        reattach_basis: None,
    };
    assert_eq!(
        read(encode_hello(&ticket, &d, Some(&claim)).unwrap()),
        plain
    );
    let Incoming::Snapshot(Snapshot { mut player, .. }) = initial(OLD, Some(DEVICE)) else {
        unreachable!()
    };
    player.cursor.position_ms = 8001;
    claim.local = Some(player);
    claim.reattach_basis = Some(ReattachBasis {
        session_id: ROOM.into(),
        generation: OWNER_GENERATION,
        revision: 8,
        occurrence: Some("fixture-visit".into()),
    });
    let hello = read(encode_hello(&ticket, &d, Some(&claim)).unwrap());
    assert_eq!(
        hello["local"]["cursor"],
        json!({"key":1,"occurrence":"fixture-visit","positionMs":8001,"desiredPlaying":true})
    );
    assert_eq!(hello["local"]["queue"][0]["artists"], json!([]));
    assert_eq!(
        hello["local"]["queue"][0]["playbackContext"],
        json!({"kind":"album","id":"album:1","name":"Album","revision":9,"fence":"opaque"})
    );
    assert_eq!(
        hello["reattachBasis"],
        json!({"sessionId":ROOM,"generation":12,"revision":8,"occurrence":"fixture-visit"})
    );
    d.can_play = false;
    let mut plain = plain;
    plain["canPlay"] = false.into();
    assert_eq!(
        read(encode_hello(&ticket, &d, Some(&claim)).unwrap()),
        plain
    );
}

#[test]
fn large_hello_has_a_separate_budget_without_truncating_rows() {
    let Incoming::Snapshot(Snapshot { mut player, .. }) = initial(OLD, None) else {
        unreachable!()
    };
    // The original fixture's deliberately long display name, replicated 999
    // times with row metadata, exceeded Go's 2 MiB player budget.
    let row = player.queue[0].clone();
    for key in 2..900 {
        player.queue.push(Track {
            key,
            order: key,
            ..row.clone()
        });
    }
    // Hit the actual player budget exactly; the hello envelope may be larger.
    let player_bytes = serde_json::to_vec(&player).unwrap().len();
    assert!((MAX_OUTBOUND..MAX_STATE).contains(&player_bytes));
    player
        .queue
        .last_mut()
        .unwrap()
        .name
        .push_str(&"w".repeat(MAX_STATE - player_bytes));
    assert_eq!(serde_json::to_vec(&player).unwrap().len(), MAX_STATE);
    let mut claim = Claim {
        local: Some(player),
        reattach_basis: None,
    };
    let ticket = "b".repeat(64);
    let hello = encode_hello(&ticket, &device(), Some(&claim)).unwrap();
    assert!(hello.len() > MAX_STATE && hello.len() < MAX_HELLO);
    let hello: Value = serde_json::from_str(&hello).unwrap();
    let queue = hello["local"]["queue"].as_array().unwrap();
    assert_eq!(queue.len(), 899);
    assert_eq!(queue.last().unwrap()["key"], 899);
    assert_eq!(encode(&hello), Err(SocketError::Invalid));
    claim
        .local
        .as_mut()
        .unwrap()
        .queue
        .last_mut()
        .unwrap()
        .name
        .push('w');
    assert_eq!(
        encode_hello(&ticket, &device(), Some(&claim)),
        Err(SocketError::Invalid)
    );
}

#[test]
fn exact_v3_frame_shapes_and_incompatible_values() {
    let command = Command {
        id: format!("{NEW}:2"),
        issued_at_ms: 54000,
        basis: basis(),
        action: Action::Shuffle {
            shuffle: false,
            seed: Some(0),
        },
    };
    let expected = json!({"type":"command","id":"new-connection:2","issuedAtMs":54000,
        "basis":{"sessionId":ROOM,"generation":12,"revision":8,"occurrence":"fixture-visit","key":1},
        "action":{"type":"shuffle","shuffle":false,"seed":0}});
    assert_eq!(to_value(&command).unwrap(), expected);
    let state = json!({"type":"state","generation":12,"revision":9,"delta":{"shuffle":false},"command":expected});
    let Incoming::State(s) = decode(&state.to_string(), DEVICE).unwrap() else {
        unreachable!()
    };
    assert_eq!(s.command, Some(command));
    assert_eq!(s.delta.shuffle, Some(false));
    assert!(matches!(
        decode(
            &json!({"type":"ownership","owner":{"generation":2},"devices":[]}).to_string(),
            DEVICE
        ),
        Ok(Incoming::Ownership(_))
    ));
    assert!(matches!(
        decode(
            &json!({"type":"observation","generation":12,"revision":8,
        "observation":{"key":0,"positionMs":0,"sampledAtMs":7000,"status":"stopped"}})
            .to_string(),
            DEVICE
        ),
        Ok(Incoming::Observation(_))
    ));
    let absent = json!({"type":"personal","update":{"missing":{"playlistId":"fixture-playlist"}}});
    let Incoming::Personal { update } = decode(&absent.to_string(), DEVICE).unwrap() else {
        panic!("expected scoped account reply");
    };
    assert_eq!(serde_json::from_value::<PersonalUpdate>(update).unwrap(),
        PersonalUpdate::Missing { playlist_id: "fixture-playlist".into() });
    for payload in [json!({"playlistId":""}), json!({"playlist_id":"p"}), json!({"playlistId":"p","offset":1})] {
        assert!(serde_json::from_value::<PersonalUpdate>(json!({"missing":payload})).is_err());
    }
    for invalid in [
        json!({"type":"player","player":{"cursor":cursor_json()}}),
        json!({"type":"grant","basis":1,"ms":15000,"handle":"legacy"}),
        json!({"type":"state","generation":12,"revision":9,"delta":{}}),
        json!({"type":"state","generation":12,"revision":9,"delta":{"repeat":"all"},"command":null}),
        json!({"type":"state","generation":12,"revision":9,"delta":{"cursor":{"key":1,"positionMs":0,"desiredPlaying":true}}}),
        json!({"type":"state","generation":12,"revision":MAX_SAFE_INTEGER+1,"delta":{"repeat":"all"}}),
        json!({"type":"observation","generation":12,"revision":8,"observation":{"key":0,"occurrence":null,"positionMs":0,"sampledAtMs":1,"status":"stopped"}}),
        json!({"type":"observation","generation":12,"revision":8,"observation":{"key":0,"positionMs":0.5,"sampledAtMs":1,"status":"stopped"}}),
        json!({"type":"ownership","owner":{"generation":2,"deviceId":null},"devices":[]}),
        json!({"type":"result","id":"new-connection:0","status":"accepted","revision":1}),
        json!({"type":"personal","update":{"snapshot":{},"part":{}}}),
        json!({"type":"personal","update":{"missing":{"playlistId":"p"},"part":null}}),
        json!({"type":"personal","update":{"missing":null}}),
    ] {
        assert!(
            matches!(decode(&invalid.to_string(), DEVICE), Err(End::Protocol)),
            "{invalid}"
        );
    }
}

#[test]
fn play_selection_keeps_catalog_occurrence_separate_from_playback_visit() {
    let mut command = Command {
        id: format!("{NEW}:1"),
        issued_at_ms: 54000,
        basis: basis(),
        action: Action::Play {
            selection: Selection::Track {
                track: TrackRef {
                    id: "recording/音".into(),
                    entry_id: None,
                    target_id: Some("derived/target".into()),
                    occurrence_id: Some("catalog-occurrence".into()),
                },
                context: Some(Context {
                    kind: ContextKind::Derived,
                    id: "derived/target".into(),
                    name: "Mix".into(),
                }),
            },
            shuffle: Some(false),
            seed: Some(0),
        },
    };
    assert!(command.valid());
    let value = to_value(&command).unwrap();
    assert_eq!(value["basis"]["occurrence"], "fixture-visit");
    assert_eq!(
        value["action"],
        json!({"type":"play","shuffle":false,"seed":0,"selection":{
            "kind":"track", "track":{"id":"recording/音","targetId":"derived/target","occurrenceId":"catalog-occurrence"},
            "context":{"kind":"derived","id":"derived/target","name":"Mix"}
        }})
    );
    let Action::Play {
        selection: Selection::Track { track, .. },
        ..
    } = &mut command.action
    else {
        unreachable!()
    };
    track.target_id = Some("unrelated".into());
    assert!(!command.valid());
}

fn report() -> Report {
    Report {
        generation: OWNER_GENERATION,
        revision: 8,
        clock_offset_ms: Some(-110),
        observation: Observation {
            key: 1,
            occurrence: Some("fixture-visit".into()),
            position_ms: 4000,
            sampled_at_ms: 20000,
            status: ObservationStatus::Playing,
        },
        advance: None,
        settlements: vec![],
    }
}

#[test]
fn conditional_seek_uses_the_shared_execute_settlement_and_commit_frames() {
    let command = json!({
        "type":"command", "id":"remote:3", "issuedAtMs":54_000,
        "basis":{"sessionId":ROOM,"generation":12,"revision":8,"occurrence":"fixture-visit","key":1},
        "action":{"type":"seek","positionMs":4500}
    });
    let mut execute = json!({"type":"execute","generation":12,"revision":8,"command":command});
    let Incoming::Execute(e) = decode(&execute.to_string(), DEVICE).unwrap() else {
        panic!("conditional Seek must decode as owner execute");
    };
    assert_eq!(e.generation, OWNER_GENERATION);
    assert_eq!(e.revision, 8);
    assert_eq!(e.command.action, Action::Seek { position_ms: 4500 });
    assert_eq!(to_value(&e.command).unwrap(), command);

    let cursor = Cursor {
        key: 1,
        occurrence: Some("fixture-visit".into()),
        position_ms: 4500,
        desired_playing: true,
    };
    let mut r = report();
    r.observation.position_ms = cursor.position_ms;
    r.settlements.push(Settlement {
        id: e.command.id,
        status: SettlementStatus::Applied,
        cursor: Some(cursor.clone()),
    });
    assert!(r.valid());
    assert_eq!(
        to_value(Outgoing::<()>::Report {
            sequence: 1,
            report: &r
        })
        .unwrap(),
        json!({
            "type":"report","generation":12,"revision":8,"sequence":1,"clockOffsetMs":-110,
            "observation":{"key":1,"occurrence":"fixture-visit","positionMs":4500,"sampledAtMs":20000,"status":"playing"},
            "settlements":[{"id":"remote:3","status":"applied","cursor":{"key":1,"occurrence":"fixture-visit","positionMs":4500,"desiredPlaying":true}}]
        })
    );

    let commit = json!({"type":"state","generation":12,"revision":9,"delta":{"cursor":cursor},"command":command});
    let Incoming::State(s) = decode(&commit.to_string(), DEVICE).unwrap() else {
        panic!("conditional Seek commit is an ordinary state frame");
    };
    assert_eq!(s.revision, 9);
    assert_eq!(s.delta.cursor, Some(cursor));
    assert_eq!(to_value(s.command.unwrap()).unwrap(), command);

    for action in [json!({"type":"pause"}), json!({"type":"seek","value":4.5})] {
        execute["command"]["action"] = action;
        assert!(matches!(
            decode(&execute.to_string(), DEVICE),
            Err(End::Protocol)
        ));
    }
}

#[tokio::test]
async fn framed_v3_command_owner_execute_and_explicit_report_writes() {
    let (mut socket, signals, mut input) = fixture();
    let mut connection = Connection::begin(initial(NEW, Some(DEVICE)), DEVICE, &signals).unwrap();
    let generation = connection.generation;
    let session = socket.session.clone();
    let first = socket.events.try_recv().unwrap();
    assert_eq!(first.generation, generation);
    assert!(matches!(
        first.kind,
        EventKind::Snapshot(Snapshot {
            owner: Owner {
                generation: OWNER_GENERATION,
                ..
            },
            ..
        })
    ));
    let (mut client, mut server) = peers().await;
    let script = async {
        let mut next_basis = basis();
        next_basis.successor_key = Some(2);
        let id = socket
            .command(generation, next_basis, Action::Next)
            .unwrap();
        assert_eq!(id, "new-connection:1");
        let command = read_json(&mut server).await;
        assert_eq!(command["id"], id);
        assert_eq!(command["basis"]["successorKey"], 2);
        assert_eq!(command["action"], json!({"type":"next"}));
        send_json(
            &mut server,
            json!({"type":"result","id":id,"status":"accepted","revision":8}),
        )
        .await;
        send_json(
            &mut server,
            json!({"type":"execute","generation":12,"revision":8,"command":command}),
        )
        .await;
        let accepted = socket.events.recv().await.unwrap();
        assert!(matches!(
            accepted.kind,
            EventKind::Result(CommandResult {
                status: ResultStatus::Accepted,
                ..
            })
        ));
        let EventKind::Execute(execute) = socket.events.recv().await.unwrap().kind else {
            panic!("owner execute")
        };
        assert_eq!(execute.command.id, id);
        let cursor = Cursor {
            key: 2,
            occurrence: Some("fresh-visit".into()),
            position_ms: 0,
            desired_playing: true,
        };
        let mut r = report();
        r.settlements.push(Settlement {
            id,
            status: SettlementStatus::Applied,
            cursor: Some(cursor),
        });
        socket.report(generation, r).unwrap();
        assert_eq!(
            read_json(&mut server).await,
            json!({
                "type":"report","generation":12,"revision":8,"sequence":1,"clockOffsetMs":-110,
                "observation":{"key":1,"occurrence":"fixture-visit","positionMs":4000,"sampledAtMs":20000,"status":"playing"},
                "settlements":[{"id":"new-connection:1","status":"applied","cursor":{"key":2,"occurrence":"fresh-visit","positionMs":0,"desiredPlaying":true}}]
            })
        );
        socket.refresh(generation, OWNER_GENERATION).unwrap();
        assert_eq!(
            read_json(&mut server).await,
            json!({"type":"refresh","generation":12})
        );
        let mut r = report();
        r.clock_offset_ms = None;
        r.observation.status = ObservationStatus::Stopped;
        r.observation.position_ms = 0;
        r.advance = Some(Advance {
            from_occurrence: "fixture-visit".into(),
            from_key: 1,
            to_key: 0,
            to_occurrence: None,
            ended_position_ms: 181_000,
        });
        socket.report(generation, r).unwrap();
        let frame = read_json(&mut server).await;
        assert_eq!(frame["sequence"], 2);
        assert!(frame.get("clockOffsetMs").is_none());
        assert_eq!(frame["observation"]["status"], "stopped");
        assert_eq!(frame["observation"]["positionMs"], 0);
        assert_eq!(
            frame["advance"],
            json!({"fromOccurrence":"fixture-visit","fromKey":1,"toKey":0,"endedPositionMs":181000})
        );
        let mut demoted = snapshot(NEW, Some("other"));
        demoted["owner"]["generation"] = 13.into();
        send_json(&mut server, demoted).await;
        assert!(matches!(
            socket.events.recv().await.unwrap().kind,
            EventKind::Snapshot(_)
        ));
        assert_eq!(
            socket.report(generation, report()),
            Err(SocketError::NotConnected)
        );
        assert_eq!(
            socket.refresh(generation, OWNER_GENERATION),
            Err(SocketError::NotConnected)
        );
        send_json(
            &mut server,
            json!({"type":"offer","id":"source:4","generation":13}),
        )
        .await;
        let EventKind::Offer(offer) = socket.events.recv().await.unwrap().kind else {
            panic!("offer")
        };
        socket.accept(generation, offer).unwrap();
        assert_eq!(
            read_json(&mut server).await,
            json!({"type":"accept","id":"source:4","generation":13})
        );
        socket.rename(generation, DEVICE, "Renamed").unwrap();
        assert_eq!(
            read_json(&mut server).await,
            json!({"type":"rename","deviceId":DEVICE,"name":"Renamed"})
        );
        socket.sync(generation).unwrap();
        assert_eq!(read_json(&mut server).await, json!({"type":"sync"}));
        drop(server);
    };
    let (result, ()) = time::timeout(Duration::from_secs(1), async {
        tokio::join!(
            connection.drive(&mut client, DEVICE, &mut input, &signals, &session, 0),
            script
        )
    })
    .await
    .unwrap();
    assert_eq!(result, Err(End::Transport));
}

fn reattach_basis() -> ReattachBasis {
    ReattachBasis {
        session_id: ROOM.into(),
        generation: OWNER_GENERATION,
        revision: 8,
        occurrence: Some("fixture-visit".into()),
    }
}

fn paused_claim_cursor() -> Cursor {
    Cursor {
        key: 1,
        occurrence: Some("fixture-visit".into()),
        position_ms: 4500,
        desired_playing: false,
    }
}

#[tokio::test]
async fn reconcile_claim_uses_exact_frame_and_shared_command_send_order() {
    let (socket, signals, mut input) = fixture();
    let mut connection = Connection::begin(initial(NEW, Some(DEVICE)), DEVICE, &signals).unwrap();
    let transport = connection.generation;
    let mut wrong = reattach_basis();
    wrong.revision -= 1;
    assert_eq!(
        socket.reconcile_claim(transport, wrong, paused_claim_cursor()),
        Err(SocketError::Invalid)
    );
    let mut wrong = reattach_basis();
    wrong.occurrence = Some("another-visit".into());
    assert_eq!(
        socket.reconcile_claim(transport, wrong, paused_claim_cursor()),
        Err(SocketError::Invalid)
    );
    let mut wrong = reattach_basis();
    wrong.generation -= 1;
    assert_eq!(
        socket.reconcile_claim(transport, wrong, paused_claim_cursor()),
        Err(SocketError::NotConnected)
    );

    assert_eq!(
        socket.command(transport, basis(), Action::Pause).unwrap(),
        "new-connection:1"
    );
    assert_eq!(
        socket
            .reconcile_claim(transport, reattach_basis(), paused_claim_cursor())
            .unwrap(),
        "new-connection:2"
    );
    assert_eq!(
        socket.reconcile_claim(transport, reattach_basis(), paused_claim_cursor()),
        Err(SocketError::Invalid)
    );
    assert_eq!(
        socket.command(transport, basis(), Action::Resume).unwrap(),
        "new-connection:3"
    );

    let (mut client, mut server) = peers().await;
    let script = async move {
        let first = read_json(&mut server).await;
        assert_eq!(first["type"], "command");
        assert_eq!(first["id"], "new-connection:1");
        assert_eq!(
            read_json(&mut server).await,
            json!({
                "type":"reconcileClaim", "id":"new-connection:2",
                "basis":{"sessionId":ROOM,"generation":12,"revision":8,"occurrence":"fixture-visit"},
                "cursor":{"key":1,"occurrence":"fixture-visit","positionMs":4500,"desiredPlaying":false}
            })
        );
        let last = read_json(&mut server).await;
        assert_eq!(last["type"], "command");
        assert_eq!(last["id"], "new-connection:3");
    };
    let (ended, ()) = time::timeout(Duration::from_secs(1), async {
        tokio::join!(
            connection.drive(
                &mut client,
                DEVICE,
                &mut input,
                &signals,
                &socket.session,
                0
            ),
            script
        )
    })
    .await
    .unwrap();
    assert_eq!(ended, Err(End::Transport));
}

#[tokio::test]
async fn reconcile_claim_rejects_followers_and_later_handoffs() {
    let (socket, signals, mut input) = fixture();
    let mut connection = Connection::begin(initial(NEW, Some("other")), DEVICE, &signals).unwrap();
    let transport = connection.generation;
    assert_eq!(
        socket.reconcile_claim(transport, reattach_basis(), paused_claim_cursor()),
        Err(SocketError::NotConnected)
    );
    assert!(input.try_recv().is_err());
    let mut handoff = snapshot(NEW, Some(DEVICE));
    handoff["owner"]["generation"] = (OWNER_GENERATION + 1).into();
    connection
        .receive(
            decode(&handoff.to_string(), DEVICE).unwrap(),
            DEVICE,
            &signals,
        )
        .unwrap();
    let mut handoff_basis = reattach_basis();
    handoff_basis.generation += 1;
    assert_eq!(
        socket.reconcile_claim(transport, handoff_basis, paused_claim_cursor()),
        Err(SocketError::Invalid)
    );
    assert!(input.try_recv().is_err());
    assert_eq!(
        socket.command(transport, basis(), Action::Pause).unwrap(),
        "new-connection:1"
    );
}

#[tokio::test]
async fn command_admission_is_bounded_and_uses_fifo_ids() {
    let (socket, signals, mut input) = fixture();
    let generation = Connection::begin(initial(NEW, None), DEVICE, &signals)
        .unwrap()
        .generation;
    for n in 1..=OUTBOUND_CAPACITY {
        assert_eq!(
            socket.command(generation, basis(), Action::Pause).unwrap(),
            format!("{NEW}:{n}")
        );
    }
    assert_eq!(
        socket.command(generation, basis(), Action::Pause),
        Err(SocketError::Busy)
    );
    for n in 1..=OUTBOUND_CAPACITY {
        let frame: Value = serde_json::from_str(&input.try_recv().unwrap().raw).unwrap();
        assert_eq!(frame["id"], format!("{NEW}:{n}"));
    }
    assert_eq!(
        socket.command(generation, basis(), Action::Pause).unwrap(),
        format!("{NEW}:{}", OUTBOUND_CAPACITY + 1)
    );
}

#[tokio::test]
async fn newly_admitted_follower_samples_clock_before_the_periodic_tick() {
    let (mut socket, signals, mut input) = fixture();
    let mut connection = Connection::begin(initial(NEW, Some("other")), DEVICE, &signals).unwrap();
    let generation = connection.generation;
    assert!(matches!(
        socket.events.try_recv().unwrap().kind,
        EventKind::Snapshot(_)
    ));
    let session = socket.session.clone();
    let (mut client, mut server) = peers().await;
    let script = async move {
        let request = read_raw_json(&mut server).await;
        let client_time = request["clientTimeMs"].as_u64().unwrap();
        assert_eq!(request, json!({"type":"ping","clientTimeMs":client_time}));
        send_json(
            &mut server,
            json!({"type":"pong","clientTimeMs":client_time,"serverTimeMs":client_time+2000}),
        )
        .await;
        let sample = socket.events.recv().await.unwrap();
        assert_eq!(sample.generation, generation);
        let EventKind::SampleClock(SampleClock {
            offset_ms: Some(offset),
            rtt_ms: Some(rtt),
        }) = sample.kind
        else {
            panic!("matched Pong must publish a clock estimate");
        };
        assert_eq!(offset, 2000 - (rtt / 2) as i64);
        drop(server);
        socket // Keep the outbound channel open until the driver observes EOF.
    };
    let (result, _socket) = time::timeout(CLOCK_INTERVAL / 2, async {
        tokio::join!(
            connection.drive(&mut client, DEVICE, &mut input, &signals, &session, 0),
            script
        )
    })
    .await
    .unwrap();
    assert_eq!(result, Err(End::Transport));
}

#[test]
fn clock_uses_monotonic_midpoint_discards_unmatched_pongs_and_resamples_after_jump() {
    let start = Instant::now();
    let point = |wall_ms, ms| ClockPoint {
        wall_ms,
        monotonic: start + Duration::from_millis(ms),
    };
    let mut clock = Clock::default();
    let a = point(1000, 0);
    assert_eq!(clock.check_wall(a), None);
    clock.pending = Some(a);
    assert_eq!(clock.pong(point(1020, 20), 999, 3010), None);
    assert_eq!(
        clock.pong(point(1040, 40), 1000, 3020),
        Some(SampleClock {
            offset_ms: Some(2000),
            rtt_ms: Some(40)
        })
    );
    let b = point(1200, 200);
    assert_eq!(clock.check_wall(b), None);
    clock.pending = Some(b);
    assert_eq!(
        clock.pong(point(1210, 210), 1200, 3210),
        Some(SampleClock {
            offset_ms: Some(2005),
            rtt_ms: Some(10)
        })
    );
    let c = point(1300, 300);
    assert_eq!(clock.check_wall(c), None);
    clock.pending = Some(c);
    assert_eq!(clock.pong(point(1380, 380), 1300, 3500), None); // poorer RTT
    assert_eq!(
        clock.check_wall(point(500, 400)),
        Some(SampleClock::default())
    );
    assert!(clock.samples.is_empty());
    let d = point(700, 600);
    assert_eq!(clock.check_wall(d), None);
    clock.pending = Some(d);
    assert_eq!(
        clock.pong(point(708, 608), 700, 3200),
        Some(SampleClock {
            offset_ms: Some(2496),
            rtt_ms: Some(8)
        })
    );
}

#[tokio::test]
async fn exhausted_startup_waits_for_explicit_new_batch_and_auth_stops_early() {
    let (mut socket, signals, input) = fixture();
    let attempts = Arc::new(AtomicUsize::new(0));
    let n = attempts.clone();
    let session = socket.session.clone();
    let task = tokio::spawn(run_links::<DuplexStream, _>(
        session.clone(),
        DEVICE.into(),
        input,
        signals,
        move |_| {
            let attempt = n.fetch_add(1, Ordering::SeqCst);
            async move {
                Err(if attempt < ONLINE_ATTEMPTS as usize {
                    End::Transport
                } else {
                    End::Expired
                })
            }
        },
    ));
    time::timeout(
        Duration::from_secs(1),
        socket.link.wait_for(|link| *link == Link::Exhausted),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(attempts.load(Ordering::SeqCst), 4);
    tokio::task::yield_now().await;
    assert_eq!(attempts.load(Ordering::SeqCst), 4);
    session.set_offline(true);
    time::timeout(
        Duration::from_secs(1),
        socket.link.wait_for(|link| *link == Link::Disconnected),
    )
    .await
    .unwrap()
    .unwrap();
    session.set_offline(false);
    task.await.unwrap();
    assert_eq!(*socket.link.borrow(), Link::Expired);
    assert_eq!(attempts.load(Ordering::SeqCst), 5);
}

#[tokio::test]
async fn established_loss_keeps_undrained_demotion_and_exhaustion_is_sticky() {
    let (mut socket, signals, input) = fixture();
    let (client, mut server) = peers().await;
    let mut first = Some(client);
    let attempts = Arc::new(AtomicUsize::new(0));
    let n = attempts.clone();
    let task = tokio::spawn(run_links(
        socket.session.clone(),
        DEVICE.into(),
        input,
        signals,
        move |_| {
            let attempt = n.fetch_add(1, Ordering::SeqCst);
            let admitted =
                (attempt == 3).then(|| (first.take().unwrap(), initial(OLD, Some(DEVICE))));
            async move { admitted.ok_or(End::Transport) }
        },
    ));
    time::timeout(Duration::from_secs(1), async {
        let event = socket.events.recv().await.unwrap();
        assert!(matches!(event.kind, EventKind::Snapshot(_)));
        assert_eq!(*socket.link.borrow(), Link::Connected(event.generation));
        assert_eq!(attempts.load(Ordering::SeqCst), 4);
        assert_eq!(socket.last_owner(event.generation), Some(true));
        assert_eq!(read_raw_json(&mut server).await["type"], "ping");
        let mut demotion = snapshot(OLD, Some("other"));
        demotion["owner"]["generation"] = (OWNER_GENERATION + 1).into();
        send_json(&mut server, demotion).await;
        // Do not drain the demotion before EOF and retry exhaustion.
        drop(server);
        socket
            .link
            .wait_for(|link| *link == Link::Exhausted)
            .await
            .unwrap();
        assert_eq!(attempts.load(Ordering::SeqCst), 8);
        assert_eq!(socket.last_owner(event.generation), Some(false));
        assert_eq!(socket.last_owner(0), None);
        assert!(matches!(
            socket.events.try_recv().unwrap().kind,
            EventKind::Snapshot(_)
        ));
        assert_eq!(
            socket.report(event.generation, report()),
            Err(SocketError::NotConnected)
        );
    })
    .await
    .unwrap();
    let final_authority = *socket.last_owner.borrow();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_eq!(*socket.link.borrow(), Link::Exhausted);
    assert_eq!(*socket.last_owner.borrow(), final_authority);
}

#[tokio::test]
async fn every_hello_asks_the_actor_for_fresh_claim_after_detachment() {
    let (mut socket, signals, input) = fixture();
    let requests = ClaimRequests {
        events: signals.events.clone(),
        order_gate: signals.order_gate.clone(),
    };
    let (first, mut first_peer) = peers().await;
    let (second, mut second_peer) = peers().await;
    let mut upgrades = VecDeque::from([first, second]);
    let session = socket.session.clone();
    let task = tokio::spawn(run_links(
        session.clone(),
        DEVICE.into(),
        input,
        signals,
        move |mode| {
            let ws = upgrades.pop_front();
            let (requests, session) = (requests.clone(), session.clone());
            async move {
                let mut ws = ws.ok_or(End::Transport)?;
                let first = hello_snapshot(
                    &mut ws,
                    &"a".repeat(64),
                    &session,
                    &device(),
                    Some(&requests),
                    mode,
                )
                .await?;
                Ok((ws, first))
            }
        },
    ));
    time::timeout(Duration::from_secs(1), async {
        let first = socket.events.recv().await.unwrap();
        assert_eq!(first.generation, 0);
        assert!(first.personal_stamp.is_none());
        let EventKind::ClaimRequested(reply) = first.kind else {
            panic!("initial claim request")
        };
        reply.send(Claim::default()).unwrap();
        assert!(read_json(&mut first_peer).await.get("local").is_none());
        send_json(&mut first_peer, snapshot(OLD, Some(DEVICE))).await;
        let EventKind::Snapshot(s) = socket.events.recv().await.unwrap().kind else {
            panic!("initial snapshot")
        };
        drop(first_peer);
        let second = socket.events.recv().await.unwrap();
        assert_eq!(*socket.link.borrow(), Link::Disconnected);
        assert_eq!(second.generation, 0);
        let EventKind::ClaimRequested(reply) = second.kind else {
            panic!("fresh claim request")
        };
        assert!(
            time::timeout(Duration::from_millis(10), second_peer.next())
                .await
                .is_err()
        );
        let mut local = s.player;
        local.cursor.position_ms = 70_222;
        let reattach_basis = ReattachBasis {
            session_id: s.session_id,
            generation: s.owner.generation,
            revision: s.revision,
            occurrence: local.cursor.occurrence.clone(),
        };
        reply
            .send(Claim {
                local: Some(local),
                reattach_basis: Some(reattach_basis),
            })
            .unwrap();
        let hello = read_json(&mut second_peer).await;
        assert_eq!(hello["local"]["cursor"]["positionMs"], 70_222);
        assert_eq!(hello["reattachBasis"]["revision"], 8);
        send_json(&mut second_peer, snapshot(NEW, Some(DEVICE))).await;
        assert!(matches!(
            socket.events.recv().await.unwrap().kind,
            EventKind::Snapshot(_)
        ));
    })
    .await
    .unwrap();
    task.abort();
}

#[tokio::test]
async fn reconnect_is_a_new_transport_not_an_owner_generation_and_superseded_is_terminal() {
    let (mut socket, signals, input) = fixture();
    let (first, old_peer) = peers().await;
    let (second, mut new_peer) = peers().await;
    let mut admissions = VecDeque::from([
        (first, initial(OLD, Some(DEVICE))),
        (second, initial(NEW, Some(DEVICE))),
    ]);
    let attempts = Arc::new(AtomicUsize::new(0));
    let n = attempts.clone();
    let task = tokio::spawn(run_links(
        socket.session.clone(),
        DEVICE.into(),
        input,
        signals,
        move |_| {
            n.fetch_add(1, Ordering::SeqCst);
            let admitted = admissions.pop_front();
            async move { admitted.ok_or(End::Transport) }
        },
    ));
    time::timeout(Duration::from_secs(1), async {
        let first = socket.events.recv().await.unwrap();
        drop(old_peer);
        let second = socket.events.recv().await.unwrap();
        assert_ne!(first.generation, second.generation);
        let [EventKind::Snapshot(old), EventKind::Snapshot(new)] = [first.kind, second.kind] else {
            panic!("snapshots")
        };
        assert_eq!(
            (old.connection_id.as_str(), new.connection_id.as_str()),
            (OLD, NEW)
        );
        assert_eq!(old.owner.generation, new.owner.generation);
        assert_eq!(
            socket.command(first.generation, basis(), Action::Pause),
            Err(SocketError::NotConnected)
        );
        let stale = WriteFrame {
            generation: first.generation,
            owner_generation: None,
            mode_stamp: 0,
            at: Instant::now(),
            raw: r#"{"type":"sync"}"#.into(),
            scoped: None,
        };
        socket.outbound.try_send(stale).ok().unwrap();
        assert_eq!(
            socket
                .command(second.generation, basis(), Action::Pause)
                .unwrap(),
            "new-connection:1"
        );
        assert_eq!(read_json(&mut new_peer).await["id"], "new-connection:1");
        socket.report(second.generation, report()).unwrap();
        assert_eq!(read_json(&mut new_peer).await["sequence"], 1);
        new_peer
            .send(Message::Close(Some(CloseFrame {
                code: CloseCode::from(4001),
                reason: "superseded".into(),
            })))
            .await
            .unwrap();
        task.await.unwrap();
        assert_eq!(*socket.link.borrow(), Link::Superseded);
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn full_event_queue_cannot_hide_a_known_demotion() {
    let (socket, signals, _) = fixture();
    let mut connection = Connection::begin(initial(OLD, Some(DEVICE)), DEVICE, &signals).unwrap();
    assert_eq!(socket.last_owner(connection.generation), Some(true));
    for _ in 1..EVENT_CAPACITY {
        connection
            .receive(
                Incoming::Personal {
                    update: json!({"change":{}}),
                },
                DEVICE,
                &signals,
            )
            .unwrap();
    }
    let mut demotion = snapshot(OLD, None);
    demotion["owner"]["generation"] = (OWNER_GENERATION + 1).into();
    let demotion = decode(&demotion.to_string(), DEVICE).unwrap();
    assert_eq!(
        connection.receive(demotion, DEVICE, &signals),
        Err(End::Backpressure)
    );
    assert_eq!(socket.last_owner(connection.generation), Some(false));
    assert_eq!(
        socket.report(connection.generation, report()),
        Err(SocketError::NotConnected)
    );
    assert_eq!(
        *socket.link.borrow(),
        Link::Connected(connection.generation)
    );
    signals.retire(Link::Disconnected);
    assert_eq!(socket.last_owner(connection.generation), Some(false));
    assert_eq!(
        socket.command(connection.generation, basis(), Action::Pause),
        Err(SocketError::NotConnected)
    );
}

#[tokio::test]
async fn personal_order_admission_rejects_published_updates_and_retired_mode() {
    let (mut socket, signals, mut written) = fixture();
    let generation = Connection::begin(initial(NEW, None), DEVICE, &signals)
        .unwrap()
        .generation;
    let command = json!({"type":"moveEntry"});
    assert_eq!(
        socket.personal_order(generation, 0, &command),
        Err(SocketError::Busy)
    );
    assert!(written.try_recv().is_err());
    socket.events.try_recv().unwrap();
    socket.session.set_offline(true);
    socket.session.set_offline(false);
    assert_eq!(
        socket.personal_order(generation, 0, &command),
        Err(SocketError::NotConnected)
    );
    assert!(written.try_recv().is_err());
    assert_eq!(socket.personal_order(generation, 2, &command), Ok(()));
    assert_eq!(
        serde_json::from_str::<Value>(&written.try_recv().unwrap().raw).unwrap(),
        json!({"type":"personal","command":{"type":"moveEntry"}})
    );
}

#[tokio::test]
async fn scoped_missing_goes_through_the_v3_socket_without_breaking_the_link() {
    use crate::personal_bridge::PersonalHub;

    let (mut socket, signals, mut input) = fixture();
    let mut connection = Connection::begin(initial(NEW, None), DEVICE, &signals).unwrap();
    let generation = connection.generation;
    let session = socket.session.clone();
    assert!(matches!(socket.events.try_recv().unwrap().kind, EventKind::Snapshot(_)));
    let epoch = 7;
    let hub = PersonalHub::default();
    let personal = hub.access();
    hub.begin(epoch, generation);
    let first = json!({"snapshot":{"playlists":[quick_playlist("target")],"recent":[]}});
    hub.receive(epoch, generation, serde_json::from_value(first).unwrap()).unwrap();
    let (_, read) = personal.watch_playlist(epoch, Some("target".into())).unwrap();
    hub.send_playlist_sync(epoch, generation, &read, || {
        socket.personal_playlist_sync(epoch, generation, &read, personal.clone(), Arc::new(AtomicU64::new(epoch))).is_ok()
    });
    let (mut client, mut server) = peers().await;
    let script = async {
        assert_eq!(read_json(&mut server).await,
            json!({"type":"personal","command":{"type":"syncPlaylist","playlistId":"target"}}));
        for update in [json!({"missing":{"playlistId":"target"}}), json!({"change":{"playlist":quick_playlist("other")}})] {
            server.send(Message::Text(json!({"type":"personal","update":update}).to_string().into())).await.unwrap();
            let event = socket.events.recv().await.unwrap();
            let EventKind::Personal(value) = event.kind else { panic!("expected personal ingress"); };
            assert!(matches!(hub.receive_stamped(epoch, event.generation, serde_json::from_value(value).unwrap(), event.personal_stamp), Ok(None)));
            socket.mark_personal_applied(event.personal_stamp.unwrap());
        }
        personal.read(|status, replica| {
            assert_eq!(status.link, crate::personal_bridge::Link::Ready);
            assert!(replica.playlist_unavailable());
            assert_eq!(replica.snapshot().unwrap().playlists[0].id, "other");
        });
        assert!(!hub.authorizes(epoch, generation, &read));
        drop(server);
    };
    let (ended, ()) = time::timeout(Duration::from_secs(2), async {
        tokio::join!(connection.drive(&mut client, DEVICE, &mut input, &signals, &session, 0), script)
    }).await.unwrap();
    assert_eq!(ended, Err(End::Transport));
}

#[tokio::test]
async fn quick_add_writer_discards_stale_scoped_reads_before_age_and_keeps_account_socket() {
    use crate::personal_bridge::PersonalHub;

    let (socket, signals, mut staging) = fixture();
    let mut connection = Connection::begin(initial(NEW, None), DEVICE, &signals).unwrap();
    let generation = connection.generation;
    let epoch = Arc::new(AtomicU64::new(7));
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
    let mut aged = staging.try_recv().unwrap();
    aged.at -= MAX_QUEUE_AGE + Duration::from_millis(1);
    let fresh = staging.try_recv().unwrap();
    personal.quick_select(7, Some("b"));
    let (_, replacement) = personal.quick_select(7, Some("a")).unwrap();
    socket
        .personal(generation, &PersonalCommand::ClearHistory)
        .unwrap();
    hub.send_playlist_sync(7, generation, &replacement, || stage(&replacement));
    let normal = staging.try_recv().unwrap();
    let latest = staging.try_recv().unwrap();
    assert_eq!(
        fresh.raw, latest.raw,
        "local identity must not change the wire"
    );
    let (queue, mut input) = mpsc::channel(OUTBOUND_CAPACITY);
    for frame in [aged, fresh, normal, latest] {
        queue.try_send(frame).ok().unwrap();
    }
    let (mut client, mut server) = peers().await;
    let script = async move {
        assert_eq!(
            read_json(&mut server).await,
            json!({"type":"personal","command":{"type":"clearHistory"}})
        );
        assert_eq!(
            read_json(&mut server).await,
            json!({"type":"personal","command":{"type":"syncPlaylist","playlistId":"a"}})
        );
    };
    let (ended, ()) = time::timeout(Duration::from_secs(1), async {
        tokio::join!(
            connection.drive(
                &mut client,
                DEVICE,
                &mut input,
                &signals,
                &socket.session,
                0
            ),
            script
        )
    })
    .await
    .unwrap();
    assert_eq!(ended, Err(End::Transport));
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
        let (mut socket, signals, mut written) = fixture();
        let session = socket.session.clone();
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
                    .event(13, EventKind::Personal(value), DEVICE, false)
                    .unwrap();
            }
            "" => {}
            _ => unreachable!(),
        }
        let (mut client, mut server) = peers().await;
        let ScopedWrite::Quick(quick) = *frame.scoped.unwrap() else {
            panic!("quick write retains its guard")
        };
        let writer = async {
            let deadline = frame.at + MAX_QUEUE_AGE;
            if quick_ready(&signals, Some(deadline)).await {
                send_quick(&mut client, frame.raw, quick, deadline).await
            } else {
                quick.personal.quick_cancel(&quick.ticket);
                Ok(())
            }
        };
        let apply = async {
            if matches!(blocked_by, "present" | "unrelated") {
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
        drop(client);
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
                )
            });
        }
    }
}

#[tokio::test]
async fn quick_add_writer_parks_unapplied_ingress_without_blocking_normal_writes() {
    use crate::personal_bridge::PersonalHub;

    let (mut socket, signals, mut input) = fixture();
    let mut connection = Connection::begin(initial(NEW, None), DEVICE, &signals).unwrap();
    let generation = connection.generation;
    let session = socket.session.clone();
    assert!(matches!(
        socket.events.try_recv().unwrap().kind,
        EventKind::Snapshot(_)
    ));
    let epoch = 7;
    let hub = PersonalHub::default();
    let personal = hub.access();
    let list = quick_playlist("target");
    let receive = |update| {
        hub.receive(epoch, generation, serde_json::from_value(update).unwrap())
            .unwrap()
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
        .event(
            generation,
            EventKind::Personal(json!({"change":{}})),
            DEVICE,
            false,
        )
        .unwrap();
    socket
        .personal(generation, &PersonalCommand::ClearHistory)
        .unwrap();

    let (mut client, mut server) = peers().await;
    let script = async {
        // ClearHistory can pass before ingress is applied, despite being queued behind Quick Add.
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
            time::timeout(Duration::from_millis(30), read_json(&mut server))
                .await
                .is_err()
        );
        drop(server);
    };
    let (ended, ()) = time::timeout(Duration::from_secs(1), async {
        tokio::join!(
            connection.drive(&mut client, DEVICE, &mut input, &signals, &session, 0),
            script
        )
    })
    .await
    .unwrap();
    assert_eq!(ended, Err(End::Transport));
}
