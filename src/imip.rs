//! iMIP (RFC 6047 / RFC 2447) MIME envelope for calendar invites.
//!
//! Shared by `gmail.rs` and `outlook.rs` so invites from Gmail and Outlook
//! accounts go out with the exact same on-the-wire MIME shape —
//! `multipart/alternative` carrying `text/plain` and
//! `text/calendar; method=<METHOD>` (base64 CTE), wrapped in
//! `multipart/mixed` when there are attachments. This mirrors Google
//! Calendar's own invite shape (kata zs8n): before this module both
//! providers' `send_email` silently dropped `EmailSubmission.calendar_ics`,
//! so `/api/calendar/invite?account=gmail|outlook` produced a bare
//! `text/plain` message with no processable `text/calendar` part.
//!
//! Only `jmap.rs` implemented `calendar_ics` (via `build_draft_email` for
//! REQUEST and `build_itip_reply_mime` for REPLY); those paths are
//! unaffected. This module is the Gmail/Outlook equivalent of
//! `build_itip_reply_mime`, generalized to REQUEST invites with cc/bcc and
//! attachments (the shape `send_invite_handler` produces).
//!
//! Empirical caveat (kata wca3, probes A2/A3): at least one Exchange Online
//! tenant strips hand-rolled `multipart/alternative` iMIP from unfamiliar
//! senders, delivering it as bare `text/plain` with no meeting request —
//! tenant-level behavior outside our control. Native Google/Fastmail invites
//! are processed fine. The exact MIME shape is pinned by the unit tests
//! below so drift is caught locally, but end-to-end delivery to a real
//! recipient (not just the Sent copy) still needs live verification per
//! provider/tenant — see the kata zs8n commit message.

use base64::Engine;

/// Fixed `multipart/alternative` boundary. Safe because both body parts are
/// base64-encoded (alphabet `[A-Za-z0-9+/=]` contains no `-`), so no base64
/// output line can collide with the `--<boundary>` delimiter. A fixed value
/// keeps the MIME shape deterministic for the pinning tests.
const ALT_BOUNDARY: &str = "supervillain-imip-alt";

/// Fixed `multipart/mixed` boundary (attachments present). Same safety
/// property as `ALT_BOUNDARY`: every part is base64, so no `-`-bearing
/// delimiter can appear in the content.
const MIXED_BOUNDARY: &str = "supervillain-imip-mixed";

/// Header-injection guard for values interpolated into the client-built
/// RFC 5322 message: CR/LF must not reach a header line. Mirrors
/// `jmap.rs::strip_crlf`.
fn strip_crlf(s: &str) -> String {
    s.replace(['\r', '\n'], "")
}

/// base64 (standard, with padding) wrapped at 76 chars per RFC 2045 §6.8.
/// Mirrors `jmap.rs::base64_mime_lines` so the two providers can't drift.
fn base64_mime_lines(data: &[u8]) -> String {
    let b64 = base64::engine::general_purpose::STANDARD.encode(data);
    b64.as_bytes()
        .chunks(76)
        .map(|c| std::str::from_utf8(c).expect("base64 output is ASCII"))
        .collect::<Vec<_>>()
        .join("\r\n")
}

/// RFC 2047 B-encode a Subject that contains non-ASCII or is too long for a
/// single header line; short ASCII subjects pass through bare. Mirrors
/// `jmap.rs::encode_subject` (event summaries are user text —
/// "Réunion d'équipe" — and the chunking doubles as line folding).
fn encode_subject(raw: &str) -> String {
    let s = strip_crlf(raw);
    if s.is_ascii() && s.len() + "Subject: ".len() <= 78 {
        return s;
    }
    fn encoded_word(chunk: &str) -> String {
        format!(
            "=?UTF-8?B?{}?=",
            base64::engine::general_purpose::STANDARD.encode(chunk.as_bytes())
        )
    }
    let mut words: Vec<String> = Vec::new();
    let mut chunk = String::new();
    for ch in s.chars() {
        if chunk.len() + ch.len_utf8() > 42 {
            words.push(encoded_word(&chunk));
            chunk.clear();
        }
        chunk.push(ch);
    }
    if !chunk.is_empty() {
        words.push(encoded_word(&chunk));
    }
    words.join("\r\n ")
}

/// True if `c` is RFC 5322 `atext` (atom text) plus space — the chars that
/// may appear bare in a display name without quoting. Space is included so
/// a plain `First Last` stays unquoted (matching `mail_builder`'s output and
/// common MTA practice); only real specials (`(`,`,`,`"`,…) trigger quoting.
fn is_atext(c: char) -> bool {
    matches!(
        c,
        'A'..='Z'
            | 'a'..='z'
            | '0'..='9'
            | '!'
            | '#'
            | '$'
            | '%'
            | '&'
            | '\''
            | '*'
            | '+'
            | '-'
            | '/'
            | '='
            | '?'
            | '^'
            | '_'
            | '`'
            | '{'
            | '|'
            | '}'
            | '~'
    ) || c == ' '
}

/// RFC 2045 `tspecials` — the chars that force quoting of a Content-Type /
/// Content-Disposition parameter token (`(` `)` `<` `>` `@` `,` `;` `:` `\`
/// `"` `/` `[` `]` `?` `=`). Notably `.` and `-` are NOT tspecials, so a
/// filename like `report.pdf` stays unquoted.
fn is_tspecial(c: char) -> bool {
    matches!(
        c,
        '(' | ')' | '<' | '>' | '@' | ',' | ';' | ':' | '\\' | '"' | '/' | '[' | ']' | '?' | '='
    )
}

/// Wrap `s` in `"..."` (escaping `"` and `\`) — the shared quoting core.
fn quote_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// RFC 5322-quote a display name when it contains any char that isn't
/// bare-legal `atext`. `First Last` passes through unquoted; `O'Brien, Jr.`
/// (comma) gets quoted.
fn quote_if_needed(s: &str) -> String {
    if !s.is_empty() && s.chars().all(is_atext) {
        return s.to_string();
    }
    quote_string(s)
}

/// RFC 2045-quote a Content-Type/Content-Disposition parameter token
/// (`name`/`filename`) when it contains `tspecials`, control chars, space,
/// or non-ASCII. `.`-bearing filenames like `report.pdf` stay unquoted.
fn quote_param_token(s: &str) -> String {
    let bare = !s.is_empty()
        && s.chars()
            .all(|c| !is_tspecial(c) && !c.is_control() && c.is_ascii() && c != ' ');
    if bare {
        return s.to_string();
    }
    quote_string(s)
}

/// Validate an attachment Content-Type before it reaches a hand-built MIME
/// header. CR/LF is stripped first as a direct injection guard, then the value
/// must be a short RFC 2045 type/subtype token; malformed client input falls
/// back to a safe generic type.
pub(crate) fn sanitize_attachment_mime_type(raw: &str) -> String {
    let value = strip_crlf(raw);
    let valid_token = |part: &str| {
        !part.is_empty()
            && part
                .chars()
                .all(|c| c.is_ascii() && !c.is_control() && c != ' ' && !is_tspecial(c))
    };
    let valid = value
        .split_once('/')
        .is_some_and(|(top, sub)| !sub.contains('/') && valid_token(top) && valid_token(sub));
    if valid && value.len() <= 60 {
        value
    } else {
        "application/octet-stream".to_string()
    }
}

fn sanitize_attachment_name(raw: &str) -> String {
    let clean: String = strip_crlf(raw)
        .chars()
        .filter(|c| !c.is_control())
        .collect();
    if clean.is_empty() {
        "attachment".to_string()
    } else {
        clean
    }
}

fn attachment_name_fallback(name: &str) -> String {
    name.rsplit_once('.')
        .map(|(_, extension)| extension)
        .filter(|extension| {
            !extension.is_empty()
                && extension.len() <= 16
                && extension
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        })
        .map(|extension| format!("attachment.{extension}"))
        .unwrap_or_else(|| "attachment".to_string())
}

fn is_rfc2231_attr_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#' | b'$' | b'&' | b'+' | b'-' | b'.' | b'^' | b'_' | b'`' | b'|' | b'~'
        )
}

/// Percent-encode UTF-8 bytes into RFC 2231 continuation chunks without ever
/// splitting a `%XX` escape. The conservative chunk width leaves ample room
/// for ` filename*NN*=` and keeps every folded header line below 78 chars.
fn rfc2231_chunks(value: &str) -> Vec<String> {
    const CHUNK_LEN: usize = 42;
    let mut chunks = Vec::new();
    let mut chunk = String::new();
    for byte in value.bytes() {
        let unit = if is_rfc2231_attr_char(byte) {
            char::from(byte).to_string()
        } else {
            format!("%{byte:02X}")
        };
        if !chunk.is_empty() && chunk.len() + unit.len() > CHUNK_LEN {
            chunks.push(std::mem::take(&mut chunk));
        }
        chunk.push_str(&unit);
    }
    if !chunk.is_empty() {
        chunks.push(chunk);
    }
    chunks
}

/// Emit a MIME header with a filename-like parameter. Short ASCII values keep
/// the ordinary `name=` / `filename=` form. Non-ASCII or long values get an
/// ASCII fallback plus RFC 2231 UTF-8 extended parameter; long encoded values
/// use numbered continuations, folded so physical lines stay within 78 chars.
pub(crate) fn push_attachment_parameter_header(
    out: &mut String,
    header_value: &str,
    parameter: &str,
    raw_value: &str,
) {
    let value = sanitize_attachment_name(raw_value);
    let plain = format!("{parameter}={}", quote_param_token(&value));
    if value.is_ascii() && header_value.len() + 2 + plain.len() <= 78 {
        out.push_str(&format!("{header_value}; {plain}\r\n"));
        return;
    }

    let fallback = attachment_name_fallback(&value);
    out.push_str(&format!(
        "{header_value};\r\n {parameter}={};\r\n",
        quote_param_token(&fallback)
    ));
    let chunks = rfc2231_chunks(&value);
    if chunks.len() == 1 {
        out.push_str(&format!(
            " {parameter}*=UTF-8''{}\r\n",
            chunks
                .first()
                .expect("sanitized attachment name is non-empty")
        ));
        return;
    }
    for (index, chunk) in chunks.iter().enumerate() {
        let charset = if index == 0 { "UTF-8''" } else { "" };
        let separator = if index + 1 == chunks.len() {
            "\r\n"
        } else {
            ";\r\n"
        };
        out.push_str(&format!(
            " {parameter}*{index}*={charset}{chunk}{separator}"
        ));
    }
}

/// Format an RFC 5322 `mailbox` (`Name <addr>` or bare `addr`). The display
/// name is header-injection-guarded and quoted when it contains special
/// chars. Recipient lists (`to`/`cc`/`bcc`) arrive as bare email strings, so
/// only the From line exercises the name branch in practice.
fn format_mailbox(name: Option<&str>, email: &str) -> String {
    let email = strip_crlf(email);
    match name.map(str::trim).filter(|n| !n.is_empty()) {
        Some(n) => format!("{} <{}>", quote_if_needed(&strip_crlf(n)), email),
        None => email,
    }
}

/// Comma-join a recipient list as bare mailboxes.
fn format_recipient_list(addrs: &[String]) -> String {
    addrs
        .iter()
        .map(|a| format_mailbox(None, a))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Extract the iCalendar `METHOD` property value (e.g. `REQUEST`, `REPLY`)
/// from the ICS body, case-insensitively. RFC 2447 §2.1 requires the MIME
/// `text/calendar` `method` parameter to match this value. Defaults to
/// `REQUEST` when absent — every invite this app sends (`send_invite_handler`
/// via `calendar::generate_invite`) carries `METHOD:REQUEST`, and REQUEST is
/// the only iMIP method a brand-new invite can carry.
pub(crate) fn extract_ics_method(ics: &str) -> String {
    for line in ics.lines() {
        let trimmed = line.trim();
        if let Some((key, value)) = trimmed.split_once(':')
            && key.eq_ignore_ascii_case("METHOD")
            && !value.trim().is_empty()
        {
            return value.trim().to_string();
        }
    }
    "REQUEST".to_string()
}

/// The domain portion of `addr` (after `@`), falling back to a sentinel so
/// the generated `Message-ID` is always well-formed even for a malformed
/// from address.
fn message_id_domain(addr: &str) -> &str {
    match addr.rsplit_once('@') {
        Some((_, dom)) if !dom.is_empty() => dom,
        _ => "supervillain.local",
    }
}

/// A resolved attachment: `(name, mime_type, bytes)`.
pub(crate) type ImipAttachment = (String, String, Vec<u8>);

/// Build the complete RFC 5322 + iMIP MIME message for a calendar invite.
///
/// Pure — no I/O, no clock dependence beyond `Utc::now()` for `Date:` (which
/// the pinning tests assert by presence, not by value). Returns raw bytes
/// ready for the provider's send path:
///   - Gmail: base64url-encode → `messages.send` `raw` field.
///   - Outlook: base64 (standard) → Graph `sendMail` with
///     `Content-Type: text/plain` (the MIME-mode request shape).
///
/// Shape (mirrors Google Calendar's own invites):
/// ```text
/// // no attachments:
/// multipart/alternative
///   ├── text/plain; charset=utf-8   (base64)
///   └── text/calendar; method=<METHOD>; charset=utf-8   (base64)
///
/// // with attachments:
/// multipart/mixed
///   ├── multipart/alternative  (text/plain + text/calendar, as above)
///   └── <attachment>…          (base64, Content-Disposition: attachment)
/// ```
///
/// `Date:` and `Message-ID:` are generated here so the message is valid
/// RFC 5322 on its own; Gmail's MTA and Graph both accept (and may replace)
/// them. `in_reply_to`/`references` are deliberately not handled — the only
/// `calendar_ics` producer (`send_invite_handler`) composes a new message,
/// never a reply.
//
// Nine args is the natural fan-out of an RFC 5322 envelope (From/To/Cc/Bcc/
// Subject + text + ICS + attachments); bundling them into a struct would
// just move the verbosity to every caller. Matches the `walk_payload`
// precedent in gmail.rs.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_imip_mime(
    from_addr: &str,
    from_display_name: Option<&str>,
    to: &[String],
    cc: &[String],
    bcc: Option<&[String]>,
    subject: &str,
    text_body: &str,
    calendar_ics: &str,
    attachments: &[ImipAttachment],
) -> Vec<u8> {
    let method = extract_ics_method(calendar_ics);
    let from = format_mailbox(from_display_name, from_addr);
    let to_line = format_recipient_list(to);

    // Header block.
    let mut out = String::with_capacity(2048);
    out.push_str(&format!("From: {from}\r\n"));
    out.push_str(&format!("To: {to_line}\r\n"));
    if !cc.is_empty() {
        out.push_str(&format!("Cc: {}\r\n", format_recipient_list(cc)));
    }
    if let Some(bcc) = bcc
        && !bcc.is_empty()
    {
        // Bcc: header drives envelope delivery for raw-MIME sends (Gmail
        // `messages.send`, Graph MIME-mode sendMail derive recipients from
        // the headers); the sending MTA strips it before delivery to To/Cc.
        out.push_str(&format!("Bcc: {}\r\n", format_recipient_list(bcc)));
    }
    out.push_str(&format!("Subject: {}\r\n", encode_subject(subject)));
    out.push_str(&format!(
        "Message-ID: <{}@{}>\r\n",
        uuid::Uuid::new_v4(),
        message_id_domain(from_addr)
    ));
    out.push_str(&format!("Date: {}\r\n", chrono::Utc::now().to_rfc2822()));
    out.push_str("MIME-Version: 1.0\r\n");

    // Body: build the alternative subtree once, then either emit it bare or
    // nest it under multipart/mixed with the attachments.
    let alt = build_alternative_body(text_body, calendar_ics, &method);

    if attachments.is_empty() {
        out.push_str(&format!(
            "Content-Type: multipart/alternative; boundary=\"{ALT_BOUNDARY}\"\r\n\r\n"
        ));
        out.push_str(&alt);
    } else {
        out.push_str(&format!(
            "Content-Type: multipart/mixed; boundary=\"{MIXED_BOUNDARY}\"\r\n\r\n"
        ));
        out.push_str(&format!("--{MIXED_BOUNDARY}\r\n"));
        out.push_str(&format!(
            "Content-Type: multipart/alternative; boundary=\"{ALT_BOUNDARY}\"\r\n\r\n"
        ));
        out.push_str(&alt);
        for (name, mime, bytes) in attachments {
            out.push_str(&format!("--{MIXED_BOUNDARY}\r\n"));
            let mime = sanitize_attachment_mime_type(mime);
            push_attachment_parameter_header(
                &mut out,
                &format!("Content-Type: {mime}"),
                "name",
                name,
            );
            out.push_str("Content-Transfer-Encoding: base64\r\n");
            push_attachment_parameter_header(
                &mut out,
                "Content-Disposition: attachment",
                "filename",
                name,
            );
            out.push_str("\r\n");
            out.push_str(&base64_mime_lines(bytes));
            out.push_str("\r\n");
        }
        out.push_str(&format!("--{MIXED_BOUNDARY}--\r\n"));
    }

    out.into_bytes()
}

/// Build an invite message that carries the ICS as an `application/ics`
/// ATTACHMENT instead of an inline `text/calendar` alternative part.
///
/// Why this exists (kata zs8n, live probes 2026-08-15): Gmail's outbound
/// pipeline strips hand-rolled inline `text/calendar` parts from
/// `messages.send` raw sends — any CTE (base64 or 7bit), with or without
/// the `method=` Content-Type parameter — flattening the delivered message
/// to bare `text/plain` before DKIM-signing (verified by matching
/// Message-IDs: the Sent copy keeps the part, the delivered copy does not).
/// An `application/ics` attachment, by contrast, makes Gmail *regenerate*
/// its canonical invite MIME on the wire (multipart/alternative with
/// `text/calendar; method=REQUEST` + the ics attachment) with the ICS
/// bytes preserved verbatim — so this shape is the only one that delivers
/// a processable invite from the Gmail API.
///
/// Shape:
/// ```text
/// multipart/mixed
///   ├── text/plain; charset=utf-8   (base64)
///   ├── invite.ics                  (application/ics, base64, attachment)
///   └── <user attachment>…          (base64, attachment)
/// ```
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_ics_attachment_mime(
    from_addr: &str,
    from_display_name: Option<&str>,
    to: &[String],
    cc: &[String],
    bcc: Option<&[String]>,
    subject: &str,
    text_body: &str,
    calendar_ics: &str,
    attachments: &[ImipAttachment],
) -> Vec<u8> {
    let from = format_mailbox(from_display_name, from_addr);
    let to_line = format_recipient_list(to);

    let mut out = String::with_capacity(2048);
    out.push_str(&format!("From: {from}\r\n"));
    out.push_str(&format!("To: {to_line}\r\n"));
    if !cc.is_empty() {
        out.push_str(&format!("Cc: {}\r\n", format_recipient_list(cc)));
    }
    if let Some(bcc) = bcc
        && !bcc.is_empty()
    {
        out.push_str(&format!("Bcc: {}\r\n", format_recipient_list(bcc)));
    }
    out.push_str(&format!("Subject: {}\r\n", encode_subject(subject)));
    out.push_str(&format!(
        "Message-ID: <{}@{}>\r\n",
        uuid::Uuid::new_v4(),
        message_id_domain(from_addr)
    ));
    out.push_str(&format!("Date: {}\r\n", chrono::Utc::now().to_rfc2822()));
    out.push_str("MIME-Version: 1.0\r\n");
    out.push_str(&format!(
        "Content-Type: multipart/mixed; boundary=\"{MIXED_BOUNDARY}\"\r\n\r\n"
    ));

    out.push_str(&format!("--{MIXED_BOUNDARY}\r\n"));
    out.push_str("Content-Type: text/plain; charset=utf-8\r\n");
    out.push_str("Content-Transfer-Encoding: base64\r\n\r\n");
    out.push_str(&base64_mime_lines(text_body.as_bytes()));
    out.push_str("\r\n");

    let ics_att: ImipAttachment = (
        "invite.ics".into(),
        "application/ics".into(),
        calendar_ics.as_bytes().to_vec(),
    );
    for (name, mime, bytes) in std::iter::once(&ics_att).chain(attachments) {
        out.push_str(&format!("--{MIXED_BOUNDARY}\r\n"));
        let mime = sanitize_attachment_mime_type(mime);
        push_attachment_parameter_header(&mut out, &format!("Content-Type: {mime}"), "name", name);
        out.push_str("Content-Transfer-Encoding: base64\r\n");
        push_attachment_parameter_header(
            &mut out,
            "Content-Disposition: attachment",
            "filename",
            name,
        );
        out.push_str("\r\n");
        out.push_str(&base64_mime_lines(bytes));
        out.push_str("\r\n");
    }
    out.push_str(&format!("--{MIXED_BOUNDARY}--\r\n"));

    out.into_bytes()
}

/// Build the `multipart/alternative` subtree (without the wrapping
/// `Content-Type` header — the caller emits that so it can scope the
/// boundary to the right container). Includes the closing `--boundary--`.
fn build_alternative_body(text_body: &str, calendar_ics: &str, method: &str) -> String {
    let mut out = String::with_capacity(1024);
    out.push_str(&format!("--{ALT_BOUNDARY}\r\n"));
    out.push_str("Content-Type: text/plain; charset=utf-8\r\n");
    out.push_str("Content-Transfer-Encoding: base64\r\n\r\n");
    out.push_str(&base64_mime_lines(text_body.as_bytes()));
    out.push_str("\r\n");
    out.push_str(&format!("--{ALT_BOUNDARY}\r\n"));
    out.push_str(&format!(
        "Content-Type: text/calendar; method={method}; charset=utf-8\r\n"
    ));
    out.push_str("Content-Transfer-Encoding: base64\r\n\r\n");
    out.push_str(&base64_mime_lines(calendar_ics.as_bytes()));
    out.push_str("\r\n");
    out.push_str(&format!("--{ALT_BOUNDARY}--\r\n"));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- build_ics_attachment_mime (Gmail shape) ----

    #[test]
    fn ics_attachment_mime_is_mixed_with_ics_attachment_and_no_inline_calendar() {
        let raw = build_ics_attachment_mime(
            "boss@example.com",
            None,
            &["guest@example.com".into()],
            &[],
            None,
            "Standup",
            "See you there",
            sample_ics(),
            &[],
        );
        let s = decode_mime_body(&raw);
        assert!(s.contains("Content-Type: multipart/mixed; boundary=\"supervillain-imip-mixed\""));
        assert!(s.contains("Content-Type: text/plain; charset=utf-8"));
        assert!(s.contains("Content-Type: application/ics; name=invite.ics"));
        assert!(s.contains("Content-Disposition: attachment; filename=invite.ics"));
        assert!(
            !s.contains("text/calendar"),
            "Gmail strips inline text/calendar — this builder must never emit one: {s}"
        );
        use base64::Engine;
        let ics_b64 = base64::engine::general_purpose::STANDARD.encode(sample_ics().as_bytes());
        assert!(
            s.contains(&ics_b64[..76]),
            "ICS bytes must ride in the attachment verbatim (base64): {s}"
        );
    }

    #[test]
    fn ics_attachment_mime_puts_invite_ics_before_user_attachments() {
        let raw = build_ics_attachment_mime(
            "boss@example.com",
            Some("The Boss"),
            &["guest@example.com".into()],
            &["observer@example.com".into()],
            None,
            "Standup",
            "body",
            sample_ics(),
            &[pdf_attachment()],
        );
        let s = decode_mime_body(&raw);
        assert!(s.contains("Cc: observer@example.com"));
        let ics_idx = s.find("name=invite.ics").expect("invite.ics present");
        let pdf_idx = s.find("report.pdf").expect("user attachment present");
        assert!(
            ics_idx < pdf_idx,
            "invite.ics leads the attachment list so Gmail's rewrite sees it first"
        );
        assert!(s.ends_with("--supervillain-imip-mixed--\r\n"));
    }

    fn sample_ics() -> &'static str {
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nMETHOD:REQUEST\r\n\
         BEGIN:VEVENT\r\nUID:abc@supervillain\r\nSUMMARY:Team Standup\r\n\
         DTSTART:20260801T100000Z\r\nDTEND:20260801T100000Z\r\n\
         ORGANIZER:mailto:boss@example.com\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
    }

    fn decode_mime_body(raw: &[u8]) -> String {
        String::from_utf8_lossy(raw).to_string()
    }

    // ---- extract_ics_method ----

    #[test]
    fn extract_method_request() {
        assert_eq!(extract_ics_method(sample_ics()), "REQUEST");
    }

    #[test]
    fn extract_method_reply() {
        let ics =
            "BEGIN:VCALENDAR\r\nMETHOD:REPLY\r\nBEGIN:VEVENT\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        assert_eq!(extract_ics_method(ics), "REPLY");
    }

    #[test]
    fn extract_method_case_insensitive_key() {
        for key in ["method", "Method", "mEtHoD"] {
            let ics = format!("BEGIN:VCALENDAR\r\n{key}:request\r\nEND:VCALENDAR\r\n");
            assert_eq!(extract_ics_method(&ics), "request");
        }
    }

    #[test]
    fn extract_method_defaults_to_request_when_absent() {
        let ics =
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        assert_eq!(extract_ics_method(ics), "REQUEST");
    }

    #[test]
    fn extract_method_ignores_method_inside_property_params() {
        // A METHOD:VALUE inside a property parameter (not a standalone
        // METHOD: property line) must not fool the line-anchored scan.
        let ics = "BEGIN:VCALENDAR\r\nX-FOO;METHOD=REQUEST:bar\r\nEND:VCALENDAR\r\n";
        assert_eq!(
            extract_ics_method(ics),
            "REQUEST",
            "defaults, no METHOD: line"
        );
    }

    // ---- MIME shape: no attachments ----

    #[test]
    fn invite_mime_no_attachments_is_multipart_alternative() {
        let raw = build_imip_mime(
            "boss@example.com",
            None,
            &["guest@example.com".into()],
            &[],
            None,
            "Team Standup",
            "You're invited",
            sample_ics(),
            &[],
        );
        let s = decode_mime_body(&raw);
        assert!(
            s.contains("Content-Type: multipart/alternative; boundary=\"supervillain-imip-alt\""),
            "no-attachment invite must be multipart/alternative: {s}"
        );
        assert!(
            !s.contains("multipart/mixed"),
            "no-attachment invite must NOT wrap in multipart/mixed: {s}"
        );
    }

    #[test]
    fn invite_mime_has_required_headers() {
        let raw = build_imip_mime(
            "boss@example.com",
            Some("Big Boss"),
            &["guest@example.com".into()],
            &["observer@example.com".into()],
            None,
            "Standup",
            "body",
            sample_ics(),
            &[],
        );
        let s = decode_mime_body(&raw);
        assert!(s.contains("From: Big Boss <boss@example.com>"));
        assert!(s.contains("To: guest@example.com"));
        assert!(s.contains("Cc: observer@example.com"));
        assert!(s.contains("Subject: Standup"));
        assert!(s.contains("MIME-Version: 1.0"));
        assert!(s.contains("Date: "));
        assert!(s.contains("Message-ID: <"));
        assert!(s.contains("@example.com>"));
    }

    #[test]
    fn invite_mime_calendar_part_carries_method_request_and_base64() {
        let ics = sample_ics();
        let raw = build_imip_mime(
            "boss@example.com",
            None,
            &["guest@example.com".into()],
            &[],
            None,
            "S",
            "body",
            ics,
            &[],
        );
        let s = decode_mime_body(&raw);
        assert!(
            s.contains("Content-Type: text/calendar; method=REQUEST; charset=utf-8"),
            "calendar part must declare method=REQUEST matching the ICS: {s}"
        );
        assert!(
            s.contains("Content-Transfer-Encoding: base64"),
            "calendar part must use base64 CTE: {s}"
        );
        // The ICS round-trips: decode the base64 calendar part and find METHOD:REQUEST.
        assert!(
            s.contains(&base64_mime_lines(ics.as_bytes())),
            "calendar part body must be the base64-encoded ICS: {s}"
        );
    }

    #[test]
    fn invite_mime_text_part_is_base64_plain() {
        let raw = build_imip_mime(
            "boss@example.com",
            None,
            &["guest@example.com".into()],
            &[],
            None,
            "S",
            "You're invited",
            sample_ics(),
            &[],
        );
        let s = decode_mime_body(&raw);
        assert!(s.contains("Content-Type: text/plain; charset=utf-8"));
        // The text body is base64-encoded in the MIME.
        assert!(
            s.contains(&base64_mime_lines("You're invited".as_bytes())),
            "text part body must be base64-encoded text_body: {s}"
        );
    }

    #[test]
    fn invite_mime_alternative_has_exactly_two_parts_and_closes() {
        let raw = build_imip_mime(
            "boss@example.com",
            None,
            &["guest@example.com".into()],
            &[],
            None,
            "S",
            "b",
            sample_ics(),
            &[],
        );
        let s = decode_mime_body(&raw);
        // Two opening delimiters (text + calendar) and one closing.
        assert_eq!(
            s.matches("--supervillain-imip-alt\r\n").count(),
            2,
            "two alternative parts: {s}"
        );
        assert!(
            s.contains("--supervillain-imip-alt--\r\n"),
            "alternative must close with --boundary--: {s}"
        );
    }

    #[test]
    fn invite_mime_bcc_header_present_when_supplied() {
        let raw = build_imip_mime(
            "boss@example.com",
            None,
            &["guest@example.com".into()],
            &[],
            Some(&["secret@example.com".into()]),
            "S",
            "b",
            sample_ics(),
            &[],
        );
        let s = decode_mime_body(&raw);
        assert!(s.contains("Bcc: secret@example.com"));
    }

    #[test]
    fn invite_mime_no_bcc_header_when_none() {
        let raw = build_imip_mime(
            "boss@example.com",
            None,
            &["guest@example.com".into()],
            &[],
            None,
            "S",
            "b",
            sample_ics(),
            &[],
        );
        let s = decode_mime_body(&raw);
        assert!(!s.contains("Bcc:"));
    }

    // ---- MIME shape: with attachments ----

    fn pdf_attachment() -> ImipAttachment {
        (
            "report.pdf".into(),
            "application/pdf".into(),
            b"%PDF-fake".to_vec(),
        )
    }

    #[test]
    fn invite_mime_with_attachments_wraps_in_multipart_mixed() {
        let raw = build_imip_mime(
            "boss@example.com",
            None,
            &["guest@example.com".into()],
            &[],
            None,
            "S",
            "b",
            sample_ics(),
            &[pdf_attachment()],
        );
        let s = decode_mime_body(&raw);
        assert!(
            s.contains("Content-Type: multipart/mixed; boundary=\"supervillain-imip-mixed\""),
            "attachment invite must wrap in multipart/mixed: {s}"
        );
        // The alternative subtree is nested inside the mixed container.
        assert!(
            s.contains("Content-Type: multipart/alternative; boundary=\"supervillain-imip-alt\""),
            "alternative must be nested inside mixed: {s}"
        );
        // Attachment part headers.
        assert!(s.contains("Content-Type: application/pdf; name=report.pdf"));
        assert!(s.contains("Content-Disposition: attachment; filename=report.pdf"));
        // Attachment body is base64 of the bytes.
        assert!(
            s.contains(&base64_mime_lines(b"%PDF-fake")),
            "attachment body must be base64-encoded bytes: {s}"
        );
        // Mixed closes.
        assert!(
            s.contains("--supervillain-imip-mixed--\r\n"),
            "mixed must close: {s}"
        );
    }

    #[test]
    fn invite_mime_multiple_attachments_each_get_their_own_part() {
        let atts = vec![
            pdf_attachment(),
            ("notes.txt".into(), "text/plain".into(), b"hello".to_vec()),
        ];
        let raw = build_imip_mime(
            "boss@example.com",
            None,
            &["guest@example.com".into()],
            &[],
            None,
            "S",
            "b",
            sample_ics(),
            &atts,
        );
        let s = decode_mime_body(&raw);
        assert!(s.contains("name=report.pdf"));
        assert!(s.contains("filename=notes.txt"));
        assert_eq!(
            s.matches("--supervillain-imip-mixed\r\n").count(),
            3,
            "1 alternative + 2 attachments = 3 mixed opening delimiters: {s}"
        );
    }

    #[test]
    fn invite_mime_attachment_headers_reject_crlf_injection() {
        let attachment = (
            "report.pdf\r\n--supervillain-imip-mixed\r\nBcc: evil@example.com".into(),
            "application/pdf\r\nBcc: evil@example.com".into(),
            b"data".to_vec(),
        );
        let raw = build_imip_mime(
            "boss@example.com",
            None,
            &["guest@example.com".into()],
            &[],
            None,
            "S",
            "b",
            sample_ics(),
            &[attachment],
        );
        let s = decode_mime_body(&raw);
        assert!(
            !s.contains("\r\nBcc: evil@example.com"),
            "attachment metadata must not inject a header: {s}"
        );
        assert_eq!(
            s.matches("--supervillain-imip-mixed\r\n").count(),
            2,
            "filename metadata must not inject a forged MIME boundary: {s}"
        );
        assert!(
            s.contains("Content-Type: application/octet-stream;"),
            "malformed MIME types must use the safe fallback: {s}"
        );
    }

    #[test]
    fn invite_mime_rfc2231_encodes_unicode_and_long_attachment_names() {
        for name in [
            "résumé.pdf".to_string(),
            format!("quarterly-{}-report.pdf", "planning".repeat(12)),
        ] {
            let attachment = (name.clone(), "application/pdf".into(), b"data".to_vec());
            let raw = build_imip_mime(
                "boss@example.com",
                None,
                &["guest@example.com".into()],
                &[],
                None,
                "S",
                "b",
                sample_ics(),
                &[attachment],
            );
            let s = decode_mime_body(&raw);
            assert!(
                s.contains("filename*=UTF-8''r%C3%A9sum%C3%A9.pdf")
                    || s.contains("filename*0*=UTF-8''quarterly-"),
                "real filename must use an RFC 2231 extended parameter: {s}"
            );
            assert!(
                !s.contains("résumé.pdf"),
                "raw non-ASCII must not appear in MIME headers: {s}"
            );
            for line in s.lines() {
                assert!(
                    line.len() <= 78,
                    "MIME line exceeds 78 chars ({}): {line:?}",
                    line.len()
                );
            }
        }
    }

    // ---- header injection + encoding guards ----

    #[test]
    fn invite_mime_strips_crlf_from_from_address() {
        let raw = build_imip_mime(
            "boss@example.com\r\nBcc: evil@example.com",
            None,
            &["guest@example.com".into()],
            &[],
            None,
            "S",
            "b",
            sample_ics(),
            &[],
        );
        let s = decode_mime_body(&raw);
        // CR/LF is stripped so the injected text collapses into the From
        // header value rather than starting a new Bcc header line.
        assert!(
            !s.contains("\r\nBcc:"),
            "CR/LF in from_addr must not inject a Bcc header line: {s}"
        );
    }

    #[test]
    fn invite_mime_quotes_display_name_with_special_chars() {
        let raw = build_imip_mime(
            "boss@example.com",
            Some("O'Brien, Jr."),
            &["guest@example.com".into()],
            &[],
            None,
            "S",
            "b",
            sample_ics(),
            &[],
        );
        let s = decode_mime_body(&raw);
        assert!(
            s.contains("From: \"O'Brien, Jr.\" <boss@example.com>"),
            "display name with comma must be quoted: {s}"
        );
    }

    #[test]
    fn invite_mime_display_name_with_space_stays_unquoted() {
        let raw = build_imip_mime(
            "boss@example.com",
            Some("Big Boss"),
            &["guest@example.com".into()],
            &[],
            None,
            "S",
            "b",
            sample_ics(),
            &[],
        );
        let s = decode_mime_body(&raw);
        assert!(
            s.contains("From: Big Boss <boss@example.com>"),
            "space-only display name stays unquoted (matches mail_builder): {s}"
        );
    }

    #[test]
    fn invite_mime_encodes_non_ascii_subject() {
        let raw = build_imip_mime(
            "boss@example.com",
            None,
            &["guest@example.com".into()],
            &[],
            None,
            "Réunion d'équipe",
            "b",
            sample_ics(),
            &[],
        );
        let s = decode_mime_body(&raw);
        assert!(
            s.contains("=?UTF-8?B?"),
            "non-ASCII subject must be RFC 2047 encoded: {s}"
        );
    }

    #[test]
    fn invite_mime_strips_crlf_from_subject() {
        let raw = build_imip_mime(
            "boss@example.com",
            None,
            &["guest@example.com".into()],
            &[],
            None,
            "S\r\nBcc: evil@example.com",
            "b",
            sample_ics(),
            &[],
        );
        let s = decode_mime_body(&raw);
        assert!(
            !s.contains("\r\nBcc:"),
            "CR/LF in subject must not inject a header line: {s}"
        );
    }

    // ---- method propagation ----

    #[test]
    fn invite_mime_calendar_method_matches_ics_method_reply() {
        let ics =
            "BEGIN:VCALENDAR\r\nMETHOD:REPLY\r\nBEGIN:VEVENT\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let raw = build_imip_mime(
            "boss@example.com",
            None,
            &["guest@example.com".into()],
            &[],
            None,
            "S",
            "b",
            ics,
            &[],
        );
        let s = decode_mime_body(&raw);
        assert!(
            s.contains("Content-Type: text/calendar; method=REPLY; charset=utf-8"),
            "calendar method parameter must track the ICS METHOD: {s}"
        );
    }

    // ---- base64 line wrapping (RFC 2045 §6.8) ----

    #[test]
    fn base64_mime_lines_wraps_at_76_chars() {
        // 200 bytes → base64 ~268 chars → must contain CRLF breaks with no
        // line over 76 chars.
        let data = vec![b'A'; 200];
        let encoded = base64_mime_lines(&data);
        for line in encoded.split("\r\n") {
            assert!(
                line.len() <= 76,
                "base64 line over 76 chars ({}): {line}",
                line.len()
            );
        }
    }
}
