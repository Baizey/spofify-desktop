//! Theme, scale and crossfade are device-local. The palette is account-scoped.

use super::{ApiError, INVALID_RESPONSE, decode};
use crate::{
    AppWindow, PaletteRow,
    presentation::{AccessRx, Account, TIMEOUT, Worker, read_with_backoff, scope_changed},
};
use reqwest::{Method, header};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use slint::{Color, ComponentHandle, ModelRc, VecModel};
use std::{
    cell::Cell,
    env,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::mpsc;

const VERSION: u8 = 1;
const MAX_BYTES: usize = 4096;
const MIN_SCALE: u8 = 85;
const MAX_SCALE: u8 = 125;
const CATALOG_VERSION: u8 = 1;
const MAX_REVISION: u64 = (1 << 53) - 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum Theme {
    Dark,
    Light,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Local {
    version: u8,
    theme: Theme,
    scale_percent: u8,
}

impl Default for Local {
    fn default() -> Self {
        Self {
            version: VERSION,
            theme: Theme::Dark,
            scale_percent: 100,
        }
    }
}

impl Local {
    fn from_ui(mode: i32, scale: f32) -> Option<Self> {
        let theme = match mode {
            0 => Theme::Dark,
            1 => Theme::Light,
            _ => return None,
        };
        if !(f32::from(MIN_SCALE) / 100.0..=f32::from(MAX_SCALE) / 100.0).contains(&scale) {
            return None;
        }
        Some(Self {
            version: VERSION,
            theme,
            scale_percent: (scale * 100.0).round() as u8,
        })
    }
    fn apply(self, ui: &AppWindow) {
        ui.set_appearance_mode(i32::from(self.theme == Theme::Light));
        ui.set_appearance_scale(f32::from(self.scale_percent) / 100.0);
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

fn parse_local(bytes: &[u8]) -> Option<Local> {
    let local: Local = parse_config(bytes)?;
    (local.version == VERSION && (MIN_SCALE..=MAX_SCALE).contains(&local.scale_percent))
        .then_some(local)
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum Id {
    Jade,
    Ember,
    Violet,
}
impl Id {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "jade" => Some(Self::Jade),
            "ember" => Some(Self::Ember),
            "violet" => Some(Self::Violet),
            _ => None,
        }
    }
    fn as_str(self) -> &'static str {
        match self {
            Self::Jade => "jade",
            Self::Ember => "ember",
            Self::Violet => "violet",
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Tokens {
    primary: String,
    on_primary: String,
    secondary: String,
    tertiary: String,
}
impl Tokens {
    fn colors(&self) -> Option<[Color; 4]> {
        Some([
            rgb(&self.primary)?,
            rgb(&self.on_primary)?,
            rgb(&self.secondary)?,
            rgb(&self.tertiary)?,
        ])
    }
}
fn rgb(value: &str) -> Option<Color> {
    let hex = value.strip_prefix('#')?;
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let packed = u32::from_str_radix(hex, 16).ok()?;
    Some(Color::from_rgb_u8(
        (packed >> 16) as u8,
        (packed >> 8) as u8,
        packed as u8,
    ))
}

#[derive(Clone, Debug, Deserialize)]
struct Preset {
    id: Id,
    name: String,
    dark: Tokens,
    light: Tokens,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Wire {
    account_id: String,
    palette_id: Id,
    revision: u64,
    catalog_version: u8,
    default_palette_id: Id,
    presets: [Preset; 3],
}

#[derive(Serialize, Clone, Copy)]
#[serde(rename_all = "camelCase")]
struct WritePalette {
    palette_id: Id,
    expected_revision: u64,
    catalog_version: u8,
}

#[derive(Clone)]
struct Palette {
    id: Id,
    revision: u64,
    presets: [Preset; 3],
}
impl Wire {
    fn validate(self, account: &str, write: Option<WritePalette>) -> Result<Palette, ApiError> {
        if self.account_id != account
            || self.catalog_version != CATALOG_VERSION
            || self.default_palette_id != Id::Jade
            || self.revision > MAX_REVISION
            || (self.revision == 0 && self.palette_id != self.default_palette_id)
            || write.is_some_and(|w| {
                self.palette_id != w.palette_id || self.revision != w.expected_revision + 1
            })
            || !self
                .presets
                .iter()
                .zip([
                    (Id::Jade, "Jade"),
                    (Id::Ember, "Ember"),
                    (Id::Violet, "Violet"),
                ])
                .all(|(p, (id, name))| {
                    p.id == id
                        && p.name == name
                        && p.dark.colors().is_some()
                        && p.light.colors().is_some()
                })
        {
            return Err(INVALID_RESPONSE);
        }
        Ok(Palette {
            id: self.palette_id,
            revision: self.revision,
            presets: self.presets,
        })
    }
}

async fn palette(account: &Account, write: Option<WritePalette>) -> Result<Palette, ApiError> {
    let api = &account.api;
    let session = &account.session;
    api.with_authenticated_network(session, async {
        let url = api.origin.endpoint("/api/me/palette");
        let method = if write.is_some() {
            Method::PUT
        } else {
            Method::GET
        };
        let mut req = api
            .request_with_method(method, url)?
            .header(header::COOKIE, session.secrets.cookie.clone());
        if let Some(body) = write {
            req = req
                .header("X-CSRF-Token", session.secrets.csrf.clone())
                .json(&body);
        }
        let response = api.send(req, Some(session), false).await?;
        if response.status() != 200 {
            return Err(INVALID_RESPONSE);
        }
        let wire: Wire = decode(response, 8192).await?;
        api.check_session(session)?;
        wire.validate(account.id(), write)
    })
    .await
}

#[derive(Default, Clone)]
struct View {
    account: Option<Account>,
    palette: Option<Palette>,
    enabled: bool,
}
type SharedView = Arc<Mutex<View>>;

fn apply_palette(ui: &AppWindow, palette: Option<&Palette>) {
    let rows = palette.map_or_else(Vec::new, |p| {
        p.presets
            .iter()
            .filter_map(|preset| {
                let tokens = if ui.get_appearance_mode() == 1 {
                    &preset.light
                } else {
                    &preset.dark
                };
                let [primary, on_primary, secondary, tertiary] = tokens.colors()?;
                Some(PaletteRow {
                    id: preset.id.as_str().into(),
                    title: preset.name.as_str().into(),
                    primary,
                    on_primary,
                    secondary,
                    tertiary,
                })
            })
            .collect()
    });
    ui.set_palettes(ModelRc::new(VecModel::from(rows)));
    ui.set_palette_id(palette.map_or("", |p| p.id.as_str()).into());
}

struct Selection {
    account: Account,
    write: WritePalette,
}

/// Install local settings synchronously before showing the window. The remote
/// palette is retained across navigation and is never persisted on this device.
pub(crate) fn install(ui: &AppWindow, access: AccessRx) -> Worker {
    let path = config_path("appearance.json");
    let initial = path
        .as_deref()
        .and_then(|p| load(p, parse_local))
        .unwrap_or_default();
    initial.apply(ui);
    apply_palette(ui, None);
    ui.set_palette_enabled(false);
    let local = Rc::new(Cell::new(initial));
    let view: SharedView = Default::default();
    let apply = {
        let (weak, view, access) = (ui.as_weak(), view.clone(), access.clone());
        Rc::new(move |next, reset| {
            let Some(ui) = weak.upgrade() else { return };
            let next: Local = next;
            next.apply(&ui);
            let held = view.lock().unwrap().clone();
            let p = held
                .account
                .as_ref()
                .filter(|a| Account::current(&access).is_some_and(|b| a.same(&b)))
                .and(held.palette.as_ref());
            apply_palette(&ui, p);
            if (reset || local.get() != next)
                && path.as_deref().is_some_and(|p| save(p, &next).is_ok())
            {
                local.set(next);
            }
        })
    };
    let changed = apply.clone();
    ui.on_appearance_changed(move |mode, scale, _legacy_accent| {
        if let Some(next) = Local::from_ui(mode, scale) {
            changed(next, false);
        }
    });
    ui.on_reset_appearance(move || apply(Local::default(), true));
    let (tx, rx) = mpsc::channel(1);
    {
        let (weak, view, access) = (ui.as_weak(), view.clone(), access.clone());
        ui.on_palette_selected(move |id| {
            let (Some(ui), Some(id)) = (weak.upgrade(), Id::parse(id.as_str())) else {
                return;
            };
            let mut view = view.lock().unwrap();
            let (Some(a), Some(p)) = (view.account.as_ref(), view.palette.as_ref()) else {
                return;
            };
            if !ui.get_signed_in()
                || !view.enabled
                || a.offline
                || !a.matches(&access)
                || p.id == id
                || p.revision >= MAX_REVISION
            {
                return;
            }
            let write = WritePalette {
                palette_id: id,
                expected_revision: p.revision,
                catalog_version: CATALOG_VERSION,
            };
            if tx
                .try_send(Selection {
                    account: a.clone(),
                    write,
                })
                .is_ok()
            {
                view.enabled = false;
                drop(view);
                ui.set_palette_enabled(false);
                ui.set_palette_status("Saving palette…".into());
            }
        });
    }
    let weak = ui.as_weak();
    Worker::start("desktop-preferences", move || run(weak, access, view, rx))
}

fn publish(
    ui: &slint::Weak<AppWindow>,
    access: &AccessRx,
    view: &SharedView,
    account: Option<&Account>,
    palette: Option<&Palette>,
    enabled: bool,
    status: &'static str,
) {
    let access = access.clone();
    let view = view.clone();
    let next = View {
        account: account.cloned(),
        palette: palette.cloned(),
        enabled,
    };
    let _ = ui.upgrade_in_event_loop(move |ui| {
        let valid = match &next.account {
            Some(a) => a.matches(&access),
            None => Account::current(&access).is_none(),
        };
        if valid {
            apply_palette(&ui, next.palette.as_ref());
            ui.set_palette_enabled(next.enabled);
            ui.set_palette_status(status.into());
            *view.lock().unwrap() = next;
        }
    });
}

fn adopt(held: &mut Option<Palette>, next: Palette) -> bool {
    // Do not roll back a usable replica on a stale read or conflicting state.
    if held.as_ref().is_some_and(|old| {
        old.revision > next.revision || old.revision == next.revision && old.id != next.id
    }) {
        return false;
    }
    *held = Some(next);
    true
}

async fn run(
    ui: slint::Weak<AppWindow>,
    mut access: AccessRx,
    view: SharedView,
    mut commands: mpsc::Receiver<Selection>,
) {
    let mut owner: Option<Account> = None;
    let mut confirmed: Option<Palette> = None;
    'account: loop {
        let account = Account::current(&access);
        if !owner
            .as_ref()
            .zip(account.as_ref())
            .is_some_and(|(a, b)| a.same(b))
        {
            confirmed = None;
        }
        owner = account.clone();
        let online = account.as_ref().filter(|a| !a.offline);
        let Some(a) = online else {
            publish(
                &ui,
                &access,
                &view,
                account.as_ref(),
                confirmed.as_ref(),
                false,
                if account.is_some() {
                    "Palette changes are unavailable offline."
                } else {
                    ""
                },
            );
            if !scope_changed(&mut access, account.as_ref()).await {
                break;
            }
            continue;
        };
        publish(&ui, &access, &view, Some(a), confirmed.as_ref(), false, "");
        let mut read = true;
        let mut ready = false;
        loop {
            if read {
                let result = tokio::select! {
                    biased;
                    change = scope_changed(&mut access, Some(a)) => { if change { continue 'account; } break 'account; }
                    r = read_with_backoff(async || palette(a, None).await) => r,
                };
                ready = match result {
                    Ok(p) => adopt(&mut confirmed, p),
                    Err(_) => false,
                };
            }
            publish(
                &ui,
                &access,
                &view,
                Some(a),
                confirmed.as_ref(),
                ready
                    && confirmed
                        .as_ref()
                        .is_some_and(|p| p.revision < MAX_REVISION),
                if ready {
                    ""
                } else {
                    "Account palettes are unavailable right now."
                },
            );
            let command = tokio::select! {
                biased;
                change = scope_changed(&mut access, Some(a)) => { if change { continue 'account; } break 'account; }
                c = commands.recv() => c,
            };
            let Some(command) = command else {
                break 'account;
            };
            read = false;
            if !ready
                || !command.account.matches(&access)
                || !confirmed.as_ref().is_some_and(|p| {
                    p.revision == command.write.expected_revision
                        && p.id != command.write.palette_id
                })
            {
                continue;
            }
            let result = tokio::select! {
                biased;
                change = scope_changed(&mut access, Some(a)) => { if change { continue 'account; } break 'account; }
                r = tokio::time::timeout(Duration::from_secs(5), palette(a, Some(command.write))) => r.unwrap_or(Err(TIMEOUT)),
            };
            // A lost reply could have committed. Never retry the PUT; reconcile
            // by reading, retaining the previous usable palette on a read gap.
            read = match result {
                Ok(p) => !adopt(&mut confirmed, p),
                Err(_) => true,
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_controls_accept_only_known_modes_and_finite_scale() {
        assert_eq!(Local::from_ui(0, 1.0), Some(Local::default()));
        assert_eq!(Local::from_ui(1, 1.103).unwrap().scale_percent, 110);
        assert_eq!(Local::from_ui(1, 0.85).unwrap().scale_percent, 85);
        assert_eq!(Local::from_ui(1, 1.25).unwrap().scale_percent, 125);
        assert!(Local::from_ui(2, 1.0).is_none());
        for scale in [f32::NAN, f32::INFINITY, 0.84, 1.26] {
            assert!(Local::from_ui(0, scale).is_none());
        }
    }

    #[test]
    fn local_config_rejects_unknown_version_or_oversized_input() {
        let valid = br#"{"version":1,"theme":"light","scale_percent":115}"#;
        assert_eq!(parse_local(valid), Local::from_ui(1, 1.15));
        assert!(parse_local(br#"{"version":2,"theme":"dark","scale_percent":100}"#).is_none());
        assert!(parse_local(&[b' '; MAX_BYTES + 1]).is_none());
        assert!(rgb("#08271d").is_some());
        for value in ["transparent", "#fff", "000000", "#zz0000", "#000000ff"] {
            assert!(rgb(value).is_none());
        }
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
        assert_eq!(load(&appearance, parse_local), Local::from_ui(1, 1.15));
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
    fn crossfade_persists_independently_of_display_defaults() {
        let config = TestConfig::new();
        let appearance = config.0.join("appearance.json");
        let playback = config.0.join("playback.json");
        let custom = Local::from_ui(1, 1.15).unwrap();
        save(&appearance, &custom).unwrap();
        let live = AtomicBool::new(load_crossfade(Some(&playback)));
        save_crossfade(Some(&playback), &live, true).unwrap();
        assert!(live.load(Ordering::Relaxed));
        assert!(load_crossfade(Some(&playback)));
        assert_eq!(load(&appearance, parse_local), Some(custom));
        save(&appearance, &Local::default()).unwrap();
        assert!(load_crossfade(Some(&playback)));
        save_crossfade(Some(&playback), &live, false).unwrap();
        assert!(!live.load(Ordering::Relaxed));
        assert!(!load(&playback, parse_playback).unwrap().crossfade);
        assert_eq!(load(&appearance, parse_local), Some(Local::default()));
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

    fn wire(id: Id, revision: u64) -> Wire {
        let tokens = Tokens {
            primary: "#66e3b4".into(),
            on_primary: "#08271d".into(),
            secondary: "#91bfff".into(),
            tertiary: "#dfbd78".into(),
        };
        Wire {
            account_id: "synthetic-account".into(),
            palette_id: id,
            revision,
            catalog_version: CATALOG_VERSION,
            default_palette_id: Id::Jade,
            presets: [
                (Id::Jade, "Jade"),
                (Id::Ember, "Ember"),
                (Id::Violet, "Violet"),
            ]
            .map(|(id, name)| Preset {
                id,
                name: name.into(),
                dark: tokens.clone(),
                light: tokens.clone(),
            }),
        }
    }

    #[test]
    fn palette_checks_principal_ack_and_monotonic_revision() {
        let account = "synthetic-account";
        let change = WritePalette {
            palette_id: Id::Ember,
            expected_revision: 0,
            catalog_version: CATALOG_VERSION,
        };
        assert!(
            wire(Id::Ember, 1)
                .validate("other-account", Some(change))
                .is_err()
        );
        assert!(wire(Id::Violet, 1).validate(account, Some(change)).is_err());
        assert!(wire(Id::Ember, 2).validate(account, Some(change)).is_err());
        let mut held = Some(wire(Id::Ember, 1).validate(account, Some(change)).unwrap());
        assert!(!adopt(
            &mut held,
            wire(Id::Jade, 0).validate(account, None).unwrap()
        ));
        assert!(!adopt(
            &mut held,
            wire(Id::Violet, 1).validate(account, None).unwrap()
        ));
        assert_eq!(held.unwrap().id, Id::Ember);
        assert_eq!(
            serde_json::to_value(change).unwrap(),
            serde_json::json!({
                "paletteId": "ember", "expectedRevision": 0, "catalogVersion": 1,
            })
        );
    }
}
