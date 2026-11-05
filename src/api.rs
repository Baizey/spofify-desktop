//! Bounded, in-memory native client for the existing auth and read-only library API.
//! No arbitrary request URLs, cookie jar, persistence, redirects, proxies or retries.

use std::{
    collections::HashSet,
    fmt,
    net::IpAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use cookie::{Cookie, Expiration};
use reqwest::{
    Client, RequestBuilder, Response,
    header::{self, HeaderValue},
};
use serde::{Deserialize, Serialize};
use url::Url;

pub const PAGE_SIZE: u32 = 20;
const MAX_BODY: usize = 256 * 1024;
const LOGIN_BODY: usize = 8 * 1024;
const MAX_MEDIA: usize = 32 * 1024 * 1024;
// Go accepts <=2MiB publications. A view can contain the room state plus up to
// 16 directive snapshots/commands: 17*2MiB plus bounded device/outcome envelopes.
const PLAYER_REQUEST: usize = 2 * 1024 * 1024;
const PLAYER_RESPONSE: usize = 36 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

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

const INVALID_ORIGIN: ApiError = error(
    "Enter an HTTPS origin, or an explicit private IP HTTP origin, without credentials, path, query or fragment.",
);
const INVALID_RESPONSE: ApiError = error("The server returned an incompatible API response.");
const INVALID_INPUT: ApiError = error("The request contains invalid or oversized input.");
const NETWORK: ApiError =
    error("Unable to reach the server. Check your connection and server address.");
const EXPIRED: ApiError = ApiError {
    message: "Your session has expired. Sign in again.",
    expired: true,
    status: 401,
};

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
        if url.scheme() == "http" && !numeric.is_some_and(private_ip) {
            return Err(INVALID_ORIGIN);
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
}

fn valid_port_suffix(suffix: &str) -> bool {
    suffix.strip_prefix(':').is_some_and(|port| {
        !port.is_empty()
            && port.bytes().all(|c| c.is_ascii_digit())
            && port.parse::<u16>().is_ok_and(|p| p != 0)
    })
}

fn private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, _, _] = ip.octets();
            ip.is_loopback() || ip.is_private() || (a == 100 && (64..=127).contains(&b))
        }
        IpAddr::V6(ip) => ip.is_loopback() || ip.segments()[0] & 0xfe00 == 0xfc00,
    }
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
    /// Lets the UI/player actor stop existing audio when a clone is revoked or expires,
    /// without requiring another network request or exposing credentials.
    pub fn is_expired(&self) -> bool {
        let expired =
            self.secrets.revoked.load(Ordering::SeqCst) || Instant::now() >= self.secrets.deadline;
        if expired {
            self.secrets.revoked.store(true, Ordering::SeqCst);
        }
        expired
    }
}

struct SessionSecrets {
    owner: Arc<()>,
    cookie: HeaderValue,
    csrf: HeaderValue,
    deadline: Instant,
    revoked: AtomicBool,
}

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
        Session {
            account_id: "synthetic".into(),
            username: "synthetic".into(),
            role: "admin".into(),
            secrets: Arc::new(SessionSecrets {
                owner: self.identity.clone(),
                cookie: HeaderValue::from_static("spofify_session=synthetic"),
                csrf: HeaderValue::from_static("synthetic"),
                deadline: Instant::now() + Duration::from_secs(60),
                revoked: AtomicBool::new(false),
            }),
        }
    }

    pub async fn login(&self, username: &str, password: &str) -> Result<Session, ApiError> {
        if !valid_text(username, 256) || password.is_empty() || password.len() > 2048 {
            return Err(INVALID_INPUT);
        }
        #[derive(Serialize)]
        struct Login<'a> {
            username: &'a str,
            password: &'a str,
        }
        // The server accepts at most 4 KiB, including JSON escaping overhead.
        let body = serde_json::to_vec(&Login { username, password }).map_err(|_| INVALID_INPUT)?;
        if body.len() > 4096 {
            return Err(INVALID_INPUT);
        }
        let request = self
            .request(self.origin.endpoint("/api/auth/login"))?
            .header("X-Spofify-Login", "1")
            .header(header::CONTENT_TYPE, "application/json")
            .body(body);
        let response = self.send(request, None, true).await?;
        if response.status() != 200 {
            return Err(INVALID_RESPONSE);
        }
        let (cookie, deadline) = session_cookie(&self.origin, response.headers())?;
        let summary: SessionSummary = decode(response, LOGIN_BODY).await?;
        if !valid_id(&summary.account_id)
            || !valid_text(&summary.username, 256)
            || !matches!(summary.role.as_str(), "admin" | "user")
            || summary.csrf_token.len() > 512
            || summary.csrf_token.is_empty()
            || !summary
                .csrf_token
                .bytes()
                .all(|b| (0x21..=0x7e).contains(&b))
            || Instant::now() >= deadline
        {
            return Err(INVALID_RESPONSE);
        }
        let mut csrf = HeaderValue::from_str(&summary.csrf_token).map_err(|_| INVALID_RESPONSE)?;
        csrf.set_sensitive(true);
        Ok(Session {
            account_id: summary.account_id,
            username: summary.username,
            role: summary.role,
            secrets: Arc::new(SessionSecrets {
                owner: self.identity.clone(),
                cookie,
                csrf,
                deadline,
                revoked: AtomicBool::new(false),
            }),
        })
    }

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
        self.check_session(session)?;
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
    }

    /// Opaque bounded coordinator command transport. Playback policy and response DTO
    /// validation belong to the player actor. A 409 never grants playback authority:
    /// callers must explicitly reconcile status before attempting any further action.
    pub async fn player(
        &self,
        session: &Session,
        command: serde_json::Value,
    ) -> Result<serde_json::Value, ApiError> {
        self.check_session(session)?;
        if !command.is_object() {
            return Err(INVALID_INPUT);
        }
        let body = serde_json::to_vec(&command).map_err(|_| INVALID_INPUT)?;
        if body.len() > PLAYER_REQUEST {
            return Err(INVALID_INPUT);
        }
        let request = self
            .request(self.origin.endpoint("/api/me/player"))?
            .timeout(Duration::from_secs(4))
            .header(header::COOKIE, session.secrets.cookie.clone())
            .header("X-CSRF-Token", session.secrets.csrf.clone())
            .header(header::CONTENT_TYPE, "application/json")
            .body(body);
        let response = self.send(request, Some(session), false).await?;
        if response.status() != 200 {
            return Err(INVALID_RESPONSE);
        }
        let crate::observation::StrictValue(state) = decode(response, PLAYER_RESPONSE).await?;
        self.check_session(session)?;
        Ok(state)
    }

    /// Observational coordinator view for explicit stale-epoch reconciliation. This
    /// sends no command and never grants permission to execute returned directives;
    /// the player actor must validate/reconcile the view before polling again.
    pub async fn player_view(&self, session: &Session) -> Result<serde_json::Value, ApiError> {
        self.check_session(session)?;
        let request = self
            .request_with_method(reqwest::Method::GET, self.origin.endpoint("/api/me/player"))?
            .timeout(Duration::from_secs(4))
            .header(header::COOKIE, session.secrets.cookie.clone());
        let response = self.send(request, Some(session), false).await?;
        if response.status() != 200 {
            return Err(INVALID_RESPONSE);
        }
        let crate::observation::StrictValue(view) = decode(response, PLAYER_RESPONSE).await?;
        self.check_session(session)?;
        Ok(view)
    }

    /// Fetch one original-format audio file into bounded ephemeral memory. The caller
    /// must obtain coordinator authority before calling and stop audio on session expiry.
    pub async fn media(&self, session: &Session, id: &str) -> Result<Vec<u8>, ApiError> {
        self.check_session(session)?;
        if !valid_id(id) {
            return Err(INVALID_INPUT);
        }
        let mut url = self.origin.0.clone();
        url.path_segments_mut()
            .map_err(|_| INVALID_INPUT)?
            .clear()
            .extend(["api", "library", "tracks", id, "audio"]);
        let request = self
            .request(url)?
            .header(header::COOKIE, session.secrets.cookie.clone())
            .timeout(Duration::from_secs(45));
        let mut response = self.send(request, Some(session), false).await?;
        if response.status() != 200 {
            return Err(INVALID_RESPONSE);
        }
        let media = content_type(&response)?;
        if !(media.starts_with("audio/") && media.len() > "audio/".len())
            && media != "application/octet-stream"
        {
            return Err(error("The server returned an unsupported audio response."));
        }
        if response
            .content_length()
            .is_some_and(|n| n > MAX_MEDIA as u64)
        {
            return Err(error(
                "This track exceeds the 32 MiB in-memory playback limit.",
            ));
        }
        let mut bytes = Vec::new();
        loop {
            self.check_session(session)?;
            let Some(chunk) = response.chunk().await.map_err(|_| NETWORK)? else {
                break;
            };
            if chunk.len() > MAX_MEDIA - bytes.len() {
                return Err(error(
                    "This track exceeds the 32 MiB in-memory playback limit.",
                ));
            }
            extend_bounded(&mut bytes, &chunk, MAX_MEDIA);
        }
        self.check_session(session)?;
        if bytes.is_empty() {
            return Err(error("The server returned an empty audio response."));
        }
        Ok(bytes)
    }

    /// Revokes the local capability before awaiting I/O, even if logout fails or is cancelled.
    pub async fn logout(&self, session: &Session) -> Result<(), ApiError> {
        self.check_session(session)?;
        session.secrets.revoked.store(true, Ordering::SeqCst);
        let request = self
            .request(self.origin.endpoint("/api/auth/logout"))?
            .header(header::COOKIE, session.secrets.cookie.clone())
            .header("X-CSRF-Token", session.secrets.csrf.clone())
            .header(header::CONTENT_TYPE, "application/json")
            .body("{}");
        let response = self.send(request, Some(session), false).await?;
        if response.status() != 204 {
            return Err(INVALID_RESPONSE);
        }
        Ok(())
    }

    fn check_session(&self, session: &Session) -> Result<(), ApiError> {
        // The API-instance capability also prevents cross-origin/cross-client cookie reuse.
        if !Arc::ptr_eq(&self.identity, &session.secrets.owner) {
            return Err(error(
                "This session belongs to a different connection. Sign in again.",
            ));
        }
        if session.is_expired() {
            return Err(EXPIRED);
        }
        Ok(())
    }

    fn request(&self, url: Url) -> Result<RequestBuilder, ApiError> {
        let method = if matches!(
            url.path(),
            "/api/auth/login" | "/api/auth/logout" | "/api/me/player"
        ) {
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
        if url.origin() != self.origin.0.origin()
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(error("Off-origin requests are not allowed."));
        }
        let accept = if url.path().ends_with("/audio") {
            "audio/*, application/octet-stream"
        } else {
            "application/json"
        };
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
        let response = request.send().await.map_err(|_| NETWORK)?;
        if response.status().is_success() {
            return Ok(response);
        }
        // Do not read or retain error bodies (which can contain credentials, URLs or paths).
        let status = response.status().as_u16();
        let mapped = match status {
            401 if login => error("Invalid username or password."),
            401 => {
                if let Some(session) = session {
                    session.secrets.revoked.store(true, Ordering::SeqCst);
                }
                EXPIRED
            }
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
        Err(ApiError { status, ..mapped })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionSummary {
    account_id: String,
    username: String,
    role: String,
    csrf_token: String,
}

/// Only login can install a cookie, and only a single exact-host root cookie is accepted.
fn session_cookie(
    origin: &Origin,
    headers: &header::HeaderMap,
) -> Result<(HeaderValue, Instant), ApiError> {
    let secure = origin.0.scheme() == "https";
    let expected = if secure {
        "__Host-spofify_session"
    } else {
        "spofify_session"
    };
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
            || value.is_empty()
            || value.len() > 512
            || !value.bytes().all(cookie_octet)
            || !attrs.contains("path")
            || cookie.path() != Some("/")
            || cookie.domain().is_some()
            || cookie.http_only() != Some(true)
            || cookie.secure().unwrap_or(false) != secure
        {
            return Err(INVALID_RESPONSE);
        }
        // Max-Age takes precedence over Expires, per RFC 6265. Persistence is required.
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
        let mut header =
            HeaderValue::from_str(&format!("{expected}={value}")).map_err(|_| INVALID_RESPONSE)?;
        header.set_sensitive(true);
        candidate = Some((header, deadline));
    }
    candidate.ok_or(INVALID_RESPONSE)
}

fn cookie_octet(byte: u8) -> bool {
    matches!(byte, 0x21 | 0x23..=0x2b | 0x2d..=0x3a | 0x3c..=0x5b | 0x5d..=0x7e)
}

fn content_type(response: &Response) -> Result<String, ApiError> {
    if response
        .headers()
        .get_all(header::CONTENT_TYPE)
        .iter()
        .count()
        != 1
    {
        return Err(INVALID_RESPONSE);
    }
    response
        .headers()
        .get(header::CONTENT_TYPE)
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

#[derive(Deserialize)]
struct WirePage {
    items: Vec<WireItem>,
    total: u32,
    offset: u32,
    limit: u32,
}

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

#[derive(Deserialize)]
struct Artist {
    id: String,
    name: String,
}
#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Coverage {
    #[serde(default)]
    in_library: u32,
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::VecDeque,
        io::{Read, Write},
        net::TcpListener,
        sync::Mutex,
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
                    // The fixture must accept legitimate full player publications,
                    // while remaining finite (2MiB body + bounded HTTP headers).
                    while bytes.len() <= PLAYER_REQUEST + 8192 && !stopping.load(Ordering::SeqCst) {
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
                        let mut saved = saved.lock().unwrap();
                        saved.push(request);
                        if saved.len() >= 32 {
                            break;
                        }
                        drop(saved);
                        // Empty response deliberately simulates disconnection after the request.
                        let response = responses
                            .pop_front()
                            .unwrap_or_else(|| reply(500, &[], "unexpected extra request"));
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

    // Avoid requiring Debug on the intentionally opaque Session type.
    fn failure<T>(result: Result<T, ApiError>) -> ApiError {
        match result {
            Err(error) => error,
            Ok(_) => panic!("Unexpected API success"),
        }
    }

    #[test]
    fn canonical_origins_and_private_http_only() {
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
        ] {
            assert!(Origin::parse(&format!("http://{host}")).is_ok(), "{host}");
        }
        for host in [
            "localhost",
            "music.local",
            "example.com",
            "8.8.8.8",
            "0.0.0.0",
            "169.254.1.2",
            "100.63.255.255",
            "100.128.0.0",
            "172.15.0.1",
            "172.32.0.1",
            "127.1",
            "2130706433",
            "0x7f000001",
            "0177.0.0.1",
            "[::]",
            "[fe80::1]",
            "[::ffff:127.0.0.1]",
            "[2001:db8::1]",
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
    async fn player_posts_csrf_and_reports_safe_conflict_without_retry() {
        let state = r#"{"revision":1,"devices":[],"activeDeviceId":""}"#;
        let wire = Wire::new(vec![
            login_reply(),
            json(state),
            reply(
                409,
                &[("Content-Type", "application/json")],
                r#"{"error":"cookie-secret /private/path","state":{"untrusted":true}}"#,
            ),
        ]);
        let api = wire.api();
        let session = api.login("listener", "password").await.unwrap();
        let command = serde_json::json!({"action":"status","deviceId":"synthetic-device"});
        assert_eq!(
            api.player(&session, command.clone()).await.unwrap()["revision"],
            1
        );
        let error = failure(api.player(&session, command.clone()).await);
        assert_eq!(error.status, 409);
        assert!(!error.expired);
        assert!(!format!("{error:?}").contains("secret"));
        let requests = wire.recorded();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[1].first_line, "POST /api/me/player HTTP/1.1");
        assert_eq!(requests[1].header("origin"), Some(wire.origin.as_str()));
        assert_eq!(
            requests[1].header("cookie"),
            Some("spofify_session=cookie-secret")
        );
        assert_eq!(requests[1].header("x-csrf-token"), Some("csrf-secret"));
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&requests[1].body).unwrap(),
            command
        );
        assert_eq!(
            failure(api.player(&session, serde_json::json!([])).await),
            INVALID_INPUT
        );
        assert_eq!(
            failure(
                api.player(
                    &session,
                    serde_json::json!({"oversized":"x".repeat(PLAYER_REQUEST)})
                )
                .await
            ),
            INVALID_INPUT
        );
        assert_eq!(wire.recorded().len(), 3);
    }

    #[tokio::test]
    async fn player_control_failures_are_single_attempt_and_auth_loss_revokes() {
        let wire = Wire::new(vec![
            login_reply(),
            reply(403, &[], "private diagnostic"),
            reply(404, &[], "private diagnostic"),
            reply(409, &[], "private diagnostic"),
            reply(401, &[], "private diagnostic"),
        ]);
        let api = wire.api();
        let session = api.login("listener", "password").await.unwrap();
        for (index, status) in [403, 404, 409, 401].into_iter().enumerate() {
            let command = serde_json::json!({"action":"command","deviceId":"controller","epoch":1,"commandId":format!("command-{index}"),"command":{"type":"pause"}});
            let e = failure(api.player(&session, command).await);
            assert_eq!(e.status, status);
            assert_eq!(e.expired, status == 401);
            assert_eq!(wire.recorded().len(), index + 2);
            assert!(!e.message.contains("private"));
        }
        assert!(session.is_expired());
        assert_eq!(failure(api.player_view(&session).await), EXPIRED);
        assert_eq!(wire.recorded().len(), 5);
    }

    #[tokio::test]
    async fn player_view_get_is_observational_authenticated_and_bounded() {
        let state = r#"{"epoch":7,"devices":[],"activeDeviceId":"","directives":[]}"#;
        let wire = Wire::new(vec![
            login_reply(),
            json(state),
            json(&" ".repeat(MAX_BODY + 1)),
        ]);
        let api = wire.api();
        let session = api.login("listener", "password").await.unwrap();
        let view = api.player_view(&session).await.unwrap();
        assert_eq!(view["epoch"], 7);
        assert_eq!(failure(api.player_view(&session).await), INVALID_RESPONSE);
        let requests = wire.recorded();
        assert_eq!(requests.len(), 3);
        for request in &requests[1..] {
            assert_eq!(request.first_line, "GET /api/me/player HTTP/1.1");
            assert_eq!(request.header("origin"), Some(wire.origin.as_str()));
            assert_eq!(
                request.header("cookie"),
                Some("spofify_session=cookie-secret")
            );
            assert_eq!(request.header("accept"), Some("application/json"));
            assert_eq!(request.header("x-csrf-token"), None);
            assert_eq!(request.header("x-spofify-login"), None);
            assert!(request.body.is_empty());
        }
    }

    #[tokio::test]
    async fn player_full_publication_envelopes_duplicate_keys_and_endpoint_bounds() {
        let mut state = crate::player::empty_snapshot();
        state["queue"]=serde_json::Value::Array((0..1000).map(|i| serde_json::json!({"id":format!("track-{i}"),"name":"n".repeat(512),"artists":[{"name":"a".repeat(512)}],"albumName":"album","albumId":"a","duration":24,"key":i+1,"order":i})).collect());
        state["position"] = serde_json::json!(0);
        assert!(crate::observation::valid_state(&state));
        let publication = serde_json::json!({"action":"poll","deviceId":"desktop","epoch":1,"stateSeq":1,"state":state});
        let bytes = publication.to_string().len();
        assert!(bytes > MAX_BODY && bytes < PLAYER_REQUEST);
        let view =
            serde_json::json!({"state":state,"directives":[{"type":"prepare","state":state}]})
                .to_string();
        assert!(view.len() > PLAYER_REQUEST && view.len() < PLAYER_RESPONSE);
        let too_big=format!("HTTP/1.1 200 Test\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n",PLAYER_RESPONSE+1).into_bytes();
        let wire = Wire::new(vec![
            login_reply(),
            json(&view),
            json(r#"{"epoch":1,"epoch":2}"#),
            too_big,
        ]);
        let api = wire.api();
        let session = api.login("listener", "password").await.unwrap();
        let got = api.player(&session, publication).await.unwrap();
        assert_eq!(got["state"]["queue"].as_array().unwrap().len(), 1000);
        assert_eq!(failure(api.player_view(&session).await), INVALID_RESPONSE);
        assert_eq!(failure(api.player_view(&session).await), INVALID_RESPONSE);
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
        let mut session = api.login("listener", "password").await.unwrap();
        api.search(&session, "one").await.unwrap();
        api.search(&session, "two").await.unwrap();
        assert_eq!(
            wire.recorded()[2].header("cookie"),
            Some("spofify_session=cookie-secret")
        );
        Arc::get_mut(&mut session.secrets).unwrap().deadline =
            Instant::now() - Duration::from_secs(1);
        assert!(failure(api.search(&session, "expired").await).expired);
        assert_eq!(wire.recorded().len(), 3);
    }
}
