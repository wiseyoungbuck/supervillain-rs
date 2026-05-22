// Desktop (macOS, Linux) implementations of the platform abstraction.
//
// The iOS port will add a parallel `ios.rs` with `KeychainTokenStore`,
// `ASWebAuthenticationSession`-based `acquire_oauth_callback`, etc.

use std::path::PathBuf;
use std::sync::Arc;

use axum::{Router, extract::Query, routing::get};
use serde::Deserialize;
use tokio::sync::{Mutex, oneshot};

use super::{OauthCallback, TokenStore, Tokens};
use crate::error::Error;

/// XDG-style config directory: `$XDG_CONFIG_HOME` if set, else `$HOME/.config`.
pub fn config_dir() -> PathBuf {
    std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .ok()
        .or_else(|| {
            std::env::var("HOME")
                .map(|h| PathBuf::from(h).join(".config"))
                .ok()
        })
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Open a URL in the user's default browser. On Omarchy, prefers
/// `omarchy-launch-webapp` so the URL opens as a webapp rather than a tab.
pub fn open_browser(url: &str) {
    let is_omarchy = PathBuf::from(std::env::var("HOME").unwrap_or_default())
        .join(".local/share/omarchy")
        .is_dir();

    let (cmd, args): (&str, Vec<&str>) = if is_omarchy {
        ("omarchy-launch-webapp", vec![url])
    } else if cfg!(target_os = "macos") {
        ("open", vec![url])
    } else {
        ("xdg-open", vec![url])
    };

    match std::process::Command::new(cmd)
        .args(&args)
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(mut child) => {
            tracing::info!("Opened browser via {cmd}");
            std::thread::spawn(move || {
                use std::io::BufRead;
                if let Some(stderr) = child.stderr.take() {
                    for line in std::io::BufReader::new(stderr)
                        .lines()
                        .map_while(Result::ok)
                    {
                        if line.contains("DEPRECATED_ENDPOINT") {
                            tracing::warn!("{line} (known Chromium issue, safe to ignore)");
                        } else if !line.is_empty() {
                            tracing::warn!("browser: {line}");
                        }
                    }
                }
                let _ = child.wait();
            });
        }
        Err(e) => tracing::warn!("Failed to open browser via {cmd}: {e}"),
    }
}

/// Initialize the tracing subscriber. Reads `RUST_LOG` env var; defaults to `info`.
/// iOS will bridge tracing to `os_log` instead.
pub fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
}

/// Maximum time to wait for the user to complete the OAuth consent flow before
/// giving up and releasing the loopback port. The user has 5 minutes to click
/// through Google/Microsoft's consent screen.
const OAUTH_CALLBACK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// Run the desktop OAuth2 callback flow: bind a loopback listener on `port`,
/// open the auth URL in the user's browser, await the `/callback` request,
/// validate the state parameter (CSRF), and return the authorization code.
///
/// Handles both success (`?code=…&state=…`) and error redirects
/// (`?error=access_denied&state=…`) without hanging the process. Bounded by
/// `OAUTH_CALLBACK_TIMEOUT` — if the user abandons the consent screen, the
/// port is released after the timeout.
///
/// iOS will provide a different impl using `ASWebAuthenticationSession` with
/// a custom URL scheme — Google blocks OAuth in WebView contexts and iOS may
/// background a loopback listener during the consent screen.
pub async fn acquire_oauth_callback(
    auth_url: &str,
    expected_state: &str,
    port: u16,
) -> Result<OauthCallback, Error> {
    let bind = format!("127.0.0.1:{port}");
    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .map_err(|e| Error::Internal(format!("Failed to bind OAuth callback on {bind}: {e}")))?;

    eprintln!("\nOpen this URL to authorize:\n\n  {auth_url}\n");
    open_browser(auth_url);

    let (tx, rx) = oneshot::channel::<CallbackResult>();
    let tx = Arc::new(Mutex::new(Some(tx)));

    let tx_clone = tx.clone();
    let callback_app = Router::new().route(
        "/callback",
        get(move |Query(params): Query<CallbackParams>| {
            let tx = tx_clone.clone();
            async move {
                let user_message = if params.error.is_some() {
                    "Authorization failed. You can close this tab and check the supervillain logs."
                } else if params.code.is_some() {
                    "Authorization successful! You can close this tab."
                } else {
                    "Missing authorization code. You can close this tab."
                };
                if let Some(sender) = tx.lock().await.take() {
                    let _ = sender.send(CallbackResult {
                        code: params.code,
                        state: params.state,
                        error: params.error,
                    });
                }
                user_message
            }
        }),
    );

    let server = axum::serve(listener, callback_app);
    let timeout = tokio::time::sleep(OAUTH_CALLBACK_TIMEOUT);
    tokio::pin!(timeout);

    let result = tokio::select! {
        server_result = server => {
            server_result.map_err(|e| Error::Internal(format!("OAuth callback server error: {e}")))?;
            return Err(Error::Internal("OAuth callback server exited without receiving callback".into()));
        }
        callback = rx => {
            callback.map_err(|_| Error::Internal("OAuth flow cancelled".into()))?
        }
        _ = &mut timeout => {
            tracing::warn!("OAuth callback timed out after {}s", OAUTH_CALLBACK_TIMEOUT.as_secs());
            return Err(Error::Auth(format!(
                "OAuth flow timed out after {}s — no callback received. \
                 Re-run to retry.",
                OAUTH_CALLBACK_TIMEOUT.as_secs()
            )));
        }
    };

    validate_callback(result, expected_state)
}

#[derive(Deserialize)]
struct CallbackParams {
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    state: String,
    #[serde(default)]
    error: Option<String>,
}

/// Internal struct passed across the oneshot channel — pre-validation.
struct CallbackResult {
    code: Option<String>,
    state: String,
    error: Option<String>,
}

/// Validate the callback (state match, presence of code, absence of error)
/// and produce either an `OauthCallback` or an `Error`. Pure function — no I/O.
fn validate_callback(result: CallbackResult, expected_state: &str) -> Result<OauthCallback, Error> {
    if let Some(err) = result.error {
        tracing::warn!(oauth_error = %err, "OAuth provider returned error redirect");
        return Err(Error::Auth(format!("OAuth provider returned error: {err}")));
    }
    if result.state != expected_state {
        tracing::warn!(
            state_matches = false,
            "OAuth callback received with mismatched state"
        );
        return Err(Error::Auth(
            "OAuth state mismatch — possible CSRF attack".into(),
        ));
    }
    let code = result.code.ok_or_else(|| {
        Error::Auth("OAuth callback received without code or error parameter".into())
    })?;
    tracing::debug!(state_matches = true, "OAuth callback received");
    Ok(OauthCallback { code })
}

// =============================================================================
// FsTokenStore — writes <tokens_dir>/<account>.json
// =============================================================================

pub struct FsTokenStore {
    tokens_dir: PathBuf,
}

impl FsTokenStore {
    pub fn new(tokens_dir: PathBuf) -> Self {
        Self { tokens_dir }
    }

    fn path(&self, account: &str) -> PathBuf {
        self.tokens_dir.join(format!("{account}.json"))
    }
}

impl TokenStore for FsTokenStore {
    fn save(&self, account: &str, tokens: &Tokens) -> Result<(), Error> {
        ensure_secure_tokens_dir(&self.tokens_dir)?;
        let json = serde_json::to_string_pretty(tokens)
            .map_err(|e| Error::Internal(format!("Failed to serialize tokens: {e}")))?;
        write_token_file(&self.path(account), json.as_bytes())
    }

    fn load(&self, account: &str) -> Option<Tokens> {
        let content = std::fs::read_to_string(self.path(account)).ok()?;
        serde_json::from_str(&content).ok()
    }

    fn delete(&self, account: &str) -> Result<(), Error> {
        let p = self.path(account);
        if p.exists() {
            std::fs::remove_file(p)
                .map_err(|e| Error::Internal(format!("Failed to delete tokens: {e}")))?;
        }
        Ok(())
    }
}

/// Create the tokens directory if missing, then on Unix chmod it to 0700 so
/// other users on a shared system can't enumerate or read the token files.
fn ensure_secure_tokens_dir(dir: &std::path::Path) -> Result<(), Error> {
    std::fs::create_dir_all(dir)
        .map_err(|e| Error::Internal(format!("Failed to create token dir: {e}")))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o700);
        if let Err(e) = std::fs::set_permissions(dir, perms) {
            tracing::warn!("Failed to chmod 700 token dir {}: {e}", dir.display());
        }
    }
    Ok(())
}

/// Write a token file with 0600 permissions on Unix (refresh + access tokens
/// are credentials — must not be world-readable). On non-Unix, falls back to
/// std::fs::write which uses platform defaults.
fn write_token_file(path: &std::path::Path, contents: &[u8]) -> Result<(), Error> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .map_err(|e| Error::Internal(format!("Failed to open token file: {e}")))?;
        file.write_all(contents)
            .map_err(|e| Error::Internal(format!("Failed to write token file: {e}")))?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, contents)
            .map_err(|e| Error::Internal(format!("Failed to write tokens: {e}")))?;
    }
    Ok(())
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn make_tokens(email: &str) -> Tokens {
        Tokens {
            access_token: "access-abc".into(),
            refresh_token: "refresh-xyz".into(),
            token_expiry: Utc::now(),
            email: email.into(),
        }
    }

    #[test]
    fn fs_token_store_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsTokenStore::new(dir.path().to_path_buf());

        let tokens = make_tokens("user@example.com");
        store.save("acct", &tokens).unwrap();

        let loaded = store.load("acct").unwrap();
        assert_eq!(loaded.access_token, "access-abc");
        assert_eq!(loaded.refresh_token, "refresh-xyz");
        assert_eq!(loaded.email, "user@example.com");
    }

    #[test]
    fn fs_token_store_load_missing_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsTokenStore::new(dir.path().to_path_buf());
        assert!(store.load("nonexistent").is_none());
    }

    #[test]
    fn fs_token_store_load_corrupted_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsTokenStore::new(dir.path().to_path_buf());
        std::fs::create_dir_all(dir.path()).unwrap();
        std::fs::write(dir.path().join("bad.json"), "not json at all {{{").unwrap();
        assert!(store.load("bad").is_none());
    }

    #[test]
    fn fs_token_store_delete() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsTokenStore::new(dir.path().to_path_buf());

        store.save("acct", &make_tokens("e@e.com")).unwrap();
        assert!(store.load("acct").is_some());
        store.delete("acct").unwrap();
        assert!(store.load("acct").is_none());
    }

    #[test]
    fn fs_token_store_delete_missing_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsTokenStore::new(dir.path().to_path_buf());
        assert!(store.delete("nonexistent").is_ok());
    }

    #[test]
    fn fs_token_store_creates_tokens_dir() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("nested/tokens/dir");
        let store = FsTokenStore::new(nested.clone());
        store.save("acct", &make_tokens("e@e.com")).unwrap();
        assert!(nested.is_dir());
    }

    // ---- Unix file-permission hardening (roborev 173 finding #3) ----

    #[cfg(unix)]
    #[test]
    fn fs_token_store_file_is_chmod_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let store = FsTokenStore::new(dir.path().to_path_buf());
        store.save("acct", &make_tokens("e@e.com")).unwrap();
        let meta = std::fs::metadata(dir.path().join("acct.json")).unwrap();
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "token file should be readable/writable only by owner"
        );
    }

    #[cfg(unix)]
    #[test]
    fn fs_token_store_dir_is_chmod_0700() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("tokens");
        let store = FsTokenStore::new(nested.clone());
        store.save("acct", &make_tokens("e@e.com")).unwrap();
        let meta = std::fs::metadata(&nested).unwrap();
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o700,
            "tokens directory should be accessible only by owner"
        );
    }

    // ---- OAuth callback validation (roborev 173 findings #2 and #9) ----

    #[test]
    fn validate_callback_success_returns_code() {
        let r = CallbackResult {
            code: Some("auth-code-123".into()),
            state: "expected-state".into(),
            error: None,
        };
        let cb = validate_callback(r, "expected-state").unwrap();
        assert_eq!(cb.code, "auth-code-123");
    }

    #[test]
    fn validate_callback_error_redirect_returns_auth_error() {
        // Google sends ?error=access_denied&state=... when the user denies consent.
        // Before the fix, missing `code` caused axum to 400 and hang the server.
        let r = CallbackResult {
            code: None,
            state: "expected-state".into(),
            error: Some("access_denied".into()),
        };
        let err = validate_callback(r, "expected-state").unwrap_err();
        match err {
            Error::Auth(msg) => assert!(msg.contains("access_denied")),
            other => panic!("expected Error::Auth, got {other:?}"),
        }
    }

    #[test]
    fn validate_callback_state_mismatch_returns_auth_error() {
        let r = CallbackResult {
            code: Some("auth-code".into()),
            state: "wrong-state".into(),
            error: None,
        };
        let err = validate_callback(r, "expected-state").unwrap_err();
        match err {
            Error::Auth(msg) => assert!(msg.contains("state mismatch")),
            other => panic!("expected Error::Auth, got {other:?}"),
        }
    }

    #[test]
    fn validate_callback_missing_code_and_error_returns_auth_error() {
        // Pathological case: callback fires with neither code nor error.
        let r = CallbackResult {
            code: None,
            state: "expected-state".into(),
            error: None,
        };
        assert!(matches!(
            validate_callback(r, "expected-state"),
            Err(Error::Auth(_))
        ));
    }

    #[test]
    fn validate_callback_error_takes_precedence_over_state_mismatch() {
        // If provider redirected with both error and a (mismatched) state,
        // surface the error — it's the more useful diagnosis.
        let r = CallbackResult {
            code: None,
            state: "wrong-state".into(),
            error: Some("invalid_request".into()),
        };
        let err = validate_callback(r, "expected-state").unwrap_err();
        match err {
            Error::Auth(msg) => assert!(msg.contains("invalid_request")),
            other => panic!("expected Error::Auth, got {other:?}"),
        }
    }
}
