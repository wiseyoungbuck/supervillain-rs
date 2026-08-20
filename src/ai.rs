//! AI assist (kata 6rhw): thread summarization and reply drafting via the
//! Claude API (`POST /v1/messages`, raw HTTP — no official Rust SDK).
//!
//! Optional feature: enabled only when `ANTHROPIC_API_KEY` is set in the
//! environment (never stored in config). The frontend reads
//! `GET /api/ai/status` and hides every affordance when disabled.

use crate::error::Error;
use crate::provider;
use crate::types::{AppState, Email};
use axum::extract::{Json, State};
use serde::Deserialize;
use std::sync::Arc;

/// Default per the execution plan (kata 6rhw): current Sonnet tier.
/// Overridable via `SUPERVILLAIN_AI_MODEL` for users who want a different
/// cost/quality point (e.g. `claude-opus-5` or `claude-haiku-4-5`).
pub const DEFAULT_AI_MODEL: &str = "claude-sonnet-5";
pub const AI_MODEL_ENV: &str = "SUPERVILLAIN_AI_MODEL";
pub const AI_KEY_ENV: &str = "ANTHROPIC_API_KEY";

const ANTHROPIC_BASE: &str = "https://api.anthropic.com";
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Body text is capped before prompt assembly so a pathological email can't
/// balloon the request (and its cost). Enough for a deep quoted thread.
pub const PROMPT_BODY_CHAR_CAP: usize = 12_000;

const SUMMARY_MAX_TOKENS: u32 = 1024;
const DRAFT_MAX_TOKENS: u32 = 2048;

const SUMMARIZE_SYSTEM: &str = "You summarize email threads for a busy reader. Reply with a \
     concise summary: 2-4 sentences covering what the thread is about, then, only when present, \
     bullet points for decisions, action items, and open questions. The email body may contain \
     the quoted history of earlier messages — treat that history as part of the thread. Output \
     plain text only, no markdown headers, no preamble.";

const DRAFT_SYSTEM: &str = "You draft reply emails on the user's behalf. Match the tone of the \
     conversation, be concise, and never invent facts, commitments, or details the user did not \
     supply. Output ONLY the reply body as plain text — no subject line, no quoted original, no \
     surrounding commentary. Do not include a signature; the mail client appends one.";

pub struct AiConfig {
    pub api_key: String,
    pub model: String,
}

/// Pure config resolution: no key (or an empty one) means the feature is
/// off; an empty model override falls back to the default rather than
/// sending a blank model string to the API.
pub fn resolve_ai_config(api_key: Option<&str>, model: Option<&str>) -> Option<AiConfig> {
    let api_key = api_key.filter(|k| !k.is_empty())?;
    let model = model.filter(|m| !m.is_empty()).unwrap_or(DEFAULT_AI_MODEL);
    Some(AiConfig {
        api_key: api_key.to_string(),
        model: model.to_string(),
    })
}

/// Production wrapper: `ANTHROPIC_API_KEY` (never stored in config or code)
/// + optional `SUPERVILLAIN_AI_MODEL`.
pub fn ai_config() -> Option<AiConfig> {
    let key = std::env::var(AI_KEY_ENV).ok();
    let model = std::env::var(AI_MODEL_ENV).ok();
    resolve_ai_config(key.as_deref(), model.as_deref())
}

/// Wire shape for `GET /api/ai/status`. The model is public information
/// (it names what the user's own key will be billed for); the key never
/// leaves the server.
pub fn ai_status_json(cfg: Option<&AiConfig>) -> serde_json::Value {
    match cfg {
        Some(cfg) => serde_json::json!({ "enabled": true, "model": cfg.model }),
        None => serde_json::json!({ "enabled": false }),
    }
}

// =============================================================================
// Prompt assembly (pure — perf budget <50ms, tested)
// =============================================================================

/// Minimal HTML→text for prompt assembly: drops tags (and the contents of
/// `<script>`/`<style>`), emits newlines at block boundaries, decodes the
/// entities that matter in mail bodies. Not a sanitizer — output goes into
/// an API prompt, never into a DOM.
pub fn strip_html(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    let mut rest = html;
    while let Some(open) = rest.find('<') {
        out.push_str(&rest[..open]);
        rest = &rest[open..];
        let lower = rest.get(1..7).unwrap_or_default().to_ascii_lowercase();
        // Skip script/style elements wholesale — their text is never body
        // content.
        let skip_until = if lower.starts_with("script") {
            Some("</script")
        } else if lower.starts_with("style") {
            Some("</style")
        } else {
            None
        };
        if let Some(close_tag) = skip_until {
            match rest.to_ascii_lowercase().find(close_tag) {
                Some(idx) => rest = &rest[idx..],
                None => return decode_entities(&out),
            }
        }
        let Some(end) = rest.find('>') else {
            // Unclosed tag: drop the tail.
            return decode_entities(&out);
        };
        let tag = rest[1..end].to_ascii_lowercase();
        if tag.starts_with("br")
            || tag.starts_with("/p")
            || tag.starts_with("/div")
            || tag.starts_with("/tr")
            || tag.starts_with("/li")
            || tag.starts_with("/h")
        {
            out.push('\n');
        }
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    decode_entities(&out)
}

fn decode_entities(s: &str) -> String {
    s.replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

/// The email's body as plain text: prefer the text part, fall back to
/// stripped HTML, then the preview. Capped at [`PROMPT_BODY_CHAR_CAP`]
/// (on a char boundary) so prompt size stays bounded.
fn body_text(email: &Email) -> String {
    let mut text = match (&email.text_body, &email.html_body) {
        (Some(t), _) if !t.is_empty() => t.clone(),
        (_, Some(h)) if !h.is_empty() => strip_html(h),
        _ => email.preview.clone(),
    };
    if text.len() > PROMPT_BODY_CHAR_CAP {
        let mut cut = PROMPT_BODY_CHAR_CAP;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push_str("\n[... truncated ...]");
    }
    text
}

fn sender_line(email: &Email) -> String {
    match email.from.first() {
        Some(a) => match &a.name {
            Some(n) if !n.is_empty() => format!("{n} <{}>", a.email),
            _ => a.email.clone(),
        },
        None => "(unknown sender)".into(),
    }
}

/// User-turn content for summarization. The body carries the quoted history
/// of earlier thread messages, which is deliberately retained — it IS the
/// thread context (provider-native thread expansion is follow-up work).
pub fn summarize_prompt(email: &Email) -> String {
    format!(
        "Summarize this email thread.\n\nSubject: {}\nFrom: {}\nDate: {}\n\nBody (may include \
         quoted history of earlier messages):\n{}",
        email.subject,
        sender_line(email),
        email.received_at.to_rfc3339(),
        body_text(email),
    )
}

/// User-turn content for reply drafting.
pub fn draft_prompt(original: Option<&Email>, instruction: &str) -> String {
    match original {
        Some(email) => format!(
            "Draft a reply to the email below.\nInstruction: {}\n\nOriginal message:\nSubject: \
             {}\nFrom: {}\nBody:\n{}",
            instruction,
            email.subject,
            sender_line(email),
            body_text(email),
        ),
        None => format!("Draft an email.\nInstruction: {instruction}"),
    }
}

// =============================================================================
// Claude API call (raw HTTP — no official Rust SDK)
// =============================================================================

#[derive(Deserialize)]
struct ClaudeResponse {
    #[serde(default)]
    content: Vec<ClaudeBlock>,
    #[serde(default)]
    stop_reason: Option<String>,
    #[serde(default)]
    stop_details: Option<ClaudeStopDetails>,
}

#[derive(Deserialize)]
struct ClaudeBlock {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: String,
}

#[derive(Deserialize)]
struct ClaudeStopDetails {
    #[serde(default)]
    explanation: Option<String>,
}

/// One `POST /v1/messages` call, returning the first text block.
/// Parameterized on the base URL so tests drive it against a loopback mock
/// (outlook `ensure_token_at` pattern); production passes
/// [`ANTHROPIC_BASE`]. Sonnet 5 runs adaptive thinking when the `thinking`
/// param is omitted, so the request stays minimal: model, max_tokens,
/// system, one user message.
pub async fn complete_at(
    client: &reqwest::Client,
    base: &str,
    cfg: &AiConfig,
    system: &str,
    user_prompt: &str,
    max_tokens: u32,
) -> Result<String, Error> {
    let resp = client
        .post(format!("{base}/v1/messages"))
        .header("x-api-key", &cfg.api_key)
        .header("anthropic-version", ANTHROPIC_VERSION)
        .json(&serde_json::json!({
            "model": cfg.model,
            "max_tokens": max_tokens,
            "system": system,
            "messages": [{ "role": "user", "content": user_prompt }],
        }))
        .send()
        .await?;

    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        tracing::warn!(http_status = %status, response_body = %text, "Claude API call failed");
        return Err(Error::Internal(format!(
            "Claude API error ({status}): {text}"
        )));
    }

    let parsed: ClaudeResponse = resp.json().await?;
    if parsed.stop_reason.as_deref() == Some("refusal") {
        let why = parsed
            .stop_details
            .and_then(|d| d.explanation)
            .unwrap_or_else(|| "no explanation given".into());
        return Err(Error::BadRequest(format!(
            "Claude declined this request: {why}"
        )));
    }
    parsed
        .content
        .into_iter()
        .find(|b| b.kind == "text")
        .map(|b| b.text)
        .ok_or_else(|| Error::Internal("Claude response contained no text".into()))
}

fn build_ai_client() -> reqwest::Client {
    // Longer than the provider clients' 30s: a summarize/draft turn with
    // adaptive thinking can legitimately run tens of seconds.
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(90))
        .build()
        .expect("failed to create HTTP client")
}

// =============================================================================
// HTTP handlers
// =============================================================================

/// `GET /api/ai/status` — lets the UI hide every AI affordance when no key
/// is configured (feature off is a state, not an error).
pub async fn ai_status_handler() -> Json<serde_json::Value> {
    Json(ai_status_json(ai_config().as_ref()))
}

fn require_ai_config() -> Result<AiConfig, Error> {
    ai_config().ok_or_else(|| {
        Error::BadRequest(
            "AI assist is not configured — set ANTHROPIC_API_KEY and restart supervillain".into(),
        )
    })
}

/// Fetch one email (with body) and release the session lock before any
/// Claude round-trip — the write-lock paths (credential rotation, token
/// refresh) must never wait behind a multi-second upstream call.
async fn fetch_email(state: &AppState, account: Option<&str>, id: &str) -> Result<Email, Error> {
    let session_lock = crate::routes::resolve_session(state, account).await?;
    let session = session_lock.read().await;
    let emails = provider::get_emails(&session, &[id.to_string()], true, None, true).await?;
    emails
        .into_iter()
        .next()
        .ok_or_else(|| Error::NotFound(format!("email '{id}' not found")))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SummarizeRequest {
    pub email_id: String,
    #[serde(default)]
    pub account: Option<String>,
}

/// `POST /api/ai/summarize` — `{emailId}` → `{summary}`.
pub async fn ai_summarize(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SummarizeRequest>,
) -> Result<Json<serde_json::Value>, Error> {
    let cfg = require_ai_config()?;
    let email = fetch_email(&state, req.account.as_deref(), &req.email_id).await?;
    let prompt = summarize_prompt(&email);
    let client = build_ai_client();
    let summary = complete_at(
        &client,
        ANTHROPIC_BASE,
        &cfg,
        SUMMARIZE_SYSTEM,
        &prompt,
        SUMMARY_MAX_TOKENS,
    )
    .await?;
    Ok(Json(serde_json::json!({ "summary": summary })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DraftRequest {
    #[serde(default)]
    pub email_id: Option<String>,
    #[serde(default)]
    pub instruction: Option<String>,
    #[serde(default)]
    pub account: Option<String>,
}

/// `POST /api/ai/draft` — `{emailId?, instruction?}` → `{draft}`. With an
/// emailId the draft replies to that message; without one it drafts from
/// the instruction alone.
pub async fn ai_draft(
    State(state): State<Arc<AppState>>,
    Json(req): Json<DraftRequest>,
) -> Result<Json<serde_json::Value>, Error> {
    let cfg = require_ai_config()?;
    let original = match &req.email_id {
        Some(id) => Some(fetch_email(&state, req.account.as_deref(), id).await?),
        None => None,
    };
    let instruction = req
        .instruction
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("Write an appropriate, concise reply.");
    let prompt = draft_prompt(original.as_ref(), instruction);
    let client = build_ai_client();
    let draft = complete_at(
        &client,
        ANTHROPIC_BASE,
        &cfg,
        DRAFT_SYSTEM,
        &prompt,
        DRAFT_MAX_TOKENS,
    )
    .await?;
    Ok(Json(serde_json::json!({ "draft": draft })))
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Email, EmailAddress};
    use chrono::{TimeZone, Utc};
    use std::collections::HashMap;

    fn mk_email(subject: &str, text_body: Option<&str>, html_body: Option<&str>) -> Email {
        Email {
            id: "e1".into(),
            blob_id: String::new(),
            thread_id: "t1".into(),
            mailbox_ids: HashMap::new(),
            keywords: HashMap::new(),
            received_at: Utc.with_ymd_and_hms(2026, 8, 18, 9, 0, 0).unwrap(),
            subject: subject.into(),
            from: vec![EmailAddress {
                name: Some("Jane Doe".into()),
                email: "jane@example.com".into(),
            }],
            to: Vec::new(),
            cc: Vec::new(),
            preview: String::new(),
            has_attachment: false,
            size: 0,
            text_body: text_body.map(String::from),
            html_body: html_body.map(String::from),
            has_calendar: false,
            calendar_ics: None,
            attachments: Vec::new(),
            in_reply_to: None,
        }
    }

    // ---- config resolution ----

    #[test]
    fn ai_config_requires_a_key_and_defaults_the_model() {
        assert!(resolve_ai_config(None, None).is_none());
        assert!(resolve_ai_config(Some(""), None).is_none());
        let cfg = resolve_ai_config(Some("sk-ant-xxx"), None).unwrap();
        assert_eq!(cfg.api_key, "sk-ant-xxx");
        assert_eq!(cfg.model, DEFAULT_AI_MODEL);
        assert_eq!(DEFAULT_AI_MODEL, "claude-sonnet-5");
    }

    #[test]
    fn ai_config_model_is_overridable_and_empty_override_falls_back() {
        let cfg = resolve_ai_config(Some("k"), Some("claude-opus-5")).unwrap();
        assert_eq!(cfg.model, "claude-opus-5");
        let cfg = resolve_ai_config(Some("k"), Some("")).unwrap();
        assert_eq!(cfg.model, DEFAULT_AI_MODEL);
    }

    #[test]
    fn ai_status_wire_shape() {
        let on = ai_status_json(Some(&AiConfig {
            api_key: "k".into(),
            model: "claude-sonnet-5".into(),
        }));
        assert_eq!(on["enabled"], true);
        assert_eq!(on["model"], "claude-sonnet-5");
        let off = ai_status_json(None);
        assert_eq!(off["enabled"], false);
        assert!(off.get("model").is_none() || off["model"].is_null());
    }

    // ---- prompt assembly (pure) ----

    #[test]
    fn strip_html_removes_tags_and_decodes_basic_entities() {
        let text = strip_html("<p>Hello <b>world</b> &amp; friends &lt;3</p>");
        assert_eq!(text.trim(), "Hello world & friends <3");
    }

    #[test]
    fn strip_html_drops_script_and_style_content() {
        let text = strip_html(
            "<style>.x{color:red}</style><p>Visible</p><script>alert('hidden')</script>",
        );
        assert!(text.contains("Visible"));
        assert!(!text.contains("alert"));
        assert!(!text.contains("color:red"));
    }

    #[test]
    fn summarize_prompt_carries_subject_sender_and_text_body() {
        let email = mk_email(
            "Q3 planning",
            Some("Let's meet Tuesday.\n> older quoted reply"),
            None,
        );
        let prompt = summarize_prompt(&email);
        assert!(prompt.contains("Q3 planning"));
        assert!(prompt.contains("jane@example.com"));
        assert!(prompt.contains("Let's meet Tuesday."));
        // The quoted history in the body IS the thread context — it must
        // survive into the prompt, not be stripped as noise.
        assert!(prompt.contains("older quoted reply"));
    }

    #[test]
    fn summarize_prompt_falls_back_to_stripped_html() {
        let email = mk_email("Hi", None, Some("<div>Rendered <i>only</i> as HTML</div>"));
        let prompt = summarize_prompt(&email);
        assert!(prompt.contains("Rendered only as HTML"));
        assert!(!prompt.contains("<div>"));
    }

    #[test]
    fn summarize_prompt_caps_body_length() {
        let huge = "word ".repeat(100_000);
        let email = mk_email("Big", Some(&huge), None);
        let prompt = summarize_prompt(&email);
        assert!(
            prompt.len() < PROMPT_BODY_CHAR_CAP + 1_000,
            "prompt must cap the body ({} chars)",
            prompt.len()
        );
    }

    #[test]
    fn draft_prompt_carries_instruction_and_original_context() {
        let email = mk_email("Q3 planning", Some("Can you send the numbers?"), None);
        let prompt = draft_prompt(Some(&email), "agree and promise them by Friday");
        assert!(prompt.contains("agree and promise them by Friday"));
        assert!(prompt.contains("Q3 planning"));
        assert!(prompt.contains("Can you send the numbers?"));
    }

    #[test]
    fn draft_prompt_works_without_an_original() {
        let prompt = draft_prompt(None, "invite the team to lunch on Thursday");
        assert!(prompt.contains("invite the team to lunch on Thursday"));
    }

    #[test]
    fn prompt_assembly_under_budget() {
        // Perf budget (kata 6rhw): prompt assembly (no network) < 50ms even
        // against a 100KB body. CI-tolerant per rate_limit.rs discipline.
        let huge = "lorem ipsum dolor ".repeat(6_000);
        let email = mk_email("Big thread", Some(&huge), None);
        let start = std::time::Instant::now();
        let s = summarize_prompt(&email);
        let d = draft_prompt(Some(&email), "say thanks");
        let elapsed = start.elapsed();
        assert!(!s.is_empty() && !d.is_empty());
        assert!(
            elapsed < std::time::Duration::from_millis(50),
            "prompt assembly took {elapsed:?}, budget 50ms"
        );
    }

    // ---- Claude API contract (loopback mock upstream — no real API calls) ----

    async fn spawn_claude_mock(
        status: u16,
        body: &'static str,
    ) -> (
        String,
        std::sync::Arc<std::sync::Mutex<Option<(String, String, String)>>>,
    ) {
        use axum::routing::post;
        // Records (x-api-key, anthropic-version, body).
        let recorded: std::sync::Arc<std::sync::Mutex<Option<(String, String, String)>>> =
            std::sync::Arc::new(std::sync::Mutex::new(None));
        let recorded_clone = recorded.clone();
        let app = axum::Router::new().route(
            "/v1/messages",
            post(move |headers: axum::http::HeaderMap, req_body: String| {
                let recorded = recorded_clone.clone();
                async move {
                    let h = |name: &str| {
                        headers
                            .get(name)
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or_default()
                            .to_string()
                    };
                    *recorded.lock().unwrap() =
                        Some((h("x-api-key"), h("anthropic-version"), req_body));
                    (
                        axum::http::StatusCode::from_u16(status).unwrap(),
                        [("content-type", "application/json")],
                        body,
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}"), recorded)
    }

    fn test_cfg() -> AiConfig {
        AiConfig {
            api_key: "sk-ant-test".into(),
            model: "claude-sonnet-5".into(),
        }
    }

    #[tokio::test]
    async fn complete_at_sends_the_documented_request_shape() {
        let (base, recorded) = spawn_claude_mock(
            200,
            r#"{"content":[{"type":"text","text":"THE SUMMARY"}],"stop_reason":"end_turn"}"#,
        )
        .await;
        let client = reqwest::Client::new();
        let out = complete_at(
            &client,
            &base,
            &test_cfg(),
            "You summarize emails.",
            "Summarize: hello",
            1024,
        )
        .await
        .unwrap();
        assert_eq!(out, "THE SUMMARY");

        let (key, version, body) = recorded.lock().unwrap().clone().expect("request recorded");
        assert_eq!(key, "sk-ant-test");
        assert_eq!(version, "2023-06-01");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["model"], "claude-sonnet-5");
        assert_eq!(v["max_tokens"], 1024);
        assert_eq!(v["system"], "You summarize emails.");
        assert_eq!(v["messages"][0]["role"], "user");
        assert_eq!(v["messages"][0]["content"], "Summarize: hello");
    }

    #[tokio::test]
    async fn complete_at_skips_thinking_blocks() {
        let (base, _) = spawn_claude_mock(
            200,
            r#"{"content":[{"type":"thinking","thinking":""},{"type":"text","text":"X"}],"stop_reason":"end_turn"}"#,
        )
        .await;
        let client = reqwest::Client::new();
        let out = complete_at(&client, &base, &test_cfg(), "s", "p", 64)
            .await
            .unwrap();
        assert_eq!(out, "X");
    }

    #[tokio::test]
    async fn complete_at_surfaces_refusals_as_errors() {
        let (base, _) = spawn_claude_mock(
            200,
            r#"{"content":[],"stop_reason":"refusal","stop_details":{"type":"refusal","category":null,"explanation":"declined"}}"#,
        )
        .await;
        let client = reqwest::Client::new();
        let err = complete_at(&client, &base, &test_cfg(), "s", "p", 64)
            .await
            .unwrap_err();
        assert!(
            err.to_string().to_lowercase().contains("declin"),
            "refusal must surface as a clear error: {err}"
        );
    }

    #[tokio::test]
    async fn complete_at_surfaces_api_errors() {
        let (base, _) = spawn_claude_mock(
            400,
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"bad model"}}"#,
        )
        .await;
        let client = reqwest::Client::new();
        let err = complete_at(&client, &base, &test_cfg(), "s", "p", 64)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("bad model"), "got: {err}");
    }

    // ---- wiring ----

    #[test]
    fn ai_routes_are_wired() {
        let routes_src = include_str!("routes.rs");
        for route in [
            "\"/api/ai/status\"",
            "\"/api/ai/summarize\"",
            "\"/api/ai/draft\"",
        ] {
            assert!(
                routes_src.contains(route),
                "router chain must expose {route}"
            );
        }
    }
}
