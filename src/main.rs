use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use vimmail::{
    gmail, jmap, outlook, platform,
    platform::{FsTokenStore, TokenStore},
    provider,
    provider::ProviderSession,
    routes, splits,
    types::{AccountError, AppState},
};

#[tokio::main]
async fn main() {
    let config_dir = platform::config_dir();
    let config_path = config_dir.join("supervillain/config");

    platform::init_tracing();

    let raw_config = load_config(&config_path);
    let accounts = parse_accounts(&raw_config, &config_path);

    if accounts.is_empty() {
        eprintln!(
            "No accounts configured.\n\nCreate {} with:\n\n  \
             username = you@fastmail.com\n  \
             api-token = your-token\n\n\
             Or for multi-account:\n\n  \
             default-account = fastmail\n\n  \
             [fastmail]\n  \
             provider = fastmail\n  \
             username = you@fastmail.com\n  \
             api-token = your-token\n\n  \
             [outlook]\n  \
             provider = outlook\n  \
             client-id = your-azure-client-id\n",
            config_path.display()
        );
        std::process::exit(1);
    }

    let preferred_default = raw_config
        .get("default-account")
        .cloned()
        .unwrap_or_else(|| accounts.keys().next().unwrap().clone());

    let tokens_dir = config_dir.join("supervillain/tokens");
    let token_store: Arc<dyn TokenStore> = Arc::new(FsTokenStore::new(tokens_dir.clone()));
    let mut sessions: HashMap<String, tokio::sync::RwLock<ProviderSession>> = HashMap::new();
    let mut account_errors: Vec<AccountError> = Vec::new();

    for (name, account) in &accounts {
        if let Err(e) = validate_provider(name, &account.provider) {
            tracing::warn!("[{name}] {}", e.error);
            account_errors.push(e);
            continue;
        }

        match account.provider.as_str() {
            "fastmail" => {
                let (username, token) =
                    match validate_fastmail_config(name, account, |k| std::env::var(k)) {
                        Ok(creds) => creds,
                        Err(e) => {
                            tracing::warn!("[{name}] {}", e.error);
                            account_errors.push(e);
                            continue;
                        }
                    };

                let mut session = jmap::JmapSession::new(&username, &format!("Bearer {token}"));
                if let Err(e) = jmap::connect(&mut session).await {
                    tracing::warn!("[{name}] Failed to connect: {e}");
                    account_errors.push(AccountError {
                        account: name.clone(),
                        provider: "fastmail".into(),
                        error: format!("Connection failed: {e}"),
                    });
                    continue;
                }

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
                    }
                    Err(e) => {
                        tracing::warn!("[{name}] Failed to fetch mailboxes: {e}");
                        account_errors.push(AccountError {
                            account: name.clone(),
                            provider: "fastmail".into(),
                            error: format!("Failed to fetch mailboxes: {e}"),
                        });
                        continue;
                    }
                }

                sessions.insert(
                    name.clone(),
                    tokio::sync::RwLock::new(ProviderSession::Fastmail(session)),
                );
            }

            "outlook" => {
                let client_id = match validate_outlook_config(name, account) {
                    Ok(id) => id,
                    Err(e) => {
                        tracing::warn!("[{name}] {}", e.error);
                        account_errors.push(e);
                        continue;
                    }
                };

                let token_path = tokens_dir.join(format!("{name}.json"));
                let session = if let Some(s) = outlook::load_tokens(&token_path, &client_id) {
                    tracing::info!("[{name}] Loaded Outlook tokens for {}", s.email);
                    s
                } else {
                    tracing::info!("[{name}] No saved tokens, starting OAuth flow...");
                    match outlook::oauth_flow(&client_id, &token_path).await {
                        Ok(s) => s,
                        Err(e) => {
                            tracing::warn!("[{name}] OAuth flow failed: {e}");
                            account_errors.push(AccountError {
                                account: name.clone(),
                                provider: "outlook".into(),
                                error: format!("OAuth flow failed: {e}"),
                            });
                            continue;
                        }
                    }
                };

                sessions.insert(
                    name.clone(),
                    tokio::sync::RwLock::new(ProviderSession::Outlook(session)),
                );
            }

            "gmail" => {
                let (client_id, client_secret) = match validate_gmail_config(name, account) {
                    Ok(creds) => creds,
                    Err(e) => {
                        tracing::warn!("[{name}] {}", e.error);
                        account_errors.push(e);
                        continue;
                    }
                };

                let session = if let Some(s) =
                    gmail::load_session(token_store.clone(), name, &client_id, &client_secret)
                {
                    tracing::info!("[{name}] Loaded Gmail tokens for {}", s.email);
                    s
                } else {
                    tracing::info!("[{name}] No saved tokens, starting Gmail OAuth flow...");
                    match gmail::oauth_flow(token_store.clone(), name, &client_id, &client_secret)
                        .await
                    {
                        Ok(s) => s,
                        Err(e) => {
                            tracing::warn!("[{name}] Gmail OAuth flow failed: {e}");
                            account_errors.push(AccountError {
                                account: name.clone(),
                                provider: "gmail".into(),
                                error: format!("Gmail OAuth flow failed: {e}"),
                            });
                            continue;
                        }
                    }
                };

                sessions.insert(
                    name.clone(),
                    tokio::sync::RwLock::new(ProviderSession::Gmail(Box::new(session))),
                );
            }

            other => {
                tracing::warn!("[{name}] Unknown provider '{other}'");
                account_errors.push(AccountError {
                    account: name.clone(),
                    provider: other.into(),
                    error: format!("Unknown provider '{other}'"),
                });
                continue;
            }
        }
    }

    let default_account = resolve_default_account(preferred_default, &sessions);

    // Auto-seed split tabs from the default account's identities
    let splits_config_path = config_dir.join("supervillain/splits.json");
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
        sessions,
        account_errors,
        default_account,
        splits_config_path,
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

// =============================================================================
// Config parsing
// =============================================================================

/// Parse top-level key = value pairs from the config file (like ghostty/omarchy).
/// Lines starting with # are comments. Blank lines are ignored.
/// Stops at the first [section] header — keys inside sections are handled by parse_accounts.
fn load_config(path: &PathBuf) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return map,
    };
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // Stop at first section header — only collect top-level keys
        if line.starts_with('[') && line.ends_with(']') {
            break;
        }
        if let Some((key, value)) = line.split_once('=') {
            map.insert(key.trim().to_string(), value.trim().to_string());
        }
    }
    map
}

/// A parsed account from the config file
struct AccountConfig {
    provider: String,
    props: HashMap<String, String>,
}

impl AccountConfig {
    fn get(&self, key: &str) -> Option<String> {
        self.props.get(key).cloned()
    }
}

fn validate_provider(name: &str, provider: &str) -> Result<(), AccountError> {
    match provider {
        "fastmail" | "outlook" | "gmail" => Ok(()),
        other => Err(AccountError {
            account: name.into(),
            provider: other.into(),
            error: format!("Unknown provider '{other}'"),
        }),
    }
}

fn validate_fastmail_config(
    name: &str,
    account: &AccountConfig,
    env_var: impl Fn(&str) -> Result<String, std::env::VarError>,
) -> Result<(String, String), AccountError> {
    let username = account
        .get("username")
        .or_else(|| env_var("FASTMAIL_USERNAME").ok())
        .ok_or_else(|| AccountError {
            account: name.into(),
            provider: "fastmail".into(),
            error: "Missing 'username' in config and FASTMAIL_USERNAME env var not set".into(),
        })?;
    let token = account
        .get("api-token")
        .or_else(|| env_var("FASTMAIL_API_TOKEN").ok())
        .ok_or_else(|| AccountError {
            account: name.into(),
            provider: "fastmail".into(),
            error: "Missing 'api-token' in config and FASTMAIL_API_TOKEN env var not set".into(),
        })?;
    Ok((username, token))
}

fn validate_outlook_config(name: &str, account: &AccountConfig) -> Result<String, AccountError> {
    account.get("client-id").ok_or_else(|| AccountError {
        account: name.into(),
        provider: "outlook".into(),
        error: "Missing 'client-id' in config".into(),
    })
}

fn validate_gmail_config(
    name: &str,
    account: &AccountConfig,
) -> Result<(String, String), AccountError> {
    let client_id = account.get("client-id").ok_or_else(|| AccountError {
        account: name.into(),
        provider: "gmail".into(),
        error: "Gmail requires both client-id and client-secret in [gmail] section. \
                Google's OAuth requires a client_secret even for Desktop apps using PKCE — \
                see README §Gmail setup."
            .into(),
    })?;
    let client_secret = account.get("client-secret").ok_or_else(|| AccountError {
        account: name.into(),
        provider: "gmail".into(),
        error: "Gmail requires both client-id and client-secret in [gmail] section. \
                Google's OAuth requires a client_secret even for Desktop apps using PKCE — \
                see README §Gmail setup."
            .into(),
    })?;
    Ok((client_id, client_secret))
}

fn resolve_default_account<V>(preferred: String, sessions: &HashMap<String, V>) -> String {
    if sessions.contains_key(&preferred) {
        preferred
    } else if let Some(first) = sessions.keys().next() {
        tracing::warn!(
            "Default account '{preferred}' failed to connect, falling back to '{first}'"
        );
        first.clone()
    } else {
        tracing::warn!("All accounts failed to connect");
        String::new()
    }
}

/// Parse config into account sections.
///
/// Supports two formats:
/// 1. Legacy flat format (single Fastmail account):
///    ```
///    username = you@fastmail.com
///    api-token = your-token
///    ```
///
/// 2. Multi-account format with [sections]:
///    ```
///    default-account = fastmail
///
///    [fastmail]
///    provider = fastmail
///    username = you@fastmail.com
///    api-token = your-token
///
///    [outlook]
///    provider = outlook
///    client-id = your-client-id
///    ```
fn parse_accounts(
    flat_config: &HashMap<String, String>,
    config_path: &PathBuf,
) -> HashMap<String, AccountConfig> {
    let mut accounts = HashMap::new();

    // Try to parse sectioned config
    let content = match std::fs::read_to_string(config_path) {
        Ok(c) => c,
        Err(_) => {
            // Fall back to env vars for legacy single-account
            if std::env::var("FASTMAIL_USERNAME").is_ok() {
                accounts.insert(
                    "fastmail".to_string(),
                    AccountConfig {
                        provider: "fastmail".to_string(),
                        props: HashMap::new(),
                    },
                );
            }
            return accounts;
        }
    };

    let mut current_section: Option<String> = None;
    let mut sections: HashMap<String, HashMap<String, String>> = HashMap::new();

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            let name = line[1..line.len() - 1].trim().to_string();
            current_section = Some(name.clone());
            sections.entry(name).or_default();
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            let key = key.trim().to_string();
            let value = value.trim().to_string();
            if let Some(ref section) = current_section {
                sections
                    .entry(section.clone())
                    .or_default()
                    .insert(key, value);
            }
        }
    }

    // If we found sections, use them
    if !sections.is_empty() {
        for (name, props) in sections {
            let provider = props
                .get("provider")
                .cloned()
                .unwrap_or_else(|| "fastmail".to_string());
            accounts.insert(name, AccountConfig { provider, props });
        }
        return accounts;
    }

    // Legacy flat format — single Fastmail account
    if flat_config.contains_key("username") || flat_config.contains_key("api-token") {
        accounts.insert(
            "fastmail".to_string(),
            AccountConfig {
                provider: "fastmail".to_string(),
                props: flat_config.clone(),
            },
        );
    }

    accounts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_config_parses_key_value() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        std::fs::write(&path, "username = alice@test.com\napi-token = tok123\n").unwrap();
        let config = load_config(&path.to_path_buf());
        assert_eq!(config.get("username").unwrap(), "alice@test.com");
        assert_eq!(config.get("api-token").unwrap(), "tok123");
    }

    #[test]
    fn load_config_ignores_comments_and_blanks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        std::fs::write(&path, "# comment\n\nkey = val\n# another\n").unwrap();
        let config = load_config(&path.to_path_buf());
        assert_eq!(config.len(), 1);
        assert_eq!(config.get("key").unwrap(), "val");
    }

    #[test]
    fn load_config_missing_file_returns_empty() {
        let config = load_config(&PathBuf::from("/tmp/supervillain-nonexistent-config"));
        assert!(config.is_empty());
    }

    #[test]
    fn load_config_trims_whitespace() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        std::fs::write(&path, "  key  =  val  \n").unwrap();
        let config = load_config(&path.to_path_buf());
        assert_eq!(config.get("key").unwrap(), "val");
    }

    #[test]
    fn load_config_handles_equals_in_value() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        std::fs::write(&path, "url = http://example.com?a=b\n").unwrap();
        let config = load_config(&path.to_path_buf());
        assert_eq!(config.get("url").unwrap(), "http://example.com?a=b");
    }

    #[test]
    fn parse_accounts_legacy_flat_format() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        std::fs::write(&path, "username = you@fastmail.com\napi-token = fmu1-xxx\n").unwrap();
        let flat = load_config(&path.to_path_buf());
        let accounts = parse_accounts(&flat, &path.to_path_buf());
        assert_eq!(accounts.len(), 1);
        let acct = accounts.get("fastmail").unwrap();
        assert_eq!(acct.provider, "fastmail");
        assert_eq!(acct.get("username").unwrap(), "you@fastmail.com");
    }

    #[test]
    fn parse_accounts_sectioned_format() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        std::fs::write(
            &path,
            "[fastmail]\nprovider = fastmail\nusername = a@fm.com\napi-token = tok\n\n[outlook]\nprovider = outlook\nclient-id = cid\n",
        )
        .unwrap();
        let flat = load_config(&path.to_path_buf());
        let accounts = parse_accounts(&flat, &path.to_path_buf());
        assert_eq!(accounts.len(), 2);
        assert_eq!(accounts.get("fastmail").unwrap().provider, "fastmail");
        assert_eq!(accounts.get("outlook").unwrap().provider, "outlook");
        assert_eq!(
            accounts.get("outlook").unwrap().get("client-id").unwrap(),
            "cid"
        );
    }

    #[test]
    fn parse_accounts_provider_defaults_to_fastmail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        std::fs::write(&path, "[myaccount]\nusername = a@b.com\napi-token = tok\n").unwrap();
        let flat = load_config(&path.to_path_buf());
        let accounts = parse_accounts(&flat, &path.to_path_buf());
        assert_eq!(accounts.get("myaccount").unwrap().provider, "fastmail");
    }

    #[test]
    fn parse_accounts_empty_file_with_env_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        std::fs::write(&path, "").unwrap();
        let flat = load_config(&path.to_path_buf());
        // No sections, no flat keys, no env vars → empty
        let accounts = parse_accounts(&flat, &path.to_path_buf());
        assert!(accounts.is_empty());
    }

    #[test]
    fn parse_accounts_sections_win_over_flat() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        // Config has both flat keys and sections — sections should be used
        std::fs::write(
            &path,
            "username = flat@test.com\napi-token = flat-tok\n\n[sectioned]\nprovider = fastmail\nusername = sect@test.com\napi-token = sect-tok\n",
        )
        .unwrap();
        let flat = load_config(&path.to_path_buf());
        let accounts = parse_accounts(&flat, &path.to_path_buf());
        // Sections present → flat ignored
        assert_eq!(accounts.len(), 1);
        assert!(accounts.contains_key("sectioned"));
    }

    fn no_env(_: &str) -> Result<String, std::env::VarError> {
        Err(std::env::VarError::NotPresent)
    }

    #[test]
    fn validate_fastmail_config_valid() {
        let account = AccountConfig {
            provider: "fastmail".into(),
            props: HashMap::from([
                ("username".into(), "alice@fastmail.com".into()),
                ("api-token".into(), "fmu1-xxx".into()),
            ]),
        };
        let result = validate_fastmail_config("personal", &account, no_env);
        assert!(result.is_ok());
        let (username, token) = result.unwrap();
        assert_eq!(username, "alice@fastmail.com");
        assert_eq!(token, "fmu1-xxx");
    }

    #[test]
    fn validate_fastmail_config_missing_username() {
        let account = AccountConfig {
            provider: "fastmail".into(),
            props: HashMap::from([("api-token".into(), "fmu1-xxx".into())]),
        };
        let result = validate_fastmail_config("personal", &account, no_env);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.account, "personal");
        assert_eq!(err.provider, "fastmail");
        assert!(err.error.contains("username"));
    }

    #[test]
    fn validate_fastmail_config_missing_token() {
        let account = AccountConfig {
            provider: "fastmail".into(),
            props: HashMap::from([("username".into(), "alice@fastmail.com".into())]),
        };
        let result = validate_fastmail_config("personal", &account, no_env);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.account, "personal");
        assert!(err.error.contains("api-token"));
    }

    #[test]
    fn validate_outlook_config_valid() {
        let account = AccountConfig {
            provider: "outlook".into(),
            props: HashMap::from([("client-id".into(), "abc-123".into())]),
        };
        let result = validate_outlook_config("work", &account);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "abc-123");
    }

    #[test]
    fn validate_outlook_config_missing_client_id() {
        let account = AccountConfig {
            provider: "outlook".into(),
            props: HashMap::new(),
        };
        let result = validate_outlook_config("work", &account);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.account, "work");
        assert_eq!(err.provider, "outlook");
        assert!(err.error.contains("client-id"));
    }

    #[test]
    fn validate_provider_known() {
        assert!(validate_provider("test", "fastmail").is_ok());
        assert!(validate_provider("test", "outlook").is_ok());
        assert!(validate_provider("test", "gmail").is_ok());
    }

    #[test]
    fn validate_provider_unknown() {
        let result = validate_provider("test", "yahoo");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.account, "test");
        assert!(err.error.contains("yahoo"));
    }

    #[test]
    fn resolve_default_account_present() {
        let sessions: HashMap<String, ()> = HashMap::from([("fastmail".into(), ())]);
        let result = resolve_default_account("fastmail".into(), &sessions);
        assert_eq!(result, "fastmail");
    }

    #[test]
    fn resolve_default_account_missing_falls_back() {
        let mut sessions: HashMap<String, ()> = HashMap::new();
        sessions.insert("outlook".into(), ());
        let result = resolve_default_account("fastmail".into(), &sessions);
        assert_eq!(result, "outlook");
    }

    #[test]
    fn resolve_default_account_empty_sessions() {
        let sessions: HashMap<String, ()> = HashMap::new();
        let result = resolve_default_account("fastmail".into(), &sessions);
        assert_eq!(result, "");
    }

    #[test]
    fn validate_fastmail_config_env_fallback_for_username() {
        let account = AccountConfig {
            provider: "fastmail".into(),
            props: HashMap::from([("api-token".into(), "fmu1-xxx".into())]),
        };
        // Simulate env var providing the username
        let env = |key: &str| match key {
            "FASTMAIL_USERNAME" => Ok("env-user@fastmail.com".into()),
            _ => Err(std::env::VarError::NotPresent),
        };
        let (username, token) = validate_fastmail_config("personal", &account, env)
            .expect("should succeed with env fallback for username");
        assert_eq!(username, "env-user@fastmail.com");
        assert_eq!(token, "fmu1-xxx");
    }

    #[test]
    fn validate_fastmail_config_env_fallback_for_token() {
        let account = AccountConfig {
            provider: "fastmail".into(),
            props: HashMap::from([("username".into(), "alice@fastmail.com".into())]),
        };
        // Simulate env var providing the token
        let env = |key: &str| match key {
            "FASTMAIL_API_TOKEN" => Ok("fmu1-from-env".into()),
            _ => Err(std::env::VarError::NotPresent),
        };
        let (username, token) = validate_fastmail_config("personal", &account, env)
            .expect("should succeed with env fallback for token");
        assert_eq!(username, "alice@fastmail.com");
        assert_eq!(token, "fmu1-from-env");
    }

    #[test]
    fn validate_fastmail_config_prefers_config_over_env() {
        let account = AccountConfig {
            provider: "fastmail".into(),
            props: HashMap::from([
                ("username".into(), "config-user@fastmail.com".into()),
                ("api-token".into(), "fmu1-config".into()),
            ]),
        };
        // Env provides different values — config should win
        let env = |key: &str| match key {
            "FASTMAIL_USERNAME" => Ok("env-user@fastmail.com".into()),
            "FASTMAIL_API_TOKEN" => Ok("fmu1-env".into()),
            _ => Err(std::env::VarError::NotPresent),
        };
        let (username, token) = validate_fastmail_config("personal", &account, env)
            .expect("config values should take precedence over env vars");
        assert_eq!(username, "config-user@fastmail.com");
        assert_eq!(token, "fmu1-config");
    }

    #[test]
    fn load_config_stops_at_section_header() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        std::fs::write(
            &path,
            "default-account = fastmail\n\n[fastmail]\nusername = a@fm.com\napi-token = tok\n",
        )
        .unwrap();
        let config = load_config(&path.to_path_buf());
        // Only the top-level key should be captured, not keys inside sections
        assert_eq!(config.len(), 1);
        assert_eq!(config.get("default-account").unwrap(), "fastmail");
        assert!(!config.contains_key("username"));
    }

    #[test]
    fn parse_accounts_default_account_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        std::fs::write(
            &path,
            "default-account = work\n\n[personal]\nprovider = fastmail\nusername = a@fm.com\napi-token = tok\n\n[work]\nprovider = fastmail\nusername = b@fm.com\napi-token = tok2\n",
        )
        .unwrap();
        let flat = load_config(&path.to_path_buf());
        // default-account should be parsed from flat config
        assert_eq!(flat.get("default-account").unwrap(), "work");
        let accounts = parse_accounts(&flat, &path.to_path_buf());
        assert_eq!(accounts.len(), 2);
    }

    #[test]
    fn validate_provider_gmail_known() {
        assert!(validate_provider("test", "gmail").is_ok());
    }

    #[test]
    fn validate_gmail_config_valid() {
        let account = AccountConfig {
            provider: "gmail".into(),
            props: HashMap::from([
                ("client-id".into(), "gmail-client-123".into()),
                ("client-secret".into(), "GOCSPX-secret".into()),
            ]),
        };
        let result = validate_gmail_config("personal", &account);
        assert!(result.is_ok());
        let (client_id, client_secret) = result.unwrap();
        assert_eq!(client_id, "gmail-client-123");
        assert_eq!(client_secret, "GOCSPX-secret");
    }

    #[test]
    fn validate_gmail_config_missing_client_id() {
        let account = AccountConfig {
            provider: "gmail".into(),
            props: HashMap::from([("client-secret".into(), "GOCSPX-secret".into())]),
        };
        let result = validate_gmail_config("personal", &account);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.account, "personal");
        assert_eq!(err.provider, "gmail");
        assert!(err.error.contains("client-id"));
        assert!(err.error.contains("README"));
    }

    #[test]
    fn validate_gmail_config_missing_client_secret() {
        let account = AccountConfig {
            provider: "gmail".into(),
            props: HashMap::from([("client-id".into(), "gmail-client-123".into())]),
        };
        let result = validate_gmail_config("personal", &account);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.account, "personal");
        assert_eq!(err.provider, "gmail");
        assert!(err.error.contains("client-secret"));
        assert!(err.error.contains("README"));
    }

    #[test]
    fn parse_accounts_three_providers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        std::fs::write(
            &path,
            "[fastmail]\nprovider = fastmail\nusername = a@fm.com\napi-token = tok\n\n[outlook]\nprovider = outlook\nclient-id = oid\n\n[gmail]\nprovider = gmail\nclient-id = gid\n",
        )
        .unwrap();
        let flat = load_config(&path.to_path_buf());
        let accounts = parse_accounts(&flat, &path.to_path_buf());
        assert_eq!(accounts.len(), 3);
        assert_eq!(accounts.get("fastmail").unwrap().provider, "fastmail");
        assert_eq!(accounts.get("outlook").unwrap().provider, "outlook");
        assert_eq!(accounts.get("gmail").unwrap().provider, "gmail");
        assert_eq!(
            accounts.get("gmail").unwrap().get("client-id").unwrap(),
            "gid"
        );
    }
}
