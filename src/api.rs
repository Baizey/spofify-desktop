//! Bounded, in-memory native transport for authenticated library and playback APIs.
//! No arbitrary request URLs, cookie jar, persistence, redirects, proxies or retries.

use std::{
    collections::HashSet,
    fmt,
    future::Future,
    net::IpAddr,
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};

use cookie::{Cookie, Expiration, time::OffsetDateTime};
use reqwest::{
    Client, RequestBuilder, Response,
    header::{self, HeaderValue},
};
use serde::Deserialize;
use tokio::sync::watch;
use url::Url;

#[path = "api_auth.rs"]
mod api_auth;
pub use api_auth::RememberedCredential;
#[path = "catalog.rs"]
pub mod catalog;
#[path = "library.rs"]
pub mod library;
#[path = "preferences.rs"]
pub(crate) mod preferences;
#[path = "reconnect.rs"]
mod reconnect;

pub const PAGE_SIZE: u32 = 20;
#[cfg(test)]
const MAX_BODY: usize = 256 * 1024;
const LOGIN_BODY: usize = 8 * 1024;
const MAX_MEDIA: usize = 32 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const SOCKET_TIMEOUT: Duration = Duration::from_secs(4);
const TICKET_BODY: usize = 1024;
const OFFLINE_MEDIA_BODY: usize = 16 * 1024;
const OFFLINE_COLLECTION_BODY: usize = 2 * 1024 * 1024;
const OFFLINE_AUDIO_TYPES: [&str; 8] = [
    "audio/mpeg",
    "audio/flac",
    "audio/mp4",
    "audio/aac",
    "audio/ogg",
    "audio/wav",
    "audio/aiff",
    "audio/webm",
];

/// All messages are local constants. Never retains a transport error, URL or server body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ApiError {
    pub message: &'static str,
    pub expired: bool,
    /// HTTP status, or zero for local validation/transport failures.
    pub status: u16,
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message)
    }
}

impl std::error::Error for ApiError {}

const INVALID_ORIGIN: ApiError =
    error("Enter an HTTP or HTTPS server address without a path, credentials, query, or fragment.");
const INVALID_RESPONSE: ApiError = error("The server returned an incompatible API response.");
const INVALID_INPUT: ApiError = error("The request contains invalid or oversized input.");
const NETWORK: ApiError =
    error("Unable to reach the server. Check your connection and server address.");
const OFFLINE: ApiError = error("This session is offline.");
const EXPIRED: ApiError = ApiError {
    message: "Your session has expired. Sign in again.",
    expired: true,
    status: 401,
};
// Rolling credentials can be revalidated after the local deadline. This is not
// evidence of server revocation and must not be used to delete the saved cookie.
const ROLLING_DEADLINE: ApiError = ApiError {
    status: 0,
    ..EXPIRED
};
const STALE_DENIAL: ApiError = error("The session changed while the request was in progress.");

const fn error(message: &'static str) -> ApiError {
    ApiError {
        message,
        expired: false,
        status: 0,
    }
}

/// Validated exact canonical origin. Validation never resolves DNS.
#[derive(Clone, PartialEq, Eq)]
pub struct Origin(Url);

impl Origin {
    pub fn parse(input: &str) -> Result<Self, ApiError> {
        let input = input.trim();
        if input.is_empty()
            || input.len() > 2048
            || input.chars().any(|c| c.is_whitespace() || c.is_control())
            || input.contains(['\\', '@', '?', '#', '%'])
        {
            return Err(INVALID_ORIGIN);
        }
        // Inspect the raw path/authority BEFORE URL normalization. Otherwise a dot path,
        // numeric host alias, or backslash can silently become a different origin.
        let (scheme, rest) = input.split_once("://").ok_or(INVALID_ORIGIN)?;
        if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
            return Err(INVALID_ORIGIN);
        }
        let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
        if !path.is_empty() || authority.is_empty() {
            return Err(INVALID_ORIGIN);
        }
        let raw_host = if let Some(bracketed) = authority.strip_prefix('[') {
            let (host, suffix) = bracketed.split_once(']').ok_or(INVALID_ORIGIN)?;
            if !suffix.is_empty() && !valid_port_suffix(suffix) {
                return Err(INVALID_ORIGIN);
            }
            host
        } else {
            let (host, suffix) = authority.split_once(':').unwrap_or((authority, ""));
            if authority.contains(':') && !valid_port_suffix(&format!(":{suffix}")) {
                return Err(INVALID_ORIGIN);
            }
            host
        };
        let url = Url::parse(input).map_err(|_| INVALID_ORIGIN)?;
        if !url.username().is_empty() || url.password().is_some() || url.host_str().is_none() {
            return Err(INVALID_ORIGIN);
        }
        // Reject noncanonical numeric spellings even though WHATWG accepts forms such
        // as 127.1, 0177.0.0.1, 2130706433, and 0x7f000001.
        let numeric = match url.host() {
            Some(url::Host::Ipv4(ip)) => Some(IpAddr::V4(ip)),
            Some(url::Host::Ipv6(ip)) => Some(IpAddr::V6(ip)),
            _ => None,
        };
        if let Some(ip) = numeric {
            if raw_host != ip.to_string() || ip.is_unspecified() {
                return Err(INVALID_ORIGIN);
            }
        }
        Ok(Self(url))
    }

    /// No trailing slash; default ports are omitted and DNS names are canonicalized.
    pub fn as_str(&self) -> &str {
        self.0.as_str().strip_suffix('/').unwrap_or(self.0.as_str())
    }

    fn endpoint(&self, path: &str) -> Url {
        let mut url = self.0.clone();
        url.set_path(path);
        url
    }

    // Dynamic segments must be validated by the fixed-route callers before they reach this
    // helper (in particular, the URL library normalizes dot segments rather than encoding them).
    fn endpoint_segments(&self, segments: &[&str]) -> Result<Url, ApiError> {
        let mut url = self.0.clone();
        url.path_segments_mut()
            .map_err(|_| INVALID_INPUT)?
            .clear()
            .extend(segments.iter().copied());
        Ok(url)
    }
}

fn valid_port_suffix(suffix: &str) -> bool {
    suffix.strip_prefix(':').is_some_and(|port| {
        !port.is_empty()
            && port.bytes().all(|c| c.is_ascii_digit())
            && port.parse::<u16>().is_ok_and(|p| p != 0)
    })
}

/// Safe summary plus opaque credentials. Intentionally neither Debug nor Serialize.
/// Clones share revocation: a 401 or logout makes all clones unusable.
#[derive(Clone)]
pub struct Session {
    pub account_id: String,
    pub username: String,
    pub role: String,
    secrets: Arc<SessionSecrets>,
}

impl Session {
    /// Whether the network credential is past its deadline or has been revoked.
    /// Local playback and storage must use `can_access_local` instead.
    pub fn is_expired(&self) -> bool {
        self.lifetime().check_live().is_err()
    }

    /// An explicitly offline session may access its account's local data past the
    /// cookie deadline. A server 401 or local logout still ends all access.
    pub fn can_access_local(&self) -> bool {
        let offline = self.is_offline();
        let lifetime = self.lifetime();
        lifetime.revocation.is_none() && (offline || !lifetime.deadline.elapsed())
    }

    /// A current HTTP 401, not local expiry or an explicit local logout.
    #[cfg(test)]
    pub(crate) fn was_server_revoked(&self) -> bool {
        let lifetime = self.lifetime();
        lifetime.deadline.server_expires.is_some() && lifetime.revocation == Some(EXPIRED)
    }

    fn lifetime(&self) -> MutexGuard<'_, SessionLifetime> {
        self.secrets.lifetime.lock().unwrap()
    }

    /// A session capability and every clone share the same explicit network mode.
    pub fn set_offline(&self, offline: bool) {
        // Serialize mode changes with the final pin rename, not its disk sync.
        let _publication = crate::playlist_pin_store::PUBLICATION_GATE.lock().unwrap();
        self.secrets.network.send_if_modified(|generation| {
            if (*generation & 1 != 0) == offline {
                return false;
            }
            *generation = generation.wrapping_add(1);
            true
        });
    }

    pub fn is_offline(&self) -> bool {
        self.network_generation() & 1 != 0
    }

    fn network_generation(&self) -> u64 {
        *self.secrets.network.borrow()
    }

    /// Any change fences an already-open socket, even if the current mode is online again.
    /// Odd generations are offline; repeated calls with the same mode do not advance it.
    pub(crate) fn network_changes(&self) -> watch::Receiver<u64> {
        self.secrets.network.subscribe()
    }
}

struct SessionSecrets {
    owner: Arc<()>,
    account_id: String,
    network: watch::Sender<u64>,
    cookie: HeaderValue,
    csrf: HeaderValue,
    lifetime: Mutex<SessionLifetime>,
}

#[derive(Clone, Copy)]
struct CredentialDeadline {
    instant: Instant,
    // The absolute server expiry is also the rolling credential version; two
    // publications of the same expiry must not rebase the monotonic deadline.
    server_expires: Option<OffsetDateTime>,
}

impl CredentialDeadline {
    fn elapsed(self) -> bool {
        Instant::now() >= self.instant
    }

    fn local_expiry(self) -> ApiError {
        if self.server_expires.is_some() {
            ROLLING_DEADLINE
        } else {
            EXPIRED
        }
    }
}

struct SessionLifetime {
    deadline: CredentialDeadline,
    revocation: Option<ApiError>,
}

impl SessionLifetime {
    fn check_live(&self) -> Result<(), ApiError> {
        self.check_revocation()?;
        if self.deadline.elapsed() {
            return Err(self.deadline.local_expiry());
        }
        Ok(())
    }

    fn check_revocation(&self) -> Result<(), ApiError> {
        self.revocation.map_or(Ok(()), Err)
    }

    fn apply_denial(&mut self, used: CredentialDeadline, error: ApiError) -> ApiError {
        if let Some(revocation) = self.revocation {
            return revocation;
        }
        if used.server_expires != self.deadline.server_expires {
            return STALE_DENIAL;
        }
        // Report a mismatch to the probe that found it, but subsequent uses
        // see an expired capability. An account mismatch is not an HTTP 401.
        self.revocation = Some(if error.status == 401 {
            EXPIRED
        } else {
            self.deadline.local_expiry()
        });
        error
    }

    fn extend(&mut self, candidate: CredentialDeadline) {
        debug_assert!(self.revocation.is_none());
        if self.deadline.server_expires.is_some()
            && candidate.server_expires > self.deadline.server_expires
            && candidate.instant > self.deadline.instant
            && !candidate.elapsed()
        {
            self.deadline = candidate;
        }
    }
}

struct NetworkFence {
    changes: watch::Receiver<u64>,
    generation: u64,
}

impl NetworkFence {
    fn new(session: &Session) -> Result<Self, ApiError> {
        let mut changes = session.network_changes();
        let generation = *changes.borrow_and_update();
        if generation & 1 != 0 {
            return Err(OFFLINE);
        }
        Ok(Self {
            changes,
            generation,
        })
    }

    async fn run<T>(
        mut self,
        operation: impl Future<Output = Result<T, ApiError>>,
    ) -> Result<T, ApiError> {
        tokio::select! {
            biased;
            _ = self.changes.changed() => Err(OFFLINE),
            result = operation => {
                if *self.changes.borrow() == self.generation {
                    result
                } else {
                    Err(OFFLINE)
                }
            }
        }
    }
}

#[cfg(test)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Page {
    pub items: Vec<Item>,
    pub total: u32,
    pub offset: u32,
    pub limit: u32,
}

/// Presentation metadata only. Private library paths and other server internals are discarded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub artists: String,
    pub album_name: String,
    pub duration_ms: u64,
    pub has_audio: bool,
    pub track_number: u32,
    pub disc_number: u32,
}

pub struct Api {
    origin: Origin,
    client: Client,
    identity: Arc<()>,
}

impl Api {
    pub fn new(origin: Origin) -> Result<Self, ApiError> {
        let client = Client::builder()
            .use_rustls_tls()
            .https_only(origin.0.scheme() == "https")
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .no_proxy()
            .http1_only()
            .read_timeout(Duration::from_secs(5))
            .connect_timeout(Duration::from_secs(5))
            .timeout(REQUEST_TIMEOUT)
            // Disable reuse too: hyper can otherwise replay a request rejected by a
            // stale pooled HTTP/1 connection below reqwest's retry-policy layer.
            .pool_max_idle_per_host(0)
            .build()
            .map_err(|_| error("Unable to initialize secure networking."))?;
        Ok(Self {
            origin,
            client,
            identity: Arc::new(()),
        })
    }

    #[cfg(test)]
    pub(crate) fn test_session(&self) -> Session {
        self.test_session_with_deadline(Instant::now() + Duration::from_secs(60))
    }

    #[cfg(test)]
    pub(crate) fn test_session_with_deadline(&self, deadline: Instant) -> Session {
        Session {
            account_id: "synthetic".into(),
            username: "synthetic".into(),
            role: "admin".into(),
            secrets: Arc::new(SessionSecrets {
                owner: self.identity.clone(),
                account_id: "synthetic".into(),
                network: watch::Sender::new(0),
                cookie: HeaderValue::from_static("spofify_session=synthetic"),
                csrf: HeaderValue::from_static("synthetic"),
                lifetime: Mutex::new(SessionLifetime {
                    deadline: CredentialDeadline {
                        instant: deadline,
                        server_expires: None,
                    },
                    revocation: None,
                }),
            }),
        }
    }

    #[cfg(test)]
    /// Bounded, explicit track search only. Empty input never contacts the server.
    pub async fn search(&self, session: &Session, query: &str) -> Result<Page, ApiError> {
        if query.len() > 1000 || query.chars().count() > 200 || query.chars().any(char::is_control)
        {
            return Err(INVALID_INPUT);
        }
        let query = query.trim();
        if query.is_empty() {
            return Ok(Page {
                items: Vec::new(),
                total: 0,
                offset: 0,
                limit: PAGE_SIZE,
            });
        }
        self.with_authenticated_network(session, async {
            let mut url = self.origin.endpoint("/api/library");
            url.query_pairs_mut()
                .append_pair("kind", "tracks")
                .append_pair("scope", "all")
                .append_pair("availability", "in-library")
                .append_pair("q", query)
                .append_pair("limit", &PAGE_SIZE.to_string())
                .append_pair("offset", "0");
            let request = self
                .request(url)?
                .header(header::COOKIE, session.secrets.cookie.clone());
            let response = self.send(request, Some(session), false).await?;
            if response.status() != 200 {
                return Err(INVALID_RESPONSE);
            }
            let wire: WirePage = decode(response, MAX_BODY).await?;
            self.check_session(session)?;
            if wire.limit != PAGE_SIZE
                || wire.offset != 0
                || wire.items.len() > PAGE_SIZE as usize
                || wire.items.len() as u32 > wire.total
            {
                return Err(INVALID_RESPONSE);
            }
            let mut seen = HashSet::new();
            let mut items = Vec::with_capacity(wire.items.len());
            for item in wire.items {
                if !seen.insert(item.id.clone()) {
                    return Err(INVALID_RESPONSE);
                }
                items.push(item.into_item("track")?);
            }
            Ok(Page {
                items,
                total: wire.total,
                offset: wire.offset,
                limit: wire.limit,
            })
        })
        .await
    }

    /// Opens the configured origin's account socket. The framing worker must
    /// fence every use against session expiry and network changes.
    pub(crate) async fn account_upgrade(
        &self,
        session: &Session,
    ) -> Result<crate::socket::AccountUpgrade, ApiError> {
        use tokio_tungstenite::tungstenite::handshake::{client::generate_key, derive_accept_key};

        self.with_authenticated_network(session, async {
            let generation = session.network_generation();
            let request = self
                .request_with_method(
                    reqwest::Method::POST,
                    self.origin.endpoint("/api/me/player/connect"),
                )?
                .timeout(SOCKET_TIMEOUT)
                .header(header::COOKIE, session.secrets.cookie.clone())
                .header("X-CSRF-Token", session.secrets.csrf.clone())
                .header(header::CONTENT_TYPE, "application/json")
                .body("{}");
            let response = self.send(request, Some(session), false).await?;
            if response.status() != 200 {
                return Err(INVALID_RESPONSE);
            }
            #[derive(Deserialize)]
            struct Ticket {
                ticket: String,
            }
            let Ticket { ticket } = decode(response, TICKET_BODY).await?;
            if !valid_lower_hex_32(&ticket) {
                return Err(INVALID_RESPONSE);
            }
            self.check_session(session)?;
            if generation & 1 != 0 || session.network_generation() != generation {
                return Err(OFFLINE);
            }

            // No ticket in the URL or upgrade headers: the framing worker sends it in hello.
            let key = generate_key();
            let expected_accept = derive_accept_key(key.as_bytes());
            let protocol = crate::socket::SUBPROTOCOL;
            let request = self
                .request_with_method(
                    reqwest::Method::GET,
                    self.origin.endpoint("/api/me/player/socket"),
                )?
                .timeout(SOCKET_TIMEOUT)
                .header(header::COOKIE, session.secrets.cookie.clone())
                .header(header::CONNECTION, "Upgrade")
                .header(header::UPGRADE, "websocket")
                .header(header::SEC_WEBSOCKET_VERSION, "13")
                .header(header::SEC_WEBSOCKET_KEY, key)
                .header(header::SEC_WEBSOCKET_PROTOCOL, protocol);
            let response = self.send_inner(request, Some(session), false, true).await?;
            if response.status() != reqwest::StatusCode::SWITCHING_PROTOCOLS {
                return Err(INVALID_RESPONSE);
            }
            validate_websocket_upgrade(&response, &expected_accept, protocol)?;
            let upgraded = tokio::time::timeout(SOCKET_TIMEOUT, response.upgrade())
                .await
                .map_err(|_| NETWORK)?
                .map_err(|_| INVALID_RESPONSE)?;
            Ok(crate::socket::AccountUpgrade { upgraded, ticket })
        })
        .await
    }

    /// Fetch one original-format audio file into bounded ephemeral memory. The caller
    /// must obtain coordinator authority before calling and stop audio on session expiry.
    pub async fn media(&self, session: &Session, id: &str) -> Result<Vec<u8>, ApiError> {
        self.with_authenticated_network(session, async {
            let request = self
                .track_request(session, id, "audio", "audio/*, application/octet-stream")?
                .timeout(Duration::from_secs(45));
            let response = self.send(request, Some(session), false).await?;
            if response.status() != 200 {
                return Err(INVALID_RESPONSE);
            }
            let media = content_type(&response)?;
            if !(media.starts_with("audio/") && media.len() > "audio/".len())
                && media != "application/octet-stream"
            {
                return Err(error("The server returned an unsupported audio response."));
            }
            let bytes = self
                .resource_bytes(
                    session,
                    response,
                    MAX_MEDIA,
                    error("This track exceeds the 32 MiB in-memory playback limit."),
                )
                .await?;
            if bytes.is_empty() {
                return Err(error("The server returned an empty audio response."));
            }
            Ok(bytes)
        })
        .await
    }

    /// Account identity for local data, not permission to send credentials. The
    /// immutable authenticated principal, not mutable display fields, defines the scope.
    pub(crate) fn authenticated_account<'a, 's>(
        &'a self,
        session: &'s Session,
    ) -> Result<(&'a str, &'s str), ApiError> {
        self.check_session_owner(session)?;
        if !session.can_access_local() {
            return Err(EXPIRED);
        }
        Ok((self.origin.as_str(), &session.secrets.account_id))
    }

    pub(crate) async fn track_resource(
        &self,
        session: &Session,
        track_id: &str,
        resource: crate::media::TrackResource,
    ) -> Result<(String, Vec<u8>), ApiError> {
        self.with_authenticated_network(session, async {
            let request =
                self.track_request(session, track_id, resource.suffix(), resource.accept())?;
            self.fetch_media_resource(session, request, resource).await
        })
        .await
    }

    /// Missing artwork is ordinary; callers provide a static fallback.
    pub(crate) async fn catalog_artwork(
        &self,
        session: &Session,
        kind: catalog::CatalogKind,
        id: &str,
    ) -> Result<Option<(String, Vec<u8>)>, ApiError> {
        self.with_authenticated_network(session, async {
            if !valid_id(id) {
                return Err(INVALID_INPUT);
            }
            let resource = crate::media::TrackResource::Artwork;
            let url = self.origin.endpoint_segments(&[
                "api",
                "library",
                kind.plural(),
                id,
                resource.suffix(),
            ])?;
            let request = self
                .request_with_accept(reqwest::Method::GET, url, resource.accept())?
                .header(header::COOKIE, session.secrets.cookie.clone());
            match self.fetch_media_resource(session, request, resource).await {
                Ok(result) => Ok(Some(result)),
                Err(err) if err.status == 404 => Ok(None),
                Err(err) => Err(err),
            }
        })
        .await
    }

    async fn fetch_media_resource(
        &self,
        session: &Session,
        request: RequestBuilder,
        resource: crate::media::TrackResource,
    ) -> Result<(String, Vec<u8>), ApiError> {
        let response = self.send(request, Some(session), false).await?;
        if response.status() != 200 {
            return Err(INVALID_RESPONSE);
        }
        let mime = content_type(&response)?;
        if !resource.accepts(&mime) {
            return Err(INVALID_RESPONSE);
        }
        let bytes = self
            .resource_bytes(session, response, resource.max_bytes(), INVALID_RESPONSE)
            .await?;
        if bytes.is_empty() {
            return Err(INVALID_RESPONSE);
        }
        Ok((mime, bytes))
    }

    /// The current per-track media descriptor, not a collection manifest.
    pub(crate) async fn offline_manifest(
        &self,
        session: &Session,
        track_id: &str,
    ) -> Result<Vec<u8>, ApiError> {
        self.with_authenticated_network(session, async {
            let url = self.offline_track_url(track_id, "media")?;
            self.offline_json_bytes(session, url, OFFLINE_MEDIA_BODY)
                .await
        })
        .await
    }

    /// Read one bounded manifest page for an album, artist, or personal playlist.
    pub(crate) async fn offline_collection_manifest(
        &self,
        session: &Session,
        resource: &str,
        id: &str,
        cursor: Option<&str>,
    ) -> Result<Vec<u8>, ApiError> {
        self.with_authenticated_network(session, async {
            match resource {
                "albums" | "artists" if valid_id(id) => (),
                "playlists" if catalog::derived_id(id) => (),
                _ => return Err(INVALID_INPUT),
            }
            if cursor.is_some_and(|value| !catalog::opaque_token(value, 4096)) {
                return Err(INVALID_INPUT);
            }
            let mut url = self
                .origin
                .endpoint_segments(&["api", "me", "offline", "v1", resource, id, "manifest"])?;
            {
                let mut query = url.query_pairs_mut();
                query.append_pair("limit", "20");
                if let Some(cursor) = cursor {
                    query.append_pair("cursor", cursor);
                }
            }
            if url.query().is_some_and(|query| query.len() > 8192) {
                return Err(INVALID_INPUT);
            }
            self.offline_json_bytes(session, url, OFFLINE_COLLECTION_BODY)
                .await
        })
        .await
    }

    /// Retrieve one pinned representation. Never follow the descriptor's downloadUrl;
    /// reconstruct the fixed route and query from independently validated values.
    pub(crate) async fn offline_audio(
        &self,
        session: &Session,
        id: &str,
        version: &str,
        content_type: &str,
        byte_length: usize,
    ) -> Result<Vec<u8>, ApiError> {
        self.with_authenticated_network(session, async {
            if !version
                .strip_prefix("sha256:")
                .is_some_and(valid_lower_hex_32)
                || !OFFLINE_AUDIO_TYPES.contains(&content_type)
                || !(1..=MAX_MEDIA).contains(&byte_length)
            {
                return Err(INVALID_INPUT);
            }
            let mut url = self.offline_track_url(id, "audio")?;
            url.query_pairs_mut().append_pair("version", version);
            let request = self
                .request_with_accept(reqwest::Method::GET, url, content_type)?
                .header(header::COOKIE, session.secrets.cookie.clone())
                .timeout(Duration::from_secs(45));
            let response = self.send(request, Some(session), false).await?;
            let headers = response.headers();
            let etag = format!("\"{version}\"");
            if response.status() != 200
                || single_header(headers, header::CONTENT_TYPE).map(HeaderValue::as_bytes)
                    != Some(content_type.as_bytes())
                || single_header(headers, header::ETAG).map(HeaderValue::as_bytes)
                    != Some(etag.as_bytes())
                || single_header(headers, header::CONTENT_LENGTH)
                    .and_then(|v| v.to_str().ok()?.parse::<usize>().ok())
                    != Some(byte_length)
                || headers.contains_key(header::CONTENT_ENCODING)
                || headers.contains_key(header::TRANSFER_ENCODING)
            {
                return Err(INVALID_RESPONSE);
            }
            let bytes = self
                .resource_bytes(session, response, byte_length, INVALID_RESPONSE)
                .await?;
            if bytes.len() != byte_length {
                return Err(INVALID_RESPONSE);
            }
            Ok(bytes)
        })
        .await
    }

    fn offline_track_url(&self, id: &str, suffix: &'static str) -> Result<Url, ApiError> {
        if !valid_id(id) {
            return Err(INVALID_INPUT);
        }
        self.origin
            .endpoint_segments(&["api", "me", "offline", "v1", "tracks", id, suffix])
    }

    async fn offline_json_bytes(
        &self,
        session: &Session,
        url: Url,
        bound: usize,
    ) -> Result<Vec<u8>, ApiError> {
        let request = self
            .request_with_method(reqwest::Method::GET, url)?
            .header(header::COOKIE, session.secrets.cookie.clone());
        let response = self.send(request, Some(session), false).await?;
        if response.status() != 200 || content_type(&response)? != "application/json" {
            return Err(INVALID_RESPONSE);
        }
        let bytes = self
            .resource_bytes(session, response, bound, INVALID_RESPONSE)
            .await?;
        if bytes.is_empty() {
            return Err(INVALID_RESPONSE);
        }
        Ok(bytes)
    }

    fn track_request(
        &self,
        session: &Session,
        id: &str,
        suffix: &'static str,
        accept: &'static str,
    ) -> Result<RequestBuilder, ApiError> {
        self.check_session(session)?;
        if !valid_id(id) {
            return Err(INVALID_INPUT);
        }
        let url = self
            .origin
            .endpoint_segments(&["api", "library", "tracks", id, suffix])?;
        Ok(self
            .request_with_accept(reqwest::Method::GET, url, accept)?
            .header(header::COOKIE, session.secrets.cookie.clone()))
    }

    async fn resource_bytes(
        &self,
        session: &Session,
        mut response: Response,
        bound: usize,
        oversized: ApiError,
    ) -> Result<Vec<u8>, ApiError> {
        self.check_session(session)?;
        if response.content_length().is_some_and(|n| n > bound as u64) {
            return Err(oversized);
        }
        let mut bytes = Vec::new();
        loop {
            self.check_session(session)?;
            let Some(chunk) = response.chunk().await.map_err(|_| NETWORK)? else {
                break;
            };
            if chunk.len() > bound - bytes.len() {
                return Err(oversized);
            }
            extend_bounded(&mut bytes, &chunk, bound);
        }
        self.check_session(session)?;
        Ok(bytes)
    }

    /// Revokes the local capability before awaiting I/O, even offline, on failure or on cancellation.
    pub async fn logout(&self, session: &Session) -> Result<(), ApiError> {
        self.check_session_owner(session)?;
        let fence = NetworkFence::new(session);
        let deadline = {
            let _publication = crate::playlist_pin_store::PUBLICATION_GATE.lock().unwrap();
            let mut lifetime = session.lifetime();
            lifetime.check_revocation()?;
            let deadline = lifetime.deadline;
            lifetime.revocation = Some(deadline.local_expiry());
            deadline
        };
        fence?
            .run(async {
                // Preserve the original ordinary-login deadline. Rolling credentials
                // may still be server-valid after the locally observed deadline.
                if deadline.server_expires.is_none() && deadline.elapsed() {
                    return Err(EXPIRED);
                }
                let request = self
                    .request(self.origin.endpoint("/api/auth/logout"))?
                    .header(header::COOKIE, session.secrets.cookie.clone())
                    .header("X-CSRF-Token", session.secrets.csrf.clone())
                    .header(header::CONTENT_TYPE, "application/json")
                    .body("{}");
                let response = self.send(request, None, false).await?;
                if response.status() != 204 {
                    return Err(INVALID_RESPONSE);
                }
                Ok(())
            })
            .await
    }

    /// Covers an entire operation, including response body consumption or a local-first
    /// online media lookup. An offline→online transition must cancel older work.
    pub(crate) async fn with_authenticated_network<T>(
        &self,
        session: &Session,
        operation: impl Future<Output = Result<T, ApiError>>,
    ) -> Result<T, ApiError> {
        self.check_session(session)?;
        NetworkFence::new(session)?
            .run(async {
                self.check_session(session)?;
                let result = operation.await?;
                self.check_session(session)?;
                Ok(result)
            })
            .await
    }

    fn check_session(&self, session: &Session) -> Result<(), ApiError> {
        self.check_session_owner(session)?;
        session.lifetime().check_live()
    }

    fn check_session_owner(&self, session: &Session) -> Result<(), ApiError> {
        // The API-instance capability prevents cross-origin/cross-client cookie or local scope reuse.
        if !Arc::ptr_eq(&self.identity, &session.secrets.owner) {
            return Err(error(
                "This session belongs to a different connection. Sign in again.",
            ));
        }
        Ok(())
    }

    fn request(&self, url: Url) -> Result<RequestBuilder, ApiError> {
        let method = if matches!(url.path(), "/api/auth/login" | "/api/auth/logout") {
            reqwest::Method::POST
        } else {
            reqwest::Method::GET
        };
        self.request_with_method(method, url)
    }

    fn request_with_method(
        &self,
        method: reqwest::Method,
        url: Url,
    ) -> Result<RequestBuilder, ApiError> {
        self.request_with_accept(method, url, "application/json")
    }

    fn request_with_accept(
        &self,
        method: reqwest::Method,
        url: Url,
        accept: &str,
    ) -> Result<RequestBuilder, ApiError> {
        if url.origin() != self.origin.0.origin()
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(error("Off-origin requests are not allowed."));
        }
        Ok(self
            .client
            .request(method, url)
            .header(header::ORIGIN, self.origin.as_str())
            .header(header::ACCEPT, accept))
    }

    async fn send(
        &self,
        request: RequestBuilder,
        session: Option<&Session>,
        login: bool,
    ) -> Result<Response, ApiError> {
        self.send_inner(request, session, login, false).await
    }

    async fn send_inner(
        &self,
        request: RequestBuilder,
        session: Option<&Session>,
        login: bool,
        upgrading: bool,
    ) -> Result<Response, ApiError> {
        let request_scope = if let Some(session) = session {
            self.check_session_owner(session)?;
            let generation = session.secrets.network.borrow();
            if *generation & 1 != 0 {
                return Err(OFFLINE);
            }
            let lifetime = session.lifetime();
            lifetime.check_live()?;
            Some((session, *generation, lifetime.deadline))
        } else {
            None
        };
        let response = request.send().await.map_err(|_| NETWORK)?;
        let status = response.status();
        if let Some((session, generation, used)) = request_scope {
            let _publication = (status == 401)
                .then(|| crate::playlist_pin_store::PUBLICATION_GATE.lock().unwrap());
            // Keep the mode fence held through publication or revocation.
            let current = session.secrets.network.borrow();
            if *current != generation {
                return Err(OFFLINE);
            }
            let mut lifetime = session.lifetime();
            if status == 401 {
                // Before checking local expiry: this denial may be authoritative
                // even if the captured deadline elapsed while it was in flight.
                return Err(lifetime.apply_denial(used, EXPIRED));
            }
            lifetime.check_live()?;
            if used.server_expires.is_some()
                && (status.is_success() || status == 304)
                && let Ok(next) = self.rolling_publication(session, response.headers())
            {
                lifetime.extend(next);
            }
            if upgrading && status == reqwest::StatusCode::SWITCHING_PROTOCOLS {
                return Ok(response); // validated by the account-upgrade caller
            }
        }
        if status.is_success() {
            return Ok(response);
        }
        // Do not read or retain error bodies (which can contain credentials, URLs or paths).
        Err(Self::response_error(status.as_u16(), login))
    }

    fn response_error(status: u16, login: bool) -> ApiError {
        let mapped = match status {
            401 if login => error("Invalid username or password."),
            401 => EXPIRED,
            403 => error("The server refused this request. Check your account permissions."),
            404 => error("This item is no longer available."),
            409 => error("The item changed on the server. Refresh and try again."),
            429 => error("Too many requests. Please try again later."),
            300..=399 => {
                error("The server redirected the request. Enter its direct origin instead.")
            }
            500..=599 => error("The server is temporarily unavailable."),
            _ => error("The server could not complete the request."),
        };
        ApiError { status, ..mapped }
    }
}

fn validate_websocket_upgrade(
    response: &Response,
    expected_accept: &str,
    protocol: &str,
) -> Result<(), ApiError> {
    let headers = response.headers();
    let connection_upgrade = headers.get_all(header::CONNECTION).iter().any(|value| {
        value.to_str().is_ok_and(|value| {
            value
                .split(',')
                .any(|token| token.trim_ascii().eq_ignore_ascii_case("upgrade"))
        })
    });
    if response.version() != reqwest::Version::HTTP_11
        || !connection_upgrade
        || !single_header(headers, header::UPGRADE)
            .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"websocket"))
        || single_header(headers, header::SEC_WEBSOCKET_PROTOCOL).map(HeaderValue::as_bytes)
            != Some(protocol.as_bytes())
        || single_header(headers, header::SEC_WEBSOCKET_ACCEPT).map(HeaderValue::as_bytes)
            != Some(expected_accept.as_bytes())
        || headers.contains_key(header::SEC_WEBSOCKET_EXTENSIONS)
        || headers.contains_key(header::CONTENT_LENGTH)
        || headers.contains_key(header::TRANSFER_ENCODING)
    {
        return Err(INVALID_RESPONSE);
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionSummary {
    account_id: String,
    username: String,
    role: String,
    csrf_token: String,
}

fn session_cookie_name(origin: &Origin) -> &'static str {
    if origin.0.scheme() == "https" {
        "__Host-spofify_session"
    } else {
        "spofify_session"
    }
}

fn session_cookie(
    origin: &Origin,
    headers: &header::HeaderMap,
) -> Result<(HeaderValue, Instant), ApiError> {
    let (header, cookie) = parse_session_cookie(origin, headers)?;
    // Max-Age takes precedence over Expires for ordinary login, per RFC 6265.
    let seconds = if let Some(age) = cookie.max_age() {
        age.whole_seconds()
    } else if let Some(Expiration::DateTime(expires)) = cookie.expires() {
        (expires - cookie::time::OffsetDateTime::now_utc()).whole_seconds()
    } else {
        return Err(INVALID_RESPONSE);
    };
    if seconds <= 0 {
        return Err(INVALID_RESPONSE);
    }
    // A monotonic deadline prevents clock rollback from resurrecting an expired token.
    let deadline = Instant::now()
        .checked_add(Duration::from_secs(seconds as u64))
        .ok_or(INVALID_RESPONSE)?;
    Ok((header, deadline))
}

/// Accept only one session cookie and require it to be a host-only root cookie.
fn parse_session_cookie<'a>(
    origin: &Origin,
    headers: &'a header::HeaderMap,
) -> Result<(HeaderValue, Cookie<'a>), ApiError> {
    let secure = origin.0.scheme() == "https";
    let expected = session_cookie_name(origin);
    let mut candidate = None;
    let mut total = 0;
    for (count, header) in headers.get_all(header::SET_COOKIE).iter().enumerate() {
        total += header.as_bytes().len();
        if count >= 16 || total > 8192 {
            return Err(INVALID_RESPONSE);
        }
        let raw = header.to_str().map_err(|_| INVALID_RESPONSE)?;
        let name = raw
            .split(';')
            .next()
            .and_then(|s| s.split_once('='))
            .map(|(name, _)| name.trim());
        if !matches!(name, Some("spofify_session" | "__Host-spofify_session")) {
            continue;
        }
        if candidate.is_some() || name != Some(expected) {
            return Err(INVALID_RESPONSE);
        }
        let cookie = Cookie::parse(raw).map_err(|_| INVALID_RESPONSE)?;
        let mut attrs = HashSet::new();
        for attr in raw.split(';').skip(1) {
            let (name, value) = attr
                .trim()
                .split_once('=')
                .map_or((attr.trim(), None), |(n, v)| (n.trim(), Some(v.trim())));
            let name = name.to_ascii_lowercase();
            if name.is_empty() || !attrs.insert(name.clone()) {
                return Err(INVALID_RESPONSE);
            }
            match name.as_str() {
                "domain" => return Err(INVALID_RESPONSE), // Even Domain=the-exact-host is not host-only.
                "path" if value != Some("/") => return Err(INVALID_RESPONSE),
                "httponly" | "secure" if value.is_some() => return Err(INVALID_RESPONSE),
                "max-age" if value.is_none_or(|v| v.parse::<i64>().is_err()) => {
                    return Err(INVALID_RESPONSE);
                }
                "expires" if !matches!(cookie.expires(), Some(Expiration::DateTime(_))) => {
                    return Err(INVALID_RESPONSE);
                }
                _ => {}
            }
        }
        let value = cookie.value();
        if cookie.name() != expected
            || !valid_session_cookie_value(value)
            || !attrs.contains("path")
            || cookie.path() != Some("/")
            || cookie.domain().is_some()
            || cookie.http_only() != Some(true)
            || cookie.secure().unwrap_or(false) != secure
        {
            return Err(INVALID_RESPONSE);
        }
        let mut header =
            HeaderValue::from_str(&format!("{expected}={value}")).map_err(|_| INVALID_RESPONSE)?;
        header.set_sensitive(true);
        candidate = Some((header, cookie));
    }
    candidate.ok_or(INVALID_RESPONSE)
}

fn valid_session_cookie_value(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 512
        && value
            .bytes()
            .all(|b| matches!(b, 0x21 | 0x23..=0x2b | 0x2d..=0x3a | 0x3c..=0x5b | 0x5d..=0x7e))
}

fn single_header(headers: &header::HeaderMap, name: header::HeaderName) -> Option<&HeaderValue> {
    let values = headers.get_all(name);
    let mut values = values.iter();
    let value = values.next()?;
    values.next().is_none().then_some(value)
}

fn content_type(response: &Response) -> Result<String, ApiError> {
    single_header(response.headers(), header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split(';').next())
        .map(|s| s.trim().to_ascii_lowercase())
        .ok_or(INVALID_RESPONSE)
}

async fn decode<T: serde::de::DeserializeOwned>(
    mut response: Response,
    bound: usize,
) -> Result<T, ApiError> {
    let media = content_type(&response)?;
    if media != "application/json"
        && !(media.starts_with("application/") && media.ends_with("+json"))
    {
        return Err(INVALID_RESPONSE);
    }
    if response.content_length().is_some_and(|n| n > bound as u64) {
        return Err(INVALID_RESPONSE);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| NETWORK)? {
        if chunk.len() > bound - body.len() {
            return Err(INVALID_RESPONSE);
        }
        extend_bounded(&mut body, &chunk, bound);
    }
    serde_json::from_slice(&body).map_err(|_| INVALID_RESPONSE)
}

// Callers check the bound first. Cap allocation growth as well as payload length.
fn extend_bounded(bytes: &mut Vec<u8>, chunk: &[u8], bound: usize) {
    let needed = bytes.len() + chunk.len();
    if needed > bytes.capacity() {
        let capacity = needed.max(bytes.capacity().saturating_mul(2)).min(bound);
        bytes.reserve_exact(capacity - bytes.len());
    }
    bytes.extend_from_slice(chunk);
}

#[cfg(test)]
#[derive(Deserialize)]
struct WirePage {
    items: Vec<WireItem>,
    total: u32,
    offset: u32,
    limit: u32,
}

#[cfg(test)]
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireItem {
    id: String,
    name: String,
    kind: String,
    #[serde(default)]
    artists: Vec<Artist>,
    #[serde(default)]
    album_name: String,
    #[serde(default)]
    duration_ms: u64,
    #[serde(default)]
    has_audio: Option<bool>,
    #[serde(default)]
    library_path: Option<String>,
    #[serde(default)]
    coverage: Coverage,
    #[serde(default)]
    track_number: u32,
    #[serde(default)]
    disc_number: u32,
}

#[cfg(test)]
#[derive(Deserialize)]
struct Artist {
    id: String,
    name: String,
}
#[cfg(test)]
#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Coverage {
    #[serde(default)]
    in_library: u32,
}

#[cfg(test)]
impl WireItem {
    fn into_item(self, kind: &str) -> Result<Item, ApiError> {
        if self.kind != kind
            || !valid_id(&self.id)
            || !valid_text(&self.name, 1024)
            || self.artists.len() > 32
            || self
                .artists
                .iter()
                .any(|a| (!a.id.is_empty() && !valid_id(&a.id)) || !valid_text(&a.name, 1024))
            || (!self.album_name.is_empty() && !valid_text(&self.album_name, 1024))
            || self.duration_ms > 7 * 24 * 60 * 60 * 1000
            || self.track_number > 100_000
            || self.disc_number > 100_000
            || self.library_path.as_ref().is_some_and(|p| p.len() > 4096)
        {
            return Err(INVALID_RESPONSE);
        }
        let artists = self
            .artists
            .iter()
            .map(|a| a.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        if artists.len() > 4096 {
            return Err(INVALID_RESPONSE);
        }
        let has_audio = if kind == "track" {
            self.has_audio
                .unwrap_or_else(|| self.library_path.as_ref().is_some_and(|p| !p.is_empty()))
        } else {
            self.coverage.in_library > 0
        };
        Ok(Item {
            id: self.id,
            name: self.name,
            kind: self.kind,
            artists,
            album_name: self.album_name,
            duration_ms: self.duration_ms,
            has_audio,
            track_number: self.track_number,
            disc_number: self.disc_number,
        })
    }
}

fn valid_text(text: &str, bound: usize) -> bool {
    !text.trim().is_empty() && text.len() <= bound && !text.chars().any(char::is_control)
}

fn valid_id(id: &str) -> bool {
    valid_text(id, 512) && id.trim() == id && id != "." && id != ".."
}

fn valid_lower_hex_32(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::VecDeque,
        io::{Read, Write},
        net::TcpListener,
        sync::atomic::{AtomicBool, Ordering},
        thread::{self, JoinHandle},
    };

    #[test]
    fn real_manual_track_projection_allows_name_only_artists() {
        let wire: WireItem = serde_json::from_value(serde_json::json!({
            "kind":"track", "id":"manual-track", "name":"Blue Hour",
            "artists":[{"id":"", "name":"Juniper Radio"}], "albumName":"Night Window",
            "durationMs":24000, "hasAudio":true,
            "coverage":{"tracks":1,"inLibrary":1,"albums":0}
        }))
        .unwrap();
        let item = wire.into_item("track").unwrap();
        assert_eq!(item.artists, "Juniper Radio");
        assert!(item.has_audio);
    }

    mod auth {
        include!("api_auth_tests.rs");
    }

    const SUMMARY: &str = r#"{"accountId":"a1","username":"listener","role":"user","csrfToken":"csrf-secret","futureField":true}"#;
    const VALID_COOKIE: &str =
        "spofify_session=cookie-secret; Path=/; HttpOnly; Max-Age=3600; SameSite=Lax";
    const RESULTS: &str = r#"{"items":[{"id":"track/1% ?#","name":"First track","kind":"track","artists":[{"id":"ar1","name":"Artist One"},{"id":"ar2","name":"Artist Two"}],"albumName":"Album One","libraryPath":"/private/synthetic/never-expose.flac","durationMs":123000,"trackNumber":2,"coverage":{"inLibrary":1}}],"total":1,"offset":0,"limit":20,"revision":42}"#;

    #[derive(Clone, Debug)]
    struct Recorded {
        first_line: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }

    impl Recorded {
        fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.as_str())
        }
    }

    /// Owned loopback fixture: finite requests/reads, joined on every exit (including panic).
    struct Wire {
        origin: Origin,
        requests: Arc<Mutex<Vec<Recorded>>>,
        stop: Arc<AtomicBool>,
        task: Option<JoinHandle<()>>,
    }

    impl Wire {
        fn new(responses: Vec<Vec<u8>>) -> Self {
            Self::with_responder(responses, |_, response| response)
        }

        fn with_responder(
            responses: Vec<Vec<u8>>,
            mut prepare: impl FnMut(&Recorded, Vec<u8>) -> Vec<u8> + Send + 'static,
        ) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let origin =
                Origin::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let stop = Arc::new(AtomicBool::new(false));
            let saved = requests.clone();
            let stopping = stop.clone();
            let task = thread::spawn(move || {
                let mut responses: VecDeque<_> = responses.into();
                let deadline = Instant::now() + Duration::from_secs(10);
                while !stopping.load(Ordering::SeqCst) && Instant::now() < deadline {
                    let (mut stream, _) = match listener.accept() {
                        Ok(pair) => pair,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(2));
                            continue;
                        }
                        Err(_) => break,
                    };
                    stream
                        .set_read_timeout(Some(Duration::from_millis(250)))
                        .unwrap();
                    stream
                        .set_write_timeout(Some(Duration::from_millis(250)))
                        .unwrap();
                    let mut bytes = Vec::new();
                    let mut chunk = [0u8; 16 * 1024];
                    let mut parsed = None;
                    // Accept bounded mutation requests plus HTTP headers.
                    while bytes.len() <= 64 * 1024 + 8192 && !stopping.load(Ordering::SeqCst) {
                        let size = match stream.read(&mut chunk) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => n,
                        };
                        bytes.extend_from_slice(&chunk[..size]);
                        if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                            let text = String::from_utf8_lossy(&bytes[..end]);
                            let mut lines = text.split("\r\n");
                            let first_line = lines.next().unwrap().to_owned();
                            let headers: Vec<_> = lines
                                .filter_map(|l| l.split_once(':'))
                                .map(|(k, v)| (k.to_owned(), v.trim().to_owned()))
                                .collect();
                            let length: usize = headers
                                .iter()
                                .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
                                .map(|(_, v)| v.parse().unwrap())
                                .unwrap_or(0);
                            if bytes.len() >= end + 4 + length {
                                parsed = Some(Recorded {
                                    first_line,
                                    headers,
                                    body: bytes[end + 4..end + 4 + length].to_vec(),
                                });
                                break;
                            }
                        }
                    }
                    if let Some(request) = parsed {
                        // Empty response deliberately simulates disconnection after the request.
                        let response = responses
                            .pop_front()
                            .unwrap_or_else(|| reply(500, &[], "unexpected extra request"));
                        let response = prepare(&request, response);
                        let mut saved = saved.lock().unwrap();
                        saved.push(request);
                        if saved.len() >= 32 {
                            break;
                        }
                        drop(saved);
                        if response == b"STALL" {
                            thread::sleep(Duration::from_millis(100));
                        } else {
                            let _ = stream.write_all(&response);
                        }
                    }
                }
            });
            Self {
                origin,
                requests,
                stop,
                task: Some(task),
            }
        }

        fn api(&self) -> Api {
            Api::new(self.origin.clone()).unwrap()
        }
        fn recorded(&self) -> Vec<Recorded> {
            self.requests.lock().unwrap().clone()
        }
    }

    impl Drop for Wire {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            if let Some(task) = self.task.take() {
                task.join().unwrap();
            }
        }
    }

    fn reply(status: u16, headers: &[(&str, &str)], body: &str) -> Vec<u8> {
        let mut raw = format!(
            "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n",
            body.len()
        );
        for (k, v) in headers {
            raw.push_str(&format!("{k}: {v}\r\n"));
        }
        raw.push_str("\r\n");
        raw.push_str(body);
        raw.into_bytes()
    }

    fn json(body: &str) -> Vec<u8> {
        reply(200, &[("Content-Type", "application/json")], body)
    }
    fn login_reply() -> Vec<u8> {
        reply(
            200,
            &[
                ("Content-Type", "application/json"),
                ("Set-Cookie", VALID_COOKIE),
            ],
            SUMMARY,
        )
    }

    // Follow the Go web test artist/album identities (libraryHandlerFixture) and both
    // role projections (listening.go / library.go). The search item is the
    // positive ListeningItem projection: no self credit, no albumId, no audio.
    // Extend the fixture with an ID-only guest, which catalog.go's upsertArtist
    // and replaceArtists persist and library.go projects without filling a name.
    #[tokio::test]
    async fn go_indexed_artist_opens_role_specific_detail_and_bounded_releases() {
        use crate::{
            desk::Access,
            pages::{self, Route},
            replica::{Replica, Trail},
        };
        use serde_json::json;

        let artist = json!({"kind":"artist","id":"root","name":"Björk","hasAudio":false,
            "coverage":{"tracks":2,"inLibrary":1,"albums":1}});
        let coverage = json!({"tracks":2,"inLibrary":1,"albums":1});
        for role in ["user", "admin"] {
            let mut detail = artist.clone();
            // Library's artist coverage includes the additional unsaved
            // summary release; indexed coverage counts only actual memberships.
            detail["coverage"]["albums"] = json!(2);
            let mut album = json!({"kind":"album","id":"album","name":"Vespertine",
                "artists":[{"id":"root","name":"Björk"},{"id":"guest-record","name":""}],
                "albumType":"album","releaseDate":"2001-08-27","relation":"release",
                "hasAudio":false,"coverage":coverage.clone()});
            let mut track = json!({"kind":"track","id":"one","name":"Hidden Place",
                "artists":[{"id":"root","name":"Björk"}],"albumId":"album","albumName":"Vespertine",
                "durationMs":180000,"discNumber":1,"trackNumber":1,"hasAudio":true,
                "coverage":{"tracks":1,"inLibrary":1,"albums":0}});
            if role == "admin" {
                for (item, status, incomplete, complete) in [
                    (&mut detail, "partial", 2, false),
                    (&mut album, "partial", 1, true),
                    (&mut track, "complete", 0, true),
                ] {
                    item.as_object_mut().unwrap().remove("hasAudio");
                    item["tracked"] = json!(true);
                    item["coverage"]["status"] = json!(status);
                    item["coverage"]["completeAlbums"] = json!(0);
                    item["coverage"]["incompleteAlbums"] = json!(incomplete);
                    item["coverage"]["metadataComplete"] = json!(complete);
                }
                track["libraryPath"] = json!("/recorded/synthetic-track.flac");
            }
            let wire = Wire::new(vec![
                json(
                    &json!({"items":[artist.clone()],"resultCount":1,
                    "expiresAt":"2026-05-01T12:00:00.123456789Z"})
                    .to_string(),
                ),
                json(&json!({"item":detail,"revision":42}).to_string()),
                // Detail and child listing legitimately read separate revisions.
                json(
                    &json!({"items":[album.clone()],"limit":20,"offset":0,"total":1,
                    "revision":43})
                    .to_string(),
                ),
                json(&json!({"item":album,"revision":43}).to_string()),
                json(
                    &json!({"items":[track],"limit":20,"offset":0,"total":1,"revision":43})
                        .to_string(),
                ),
            ]);
            let api = Arc::new(wire.api());
            let mut session = api.test_session();
            session.role = role.into();
            let access = Access {
                epoch: 1,
                api,
                session,
                offline: false,
                personal: Default::default(),
            };
            let history = Route::History;
            let history_page = Arc::new(pages::load(&access, &history, None).await.unwrap());
            assert!(history_page.rows.is_empty());
            assert!(!history_page.status.is_empty());
            let query = Route::Search("Björk".into());
            let search = Arc::new(pages::load(&access, &query, None).await.unwrap());
            let hit = &search.rows[0];
            assert!(hit.music.credits.is_empty());
            assert_eq!(hit.title, "Björk");
            let target = hit.route.clone().unwrap();
            assert_eq!(target, Route::Artist("root".into()));
            let page = pages::load(&access, &target, None).await.unwrap();
            assert_eq!(page.title, "Björk");
            assert_eq!(page.subtitle, "1 available song");
            assert_eq!(page.rows.len(), 1);
            assert!(!page.more);
            assert_eq!(page.rows[0].music.credits[1].name, "Unknown artist");
            assert_eq!(
                page.rows[0].music.credits[1].target,
                Some(Route::Artist("guest-record".into()))
            );
            let release = page.rows[0].route.clone().unwrap();
            assert_eq!(release, Route::Album("album".into()));
            let album = pages::load(&access, &release, None).await.unwrap();
            assert_eq!(album.title, "Vespertine");
            assert_eq!(album.music.credits[0].target, Some(target.clone()));
            assert!(
                album.music.album.is_none(),
                "album has no redundant self link"
            );
            let recording = &album.rows[0];
            assert!(recording.route.is_none());
            assert_eq!(
                recording.music.album.as_ref().unwrap().target,
                Some(release.clone())
            );
            assert_eq!(
                recording
                    .playback
                    .as_ref()
                    .unwrap()
                    .context()
                    .unwrap()
                    .info_route(),
                release
            );
            assert!(!format!("{recording:?}").contains("/recorded/"));

            let mut trail = Trail::default();
            let mut replica = Replica::default();
            replica.put(history.clone(), history_page.clone());
            replica.put(query.clone(), search.clone());
            trail.visit(&history, &query);
            trail.visit(&query, &target);
            trail.visit(&target, &release);
            assert_eq!(trail.pop(), Some(target));
            let restored_query = trail.pop().unwrap();
            assert_eq!(restored_query, query);
            assert!(Arc::ptr_eq(
                &replica.get(&restored_query).unwrap().0,
                &search
            ));
            assert_eq!(trail.pop(), Some(history.clone()));
            assert!(Arc::ptr_eq(
                &replica.get(&history).unwrap().0,
                &history_page
            ));

            let reads = wire.recorded();
            assert_eq!(
                reads
                    .iter()
                    .map(|r| r.first_line.as_str())
                    .collect::<Vec<_>>(),
                [
                    "GET /search?query=Bj%C3%B6rk&limit=20 HTTP/1.1",
                    "GET /api/library/artists/root HTTP/1.1",
                    "GET /api/library?kind=albums&scope=all&availability=in-library&limit=20&offset=0&artistId=root&releaseGroup=all HTTP/1.1",
                    "GET /api/library/albums/album HTTP/1.1",
                    "GET /api/library?kind=tracks&scope=all&availability=in-library&limit=20&offset=0&albumId=album HTTP/1.1",
                ]
            );
        }
    }

    // Avoid requiring Debug on the intentionally opaque Session type.
    fn failure<T>(result: Result<T, ApiError>) -> ApiError {
        match result {
            Err(error) => error,
            Ok(_) => panic!("Unexpected API success"),
        }
    }

    #[tokio::test]
    async fn explicit_offline_blocks_authenticated_io_and_logout_still_revokes() {
        let wire = Wire::new(vec![]);
        let api = wire.api();
        let session = api.test_session();
        let mut clone = session.clone();
        clone.account_id = "different-display-account".into();
        let changes = session.network_changes();
        clone.set_offline(true);
        assert!(session.is_offline());
        assert_eq!(*changes.borrow(), 1);
        clone.set_offline(true);
        assert_eq!(*changes.borrow(), 1);
        assert_eq!(
            api.authenticated_account(&clone).unwrap(),
            (wire.origin.as_str(), "synthetic")
        );
        assert_eq!(failure(api.search(&session, "track").await), OFFLINE);
        assert_eq!(failure(api.media(&session, "track").await), OFFLINE);
        assert_eq!(failure(api.account_upgrade(&session).await), OFFLINE);
        assert_eq!(failure(api.logout(&clone).await), OFFLINE);
        assert!(session.is_expired());
        assert!(wire.recorded().is_empty());
    }

    #[tokio::test]
    async fn expired_cookie_keeps_only_the_explicit_offline_local_capability() {
        let wire = Wire::new(vec![]);
        let api = wire.api();
        let foreign = wire.api();
        let session = api.test_session_with_deadline(Instant::now());
        let clone = session.clone();

        assert!(session.is_expired());
        assert!(!clone.can_access_local());
        assert_eq!(failure(api.authenticated_account(&clone)), EXPIRED);
        session.set_offline(true);
        assert!(session.is_expired());
        assert!(clone.can_access_local());
        assert_eq!(
            api.authenticated_account(&clone).unwrap(),
            (wire.origin.as_str(), "synthetic")
        );
        assert!(foreign.authenticated_account(&clone).is_err());
        assert!(api.media(&clone, "track").await.is_err());

        clone.set_offline(false);
        assert!(session.is_expired());
        assert!(!session.can_access_local());
        assert_eq!(failure(api.authenticated_account(&session)), EXPIRED);
        assert_eq!(failure(api.search(&session, "track").await), EXPIRED);
        assert_eq!(failure(api.account_upgrade(&session).await), EXPIRED);

        session.set_offline(true);
        assert!(session.can_access_local());
        assert_eq!(failure(api.logout(&clone).await), OFFLINE);
        assert!(!session.can_access_local());
        assert_eq!(failure(api.authenticated_account(&session)), EXPIRED);
        assert!(wire.recorded().is_empty());
    }

    #[tokio::test]
    async fn server_401_stays_revoked_in_explicit_offline_mode() {
        let wire = Wire::new(vec![reply(401, &[], "synthetic denial")]);
        let api = wire.api();
        let session = api.test_session();
        let clone = session.clone();
        assert!(clone.can_access_local());
        assert_eq!(failure(api.search(&session, "track").await), EXPIRED);
        clone.set_offline(true);
        assert!(session.is_expired());
        assert!(!session.can_access_local());
        assert_eq!(failure(api.authenticated_account(&session)), EXPIRED);
        assert_eq!(wire.recorded().len(), 1);
    }

    #[tokio::test]
    async fn offline_online_cycle_disposes_an_in_flight_response() {
        let api = Api::new(Origin::parse("http://127.0.0.1").unwrap()).unwrap();
        let session = api.test_session();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        let ongoing = api.with_authenticated_network(&session, async {
            started_tx.send(()).unwrap();
            response_rx.await.map_err(|_| NETWORK)
        });
        let cycle = async {
            started_rx.await.unwrap();
            session.set_offline(true);
            session.set_offline(false);
            let _ = response_tx.send("old response");
        };
        let (result, ()) = tokio::join!(ongoing, cycle);
        assert_eq!(failure(result), OFFLINE);
        assert!(!session.is_offline());
        assert_eq!(
            api.with_authenticated_network(&session, async { Ok("new response") })
                .await
                .unwrap(),
            "new response"
        );
    }

    #[tokio::test]
    async fn track_resources_encode_segments_and_enforce_type_status_and_chunk_bounds() {
        use crate::media::TrackResource::{Artwork, Lyrics, Waveform};

        let oversized = "x".repeat(Waveform.max_bytes() + 1);
        let chunked = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n{oversized}\r\n0\r\n\r\n",
            oversized.len()
        );
        let wire = Wire::new(vec![
            reply(
                200,
                &[("Content-Type", "Image/PNG; charset=binary")],
                "image",
            ),
            reply(200, &[("Content-Type", "image/svg+xml")], "image"),
            reply(
                200,
                &[
                    ("Content-Type", "application/json"),
                    ("Content-Type", "application/json"),
                ],
                "{}",
            ),
            chunked.into_bytes(),
            reply(206, &[("Content-Type", "application/json")], "{}"),
            reply(401, &[], "synthetic error"),
        ]);
        let api = wire.api();
        let session = api.test_session();
        assert_eq!(
            api.track_resource(&session, "x/y% ?#", Artwork)
                .await
                .unwrap(),
            ("image/png".into(), b"image".to_vec())
        );
        for resource in [Artwork, Lyrics, Waveform, Lyrics] {
            assert_eq!(
                failure(api.track_resource(&session, "id", resource).await),
                INVALID_RESPONSE
            );
        }
        assert_eq!(
            failure(api.track_resource(&session, "..", Lyrics).await),
            INVALID_INPUT
        );
        assert_eq!(
            failure(api.track_resource(&session, "id", Lyrics).await),
            EXPIRED
        );
        assert!(session.is_expired());
        let requests = wire.recorded();
        assert_eq!(requests.len(), 6);
        assert_eq!(
            requests[0].first_line,
            "GET /api/library/tracks/x%2Fy%25%20%3F%23/artwork HTTP/1.1"
        );
        assert_eq!(requests[0].header("accept"), Some("image/png, image/jpeg"));
        assert_eq!(
            requests[3].first_line,
            "GET /api/library/tracks/id/waveform HTTP/1.1"
        );
        assert!(requests.iter().all(|request| {
            request.header("cookie") == Some("spofify_session=synthetic")
                && request.header("origin") == Some(wire.origin.as_str())
                && request.header("x-csrf-token").is_none()
        }));
    }

    #[tokio::test]
    async fn catalog_artwork_uses_typed_routes_and_only_404_is_absence() {
        use catalog::CatalogKind::{Album, Artist, Track};
        let oversized = format!(
            "HTTP/1.1 200 Test\r\nContent-Length: {}\r\nContent-Type: image/png\r\nConnection: close\r\n\r\n",
            2 * 1024 * 1024 + 1
        );
        let wire = Wire::new(vec![
            reply(200, &[("Content-Type", "image/jpeg")], "jpeg"),
            reply(404, &[], "no cover"),
            oversized.into_bytes(),
            reply(403, &[], "forbidden"),
        ]);
        let api = wire.api();
        let session = api.test_session();
        assert_eq!(
            api.catalog_artwork(&session, Album, "al%?").await.unwrap(),
            Some(("image/jpeg".into(), b"jpeg".to_vec()))
        );
        assert_eq!(
            api.catalog_artwork(&session, Artist, "ar").await.unwrap(),
            None
        );
        assert_eq!(
            failure(api.catalog_artwork(&session, Track, "tr").await),
            INVALID_RESPONSE
        );
        assert_eq!(
            failure(api.catalog_artwork(&session, Artist, "ar").await).status,
            403
        );
        let requests = wire.recorded();
        assert_eq!(
            requests[0].first_line,
            "GET /api/library/albums/al%25%3F/artwork HTTP/1.1"
        );
        assert_eq!(
            requests[1].first_line,
            "GET /api/library/artists/ar/artwork HTTP/1.1"
        );
        assert_eq!(
            requests[2].first_line,
            "GET /api/library/tracks/tr/artwork HTTP/1.1"
        );
        assert_eq!(requests[2].header("accept"), Some("image/png, image/jpeg"));
    }

    #[tokio::test]
    async fn offline_audio_is_pinned_to_exact_origin_version_and_representation() {
        let version = format!("sha256:{}", "0".repeat(64));
        let etag = format!("\"{version}\"");
        let weak = format!("W/{etag}");
        let wire = Wire::new(vec![
            json(r#"{"version":1}"#),
            reply(
                200,
                &[("Content-Type", "audio/flac"), ("ETag", &etag)],
                "beep",
            ),
            reply(
                200,
                &[("Content-Type", "audio/flac"), ("ETag", &weak)],
                "beep",
            ),
            reply(
                200,
                &[
                    ("Content-Type", "audio/flac; charset=binary"),
                    ("ETag", &etag),
                ],
                "beep",
            ),
            reply(
                200,
                &[("Content-Type", "audio/flac"), ("ETag", &etag)],
                "beep!",
            ),
            reply(409, &[], "media changed"),
        ]);
        let api = wire.api();
        let session = api.test_session();
        let id = "track% ?#";
        assert_eq!(
            api.offline_manifest(&session, id).await.unwrap(),
            br#"{"version":1}"#
        );
        assert_eq!(
            api.offline_audio(&session, id, &version, "audio/flac", 4)
                .await
                .unwrap(),
            b"beep"
        );
        for _ in 0..3 {
            assert_eq!(
                failure(
                    api.offline_audio(&session, id, &version, "audio/flac", 4)
                        .await
                ),
                INVALID_RESPONSE
            );
        }
        assert_eq!(
            failure(
                api.offline_audio(&session, id, &version, "audio/flac", 4)
                    .await
            )
            .status,
            409
        );
        assert_eq!(
            failure(
                api.offline_audio(&session, id, "sha256:unbound", "audio/flac", 4)
                    .await
            ),
            INVALID_INPUT
        );
        assert_eq!(
            failure(
                api.offline_audio(&session, id, &version, "audio/flac", MAX_MEDIA + 1)
                    .await
            ),
            INVALID_INPUT
        );
        session.set_offline(true);
        assert_eq!(failure(api.offline_manifest(&session, id).await), OFFLINE);
        let requests = wire.recorded();
        assert_eq!(requests.len(), 6);
        assert_eq!(
            requests[0].first_line,
            "GET /api/me/offline/v1/tracks/track%25%20%3F%23/media HTTP/1.1"
        );
        assert_eq!(
            requests[1].first_line,
            format!(
                "GET /api/me/offline/v1/tracks/track%25%20%3F%23/audio?version=sha256%3A{} HTTP/1.1",
                "0".repeat(64)
            )
        );
        assert_eq!(requests[1].header("accept"), Some("audio/flac"));
        assert!(requests[1].header("range").is_none());
        assert!(requests.iter().all(|request| {
            request.header("cookie") == Some("spofify_session=synthetic")
                && request.header("origin") == Some(wire.origin.as_str())
        }));
    }

    #[tokio::test]
    async fn offline_collections_use_only_fixed_manifest_routes() {
        let wire = Wire::new(vec![json("{}"), json("{}"), json("{}")]);
        let api = wire.api();
        let session = api.test_session();
        for (kind, id, cursor) in [
            ("albums", "album %?", None),
            ("artists", "artist:1", Some("opaque&=cursor")),
            ("playlists", "pl1_demo", Some("of1_ok.test")),
        ] {
            assert_eq!(
                api.offline_collection_manifest(&session, kind, id, cursor)
                    .await
                    .unwrap(),
                b"{}"
            );
        }
        assert_eq!(
            failure(
                api.offline_collection_manifest(&session, "tracks", "t", None)
                    .await
            ),
            INVALID_INPUT
        );
        assert_eq!(
            failure(
                api.offline_collection_manifest(&session, "playlists", "not/a/playlist", None)
                    .await
            ),
            INVALID_INPUT
        );
        assert_eq!(
            failure(
                api.offline_collection_manifest(&session, "artists", "ar", Some("\n"))
                    .await
            ),
            INVALID_INPUT
        );
        let requests = wire.recorded();
        assert_eq!(requests.len(), 3);
        assert_eq!(
            requests[0].first_line,
            "GET /api/me/offline/v1/albums/album%20%25%3F/manifest?limit=20 HTTP/1.1"
        );
        assert_eq!(
            requests[1].first_line,
            "GET /api/me/offline/v1/artists/artist:1/manifest?limit=20&cursor=opaque%26%3Dcursor HTTP/1.1"
        );
        assert_eq!(
            requests[2].first_line,
            "GET /api/me/offline/v1/playlists/pl1_demo/manifest?limit=20&cursor=of1_ok.test HTTP/1.1"
        );
    }

    const SOCKET_TICKET: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn socket_reply(request: &Recorded, extra: &str) -> Vec<u8> {
        use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
        let key = request.header("sec-websocket-key").unwrap();
        let accept = derive_accept_key(key.as_bytes());
        let protocol = crate::socket::SUBPROTOCOL;
        format!(
            "HTTP/1.1 101 Switching Protocols\r\nConnection: keep-alive, Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: {accept}\r\nSec-WebSocket-Protocol: {protocol}\r\n{extra}\r\n"
        ).into_bytes()
    }

    #[tokio::test]
    async fn account_upgrade_uses_csrf_ticket_and_exact_credentialed_handshake() {
        let wire = Wire::with_responder(
            vec![
                json(&format!(r#"{{"ticket":"{SOCKET_TICKET}"}}"#)),
                Vec::new(),
            ],
            |request, response| {
                if response.is_empty() {
                    socket_reply(request, "")
                } else {
                    response
                }
            },
        );
        let api = wire.api();
        let session = api.test_session();
        let upgrade = api.account_upgrade(&session).await.unwrap();
        assert_eq!(upgrade.ticket, SOCKET_TICKET);
        let requests = wire.recorded();
        let [ticket, socket] = requests.as_slice() else {
            panic!("expected exactly two requests")
        };
        assert_eq!(ticket.first_line, "POST /api/me/player/connect HTTP/1.1");
        assert_eq!(ticket.body, b"{}");
        assert_eq!(ticket.header("x-csrf-token"), Some("synthetic"));
        assert_eq!(ticket.header("content-type"), Some("application/json"));
        assert_eq!(socket.first_line, "GET /api/me/player/socket HTTP/1.1");
        assert_eq!(
            socket.header("sec-websocket-protocol"),
            Some("spofify.account-state.v1")
        );
        assert_eq!(socket.header("sec-websocket-version"), Some("13"));
        assert!(socket.body.is_empty());
        assert!(socket.header("x-csrf-token").is_none());
        assert!(
            socket
                .headers
                .iter()
                .all(|(_, value)| !value.contains(SOCKET_TICKET))
        );
        for request in [ticket, socket] {
            assert_eq!(request.header("cookie"), Some("spofify_session=synthetic"));
            assert_eq!(request.header("origin"), Some(wire.origin.as_str()));
        }
    }

    #[tokio::test]
    async fn account_upgrade_rejects_bad_ticket_and_bad_handshake_and_revokes_on_401() {
        let wire = Wire::new(vec![json(r#"{"ticket":"short"}"#)]);
        let api = wire.api();
        let session = api.test_session();
        assert_eq!(
            failure(api.account_upgrade(&session).await),
            INVALID_RESPONSE
        );
        assert_eq!(wire.recorded().len(), 1);

        let ticket = json(&format!(r#"{{"ticket":"{SOCKET_TICKET}"}}"#));
        let wire = Wire::with_responder(vec![ticket.clone(), Vec::new()], |request, response| {
            if response.is_empty() {
                socket_reply(request, "Sec-WebSocket-Extensions: permessage-deflate\r\n")
            } else {
                response
            }
        });
        let api = wire.api();
        let session = api.test_session();
        assert_eq!(
            failure(api.account_upgrade(&session).await),
            INVALID_RESPONSE
        );
        assert_eq!(wire.recorded().len(), 2);

        let wire = Wire::with_responder(vec![ticket.clone(), Vec::new()], |request, response| {
            if response.is_empty() {
                String::from_utf8(socket_reply(request, ""))
                    .unwrap()
                    .replace("spofify.account-state.v1", "spofify.library-playback.v2")
                    .into_bytes()
            } else {
                response
            }
        });
        let api = wire.api();
        let session = api.test_session();
        assert_eq!(
            failure(api.account_upgrade(&session).await),
            INVALID_RESPONSE
        );
        assert_eq!(wire.recorded().len(), 2);

        let wire = Wire::new(vec![ticket, reply(401, &[], "synthetic error")]);
        let api = wire.api();
        let session = api.test_session();
        assert_eq!(failure(api.account_upgrade(&session).await), EXPIRED);
        assert!(session.is_expired());
        assert_eq!(failure(api.account_upgrade(&session).await), EXPIRED);
        assert_eq!(wire.recorded().len(), 2);
    }

    #[test]
    fn explicit_http_and_https_accept_canonical_origins() {
        assert_eq!(
            Origin::parse(" HTTPS://Example.COM:443/ ")
                .unwrap()
                .as_str(),
            "https://example.com"
        );
        assert_eq!(
            Origin::parse("http://127.0.0.1:80/").unwrap().as_str(),
            "http://127.0.0.1"
        );
        for host in [
            "127.0.0.1",
            "127.255.255.254",
            "10.0.0.0",
            "172.16.0.0",
            "172.31.255.255",
            "192.168.1.2",
            "100.64.0.0",
            "100.127.255.255",
            "[::1]",
            "[fc00::]",
            "[fd7a:115c:a1e0::1]",
            "localhost",
            "music.local",
            "example.com",
            "203.0.113.7",
            "[2001:db8::1]",
        ] {
            assert!(Origin::parse(&format!("http://{host}")).is_ok(), "{host}");
        }
        for host in [
            "0.0.0.0",
            "127.1",
            "2130706433",
            "0x7f000001",
            "0177.0.0.1",
            "[::]",
            "[fc00::1%25eth0]",
        ] {
            assert!(Origin::parse(&format!("http://{host}")).is_err(), "{host}");
        }
        for origin in [
            "https://u:p@example.com",
            "https://@example.com",
            "https://example.com/a",
            "https://example.com/..",
            "https://example.com//",
            "https://example.com/%2f",
            "https://example.com?",
            "https://example.com/#",
            "https://example.com\\x",
            "https://exa\nmple.com",
            "https://example.com:",
            "https://example.com:99999",
            "https://example.com:0",
            "ftp://example.com",
            "example.com",
        ] {
            assert!(Origin::parse(origin).is_err(), "{origin}");
        }
    }

    #[test]
    fn cookie_policy_rejects_missing_unsafe_duplicate_or_expired_attributes() {
        let origin = Origin::parse("http://127.0.0.1:1234").unwrap();
        let check = |cookie: &str| {
            let mut headers = header::HeaderMap::new();
            headers.insert(header::SET_COOKIE, HeaderValue::from_str(cookie).unwrap());
            session_cookie(&origin, &headers).is_ok()
        };
        assert!(check(VALID_COOKIE));
        assert!(check(
            "spofify_session=x; Path=/; HttpOnly; Expires=Wed, 01 Jan 2031 00:00:00 GMT"
        ));
        for cookie in [
            "spofify_session=x; Path=/; HttpOnly",
            "spofify_session=x; Path=/; Max-Age=3600",
            "spofify_session=x; HttpOnly; Max-Age=3600",
            "spofify_session=x; Path=/api; HttpOnly; Max-Age=3600",
            "spofify_session=x; Path=/; HttpOnly; Max-Age=3600; Secure",
            "spofify_session=x; Path=/; HttpOnly; Max-Age=3600; Domain=127.0.0.1",
            "spofify_session=x; Path=/; HttpOnly; Max-Age=3600; Domain=",
            "spofify_session=x; Path=/; Path=/; HttpOnly; Max-Age=3600",
            "spofify_session=x; Path=/; HttpOnly=false; Max-Age=3600",
            "spofify_session=x; Path=/; HttpOnly; Max-Age=0",
            "spofify_session=x; Path=/; HttpOnly; Max-Age=-1",
            "spofify_session=x; Path=/; HttpOnly; Max-Age=bad",
            "spofify_session=x; Path=/; HttpOnly; Expires=Thu, 01 Jan 1970 00:00:01 GMT",
            "spofify_session=x; Path=/; HttpOnly; Expires=invalid",
            "spofify_session=; Path=/; HttpOnly; Max-Age=3600",
            "__Host-spofify_session=x; Path=/; HttpOnly; Max-Age=3600",
        ] {
            assert!(!check(cookie), "accepted unsafe cookie fixture");
        }
        let secure = Origin::parse("https://example.com").unwrap();
        let mut headers = header::HeaderMap::new();
        headers.insert(
            header::SET_COOKIE,
            HeaderValue::from_static(
                "__Host-spofify_session=x; Path=/; HttpOnly; Secure; Max-Age=3600",
            ),
        );
        assert!(session_cookie(&secure, &headers).is_ok());
        headers.append(
            header::SET_COOKIE,
            headers.get(header::SET_COOKIE).unwrap().clone(),
        );
        assert!(session_cookie(&secure, &headers).is_err());
        headers.clear();
        headers.insert(header::SET_COOKIE, HeaderValue::from_static(VALID_COOKIE));
        assert!(session_cookie(&secure, &headers).is_err());
    }

    #[tokio::test]
    async fn login_search_and_logout_match_wire_contract_without_leaking_private_paths() {
        let wire = Wire::new(vec![login_reply(), json(RESULTS), reply(204, &[], "")]);
        let api = wire.api();
        let session = api.login("listener", "synthetic-password").await.unwrap();
        assert_eq!(session.username, "listener");
        let page = api.search(&session, "  A & B/é%  ").await.unwrap();
        assert_eq!(page.limit, 20);
        assert_eq!(page.items.len(), 1);
        let track = &page.items[0];
        assert_eq!(track.artists, "Artist One, Artist Two");
        assert!(track.has_audio);
        assert_eq!(track.duration_ms, 123000);
        assert!(!format!("{page:?}").contains("/private/"));
        let clone = session.clone();
        api.logout(&session).await.unwrap();
        assert!(failure(api.search(&clone, "more").await).expired);
        let requests = wire.recorded();
        assert_eq!(requests.len(), 3);
        let login = &requests[0];
        assert_eq!(login.first_line, "POST /api/auth/login HTTP/1.1");
        assert_eq!(login.header("origin"), Some(wire.origin.as_str()));
        assert_eq!(login.header("x-spofify-login"), Some("1"));
        assert_eq!(login.header("x-spofify-session-mode"), None);
        assert_eq!(login.header("cookie"), None);
        assert_eq!(login.header("x-csrf-token"), None);
        let body: serde_json::Value = serde_json::from_slice(&login.body).unwrap();
        assert_eq!(
            body,
            serde_json::json!({"username":"listener","password":"synthetic-password"})
        );
        let search = &requests[1];
        let target = search.first_line.split_whitespace().nth(1).unwrap();
        let url = Url::parse(&format!("{}{target}", wire.origin.as_str())).unwrap();
        let query: std::collections::BTreeMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(
            query,
            [
                ("kind", "tracks"),
                ("scope", "all"),
                ("availability", "in-library"),
                ("q", "A & B/é%"),
                ("limit", "20"),
                ("offset", "0")
            ]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect()
        );
        assert_eq!(
            search.header("cookie"),
            Some("spofify_session=cookie-secret")
        );
        assert_eq!(search.header("x-csrf-token"), None);
        let logout = &requests[2];
        assert_eq!(logout.first_line, "POST /api/auth/logout HTTP/1.1");
        assert_eq!(logout.header("x-csrf-token"), Some("csrf-secret"));
        assert_eq!(logout.header("cookie"), search.header("cookie"));
        assert_eq!(logout.header("origin"), Some(wire.origin.as_str()));
        assert_eq!(logout.body, b"{}");
    }

    #[tokio::test]
    async fn empty_or_invalid_search_and_cross_client_session_never_contact_server() {
        let wire = Wire::new(vec![login_reply()]);
        let api = wire.api();
        let session = api.login("listener", "password").await.unwrap();
        for query in ["", "   "] {
            assert!(api.search(&session, query).await.unwrap().items.is_empty());
        }
        for query in ["a\nb".to_owned(), "x".repeat(201), "é".repeat(201)] {
            assert_eq!(failure(api.search(&session, &query).await), INVALID_INPUT);
        }
        assert!(wire.api().search(&session, "track").await.is_err());
        assert!(api.login("listener", &"x".repeat(2049)).await.is_err());
        assert_eq!(wire.recorded().len(), 1);
    }

    #[tokio::test]
    async fn unauthorized_revokes_clones_but_failed_login_is_not_session_expiry() {
        let malicious = r#"{"error":"csrf-secret cookie-secret /private/path password"}"#;
        let wire = Wire::new(vec![
            reply(401, &[], malicious),
            login_reply(),
            reply(401, &[], malicious),
        ]);
        let api = wire.api();
        let error = failure(api.login("listener", "password").await);
        assert!(!error.expired);
        assert_eq!(error.message, "Invalid username or password.");
        let session = api.login("listener", "password").await.unwrap();
        let clone = session.clone();
        assert!(failure(api.search(&session, "track").await).expired);
        let error = failure(api.search(&clone, "track").await);
        assert!(error.expired);
        assert!(!format!("{error:?} {error}").contains("secret"));
        assert_eq!(wire.recorded().len(), 3);
    }

    #[tokio::test]
    async fn failed_or_cancelled_logout_cannot_leave_a_usable_clone() {
        let wire = Wire::new(vec![
            login_reply(),
            reply(503, &[("Retry-After", "0")], "secret"),
        ]);
        let api = wire.api();
        let session = api.login("listener", "password").await.unwrap();
        let clone = session.clone();
        assert!(api.logout(&session).await.is_err());
        assert!(failure(api.search(&clone, "track").await).expired);
        assert_eq!(wire.recorded().len(), 2);

        let wire = Wire::new(vec![login_reply(), Vec::new()]);
        let api = wire.api();
        let session = api.login("listener", "password").await.unwrap();
        let logout = api.logout(&session);
        tokio::pin!(logout);
        // Poll once into the request await, then drop the cancellation future.
        tokio::select! {
            _ = &mut logout => {},
            _ = tokio::time::sleep(Duration::from_millis(1)) => {},
        }
        assert!(failure(api.search(&session, "track").await).expired);
    }

    #[tokio::test]
    async fn redirects_never_forward_cookie_or_credentials_and_statuses_never_retry() {
        let destination = Wire::new(vec![]);
        let location = format!("{}/stolen", destination.origin.as_str());
        let wire = Wire::new(vec![
            login_reply(),
            reply(302, &[("Location", &location)], ""),
            reply(503, &[("Retry-After", "0")], "secret"),
            Vec::new(),
        ]);
        let api = wire.api();
        let session = api.login("listener", "password").await.unwrap();
        assert!(!failure(api.search(&session, "track").await).expired);
        assert_eq!(destination.recorded().len(), 0);
        assert!(api.login("listener", "password").await.is_err());
        assert_eq!(wire.recorded().len(), 3);
        assert!(api.login("listener", "password").await.is_err());
        assert_eq!(wire.recorded().len(), 4);
    }

    #[tokio::test]
    async fn malformed_or_oversized_json_pages_are_rejected() {
        let mut bad = vec![
            json("not JSON secret"),
            reply(200, &[("Content-Type", "text/html")], RESULTS),
            json(&RESULTS.replace("\"limit\":20", "\"limit\":100")),
            json(&RESULTS.replace("\"offset\":0", "\"offset\":1")),
            json(&RESULTS.replace("\"total\":1", "\"total\":0")),
            json(&RESULTS.replace("\"kind\":\"track\"", "\"kind\":\"album\"")),
            json(&RESULTS.replace("\"durationMs\":123000", "\"durationMs\":-1")),
            json(&RESULTS.replace("First track", &"x".repeat(1025))),
            json(&" ".repeat(MAX_BODY + 1)),
            json(r#"{"items":null,"total":0,"offset":0,"limit":20}"#),
        ];
        let count = bad.len();
        bad.insert(0, login_reply());
        let wire = Wire::new(bad);
        let api = wire.api();
        let session = api.login("listener", "password").await.unwrap();
        for _ in 0..count {
            assert_eq!(
                failure(api.search(&session, "track").await),
                INVALID_RESPONSE
            );
        }
        assert_eq!(wire.recorded().len(), count + 1);
    }

    #[tokio::test]
    async fn login_requires_valid_summary_and_exactly_one_valid_cookie() {
        let responses = vec![
            json(SUMMARY),
            reply(
                200,
                &[
                    ("Content-Type", "application/json"),
                    ("Set-Cookie", VALID_COOKIE),
                    ("Set-Cookie", VALID_COOKIE),
                ],
                SUMMARY,
            ),
            reply(
                200,
                &[
                    ("Content-Type", "application/json"),
                    ("Set-Cookie", VALID_COOKIE),
                ],
                &SUMMARY.replace("\"user\"", "\"root\""),
            ),
            reply(
                200,
                &[
                    ("Content-Type", "application/json"),
                    ("Set-Cookie", VALID_COOKIE),
                ],
                &SUMMARY.replace("csrf-secret", ""),
            ),
            reply(
                200,
                &[
                    ("Content-Type", "application/json"),
                    ("Set-Cookie", VALID_COOKIE),
                ],
                &SUMMARY.replace("csrf-secret", "csrf\\r\\nInjected"),
            ),
        ];
        let count = responses.len();
        let wire = Wire::new(responses);
        let api = wire.api();
        for _ in 0..count {
            assert_eq!(
                failure(api.login("listener", "password").await),
                INVALID_RESPONSE
            );
        }
        assert_eq!(wire.recorded().len(), count);
    }

    #[tokio::test]
    async fn account_ticket_failures_are_bounded_redacted_and_never_retried() {
        let ticket = format!(r#"{{"ticket":"{SOCKET_TICKET}"}}"#);
        let wire = Wire::new(vec![
            json(&format!("{ticket}{}", " ".repeat(TICKET_BODY))),
            json(&format!(
                r#"{{"ticket":"{SOCKET_TICKET}","ticket":"{SOCKET_TICKET}"}}"#
            )),
            reply(409, &[], "private diagnostic and synthetic secret"),
            reply(401, &[], "private diagnostic"),
        ]);
        let api = wire.api();
        let session = api.test_session();
        for index in 0..2 {
            assert_eq!(
                failure(api.account_upgrade(&session).await),
                INVALID_RESPONSE
            );
            assert_eq!(wire.recorded().len(), index + 1);
        }
        let err = failure(api.account_upgrade(&session).await);
        assert_eq!(err.status, 409);
        assert!(!err.expired);
        assert!(!format!("{err:?}").contains("secret"));
        assert_eq!(wire.recorded().len(), 3);
        assert_eq!(failure(api.account_upgrade(&session).await), EXPIRED);
        assert_eq!(failure(api.account_upgrade(&session).await), EXPIRED);
        assert_eq!(wire.recorded().len(), 4);
    }

    #[tokio::test]
    async fn media_is_encoded_once_authenticated_and_mime_body_bounded() {
        let oversized = format!("HTTP/1.1 200 Test\r\nContent-Length: {}\r\nContent-Type: audio/flac\r\nConnection: close\r\n\r\n", MAX_MEDIA + 1).into_bytes();
        let wire = Wire::new(vec![
            login_reply(),
            reply(200, &[("Content-Type", "audio/flac")], "synthetic-audio"),
            reply(
                200,
                &[("Content-Type", "application/octet-stream")],
                "synthetic-audio",
            ),
            reply(200, &[("Content-Type", "text/html")], "secret"),
            oversized,
            reply(200, &[("Content-Type", "audio/flac")], ""),
            reply(401, &[], "secret"),
        ]);
        let api = wire.api();
        let session = api.login("listener", "password").await.unwrap();
        assert_eq!(
            api.media(&session, "a/b%2Fc ?#").await.unwrap(),
            b"synthetic-audio"
        );
        assert_eq!(
            api.media(&session, "track").await.unwrap(),
            b"synthetic-audio"
        );
        for _ in 0..3 {
            assert!(api.media(&session, "track").await.is_err());
        }
        assert!(failure(api.media(&session, "track").await).expired);
        assert!(failure(api.media(&session, "track").await).expired);
        let requests = wire.recorded();
        assert_eq!(requests.len(), 7);
        assert_eq!(
            requests[1].first_line,
            "GET /api/library/tracks/a%2Fb%252Fc%20%3F%23/audio HTTP/1.1"
        );
        assert_eq!(
            requests[1].header("cookie"),
            Some("spofify_session=cookie-secret")
        );
        assert_eq!(requests[1].header("x-csrf-token"), None);
        assert_eq!(requests[1].header("origin"), Some(wire.origin.as_str()));
    }

    #[tokio::test]
    async fn chunked_json_bound_and_request_deadline_are_enforced() {
        let body = " ".repeat(MAX_BODY + 1);
        let chunked = format!("HTTP/1.1 200 Test\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n{}\r\n0\r\n\r\n", body.len(), body).into_bytes();
        let wire = Wire::new(vec![login_reply(), chunked, b"STALL".to_vec()]);
        let mut api = wire.api();
        let session = api.login("listener", "password").await.unwrap();
        assert_eq!(
            failure(api.search(&session, "track").await),
            INVALID_RESPONSE
        );
        // The production bound is 15 seconds; exercise the same transport deadline cheaply.
        api.client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .timeout(Duration::from_millis(20))
            .build()
            .unwrap();
        assert_eq!(failure(api.search(&session, "track").await), NETWORK);
        assert_eq!(wire.recorded().len(), 3);
    }

    #[tokio::test]
    async fn expired_cookie_is_not_sent_and_unsolicited_search_cookie_is_ignored() {
        let wire = Wire::new(vec![
            login_reply(),
            reply(
                200,
                &[
                    ("Content-Type", "application/json"),
                    (
                        "Set-Cookie",
                        "spofify_session=forged; Path=/; HttpOnly; Max-Age=3600",
                    ),
                ],
                RESULTS,
            ),
            json(RESULTS),
        ]);
        let api = wire.api();
        let session = api.login("listener", "password").await.unwrap();
        api.search(&session, "one").await.unwrap();
        api.search(&session, "two").await.unwrap();
        assert_eq!(
            wire.recorded()[2].header("cookie"),
            Some("spofify_session=cookie-secret")
        );
        session.lifetime().deadline.instant = Instant::now() - Duration::from_secs(1);
        assert!(failure(api.search(&session, "expired").await).expired);
        assert_eq!(wire.recorded().len(), 3);
    }
}
