//! Headless engine + real Rodio decoder and mixer. The external acceptance
//! test additionally measures the Slint click and OS audio backend.
use super::*;
use crate::{audio::Audio, player, socket::TestPeer};
use rodio::mixer::{MixerSource, mixer};
use serde_json::json;
use std::time::Duration;

const RATE: u32 = 8000;
const GEN: Generation = 101;

fn media() -> Arc<[u8]> {
    let len = 2 * RATE;
    let mut b = Vec::new();
    b.extend(b"RIFF");
    b.extend((36 + 2 * len).to_le_bytes());
    b.extend(b"WAVEfmt ");
    b.extend(16u32.to_le_bytes());
    b.extend(1u16.to_le_bytes());
    b.extend(1u16.to_le_bytes());
    b.extend(RATE.to_le_bytes());
    b.extend((2 * RATE).to_le_bytes());
    b.extend(2u16.to_le_bytes());
    b.extend(16u16.to_le_bytes());
    b.extend(b"data");
    b.extend((2 * len).to_le_bytes());
    for i in 0..len {
        // Audible 400 Hz tone, with distinct intro and body amplitudes so
        // reopening the decoder or stopping it is observable.
        let amplitude = if i < RATE / 4 { 8192i16 } else { 16384i16 };
        let sample = if i / 10 % 2 == 0 {
            amplitude
        } else {
            -amplitude
        };
        b.extend(sample.to_le_bytes());
    }
    b.into()
}
fn outputs(owner: &str) -> Outputs {
    serde_json::from_value(json!({"owner":owner,"devices":[
        {"id":"desktop","deviceId":"synthetic","name":"Desktop","canPlay":true}
    ]}))
    .unwrap()
}
fn fixture(owner: &str) -> (Engine, TestPeer, MixerSource) {
    let mut e = super::super::tests::engine();
    let api = Arc::new(super::super::connection("http://127.0.0.1:1").unwrap());
    let session = api.test_session();
    let (engine_socket, peer) = socket::Socket::test_peer(session.clone(), GEN);
    e.socket = Some(engine_socket);
    e.account = Some(super::super::Account {
        api,
        session: session.clone(),
    });
    let (mixer, pcm) = mixer(1, RATE);
    let mut audio = Audio::new(
        Some(session),
        false,
        e.shared_epoch.clone(),
        e.epoch,
        e.master_gain.clone(),
    );
    audio.use_test_output(mixer, "tone".into(), media());
    e.audio = Some(audio);
    e.handle_event(Event {
        generation: GEN,
        kind: EventKind::Snapshot {
            connection_id: "desktop".into(),
            player: State {
                cursor: Cursor {
                    run: u64::from(!owner.is_empty()),
                    ..Cursor::default()
                },
                ..State::default()
            },
            outputs: outputs(owner),
        },
    });
    (e, peer, pcm)
}
fn play(engine: &mut Engine, peer: &mut TestPeer) {
    engine.online_action(Action::Play {
        selection: player::Selection::Track {
            track: player::TrackRef {
                id: "tone".into(),
                ..Default::default()
            },
            context: None,
        },
        shuffle: false,
    });
    let command = peer.read().expect("admitted Play");
    assert_eq!(command["type"], "play");
    assert_eq!(
        command["selection"],
        json!({"kind":"track", "track":{"id":"tone"}})
    );
    assert!(
        peer.read().is_none(),
        "no automatic select or provisional fact"
    );
    assert!(
        engine.media.is_none(),
        "use verified same-account RAM media"
    );
}
fn update(peer: &TestPeer, kind: EventKind) {
    peer.events
        .try_send(Event {
            generation: GEN,
            kind,
        })
        .unwrap();
}
fn first_pcm(pcm: &mut MixerSource) -> Option<f32> {
    // Rodio applies sink controls at its 5 ms periodic access boundary.
    pcm.take((RATE / 10) as usize)
        .find(|&sample| sample != 0.0)
        .map(f32::abs)
}

fn election_pulse(peer: &TestPeer, run: u64, owner: &str) {
    update(
        peer,
        EventKind::Player(socket::PlayerDelta {
            cursor: Some(Cursor {
                run,
                ..Cursor::default()
            }),
            ..Default::default()
        }),
    );
    update(peer, EventKind::Outputs(outputs(owner)));
}

async fn play_before_confirm(vacant: bool) {
    let (mut e, mut peer, mut pcm) = fixture("desktop");
    if vacant {
        // A previously available transfer target left while Desktop was
        // retiring. Go publishes SetOutput's pulse and still-empty outputs.
        update(&peer, EventKind::Outputs(outputs("")));
        e.tick_online(None);
        assert_eq!(peer.read().unwrap(), json!({"type":"release"}));
        election_pulse(&peer, 2, "");
        e.tick_online(None);
        assert!(e.online.locally_vacant() && !e.online.is_owner());
    }
    let click = Instant::now();
    play(&mut e, &mut peer);
    let mut view = PlayerView::default();
    e.project_online(&mut view);
    assert_eq!(view.status, "Starting playback…");
    assert!(!view.playing && view.queue.is_empty());
    e.tick_online(None);
    assert_eq!(first_pcm(&mut pcm), Some(0.25));
    let first = click.elapsed();
    if vacant {
        // No election response, cursor, or grant existed at first PCM.
        election_pulse(&peer, 3, "desktop");
        e.tick_online(None);
        assert!(e.online.is_owner());
    }
    // Simulate additional hardware callbacks while the Go reply is delayed.
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(pcm.nth((RATE / 2) as usize).map(f32::abs), Some(0.5));
    assert!(e.online.execution.is_none() && peer.read().is_none());
    let before_confirm = click.elapsed();
    confirm_play(&peer);
    e.tick_online(None);
    tokio::time::sleep(Duration::from_millis(220)).await;
    assert!(e.online.execution.is_none() && peer.read().is_none());
    assert_eq!(pcm.next().map(f32::abs), Some(0.5));
    let grant_at = click.elapsed();
    peer.authority.send_replace(Some(OutputAuthority {
        generation: GEN,
        deadline: Instant::now() + Duration::from_secs(2),
        attached: true,
        resume_through: 0,
    }));
    e.tick_online(None);
    let fact = peer
        .read()
        .expect("fact only after canonical attached execution");
    assert_eq!(fact["type"], "fact");
    assert_eq!(fact["chain"], "confirmed");
    assert!(fact["elapsed"].as_f64().unwrap() > 0.4);
    assert_eq!(
        pcm.next().map(f32::abs),
        Some(0.5),
        "promotion must not replay the 0.25 intro or drop a sample"
    );
    println!(
        "headless engine + Rodio (non-output vacancy: {vacant}) first PCM {first:?}; cursor {before_confirm:?}; grant {grant_at:?} retained decoder"
    );
}

fn confirm_play(peer: &TestPeer) {
    let delta = serde_json::from_value(json!({
        "source":"desktop", "cursor":{"key":1,"chain":"confirmed","visit":0,"run":0,"elapsed":0,"desired":true,"playing":false},
        "queue":{"kind":"replace","rows":[{"id":"tone","key":1,"order":1,"duration":2.0,"intent":"low"}]}
    })).unwrap();
    update(peer, EventKind::Player(delta));
}

#[tokio::test]
async fn canonical_fallback_plays_after_startup_drain_or_preview_deadline() {
    for startup_drain in [true, false] {
        let (mut e, mut peer, mut pcm) = fixture(if startup_drain { "" } else { "desktop" });
        play(&mut e, &mut peer);
        e.tick_online(None);
        if startup_drain {
            assert_eq!(first_pcm(&mut pcm), None);
        } else {
            assert_eq!(first_pcm(&mut pcm), Some(0.25));
            assert_eq!(pcm.nth((RATE / 2) as usize).map(f32::abs), Some(0.5));
            tokio::time::sleep(Duration::from_millis(1250)).await;
            assert_eq!(first_pcm(&mut pcm), None, "the expired body must be fenced");
            e.tick_online(None);
            assert!(!e.online.starting_locally());
        }
        confirm_play(&peer);
        if startup_drain {
            let cursor = Cursor {
                key: 1,
                chain: "confirmed".into(),
                run: 1,
                desired: true,
                ..Cursor::default()
            };
            update(
                &peer,
                EventKind::Player(socket::PlayerDelta {
                    cursor: Some(cursor),
                    ..Default::default()
                }),
            );
            update(&peer, EventKind::Outputs(outputs("desktop")));
        }
        e.tick_online(None);
        assert_eq!(first_pcm(&mut pcm), None);
        assert!(e.media.is_none() && peer.read().is_none());
        peer.authority.send_replace(Some(OutputAuthority {
            generation: GEN,
            deadline: Instant::now() + Duration::from_secs(10),
            attached: true,
            resume_through: 0,
        }));
        e.tick_online(None);
        e.media.take().expect("canonical load after grant").abort();
        e.complete_online(
            e.operation,
            Ok(OnlineMedia {
                bytes: media(),
                downloaded: false,
            }),
        );
        e.tick_online(None);
        let first_fact = peer.read().expect("canonical initial buffering fact");
        assert_eq!(first_fact["buffering"], true);
        assert_eq!(first_fact["playing"], false);
        assert_eq!(first_fact["run"], u64::from(startup_drain));
        assert_eq!(
            first_pcm(&mut pcm),
            Some(0.25),
            "authorized decoder starts on the intro"
        );
        e.tick_online(None);
        assert!(
            peer.read().is_none(),
            "playing fact waits for the report cadence, not PCM"
        );
        e.online.execution.as_mut().unwrap().last_fact = Some(Instant::now() - REPORT_INTERVAL);
        e.tick_online(None);
        assert_eq!(peer.read().unwrap()["playing"], true);
    }
}

#[tokio::test]
async fn real_pcm_precedes_delayed_play_receipt_and_grant_then_keeps_its_decoder() {
    play_before_confirm(false).await;
    play_before_confirm(true).await;
}

#[tokio::test]
async fn uncached_media_load_and_resume_need_no_new_response_to_start() {
    let (mut e, mut peer, mut pcm) = fixture("desktop");
    e.audio.as_mut().unwrap().discard_cache();
    let mut state: State = serde_json::from_value(json!({
        "cursor":{"key":1,"chain":"paused","visit":0,"run":2,"elapsed":0.5,"desired":false,"playing":false},
        "queue":[{"id":"tone","key":1,"order":1,"duration":2.0,"intent":"low"}], "shuffle":false,"repeat":"off"
    })).unwrap();
    e.online.state = Some(state.clone());
    e.online_action(Action::Resume);
    assert_eq!(peer.read().unwrap(), json!({"type":"resume"}));
    // The authenticated load is already scheduled before any Go response.
    // Inject its verified result rather than opening TCP in a headless test.
    e.media.take().expect("concurrent media work").abort();
    e.complete_online(
        e.operation,
        Ok(OnlineMedia {
            bytes: media(),
            downloaded: false,
        }),
    );
    assert_eq!(
        first_pcm(&mut pcm),
        Some(0.5),
        "Resume seeks before rendering its first sample"
    );
    assert!(e.online.execution.is_none() && peer.read().is_none());
    state.cursor.run += 1;
    state.cursor.desired = true;
    update(
        &peer,
        EventKind::Player(socket::PlayerDelta {
            cursor: Some(state.cursor),
            ..Default::default()
        }),
    );
    peer.authority.send_replace(Some(OutputAuthority {
        generation: GEN,
        deadline: Instant::now() + Duration::from_secs(2),
        attached: true,
        resume_through: 1,
    }));
    e.tick_online(None);
    let fact = peer.read().unwrap();
    assert_eq!(fact["chain"], "paused");
    assert_eq!(fact["run"], 3);
    assert_eq!(pcm.next().map(f32::abs), Some(0.5));
}

#[tokio::test]
async fn known_remote_is_silent_and_local_start_yields_on_output_or_account_correction() {
    let (mut e, mut peer, mut pcm) = fixture("detached-remote");
    play(&mut e, &mut peer);
    e.tick_online(None);
    assert!(pcm.next().is_none() && peer.read().is_none());
    for account in [false, true] {
        let (mut e, mut peer, mut pcm) = fixture("desktop");
        play(&mut e, &mut peer);
        e.tick_online(None);
        assert_eq!(first_pcm(&mut pcm), Some(0.25));
        if account {
            // The account fence must take effect at the sample boundary,
            // with no wait for the actor to consume the logout command.
            e.shared_epoch.store(e.epoch + 1, Ordering::SeqCst);
        } else {
            update(&peer, EventKind::Outputs(outputs("remote")));
            e.tick_online(None);
        }
        assert!(pcm.by_ref().take(256).all(|s| s == 0.0));
        assert!(peer.read().is_none());
    }
}
