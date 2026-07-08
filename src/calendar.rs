use crate::types::{Attendee, CalendarEvent, RsvpStatus};
use chrono::{
    DateTime, FixedOffset, Local, NaiveDate, NaiveDateTime, NaiveTime, Offset, TimeZone, Utc,
};
use chrono_tz::Tz;
use regex::Regex;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::LazyLock;

static PARTSTAT_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"PARTSTAT=\w[\w-]*").unwrap());

// =============================================================================
// ICS Parsing (hand-rolled)
// =============================================================================

pub fn parse_ics(data: &str) -> Option<CalendarEvent> {
    let data = data.trim();
    if !data.contains("BEGIN:VCALENDAR") {
        return None;
    }

    // Extract METHOD from VCALENDAR level. Default to PUBLISH (not an invitation)
    // so that standalone .ics exports don't trigger auto-add to calendar.
    let method = extract_property(data, "METHOD").unwrap_or_else(|| "PUBLISH".into());

    // Find VEVENT block
    let vevent_start = data.find("BEGIN:VEVENT")?;
    let vevent_end = data.find("END:VEVENT")?;
    let vevent = &data[vevent_start..vevent_end + "END:VEVENT".len()];

    // Unfold lines (RFC 5545: continuation lines start with space or tab)
    let unfolded = unfold_lines(vevent);

    // Extract VTIMEZONE UTC offsets from the full calendar data so we can
    // resolve TZID references on DTSTART/DTEND inside the VEVENT.
    let tz_offsets = parse_vtimezone_offsets(data);

    let uid = extract_property(&unfolded, "UID")?;
    let summary = extract_property(&unfolded, "SUMMARY").unwrap_or_default();
    let location = extract_property(&unfolded, "LOCATION");
    let description = extract_property(&unfolded, "DESCRIPTION");
    let sequence: i32 = extract_property(&unfolded, "SEQUENCE")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let dtstart = parse_ics_datetime_property(&unfolded, "DTSTART", &tz_offsets)?;
    let dtend = parse_ics_datetime_property(&unfolded, "DTEND", &tz_offsets);

    let status = extract_property(&unfolded, "STATUS");

    let (organizer_email, organizer_name) = parse_organizer(&unfolded);
    let attendees = parse_attendees(&unfolded);

    // Some services (e.g. Lumo) send METHOD:REQUEST with STATUS:CANCELLED
    // inside the VEVENT instead of using METHOD:CANCEL at the calendar level.
    // Normalize to METHOD=CANCEL so callers only need to check one field.
    let method = if status.as_deref() == Some("CANCELLED") {
        "CANCEL".into()
    } else {
        method
    };

    Some(CalendarEvent {
        uid,
        summary,
        dtstart,
        dtend,
        location,
        description,
        organizer_email,
        organizer_name,
        attendees,
        sequence,
        method,
        raw_ics: data.to_string(),
        user_rsvp_status: None,
        is_update: false,
    })
}

// =============================================================================
// Invite update decision (RFC 5546 SEQUENCE semantics + anti-spoof)
// =============================================================================

/// Outcome of comparing an incoming REQUEST invite against the event already
/// stored in the user's calendar for the same UID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InviteAction {
    /// No stored event for this UID — the normal first-time auto-add path.
    NoStored,
    /// Stored SEQUENCE >= incoming — idempotent re-receipt; nothing new to do.
    Unchanged,
    /// Higher incoming SEQUENCE from the verified organizer — overwrite the
    /// stored event and reset the user's now-stale RSVP.
    Update,
    /// Higher incoming SEQUENCE but the sender is not the stored organizer (or
    /// the organizer / sender is missing) — a possible spoofed overwrite.
    /// Touch nothing; render the incoming ICS as-is.
    RejectSpoof,
}

/// Decide how an incoming REQUEST invite relates to the stored event.
///
/// RFC 5546 uses SEQUENCE to order revisions of the same event (UID); a higher
/// SEQUENCE is a genuine reschedule/update. But SEQUENCE alone is forgeable:
/// any sender who learns a UID could mail an ICS with SEQUENCE=999 and silently
/// overwrite a real event. We therefore only honor an update when the message
/// sender matches the *stored* event's organizer (case-insensitively). See the
/// bwjm test matrix T9.4.
///
/// `stored_seq` is `None` when the event isn't in the calendar yet (or the
/// lookup failed — we degrade to the first-time add path). An empty organizer
/// or sender is treated as missing.
pub fn invite_update_decision(
    stored_seq: Option<i32>,
    incoming_seq: i32,
    stored_organizer_email: Option<&str>,
    sender_email: Option<&str>,
) -> InviteAction {
    let Some(stored_seq) = stored_seq else {
        return InviteAction::NoStored;
    };
    if incoming_seq <= stored_seq {
        return InviteAction::Unchanged;
    }
    // incoming_seq > stored_seq: a claimed update. Verify it came from the
    // organizer of record before letting it touch the calendar.
    match (stored_organizer_email, sender_email) {
        (Some(org), Some(sender)) if !org.is_empty() && org.eq_ignore_ascii_case(sender) => {
            InviteAction::Update
        }
        _ => InviteAction::RejectSpoof,
    }
}

fn unfold_lines(s: &str) -> String {
    // ICS line folding: CRLF followed by single whitespace = continuation
    let s = s.replace("\r\n ", "").replace("\r\n\t", "");
    // Also handle bare LF folding
    s.replace("\n ", "").replace("\n\t", "")
}

fn extract_property(text: &str, name: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        // Match "NAME:value" or "NAME;params:value"
        // Must verify the character after the property name is ':' or ';'
        // to avoid false positives (e.g. "UIDX" matching "UID")
        if let Some(rest) = line.strip_prefix(name) {
            if let Some(stripped) = rest.strip_prefix(':') {
                return Some(stripped.to_string());
            }
            if let Some(rest_after_semi) = rest.strip_prefix(';') {
                // Has parameters — find the colon after params
                if let Some(colon_pos) = rest_after_semi.find(':') {
                    return Some(rest_after_semi[colon_pos + 1..].to_string());
                }
            }
            // If next char is neither ':' nor ';', this is a different property — skip
        }
    }
    None
}

/// Parse VTIMEZONE blocks from the full ICS data. Returns a map from TZID
/// to the STANDARD component's UTCOFFSETTO (falls back to DAYLIGHT if no STANDARD).
///
/// This is the fallback path used only when a TZID is not a recognized IANA
/// name (e.g. Outlook's "Pacific Standard Time"). For IANA-named TZIDs, the
/// parser uses chrono-tz directly, which resolves DST correctly at the
/// event's instant.
fn parse_vtimezone_offsets(data: &str) -> HashMap<String, FixedOffset> {
    let mut offsets = HashMap::new();
    let unfolded = unfold_lines(data);

    // Walk through each VTIMEZONE block
    let mut search_from = 0;
    while let Some(tz_start) = unfolded[search_from..].find("BEGIN:VTIMEZONE") {
        let tz_start = search_from + tz_start;
        let Some(tz_end) = unfolded[tz_start..].find("END:VTIMEZONE") else {
            break;
        };
        let tz_block = &unfolded[tz_start..tz_start + tz_end];
        search_from = tz_start + tz_end;

        let Some(tzid) = extract_property(tz_block, "TZID") else {
            continue;
        };

        // Prefer STANDARD offset; fall back to DAYLIGHT if no STANDARD block
        let offset = extract_sub_block_offset(tz_block, "STANDARD")
            .or_else(|| extract_sub_block_offset(tz_block, "DAYLIGHT"));

        if let Some(offset) = offset {
            offsets.insert(tzid, offset);
        }
    }
    offsets
}

/// Extract UTCOFFSETTO from a STANDARD or DAYLIGHT sub-block within a VTIMEZONE.
fn extract_sub_block_offset(tz_block: &str, sub_name: &str) -> Option<FixedOffset> {
    let begin = format!("BEGIN:{sub_name}");
    let start = tz_block.find(&begin)?;
    let end_marker = format!("END:{sub_name}");
    let end = tz_block[start..].find(&end_marker)?;
    let sub_block = &tz_block[start..start + end];
    let offset_str = extract_property(sub_block, "UTCOFFSETTO")?;
    parse_utc_offset(&offset_str)
}

/// Parse an ICS UTC offset string like "+0530", "-0800", "+0000" into a FixedOffset.
fn parse_utc_offset(s: &str) -> Option<FixedOffset> {
    let s = s.trim();
    if s.len() < 5 {
        return None;
    }
    let sign: i32 = if s.starts_with('-') { -1 } else { 1 };
    let hours: i32 = s[1..3].parse().ok()?;
    let minutes: i32 = s[3..5].parse().ok()?;
    let total_seconds = sign * (hours * 3600 + minutes * 60);
    FixedOffset::east_opt(total_seconds)
}

fn parse_ics_datetime_property(
    text: &str,
    name: &str,
    tz_offsets: &HashMap<String, FixedOffset>,
) -> Option<DateTime<Utc>> {
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        let rest = match line.strip_prefix(name) {
            Some(r) => r,
            None => continue,
        };

        // Must start with ':' or ';' to avoid prefix false positives
        let (params, value) = if let Some(stripped) = rest.strip_prefix(':') {
            ("", stripped)
        } else if rest.starts_with(';') {
            let colon = rest.find(':')?;
            (&rest[1..colon], &rest[colon + 1..])
        } else {
            continue;
        };

        // All-day events: VALUE=DATE — no timezone conversion needed
        let is_date_only = params.contains("VALUE=DATE") && !params.contains("VALUE=DATE-TIME");
        let is_date_only = is_date_only || value.len() == 8;

        if is_date_only {
            let date = NaiveDate::parse_from_str(value.trim(), "%Y%m%d").ok()?;
            let dt = NaiveDateTime::new(date, NaiveTime::from_hms_opt(0, 0, 0).unwrap());
            return Some(DateTime::from_naive_utc_and_offset(dt, Utc));
        }

        let value = value.trim();

        // Case 1: Explicit UTC — trailing Z
        if value.ends_with('Z') {
            let dt =
                NaiveDateTime::parse_from_str(value.trim_end_matches('Z'), "%Y%m%dT%H%M%S").ok()?;
            return Some(DateTime::from_naive_utc_and_offset(dt, Utc));
        }

        let dt = NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%S").ok()?;

        // Case 2: TZID parameter. Prefer chrono-tz (IANA-aware, handles DST
        // correctly at the event's instant). Fall back to the VTIMEZONE
        // offset table for non-IANA TZIDs (e.g. Outlook's "Pacific Standard
        // Time" labels).
        if let Some(tzid) = extract_param_from_str(params, "TZID") {
            if let Ok(tz) = Tz::from_str(&tzid) {
                let resolved = tz
                    .from_local_datetime(&dt)
                    .earliest()
                    .or_else(|| tz.from_local_datetime(&dt).latest())?;
                return Some(resolved.with_timezone(&Utc));
            }
            if let Some(offset) = tz_offsets.get(&tzid) {
                let local = offset.from_local_datetime(&dt).earliest()?;
                return Some(local.with_timezone(&Utc));
            }
        }

        // Case 3: Floating time (no Z, no TZID) — interpret as system local tz.
        // Use from_local_datetime on the event's date to get the correct DST offset.
        let local = Local.from_local_datetime(&dt).earliest()?;
        return Some(local.with_timezone(&Utc));
    }
    None
}

/// Extract a parameter value from the params portion of an ICS property line.
/// e.g. extract_param_from_str("TZID=America/New_York;VALUE=DATE-TIME", "TZID")
/// returns Some("America/New_York")
fn extract_param_from_str(params: &str, param_name: &str) -> Option<String> {
    let search = format!("{param_name}=");
    let pos = params.find(&search)?;
    let start = pos + search.len();
    let rest = &params[start..];
    let end = rest.find(';').unwrap_or(rest.len());
    Some(rest[..end].to_string())
}

fn parse_organizer(text: &str) -> (String, Option<String>) {
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if !line.starts_with("ORGANIZER") {
            continue;
        }

        let name = extract_param(line, "CN");
        let email = extract_mailto(line);
        return (email, name);
    }
    (String::new(), None)
}

fn parse_attendees(text: &str) -> Vec<Attendee> {
    let mut attendees = Vec::new();
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if !line.starts_with("ATTENDEE") {
            continue;
        }

        let email = extract_mailto(line);
        if email.is_empty() {
            continue;
        }
        let name = extract_param(line, "CN");
        let status = extract_param(line, "PARTSTAT").unwrap_or_else(|| "NEEDS-ACTION".into());

        attendees.push(Attendee {
            email,
            name,
            status,
        });
    }
    attendees
}

fn extract_mailto(line: &str) -> String {
    // Look for mailto: (case-insensitive)
    let lower = line.to_lowercase();
    if let Some(pos) = lower.find("mailto:") {
        let start = pos + "mailto:".len();
        let rest = &line[start..];
        // Email ends at next non-email char
        let end = rest.find([';', ',', '\r', '\n', ' ']).unwrap_or(rest.len());
        return rest[..end].to_string();
    }
    String::new()
}

fn extract_param(line: &str, param_name: &str) -> Option<String> {
    let search = format!("{param_name}=");
    let pos = line.find(&search)?;
    let start = pos + search.len();
    let rest = &line[start..];

    // Value may be quoted or unquoted
    if let Some(stripped) = rest.strip_prefix('"') {
        let end = stripped.find('"')?;
        Some(stripped[..end].to_string())
    } else {
        let end = rest.find([';', ':', ',', '\r', '\n']).unwrap_or(rest.len());
        Some(rest[..end].to_string())
    }
}

// =============================================================================
// PARTSTAT Update
// =============================================================================

/// Replace the PARTSTAT value for `attendee_email` in the given ICS data.
/// Output is always unfolded (RFC 5545 line continuations removed).
pub fn update_partstat(raw_ics: &str, attendee_email: &str, status: &RsvpStatus) -> String {
    let raw_ics = unfold_lines(raw_ics);
    let new_partstat = format!("PARTSTAT={}", status.as_ics_str());
    let email_lower = attendee_email.to_lowercase();

    // Split on \n but preserve \r if present to keep original line endings
    raw_ics
        .split('\n')
        .map(|line| {
            let trimmed = line.trim_end_matches('\r');
            if trimmed.starts_with("ATTENDEE")
                && trimmed
                    .to_lowercase()
                    .contains(&format!("mailto:{email_lower}"))
            {
                let updated = PARTSTAT_RE.replace(trimmed, new_partstat.as_str());
                if line.ends_with('\r') {
                    format!("{updated}\r")
                } else {
                    updated.to_string()
                }
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// =============================================================================
// RSVP Generation
// =============================================================================

pub fn generate_rsvp(event: &CalendarEvent, attendee_email: &str, status: &RsvpStatus) -> String {
    debug_assert!(
        !attendee_email.is_empty(),
        "attendee_email must not be empty"
    );

    // Find the attendee's CN from the original event
    let cn = event
        .attendees
        .iter()
        .find(|a| a.email.eq_ignore_ascii_case(attendee_email))
        .and_then(|a| a.name.clone());

    let cn_param = match &cn {
        Some(name) => format!(";CN={}", escape_param_value(name)),
        None => String::new(),
    };

    let dtstart = format_ics_datetime(event.dtstart);
    let dtend_line = event
        .dtend
        .map(|dt| format!("DTEND:{}\r\n", format_ics_datetime(dt)))
        .unwrap_or_default();

    let organizer_cn = event
        .organizer_name
        .as_ref()
        .map(|n| format!(";CN={}", escape_param_value(n)))
        .unwrap_or_default();

    format!(
        "BEGIN:VCALENDAR\r\n\
         VERSION:2.0\r\n\
         PRODID:-//Supervillain//EN\r\n\
         METHOD:REPLY\r\n\
         BEGIN:VEVENT\r\n\
         UID:{uid}\r\n\
         DTSTART:{dtstart}\r\n\
         {dtend_line}\
         SUMMARY:{summary}\r\n\
         ORGANIZER{organizer_cn}:mailto:{organizer_email}\r\n\
         ATTENDEE{cn_param};PARTSTAT={partstat}:mailto:{attendee_email}\r\n\
         SEQUENCE:{sequence}\r\n\
         END:VEVENT\r\n\
         END:VCALENDAR",
        uid = event.uid,
        dtstart = dtstart,
        dtend_line = dtend_line,
        summary = escape_text(&event.summary),
        organizer_cn = organizer_cn,
        organizer_email = sanitize_address(&event.organizer_email),
        cn_param = cn_param,
        partstat = status.as_ics_str(),
        attendee_email = sanitize_address(attendee_email),
        sequence = event.sequence,
    )
}

fn format_ics_datetime(dt: DateTime<Utc>) -> String {
    dt.format("%Y%m%dT%H%M%SZ").to_string()
}

fn format_ics_datetime_local(dt: DateTime<Tz>) -> String {
    dt.format("%Y%m%dT%H%M%S").to_string()
}

fn format_offset_hhmm(offset_seconds: i32) -> String {
    let sign = if offset_seconds < 0 { '-' } else { '+' };
    let abs = offset_seconds.unsigned_abs() as i32;
    let h = abs / 3600;
    let m = (abs % 3600) / 60;
    format!("{sign}{h:02}{m:02}")
}

/// Synthesize a minimal VTIMEZONE block covering the offset that applies at
/// the given instant in the given IANA timezone.
///
/// This deliberately emits a single STANDARD sub-block carrying the offset
/// effective at `dt`, not the full set of DST transition rules. That is
/// correct for one-shot events (the only kind we generate — we don't emit
/// RRULE) because RFC 5545 only requires VTIMEZONE to cover the date range
/// referenced by VEVENTs in the same calendar object. Recipients see the
/// right wall-clock time. Receiving clients that follow up by computing
/// DST transitions for the same TZID will fall back to their own tzdata,
/// since the TZID is an IANA name they already know.
///
/// If we ever start generating recurring events, this needs to grow real
/// STANDARD/DAYLIGHT pairs derived from `chrono_tz::Tz` transitions.
fn synth_vtimezone(tz: Tz, dt: DateTime<Tz>) -> String {
    let offset = dt.offset().fix();
    let offset_str = format_offset_hhmm(offset.local_minus_utc());
    let tzname = format!("{}", dt.format("%Z"));
    // X-LIC-LOCATION (libical extension, RFC 7808 §7.1.1) labels the
    // VTIMEZONE with its IANA name so strict parsers that cache VTIMEZONE
    // definitions by TZID can map back to their own IANA rules for *other*
    // events sharing the same TZID — even though our single STANDARD with
    // TZOFFSETFROM==TZOFFSETTO advertises one year-round offset. Roborev 186 #9.
    format!(
        "BEGIN:VTIMEZONE\r\n\
         TZID:{tzid}\r\n\
         X-LIC-LOCATION:{tzid}\r\n\
         BEGIN:STANDARD\r\n\
         DTSTART:19700101T000000\r\n\
         TZOFFSETFROM:{offset}\r\n\
         TZOFFSETTO:{offset}\r\n\
         TZNAME:{tzname}\r\n\
         END:STANDARD\r\n\
         END:VTIMEZONE\r\n",
        tzid = tz.name(),
        offset = offset_str,
        tzname = if tzname.is_empty() {
            tz.name().to_string()
        } else {
            tzname
        },
    )
}

fn attendee_line(att: &Attendee) -> String {
    let cn_param = att
        .name
        .as_ref()
        .map(|n| format!(";CN={}", escape_param_value(n)))
        .unwrap_or_default();
    let partstat = if att.status.is_empty() {
        "NEEDS-ACTION".to_string()
    } else {
        att.status.clone()
    };
    format!(
        "ATTENDEE{cn};RSVP=TRUE;PARTSTAT={partstat}:mailto:{email}\r\n",
        cn = cn_param,
        partstat = partstat,
        email = sanitize_address(&att.email),
    )
}

/// Escape a parameter value (e.g. `CN=...`) per RFC 5545 §3.1, with
/// defense-in-depth against naive parsers.
///
/// Without this, `name: "Bob\r\nATTENDEE;PARTSTAT=ACCEPTED:mailto:victim@x"`
/// would inject a second ATTENDEE line into the calendar object (the
/// receiver's client may show fake attendees or auto-accept on the user's
/// behalf). Spec-wise DQUOTE-wrapping handles `,`, `;`, `:`, but a naive
/// parser that splits on the first unescaped `:` would still see a colon
/// inside a quoted CN as the property-value separator. We therefore strip
/// `:` and `"` outright — neither is legitimate in a display name — and
/// DQUOTE-wrap when the value contains other terminator chars.
fn escape_param_value(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .filter(|c| !c.is_control() && *c != '"' && *c != ':')
        .collect();
    if cleaned.chars().any(|c| c == ',' || c == ';' || c == ' ') {
        format!("\"{cleaned}\"")
    } else {
        cleaned
    }
}

/// Sanitize a value emitted after `mailto:` so it can't terminate the line
/// or smuggle in additional iCal properties. Email addresses may not
/// legitimately contain CR/LF or `:` in the visible portion; rejecting
/// those characters by stripping them is the safe + ergonomic choice.
fn sanitize_address(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control() && *c != ',' && *c != ';' && *c != '"')
        .collect()
}

/// Build an iTIP REQUEST (a calendar invite) with TZID-qualified DTSTART/DTEND.
#[allow(clippy::too_many_arguments)]
pub fn generate_invite(
    organizer_email: &str,
    organizer_name: Option<&str>,
    summary: &str,
    description: Option<&str>,
    location: Option<&str>,
    dtstart: DateTime<Tz>,
    dtend: DateTime<Tz>,
    attendees: &[Attendee],
    uid: Option<&str>,
) -> String {
    let tz = dtstart.timezone();
    let uid = uid
        .map(str::to_string)
        .unwrap_or_else(|| format!("{}@supervillain", uuid::Uuid::new_v4()));
    let tzid = tz.name();
    let dtstamp = Utc::now().format("%Y%m%dT%H%M%SZ");

    let vtimezone = synth_vtimezone(tz, dtstart);

    let organizer_cn = organizer_name
        .map(|n| format!(";CN={}", escape_param_value(n)))
        .unwrap_or_default();
    let organizer_email_safe = sanitize_address(organizer_email);

    let mut attendee_lines = String::new();
    for att in attendees {
        attendee_lines.push_str(&attendee_line(att));
    }

    let description_line = description
        .map(|d| format!("DESCRIPTION:{}\r\n", escape_text(d)))
        .unwrap_or_default();
    let location_line = location
        .map(|l| format!("LOCATION:{}\r\n", escape_text(l)))
        .unwrap_or_default();

    format!(
        "BEGIN:VCALENDAR\r\n\
         VERSION:2.0\r\n\
         PRODID:-//Supervillain//EN\r\n\
         METHOD:REQUEST\r\n\
         {vtimezone}\
         BEGIN:VEVENT\r\n\
         UID:{uid}\r\n\
         DTSTAMP:{dtstamp}\r\n\
         DTSTART;TZID={tzid}:{dtstart}\r\n\
         DTEND;TZID={tzid}:{dtend}\r\n\
         SUMMARY:{summary}\r\n\
         {description_line}\
         {location_line}\
         ORGANIZER{organizer_cn}:mailto:{organizer_email}\r\n\
         {attendee_lines}\
         SEQUENCE:0\r\n\
         STATUS:CONFIRMED\r\n\
         END:VEVENT\r\n\
         END:VCALENDAR",
        dtstart = format_ics_datetime_local(dtstart),
        dtend = format_ics_datetime_local(dtend),
        summary = escape_text(summary),
        organizer_email = organizer_email_safe,
    )
}

/// Build an iTIP REPLY that quotes the event time in the responder's primary TZ
/// (rather than UTC-Z). Recipients see times in the TZ the responder set, which
/// is friendlier than a raw Zulu timestamp when their client doesn't reformat.
pub fn generate_rsvp_with_tz(
    event: &CalendarEvent,
    attendee_email: &str,
    status: &RsvpStatus,
    reply_tz: Tz,
) -> String {
    debug_assert!(
        !attendee_email.is_empty(),
        "attendee_email must not be empty"
    );

    let cn = event
        .attendees
        .iter()
        .find(|a| a.email.eq_ignore_ascii_case(attendee_email))
        .and_then(|a| a.name.clone());
    let cn_param = match &cn {
        Some(name) => format!(";CN={}", escape_param_value(name)),
        None => String::new(),
    };

    let dtstart_local = event.dtstart.with_timezone(&reply_tz);
    let dtend_local = event.dtend.map(|dt| dt.with_timezone(&reply_tz));
    let tzid = reply_tz.name();
    let vtimezone = synth_vtimezone(reply_tz, dtstart_local);

    let dtstart_line = format!(
        "DTSTART;TZID={tzid}:{}\r\n",
        format_ics_datetime_local(dtstart_local)
    );
    let dtend_line = dtend_local
        .map(|dt| format!("DTEND;TZID={tzid}:{}\r\n", format_ics_datetime_local(dt)))
        .unwrap_or_default();

    let organizer_cn = event
        .organizer_name
        .as_ref()
        .map(|n| format!(";CN={}", escape_param_value(n)))
        .unwrap_or_default();

    format!(
        "BEGIN:VCALENDAR\r\n\
         VERSION:2.0\r\n\
         PRODID:-//Supervillain//EN\r\n\
         METHOD:REPLY\r\n\
         {vtimezone}\
         BEGIN:VEVENT\r\n\
         UID:{uid}\r\n\
         {dtstart_line}\
         {dtend_line}\
         SUMMARY:{summary}\r\n\
         ORGANIZER{organizer_cn}:mailto:{organizer_email}\r\n\
         ATTENDEE{cn_param};PARTSTAT={partstat}:mailto:{attendee_email}\r\n\
         SEQUENCE:{sequence}\r\n\
         END:VEVENT\r\n\
         END:VCALENDAR",
        uid = sanitize_token(&event.uid),
        summary = escape_text(&event.summary),
        organizer_email = sanitize_address(&event.organizer_email),
        attendee_email = sanitize_address(attendee_email),
        partstat = status.as_ics_str(),
        sequence = event.sequence,
    )
}

fn escape_text(s: &str) -> String {
    // RFC 5545: backslash, newline, comma, semicolon need escaping in TEXT values.
    // CR has no escape — strict parsers reject a bare CR mid-line. Normalize
    // CR/CRLF to a single escaped \n so a `summary` containing \r can't
    // produce malformed ICS or smuggle in a property break on receipt.
    s.replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace(',', "\\,")
        .replace(';', "\\;")
}

/// Sanitize an opaque token (UID, SEQUENCE-adjacent values) so an attacker-
/// controlled invite can't smuggle line breaks into the REPLY we generate.
/// UIDs in the wild are usually UUIDs or base64; control characters and the
/// iCal terminators have no legitimate place there.
fn sanitize_token(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control() && *c != ';' && *c != ':' && *c != ',' && *c != '"')
        .collect()
}

/// RFC 4791: stored calendar objects must not contain METHOD.
/// METHOD is an iTIP transport property (RFC 5546) — it tells recipients
/// how to process the message (REQUEST = invitation, REPLY = response).
/// CalDAV servers may misinterpret METHOD:REQUEST as a new scheduling
/// action rather than a simple event store.
pub fn strip_method(ics: &str) -> String {
    ics.lines()
        .filter(|line| !line.trim_end_matches('\r').starts_with("METHOD:"))
        .collect::<Vec<_>>()
        .join("\n")
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_ICS: &str = "\
BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
PRODID:-//Test//Test//EN\r\n\
METHOD:REQUEST\r\n\
BEGIN:VEVENT\r\n\
UID:test-uid-123@example.com\r\n\
DTSTART:20260215T100000Z\r\n\
DTEND:20260215T110000Z\r\n\
SUMMARY:Team Standup\r\n\
LOCATION:Conference Room B\r\n\
DESCRIPTION:Daily standup meeting\r\n\
ORGANIZER;CN=Alice:mailto:alice@example.com\r\n\
ATTENDEE;CN=Bob;PARTSTAT=NEEDS-ACTION:mailto:bob@example.com\r\n\
ATTENDEE;CN=Carol;PARTSTAT=ACCEPTED:mailto:carol@example.com\r\n\
SEQUENCE:0\r\n\
END:VEVENT\r\n\
END:VCALENDAR";

    const SAMPLE_ICS_NO_LOCATION: &str = "\
BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
METHOD:REQUEST\r\n\
BEGIN:VEVENT\r\n\
UID:no-loc-uid@example.com\r\n\
DTSTART:20260215T100000Z\r\n\
DTEND:20260215T110000Z\r\n\
SUMMARY:Quick Sync\r\n\
ORGANIZER;CN=Alice:mailto:alice@example.com\r\n\
SEQUENCE:0\r\n\
END:VEVENT\r\n\
END:VCALENDAR";

    const SAMPLE_ICS_ALL_DAY: &str = "\
BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
METHOD:REQUEST\r\n\
BEGIN:VEVENT\r\n\
UID:all-day-uid@example.com\r\n\
DTSTART;VALUE=DATE:20260215\r\n\
SUMMARY:All Day Event\r\n\
ORGANIZER;CN=Alice:mailto:alice@example.com\r\n\
SEQUENCE:0\r\n\
END:VEVENT\r\n\
END:VCALENDAR";

    const SAMPLE_ICS_NO_DTEND: &str = "\
BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
METHOD:REQUEST\r\n\
BEGIN:VEVENT\r\n\
UID:no-dtend-uid@example.com\r\n\
DTSTART:20260215T100000Z\r\n\
SUMMARY:Open Ended\r\n\
ORGANIZER;CN=Alice:mailto:alice@example.com\r\n\
ATTENDEE;CN=Bob;PARTSTAT=NEEDS-ACTION:mailto:bob@example.com\r\n\
SEQUENCE:1\r\n\
END:VEVENT\r\n\
END:VCALENDAR";

    const SAMPLE_ICS_ATTENDEE_NO_CN: &str = "\
BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
METHOD:REQUEST\r\n\
BEGIN:VEVENT\r\n\
UID:no-cn-uid@example.com\r\n\
DTSTART:20260215T100000Z\r\n\
SUMMARY:Test\r\n\
ORGANIZER;CN=Alice:mailto:alice@example.com\r\n\
ATTENDEE;PARTSTAT=ACCEPTED:mailto:dave@example.com\r\n\
SEQUENCE:0\r\n\
END:VEVENT\r\n\
END:VCALENDAR";

    // --- parse_ics tests ---

    #[test]
    fn parse_basic_event() {
        let event = parse_ics(SAMPLE_ICS).unwrap();
        assert_eq!(event.uid, "test-uid-123@example.com");
        assert_eq!(event.summary, "Team Standup");
        assert_eq!(event.location, Some("Conference Room B".into()));
        assert_eq!(event.description, Some("Daily standup meeting".into()));
        assert_eq!(event.sequence, 0);
        assert!(event.dtend.is_some());
    }

    #[test]
    fn parse_organizer_email() {
        let event = parse_ics(SAMPLE_ICS).unwrap();
        assert_eq!(event.organizer_email, "alice@example.com");
    }

    #[test]
    fn parse_organizer_name() {
        let event = parse_ics(SAMPLE_ICS).unwrap();
        assert_eq!(event.organizer_name, Some("Alice".into()));
    }

    #[test]
    fn parse_attendees() {
        let event = parse_ics(SAMPLE_ICS).unwrap();
        assert_eq!(event.attendees.len(), 2);
        assert_eq!(event.attendees[0].email, "bob@example.com");
        assert_eq!(event.attendees[0].name, Some("Bob".into()));
        assert_eq!(event.attendees[0].status, "NEEDS-ACTION");
        assert_eq!(event.attendees[1].email, "carol@example.com");
        assert_eq!(event.attendees[1].name, Some("Carol".into()));
        assert_eq!(event.attendees[1].status, "ACCEPTED");
    }

    #[test]
    fn parse_missing_location() {
        let event = parse_ics(SAMPLE_ICS_NO_LOCATION).unwrap();
        assert!(event.location.is_none());
        assert_eq!(event.summary, "Quick Sync");
    }

    #[test]
    fn parse_missing_dtend() {
        let event = parse_ics(SAMPLE_ICS_NO_DTEND).unwrap();
        assert!(event.dtend.is_none());
    }

    #[test]
    fn parse_all_day_event() {
        let event = parse_ics(SAMPLE_ICS_ALL_DAY).unwrap();
        assert_eq!(event.dtstart.hour(), 0);
        assert_eq!(event.dtstart.minute(), 0);
    }

    #[test]
    fn parse_preserves_raw_ics() {
        let event = parse_ics(SAMPLE_ICS).unwrap();
        assert!(event.raw_ics.contains("VEVENT"));
        assert!(event.raw_ics.contains("Team Standup"));
    }

    #[test]
    fn parse_invalid_ics_returns_none() {
        assert!(parse_ics("this is not valid ICS data").is_none());
    }

    #[test]
    fn parse_no_vevent_returns_none() {
        let data = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nEND:VCALENDAR";
        assert!(parse_ics(data).is_none());
    }

    #[test]
    fn parse_no_uid_returns_none() {
        let data = "\
BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
BEGIN:VEVENT\r\n\
SUMMARY:No UID\r\n\
DTSTART:20260215T100000Z\r\n\
END:VEVENT\r\n\
END:VCALENDAR";
        assert!(parse_ics(data).is_none());
    }

    #[test]
    fn parse_attendee_without_cn() {
        let event = parse_ics(SAMPLE_ICS_ATTENDEE_NO_CN).unwrap();
        assert_eq!(event.attendees.len(), 1);
        assert!(event.attendees[0].name.is_none());
        assert_eq!(event.attendees[0].email, "dave@example.com");
        assert_eq!(event.attendees[0].status, "ACCEPTED");
    }

    #[test]
    fn parse_user_rsvp_status_is_none() {
        let event = parse_ics(SAMPLE_ICS).unwrap();
        assert!(
            event.user_rsvp_status.is_none(),
            "parse_ics should not populate user_rsvp_status"
        );
    }

    #[test]
    fn parse_method() {
        let event = parse_ics(SAMPLE_ICS).unwrap();
        assert_eq!(event.method, "REQUEST");
    }

    #[test]
    fn parse_dtstart_value() {
        let event = parse_ics(SAMPLE_ICS).unwrap();
        assert_eq!(event.dtstart.year(), 2026);
        assert_eq!(event.dtstart.month(), 2);
        assert_eq!(event.dtstart.day(), 15);
        assert_eq!(event.dtstart.hour(), 10);
    }

    /// Regression for the documented DST limitation in the previous
    /// parse_vtimezone_offsets-only path: an event on a date where DST is in
    /// effect must use the *DST* offset. 2026-07-15 10:00 LA is PDT (-07:00)
    /// → 17:00 UTC. The old single-offset path used PST (-08:00) → 18:00.
    #[test]
    fn parse_tzid_uses_dst_when_event_in_summer() {
        let ics = "\
BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
PRODID:-//Test//EN\r\n\
METHOD:REQUEST\r\n\
BEGIN:VEVENT\r\n\
UID:dst-test@example.com\r\n\
SUMMARY:Summer meeting\r\n\
DTSTART;TZID=America/Los_Angeles:20260715T100000\r\n\
DTEND;TZID=America/Los_Angeles:20260715T110000\r\n\
ORGANIZER:mailto:alice@example.com\r\n\
ATTENDEE:mailto:bob@example.com\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";
        let event = parse_ics(ics).unwrap();
        assert_eq!(event.dtstart.hour(), 17, "PDT → 17:00 UTC, not 18:00");
        assert_eq!(event.dtstart.day(), 15);
    }

    /// And the standard-time counterpart: 2026-01-15 10:00 LA = 18:00 UTC.
    #[test]
    fn parse_tzid_uses_standard_when_event_in_winter() {
        let ics = "\
BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
PRODID:-//Test//EN\r\n\
METHOD:REQUEST\r\n\
BEGIN:VEVENT\r\n\
UID:dst-test-winter@example.com\r\n\
SUMMARY:Winter meeting\r\n\
DTSTART;TZID=America/Los_Angeles:20260115T100000\r\n\
DTEND;TZID=America/Los_Angeles:20260115T110000\r\n\
ORGANIZER:mailto:alice@example.com\r\n\
ATTENDEE:mailto:bob@example.com\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";
        let event = parse_ics(ics).unwrap();
        assert_eq!(event.dtstart.hour(), 18);
    }

    // --- generate_rsvp tests ---

    fn sample_event() -> CalendarEvent {
        parse_ics(SAMPLE_ICS).unwrap()
    }

    #[test]
    fn rsvp_method_reply() {
        let rsvp = generate_rsvp(&sample_event(), "bob@example.com", &RsvpStatus::Accepted);
        assert!(rsvp.contains("METHOD:REPLY"));
    }

    #[test]
    fn rsvp_includes_uid() {
        let rsvp = generate_rsvp(&sample_event(), "bob@example.com", &RsvpStatus::Accepted);
        assert!(rsvp.contains("test-uid-123@example.com"));
    }

    #[test]
    fn rsvp_attendee_accepted() {
        let rsvp = generate_rsvp(&sample_event(), "bob@example.com", &RsvpStatus::Accepted);
        assert!(rsvp.contains("bob@example.com"));
        assert!(rsvp.contains("ACCEPTED"));
    }

    #[test]
    fn rsvp_tentative() {
        let rsvp = generate_rsvp(&sample_event(), "bob@example.com", &RsvpStatus::Tentative);
        assert!(rsvp.contains("TENTATIVE"));
    }

    #[test]
    fn rsvp_declined() {
        let rsvp = generate_rsvp(&sample_event(), "bob@example.com", &RsvpStatus::Declined);
        assert!(rsvp.contains("DECLINED"));
    }

    #[test]
    fn rsvp_includes_organizer() {
        let rsvp = generate_rsvp(&sample_event(), "bob@example.com", &RsvpStatus::Accepted);
        assert!(rsvp.contains("alice@example.com"));
    }

    #[test]
    fn rsvp_preserves_cn() {
        let rsvp = generate_rsvp(&sample_event(), "bob@example.com", &RsvpStatus::Accepted);
        assert!(rsvp.contains("CN=Bob"));
    }

    #[test]
    fn rsvp_unknown_attendee() {
        // Should still work even if email not in original attendees
        let rsvp = generate_rsvp(
            &sample_event(),
            "unknown@example.com",
            &RsvpStatus::Accepted,
        );
        assert!(rsvp.contains("unknown@example.com"));
        assert!(rsvp.contains("ACCEPTED"));
    }

    #[test]
    fn rsvp_is_parseable() {
        let rsvp = generate_rsvp(&sample_event(), "bob@example.com", &RsvpStatus::Accepted);
        assert!(rsvp.starts_with("BEGIN:VCALENDAR"));
        let parsed = parse_ics(&rsvp).unwrap();
        assert_eq!(parsed.uid, "test-uid-123@example.com");
        assert_eq!(parsed.method, "REPLY");
    }

    #[test]
    fn rsvp_no_dtend() {
        let event = parse_ics(SAMPLE_ICS_NO_DTEND).unwrap();
        let rsvp = generate_rsvp(&event, "bob@example.com", &RsvpStatus::Accepted);
        assert!(rsvp.contains("METHOD:REPLY"));
        assert!(!rsvp.contains("DTEND"));
    }

    // --- extract_property prefix false-positive tests ---

    #[test]
    fn property_prefix_not_confused_with_longer_name() {
        // "UIDX:something" should NOT match when extracting "UID"
        let ics = "\
BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
METHOD:REQUEST\r\n\
BEGIN:VEVENT\r\n\
UIDX:wrong-value\r\n\
UID:correct-uid@example.com\r\n\
DTSTART:20260215T100000Z\r\n\
SUMMARY:Test\r\n\
ORGANIZER:mailto:org@example.com\r\n\
SEQUENCE:0\r\n\
END:VEVENT\r\n\
END:VCALENDAR";
        let event = parse_ics(ics).unwrap();
        assert_eq!(event.uid, "correct-uid@example.com");
    }

    #[test]
    fn datetime_property_prefix_not_confused() {
        // DTSTARTX should not match DTSTART
        let ics = "\
BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
METHOD:REQUEST\r\n\
BEGIN:VEVENT\r\n\
UID:prefix-test@example.com\r\n\
DTSTARTX:19700101T000000Z\r\n\
DTSTART:20260301T140000Z\r\n\
SUMMARY:Prefix Test\r\n\
ORGANIZER:mailto:org@example.com\r\n\
SEQUENCE:0\r\n\
END:VEVENT\r\n\
END:VCALENDAR";
        let event = parse_ics(ics).unwrap();
        assert_eq!(event.dtstart.year(), 2026);
        assert_eq!(event.dtstart.month(), 3);
    }

    // --- invitation lifecycle tests ---
    // The whole auto-add/remove/re-add flow depends on one invariant:
    // the UID never changes across parse → rsvp → parse cycles.
    // CalDAV uses UID as the filename, so if it drifts, we'd create
    // orphan events or fail to delete the right one.

    #[test]
    fn uid_stable_through_full_rsvp_lifecycle() {
        let original = parse_ics(SAMPLE_ICS).unwrap();
        let uid = &original.uid;

        // Accept → parse back
        let accept_ics = generate_rsvp(&original, "bob@example.com", &RsvpStatus::Accepted);
        let accepted = parse_ics(&accept_ics).unwrap();
        assert_eq!(&accepted.uid, uid);

        // Decline → parse back
        let decline_ics = generate_rsvp(&original, "bob@example.com", &RsvpStatus::Declined);
        let declined = parse_ics(&decline_ics).unwrap();
        assert_eq!(&declined.uid, uid);

        // Re-accept after decline → parse back (the mis-click recovery path)
        let reaccept_ics = generate_rsvp(&original, "bob@example.com", &RsvpStatus::Accepted);
        let reaccepted = parse_ics(&reaccept_ics).unwrap();
        assert_eq!(&reaccepted.uid, uid);

        // Tentative → parse back
        let maybe_ics = generate_rsvp(&original, "bob@example.com", &RsvpStatus::Tentative);
        let maybe = parse_ics(&maybe_ics).unwrap();
        assert_eq!(&maybe.uid, uid);
    }

    #[test]
    fn rsvp_always_produces_reply_method() {
        // RSVP responses must be METHOD:REPLY, never REQUEST.
        // If a REPLY leaked back as REQUEST, the auto-add path
        // would fire when viewing a sent RSVP email — creating
        // duplicate calendar entries.
        let event = sample_event();
        for status in &[
            RsvpStatus::Accepted,
            RsvpStatus::Tentative,
            RsvpStatus::Declined,
        ] {
            let ics = generate_rsvp(&event, "bob@example.com", status);
            let parsed = parse_ics(&ics).unwrap();
            assert_eq!(
                parsed.method,
                "REPLY",
                "RSVP with status {} must be REPLY",
                status.as_ics_str()
            );
            assert_ne!(parsed.method, "REQUEST");
        }
    }

    #[test]
    fn cancel_method_parsed_correctly() {
        // Auto-add gates on method == "REQUEST". Cancelled events
        // must parse as "CANCEL" so they don't get auto-added.
        let ics = "\
BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
METHOD:CANCEL\r\n\
BEGIN:VEVENT\r\n\
UID:cancel-uid@example.com\r\n\
DTSTART:20260215T100000Z\r\n\
SUMMARY:Cancelled Meeting\r\n\
ORGANIZER;CN=Alice:mailto:alice@example.com\r\n\
SEQUENCE:1\r\n\
END:VEVENT\r\n\
END:VCALENDAR";
        let event = parse_ics(ics).unwrap();
        assert_eq!(event.method, "CANCEL");
        assert_ne!(event.method, "REQUEST");
    }

    #[test]
    fn reply_method_not_request() {
        // When we receive a REPLY (someone else RSVPing), it must
        // not trigger auto-add. Verify parsing preserves the method.
        let ics = "\
BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
METHOD:REPLY\r\n\
BEGIN:VEVENT\r\n\
UID:reply-uid@example.com\r\n\
DTSTART:20260215T100000Z\r\n\
SUMMARY:Someone Replied\r\n\
ORGANIZER;CN=Alice:mailto:alice@example.com\r\n\
ATTENDEE;CN=Bob;PARTSTAT=ACCEPTED:mailto:bob@example.com\r\n\
SEQUENCE:0\r\n\
END:VEVENT\r\n\
END:VCALENDAR";
        let event = parse_ics(ics).unwrap();
        assert_eq!(event.method, "REPLY");
        assert_ne!(event.method, "REQUEST");
    }

    #[test]
    fn no_method_defaults_not_request() {
        // Some ICS files omit METHOD entirely (e.g. .ics file exports).
        // These should NOT trigger auto-add.
        let ics = "\
BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
BEGIN:VEVENT\r\n\
UID:no-method-uid@example.com\r\n\
DTSTART:20260215T100000Z\r\n\
SUMMARY:No Method\r\n\
ORGANIZER;CN=Alice:mailto:alice@example.com\r\n\
SEQUENCE:0\r\n\
END:VEVENT\r\n\
END:VCALENDAR";
        let event = parse_ics(ics).unwrap();
        assert_ne!(event.method, "REQUEST");
    }

    #[test]
    fn uid_with_special_chars_survives_rsvp() {
        // Real-world UIDs often contain @, dots, slashes, etc.
        // These become part of the CalDAV filename, so they must
        // survive the generate_rsvp → parse_ics round-trip intact.
        let ics = "\
BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
METHOD:REQUEST\r\n\
BEGIN:VEVENT\r\n\
UID:040000008200E00074C5B7101A82E0080000000060A7B920@calendar.google.com/extra\r\n\
DTSTART:20260215T100000Z\r\n\
SUMMARY:Real World UID\r\n\
ORGANIZER;CN=Alice:mailto:alice@example.com\r\n\
ATTENDEE;CN=Bob;PARTSTAT=NEEDS-ACTION:mailto:bob@example.com\r\n\
SEQUENCE:0\r\n\
END:VEVENT\r\n\
END:VCALENDAR";
        let event = parse_ics(ics).unwrap();
        let original_uid = event.uid.clone();

        let rsvp = generate_rsvp(&event, "bob@example.com", &RsvpStatus::Accepted);
        let parsed = parse_ics(&rsvp).unwrap();
        assert_eq!(parsed.uid, original_uid);
    }

    #[test]
    fn decline_rsvp_still_contains_event_metadata() {
        // When we decline and the backend calls remove_from_calendar,
        // it needs the UID. But the RSVP handler also returns the
        // parsed event to the frontend. Verify the decline ICS has
        // enough data to parse fully (summary, organizer, etc).
        let event = sample_event();
        let ics = generate_rsvp(&event, "bob@example.com", &RsvpStatus::Declined);
        let parsed = parse_ics(&ics).unwrap();

        assert_eq!(parsed.uid, event.uid);
        assert_eq!(parsed.summary, event.summary);
        assert_eq!(parsed.organizer_email, event.organizer_email);
        assert_eq!(parsed.method, "REPLY");
        assert_eq!(parsed.attendees.len(), 1);
        assert_eq!(parsed.attendees[0].status, "DECLINED");
    }

    // --- update_partstat tests ---

    #[test]
    fn update_partstat_changes_matching_attendee() {
        let result = update_partstat(SAMPLE_ICS, "bob@example.com", &RsvpStatus::Accepted);
        // Bob's line should now have ACCEPTED
        for line in result.lines() {
            let trimmed = line.trim_end_matches('\r');
            if trimmed.starts_with("ATTENDEE") && trimmed.to_lowercase().contains("bob@example.com")
            {
                assert!(
                    trimmed.contains("PARTSTAT=ACCEPTED"),
                    "Bob's PARTSTAT should be ACCEPTED: {trimmed}"
                );
            }
        }
    }

    #[test]
    fn update_partstat_preserves_other_attendees() {
        let result = update_partstat(SAMPLE_ICS, "bob@example.com", &RsvpStatus::Accepted);
        // Carol's line should still have ACCEPTED (unchanged)
        for line in result.lines() {
            let trimmed = line.trim_end_matches('\r');
            if trimmed.starts_with("ATTENDEE")
                && trimmed.to_lowercase().contains("carol@example.com")
            {
                assert!(
                    trimmed.contains("PARTSTAT=ACCEPTED"),
                    "Carol's PARTSTAT should be unchanged: {trimmed}"
                );
            }
        }
    }

    #[test]
    fn update_partstat_case_insensitive_email() {
        let result = update_partstat(SAMPLE_ICS, "Bob@Example.COM", &RsvpStatus::Tentative);
        for line in result.lines() {
            let trimmed = line.trim_end_matches('\r');
            if trimmed.starts_with("ATTENDEE") && trimmed.to_lowercase().contains("bob@example.com")
            {
                assert!(
                    trimmed.contains("PARTSTAT=TENTATIVE"),
                    "Case-insensitive match should update PARTSTAT: {trimmed}"
                );
            }
        }
    }

    #[test]
    fn update_partstat_preserves_full_ics() {
        let result = update_partstat(SAMPLE_ICS, "bob@example.com", &RsvpStatus::Accepted);
        assert!(result.contains("LOCATION:Conference Room B"));
        assert!(result.contains("DESCRIPTION:Daily standup meeting"));
        assert!(result.contains("ORGANIZER;CN=Alice:mailto:alice@example.com"));
        assert!(result.contains("SUMMARY:Team Standup"));
        assert!(result.contains("UID:test-uid-123@example.com"));
        // Should still be parseable
        let event = parse_ics(&result).unwrap();
        assert_eq!(event.uid, "test-uid-123@example.com");
        assert_eq!(event.attendees.len(), 2);
    }

    #[test]
    fn update_partstat_no_match_returns_unchanged() {
        let result = update_partstat(SAMPLE_ICS, "nobody@example.com", &RsvpStatus::Accepted);
        // unfold_lines normalizes the output, so compare against unfolded input
        assert_eq!(result, unfold_lines(SAMPLE_ICS));
    }

    #[test]
    fn update_partstat_handles_folded_attendee() {
        let folded_ics = "BEGIN:VCALENDAR\r\n\
            VERSION:2.0\r\n\
            METHOD:REQUEST\r\n\
            BEGIN:VEVENT\r\n\
            UID:folded-test@example.com\r\n\
            DTSTART:20250115T100000Z\r\n\
            SUMMARY:Folded Test\r\n\
            ORGANIZER;CN=Alice:mailto:alice@example.com\r\n\
            ATTENDEE;CN=Bob;PARTSTAT=\r\n NEEDS-ACTION:mailto:bob@example.com\r\n\
            END:VEVENT\r\n\
            END:VCALENDAR";
        let result = update_partstat(folded_ics, "bob@example.com", &RsvpStatus::Accepted);
        assert!(
            result.contains("PARTSTAT=ACCEPTED"),
            "folded PARTSTAT should be updated: {result}"
        );
        assert!(result.contains("mailto:bob@example.com"));
    }

    // --- STATUS:CANCELLED normalization tests ---

    #[test]
    fn status_cancelled_normalizes_method_to_cancel() {
        // Services like Lumo send METHOD:REQUEST with STATUS:CANCELLED
        // in the VEVENT. We normalize to method == "CANCEL" so the
        // auto-remove path fires correctly.
        let ics = "\
BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
METHOD:REQUEST\r\n\
BEGIN:VEVENT\r\n\
UID:lumo-cancel-uid@example.com\r\n\
DTSTART:20260215T100000Z\r\n\
SUMMARY:Cancelled via Lumo\r\n\
STATUS:CANCELLED\r\n\
ORGANIZER;CN=Alice:mailto:alice@example.com\r\n\
SEQUENCE:1\r\n\
END:VEVENT\r\n\
END:VCALENDAR";
        let event = parse_ics(ics).unwrap();
        assert_eq!(event.method, "CANCEL");
    }

    #[test]
    fn status_confirmed_preserves_method() {
        let ics = "\
BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
METHOD:REQUEST\r\n\
BEGIN:VEVENT\r\n\
UID:confirmed-uid@example.com\r\n\
DTSTART:20260215T100000Z\r\n\
SUMMARY:Confirmed Meeting\r\n\
STATUS:CONFIRMED\r\n\
ORGANIZER;CN=Alice:mailto:alice@example.com\r\n\
SEQUENCE:0\r\n\
END:VEVENT\r\n\
END:VCALENDAR";
        let event = parse_ics(ics).unwrap();
        assert_eq!(event.method, "REQUEST");
    }

    #[test]
    fn no_status_preserves_method() {
        // Most normal invitations have no STATUS field
        let event = parse_ics(SAMPLE_ICS).unwrap();
        assert_eq!(event.method, "REQUEST");
    }

    // --- strip_method tests ---

    #[test]
    fn strip_method_removes_method_request() {
        let ics =
            "BEGIN:VCALENDAR\nVERSION:2.0\nMETHOD:REQUEST\nBEGIN:VEVENT\nEND:VEVENT\nEND:VCALENDAR";
        let result = strip_method(ics);
        assert!(!result.contains("METHOD:"));
        assert!(result.contains("BEGIN:VCALENDAR"));
        assert!(result.contains("BEGIN:VEVENT"));
    }

    #[test]
    fn strip_method_no_method_unchanged() {
        let ics = "BEGIN:VCALENDAR\nVERSION:2.0\nBEGIN:VEVENT\nEND:VEVENT\nEND:VCALENDAR";
        let result = strip_method(ics);
        assert_eq!(result, ics);
    }

    #[test]
    fn strip_method_crlf_line_endings() {
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nEND:VEVENT\r\nEND:VCALENDAR";
        let result = strip_method(ics);
        assert!(!result.contains("METHOD:"));
        assert!(result.contains("BEGIN:VCALENDAR"));
    }

    // --- invite_update_decision tests (RFC 5546 SEQUENCE + anti-spoof) ---

    #[test]
    fn invite_decision_no_stored_event() {
        // Nothing stored yet — the normal first-time auto-add path.
        assert_eq!(
            invite_update_decision(None, 0, None, Some("alice@example.com")),
            InviteAction::NoStored
        );
        // A high incoming SEQUENCE is still NoStored when nothing is stored.
        assert_eq!(
            invite_update_decision(
                None,
                5,
                Some("alice@example.com"),
                Some("alice@example.com")
            ),
            InviteAction::NoStored
        );
    }

    #[test]
    fn invite_decision_unchanged_when_seq_not_higher() {
        // Equal SEQUENCE — idempotent re-receipt of the same invite.
        assert_eq!(
            invite_update_decision(
                Some(2),
                2,
                Some("alice@example.com"),
                Some("alice@example.com")
            ),
            InviteAction::Unchanged
        );
        // Lower incoming SEQUENCE — out-of-order / replayed older invite.
        assert_eq!(
            invite_update_decision(
                Some(3),
                1,
                Some("alice@example.com"),
                Some("alice@example.com")
            ),
            InviteAction::Unchanged
        );
    }

    #[test]
    fn invite_decision_update_when_organizer_matches() {
        assert_eq!(
            invite_update_decision(
                Some(0),
                1,
                Some("alice@example.com"),
                Some("alice@example.com")
            ),
            InviteAction::Update
        );
    }

    #[test]
    fn invite_decision_update_organizer_match_is_case_insensitive() {
        assert_eq!(
            invite_update_decision(
                Some(1),
                2,
                Some("Alice@Example.COM"),
                Some("alice@example.com")
            ),
            InviteAction::Update
        );
    }

    #[test]
    fn invite_decision_reject_spoof_on_sender_mismatch() {
        // Higher SEQUENCE but the sender is not the stored organizer: a sender
        // who knows the UID trying to overwrite a real event (bwjm T9.4).
        assert_eq!(
            invite_update_decision(
                Some(0),
                9,
                Some("alice@example.com"),
                Some("mallory@evil.example")
            ),
            InviteAction::RejectSpoof
        );
    }

    #[test]
    fn invite_decision_reject_spoof_on_missing_organizer() {
        // No stored organizer to verify against — can't trust the update.
        assert_eq!(
            invite_update_decision(Some(0), 1, None, Some("alice@example.com")),
            InviteAction::RejectSpoof
        );
        // An empty stored organizer counts as missing.
        assert_eq!(
            invite_update_decision(Some(0), 1, Some(""), Some("alice@example.com")),
            InviteAction::RejectSpoof
        );
    }

    #[test]
    fn invite_decision_reject_spoof_on_missing_sender() {
        assert_eq!(
            invite_update_decision(Some(0), 1, Some("alice@example.com"), None),
            InviteAction::RejectSpoof
        );
    }

    // --- timezone handling tests ---

    #[test]
    fn parse_utc_z_suffix_unchanged() {
        // Z suffix = already UTC, should parse as-is
        let event = parse_ics(SAMPLE_ICS).unwrap();
        assert_eq!(event.dtstart.hour(), 10);
        assert_eq!(event.dtstart.minute(), 0);
    }

    #[test]
    fn parse_tzid_converts_to_utc() {
        // DTSTART with TZID=America/New_York and a VTIMEZONE block.
        // 10:00 EST (UTC-5) should become 15:00 UTC.
        let ics = "\
BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
METHOD:REQUEST\r\n\
BEGIN:VTIMEZONE\r\n\
TZID:America/New_York\r\n\
BEGIN:STANDARD\r\n\
DTSTART:19701101T020000\r\n\
UTCOFFSETTO:-0500\r\n\
UTCOFFSETFROM:-0400\r\n\
END:STANDARD\r\n\
BEGIN:DAYLIGHT\r\n\
DTSTART:19700308T020000\r\n\
UTCOFFSETTO:-0400\r\n\
UTCOFFSETFROM:-0500\r\n\
END:DAYLIGHT\r\n\
END:VTIMEZONE\r\n\
BEGIN:VEVENT\r\n\
UID:tz-test@example.com\r\n\
DTSTART;TZID=America/New_York:20260215T100000\r\n\
DTEND;TZID=America/New_York:20260215T110000\r\n\
SUMMARY:Eastern Time Meeting\r\n\
ORGANIZER;CN=Alice:mailto:alice@example.com\r\n\
SEQUENCE:0\r\n\
END:VEVENT\r\n\
END:VCALENDAR";
        let event = parse_ics(ics).unwrap();
        // 10:00 EST = 15:00 UTC
        assert_eq!(event.dtstart.hour(), 15);
        assert_eq!(event.dtstart.minute(), 0);
        // 11:00 EST = 16:00 UTC
        let dtend = event.dtend.unwrap();
        assert_eq!(dtend.hour(), 16);
    }

    #[test]
    fn parse_tzid_positive_offset() {
        // TZID=Asia/Kolkata (UTC+0530). 10:00 IST should become 04:30 UTC.
        let ics = "\
BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
METHOD:REQUEST\r\n\
BEGIN:VTIMEZONE\r\n\
TZID:Asia/Kolkata\r\n\
BEGIN:STANDARD\r\n\
DTSTART:19700101T000000\r\n\
UTCOFFSETTO:+0530\r\n\
UTCOFFSETFROM:+0530\r\n\
END:STANDARD\r\n\
END:VTIMEZONE\r\n\
BEGIN:VEVENT\r\n\
UID:ist-test@example.com\r\n\
DTSTART;TZID=Asia/Kolkata:20260215T100000\r\n\
SUMMARY:IST Meeting\r\n\
ORGANIZER;CN=Alice:mailto:alice@example.com\r\n\
SEQUENCE:0\r\n\
END:VEVENT\r\n\
END:VCALENDAR";
        let event = parse_ics(ics).unwrap();
        assert_eq!(event.dtstart.hour(), 4);
        assert_eq!(event.dtstart.minute(), 30);
    }

    #[test]
    fn parse_floating_time_uses_local_tz() {
        // No Z, no TZID — floating time. Should be interpreted as system local.
        // We can't assert the exact UTC hour (depends on where tests run),
        // but we can verify it parses and the offset is applied.
        let ics = "\
BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
METHOD:REQUEST\r\n\
BEGIN:VEVENT\r\n\
UID:floating-test@example.com\r\n\
DTSTART:20260215T100000\r\n\
SUMMARY:Floating Time\r\n\
ORGANIZER;CN=Alice:mailto:alice@example.com\r\n\
SEQUENCE:0\r\n\
END:VEVENT\r\n\
END:VCALENDAR";
        let event = parse_ics(ics).unwrap();
        // Verify the local offset was applied: 10:00 local != 10:00 UTC
        // unless we're in UTC. Compute expected value from system tz.
        // Use the offset at the event's date (Feb 15), not the current date,
        // so this test is correct across DST boundaries.
        let event_date = NaiveDateTime::new(
            NaiveDate::from_ymd_opt(2026, 2, 15).unwrap(),
            NaiveTime::from_hms_opt(10, 0, 0).unwrap(),
        );
        let local_offset = Local
            .from_local_datetime(&event_date)
            .earliest()
            .unwrap()
            .offset()
            .local_minus_utc();
        let expected_utc_hour = (10 - local_offset / 3600 + 24) % 24;
        assert_eq!(event.dtstart.hour() as i32, expected_utc_hour);
    }

    #[test]
    fn parse_vtimezone_offsets_extracts_multiple() {
        let ics = "\
BEGIN:VCALENDAR\r\n\
BEGIN:VTIMEZONE\r\n\
TZID:America/New_York\r\n\
BEGIN:STANDARD\r\n\
UTCOFFSETTO:-0500\r\n\
UTCOFFSETFROM:-0400\r\n\
END:STANDARD\r\n\
END:VTIMEZONE\r\n\
BEGIN:VTIMEZONE\r\n\
TZID:Europe/London\r\n\
BEGIN:STANDARD\r\n\
UTCOFFSETTO:+0000\r\n\
UTCOFFSETFROM:+0100\r\n\
END:STANDARD\r\n\
END:VTIMEZONE\r\n\
END:VCALENDAR";
        let offsets = parse_vtimezone_offsets(ics);
        assert_eq!(offsets.len(), 2);
        assert_eq!(offsets["America/New_York"].local_minus_utc(), -5 * 3600);
        assert_eq!(offsets["Europe/London"].local_minus_utc(), 0);
    }

    #[test]
    fn parse_utc_offset_various_formats() {
        assert_eq!(parse_utc_offset("+0000").unwrap().local_minus_utc(), 0);
        assert_eq!(
            parse_utc_offset("-0500").unwrap().local_minus_utc(),
            -5 * 3600
        );
        assert_eq!(
            parse_utc_offset("+0530").unwrap().local_minus_utc(),
            5 * 3600 + 30 * 60
        );
        assert_eq!(
            parse_utc_offset("+1200").unwrap().local_minus_utc(),
            12 * 3600
        );
        assert!(parse_utc_offset("bad").is_none());
        assert!(parse_utc_offset("").is_none());
    }

    use chrono::{Datelike, Timelike};

    // =========================================================================
    // RSVP lifecycle verification tests (THE-192)
    //
    // These simulate the full RSVP flow without network calls:
    //   parse ICS → RSVP → update CalDAV ICS → re-parse → verify persisted status
    // =========================================================================

    const INVITE_ICS: &str = "\
BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
METHOD:REQUEST\r\n\
BEGIN:VEVENT\r\n\
UID:lifecycle-test@example.com\r\n\
DTSTART:20260301T140000Z\r\n\
DTEND:20260301T150000Z\r\n\
SUMMARY:Team Standup\r\n\
LOCATION:Room 42\r\n\
ORGANIZER;CN=Alice:mailto:alice@example.com\r\n\
ATTENDEE;CN=Bob;PARTSTAT=NEEDS-ACTION:mailto:bob@example.com\r\n\
ATTENDEE;CN=Carol;PARTSTAT=NEEDS-ACTION:mailto:carol@example.com\r\n\
SEQUENCE:0\r\n\
END:VEVENT\r\n\
END:VCALENDAR";

    /// Verify: accept RSVP → persist to CalDAV → re-read → status is ACCEPTED
    #[test]
    fn lifecycle_accept_persists_and_reads_back() {
        let event = parse_ics(INVITE_ICS).unwrap();
        assert_eq!(event.user_rsvp_status, None);

        // Simulate backend rsvp(): update_partstat writes to CalDAV
        let updated_ics = update_partstat(INVITE_ICS, "bob@example.com", &RsvpStatus::Accepted);

        // Simulate get_email(): re-parse the stored ICS (what CalDAV returns)
        let re_read = parse_ics(&updated_ics).unwrap();
        let bob = re_read
            .attendees
            .iter()
            .find(|a| a.email == "bob@example.com")
            .unwrap();
        assert_eq!(bob.status, "ACCEPTED");

        // Carol is unchanged
        let carol = re_read
            .attendees
            .iter()
            .find(|a| a.email == "carol@example.com")
            .unwrap();
        assert_eq!(carol.status, "NEEDS-ACTION");
    }

    /// Verify: accept → navigate away → return → change to Decline → re-read
    #[test]
    fn lifecycle_change_accept_to_decline() {
        // First: accept
        let after_accept = update_partstat(INVITE_ICS, "bob@example.com", &RsvpStatus::Accepted);
        let event = parse_ics(&after_accept).unwrap();
        assert_eq!(
            event
                .attendees
                .iter()
                .find(|a| a.email == "bob@example.com")
                .unwrap()
                .status,
            "ACCEPTED"
        );

        // Then: change to decline (decline removes from calendar, but the ICS
        // was previously stored with ACCEPTED — this test verifies the update path
        // works on already-updated ICS)
        let after_decline =
            update_partstat(&after_accept, "bob@example.com", &RsvpStatus::Declined);
        let event2 = parse_ics(&after_decline).unwrap();
        assert_eq!(
            event2
                .attendees
                .iter()
                .find(|a| a.email == "bob@example.com")
                .unwrap()
                .status,
            "DECLINED"
        );
    }

    /// Verify: decline → change back to accept → status is ACCEPTED
    #[test]
    fn lifecycle_re_rsvp_decline_then_accept() {
        let after_decline = update_partstat(INVITE_ICS, "bob@example.com", &RsvpStatus::Declined);

        // Re-accept: the original ICS is used for the upsert (not the declined one,
        // since decline removes from calendar). This tests accept from the original invite.
        let after_reaccept = update_partstat(INVITE_ICS, "bob@example.com", &RsvpStatus::Accepted);
        let event = parse_ics(&after_reaccept).unwrap();
        assert_eq!(
            event
                .attendees
                .iter()
                .find(|a| a.email == "bob@example.com")
                .unwrap()
                .status,
            "ACCEPTED"
        );

        // Also verify that updating the declined version works too
        let after_reaccept2 =
            update_partstat(&after_decline, "bob@example.com", &RsvpStatus::Accepted);
        let event2 = parse_ics(&after_reaccept2).unwrap();
        assert_eq!(
            event2
                .attendees
                .iter()
                .find(|a| a.email == "bob@example.com")
                .unwrap()
                .status,
            "ACCEPTED"
        );
    }

    /// Verify: accept → re-read → CalDAV status matches rsvp() response
    #[test]
    fn lifecycle_rsvp_response_matches_persisted_status() {
        for status in &[
            RsvpStatus::Accepted,
            RsvpStatus::Tentative,
            RsvpStatus::Declined,
        ] {
            // What rsvp() returns to the frontend
            let rsvp_response_status = status.as_ics_str().to_string();

            // What CalDAV would store and get_rsvp_status() would return
            let updated_ics = update_partstat(INVITE_ICS, "bob@example.com", status);
            let re_read = parse_ics(&updated_ics).unwrap();
            let persisted_status = re_read
                .attendees
                .iter()
                .find(|a| a.email == "bob@example.com")
                .unwrap()
                .status
                .clone();

            assert_eq!(
                rsvp_response_status, persisted_status,
                "rsvp() response status must match CalDAV-persisted status for {:?}",
                status
            );
        }
    }

    /// Verify: iTIP REPLY method is always REPLY, never REQUEST
    /// (prevents auto-add loop when viewing sent RSVP emails)
    #[test]
    fn lifecycle_rsvp_reply_never_triggers_auto_add() {
        let event = parse_ics(INVITE_ICS).unwrap();
        assert_eq!(event.method, "REQUEST", "original invite should be REQUEST");

        for status in &[
            RsvpStatus::Accepted,
            RsvpStatus::Tentative,
            RsvpStatus::Declined,
        ] {
            let reply_ics = generate_rsvp(&event, "bob@example.com", status);
            let reply = parse_ics(&reply_ics).unwrap();
            assert_eq!(reply.method, "REPLY");
            assert_ne!(reply.method, "REQUEST");
        }
    }

    /// Verify: CANCEL events don't get RSVP'd
    #[test]
    fn lifecycle_cancel_method_not_request() {
        let cancel_ics = INVITE_ICS.replace("METHOD:REQUEST", "METHOD:CANCEL");
        let event = parse_ics(&cancel_ics).unwrap();
        assert_eq!(event.method, "CANCEL");
        assert_ne!(event.method, "REQUEST");
    }

    /// Verify: user_rsvp_status is always None from parse_ics (populated server-side only)
    #[test]
    fn lifecycle_parse_never_sets_user_rsvp_status() {
        // Even after updating PARTSTAT, parse_ics never sets user_rsvp_status
        let updated = update_partstat(INVITE_ICS, "bob@example.com", &RsvpStatus::Accepted);
        let event = parse_ics(&updated).unwrap();
        assert_eq!(event.user_rsvp_status, None);
    }

    /// Verify: UID survives the full accept→decline→re-accept cycle
    #[test]
    fn lifecycle_uid_stable_through_rsvp_changes() {
        let original = parse_ics(INVITE_ICS).unwrap();
        let uid = &original.uid;

        let after_accept = update_partstat(INVITE_ICS, "bob@example.com", &RsvpStatus::Accepted);
        assert_eq!(parse_ics(&after_accept).unwrap().uid, *uid);

        let after_decline =
            update_partstat(&after_accept, "bob@example.com", &RsvpStatus::Declined);
        assert_eq!(parse_ics(&after_decline).unwrap().uid, *uid);

        let after_reaccept =
            update_partstat(&after_decline, "bob@example.com", &RsvpStatus::Accepted);
        assert_eq!(parse_ics(&after_reaccept).unwrap().uid, *uid);
    }

    /// Verify: update_partstat on CalDAV-stored ICS preserves other attendees
    #[test]
    fn lifecycle_rsvp_does_not_clobber_other_attendees() {
        // Bob accepts
        let after_bob = update_partstat(INVITE_ICS, "bob@example.com", &RsvpStatus::Accepted);
        // Carol declines
        let after_both = update_partstat(&after_bob, "carol@example.com", &RsvpStatus::Declined);

        let event = parse_ics(&after_both).unwrap();
        let bob = event
            .attendees
            .iter()
            .find(|a| a.email == "bob@example.com")
            .unwrap();
        let carol = event
            .attendees
            .iter()
            .find(|a| a.email == "carol@example.com")
            .unwrap();
        assert_eq!(bob.status, "ACCEPTED");
        assert_eq!(carol.status, "DECLINED");
    }

    // --- generate_invite + generate_rsvp_with_tz tests ---

    #[test]
    fn invite_emits_tzid_dtstart_and_vtimezone() {
        let tz: Tz = "America/Los_Angeles".parse().unwrap();
        let start = tz.with_ymd_and_hms(2026, 6, 1, 10, 0, 0).unwrap();
        let end = tz.with_ymd_and_hms(2026, 6, 1, 11, 0, 0).unwrap();
        let ics = generate_invite(
            "alice@example.com",
            Some("Alice"),
            "Sync up",
            None,
            Some("Coffee shop"),
            start,
            end,
            &[Attendee {
                email: "bob@example.com".into(),
                name: Some("Bob".into()),
                status: "NEEDS-ACTION".into(),
            }],
            Some("test-uid"),
        );
        assert!(ics.contains("METHOD:REQUEST"));
        assert!(ics.contains("DTSTART;TZID=America/Los_Angeles:20260601T100000"));
        assert!(ics.contains("DTEND;TZID=America/Los_Angeles:20260601T110000"));
        assert!(ics.contains("UID:test-uid"));
        assert!(ics.contains("SUMMARY:Sync up"));
        assert!(ics.contains("LOCATION:Coffee shop"));
        assert!(ics.contains("BEGIN:VTIMEZONE"));
        assert!(ics.contains("TZID:America/Los_Angeles"));
    }

    #[test]
    fn invite_roundtrips_through_parser() {
        let tz: Tz = "Europe/London".parse().unwrap();
        let start = tz.with_ymd_and_hms(2026, 7, 1, 14, 30, 0).unwrap();
        let end = tz.with_ymd_and_hms(2026, 7, 1, 15, 30, 0).unwrap();
        let ics = generate_invite(
            "alice@example.com",
            None,
            "Quarterly review",
            None,
            None,
            start,
            end,
            &[Attendee {
                email: "bob@example.com".into(),
                name: None,
                status: "NEEDS-ACTION".into(),
            }],
            None,
        );
        let parsed = parse_ics(&ics).unwrap();
        // 14:30 London in July is BST (UTC+1) → 13:30 UTC.
        assert_eq!(parsed.dtstart.hour(), 13);
        assert_eq!(parsed.dtstart.minute(), 30);
    }

    #[test]
    fn vtimezone_includes_x_lic_location() {
        // Roborev 186 #9: strict parsers caching VTIMEZONE by TZID can use
        // X-LIC-LOCATION to map back to IANA rules.
        let tz: Tz = "America/New_York".parse().unwrap();
        let dtstart = tz.with_ymd_and_hms(2026, 7, 4, 12, 0, 0).unwrap();
        let dtend = tz.with_ymd_and_hms(2026, 7, 4, 13, 0, 0).unwrap();
        let ics = generate_invite(
            "a@x.com",
            None,
            "Test",
            None,
            None,
            dtstart,
            dtend,
            &[],
            None,
        );
        assert!(
            ics.contains("X-LIC-LOCATION:America/New_York"),
            "VTIMEZONE must include X-LIC-LOCATION for IANA mapping"
        );
    }

    #[test]
    fn rsvp_with_tz_emits_tzid_not_z() {
        let tz: Tz = "America/New_York".parse().unwrap();
        let rsvp = generate_rsvp_with_tz(
            &sample_event(),
            "bob@example.com",
            &RsvpStatus::Accepted,
            tz,
        );
        assert!(rsvp.contains("METHOD:REPLY"));
        assert!(rsvp.contains("DTSTART;TZID=America/New_York:"));
        assert!(!rsvp.contains("DTSTART:20260215T100000Z"));
        assert!(rsvp.contains("PARTSTAT=ACCEPTED"));
    }

    #[test]
    fn rsvp_with_tz_roundtrip_preserves_instant() {
        let tz: Tz = "America/New_York".parse().unwrap();
        let original = sample_event();
        let rsvp = generate_rsvp_with_tz(&original, "bob@example.com", &RsvpStatus::Accepted, tz);
        let parsed = parse_ics(&rsvp).unwrap();
        // The UTC instant must round-trip even though wall-clock is now NYC.
        assert_eq!(parsed.dtstart, original.dtstart);
        assert_eq!(parsed.method, "REPLY");
    }

    // ---- ICS property-injection hardening (roborev 186 #2) ----

    #[test]
    fn invite_with_crlf_in_attendee_name_does_not_inject_property() {
        let tz: Tz = "America/New_York".parse().unwrap();
        let dtstart = tz.with_ymd_and_hms(2026, 2, 15, 10, 0, 0).unwrap();
        let dtend = tz.with_ymd_and_hms(2026, 2, 15, 11, 0, 0).unwrap();
        let attendees = vec![Attendee {
            email: "victim@example.com".into(),
            // Classic CRLF-injection payload: a malicious calendar invite
            // would attempt to inject a second ATTENDEE line that the
            // receiver's client treats as auto-accepted.
            name: Some(
                "Bob\r\nATTENDEE;PARTSTAT=ACCEPTED;CN=Spoofed:mailto:attacker@evil.example".into(),
            ),
            status: "NEEDS-ACTION".into(),
        }];
        let ics = generate_invite(
            "alice@example.com",
            Some("Alice"),
            "Meeting",
            None,
            None,
            dtstart,
            dtend,
            &attendees,
            None,
        );
        // Exactly one line begins with ATTENDEE — the legitimate one.
        // The attacker's name may appear *inside* the (quoted) CN value
        // as harmless text; the security property is that no second
        // property line is emitted AND a real ICS parser sees only the
        // legitimate attendee.
        let attendee_lines = ics
            .split("\r\n")
            .filter(|line| line.starts_with("ATTENDEE"))
            .count();
        assert_eq!(
            attendee_lines, 1,
            "must not inject a second ATTENDEE line via CN= CRLF injection"
        );
        let parsed = parse_ics(&ics).expect("must round-trip through parser");
        assert_eq!(
            parsed.attendees.len(),
            1,
            "parser must see exactly one attendee, not the smuggled second"
        );
        assert_eq!(
            parsed.attendees[0].email, "victim@example.com",
            "parser must resolve to the legitimate attendee email"
        );
    }

    #[test]
    fn invite_quotes_cn_containing_param_terminators() {
        // RFC 5545 §3.1: param values containing ',' ':' ';' must be DQUOTE-wrapped.
        let tz: Tz = "America/New_York".parse().unwrap();
        let dtstart = tz.with_ymd_and_hms(2026, 2, 15, 10, 0, 0).unwrap();
        let dtend = tz.with_ymd_and_hms(2026, 2, 15, 11, 0, 0).unwrap();
        let attendees = vec![Attendee {
            email: "bob@example.com".into(),
            name: Some("Smith, Bob".into()),
            status: "NEEDS-ACTION".into(),
        }];
        let ics = generate_invite(
            "alice@example.com",
            None,
            "Meeting",
            None,
            None,
            dtstart,
            dtend,
            &attendees,
            None,
        );
        assert!(
            ics.contains(r#";CN="Smith, Bob""#),
            "CN containing ',' must be DQUOTE-wrapped, got: {ics}"
        );
    }

    #[test]
    fn rsvp_with_crlf_in_organizer_does_not_inject_property() {
        let tz: Tz = "America/New_York".parse().unwrap();
        let mut evil = sample_event();
        evil.organizer_email = "alice@example.com\r\nATTENDEE:mailto:spoof@evil.example".into();
        let rsvp = generate_rsvp_with_tz(&evil, "bob@example.com", &RsvpStatus::Accepted, tz);
        // Exactly one ORGANIZER property line. The injection's `ATTENDEE:`
        // and the trailing `mailto:` may survive as harmless characters
        // inside the ORGANIZER value (no parser recognizes them as a new
        // property after `\r\n` stripping); the security property is that
        // no second property line appears.
        let organizer_lines = rsvp
            .split("\r\n")
            .filter(|line| line.starts_with("ORGANIZER"))
            .count();
        assert_eq!(organizer_lines, 1);
        for line in rsvp.split("\r\n") {
            assert!(
                !line.starts_with("ATTENDEE") || !line.contains(":mailto:spoof@evil.example"),
                "smuggled mailto must not become a standalone ATTENDEE line"
            );
        }
    }
}
