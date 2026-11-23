//! Exact-origin login and server-revalidated remembered credentials. No storage I/O.

use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use cookie::{Expiration, SameSite, time::OffsetDateTime};
use reqwest::{
    Response,
    header::{self, HeaderValue},
};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use super::{
    Api, ApiError, CredentialDeadline, INVALID_INPUT, INVALID_RESPONSE, LOGIN_BODY, NETWORK,
    NetworkCredentials, Origin, Session, SessionLifetime, SessionSecrets, SessionSummary,
    content_type, decode, decode_with, error, parse_session_cookie, session_cookie,
    session_cookie_name, single_header, valid_id, valid_session_cookie_value, valid_text,
};

const MODE_HEADER: &str = "x-spofify-session-mode";
const ROLLING_V1: &str = "rolling-v1";
const ROLLING_LIFETIME: Duration = Duration::from_secs(30 * 24 * 60 * 60);
const NOT_REMEMBERED: ApiError = error("This sign-in did not enable Remember me.");
const TRANSIENT_NETWORK: ApiError = error("The connection to the server was interrupted.");
const WRONG_ORIGIN: ApiError = error("The remembered sign-in is for a different server address.");
const WRONG_ACCOUNT: ApiError = ApiError {
    message: "The server returned a different account. Sign in again.",
    expired: true,
    status: 0,
};

impl ApiError {
    pub(crate) fn can_retry_restore(self) -> bool {
        self == TRANSIENT_NETWORK || crate::api_transport::is_transient_status(self.status)
    }

    pub(crate) fn rejects_local_grant(self) -> bool {
        self.expired
            || matches!(self.status, 401 | 403)
            || matches!(self, INVALID_RESPONSE | INVALID_INPUT | WRONG_ORIGIN)
    }
}

fn restore_transport(error: reqwest::Error) -> ApiError {
    if crate::api_transport::is_transient_error(&error) {
        TRANSIENT_NETWORK
    } else {
        NETWORK
    }
}

/// Legacy online keyring record. A bearer and account binding are not local authority.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RememberedCredential {
    origin: String,
    account_id: String,
    cookie: String,
}

impl RememberedCredential {
    fn header(&self, origin: &Origin) -> Result<HeaderValue, ApiError> {
        if self.origin != origin.as_str() {
            return Err(WRONG_ORIGIN);
        }
        if !valid_id(&self.account_id) {
            return Err(INVALID_INPUT);
        }
        let (name, value) = self.cookie.split_once('=').ok_or(INVALID_INPUT)?;
        if name != session_cookie_name(origin) || !valid_session_cookie_value(value) {
            return Err(INVALID_INPUT);
        }
        let mut cookie = HeaderValue::from_str(&self.cookie).map_err(|_| INVALID_INPUT)?;
        cookie.set_sensitive(true);
        Ok(cookie)
    }
}

#[derive(Clone, Copy, Serialize, Deserialize)]
enum LocalFormat {
    #[serde(rename = "local-v1")]
    ManualOffline,
    #[serde(rename = "local-online-v1")]
    AutoOffline,
}

/// Authority derived from an accepted, remembered server session. The format
/// records the startup preference, not permission to open a transport. No
/// password or CSRF is stored; the cookie is only for separate HTTP validation.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct LocalGrant {
    format: LocalFormat,
    credential: RememberedCredential,
    username: String,
    role: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub(crate) enum RememberedLogin {
    Online(RememberedCredential),
    Offline(LocalGrant),
}

impl RememberedLogin {
    pub(crate) fn credential(&self) -> &RememberedCredential {
        match self {
            Self::Online(credential) => credential,
            Self::Offline(grant) => &grant.credential,
        }
    }
}

impl LocalGrant {
    pub(crate) fn manual_offline(&self) -> bool {
        matches!(self.format, LocalFormat::ManualOffline)
    }

    pub(crate) fn set_manual_offline(&mut self, manual: bool) {
        self.format = if manual {
            LocalFormat::ManualOffline
        } else {
            LocalFormat::AutoOffline
        };
    }

    pub(crate) fn matches_principal(&self, username: &str, account_id: &str) -> bool {
        self.username == username && self.credential.account_id == account_id
    }

    pub(crate) fn validate(&self, origin: &Origin, username: &str) -> Result<(), ApiError> {
        self.credential.header(origin)?;
        if self.username != username
            || !valid_text(&self.username, 256)
            || !matches!(self.role.as_str(), "admin" | "user")
        {
            return Err(INVALID_INPUT);
        }
        Ok(())
    }

    pub(crate) fn admit(
        &self,
        origin: &Origin,
        username: &str,
    ) -> Result<(Api, Session), ApiError> {
        self.validate(origin, username)?;
        let api = Api {
            origin: origin.clone(),
            client: None,
            audio_client: None,
            identity: Arc::new(()),
        };
        let session = Session {
            account_id: self.credential.account_id.clone(),
            username: self.username.clone(),
            role: self.role.clone(),
            secrets: Arc::new(SessionSecrets {
                owner: api.identity.clone(),
                account_id: self.credential.account_id.clone(),
                network: watch::Sender::new(1),
                credentials: None,
                lifetime: Mutex::new(SessionLifetime {
                    // Local authority has no usable network deadline (or transport).
                    deadline: CredentialDeadline {
                        instant: Instant::now(),
                        server_expires: None,
                    },
                    revocation: None,
                }),
            }),
        };
        Ok((api, session))
    }
}

impl Session {
    /// Match a retained record against the immutable account binding, not the
    /// mutable UI summary. Callers separately check the exact slot and origin.
    pub(crate) fn remembered_account_id(&self) -> &str {
        &self.secrets.account_id
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RollingLoginSummary {
    session_mode: String,
    #[serde(flatten)]
    summary: SessionSummary,
}

impl Api {
    pub async fn login(&self, username: &str, password: &str) -> Result<Session, ApiError> {
        self.password_login(username, password, false).await
    }

    /// Explicit opt-in. Never silently fall back to an ordinary login or retry a password.
    pub async fn login_remembered(
        &self,
        username: &str,
        password: &str,
    ) -> Result<Session, ApiError> {
        self.password_login(username, password, true).await
    }

    async fn password_login(
        &self,
        username: &str,
        password: &str,
        remembered: bool,
    ) -> Result<Session, ApiError> {
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
        let mut request = self
            .request(self.origin.endpoint("/api/auth/login"))?
            .header("X-Spofify-Login", "1")
            .header(header::CONTENT_TYPE, "application/json")
            .body(body);
        if remembered {
            request = request.header(MODE_HEADER, ROLLING_V1);
        }
        let response = self.send(request, None, true).await?;
        if response.status() != 200 {
            return Err(INVALID_RESPONSE);
        }
        if remembered {
            let (cookie, deadline) = rolling_cookie(&self.origin, &response)?;
            let login: RollingLoginSummary = decode(response, LOGIN_BODY).await?;
            if login.session_mode != ROLLING_V1 {
                return Err(INVALID_RESPONSE);
            }
            self.session_from_summary(login.summary, cookie, deadline)
        } else {
            let (cookie, instant) = session_cookie(&self.origin, response.headers())?;
            let summary: SessionSummary = decode(response, LOGIN_BODY).await?;
            let deadline = CredentialDeadline {
                instant,
                server_expires: None,
            };
            self.session_from_summary(summary, cookie, deadline)
        }
    }

    /// Prepare a typed auto-offline record from a fresh remembered sign-in or
    /// an HTTP-validated legacy login. Only the engine's ordered writer may
    /// persist it after admitting that account.
    pub(crate) fn remembered_login(&self, session: &Session) -> Result<RememberedLogin, ApiError> {
        // A new grant requires a live, non-revoked rolling credential. A
        // previously installed grant can outlast the cookie and change its
        // preference without exporting a new credential.
        let grant = LocalGrant {
            format: LocalFormat::AutoOffline,
            credential: self.remembered_credential(session)?,
            username: session.username.clone(),
            role: session.role.clone(),
        };
        grant.validate(&self.origin, &session.username)?;
        Ok(RememberedLogin::Offline(grant))
    }

    /// Export only an active, negotiated session owned by this connection.
    pub fn remembered_credential(
        &self,
        session: &Session,
    ) -> Result<RememberedCredential, ApiError> {
        self.check_session_owner(session)?;
        let lifetime = session.lifetime();
        lifetime.check_live()?;
        if lifetime.deadline.server_expires.is_none() {
            return Err(NOT_REMEMBERED);
        }
        Ok(RememberedCredential {
            origin: self.origin.as_str().to_owned(),
            account_id: session.secrets.account_id.clone(),
            cookie: session
                .cookie()?
                .to_str()
                .map_err(|_| INVALID_RESPONSE)?
                .to_owned(),
        })
    }

    /// A saved cookie by itself grants no local or network session.
    /// The caller owns cancellation and fences the result against newer sign-in intents.
    #[cfg(test)]
    pub async fn resume(&self, credential: &RememberedCredential) -> Result<Session, ApiError> {
        self.resume_if(credential, || true).await
    }

    pub(crate) async fn resume_if(
        &self,
        credential: &RememberedCredential,
        current: impl FnOnce() -> bool,
    ) -> Result<Session, ApiError> {
        let saved_cookie = credential.header(&self.origin)?;
        let request = self
            .request_with_method(
                reqwest::Method::GET,
                self.origin.endpoint("/api/auth/session"),
            )?
            .header(header::CACHE_CONTROL, "no-cache, no-store")
            .header(header::COOKIE, saved_cookie.clone());
        // Use the same origin-bound client as other requests, but preserve
        // transport classification for this bounded, replayable GET only.
        if !current() {
            return Err(super::OFFLINE);
        }
        let response = request.send().await.map_err(restore_transport)?;
        if !response.status().is_success() {
            return Err(Self::response_error(response.status().as_u16(), false));
        }
        let (cookie, deadline) = rolling_cookie(&self.origin, &response)?;
        if cookie != saved_cookie {
            return Err(INVALID_RESPONSE); // rolling-v1 never replaces the bearer
        }
        let summary: SessionSummary = decode_with(response, LOGIN_BODY, restore_transport).await?;
        // The GET has no sessionMode field in its body; its response header is authoritative.
        let session = self.session_from_summary(summary, cookie, deadline)?;
        if session.secrets.account_id != credential.account_id {
            return Err(WRONG_ACCOUNT);
        }
        Ok(session)
    }

    fn session_from_summary(
        &self,
        summary: SessionSummary,
        cookie: HeaderValue,
        deadline: CredentialDeadline,
    ) -> Result<Session, ApiError> {
        if !valid_id(&summary.account_id)
            || !valid_text(&summary.username, 256)
            || !matches!(summary.role.as_str(), "admin" | "user")
            || summary.csrf_token.len() > 512
            || summary.csrf_token.is_empty()
            || !summary
                .csrf_token
                .bytes()
                .all(|b| (0x21..=0x7e).contains(&b))
            || deadline.elapsed()
        {
            return Err(INVALID_RESPONSE);
        }
        let mut csrf = HeaderValue::from_str(&summary.csrf_token).map_err(|_| INVALID_RESPONSE)?;
        csrf.set_sensitive(true);
        Ok(Session {
            account_id: summary.account_id.clone(),
            username: summary.username,
            role: summary.role,
            secrets: Arc::new(SessionSecrets {
                owner: self.identity.clone(),
                account_id: summary.account_id,
                network: watch::Sender::new(0),
                credentials: Some(NetworkCredentials { cookie, csrf }),
                lifetime: Mutex::new(SessionLifetime {
                    deadline,
                    revocation: None,
                }),
            }),
        })
    }

    pub(super) fn rolling_publication(
        &self,
        session: &Session,
        headers: &header::HeaderMap,
    ) -> Result<CredentialDeadline, ApiError> {
        let (cookie, deadline) = parse_rolling_publication(&self.origin, headers)?;
        if &cookie != session.cookie()? {
            return Err(INVALID_RESPONSE);
        }
        Ok(deadline)
    }
}

fn rolling_cookie(
    origin: &Origin,
    response: &Response,
) -> Result<(HeaderValue, CredentialDeadline), ApiError> {
    if response.status() != 200 || content_type(response)? != "application/json" {
        return Err(INVALID_RESPONSE);
    }
    parse_rolling_publication(origin, response.headers())
}

fn parse_rolling_publication(
    origin: &Origin,
    headers: &header::HeaderMap,
) -> Result<(HeaderValue, CredentialDeadline), ApiError> {
    if single_header(headers, header::HeaderName::from_static(MODE_HEADER))
        .map(HeaderValue::as_bytes)
        != Some(ROLLING_V1.as_bytes())
    {
        return Err(INVALID_RESPONSE);
    }
    let (header, cookie) = parse_session_cookie(origin, headers)?;
    let Some(Expiration::DateTime(expires)) = cookie.expires() else {
        return Err(INVALID_RESPONSE);
    };
    if cookie.same_site() != Some(SameSite::Lax)
        || cookie.max_age().is_some_and(|age| !age.is_positive())
    {
        return Err(INVALID_RESPONSE);
    }
    // Expires is the actual server deadline. Max-Age must not create a new window.
    // Sample monotonic time first so scheduling delays cannot lengthen the deadline.
    let now = Instant::now();
    let remaining =
        Duration::try_from(expires - OffsetDateTime::now_utc()).map_err(|_| INVALID_RESPONSE)?;
    if remaining.is_zero() || remaining > ROLLING_LIFETIME {
        return Err(INVALID_RESPONSE);
    }
    Ok((
        header,
        CredentialDeadline {
            instant: now.checked_add(remaining).ok_or(INVALID_RESPONSE)?,
            server_expires: Some(expires),
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::OFFLINE;

    #[tokio::test]
    async fn validated_local_grant_is_transportless_and_never_upgrades_in_place() {
        let origin = Origin::parse("https://music.example").unwrap();
        let api = Api::new(origin.clone()).unwrap();
        let cookie = HeaderValue::from_static("__Host-spofify_session=synthetic");
        let summary = SessionSummary {
            account_id: "account-one".into(),
            username: "listener".into(),
            role: "user".into(),
            csrf_token: "synthetic-csrf".into(),
        };
        let deadline = CredentialDeadline {
            instant: Instant::now() + Duration::from_secs(10),
            server_expires: Some(OffsetDateTime::now_utc() + Duration::from_secs(10)),
        };
        let server = api.session_from_summary(summary, cookie, deadline).unwrap();
        let legacy = serde_json::to_vec(&api.remembered_credential(&server).unwrap()).unwrap();
        assert!(matches!(
            serde_json::from_slice(&legacy),
            Ok(RememberedLogin::Online(_))
        ));
        let saved = serde_json::to_vec(&api.remembered_login(&server).unwrap()).unwrap();
        let encoded = String::from_utf8_lossy(&saved);
        assert!(!encoded.contains("synthetic-csrf"));
        assert!(encoded.contains("local-online-v1"));
        let RememberedLogin::Offline(grant) = serde_json::from_slice(&saved).unwrap() else {
            panic!("expected a local grant")
        };
        assert!(!grant.manual_offline());
        server.set_offline(true);
        assert!(
            grant
                .admit(&Origin::parse("https://other.example").unwrap(), "listener")
                .is_err()
        );
        assert!(grant.admit(&origin, "impostor").is_err());
        let (local_api, local) = grant.admit(&origin, "listener").unwrap();
        let clone = local.clone();
        assert!(local_api.client.is_none() && local_api.audio_client.is_none());
        assert!(local.secrets.credentials.is_none());
        assert_eq!(
            local_api.authenticated_account(&local).unwrap().1,
            "account-one"
        );
        assert!(api.authenticated_account(&local).is_err());
        assert!(local_api.authenticated_account(&server).is_err());
        local.set_offline(false);
        assert!(local.is_offline() && local.is_expired() && local.can_access_local());
        assert_eq!(
            local_api.resume(&grant.credential).await.err(),
            Some(OFFLINE)
        );
        local.retire_local_grant();
        assert!(!clone.can_access_local());
        assert!(local_api.authenticated_account(&clone).is_err());
        server.lifetime().deadline.instant = Instant::now();
        assert!(server.can_access_local()); // running local access can outlast its bearer
        assert!(api.remembered_login(&server).is_err()); // cannot create a new cold grant now
    }
}

#[cfg(test)]
pub(super) fn test_rolling_cookie(value: &str, expires: OffsetDateTime) -> String {
    let weekday = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"]
        [usize::from(expires.weekday().number_days_from_monday())];
    let month = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ][usize::from(u8::from(expires.month())) - 1];
    format!(
        "spofify_session={value}; Path=/; HttpOnly; SameSite=Lax; \
         Expires={weekday}, {:02} {month} {:04} {:02}:{:02}:{:02} GMT; Max-Age=2591999",
        expires.day(),
        expires.year(),
        expires.hour(),
        expires.minute(),
        expires.second(),
    )
}
