//! Linux media keys. This is an adapter to the existing player, not an output or a second session.
use crate::AppWindow;
use slint::{ComponentHandle, Timer, TimerMode};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, SyncSender},
    },
    time::{Duration, Instant},
};
use zbus::{
    blocking::connection::Builder,
    fdo, interface,
    zvariant::{ObjectPath, OwnedValue, Str},
};

const PATH: &str = "/org/mpris/MediaPlayer2";
const PLAYER: &str = "org.mpris.MediaPlayer2.Player";
type Values = HashMap<&'static str, OwnedValue>;

#[derive(Clone, Default, PartialEq)]
struct View {
    epoch: u64,
    track: String,
    title: String,
    duration_us: i64,
    playing: bool,
    control: bool,
    skip: bool,
}
impl View {
    fn status(&self) -> &'static str {
        if self.track.is_empty() {
            "Stopped"
        } else if self.playing {
            "Playing"
        } else {
            "Paused"
        }
    }
    fn metadata(&self) -> Values {
        if self.track.is_empty() {
            return Values::new();
        }
        // Never publish server URLs, account identifiers, private paths or artwork tokens.
        // SetPosition is not advertised; provide an opaque object path for this catalog item.
        use std::hash::{DefaultHasher, Hash, Hasher};
        let mut key = DefaultHasher::new();
        (self.epoch, &self.track).hash(&mut key);
        let path = ObjectPath::try_from(format!(
            "/org/mpris/MediaPlayer2/track/t{:016x}",
            key.finish()
        ))
        .unwrap();
        Values::from([
            ("mpris:trackid", path.into()),
            (
                "xesam:title",
                Str::from(self.title.replace('\0', "")).into(),
            ),
            ("mpris:length", self.duration_us.into()),
        ])
    }
}

#[derive(Clone, Copy, Debug)]
enum Action {
    Toggle,
    Pause,
    Play,
    Next,
    Previous,
}
struct Tap {
    epoch: u64,
    when: Instant,
    action: Action,
}
#[derive(Clone)]
struct Player {
    view: Arc<Mutex<(View, i64)>>,
    epoch: Arc<AtomicU64>,
    actions: SyncSender<Tap>,
}
impl Player {
    fn current(&self) -> (View, i64) {
        let view = self.view.lock().unwrap();
        if self.epoch.load(Ordering::SeqCst) == view.0.epoch {
            view.clone()
        } else {
            Default::default()
        }
    }
    fn send(&self, action: Action) -> fdo::Result<()> {
        let v = self.current().0;
        let permitted = match action {
            Action::Next | Action::Previous => v.skip,
            _ => v.control,
        };
        if permitted {
            self.actions
                .try_send(Tap {
                    epoch: v.epoch,
                    when: Instant::now(),
                    action,
                })
                .map_err(|_| fdo::Error::LimitsExceeded("Media controls busy".into()))?;
        }
        Ok(())
    }
}
fn unsupported() -> fdo::Result<()> {
    Err(fdo::Error::NotSupported("Not available".into()))
}

#[interface(name = "org.mpris.MediaPlayer2.Player")]
impl Player {
    fn play_pause(&self) -> fdo::Result<()> {
        self.send(Action::Toggle)
    }
    fn pause(&self) -> fdo::Result<()> {
        self.send(Action::Pause)
    }
    fn play(&self) -> fdo::Result<()> {
        self.send(Action::Play)
    }
    fn next(&self) -> fdo::Result<()> {
        self.send(Action::Next)
    }
    fn previous(&self) -> fdo::Result<()> {
        self.send(Action::Previous)
    }
    fn stop(&self) -> fdo::Result<()> {
        unsupported()
    }
    fn open_uri(&self, _uri: &str) -> fdo::Result<()> {
        unsupported()
    }
    fn seek(&self, _offset: i64) -> fdo::Result<()> {
        unsupported()
    }
    fn set_position(&self, _track_id: ObjectPath<'_>, _position: i64) -> fdo::Result<()> {
        unsupported()
    }
    #[zbus(property)]
    fn playback_status(&self) -> String {
        self.current().0.status().into()
    }
    #[zbus(property)]
    fn metadata(&self) -> Values {
        self.current().0.metadata()
    }
    #[zbus(property(emits_changed_signal = "false"))]
    fn position(&self) -> i64 {
        self.current().1
    }
    #[zbus(property)]
    fn can_play(&self) -> bool {
        self.current().0.control
    }
    #[zbus(property)]
    fn can_pause(&self) -> bool {
        self.current().0.control
    }
    #[zbus(property)]
    fn can_go_next(&self) -> bool {
        self.current().0.skip
    }
    #[zbus(property)]
    fn can_go_previous(&self) -> bool {
        self.current().0.skip
    }
    #[zbus(property)]
    fn can_seek(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn can_control(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn rate(&self) -> f64 {
        1.
    }
    #[zbus(property)]
    fn set_rate(&self, value: f64) -> fdo::Result<()> {
        if value == 1. { Ok(()) } else { unsupported() }
    }
    #[zbus(property)]
    fn minimum_rate(&self) -> f64 {
        1.
    }
    #[zbus(property)]
    fn maximum_rate(&self) -> f64 {
        1.
    }
    #[zbus(property)]
    fn volume(&self) -> f64 {
        1.
    }
    #[zbus(property)]
    fn set_volume(&self, _value: f64) -> fdo::Result<()> {
        unsupported()
    }
}
struct Application;
#[interface(name = "org.mpris.MediaPlayer2")]
impl Application {
    fn raise(&self) -> fdo::Result<()> {
        unsupported()
    }
    fn quit(&self) -> fdo::Result<()> {
        unsupported()
    }
    #[zbus(property)]
    fn identity(&self) -> &str {
        "Spofify"
    }
    #[zbus(property)]
    fn desktop_entry(&self) -> &str {
        "spofify-desktop"
    }
    #[zbus(property)]
    fn can_quit(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn can_raise(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn has_track_list(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn supported_uri_schemes(&self) -> Vec<String> {
        vec![]
    }
    #[zbus(property)]
    fn supported_mime_types(&self) -> Vec<String> {
        vec![]
    }
}

/// Bus traffic never blocks the UI. A missing session bus leaves native controls unaffected.
pub fn attach(ui: &AppWindow, epoch: Arc<AtomicU64>) -> Timer {
    let (actions, taps) = mpsc::sync_channel(12);
    let view = Arc::new(Mutex::new((View::default(), 0)));
    let player = Player {
        view: view.clone(),
        epoch: epoch.clone(),
        actions,
    };
    let (notify, changes) = mpsc::sync_channel(1);
    let _ = std::thread::Builder::new()
        .name("desktop-media-keys".into())
        .spawn(move || -> zbus::Result<()> {
            let name = format!(
                "org.mpris.MediaPlayer2.spofify.instance{}",
                std::process::id()
            );
            let bus = Builder::session()?
                .serve_at(PATH, Application)?
                .serve_at(PATH, player.clone())?
                .name(name)?
                .build()?;
            while changes.recv().is_ok() {
                let v = player.current().0;
                let properties = Values::from([
                    ("PlaybackStatus", Str::from(v.status()).into()),
                    ("Metadata", v.metadata().into()),
                    ("CanPlay", v.control.into()),
                    ("CanPause", v.control.into()),
                    ("CanGoNext", v.skip.into()),
                    ("CanGoPrevious", v.skip.into()),
                ]);
                bus.emit_signal(
                    None::<&str>,
                    PATH,
                    "org.freedesktop.DBus.Properties",
                    "PropertiesChanged",
                    &(PLAYER, properties, Vec::<String>::new()),
                )?;
            }
            Ok(())
        });
    let weak = ui.as_weak();
    let timer = Timer::default();
    timer.start(TimerMode::Repeated, Duration::from_millis(125), move || {
        let Some(ui) = weak.upgrade() else { return };
        let owner = epoch.load(Ordering::SeqCst);
        let (next, pos) = if ui.get_signed_in() {
            (
                View {
                    epoch: owner,
                    track: ui.get_now_track_id().into(),
                    title: ui.get_now_title().into(),
                    duration_us: micros(ui.get_duration()),
                    playing: ui.get_playing(),
                    control: ui.get_can_control(),
                    skip: ui.get_remote_controls(),
                },
                micros(ui.get_elapsed()),
            )
        } else {
            (
                View {
                    epoch: owner,
                    ..View::default()
                },
                0,
            )
        };
        {
            let mut old = view.lock().unwrap();
            let changed = old.0 != next;
            *old = (next, pos);
            if changed {
                let _ = notify.try_send(());
            }
        }
        for tap in taps.try_iter().take(12) {
            if tap.epoch != owner
                || !ui.get_signed_in()
                || tap.when.elapsed() >= Duration::from_secs(1)
            {
                continue;
            }
            match tap.action {
                Action::Next if ui.get_remote_controls() => ui.invoke_next(),
                Action::Previous if ui.get_remote_controls() => ui.invoke_previous(),
                Action::Toggle if ui.get_can_control() => ui.invoke_toggle_play(),
                Action::Pause if ui.get_can_control() && ui.get_playing() => {
                    ui.invoke_toggle_play()
                }
                Action::Play if ui.get_can_control() && !ui.get_playing() => {
                    ui.invoke_toggle_play()
                }
                _ => (),
            }
        }
    });
    timer
}
fn micros(seconds: f32) -> i64 {
    (f64::from(seconds.max(0.)) * 1_000_000.) as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "run inside dbus-run-session to verify a real session-bus round trip"]
    fn desktop_controller_round_trip() -> zbus::Result<()> {
        let (actions, taps) = mpsc::sync_channel(2);
        let player = Player {
            actions,
            epoch: Arc::new(AtomicU64::new(7)),
            view: Arc::new(Mutex::new((
                View {
                    epoch: 7,
                    control: true,
                    skip: true,
                    track: "library-item".into(),
                    title: "Fixture".into(),
                    ..View::default()
                },
                0,
            ))),
        };
        let name = format!("org.mpris.MediaPlayer2.spofify.test{}", std::process::id());
        let _service = Builder::session()?
            .serve_at(PATH, Application)?
            .serve_at(PATH, player)?
            .name(name.as_str())?
            .build()?;
        let client = zbus::blocking::Connection::session()?;
        let proxy = zbus::blocking::Proxy::new(&client, name.as_str(), PATH, PLAYER)?;
        assert_eq!(proxy.get_property::<String>("PlaybackStatus")?, "Paused");
        assert_eq!(
            proxy
                .get_property::<HashMap<String, OwnedValue>>("Metadata")?
                .len(),
            3
        );
        proxy.call::<_, _, ()>("Next", &())?;
        let tap = taps.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(matches!(tap.action, Action::Next));
        assert_eq!(tap.epoch, 7);
        Ok(())
    }
    #[test]
    fn retirement_removes_metadata_and_controls_even_before_next_ui_tick() {
        let (actions, rx) = mpsc::sync_channel(1);
        let p = Player {
            actions,
            epoch: Arc::new(AtomicU64::new(2)),
            view: Arc::new(Mutex::new((
                View {
                    epoch: 2,
                    track: "opaque/id".into(),
                    title: "Song".into(),
                    control: true,
                    ..Default::default()
                },
                12,
            ))),
        };
        assert_eq!(p.current().0.metadata().len(), 3);
        p.send(Action::Pause).unwrap();
        assert_eq!(rx.try_recv().unwrap().epoch, 2);
        p.epoch.fetch_add(1, Ordering::SeqCst);
        assert!(p.current().0.metadata().is_empty());
        assert_eq!(p.current().1, 0);
        p.send(Action::Play).unwrap();
        assert!(rx.try_recv().is_err());
    }
}
