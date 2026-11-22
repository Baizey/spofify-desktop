use super::*;
use serde_json::json;
use std::cell::Cell;
use tokio::io::DuplexStream;

type Peer = WebSocketStream<DuplexStream>;
const CONNECTION: &str = "bulk-connection";
const DEVICE: &str = "desktop-installation";
const TARGET: &str = "target";

fn player_snapshot() -> Value {
    json!({
        "type":"snapshot", "connectionId":CONNECTION, "owner":"playing", "devices":[],
        "player":{"queue":[],"cursor":Cursor::default(),"repeat":"off","shuffle":false}
    })
}

fn summary(id: &str, revision: u64) -> Value {
    json!({"id":id, "name":"Mix", "description":"", "kind":"manual", "countState":"exact",
        "trackCount":0, "availableCount":0, "revision":revision,
        "createdAt":"2026-08-03T00:00:00Z", "updatedAt":"2026-08-03T00:00:00Z",
        "capabilities":{"editMetadata":true,"editEntries":true,"editDefinition":false,
            "play":true,"copyToManual":false,"delete":true,"addTracks":true}})
}

fn add(track: impl Into<String>) -> PersonalCommand {
    PersonalCommand::AddEntry {
        playlist_id: TARGET.into(),
        track_id: track.into(),
    }
}

async fn reply(peer: &mut Peer, update: Value) {
    peer.send(Message::Text(
        json!({"type":"personal", "update":update})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
}

async fn initial(peer: &mut Peer, playlists: Vec<Value>) {
    reply(
        peer,
        json!({"snapshot":{"playlists":playlists,"recent":[]}}),
    )
    .await;
}

async fn send_player(peer: &mut Peer) {
    peer.send(Message::Text(player_snapshot().to_string().into()))
        .await
        .unwrap();
}

async fn next_frame(peer: &mut Peer) -> Value {
    let msg = peer.next().await.unwrap().unwrap();
    serde_json::from_str(msg.to_text().unwrap()).unwrap()
}

async fn links(capacity: usize) -> (Session, Peer, Peer, Incoming) {
    let api = Api::new(crate::api::Origin::parse("http://127.0.0.1:1").unwrap()).unwrap();
    let session = api.test_session();
    let (client, peer) = tokio::io::duplex(capacity);
    let client = WebSocketStream::from_raw_socket(
        client,
        Role::Client,
        Some(WebSocketConfig::default().write_buffer_size(0)),
    )
    .await;
    let peer = WebSocketStream::from_raw_socket(peer, Role::Server, None).await;
    let frame = player_snapshot().to_string();
    assert!(matches!(decode(&frame, DEVICE), Err(End::Protocol)));
    let frame = decode_link(&frame, DEVICE, true).unwrap();
    (session, client, peer, frame)
}

#[tokio::test]
async fn bulk_order_waits_for_the_latest_primary_personal_publication() {
    let api = Api::new(crate::api::Origin::parse("http://127.0.0.1:1").unwrap()).unwrap();
    let (socket, _peer) = Socket::test_peer(api.test_session(), 1);
    let order = socket.bulk_order();
    let mut changes = order.changes();
    assert_eq!(order.when_applied(|| 0), Some(0));
    *socket.order_gate.lock().unwrap() = 2;
    socket.mark_personal_applied(1);
    assert_eq!(*changes.borrow_and_update(), 1);
    assert_eq!(order.when_applied(|| 1), None);
    socket.mark_personal_applied(2);
    assert!(changes.has_changed().unwrap());
    assert_eq!(
        order.when_applied(|| {
            assert!(socket.order_gate.try_lock().is_err());
            2
        }),
        Some(2)
    );
}

#[test]
fn bulk_snapshot_allows_shared_identity_with_or_without_its_nonplaying_row() {
    let mut frame = player_snapshot();
    frame["devices"] = json!([
        {"id":"playing","deviceId":DEVICE,"name":"Spofify Desktop","canPlay":true},
        {"id":CONNECTION,"deviceId":DEVICE,"name":"Spofify Desktop","canPlay":false}
    ]);
    assert!(decode_link(&frame.to_string(), DEVICE, true).is_ok());
    frame["devices"][1]["canPlay"] = true.into();
    assert!(matches!(
        decode_link(&frame.to_string(), DEVICE, true),
        Err(End::Protocol)
    ));
    frame["devices"].as_array_mut().unwrap().pop();
    assert!(decode_link(&frame.to_string(), DEVICE, true).is_ok());
    assert!(matches!(
        decode(&frame.to_string(), DEVICE),
        Err(End::Protocol)
    ));
}

#[tokio::test]
async fn bulk_initial_and_fifo_confirmation_ignore_player_and_broadcasts() {
    let (session, client, mut peer, snapshot) = links(4096).await;
    time::timeout(Duration::from_secs(2), async {
        let (ready, proceed) = tokio::sync::oneshot::channel();
        let (pong, got_pong) = tokio::sync::oneshot::channel();
        let script = async {
            reply(&mut peer, json!({"change":{"playlist":summary(TARGET, 3)}})).await;
            initial(&mut peer, vec![summary(TARGET, 4)]).await;
            assert_eq!(next_frame(&mut peer).await,
                json!({"type":"personal","command":{"type":"addEntry","playlistId":TARGET,"trackId":"first"}}));
            assert_eq!(next_frame(&mut peer).await, json!({"type":"sync"}));
            send_player(&mut peer).await;
            reply(&mut peer, json!({"change":{"playlist":summary(TARGET, 5)}})).await;
            peer.send(Message::Ping(vec![3, 7].into())).await.unwrap();
            assert_eq!(peer.next().await.unwrap().unwrap(), Message::Pong(vec![3, 7].into()));
            pong.send(()).unwrap();
            proceed.await.unwrap();
            initial(&mut peer, vec![summary(TARGET, 6)]).await;
        };
        let run = async {
            let mut link = BulkLink::finish_open(client, snapshot, &session, 0, DEVICE).await.unwrap();
            assert!(!link.add(&add("blocked"), |_| false).await.unwrap());
            assert!(link.add(&add("first"), |start| {
                assert!(PUBLICATION_GATE.try_lock().is_err());
                assert!(start());
                assert!(!start(), "an add cannot dispatch twice");
                true
            }).await.unwrap());
            let check = link.checkpoint(TARGET);
            tokio::pin!(check);
            tokio::select! {
                biased;
                result = &mut check => panic!("premature checkpoint: {result:?}"),
                result = got_pong => result.unwrap(),
            }
            ready.send(()).unwrap();
            assert_eq!(check.await, Ok(6));
        };
        tokio::join!(run, script);
    }).await.unwrap();
}

#[tokio::test]
async fn bulk_caps_started_window_and_resolves_omitted_target_with_one_scoped_part() {
    let (session, client, mut peer, snapshot) = links(4096).await;
    time::timeout(Duration::from_secs(2), async {
        let script = async {
            initial(&mut peer, vec![]).await;
            for index in 0..BULK_WINDOW {
                assert_eq!(next_frame(&mut peer).await,
                    json!({"type":"personal","command":{"type":"addEntry","playlistId":TARGET,"trackId":index.to_string()}}));
            }
            assert_eq!(next_frame(&mut peer).await, json!({"type":"sync"}));
            send_player(&mut peer).await;
            initial(&mut peer, vec![summary("other", 100)]).await;
            assert_eq!(next_frame(&mut peer).await,
                json!({"type":"personal","command":{"type":"syncPlaylist","playlistId":TARGET}}));
            reply(&mut peer, json!({"part":{"summary":summary("other",101),"token":"other-token","offset":0,"rows":[],"more":false}})).await;
            reply(&mut peer, json!({"part":{"summary":summary(TARGET,19),"token":"token","offset":0,"rows":[],"more":true}})).await;
            assert_eq!(next_frame(&mut peer).await["command"]["trackId"], "next");
        };
        let run = async {
            let mut link = BulkLink::finish_open(client, snapshot, &session, 0, DEVICE).await.unwrap();
            for i in 0..BULK_WINDOW {
                assert!(link.add(&add(i.to_string()), |start| start()).await.unwrap());
            }
            assert_eq!(link.add(&add("above cap"), |_| panic!("at cap")).await, Err(SocketError::Busy));
            assert_eq!(link.checkpoint(TARGET).await, Ok(19));
            assert!(link.add(&add("next"), |start| start()).await.unwrap());
        };
        tokio::join!(run, script);
    }).await.unwrap();
}

#[tokio::test]
async fn bulk_maintains_pong_during_primary_wait_then_reuses_the_same_link() {
    let (session, client, mut peer, snapshot) = links(4096).await;
    time::timeout(Duration::from_secs(2), async {
        let (primary_ready, primary_progress) = tokio::sync::oneshot::channel();
        let script = async {
            initial(&mut peer, vec![]).await;
            assert_eq!(next_frame(&mut peer).await, json!({"type":"sync"}));
            send_player(&mut peer).await;
            initial(&mut peer, vec![summary(TARGET, 6)]).await;
            reply(&mut peer, json!({"change":{"playlist":summary(TARGET, 7)}})).await;
            peer.send(Message::Ping(vec![8, 9].into())).await.unwrap();
            assert_eq!(
                peer.next().await.unwrap().unwrap(),
                Message::Pong(vec![8, 9].into())
            );
            primary_ready.send(()).unwrap();
            assert_eq!(
                next_frame(&mut peer).await["command"]["trackId"],
                "next window"
            );
        };
        let run = async {
            let mut link = BulkLink::finish_open(client, snapshot, &session, 0, DEVICE)
                .await
                .unwrap();
            assert_eq!(link.checkpoint(TARGET).await, Ok(6));
            tokio::select! {
                biased;
                result = link.maintain() => panic!("maintenance ended: {result:?}"),
                result = primary_progress => result.unwrap(),
            }
            assert_eq!(
                link.add(&add("next window"), |start| start()).await,
                Ok(true)
            );
        };
        tokio::join!(run, script);
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn bulk_disconnected_after_player_snapshot_cannot_confirm_or_send_more() {
    let (session, client, mut peer, snapshot) = links(4096).await;
    time::timeout(Duration::from_secs(2), async {
        let script = async move {
            initial(&mut peer, vec![]).await;
            assert_eq!(next_frame(&mut peer).await["command"]["trackId"], "first");
            assert_eq!(next_frame(&mut peer).await, json!({"type":"sync"}));
            send_player(&mut peer).await;
            reply(&mut peer, json!({"change":{"playlist":summary(TARGET, 8)}})).await;
            drop(peer);
        };
        let run = async {
            let mut link = BulkLink::finish_open(client, snapshot, &session, 0, DEVICE)
                .await
                .unwrap();
            assert!(link.add(&add("first"), |start| start()).await.unwrap());
            assert_eq!(
                link.checkpoint(TARGET).await,
                Err(SocketError::NotConnected)
            );
            assert_eq!(
                link.add(&add("not replayed"), |_| panic!("link retired"))
                    .await,
                Err(SocketError::NotConnected)
            );
        };
        tokio::join!(run, script);
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn bulk_canceled_flush_retires_the_link_and_only_started_attempt_is_ambiguous() {
    let (session, client, mut peer, snapshot) = links(64).await;
    let (link, ()) = tokio::join!(
        BulkLink::finish_open(client, snapshot, &session, 0, DEVICE),
        initial(&mut peer, vec![])
    );
    let mut link = link.unwrap();
    let started = Cell::new(false);
    let cmd = add("w".repeat(512));
    {
        // The peer intentionally never reads. The large frame can start but
        // cannot flush into the small duplex pipe, so caller cancellation wins.
        let write = link.add(&cmd, |start| {
            started.set(start());
            started.get()
        });
        tokio::pin!(write);
        assert!(futures_util::poll!(&mut write).is_pending());
        assert!(started.get());
    }
    assert_eq!(
        link.checkpoint(TARGET).await,
        Err(SocketError::NotConnected)
    );
    assert_eq!(
        link.add(&add("not replayed"), |_| panic!("link retired"))
            .await,
        Err(SocketError::NotConnected)
    );
}

#[tokio::test]
async fn bulk_mode_round_trip_and_revocation_both_veto_before_start() {
    for revoke in [false, true] {
        let (session, client, mut peer, snapshot) = links(4096).await;
        initial(&mut peer, vec![]).await;
        let mut link = BulkLink::finish_open(client, snapshot, &session, 0, DEVICE)
            .await
            .unwrap();
        if revoke {
            session.retire_local_grant();
        } else {
            session.set_offline(true);
            session.set_offline(false);
        }
        assert_eq!(
            link.add(&add("fenced"), |_| panic!("stale scope")).await,
            Err(SocketError::NotConnected)
        );
        assert!(!matches!(peer.next().await, Some(Ok(Message::Text(_)))));
    }
}
