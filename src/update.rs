//! User-local Linux releases verified against GitHub release metadata.
//! Installing a new image never restarts the player.
use crate::{
    api_transport::{is_transient_error, is_transient_status},
    mode::UpdateNetwork,
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
    fs::{self, File, OpenOptions},
    future::Future,
    io::{self, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::mpsc,
    thread,
    time::Duration,
};
use tokio::{
    sync::watch,
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
fn current_image(path: &Path, running: &fs::Metadata) -> io::Result<bool> {
    let parent = fs::symlink_metadata(path.parent().ok_or(io::ErrorKind::InvalidInput)?)?;
    let installed = fs::symlink_metadata(path)?;
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

// The scheduler and installer work with replayable reads. Production has only
// these anonymous HTTPS transports; the tests supply private byte streams.
trait ReleaseSource {
    async fn metadata(&self) -> Result<Vec<u8>, ReadFailure>;
    async fn stage(&self, update: &Update, target: &Path) -> Result<(Staged, File), ReadFailure>;
}

struct GitHub {
    api: Client,
    assets: Client,
}

impl GitHub {
    fn new() -> Option<Self> {
        Some(Self {
            api: client().redirect(Policy::none()).build().ok()?,
            assets: asset_client().ok()?,
        })
    }
}

impl ReleaseSource for GitHub {
    async fn metadata(&self) -> Result<Vec<u8>, ReadFailure> {
        let response = fetch(release_request(&self.api)).await?;
        bounded_metadata(
            response.content_length(),
            response
                .bytes_stream()
                .map(|chunk| chunk.map_err(Into::into)),
        )
        .await
    }

    async fn stage(&self, update: &Update, target: &Path) -> Result<(Staged, File), ReadFailure> {
        let response = fetch(self.assets.get(&update.url)).await?;
        stage_download(
            response.content_length(),
            response
                .bytes_stream()
                .map(|chunk| chunk.map_err(Into::into)),
            update,
            target,
        )
        .await
    }
}

async fn check(
    source: &impl ReleaseSource,
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
            source.metadata().await
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
    source: &impl ReleaseSource,
    update: &Update,
    target: &Path,
    running_image: &fs::Metadata,
    online: impl Fn() -> bool,
) -> Result<(), ReadFailure> {
    use ReadFailure::{Permanent, Transient};
    let deadline = Instant::now() + READ_BUDGET;
    let eligible = || online() && current_image(target, running_image).ok() == Some(true);
    if !eligible() {
        return Err(Permanent);
    }
    // Use the same interprocess directory lock as the managed installer. An
    // older process may have passed the initial check before a competing
    // updater ran; recheck the running inode under the lock and before rename.
    let lock = File::open(target.parent().ok_or(Permanent)?).map_err(|_| Permanent)?;
    flock(&lock, FlockOperation::NonBlockingLockExclusive).map_err(|_| Transient)?;
    let (stage, file) = retry_read(
        deadline,
        || async {
            if !eligible() {
                return Err(Permanent);
            }
            source.stage(update, target).await
        },
        tokio::time::sleep,
    )
    .await?;
    install_staged(stage, file, target, deadline, eligible).ok_or(Permanent)?;
    let _ = lock.sync_all();
    Ok(())
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
    // Serialize final admission and rename with explicit network-mode changes.
    // A blocking sync or a wait for this gate must not extend the deadline.
    let _publication = PUBLICATION_GATE.lock().ok()?;
    if !may_replace() || Instant::now() >= deadline {
        return None;
    }
    fs::rename(&stage.0, target).ok()
}

const READY_NOTICE: &str = "Update ready. Restart Spofify to use it.";

pub fn start(
    ui: &crate::AppWindow,
    network: watch::Receiver<UpdateNetwork>,
) -> Option<mpsc::Receiver<()>> {
    if cfg!(debug_assertions) {
        return None;
    }
    let running = Version::parse(env!("CARGO_PKG_VERSION"))?;
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())?;
    let target = home.join(".local/lib/spofify-desktop/spofify-desktop");
    let weak = ui.as_weak();
    let (lifetime, stopped) = mpsc::channel();
    thread::Builder::new()
        .name("spofify-updates".into())
        .spawn(move || {
            let _lifetime = lifetime;
            let Ok(running_image) = fs::metadata("/proc/self/exe") else {
                return;
            };
            if current_image(&target, &running_image).ok() != Some(true) {
                return;
            }
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            runtime.block_on(run_updates(
                running,
                &target,
                &running_image,
                network,
                GitHub::new,
                || {
                    let _ =
                        weak.upgrade_in_event_loop(|ui| ui.set_update_notice(READY_NOTICE.into()));
                },
            ));
            // Do not keep a closing app waiting on a cancelled DNS lookup.
            runtime.shutdown_background();
        })
        .ok()?;
    Some(stopped)
}

#[derive(Debug, PartialEq, Eq)]
enum Cycle {
    Stale,
    Unchanged,
    Installed,
    TransientFailure,
    Rejected(Version),
}

async fn update_once(
    source: &impl ReleaseSource,
    running: Version,
    target: &Path,
    running_image: &fs::Metadata,
    rejected: Option<Version>,
    online: impl Fn() -> bool,
) -> Cycle {
    if current_image(target, running_image).ok() != Some(true) {
        return Cycle::Stale;
    }
    match check(source, running, &online).await {
        Ok(Some(update)) if rejected.is_none_or(|version| update.version > version) => {
            match download(source, &update, target, running_image, online).await {
                Ok(()) => Cycle::Installed,
                Err(ReadFailure::Transient) => Cycle::TransientFailure,
                Err(ReadFailure::Permanent) => Cycle::Rejected(update.version),
            }
        }
        Err(ReadFailure::Transient) => Cycle::TransientFailure,
        _ => Cycle::Unchanged,
    }
}

async fn run_updates<T: ReleaseSource>(
    running: Version,
    target: &Path,
    running_image: &fs::Metadata,
    mut network: watch::Receiver<UpdateNetwork>,
    build_source: impl FnOnce() -> Option<T>,
    show_ready: impl FnOnce(),
) {
    let mut build_source = Some(build_source);
    let mut source = None;
    let mut rejected = None;
    let mut next_check = Instant::now();
    loop {
        match network
            .wait_for(|state| matches!(state, UpdateNetwork::Online | UpdateNetwork::Closed))
            .await
        {
            Ok(state) if *state == UpdateNetwork::Online => (),
            _ => return,
        }
        // wait_for marked the online version seen. Even an offline→online
        // round trip invalidates all work admitted by that old version.
        let current = network.clone();
        let online = || matches!(current.has_changed(), Ok(false));
        if !online() {
            continue;
        }
        if source.is_none() {
            source = build_source.take().and_then(|build| build());
        }
        let Some(source) = &source else { return };
        let outcome = tokio::select! {
            biased;
            _ = network.changed() => continue,
            outcome = async {
                tokio::time::sleep_until(next_check).await;
                update_once(source, running, target, running_image, rejected, online).await
            } => outcome,
        };
        match outcome {
            Cycle::Installed => {
                // Installation won its publication race. Do not hide ready
                // on a later offline change or sign-out.
                show_ready();
                return;
            }
            Cycle::Stale => return,
            _ if !online() => continue,
            Cycle::Rejected(version) => rejected = Some(version),
            _ => (),
        }
        // Neither a transient retry budget nor the periodic cadence is reset
        // by account activity or by toggling mode after a completed check.
        next_check = Instant::now()
            + if outcome == Cycle::TransientFailure {
                CHECK_FAILURE_INTERVAL
            } else {
                CHECK_INTERVAL
            };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mode::ModeControl;
    use futures_util::stream;
    use serde_json::{Value, json};
    use std::{cell::Cell, future::ready, os::unix::fs::DirBuilderExt, sync::atomic::AtomicU64};

    const EPOCH: u64 = 1;

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

    // A private standalone ELF fixture: the only thing it does is exit with
    // the supplied code. Never runs the installed Desktop or loads app state.
    fn runnable_fixture(code: u8) -> Vec<u8> {
        const BASE: u64 = 0x0040_0000;
        const PH_SIZE: u64 = 56;
        let entry = ELF_HEADER_LEN as u64 + PH_SIZE;
        let image_len = entry + 12;
        let mut elf = fixture_elf().to_vec();
        elf.resize(entry as usize, 0);
        elf[16] = 2; // ET_EXEC
        elf[24..32].copy_from_slice(&(BASE + entry).to_le_bytes());
        elf[32..40].copy_from_slice(&(ELF_HEADER_LEN as u64).to_le_bytes());
        elf[54..58].copy_from_slice(&[PH_SIZE as u8, 0, 1, 0]);
        elf[64..72].copy_from_slice(&[1, 0, 0, 0, 5, 0, 0, 0]); // PT_LOAD, R+X
        elf[80..88].copy_from_slice(&BASE.to_le_bytes());
        elf[88..96].copy_from_slice(&BASE.to_le_bytes());
        elf[96..104].copy_from_slice(&image_len.to_le_bytes());
        elf[104..112].copy_from_slice(&image_len.to_le_bytes());
        elf[112..120].copy_from_slice(&4096u64.to_le_bytes());
        elf.extend([0xb8, 60, 0, 0, 0, 0xbf, code, 0, 0, 0, 0x0f, 0x05]);
        elf
    }

    #[derive(Clone, Copy)]
    enum Body {
        Complete,
        WrongDigest,
        Interrupted,
        Pending,
    }

    struct FixtureSource<'a> {
        payload: &'a [u8],
        body: Cell<Body>,
        checks: Cell<usize>,
        downloads: Cell<usize>,
        stage_polled: tokio::sync::Notify,
    }

    impl<'a> FixtureSource<'a> {
        fn new(payload: &'a [u8], body: Body) -> Self {
            Self {
                payload,
                body: Cell::new(body),
                checks: Cell::new(0),
                downloads: Cell::new(0),
                stage_polled: Default::default(),
            }
        }
    }

    impl ReleaseSource for &FixtureSource<'_> {
        async fn metadata(&self) -> Result<Vec<u8>, ReadFailure> {
            self.checks.set(self.checks.get() + 1);
            Ok(serde_json::to_vec(&fixture_release("v1.0.1", self.payload)).unwrap())
        }

        async fn stage(
            &self,
            update: &Update,
            target: &Path,
        ) -> Result<(Staged, File), ReadFailure> {
            let attempt = self.downloads.get() + 1;
            self.downloads.set(attempt);
            let body = self.body.get();
            let mut data = self.payload.to_vec();
            if matches!(body, Body::WrongDigest) {
                *data.last_mut().unwrap() ^= 1;
            }
            let result = match body {
                Body::Pending => {
                    let tail = stream::poll_fn(|_| {
                        self.stage_polled.notify_one();
                        std::task::Poll::Pending
                    });
                    stage_download(
                        None,
                        stream::iter([Ok(&data[..3])]).chain(tail),
                        update,
                        target,
                    )
                    .await
                }
                Body::Interrupted => {
                    stage_download(
                        None,
                        stream::iter([Ok(&data[..3]), Err(ReadFailure::Transient)]),
                        update,
                        target,
                    )
                    .await
                }
                _ => stage_download(None, stream::iter([Ok(data)]), update, target).await,
            };
            if !matches!(body, Body::Interrupted) || attempt == READ_BACKOFF.len() + 1 {
                self.stage_polled.notify_one();
            }
            result
        }
    }

    #[tokio::test]
    async fn startup_waits_for_policy_then_updates_without_sign_in_or_restarting() {
        let dir = Fixture::new();
        let target = dir.target();
        let previous = runnable_fixture(21);
        fs::write(&target, &previous).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
        let manual_reopen = || std::process::Command::new(&target).status().unwrap().code();
        assert_eq!(manual_reopen(), Some(21));
        let running_image = fs::metadata(&target).unwrap();
        let mode = ModeControl::default();
        let next = runnable_fixture(32);
        let source = FixtureSource::new(&next, Body::Complete);
        let ready_count = Cell::new(0);
        let built = Cell::new(false);
        let updater = run_updates(
            Version([1, 0, 0]),
            &target,
            &running_image,
            mode.update_network(),
            || {
                built.set(true);
                Some(&source)
            },
            || ready_count.set(ready_count.get() + 1),
        );
        tokio::pin!(updater);
        assert!(futures_util::poll!(&mut updater).is_pending());
        assert!(!built.get()); // Preferences have not loaded; no transport exists yet.
        dir.assert_clean(&previous);
        assert_eq!(ready_count.get(), 0);

        mode.resolve_update_startup(EPOCH, &AtomicU64::new(EPOCH)); // No saved credentials or account exists.
        tokio::time::timeout(Duration::from_secs(2), &mut updater)
            .await
            .unwrap();
        assert!(built.get());
        assert_eq!((source.checks.get(), source.downloads.get()), (1, 1));
        assert_eq!(ready_count.get(), 1);
        dir.assert_clean(&next);
        assert!(!current_image(&target, &running_image).unwrap());
        mode.block_updates();
        assert_eq!(ready_count.get(), 1); // Subsequent mode changes cannot clear verified ready.
        assert_eq!(manual_reopen(), Some(32)); // Only this explicit fixture reopen runs v2.
    }

    #[tokio::test]
    async fn cold_offline_and_unresolved_startup_never_build_transport() {
        let dir = Fixture::new();
        let target = dir.target();
        let running_image = fs::metadata(&target).unwrap();
        let mode = ModeControl::default();
        let updater = run_updates::<&FixtureSource<'_>>(
            Version([1, 0, 0]),
            &target,
            &running_image,
            mode.update_network(),
            || panic!("constructed transport without online permission"),
            || panic!("unexpected notice"),
        );
        tokio::pin!(updater);
        assert!(futures_util::poll!(&mut updater).is_pending());
        mode.block_updates(); // Admission of the loaded cold grant.
        mode.resolve_update_startup(EPOCH, &AtomicU64::new(EPOCH)); // Cannot supersede Work offline.
        assert!(futures_util::poll!(&mut updater).is_pending());
        mode.close_updates();
        updater.await;
        dir.assert_clean(b"old image");
    }

    #[tokio::test]
    async fn runtime_offline_cancels_a_partial_download_and_explicit_online_can_resume() {
        let dir = Fixture::new();
        let target = dir.target();
        let running_image = fs::metadata(&target).unwrap();
        let next = fixture_elf();
        let source = FixtureSource::new(&next, Body::Pending);
        let mode = ModeControl::default();
        mode.resolve_update_startup(EPOCH, &AtomicU64::new(EPOCH));
        let ready = Cell::new(false);
        let updater = run_updates(
            Version([1, 0, 0]),
            &target,
            &running_image,
            mode.update_network(),
            || Some(&source),
            || ready.set(true),
        );
        tokio::pin!(updater);
        tokio::select! {
            _ = &mut updater => panic!("returned while a partial asset was still pending"),
            _ = source.stage_polled.notified() => (),
            _ = tokio::time::sleep(Duration::from_secs(2)) => panic!("download did not start"),
        }
        assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 2);
        mode.block_updates();
        assert!(futures_util::poll!(&mut updater).is_pending());
        dir.assert_clean(b"old image");
        assert!(!ready.get());
        assert_eq!((source.checks.get(), source.downloads.get()), (1, 1));
        source.body.set(Body::Complete);
        mode.allow_updates(EPOCH, &AtomicU64::new(EPOCH));
        tokio::time::timeout(Duration::from_secs(2), &mut updater)
            .await
            .unwrap();
        assert_eq!((source.checks.get(), source.downloads.get()), (2, 2));
        assert!(ready.get());
        dir.assert_clean(&next);
    }

    #[tokio::test]
    async fn closing_cancels_an_active_download_and_cleans_the_partial_stage() {
        let dir = Fixture::new();
        let target = dir.target();
        let running_image = fs::metadata(&target).unwrap();
        let next = fixture_elf();
        let source = FixtureSource::new(&next, Body::Pending);
        let mode = ModeControl::default();
        mode.resolve_update_startup(EPOCH, &AtomicU64::new(EPOCH));
        let updater = run_updates(
            Version([1, 0, 0]),
            &target,
            &running_image,
            mode.update_network(),
            || Some(&source),
            || panic!("partial stage appeared ready"),
        );
        tokio::pin!(updater);
        tokio::select! {
            _ = &mut updater => panic!("returned while a partial asset was still pending"),
            _ = source.stage_polled.notified() => (),
            _ = tokio::time::sleep(Duration::from_secs(2)) => panic!("download did not start"),
        }
        assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 2);
        mode.close_updates();
        tokio::time::timeout(Duration::from_secs(2), &mut updater)
            .await
            .unwrap();
        dir.assert_clean(b"old image");
    }

    #[tokio::test]
    async fn failed_checksum_or_transport_never_publishes_a_notice() {
        let dir = Fixture::new();
        let target = dir.target();
        let running_image = fs::metadata(&target).unwrap();
        let next = fixture_elf();
        for (body, tries) in [(Body::WrongDigest, 1), (Body::Interrupted, 3)] {
            let source = FixtureSource::new(&next, body);
            let mode = ModeControl::default();
            mode.resolve_update_startup(EPOCH, &AtomicU64::new(EPOCH));
            let updater = run_updates(
                Version([1, 0, 0]),
                &target,
                &running_image,
                mode.update_network(),
                || Some(&source),
                || panic!("failed update appeared ready"),
            );
            tokio::pin!(updater);
            tokio::select! {
                _ = &mut updater => panic!("returned instead of waiting for next check"),
                _ = source.stage_polled.notified() => (),
                _ = tokio::time::sleep(Duration::from_secs(4)) => panic!("attempts did not finish"),
            }
            assert!(futures_util::poll!(&mut updater).is_pending());
            assert_eq!((source.checks.get(), source.downloads.get()), (1, tries));
            // A toggle after a completed failure must preserve the next check
            // deadline, not immediately retry the same release.
            mode.block_updates();
            mode.allow_updates(EPOCH, &AtomicU64::new(EPOCH));
            assert!(futures_util::poll!(&mut updater).is_pending());
            assert_eq!((source.checks.get(), source.downloads.get()), (1, tries));
            dir.assert_clean(b"old image");
            mode.close_updates();
            updater.await;
        }
    }

    #[tokio::test]
    async fn competing_lock_and_stale_running_inode_prevent_replacement() {
        let dir = Fixture::new();
        let target = dir.target();
        let running_image = fs::metadata(&target).unwrap();
        let next = fixture_elf();
        let fixture_source = FixtureSource::new(&next, Body::Complete);
        let source = &fixture_source;
        let run = || {
            update_once(
                &source,
                Version([1, 0, 0]),
                &target,
                &running_image,
                None,
                || true,
            )
        };
        let lock = File::open(&dir.0).unwrap();
        flock(&lock, FlockOperation::NonBlockingLockExclusive).unwrap();
        assert_eq!(run().await, Cycle::TransientFailure);
        assert_eq!((source.checks.get(), source.downloads.get()), (1, 0));
        dir.assert_clean(b"old image");
        drop(lock);
        assert_eq!(run().await, Cycle::Installed);
        assert_eq!(run().await, Cycle::Stale);
        assert_eq!((source.checks.get(), source.downloads.get()), (2, 1));
        dir.assert_clean(&next);
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
    async fn publication_fences_offline_round_trips_and_deadline_under_the_gate() {
        let dir = Fixture::new();
        let target = dir.target();
        let next = fixture_elf();
        let update = parse_fixture(&fixture_release("v1.0.1", &next)).unwrap();
        let now = Instant::now();
        for (offline, resume, deadline) in [
            (true, false, now + READ_BUDGET),
            (true, true, now + READ_BUDGET),
            (false, false, now),
        ] {
            let mode = ModeControl::default();
            mode.resolve_update_startup(EPOCH, &AtomicU64::new(EPOCH));
            let network = mode.update_network();
            let chunks = stream::iter([Ok(next)]);
            let (stage, file) = stage_download(None, chunks, &update, &target)
                .await
                .unwrap();
            if offline {
                mode.block_updates();
            }
            if resume {
                mode.allow_updates(EPOCH, &AtomicU64::new(EPOCH));
            }
            assert_eq!(
                install_staged(stage, file, &target, deadline, || {
                    assert!(matches!(
                        PUBLICATION_GATE.try_lock(),
                        Err(std::sync::TryLockError::WouldBlock)
                    ));
                    matches!(network.has_changed(), Ok(false))
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
