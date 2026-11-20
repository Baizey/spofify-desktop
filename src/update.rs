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
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::sync::watch;

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
#[derive(Deserialize)]
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
        let path = parent.join(format!(".spofify-update-{}", uuid::Uuid::new_v4()));
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
    channel: &Channel,
    target: &Path,
    online: impl Fn() -> bool,
) -> Option<()> {
    if !online() || !current_image(target).ok()? {
        return Some(());
    }
    let parent = target.parent()?;
    let lock = File::open(parent).ok()?;
    if flock(&lock, FlockOperation::NonBlockingLockExclusive).is_err()
        || !current_image(target).ok()?
    {
        return Some(());
    }
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
    let Some(manifest) = channel.manifest(&body) else {
        return Some(());
    };
    let response = fetch(
        http,
        channel.url(&format!("download/v{}/{BINARY_ASSET}", manifest.version)),
    )
    .await?;
    save_update(response, &manifest, target, || {
        online() && current_image(target).ok() == Some(true)
    })
    .await?;
    let _ = lock.sync_all();
    Some(())
}

async fn save_update(
    mut response: Response,
    manifest: &Manifest,
    target: &Path,
    may_replace: impl FnOnce() -> bool,
) -> Option<()> {
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
    if may_replace() {
        fs::rename(&stage.0, target).ok()?;
    }
    Some(())
}

pub fn start(mut access: watch::Receiver<Option<Access>>) {
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
    let _ = std::thread::Builder::new().name("spofify-updates".into()).spawn(move || {
        let Ok(runtime) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return };
        runtime.block_on(async move {
            let Ok(http) = client() else { return };
            loop {
                if access.wait_for(|a| a.as_ref().is_some_and(|a| !a.offline && !a.session.is_offline())).await.is_err() { return; }
                let Some(session) = access.borrow().as_ref().map(|a| a.session.clone()) else { continue };
                let mut mode = session.network_changes();
                let generation = *mode.borrow_and_update();
                if generation & 1 != 0 || session.is_offline() { continue; }
                let current = session.network_changes();
                let online = || *current.borrow() == generation && session.can_access_local();
                let interval = tokio::select! {
                    biased;
                    _ = mode.changed() => continue,
                    _ = access.changed() => continue,
                    result = check(&http, &channel, &target, online) => if result.is_some() { CHECK_INTERVAL } else { RETRY_INTERVAL },
                };
                // In-place rename made this instance old. Only the new image may update again.
                if current_image(&target).ok() != Some(true) { return; }
                tokio::time::sleep(interval).await;
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;
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
                save_update(response, &manifest, &target, || admitted)
                    .await
                    .is_some(),
                succeeds
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
