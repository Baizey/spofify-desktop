//! Density, crossfade and output volume are device-local preferences.

use crate::{AppWindow, audio::MasterGain};
use serde::{
    Deserialize, Serialize,
    de::{DeserializeOwned, IgnoredAny},
};
use slint::ComponentHandle;
use std::{
    env,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const VERSION: u8 = 1;
const MAX_BYTES: usize = 4096;
const MIN_SCALE: u8 = 85;
const MAX_SCALE: u8 = 125;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
struct Density {
    version: u8,
    scale_percent: u8,
}

impl Default for Density {
    fn default() -> Self {
        Self {
            version: VERSION,
            scale_percent: 100,
        }
    }
}

impl Density {
    fn from_ui(scale: f32) -> Option<Self> {
        if !(f32::from(MIN_SCALE) / 100.0..=f32::from(MAX_SCALE) / 100.0).contains(&scale) {
            return None;
        }
        Some(Self {
            version: VERSION,
            scale_percent: (scale * 100.0).round() as u8,
        })
    }

    fn apply(self, ui: &AppWindow) {
        ui.set_density_scale(f32::from(self.scale_percent) / 100.0);
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Playback {
    version: u8,
    crossfade: bool,
}

fn config_path(file: &str) -> Option<PathBuf> {
    let base = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| {
            env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .map(|p| p.join(".config"))
        })?;
    Some(base.join("spofify-desktop").join(file))
}

fn load<T>(path: &Path, parse: fn(&[u8]) -> Option<T>) -> Option<T> {
    let meta = fs::symlink_metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_BYTES as u64 {
        return None;
    }
    let mut bytes = Vec::new();
    File::open(path)
        .ok()?
        .take(MAX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    parse(&bytes)
}

fn parse_config<T: DeserializeOwned>(bytes: &[u8]) -> Option<T> {
    if bytes.len() > MAX_BYTES {
        return None;
    }
    serde_json::from_slice(bytes).ok()
}

const MANUAL_MODE: &str = "offline-mode-v2.json";

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ManualMode {
    manual_offline: bool,
}

#[cfg(not(test))]
pub(crate) fn manual_offline() -> bool {
    config_path(MANUAL_MODE).is_some_and(|path| manual_mode_at(&path))
}

pub(crate) fn manual_mode_at(path: &Path) -> bool {
    // A missing preference means the default. Unreadable existing state is
    // not permission to start networking.
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => false,
        _ => load(path, parse_config::<ManualMode>).is_none_or(|mode| mode.manual_offline),
    }
}

#[cfg(not(test))]
pub(crate) fn save_manual_offline(manual_offline: bool) -> io::Result<()> {
    let path = config_path(MANUAL_MODE).ok_or(io::ErrorKind::NotFound)?;
    save_manual_mode_at(&path, manual_offline)
}

pub(crate) fn save_manual_mode_at(path: &Path, manual_offline: bool) -> io::Result<()> {
    save(path, &ManualMode { manual_offline })?;
    let parent = path.parent().ok_or(io::ErrorKind::NotFound)?;
    File::open(parent)?.sync_all()?;
    if let Some(config) = parent.parent() {
        File::open(config)?.sync_all()?;
    }
    Ok(())
}

fn parse_density(bytes: &[u8]) -> Option<Density> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct StoredDensity {
        version: u8,
        scale_percent: u8,
        // Read v1 appearance files without applying or writing their obsolete theme.
        #[serde(default, rename = "theme")]
        _legacy_theme: Option<IgnoredAny>,
    }
    let StoredDensity {
        version,
        scale_percent,
        ..
    } = parse_config(bytes)?;
    (version == VERSION && (MIN_SCALE..=MAX_SCALE).contains(&scale_percent)).then_some(Density {
        version,
        scale_percent,
    })
}

fn parse_playback(bytes: &[u8]) -> Option<Playback> {
    let playback: Playback = parse_config(bytes)?;
    (playback.version == VERSION).then_some(playback)
}

struct TempFile(Option<PathBuf>);
impl Drop for TempFile {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = fs::remove_file(path);
        }
    }
}

fn save(path: &Path, config: &impl Serialize) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Missing config directory"))?;
    let mut dirs = fs::DirBuilder::new();
    dirs.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        dirs.mode(0o700);
    }
    dirs.create(parent)?;
    let bytes = serde_json::to_vec(config)?;
    let tmp = parent.join(format!(".preferences-{}.tmp", uuid::Uuid::new_v4()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&tmp)?;
    let mut pending = TempFile(Some(tmp));
    file.write_all(&bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(pending.0.as_ref().expect("owned temporary file"), path)?;
    pending.0 = None;
    Ok(())
}

fn load_crossfade(path: Option<&Path>) -> bool {
    path.and_then(|p| load(p, parse_playback))
        .is_some_and(|p| p.crossfade)
}

fn save_crossfade(path: Option<&Path>, live: &AtomicBool, enabled: bool) -> io::Result<()> {
    let path = path.ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "No config path"))?;
    save(
        path,
        &Playback {
            version: VERSION,
            crossfade: enabled,
        },
    )?;
    live.store(enabled, Ordering::Relaxed);
    Ok(())
}

/// Bind the offline-capable playback preference before the engine starts.
pub(crate) fn install_playback(ui: &AppWindow) -> Arc<AtomicBool> {
    let path = config_path("playback.json");
    let initial = load_crossfade(path.as_deref());
    let live = Arc::new(AtomicBool::new(initial));
    ui.set_crossfade_enabled(initial);
    let (weak, setting) = (ui.as_weak(), live.clone());
    ui.on_crossfade_changed(move |enabled| {
        let Some(ui) = weak.upgrade() else { return };
        if save_crossfade(path.as_deref(), &setting, enabled).is_ok() {
            ui.set_crossfade_enabled(enabled);
            ui.set_crossfade_status("".into());
        } else {
            ui.set_crossfade_status(
                "Couldn’t save Crossfade on this desktop. The setting hasn’t changed.".into(),
            );
        }
    });
    live
}

const VOLUME_SAVE_DELAY: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct LocalVolume {
    version: u8,
    level: f32,
    muted: bool,
}

impl Default for LocalVolume {
    fn default() -> Self {
        Self {
            version: VERSION,
            level: 1.0,
            muted: false,
        }
    }
}

impl LocalVolume {
    fn set_level(&mut self, level: f32) {
        if level.is_finite() {
            self.level = level.clamp(0.0, 1.0);
        }
    }

    fn effective(self) -> f32 {
        if self.muted { 0.0 } else { self.level }
    }

    fn apply(self, ui: &AppWindow, gain: &MasterGain) {
        gain.set(self.effective());
        ui.set_local_volume(self.level);
        ui.set_local_muted(self.muted);
    }
}

fn parse_volume(bytes: &[u8]) -> Option<LocalVolume> {
    let volume: LocalVolume = parse_config(bytes)?;
    (volume.version == VERSION && (0.0..=1.0).contains(&volume.level)).then_some(volume)
}

struct VolumeState {
    value: LocalVolume,
    changed: Option<Instant>,
    closed: bool,
}

struct PendingVolume {
    state: Mutex<VolumeState>,
    wake: Condvar,
}

impl PendingVolume {
    fn update(&self, change: impl FnOnce(&mut LocalVolume)) -> LocalVolume {
        let mut state = self.state.lock().unwrap();
        let before = state.value;
        change(&mut state.value);
        // A dirty writer already has a wakeup scheduled. Drag samples only
        // extend its deadline; they do not queue individual disk writes.
        if before != state.value && state.changed.replace(Instant::now()).is_none() {
            self.wake.notify_one();
        }
        state.value
    }
}

pub(crate) struct VolumeWriter {
    pending: Arc<PendingVolume>,
    worker: Option<JoinHandle<()>>,
}

impl VolumeWriter {
    fn start(path: Option<PathBuf>, initial: LocalVolume) -> Self {
        let pending = Arc::new(PendingVolume {
            state: Mutex::new(VolumeState {
                value: initial,
                changed: None,
                closed: false,
            }),
            wake: Condvar::new(),
        });
        let worker = path.and_then(|path| {
            let pending = pending.clone();
            thread::Builder::new()
                .name("desktop-volume-save".into())
                .spawn(move || write_volume(&pending, &path, initial))
                .map_err(|_| eprintln!("Could not start the local volume settings writer."))
                .ok()
        });
        Self { pending, worker }
    }
}

impl Drop for VolumeWriter {
    fn drop(&mut self) {
        self.pending.state.lock().unwrap().closed = true;
        self.pending.wake.notify_one();
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            eprintln!("Local volume settings writer closed unexpectedly.");
        }
    }
}

fn write_volume(pending: &PendingVolume, path: &Path, mut saved: LocalVolume) {
    let mut state = pending.state.lock().unwrap();
    loop {
        let Some(changed) = state.changed else {
            if state.closed {
                return;
            }
            state = pending.wake.wait(state).unwrap();
            continue;
        };
        let wait = VOLUME_SAVE_DELAY.saturating_sub(changed.elapsed());
        if !state.closed && !wait.is_zero() {
            state = pending.wake.wait_timeout(state, wait).unwrap().0;
            continue;
        }
        let next = state.value;
        state.changed = None;
        // Neither persistence nor its failures may block live gain updates.
        drop(state);
        if next != saved {
            match save(path, &next) {
                Ok(()) => saved = next,
                Err(_) => eprintln!("Could not save local volume settings."),
            }
        }
        state = pending.state.lock().unwrap();
    }
}

/// Install the desktop output control independently of login and playback mode.
pub(crate) fn install_local_volume(ui: &AppWindow) -> (MasterGain, VolumeWriter) {
    let path = config_path("local-volume.json");
    let initial = path
        .as_deref()
        .and_then(|path| load(path, parse_volume))
        .unwrap_or_default();
    let gain = MasterGain::default();
    initial.apply(ui, &gain);
    let writer = VolumeWriter::start(path, initial);
    let (weak, live, pending) = (ui.as_weak(), gain.clone(), writer.pending.clone());
    let apply = move |change: &dyn Fn(&mut LocalVolume)| {
        if let Some(ui) = weak.upgrade() {
            pending.update(change).apply(&ui, &live);
        }
    };
    let set_level = apply.clone();
    ui.on_local_volume_change(move |level| set_level(&|volume| volume.set_level(level)));
    ui.on_local_mute_toggle(move || apply(&|volume| volume.muted = !volume.muted));
    (gain, writer)
}

/// Read density before showing the window; update the file only on a user change.
pub(crate) fn install_density(ui: &AppWindow) {
    let path = config_path("appearance.json");
    path.as_deref()
        .and_then(|p| load(p, parse_density))
        .unwrap_or_default()
        .apply(ui);
    let weak = ui.as_weak();
    ui.on_density_changed(move |scale| {
        if let (Some(ui), Some(next)) = (weak.upgrade(), Density::from_ui(scale)) {
            if ui.get_density_scale() == f32::from(next.scale_percent) / 100.0 {
                return;
            }
            next.apply(&ui);
            if let Some(path) = path.as_deref() {
                let _ = save(path, &next);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn density_is_bounded_and_versioned() {
        for pct in [85, 100, 125] {
            let density = Density {
                version: VERSION,
                scale_percent: pct,
            };
            let json = serde_json::to_vec(&density).unwrap();
            assert_eq!(parse_density(&json), Some(density));
        }
        for invalid in [
            br#"{"version":2,"theme":"dark","scale_percent":100}"#.as_slice(),
            br#"{"version":1,"scale_percent":84}"#,
            br#"{"version":1,"scale_percent":126}"#,
            br#"{"version":1,"scale_percent":"100"}"#,
        ] {
            assert!(parse_density(invalid).is_none());
        }
        assert!(parse_density(&[b' '; MAX_BYTES + 1]).is_none());
    }

    #[test]
    fn legacy_theme_is_ignored_without_losing_density_or_writing_theme() {
        for theme in ["dark", "light", "unsupported"] {
            let bytes = format!(r#"{{"version":1,"theme":"{theme}","scale_percent":115}}"#);
            assert_eq!(parse_density(bytes.as_bytes()), Density::from_ui(1.15));
        }
        let stored = serde_json::to_value(Density::from_ui(1.15).unwrap()).unwrap();
        assert_eq!(stored, serde_json::json!({"version":1,"scale_percent":115}));
    }

    struct TestConfig(PathBuf);

    impl TestConfig {
        fn new() -> Self {
            let path =
                env::temp_dir().join(format!("spofify-preferences-{}", uuid::Uuid::new_v4()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestConfig {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn existing_appearance_without_playback_config_keeps_crossfade_off() {
        let config = TestConfig::new();
        let appearance = config.0.join("appearance.json");
        let playback = config.0.join("playback.json");
        let old_json = br#"{"version":1,"theme":"light","scale_percent":115}"#;
        fs::write(&appearance, old_json).unwrap();
        assert_eq!(load(&appearance, parse_density), Density::from_ui(1.15));
        assert!(!load_crossfade(Some(&playback)));
        assert!(!load_crossfade(None));
        assert_eq!(fs::read(&appearance).unwrap(), old_json.as_slice());
        assert!(!playback.exists());
    }

    #[test]
    fn playback_config_is_bounded_and_versioned() {
        assert!(parse_playback(br#"{"version":1,"crossfade":true}"#).is_some());
        assert!(parse_playback(br#"{"version":2,"crossfade":true}"#).is_none());
        assert!(parse_playback(br#"{"version":1,"crossfade":"true"}"#).is_none());
        assert!(parse_playback(&[b' '; MAX_BYTES + 1]).is_none());
    }

    #[test]
    fn crossfade_persists_independently_of_density() {
        let config = TestConfig::new();
        let appearance = config.0.join("appearance.json");
        let playback = config.0.join("playback.json");
        let custom = Density::from_ui(1.15).unwrap();
        save(&appearance, &custom).unwrap();
        let live = AtomicBool::new(load_crossfade(Some(&playback)));
        save_crossfade(Some(&playback), &live, true).unwrap();
        assert!(live.load(Ordering::Relaxed));
        assert!(load_crossfade(Some(&playback)));
        assert_eq!(load(&appearance, parse_density), Some(custom));
        save(&appearance, &Density::default()).unwrap();
        assert!(load_crossfade(Some(&playback)));
        save_crossfade(Some(&playback), &live, false).unwrap();
        assert!(!live.load(Ordering::Relaxed));
        assert!(!load(&playback, parse_playback).unwrap().crossfade);
        assert_eq!(load(&appearance, parse_density), Some(Density::default()));
    }

    #[test]
    fn failed_crossfade_save_keeps_live_state_and_cleans_up() {
        let config = TestConfig::new();
        let playback = config.0.join("playback.json");
        fs::create_dir(&playback).unwrap();
        let live = AtomicBool::new(true);
        assert!(save_crossfade(None, &live, false).is_err());
        assert!(save_crossfade(Some(&playback), &live, false).is_err());
        assert!(live.load(Ordering::Relaxed));
        assert!(playback.is_dir());
        assert_eq!(fs::read_dir(&config.0).unwrap().count(), 1);
    }

    #[test]
    fn local_volume_keeps_a_separate_level_while_muted() {
        let mut volume = LocalVolume::default();
        assert_eq!(volume.effective(), 1.0);
        volume.set_level(0.35);
        volume.muted = true;
        assert_eq!(volume.effective(), 0.0);
        assert_eq!(volume.level, 0.35);
        volume.set_level(f32::NAN);
        volume.set_level(f32::INFINITY);
        volume.muted = false;
        assert_eq!(volume.effective(), 0.35);
        volume.set_level(-1.0);
        assert_eq!(volume.effective(), 0.0);
        assert!(!volume.muted);
        volume.set_level(1.1);
        assert_eq!(volume.effective(), 1.0);
    }

    #[test]
    fn local_volume_config_is_bounded_versioned_and_validated() {
        assert_eq!(
            parse_volume(br#"{"version":1,"level":0.0,"muted":false}"#)
                .unwrap()
                .effective(),
            0.0
        );
        for invalid in [
            br#"{"version":2,"level":0.5,"muted":true}"#.as_slice(),
            br#"{"version":1,"level":-0.5,"muted":false}"#,
            br#"{"version":1,"level":1.5,"muted":false}"#,
            br#"{"version":1,"level":null,"muted":false}"#,
            br#"{"version":1,"level":0.5}"#,
            br#"{"version":1,"level":0.5,"muted":false,"account":"not-local"}"#,
        ] {
            assert!(parse_volume(invalid).is_none());
        }
        assert!(parse_volume(&[b' '; MAX_BYTES + 1]).is_none());
    }

    #[test]
    fn closing_local_volume_writer_flushes_the_latest_state_without_touching_crossfade() {
        let config = TestConfig::new();
        let volume = config.0.join("local-volume.json");
        let playback = config.0.join("playback.json");
        let unrelated = br#"{"version":1,"crossfade":true}"#;
        fs::write(&playback, unrelated).unwrap();
        let writer = VolumeWriter::start(Some(volume.clone()), LocalVolume::default());
        writer.pending.update(|v| v.set_level(0.25));
        writer.pending.update(|v| v.set_level(0.35));
        writer.pending.update(|v| v.muted = true);
        drop(writer);
        let mut saved = load(&volume, parse_volume).unwrap();
        assert_eq!(saved.level, 0.35);
        assert!(saved.muted);
        assert_eq!(saved.effective(), 0.0);
        saved.muted = false;
        assert_eq!(saved.effective(), 0.35);
        assert_eq!(fs::read(&playback).unwrap(), unrelated);
        assert_eq!(fs::read_dir(&config.0).unwrap().count(), 2);
    }
}
