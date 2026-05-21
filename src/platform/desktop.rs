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

/// Run the desktop OAuth2 callback flow: bind a loopback listener on `port`,
/// open the auth URL in the user's browser, await the `/callback` request,
/// validate the state parameter (CSRF), and return the authorization code.
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

    let (tx, rx) = oneshot::channel::<(String, String)>();
    let tx = Arc::new(Mutex::new(Some(tx)));

    #[derive(Deserialize)]
    struct CallbackParams {
        code: String,
        #[serde(default)]
        state: String,
    }

    let tx_clone = tx.clone();
    let callback_app = Router::new().route(
        "/callback",
        get(move |Query(params): Query<CallbackParams>| {
            let tx = tx_clone.clone();
            async move {
                if let Some(sender) = tx.lock().await.take() {
                    let _ = sender.send((params.code, params.state));
                }
                "Authorization successful! You can close this tab."
            }
        }),
    );

    let server = axum::serve(listener, callback_app);

    tokio::select! {
        result = server => {
            result.map_err(|e| Error::Internal(format!("OAuth callback server error: {e}")))?;
            Err(Error::Internal("OAuth callback server exited without receiving code".into()))
        }
        code_and_state = rx => {
            let (code, state) = code_and_state
                .map_err(|_| Error::Internal("OAuth flow cancelled".into()))?;

            if state != expected_state {
                tracing::warn!(state_matches = false, "OAuth callback received with mismatched state");
                return Err(Error::Auth("OAuth state mismatch — possible CSRF attack".into()));
            }
            tracing::debug!(state_matches = true, "OAuth callback received");

            Ok(OauthCallback { code })
        }
    }
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
        std::fs::create_dir_all(&self.tokens_dir)
            .map_err(|e| Error::Internal(format!("Failed to create token dir: {e}")))?;
        let json = serde_json::to_string_pretty(tokens)
            .map_err(|e| Error::Internal(format!("Failed to serialize tokens: {e}")))?;
        std::fs::write(self.path(account), json)
            .map_err(|e| Error::Internal(format!("Failed to write tokens: {e}")))?;
        Ok(())
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
}
