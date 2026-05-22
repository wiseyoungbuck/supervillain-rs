// Platform abstraction for OS-specific behaviors.
//
// Provider code calls `crate::platform::*` and never touches OS APIs directly.
// Today only the `desktop` module exists. When the iOS port lands (per the
// Phase 3 plan, future work), an `ios` module will be added behind `cfg`-gated
// re-exports below.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::Error;

pub mod desktop;

pub use desktop::{FsTokenStore, acquire_oauth_callback, config_dir, init_tracing, open_browser};

/// OAuth tokens persisted between sessions. Same shape across all providers
/// that use OAuth2 (Outlook, Gmail today; O365 email later).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
    pub token_expiry: DateTime<Utc>,
    pub email: String,
}

/// Persistence layer for OAuth tokens. `FsTokenStore` is the desktop impl;
/// iOS will provide a `KeychainTokenStore`.
pub trait TokenStore: Send + Sync {
    fn save(&self, account: &str, tokens: &Tokens) -> Result<(), Error>;
    fn load(&self, account: &str) -> Option<Tokens>;
    fn delete(&self, account: &str) -> Result<(), Error>;
}

/// Result of an OAuth2 authorization callback. State validation happens inside
/// `acquire_oauth_callback`; callers only see the authorization code.
#[derive(Debug)]
pub struct OauthCallback {
    pub code: String,
}
