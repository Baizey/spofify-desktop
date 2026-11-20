//! User-local Linux releases verified against GitHub release metadata.
//! Installing a new image never restarts the player.
use crate::desk::Access;
use futures_util::{Stream, StreamExt};
use reqwest::{Client, ClientBuilder, RequestBuilder, Response, StatusCode, Url, redirect::Policy};
use ring::digest;
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

const LATEST_RELEASE_API: &str =
    "https://api.github.com/repos/Baizey/spofify-desktop/releases/latest";
const ASSET_BASE: &str = "https://github.com/Baizey/spofify-desktop/releases/download";
const BINARY_ASSET: &str = "spofify-desktop-linux-x86_64";
const MAX_METADATA: usize = 1024 * 1024;
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
struct Release {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<ReleaseAsset>,
}

#[derive(Deserialize)]
struct ReleaseAsset {
    name: String,
    state: String,
    browser_download_url: String,
    size: u64,
    digest: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
struct Update {
    version: String,
    url: String,
    bytes: u64,
    sha256: [u8; 32],
}

fn decode_hex<const N: usize>(s: &str) -> Option<[u8; N]> {
    if s.len() != N * 2 {
        return None;
    }
    fn digit(c: u8) -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    }
    let mut output = [0; N];
    for (slot, pair) in output.iter_mut().zip(s.as_bytes().chunks_exact(2)) {
        *slot = digit(pair[0])? * 16 + digit(pair[1])?;
    }
    Some(output)
}

fn available_update(body: &[u8], running: Version) -> Option<Update> {
    let release: Release = serde_json::from_slice(body).ok()?;
    let version = release.tag_name.strip_prefix('v')?;
    if release.draft || release.prerelease || Version::parse(version)? <= running {
        return None;
    }
    let mut matching = release
        .assets
        .into_iter()
        .filter(|a| a.name == BINARY_ASSET);
    let asset = matching.next()?;
    let url = format!("{ASSET_BASE}/{}/{BINARY_ASSET}", release.tag_name);
    if matching.next().is_some()
        || asset.state != "uploaded"
        || asset.browser_download_url != url
        || !(1..=MAX_BINARY).contains(&asset.size)
    {
        return None;
    }
    Some(Update {
        version: version.into(),
        url,
        bytes: asset.size,
        sha256: decode_hex(asset.digest?.strip_prefix("sha256:")?)?,
    })
}

fn allowed_asset_redirect(url: &Url, previous: usize) -> bool {
    // The canonical GitHub download may send one redirect to GitHub's asset
    // storage. Do not follow repository moves or aliases back through github.com.
    previous == 1
        && url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && url.port().is_none()
        && url.fragment().is_none()
        && matches!(
            url.host_str(),
            Some("release-assets.githubusercontent.com" | "objects.githubusercontent.com")
        )
        && url.path().starts_with("/github-production-release-asset/")
}

fn client() -> ClientBuilder {
    Client::builder()
        .https_only(true)
        .no_proxy()
        .referer(false)
        .user_agent("Spofify Desktop updater")
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(90))
}

fn asset_client() -> Result<Client, reqwest::Error> {
    client()
        .redirect(Policy::custom(|attempt| {
            if allowed_asset_redirect(attempt.url(), attempt.previous().len()) {
                attempt.follow()
            } else {
                attempt.error("Unexpected release asset redirect")
            }
        }))
        .build()
}

fn release_request(http: &Client) -> RequestBuilder {
    http.get(LATEST_RELEASE_API)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
}

async fn fetch(request: RequestBuilder) -> Option<Response> {
    let response = request.send().await.ok()?;
    (response.status() == StatusCode::OK).then_some(response)
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

async fn check(http: &Client, running: Version) -> Option<Option<Update>> {
    let response = fetch(release_request(http)).await?;
    let body = bounded_metadata(response.content_length(), response.bytes_stream()).await?;
    Some(available_update(&body, running))
}

async fn bounded_metadata<T: AsRef<[u8]>, E>(
    content_length: Option<u64>,
    chunks: impl Stream<Item = Result<T, E>>,
) -> Option<Vec<u8>> {
    if content_length.is_some_and(|n| n > MAX_METADATA as u64) {
        return None;
    }
    let mut body = Vec::new();
    tokio::pin!(chunks);
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk.ok()?;
        let chunk = chunk.as_ref();
        if chunk.len() > MAX_METADATA - body.len() {
            return None;
        }
        body.extend_from_slice(chunk);
    }
    Some(body)
}

async fn download(
    http: &Client,
    update: &Update,
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
    let response = fetch(http.get(&update.url)).await?;
    if !save_update(
        response.content_length(),
        response.bytes_stream(),
        update,
        target,
        || online() && current_image(target).ok() == Some(true),
    )
    .await?
    {
        return None;
    }
    let _ = lock.sync_all();
    Some(())
}

async fn save_update<T: AsRef<[u8]>, E>(
    content_length: Option<u64>,
    chunks: impl Stream<Item = Result<T, E>>,
    update: &Update,
    target: &Path,
    may_replace: impl FnOnce() -> bool,
) -> Option<bool> {
    if content_length.is_some_and(|n| n != update.bytes) {
        return None;
    }
    let (stage, mut file) = Staged::create(target.parent()?).ok()?;
    let mut hash = digest::Context::new(&digest::SHA256);
    let mut bytes = 0;
    tokio::pin!(chunks);
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk.ok()?;
        let chunk = chunk.as_ref();
        if chunk.len() as u64 > update.bytes - bytes {
            return None;
        }
        bytes += chunk.len() as u64;
        hash.update(chunk);
        file.write_all(chunk).ok()?;
    }
    if bytes != update.bytes || hash.finish().as_ref() != update.sha256 {
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
    if cfg!(debug_assertions) || !cfg!(target_arch = "x86_64") {
        return;
    }
    let Some(running) = Version::parse(env!("CARGO_PKG_VERSION")) else {
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
            runtime.block_on(run_updates(running, target, weak, access, requests));
        });
}

async fn run_updates(
    running: Version,
    target: PathBuf,
    ui: slint::Weak<crate::AppWindow>,
    mut access: watch::Receiver<Option<Access>>,
    mut requests: mpsc::Receiver<Uuid>,
) {
    let Ok(api) = client().redirect(Policy::none()).build() else {
        return;
    };
    let Ok(assets) = asset_client() else { return };
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
        let mut offer: Option<(Update, Uuid)> = None;
        let mut delay = Duration::ZERO;
        loop {
            // Refresh the offer without making the visible action unresponsive.
            let wait = delay;
            let refresh = async {
                tokio::time::sleep(wait).await;
                if online() && current_image(&target).ok() == Some(true) {
                    check(&api, running).await
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
                        let Some((update, id)) = offer.as_mut().filter(|(_, id)| *id == request) else {
                            continue;
                        };
                        show(&ui, Notice::Downloading);
                        let installed = tokio::select! {
                            biased;
                            _ = mode.changed() => continue 'connection,
                            _ = async { let _ = access.wait_for(disconnected).await; } => continue 'connection,
                            installed = download(&assets, update, &target, online) => installed,
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
                        if found.as_ref() != offer.as_ref().map(|(update, _)| update) {
                            offer = found.map(|update| (update, Uuid::new_v4()));
                            if let Some((update, id)) = &offer {
                                show(&ui, Notice::Available(&update.version, *id));
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
    use serde_json::{Value, json};

    fn fixture_release(tag: &str, binary: &[u8]) -> Value {
        let sha256: String = digest::digest(&digest::SHA256, binary)
            .as_ref()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        json!({
            "tag_name": tag, "draft": false, "prerelease": false,
            "body": "Release notes are not trusted for updater decisions.",
            "assets": [{
                "id": 123, "name": BINARY_ASSET, "state": "uploaded",
                "size": binary.len(), "digest": format!("sha256:{sha256}"),
                "browser_download_url": format!("{ASSET_BASE}/{tag}/{BINARY_ASSET}"),
            }, {
                "id": 456, "name": "spofify-desktop-1.0.1-linux-x86_64.tar.gz",
                "state": "uploaded", "size": 1234, "digest": null,
                "browser_download_url": format!("{ASSET_BASE}/{tag}/installer.tar.gz"),
            }],
        })
    }

    fn parse_fixture(release: &Value) -> Option<Update> {
        available_update(&serde_json::to_vec(release).unwrap(), Version([1, 0, 0]))
    }

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
    fn api_request_uses_only_the_public_canonical_latest_endpoint() {
        let api = client().redirect(Policy::none()).build().unwrap();
        let req = release_request(&api).build().unwrap();
        assert_eq!(req.method(), reqwest::Method::GET);
        assert_eq!(
            req.url().as_str(),
            "https://api.github.com/repos/Baizey/spofify-desktop/releases/latest"
        );
        assert_eq!(req.headers()["accept"], "application/vnd.github+json");
        assert!(req.headers().contains_key("x-github-api-version"));
        assert!(!req.headers().contains_key("authorization"));
        assert!(!req.headers().contains_key("cookie"));
    }

    #[test]
    fn release_must_be_stable_canonical_and_newer() {
        let binary = b"test image";
        let good = fixture_release("v1.0.1", binary);
        let update = parse_fixture(&good).unwrap();
        assert_eq!(update.version, "1.0.1");
        assert_eq!(update.bytes, binary.len() as u64);
        assert_eq!(
            update.sha256,
            digest::digest(&digest::SHA256, binary).as_ref()
        );
        for tag in [
            "v1.0.0",
            "v0.9.0",
            "1.0.1",
            "V1.0.1",
            "v1.00.1",
            "v1.0.1-rc.1",
            "v1.0.1+build",
            "v1.0.1/asset",
        ] {
            assert!(
                parse_fixture(&fixture_release(tag, binary)).is_none(),
                "{tag}"
            );
        }
        for field in ["draft", "prerelease"] {
            let mut bad = good.clone();
            bad[field] = json!(true);
            assert!(parse_fixture(&bad).is_none(), "{field}");
            bad.as_object_mut().unwrap().remove(field);
            assert!(parse_fixture(&bad).is_none(), "missing {field}");
        }
    }

    #[test]
    fn exact_uploaded_asset_requires_one_size_digest_and_canonical_url() {
        let good = fixture_release("v1.0.1", b"test image");
        let mut duplicate = good.clone();
        duplicate["assets"]
            .as_array_mut()
            .unwrap()
            .push(good["assets"][0].clone());
        assert!(parse_fixture(&duplicate).is_none());
        let mut missing = good.clone();
        missing["assets"][0]
            .as_object_mut()
            .unwrap()
            .remove("digest");
        assert!(parse_fixture(&missing).is_none());
        for (field, value) in [
            ("name", json!("different-asset")),
            ("state", json!("starter")),
            ("size", json!(0)),
            ("size", json!(MAX_BINARY + 1)),
            ("size", json!(-1)),
            ("digest", Value::Null),
            ("digest", json!(format!("sha512:{}", "00".repeat(32)))),
            ("digest", json!(format!("sha256:{}", "zz".repeat(32)))),
            ("digest", json!(format!("sha256:{}", "aa".repeat(31)))),
        ] {
            let mut bad = good.clone();
            bad["assets"][0][field] = value;
            assert!(parse_fixture(&bad).is_none(), "{bad}");
        }
        let canonical = good["assets"][0]["browser_download_url"].as_str().unwrap();
        for url in [
            canonical.replace("Baizey", "other-owner"),
            canonical.replace("spofify-desktop/releases", "other-repo/releases"),
            canonical.replace("v1.0.1", "v1.0.2"),
            canonical.replace(BINARY_ASSET, "other-asset"),
            canonical.replace("github.com", "github.com.evil.invalid"),
            canonical.replace("https://", "http://"),
            format!("{canonical}?raw=true"),
            format!("{canonical}#file"),
        ] {
            let mut bad = good.clone();
            bad["assets"][0]["browser_download_url"] = json!(url);
            assert!(parse_fixture(&bad).is_none(), "{url}");
        }
    }

    #[tokio::test]
    async fn bounds_metadata_before_and_during_streaming() {
        use futures_util::stream;
        let max = vec![0u8; MAX_METADATA];
        let ok = [Ok::<_, ()>(max.as_slice())];
        assert_eq!(
            bounded_metadata(None, stream::iter(ok))
                .await
                .unwrap()
                .len(),
            MAX_METADATA
        );
        let over = [ok[0], Ok(b"x".as_slice())];
        assert!(bounded_metadata(None, stream::iter(over)).await.is_none());
        assert!(
            bounded_metadata(Some(MAX_METADATA as u64 + 1), stream::iter(ok))
                .await
                .is_none()
        );
        assert!(
            bounded_metadata(None, stream::iter([ok[0], Err(())]))
                .await
                .is_none()
        );
    }
    #[tokio::test]
    async fn downloaded_bytes_replace_atomically_only_after_verification_and_admission() {
        use futures_util::stream;
        use std::io::Read;
        use std::os::unix::fs::DirBuilderExt;
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
        let update = parse_fixture(&fixture_release("v1.0.1", next)).unwrap();
        for (payload, length, interrupted, admitted, succeeds) in [
            (&b"bad image"[..], None, false, true, false),
            (&b"short"[..], None, false, true, false),
            (&b"new image and extra"[..], None, false, true, false),
            (&next[..], Some(1), false, true, false),
            (&next[..], None, true, true, false),
            (&next[..], None, false, false, true),
            (&next[..], Some(update.bytes), false, true, true),
        ] {
            let chunks = payload
                .chunks(3)
                .map(Ok)
                .chain(interrupted.then_some(Err(())));
            assert_eq!(
                save_update(length, stream::iter(chunks), &update, &target, || admitted).await,
                succeeds.then_some(admitted)
            );
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
    fn redirects_allow_only_one_hop_to_github_asset_storage() {
        for host in [
            "release-assets.githubusercontent.com",
            "objects.githubusercontent.com",
        ] {
            let allowed = Url::parse(&format!(
                "https://{host}/github-production-release-asset/123/test?signature=example"
            ))
            .unwrap();
            assert!(allowed_asset_redirect(&allowed, 1));
            assert!(!allowed_asset_redirect(&allowed, 2));
        }
        for bad in [
            "https://github.com/other/repo/releases/download/v1.0.1/file",
            "https://api.github.com/repos/other/repo/releases/latest",
            "http://release-assets.githubusercontent.com/github-production-release-asset/123/test",
            "https://release-assets.githubusercontent.com.evil.invalid/github-production-release-asset/123/test",
            "https://evil@release-assets.githubusercontent.com/github-production-release-asset/123/test",
            "https://release-assets.githubusercontent.com:444/github-production-release-asset/123/test",
            "https://release-assets.githubusercontent.com/github-production-release-asset/123/test#file",
            "https://release-assets.githubusercontent.com/unexpected/file",
        ] {
            assert!(
                !allowed_asset_redirect(&Url::parse(bad).unwrap(), 1),
                "{bad}"
            );
        }
    }
}
