use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use vimmail::{jmap, outlook, provider::ProviderSession, routes, splits, types::AppState};

#[tokio::main]
async fn main() {
    let config_dir = resolve_config_dir();
    let config_path = config_dir.join("supervillain/config");

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

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

    let default_account = raw_config
        .get("default-account")
        .cloned()
        .unwrap_or_else(|| accounts.keys().next().unwrap().clone());

    let tokens_dir = config_dir.join("supervillain/tokens");
    let mut sessions: HashMap<String, tokio::sync::RwLock<ProviderSession>> = HashMap::new();

    for (name, account) in &accounts {
        match account.provider.as_str() {
            "fastmail" => {
                let username = account
                    .get("username")
                    .or_else(|| std::env::var("FASTMAIL_USERNAME").ok())
                    .unwrap_or_else(|| {
                        eprintln!("username not set for account [{name}]");
                        std::process::exit(1);
                    });

                let token = account
                    .get("api-token")
                    .or_else(|| std::env::var("FASTMAIL_API_TOKEN").ok())
                    .unwrap_or_else(|| {
                        eprintln!("api-token not set for account [{name}]");
                        std::process::exit(1);
                    });

                let mut session = jmap::JmapSession::new(&username, &format!("Bearer {token}"));
                jmap::connect(&mut session)
                    .await
                    .expect("Failed to connect to Fastmail");

                // Cache mailboxes
                let mailboxes = jmap::get_mailboxes(&session)
                    .await
                    .expect("Failed to fetch mailboxes");
                for mb in &mailboxes {
                    if let Some(ref role) = mb.role {
                        session.mailbox_cache.insert(role.clone(), mb.clone());
                    }
                }
                tracing::info!(
                    "[{name}] Connected as {username}, {} mailboxes",
                    mailboxes.len()
                );

                sessions.insert(
                    name.clone(),
                    tokio::sync::RwLock::new(ProviderSession::Fastmail(session)),
                );
            }

            "outlook" => {
                let client_id = account.get("client-id").unwrap_or_else(|| {
                    eprintln!("client-id not set for account [{name}]");
                    std::process::exit(1);
                });

                let token_path = tokens_dir.join(format!("{name}.json"));
                let session = if let Some(s) = outlook::load_tokens(&token_path, &client_id) {
                    tracing::info!("[{name}] Loaded Outlook tokens for {}", s.email);
                    s
                } else {
                    tracing::info!("[{name}] No saved tokens, starting OAuth flow...");
                    outlook::oauth_flow(&client_id, &token_path)
                        .await
                        .expect("Outlook OAuth flow failed")
                };

                sessions.insert(
                    name.clone(),
                    tokio::sync::RwLock::new(ProviderSession::Outlook(session)),
                );
            }

            other => {
                eprintln!("Unknown provider '{other}' for account [{name}]");
                std::process::exit(1);
            }
        }
    }

    // Auto-seed split tabs from the default account's identities
    let splits_config_path = config_dir.join("supervillain/splits.json");
    if let Some(session_lock) = sessions.get(&default_account) {
        let mut session = session_lock.write().await;
        match &mut *session {
            ProviderSession::Fastmail(s) => match jmap::get_identities(s).await {
                Ok(identities) => {
                    if let Some(config) =
                        splits::seed_from_identities(&identities, &splits_config_path)
                    {
                        let names: Vec<_> = config.splits.iter().map(|s| s.name.as_str()).collect();
                        tracing::info!("Auto-created split tabs: {}", names.join(", "));
                    }
                }
                Err(e) => tracing::warn!("Failed to fetch identities for split seeding: {e}"),
            },
            ProviderSession::Outlook(_) => {
                // Outlook doesn't support identity-based split seeding yet
            }
        }
    }

    let state = Arc::new(AppState {
        sessions,
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
        open_browser(&url);
    }

    axum::serve(listener, app).await.unwrap();
}

fn resolve_config_dir() -> PathBuf {
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

fn open_browser(url: &str) {
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

// =============================================================================
// Config parsing
// =============================================================================

/// Parse a simple key = value config file (like ghostty/omarchy).
/// Lines starting with # are comments. Blank lines are ignored.
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
}
