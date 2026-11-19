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
    Api, ApiError, CredentialDeadline, INVALID_INPUT, INVALID_RESPONSE, LOGIN_BODY, Origin,
    Session, SessionLifetime, SessionSecrets, SessionSummary, content_type, decode, error,
    parse_session_cookie, session_cookie, session_cookie_name, single_header, valid_id,
    valid_session_cookie_value, valid_text,
};

const MODE_HEADER: &str = "x-spofify-session-mode";
const ROLLING_V1: &str = "rolling-v1";
const ROLLING_LIFETIME: Duration = Duration::from_secs(30 * 24 * 60 * 60);
const NOT_REMEMBERED: ApiError = error("This sign-in did not enable Remember me.");
const WRONG_ORIGIN: ApiError = error("The remembered sign-in is for a different server address.");
const WRONG_ACCOUNT: ApiError = ApiError {
    message: "The server returned a different account. Sign in again.",
    expired: true,
    status: 0,
};

/// Secret-service payload only. No cached summary, CSRF token, password or expiry;
/// the account is a binding to check against the server, not an authenticated identity.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RememberedCredential {
    origin: String,
    account_id: String,
    cookie: String,
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
                .secrets
                .cookie
                .to_str()
                .map_err(|_| INVALID_RESPONSE)?
                .to_owned(),
        })
    }

    /// A saved cookie grants no local or network session before this probe succeeds.
    /// The caller owns cancellation and fences the result against newer sign-in intents.
    pub async fn resume(&self, credential: &RememberedCredential) -> Result<Session, ApiError> {
        // Require the stored canonical spelling; no origin aliases or scheme/port changes.
        if credential.origin != self.origin.as_str() {
            return Err(WRONG_ORIGIN);
        }
        if !valid_id(&credential.account_id) {
            return Err(INVALID_INPUT);
        }
        let (name, value) = credential.cookie.split_once('=').ok_or(INVALID_INPUT)?;
        if name != session_cookie_name(&self.origin) || !valid_session_cookie_value(value) {
            return Err(INVALID_INPUT);
        }
        let mut saved_cookie =
            HeaderValue::from_str(&credential.cookie).map_err(|_| INVALID_INPUT)?;
        saved_cookie.set_sensitive(true);
        let request = self
            .request_with_method(
                reqwest::Method::GET,
                self.origin.endpoint("/api/auth/session"),
            )?
            .header(header::CACHE_CONTROL, "no-cache, no-store")
            .header(header::COOKIE, saved_cookie.clone());
        let response = self.send(request, None, false).await?;
        let (cookie, deadline) = rolling_cookie(&self.origin, &response)?;
        if cookie != saved_cookie {
            return Err(INVALID_RESPONSE); // rolling-v1 never replaces the bearer
        }
        let summary: SessionSummary = decode(response, LOGIN_BODY).await?;
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
                cookie,
                csrf,
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
        if cookie != session.secrets.cookie {
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
