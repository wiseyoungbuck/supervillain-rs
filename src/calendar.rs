use crate::types::{Attendee, CalendarEvent};
use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Utc};

// =============================================================================
// ICS Parsing (hand-rolled)
// =============================================================================

pub fn parse_ics(data: &str) -> Option<CalendarEvent> {
    let data = data.trim();
    if !data.contains("BEGIN:VCALENDAR") {
        return None;
    }

    // Extract METHOD from VCALENDAR level
    let method = extract_property(data, "METHOD").unwrap_or_else(|| "REQUEST".into());

    // Find VEVENT block
    let vevent_start = data.find("BEGIN:VEVENT")?;
    let vevent_end = data.find("END:VEVENT")?;
    let vevent = &data[vevent_start..vevent_end + "END:VEVENT".len()];

    // Unfold lines (RFC 5545: continuation lines start with space or tab)
    let unfolded = unfold_lines(vevent);

    let uid = extract_property(&unfolded, "UID")?;
    let summary = extract_property(&unfolded, "SUMMARY").unwrap_or_default();
    let location = extract_property(&unfolded, "LOCATION");
    let description = extract_property(&unfolded, "DESCRIPTION");
    let sequence: i32 = extract_property(&unfolded, "SEQUENCE")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let dtstart = parse_ics_datetime_property(&unfolded, "DTSTART")?;
    let dtend = parse_ics_datetime_property(&unfolded, "DTEND");

    let (organizer_email, organizer_name) = parse_organizer(&unfolded);
    let attendees = parse_attendees(&unfolded);

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
    })
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
        if let Some(rest) = line.strip_prefix(name) {
            if let Some(stripped) = rest.strip_prefix(':') {
                return Some(stripped.to_string());
            }
            if rest.starts_with(';') {
                // Has parameters — find the colon after params
                if let Some(colon_pos) = rest.find(':') {
                    return Some(rest[colon_pos + 1..].to_string());
                }
            }
        }
    }
    None
}

fn parse_ics_datetime_property(text: &str, name: &str) -> Option<DateTime<Utc>> {
    // Find the line for this property
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if !line.starts_with(name) {
            continue;
        }

        let rest = &line[name.len()..];
        let value = if let Some(stripped) = rest.strip_prefix(':') {
            stripped
        } else if rest.starts_with(';') {
            rest.find(':').map(|i| &rest[i + 1..])?
        } else {
            continue;
        };

        // Check if VALUE=DATE (all-day event)
        let is_date_only = rest.contains("VALUE=DATE") && !rest.contains("VALUE=DATE-TIME");
        let is_date_only = is_date_only || value.len() == 8; // YYYYMMDD

        if is_date_only {
            let date = NaiveDate::parse_from_str(value.trim(), "%Y%m%d").ok()?;
            let dt = NaiveDateTime::new(date, NaiveTime::from_hms_opt(0, 0, 0).unwrap());
            return Some(DateTime::from_naive_utc_and_offset(dt, Utc));
        }

        // Full datetime: 20260215T100000Z or 20260215T100000
        let value = value.trim().trim_end_matches('Z');
        let dt = NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%S").ok()?;
        return Some(DateTime::from_naive_utc_and_offset(dt, Utc));
    }
    None
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
// RSVP Generation
// =============================================================================

pub fn generate_rsvp(event: &CalendarEvent, attendee_email: &str, status: &str) -> String {
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
        Some(name) => format!(";CN={name}"),
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
        .map(|n| format!(";CN={n}"))
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
         ATTENDEE{cn_param};PARTSTAT={status}:mailto:{attendee_email}\r\n\
         SEQUENCE:{sequence}\r\n\
         END:VEVENT\r\n\
         END:VCALENDAR",
        uid = event.uid,
        dtstart = dtstart,
        dtend_line = dtend_line,
        summary = event.summary,
        organizer_cn = organizer_cn,
        organizer_email = event.organizer_email,
        cn_param = cn_param,
        status = status,
        attendee_email = attendee_email,
        sequence = event.sequence,
    )
}

fn format_ics_datetime(dt: DateTime<Utc>) -> String {
    dt.format("%Y%m%dT%H%M%SZ").to_string()
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

    // --- generate_rsvp tests ---

    fn sample_event() -> CalendarEvent {
        parse_ics(SAMPLE_ICS).unwrap()
    }

    #[test]
    fn rsvp_method_reply() {
        let rsvp = generate_rsvp(&sample_event(), "bob@example.com", "ACCEPTED");
        assert!(rsvp.contains("METHOD:REPLY"));
    }

    #[test]
    fn rsvp_includes_uid() {
        let rsvp = generate_rsvp(&sample_event(), "bob@example.com", "ACCEPTED");
        assert!(rsvp.contains("test-uid-123@example.com"));
    }

    #[test]
    fn rsvp_attendee_accepted() {
        let rsvp = generate_rsvp(&sample_event(), "bob@example.com", "ACCEPTED");
        assert!(rsvp.contains("bob@example.com"));
        assert!(rsvp.contains("ACCEPTED"));
    }

    #[test]
    fn rsvp_tentative() {
        let rsvp = generate_rsvp(&sample_event(), "bob@example.com", "TENTATIVE");
        assert!(rsvp.contains("TENTATIVE"));
    }

    #[test]
    fn rsvp_declined() {
        let rsvp = generate_rsvp(&sample_event(), "bob@example.com", "DECLINED");
        assert!(rsvp.contains("DECLINED"));
    }

    #[test]
    fn rsvp_includes_organizer() {
        let rsvp = generate_rsvp(&sample_event(), "bob@example.com", "ACCEPTED");
        assert!(rsvp.contains("alice@example.com"));
    }

    #[test]
    fn rsvp_preserves_cn() {
        let rsvp = generate_rsvp(&sample_event(), "bob@example.com", "ACCEPTED");
        assert!(rsvp.contains("CN=Bob"));
    }

    #[test]
    fn rsvp_unknown_attendee() {
        // Should still work even if email not in original attendees
        let rsvp = generate_rsvp(&sample_event(), "unknown@example.com", "ACCEPTED");
        assert!(rsvp.contains("unknown@example.com"));
        assert!(rsvp.contains("ACCEPTED"));
    }

    #[test]
    fn rsvp_is_parseable() {
        let rsvp = generate_rsvp(&sample_event(), "bob@example.com", "ACCEPTED");
        assert!(rsvp.starts_with("BEGIN:VCALENDAR"));
        let parsed = parse_ics(&rsvp).unwrap();
        assert_eq!(parsed.uid, "test-uid-123@example.com");
        assert_eq!(parsed.method, "REPLY");
    }

    #[test]
    fn rsvp_no_dtend() {
        let event = parse_ics(SAMPLE_ICS_NO_DTEND).unwrap();
        let rsvp = generate_rsvp(&event, "bob@example.com", "ACCEPTED");
        assert!(rsvp.contains("METHOD:REPLY"));
        assert!(!rsvp.contains("DTEND"));
    }

    use chrono::{Datelike, Timelike};
}
