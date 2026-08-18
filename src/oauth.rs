// =============================================================================
// Shared OAuth2 PKCE utilities
// =============================================================================

/// Generate a random code verifier for PKCE (43-128 chars, unreserved charset)
pub fn generate_code_verifier() -> String {
    use rand::Rng;
    const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";
    let mut rng = rand::rng();
    (0..64)
        .map(|_| {
            let idx = rng.random_range(0..CHARSET.len());
            CHARSET[idx] as char
        })
        .collect()
}

/// Generate a random state parameter for CSRF protection
pub fn generate_state() -> String {
    use rand::Rng;
    let mut rng = rand::rng();
    (0..32)
        .map(|_| format!("{:02x}", rng.random_range(0u8..=255)))
        .collect()
}

/// S256 code challenge from verifier
pub fn code_challenge(verifier: &str) -> String {
    use base64::Engine;
    let digest = sha256(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

fn sha256(data: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize().into()
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_empty() {
        let hash = sha256(b"");
        let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn sha256_hello() {
        let hash = sha256(b"hello");
        let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn sha256_multiblock() {
        let data = b"The quick brown fox jumps over the lazy dog. And then some more text to exceed 64 bytes.";
        let hash = sha256(data);
        let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex.len(), 64);
        assert_eq!(hash, sha256(data));
    }

    #[test]
    fn code_verifier_length_and_charset() {
        let v = generate_code_verifier();
        assert_eq!(v.len(), 64);
        assert!(
            v.chars()
                .all(|c| c.is_ascii_alphanumeric() || "-._~".contains(c))
        );
    }

    #[test]
    fn code_challenge_is_base64url() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = code_challenge(verifier);
        assert!(!challenge.contains('='));
        assert!(!challenge.contains('+'));
        assert!(!challenge.contains('/'));
        assert!(!challenge.is_empty());
    }

    #[test]
    fn code_challenge_rfc7636_appendix_b() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = code_challenge(verifier);
        assert_eq!(challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn state_is_hex_and_correct_length() {
        let state = generate_state();
        assert_eq!(state.len(), 64);
        assert!(state.chars().all(|c| c.is_ascii_hexdigit()));
    }

    // =========================================================================
    // Fastmail OAuth (kata ngzw)
    // =========================================================================

    use chrono::{Duration as ChronoDuration, TimeZone, Utc};
    use std::collections::HashMap;

    /// Spawn a loopback token endpoint that records the form body of the
    /// first POST and replies with `body` (status 200) or the given error
    /// status/body. Same no-mock-framework style as outlook's
    /// `ensure_token_at` tests.
    async fn spawn_token_endpoint(
        status: u16,
        body: &'static str,
    ) -> (String, std::sync::Arc<std::sync::Mutex<Option<String>>>) {
        use axum::routing::post;
        let received: std::sync::Arc<std::sync::Mutex<Option<String>>> =
            std::sync::Arc::new(std::sync::Mutex::new(None));
        let received_clone = received.clone();
        let app = axum::Router::new().route(
            "/token",
            post(move |req_body: String| {
                let received = received_clone.clone();
                async move {
                    *received.lock().unwrap() = Some(req_body);
                    (
                        axum::http::StatusCode::from_u16(status).unwrap(),
                        [("content-type", "application/json")],
                        body,
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}/token"), received)
    }

    fn form_pairs(body: &str) -> HashMap<String, String> {
        url::form_urlencoded::parse(body.as_bytes())
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect()
    }

    #[test]
    fn fastmail_auth_url_contains_required_params() {
        let url = fastmail_auth_url("fm-client-1", "verifier-abc", "state-xyz");
        assert!(url.starts_with("https://api.fastmail.com/oauth/authorize?"));
        let parsed = url::Url::parse(&url).unwrap();
        let q: HashMap<String, String> = parsed
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        assert_eq!(q["client_id"], "fm-client-1");
        assert_eq!(q["response_type"], "code");
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(q["state"], "state-xyz");
        assert_eq!(q["code_challenge"], code_challenge("verifier-abc"));
        assert_eq!(q["redirect_uri"], "http://127.0.0.1:8402/callback");
        // JMAP-typed scopes; CalDAV coverage is the open registration
        // question documented on the SCOPES const.
        let scope = &q["scope"];
        assert!(scope.contains("urn:ietf:params:jmap:core"));
        assert!(scope.contains("urn:ietf:params:jmap:mail"));
        assert!(scope.contains("urn:ietf:params:jmap:submission"));
    }

    #[test]
    fn fastmail_client_id_env_overrides_shipped_const() {
        assert_eq!(
            resolve_fastmail_client_id(Some("env-client-id")),
            Some("env-client-id".to_string())
        );
        // Empty env var means "unset", not "empty client id".
        let fallback = (!FASTMAIL_CLIENT_ID.is_empty()).then(|| FASTMAIL_CLIENT_ID.to_string());
        assert_eq!(resolve_fastmail_client_id(Some("")), fallback);
        assert_eq!(resolve_fastmail_client_id(None), fallback);
    }

    #[tokio::test]
    async fn fastmail_exchange_code_posts_pkce_form_and_parses_tokens() {
        let (token_url, received) = spawn_token_endpoint(
            200,
            r#"{"access_token":"at-1","refresh_token":"rt-1","expires_in":3600,"token_type":"bearer","scope":"urn:ietf:params:jmap:core"}"#,
        )
        .await;
        let client = reqwest::Client::new();
        let resp = fastmail_exchange_code_at(&client, &token_url, "cid-1", "code-1", "ver-1")
            .await
            .unwrap();
        assert_eq!(resp.access_token, "at-1");
        assert_eq!(resp.refresh_token.as_deref(), Some("rt-1"));
        assert_eq!(resp.expires_in, 3600);

        let body = received.lock().unwrap().clone().expect("request recorded");
        let form = form_pairs(&body);
        assert_eq!(form["grant_type"], "authorization_code");
        assert_eq!(form["client_id"], "cid-1");
        assert_eq!(form["code"], "code-1");
        assert_eq!(form["code_verifier"], "ver-1");
        assert_eq!(form["redirect_uri"], "http://127.0.0.1:8402/callback");
    }

    #[tokio::test]
    async fn fastmail_refresh_posts_refresh_grant_and_rotates_tokens() {
        let (token_url, received) = spawn_token_endpoint(
            200,
            r#"{"access_token":"at-2","refresh_token":"rt-2","expires_in":3600}"#,
        )
        .await;
        let client = reqwest::Client::new();
        let before = Utc::now();
        let resp = fastmail_refresh_at(&client, &token_url, "cid-1", "rt-1")
            .await
            .unwrap();

        let mut tokens = crate::platform::Tokens {
            access_token: "at-1".into(),
            refresh_token: "rt-1".into(),
            token_expiry: before,
            email: "u@fm.com".into(),
        };
        apply_rotated_tokens(&mut tokens, &resp, before);
        // Rotate-on-refresh: Fastmail invalidates the old refresh token, so
        // the new one MUST replace it.
        assert_eq!(tokens.access_token, "at-2");
        assert_eq!(tokens.refresh_token, "rt-2");
        assert_eq!(tokens.token_expiry, before + ChronoDuration::seconds(3600));

        let body = received.lock().unwrap().clone().expect("request recorded");
        let form = form_pairs(&body);
        assert_eq!(form["grant_type"], "refresh_token");
        assert_eq!(form["refresh_token"], "rt-1");
        assert_eq!(form["client_id"], "cid-1");
    }

    #[test]
    fn apply_rotated_tokens_keeps_old_refresh_when_response_omits_it() {
        let now = Utc.with_ymd_and_hms(2026, 8, 18, 12, 0, 0).unwrap();
        let mut tokens = crate::platform::Tokens {
            access_token: "at-1".into(),
            refresh_token: "rt-1".into(),
            token_expiry: now,
            email: "u@fm.com".into(),
        };
        let resp = FastmailTokenResponse {
            access_token: "at-2".into(),
            refresh_token: None,
            expires_in: 60,
        };
        apply_rotated_tokens(&mut tokens, &resp, now);
        assert_eq!(tokens.access_token, "at-2");
        assert_eq!(tokens.refresh_token, "rt-1");
    }

    #[tokio::test]
    async fn fastmail_refresh_invalid_grant_is_terminal() {
        let (token_url, _) =
            spawn_token_endpoint(400, r#"{"error":"invalid_grant"}"#).await;
        let client = reqwest::Client::new();
        let err = fastmail_refresh_at(&client, &token_url, "cid-1", "rt-dead")
            .await
            .unwrap_err();
        assert!(matches!(err, FastmailRefreshError::InvalidGrant(_)));
    }

    #[tokio::test]
    async fn fastmail_refresh_transient_error_is_not_terminal() {
        let (token_url, _) = spawn_token_endpoint(503, "upstream busy").await;
        let client = reqwest::Client::new();
        let err = fastmail_refresh_at(&client, &token_url, "cid-1", "rt-1")
            .await
            .unwrap_err();
        assert!(matches!(err, FastmailRefreshError::Other(_)));
    }

    #[test]
    fn due_for_refresh_selects_expired_and_expiring_only() {
        let now = Utc.with_ymd_and_hms(2026, 8, 18, 12, 0, 0).unwrap();
        let margin = ChronoDuration::seconds(300);
        let expiries = vec![
            ("expired".to_string(), now - ChronoDuration::seconds(10)),
            ("soon".to_string(), now + ChronoDuration::seconds(60)),
            ("fresh".to_string(), now + ChronoDuration::seconds(3600)),
        ];
        let due = due_for_refresh(&expiries, now, margin);
        assert_eq!(due, vec!["expired".to_string(), "soon".to_string()]);
    }

    #[test]
    fn refresh_scheduling_50_accounts_under_budget() {
        // Perf budget (kata ngzw): token-refresh scheduling over 50 accounts
        // < 10ms. Actual cost is microseconds; the budget carries ~5x CI
        // headroom (same discipline as rate_limit.rs timing asserts).
        let now = Utc.with_ymd_and_hms(2026, 8, 18, 12, 0, 0).unwrap();
        let margin = ChronoDuration::seconds(300);
        let expiries: Vec<(String, chrono::DateTime<Utc>)> = (0..50)
            .map(|i| {
                (
                    format!("acct-{i}"),
                    now + ChronoDuration::seconds(if i % 2 == 0 { 60 } else { 3600 }),
                )
            })
            .collect();
        let start = std::time::Instant::now();
        let due = due_for_refresh(&expiries, now, margin);
        let elapsed = start.elapsed();
        assert_eq!(due.len(), 25);
        assert!(
            elapsed < std::time::Duration::from_millis(10),
            "scheduling 50 accounts took {elapsed:?}, budget 10ms"
        );
    }
}
