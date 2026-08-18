//! AI assist (kata 6rhw): thread summarization and reply drafting via the
//! Claude API (`POST /v1/messages`, raw HTTP — no official Rust SDK).
//!
//! Optional feature: enabled only when `ANTHROPIC_API_KEY` is set in the
//! environment (never stored in config). The frontend reads
//! `GET /api/ai/status` and hides every affordance when disabled.

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
        let email = mk_email("Q3 planning", Some("Let's meet Tuesday.\n> older quoted reply"), None);
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
            post(
                move |headers: axum::http::HeaderMap, req_body: String| {
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
                },
            ),
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
            assert!(routes_src.contains(route), "router chain must expose {route}");
        }
    }
}
