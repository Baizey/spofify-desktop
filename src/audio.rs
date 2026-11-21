//! Native output. The decoder checks its exact operation capability, session
//! and account/mode epoch at the sample boundary, outside the actor's scheduler.
use crate::{
    api::Session,
    player::{Authority, Status},
};
use rodio::{
    Decoder, OutputStream, OutputStreamBuilder, Sink, Source, mixer::Mixer, source::SeekError,
};
use std::{
    io::Cursor,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
    time::Duration,
};

const MAX_SECONDS: f64 = 604800.0;
pub(crate) const CROSSFADE_SECONDS: f64 = 5.0;
const MIN_OVERLAP_SECONDS: f64 = 1.0;
const MAX_OVERRUN_SECONDS: f64 = 0.25;
const OUTGOING_FLOOR: f32 = 0.05;
const MAX_GAIN_STEP: f32 = 0.05;
// Rodio 0.21.1 polls each Sink's controls on its own 5 ms cadence.
const GAIN_SETTLE: Duration = Duration::from_millis(10);
const UNITY: f32 = 1.0;

/// Device-local scalar, shared by every run without involving playback commands.
#[derive(Clone)]
pub(crate) struct MasterGain(Arc<AtomicU32>);

impl MasterGain {
    pub(crate) fn set(&self, gain: f32) {
        if gain.is_finite() {
            self.0
                .store(gain.clamp(0.0, UNITY).to_bits(), Ordering::Relaxed);
        }
    }

    fn get(&self) -> f32 {
        f32::from_bits(self.0.load(Ordering::Relaxed))
    }
}

impl Default for MasterGain {
    fn default() -> Self {
        Self(Arc::new(AtomicU32::new(UNITY.to_bits())))
    }
}

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
    primary: Option<Run>,
    crossfade: Option<Crossfade>,
    master_gain: MasterGain,
    cached: Option<(String, Arc<[u8]>)>,
    session: Option<Session>,
    network: Option<(tokio::sync::watch::Receiver<u64>, u64)>,
    offline: bool,
    epoch: Arc<AtomicU64>,
    expected: u64,
    elapsed: f64,
    duration: f64,
}
struct Run {
    sink: Sink,
    authority: Authority,
    started: Arc<AtomicBool>,
    exhausted: Arc<AtomicBool>,
    fenced_out: Arc<AtomicBool>,
    decoded_duration: Option<Duration>,
}
impl Run {
    fn has_started(&self) -> bool {
        self.started.load(Ordering::Acquire)
    }
    fn was_fenced(&self) -> bool {
        self.fenced_out.load(Ordering::Acquire)
    }
    fn ended(&self) -> bool {
        self.sink.empty() && self.exhausted.load(Ordering::Acquire)
    }
    fn can_continue(&self) -> bool {
        self.authority.is_valid()
            && !self.was_fenced()
            && !self.exhausted.load(Ordering::Acquire)
            && !self.sink.empty()
            && !self.sink.is_paused()
    }
}
impl Drop for Run {
    fn drop(&mut self) {
        self.authority.revoke();
        self.sink.stop();
    }
}

struct Crossfade {
    incoming: Run,
    cached: (String, Arc<[u8]>),
    overlap: f64,
    attenuation: f32,
    incoming_gain: f32,
    position_at_update: Duration,
}

impl Audio {
    pub fn new(
        session: Option<Session>,
        offline: bool,
        epoch: Arc<AtomicU64>,
        expected: u64,
        master_gain: MasterGain,
    ) -> Self {
        let network = session.as_ref().map(|s| {
            let changes = s.network_changes();
            let stamp = *changes.borrow();
            (changes, stamp)
        });
        Self {
            stream: None,
            primary: None,
            crossfade: None,
            master_gain,
            cached: None,
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
        let mixer = self.stream.as_ref().expect("output initialized").mixer();
        let run = self.attach(mixer, source, authority, UNITY);
        if self.current() && run.authority.is_valid() {
            run.sink.play();
            self.primary = Some(run);
            Status::Ok
        } else {
            Status::Rejected
        }
    }
    fn attach<S: Source<Item = f32> + Send + 'static>(
        &self,
        mixer: &Mixer,
        source: S,
        authority: Authority,
        envelope: f32,
    ) -> Run {
        let decoded_duration = source.total_duration();
        let sink = Sink::connect_new(mixer);
        sink.pause();
        sink.set_volume(envelope);
        let started = Arc::new(AtomicBool::new(false));
        let exhausted = Arc::new(AtomicBool::new(false));
        let fenced_out = Arc::new(AtomicBool::new(false));
        sink.append(Fenced {
            inner: source,
            master_gain: self.master_gain.clone(),
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
        Run {
            sink,
            authority,
            started,
            exhausted,
            fenced_out,
            decoded_duration,
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
        self.primary = None;
        self.crossfade = None;
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
    pub fn has_started(&self) -> bool {
        self.primary.as_ref().is_some_and(Run::has_started)
    }
    pub fn was_fenced(&self) -> bool {
        self.primary.as_ref().is_some_and(Run::was_fenced)
    }
    pub fn facts(&self) -> Facts {
        let mut facts = Facts {
            elapsed: self.elapsed,
            duration: self.duration,
            playing: false,
            loading: false,
            ended: false,
        };
        if let Some(run) = &self.primary {
            let allowed = self.current() && run.authority.is_valid() && !run.was_fenced();
            let started = run.has_started();
            facts.ended = allowed && run.ended();
            let elapsed = (self.elapsed + run.sink.get_pos().as_secs_f64()).min(MAX_SECONDS);
            facts.elapsed = if self.duration > 0.0 && facts.ended {
                self.duration
            } else if self.duration > 0.0 {
                elapsed.min(self.duration)
            } else {
                elapsed
            };
            facts.playing = allowed && started && !facts.ended && !run.sink.is_paused();
            facts.loading = allowed && !started && !facts.ended;
        }
        facts
    }

    /// Only decoder timing qualifies for an overlap; catalog durations are
    /// still useful to display normal playback and never trigger mixing.
    pub fn crossfade_remaining(&self) -> Option<f64> {
        let run = self.primary.as_ref()?;
        let duration = fade_duration(run.decoded_duration)?;
        if !self.current() || !run.can_continue() || !run.has_started() {
            return None;
        }
        let remaining = duration - (self.elapsed + run.sink.get_pos().as_secs_f64());
        (remaining > 0.0).then_some(remaining)
    }

    /// Admit a separate run; `authority` must not be shared with the primary.
    /// Once admitted, cancellation permanently revokes the incoming capability.
    pub fn start_crossfade(&mut self, id: String, bytes: Arc<[u8]>, authority: Authority) -> bool {
        let Some(mixer) = self.stream.as_ref().map(|stream| stream.mixer().clone()) else {
            return false;
        };
        self.start_crossfade_on(&mixer, id, bytes, authority)
    }
    fn start_crossfade_on(
        &mut self,
        mixer: &Mixer,
        id: String,
        bytes: Arc<[u8]>,
        authority: Authority,
    ) -> bool {
        if self.crossfade.is_some() || !authority.may_start() || self.overlap().is_none() {
            return false;
        }
        let Ok(source) = decoder(bytes.clone(), 0.0) else {
            return false;
        };
        if fade_duration(source.total_duration()).is_none()
            || source.current_span_len() == Some(0)
            || source.channels() == 0
            || source.sample_rate() == 0
        {
            return false;
        }
        // Loading a decoder can take time. Recheck both sides before making
        // the incoming run available to the mixer, initially silent.
        let Some(overlap) = self.overlap() else {
            return false;
        };
        if !authority.may_start() {
            return false;
        }
        let incoming = self.attach(mixer, source, authority, 0.0);
        incoming.sink.play();
        self.crossfade = Some(Crossfade {
            incoming,
            cached: (id, bytes),
            overlap,
            attenuation: 0.0,
            incoming_gain: 0.0,
            position_at_update: Duration::ZERO,
        });
        true
    }
    fn overlap(&self) -> Option<f64> {
        self.crossfade_remaining()
            .filter(|remaining| (MIN_OVERLAP_SECONDS..=CROSSFADE_SECONDS).contains(remaining))
    }

    pub fn maintain_crossfade(&mut self) {
        let Some(fade) = &self.crossfade else {
            return;
        };
        let outgoing = self.primary.as_ref().expect("overlap has a primary run");
        if !self.current()
            || !outgoing.authority.is_valid()
            || outgoing.was_fenced()
            || outgoing.sink.is_paused()
            || !fade.incoming.can_continue()
            || (fade.incoming_gain == 0.0 && !fade.incoming.authority.may_start())
        {
            self.cancel_crossfade();
            return;
        }
        let position = outgoing.sink.get_pos();
        // Natural EOF is a handoff point, not a failure of the overlap.
        if outgoing.exhausted.load(Ordering::Acquire) {
            return;
        }
        let duration = fade_duration(outgoing.decoded_duration).expect("overlap has a duration");
        let remaining = duration - (self.elapsed + position.as_secs_f64());
        // Decoders may estimate duration (e.g. MP3 without a seek table). Keep
        // an audible tail through modest errors; don't let an early estimate
        // leave a primary ducked indefinitely when it still has samples.
        if remaining < -MAX_OVERRUN_SECONDS {
            self.cancel_crossfade();
            return;
        }
        if !fade.incoming.has_started()
            || fade.incoming.sink.get_pos().is_zero()
            || position.saturating_sub(fade.position_at_update) < GAIN_SETTLE
        {
            return;
        }
        let progress = (1.0 - remaining / fade.overlap).clamp(0.0, 1.0) as f32;
        // Mostly linear with modest easing near the endpoints. These are
        // envelope factors only; the master gain is applied in the source.
        let eased = progress + 0.25 * (progress * progress * (3.0 - 2.0 * progress) - progress);
        let target = (UNITY - OUTGOING_FLOOR) * eased;
        let attenuation = target
            .max(fade.attenuation)
            .min(fade.attenuation + MAX_GAIN_STEP);
        // Spend only the headroom reserved by the previous outgoing update.
        // GAIN_SETTLE on the outgoing sample clock lets it apply that update
        // before the incoming sink can use it, even if their polls stagger.
        let incoming_gain = fade.attenuation;
        if attenuation > fade.attenuation || incoming_gain > fade.incoming_gain {
            outgoing.sink.set_volume(UNITY - attenuation);
            let fade = self.crossfade.as_mut().unwrap();
            fade.incoming.sink.set_volume(incoming_gain);
            fade.attenuation = attenuation;
            fade.incoming_gain = incoming_gain;
            // Sample after publishing the gains, so time spent preempted
            // before the set_volume calls cannot count as settling time.
            fade.position_at_update = outgoing.sink.get_pos();
        }
    }

    pub fn promote_crossfade(&mut self) -> bool {
        if self.crossfade.is_none() || !self.facts().ended {
            return false;
        }
        let fade = self.crossfade.as_ref().unwrap();
        if fade.incoming_gain == 0.0 || !fade.incoming.can_continue() {
            // A run whose envelope never opened should start from zero
            // through ordinary playback, not inherit pre-roll samples.
            self.cancel_crossfade();
            return false;
        }
        let fade = self.crossfade.take().unwrap();
        self.primary = None;
        fade.incoming.sink.set_volume(UNITY);
        self.duration = fade.incoming.decoded_duration.unwrap().as_secs_f64();
        self.elapsed = 0.0;
        self.cached = Some(fade.cached);
        self.primary = Some(fade.incoming);
        true
    }

    pub fn cancel_crossfade(&mut self) {
        if let Some(fade) = self.crossfade.take() {
            drop(fade);
            if let Some(run) = &self.primary {
                run.sink.set_volume(UNITY);
            }
        }
    }
}
impl Drop for Audio {
    fn drop(&mut self) {
        self.clear();
    }
}

fn fade_duration(duration: Option<Duration>) -> Option<f64> {
    duration
        .map(|d| d.as_secs_f64())
        .filter(|d| *d > CROSSFADE_SECONDS && *d <= MAX_SECONDS)
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
    master_gain: MasterGain,
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
            // Keep consuming samples at zero gain: mute must not change
            // transport, authority, or natural EOF. The separate Sink
            // envelope still controls the crossfade and its gain settling.
            sample.map(|value| {
                let gain = self.master_gain.get();
                if gain == 0.0 { 0.0 } else { value * gain }
            })
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
    use rodio::mixer::{MixerSource, mixer};
    use std::time::Instant;

    const RATE: u32 = 8000;

    fn wav() -> Arc<[u8]> {
        wav_samples(100, 1000)
    }

    fn wav_samples(samples: u32, amplitude: i16) -> Arc<[u8]> {
        let mut b = Vec::new();
        b.extend(b"RIFF");
        b.extend((36 + 2 * samples).to_le_bytes());
        b.extend(b"WAVEfmt ");
        b.extend(16u32.to_le_bytes());
        b.extend(1u16.to_le_bytes());
        b.extend(1u16.to_le_bytes());
        b.extend(RATE.to_le_bytes());
        b.extend((2 * RATE).to_le_bytes());
        b.extend(2u16.to_le_bytes());
        b.extend(16u16.to_le_bytes());
        b.extend(b"data");
        b.extend((2 * samples).to_le_bytes());
        for _ in 0..samples {
            b.extend(amplitude.to_le_bytes());
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
                master_gain: MasterGain::default(),
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
            master_gain: MasterGain::default(),
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
    fn pump(output: &mut MixerSource, ms: u32) -> f32 {
        output
            .nth((RATE * ms / 1000 - 1) as usize)
            .expect("mixer has a sink")
    }

    fn playing(elapsed: f64) -> (Audio, Mixer, MixerSource, Authority) {
        playing_with_amplitude(elapsed, 8192)
    }

    fn playing_with_amplitude(
        elapsed: f64,
        amplitude: i16,
    ) -> (Audio, Mixer, MixerSource, Authority) {
        let (mixer, mut output) = mixer(1, RATE);
        let mut audio = Audio::new(
            None,
            true,
            Arc::new(AtomicU64::new(1)),
            1,
            MasterGain::default(),
        );
        let authority = Authority::offline();
        let bytes = wav_samples(9 * RATE, amplitude);
        let run = audio.attach(
            &mixer,
            decoder(bytes.clone(), elapsed).unwrap(),
            authority.clone(),
            UNITY,
        );
        audio.anchor(elapsed, 9.0);
        run.sink.play();
        audio.primary = Some(run);
        audio.cached = Some(("old".into(), bytes));
        assert!((pump(&mut output, 100) - f32::from(amplitude) / 32768.0).abs() < 1e-5);
        (audio, mixer, output, authority)
    }

    fn begin(audio: &mut Audio, mixer: &Mixer, authority: Authority) -> bool {
        audio.start_crossfade_on(mixer, "new".into(), wav_samples(7 * RATE, 16384), authority)
    }

    #[test]
    fn only_playing_decoder_duration_qualifies_and_late_results_are_skipped() {
        let (mut audio, mixer, _, _) = playing(6.0);
        assert!((2.8..3.0).contains(&audio.crossfade_remaining().unwrap()));
        for duration in [None, Some(Duration::ZERO), Some(Duration::from_secs(5))] {
            audio.primary.as_mut().unwrap().decoded_duration = duration;
            assert!(audio.crossfade_remaining().is_none());
        }
        audio.primary.as_mut().unwrap().decoded_duration = Some(Duration::from_secs(9));
        audio.primary.as_ref().unwrap().sink.pause();
        assert!(audio.crossfade_remaining().is_none());

        for elapsed in [3.0, 8.5] {
            let (mut audio, mixer, _, _) = playing(elapsed);
            assert!(!begin(&mut audio, &mixer, Authority::offline()));
            assert_eq!(audio.primary.as_ref().unwrap().sink.volume(), UNITY);
        }
        assert!(!begin(&mut audio, &mixer, Authority::offline()));
    }

    #[test]
    fn mix_reports_primary_then_hands_off_the_same_incoming_position() {
        let (mut audio, mixer, mut output, old) = playing(6.0);
        let new = Authority::offline();
        assert!(begin(&mut audio, &mixer, new.clone()));
        audio.maintain_crossfade();
        assert_eq!(audio.primary.as_ref().unwrap().sink.volume(), UNITY);
        assert_eq!(
            audio.crossfade.as_ref().unwrap().incoming.sink.volume(),
            0.0
        );
        assert!(!audio.promote_crossfade(), "no promotion before real EOF");
        assert!((pump(&mut output, 100) - 0.25).abs() < 1e-5);
        for _ in 0..65 {
            audio.maintain_crossfade();
            pump(&mut output, 20);
        }
        let fade = audio.crossfade.as_ref().unwrap();
        let outgoing_gain = audio.primary.as_ref().unwrap().sink.volume();
        let incoming_gain = fade.incoming.sink.volume();
        assert!((0.4..0.6).contains(&incoming_gain));
        let reserved = UNITY - outgoing_gain - incoming_gain;
        assert!((-1e-6..=MAX_GAIN_STEP + 1e-6).contains(&reserved));
        let wanted = 0.25 * outgoing_gain + 0.5 * incoming_gain;
        assert!((pump(&mut output, 20) - wanted).abs() < 1e-5);
        assert!(audio.facts().playing);
        assert_eq!(audio.facts().duration, 9.0);
        assert!(audio.cached("old").is_some() && audio.cached("new").is_none());
        for _ in 0..100 {
            pump(&mut output, 20);
            audio.maintain_crossfade();
            if audio.facts().ended {
                break;
            }
        }
        assert!(audio.facts().ended);
        let pos = audio.crossfade.as_ref().unwrap().incoming.sink.get_pos();
        assert!(pos > Duration::from_secs(2));
        assert!(audio.promote_crossfade());
        assert!(!old.is_valid() && new.is_valid());
        let facts = audio.facts();
        assert!(facts.playing && !facts.ended && !facts.loading);
        assert_eq!(facts.elapsed, pos.as_secs_f64());
        assert_eq!(facts.duration, 7.0);
        assert!(audio.cached("old").is_none() && audio.cached("new").is_some());
        assert!((pump(&mut output, 20) - 0.5).abs() < 1e-5);
        assert!(audio.facts().elapsed > facts.elapsed);
    }

    #[test]
    fn master_gain_scales_both_envelopes_and_muted_fade_still_hands_off() {
        let (mut audio, mixer, mut output, old) = playing(6.0);
        let gain = audio.master_gain.clone();
        let new = Authority::offline();
        gain.set(0.4);
        assert!(begin(&mut audio, &mixer, new.clone()));
        assert!((pump(&mut output, 100) - 0.1).abs() < 1e-5);
        for _ in 0..65 {
            audio.maintain_crossfade();
            pump(&mut output, 20);
        }
        let incoming = audio.crossfade.as_ref().unwrap().incoming_gain;
        let outgoing = audio.primary.as_ref().unwrap().sink.volume();
        assert!((0.4..0.6).contains(&incoming));
        let mix = 0.25 * outgoing + 0.5 * incoming;
        for level in [0.4, 0.8, 0.0] {
            gain.set(level);
            assert!((pump(&mut output, 20) - mix * level).abs() < 1e-5);
        }
        assert!(audio.facts().playing && old.is_valid() && new.is_valid());
        for _ in 0..100 {
            assert_eq!(pump(&mut output, 20), 0.0);
            audio.maintain_crossfade();
            if audio.facts().ended {
                break;
            }
        }
        let pos = audio.crossfade.as_ref().unwrap().incoming.sink.get_pos();
        assert!(audio.promote_crossfade());
        assert_eq!(audio.facts().elapsed, pos.as_secs_f64());
        assert!(!old.is_valid() && new.is_valid());
        assert_eq!(pump(&mut output, 20), 0.0);
        gain.set(0.8);
        assert!((pump(&mut output, 20) - 0.4).abs() < 1e-5);
    }

    #[test]
    fn master_gain_survives_cancel_and_recreated_online_and_offline_runs() {
        let (mut audio, mixer, mut output, old) = playing(6.0);
        let gain = audio.master_gain.clone();
        let new = Authority::offline();
        assert!(begin(&mut audio, &mixer, new.clone()));
        pump(&mut output, 100);
        audio.maintain_crossfade();
        assert!(audio.primary.as_ref().unwrap().sink.volume() < UNITY);
        gain.set(0.3);
        audio.cancel_crossfade();
        assert!(!new.is_valid() && old.is_valid());
        assert!((pump(&mut output, 20) - 0.075).abs() < 1e-5);
        audio.clear();
        assert!(!old.is_valid());
        for offline in [false, true] {
            let (mixer, mut output) = rodio::mixer::mixer(1, RATE);
            let audio = Audio::new(None, offline, Arc::new(AtomicU64::new(2)), 2, gain.clone());
            let grant = if offline {
                Authority::offline()
            } else {
                Authority::online(|| true, || true)
            };
            let source = decoder(wav_samples(RATE, 16384), 0.0).unwrap();
            let run = audio.attach(&mixer, source, grant, UNITY);
            run.sink.play();
            assert!((pump(&mut output, 20) - 0.15).abs() < 1e-5);
        }
    }

    #[test]
    fn underestimated_end_keeps_a_primary_tail_then_abandons_the_fade() {
        let (mut audio, mixer, mut output, old) = playing(5.0);
        // The WAV actually runs to nine seconds: simulate a decoder estimate
        // that put EOF a full second early, using a silent incoming fixture
        // so any output below is really the primary.
        audio.primary.as_mut().unwrap().decoded_duration = Some(Duration::from_secs(8));
        audio.anchor(5.0, 8.0);
        let new = Authority::offline();
        assert!(audio.start_crossfade_on(
            &mixer,
            "new".into(),
            wav_samples(7 * RATE, 0),
            new.clone()
        ));
        let mut saw_overrun = false;
        for _ in 0..170 {
            let sample = pump(&mut output, 20);
            assert!(sample >= 0.25 * OUTGOING_FLOOR - 1e-6);
            audio.maintain_crossfade();
            if audio.crossfade.is_none() {
                break;
            }
            if audio.crossfade_remaining().is_none() {
                saw_overrun = true;
                let volume = audio.primary.as_ref().unwrap().sink.volume();
                assert!((volume - OUTGOING_FLOOR).abs() < 1e-3);
                assert!(audio.facts().playing && !audio.facts().ended);
            }
        }
        assert!(saw_overrun, "a brief inaccurate tail is not treated as EOF");
        assert!(audio.crossfade.is_none() && !new.is_valid() && old.is_valid());
        assert!(audio.facts().playing && !audio.promote_crossfade());
        assert!((pump(&mut output, 20) - 0.25).abs() < 1e-5);
        pump(&mut output, 1000);
        assert!(
            audio.facts().ended,
            "the same primary runs through real EOF"
        );
    }

    #[test]
    fn staggered_sink_gain_polls_do_not_clip_full_scale_correlated_samples() {
        for offset in 1..=5 {
            let (mut audio, mixer, mut output, _) = playing_with_amplitude(7.5, i16::MIN);
            pump(&mut output, offset);
            assert!(audio.start_crossfade_on(
                &mixer,
                "new".into(),
                wav_samples(7 * RATE, i16::MIN),
                Authority::offline(),
            ));
            for _ in 0..76 {
                audio.maintain_crossfade();
                // A 17 ms actor cadence walks across both sinks' 5 ms
                // polls. Check every sample, not just the settled mix.
                for sample in output.by_ref().take((RATE * 17 / 1000) as usize) {
                    assert!(
                        sample.abs() <= UNITY + 1e-6,
                        "clipped: {sample} at offset {offset}"
                    );
                }
            }
            let fade = audio.crossfade.as_ref().unwrap();
            assert!(fade.incoming_gain > 0.8);
            assert!(
                (UNITY - audio.primary.as_ref().unwrap().sink.volume() - fade.incoming_gain)
                    <= MAX_GAIN_STEP + 1e-6
            );
        }
    }

    #[test]
    fn invalid_or_too_short_incoming_does_not_duck_primary() {
        let (mut audio, mixer, mut output, old) = playing(6.0);
        for bytes in [Arc::from([1u8]), wav_samples(5 * RATE, 16384)] {
            assert!(!audio.start_crossfade_on(&mixer, "new".into(), bytes, Authority::offline()));
        }
        // The declared length passes header validation, but no samples decode.
        let valid = wav_samples(7 * RATE, 16384);
        let truncated = Arc::from(&valid[..44]);
        let new = Authority::offline();
        if audio.start_crossfade_on(&mixer, "new".into(), truncated, new.clone()) {
            audio.maintain_crossfade();
            assert_eq!(audio.primary.as_ref().unwrap().sink.volume(), UNITY);
            assert!((pump(&mut output, 100) - 0.25).abs() < 1e-5);
            audio.maintain_crossfade();
            assert!(!new.is_valid());
        }
        assert!(audio.crossfade.is_none() && old.is_valid());
        assert!((pump(&mut output, 100) - 0.25).abs() < 1e-5);
    }

    #[test]
    fn incoming_eof_before_its_declared_duration_restores_primary() {
        let (mut audio, mixer, mut output, _) = playing(6.0);
        let bytes = wav_samples(7 * RATE, 16384);
        // Enough payload to become audible, but much shorter than its header.
        let truncated = Arc::from(&bytes[..44 + RATE as usize]);
        let new = Authority::offline();
        assert!(audio.start_crossfade_on(&mixer, "new".into(), truncated, new.clone()));
        pump(&mut output, 100);
        audio.maintain_crossfade();
        pump(&mut output, 20);
        audio.maintain_crossfade();
        assert!(audio.crossfade.as_ref().unwrap().incoming_gain > 0.0);
        pump(&mut output, 600);
        audio.maintain_crossfade();
        assert!(audio.crossfade.is_none() && !new.is_valid());
        assert!(audio.facts().playing);
        assert!((pump(&mut output, 20) - 0.25).abs() < 1e-5);
    }

    #[test]
    fn fenced_and_cancelled_incoming_recover_primary_and_transport_retires_both() {
        let (mut audio, mixer, mut output, old) = playing(6.0);
        let new = Authority::offline();
        assert!(begin(&mut audio, &mixer, new.clone()));
        pump(&mut output, 100);
        audio.maintain_crossfade();
        assert!(audio.primary.as_ref().unwrap().sink.volume() < UNITY);
        new.revoke();
        pump(&mut output, 20);
        assert!(audio.crossfade.as_ref().unwrap().incoming.was_fenced());
        audio.maintain_crossfade();
        assert!(audio.crossfade.is_none());
        assert!((pump(&mut output, 20) - 0.25).abs() < 1e-5);

        let new = Authority::offline();
        assert!(begin(&mut audio, &mixer, new.clone()));
        pump(&mut output, 100);
        audio.maintain_crossfade();
        audio.cancel_crossfade();
        assert!(!new.is_valid() && old.is_valid());
        assert!((pump(&mut output, 20) - 0.25).abs() < 1e-5);

        let new = Authority::offline();
        assert!(begin(&mut audio, &mixer, new.clone()));
        assert_eq!(
            audio.play(
                "bad".into(),
                Arc::from([1u8]),
                0.0,
                0.0,
                Authority::offline()
            ),
            Status::Media
        );
        assert!(audio.crossfade.is_none() && audio.primary.is_none());
        assert!(!new.is_valid() && !old.is_valid());
    }

    #[test]
    fn epoch_fences_both_overlapping_decoders_at_the_sample_boundary() {
        let (mut audio, mixer, mut output, _) = playing(6.0);
        assert!(begin(&mut audio, &mixer, Authority::offline()));
        pump(&mut output, 100);
        audio.maintain_crossfade();
        pump(&mut output, 20);
        audio.maintain_crossfade();
        assert!(audio.crossfade.as_ref().unwrap().incoming_gain > 0.0);
        assert!(pump(&mut output, 20) > 0.25);

        audio.epoch.store(2, Ordering::SeqCst);
        assert_eq!(pump(&mut output, 20), 0.0);
        assert!(audio.was_fenced());
        assert!(audio.crossfade.as_ref().unwrap().incoming.was_fenced());
        assert!(!audio.facts().ended);
        audio.maintain_crossfade();
        assert!(audio.crossfade.is_none());
    }

    #[test]
    fn malformed_media_never_opens_an_output() {
        let mut a = Audio::new(
            None,
            false,
            Arc::new(AtomicU64::new(1)),
            1,
            MasterGain::default(),
        );
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
