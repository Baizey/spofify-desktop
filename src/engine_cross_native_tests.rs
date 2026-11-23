//! Opt-in acceptance against the disposable Go fixture and native peers.
use super::{Account, Engine, PlayerView, Reply, TICK, connection, socket_signal, tests};
use crate::{audio::Audio, audio_input::AudioInput};
use rodio::mixer::{MixerSource, mixer};
use std::{
    env,
    io::Read,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{sync::mpsc, time};

const RATE: u32 = 8_000;
const WAV_LEN: u64 = 44 + 75 * 2 * RATE as u64;
const SHORT_WAV_LEN: u64 = 44 + 3 * 2 * RATE as u64;
const PCM_BLOCK: usize = RATE as usize / 50; // 20 ms, independent of wall time.

fn fixture_env(name: &str) -> String {
    env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| panic!("pair.py must set {name}"))
}

async fn in_memory_wav(source: AudioInput) -> AudioInput {
    // The production loader fetched this source from Go. Keep the exact fixture
    // bytes, but remove download and disk timing from decoder/mixer consumption.
    let len = source.len();
    assert!(
        matches!(len, WAV_LEN | SHORT_WAV_LEN),
        "expected a fixture WAV"
    );
    let budget = Duration::from_secs(5);
    let deadline = Instant::now() + budget;
    let bytes = time::timeout(
        budget,
        tokio::task::spawn_blocking(move || {
            source
                .await_complete(|| Instant::now() < deadline)
                .expect("Go fixture WAV transfer did not complete");
            let mut bytes = Vec::with_capacity(len as usize);
            source
                .reader()
                .read_to_end(&mut bytes)
                .expect("read fixture WAV");
            assert_eq!(bytes.len() as u64, len);
            assert_eq!(&bytes[..4], b"RIFF");
            assert_eq!(&bytes[8..12], b"WAVE");
            Arc::<[u8]>::from(bytes)
        }),
    )
    .await
    .expect("timed out reading fixture WAV")
    .expect("fixture WAV reader failed");
    bytes.into()
}

struct Desktop {
    engine: Engine,
    replies: mpsc::Receiver<Reply>,
    ticks: time::Interval,
    pcm: MixerSource,
}

impl Desktop {
    async fn step(&mut self) {
        let e = &mut self.engine;
        tokio::select! {
            _ = self.ticks.tick() => e.tick(),
            event = socket_signal(e.socket.as_mut()) => e.tick_with_event(event),
            Some(reply) = self.replies.recv() => match reply {
                Reply::Media { epoch, request, source } => {
                    let source = source.expect("Go fixture media load failed");
                    let source = in_memory_wav(source).await;
                    e.reply(Reply::Media { epoch, request, source: Some(source) });
                }
                other => e.reply(other),
            },
        }
    }

    async fn until<T>(
        &mut self,
        seconds: u64,
        phase: &str,
        mut check: impl FnMut(&mut Self) -> Option<T>,
    ) -> T {
        time::timeout(Duration::from_secs(seconds), async {
            loop {
                if let Some(result) = check(self) {
                    return result;
                }
                self.step().await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "timed out waiting for {phase}; Desktop link: {:?}",
                self.engine.link()
            )
        })
    }

    fn view(&self) -> PlayerView {
        let mut view = PlayerView::default();
        self.engine.project_online(&mut view);
        view
    }

    fn assert_silent(&mut self) {
        let audio = self.engine.audio.as_ref().expect("account audio scope");
        assert_eq!(
            audio.run_identity(),
            None,
            "Desktop retained its native run"
        );
        assert!(!audio.facts().playing);
        assert!(
            self.pcm
                .by_ref()
                .take(PCM_BLOCK)
                .all(|sample| sample == 0.0)
        );
    }
}

#[tokio::test]
#[ignore = "requires the disposable Go fixture and paired Android runner via pair.py"]
async fn desktop_cross_native_play_and_handoff() {
    let origin = fixture_env("SPOFIFY_V3_FIXTURE_URL");
    assert!(
        origin
            .strip_prefix("http://127.0.0.1:")
            .is_some_and(|port| {
                port.bytes().all(|byte| byte.is_ascii_digit())
                    && port.parse::<u16>().is_ok_and(|n| n != 0)
            }),
        "refusing a non-fixture origin"
    );
    let device = fixture_env("SPOFIFY_V3_FIXTURE_DEVICE");
    let peer = fixture_env("SPOFIFY_V3_FIXTURE_PEER");
    let track = fixture_env("SPOFIFY_V3_FIXTURE_TRACK");
    let control = std::path::PathBuf::from(fixture_env("CROSS_NATIVE_CONTROL"));
    assert_eq!(device, "cross-rust");
    assert_eq!(peer, "cross-android");
    let api = Arc::new(connection(&origin).expect("fixture API"));
    let session = api
        .login(
            &fixture_env("SPOFIFY_V3_FIXTURE_USER"),
            &fixture_env("SPOFIFY_V3_FIXTURE_PASSWORD"),
        )
        .await
        .expect("fixture login failed");

    let mut engine = tests::engine();
    let (results, replies) = mpsc::channel(16);
    engine.results = results;
    engine.device_id.clone_from(&device);
    let (mixer, pcm) = mixer(1, RATE);
    let mut audio = Audio::new_account_scoped(
        session.clone(),
        engine.shared_epoch.clone(),
        engine.epoch,
        engine.master_gain.clone(),
    );
    audio.use_test_streaming_output(mixer, 1, RATE);
    engine.audio = Some(audio);
    engine.account = Some(Account { api, session });
    engine.connect(); // Real ticket admission and WebSocket actor.
    let mut ticks = time::interval(TICK);
    ticks.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    let mut desktop = Desktop {
        engine,
        replies,
        ticks,
        pcm,
    };

    let (binding, room, initial_owner) = desktop
        .until(20, "Desktop's first snapshot", |d| {
            let binding = d.engine.online_generation()?;
            let projection = &d.engine.online.projection;
            assert_eq!(
                projection.owner()?.device_id.as_deref(),
                Some(device.as_str())
            );
            Some((
                binding,
                projection.session_id()?.to_owned(),
                projection.owner()?.generation,
            ))
        })
        .await;
    desktop.assert_silent();

    // Desktop never issues Play. Android's real command must make its way
    // through Go before Desktop starts and samples a native decoder run.
    let mut heard = false;
    let (visit, view_token) = desktop
        .until(90, "Desktop PCM from Android Play", |d| {
            let e = &d.engine;
            assert_eq!(e.online_generation(), Some(binding));
            assert!(
                e.online.is_owner(&device),
                "Desktop demoted before audible Play"
            );
            let state = e.online.projection.confirmed()?;
            let current = state.current()?;
            assert_eq!(current.id, track);
            assert!(state.cursor.desired_playing);
            let audio = e.audio.as_ref().expect("account output");
            audio.run_identity()?;
            assert!(audio.failure().is_none(), "Desktop decoder failed");
            // Poll the worker without sleeping for playback or consuming to EOF.
            // Leading underrun silence doesn't advance the native sample counter.
            for sample in d.pcm.by_ref().take(PCM_BLOCK) {
                heard |= sample.abs() > 0.001;
            }
            let facts = audio.facts();
            if !facts.playing || facts.elapsed < 0.020 {
                return None;
            }
            assert!(heard, "Desktop advanced without rendering fixture PCM");
            let view = d.view();
            assert!(view.playing);
            assert_eq!(view.track, track);
            Some((state.cursor.clone(), view.media_track_token))
        })
        .await;

    // On the very tick that applies demotion, the old run (including any
    // buffered samples) must be silent. Only a fresh Android observation can
    // now make the follower's PlayerView show Playing.
    desktop
        .until(
            30,
            "Android handoff and projected Playing observation",
            |d| {
                let e = &d.engine;
                assert_eq!(e.online_generation(), Some(binding));
                let projection = &e.online.projection;
                assert_eq!(projection.session_id(), Some(room.as_str()));
                let owner = projection.owner()?;
                if owner.device_id.as_deref() == Some(device.as_str()) {
                    assert_eq!(owner.generation, initial_owner);
                    return None;
                }
                assert_eq!(owner.device_id.as_deref(), Some(peer.as_str()));
                assert_eq!(owner.generation, initial_owner + 1);
                let cursor = &projection.confirmed()?.cursor;
                assert_eq!(cursor.key, visit.key);
                assert_eq!(cursor.occurrence, visit.occurrence);
                assert!(!e.online.detached);
                assert!(e.online.native.cursor().is_none());
                d.assert_silent();
                let view = d.view();
                assert_eq!(view.track, track);
                assert_eq!(view.media_track_token, view_token);
                assert!(
                    view.devices
                        .iter()
                        .any(|row| row.id.as_str() == peer.as_str() && row.selected),
                    "Android {peer} missing or unselected after handoff to generation {}; projected roster (id, selected, available, can_play): {:?}",
                    initial_owner + 1,
                    view.devices.iter().map(|row| (row.id.as_str(), row.selected, row.available, row.can_play)).collect::<Vec<_>>(),
                );
                (view.playing && view.elapsed > 0.0).then_some(())
            },
        )
        .await;
    // Reciprocal proxy control: issue an ordinary Seek from the now-following
    // Desktop. Android must execute it without a new output selection.
    desktop.engine.action(crate::player::Action::Seek(5.0));
    desktop
        .until(
            6,
            "Android applying Desktop follower Seek without handoff",
            |d| {
                let e = &d.engine;
                assert_eq!(e.online_generation(), Some(binding));
                let owner = e.online.projection.owner()?;
                assert_eq!(owner.device_id.as_deref(), Some(peer.as_str()));
                assert_eq!(owner.generation, initial_owner + 1);
                let cursor = &e.online.projection.confirmed()?.cursor;
                assert_eq!(
                    (cursor.key, &cursor.occurrence),
                    (visit.key, &visit.occurrence)
                );
                let seek_applied = cursor.position_ms == 5000;
                d.assert_silent();
                let view = d.view();
                (seek_applied && view.playing && view.elapsed >= 5.0).then_some(())
            },
        )
        .await;
    // Keep the Android fixture alive until this actor has finished checking
    // the selected device, fresh Playing observation, and stopped local PCM.
    std::fs::File::create_new(control.join("cross-rust-asserted"))
        .expect("signal completed Desktop handoff checks to the paired fixture");
}

mod seek {
    use super::*;
    use crate::{player, socket};
    use serde::Deserialize;
    use std::{
        fs,
        path::{Path, PathBuf},
    };

    const OWNER: &str = "seek-rust-owner";
    const CONTROLLER: &str = "seek-rust-controller";
    const TARGET_MS: u64 = 2_500;

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Fixture {
        origin: String,
        username: String,
        password: String,
        track_ids: Vec<String>,
    }

    impl Fixture {
        async fn engine(&self, id: &str) -> Engine {
            // Independent login and API clients, not two clones of a session.
            let api = Arc::new(connection(&self.origin).expect("fixture API"));
            let session = api
                .login(&self.username, &self.password)
                .await
                .expect("fixture login failed");
            let mut e = tests::engine();
            e.device_id = id.into();
            e.account = Some(Account { api, session });
            e
        }
    }

    struct ExecuteGate {
        waiting: PathBuf,
        entered: PathBuf,
    }

    impl ExecuteGate {
        fn arm(control: &Path) -> Self {
            let waiting = control.join(format!("hold.out.{OWNER}.execute"));
            let entered = control.join(format!("entered.out.{OWNER}.execute"));
            assert!(!entered.exists(), "previous execute gate was not released");
            fs::File::create_new(&waiting).expect("arm fixture execute latch");
            Self { waiting, entered }
        }

        async fn await_entered(&self) {
            time::timeout(Duration::from_secs(2), async {
                while !self.entered.exists() {
                    time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("Go did not dispatch conditional Seek to the owner latch");
        }

        fn release(self) {
            fs::remove_file(&self.entered).expect("release fixture execute latch");
        }
    }

    impl Drop for ExecuteGate {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.waiting);
            let _ = fs::remove_file(&self.entered);
        }
    }

    fn seek_result(control: &Path, id: &str) -> Option<socket::CommandResult> {
        let wire = fs::read_to_string(control.join("wire.jsonl")).expect("fixture wire journal");
        // The journal is flushed per frame; ignore an unfinished tail. The
        // controller's real engine consumes these results independently.
        wire.split_inclusive('\n')
            .filter(|line| line.ends_with('\n'))
            .find_map(|line| {
                let entry: serde_json::Value =
                    serde_json::from_str(line).expect("fixture journal frame");
                if entry["direction"] != "out"
                    || entry["deviceId"] != CONTROLLER
                    || entry["type"] != "result"
                    || entry["frame"]["id"] != id
                {
                    return None;
                }
                let result: socket::CommandResult =
                    serde_json::from_value(entry["frame"].clone()).expect("fixture result frame");
                result.status.is_terminal().then_some(result)
            })
    }

    fn shared_visit(owner: &Engine, controller: &Engine) -> Option<(u64, socket::Cursor)> {
        let local = &owner.online.projection;
        let remote = &controller.online.projection;
        owner.online_generation()?;
        controller.online_generation()?;
        assert!(owner.online.is_owner(OWNER) && controller.online.is_owner(OWNER));
        assert_eq!(local.session_id(), remote.session_id());
        assert_eq!(local.owner(), remote.owner());
        let cursor = &local.confirmed()?.cursor;
        (local.revision() == remote.revision() && *cursor == remote.confirmed()?.cursor)
            .then(|| (local.revision(), cursor.clone()))
    }

    async fn consume_eof(owner: &mut Desktop) {
        // Advance only by pulling actual mixer samples. Do not let Engine tick
        // (or EOS be published) until the caller picks the ordering.
        time::timeout(Duration::from_secs(2), async {
            loop {
                for _ in owner.pcm.by_ref().take(4 * PCM_BLOCK) {}
                let audio = owner.engine.audio.as_ref().unwrap();
                assert!(
                    audio.failure().is_none(),
                    "fixture decoder failed before EOF"
                );
                if audio.facts().ended {
                    assert_eq!(audio.facts().elapsed, 3.0);
                    break;
                }
                time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("fixture mixer did not observe actual EOF");
    }

    async fn apply_latched_execute(owner: &mut Desktop) -> socket::Execute {
        let mut auxiliary = Vec::new();
        let (execute, event) = time::timeout(Duration::from_secs(2), async {
            loop {
                let event = owner
                    .engine
                    .socket
                    .as_mut()
                    .unwrap()
                    .events
                    .recv()
                    .await
                    .expect("owner socket closed");
                match &event.kind {
                    socket::EventKind::Execute(execute) => break (execute.clone(), event),
                    socket::EventKind::SampleClock(_) | socket::EventKind::Observation(_) => {
                        auxiliary.push(event)
                    }
                    _ => panic!("unexpected owner state change before Execute"),
                }
            }
        })
        .await
        .expect("owner did not receive Go's released Execute");
        // Only passive observations/clock samples may precede the execute;
        // applying them through a full tick now would consume the deliberate
        // just-before-execute EOF and test the wrong ordering.
        owner.engine.tick_with_event(Some(event));
        for event in auxiliary {
            owner.engine.tick_with_event(Some(event));
        }
        execute
    }

    pub(super) async fn run() {
        let descriptor =
            env::var_os("CROSS_NATIVE_READY").expect("run via the disposable Go fixture");
        let fixture: Fixture =
            serde_json::from_slice(&fs::read(descriptor).expect("read fixture descriptor"))
                .unwrap_or_else(|_| panic!("invalid fixture descriptor"));
        assert!(
            fixture
                .origin
                .strip_prefix("http://127.0.0.1:")
                .is_some_and(|port| {
                    port.bytes().all(|byte| byte.is_ascii_digit())
                        && port.parse::<u16>().is_ok_and(|n| n != 0)
                }),
            "refusing a non-fixture origin"
        );
        let track = fixture
            .track_ids
            .get(1)
            .expect("fixture's three-second WAV")
            .clone();
        let control =
            PathBuf::from(env::var_os("CROSS_NATIVE_CONTROL").expect("fixture control directory"));

        let mut engine = fixture.engine(OWNER).await;
        let (results, replies) = mpsc::channel(16);
        engine.results = results;
        let (mixer, pcm) = mixer(1, RATE);
        let mut audio = Audio::new_account_scoped(
            engine.account.as_ref().unwrap().session.clone(),
            engine.shared_epoch.clone(),
            engine.epoch,
            engine.master_gain.clone(),
        );
        audio.use_test_streaming_output(mixer, 1, RATE);
        engine.audio = Some(audio);
        engine.connect();
        let mut ticks = time::interval(TICK);
        ticks.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
        let mut owner = Desktop {
            engine,
            replies,
            ticks,
            pcm,
        };
        let binding = owner
            .until(15, "Seek owner snapshot", |d| {
                let binding = d.engine.online_generation()?;
                assert!(d.engine.online.is_owner(OWNER));
                Some(binding)
            })
            .await;
        let mut controller = fixture.engine(CONTROLLER).await;
        drop(fixture);
        controller.connect();
        let follower_binding = owner
            .until(15, "authenticated controller snapshot", |d| {
                controller.tick();
                shared_visit(&d.engine, &controller)?;
                controller.online_generation()
            })
            .await;
        assert_ne!(binding, follower_binding);
        controller.online_action(player::Action::Repeat(player::Repeat::One));
        owner
            .until(5, "shared repeat-one", |d| {
                controller.tick();
                shared_visit(&d.engine, &controller)?;
                (d.engine.online.projection.confirmed()?.repeat == socket::Repeat::One)
                    .then_some(())
            })
            .await;
        controller.online_action(player::Action::Play {
            selection: player::Selection::Track {
                track: player::TrackRef {
                    id: track.clone(),
                    ..Default::default()
                },
                context: None,
            },
            shuffle: false,
        });
        owner
            .until(8, "initial measured PCM", |d| {
                controller.tick();
                shared_visit(&d.engine, &controller)?;
                if d.engine.online.projection.confirmed()?.current()?.id != track {
                    return None;
                }
                let audio = d.engine.audio.as_ref().unwrap();
                audio.run_identity()?;
                let audible = d
                    .pcm
                    .by_ref()
                    .take(PCM_BLOCK)
                    .any(|sample| sample.abs() > 0.001);
                (audible && audio.facts().playing).then_some(())
            })
            .await;

        for eof_first in [false, true] {
            let (revision, before) = shared_visit(&owner.engine, &controller).unwrap();
            let before_run = owner
                .engine
                .audio
                .as_ref()
                .unwrap()
                .run_identity()
                .expect("live visit");
            let latch = ExecuteGate::arm(&control);
            controller.online_action(player::Action::Seek(TARGET_MS as f64 / 1000.0));
            latch.await_entered().await;
            owner.engine.tick(); // Drain everything already sent while Execute is still held.
            assert_eq!(
                shared_visit(&owner.engine, &controller),
                Some((revision, before.clone()))
            );
            if eof_first {
                consume_eof(&mut owner).await;
                assert_eq!(owner.engine.online.native.cursor(), Some(&before));
            }
            latch.release();
            let exec = apply_latched_execute(&mut owner).await;
            assert_eq!(exec.revision, revision);
            assert_eq!(exec.command.basis.key, Some(before.key));
            assert_eq!(exec.command.basis.occurrence, before.occurrence);
            assert_eq!(
                exec.command.action,
                socket::Action::Seek {
                    position_ms: TARGET_MS
                }
            );
            let landed = owner.engine.online.native.cursor().unwrap().clone();
            let landed_run = owner.engine.audio.as_ref().unwrap().run_identity().unwrap();
            assert_ne!(landed_run, before_run);
            assert_eq!(landed.key, before.key);
            assert!(landed.desired_playing);
            if eof_first {
                assert_ne!(landed.occurrence, before.occurrence);
                assert_eq!(landed.position_ms, 0);
            } else {
                assert_eq!(landed.occurrence, before.occurrence);
                assert_eq!(landed.position_ms, TARGET_MS);
            }
            owner
                .until(5, "conditional Seek outcome on both clients", |d| {
                    controller.tick();
                    assert_eq!(d.engine.online_generation(), Some(binding));
                    assert_eq!(controller.online_generation(), Some(follower_binding));
                    let (committed, cursor) = shared_visit(&d.engine, &controller)?;
                    let result = seek_result(&control, &exec.command.id)?;
                    assert_eq!(
                        result.status,
                        if eof_first {
                            socket::ResultStatus::Stale
                        } else {
                            socket::ResultStatus::Applied
                        }
                    );
                    assert_eq!(result.revision, revision + 1);
                    if committed == revision {
                        return None;
                    }
                    assert_eq!(committed, revision + 1);
                    assert_eq!(cursor, landed);
                    assert!(
                        controller.audio.is_none(),
                        "controller acquired native output"
                    );
                    if controller.online.visible()?.cursor != landed {
                        return None; // Still waiting for the command's terminal projection.
                    }
                    assert_eq!(
                        d.engine.audio.as_ref().unwrap().run_identity(),
                        Some(landed_run),
                        "commit echo reopened native PCM"
                    );
                    Some(())
                })
                .await;

            if !eof_first {
                // This EOF belongs to the *sought* run. It must still create
                // exactly one repeat visit, which is the source of the next race.
                consume_eof(&mut owner).await;
                owner
                    .until(5, "native repeat after applied Seek", |d| {
                        controller.tick();
                        let (committed, cursor) = shared_visit(&d.engine, &controller)?;
                        if committed == revision + 1 {
                            return None;
                        }
                        assert_eq!(committed, revision + 2);
                        assert_eq!(cursor.key, before.key);
                        assert_ne!(cursor.occurrence, before.occurrence);
                        assert_eq!(cursor.position_ms, 0);
                        assert!(cursor.desired_playing);
                        Some(())
                    })
                    .await;
            }
        }
    }
}

#[tokio::test]
#[ignore = "requires a disposable Go fixture with conditional Seek and wire latches"]
async fn desktop_cross_native_conditional_seek() {
    seek::run().await;
}
