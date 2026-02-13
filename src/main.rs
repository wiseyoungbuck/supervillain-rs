use std::path::PathBuf;
use std::sync::Arc;

use vimmail::{jmap, routes, types::AppState};

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt::init();

    let username = std::env::var("FASTMAIL_USERNAME").expect("FASTMAIL_USERNAME not set");
    let token = std::env::var("FASTMAIL_API_TOKEN").expect("FASTMAIL_API_TOKEN not set");

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

    let config_dir = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .ok()
        .or_else(|| {
            std::env::var("HOME")
                .map(|h| PathBuf::from(h).join(".config"))
                .ok()
        })
        .unwrap_or_else(|| PathBuf::from("."));

    let state = Arc::new(AppState {
        session: tokio::sync::RwLock::new(session),
        splits_config_path: config_dir.join("vimmail/splits.json"),
    });

    let app = routes::router(state).nest_service(
        "/",
        tower_http::services::ServeDir::new("static").append_index_html_on_directories(true),
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:8000")
        .await
        .unwrap();
    tracing::info!("Listening on http://127.0.0.1:8000");
    axum::serve(listener, app).await.unwrap();
}
