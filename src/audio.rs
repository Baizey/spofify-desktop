//! Native output. PCM reaches the mixer only through an exact operation
//! capability, session, and account/mode epoch checked at the sample boundary.
#[path = "audio_pcm.rs"]
mod pcm;

use crate::{
    api::Session,
    audio_input::AudioInput,
    player::{Authority, Status},
};
use pcm::{Format, NativeSinkOutput, Pcm, State};
use rodio::{OutputStream, OutputStreamBuilder, Sink, mixer::Mixer};
use std::{
    sync::{
        Arc,
        atomic::{AtomicU32, AtomicU64, Ordering},
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
    #[cfg(test)]
    test_mixer: Option<(Mixer, Format)>,
    #[cfg(test)]
    test_eager: bool,
    primary: Option<Run>,
    crossfade: Option<Crossfade>,
    master_gain: MasterGain,
    cached: Option<(String, AudioInput)>,
    scope: OutputScope,
    elapsed: f64,
    duration: f64,
}
struct Run {
    sink: Sink,
    authority: Authority,
    pcm: Arc<State>,
    #[cfg(test)]
    duration_override: Option<Option<Duration>>,
}
impl Run {
    fn duration(&self) -> Option<Duration> {
        #[cfg(test)]
        if let Some(duration) = self.duration_override {
            return duration;
        }
        self.pcm.duration()
    }
    fn has_started(&self) -> bool {
        self.pcm.started.load(Ordering::Acquire)
    }
    fn was_fenced(&self) -> bool {
        self.pcm.fenced_out.load(Ordering::Acquire)
    }
    fn ended(&self) -> bool {
        self.sink.empty() && self.pcm.exhausted.load(Ordering::Acquire)
    }
    fn can_continue(&self) -> bool {
        self.authority.is_valid()
            && !self.was_fenced()
            && self.pcm.failure().is_none()
            && !self.pcm.exhausted.load(Ordering::Acquire)
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
    cached: (String, AudioInput),
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
        let network = session.as_ref().map(Session::network_changes);
        Self {
            stream: None,
            #[cfg(test)]
            test_mixer: None,
            #[cfg(test)]
            test_eager: false,
            primary: None,
            crossfade: None,
            master_gain,
            cached: None,
            scope: OutputScope {
                session,
                network,
                offline,
                epoch,
                expected,
            },
            elapsed: 0.0,
            duration: 0.0,
        }
    }
    #[cfg(test)]
    pub(crate) fn use_test_output(&mut self, mixer: Mixer, id: String, bytes: Arc<[u8]>) {
        // The headless engine PCM fixtures both encode and mix at 8 kHz mono.
        self.use_test_streaming_output(mixer, 1, 8000);
        self.test_eager = true;
        self.cached = Some((id, bytes.into()));
    }
    #[cfg(test)]
    pub(crate) fn use_test_streaming_output(&mut self, mixer: Mixer, channels: u16, rate: u32) {
        self.test_mixer = Some((mixer, Format { channels, rate }));
        self.test_eager = false;
    }
    pub fn cached(&self, id: &str) -> Option<AudioInput> {
        self.cached
            .as_ref()
            .filter(|(loaded, _)| loaded == id)
            .map(|(_, source)| source.clone())
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
    fn playback_mixer(&self) -> Option<(Mixer, Format)> {
        #[cfg(test)]
        if let Some(output) = &self.test_mixer {
            return Some(output.clone());
        }
        self.stream.as_ref().map(|stream| {
            let config = stream.config();
            (
                stream.mixer().clone(),
                Format {
                    channels: config.channel_count(),
                    rate: config.sample_rate(),
                },
            )
        })
    }
    pub fn play(
        &mut self,
        id: String,
        source: impl Into<AudioInput>,
        elapsed: f64,
        duration: f64,
        authority: Authority,
    ) -> Status {
        self.stop();
        self.anchor(elapsed, duration);
        if !self.current() || !authority.may_start() {
            return Status::Rejected;
        }
        if self.playback_mixer().is_none()
            && let Err(e) = self.output()
        {
            return e;
        }
        let output = self.playback_mixer().expect("output initialized");
        let source = source.into();
        let run = match self.attach(&output, source.clone(), self.elapsed, authority, false) {
            Ok(run) => run,
            Err(e) => return e,
        };
        self.cached = Some((id, source));
        self.primary = Some(run);
        Status::Ok
    }
    fn attach(
        &self,
        (mixer, format): &(Mixer, Format),
        input: AudioInput,
        elapsed: f64,
        authority: Authority,
        fade: bool,
    ) -> Result<Run, Status> {
        if !self.current() || !authority.may_start() {
            return Err(Status::Rejected);
        }
        let pcm = Arc::new(State::new(*format));
        let fence = Fence {
            scope: self.scope.clone(),
            authority: authority.clone(),
        };
        // Only old deterministic headless tests opt into eager in-memory PCM.
        #[cfg(test)]
        let spawn = !(self.test_eager && input.cacheable());
        #[cfg(test)]
        if !spawn {
            pcm.decode(input.clone(), elapsed, fade, fence.clone(), false);
            if let Some(e) = pcm.failure() {
                authority.revoke();
                return Err(e);
            }
        }
        #[cfg(not(test))]
        let spawn = true;
        if spawn {
            pcm.spawn(input.decoder_input(), elapsed, fade, fence.clone())?;
        }
        let (sink, output) = Sink::new();
        sink.set_volume(if fade { 0.0 } else { UNITY });
        let run = Run {
            sink,
            authority,
            pcm: pcm.clone(),
            #[cfg(test)]
            duration_override: None,
        };
        if !self.current() || !run.authority.may_start() {
            return Err(Status::Rejected);
        }
        run.sink
            .append(Pcm::new(pcm, fence, self.master_gain.clone()));
        mixer.add(NativeSinkOutput(output, *format));
        Ok(run)
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
        let facts = self.facts();
        self.primary = None;
        self.crossfade = None;
        self.anchor(facts.elapsed, facts.duration);
        if self
            .cached
            .as_ref()
            .is_some_and(|(_, source)| !source.cacheable())
        {
            self.discard_cache();
        }
    }
    pub fn clear(&mut self) {
        self.stop();
        self.cached = None;
        self.stream = None;
        self.anchor(0.0, 0.0);
    }
    fn current(&self) -> bool {
        self.scope.current()
    }
    pub fn failure(&self) -> Option<Status> {
        self.primary.as_ref().and_then(|run| run.pcm.failure())
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
            let active = allowed && run.pcm.failure().is_none() && !run.sink.is_paused();
            let buffering = run.pcm.buffering();
            facts.ended = active && run.ended();
            facts.duration = run
                .duration()
                .map_or(self.duration, |d| d.as_secs_f64().min(MAX_SECONDS));
            let elapsed = (self.elapsed + run.pcm.position().as_secs_f64()).min(MAX_SECONDS);
            facts.elapsed = if facts.duration > 0.0 && facts.ended {
                facts.duration
            } else if facts.duration > 0.0 {
                elapsed.min(facts.duration)
            } else {
                elapsed
            };
            facts.playing = active && started && !buffering && !facts.ended;
            facts.loading = active && (!started || buffering) && !facts.ended;
        }
        facts
    }

    /// Only decoder timing qualifies for an overlap; catalog durations are
    /// still useful to display normal playback and never trigger mixing.
    pub fn crossfade_remaining(&self) -> Option<f64> {
        let run = self.primary.as_ref()?;
        let duration = fade_duration(run.duration())?;
        if !self.current() || !run.can_continue() || !run.has_started() || run.pcm.buffering() {
            return None;
        }
        let remaining = duration - (self.elapsed + run.pcm.position().as_secs_f64());
        (remaining > 0.0).then_some(remaining)
    }

    /// Admit a separate run; `authority` must not be shared with the primary.
    /// Once admitted, cancellation permanently revokes the incoming capability.
    pub fn start_crossfade(
        &mut self,
        id: String,
        source: impl Into<AudioInput>,
        authority: Authority,
    ) -> bool {
        if self.crossfade.is_some() || !authority.may_start() {
            return false;
        }
        let (Some(output), Some(overlap)) = (self.playback_mixer(), self.overlap()) else {
            return false;
        };
        let source = source.into();
        let Ok(incoming) = self.attach(&output, source.clone(), 0.0, authority, true) else {
            return false;
        };
        // Pending decoder runs are silent; actual consumption, not admission
        // or even successful probing, opens the fade envelope.
        self.crossfade = Some(Crossfade {
            incoming,
            cached: (id, source),
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
            || outgoing.pcm.failure().is_some()
            || !fade.incoming.can_continue()
            || (fade.attenuation > 0.0 && fade.incoming.pcm.buffering())
            || (fade.incoming_gain == 0.0 && !fade.incoming.authority.may_start())
        {
            self.cancel_crossfade();
            return;
        }
        let position = outgoing.pcm.position();
        // Natural EOF is a handoff point, not a failure of the overlap.
        if outgoing.pcm.exhausted.load(Ordering::Acquire) {
            return;
        }
        if outgoing.pcm.buffering() {
            // A stalled primary cannot settle its envelope or hold the incoming
            // song near the canonical transition. Resume the primary alone.
            self.cancel_crossfade();
            return;
        }
        let duration = fade_duration(outgoing.duration()).expect("overlap has a duration");
        let remaining = duration - (self.elapsed + position.as_secs_f64());
        // Decoders may estimate duration (e.g. MP3 without a seek table). Keep
        // an audible tail through modest errors; don't let an early estimate
        // leave a primary ducked indefinitely when it still has samples.
        if remaining < -MAX_OVERRUN_SECONDS {
            self.cancel_crossfade();
            return;
        }
        if !fade.incoming.has_started()
            || fade.incoming.pcm.buffering()
            || fade.incoming.pcm.position().is_zero()
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
            fade.position_at_update = outgoing.pcm.position();
        }
    }

    pub fn promote_crossfade(&mut self) -> bool {
        if self.crossfade.is_none() || !self.facts().ended {
            return false;
        }
        let fade = self.crossfade.as_ref().unwrap();
        if fade.incoming_gain == 0.0
            || !fade.incoming.can_continue()
            || fade.incoming.pcm.buffering()
        {
            // A run whose envelope never opened should start from zero
            // through ordinary playback, not inherit pre-roll samples.
            self.cancel_crossfade();
            return false;
        }
        let fade = self.crossfade.take().unwrap();
        self.primary = None;
        fade.incoming.sink.set_volume(UNITY);
        self.duration = fade.incoming.duration().unwrap().as_secs_f64();
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

#[derive(Clone)]
struct OutputScope {
    session: Option<Session>,
    network: Option<tokio::sync::watch::Receiver<u64>>,
    offline: bool,
    epoch: Arc<AtomicU64>,
    expected: u64,
}
impl OutputScope {
    fn current(&self) -> bool {
        self.epoch.load(Ordering::SeqCst) == self.expected
            && self
                .network
                .as_ref()
                .is_none_or(|mode| matches!(mode.has_changed(), Ok(false)))
            && self.session.as_ref().is_none_or(|session| {
                // Offline is an explicit local grant; cookie time is irrelevant
                // until Go Online. A mode round trip still retires this scope.
                session.is_offline() == self.offline && session.can_access_local()
            })
    }
}

#[derive(Clone)]
struct Fence {
    scope: OutputScope,
    authority: Authority,
}
impl Fence {
    fn live(&self, state: &State) -> bool {
        if state.fenced_out.load(Ordering::Acquire) {
            return false;
        }
        let live = self.scope.current()
            && self.authority.is_valid()
            && (state.started.load(Ordering::Acquire)
                || self.authority.may_start()
                // The worker and mixer can cross the admission boundary at
                // the same time. Already-consumed PCM may use ordinary grace.
                || state.started.load(Ordering::Acquire));
        if !live {
            state.fenced_out.store(true, Ordering::Release);
        }
        live
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rodio::mixer::{MixerSource, mixer};
    use std::{
        fs::{self, File, OpenOptions},
        io::{Seek, SeekFrom, Write},
        thread,
        time::Instant,
    };

    const RATE: u32 = 8000;

    fn wav_header(samples: u32) -> Vec<u8> {
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
        b
    }

    fn wav_samples(samples: u32, amplitude: i16) -> Arc<[u8]> {
        let mut b = wav_header(samples);
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
            let scope = OutputScope {
                session: None,
                network: None,
                offline: !online,
                epoch: epoch.clone(),
                expected: 1,
            };
            let fence = Fence {
                scope,
                authority: grant.clone(),
            };
            let state = Arc::new(State::new(Format {
                channels: 1,
                rate: RATE,
            }));
            state.decode(
                wav_samples(100, 1000).into(),
                0.004,
                false,
                fence.clone(),
                false,
            );
            let mut source = Pcm::new(state, fence, MasterGain::default());
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
        let attached = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let start = attached.clone();
        let grant = Authority::online(|| true, move || start.load(Ordering::Acquire));
        let scope = OutputScope {
            session: None,
            network: None,
            offline: false,
            epoch: Arc::new(AtomicU64::new(1)),
            expected: 1,
        };
        let fence = Fence {
            scope,
            authority: grant,
        };
        let state = Arc::new(State::new(Format {
            channels: 1,
            rate: RATE,
        }));
        let mut source = Pcm::new(state.clone(), fence, MasterGain::default());
        assert!(source.next().is_none());
        assert!(state.fenced_out.load(Ordering::Acquire));
        assert!(!state.exhausted.load(Ordering::Acquire));
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
        audio.use_test_output(mixer.clone(), "old".into(), bytes.clone());
        assert_eq!(
            audio.play("old".into(), bytes, elapsed, 9.0, authority.clone()),
            Status::Ok
        );
        assert!((pump(&mut output, 100) - f32::from(amplitude) / 32768.0).abs() < 1e-5);
        (audio, mixer, output, authority)
    }

    fn begin(audio: &mut Audio, _mixer: &Mixer, authority: Authority) -> bool {
        audio.start_crossfade("new".into(), wav_samples(7 * RATE, 16384), authority)
    }

    #[test]
    fn only_playing_decoder_duration_qualifies_and_late_results_are_skipped() {
        let (mut audio, mixer, _, _) = playing(6.0);
        assert!((2.8..3.0).contains(&audio.crossfade_remaining().unwrap()));
        for duration in [None, Some(Duration::ZERO), Some(Duration::from_secs(5))] {
            audio.primary.as_mut().unwrap().duration_override = Some(duration);
            assert!(audio.crossfade_remaining().is_none());
        }
        audio.primary.as_mut().unwrap().duration_override = None;
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
        let pos = audio.crossfade.as_ref().unwrap().incoming.pcm.position();
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
        let pos = audio.crossfade.as_ref().unwrap().incoming.pcm.position();
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
            let mut audio = Audio::new(None, offline, Arc::new(AtomicU64::new(2)), 2, gain.clone());
            let grant = if offline {
                Authority::offline()
            } else {
                Authority::online(|| true, || true)
            };
            let source = wav_samples(RATE, 16384);
            audio.use_test_output(mixer, "tone".into(), source.clone());
            assert_eq!(
                audio.play("tone".into(), source, 0.0, 1.0, grant),
                Status::Ok
            );
            assert!((pump(&mut output, 20) - 0.15).abs() < 1e-5);
        }
    }

    #[test]
    fn underestimated_end_keeps_a_primary_tail_then_abandons_the_fade() {
        let (mut audio, _, mut output, old) = playing(5.0);
        // The WAV actually runs to nine seconds: simulate a decoder estimate
        // that put EOF a full second early, using a silent incoming fixture
        // so any output below is really the primary.
        audio.primary.as_mut().unwrap().duration_override = Some(Some(Duration::from_secs(8)));
        audio.anchor(5.0, 8.0);
        let new = Authority::offline();
        assert!(audio.start_crossfade("new".into(), wav_samples(7 * RATE, 0), new.clone()));
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
            let (mut audio, _, mut output, _) = playing_with_amplitude(7.5, i16::MIN);
            pump(&mut output, offset);
            assert!(audio.start_crossfade(
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
        let (mut audio, _, mut output, old) = playing(6.0);
        for bytes in [Arc::from([1u8]), wav_samples(5 * RATE, 16384)] {
            assert!(!audio.start_crossfade("new".into(), bytes, Authority::offline()));
        }
        // The declared length passes header validation, but no samples decode.
        let valid = wav_samples(7 * RATE, 16384);
        let truncated = Arc::<[u8]>::from(&valid[..44]);
        let new = Authority::offline();
        if audio.start_crossfade("new".into(), truncated, new.clone()) {
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
    fn corrupt_incoming_eof_before_its_declared_duration_never_ducks_primary() {
        let (mut audio, _, mut output, _) = playing(6.0);
        let bytes = wav_samples(7 * RATE, 16384);
        // This is a complete representation with a lying RIFF header, not
        // a short transfer. The worker can decode some PCM before noticing.
        let truncated = Arc::<[u8]>::from(&bytes[..44 + RATE as usize / 2]);
        audio.test_eager = false;
        let new = Authority::offline();
        assert!(audio.start_crossfade("new".into(), truncated, new.clone()));
        wait_until(|| {
            audio.crossfade.as_ref().unwrap().incoming.pcm.failure() == Some(Status::Media)
        });
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
                Arc::<[u8]>::from([1u8]),
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

        audio.scope.epoch.store(2, Ordering::SeqCst);
        assert_eq!(pump(&mut output, 20), 0.0);
        assert!(audio.was_fenced());
        assert!(audio.crossfade.as_ref().unwrap().incoming.was_fenced());
        assert!(!audio.facts().ended);
        audio.maintain_crossfade();
        assert!(audio.crossfade.is_none());
    }

    fn async_output(channels: u16) -> (Audio, MixerSource) {
        let (mixer, output) = mixer(channels, RATE);
        let mut audio = Audio::new(
            None,
            true,
            Arc::new(AtomicU64::new(1)),
            1,
            MasterGain::default(),
        );
        audio.use_test_streaming_output(mixer, channels, RATE);
        (audio, output)
    }

    fn wait_until(mut ready: impl FnMut() -> bool) {
        let until = Instant::now() + Duration::from_secs(5);
        while !ready() {
            assert!(
                Instant::now() < until,
                "timed out waiting for decoder worker"
            );
            thread::sleep(Duration::from_millis(1));
        }
    }

    fn fixture_file() -> File {
        let path = std::env::temp_dir().join(format!("spofify-pcm-{}.wav", uuid::Uuid::new_v4()));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        fs::remove_file(&path).unwrap();
        file
    }

    #[test]
    fn file_over_32_mib_starts_seeks_and_ends_with_bounded_backpressure() {
        let frames = 32 * 1024 * 1024 / 2 + RATE;
        let len = 44 + u64::from(frames) * 2;
        let mut file = fixture_file();
        file.write_all(&wav_header(frames)).unwrap();
        let intro = vec![8192i16; 8192];
        let intro = intro
            .into_iter()
            .flat_map(i16::to_le_bytes)
            .collect::<Vec<_>>();
        file.write_all(&intro).unwrap();
        file.set_len(len).unwrap(); // sparse silence is valid PCM
        file.seek(SeekFrom::End(-800)).unwrap();
        file.write_all(&16384i16.to_le_bytes().repeat(400)).unwrap();
        let source = AudioInput::file(file, 0, len);
        assert!(source.len() > 32 * 1024 * 1024);

        let (mut audio, mut output) = async_output(1);
        let first = Authority::offline();
        assert_eq!(
            audio.play("large".into(), source.clone(), 0.0, 0.0, first.clone()),
            Status::Ok
        );
        let retired = audio.primary.as_ref().unwrap().pcm.clone();
        wait_until(|| retired.queued_frames() == State::queue_limit_frames());
        assert!(!audio.has_started() && audio.facts().elapsed == 0.0 && audio.facts().loading);
        assert!(
            !retired.worker_exited.load(Ordering::Acquire),
            "producer is backpressured"
        );
        assert_eq!(pump(&mut output, 20), 0.25);
        assert_eq!(audio.facts().elapsed, 0.020);
        wait_until(|| retired.queued_frames() == State::queue_limit_frames());

        let duration = f64::from(frames) / f64::from(RATE);
        let target = duration - 0.025;
        assert_eq!(
            audio.play(
                "large".into(),
                source,
                target,
                duration,
                Authority::offline()
            ),
            Status::Ok
        );
        assert!(!first.is_valid());
        let run = &audio.primary.as_ref().unwrap().pcm;
        wait_until(|| run.worker_exited.load(Ordering::Acquire));
        wait_until(|| retired.worker_exited.load(Ordering::Acquire));
        assert!(
            retired.fenced_out.load(Ordering::Acquire),
            "retired backpressured worker checked its fence without mixer help"
        );
        assert!(!audio.has_started() && !audio.facts().ended);
        assert_eq!(audio.facts().elapsed, target);
        assert_eq!(pump(&mut output, 10), 0.5, "only new seek PCM is audible");
        assert!((audio.facts().elapsed - target - 0.010).abs() < 1e-8);
        pump(&mut output, 100);
        assert_eq!(audio.failure(), None);
        assert!(audio.facts().ended && audio.has_started());
        assert_eq!(audio.facts().elapsed, duration);
    }

    #[test]
    fn backing_file_read_failure_is_not_natural_eof() {
        let mut file = fixture_file();
        let frames = RATE * 2;
        file.write_all(&wav_header(frames)).unwrap();
        file.write_all(&8192i16.to_le_bytes().repeat(2000)).unwrap();
        let (mut audio, mut output) = async_output(1);
        let source = AudioInput::file(file, 0, 44 + u64::from(frames) * 2);
        assert_eq!(
            audio.play("short".into(), source, 0.0, 2.0, Authority::offline()),
            Status::Ok
        );
        wait_until(|| audio.failure() == Some(Status::Load));
        pump(&mut output, 100);
        let facts = audio.facts();
        assert!(!facts.ended && !facts.loading && !facts.playing);
    }

    #[test]
    fn complete_representation_with_a_truncated_wav_payload_is_media_not_eof() {
        let mut file = fixture_file();
        file.write_all(&wav_header(RATE * 3)).unwrap();
        file.write_all(&8192i16.to_le_bytes().repeat(2000)).unwrap();
        let (mut audio, mut output) = async_output(1);
        // Actual input length is known and satisfied; only RIFF claims more.
        let source = AudioInput::file(file, 0, 44 + 4000);
        assert_eq!(
            audio.play("bad".into(), source, 0.0, 3.0, Authority::offline()),
            Status::Ok
        );
        wait_until(|| audio.failure() == Some(Status::Media));
        pump(&mut output, 100);
        assert!(!audio.facts().ended && !audio.facts().loading);
    }

    #[test]
    fn malformed_media_fails_asynchronously_without_starting_or_ending() {
        let (mut audio, mut output) = async_output(1);
        assert_eq!(
            audio.play(
                "bad".into(),
                Arc::<[u8]>::from([1]),
                0.0,
                0.0,
                Authority::offline()
            ),
            Status::Ok
        );
        wait_until(|| audio.failure() == Some(Status::Media));
        assert_eq!(pump(&mut output, 20), 0.0);
        assert!(!audio.has_started());
        assert!(!audio.facts().ended && !audio.facts().loading);
    }
}
