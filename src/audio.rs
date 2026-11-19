//! Native output. The decoder checks its exact operation capability, session
//! and account/mode epoch at the sample boundary, outside the actor's scheduler.
use crate::{
    api::Session,
    player::{Authority, Status},
};
use rodio::{Decoder, OutputStream, OutputStreamBuilder, Sink, Source, source::SeekError};
use std::{
    io::Cursor,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

const MAX_SECONDS: f64 = 604800.0;

#[derive(Clone, Copy, Debug)]
pub struct Facts {
    pub elapsed: f64,
    pub duration: f64,
    pub playing: bool,
    pub loading: bool,
    pub ended: bool,
}

pub struct Audio {
    stream: Option<OutputStream>,
    sink: Option<Sink>,
    cached: Option<(String, Arc<[u8]>)>,
    authority: Option<Authority>,
    started: Option<Arc<AtomicBool>>,
    exhausted: Option<Arc<AtomicBool>>,
    fenced_out: Option<Arc<AtomicBool>>,
    session: Option<Session>,
    network: Option<(tokio::sync::watch::Receiver<u64>, u64)>,
    offline: bool,
    epoch: Arc<AtomicU64>,
    expected: u64,
    elapsed: f64,
    duration: f64,
}
impl Audio {
    pub fn new(
        session: Option<Session>,
        offline: bool,
        epoch: Arc<AtomicU64>,
        expected: u64,
    ) -> Self {
        let network = session.as_ref().map(|s| {
            let changes = s.network_changes();
            let stamp = *changes.borrow();
            (changes, stamp)
        });
        Self {
            stream: None,
            sink: None,
            cached: None,
            authority: None,
            started: None,
            exhausted: None,
            fenced_out: None,
            session,
            network,
            offline,
            epoch,
            expected,
            elapsed: 0.0,
            duration: 0.0,
        }
    }
    pub fn cached(&self, id: &str) -> Option<Arc<[u8]>> {
        self.cached
            .as_ref()
            .filter(|(loaded, _)| loaded == id)
            .map(|(_, bytes)| bytes.clone())
    }
    pub fn discard_cache(&mut self) {
        self.cached = None;
    }
    fn output(&mut self) -> Result<(), Status> {
        if self.stream.is_none() {
            let mut stream =
                OutputStreamBuilder::open_default_stream().map_err(|_| Status::Blocked)?;
            stream.log_on_drop(false);
            self.stream = Some(stream);
        }
        Ok(())
    }
    pub fn play(
        &mut self,
        id: String,
        bytes: Arc<[u8]>,
        elapsed: f64,
        duration: f64,
        authority: Authority,
    ) -> Status {
        self.stop();
        self.anchor(elapsed, duration);
        if !self.current() || !authority.may_start() {
            return Status::Rejected;
        }
        let source = match decoder(bytes.clone(), self.elapsed) {
            Ok(source) => source,
            Err(e) => return e,
        };
        if let Some(duration) = source.total_duration() {
            self.duration = duration.as_secs_f64().min(MAX_SECONDS);
            self.elapsed = self.elapsed.min(self.duration);
        }
        self.cached = Some((id, bytes));
        if let Err(e) = self.output() {
            return e;
        }
        if !self.current() || !authority.may_start() {
            return Status::Rejected;
        }
        let sink = Sink::connect_new(self.stream.as_ref().expect("output initialized").mixer());
        sink.pause();
        let started = Arc::new(AtomicBool::new(false));
        let exhausted = Arc::new(AtomicBool::new(false));
        let fenced_out = Arc::new(AtomicBool::new(false));
        sink.append(Fenced {
            inner: source,
            started: started.clone(),
            exhausted: exhausted.clone(),
            fenced_out: fenced_out.clone(),
            authority: authority.clone(),
            session: self.session.clone(),
            network: self.network.clone(),
            offline: self.offline,
            epoch: self.epoch.clone(),
            expected: self.expected,
        });
        self.authority = Some(authority);
        self.started = Some(started);
        self.exhausted = Some(exhausted);
        self.fenced_out = Some(fenced_out);
        self.sink = Some(sink);
        if self.allowed() {
            self.sink.as_ref().unwrap().play();
            Status::Ok
        } else {
            self.stop();
            Status::Rejected
        }
    }
    pub fn anchor(&mut self, elapsed: f64, duration: f64) {
        self.elapsed = if duration > 0.0 {
            elapsed.min(duration)
        } else {
            elapsed
        };
        self.duration = duration;
    }
    pub fn stop(&mut self) {
        let elapsed = self.facts().elapsed;
        if let Some(authority) = self.authority.take() {
            authority.revoke();
        }
        if let Some(sink) = self.sink.take() {
            sink.stop();
        }
        self.started = None;
        self.exhausted = None;
        self.fenced_out = None;
        self.elapsed = elapsed;
    }
    pub fn clear(&mut self) {
        self.stop();
        self.cached = None;
        self.stream = None;
        self.anchor(0.0, 0.0);
    }
    fn current(&self) -> bool {
        self.epoch.load(Ordering::SeqCst) == self.expected
            && network_current(&self.network)
            && self
                .session
                .as_ref()
                .is_none_or(|s| session_allows_output(s, self.offline))
    }
    fn allowed(&self) -> bool {
        self.current() && self.authority.as_ref().is_some_and(Authority::is_valid)
    }
    pub fn has_started(&self) -> bool {
        self.started
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Acquire))
    }
    pub fn was_fenced(&self) -> bool {
        self.fenced_out
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Acquire))
    }
    pub fn facts(&self) -> Facts {
        let allowed = self.allowed() && !self.was_fenced();
        let started = self.has_started();
        let (elapsed, playing, ended) =
            self.sink
                .as_ref()
                .map_or((self.elapsed, false, false), |sink| {
                    let ended = allowed
                        && sink.empty()
                        && self
                            .exhausted
                            .as_ref()
                            .is_some_and(|flag| flag.load(Ordering::Acquire));
                    let elapsed = (self.elapsed + sink.get_pos().as_secs_f64()).min(MAX_SECONDS);
                    let elapsed = if self.duration > 0.0 {
                        if ended {
                            self.duration
                        } else {
                            elapsed.min(self.duration)
                        }
                    } else {
                        elapsed
                    };
                    (
                        elapsed,
                        allowed && started && !ended && !sink.is_paused(),
                        ended,
                    )
                });
        Facts {
            elapsed,
            duration: self.duration,
            playing,
            loading: allowed && !started && !ended && self.sink.is_some(),
            ended,
        }
    }
}
impl Drop for Audio {
    fn drop(&mut self) {
        self.clear();
    }
}

fn network_current(network: &Option<(tokio::sync::watch::Receiver<u64>, u64)>) -> bool {
    network
        .as_ref()
        .is_none_or(|(changes, stamp)| *changes.borrow() == *stamp)
}

fn session_allows_output(session: &Session, offline: bool) -> bool {
    // Offline is an explicit local grant; cookie time is irrelevant until
    // Go Online. Account/mode retirement revokes the per-output capability.
    session.is_offline() == offline && session.can_access_local()
}

fn decoder(bytes: Arc<[u8]>, elapsed: f64) -> Result<Decoder<Cursor<Arc<[u8]>>>, Status> {
    // Cursor::try_from does not set the length/seekability required by
    // Symphonia. Encoded bytes are complete, so advertise both explicitly.
    let byte_len = bytes.len() as u64;
    let mut source = Decoder::builder()
        .with_data(Cursor::new(bytes))
        .with_byte_len(byte_len)
        .with_seekable(true)
        .build()
        .map_err(|_| Status::Media)?;
    if elapsed > 0.0 {
        let seek = Duration::from_secs_f64(elapsed);
        source
            .try_seek(
                source
                    .total_duration()
                    .map_or(seek, |duration| duration.min(seek)),
            )
            .map_err(|_| Status::Media)?;
    }
    Ok(source)
}
struct Fenced<S> {
    inner: S,
    started: Arc<AtomicBool>,
    exhausted: Arc<AtomicBool>,
    fenced_out: Arc<AtomicBool>,
    authority: Authority,
    session: Option<Session>,
    network: Option<(tokio::sync::watch::Receiver<u64>, u64)>,
    offline: bool,
    epoch: Arc<AtomicU64>,
    expected: u64,
}
impl<S: Source<Item = f32>> Iterator for Fenced<S> {
    type Item = f32;
    fn next(&mut self) -> Option<f32> {
        if self.fenced_out.load(Ordering::Acquire) || self.exhausted.load(Ordering::Acquire) {
            return None;
        }
        if self.epoch.load(Ordering::SeqCst) != self.expected
            || !self.authority.is_valid()
            || (!self.started.load(Ordering::Acquire) && !self.authority.may_start())
            || !network_current(&self.network)
            || self
                .session
                .as_ref()
                .is_some_and(|s| !session_allows_output(s, self.offline))
        {
            self.fenced_out.store(true, Ordering::Release);
            None
        } else {
            let sample = self.inner.next();
            if sample.is_none() {
                self.exhausted.store(true, Ordering::Release);
            } else if !self.started.load(Ordering::Relaxed) {
                self.started.store(true, Ordering::Release);
            }
            sample
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
    fn try_seek(&mut self, pos: Duration) -> Result<(), SeekError> {
        self.inner.try_seek(pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;
    fn wav() -> Arc<[u8]> {
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
        b.into()
    }
    #[test]
    fn real_decoder_seeks_and_sample_fences_revocation_and_epoch() {
        for online in [false, true] {
            let epoch = Arc::new(AtomicU64::new(1));
            let grant = if online {
                let deadline = Instant::now() + Duration::from_secs(1);
                Authority::online(move || Instant::now() < deadline, || true)
            } else {
                Authority::offline()
            };
            let mut source = Fenced {
                inner: decoder(wav(), 0.004).unwrap(),
                started: Arc::new(AtomicBool::new(false)),
                exhausted: Arc::new(AtomicBool::new(false)),
                fenced_out: Arc::new(AtomicBool::new(false)),
                authority: grant.clone(),
                session: None,
                network: None,
                offline: !online,
                epoch: epoch.clone(),
                expected: 1,
            };
            assert!(source.next().unwrap() > 0.0);
            if online {
                grant.revoke();
            } else {
                epoch.store(2, Ordering::SeqCst);
            }
            assert!(source.next().is_none());
        }
    }
    #[test]
    fn an_unstarted_decoder_cannot_use_grace_and_fencing_is_not_natural_end() {
        let attached = Arc::new(AtomicBool::new(false));
        let start = attached.clone();
        let grant = Authority::online(|| true, move || start.load(Ordering::Acquire));
        let mut source = Fenced {
            inner: decoder(wav(), 0.0).unwrap(),
            started: Arc::default(),
            exhausted: Arc::default(),
            fenced_out: Arc::default(),
            authority: grant,
            session: None,
            network: None,
            offline: false,
            epoch: Arc::new(AtomicU64::new(1)),
            expected: 1,
        };
        assert!(source.next().is_none());
        assert!(source.fenced_out.load(Ordering::Acquire));
        assert!(!source.exhausted.load(Ordering::Acquire));
        attached.store(true, Ordering::Release);
        assert!(
            source.next().is_none(),
            "a retired source must not revive after reattach"
        );
    }
    #[test]
    fn malformed_media_never_opens_an_output() {
        let mut a = Audio::new(None, false, Arc::new(AtomicU64::new(1)), 1);
        assert_eq!(
            a.play(
                "bad".into(),
                Arc::from([1u8]),
                0.0,
                0.0,
                Authority::offline()
            ),
            Status::Media
        );
        assert!(a.stream.is_none());
    }
}
