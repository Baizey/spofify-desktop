//! Linux media keys. This is an adapter to the existing player, not an output or a second session.
use crate::{AppWindow, engine::Command, player};
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

#[derive(Clone, Default)]
struct View {
    epoch: u64,
    track_token: String,
    room_token: String,
    title: String,
    duration_us: i64,
    playing: bool,
    control: bool,
    skip: bool,
    seek: bool,
}
impl View {
    fn status(&self) -> &'static str {
        if self.track_token.is_empty() {
            "Stopped"
        } else if self.playing {
            "Playing"
        } else {
            "Paused"
        }
    }
    fn track_path(&self) -> ObjectPath<'static> {
        // Use a stable queue occurrence rather than a catalog ID or a room
        // revision. A separate guard binds queued taps to the displayed run.
        use std::hash::{DefaultHasher, Hash, Hasher};
        let mut key = DefaultHasher::new();
        (self.epoch, &self.track_token).hash(&mut key);
        ObjectPath::try_from(format!("{PATH}/track/t{:016x}", key.finish())).unwrap()
    }
    fn metadata(&self) -> Values {
        if self.track_token.is_empty() {
            return Values::new();
        }
        // Never publish server URLs, account identifiers, private paths or artwork tokens.
        let path = self.track_path();
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
    Stop,
    Seek(i64),
    SetPosition(i64),
}
struct Tap {
    epoch: u64,
    when: Instant,
    action: Action,
    room_token: String,
}
impl Tap {
    fn guarded_action(&self, view: &View, position: i64) -> Option<player::Action> {
        if self.epoch != view.epoch
            || self.room_token.is_empty()
            || self.room_token != view.room_token
        {
            return None;
        }
        let micros = match self.action {
            // The engine's Pause stops output without discarding the shared
            // queue. Its Close command is destructive, not a transport stop.
            Action::Stop | Action::Pause if view.control => return Some(player::Action::Pause),
            Action::Play if view.control => return Some(player::Action::Resume),
            Action::Next if view.skip => return Some(player::Action::Next),
            Action::Previous if view.skip => return Some(player::Action::Previous),
            Action::Seek(offset) if view.seek => {
                let target = position.saturating_add(offset);
                if target > view.duration_us {
                    return Some(player::Action::Next);
                }
                target.max(0)
            }
            Action::SetPosition(target)
                if view.seek && (0..=view.duration_us).contains(&target) =>
            {
                target
            }
            _ => return None,
        };
        Some(player::Action::Seek(micros as f64 / 1_000_000.))
    }
}
#[derive(Default)]
struct Snapshot {
    view: View,
    position: i64,
    seeked: Option<i64>,
    properties_dirty: bool,
}
impl Snapshot {
    fn update(&mut self, next: View, position: i64, elapsed: Duration) -> bool {
        let previous = &self.view;
        let same_track = !next.track_token.is_empty()
            && next.track_token == previous.track_token
            && next.epoch == previous.epoch;
        if !same_track || previous.room_token != next.room_token {
            self.seeked = None;
        }
        // Position has no PropertiesChanged notification. Report observed
        // playback jumps, not a speculative result at command enqueue time.
        let travel = if previous.playing {
            elapsed.as_micros() as i64
        } else {
            0
        };
        let jump = same_track
            && next.seek
            && previous.seek
            && next.room_token != previous.room_token
            && position.abs_diff(self.position.saturating_add(travel)) > 250_000;
        if jump {
            self.seeked = Some(position);
        }
        // A new transport revision alone is not a metadata/property change.
        let changed = previous.epoch != next.epoch
            || previous.track_token != next.track_token
            || previous.title != next.title
            || previous.duration_us != next.duration_us
            || previous.playing != next.playing
            || previous.control != next.control
            || previous.skip != next.skip
            || previous.seek != next.seek;
        self.view = next;
        self.position = position;
        self.properties_dirty |= changed;
        changed || jump
    }
}
#[derive(Clone)]
struct Player {
    view: Arc<Mutex<Snapshot>>,
    epoch: Arc<AtomicU64>,
    actions: SyncSender<Tap>,
}
impl Player {
    fn current(&self) -> (View, i64) {
        let view = self.view.lock().unwrap();
        if self.epoch.load(Ordering::SeqCst) == view.view.epoch {
            (view.view.clone(), view.position)
        } else {
            Default::default()
        }
    }
    fn send(&self, action: Action) -> fdo::Result<()> {
        let v = self.current().0;
        let permitted = match action {
            Action::Next | Action::Previous => v.skip,
            Action::Seek(_) => v.seek,
            Action::Stop => v.control && !v.room_token.is_empty(),
            _ => v.control,
        };
        if permitted {
            self.enqueue(v, action)
        } else {
            Ok(())
        }
    }
    fn publish_change(&self, bus: &zbus::blocking::Connection) -> zbus::Result<()> {
        let (v, seeked, changed) = {
            let mut snapshot = self.view.lock().unwrap();
            let changed = std::mem::take(&mut snapshot.properties_dirty);
            if self.epoch.load(Ordering::SeqCst) == snapshot.view.epoch {
                (snapshot.view.clone(), snapshot.seeked.take(), changed)
            } else {
                (View::default(), None, true)
            }
        };
        if changed {
            let properties = Values::from([
                ("PlaybackStatus", Str::from(v.status()).into()),
                ("Metadata", v.metadata().into()),
                ("CanPlay", v.control.into()),
                ("CanPause", v.control.into()),
                ("CanGoNext", v.skip.into()),
                ("CanGoPrevious", v.skip.into()),
                ("CanSeek", v.seek.into()),
            ]);
            bus.emit_signal(
                None::<&str>,
                PATH,
                "org.freedesktop.DBus.Properties",
                "PropertiesChanged",
                &(PLAYER, properties, Vec::<String>::new()),
            )?;
        }
        if let Some(position) = seeked
            && self.epoch.load(Ordering::SeqCst) == v.epoch
        {
            bus.emit_signal(None::<&str>, PATH, PLAYER, "Seeked", &(position,))?;
        }
        Ok(())
    }
    fn enqueue(&self, view: View, action: Action) -> fdo::Result<()> {
        self.actions
            .try_send(Tap {
                epoch: view.epoch,
                when: Instant::now(),
                action,
                room_token: view.room_token,
            })
            .map_err(|_| fdo::Error::LimitsExceeded("Media controls busy".into()))
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
        self.send(Action::Stop)
    }
    fn open_uri(&self, _uri: &str) -> fdo::Result<()> {
        unsupported()
    }
    fn seek(&self, offset: i64) -> fdo::Result<()> {
        if offset == 0 {
            Ok(())
        } else {
            self.send(Action::Seek(offset))
        }
    }
    fn set_position(&self, track_id: ObjectPath<'_>, position: i64) -> fdo::Result<()> {
        let v = self.current().0;
        if v.seek && (0..=v.duration_us).contains(&position) && track_id == v.track_path() {
            self.enqueue(v, Action::SetPosition(position))
        } else {
            Ok(())
        }
    }
    #[zbus(signal)]
    async fn seeked(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        position: i64,
    ) -> zbus::Result<()>;
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
        self.current().0.seek
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
pub fn attach(
    ui: &AppWindow,
    epoch: Arc<AtomicU64>,
    dispatch: impl Fn(&AppWindow, Command) + 'static,
) -> Timer {
    let (actions, taps) = mpsc::sync_channel(12);
    let view = Arc::new(Mutex::new(Snapshot::default()));
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
                player.publish_change(&bus)?;
            }
            Ok(())
        });
    let weak = ui.as_weak();
    let timer = Timer::default();
    let last_sample = std::cell::Cell::new(Instant::now());
    timer.start(TimerMode::Repeated, Duration::from_millis(125), move || {
        let Some(ui) = weak.upgrade() else { return };
        let owner = epoch.load(Ordering::SeqCst);
        let (next, pos) = if ui.get_signed_in() {
            let track_token = ui.get_media_track_token();
            let room_token = ui.get_room_token();
            let duration_us = micros(ui.get_duration());
            let available = !room_token.is_empty()
                && (!(ui.get_manual_offline() || ui.get_reconnecting()) || ui.get_local_playback());
            (
                View {
                    epoch: owner,
                    seek: available
                        && ui.get_can_seek()
                        && !track_token.is_empty()
                        && duration_us > 0,
                    track_token: track_token.into(),
                    room_token: room_token.into(),
                    title: ui.get_now_title().into(),
                    duration_us,
                    playing: ui.get_playing(),
                    control: available && ui.get_can_control(),
                    skip: available && ui.get_remote_controls(),
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
            let now = Instant::now();
            if old.update(next.clone(), pos, now - last_sample.replace(now)) {
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
                Action::Toggle if next.control && tap.room_token == next.room_token => {
                    dispatch(
                        &ui,
                        Command::GuardedToggle(tap.room_token).scoped(tap.epoch),
                    );
                }
                _ => {
                    if let Some(action) = tap.guarded_action(&next, pos) {
                        dispatch(
                            &ui,
                            Command::Guarded(tap.room_token, action).scoped(tap.epoch),
                        );
                    }
                }
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
            view: Arc::new(Mutex::new(Snapshot {
                view: fixture(),
                position: 3_000_000,
                ..Default::default()
            })),
        };
        let name = format!("org.mpris.MediaPlayer2.spofify.test{}", std::process::id());
        let service = Builder::session()?
            .serve_at(PATH, Application)?
            .serve_at(PATH, player.clone())?
            .name(name.as_str())?
            .build()?;
        let client = zbus::blocking::Connection::session()?;
        let proxy = zbus::blocking::Proxy::new(&client, name.as_str(), PATH, PLAYER)?;
        assert_eq!(proxy.get_property::<String>("PlaybackStatus")?, "Paused");
        assert!(proxy.get_property::<bool>("CanSeek")?);
        assert_eq!(proxy.get_property::<i64>("Position")?, 3_000_000);
        let mut metadata = proxy.get_property::<HashMap<String, OwnedValue>>("Metadata")?;
        assert_eq!(metadata.len(), 3);
        let path = ObjectPath::try_from(metadata.remove("mpris:trackid").unwrap()).unwrap();
        assert_eq!(path, fixture().track_path());
        for (method, action) in [
            ("Next", Action::Next),
            ("Previous", Action::Previous),
            ("Pause", Action::Pause),
            ("Play", Action::Play),
            ("PlayPause", Action::Toggle),
            ("Stop", Action::Stop),
        ] {
            proxy.call::<_, _, ()>(method, &())?;
            let tap = taps.recv_timeout(Duration::from_secs(2)).unwrap();
            assert_eq!(
                std::mem::discriminant(&tap.action),
                std::mem::discriminant(&action)
            );
            assert_eq!(tap.epoch, 7);
        }
        proxy.call::<_, _, ()>("Seek", &(-2_000_000i64,))?;
        let tap = taps.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(matches!(tap.action, Action::Seek(-2_000_000)));
        proxy.call::<_, _, ()>("SetPosition", &(&path, 8_000_000i64))?;
        let tap = taps.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(matches!(tap.action, Action::SetPosition(8_000_000)));
        for invalid in [-1i64, 10_000_001] {
            proxy.call::<_, _, ()>("SetPosition", &(&path, invalid))?;
        }
        let mut seeked = proxy.receive_signal("Seeked")?;
        let properties = zbus::blocking::Proxy::new(
            &client,
            name.as_str(),
            PATH,
            "org.freedesktop.DBus.Properties",
        )?;
        let mut property_changes = properties.receive_signal("PropertiesChanged")?;
        assert!(player.view.lock().unwrap().update(
            View {
                room_token: "5:4".into(),
                ..fixture()
            },
            8_000_000,
            Duration::from_millis(125),
        ));
        player.publish_change(&service)?;
        let pos = seeked.next().unwrap().body().deserialize::<i64>()?;
        assert_eq!(pos, 8_000_000);
        assert!(taps.try_recv().is_err());
        proxy.call::<_, _, ()>("SetPosition", &(&path, 8_000_000i64))?;
        let tap = taps.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(tap.room_token, "5:4");
        let step = Duration::from_millis(125);
        let mut next = View {
            track_token: "online:another-key:chain:visit".into(),
            ..fixture()
        };
        assert!(player.view.lock().unwrap().update(next.clone(), 0, step));
        player.publish_change(&service)?;
        proxy.call::<_, _, ()>("SetPosition", &(&path, 8_000_000i64))?;
        next.seek = false;
        assert!(player.view.lock().unwrap().update(next, 0, step));
        player.publish_change(&service)?;
        assert!(taps.try_recv().is_err());
        for expected in [true, false] {
            let (interface, values, _): (String, HashMap<String, OwnedValue>, Vec<String>) =
                property_changes.next().unwrap().body().deserialize()?;
            assert_eq!(interface, PLAYER);
            assert_eq!(bool::try_from(&values["CanSeek"]).unwrap(), expected);
            assert!(!values.contains_key("Position"));
        }
        proxy.call::<_, _, ()>("Seek", &(1_000_000i64,))?;
        assert!(taps.try_recv().is_err());
        Ok(())
    }
    fn fixture() -> View {
        View {
            epoch: 7,
            control: true,
            skip: true,
            seek: true,
            track_token: "online:queue-key:chain:visit".into(),
            room_token: "5:3".into(),
            title: "Fixture".into(),
            duration_us: 10_000_000,
            ..Default::default()
        }
    }

    #[test]
    fn seeks_are_bounded_and_retire_on_run_change() {
        let v = fixture();
        let mut tap = Tap {
            epoch: v.epoch,
            when: Instant::now(),
            action: Action::Seek(-2_000_000),
            room_token: v.room_token.clone(),
        };
        let pos = 3_000_000;
        assert!(matches!(
            tap.guarded_action(&v, pos),
            Some(player::Action::Seek(1.0))
        ));
        tap.action = Action::Seek(i64::MIN);
        assert!(matches!(
            tap.guarded_action(&v, pos),
            Some(player::Action::Seek(0.0))
        ));
        tap.action = Action::Seek(i64::MAX);
        assert!(matches!(
            tap.guarded_action(&v, pos),
            Some(player::Action::Next)
        ));
        tap.action = Action::SetPosition(v.duration_us);
        assert!(matches!(
            tap.guarded_action(&v, pos),
            Some(player::Action::Seek(10.0))
        ));
        assert!(
            tap.guarded_action(
                &View {
                    room_token: "5:4".into(),
                    ..v.clone()
                },
                pos
            )
            .is_none()
        );
        assert!(
            tap.guarded_action(
                &View {
                    room_token: "local:6:3".into(),
                    ..v.clone()
                },
                pos
            )
            .is_none()
        );
        assert!(
            tap.guarded_action(
                &View {
                    seek: false,
                    ..v.clone()
                },
                pos
            )
            .is_none()
        );
        tap.action = Action::Stop;
        assert!(matches!(
            tap.guarded_action(&v, pos),
            Some(player::Action::Pause)
        ));
    }

    #[test]
    fn notifies_observed_seek_but_not_progress_or_mode_switch() {
        let mut snapshot = Snapshot::default();
        let step = Duration::from_millis(125);
        let mut view = View {
            playing: true,
            ..fixture()
        };
        assert!(snapshot.update(view.clone(), 3_000_000, step));
        assert!(!snapshot.update(view.clone(), 3_125_000, step));
        view.room_token = "5:4".into();
        assert_eq!(view.track_path(), fixture().track_path());
        assert!(!snapshot.update(view.clone(), 3_250_000, step));
        assert!(snapshot.seeked.is_none());
        view.room_token = "5:5".into();
        snapshot.update(view.clone(), 9_000_000, step);
        assert_eq!(snapshot.seeked, Some(9_000_000));
        view.room_token = "local:6:3".into();
        view.track_token = "local:6:3:1".into();
        assert_ne!(view.track_path(), fixture().track_path());
        snapshot.update(view, 0, step);
        assert!(snapshot.seeked.is_none());
    }

    #[test]
    fn retirement_removes_metadata_and_controls_even_before_next_ui_tick() {
        let (actions, rx) = mpsc::sync_channel(1);
        let p = Player {
            actions,
            epoch: Arc::new(AtomicU64::new(2)),
            view: Arc::new(Mutex::new(Snapshot {
                view: View {
                    epoch: 2,
                    ..fixture()
                },
                position: 12,
                ..Default::default()
            })),
        };
        assert_eq!(p.current().0.metadata().len(), 3);
        p.send(Action::Pause).unwrap();
        assert_eq!(rx.try_recv().unwrap().epoch, 2);
        p.epoch.fetch_add(1, Ordering::SeqCst);
        assert!(p.current().0.metadata().is_empty());
        assert_eq!(p.current().1, 0);
        p.send(Action::Play).unwrap();
        p.seek(1).unwrap();
        p.stop().unwrap();
        assert!(!p.can_seek());
        assert!(rx.try_recv().is_err());
    }
}
