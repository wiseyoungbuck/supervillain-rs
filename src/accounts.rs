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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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

    /// The email address the config alone can vouch for, before any session
    /// exists. Fastmail's username is canonical; OAuth providers may not know
    /// it until the first authorize populates `email`.
    pub fn configured_email(&self) -> Option<&str> {
        match self {
            Self::Fastmail { username, .. } => Some(username),
            Self::Outlook { email, .. } | Self::Gmail { email, .. } => email.as_deref(),
        }
    }

    /// OAuth client id, for providers that have one.
    pub fn oauth_client_id(&self) -> Option<&str> {
        match self {
            Self::Outlook { client_id, .. } | Self::Gmail { client_id, .. } => Some(client_id),
            Self::Fastmail { .. } => None,
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
pub fn parse_config(path: &Path) -> (ConfigFile, Vec<ConfigParseError>) {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return (ConfigFile::default(), Vec::new()),
    };
    parse_config_str(&content)
}

/// Surfaced to the UI so a hand-edited config that fails to parse doesn't
/// vanish into a log line. One per malformed section.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigParseError {
    /// Section name as it appeared in the config, sanitized when the original
    /// header was rejected by `validate_section_name` (path traversal,
    /// embedded `[`/`]`/`=`/newlines, etc.) so it's safe to drop into a UI
    /// string without re-introducing the validation-bypass payload.
    pub section: String,
    /// Provider key from the section, or empty when none was supplied or the
    /// section header itself was invalid. UI code must treat empty as "no
    /// provider claimed" and not as a real provider value.
    pub provider: String,
    pub reason: String,
}

/// Substituted for a hostile section name in `ConfigParseError.section` so
/// nothing the validator already rejected (path separators, embedded
/// brackets, newlines, quotes) reaches the UI banner.
const MALFORMED_SECTION_PLACEHOLDER: &str = "<malformed section>";

/// Substituted into `ConfigParseError.provider` when the provider string is
/// not one of the recognized providers (`fastmail`, `outlook`, `gmail`).
/// Unknown providers — including hostile strings from a tampered config —
/// would otherwise echo into the UI banner where `escapeHtml` doesn't
/// encode `"` / `'` (attribute-context render). Known provider names are
/// short safe-by-construction tokens.
const MALFORMED_PROVIDER_PLACEHOLDER: &str = "<unknown provider>";

const KNOWN_PROVIDERS: &[&str] = &["fastmail", "outlook", "gmail"];

// =============================================================================
// Startup config validation → UI-visible errors
// =============================================================================

/// Build the list of startup-time config errors that the UI banner displays.
///
/// Three sources, each routed to the same `AccountError` shape so the
/// existing red-banner code in `static/app.js` renders them without changes:
///
///   * Account INI (`config_path`) — one `AccountError` per malformed section.
///   * `splits.json` — one entry on parse/IO failure (missing file is fine).
///   * `timezone.json` — same pattern.
///
/// The helper itself emits no `tracing` side effects. (Upstream callers —
/// `parse_config_str`, `splits::load`, `timezone::load` — may emit `warn!`
/// when they produce the inputs to this function; that logging is their
/// concern, not the helper's.) Tests can call this directly without
/// spinning up a tokio runtime.
pub fn startup_config_errors(
    config_path: &Path,
    parse_errors: Vec<ConfigParseError>,
    splits_path: &Path,
    splits_result: Result<Option<crate::types::SplitsConfig>, String>,
    timezone_path: &Path,
    timezone_result: Result<Option<crate::timezone::TimezoneConfig>, String>,
) -> Vec<crate::types::AccountError> {
    let mut errors: Vec<crate::types::AccountError> = parse_errors
        .into_iter()
        .map(|e| crate::types::AccountError {
            account: e.section,
            provider: e.provider,
            error: format!(
                "Config error: {} — edit {}",
                e.reason,
                config_path.display()
            ),
        })
        .collect();

    if let Err(reason) = splits_result {
        errors.push(crate::types::AccountError {
            account: splits_path.display().to_string(),
            // Empty provider matches the convention used by section-level
            // parse errors that have no provider claim; UI must not treat
            // it as a real provider key.
            provider: String::new(),
            error: format!("Config error: {reason} — using defaults until fixed"),
        });
    }

    if let Err(reason) = timezone_result {
        errors.push(crate::types::AccountError {
            account: timezone_path.display().to_string(),
            provider: String::new(),
            error: format!("Config error: {reason} — using defaults until fixed"),
        });
    }

    errors
}

/// Pure parser; tested without filesystem.
pub fn parse_config_str(content: &str) -> (ConfigFile, Vec<ConfigParseError>) {
    let mut default_account: Option<String> = None;
    let mut current_section: Option<String> = None;
    let mut sections: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let mut errors: Vec<ConfigParseError> = Vec::new();

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            let name = line[1..line.len() - 1].trim().to_string();
            // Defense-in-depth: validate the section name at parse time so a
            // hand-edited config can't smuggle a path-traversal name through
            // startup (the in-app UI's `validate_section_name` enforces the
            // same rules for newly-added accounts).
            if let Err(e) = validate_section_name(&name) {
                tracing::warn!("[{name}] skipping malformed section header: {e}");
                // `name` was rejected because it contains hostile characters;
                // don't echo it back into the UI banner. The log line above
                // still preserves the original for operator debugging.
                errors.push(ConfigParseError {
                    section: MALFORMED_SECTION_PLACEHOLDER.into(),
                    provider: String::new(),
                    reason: format!("Invalid section name: {e}"),
                });
                current_section = None;
                continue;
            }
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
        let Some(provider) = props.get("provider").cloned() else {
            // Previously defaulted to "fastmail", which produced a misleading
            // "missing username" error for sections that omitted `provider`
            // but were obviously a different provider (e.g. a Gmail section
            // with client-id / client-secret but no provider line).
            tracing::warn!("[{name}] skipping account: missing required field `provider`");
            errors.push(ConfigParseError {
                section: name,
                provider: String::new(),
                reason: "missing required field `provider`".into(),
            });
            continue;
        };
        match account_from_props(&provider, &props) {
            Ok(acct) => {
                accounts.insert(name, acct);
            }
            Err(reason) => {
                tracing::warn!("[{name}] skipping account ({provider}): {reason}");
                // Only echo the provider into the UI banner if it's one of
                // the known providers (safe-by-construction tokens). Unknown
                // / tampered values get a placeholder so a quote or `>` in
                // the bad string can't escape the UI's attribute-context
                // render.
                let safe_provider = if KNOWN_PROVIDERS.contains(&provider.as_str()) {
                    provider
                } else {
                    MALFORMED_PROVIDER_PLACEHOLDER.into()
                };
                errors.push(ConfigParseError {
                    section: name,
                    provider: safe_provider,
                    reason,
                });
            }
        }
    }

    (
        ConfigFile {
            default_account,
            accounts,
        },
        errors,
    )
}

fn account_from_props(
    provider: &str,
    props: &BTreeMap<String, String>,
) -> Result<AccountConfig, String> {
    let require = |key: &str| -> Result<String, String> {
        props
            .get(key)
            .cloned()
            .ok_or_else(|| format!("missing required field `{key}`"))
    };
    match provider {
        "fastmail" => Ok(AccountConfig::Fastmail {
            username: require("username")?,
            api_token: require("api-token")?,
        }),
        "outlook" => Ok(AccountConfig::Outlook {
            client_id: require("client-id")?,
            // Accept `username` as a synonym for `email` so configs predating
            // the typed enum still parse — Outlook's OAuth populates email
            // from Graph; user-facing label was historically `username`.
            email: props
                .get("email")
                .or_else(|| props.get("username"))
                .cloned(),
        }),
        "gmail" => Ok(AccountConfig::Gmail {
            client_id: require("client-id")?,
            client_secret: require("client-secret")?,
            email: props.get("email").cloned(),
        }),
        other => Err(format!("unknown provider `{other}`")),
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
    let serialized = serialize_config(cfg);
    atomic_write_bytes(path, serialized.as_bytes(), /* secret */ true)
}

/// Atomic file write with the same crash-safety guarantees as
/// `atomic_write_config`, but for arbitrary bytes. Used by
/// `timezone::save_config` (and any other typed-config module that wants
/// the same durability story).
///
/// `secret = true` sets mode 0o600 on POSIX (credentials); `false` uses
/// 0o644 (non-sensitive config). Tmpfile name combines PID and an in-
/// process monotonic counter so concurrent writers in the same process
/// don't collide on the tmp path.
pub fn atomic_write_bytes(path: &Path, bytes: &[u8], secret: bool) -> io::Result<()> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    static ATOMIC_WRITE_SEQ: AtomicU64 = AtomicU64::new(0);
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
    std::fs::create_dir_all(parent)?;
    let seq = ATOMIC_WRITE_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = parent.join(format!(
        ".{}.tmp.{}.{seq}",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("config"),
        std::process::id()
    ));
    {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = if secret { 0o600 } else { 0o644 };
            f.set_permissions(std::fs::Permissions::from_mode(mode))?;
        }
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
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

/// Canonical rule list for account ids. Referenced from README + CHANGELOG;
/// keep this doc-comment as the single source of truth so the rules don't
/// drift across documents.
///
/// Section names appear inside `[brackets]` in the config file AND are used
/// as the filename stem for token storage (`{tokens_dir}/{name}.json`). The
/// rules below serve both: round-trip safety in the INI file AND path-safety
/// when joined with `tokens_dir`. Without this, a name of `..%2Fconfig`
/// would let `POST/DELETE /api/accounts/{id}` escape the tokens directory.
///
/// Rules (rejected if violated):
/// - empty or longer than 64 characters
/// - `.` or `..` (current/parent directory)
/// - starts with `.` (hidden file)
/// - contains `/`, `\`, or NUL (path separators)
/// - contains `[`, `]`, `=`, `#`, `\n`, or `\r` (INI structural characters)
/// - any other control character (`is_control()`)
///
/// Defense in depth: `parse_config_str` also runs this on every section header
/// at parse time so a hand-edited config can't smuggle a traversal name
/// through startup. `token_file_path` carries a `debug_assert!` tripwire.
pub fn validate_section_name(s: &str) -> Result<(), &'static str> {
    if s.is_empty() {
        return Err("name must not be empty");
    }
    if s.len() > 64 {
        return Err("name must be 64 characters or fewer");
    }
    if s == "." || s == ".." {
        return Err("name must not be '.' or '..'");
    }
    if s.starts_with('.') {
        return Err("name must not start with '.'");
    }
    for c in s.chars() {
        // Path separators and traversal characters: rejected for filesystem
        // safety because the name is joined with `tokens_dir` as a filename.
        if c == '/' || c == '\\' || c == '\0' {
            return Err("name must not contain '/', '\\\\', or NUL");
        }
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

/// Cheap structural checks on OAuth credentials that catch paste errors at
/// the boundary. Without this, a wrong client-id survives until token
/// refresh and surfaces days later as an opaque "Token refresh failed"
/// mid-session (or an Azure error page mid-authorize). Shape-only by design:
/// no network, no live validation — the provider stays authoritative.
///
/// Shapes enforced: Azure Application (client) IDs are GUIDs; Google OAuth
/// client IDs end in `.apps.googleusercontent.com`. Both have been stable
/// for over a decade. A leading `fmu1-` (Fastmail API token prefix) gets a
/// targeted message because that's the observed real-world paste error.
pub fn credential_shape_error(acct: &AccountConfig) -> Option<String> {
    match acct {
        AccountConfig::Fastmail { .. } => None,
        AccountConfig::Outlook { client_id, .. } => {
            if is_guid(client_id) {
                None
            } else if client_id.starts_with("fmu1-") {
                Some(
                    "client-id looks like a Fastmail API token; Outlook needs the Azure \
                     Application (client) ID — a GUID like 00000000-0000-0000-0000-000000000000"
                        .into(),
                )
            } else {
                Some(
                    "client-id is not an Azure Application (client) ID (expected a GUID \
                     like 00000000-0000-0000-0000-000000000000)"
                        .into(),
                )
            }
        }
        AccountConfig::Gmail { client_id, .. } => {
            if client_id.ends_with(".apps.googleusercontent.com") {
                None
            } else if client_id.starts_with("fmu1-") {
                Some(
                    "client-id looks like a Fastmail API token; Gmail needs the OAuth \
                     client ID ending in .apps.googleusercontent.com"
                        .into(),
                )
            } else {
                Some(
                    "client-id is not a Google OAuth client ID (expected to end with \
                     .apps.googleusercontent.com)"
                        .into(),
                )
            }
        }
    }
}

fn is_guid(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    parts.len() == 5
        && [8usize, 4, 4, 4, 12]
            .iter()
            .zip(&parts)
            .all(|(len, p)| p.len() == *len && p.chars().all(|c| c.is_ascii_hexdigit()))
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
            } else if let Some(msg) = credential_shape_error(cfg) {
                errs.push(FieldError::new(FieldId::ClientId, msg));
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
            } else if let Some(msg) = credential_shape_error(cfg) {
                errs.push(FieldError::new(FieldId::ClientId, msg));
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
/// for that account; `None` means idle.
///
/// `std::sync::Mutex` rather than `tokio::sync::Mutex` so the RAII
/// `AuthorizingGuard` can clear the slot from `Drop` (which is sync).
/// The critical section is two atomic ops, never held across `.await` —
/// so blocking briefly is safe.
pub type AuthorizingSlot = std::sync::Mutex<Option<String>>;

/// RAII guard: claim the slot on construction, release on Drop. Without
/// this, a panic anywhere in `authorize_account`'s OAuth flow would leak
/// the slot — every subsequent /authorize would 409 until restart
/// (roborev 186 #4).
#[derive(Debug)]
pub struct AuthorizingGuard<'a> {
    slot: &'a AuthorizingSlot,
}

impl<'a> AuthorizingGuard<'a> {
    /// Claim the slot for `account`. Returns Err containing the existing
    /// holder's id if another flow is already in progress.
    pub fn try_claim(slot: &'a AuthorizingSlot, account: &str) -> Result<Self, String> {
        let mut guard = slot.lock().expect("AuthorizingSlot mutex poisoned");
        if let Some(ref existing) = *guard {
            return Err(existing.clone());
        }
        *guard = Some(account.to_string());
        Ok(Self { slot })
    }
}

impl Drop for AuthorizingGuard<'_> {
    fn drop(&mut self) {
        // If the mutex is poisoned the process is already in trouble; we
        // still attempt to release so a subsequent claim isn't blocked
        // forever just because a previous holder panicked.
        if let Ok(mut guard) = self.slot.lock() {
            *guard = None;
        }
    }
}

// =============================================================================
// Token file path
// =============================================================================

/// Where this account's OAuth tokens live on disk. Matches the layout
/// established by `outlook::load_tokens` / `gmail::load_session`.
///
/// All callers must validate `account` via `validate_section_name` before
/// reaching here; the debug_assert is a tripwire that catches regressions
/// in test builds. Registry-key lookups (`reg.account_configs.contains_key`)
/// in the HTTP handlers provide the primary defense: ids never reach the
/// in-memory registry without validation, so a traversal token can't be
/// passed through to here at runtime.
pub fn token_file_path(tokens_dir: &Path, account: &str) -> PathBuf {
    debug_assert!(
        validate_section_name(account).is_ok(),
        "token_file_path called with unvalidated account id: {account:?}"
    );
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

/// Build the `GET /api/accounts` wire list from config + live-session info.
///
/// Every configured account appears — accounts without a live session are
/// included with `authStatus: "pending"` so the UI can show them with an
/// Authorize affordance instead of silently omitting them (the old behavior,
/// which made configured-but-unauthorized accounts invisible in the client).
///
/// `live` maps account id → (email, provider) for accounts with sessions;
/// session data wins over config because it's authoritative post-connect.
/// `clientId` is exposed for OAuth providers so the settings edit form can
/// display it (it is public, not a secret).
pub fn wire_account_list(
    configs: &BTreeMap<String, AccountConfig>,
    live: &std::collections::HashMap<String, (String, String)>,
    default_account: &str,
) -> Vec<serde_json::Value> {
    configs
        .iter()
        .map(|(id, acct)| {
            let session = live.get(id);
            let email = match session {
                Some((email, _)) => Some(email.as_str()),
                None => acct.configured_email(),
            };
            let provider = match session {
                Some((_, provider)) => provider.as_str(),
                None => acct.provider_str(),
            };
            serde_json::json!({
                "id": id,
                "email": email,
                "provider": provider,
                "isDefault": id == default_account,
                "authStatus": if session.is_some() { "ok" } else { "pending" },
                "clientId": acct.oauth_client_id(),
            })
        })
        .collect()
}

/// Compare the config file on disk against the running registry's accounts.
/// Hand-edits after startup never take effect (main.rs loads config once),
/// so a divergent file means the user is waiting on changes that will never
/// arrive — return a banner telling them to restart.
///
/// Parse errors are compared against the startup snapshot rather than
/// requiring the parsed accounts to differ: a hand-edit that adds a
/// *malformed* section is dropped by the parser (accounts stay equal), but
/// the new parse error is still evidence the file changed. Conversely, a
/// file whose parse errors are unchanged since startup hasn't been edited —
/// those errors were already surfaced by `startup_config_errors`.
///
/// `default_account` is deliberately NOT compared: the registry's default may
/// legitimately diverge from disk when the configured default failed to
/// connect and `resolve_default_account` picked a fallback.
pub fn stale_config_banner(
    config_path: &Path,
    disk: &ConfigFile,
    disk_parse_errors: &[ConfigParseError],
    startup_parse_errors: &[ConfigParseError],
    running: &BTreeMap<String, AccountConfig>,
) -> Option<crate::types::AccountError> {
    if disk.accounts == *running && disk_parse_errors == startup_parse_errors {
        return None;
    }
    Some(crate::types::AccountError {
        account: config_path.display().to_string(),
        // Rendered by the UI banner as a parenthetical label; a real
        // provider name would be wrong here and an empty one renders as "()".
        provider: "config".into(),
        error: "Config file changed on disk after startup — restart supervillain to apply \
                hand-edits (Settings changes apply immediately and would overwrite them)"
            .into(),
    })
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

    // Take the lock, mutate, write config — all held under the write lock
    // (Carmack et al.: a millisecond of held-lock during disk write beats
    // the lost-update race where T2's snapshot overwrites T1's commit).
    {
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
        atomic_write_config(&state.config_path, &reg.snapshot())
            .map_err(|e| Error::Internal(format!("failed to write config: {e}")))?;
        // Inside the write lock so no GET can observe the clean file against
        // the stale baseline (sync lock, held for a clear() — never awaits).
        state.reset_config_error_baseline();
    }

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
    {
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
        atomic_write_config(&state.config_path, &reg.snapshot())
            .map_err(|e| Error::Internal(format!("failed to write config: {e}")))?;
        state.reset_config_error_baseline();
    }

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
    {
        let mut reg = state.accounts.write().await;
        if !reg.account_configs.contains_key(&id) {
            return Err(Error::NotFound(format!("account '{id}' not found")));
        }
        reg.default_account = id.clone();
        atomic_write_config(&state.config_path, &reg.snapshot())
            .map_err(|e| Error::Internal(format!("failed to write config: {e}")))?;
        state.reset_config_error_baseline();
    }
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

    // RAII guard: slot is released when `_guard` drops, even on panic.
    let _guard = AuthorizingGuard::try_claim(&state.authorizing, &id)
        .map_err(|other| Error::Conflict(format!("another authorization in progress: {other}")))?;

    let outcome = run_and_install_authorize(&id, &account, &state).await;

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

    let is_default = {
        let mut reg = state.accounts.write().await;
        reg.sessions.insert(
            id.to_string(),
            SessionLock::new(tokio::sync::RwLock::new(session)),
        );
        reg.account_configs
            .insert(id.to_string(), updated_account.clone());
        promote_default_if_empty(&mut reg, id);
        atomic_write_config(&state.config_path, &reg.snapshot())
            .map_err(|e| format!("failed to write config: {e}"))?;
        state.reset_config_error_baseline();
        reg.default_account == id
    };

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
        let (parsed, errors) = parse_config_str(&s);
        assert!(errors.is_empty());
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
        let (parsed, _) = parse_config_str(original);
        let reserialized = serialize_config(&parsed);
        let (reparsed, _) = parse_config_str(&reserialized);
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
        // Outlook needs client-id; without it the section is skipped and
        // the parser emits a structured error so the UI can surface it.
        let s = "[broken]\nprovider = outlook\n\n[ok]\nprovider = fastmail\nusername = u@fm.com\napi-token = t\n";
        let (cfg, errors) = parse_config_str(s);
        assert!(cfg.accounts.contains_key("ok"));
        assert!(!cfg.accounts.contains_key("broken"));
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].section, "broken");
        assert_eq!(errors[0].provider, "outlook");
        assert!(errors[0].reason.contains("client-id"));
    }

    #[test]
    fn parse_reports_typoed_key_as_missing_field() {
        // Regression: a config with `client = ...` instead of `client-id = ...`
        // used to be silently dropped. It must now surface as a parse error.
        let s = "[gmail]\nprovider = gmail\nclient = abc.apps.googleusercontent.com\nclient-secret = secret\n";
        let (cfg, errors) = parse_config_str(s);
        assert!(cfg.accounts.is_empty());
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].section, "gmail");
        assert_eq!(errors[0].provider, "gmail");
        assert!(errors[0].reason.contains("client-id"));
    }

    #[test]
    fn parse_rejects_path_traversal_section_names() {
        // A hand-edited config can't smuggle a traversal name through startup.
        // The hostile name is replaced with a placeholder in the surfaced
        // error so the UI banner never echoes back the validator-rejected
        // string (which by definition contains characters the validator
        // banned, including ones escapeHtml does not encode).
        let bad = "\
[../escape]
provider = fastmail
username = u@fm.com
api-token = tok

[ok]
provider = fastmail
username = ok@fm.com
api-token = tok
";
        let (cfg, errors) = parse_config_str(bad);
        assert!(!cfg.accounts.contains_key("../escape"));
        assert!(cfg.accounts.contains_key("ok"));
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].section, MALFORMED_SECTION_PLACEHOLDER);
        assert!(errors[0].provider.is_empty());
        assert!(errors[0].reason.starts_with("Invalid section name:"));
    }

    #[test]
    fn parse_unknown_provider_is_reported_with_placeholder() {
        // Unknown provider names are NOT echoed into ConfigParseError.provider
        // — they get the placeholder, so a tampered config with a hostile
        // string (quotes, `>`, etc.) can't escape attribute-context HTML
        // in the UI banner. The reason text still says "unknown provider"
        // and the tracing::warn line preserves the original for debugging.
        let s = "[bad]\nprovider = yahoo\nusername = u@y.com\n";
        let (cfg, errors) = parse_config_str(s);
        assert!(cfg.accounts.is_empty());
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].provider, MALFORMED_PROVIDER_PLACEHOLDER);
        assert!(errors[0].reason.contains("unknown provider"));
    }

    #[test]
    fn parse_hostile_provider_string_does_not_reach_ui() {
        // The XSS shape the previous review flagged: a tampered config with
        // a provider value containing `"` or `>` would land verbatim in
        // ConfigParseError.provider and then (combined with the UI's
        // attribute-context escapeHtml gap) escape its render context.
        // Sanitization on the parser side closes the vector at the source.
        let hostile = "\"><script>alert(1)</script>";
        let s = format!("[bad]\nprovider = {hostile}\n");
        let (cfg, errors) = parse_config_str(&s);
        assert!(cfg.accounts.is_empty());
        assert_eq!(errors.len(), 1);
        // The hostile bytes must not appear in the field that the UI banner
        // renders; only the fixed-shape placeholder constant does.
        assert_eq!(errors[0].provider, MALFORMED_PROVIDER_PLACEHOLDER);
        assert!(!errors[0].provider.contains("script"));
        assert!(!errors[0].provider.contains("alert"));
    }

    #[test]
    fn parse_missing_provider_key_is_reported_distinctly() {
        // Regression: a section without a `provider` line used to default to
        // "fastmail" and then emit a misleading "missing username" error
        // even if the keys were obviously Gmail-shaped. Surface the actual
        // missing field so the user knows what to add.
        let s = "[acct]\nclient-id = cid\nclient-secret = cs\n";
        let (cfg, errors) = parse_config_str(s);
        assert!(cfg.accounts.is_empty());
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].section, "acct");
        assert!(errors[0].provider.is_empty());
        assert!(errors[0].reason.contains("`provider`"));
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
        // Path-traversal hardening (roborev 186 #1):
        assert!(validate_section_name("..").is_err(), "must reject '..'");
        assert!(validate_section_name(".").is_err(), "must reject '.'");
        assert!(
            validate_section_name(".hidden").is_err(),
            "must reject leading dot (also addresses #10)"
        );
        assert!(validate_section_name("foo/bar").is_err(), "must reject '/'");
        assert!(
            validate_section_name("foo\\bar").is_err(),
            "must reject '\\\\'"
        );
        assert!(
            validate_section_name("foo\0bar").is_err(),
            "must reject NUL"
        );
        // The on-wire attack vector `POST /api/accounts/..%2Fconfig` is
        // URL-decoded by axum to `id = "../config"` BEFORE this validator
        // sees it. Both the encoded literal and the decoded form must be
        // rejected, which the leading-dot + '/' rules handle together.
        assert!(
            validate_section_name("..%2Fconfig").is_err(),
            "leading-dot rule rejects the literal '..%2Fconfig'"
        );
        assert!(
            validate_section_name("../config").is_err(),
            "'/' + leading-dot rules reject the decoded form"
        );
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

    // ---- Credential shape validation ----

    #[test]
    fn credential_shape_flags_fastmail_token_pasted_as_outlook_client_id() {
        // The real-world paste error: a Fastmail api-token in an Outlook
        // section. Without this check it survives until token refresh and
        // surfaces days later as an opaque "Token refresh failed".
        let acct = outlook("fmu1-9d4140f1-deadbeef-0-cafe", None);
        let msg = credential_shape_error(&acct).expect("fmu1- token must be flagged");
        assert!(
            msg.contains("Fastmail"),
            "message should name the likely mistake: {msg}"
        );
    }

    #[test]
    fn credential_shape_requires_guid_for_outlook() {
        assert!(credential_shape_error(&outlook("not-a-guid", None)).is_some());
        assert!(
            credential_shape_error(&outlook("0e86662a-14b9-4e95-97d6-e91972f91d48", None))
                .is_none()
        );
        // Azure portal shows uppercase hex in some views; both must pass.
        assert!(
            credential_shape_error(&outlook("0E86662A-14B9-4E95-97D6-E91972F91D48", None))
                .is_none()
        );
        // Right lengths, non-hex chars: still rejected.
        assert!(
            credential_shape_error(&outlook("0e86662g-14b9-4e95-97d6-e91972f91d48", None))
                .is_some()
        );
    }

    #[test]
    fn credential_shape_requires_googleusercontent_suffix_for_gmail() {
        // An Azure GUID pasted into a Gmail section is wrong too.
        assert!(
            credential_shape_error(&gmail("0e86662a-14b9-4e95-97d6-e91972f91d48", "s", None))
                .is_some()
        );
        assert!(
            credential_shape_error(&gmail("123-abc.apps.googleusercontent.com", "s", None))
                .is_none()
        );
        let fm = credential_shape_error(&gmail("fmu1-sometoken", "s", None))
            .expect("fmu1- flagged for gmail too");
        assert!(fm.contains("Fastmail"));
    }

    #[test]
    fn credential_shape_ignores_fastmail_accounts() {
        assert!(credential_shape_error(&fastmail("u@fm.com", "fmu1-tok")).is_none());
    }

    #[test]
    fn validate_account_rejects_malformed_client_id_shape() {
        let errs = validate_account(&outlook("fmu1-token-pasted-here", None), "ok").unwrap_err();
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].field, FieldId::ClientId);
        // Empty client-id still reports the empty message, not the shape one.
        let errs = validate_account(&outlook("", None), "ok").unwrap_err();
        assert_eq!(errs[0].field, FieldId::ClientId);
        assert!(errs[0].message.contains("empty"));
    }

    // ---- Wire account list (GET /api/accounts) ----

    fn live(entries: &[(&str, &str, &str)]) -> std::collections::HashMap<String, (String, String)> {
        entries
            .iter()
            .map(|(id, email, provider)| {
                (
                    (*id).to_string(),
                    ((*email).to_string(), (*provider).to_string()),
                )
            })
            .collect()
    }

    #[test]
    fn wire_list_includes_pending_accounts_with_config_email() {
        // An OAuth account with no live session must still appear, marked
        // pending, with whatever email the config knows. This is the fix for
        // configured accounts being invisible in the client.
        let mut configs = BTreeMap::new();
        configs.insert(
            "gm".into(),
            gmail("x.apps.googleusercontent.com", "s", Some("u@g.com")),
        );
        configs.insert(
            "ms".into(),
            outlook("0e86662a-14b9-4e95-97d6-e91972f91d48", None),
        );
        let list = wire_account_list(&configs, &live(&[]), "gm");
        assert_eq!(list.len(), 2);
        assert_eq!(list[0]["id"], "gm");
        assert_eq!(list[0]["authStatus"], "pending");
        assert_eq!(list[0]["email"], "u@g.com");
        assert_eq!(list[0]["isDefault"], true);
        // Outlook without a configured email: null, not a panic or "".
        assert_eq!(list[1]["id"], "ms");
        assert!(list[1]["email"].is_null());
        assert_eq!(list[1]["isDefault"], false);
    }

    #[test]
    fn wire_list_uses_session_email_and_marks_ok() {
        let mut configs = BTreeMap::new();
        configs.insert("fm".into(), fastmail("config@fm.com", "tok"));
        // Session email wins over config (session is authoritative post-connect).
        let list = wire_account_list(&configs, &live(&[("fm", "live@fm.com", "fastmail")]), "fm");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0]["authStatus"], "ok");
        assert_eq!(list[0]["email"], "live@fm.com");
        assert_eq!(list[0]["provider"], "fastmail");
    }

    #[test]
    fn wire_list_exposes_client_id_for_oauth_only() {
        // The settings edit form reads clientId from this list; Fastmail has
        // none and must not grow a null field the UI trips on.
        let mut configs = BTreeMap::new();
        configs.insert("fm".into(), fastmail("u@fm.com", "tok"));
        configs.insert(
            "ms".into(),
            outlook("0e86662a-14b9-4e95-97d6-e91972f91d48", None),
        );
        let list = wire_account_list(&configs, &live(&[]), "");
        assert_eq!(list[1]["clientId"], "0e86662a-14b9-4e95-97d6-e91972f91d48");
        assert!(list[0].get("clientId").is_none() || list[0]["clientId"].is_null());
    }

    // ---- Stale-config detection ----

    #[test]
    fn stale_banner_none_when_disk_matches_registry() {
        let mut accounts = BTreeMap::new();
        accounts.insert("fm".to_string(), fastmail("u@fm.com", "tok"));
        let disk = ConfigFile {
            default_account: Some("fm".into()),
            accounts: accounts.clone(),
        };
        assert!(stale_config_banner(Path::new("/x/config"), &disk, &[], &[], &accounts).is_none());
    }

    #[test]
    fn stale_banner_fires_when_disk_has_hand_edits() {
        let mut running = BTreeMap::new();
        running.insert("fm".to_string(), fastmail("u@fm.com", "tok"));
        // Hand-edit added a section after startup.
        let mut edited = running.clone();
        edited.insert("new-acct".to_string(), fastmail("n@fm.com", "t2"));
        let disk = ConfigFile {
            default_account: Some("fm".into()),
            accounts: edited,
        };
        let banner = stale_config_banner(Path::new("/x/config"), &disk, &[], &[], &running)
            .expect("must fire");
        assert!(
            banner.error.contains("restart"),
            "tells the user the fix: {}",
            banner.error
        );
        assert_eq!(banner.account, "/x/config");
        // Labeled "config" so the UI banner doesn't render an empty "()"
        // or imply a provider connection failure.
        assert_eq!(banner.provider, "config");
    }

    #[test]
    fn stale_banner_fires_when_field_edited_in_place() {
        let mut running = BTreeMap::new();
        running.insert("fm".to_string(), fastmail("u@fm.com", "old-tok"));
        let mut edited = BTreeMap::new();
        edited.insert("fm".to_string(), fastmail("u@fm.com", "new-tok"));
        let disk = ConfigFile {
            default_account: Some("fm".into()),
            accounts: edited,
        };
        assert!(stale_config_banner(Path::new("/x/config"), &disk, &[], &[], &running).is_some());
    }

    #[test]
    fn stale_banner_ignores_default_account_divergence() {
        // The registry's default may legitimately differ from disk when the
        // configured default failed to connect and startup picked a fallback.
        // That must NOT read as "config changed on disk".
        let mut accounts = BTreeMap::new();
        accounts.insert("fm".to_string(), fastmail("u@fm.com", "tok"));
        let disk = ConfigFile {
            default_account: Some("something-else".into()),
            accounts: accounts.clone(),
        };
        assert!(stale_config_banner(Path::new("/x/config"), &disk, &[], &[], &accounts).is_none());
    }

    #[test]
    fn stale_banner_fires_when_hand_edit_adds_malformed_section() {
        // A malformed section is dropped by the parser, so the account maps
        // stay equal — the fresh parse error is the only evidence the file
        // changed. Roborev job 267 finding #2.
        let mut running = BTreeMap::new();
        running.insert("fm".to_string(), fastmail("u@fm.com", "tok"));
        let disk = ConfigFile {
            default_account: Some("fm".into()),
            accounts: running.clone(),
        };
        let new_err = vec![ConfigParseError {
            section: "typo".into(),
            provider: String::new(),
            reason: "missing required field `provider`".into(),
        }];
        assert!(
            stale_config_banner(Path::new("/x/config"), &disk, &new_err, &[], &running).is_some()
        );
    }

    #[test]
    fn stale_banner_quiet_when_parse_errors_unchanged_since_startup() {
        // A config that was already broken at startup and untouched since
        // must NOT read as "changed on disk" — startup_config_errors already
        // surfaced those parse errors.
        let mut running = BTreeMap::new();
        running.insert("fm".to_string(), fastmail("u@fm.com", "tok"));
        let disk = ConfigFile {
            default_account: Some("fm".into()),
            accounts: running.clone(),
        };
        let startup_err = vec![ConfigParseError {
            section: "broken".into(),
            provider: "outlook".into(),
            reason: "missing required field `client-id`".into(),
        }];
        assert!(
            stale_config_banner(
                Path::new("/x/config"),
                &disk,
                &startup_err,
                &startup_err,
                &running
            )
            .is_none()
        );
    }

    #[test]
    fn reset_config_error_baseline_clears_seeded_errors() {
        // The roborev-268 fix: after an app-made config write, the parse-
        // error baseline must be empty so the clean rewrite doesn't read as
        // a hand-edit forever.
        let state = crate::types::AppState {
            accounts: tokio::sync::RwLock::new(empty_registry()),
            account_errors: tokio::sync::RwLock::new(Vec::new()),
            splits_config_path: PathBuf::from("/x/splits.json"),
            timezone_config_path: PathBuf::from("/x/timezone.json"),
            timezone_write_lock: tokio::sync::Mutex::new(()),
            config_path: PathBuf::from("/x/config"),
            tokens_dir: PathBuf::from("/x/tokens"),
            token_store: std::sync::Arc::new(crate::platform::FsTokenStore::new(PathBuf::from(
                "/x/tokens",
            ))),
            authorizing: AuthorizingSlot::default(),
            config_error_baseline: std::sync::RwLock::new(vec![ConfigParseError {
                section: "broken".into(),
                provider: String::new(),
                reason: "missing required field `provider`".into(),
            }]),
            prefetch: std::sync::Arc::new(crate::prefetch::PrefetchCache::new()),
        };
        state.reset_config_error_baseline();
        assert!(state.config_error_baseline.read().unwrap().is_empty());
    }

    #[test]
    fn every_app_config_write_site_resets_the_baseline() {
        // Source-level tripwire: each handler write of the config file must
        // be followed by a baseline reset, or a broken-at-startup config
        // plus that write path regresses to the permanent-banner bug
        // (roborev 268 #1). Only handler code is scanned — the test module
        // is sliced off so its own occurrences don't skew the counts.
        let src = include_str!("accounts.rs");
        let handler_src = &src[..src.find("mod tests").expect("tests module exists")];
        let writes = handler_src
            .matches("atomic_write_config(&state.config_path")
            .count();
        let resets = handler_src
            .matches("state.reset_config_error_baseline()")
            .count();
        assert_eq!(
            writes, resets,
            "every atomic_write_config(&state.config_path, ..) handler site must be \
             paired with state.reset_config_error_baseline() ({writes} writes vs \
             {resets} resets)"
        );
        assert!(writes >= 4, "expected the four known handler write sites");
    }

    #[test]
    fn stale_banner_fires_when_startup_parse_error_fixed_on_disk() {
        // The inverse: user fixed the broken section after startup. The
        // parse errors went away, accounts differ or not — either way the
        // file changed and a restart is needed to pick up the fix.
        let mut running = BTreeMap::new();
        running.insert("fm".to_string(), fastmail("u@fm.com", "tok"));
        let disk = ConfigFile {
            default_account: Some("fm".into()),
            accounts: running.clone(),
        };
        let startup_err = vec![ConfigParseError {
            section: "broken".into(),
            provider: "outlook".into(),
            reason: "missing required field `client-id`".into(),
        }];
        assert!(
            stale_config_banner(Path::new("/x/config"), &disk, &[], &startup_err, &running)
                .is_some()
        );
    }

    // ---- Authorize single-flight ----

    #[test]
    fn authorize_acquires_global_lock_then_releases() {
        let slot = AuthorizingSlot::default();
        let guard = AuthorizingGuard::try_claim(&slot, "fm").expect("first claim succeeds");
        assert_eq!(slot.lock().unwrap().as_deref(), Some("fm"));
        drop(guard);
        assert!(slot.lock().unwrap().is_none(), "drop releases the slot");
    }

    #[test]
    fn authorize_returns_409_when_already_in_progress() {
        let slot = AuthorizingSlot::default();
        let _guard = AuthorizingGuard::try_claim(&slot, "fm").expect("first claim succeeds");
        let err = AuthorizingGuard::try_claim(&slot, "outlook").unwrap_err();
        assert_eq!(err, "fm", "should report the existing holder");
    }

    /// Panic-safety regression (roborev 186 #4): a panic inside the OAuth
    /// flow must not leak the slot. RAII Drop releases regardless.
    #[test]
    fn authorize_slot_released_on_panic() {
        let slot = AuthorizingSlot::default();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = AuthorizingGuard::try_claim(&slot, "fm").unwrap();
            panic!("simulated OAuth flow panic");
        }));
        assert!(result.is_err(), "the closure must panic");
        assert!(
            slot.lock().unwrap().is_none(),
            "Drop must release the slot even when the holder panics"
        );
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

    // ---- Registry snapshot/round-trip (roborev 186 #8) ----
    //
    // The HTTP handlers mutate `AccountRegistry` under a write lock and then
    // call `atomic_write_config(&reg.snapshot())`. These tests exercise the
    // snapshot-and-write portion against a tempdir, which is the part of the
    // handler we can pin without a full tower::oneshot harness. Combined
    // with the existing pure-helper coverage of the lock-held write, the
    // 409 conflict path, and the panic-safety guard, this gives the new
    // handlers concrete regression coverage at the integration boundary.

    fn empty_registry() -> AccountRegistry {
        AccountRegistry {
            sessions: std::collections::HashMap::new(),
            account_configs: BTreeMap::new(),
            default_account: String::new(),
        }
    }

    // ---- startup_config_errors (the seam main.rs uses) ----

    #[test]
    fn startup_errors_combine_all_three_sources_with_resolved_paths() {
        // Parse error from the INI file, parse error from splits.json, parse
        // error from timezone.json. Each must surface with the actual file
        // path that was checked (not a hardcoded ~/.config/... string), so
        // users with XDG_CONFIG_HOME set or custom paths see something
        // actionable.
        let cfg_path = Path::new("/custom/xdg/config");
        let splits_path = Path::new("/custom/xdg/splits.json");
        let tz_path = Path::new("/custom/xdg/timezone.json");

        let parse_errors = vec![ConfigParseError {
            section: "gmail".into(),
            provider: "gmail".into(),
            reason: "missing required field `client-id`".into(),
        }];
        let splits_err = Err("JSON parse failed: line 1".to_string());
        let tz_err = Err("JSON parse failed: line 2".to_string());

        let errors = startup_config_errors(
            cfg_path,
            parse_errors,
            splits_path,
            splits_err,
            tz_path,
            tz_err,
        );

        assert_eq!(errors.len(), 3);

        // Account INI parse error keeps section+provider, references the
        // actual config path.
        assert_eq!(errors[0].account, "gmail");
        assert_eq!(errors[0].provider, "gmail");
        assert!(errors[0].error.contains("/custom/xdg/config"));
        assert!(errors[0].error.contains("client-id"));

        // splits.json: account = real path, provider empty (not "config").
        assert_eq!(errors[1].account, "/custom/xdg/splits.json");
        assert!(errors[1].provider.is_empty());
        assert!(errors[1].error.contains("using defaults until fixed"));

        // timezone.json: same shape.
        assert_eq!(errors[2].account, "/custom/xdg/timezone.json");
        assert!(errors[2].provider.is_empty());
        assert!(errors[2].error.contains("using defaults until fixed"));
    }

    #[test]
    fn startup_errors_mixed_sources_preserve_order() {
        // Mixed case — parse_errors present, splits Ok, timezone Err — pins
        // the ordering contract that the UI banner relies on:
        // parse errors first, then splits (if any), then timezone (if any).
        // A reordering or accidental dedup would slip past the all-Ok and
        // all-Err endpoint tests; this one wouldn't.
        let errors = startup_config_errors(
            Path::new("/x/config"),
            vec![ConfigParseError {
                section: "fastmail".into(),
                provider: "fastmail".into(),
                reason: "missing required field `api-token`".into(),
            }],
            Path::new("/x/splits.json"),
            Ok(None),
            Path::new("/x/timezone.json"),
            Err("EOF while parsing".into()),
        );

        assert_eq!(errors.len(), 2);
        // Index 0 must be the parse error (config path interpolated, real
        // provider preserved because it's a known token).
        assert_eq!(errors[0].account, "fastmail");
        assert_eq!(errors[0].provider, "fastmail");
        assert!(errors[0].error.contains("/x/config"));
        // Index 1 must be timezone (splits was Ok so it didn't push).
        assert_eq!(errors[1].account, "/x/timezone.json");
        assert!(errors[1].provider.is_empty());
        assert!(errors[1].error.contains("EOF while parsing"));
    }

    #[test]
    fn startup_errors_empty_when_everything_is_clean() {
        // No parse errors, splits/timezone both Ok (whether the file was
        // present or not) → empty error list. Banner stays hidden.
        let errors = startup_config_errors(
            Path::new("/x/config"),
            Vec::new(),
            Path::new("/x/splits.json"),
            Ok(None),
            Path::new("/x/timezone.json"),
            Ok(None),
        );
        assert!(errors.is_empty());
    }

    #[test]
    fn registry_snapshot_roundtrips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");

        let mut reg = empty_registry();
        reg.account_configs
            .insert("fm".into(), fastmail("u@fm.com", "tok"));
        reg.default_account = "fm".into();

        atomic_write_config(&path, &reg.snapshot()).unwrap();
        let (parsed, _) = parse_config(&path);
        assert_eq!(parsed.default_account.as_deref(), Some("fm"));
        assert_eq!(parsed.accounts.len(), 1);
    }

    #[test]
    fn registry_delete_then_snapshot_promotes_first_remaining() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");

        let mut reg = empty_registry();
        reg.account_configs
            .insert("alpha".into(), fastmail("a@a.com", "t"));
        reg.account_configs
            .insert("beta".into(), fastmail("b@b.com", "t"));
        reg.default_account = "alpha".into();
        atomic_write_config(&path, &reg.snapshot()).unwrap();

        // Simulate the delete handler's in-memory mutation.
        reg.account_configs.remove("alpha");
        reg.default_account = reg
            .account_configs
            .keys()
            .next()
            .cloned()
            .unwrap_or_default();
        atomic_write_config(&path, &reg.snapshot()).unwrap();

        let (parsed, _) = parse_config(&path);
        assert_eq!(parsed.accounts.len(), 1);
        assert_eq!(parsed.default_account.as_deref(), Some("beta"));
    }

    #[test]
    fn registry_promote_default_when_empty_helper_idempotent() {
        let mut reg = empty_registry();
        reg.account_configs
            .insert("only".into(), fastmail("u@u.com", "t"));
        // First call promotes.
        promote_default_if_empty(&mut reg, "only");
        assert_eq!(reg.default_account, "only");
        // Second call is a no-op.
        promote_default_if_empty(&mut reg, "different");
        assert_eq!(reg.default_account, "only");
    }

    #[test]
    fn atomic_write_seq_disambiguates_concurrent_tmpfiles() {
        // Roborev 186 #3: two same-process writes must not clobber each
        // other's tmpfile. The per-call AtomicU64 counter guarantees unique
        // tmp names even from the same PID.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        let mut reg = empty_registry();
        reg.account_configs
            .insert("fm".into(), fastmail("u@fm.com", "t"));

        // Two sequential writes — both succeed because the second tmp file
        // gets a different name even though they share PID and target path.
        atomic_write_config(&path, &reg.snapshot()).unwrap();
        atomic_write_config(&path, &reg.snapshot()).unwrap();
        // No stray .tmp files remain.
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().to_string()))
            .collect();
        assert!(
            entries.iter().all(|n| !n.contains(".tmp.")),
            "no .tmp file should remain after concurrent writes, got {entries:?}"
        );
    }

    #[test]
    fn authorize_releases_lock_on_oauth_error() {
        // Simulate the route-handler shape: claim, work returns Err, drop releases.
        fn run(slot: &AuthorizingSlot) -> Result<(), &'static str> {
            let _guard = AuthorizingGuard::try_claim(slot, "fm").map_err(|_| "busy")?;
            Err("oauth failed")
        }
        let slot = AuthorizingSlot::default();
        assert!(run(&slot).is_err());
        // After the simulated error, the slot must be free for retry.
        assert!(slot.lock().unwrap().is_none());
    }
}
