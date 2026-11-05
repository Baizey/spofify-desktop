//! Native decoder/output. Authenticated media bytes are memory-only and bounded by Api.
//! The source itself fences every sample, independently of UI/network task scheduling.
use crate::{
    api::Session,
    player::{self, Authority, Endpoint, Outcome, Status},
};
use rodio::{Decoder, OutputStream, OutputStreamBuilder, Sink, Source};
use serde_json::{Value, json};
use std::{
    io::Cursor,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

pub struct Audio {
    stream: Option<OutputStream>,
    sink: Option<Sink>,
    prepared: Option<(String, Vec<u8>)>,
    state: Value,
    authority: Option<Authority>,
    session: Session,
    epoch: Arc<AtomicU64>,
    expected_epoch: u64,
}
impl Audio {
    pub fn new(session: Session, epoch: Arc<AtomicU64>, expected_epoch: u64) -> Self {
        Self {
            stream: None,
            sink: None,
            prepared: None,
            state: player::empty_snapshot(),
            authority: None,
            session,
            epoch,
            expected_epoch,
        }
    }
    pub fn preload(&mut self, id: String, bytes: Vec<u8>) {
        self.prepared = Some((id, bytes));
    }
    pub fn clear(&mut self) {
        if let Some(sink) = self.sink.take() {
            sink.stop();
        }
        self.prepared = None;
        self.authority = None;
        self.state = player::empty_snapshot();
    }
    fn allowed(&self) -> bool {
        self.epoch.load(Ordering::SeqCst) == self.expected_epoch
            && !self.session.is_expired()
            && self.authority.as_ref().is_some_and(Authority::is_valid)
    }
    fn outcome(&self, status: Status) -> Outcome {
        Outcome {
            status,
            state: self.snapshot(),
        }
    }
    fn fail(&mut self, status: Status) -> Outcome {
        self.stop();
        self.state["error"] = json!(if status == Status::Blocked {
            "blocked"
        } else {
            "media"
        });
        self.outcome(status)
    }
}
impl Endpoint for Audio {
    fn stop(&mut self) {
        if let Some(sink) = &self.sink {
            sink.pause();
        }
        self.state["playing"] = json!(false);
        self.state["waiting"] = json!(false);
        self.state["loading"] = json!(false);
    }
    fn snapshot(&self) -> Value {
        let mut state = self.state.clone();
        if let Some(sink) = &self.sink {
            state["elapsed"] = json!(sink.get_pos().as_secs_f64().min(604800.0));
            state["playing"] = json!(!sink.is_paused() && !sink.empty() && self.allowed());
        } else {
            state["playing"] = json!(false);
        }
        state
    }
    fn prepare(&mut self, state: &Value) -> bool {
        // Admission validates supported metadata only. A used room can retain a paused
        // track whose bytes this fresh session has never loaded.
        player::valid_snapshot(state)
    }
    fn activate(&mut self, state: &Value, authority: Authority) -> Outcome {
        if !player::valid_snapshot(state) || !authority.is_valid() {
            self.stop();
            return self.outcome(Status::Rejected);
        }
        self.authority = Some(authority.clone());
        if state["queue"].as_array().is_some_and(Vec::is_empty) {
            self.clear();
            self.authority = Some(authority);
            return self.outcome(Status::Ok);
        }
        let id = state["queue"][0]["id"].as_str().unwrap_or("");
        if self
            .prepared
            .as_ref()
            .is_some_and(|(loaded, _)| loaded == id)
        {
            if let Some(sink) = self.sink.take() {
                sink.stop();
            }
            self.state = state.clone();
            self.state["playing"] = json!(false);
            let (_, bytes) = self.prepared.take().expect("checked prepared media");
            let Ok(decoder) = Decoder::try_from(Cursor::new(bytes)) else {
                return self.fail(Status::Media);
            };
            if self.stream.is_none() {
                match OutputStreamBuilder::open_default_stream() {
                    Ok(mut stream) => {
                        stream.log_on_drop(false);
                        self.stream = Some(stream);
                    }
                    Err(_) => return self.fail(Status::Blocked),
                }
            }
            let sink = Sink::connect_new(self.stream.as_ref().expect("output initialized").mixer());
            sink.pause();
            sink.append(Fenced {
                inner: decoder,
                authority,
                session: self.session.clone(),
                epoch: self.epoch.clone(),
                expected: self.expected_epoch,
            });
            self.sink = Some(sink);
        } else if self.sink.is_none() || self.state["queue"][0]["id"] != id {
            if let Some(sink) = self.sink.take() {
                sink.stop();
            }
            self.state = state.clone();
            if state["playing"] == false {
                // Recovery is metadata-only and paused. Explicit Play will fetch bytes
                // only after the new lease and activation ACK have been established.
                self.stop();
                self.state["error"] = json!("");
                return self.outcome(Status::Ok);
            }
            return self.fail(Status::Media);
        }
        self.state["error"] = json!("");
        if state["playing"] == true && self.allowed() {
            if let Some(sink) = &self.sink {
                sink.play();
            }
        } else {
            self.stop();
        }
        self.outcome(Status::Ok)
    }
    fn execute(&mut self, command: &Value, authority: Authority) -> Outcome {
        if !authority.is_valid() {
            self.stop();
            return self.outcome(Status::Rejected);
        }
        self.authority = Some(authority);
        match command["type"].as_str() {
            Some("pause") => {
                self.stop();
                self.state["error"] = json!("");
                self.outcome(Status::Ok)
            }
            Some("resume") if self.sink.as_ref().is_some_and(|s| !s.empty()) && self.allowed() => {
                self.sink.as_ref().unwrap().play();
                self.state["error"] = json!("");
                self.outcome(Status::Ok)
            }
            Some("clear") => {
                // Restricted receiver has no upcoming entries. Keep current/history,
                // sink, elapsed and playing intent intact; this is not close/reset.
                self.outcome(Status::Ok)
            }
            Some("close") => {
                self.clear();
                self.outcome(Status::Ok)
            }
            _ => self.outcome(Status::Rejected),
        }
    }
}
impl Drop for Audio {
    fn drop(&mut self) {
        self.clear();
    }
}

struct Fenced<S> {
    inner: S,
    authority: Authority,
    session: Session,
    epoch: Arc<AtomicU64>,
    expected: u64,
}
impl<S: Source<Item = f32>> Iterator for Fenced<S> {
    type Item = f32;
    fn next(&mut self) -> Option<f32> {
        if self.epoch.load(Ordering::SeqCst) != self.expected
            || self.session.is_expired()
            || !self.authority.is_valid()
        {
            None
        } else {
            self.inner.next()
        }
    }
}
impl<S: Source<Item = f32>> Source for Fenced<S> {
    fn current_span_len(&self) -> Option<usize> {
        self.inner.current_span_len()
    }
    fn channels(&self) -> u16 {
        self.inner.channels()
    }
    fn sample_rate(&self) -> u32 {
        self.inner.sample_rate()
    }
    fn total_duration(&self) -> Option<Duration> {
        self.inner.total_duration()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{Api, Origin};
    use std::time::Instant;
    fn setup() -> (Api, Audio, player::Player, Arc<AtomicU64>) {
        let api = Api::new(Origin::parse("http://127.0.0.1:1").unwrap()).unwrap();
        let epoch = Arc::new(AtomicU64::new(1));
        let mut audio = Audio::new(api.test_session(), epoch.clone(), 1);
        let mut player = player::Player::new();
        let now = Instant::now();
        let request = player.register("Synthetic", now).unwrap();
        player.consume(&request, json!({"deviceId":"desktop", "devices":[{"deviceId":"desktop"}], "activeDeviceId":"desktop", "epoch":1, "leaseMs":8000, "state":null, "stateSeq":0, "transfer":null, "directives":[]}), &mut audio, now).unwrap();
        (api, audio, player, epoch)
    }
    fn wav() -> Vec<u8> {
        // Real little-endian mono PCM WAV; no output device is needed for decoding.
        let mut b = Vec::new();
        b.extend(b"RIFF");
        b.extend(236u32.to_le_bytes());
        b.extend(b"WAVEfmt ");
        b.extend(16u32.to_le_bytes());
        b.extend(1u16.to_le_bytes());
        b.extend(1u16.to_le_bytes());
        b.extend(8000u32.to_le_bytes());
        b.extend(16000u32.to_le_bytes());
        b.extend(2u16.to_le_bytes());
        b.extend(16u16.to_le_bytes());
        b.extend(b"data");
        b.extend(200u32.to_le_bytes());
        for _ in 0..100 {
            b.extend(1000i16.to_le_bytes());
        }
        b
    }
    #[test]
    fn incoming_clear_preserves_current_but_close_resets() {
        let (_, mut audio, mut player, _) = setup();
        let now = Instant::now();
        let mut state=player::single_track(&json!({"id":"track","name":"Current","artists":[{"name":"Manual"}],"albumName":"Album","albumId":"","duration":24}),1).unwrap();
        state["playing"] = json!(false);
        player.play(state, &mut audio, now).unwrap();
        let before = audio.snapshot();
        assert_eq!(
            player
                .local(json!({"type":"clear"}), &mut audio, now)
                .unwrap(),
            Status::Ok
        );
        assert_eq!(audio.snapshot(), before);
        assert_eq!(
            player
                .local(json!({"type":"close"}), &mut audio, now)
                .unwrap(),
            Status::Ok
        );
        assert_eq!(audio.snapshot(), player::empty_snapshot());
    }
    #[test]
    fn incoming_clear_ack_preserves_existing_blocked_and_media_errors() {
        for error in ["blocked", "media"] {
            let (_, mut audio, mut player, _) = setup();
            let now = Instant::now();
            let mut state=player::single_track(&json!({"id":"track","name":"Current","artists":[],"albumName":"Album","albumId":"","duration":24}),1).unwrap();
            state["playing"] = json!(false);
            player.play(state, &mut audio, now).unwrap();
            audio.state["error"] = json!(error);
            let before = audio.snapshot();
            let poll = player.poll(&mut audio, now).unwrap();
            player.consume(&poll,json!({"devices":[{"deviceId":"desktop"}],"activeDeviceId":"desktop","epoch":1,"leaseMs":8000,"state":before,"stateSeq":1,"transfer":null,"directives":[{"deliveryId":"clear","epoch":1,"type":"command","command":{"type":"clear"}}]}),&mut audio,now).unwrap();
            assert_eq!(audio.snapshot(), before);
            let ack = player.poll(&mut audio, now).unwrap();
            assert_eq!(ack.body["acks"][0]["status"], "ok");
            assert_eq!(ack.body["state"], before);
        }
    }
    #[test]
    fn real_decode_and_sample_fences_for_epoch_lease_and_revocation() {
        for reason in 0..3 {
            let (_, mut audio, mut player, epoch) = setup();
            let decoder = Decoder::try_from(Cursor::new(wav())).unwrap();
            let mut source = Fenced {
                inner: decoder,
                authority: player.authority(Instant::now()).unwrap(),
                session: audio.session.clone(),
                epoch: epoch.clone(),
                expected: 1,
            };
            assert!(source.next().unwrap() > 0.0);
            match reason {
                0 => {
                    epoch.store(2, Ordering::SeqCst);
                }
                1 => player.tick(&mut audio, Instant::now() + Duration::from_secs(7)),
                _ => player.failed(&mut audio),
            }
            assert_eq!(source.next(), None);
            assert_eq!(source.next(), None);
        }
    }
    #[tokio::test]
    async fn logout_revokes_decoder_even_when_network_fails() {
        let (api, audio, player, epoch) = setup();
        let mut source = Fenced {
            inner: Decoder::try_from(Cursor::new(wav())).unwrap(),
            authority: player.authority(Instant::now()).unwrap(),
            session: audio.session.clone(),
            epoch,
            expected: 1,
        };
        assert!(source.next().is_some());
        let _ = api.logout(&audio.session).await;
        assert_eq!(source.next(), None);
    }
    #[test]
    fn retained_single_track_admission_is_paused_without_cached_media() {
        let (_, mut audio, mut player, _) = setup();
        let now = Instant::now();
        let mut state = player::single_track(
            &json!({"id":"retained","name":"Retained", "artists":[],"albumName":"","duration":24}),
            1,
        )
        .unwrap();
        // Go playbackArtist uses omitempty for an unlinked artist ID.
        state["queue"][0]["artists"] = json!([{"name":"Synthetic artist"}]);
        let view = |epoch, active: &str, directives| json!({"deviceId":"desktop", "devices":[{"deviceId":"desktop"}], "activeDeviceId":active, "epoch":epoch, "leaseMs":8000, "state":state, "stateSeq":5, "transfer":null, "directives":directives});
        player
            .reconcile(view(2, "", json!([])), &mut audio, now)
            .unwrap();
        let claim = player.claim("fresh-claim", now).unwrap();
        player
            .consume(&claim, view(2, "", json!([])), &mut audio, now)
            .unwrap();
        let poll = player.poll(&mut audio, now).unwrap();
        player
            .consume(
                &poll,
                view(
                    2,
                    "",
                    json!([{"deliveryId":"prepare","epoch":2,"type":"prepare","state":state}]),
                ),
                &mut audio,
                now,
            )
            .unwrap();
        let ack = player.poll(&mut audio, now).unwrap();
        assert_eq!(ack.body["acks"][0]["status"], "ok");
        player
            .consume(
                &ack,
                view(
                    3,
                    "desktop",
                    json!([{"deliveryId":"activate","epoch":3,"type":"activate","state":state}]),
                ),
                &mut audio,
                now,
            )
            .unwrap();
        let ack = player.poll(&mut audio, now).unwrap();
        assert_eq!(ack.body["acks"][0]["status"], "ok");
        assert_eq!(ack.body["state"]["playing"], false);
        assert!(audio.stream.is_none() && audio.sink.is_none());
        player
            .consume(&ack, view(3, "desktop", json!([])), &mut audio, now)
            .unwrap();
        // Explicit selected media is admitted under the lease (bad bytes fail decoding,
        // rather than deadlocking prepare or starting any output).
        audio.preload("retained".into(), b"bad media".to_vec());
        assert_eq!(player.play(state, &mut audio, now).unwrap(), Status::Media);
        assert!(audio.stream.is_none());
    }

    #[test]
    fn malformed_media_reports_failure_without_opening_output() {
        let (_, mut audio, mut player, _) = setup();
        audio.preload("song".into(), b"not media".to_vec());
        let state = player::single_track(
            &json!({"id":"song","name":"Song","artists":[],"albumName":"","duration":1}),
            1,
        )
        .unwrap();
        assert_eq!(
            player.play(state, &mut audio, Instant::now()).unwrap(),
            Status::Media
        );
        assert_eq!(audio.snapshot()["playing"], false);
        assert_eq!(audio.snapshot()["error"], "media");
        assert!(audio.stream.is_none());
    }
}
