//! User-local Linux releases verified against GitHub release metadata.
//! Installing a new image never restarts the player.
use crate::{
    api_transport::{is_transient_error, is_transient_status},
    desk::Access,
    playlist_pin_store::PUBLICATION_GATE,
};
use futures_util::{Stream, StreamExt};
use reqwest::{
    Client, ClientBuilder, RequestBuilder, Response, StatusCode, Url, header::RETRY_AFTER,
    redirect::Policy,
};
use ring::digest;
use rustix::{
    fs::{FlockOperation, flock},
    process::geteuid,
};
use serde::Deserialize;
use slint::ComponentHandle;
use std::{
    fmt,
    fs::{self, File, OpenOptions},
    future::Future,
    io::{self, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    sync::{mpsc, watch},
    time::{Instant, timeout_at},
};
use uuid::Uuid;

const LATEST_RELEASE_API: &str =
    "https://api.github.com/repos/Baizey/spofify-desktop/releases/latest";
const ASSET_BASE: &str = "https://github.com/Baizey/spofify-desktop/releases/download";
const BINARY_ASSET: &str = "spofify-desktop-linux-x86_64";
const MAX_METADATA: usize = 1024 * 1024;
const MAX_BINARY: u64 = 64 * 1024 * 1024;
const ELF_HEADER_LEN: usize = 64;
const READ_BUDGET: Duration = Duration::from_secs(90);
const CHECK_INTERVAL: Duration = Duration::from_secs(4 * 60 * 60);
const CHECK_FAILURE_INTERVAL: Duration = Duration::from_secs(15 * 60);
const READ_BACKOFF: [Duration; 2] = [Duration::from_millis(250), Duration::from_millis(500)];

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

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [major, minor, patch] = self.0;
        write!(f, "{major}.{minor}.{patch}")
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
    version: Version,
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
    let version = Version::parse(release.tag_name.strip_prefix('v')?)?;
    if release.draft || release.prerelease || version <= running {
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
        version,
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
        .retry(reqwest::retry::never())
        .user_agent("Spofify Desktop updater")
        .connect_timeout(Duration::from_secs(10))
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReadFailure {
    Transient,
    Permanent,
}

impl From<reqwest::Error> for ReadFailure {
    fn from(error: reqwest::Error) -> Self {
        if !error.is_builder() && !error.is_redirect() && is_transient_error(&error) {
            Self::Transient
        } else {
            Self::Permanent
        }
    }
}

fn response_status(status: StatusCode, has_retry_after: bool) -> Result<(), ReadFailure> {
    match status {
        StatusCode::OK => Ok(()),
        // Leave a server-directed wait to the normal periodic checks, rather
        // than issuing another request after our short fixed delay.
        status if !has_retry_after && is_transient_status(status.as_u16()) => {
            Err(ReadFailure::Transient)
        }
        _ => Err(ReadFailure::Permanent),
    }
}

async fn fetch(request: RequestBuilder) -> Result<Response, ReadFailure> {
    let response = request.send().await?;
    response_status(
        response.status(),
        response.headers().contains_key(RETRY_AFTER),
    )?;
    Ok(response)
}

// Only wrap replayable reads, never installation. Inject the wait so tests
// can check the complete retry sequence without real timers or connections.
async fn retry_read<T, R, W>(
    deadline: Instant,
    mut read: impl FnMut() -> R,
    mut wait: impl FnMut(Duration) -> W,
) -> Result<T, ReadFailure>
where
    R: Future<Output = Result<T, ReadFailure>>,
    W: Future<Output = ()>,
{
    timeout_at(deadline, async {
        let mut backoff = READ_BACKOFF.into_iter();
        loop {
            if Instant::now() >= deadline {
                return Err(ReadFailure::Transient);
            }
            let result = read().await;
            if Instant::now() >= deadline {
                return Err(ReadFailure::Transient);
            }
            match (result, backoff.next()) {
                (Err(ReadFailure::Transient), Some(delay)) => wait(delay).await,
                (result, _) => return result,
            }
        }
    })
    .await
    .unwrap_or(Err(ReadFailure::Transient))
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

async fn check(
    http: &Client,
    running: Version,
    online: impl Fn() -> bool,
) -> Result<Option<Update>, ReadFailure> {
    let deadline = Instant::now() + READ_BUDGET;
    let body = retry_read(
        deadline,
        || async {
            if !online() {
                return Err(ReadFailure::Permanent);
            }
            let response = fetch(release_request(http)).await?;
            bounded_metadata(
                response.content_length(),
                response
                    .bytes_stream()
                    .map(|chunk| chunk.map_err(Into::into)),
            )
            .await
        },
        tokio::time::sleep,
    )
    .await?;
    // A complete but invalid manifest cannot benefit from a retry.
    let update = available_update(&body, running);
    if Instant::now() >= deadline {
        Err(ReadFailure::Transient)
    } else {
        Ok(update)
    }
}

async fn bounded_metadata<T: AsRef<[u8]>>(
    content_length: Option<u64>,
    chunks: impl Stream<Item = Result<T, ReadFailure>>,
) -> Result<Vec<u8>, ReadFailure> {
    if content_length.is_some_and(|n| n > MAX_METADATA as u64) {
        return Err(ReadFailure::Permanent);
    }
    let mut body = Vec::new();
    tokio::pin!(chunks);
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk?;
        let chunk = chunk.as_ref();
        if chunk.len() > MAX_METADATA - body.len() {
            return Err(ReadFailure::Permanent);
        }
        body.extend_from_slice(chunk);
    }
    Ok(body)
}

async fn download(
    http: &Client,
    update: &Update,
    target: &Path,
    online: impl Fn() -> bool,
) -> Option<()> {
    let deadline = Instant::now() + READ_BUDGET;
    if !online() || !current_image(target).ok()? {
        return None;
    }
    let lock = File::open(target.parent()?).ok()?;
    flock(&lock, FlockOperation::NonBlockingLockExclusive).ok()?;
    let (stage, file) = retry_read(
        deadline,
        || async {
            if !online() || current_image(target).ok() != Some(true) {
                return Err(ReadFailure::Permanent);
            }
            let response = fetch(http.get(&update.url)).await?;
            stage_download(
                response.content_length(),
                response
                    .bytes_stream()
                    .map(|chunk| chunk.map_err(Into::into)),
                update,
                target,
            )
            .await
        },
        tokio::time::sleep,
    )
    .await
    .ok()?;
    install_staged(stage, file, target, deadline, || {
        current_image(target).ok() == Some(true) && online()
    })?;
    let _ = lock.sync_all();
    Some(())
}

async fn stage_download<T: AsRef<[u8]>>(
    content_length: Option<u64>,
    chunks: impl Stream<Item = Result<T, ReadFailure>>,
    update: &Update,
    target: &Path,
) -> Result<(Staged, File), ReadFailure> {
    let permanent = ReadFailure::Permanent;
    if update.bytes < ELF_HEADER_LEN as u64 || content_length.is_some_and(|n| n != update.bytes) {
        return Err(permanent);
    }
    let parent = target.parent().ok_or(permanent)?;
    let (stage, mut file) = Staged::create(parent).map_err(|_| permanent)?;
    let mut hash = digest::Context::new(&digest::SHA256);
    let mut header = [0; ELF_HEADER_LEN];
    let mut bytes = 0;
    tokio::pin!(chunks);
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk?;
        let chunk = chunk.as_ref();
        if chunk.len() as u64 > update.bytes - bytes {
            return Err(permanent);
        }
        if bytes < ELF_HEADER_LEN as u64 {
            let offset = bytes as usize;
            let n = chunk.len().min(ELF_HEADER_LEN - offset);
            header[offset..offset + n].copy_from_slice(&chunk[..n]);
            if offset + n == ELF_HEADER_LEN && !executable_header(&header) {
                return Err(permanent);
            }
        }
        bytes += chunk.len() as u64;
        hash.update(chunk);
        file.write_all(chunk).map_err(|_| permanent)?;
    }
    if bytes != update.bytes || hash.finish().as_ref() != update.sha256 {
        return Err(permanent);
    }
    Ok((stage, file))
}

// Sanity check only; the digest still verifies the exact published bytes.
fn executable_header(header: &[u8; ELF_HEADER_LEN]) -> bool {
    header[..7] == *b"\x7fELF\x02\x01\x01"
        && matches!(u16::from_le_bytes([header[16], header[17]]), 2 | 3) // ET_EXEC or ET_DYN
        && header[18..24] == [62, 0, 1, 0, 0, 0] // x86-64, ELF version 1
        && header[52..54] == [ELF_HEADER_LEN as u8, 0]
}

fn install_staged(
    stage: Staged,
    file: File,
    target: &Path,
    deadline: Instant,
    may_replace: impl FnOnce() -> bool,
) -> Option<()> {
    file.set_permissions(fs::Permissions::from_mode(0o755))
        .ok()?;
    file.sync_all().ok()?;
    // Serialize final admission and rename with epoch and network-mode changes.
    // A blocking sync or a wait for this gate must not extend the deadline.
    let _publication = PUBLICATION_GATE.lock().ok()?;
    if !may_replace() || Instant::now() >= deadline {
        return None;
    }
    fs::rename(&stage.0, target).ok()
}

#[derive(Debug, PartialEq, Eq)]
enum Notice {
    Hidden,
    Available(Version, Uuid),
    Downloading,
    Failed,
    Ready,
}

impl Notice {
    fn banner(self) -> (String, &'static str, Option<Uuid>) {
        match self {
            Self::Hidden => (String::new(), "", None),
            Self::Available(v, id) => (format!("Spofify {v} is available"), "Update", Some(id)),
            Self::Downloading => ("Downloading update…".into(), "", None),
            Self::Failed => ("Couldn’t install update".into(), "", None),
            Self::Ready => ("Update ready. Reopen Spofify to apply it.".into(), "", None),
        }
    }
}

#[derive(Default)]
struct Offers {
    pending: Option<(Update, Uuid)>,
    failed: Option<Version>,
}

impl Offers {
    fn notice(&self) -> Notice {
        if let Some((update, id)) = &self.pending {
            Notice::Available(update.version, *id)
        } else if self.failed.is_some() {
            Notice::Failed
        } else {
            Notice::Hidden
        }
    }

    fn refresh(&mut self, found: Option<Update>) -> Option<Notice> {
        // Periodic checks and reconnects must not re-offer a failed version,
        // even if its metadata changes. A newer version can ask for consent.
        let found = found.filter(|update| self.failed.is_none_or(|v| update.version > v));
        if found.as_ref() == self.pending.as_ref().map(|(update, _)| update) {
            return None;
        }
        self.pending = found.map(|update| (update, Uuid::new_v4()));
        Some(self.notice())
    }

    fn approve(&mut self, request: Uuid) -> Option<Update> {
        self.pending.as_ref().filter(|(_, id)| *id == request)?;
        self.pending.take().map(|(update, _)| update)
    }

    fn reset_offer(&mut self) -> Notice {
        self.pending = None;
        self.notice()
    }

    fn fail(&mut self, update: Update) -> Notice {
        self.failed = Some(update.version);
        self.notice()
    }
}

fn show(ui: &slint::Weak<crate::AppWindow>, notice: Notice) {
    let (message, button, action) = notice.banner();
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

pub fn start(
    ui: &crate::AppWindow,
    access: watch::Receiver<Option<Access>>,
    shared_epoch: Arc<AtomicU64>,
) {
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
            runtime.block_on(run_updates(
                running,
                target,
                weak,
                access,
                requests,
                shared_epoch,
            ));
        });
}

async fn run_updates(
    running: Version,
    target: PathBuf,
    ui: slint::Weak<crate::AppWindow>,
    mut access: watch::Receiver<Option<Access>>,
    mut requests: mpsc::Receiver<Uuid>,
    shared_epoch: Arc<AtomicU64>,
) {
    let Ok(api) = client().redirect(Policy::none()).build() else {
        return;
    };
    let Ok(assets) = asset_client() else { return };
    let live_access = access.clone();
    let mut offers = Offers::default();
    'connection: loop {
        show(&ui, offers.reset_offer());
        if access.has_changed().is_err() {
            return;
        }
        let (epoch, session) = match access
            .wait_for(|a| {
                a.as_ref().is_some_and(|a| {
                    a.epoch == shared_epoch.load(Ordering::SeqCst)
                        && !a.offline
                        && !a.session.is_offline()
                        && !a.session.is_expired()
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
            current_connection(&shared_epoch, epoch, &current, generation)
                && !session.is_expired()
                && live_access.has_changed().is_ok()
                && !disconnected(&live_access.borrow())
        };
        if !online() {
            continue;
        }
        let mut delay = Duration::ZERO;
        loop {
            // Refresh the offer without making the visible action unresponsive.
            let wait = delay;
            let refresh = async {
                tokio::time::sleep(wait).await;
                if online() && current_image(&target).ok() == Some(true) {
                    check(&api, running, online).await
                } else {
                    Ok(None)
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
                        let Some(update) = offers.approve(request) else {
                            continue;
                        };
                        show(&ui, Notice::Downloading);
                        let installed = tokio::select! {
                            biased;
                            _ = mode.changed() => continue 'connection,
                            _ = async { let _ = access.wait_for(disconnected).await; } => continue 'connection,
                            installed = download(&assets, &update, &target, online) => installed,
                        };
                        if installed.is_some() {
                            show(&ui, Notice::Ready);
                            return;
                        }
                        // An epoch change can precede the Access notification.
                        // A cancelled download must not suppress a future offer.
                        if !online() {
                            continue 'connection;
                        }
                        show(&ui, offers.fail(update));
                        if current_image(&target).ok() != Some(true) {
                            return;
                        }
                        delay = CHECK_INTERVAL;
                    }
                    found = &mut refresh => {
                        if current_image(&target).ok() != Some(true) {
                            show(&ui, offers.reset_offer());
                            return;
                        }
                        if !online() {
                            continue 'connection;
                        }
                        delay = if found == Err(ReadFailure::Transient) {
                            CHECK_FAILURE_INTERVAL
                        } else {
                            CHECK_INTERVAL
                        };
                        if let Some(notice) = offers.refresh(found.ok().flatten()) {
                            show(&ui, notice);
                        }
                    }
                }
                break;
            }
        }
    }
}

fn current_connection(
    shared_epoch: &AtomicU64,
    epoch: u64,
    network: &watch::Receiver<u64>,
    generation: u64,
) -> bool {
    shared_epoch.load(Ordering::SeqCst) == epoch
        && generation & 1 == 0
        && *network.borrow() == generation
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::stream;
    use serde_json::{Value, json};
    use std::{cell::Cell, future::ready, os::unix::fs::DirBuilderExt};

    #[test]
    fn banner_only_accepts_a_current_update_click() {
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
        let set_banner = |notice: Notice| {
            let (message, button, action) = notice.banner();
            ui.set_update_notice(message.into());
            ui.set_update_button(button.into());
            ui.set_update_action(action.map(|id| id.to_string()).unwrap_or_default().into());
        };
        let offer = Uuid::new_v4();
        set_banner(Notice::Available(Version([1, 0, 1]), offer));
        render();
        assert!(requests.try_recv().is_err());
        press();
        let refreshed = Uuid::new_v4();
        set_banner(Notice::Available(Version([1, 0, 2]), refreshed));
        render();
        release();
        assert!(requests.try_recv().is_err());
        click();
        assert_eq!(requests.try_recv().unwrap(), refreshed);
        click();
        assert!(requests.try_recv().is_err());
        for notice in [Notice::Downloading, Notice::Failed, Notice::Ready] {
            set_banner(notice);
            render();
            click();
            assert!(requests.try_recv().is_err());
            assert!(!ui.get_update_notice().is_empty());
            assert!(ui.get_update_button().is_empty());
            assert!(ui.get_update_action().is_empty());
        }
        assert!(ui.get_playing());
        assert_eq!(ui.get_now_track_id(), "test-track");
    }

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
    fn approval_is_single_use_and_failure_survives_periodic_checks_and_reconnects() {
        let update = || parse_fixture(&fixture_release("v1.9.0", b"update")).unwrap();
        let mut offers = Offers::default();
        assert_eq!(offers.notice(), Notice::Hidden);
        assert!(offers.approve(Uuid::nil()).is_none());
        let Some(Notice::Available(version, id)) = offers.refresh(Some(update())) else {
            panic!("expected an update offer");
        };
        assert_eq!(offers.refresh(Some(update())), None);
        assert!(offers.approve(Uuid::nil()).is_none());
        assert_eq!(offers.notice(), Notice::Available(version, id));
        let approved = offers.approve(id).unwrap();
        assert_eq!(approved, update());
        assert!(offers.approve(id).is_none());

        assert_eq!(offers.fail(approved), Notice::Failed);
        // No response, the same version, a changed digest, or an older
        // release cannot clear the failure or add another consent action.
        for found in [
            None,
            Some(update()),
            parse_fixture(&fixture_release("v1.9.0", b"changed")),
            parse_fixture(&fixture_release("v1.8.0", b"older")),
        ] {
            assert_eq!(offers.refresh(found), None);
            assert_eq!(offers.notice(), Notice::Failed);
        }
        assert_eq!(offers.reset_offer(), Notice::Failed);
        let newer = || parse_fixture(&fixture_release("v1.10.0", b"newer")).unwrap();
        let Some(Notice::Available(_, new_id)) = offers.refresh(Some(newer())) else {
            panic!("expected a fresh offer for a newer version");
        };
        assert_eq!(offers.reset_offer(), Notice::Failed);
        let Some(Notice::Available(_, reconnected_id)) = offers.refresh(Some(newer())) else {
            panic!("expected fresh consent after reconnecting");
        };
        assert!(offers.approve(id).is_none());
        assert!(offers.approve(new_id).is_none());
        assert_eq!(offers.approve(reconnected_id), Some(newer()));
        // Cancellation consumes the click, but not the release.
        assert_eq!(offers.reset_offer(), Notice::Failed);
        assert!(matches!(
            offers.refresh(Some(newer())),
            Some(Notice::Available(_, _))
        ));
    }

    #[test]
    fn only_selected_statuses_without_retry_after_are_transient() {
        assert_eq!(response_status(StatusCode::OK, false), Ok(()));
        for status in [408, 500, 502, 503, 504] {
            let status = StatusCode::from_u16(status).unwrap();
            assert_eq!(response_status(status, false), Err(ReadFailure::Transient));
            assert_eq!(response_status(status, true), Err(ReadFailure::Permanent));
        }
        for status in [206, 302, 401, 403, 404, 410, 429, 501, 505] {
            let status = StatusCode::from_u16(status).unwrap();
            assert_eq!(response_status(status, false), Err(ReadFailure::Permanent));
        }
    }

    #[tokio::test]
    async fn transient_reads_are_bounded_and_stop_at_success_or_permanent_error() {
        use ReadFailure::{Permanent, Transient};
        let cases = [
            (vec![Ok(())], Ok(()), &READ_BACKOFF[..0]),
            (vec![Err(Transient), Ok(())], Ok(()), &READ_BACKOFF[..1]),
            (vec![Err(Permanent)], Err(Permanent), &READ_BACKOFF[..0]),
            (
                vec![Err(Transient), Err(Permanent)],
                Err(Permanent),
                &READ_BACKOFF[..1],
            ),
            (vec![Err(Transient); 3], Err(Transient), &READ_BACKOFF),
        ];
        for (attempts, result, expected_delays) in cases {
            let mut attempts = attempts.into_iter();
            let mut delays = Vec::new();
            assert_eq!(
                retry_read(
                    Instant::now() + READ_BUDGET,
                    || ready(attempts.next().expect("extra attempt")),
                    |delay| {
                        delays.push(delay);
                        ready(())
                    },
                )
                .await,
                result,
            );
            assert_eq!(attempts.len(), 0);
            assert_eq!(delays, expected_delays);
        }
    }

    #[tokio::test]
    async fn backoff_cannot_start_another_read_at_the_deadline() {
        let attempts = Cell::new(0);
        let deadline = Instant::now() + Duration::from_millis(5);
        let result = retry_read(
            deadline,
            || {
                attempts.set(attempts.get() + 1);
                ready(Err::<(), _>(ReadFailure::Transient))
            },
            |_| tokio::time::sleep_until(deadline),
        )
        .await;
        assert_eq!(result, Err(ReadFailure::Transient));
        // A delayed first poll may use up the budget without starting any read.
        assert!(attempts.get() <= 1);
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
        assert_eq!(update.version, Version([1, 0, 1]));
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
        use ReadFailure::{Permanent, Transient};
        let max = vec![0u8; MAX_METADATA];
        let ok = [Ok(max.as_slice())];
        assert_eq!(
            bounded_metadata(None, stream::iter(ok))
                .await
                .unwrap()
                .len(),
            MAX_METADATA
        );
        let over = [ok[0], Ok(b"x".as_slice())];
        assert_eq!(
            bounded_metadata(None, stream::iter(over)).await,
            Err(Permanent)
        );
        assert_eq!(
            bounded_metadata(Some(MAX_METADATA as u64 + 1), stream::iter(ok)).await,
            Err(Permanent)
        );
        assert_eq!(
            bounded_metadata(None, stream::iter([ok[0], Err(Transient)])).await,
            Err(Transient)
        );
    }

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("spofify-upgrade-{}", Uuid::new_v4()));
            fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
            let dir = Self(path);
            fs::write(dir.target(), b"old image").unwrap();
            dir
        }

        fn target(&self) -> PathBuf {
            self.0.join("spofify-desktop")
        }

        fn assert_clean(&self, image: &[u8]) {
            assert_eq!(fs::read(self.target()).unwrap(), image);
            assert_eq!(fs::read_dir(&self.0).unwrap().count(), 1);
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    // Header only: deliberately not a runnable file.
    fn fixture_elf() -> [u8; ELF_HEADER_LEN] {
        let mut header = [0; ELF_HEADER_LEN];
        header[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
        header[16] = 3;
        header[18] = 62;
        header[20] = 1;
        header[52] = ELF_HEADER_LEN as u8;
        header
    }

    #[test]
    fn executable_header_requires_linux_architecture_and_executable_type() {
        let mut header = fixture_elf();
        assert!(executable_header(&header));
        header[16] = 2;
        assert!(executable_header(&header));
        for (offset, bad) in [(0, 0), (4, 1), (5, 2), (16, 1), (18, 183), (52, 0)] {
            let mut invalid = header;
            invalid[offset] = bad;
            assert!(!executable_header(&invalid));
        }
    }

    #[tokio::test]
    async fn invalid_payloads_and_local_io_failures_are_not_retried() {
        let dir = Fixture::new();
        let target = dir.target();
        let next = fixture_elf();
        let update = parse_fixture(&fixture_release("v1.0.1", &next)).unwrap();
        let non_elf = [0; ELF_HEADER_LEN + 1];
        let non_elf_update = parse_fixture(&fixture_release("v1.0.1", &non_elf)).unwrap();
        let interrupted = stream::iter([
            Ok(&non_elf[..3]),
            Ok(&non_elf[3..ELF_HEADER_LEN]),
            Err(ReadFailure::Transient),
        ]);
        assert_eq!(
            stage_download(None, interrupted, &non_elf_update, &target)
                .await
                .err(),
            Some(ReadFailure::Permanent),
        );
        dir.assert_clean(b"old image");
        let mut bad_digest = next;
        bad_digest[63] = 1;
        let overlong = [&next[..], b"extra"].concat();
        for (payload, length, destination, update) in [
            (&bad_digest[..], None, target.clone(), &update),
            (&non_elf[..], None, target.clone(), &non_elf_update),
            (&next[..3], None, target.clone(), &update),
            (&overlong[..], None, target.clone(), &update),
            (&next[..], Some(1), target.clone(), &update),
            (
                &next[..],
                Some(update.bytes),
                dir.0.join("missing/file"),
                &update,
            ),
        ] {
            let attempts = Cell::new(0);
            let result = retry_read(
                Instant::now() + READ_BUDGET,
                || {
                    attempts.set(attempts.get() + 1);
                    stage_download(
                        length,
                        stream::iter(payload.chunks(3).map(Ok)),
                        update,
                        &destination,
                    )
                },
                |_| -> std::future::Ready<()> { panic!("permanent failure must not wait") },
            )
            .await;
            assert_eq!(result.err(), Some(ReadFailure::Permanent));
            assert_eq!(attempts.get(), 1);
            dir.assert_clean(b"old image");
        }
    }

    #[tokio::test]
    async fn interrupted_download_retries_from_scratch_before_atomic_install() {
        use std::io::Read;
        let dir = Fixture::new();
        let target = dir.target();
        let next = fixture_elf();
        let update = parse_fixture(&fixture_release("v1.0.1", &next)).unwrap();
        let mut opened_before_upgrade = File::open(&target).unwrap();
        let attempts = Cell::new(0);
        let deadline = Instant::now() + READ_BUDGET;
        let (stage, file) = retry_read(
            deadline,
            || {
                let attempt = attempts.get();
                attempts.set(attempt + 1);
                let chunks = if attempt == 0 {
                    [Ok(&next[..3]), Err(ReadFailure::Transient)]
                } else {
                    [Ok(&next[..3]), Ok(&next[3..])]
                };
                stage_download(Some(update.bytes), stream::iter(chunks), &update, &target)
            },
            |delay| {
                assert_eq!(delay, Duration::from_millis(250));
                dir.assert_clean(b"old image");
                ready(())
            },
        )
        .await
        .unwrap();
        assert_eq!(attempts.get(), 2);
        assert_eq!(fs::read(&target).unwrap(), b"old image");
        assert_eq!(file.metadata().unwrap().mode() & 0o077, 0);
        assert_eq!(
            install_staged(stage, file, &target, deadline, || true),
            Some(())
        );
        dir.assert_clean(&next);
        let mut preserved = Vec::new();
        opened_before_upgrade.read_to_end(&mut preserved).unwrap();
        assert_eq!(preserved, b"old image");
        assert_eq!(fs::metadata(&target).unwrap().mode() & 0o777, 0o755);
    }

    #[tokio::test]
    async fn cancel_denial_and_rename_failure_clean_the_stage_without_replacing_image() {
        let dir = Fixture::new();
        let target = dir.target();
        let next = fixture_elf();
        let update = parse_fixture(&fixture_release("v1.0.1", &next)).unwrap();
        {
            let chunks = stream::iter([Ok(&next[..3])]).chain(stream::pending());
            let download = stage_download(None, chunks, &update, &target);
            tokio::pin!(download);
            assert!(futures_util::poll!(&mut download).is_pending());
            assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 2);
        }
        dir.assert_clean(b"old image");
        for (admitted, destination) in [(false, &target), (true, &dir.0)] {
            let chunks = stream::iter([Ok(next)]);
            let (stage, file) = stage_download(None, chunks, &update, &target)
                .await
                .unwrap();
            // The final guard may deny the install. A rename to the fixture's
            // directory must also fail without disturbing the installed file.
            let deadline = Instant::now() + READ_BUDGET;
            assert_eq!(
                install_staged(stage, file, destination, deadline, || admitted),
                None
            );
            dir.assert_clean(b"old image");
        }
    }
    #[tokio::test]
    async fn publication_fences_stale_epoch_network_and_deadline_under_the_gate() {
        let dir = Fixture::new();
        let target = dir.target();
        let next = fixture_elf();
        let update = parse_fixture(&fixture_release("v1.0.1", &next)).unwrap();
        let now = Instant::now();
        for (actual_epoch, actual_generation, deadline) in [
            (2, 10, now + READ_BUDGET), // logout before Access is republished
            (1, 11, now + READ_BUDGET), // offline
            (1, 12, now + READ_BUDGET), // back online, but different generation
            (1, 10, now),               // expired despite an otherwise live session
        ] {
            let epoch = AtomicU64::new(actual_epoch);
            let (_changes, network) = watch::channel(actual_generation);
            let chunks = stream::iter([Ok(next)]);
            let (stage, file) = stage_download(None, chunks, &update, &target)
                .await
                .unwrap();
            assert_eq!(
                install_staged(stage, file, &target, deadline, || {
                    assert!(matches!(
                        PUBLICATION_GATE.try_lock(),
                        Err(std::sync::TryLockError::WouldBlock)
                    ));
                    current_connection(&epoch, 1, &network, 10)
                }),
                None
            );
            dir.assert_clean(b"old image");
        }
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
