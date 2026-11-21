use super::*;
use crate::api::api_auth::test_rolling_cookie;
use cookie::time::Duration as WallDuration;

const ROLLING_SUMMARY: &str = r#"{"accountId":"a1","username":"listener","role":"user","csrfToken":"initial-csrf","sessionMode":"rolling-v1"}"#;
const RESUMED_SUMMARY: &str =
    r#"{"accountId":"a1","username":"renamed","role":"admin","csrfToken":"fresh-csrf"}"#;

fn rolling_reply(cookie: &str, body: &str) -> Vec<u8> {
    reply(
        200,
        &[
            ("Content-Type", "application/json; charset=utf-8"),
            ("X-Spofify-Session-Mode", "rolling-v1"),
            ("Set-Cookie", cookie),
        ],
        body,
    )
}

fn saved(origin: &str, cookie: &str) -> RememberedCredential {
    serde_json::from_value(
        serde_json::json!({"origin": origin, "accountId": "a1", "cookie": cookie}),
    )
    .unwrap()
}

#[tokio::test]
async fn remember_exports_only_the_bound_cookie_and_resume_uses_the_server_summary() {
    let now = OffsetDateTime::now_utc();
    let old = test_rolling_cookie("cookie-secret", now + WallDuration::hours(1));
    let newer = test_rolling_cookie("cookie-secret", now + WallDuration::days(29));
    let wire = Wire::new(vec![
        rolling_reply(&old, ROLLING_SUMMARY),
        rolling_reply(&newer, RESUMED_SUMMARY),
        reply(204, &[], ""),
    ]);
    let api = wire.api();
    let before = Instant::now();
    let mut initial = api
        .login_remembered("listener", "synthetic-password")
        .await
        .unwrap();
    assert!(initial.lifetime().deadline.instant < before + Duration::from_secs(3601));
    initial.account_id = "mutable-display-field".into();
    let saved = api.remembered_credential(&initial).unwrap();
    let serialized = serde_json::to_value(saved).unwrap();
    assert_eq!(
        serialized,
        serde_json::json!({
            "origin": wire.origin.as_str(), "accountId": "a1", "cookie": "spofify_session=cookie-secret"
        })
    );
    let restored: RememberedCredential = serde_json::from_value(serialized.clone()).unwrap();
    let mut unexpected = serialized;
    unexpected["deadline"] = serde_json::json!("cached");
    assert!(serde_json::from_value::<RememberedCredential>(unexpected).is_err());
    let new_api = wire.api();
    assert!(new_api.remembered_credential(&initial).is_err());
    let session = new_api.resume(&restored).await.unwrap();
    assert_eq!(
        (&*session.account_id, &*session.username, &*session.role),
        ("a1", "renamed", "admin")
    );
    assert!(session.lifetime().deadline.instant > initial.lifetime().deadline.instant);
    new_api.logout(&session).await.unwrap();
    assert!(!session.can_access_local());
    assert!(!session.was_server_revoked());
    assert_eq!(
        failure(new_api.remembered_credential(&session)),
        ROLLING_DEADLINE
    );
    let requests = wire.recorded();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0].first_line, "POST /api/auth/login HTTP/1.1");
    assert_eq!(
        requests[0].header("x-spofify-session-mode"),
        Some("rolling-v1")
    );
    assert_eq!(requests[0].header("x-spofify-login"), Some("1"));
    assert_eq!(requests[0].header("cookie"), None);
    let resume = &requests[1];
    assert_eq!(resume.first_line, "GET /api/auth/session HTTP/1.1");
    assert_eq!(resume.header("origin"), Some(wire.origin.as_str()));
    assert_eq!(resume.header("cache-control"), Some("no-cache, no-store"));
    assert_eq!(
        resume.header("cookie"),
        Some("spofify_session=cookie-secret")
    );
    for header in ["x-csrf-token", "x-spofify-login", "x-spofify-session-mode"] {
        assert_eq!(resume.header(header), None);
    }
    assert!(resume.body.is_empty());
    assert_eq!(requests[2].header("x-csrf-token"), Some("fresh-csrf"));
}

#[tokio::test]
async fn resume_fences_origins_and_only_uses_valid_same_account_publication() {
    let future = OffsetDateTime::now_utc() + WallDuration::hours(2);
    let cookie = test_rolling_cookie("saved-token", future);
    let substituted = test_rolling_cookie("different-token", future);
    let destination = Wire::new(vec![]);
    let location = format!("{}/api/auth/session", destination.origin.as_str());
    let wire = Wire::new(vec![
        reply(503, &[], "unavailable"),
        Vec::new(),
        reply(302, &[("Location", &location)], ""),
        json(RESUMED_SUMMARY),
        rolling_reply(&substituted, RESUMED_SUMMARY),
        rolling_reply(&cookie, &RESUMED_SUMMARY.replace("a1", "a2")),
        reply(401, &[], "authoritative denial"),
        rolling_reply(&cookie, RESUMED_SUMMARY),
    ]);
    let api = wire.api();
    let origin = wire.origin.as_str();
    let credential = saved(origin, "spofify_session=saved-token");
    assert!(!failure(destination.api().resume(&credential).await).expired);
    assert!(
        !failure(
            api.resume(&saved(&format!("{origin}/"), "spofify_session=saved-token"))
                .await
        )
        .expired
    );
    for malformed in [
        "spofify_session=saved-token; other=x",
        "__Host-spofify_session=saved-token",
        "spofify_session=",
    ] {
        assert_eq!(
            failure(api.resume(&saved(origin, malformed)).await),
            INVALID_INPUT
        );
    }
    assert!(wire.recorded().is_empty());
    for expected in [503, 0, 302, 0, 0] {
        let error = failure(api.resume(&credential).await);
        assert!(!error.expired);
        assert_eq!(error.status, expected);
    }
    assert!(failure(api.resume(&credential).await).expired); // different account
    assert_eq!(failure(api.resume(&credential).await), EXPIRED);
    assert!(api.resume(&credential).await.unwrap().can_access_local());
    assert_eq!(wire.recorded().len(), 8);
    assert!(destination.recorded().is_empty());
}

#[test]
fn resume_backoff_classifies_only_known_transient_failures() {
    for status in [408, 500, 502, 503, 504] {
        assert!(Api::response_error(status, false).can_retry_restore());
    }
    for status in [302, 400, 401, 403, 404, 429, 501, 505] {
        assert!(!Api::response_error(status, false).can_retry_restore());
    }
    assert!(!INVALID_RESPONSE.can_retry_restore());
    assert!(!INVALID_INPUT.can_retry_restore());
    assert!(!NETWORK.can_retry_restore());
}

#[tokio::test]
async fn opted_in_login_requires_both_mode_indicators_and_absolute_expiry() {
    let now = OffsetDateTime::now_utc();
    let cookie = test_rolling_cookie("test", now + WallDuration::hours(2));
    let expired = test_rolling_cookie("test", now - WallDuration::seconds(2));
    let overlong = test_rolling_cookie("test", now + WallDuration::days(31));
    let wire = Wire::new(vec![
        login_reply(),
        rolling_reply(&cookie, SUMMARY),
        rolling_reply(&cookie, &ROLLING_SUMMARY.replace("rolling-v1", "other")),
        rolling_reply(&expired, ROLLING_SUMMARY),
        rolling_reply(&overlong, ROLLING_SUMMARY),
        rolling_reply(VALID_COOKIE, ROLLING_SUMMARY),
        rolling_reply(
            &cookie.replace("Max-Age=2591999", "Max-Age=0"),
            ROLLING_SUMMARY,
        ),
        rolling_reply(
            &cookie.replace("SameSite=Lax", "Domain=127.0.0.1"),
            ROLLING_SUMMARY,
        ),
        reply(
            200,
            &[
                ("Content-Type", "application/json"),
                ("Set-Cookie", &cookie),
                ("X-Spofify-Session-Mode", "rolling-v1"),
                ("X-Spofify-Session-Mode", "rolling-v1"),
            ],
            ROLLING_SUMMARY,
        ),
        reply(401, &[], "bad password"),
        login_reply(),
    ]);
    let api = wire.api();
    for _ in 0..9 {
        assert_eq!(
            failure(api.login_remembered("listener", "password").await),
            INVALID_RESPONSE
        );
    }
    let error = failure(api.login_remembered("listener", "password").await);
    assert_eq!(error.status, 401);
    assert!(!error.expired);
    let ordinary = api.login("listener", "password").await.unwrap();
    assert!(api.remembered_credential(&ordinary).is_err());
    assert_eq!(wire.recorded().len(), 11);
}

#[tokio::test]
async fn live_publication_advances_once_and_only_with_the_same_cookie() {
    let now = OffsetDateTime::now_utc();
    let old = test_rolling_cookie("synthetic", now + WallDuration::hours(1));
    let newer = test_rolling_cookie("synthetic", now + WallDuration::hours(2));
    let other = test_rolling_cookie("other", now + WallDuration::hours(3));
    let wire = Wire::new(vec![
        rolling_reply(&old, ROLLING_SUMMARY),
        rolling_reply(&newer, RESULTS),
        rolling_reply(&other, RESULTS),
        rolling_reply(&old, RESULTS),
        rolling_reply(&newer, RESULTS),
        reply(401, &[], "expired"),
    ]);
    let api = wire.api();
    let session = api.login_remembered("listener", "password").await.unwrap();
    let before = session.lifetime().deadline;
    api.search(&session, "track").await.unwrap();
    let after = session.lifetime().deadline;
    assert!(after.server_expires > before.server_expires && after.instant > before.instant);
    for _ in 0..3 {
        api.search(&session, "track").await.unwrap();
        assert_eq!(session.lifetime().deadline.instant, after.instant);
    }
    assert_eq!(
        session.lifetime().apply_denial(before, EXPIRED),
        STALE_DENIAL
    );
    assert!(session.can_access_local());
    assert_eq!(failure(api.search(&session, "track").await), EXPIRED);
    assert!(session.was_server_revoked());
    assert!(!session.can_access_local());
}
