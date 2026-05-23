//! Account configuration: typed config model, INI parse/serialize, atomic
//! write, validators, OAuth single-flight, and HTTP routes for in-app account
//! management.
//!
//! The on-disk format is the existing INI-style config at
//! `~/.config/supervillain/config`. This module is the only writer; it
//! regenerates the file from `ConfigFile` on every save. Hand-edited
//! comments are not preserved.
//!
//! Test discipline: pure helpers tested inline. No HTTP mocking.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

// =============================================================================
// Typed account config
// =============================================================================

/// One account's provider-specific configuration.
///
/// Deserialized from JSON request bodies via serde's tag-based discrimination;
/// converted to/from the on-disk INI format by `account_from_props` /
/// `account_to_ini_lines` below.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "provider", rename_all = "lowercase")]
pub enum AccountConfig {
    Fastmail {
        username: String,
        #[serde(rename = "api-token")]
        api_token: String,
    },
    Outlook {
        #[serde(rename = "client-id")]
        client_id: String,
        #[serde(default)]
        email: Option<String>,
    },
    Gmail {
        #[serde(rename = "client-id")]
        client_id: String,
        #[serde(rename = "client-secret")]
        client_secret: String,
        #[serde(default)]
        email: Option<String>,
    },
}

impl AccountConfig {
    pub fn provider_str(&self) -> &'static str {
        match self {
            Self::Fastmail { .. } => "fastmail",
            Self::Outlook { .. } => "outlook",
            Self::Gmail { .. } => "gmail",
        }
    }
}

/// The complete on-disk configuration: a default account selector plus a map
/// of named accounts. `BTreeMap` keeps section ordering deterministic so
/// successive saves produce diff-stable output.
#[derive(Clone, Debug, Default)]
pub struct ConfigFile {
    pub default_account: Option<String>,
    pub accounts: BTreeMap<String, AccountConfig>,
}

// =============================================================================
// Parse: INI → ConfigFile
// =============================================================================

/// Read a config file from disk. Returns an empty `ConfigFile` if the file
/// is missing or unreadable — startup is non-fatal on first run.
pub fn parse_config(path: &Path) -> ConfigFile {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return ConfigFile::default(),
    };
    parse_config_str(&content)
}

/// Pure parser; tested without filesystem.
pub fn parse_config_str(content: &str) -> ConfigFile {
    let mut default_account: Option<String> = None;
    let mut current_section: Option<String> = None;
    let mut sections: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            let name = line[1..line.len() - 1].trim().to_string();
            sections.entry(name.clone()).or_default();
            current_section = Some(name);
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim().to_string();
        let value = value.trim().to_string();
        match &current_section {
            None => {
                if key == "default-account" {
                    default_account = Some(value);
                }
            }
            Some(section) => {
                sections
                    .entry(section.clone())
                    .or_default()
                    .insert(key, value);
            }
        }
    }

    let mut accounts = BTreeMap::new();
    for (name, props) in sections {
        let provider = props
            .get("provider")
            .cloned()
            .unwrap_or_else(|| "fastmail".to_string());
        if let Some(acct) = account_from_props(&provider, &props) {
            accounts.insert(name, acct);
        } else {
            tracing::warn!(
                "[{name}] skipping account: provider '{provider}' missing required fields"
            );
        }
    }

    ConfigFile {
        default_account,
        accounts,
    }
}

fn account_from_props(provider: &str, props: &BTreeMap<String, String>) -> Option<AccountConfig> {
    match provider {
        "fastmail" => Some(AccountConfig::Fastmail {
            username: props.get("username")?.clone(),
            api_token: props.get("api-token")?.clone(),
        }),
        "outlook" => Some(AccountConfig::Outlook {
            client_id: props.get("client-id")?.clone(),
            // Accept `username` as a synonym for `email` so configs predating
            // the typed enum still parse — Outlook's OAuth populates email
            // from Graph; user-facing label was historically `username`.
            email: props
                .get("email")
                .or_else(|| props.get("username"))
                .cloned(),
        }),
        "gmail" => Some(AccountConfig::Gmail {
            client_id: props.get("client-id")?.clone(),
            client_secret: props.get("client-secret")?.clone(),
            email: props.get("email").cloned(),
        }),
        _ => None,
    }
}

// =============================================================================
// Serialize: ConfigFile → INI
// =============================================================================

/// Pure serializer; deterministic output (sections + keys in BTreeMap order).
pub fn serialize_config(cfg: &ConfigFile) -> String {
    let mut out = String::new();
    if let Some(ref d) = cfg.default_account {
        out.push_str(&format!("default-account = {d}\n\n"));
    }
    let mut first = true;
    for (name, acct) in &cfg.accounts {
        if !first {
            out.push('\n');
        }
        first = false;
        for line in account_to_ini_lines(name, acct) {
            out.push_str(&line);
            out.push('\n');
        }
    }
    out
}

fn account_to_ini_lines(name: &str, acct: &AccountConfig) -> Vec<String> {
    let mut lines = vec![format!("[{name}]")];
    lines.push(format!("provider = {}", acct.provider_str()));
    match acct {
        AccountConfig::Fastmail {
            username,
            api_token,
        } => {
            lines.push(format!("username = {username}"));
            lines.push(format!("api-token = {api_token}"));
        }
        AccountConfig::Outlook { client_id, email } => {
            lines.push(format!("client-id = {client_id}"));
            if let Some(e) = email {
                lines.push(format!("email = {e}"));
            }
        }
        AccountConfig::Gmail {
            client_id,
            client_secret,
            email,
        } => {
            lines.push(format!("client-id = {client_id}"));
            lines.push(format!("client-secret = {client_secret}"));
            if let Some(e) = email {
                lines.push(format!("email = {e}"));
            }
        }
    }
    lines
}

// =============================================================================
// Atomic write
// =============================================================================

/// Write `cfg` to `path` atomically. On POSIX: tmpfile (0600) → fsync file →
/// rename → fsync parent dir. The parent-dir fsync defends against ext4
/// `data=writeback` losing the rename on crash.
pub fn atomic_write_config(path: &Path, cfg: &ConfigFile) -> io::Result<()> {
    use std::io::Write;
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "config path has no parent"))?;
    std::fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(
        ".{}.tmp.{}",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("config"),
        std::process::id()
    ));
    let serialized = serialize_config(cfg);
    {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        f.write_all(serialized.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    // fsync the parent directory so the rename survives a crash.
    if let Ok(dir) = std::fs::File::open(parent) {
        let _ = dir.sync_all();
    }
    Ok(())
}

// =============================================================================
// Validation
// =============================================================================

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum FieldId {
    Name,
    Username,
    ApiToken,
    ClientId,
    ClientSecret,
    Email,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct FieldError {
    pub field: FieldId,
    pub message: String,
}

impl FieldError {
    fn new(field: FieldId, message: impl Into<String>) -> Self {
        Self {
            field,
            message: message.into(),
        }
    }
}

/// Section names appear inside `[brackets]` in the config file. Forbid
/// control chars, brackets, `=`, and `#` so the round-trip is unambiguous.
pub fn validate_section_name(s: &str) -> Result<(), &'static str> {
    if s.is_empty() {
        return Err("name must not be empty");
    }
    if s.len() > 64 {
        return Err("name must be 64 characters or fewer");
    }
    for c in s.chars() {
        if c == '[' || c == ']' || c == '\n' || c == '\r' || c == '=' || c == '#' {
            return Err("name must not contain [, ], =, #, or newlines");
        }
        if c.is_control() {
            return Err("name must not contain control characters");
        }
    }
    Ok(())
}

/// Minimal email validator: a single `@`, non-empty local and domain parts,
/// no whitespace. Strict enough to catch obvious typos at the boundary; not
/// a full RFC validator (the provider does that authoritatively).
pub fn validate_email(s: &str) -> Result<(), &'static str> {
    if s.is_empty() {
        return Err("email must not be empty");
    }
    if s.chars().any(|c| c.is_whitespace()) {
        return Err("email must not contain whitespace");
    }
    let Some((local, domain)) = s.split_once('@') else {
        return Err("email must contain '@'");
    };
    if local.is_empty() || domain.is_empty() {
        return Err("email must have a local part and a domain");
    }
    if !domain.contains('.') {
        return Err("email domain must contain '.'");
    }
    Ok(())
}

/// Aggregate per-field errors so the UI can highlight every offending input
/// in one pass. Returns errors in DOM order (top-down through the form).
pub fn validate_account(cfg: &AccountConfig, name: &str) -> Result<(), Vec<FieldError>> {
    let mut errs = Vec::new();
    if let Err(e) = validate_section_name(name) {
        errs.push(FieldError::new(FieldId::Name, e));
    }
    match cfg {
        AccountConfig::Fastmail {
            username,
            api_token,
        } => {
            if let Err(e) = validate_email(username) {
                errs.push(FieldError::new(FieldId::Username, e));
            }
            if api_token.trim().is_empty() {
                errs.push(FieldError::new(
                    FieldId::ApiToken,
                    "api-token must not be empty",
                ));
            }
        }
        AccountConfig::Outlook { client_id, email } => {
            if client_id.trim().is_empty() {
                errs.push(FieldError::new(
                    FieldId::ClientId,
                    "client-id must not be empty",
                ));
            }
            if let Some(e) = email
                && let Err(err) = validate_email(e)
            {
                errs.push(FieldError::new(FieldId::Email, err));
            }
        }
        AccountConfig::Gmail {
            client_id,
            client_secret,
            email,
        } => {
            if client_id.trim().is_empty() {
                errs.push(FieldError::new(
                    FieldId::ClientId,
                    "client-id must not be empty",
                ));
            }
            if client_secret.trim().is_empty() {
                errs.push(FieldError::new(
                    FieldId::ClientSecret,
                    "client-secret must not be empty",
                ));
            }
            if let Some(e) = email
                && let Err(err) = validate_email(e)
            {
                errs.push(FieldError::new(FieldId::Email, err));
            }
        }
    }
    if errs.is_empty() { Ok(()) } else { Err(errs) }
}

// =============================================================================
// OAuth single-flight
// =============================================================================

/// Single global slot. `Some(account)` means an OAuth flow is in progress
/// for that account; `None` means idle. Held across the long-poll request.
pub type AuthorizingSlot = tokio::sync::Mutex<Option<String>>;

/// Try to claim the slot for `account`. Returns Err on contention; caller
/// must release via `release_authorizing` when the flow finishes.
pub async fn try_claim_authorizing(slot: &AuthorizingSlot, account: &str) -> Result<(), String> {
    let mut guard = slot.lock().await;
    if let Some(ref existing) = *guard {
        return Err(existing.clone());
    }
    *guard = Some(account.to_string());
    Ok(())
}

pub async fn release_authorizing(slot: &AuthorizingSlot) {
    *slot.lock().await = None;
}

// =============================================================================
// Token file path
// =============================================================================

/// Where this account's OAuth tokens live on disk. Matches the layout
/// established by `outlook::load_tokens` / `gmail::load_session`.
pub fn token_file_path(tokens_dir: &Path, account: &str) -> PathBuf {
    tokens_dir.join(format!("{account}.json"))
}

// =============================================================================
// Pure operations on ConfigFile (testable without HTTP / network)
// =============================================================================

/// Reject in-place provider changes. The wire path is `/api/accounts/{id}`;
/// switching provider on the same id would silently change credential
/// semantics and orphan tokens — easier to require delete + re-add.
pub fn check_provider_change(
    existing: &AccountConfig,
    new: &AccountConfig,
) -> Result<(), &'static str> {
    if existing.provider_str() == new.provider_str() {
        Ok(())
    } else {
        Err("provider cannot be changed; delete and re-add instead")
    }
}

/// When updating an existing account, an empty secret field means "keep
/// existing value." UI doesn't echo secrets back; this lets the user save
/// non-secret edits without retyping the api-token / client-secret.
pub fn merge_secrets(existing: &AccountConfig, new: AccountConfig) -> AccountConfig {
    match (existing, new) {
        (
            AccountConfig::Fastmail { api_token: old, .. },
            AccountConfig::Fastmail {
                username,
                api_token: incoming,
            },
        ) => AccountConfig::Fastmail {
            username,
            api_token: if incoming.is_empty() {
                old.clone()
            } else {
                incoming
            },
        },
        (
            AccountConfig::Gmail {
                client_secret: old, ..
            },
            AccountConfig::Gmail {
                client_id,
                client_secret: incoming,
                email,
            },
        ) => AccountConfig::Gmail {
            client_id,
            client_secret: if incoming.is_empty() {
                old.clone()
            } else {
                incoming
            },
            email,
        },
        (_, new) => new,
    }
}

/// Remove `id` from `cfg`. If it was the default, promote the
/// alphabetically-first remaining account (or `None` if registry is empty).
/// Returns true if the account existed.
pub fn delete_and_pick_new_default(cfg: &mut ConfigFile, id: &str) -> bool {
    let removed = cfg.accounts.remove(id).is_some();
    if cfg.default_account.as_deref() == Some(id) {
        cfg.default_account = cfg.accounts.keys().next().cloned();
    }
    removed
}

/// Idempotent set-default. Errors if the account isn't present.
pub fn set_default_in_config(cfg: &mut ConfigFile, id: &str) -> Result<(), &'static str> {
    if !cfg.accounts.contains_key(id) {
        return Err("account not found");
    }
    cfg.default_account = Some(id.to_string());
    Ok(())
}

// =============================================================================
// HTTP route handlers
// =============================================================================

use crate::error::Error;
use crate::provider::ProviderSession;
use crate::types::{AccountError, AccountRegistry, AppState, SessionLock};
use axum::Router;
use axum::extract::{Json, Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::post;
use std::sync::Arc;

/// Sub-router for `/api/accounts/*`. Merged into the main router so the
/// existing `GET /api/accounts` listing stays where it is.
pub fn router() -> Router<Arc<AppState>> {
    use axum::routing::put;
    Router::new()
        .route(
            "/api/accounts/{id}",
            post(upsert_account).delete(delete_account),
        )
        .route("/api/accounts/{id}/default", put(set_default_account))
        .route("/api/accounts/{id}/authorize", post(authorize_account))
}

/// Auth state for an account on the wire. Replaces the overloaded
/// `needs_auth: bool` so the UI can distinguish "fresh / never tried" from
/// "live session present" (failures surface via `state.account_errors`).
#[derive(serde::Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AuthStatus {
    /// Live session exists.
    Ok,
    /// No live session. UI shows the [Authorize] button.
    Pending,
}

#[derive(serde::Serialize)]
struct AccountResponse {
    id: String,
    provider: String,
    email: Option<String>,
    /// Present for OAuth providers (Outlook/Gmail); omitted for Fastmail.
    /// Lets the UI display the existing client-id when editing without the
    /// user having to retype it.
    #[serde(rename = "clientId", skip_serializing_if = "Option::is_none")]
    client_id: Option<String>,
    #[serde(rename = "isDefault")]
    is_default: bool,
    #[serde(rename = "authStatus")]
    auth_status: AuthStatus,
}

fn account_response(
    id: &str,
    acct: &AccountConfig,
    session: Option<&ProviderSession>,
    is_default: bool,
) -> AccountResponse {
    let provider = acct.provider_str().to_string();
    let email = match (acct, session) {
        (_, Some(s)) => Some(s.username().to_string()),
        (AccountConfig::Fastmail { username, .. }, None) => Some(username.clone()),
        (AccountConfig::Outlook { email, .. }, None) => email.clone(),
        (AccountConfig::Gmail { email, .. }, None) => email.clone(),
    };
    let client_id = match acct {
        AccountConfig::Outlook { client_id, .. } | AccountConfig::Gmail { client_id, .. } => {
            Some(client_id.clone())
        }
        AccountConfig::Fastmail { .. } => None,
    };
    AccountResponse {
        id: id.to_string(),
        provider,
        email,
        client_id,
        is_default,
        auth_status: if session.is_some() {
            AuthStatus::Ok
        } else {
            AuthStatus::Pending
        },
    }
}

/// Single point of mutation for `state.account_errors` — keeps the three
/// previous push/retain sites converged so future refactors can't diverge.
pub async fn clear_errors_for(state: &AppState, account_id: &str) {
    state
        .account_errors
        .write()
        .await
        .retain(|e| e.account != account_id);
}

pub async fn clear_setup_sentinel(state: &AppState) {
    state
        .account_errors
        .write()
        .await
        .retain(|e| e.provider != "setup");
}

pub async fn push_error(state: &AppState, err: AccountError) {
    state.account_errors.write().await.push(err);
}

/// Apply a default-when-empty rule to the registry. Pure helper so the
/// "empty default → promote new account" branch is testable.
fn promote_default_if_empty(reg: &mut AccountRegistry, id: &str) {
    if reg.default_account.is_empty() {
        reg.default_account = id.to_string();
    }
}

/// `POST /api/accounts/{id}` — upsert.
///
/// Body is an `AccountConfig` payload (serde discriminates on `provider`).
/// Create path: session is built outside the registry write lock so other
/// reads don't stall during a 500ms-2s Fastmail connect.
async fn upsert_account(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
    Json(incoming): Json<AccountConfig>,
) -> Result<impl IntoResponse, Error> {
    // Look up the existing account (if any) in the in-memory registry — no
    // disk read needed; the registry is the canonical post-startup mirror.
    let (is_new, cfg) = {
        let reg = state.accounts.read().await;
        match reg.account_configs.get(&id) {
            Some(existing) => {
                check_provider_change(existing, &incoming)
                    .map_err(|e| Error::BadRequest(e.into()))?;
                (false, merge_secrets(existing, incoming))
            }
            None => (true, incoming),
        }
    };

    if let Err(errs) = validate_account(&cfg, &id) {
        return Err(Error::BadRequest(
            serde_json::to_string(&errs).unwrap_or_else(|_| "validation failed".into()),
        ));
    }

    // For a new Fastmail account, build the session OUTSIDE any lock —
    // the 500ms-2s network call must not block other readers.
    let mut new_session: Option<ProviderSession> = None;
    let mut needs_auth = false;
    if is_new {
        match &cfg {
            AccountConfig::Fastmail {
                username,
                api_token,
            } => {
                let mut sess =
                    crate::jmap::JmapSession::new(username, &format!("Bearer {api_token}"));
                crate::jmap::connect(&mut sess)
                    .await
                    .map_err(|e| Error::BadRequest(format!("connection failed: {e}")))?;
                if let Ok(mailboxes) = crate::jmap::get_mailboxes(&sess).await {
                    for mb in &mailboxes {
                        if let Some(ref role) = mb.role {
                            sess.mailbox_cache.insert(role.clone(), mb.clone());
                        }
                    }
                }
                new_session = Some(ProviderSession::Fastmail(Box::new(sess)));
            }
            AccountConfig::Outlook { .. } | AccountConfig::Gmail { .. } => {
                needs_auth = true;
            }
        }
    }

    // Take the lock, mutate in-memory state, snapshot for disk — all
    // microseconds. Disk write happens AFTER the lock is released.
    let snapshot = {
        let mut reg = state.accounts.write().await;
        if is_new && reg.account_configs.contains_key(&id) {
            return Err(Error::Conflict(format!("account '{id}' already exists")));
        }
        if let Some(session) = new_session {
            reg.sessions.insert(
                id.clone(),
                SessionLock::new(tokio::sync::RwLock::new(session)),
            );
        }
        reg.account_configs.insert(id.clone(), cfg.clone());
        promote_default_if_empty(&mut reg, &id);
        reg.snapshot()
    };

    atomic_write_config(&state.config_path, &snapshot)
        .map_err(|e| Error::Internal(format!("failed to write config: {e}")))?;

    clear_setup_sentinel(&state).await;
    if needs_auth {
        push_error(
            &state,
            AccountError {
                account: id.clone(),
                provider: cfg.provider_str().into(),
                error: "Not authorized — click Authorize to complete setup".into(),
            },
        )
        .await;
    }

    let reg = state.accounts.read().await;
    let is_default = reg.default_account == id;
    let session_guard;
    let session_ref = match reg.sessions.get(&id) {
        Some(lock) => {
            session_guard = lock.read().await;
            Some(&*session_guard)
        }
        None => None,
    };
    let resp = account_response(&id, &cfg, session_ref, is_default);
    let status = if is_new {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((status, Json(resp)))
}

/// `DELETE /api/accounts/{id}` — remove from registry, delete token file,
/// rewrite config.
///
/// No provider-side revocation: Outlook has no per-token revoke endpoint;
/// Fastmail api-tokens are managed via the user's Fastmail UI; Gmail revoke
/// would require pulling a live access token out of GmailSession which we
/// don't expose. To fully revoke OAuth grants, users visit the provider's
/// account-management UI (documented in README). This handler only drops
/// local credentials.
async fn delete_account(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<impl IntoResponse, Error> {
    let snapshot = {
        let mut reg = state.accounts.write().await;
        if !reg.account_configs.contains_key(&id) {
            return Err(Error::NotFound(format!("account '{id}' not found")));
        }
        reg.sessions.remove(&id);
        reg.account_configs.remove(&id);
        if reg.default_account == id {
            reg.default_account = reg
                .account_configs
                .keys()
                .next()
                .cloned()
                .unwrap_or_default();
        }
        reg.snapshot()
    };

    atomic_write_config(&state.config_path, &snapshot)
        .map_err(|e| Error::Internal(format!("failed to write config: {e}")))?;

    if let Err(e) = std::fs::remove_file(token_file_path(&state.tokens_dir, &id))
        && e.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!("[{id}] failed to remove token file: {e}");
    }
    clear_errors_for(&state, &id).await;

    Ok(StatusCode::NO_CONTENT)
}

/// `PUT /api/accounts/{id}/default` — set the default account. Idempotent.
async fn set_default_account(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<impl IntoResponse, Error> {
    let snapshot = {
        let mut reg = state.accounts.write().await;
        if !reg.account_configs.contains_key(&id) {
            return Err(Error::NotFound(format!("account '{id}' not found")));
        }
        reg.default_account = id.clone();
        reg.snapshot()
    };
    atomic_write_config(&state.config_path, &snapshot)
        .map_err(|e| Error::Internal(format!("failed to write config: {e}")))?;
    Ok(StatusCode::OK)
}

/// `POST /api/accounts/{id}/authorize` — long-poll OAuth.
///
/// Single-flight via `state.authorizing`. The slot is held through the
/// entire flow — OAuth flow + session install + config write — so a second
/// authorize cannot race finalize. The existing `acquire_oauth_callback`
/// 5-minute timeout caps the wait.
async fn authorize_account(
    State(state): State<Arc<AppState>>,
    AxumPath(id): AxumPath<String>,
) -> Result<impl IntoResponse, Error> {
    let account = {
        let reg = state.accounts.read().await;
        reg.account_configs
            .get(&id)
            .cloned()
            .ok_or_else(|| Error::NotFound(format!("account '{id}' not found")))?
    };

    // Fast-fail Fastmail BEFORE claiming the global slot — better error
    // surface than wrapping it in "authorization failed: ...".
    if matches!(account, AccountConfig::Fastmail { .. }) {
        return Err(Error::BadRequest(
            "Fastmail does not use OAuth — update credentials via POST /api/accounts/{id}".into(),
        ));
    }

    try_claim_authorizing(&state.authorizing, &id)
        .await
        .map_err(|other| Error::Conflict(format!("another authorization in progress: {other}")))?;

    // Defer-release: slot stays held through finalize regardless of outcome.
    let outcome = run_and_install_authorize(&id, &account, &state).await;
    release_authorizing(&state.authorizing).await;

    let (updated_account, is_default) =
        outcome.map_err(|e| Error::BadRequest(format!("authorization failed: {e}")))?;

    clear_errors_for(&state, &id).await;

    let reg = state.accounts.read().await;
    let session_guard = reg.sessions.get(&id).unwrap().read().await;
    let resp = account_response(&id, &updated_account, Some(&*session_guard), is_default);
    Ok((StatusCode::OK, Json(resp)))
}

/// Run the OAuth flow, install the resulting session, write config. Returns
/// the updated account config + is_default flag for the response. All work
/// happens with the single-flight slot held (caller manages it).
async fn run_and_install_authorize(
    id: &str,
    account: &AccountConfig,
    state: &AppState,
) -> Result<(AccountConfig, bool), String> {
    let session = run_authorize(id, account, state).await?;

    let email_from_session = match &session {
        ProviderSession::Fastmail(s) => Some(s.username.clone()),
        ProviderSession::Outlook(s) => Some(s.email.clone()),
        ProviderSession::Gmail(s) => Some(s.email.clone()),
    };
    let updated_account = update_email_from_session(account.clone(), email_from_session);

    let (snapshot, is_default) = {
        let mut reg = state.accounts.write().await;
        reg.sessions.insert(
            id.to_string(),
            SessionLock::new(tokio::sync::RwLock::new(session)),
        );
        reg.account_configs
            .insert(id.to_string(), updated_account.clone());
        promote_default_if_empty(&mut reg, id);
        let is_default = reg.default_account == id;
        (reg.snapshot(), is_default)
    };

    atomic_write_config(&state.config_path, &snapshot)
        .map_err(|e| format!("failed to write config: {e}"))?;

    Ok((updated_account, is_default))
}

/// Pure helper: copy the session's email into the AccountConfig (OAuth
/// providers only; Fastmail's username is canonical from config).
pub fn update_email_from_session(
    account: AccountConfig,
    email_from_session: Option<String>,
) -> AccountConfig {
    match (account, email_from_session) {
        (AccountConfig::Outlook { client_id, .. }, Some(email)) => AccountConfig::Outlook {
            client_id,
            email: Some(email),
        },
        (
            AccountConfig::Gmail {
                client_id,
                client_secret,
                ..
            },
            Some(email),
        ) => AccountConfig::Gmail {
            client_id,
            client_secret,
            email: Some(email),
        },
        (other, _) => other,
    }
}

async fn run_authorize(
    id: &str,
    account: &AccountConfig,
    state: &AppState,
) -> Result<ProviderSession, String> {
    let tokens_dir = &state.tokens_dir;
    match account {
        AccountConfig::Fastmail { .. } => {
            // Unreachable: `authorize_account` fast-fails Fastmail before
            // reaching here. Keep the arm exhaustive for the compiler.
            Err("Fastmail does not use OAuth".into())
        }
        AccountConfig::Outlook { client_id, .. } => {
            let token_path = token_file_path(tokens_dir, id);
            let session = crate::outlook::oauth_flow(client_id, &token_path)
                .await
                .map_err(|e| e.to_string())?;
            Ok(ProviderSession::Outlook(Box::new(session)))
        }
        AccountConfig::Gmail {
            client_id,
            client_secret,
            ..
        } => {
            let session =
                crate::gmail::oauth_flow(state.token_store.clone(), id, client_id, client_secret)
                    .await
                    .map_err(|e| e.to_string())?;
            Ok(ProviderSession::Gmail(Box::new(session)))
        }
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn fastmail(username: &str, token: &str) -> AccountConfig {
        AccountConfig::Fastmail {
            username: username.into(),
            api_token: token.into(),
        }
    }
    fn outlook(client_id: &str, email: Option<&str>) -> AccountConfig {
        AccountConfig::Outlook {
            client_id: client_id.into(),
            email: email.map(String::from),
        }
    }
    fn gmail(client_id: &str, secret: &str, email: Option<&str>) -> AccountConfig {
        AccountConfig::Gmail {
            client_id: client_id.into(),
            client_secret: secret.into(),
            email: email.map(String::from),
        }
    }

    // ---- Config serialize / parse / round-trip ----

    #[test]
    fn serialize_round_trips_all_three_providers() {
        let mut accounts = BTreeMap::new();
        accounts.insert("fm".to_string(), fastmail("alice@fm.com", "fmu1-tok"));
        accounts.insert(
            "ms".to_string(),
            outlook("client-abc", Some("alice@outlook.com")),
        );
        accounts.insert(
            "gm".to_string(),
            gmail("cid", "cs", Some("alice@gmail.com")),
        );
        let cfg = ConfigFile {
            default_account: Some("fm".into()),
            accounts,
        };
        let s = serialize_config(&cfg);
        let parsed = parse_config_str(&s);
        assert_eq!(parsed.default_account.as_deref(), Some("fm"));
        assert_eq!(parsed.accounts.len(), 3);
        match parsed.accounts.get("fm").unwrap() {
            AccountConfig::Fastmail {
                username,
                api_token,
            } => {
                assert_eq!(username, "alice@fm.com");
                assert_eq!(api_token, "fmu1-tok");
            }
            _ => panic!("expected fastmail"),
        }
        match parsed.accounts.get("ms").unwrap() {
            AccountConfig::Outlook { client_id, email } => {
                assert_eq!(client_id, "client-abc");
                assert_eq!(email.as_deref(), Some("alice@outlook.com"));
            }
            _ => panic!("expected outlook"),
        }
        match parsed.accounts.get("gm").unwrap() {
            AccountConfig::Gmail {
                client_id,
                client_secret,
                email,
            } => {
                assert_eq!(client_id, "cid");
                assert_eq!(client_secret, "cs");
                assert_eq!(email.as_deref(), Some("alice@gmail.com"));
            }
            _ => panic!("expected gmail"),
        }
    }

    #[test]
    fn serialize_emits_default_account_before_sections() {
        let mut accounts = BTreeMap::new();
        accounts.insert("fm".to_string(), fastmail("u", "t"));
        let cfg = ConfigFile {
            default_account: Some("fm".into()),
            accounts,
        };
        let s = serialize_config(&cfg);
        let default_pos = s.find("default-account").unwrap();
        let section_pos = s.find("[fm]").unwrap();
        assert!(default_pos < section_pos);
    }

    #[test]
    fn serialize_sorts_sections_and_keys_for_diff_stability() {
        let mut accounts = BTreeMap::new();
        // Insert in reverse alpha order to confirm BTreeMap sorts them.
        accounts.insert("zeta".to_string(), fastmail("z@z.com", "ztok"));
        accounts.insert("alpha".to_string(), fastmail("a@a.com", "atok"));
        let cfg = ConfigFile {
            default_account: None,
            accounts,
        };
        let s = serialize_config(&cfg);
        let alpha_pos = s.find("[alpha]").unwrap();
        let zeta_pos = s.find("[zeta]").unwrap();
        assert!(alpha_pos < zeta_pos, "sections should be alpha-sorted");
        // Keys within an alpha section should appear in canonical order:
        // provider, username, api-token.
        let alpha_block = &s[alpha_pos..zeta_pos];
        let p = alpha_block.find("provider").unwrap();
        let u = alpha_block.find("username").unwrap();
        let t = alpha_block.find("api-token").unwrap();
        assert!(p < u && u < t, "keys should be in canonical order");
    }

    #[test]
    fn parse_then_serialize_idempotent() {
        let original = "\
default-account = personal

[personal]
provider = fastmail
username = u@fm.com
api-token = tok
";
        let parsed = parse_config_str(original);
        let reserialized = serialize_config(&parsed);
        let reparsed = parse_config_str(&reserialized);
        assert_eq!(reparsed.default_account, parsed.default_account);
        assert_eq!(reparsed.accounts.len(), parsed.accounts.len());
        assert_eq!(reserialized, serialize_config(&reparsed));
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_creates_file_with_mode_600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        let mut accounts = BTreeMap::new();
        accounts.insert("fm".to_string(), fastmail("u@fm.com", "tok"));
        let cfg = ConfigFile {
            default_account: Some("fm".into()),
            accounts,
        };
        atomic_write_config(&path, &cfg).unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "config file must be mode 0600, got {mode:o}");
    }

    #[test]
    fn atomic_write_replaces_existing_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        let mut accounts = BTreeMap::new();
        accounts.insert("fm".to_string(), fastmail("first@fm.com", "tok1"));
        let cfg1 = ConfigFile {
            default_account: Some("fm".into()),
            accounts: accounts.clone(),
        };
        atomic_write_config(&path, &cfg1).unwrap();
        let first = std::fs::read_to_string(&path).unwrap();
        assert!(first.contains("first@fm.com"));

        let mut accounts2 = BTreeMap::new();
        accounts2.insert("fm".to_string(), fastmail("second@fm.com", "tok2"));
        let cfg2 = ConfigFile {
            default_account: Some("fm".into()),
            accounts: accounts2,
        };
        atomic_write_config(&path, &cfg2).unwrap();
        let second = std::fs::read_to_string(&path).unwrap();
        assert!(second.contains("second@fm.com"));
        assert!(!second.contains("first@fm.com"));
    }

    #[test]
    fn atomic_write_does_not_leave_tmp_on_success() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        let mut accounts = BTreeMap::new();
        accounts.insert("fm".to_string(), fastmail("u@fm.com", "tok"));
        let cfg = ConfigFile {
            default_account: None,
            accounts,
        };
        atomic_write_config(&path, &cfg).unwrap();
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().to_string()))
            .collect();
        assert!(
            entries.iter().all(|n| !n.contains(".tmp.")),
            "no .tmp. file should remain after a successful write; got {entries:?}"
        );
        assert!(entries.iter().any(|n| n == "config"));
    }

    #[test]
    fn parse_skips_account_with_missing_required_fields() {
        // Outlook needs client-id; without it the section is skipped.
        let s = "[broken]\nprovider = outlook\n\n[ok]\nprovider = fastmail\nusername = u@fm.com\napi-token = t\n";
        let cfg = parse_config_str(s);
        assert!(cfg.accounts.contains_key("ok"));
        assert!(!cfg.accounts.contains_key("broken"));
    }

    #[test]
    fn parse_unknown_provider_is_skipped() {
        let s = "[bad]\nprovider = yahoo\nusername = u@y.com\n";
        let cfg = parse_config_str(s);
        assert!(cfg.accounts.is_empty());
    }

    // ---- Validators ----

    #[test]
    fn validate_section_name_rejects_brackets_newlines_and_empty() {
        assert!(validate_section_name("").is_err());
        assert!(validate_section_name("ok").is_ok());
        assert!(validate_section_name("with[bracket").is_err());
        assert!(validate_section_name("with]bracket").is_err());
        assert!(validate_section_name("with\nnewline").is_err());
        assert!(validate_section_name("with=eq").is_err());
        assert!(validate_section_name("with#hash").is_err());
        assert!(validate_section_name(&"x".repeat(65)).is_err());
        assert!(validate_section_name(&"x".repeat(64)).is_ok());
    }

    #[test]
    fn validate_email_accepts_simple_rejects_missing_at_and_whitespace() {
        assert!(validate_email("a@b.com").is_ok());
        assert!(validate_email("").is_err());
        assert!(validate_email("a@b").is_err()); // no dot in domain
        assert!(validate_email("nofatall.com").is_err());
        assert!(validate_email("has space@b.com").is_err());
        assert!(validate_email("@b.com").is_err());
        assert!(validate_email("a@").is_err());
    }

    #[test]
    fn validate_account_aggregates_field_errors_in_dom_order() {
        // Fastmail with bad name, bad username, empty token: all three reported.
        let bad = fastmail("not-an-email", "");
        let errs = validate_account(&bad, "bad name with spaces[").unwrap_err();
        assert_eq!(errs.len(), 3);
        assert_eq!(errs[0].field, FieldId::Name);
        assert_eq!(errs[1].field, FieldId::Username);
        assert_eq!(errs[2].field, FieldId::ApiToken);
    }

    #[test]
    fn serde_rejects_unknown_provider_and_missing_required_fields() {
        // Unknown provider tag → deserialize fails.
        let bad = serde_json::json!({"provider": "yahoo", "username": "u@y.com"});
        assert!(serde_json::from_value::<AccountConfig>(bad).is_err());

        // Missing required Fastmail fields → fails.
        let missing = serde_json::json!({"provider": "fastmail", "username": "u@fm.com"});
        assert!(serde_json::from_value::<AccountConfig>(missing).is_err());

        // Good Fastmail payload → parses.
        let good = serde_json::json!({
            "provider": "fastmail",
            "username": "u@fm.com",
            "api-token": "tok"
        });
        let parsed: AccountConfig = serde_json::from_value(good).unwrap();
        assert_eq!(parsed.provider_str(), "fastmail");
    }

    // ---- Authorize single-flight ----

    #[tokio::test]
    async fn authorize_acquires_global_lock_then_releases() {
        let slot = AuthorizingSlot::default();
        assert!(try_claim_authorizing(&slot, "fm").await.is_ok());
        // While claimed, the slot reflects the holder.
        assert_eq!(slot.lock().await.as_deref(), Some("fm"));
        release_authorizing(&slot).await;
        assert!(slot.lock().await.is_none());
    }

    #[tokio::test]
    async fn authorize_returns_409_when_already_in_progress() {
        let slot = AuthorizingSlot::default();
        try_claim_authorizing(&slot, "fm").await.unwrap();
        let err = try_claim_authorizing(&slot, "outlook").await.unwrap_err();
        assert_eq!(err, "fm", "should report the existing holder");
    }

    // ---- Pure route helpers ----

    #[test]
    fn upsert_update_path_rejects_provider_change_via_id() {
        let existing = fastmail("u@fm.com", "tok");
        let new = outlook("client-id", None);
        assert_eq!(
            check_provider_change(&existing, &new),
            Err("provider cannot be changed; delete and re-add instead")
        );
        // Same-provider update passes.
        let updated = fastmail("u2@fm.com", "tok2");
        assert!(check_provider_change(&existing, &updated).is_ok());
    }

    #[test]
    fn upsert_update_preserves_secrets_when_field_absent() {
        // Fastmail: empty api-token preserves existing.
        let existing = fastmail("u@fm.com", "secret-tok");
        let patch = fastmail("u@fm.com", "");
        let merged = merge_secrets(&existing, patch);
        match merged {
            AccountConfig::Fastmail { api_token, .. } => assert_eq!(api_token, "secret-tok"),
            _ => panic!("wrong variant"),
        }
        // Fastmail: non-empty api-token overrides.
        let existing = fastmail("u@fm.com", "old");
        let patch = fastmail("u@fm.com", "new");
        match merge_secrets(&existing, patch) {
            AccountConfig::Fastmail { api_token, .. } => assert_eq!(api_token, "new"),
            _ => panic!("wrong variant"),
        }
        // Gmail: empty client-secret preserves existing.
        let existing = gmail("cid", "secret", Some("u@g.com"));
        let patch = gmail("cid", "", Some("u@g.com"));
        match merge_secrets(&existing, patch) {
            AccountConfig::Gmail { client_secret, .. } => assert_eq!(client_secret, "secret"),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn delete_promotes_first_remaining_account_to_default() {
        let mut cfg = ConfigFile {
            default_account: Some("alpha".into()),
            accounts: BTreeMap::new(),
        };
        cfg.accounts
            .insert("alpha".into(), fastmail("a@a.com", "t"));
        cfg.accounts.insert("beta".into(), fastmail("b@b.com", "t"));
        cfg.accounts
            .insert("gamma".into(), fastmail("g@g.com", "t"));
        assert!(delete_and_pick_new_default(&mut cfg, "alpha"));
        // BTreeMap iteration order is alpha → beta → gamma; after delete, beta is first.
        assert_eq!(cfg.default_account.as_deref(), Some("beta"));
    }

    #[test]
    fn delete_last_account_clears_default() {
        let mut cfg = ConfigFile {
            default_account: Some("only".into()),
            accounts: BTreeMap::new(),
        };
        cfg.accounts.insert("only".into(), fastmail("u@u.com", "t"));
        assert!(delete_and_pick_new_default(&mut cfg, "only"));
        assert!(cfg.default_account.is_none());
    }

    #[test]
    fn delete_non_default_account_leaves_default_alone() {
        let mut cfg = ConfigFile {
            default_account: Some("alpha".into()),
            accounts: BTreeMap::new(),
        };
        cfg.accounts
            .insert("alpha".into(), fastmail("a@a.com", "t"));
        cfg.accounts.insert("beta".into(), fastmail("b@b.com", "t"));
        assert!(delete_and_pick_new_default(&mut cfg, "beta"));
        assert_eq!(cfg.default_account.as_deref(), Some("alpha"));
    }

    #[test]
    fn set_default_idempotent_returns_ok() {
        let mut cfg = ConfigFile {
            default_account: None,
            accounts: BTreeMap::new(),
        };
        cfg.accounts.insert("fm".into(), fastmail("u@fm.com", "t"));
        assert!(set_default_in_config(&mut cfg, "fm").is_ok());
        assert_eq!(cfg.default_account.as_deref(), Some("fm"));
        // Second call: still Ok, no change.
        assert!(set_default_in_config(&mut cfg, "fm").is_ok());
        assert_eq!(cfg.default_account.as_deref(), Some("fm"));
    }

    #[test]
    fn set_default_rejects_unknown_account() {
        let mut cfg = ConfigFile::default();
        assert!(set_default_in_config(&mut cfg, "nope").is_err());
    }

    #[tokio::test]
    async fn authorize_releases_lock_on_oauth_error() {
        // Simulate the route-handler shape: claim, work returns Err, release.
        async fn run(slot: &AuthorizingSlot) -> Result<(), &'static str> {
            try_claim_authorizing(slot, "fm")
                .await
                .map_err(|_| "busy")?;
            let result: Result<(), &'static str> = Err("oauth failed");
            release_authorizing(slot).await;
            result
        }
        let slot = AuthorizingSlot::default();
        assert!(run(&slot).await.is_err());
        // After the simulated error, the slot must be free for retry.
        assert!(slot.lock().await.is_none());
    }
}
