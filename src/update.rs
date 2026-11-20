//! Signed, user-local Linux releases. No updater runs in source builds or on
//! distro-managed binaries. Installing a new image never restarts the player.
use crate::desk::Access;
use reqwest::{Client, Response, Url, redirect::Policy};
use ring::{digest, signature};
use rustix::{
    fs::{FlockOperation, flock},
    process::geteuid,
};
use serde::Deserialize;
use slint::ComponentHandle;
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::sync::{mpsc, watch};
use uuid::Uuid;

const MANIFEST_ASSET: &str = "update-v1-linux-x86_64.json";
const BINARY_ASSET: &str = "spofify-desktop-linux-x86_64";
const MAX_MANIFEST: usize = 8 * 1024;
const MAX_BINARY: u64 = 64 * 1024 * 1024;
const CHECK_INTERVAL: Duration = Duration::from_secs(4 * 60 * 60);
const RETRY_INTERVAL: Duration = Duration::from_secs(15 * 60);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Version([u32; 3]);
impl Version {
    fn parse(value: &str) -> Option<Self> {
        let mut parts = value.split('.');
        let mut parsed = [0; 3];
        for n in &mut parsed {
            let p = parts.next()?;
            if p.is_empty()
                || p.len() > 10
                || p.len() > 1 && p.starts_with('0')
                || !p.bytes().all(|c| c.is_ascii_digit())
            {
                return None;
            }
            *n = p.parse().ok()?;
        }
        parts.next().is_none().then_some(Self(parsed))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    manifest: String,
    signature: String,
}
#[derive(Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema: u8,
    repository: String,
    asset: String,
    version: String,
    bytes: u64,
    sha256: String,
}

fn decode_hex<const N: usize>(s: &str) -> Option<[u8; N]> {
    if s.len() != N * 2 {
        return None;
    }
    fn digit(c: u8) -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            _ => None,
        }
    }
    let mut output = [0; N];
    for (slot, pair) in output.iter_mut().zip(s.as_bytes().chunks_exact(2)) {
        *slot = digit(pair[0])? * 16 + digit(pair[1])?;
    }
    Some(output)
}

struct Channel {
    repository: &'static str,
    key: [u8; 32],
    running: Version,
}
impl Channel {
    fn configured() -> Option<Self> {
        if cfg!(debug_assertions) {
            return None;
        }
        let repository = option_env!("SPOFIFY_UPDATE_REPO")?;
        let (owner, name) = repository.split_once('/')?;
        let safe_segment = |s: &str| {
            !s.is_empty()
                && s.len() <= 100
                && s.as_bytes()[0].is_ascii_alphanumeric()
                && s.bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.'))
        };
        if !safe_segment(owner) || !safe_segment(name) {
            return None;
        }
        Some(Self {
            repository,
            key: decode_hex(option_env!("SPOFIFY_UPDATE_PUBLIC_KEY")?)?,
            running: Version::parse(env!("CARGO_PKG_VERSION"))?,
        })
    }
    fn manifest(&self, body: &[u8]) -> Option<Manifest> {
        if body.len() > MAX_MANIFEST {
            return None;
        }
        let signed: Envelope = serde_json::from_slice(body).ok()?;
        let sig = decode_hex::<64>(&signed.signature)?;
        signature::UnparsedPublicKey::new(&signature::ED25519, self.key)
            .verify(signed.manifest.as_bytes(), &sig)
            .ok()?;
        let manifest: Manifest = serde_json::from_str(&signed.manifest).ok()?;
        (manifest.schema == 1
            && manifest.repository == self.repository
            && manifest.asset == BINARY_ASSET
            && (1..=MAX_BINARY).contains(&manifest.bytes)
            && decode_hex::<32>(&manifest.sha256).is_some()
            && Version::parse(&manifest.version)? > self.running)
            .then_some(manifest)
    }
    fn url(&self, suffix: &str) -> String {
        format!("https://github.com/{}/releases/{suffix}", self.repository)
    }
}

fn allowed_url(url: &Url) -> bool {
    url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && url.port().is_none()
        && matches!(
            url.host_str(),
            Some(
                "github.com"
                    | "release-assets.githubusercontent.com"
                    | "objects.githubusercontent.com"
            )
        )
}
fn client() -> Result<Client, reqwest::Error> {
    Client::builder()
        .https_only(true)
        .referer(false)
        .user_agent("Spofify Desktop updater")
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(90))
        .redirect(Policy::custom(|attempt| {
            if attempt.previous().len() < 5 && allowed_url(attempt.url()) {
                attempt.follow()
            } else {
                attempt.error("Unexpected release redirect")
            }
        }))
        .build()
}
async fn fetch(http: &Client, url: String) -> Option<Response> {
    http.get(url).send().await.ok()?.error_for_status().ok()
}

/// The managed package opts in by placing the executable in its dedicated
/// directory. Refuse symlinks, unowned/writable directories and stale processes.
fn current_image(path: &Path) -> io::Result<bool> {
    let parent = fs::symlink_metadata(path.parent().ok_or(io::ErrorKind::InvalidInput)?)?;
    let installed = fs::symlink_metadata(path)?;
    let running = fs::metadata("/proc/self/exe")?;
    let uid = geteuid().as_raw();
    Ok(parent.is_dir()
        && parent.uid() == uid
        && parent.mode() & 0o022 == 0
        && installed.is_file()
        && installed.uid() == uid
        && installed.mode() & 0o7022 == 0
        && running.dev() == installed.dev()
        && running.ino() == installed.ino())
}

struct Staged(PathBuf);
impl Staged {
    fn create(parent: &Path) -> io::Result<(Self, File)> {
        let path = parent.join(format!(".spofify-update-{}", Uuid::new_v4()));
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        Ok((Self(path), file))
    }
}
impl Drop for Staged {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

async fn check(http: &Client, channel: &Channel) -> Option<Option<Manifest>> {
    let mut response = fetch(
        http,
        channel.url(&format!("latest/download/{MANIFEST_ASSET}")),
    )
    .await?;
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if chunk.len() > MAX_MANIFEST - body.len() {
            return None;
        }
        body.extend_from_slice(&chunk);
    }
    Some(channel.manifest(&body))
}

async fn download(
    http: &Client,
    channel: &Channel,
    manifest: &Manifest,
    target: &Path,
    online: impl Fn() -> bool,
) -> Option<()> {
    if !online() || !current_image(target).ok()? {
        return None;
    }
    let lock = File::open(target.parent()?).ok()?;
    if flock(&lock, FlockOperation::NonBlockingLockExclusive).is_err()
        || !current_image(target).ok()?
    {
        return None;
    }
    let response = fetch(
        http,
        channel.url(&format!("download/v{}/{BINARY_ASSET}", manifest.version)),
    )
    .await?;
    if !save_update(response, manifest, target, || {
        online() && current_image(target).ok() == Some(true)
    })
    .await?
    {
        return None;
    }
    let _ = lock.sync_all();
    Some(())
}

async fn save_update(
    mut response: Response,
    manifest: &Manifest,
    target: &Path,
    may_replace: impl FnOnce() -> bool,
) -> Option<bool> {
    if response
        .content_length()
        .is_some_and(|n| n != manifest.bytes)
    {
        return None;
    }
    let (stage, mut file) = Staged::create(target.parent()?).ok()?;
    let mut hash = digest::Context::new(&digest::SHA256);
    let mut bytes = 0;
    while let Some(chunk) = response.chunk().await.ok()? {
        bytes += chunk.len() as u64;
        if bytes > manifest.bytes {
            return None;
        }
        hash.update(&chunk);
        file.write_all(&chunk).ok()?;
    }
    if bytes != manifest.bytes || hash.finish().as_ref() != decode_hex::<32>(&manifest.sha256)? {
        return None;
    }
    file.set_permissions(fs::Permissions::from_mode(0o755))
        .ok()?;
    file.sync_all().ok()?;
    if !may_replace() {
        return Some(false);
    }
    fs::rename(&stage.0, target).ok()?;
    Some(true)
}

enum Notice<'a> {
    Hidden,
    Available(&'a str, Uuid),
    Downloading,
    Failed(Uuid),
    Ready,
}

fn show(ui: &slint::Weak<crate::AppWindow>, notice: Notice<'_>) {
    let (message, button, action) = match notice {
        Notice::Hidden => (String::new(), "", None),
        Notice::Available(v, id) => (format!("Spofify {v} is available"), "Update", Some(id)),
        Notice::Downloading => ("Downloading update…".into(), "", None),
        Notice::Failed(id) => ("Couldn’t download update".into(), "Retry", Some(id)),
        Notice::Ready => ("Update ready. Reopen Spofify to apply it.".into(), "", None),
    };
    let _ = ui.upgrade_in_event_loop(move |ui| {
        ui.set_update_notice(message.into());
        ui.set_update_action(action.map_or_else(String::new, |id| id.to_string()).into());
        ui.set_update_button(button.into());
    });
}

fn bind_action(ui: &crate::AppWindow, request: mpsc::Sender<Uuid>) {
    let weak = ui.as_weak();
    ui.on_download_update(move || {
        let Some(ui) = weak.upgrade() else { return };
        if let Ok(id) = Uuid::parse_str(&ui.get_update_action())
            && request.try_send(id).is_ok()
        {
            // Consume the offer before another input event can replay the click.
            ui.set_update_action(Default::default());
        }
    });
}

pub fn start(ui: &crate::AppWindow, access: watch::Receiver<Option<Access>>) {
    let Some(channel) = Channel::configured() else {
        return;
    };
    let Some(home) = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
    else {
        return;
    };
    let target = home.join(".local/lib/spofify-desktop/spofify-desktop");
    if current_image(&target).ok() != Some(true) {
        return;
    }
    let (request, requests) = mpsc::channel(1);
    let weak = ui.as_weak();
    bind_action(ui, request);
    let _ = std::thread::Builder::new()
        .name("spofify-updates".into())
        .spawn(move || {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            runtime.block_on(run_updates(channel, target, weak, access, requests));
        });
}

async fn run_updates(
    channel: Channel,
    target: PathBuf,
    ui: slint::Weak<crate::AppWindow>,
    mut access: watch::Receiver<Option<Access>>,
    mut requests: mpsc::Receiver<Uuid>,
) {
    let Ok(http) = client() else { return };
    let live_access = access.clone();
    'connection: loop {
        show(&ui, Notice::Hidden);
        if access.has_changed().is_err() {
            return;
        }
        let (epoch, session) = match access
            .wait_for(|a| {
                a.as_ref().is_some_and(|a| {
                    !a.offline && !a.session.is_offline() && !a.session.is_expired()
                })
            })
            .await
        {
            Ok(access) => {
                let a = access.as_ref().expect("online access");
                (a.epoch, a.session.clone())
            }
            Err(_) => return,
        };
        let mut mode = session.network_changes();
        let generation = *mode.borrow_and_update();
        let current = session.network_changes();
        // Recent-list updates also publish Access; they must not restart the updater.
        let disconnected =
            |a: &Option<Access>| a.as_ref().is_none_or(|a| a.epoch != epoch || a.offline);
        let online = || {
            generation & 1 == 0
                && *current.borrow() == generation
                && !session.is_expired()
                && live_access.has_changed().is_ok()
                && !disconnected(&live_access.borrow())
        };
        if !online() {
            continue;
        }
        let mut offer: Option<(Manifest, Uuid)> = None;
        let mut delay = Duration::ZERO;
        loop {
            // Refresh the manifest without making the visible action unresponsive.
            let wait = delay;
            let refresh = async {
                tokio::time::sleep(wait).await;
                if online() && current_image(&target).ok() == Some(true) {
                    check(&http, &channel).await
                } else {
                    None
                }
            };
            tokio::pin!(refresh);
            loop {
                tokio::select! {
                    biased;
                    _ = mode.changed() => continue 'connection,
                    _ = async { let _ = access.wait_for(disconnected).await; } => continue 'connection,
                    request = requests.recv() => {
                        let Some(request) = request else { return };
                        let Some((manifest, id)) = offer.as_mut().filter(|(_, id)| *id == request) else {
                            continue;
                        };
                        show(&ui, Notice::Downloading);
                        let installed = tokio::select! {
                            biased;
                            _ = mode.changed() => continue 'connection,
                            _ = async { let _ = access.wait_for(disconnected).await; } => continue 'connection,
                            installed = download(&http, &channel, manifest, &target, online) => installed,
                        };
                        if installed.is_some() {
                            show(&ui, Notice::Ready);
                            return;
                        }
                        if current_image(&target).ok() != Some(true) {
                            show(&ui, Notice::Hidden);
                            return;
                        }
                        *id = Uuid::new_v4();
                        show(&ui, Notice::Failed(*id));
                        delay = RETRY_INTERVAL;
                    }
                    found = &mut refresh => {
                        if current_image(&target).ok() != Some(true) {
                            show(&ui, Notice::Hidden);
                            return;
                        }
                        if !online() {
                            continue 'connection;
                        }
                        delay = if found.is_some() { CHECK_INTERVAL } else { RETRY_INTERVAL };
                        let found = found.flatten();
                        if found.as_ref() != offer.as_ref().map(|(manifest, _)| manifest) {
                            offer = found.map(|manifest| (manifest, Uuid::new_v4()));
                            if let Some((manifest, id)) = &offer {
                                show(&ui, Notice::Available(&manifest.version, *id));
                            } else {
                                show(&ui, Notice::Hidden);
                            }
                        }
                    }
                }
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn banner_requires_a_fresh_click_for_each_attempt() {
        use slint::{
            LogicalPosition, PhysicalSize, PlatformError,
            platform::{
                Platform, PointerEventButton, WindowAdapter, WindowEvent,
                software_renderer::{MinimalSoftwareWindow, Rgb565Pixel},
            },
        };
        use std::rc::Rc;

        struct TestWindow(Rc<MinimalSoftwareWindow>);
        impl Platform for TestWindow {
            fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
                Ok(self.0.clone())
            }
        }
        let window = MinimalSoftwareWindow::new(Default::default());
        slint::platform::set_platform(Box::new(TestWindow(window.clone()))).unwrap();
        let ui = crate::AppWindow::new().unwrap();
        ui.set_signed_in(true);
        ui.set_playing(true);
        ui.set_now_track_id("test-track".into());
        ui.show().unwrap();
        ui.window().set_size(PhysicalSize::new(480, 440));
        let mut pixels = vec![Rgb565Pixel::default(); 480 * 440];
        let mut render = || {
            window.draw_if_needed(|renderer| {
                renderer.render(&mut pixels, 480);
            });
        };
        let position = LogicalPosition::new(448.0, 20.0);
        let button = PointerEventButton::Left;
        let press = || {
            ui.window()
                .dispatch_event(WindowEvent::PointerPressed { position, button })
        };
        let release = || {
            ui.window()
                .dispatch_event(WindowEvent::PointerReleased { position, button })
        };
        let click = || {
            press();
            release();
        };
        let (sender, mut requests) = mpsc::channel(1);
        bind_action(&ui, sender);
        let offer = Uuid::new_v4();
        ui.set_update_notice("Spofify 0.2.0 is available".into());
        ui.set_update_button("Update".into());
        ui.set_update_action(offer.to_string().into());
        render();
        assert!(requests.try_recv().is_err());
        press();
        let refreshed = Uuid::new_v4();
        ui.set_update_notice("Spofify 0.3.0 is available".into());
        ui.set_update_action(refreshed.to_string().into());
        render();
        release();
        assert!(requests.try_recv().is_err());
        click();
        assert_eq!(requests.try_recv().unwrap(), refreshed);
        click();
        assert!(requests.try_recv().is_err());
        ui.set_update_notice("Downloading update…".into());
        ui.set_update_button("".into());
        render();
        click();
        assert!(requests.try_recv().is_err());
        let retry = Uuid::new_v4();
        ui.set_update_notice("Couldn’t download update".into());
        ui.set_update_button("Retry".into());
        ui.set_update_action(retry.to_string().into());
        render();
        click();
        assert_eq!(requests.try_recv().unwrap(), retry);
        ui.set_update_notice("Update ready. Reopen Spofify to apply it.".into());
        ui.set_update_button("".into());
        render();
        assert!(ui.get_playing());
        assert_eq!(ui.get_now_track_id(), "test-track");
    }
    use ring::signature::{Ed25519KeyPair, KeyPair};
    use serde_json::json;

    #[test]
    fn version_is_numeric_and_canonical() {
        assert!(Version::parse("1.10.0") > Version::parse("1.9.99"));
        for bad in [
            "v1.0.0",
            "1.0",
            "1.0.0.0",
            "01.0.0",
            "1.0.0-rc1",
            "+1.0.0",
            "1.0.0/asset",
        ] {
            assert!(Version::parse(bad).is_none(), "{bad}");
        }
    }
    #[test]
    fn updates_require_exact_signature_channel_asset_and_newer_version() {
        let key = Ed25519KeyPair::from_seed_unchecked(&[3; 32]).unwrap();
        let channel = Channel {
            repository: "owner/downloads",
            key: key.public_key().as_ref().try_into().unwrap(),
            running: Version([1, 0, 0]),
        };
        let envelope = |version, repo, asset, size, tamper| {
            let manifest = json!({"schema":1,"repository":repo,"asset":asset,"version":version,"bytes":size,"sha256":"aa".repeat(32)}).to_string();
            let sig: String = key
                .sign(manifest.as_bytes())
                .as_ref()
                .iter()
                .map(|n| format!("{n:02x}"))
                .collect();
            let manifest = if tamper {
                format!("{manifest} ")
            } else {
                manifest
            };
            json!({"manifest":manifest,"signature":sig}).to_string()
        };
        let good = envelope("1.0.1", channel.repository, BINARY_ASSET, 1024, false);
        assert_eq!(channel.manifest(good.as_bytes()).unwrap().version, "1.0.1");
        for bad in [
            envelope("1.0.0", channel.repository, BINARY_ASSET, 1024, false),
            envelope("0.9.0", channel.repository, BINARY_ASSET, 1024, false),
            envelope("1.0.1", "other/downloads", BINARY_ASSET, 1024, false),
            envelope("1.0.1", channel.repository, "different-asset", 1024, false),
            envelope(
                "1.0.1",
                channel.repository,
                BINARY_ASSET,
                MAX_BINARY + 1,
                false,
            ),
            envelope("1.0.1", channel.repository, BINARY_ASSET, 1024, true),
        ] {
            assert!(channel.manifest(bad.as_bytes()).is_none());
        }
    }
    #[tokio::test]
    async fn downloaded_bytes_replace_atomically_only_after_verification_and_admission() {
        use std::io::Read;
        use std::os::unix::fs::DirBuilderExt;
        use tokio::{
            io::{AsyncReadExt, AsyncWriteExt},
            net::TcpListener,
        };
        struct Fixture(PathBuf);
        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        let dir =
            Fixture(std::env::temp_dir().join(format!("spofify-upgrade-{}", uuid::Uuid::new_v4())));
        fs::DirBuilder::new().mode(0o700).create(&dir.0).unwrap();
        let target = dir.0.join("spofify-desktop");
        fs::write(&target, b"old image").unwrap();
        let mut opened_before_upgrade = File::open(&target).unwrap();
        let next = b"new image";
        let manifest = Manifest {
            schema: 1,
            repository: String::new(),
            asset: BINARY_ASSET.into(),
            version: "1.0.1".into(),
            bytes: next.len() as u64,
            sha256: digest::digest(&digest::SHA256, next)
                .as_ref()
                .iter()
                .map(|n| format!("{n:02x}"))
                .collect(),
        };
        let http = Client::builder().no_proxy().build().unwrap();
        for (payload, admitted, succeeds) in [
            (&b"bad image"[..], true, false),
            (&b"short"[..], true, false),
            (&next[..], false, true),
            (&next[..], true, true),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                socket.read(&mut [0; 2048]).await.unwrap();
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    payload.len()
                );
                socket.write_all(header.as_bytes()).await.unwrap();
                socket.write_all(payload).await.unwrap();
            });
            let response = fetch(&http, url).await.unwrap();
            assert_eq!(
                save_update(response, &manifest, &target, || admitted).await,
                succeeds.then_some(admitted)
            );
            server.await.unwrap();
            assert_eq!(
                fs::read(&target).unwrap(),
                if admitted && succeeds {
                    &next[..]
                } else {
                    &b"old image"[..]
                }
            );
            assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 1);
        }
        let mut preserved = Vec::new();
        opened_before_upgrade.read_to_end(&mut preserved).unwrap();
        assert_eq!(preserved, b"old image");
        assert_eq!(fs::metadata(&target).unwrap().mode() & 0o777, 0o755);
    }
    #[test]
    fn redirects_stay_on_exact_github_https_origins() {
        assert!(allowed_url(
            &Url::parse("https://release-assets.githubusercontent.com/file").unwrap()
        ));
        for bad in [
            "http://github.com/file",
            "https://github.com.evil.invalid/file",
            "https://evil@github.com/file",
            "https://github.com:444/file",
        ] {
            assert!(!allowed_url(&Url::parse(bad).unwrap()));
        }
    }
}
