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
    session: Option<Session>,
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
        Self {
            stream: None,
            sink: None,
            cached: None,
            authority: None,
            started: None,
            session,
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
    /// Decode enough to validate the media and open the output without appending
    /// or starting any samples. Keep only one bounded encoded file in memory.
    pub fn prepare(&mut self, id: String, bytes: Arc<[u8]>, elapsed: f64) -> Status {
        if !self.current() {
            return Status::Rejected;
        }
        let status = decoder(bytes.clone(), elapsed).and_then(|_| self.output());
        if status.is_ok() {
            self.cached = Some((id, bytes));
        }
        status.err().unwrap_or(Status::Ok)
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
        if !self.current() || !authority.is_valid() {
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
        if !self.current() || !authority.is_valid() {
            return Status::Rejected;
        }
        let sink = Sink::connect_new(self.stream.as_ref().expect("output initialized").mixer());
        sink.pause();
        let started = Arc::new(AtomicBool::new(false));
        sink.append(Fenced {
            inner: source,
            started: started.clone(),
            authority: authority.clone(),
            session: self.session.clone(),
            offline: self.offline,
            epoch: self.epoch.clone(),
            expected: self.expected,
        });
        self.authority = Some(authority);
        self.started = Some(started);
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
            && self
                .session
                .as_ref()
                .is_none_or(|s| session_allows_output(s, self.offline))
    }
    fn allowed(&self) -> bool {
        self.current() && self.authority.as_ref().is_some_and(Authority::is_valid)
    }
    pub fn facts(&self) -> Facts {
        let allowed = self.allowed();
        let started = self
            .started
            .as_ref()
            .is_some_and(|started| started.load(Ordering::Acquire));
        let (elapsed, playing, ended) =
            self.sink
                .as_ref()
                .map_or((self.elapsed, false, false), |sink| {
                    let ended = allowed && sink.empty();
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
    authority: Authority,
    session: Option<Session>,
    offline: bool,
    epoch: Arc<AtomicU64>,
    expected: u64,
}
impl<S: Source<Item = f32>> Iterator for Fenced<S> {
    type Item = f32;
    fn next(&mut self) -> Option<f32> {
        if self.epoch.load(Ordering::SeqCst) != self.expected
            || !self.authority.is_valid()
            || self
                .session
                .as_ref()
                .is_some_and(|s| !session_allows_output(s, self.offline))
        {
            None
        } else {
            let sample = self.inner.next();
            if sample.is_some() && !self.started.load(Ordering::Relaxed) {
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
                Authority::new(Instant::now() + Duration::from_secs(1))
            } else {
                Authority::offline()
            };
            let mut source = Fenced {
                inner: decoder(wav(), 0.004).unwrap(),
                started: Arc::new(AtomicBool::new(false)),
                authority: grant.clone(),
                session: None,
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
    fn malformed_media_never_opens_an_output() {
        let mut a = Audio::new(None, false, Arc::new(AtomicU64::new(1)), 1);
        assert_eq!(
            a.prepare("bad".into(), Arc::from([1u8]), 0.0),
            Status::Media
        );
        assert!(a.stream.is_none());
    }
}
