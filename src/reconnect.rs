//! The only authenticated network exception while the session is fenced offline.

use std::{sync::atomic::Ordering, time::Duration};

use reqwest::{Method, header};

use super::{
    Api, ApiError, EXPIRED, INVALID_RESPONSE, LOGIN_BODY, NETWORK, OFFLINE, Session,
    SessionSummary, content_type, decode, error, valid_id, valid_text,
};

const PROBE_TIMEOUT: Duration = Duration::from_secs(4);
const NOT_FENCED: ApiError = error("Reconnect validation requires an offline session.");
const WRONG_ACCOUNT: ApiError = ApiError {
    message: "The server returned a different account. Sign in again.",
    expired: true,
    status: 0,
};

impl Api {
    /// Validate the existing credential once, only for an explicit Go online intent.
    /// The caller owns the intent token, cancellation, retries and any mode change.
    /// Success does not open networking or refresh credentials.
    pub(crate) async fn validate_reconnect(&self, session: &Session) -> Result<(), ApiError> {
        self.check_session(session)?;
        let mut changes = session.network_changes();
        let generation = *changes.borrow_and_update();
        if generation & 1 == 0 {
            return Err(NOT_FENCED);
        }

        let result = tokio::select! {
            biased;
            _ = changes.changed() => return Err(OFFLINE),
            _ = tokio::time::sleep_until(session.secrets.deadline.into()) => return Err(EXPIRED),
            result = async {
                self.check_session(session)?;
                if session.network_generation() != generation {
                    return Err(OFFLINE);
                }
                // Go authenticates this GET by cookie alone. It returns the login summary
                // (including CSRF for writes), with no server deadline. Never send CSRF or
                // install Set-Cookie here; the desktop uses a fixed lifetime from login.
                let response = self
                    .request_with_method(Method::GET, self.origin.endpoint("/api/auth/session"))?
                    .header(header::COOKIE, session.secrets.cookie.clone())
                    .timeout(PROBE_TIMEOUT)
                    .send()
                    .await
                    .map_err(|_| NETWORK)?;
                match response.status().as_u16() {
                    200 if content_type(&response)? == "application/json" => {
                        decode::<SessionSummary>(response, LOGIN_BODY).await
                    }
                    200..=299 => Err(INVALID_RESPONSE),
                    status => Err(Self::response_error(status, None, false)),
                }
            } => result,
        };

        // Serialize the decision with changes to the transport fence. Repeated Work
        // offline (no generation change) is covered by the caller's intent token.
        let current = changes.borrow();
        if *current != generation {
            return Err(OFFLINE);
        }
        self.check_session(session)?;
        let summary = match result {
            Err(err) if err.status == 401 => {
                return Err(Self::response_error(401, Some(session), false));
            }
            other => other?,
        };
        if !valid_id(&summary.account_id) {
            return Err(INVALID_RESPONSE);
        }
        if summary.account_id != session.secrets.account_id {
            session.secrets.revoked.store(true, Ordering::SeqCst);
            return Err(WRONG_ACCOUNT);
        }
        if !valid_text(&summary.username, 256)
            || !matches!(summary.role.as_str(), "admin" | "user")
            || summary.csrf_token.as_bytes() != session.secrets.csrf.as_bytes()
        {
            return Err(INVALID_RESPONSE);
        }
        self.check_session(session)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{api::Origin, media::TrackResource};
    use std::time::Instant;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
        time::timeout,
    };

    const TEST_TIMEOUT: Duration = Duration::from_secs(6);
    const SAME_ACCOUNT: &str =
        r#"{"accountId":"synthetic","username":"renamed","role":"user","csrfToken":"synthetic"}"#;
    const OTHER_ACCOUNT: &str =
        r#"{"accountId":"other","username":"other","role":"user","csrfToken":"synthetic"}"#;

    async fn mock() -> (TcpListener, Api) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = Origin::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        (listener, Api::new(origin).unwrap())
    }

    async fn receive_probe(listener: &TcpListener) -> TcpStream {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0; 4096];
        let mut len = 0;
        while !request[..len].ends_with(b"\r\n\r\n") {
            let n = stream.read(&mut request[len..]).await.unwrap();
            assert!(n > 0, "incomplete or oversized mock request");
            len += n;
        }
        let request = std::str::from_utf8(&request[..len]).unwrap();
        let mut lines = request.lines();
        assert_eq!(lines.next(), Some("GET /api/auth/session HTTP/1.1"));
        let headers: Vec<_> = lines.filter_map(|line| line.split_once(':')).collect();
        let values = |name: &str| {
            headers
                .iter()
                .filter(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.trim())
                .collect::<Vec<_>>()
        };
        let authority = listener.local_addr().unwrap().to_string();
        let origin = format!("http://{authority}");
        assert_eq!(values("Host"), [authority.as_str()]);
        assert_eq!(values("Origin"), [origin.as_str()]);
        assert_eq!(values("Cookie"), ["spofify_session=synthetic"]);
        assert_eq!(values("Accept"), ["application/json"]);
        assert!(values("X-CSRF-Token").is_empty());
        assert!(values("Transfer-Encoding").is_empty());
        assert!(values("Content-Length").iter().all(|len| *len == "0"));
        stream
    }

    fn reply(status: u16, extra_headers: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status} Mock\r\nContent-Type: application/json; charset=utf-8\r\n\
             Content-Length: {}\r\nConnection: close\r\n{extra_headers}\r\n{body}",
            body.len()
        )
    }

    async fn assert_no_more_http(listener: &TcpListener) {
        assert!(
            timeout(Duration::from_millis(25), listener.accept())
                .await
                .is_err()
        );
    }

    async fn assert_media_fenced(api: &Api, session: &Session, expected: ApiError) {
        assert!(session.is_offline());
        assert_eq!(api.media(session, "track").await, Err(expected));
        assert_eq!(
            api.track_resource(session, "track", TrackResource::Artwork)
                .await,
            Err(expected)
        );
    }

    #[tokio::test]
    async fn only_the_probe_reaches_http_and_only_authoritative_denials_revoke() {
        let ignored_cookie =
            "Set-Cookie: spofify_session=discard; Domain=other.invalid; Path=/; Max-Age=0\r\n";
        let oversized = "x".repeat(LOGIN_BODY + 1);
        let chunked = format!(
            "HTTP/1.1 200 Mock\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\
             Connection: close\r\n\r\n{:x}\r\n{oversized}\r\n0\r\n\r\n",
            oversized.len()
        );
        for (response, expected) in [
            (reply(200, ignored_cookie, SAME_ACCOUNT), Ok(())),
            (reply(401, "", "{}"), Err(EXPIRED)),
            (reply(200, "", OTHER_ACCOUNT), Err(WRONG_ACCOUNT)),
            (
                reply(503, "", "{}"),
                Err(Api::response_error(503, None, false)),
            ),
            (String::new(), Err(NETWORK)), // disconnect without a response
            (
                reply(302, "Location: /api/library/tracks/track/audio\r\n", ""),
                Err(Api::response_error(302, None, false)),
            ),
            (reply(200, "", &oversized), Err(INVALID_RESPONSE)),
            (chunked, Err(INVALID_RESPONSE)),
        ] {
            let (listener, api) = mock().await;
            let mut session = api.test_session();
            let clone = session.clone();
            session.account_id = "other".into(); // presentation cannot choose the principal
            session.set_offline(true);
            let generation = session.network_generation();
            let deadline = session.secrets.deadline;
            let server = async {
                let mut stream = receive_probe(&listener).await;
                assert_media_fenced(&api, &clone, OFFLINE).await;
                // An oversized body can cause the receiver to close before the write finishes.
                let _ = stream.write_all(response.as_bytes()).await;
            };
            let result = timeout(TEST_TIMEOUT, async {
                assert_media_fenced(&api, &clone, OFFLINE).await;
                let (result, ()) = tokio::join!(api.validate_reconnect(&session), server);
                result
            })
            .await
            .unwrap();
            assert_eq!(result, expected);
            let revoked = expected.is_err_and(|err| err.expired);
            assert_eq!(clone.can_access_local(), !revoked);
            assert_eq!(session.network_generation(), generation);
            assert_eq!(session.secrets.deadline, deadline);
            assert_eq!(
                session.secrets.cookie.as_bytes(),
                b"spofify_session=synthetic"
            );
            assert_media_fenced(&api, &clone, if revoked { EXPIRED } else { OFFLINE }).await;
            assert_no_more_http(&listener).await;
        }
    }

    #[tokio::test]
    async fn preflight_never_sends_an_online_foreign_expired_or_revoked_session() {
        let (listener, api) = mock().await;
        let online = api.test_session();
        let foreign = Api::new(api.origin.clone()).unwrap().test_session();
        let expired = api.test_session_with_deadline(Instant::now());
        let revoked = api.test_session();
        for session in [&foreign, &expired, &revoked] {
            session.set_offline(true);
        }
        assert_eq!(api.logout(&revoked).await, Err(OFFLINE));

        assert_eq!(api.validate_reconnect(&online).await, Err(NOT_FENCED));
        assert_eq!(
            api.validate_reconnect(&foreign).await,
            api.check_session_owner(&foreign)
        );
        assert_eq!(api.validate_reconnect(&expired).await, Err(EXPIRED));
        assert_eq!(api.validate_reconnect(&revoked).await, Err(EXPIRED));
        assert!(foreign.can_access_local());
        assert!(expired.can_access_local());
        assert!(!revoked.can_access_local());
        assert_media_fenced(&api, &expired, EXPIRED).await;
        assert_media_fenced(&api, &revoked, EXPIRED).await;
        assert_no_more_http(&listener).await;
    }

    #[tokio::test]
    async fn a_superseded_generation_cannot_apply_a_pending_denial() {
        let (listener, api) = mock().await;
        let session = api.test_session();
        session.set_offline(true);
        let server = async {
            let mut stream = receive_probe(&listener).await;
            session.set_offline(false);
            session.set_offline(true);
            let _ = stream.write_all(reply(401, "", "{}").as_bytes()).await;
        };
        let (result, ()) = timeout(TEST_TIMEOUT, async {
            tokio::join!(api.validate_reconnect(&session), server)
        })
        .await
        .unwrap();
        assert_eq!(result, Err(OFFLINE));
        assert!(session.can_access_local());
        assert_media_fenced(&api, &session, OFFLINE).await;
        assert_no_more_http(&listener).await;
    }
}
