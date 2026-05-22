// Cross-provider pure utilities.
//
// Helpers that two or more providers need verbatim live here so a tweak in
// one place can't drift between them. Cache structs, mutation body
// builders, payload parsers, and protocol-specific error classifiers stay
// in the provider modules — those encode provider differences.
//
// Today: Gmail (src/gmail.rs) and the Phase 4 Outlook email provider
// (src/outlook.rs) both import from here.

use crate::error::Error;

// =============================================================================
// Upload cache size caps — shared by Gmail and Outlook synthetic-blob caches.
// One tuning point so the two providers can't silently drift apart.
// =============================================================================

/// Maximum number of buffered uploads per session before further uploads
/// fail with `BadRequest`. Keeps memory bounded if a client misbehaves.
pub const UPLOAD_CACHE_CAP: usize = 32;

/// Per-attachment upper bound. Gmail's `messages.send` rejects RFC822 over
/// ~25 MiB; Graph accepts larger but we cap symmetrically so the two
/// providers behave identically from the user's perspective. Failing fast
/// at upload beats constructing a doomed send payload.
pub const MAX_BLOB_BYTES: usize = 25 * 1024 * 1024;

/// Aggregate per-session cap. Pins RAM at 50 MiB worst-case.
pub const MAX_UPLOAD_CACHE_BYTES: usize = 50 * 1024 * 1024;

// =============================================================================
// Pure helpers
// =============================================================================

/// Decide whether an OAuth refresh-token failure should evict the stored
/// tokens. Returns true only on 4xx + body containing `"invalid_grant"`.
/// Other failures (5xx, network, malformed request, generic 401 without
/// invalid_grant) preserve the tokens, because clearing on transient
/// trouble would force a re-OAuth dance on every provider blip.
///
/// Identical for Gmail and Outlook — both Google and Microsoft Graph
/// surface `invalid_grant` on revoked or expired refresh tokens.
pub fn should_clear_tokens_on_refresh_failure(status: reqwest::StatusCode, body: &str) -> bool {
    status.is_client_error() && body.contains("invalid_grant")
}

/// Percent-encode a string for safe interpolation as a single URL path
/// segment. Encodes everything that isn't RFC 3986 unreserved
/// (`A-Za-z0-9-._~`). Provider message/attachment IDs are subsets of
/// unreserved, so this is a no-op for real IDs; it's defense-in-depth
/// against future untrusted-input flows where a client-controlled string
/// might reach a URL builder.
pub fn encode_path_segment(s: &str) -> String {
    use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};
    // RFC 3986 path segment: pchar = unreserved / pct-encoded / sub-delims / ":" / "@"
    // We're stricter — only allow unreserved.
    const PATH_SEG: &AsciiSet = &CONTROLS
        .add(b' ')
        .add(b'"')
        .add(b'<')
        .add(b'>')
        .add(b'`')
        .add(b'#')
        .add(b'?')
        .add(b'{')
        .add(b'}')
        .add(b'/')
        .add(b'%')
        .add(b'&')
        .add(b'=')
        .add(b'+')
        .add(b':')
        .add(b'@')
        .add(b';')
        .add(b',')
        .add(b'$');
    utf8_percent_encode(s, PATH_SEG).to_string()
}

/// Best-effort MIME type from a filename extension. Used by attachment
/// download paths when the provider doesn't return a usable Content-Type
/// (Gmail's `messages.attachments.get` returns only base64 bytes).
///
/// Falls back to `application/octet-stream` for unknown extensions, which
/// browsers treat as a download (with the URL path's filename).
pub fn mime_type_from_filename(filename: &str) -> &'static str {
    let lower = filename.to_ascii_lowercase();
    let ext = match lower.rsplit_once('.') {
        Some((_, ext)) => ext,
        None => return "application/octet-stream",
    };
    match ext {
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "heic" => "image/heic",
        "txt" | "log" | "md" => "text/plain",
        "html" | "htm" => "text/html",
        "csv" => "text/csv",
        "ics" => "text/calendar",
        "json" => "application/json",
        "xml" => "application/xml",
        "zip" => "application/zip",
        "gz" | "tgz" => "application/gzip",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "ppt" => "application/vnd.ms-powerpoint",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "mp4" => "video/mp4",
        "mov" => "video/quicktime",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        _ => "application/octet-stream",
    }
}

// Suppress unused warning until Outlook starts using Error directly in this
// module's tests; the function is exposed for cross-provider use.
#[allow(dead_code)]
fn _force_error_in_scope() -> Error {
    Error::Internal("never called".into())
}

// =============================================================================
// Tests — moved verbatim from src/gmail.rs::tests when these helpers lived
// there. The behavior contract is identical; the test names match for git
// blame continuity.
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ---- should_clear_tokens_on_refresh_failure ----

    #[test]
    fn should_clear_tokens_on_invalid_grant() {
        let s = reqwest::StatusCode::BAD_REQUEST;
        let body =
            r#"{"error":"invalid_grant","error_description":"Token has been expired or revoked."}"#;
        assert!(should_clear_tokens_on_refresh_failure(s, body));
    }

    #[test]
    fn should_clear_tokens_on_invalid_grant_401() {
        let s = reqwest::StatusCode::UNAUTHORIZED;
        let body = r#"{"error":"invalid_grant"}"#;
        assert!(should_clear_tokens_on_refresh_failure(s, body));
    }

    #[test]
    fn should_not_clear_on_transient_5xx() {
        let s = reqwest::StatusCode::INTERNAL_SERVER_ERROR;
        let body = "provider internal error";
        assert!(!should_clear_tokens_on_refresh_failure(s, body));
    }

    #[test]
    fn should_not_clear_on_4xx_without_invalid_grant() {
        let s = reqwest::StatusCode::BAD_REQUEST;
        let body = r#"{"error":"invalid_request"}"#;
        assert!(!should_clear_tokens_on_refresh_failure(s, body));
    }

    #[test]
    fn should_not_clear_on_empty_body() {
        let s = reqwest::StatusCode::BAD_REQUEST;
        assert!(!should_clear_tokens_on_refresh_failure(s, ""));
    }

    // ---- encode_path_segment ----

    #[test]
    fn encode_path_segment_passes_alphanumeric_through() {
        assert_eq!(encode_path_segment("190abc-DEF_xyz"), "190abc-DEF_xyz");
    }

    #[test]
    fn encode_path_segment_encodes_slash() {
        assert_eq!(encode_path_segment("a/b"), "a%2Fb");
    }

    #[test]
    fn encode_path_segment_encodes_query_and_fragment_chars() {
        assert_eq!(encode_path_segment("a?b"), "a%3Fb");
        assert_eq!(encode_path_segment("a#b"), "a%23b");
        assert_eq!(encode_path_segment("a&b"), "a%26b");
    }

    #[test]
    fn encode_path_segment_encodes_path_traversal_separators() {
        let encoded = encode_path_segment("../etc/passwd");
        assert!(!encoded.contains('/'));
        assert!(encoded.contains("%2F"));
    }

    // ---- mime_type_from_filename ----

    #[test]
    fn mime_pdf() {
        assert_eq!(mime_type_from_filename("report.pdf"), "application/pdf");
    }

    #[test]
    fn mime_case_insensitive_extension() {
        assert_eq!(mime_type_from_filename("PHOTO.JPG"), "image/jpeg");
        assert_eq!(mime_type_from_filename("Doc.PDF"), "application/pdf");
    }

    #[test]
    fn mime_jpeg_both_extensions() {
        assert_eq!(mime_type_from_filename("a.jpg"), "image/jpeg");
        assert_eq!(mime_type_from_filename("b.jpeg"), "image/jpeg");
    }

    #[test]
    fn mime_calendar() {
        assert_eq!(mime_type_from_filename("invite.ics"), "text/calendar");
    }

    #[test]
    fn mime_office_docx() {
        assert_eq!(
            mime_type_from_filename("contract.docx"),
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
        );
    }

    #[test]
    fn mime_unknown_extension_falls_back_to_octet_stream() {
        assert_eq!(
            mime_type_from_filename("mystery.xyzfoo"),
            "application/octet-stream"
        );
    }

    #[test]
    fn mime_no_extension_falls_back_to_octet_stream() {
        assert_eq!(
            mime_type_from_filename("README"),
            "application/octet-stream"
        );
    }

    #[test]
    fn mime_dot_at_start_treated_as_extension() {
        assert_eq!(
            mime_type_from_filename(".bashrc"),
            "application/octet-stream"
        );
    }

    #[test]
    fn mime_double_extension_uses_last() {
        assert_eq!(mime_type_from_filename("backup.tar.gz"), "application/gzip");
    }

    #[test]
    fn mime_common_image_and_av_extensions() {
        assert_eq!(mime_type_from_filename("a.svg"), "image/svg+xml");
        assert_eq!(mime_type_from_filename("a.webp"), "image/webp");
        assert_eq!(mime_type_from_filename("a.heic"), "image/heic");
        assert_eq!(mime_type_from_filename("a.mp4"), "video/mp4");
        assert_eq!(mime_type_from_filename("a.mov"), "video/quicktime");
        assert_eq!(mime_type_from_filename("a.mp3"), "audio/mpeg");
        assert_eq!(mime_type_from_filename("a.wav"), "audio/wav");
    }

    #[test]
    fn mime_common_text_and_data_extensions() {
        assert_eq!(mime_type_from_filename("a.csv"), "text/csv");
        assert_eq!(mime_type_from_filename("a.xml"), "application/xml");
        assert_eq!(mime_type_from_filename("a.zip"), "application/zip");
        assert_eq!(mime_type_from_filename("a.tgz"), "application/gzip");
        assert_eq!(mime_type_from_filename("a.json"), "application/json");
    }
}
