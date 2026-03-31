//! SMTP invite email composition for calendar events.

use crate::calendar::ics::{
    EventScheduleContext, build_cancelled_scheduling_message, build_scheduling_message,
};
use crate::calendar::types::{CalendarEvent, CalendarEventDraft};

use anyhow::Result;
use std::collections::BTreeSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InviteEmailKind {
    Request,
    Update,
    Cancel,
}

#[derive(Debug, Clone)]
pub struct InviteEmail {
    pub recipients: Vec<String>,
    pub subject: String,
    pub body: String,
    pub attachment_name: String,
    pub attachment_bytes: Vec<u8>,
    pub attachment_mime_type: String,
}

pub fn build_request_invite_email(
    draft: &CalendarEventDraft,
    schedule: &EventScheduleContext,
    stored_ics: &str,
    is_update: bool,
) -> Result<Option<InviteEmail>> {
    let recipients = collect_recipients(
        draft
            .attendees
            .iter()
            .map(|attendee| attendee.email.as_str()),
        Some(schedule.organizer_email.as_str()),
    );
    if recipients.is_empty() {
        return Ok(None);
    }

    let kind = if is_update {
        InviteEmailKind::Update
    } else {
        InviteEmailKind::Request
    };
    let method = "REQUEST";
    let scheduling_ics = build_scheduling_message(stored_ics, method)?;

    Ok(Some(InviteEmail {
        recipients,
        subject: invite_subject(kind, draft.summary.as_str()),
        body: invite_body(
            kind,
            draft.summary.as_str(),
            draft.description.as_deref(),
            draft.location.as_deref(),
            &format_event_window(
                &draft.start_at,
                &draft.end_at,
                draft.timezone.as_deref(),
                draft.all_day,
            ),
            schedule,
        ),
        attachment_name: "invite.ics".to_string(),
        attachment_bytes: scheduling_ics.into_bytes(),
        attachment_mime_type: "text/calendar; method=REQUEST; charset=utf-8".to_string(),
    }))
}

pub fn build_cancel_invite_email(
    current_event: &CalendarEvent,
    schedule: &EventScheduleContext,
) -> Result<Option<InviteEmail>> {
    let recipients = collect_recipients(
        current_event
            .attendees
            .iter()
            .filter_map(|attendee| attendee.email.as_deref()),
        Some(schedule.organizer_email.as_str()),
    );
    build_cancel_invite_email_for_recipients(current_event, schedule, recipients)
}

pub fn build_removed_attendee_cancel_invite_email(
    current_event: &CalendarEvent,
    draft: &CalendarEventDraft,
    schedule: &EventScheduleContext,
) -> Result<Option<InviteEmail>> {
    let recipients =
        collect_removed_recipients(current_event, draft, schedule.organizer_email.as_str());
    build_cancel_invite_email_for_recipients(current_event, schedule, recipients)
}

fn build_cancel_invite_email_for_recipients(
    current_event: &CalendarEvent,
    schedule: &EventScheduleContext,
    recipients: Vec<String>,
) -> Result<Option<InviteEmail>> {
    if recipients.is_empty() {
        return Ok(None);
    }

    let summary = current_event.summary.as_deref().unwrap_or("Untitled event");
    let scheduling_ics =
        build_cancelled_scheduling_message(&current_event.raw_ics, current_event, schedule)?;

    Ok(Some(InviteEmail {
        recipients,
        subject: invite_subject(InviteEmailKind::Cancel, summary),
        body: invite_body(
            InviteEmailKind::Cancel,
            summary,
            current_event.description.as_deref(),
            current_event.location.as_deref(),
            &format_event_window(
                &current_event.start_at_utc,
                &current_event.end_at_utc,
                current_event.timezone.as_deref(),
                current_event.all_day,
            ),
            schedule,
        ),
        attachment_name: "cancel.ics".to_string(),
        attachment_bytes: scheduling_ics.into_bytes(),
        attachment_mime_type: "text/calendar; method=CANCEL; charset=utf-8".to_string(),
    }))
}

fn collect_removed_recipients(
    current_event: &CalendarEvent,
    draft: &CalendarEventDraft,
    organizer_email: &str,
) -> Vec<String> {
    let existing = collect_recipients(
        current_event
            .attendees
            .iter()
            .filter_map(|attendee| attendee.email.as_deref()),
        Some(organizer_email),
    );
    let updated = collect_recipients(
        draft
            .attendees
            .iter()
            .map(|attendee| attendee.email.as_str()),
        Some(organizer_email),
    );

    let updated: BTreeSet<_> = updated.into_iter().collect();
    existing
        .into_iter()
        .filter(|email| !updated.contains(email))
        .collect()
}

fn collect_recipients<'a>(
    emails: impl IntoIterator<Item = &'a str>,
    organizer_email: Option<&str>,
) -> Vec<String> {
    let organizer_email = organizer_email.map(normalize_email);
    let mut seen = BTreeSet::new();

    for email in emails {
        let normalized = normalize_email(email);
        if normalized.is_empty() {
            continue;
        }
        if organizer_email.as_deref() == Some(normalized.as_str()) {
            continue;
        }
        seen.insert(normalized);
    }

    seen.into_iter().collect()
}

fn normalize_email(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

fn invite_subject(kind: InviteEmailKind, summary: &str) -> String {
    match kind {
        InviteEmailKind::Request => format!("Invitation: {summary}"),
        InviteEmailKind::Update => format!("Updated invitation: {summary}"),
        InviteEmailKind::Cancel => format!("Canceled: {summary}"),
    }
}

fn invite_body(
    kind: InviteEmailKind,
    summary: &str,
    description: Option<&str>,
    location: Option<&str>,
    when: &str,
    schedule: &EventScheduleContext,
) -> String {
    let opening = match kind {
        InviteEmailKind::Request => "You are invited to a meeting.",
        InviteEmailKind::Update => "A meeting invitation has been updated.",
        InviteEmailKind::Cancel => "This meeting has been canceled.",
    };

    let mut lines = vec![
        opening.to_string(),
        String::new(),
        format!("Summary: {summary}"),
        format!("When: {when}"),
    ];

    if let Some(location) = location.filter(|value| !value.trim().is_empty()) {
        lines.push(format!("Location: {}", location.trim()));
    }

    if let Some(name) = schedule
        .organizer_name
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        lines.push(format!(
            "Organizer: {} <{}>",
            name.trim(),
            schedule.organizer_email
        ));
    } else {
        lines.push(format!("Organizer: {}", schedule.organizer_email));
    }

    if let Some(description) = description.filter(|value| !value.trim().is_empty()) {
        lines.push(String::new());
        lines.push("Description:".to_string());
        lines.push(description.trim().to_string());
    }

    lines.join("\n")
}

fn format_event_window(
    start_at: &str,
    end_at: &str,
    timezone: Option<&str>,
    all_day: bool,
) -> String {
    if all_day {
        return timezone
            .filter(|value| !value.trim().is_empty())
            .map(|timezone| format!("{start_at} to {end_at} ({timezone})"))
            .unwrap_or_else(|| format!("{start_at} to {end_at}"));
    }

    timezone
        .filter(|value| !value.trim().is_empty())
        .map(|timezone| format!("{start_at} to {end_at} ({timezone})"))
        .unwrap_or_else(|| format!("{start_at} to {end_at}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calendar::types::{CalendarAttendee, CalendarAttendeeInput};

    fn invite_schedule() -> EventScheduleContext {
        EventScheduleContext {
            organizer_name: Some("Spacebot".to_string()),
            organizer_email: "bot@example.com".to_string(),
        }
    }

    #[test]
    fn request_invite_skips_organizer_and_deduplicates_recipients() {
        let draft = CalendarEventDraft {
            summary: "Team sync".to_string(),
            description: Some("Agenda".to_string()),
            location: Some("Google Meet".to_string()),
            start_at: "2026-03-31T10:00".to_string(),
            end_at: "2026-03-31T11:00".to_string(),
            timezone: Some("Asia/Singapore".to_string()),
            all_day: false,
            recurrence_rule: None,
            attendees: vec![
                CalendarAttendeeInput {
                    email: "alice@example.com".to_string(),
                    common_name: None,
                    role: None,
                    partstat: None,
                    rsvp: false,
                },
                CalendarAttendeeInput {
                    email: "ALICE@example.com".to_string(),
                    common_name: None,
                    role: None,
                    partstat: None,
                    rsvp: false,
                },
                CalendarAttendeeInput {
                    email: "bot@example.com".to_string(),
                    common_name: None,
                    role: None,
                    partstat: None,
                    rsvp: false,
                },
            ],
        };
        let stored_ics = indoc::indoc! {"
            BEGIN:VCALENDAR
            VERSION:2.0
            PRODID:-//Spacebot//Calendar//EN
            CALSCALE:GREGORIAN
            BEGIN:VEVENT
            UID:uid-team-sync
            DTSTAMP:20260331T020000Z
            LAST-MODIFIED:20260331T020000Z
            SEQUENCE:0
            SUMMARY:Team sync
            DTSTART;TZID=Asia/Singapore:20260331T100000
            DTEND;TZID=Asia/Singapore:20260331T110000
            ORGANIZER;CN=Spacebot:MAILTO:bot@example.com
            ATTENDEE;ROLE=REQ-PARTICIPANT;PARTSTAT=NEEDS-ACTION;RSVP=TRUE:MAILTO:alice@example.com
            STATUS:CONFIRMED
            TRANSP:OPAQUE
            END:VEVENT
            END:VCALENDAR
        "}
        .replace('\n', "\r\n");

        let email = build_request_invite_email(&draft, &invite_schedule(), &stored_ics, false)
            .expect("request invite email should build")
            .expect("one invite recipient should remain");

        assert_eq!(email.recipients, vec!["alice@example.com".to_string()]);
        assert!(email.attachment_mime_type.contains("method=REQUEST"));
        assert!(String::from_utf8_lossy(&email.attachment_bytes).contains("METHOD:REQUEST\r\n"));
    }

    #[test]
    fn cancel_invite_uses_cancel_method() {
        let event = CalendarEvent {
            id: "event-1".to_string(),
            resource_id: "resource-1".to_string(),
            calendar_href: "https://example.com/cal/".to_string(),
            remote_href: "https://example.com/cal/event-1.ics".to_string(),
            remote_uid: "uid-team-sync".to_string(),
            recurrence_id_utc: None,
            summary: Some("Team sync".to_string()),
            description: Some("Agenda".to_string()),
            location: None,
            status: Some("CONFIRMED".to_string()),
            organizer_name: Some("Spacebot".to_string()),
            organizer_email: Some("bot@example.com".to_string()),
            start_at_utc: "2026-03-31T02:00:00+00:00".to_string(),
            end_at_utc: "2026-03-31T03:00:00+00:00".to_string(),
            timezone: Some("Asia/Singapore".to_string()),
            all_day: false,
            recurrence_rule: None,
            recurrence_exdates_json: None,
            sequence: 2,
            transparency: Some("OPAQUE".to_string()),
            etag: None,
            raw_ics: indoc::indoc! {"
                BEGIN:VCALENDAR
                VERSION:2.0
                PRODID:-//Spacebot//Calendar//EN
                CALSCALE:GREGORIAN
                BEGIN:VEVENT
                UID:uid-team-sync
                DTSTAMP:20260331T020000Z
                LAST-MODIFIED:20260331T020000Z
                SEQUENCE:2
                SUMMARY:Team sync
                DTSTART;TZID=Asia/Singapore:20260331T100000
                DTEND;TZID=Asia/Singapore:20260331T110000
                ATTENDEE;ROLE=REQ-PARTICIPANT;PARTSTAT=NEEDS-ACTION;RSVP=TRUE:MAILTO:alice@example.com
                STATUS:CONFIRMED
                TRANSP:OPAQUE
                END:VEVENT
                END:VCALENDAR
            "}
            .replace('\n', "\r\n"),
            deleted: false,
            attendees: vec![CalendarAttendee {
                id: "attendee-1".to_string(),
                event_id: "event-1".to_string(),
                email: Some("alice@example.com".to_string()),
                common_name: Some("Alice".to_string()),
                role: Some("REQ-PARTICIPANT".to_string()),
                partstat: Some("NEEDS-ACTION".to_string()),
                rsvp: true,
                is_organizer: false,
            }],
        };

        let email = build_cancel_invite_email(&event, &invite_schedule())
            .expect("cancel invite email should build")
            .expect("cancel invite should target attendees");

        assert_eq!(email.recipients, vec!["alice@example.com".to_string()]);
        assert!(email.attachment_mime_type.contains("method=CANCEL"));
        assert!(String::from_utf8_lossy(&email.attachment_bytes).contains("METHOD:CANCEL\r\n"));
    }

    #[test]
    fn removed_attendee_cancel_targets_only_removed_recipients() {
        let current_event = CalendarEvent {
            id: "event-1".to_string(),
            resource_id: "resource-1".to_string(),
            calendar_href: "https://example.com/cal/".to_string(),
            remote_href: "https://example.com/cal/event-1.ics".to_string(),
            remote_uid: "uid-team-sync".to_string(),
            recurrence_id_utc: None,
            summary: Some("Team sync".to_string()),
            description: Some("Agenda".to_string()),
            location: None,
            status: Some("CONFIRMED".to_string()),
            organizer_name: Some("Spacebot".to_string()),
            organizer_email: Some("bot@example.com".to_string()),
            start_at_utc: "2026-03-31T02:00:00+00:00".to_string(),
            end_at_utc: "2026-03-31T03:00:00+00:00".to_string(),
            timezone: Some("Asia/Singapore".to_string()),
            all_day: false,
            recurrence_rule: None,
            recurrence_exdates_json: None,
            sequence: 2,
            transparency: Some("OPAQUE".to_string()),
            etag: None,
            raw_ics: indoc::indoc! {"
                BEGIN:VCALENDAR
                VERSION:2.0
                PRODID:-//Spacebot//Calendar//EN
                CALSCALE:GREGORIAN
                BEGIN:VEVENT
                UID:uid-team-sync
                DTSTAMP:20260331T020000Z
                LAST-MODIFIED:20260331T020000Z
                SEQUENCE:2
                SUMMARY:Team sync
                DTSTART;TZID=Asia/Singapore:20260331T100000
                DTEND;TZID=Asia/Singapore:20260331T110000
                ORGANIZER;CN=Spacebot:MAILTO:bot@example.com
                ATTENDEE;CN=Alice;PARTSTAT=ACCEPTED:MAILTO:alice@example.com
                ATTENDEE;CN=Bob;PARTSTAT=NEEDS-ACTION:MAILTO:bob@example.com
                STATUS:CONFIRMED
                END:VEVENT
                END:VCALENDAR
            "}
            .replace('\n', "\r\n"),
            deleted: false,
            attendees: vec![
                CalendarAttendee {
                    id: "attendee-1".to_string(),
                    event_id: "event-1".to_string(),
                    email: Some("alice@example.com".to_string()),
                    common_name: Some("Alice".to_string()),
                    role: None,
                    partstat: Some("ACCEPTED".to_string()),
                    rsvp: false,
                    is_organizer: false,
                },
                CalendarAttendee {
                    id: "attendee-2".to_string(),
                    event_id: "event-1".to_string(),
                    email: Some("bob@example.com".to_string()),
                    common_name: Some("Bob".to_string()),
                    role: None,
                    partstat: Some("NEEDS-ACTION".to_string()),
                    rsvp: false,
                    is_organizer: false,
                },
            ],
        };
        let updated_draft = CalendarEventDraft {
            summary: "Team sync".to_string(),
            description: Some("Agenda".to_string()),
            location: Some("Google Meet".to_string()),
            start_at: "2026-03-31T10:00".to_string(),
            end_at: "2026-03-31T11:00".to_string(),
            timezone: Some("Asia/Singapore".to_string()),
            all_day: false,
            recurrence_rule: None,
            attendees: vec![CalendarAttendeeInput {
                email: "alice@example.com".to_string(),
                common_name: None,
                role: None,
                partstat: None,
                rsvp: false,
            }],
        };

        let email = build_removed_attendee_cancel_invite_email(
            &current_event,
            &updated_draft,
            &invite_schedule(),
        )
        .expect("removed-attendee cancel should build")
        .expect("one removed attendee should receive cancel");

        assert_eq!(email.recipients, vec!["bob@example.com".to_string()]);
        assert!(email.attachment_mime_type.contains("method=CANCEL"));
        assert!(String::from_utf8_lossy(&email.attachment_bytes).contains("METHOD:CANCEL\r\n"));
    }
}
