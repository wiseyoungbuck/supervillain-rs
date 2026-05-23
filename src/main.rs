use std::collections::HashMap;
use std::sync::Arc;

use vimmail::{
    accounts::{self, AccountConfig},
    gmail, jmap, outlook, platform,
    platform::{FsTokenStore, TokenStore},
    provider,
    provider::ProviderSession,
    routes, splits,
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

    let cfg = accounts::parse_config(&config_path);
    let token_store: Arc<dyn TokenStore> = Arc::new(FsTokenStore::new(tokens_dir.clone()));

    let mut sessions: HashMap<String, SessionLock> = HashMap::new();
    let mut account_errors: Vec<AccountError> = Vec::new();

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
            "No accounts configured or connected. Open http://127.0.0.1:8000/ \
             to add an account via the settings UI."
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
                if let Some(config) = splits::seed_from_identities(&identities, &splits_config_path)
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
        config_path,
        tokens_dir,
        token_store,
        authorizing: accounts::AuthorizingSlot::default(),
    });

    let app = routes::router(state);

    let addr = "127.0.0.1:8000";
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap_or_else(|e| {
        panic!("Failed to bind to {addr}: {e}. Is another instance of supervillain already running? Try: kill $(lsof -ti :{port})", port = addr.split(':').next_back().unwrap_or("8000"));
    });
    let url = format!("http://{addr}");
    tracing::info!("Listening on {url}");

    if !std::env::args().any(|a| a == "--no-browser") {
        platform::open_browser(&url);
    }

    axum::serve(listener, app).await.unwrap();
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
}
