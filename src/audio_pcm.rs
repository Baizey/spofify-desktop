//! Decode, seek and convert to the output format off the mixer. Only bounded
//! PCM and small atomic publications cross from the worker to the callback.
use super::{Fence, MasterGain, fade_duration};
use crate::{audio_input::AudioInput, player::Status};
use rodio::{Decoder, Source, queue::SourcesQueueOutput, source::UniformSourceIterator};
use std::{
    cell::Cell,
    collections::VecDeque,
    io::{self, BufReader, Read, Seek, SeekFrom},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

const CHUNK_FRAMES: usize = 512;
const QUEUE_CHUNKS: usize = 8;
const READ_BUFFER: usize = 32 * 1024;
const POLL: Duration = Duration::from_millis(2);
const DISK_READ_STALL: Duration = Duration::from_secs(10);
const IO_IDLE: u64 = 0;
const IO_TIMED_OUT: u64 = u64::MAX;
// A stopped worker can remain in an uninterruptible filesystem operation.
// Keep its slot until it actually exits, and never let previews exhaust the
// primary pool. A promoted preview retains its original slot until exit.
static PRIMARY_WORKERS: AtomicUsize = AtomicUsize::new(0);
static PREVIEW_WORKERS: AtomicUsize = AtomicUsize::new(0);
const PRIMARY_LIMIT: usize = 8;
const PREVIEW_LIMIT: usize = 4;
const DECODING: u8 = 0;
const EOF: u8 = 1;
const MEDIA: u8 = 2;
const LOAD: u8 = 3;

#[derive(Clone, Copy)]
pub(super) struct Format {
    pub channels: u16,
    pub rate: u32,
}
impl Format {
    fn chunk_len(self) -> usize {
        CHUNK_FRAMES * usize::from(self.channels)
    }
}

/// A Rodio Sink's empty queue initially advertises 44.1 kHz mono. The mixer
/// otherwise prefetches, resamples and re-channels the first queued PCM span
/// as if it were in that placeholder format, including consuming PCM early.
/// Each sink carries just one run already converted to the mixer's format.
pub(super) struct NativeSinkOutput(pub SourcesQueueOutput, pub Format);
impl Iterator for NativeSinkOutput {
    type Item = f32;
    fn next(&mut self) -> Option<f32> {
        self.0.next()
    }
}
impl Source for NativeSinkOutput {
    fn current_span_len(&self) -> Option<usize> {
        None
    }
    fn channels(&self) -> u16 {
        self.1.channels
    }
    fn sample_rate(&self) -> u32 {
        self.1.rate
    }
    fn total_duration(&self) -> Option<Duration> {
        None
    }
}

pub(super) struct State {
    format: Format,
    queue: Mutex<VecDeque<f32>>,
    duration: OnceLock<Option<Duration>>,
    frames: AtomicU64,
    buffering: AtomicBool,
    terminal: AtomicU8,
    io: IoWatch,
    pub started: AtomicBool,
    pub exhausted: AtomicBool,
    pub fenced_out: AtomicBool,
    #[cfg(test)]
    pub worker_exited: AtomicBool,
}
impl State {
    pub fn new(format: Format) -> Self {
        Self {
            format,
            queue: Mutex::new(VecDeque::with_capacity(format.chunk_len() * QUEUE_CHUNKS)),
            duration: OnceLock::new(),
            frames: AtomicU64::new(0),
            buffering: AtomicBool::new(true),
            terminal: AtomicU8::new(DECODING),
            io: IoWatch::new(),
            started: AtomicBool::new(false),
            exhausted: AtomicBool::new(false),
            fenced_out: AtomicBool::new(false),
            #[cfg(test)]
            worker_exited: AtomicBool::new(false),
        }
    }
    pub fn duration(&self) -> Option<Duration> {
        self.duration.get().copied().flatten()
    }
    pub fn position(&self) -> Duration {
        let frames = self.frames.load(Ordering::Acquire);
        let rate = u64::from(self.format.rate);
        Duration::new(
            frames / rate,
            ((frames % rate) * 1_000_000_000 / rate) as u32,
        )
    }
    pub fn buffering(&self) -> bool {
        self.buffering.load(Ordering::Acquire)
    }
    #[cfg(test)]
    pub fn queued_frames(&self) -> usize {
        self.queue.lock().unwrap().len() / usize::from(self.format.channels)
    }
    #[cfg(test)]
    pub fn queue_limit_frames() -> usize {
        CHUNK_FRAMES * QUEUE_CHUNKS
    }
    pub fn failure(&self) -> Option<Status> {
        // The actor polls this even when a read cannot return to its worker.
        if self.terminal.load(Ordering::Acquire) == DECODING && self.io.expire_at(self.io.now()) {
            self.publish(LOAD);
        }
        match self.terminal.load(Ordering::Acquire) {
            MEDIA => Some(Status::Media),
            LOAD => Some(Status::Load),
            _ => None,
        }
    }
    fn publish(&self, terminal: u8) {
        let _ = self.terminal.compare_exchange(
            DECODING,
            terminal,
            Ordering::Release,
            Ordering::Relaxed,
        );
    }
    pub fn spawn(
        self: &Arc<Self>,
        input: AudioInput,
        elapsed: f64,
        fade: bool,
        fence: Fence,
    ) -> Result<(), Status> {
        let (count, limit) = if fade {
            (&PREVIEW_WORKERS, PREVIEW_LIMIT)
        } else {
            (&PRIMARY_WORKERS, PRIMARY_LIMIT)
        };
        let slot = WorkerSlot::acquire(count, limit).ok_or(Status::Load)?;
        let state = self.clone();
        thread::Builder::new()
            .name("audio-decode".into())
            .spawn(move || {
                let _slot = slot;
                state.decode(input, elapsed, fade, fence, true);
            })
            .map(|_| ())
            .map_err(|_| Status::Load)
    }
    pub(super) fn decode(
        self: &Arc<Self>,
        input: AudioInput,
        elapsed: f64,
        fade: bool,
        fence: Fence,
        bounded: bool,
    ) {
        struct OnExit<'a>(&'a State);
        impl Drop for OnExit<'_> {
            fn drop(&mut self) {
                // A decoder panic must not leave the consumer loading forever.
                if thread::panicking() {
                    self.0.publish(MEDIA);
                }
                #[cfg(test)]
                self.0.worker_exited.store(true, Ordering::Release);
            }
        }
        let _on_exit = OnExit(self);
        let state = self.clone();
        let live = move || !state.io.timed_out() && fence.live(&state);
        if !live() {
            return;
        }
        let reader_live = live.clone();
        let state = self.clone();
        let io_failed = Arc::new(AtomicBool::new(false));
        let mut reader = BufReader::with_capacity(
            READ_BUFFER,
            ObservedReader {
                inner: input.reader_with_cancel(move || {
                    // Waiting for not-yet-spooled bytes checks liveness every
                    // few milliseconds; a stuck pread does not check in.
                    state.io.pulse();
                    reader_live()
                }),
                state: self.clone(),
                failed: io_failed.clone(),
            },
        );
        let source_failed = || io_failed.load(Ordering::Relaxed) || input.is_failed();
        let result = (|| {
            let mut header = [0; 12];
            reader.read_exact(&mut header).map_err(|_| ())?;
            reader
                .seek_relative(-(header.len() as i64))
                .map_err(|_| ())?;
            // With Rodio's enabled codecs, RIFF/WAVE is exact-length PCM.
            // Don't infer corruption from duration for compressed formats with
            // estimates, preroll, padding, or approximate seek positions.
            let wave = &header[..4] == b"RIFF" && &header[8..] == b"WAVE";
            let mut decoder = Decoder::builder()
                .with_data(reader)
                .with_byte_len(input.len())
                .with_seekable(true)
                .build()
                .map_err(|_| ())?;
            let duration = decoder.total_duration();
            if decoder.channels() == 0
                || decoder.sample_rate() == 0
                || (fade && fade_duration(duration).is_none())
            {
                return Err(());
            }
            let seek = Duration::from_secs_f64(elapsed.clamp(0.0, super::MAX_SECONDS));
            let seek = duration.map_or(seek, |end| seek.min(end));
            if !seek.is_zero() {
                decoder.try_seek(seek).map_err(|_| ())?;
            }
            let minimum_wave_samples = if wave {
                duration.map(|end| {
                    let rate = u128::from(decoder.sample_rate());
                    let frames = |t: Duration| (t.as_nanos() * rate + 500_000_000) / 1_000_000_000;
                    let expected = frames(end).saturating_sub(frames(seek));
                    // Symphonia's accurate-seek refinement uses floating-point
                    // timestamp conversion. Tolerate two frames only after seek.
                    expected.saturating_sub(if seek.is_zero() { 0 } else { 2 })
                        * u128::from(decoder.channels())
                })
            } else {
                None
            };
            Ok((
                decoder,
                duration,
                !seek.is_zero() && duration == Some(seek),
                minimum_wave_samples,
            ))
        })();
        if !live() {
            return;
        }
        let Ok((decoder, duration, seek_to_end, minimum_wave_samples)) = result else {
            self.publish(if source_failed() { LOAD } else { MEDIA });
            return;
        };
        let _ = self.duration.set(duration);
        let decoded_samples = Cell::new(0);
        let decoded = CountedPcm {
            inner: decoder,
            count: minimum_wave_samples.map(|_| &decoded_samples),
        };
        let mut samples =
            UniformSourceIterator::new(decoded, self.format.channels, self.format.rate);
        let chunk_len = self.format.chunk_len();
        let mut chunk = Vec::with_capacity(chunk_len);
        let mut wrote_pcm = false;
        loop {
            if !live() {
                return;
            }
            if source_failed() {
                self.publish(LOAD);
                return;
            }
            // Backpressure is worker-only, checked before any further decoder I/O.
            if bounded && self.queue.lock().unwrap().len() > chunk_len * (QUEUE_CHUNKS - 1) {
                thread::sleep(POLL);
                continue;
            }
            chunk.clear();
            chunk.extend(samples.by_ref().take(chunk_len));
            if !live() {
                return;
            }
            if source_failed() {
                self.publish(LOAD);
                return;
            }
            let eof = chunk.len() < chunk_len;
            // Never expose a partial interleaved frame to the mixer.
            let partial_frame = chunk.len() % usize::from(self.format.channels);
            chunk.truncate(chunk.len() - partial_frame);
            if !chunk.is_empty() {
                self.queue.lock().unwrap().extend(chunk.iter().copied());
                wrote_pcm = true;
            }
            if eof {
                // Some containers finish decoding before the HTTP body/trailer.
                // Keep EOF provisional until the transfer has really succeeded.
                let terminal = if input.await_complete(&live).is_err() || source_failed() {
                    LOAD
                } else if partial_frame != 0
                    || (!wrote_pcm && !seek_to_end)
                    || minimum_wave_samples.is_some_and(|minimum| decoded_samples.get() < minimum)
                {
                    MEDIA
                } else {
                    EOF
                };
                if live() {
                    self.publish(terminal);
                }
                return;
            }
        }
    }
}

// Count native PCM, not the resampler's rounded per-span output, when
// validating an exact-length WAV at EOF.
struct CountedPcm<'a, S> {
    inner: S,
    count: Option<&'a Cell<u128>>,
}
impl<S: Source> Iterator for CountedPcm<'_, S> {
    type Item = f32;
    fn next(&mut self) -> Option<f32> {
        let next = self.inner.next()?;
        if let Some(count) = self.count {
            count.set(count.get() + 1);
        }
        Some(next)
    }
}
impl<S: Source> Source for CountedPcm<'_, S> {
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

struct WorkerSlot(&'static AtomicUsize);
impl WorkerSlot {
    fn acquire(count: &'static AtomicUsize, limit: usize) -> Option<Self> {
        let mut n = count.load(Ordering::Relaxed);
        while n < limit {
            match count.compare_exchange_weak(n, n + 1, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => return Some(Self(count)),
                Err(current) => n = current,
            }
        }
        None
    }
}
impl Drop for WorkerSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

struct IoWatch {
    origin: Instant,
    stamp: AtomicU64,
}
impl IoWatch {
    fn new() -> Self {
        Self {
            origin: Instant::now(),
            stamp: AtomicU64::new(IO_IDLE),
        }
    }
    fn now(&self) -> u64 {
        self.origin
            .elapsed()
            .as_nanos()
            .min(u128::from(IO_TIMED_OUT - 2)) as u64
            + 1
    }
    fn begin(&self) -> io::Result<IoStep<'_>> {
        self.stamp
            .compare_exchange(IO_IDLE, self.now(), Ordering::Relaxed, Ordering::Relaxed)
            .map(|_| IoStep(self))
            .map_err(|_| io::Error::from(io::ErrorKind::TimedOut))
    }
    fn pulse(&self) {
        let stamp = self.stamp.load(Ordering::Relaxed);
        if !matches!(stamp, IO_IDLE | IO_TIMED_OUT) {
            let _ = self.stamp.compare_exchange(
                stamp,
                self.now(),
                Ordering::Relaxed,
                Ordering::Relaxed,
            );
        }
    }
    fn timed_out(&self) -> bool {
        self.stamp.load(Ordering::Relaxed) == IO_TIMED_OUT
    }
    fn expire_at(&self, now: u64) -> bool {
        let stamp = self.stamp.load(Ordering::Relaxed);
        stamp == IO_TIMED_OUT
            || (stamp != IO_IDLE
                && now.saturating_sub(stamp) >= DISK_READ_STALL.as_nanos() as u64
                && self
                    .stamp
                    .compare_exchange(stamp, IO_TIMED_OUT, Ordering::Relaxed, Ordering::Relaxed)
                    .is_ok())
    }
}
struct IoStep<'a>(&'a IoWatch);
impl Drop for IoStep<'_> {
    fn drop(&mut self) {
        let stamp = self.0.stamp.load(Ordering::Relaxed);
        if stamp != IO_TIMED_OUT {
            let _ =
                self.0
                    .stamp
                    .compare_exchange(stamp, IO_IDLE, Ordering::Relaxed, Ordering::Relaxed);
        }
    }
}

// Rodio's iterator can turn I/O errors into None. Observe errors at the actual
// I/O boundary so they cannot become natural EOF. Decoder errors internal to
// Rodio/Symphonia aren't observable this way; those iterators may skip bad
// packets or silently stop without exposing a distinct terminal error.
struct ObservedReader<R> {
    inner: R,
    state: Arc<State>,
    failed: Arc<AtomicBool>,
}
impl<R: Read> Read for ObservedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let _step = self.state.io.begin()?;
        self.inner.read(buf).inspect_err(|e| {
            if e.kind() != io::ErrorKind::Interrupted {
                self.failed.store(true, Ordering::Relaxed);
            }
        })
    }
}
impl<R: Seek> Seek for ObservedReader<R> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let _step = self.state.io.begin()?;
        self.inner.seek(pos).inspect_err(|e| {
            if !matches!(
                e.kind(),
                io::ErrorKind::InvalidInput
                    | io::ErrorKind::Unsupported
                    | io::ErrorKind::Interrupted
            ) {
                self.failed.store(true, Ordering::Relaxed);
            }
        })
    }
}

pub(super) struct Pcm {
    state: Arc<State>,
    fence: Fence,
    gain: MasterGain,
    chunk: Vec<f32>,
    next: usize,
    channel: u16,
    real_frame: bool,
}
impl Pcm {
    pub fn new(state: Arc<State>, fence: Fence, gain: MasterGain) -> Self {
        Self {
            chunk: Vec::with_capacity(state.format.chunk_len()),
            state,
            fence,
            gain,
            next: 0,
            channel: 0,
            real_frame: false,
        }
    }
}
impl Iterator for Pcm {
    type Item = f32;
    fn next(&mut self) -> Option<f32> {
        let state = &self.state;
        if state.exhausted.load(Ordering::Acquire) || !self.fence.live(state) {
            return None;
        }
        let terminal = state.terminal.load(Ordering::Acquire);
        if matches!(terminal, MEDIA | LOAD) {
            return None;
        }
        if self.channel == 0 {
            if self.next == self.chunk.len() {
                self.chunk.clear();
                self.next = 0;
                // Never wait for the worker on this thread. This lock only
                // protects PCM copies; the decoder and source I/O are outside it.
                if let Ok(mut queue) = state.queue.try_lock() {
                    let available = queue.len().min(state.format.chunk_len());
                    if available == 0 && terminal == EOF {
                        state.exhausted.store(true, Ordering::Release);
                        return None;
                    }
                    self.chunk.extend(queue.drain(..available));
                }
            }
            self.real_frame = self.next < self.chunk.len();
            state.buffering.store(!self.real_frame, Ordering::Release);
        }
        // Stay on the same channel for an entire real or silent output frame.
        // Underrun silence never advances media time or admits a start grant.
        let sample = if self.real_frame {
            let sample = self.chunk[self.next];
            self.next += 1;
            if !state.started.load(Ordering::Relaxed) {
                state.started.store(true, Ordering::Release);
            }
            let gain = self.gain.get();
            if gain == 0.0 { 0.0 } else { sample * gain }
        } else {
            0.0
        };
        self.channel += 1;
        if self.channel == state.format.channels {
            self.channel = 0;
            if self.real_frame {
                state.frames.fetch_add(1, Ordering::Release);
            }
        }
        Some(sample)
    }
}
impl Source for Pcm {
    fn current_span_len(&self) -> Option<usize> {
        None
    }
    fn channels(&self) -> u16 {
        self.state.format.channels
    }
    fn sample_rate(&self) -> u32 {
        self.state.format.rate
    }
    fn total_duration(&self) -> Option<Duration> {
        self.state.duration()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{audio::OutputScope, player::Authority};

    #[test]
    fn watchdog_reports_stuck_io_without_releasing_its_worker_slot() {
        static PRIMARY: AtomicUsize = AtomicUsize::new(0);
        static PREVIEW: AtomicUsize = AtomicUsize::new(0);
        let _primary = WorkerSlot::acquire(&PRIMARY, 1).unwrap();
        let preview = WorkerSlot::acquire(&PREVIEW, 1).unwrap();
        let state = State::new(Format {
            channels: 1,
            rate: 8000,
        });
        let active_read = state.io.begin().unwrap();
        let stamp = state.io.stamp.load(Ordering::Relaxed);
        let delay = DISK_READ_STALL.as_nanos() as u64;
        assert!(!state.io.expire_at(stamp + delay - 1));
        assert!(state.io.expire_at(stamp + delay));
        assert_eq!(state.failure(), Some(Status::Load));
        assert!(WorkerSlot::acquire(&PREVIEW, 1).is_none());
        assert!(!state.exhausted.load(Ordering::Acquire));
        drop(active_read);
        assert!(state.io.timed_out());
        drop(preview);
        assert!(WorkerSlot::acquire(&PREVIEW, 1).is_some());
    }

    #[test]
    fn underrun_is_a_whole_frame_and_contention_does_not_wait() {
        let state = Arc::new(State::new(Format {
            channels: 2,
            rate: 8000,
        }));
        let scope = OutputScope {
            session: None,
            network: None,
            offline: true,
            epoch: Arc::new(AtomicU64::new(1)),
            expected: 1,
        };
        let fence = Fence {
            scope,
            authority: Authority::offline(),
        };
        let mut pcm = Pcm::new(state.clone(), fence, MasterGain::default());
        let mut queue = state.queue.lock().unwrap();
        assert_eq!(pcm.next(), Some(0.0)); // first silent channel, despite contention
        queue.extend([0.25, 0.75]);
        drop(queue); // samples arrived between channels
        assert_eq!(pcm.next(), Some(0.0));
        assert!(state.position().is_zero());
        assert!(!state.started.load(Ordering::Acquire));
        assert!(!state.exhausted.load(Ordering::Acquire));
        assert_eq!(pcm.next(), Some(0.25));
        assert_eq!(pcm.next(), Some(0.75));
        assert_eq!(state.position(), Duration::from_micros(125));
    }
}
