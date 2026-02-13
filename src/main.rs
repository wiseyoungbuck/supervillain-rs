use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use vimmail::{jmap, routes, types::AppState};

#[tokio::main]
async fn main() {
    let config_dir = resolve_config_dir();
    let config_path = config_dir.join("supervillain/config");

    // Load config file, then fall back to env vars
    let config = load_config(&config_path);
    tracing_subscriber::fmt::init();

    let username = config
        .get("username")
        .cloned()
        .or_else(|| std::env::var("FASTMAIL_USERNAME").ok())
        .unwrap_or_else(|| {
            eprintln!(
                "username not set.\n\nCreate {config_path} with:\n\n  \
                 username = you@fastmail.com\n  \
                 api-token = your-token\n",
                config_path = config_path.display()
            );
            std::process::exit(1);
        });

    let token = config
        .get("api-token")
        .cloned()
        .or_else(|| std::env::var("FASTMAIL_API_TOKEN").ok())
        .unwrap_or_else(|| {
            eprintln!(
                "api-token not set.\n\nCreate {config_path} with:\n\n  \
                 username = you@fastmail.com\n  \
                 api-token = your-token\n",
                config_path = config_path.display()
            );
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
    tracing::info!("Connected as {}, {} mailboxes", username, mailboxes.len());

    let state = Arc::new(AppState {
        session: tokio::sync::RwLock::new(session),
        splits_config_path: config_dir.join("vimmail/splits.json"),
    });

    let app = routes::router(state).fallback_service(
        tower_http::services::ServeDir::new("static").append_index_html_on_directories(true),
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:8000")
        .await
        .unwrap();
    tracing::info!("Listening on http://127.0.0.1:8000");
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
