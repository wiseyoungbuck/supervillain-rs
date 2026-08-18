// =============================================================================
// Shared OAuth2 PKCE utilities + the Fastmail OAuth flow.
//
// Outlook and Gmail keep their flows in their own modules (they predate
// this one); Fastmail's lives here because its session type (JmapSession)
// already has a home in jmap.rs and only the OAuth mechanics are new.
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
// Fastmail OAuth (kata ngzw)
//
// Contract per https://www.fastmail.com/for-developers/oauth/ :
//   * Authorization Code + PKCE S256 (mandatory), loopback redirect URIs
//     allowed (`http://127.0.0.1:<port>/…`).
//   * One endpoint for both the initial code exchange and refresh.
//   * Rotate-on-refresh is mandatory: every refresh returns a new
//     refresh_token; reusing an old one revokes the grant.
//   * The same bearer is accepted at api.fastmail.com (JMAP) and
//     caldav.fastmail.com (CalDAV) — one credential, both protocols
//     (DAVx5 and Morgen ship exactly this).
// =============================================================================

use crate::error::Error;
use crate::platform::TokenStore;
use std::sync::Arc;

pub const FASTMAIL_AUTH_URL: &str = "https://api.fastmail.com/oauth/authorize";
/// Token endpoint — Fastmail uses `/oauth/refresh` for BOTH the initial
/// authorization-code exchange and subsequent refreshes.
pub const FASTMAIL_TOKEN_URL: &str = "https://api.fastmail.com/oauth/refresh";

/// The client id assigned to supervillain by Fastmail.
///
/// Empty until registration completes: Fastmail has NO self-service OAuth
/// client portal — the maintainer must email Fastmail partnerships with
/// clientName / logoUrl / clientUrl / tosUrl / policyUrl / supportUrl /
/// scopes / redirectUris, and Fastmail assigns the client_id by hand (see
/// docs/fastmail-oauth.md for the full registration checklist). Until then,
/// `SUPERVILLAIN_FASTMAIL_CLIENT_ID` overrides this at runtime.
pub const FASTMAIL_CLIENT_ID: &str = "";
pub const FASTMAIL_CLIENT_ID_ENV: &str = "SUPERVILLAIN_FASTMAIL_CLIENT_ID";

/// User-facing guidance when no client id is available anywhere.
pub const FASTMAIL_CLIENT_ID_HELP: &str = "Fastmail OAuth client id not configured. Fastmail has no self-service OAuth \
     registration — the app maintainer must register supervillain with Fastmail \
     partnerships (see docs/fastmail-oauth.md), then ship the assigned id or set \
     SUPERVILLAIN_FASTMAIL_CLIENT_ID.";

/// 8400 = Outlook, 8401 = Gmail, 8402 = Fastmail.
const FASTMAIL_CALLBACK_PORT: u16 = 8402;
const FASTMAIL_REDIRECT_URI: &str = "http://127.0.0.1:8402/callback";

/// JMAP-typed scopes for mail + send. Fastmail's published scope list has
/// no CalDAV/CardDAV entry, yet shipping clients (DAVx5) sync CalDAV over
/// the OAuth bearer — whether an unlisted CalDAV scope exists, or the
/// bearer grants protocol access with scopes only gating JMAP data types,
/// is the open question to settle in the registration email (kata ngzw).
const FASTMAIL_SCOPES: &str =
    "urn:ietf:params:jmap:core urn:ietf:params:jmap:mail urn:ietf:params:jmap:submission";

/// Refresh when the access token expires within this window. Ticks run
/// every 60s (see `accounts::spawn_fastmail_token_refresher`), so a 300s
/// margin gives ~5 attempts before a token actually lapses mid-request.
pub const FASTMAIL_REFRESH_MARGIN_SECS: i64 = 300;

/// Client-id resolution, pure for testability: a non-empty env value wins,
/// else the shipped const (empty const → `None`). The env override exists
/// so a self-hoster who registered their own client (or the maintainer,
/// pre-release) can run OAuth before a const id ships.
pub fn resolve_fastmail_client_id(env_value: Option<&str>) -> Option<String> {
    match env_value.filter(|v| !v.is_empty()) {
        Some(v) => Some(v.to_string()),
        None => (!FASTMAIL_CLIENT_ID.is_empty()).then(|| FASTMAIL_CLIENT_ID.to_string()),
    }
}

/// Production wrapper: reads `SUPERVILLAIN_FASTMAIL_CLIENT_ID`.
pub fn fastmail_client_id() -> Option<String> {
    let env = std::env::var(FASTMAIL_CLIENT_ID_ENV).ok();
    resolve_fastmail_client_id(env.as_deref())
}

/// Build the Fastmail authorization URL with PKCE (S256 mandatory).
pub fn fastmail_auth_url(client_id: &str, code_verifier: &str, state: &str) -> String {
    let challenge = code_challenge(code_verifier);
    let mut url = url::Url::parse(FASTMAIL_AUTH_URL).expect("valid Fastmail auth base URL");
    url.query_pairs_mut()
        .append_pair("client_id", client_id)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", FASTMAIL_REDIRECT_URI)
        .append_pair("scope", FASTMAIL_SCOPES)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state);
    url.to_string()
}

#[derive(Debug, serde::Deserialize)]
pub struct FastmailTokenResponse {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_in: i64,
}

/// Exchange an authorization code for tokens. Parameterized on the token
/// endpoint so tests can point it at a loopback recorder (same pattern as
/// outlook's `ensure_token_at`); production callers pass
/// [`FASTMAIL_TOKEN_URL`].
pub async fn fastmail_exchange_code_at(
    client: &reqwest::Client,
    token_url: &str,
    client_id: &str,
    code: &str,
    code_verifier: &str,
) -> Result<FastmailTokenResponse, Error> {
    let resp = client
        .post(token_url)
        .form(&[
            ("client_id", client_id),
            ("redirect_uri", FASTMAIL_REDIRECT_URI),
            ("grant_type", "authorization_code"),
            ("code", code),
            ("code_verifier", code_verifier),
        ])
        .send()
        .await?;

    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        tracing::warn!(http_status = %status, response_body = %text, "Fastmail token exchange failed");
        return Err(Error::Auth(format!(
            "Fastmail token exchange failed ({status}): {text}"
        )));
    }
    Ok(resp.json().await?)
}

/// Refresh failure, split by recoverability so callers can't conflate
/// "retry next tick" with "this refresh token is dead".
#[derive(Debug)]
pub enum FastmailRefreshError {
    /// `invalid_grant`: the stored refresh token was revoked or already
    /// rotated by another client. Retrying can never succeed — callers
    /// must clear stored tokens (rotate-on-refresh makes stale copies
    /// permanently invalid).
    InvalidGrant(String),
    /// Anything else (network, 5xx): transient, retry later.
    Other(Error),
}

impl From<reqwest::Error> for FastmailRefreshError {
    fn from(e: reqwest::Error) -> Self {
        Self::Other(e.into())
    }
}

/// Refresh an access token. On success the response carries a NEW
/// refresh_token that MUST replace the stored one (`apply_rotated_tokens`).
pub async fn fastmail_refresh_at(
    client: &reqwest::Client,
    token_url: &str,
    client_id: &str,
    refresh_token: &str,
) -> Result<FastmailTokenResponse, FastmailRefreshError> {
    let resp = client
        .post(token_url)
        .form(&[
            ("client_id", client_id),
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
        ])
        .send()
        .await?;

    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        tracing::warn!(http_status = %status, response_body = %text, "Fastmail token refresh failed");
        if crate::provider_utils::should_clear_tokens_on_refresh_failure(status, &text) {
            return Err(FastmailRefreshError::InvalidGrant(format!(
                "({status}): {text}"
            )));
        }
        return Err(FastmailRefreshError::Other(Error::Auth(format!(
            "Fastmail token refresh failed ({status}): {text}"
        ))));
    }
    resp.json().await.map_err(FastmailRefreshError::from)
}

/// Apply a token response to stored tokens: new access token + expiry, and
/// — rotate-on-refresh — the new refresh token when present. Fastmail
/// always rotates; tolerating an absent one matches Google/Microsoft
/// behavior at zero cost.
pub fn apply_rotated_tokens(
    tokens: &mut crate::platform::Tokens,
    resp: &FastmailTokenResponse,
    now: chrono::DateTime<chrono::Utc>,
) {
    tokens.access_token = resp.access_token.clone();
    if let Some(ref rt) = resp.refresh_token {
        tokens.refresh_token = rt.clone();
    }
    tokens.token_expiry = now + chrono::Duration::seconds(resp.expires_in);
}

/// The refresh-scheduling scan: which accounts' tokens expire within
/// `margin` of `now`. Pure — the daemon feeds it (id, expiry) pairs loaded
/// from the token store. Perf budget (kata ngzw): 50 accounts < 10ms.
pub fn due_for_refresh(
    expiries: &[(String, chrono::DateTime<chrono::Utc>)],
    now: chrono::DateTime<chrono::Utc>,
    margin: chrono::Duration,
) -> Vec<String> {
    expiries
        .iter()
        .filter(|(_, expiry)| now + margin >= *expiry)
        .map(|(id, _)| id.clone())
        .collect()
}

fn build_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .expect("failed to create HTTP client")
}

/// Fill the session's mailbox role cache, best-effort (same as the
/// account-create path in `accounts::upsert_account`).
async fn cache_mailbox_roles(session: &mut crate::jmap::JmapSession) {
    if let Ok(mailboxes) = crate::jmap::get_mailboxes(session).await {
        for mb in &mailboxes {
            if let Some(ref role) = mb.role {
                session.mailbox_cache.insert(role.clone(), mb.clone());
            }
        }
    }
}

/// One-shot Sign-in-with-Fastmail: loopback authorize → code exchange →
/// connect (which discovers the username from the JMAP session object) →
/// persist tokens. Mirrors `outlook::oauth_flow` / `gmail::oauth_flow`.
pub async fn fastmail_oauth_flow(
    client_id: &str,
    token_store: Arc<dyn TokenStore>,
    account_id: &str,
) -> Result<crate::jmap::JmapSession, Error> {
    let code_verifier = generate_code_verifier();
    let expected_state = generate_state();
    let url = fastmail_auth_url(client_id, &code_verifier, &expected_state);

    let callback =
        crate::platform::acquire_oauth_callback(&url, &expected_state, FASTMAIL_CALLBACK_PORT)
            .await?;

    let client = build_http_client();
    let token_resp = fastmail_exchange_code_at(
        &client,
        FASTMAIL_TOKEN_URL,
        client_id,
        &callback.code,
        &code_verifier,
    )
    .await?;
    let refresh_token = token_resp.refresh_token.clone().ok_or_else(|| {
        Error::Auth("Fastmail did not return a refresh_token on initial consent".into())
    })?;

    let mut session = crate::jmap::JmapSession::new_oauth("", &token_resp.access_token);
    crate::jmap::connect(&mut session).await?;
    if session.username.is_empty() {
        return Err(Error::Auth(
            "Fastmail session did not report a username".into(),
        ));
    }
    cache_mailbox_roles(&mut session).await;

    token_store.save(
        account_id,
        &crate::platform::Tokens {
            access_token: token_resp.access_token,
            refresh_token,
            token_expiry: chrono::Utc::now() + chrono::Duration::seconds(token_resp.expires_in),
            email: session.username.clone(),
        },
    )?;
    tracing::info!("Fastmail OAuth completed for {}", session.username);
    Ok(session)
}

/// Rebuild a Fastmail OAuth session from stored tokens at startup —
/// refresh first if the access token is stale (the app may have been off
/// past the expiry). Missing tokens → the same "click Authorize" error
/// surface as Outlook/Gmail.
pub async fn load_fastmail_oauth_session(
    account_id: &str,
    token_store: &Arc<dyn TokenStore>,
) -> Result<crate::jmap::JmapSession, Error> {
    let Some(mut tokens) = token_store.load(account_id) else {
        return Err(Error::Auth(
            "Not authorized — open settings and click Authorize".into(),
        ));
    };

    let now = chrono::Utc::now();
    if now + chrono::Duration::seconds(FASTMAIL_REFRESH_MARGIN_SECS) >= tokens.token_expiry {
        let client_id =
            fastmail_client_id().ok_or_else(|| Error::Auth(FASTMAIL_CLIENT_ID_HELP.into()))?;
        let client = build_http_client();
        match fastmail_refresh_at(
            &client,
            FASTMAIL_TOKEN_URL,
            &client_id,
            &tokens.refresh_token,
        )
        .await
        {
            Ok(resp) => {
                apply_rotated_tokens(&mut tokens, &resp, chrono::Utc::now());
                token_store.save(account_id, &tokens)?;
            }
            Err(FastmailRefreshError::InvalidGrant(detail)) => {
                // Same recovery as Outlook 0ch3: a dead refresh token can
                // never succeed again — clear it so authStatus goes pending
                // instead of every startup re-failing the same refresh.
                let _ = token_store.delete(account_id);
                return Err(Error::Auth(format!(
                    "Fastmail refresh token expired or revoked. Stored tokens cleared; \
                     open settings and click Authorize to reconnect. {detail}"
                )));
            }
            Err(FastmailRefreshError::Other(e)) => return Err(e),
        }
    }

    let mut session = crate::jmap::JmapSession::new_oauth(&tokens.email, &tokens.access_token);
    crate::jmap::connect(&mut session).await?;
    cache_mailbox_roles(&mut session).await;
    Ok(session)
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
        let (token_url, _) = spawn_token_endpoint(400, r#"{"error":"invalid_grant"}"#).await;
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
