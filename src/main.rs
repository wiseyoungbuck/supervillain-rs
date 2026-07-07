use std::collections::HashMap;
use std::sync::Arc;

use supervillain::{
    accounts::{self, AccountConfig},
    gmail, jmap, outlook, platform,
    platform::{FsTokenStore, TokenStore},
    prefetch, provider,
    provider::ProviderSession,
    routes, splits, timezone,
    types::{AccountError, AccountRegistry, AppState, SessionLock},
};

#[tokio::main]
async fn main() {
    let config_dir = platform::config_dir();
    let config_path = config_dir.join("supervillain/config");
    let tokens_dir = config_dir.join("supervillain/tokens");
    let splits_config_path = config_dir.join("supervillain/splits.json");
    let timezone_config_path = config_dir.join("supervillain/timezone.json");

    platform::init_tracing();

    let (cfg, parse_errors) = accounts::parse_config(&config_path);
    let token_store: Arc<dyn TokenStore> = Arc::new(FsTokenStore::new(tokens_dir.clone()));

    let mut sessions: HashMap<String, SessionLock> = HashMap::new();
    // Validate sibling config files at startup. Route handlers tolerate parse
    // failures by falling back to defaults (a transient FS error shouldn't
    // 500 a request); startup is the one place we can loudly tell the user
    // their hand-edited file is broken before defaults silently kick in.
    let mut account_errors: Vec<AccountError> = accounts::startup_config_errors(
        &config_path,
        parse_errors.clone(),
        &splits_config_path,
        splits::try_load_splits(&splits_config_path),
        &timezone_config_path,
        timezone::try_load_config(&timezone_config_path),
    );

    for (name, account) in &cfg.accounts {
        match load_session(name, account, &tokens_dir, &token_store).await {
            Ok(session) => {
                sessions.insert(
                    name.clone(),
                    SessionLock::new(tokio::sync::RwLock::new(session)),
                );
            }
            Err(e) => {
                tracing::warn!("[{name}] {}", e.error);
                account_errors.push(e);
            }
        }
    }

    if sessions.is_empty() {
        tracing::warn!(
            "No accounts configured or connected. Open {}/ to add an account \
             via the settings UI.",
            browser_url(&bind_addr(
                std::env::var("SUPERVILLAIN_BIND").ok().as_deref()
            ))
        );
        if cfg.accounts.is_empty() {
            account_errors.push(AccountError {
                account: "setup".into(),
                provider: "setup".into(),
                error: "No accounts configured — visit settings to add one".into(),
            });
        }
    }

    let default_account =
        resolve_default_account(cfg.default_account.clone().unwrap_or_default(), &sessions);

    // Auto-seed split tabs from the default account's identities. Skipped on
    // an empty registry; the first-run UI will surface the same prompt.
    if let Some(session_lock) = sessions.get(&default_account) {
        let mut session = session_lock.write().await;
        match provider::get_identities(&mut session).await {
            Ok(identities) => {
                if let Some(config) =
                    splits::seed_from_identities(&identities, &default_account, &splits_config_path)
                {
                    let names: Vec<_> = config.splits.iter().map(|s| s.name.as_str()).collect();
                    tracing::info!("Auto-created split tabs: {}", names.join(", "));
                }
            }
            Err(e) => tracing::warn!("Failed to fetch identities for split seeding: {e}"),
        }
    }

    let state = Arc::new(AppState {
        accounts: tokio::sync::RwLock::new(AccountRegistry {
            sessions,
            account_configs: cfg.accounts.clone(),
            default_account,
        }),
        account_errors: tokio::sync::RwLock::new(account_errors),
        splits_config_path,
        timezone_config_path,
        timezone_write_lock: tokio::sync::Mutex::new(()),
        config_path,
        tokens_dir,
        token_store,
        authorizing: accounts::AuthorizingSlot::default(),
        config_error_baseline: std::sync::RwLock::new(parse_errors),
        prefetch: std::sync::Arc::new(prefetch::PrefetchCache::new()),
    });

    // Kick off the background prefetch warmer. The first pass starts
    // ~200 ms after spawn (let the HTTP server bind first) and re-runs
    // every 5 minutes for every connected account, keeping the
    // mailbox / identity / inbox / split-count caches warm so account
    // switches return from cache instead of waiting on ~24 s of Gmail
    // split-count requests.
    prefetch::spawn_warmer(state.clone(), std::time::Duration::from_secs(300));

    let app = routes::router(state);

    let addr = bind_addr(std::env::var("SUPERVILLAIN_BIND").ok().as_deref());
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap_or_else(|e| {
        panic!("Failed to bind to {addr}: {e}. Is another instance of supervillain already running? Try: kill $(lsof -ti :{port})", port = addr.split(':').next_back().unwrap_or("8000"));
    });
    let url = browser_url(&addr);
    tracing::info!("Listening on {addr}; local UI at {url}");

    if !std::env::args().any(|a| a == "--no-browser") {
        platform::open_browser(&url);
    }

    axum::serve(listener, app).await.unwrap();
}

/// Bind address: `SUPERVILLAIN_BIND` env var, defaulting to loopback.
/// Binding beyond loopback (e.g. `0.0.0.0:8000` for LAN/tailnet access,
/// as scripts/upgrade.sh and the launcher do) is an explicit per-deploy
/// opt-in — there is no authentication layer, so a non-loopback bind
/// trusts every host that can reach the interface (roborev 273).
fn bind_addr(env_value: Option<&str>) -> String {
    match env_value.map(str::trim) {
        Some(v) if !v.is_empty() => v.to_string(),
        _ => "127.0.0.1:8000".to_string(),
    }
}

/// Local UI URL for the auto-opened browser, derived from the bind
/// address so the port is defined in exactly one place. A wildcard bind
/// isn't routable in a URL; loopback always is.
fn browser_url(addr: &str) -> String {
    let (host, port) = addr.rsplit_once(':').unwrap_or((addr, "8000"));
    let host = match host {
        "0.0.0.0" | "[::]" | "::" | "" => "127.0.0.1",
        h => h,
    };
    format!("http://{host}:{port}")
}

/// Resolve the effective default account. Prefer the configured value if it
/// connected; otherwise pick any connected account; otherwise empty string.
fn resolve_default_account<V>(preferred: String, sessions: &HashMap<String, V>) -> String {
    if sessions.contains_key(&preferred) {
        preferred
    } else if let Some(first) = sessions.keys().next() {
        if !preferred.is_empty() {
            tracing::warn!(
                "Default account '{preferred}' failed to connect, falling back to '{first}'"
            );
        }
        first.clone()
    } else {
        String::new()
    }
}

/// Load a session for one account. Synchronous connect for Fastmail (HTTP
/// only). For Outlook/Gmail, only load existing tokens — never block startup
/// on a browser-driven OAuth flow. Missing tokens surface as an
/// account_error that the UI exposes via the Authorize button.
async fn load_session(
    name: &str,
    account: &AccountConfig,
    tokens_dir: &std::path::Path,
    token_store: &Arc<dyn TokenStore>,
) -> Result<ProviderSession, AccountError> {
    // Fail fast on credentials that can't possibly work (e.g. a Fastmail
    // token pasted as an Azure client-id). Loading a session anyway would
    // produce a zombie account whose every token refresh fails with an
    // opaque provider error.
    if let Some(msg) = accounts::credential_shape_error(account) {
        return Err(AccountError {
            account: name.into(),
            provider: account.provider_str().into(),
            error: format!("{msg} — fix the account in Settings"),
        });
    }
    match account {
        AccountConfig::Fastmail {
            username,
            api_token,
        } => {
            let mut session = jmap::JmapSession::new(username, &format!("Bearer {api_token}"));
            jmap::connect(&mut session)
                .await
                .map_err(|e| AccountError {
                    account: name.into(),
                    provider: "fastmail".into(),
                    error: format!("Connection failed: {e}"),
                })?;
            match jmap::get_mailboxes(&session).await {
                Ok(mailboxes) => {
                    for mb in &mailboxes {
                        if let Some(ref role) = mb.role {
                            session.mailbox_cache.insert(role.clone(), mb.clone());
                        }
                    }
                    tracing::info!(
                        "[{name}] Connected as {username}, {} mailboxes",
                        mailboxes.len()
                    );
                    Ok(ProviderSession::Fastmail(Box::new(session)))
                }
                Err(e) => Err(AccountError {
                    account: name.into(),
                    provider: "fastmail".into(),
                    error: format!("Failed to fetch mailboxes: {e}"),
                }),
            }
        }

        AccountConfig::Outlook { client_id, .. } => {
            let token_path = accounts::token_file_path(tokens_dir, name);
            if let Some(s) = outlook::load_tokens(&token_path, client_id) {
                tracing::info!("[{name}] Loaded Outlook tokens for {}", s.email);
                Ok(ProviderSession::Outlook(Box::new(s)))
            } else {
                Err(AccountError {
                    account: name.into(),
                    provider: "outlook".into(),
                    error: "Not authorized — open settings and click Authorize".into(),
                })
            }
        }

        AccountConfig::Gmail {
            client_id,
            client_secret,
            ..
        } => {
            if let Some(s) =
                gmail::load_session(token_store.clone(), name, client_id, client_secret)
            {
                tracing::info!("[{name}] Loaded Gmail tokens for {}", s.email);
                Ok(ProviderSession::Gmail(Box::new(s)))
            } else {
                Err(AccountError {
                    account: name.into(),
                    provider: "gmail".into(),
                    error: "Not authorized — open settings and click Authorize".into(),
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_default_account_present() {
        let sessions: HashMap<String, ()> = HashMap::from([("fastmail".into(), ())]);
        assert_eq!(
            resolve_default_account("fastmail".into(), &sessions),
            "fastmail"
        );
    }

    #[test]
    fn resolve_default_account_missing_falls_back() {
        let sessions: HashMap<String, ()> = HashMap::from([("outlook".into(), ())]);
        assert_eq!(
            resolve_default_account("fastmail".into(), &sessions),
            "outlook"
        );
    }

    #[test]
    fn resolve_default_account_empty_sessions() {
        let sessions: HashMap<String, ()> = HashMap::new();
        assert_eq!(resolve_default_account("fastmail".into(), &sessions), "");
    }

    #[test]
    fn resolve_default_account_empty_preferred_picks_first() {
        let sessions: HashMap<String, ()> = HashMap::from([("only".into(), ())]);
        assert_eq!(resolve_default_account(String::new(), &sessions), "only");
    }

    // ---- bind_addr / browser_url (roborev 273) ----

    #[test]
    fn bind_addr_defaults_to_loopback() {
        // Safe by default: exposing the unauthenticated API beyond this
        // machine must be an explicit opt-in, not the compiled-in state.
        assert_eq!(bind_addr(None), "127.0.0.1:8000");
    }

    #[test]
    fn bind_addr_env_overrides() {
        assert_eq!(bind_addr(Some("0.0.0.0:8000")), "0.0.0.0:8000");
        assert_eq!(bind_addr(Some("100.64.1.5:9000")), "100.64.1.5:9000");
    }

    #[test]
    fn bind_addr_blank_env_falls_back_to_default() {
        assert_eq!(bind_addr(Some("")), "127.0.0.1:8000");
        assert_eq!(bind_addr(Some("   ")), "127.0.0.1:8000");
    }

    #[test]
    fn browser_url_wildcard_bind_opens_loopback() {
        assert_eq!(browser_url("0.0.0.0:8000"), "http://127.0.0.1:8000");
    }

    #[test]
    fn browser_url_carries_the_bind_port() {
        // The port must be defined once (in the bind address) and derived
        // everywhere else — a changed port silently opening the wrong URL
        // was the original duplication bug.
        assert_eq!(browser_url("127.0.0.1:9000"), "http://127.0.0.1:9000");
        assert_eq!(browser_url("0.0.0.0:9000"), "http://127.0.0.1:9000");
    }

    #[test]
    fn browser_url_specific_host_is_used_as_is() {
        // Bound to a single non-loopback interface (e.g. a tailnet IP),
        // loopback may not be listening at all — open what we bound.
        assert_eq!(browser_url("100.64.1.5:8000"), "http://100.64.1.5:8000");
    }
}
