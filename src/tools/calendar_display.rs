use crate::agent::channel_prompt::{TemporalContext, TemporalTimezone};
use crate::calendar::{
    CalendarAttendee, CalendarAvailabilitySlot, CalendarEvent, CalendarOccurrence,
};
use crate::config::RuntimeConfig;

use chrono::{DateTime, Local, Utc};
use serde::Serialize;

#[derive(Debug, Serialize)]
pub(crate) struct CalendarOccurrenceDisplay {
    pub occurrence_id: String,
    pub event_id: String,
    pub series_event_id: String,
    pub remote_uid: String,
    pub summary: Option<String>,
    pub description: Option<String>,
    pub location: Option<String>,
    pub status: Option<String>,
    pub organizer_name: Option<String>,
    pub organizer_email: Option<String>,
    pub all_day: bool,
    pub recurring: bool,
    pub override_instance: bool,
    pub can_edit_series: bool,
    pub attendee_count: usize,
    pub start_at_utc: String,
    pub end_at_utc: String,
    pub display_date: String,
    pub display_start: String,
    pub display_end: String,
    pub display_range: String,
    pub display_timezone: String,
    pub event_timezone: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct CalendarEventDisplay {
    pub id: String,
    pub resource_id: String,
    pub calendar_href: String,
    pub remote_href: String,
    pub remote_uid: String,
    pub recurrence_id_utc: Option<String>,
    pub summary: Option<String>,
    pub description: Option<String>,
    pub location: Option<String>,
    pub status: Option<String>,
    pub organizer_name: Option<String>,
    pub organizer_email: Option<String>,
    pub all_day: bool,
    pub recurrence_rule: Option<String>,
    pub recurrence_exdates_json: Option<String>,
    pub sequence: i64,
    pub transparency: Option<String>,
    pub etag: Option<String>,
    pub deleted: bool,
    pub attendees: Vec<CalendarAttendee>,
    pub attendee_count: usize,
    pub start_at_utc: String,
    pub end_at_utc: String,
    pub display_date: String,
    pub display_start: String,
    pub display_end: String,
    pub display_range: String,
    pub display_timezone: String,
    pub event_timezone: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct CalendarAvailabilitySlotDisplay {
    pub start_at_utc: String,
    pub end_at_utc: String,
    pub display_start: String,
    pub display_end: String,
    pub display_range: String,
    pub display_timezone: String,
}

pub(crate) fn guidance_summary(display_timezone: &str) -> String {
    format!(
        "Use the display_* fields when replying to the user. They are already rendered in {display_timezone}. Fields ending in _utc are exact reference values."
    )
}

pub(crate) fn display_timezone_label(runtime_config: &RuntimeConfig) -> String {
    let temporal_context = TemporalContext::from_runtime(runtime_config);
    timezone_name(&temporal_context)
}

pub(crate) fn occurrence_display(
    runtime_config: &RuntimeConfig,
    occurrence: &CalendarOccurrence,
) -> Result<CalendarOccurrenceDisplay, String> {
    let temporal_context = TemporalContext::from_runtime(runtime_config);
    occurrence_display_with_context(&temporal_context, occurrence)
}

pub(crate) fn event_display(
    runtime_config: &RuntimeConfig,
    event: &CalendarEvent,
) -> Result<CalendarEventDisplay, String> {
    let temporal_context = TemporalContext::from_runtime(runtime_config);
    event_display_with_context(&temporal_context, event)
}

pub(crate) fn availability_slot_display(
    runtime_config: &RuntimeConfig,
    slot: &CalendarAvailabilitySlot,
) -> Result<CalendarAvailabilitySlotDisplay, String> {
    let temporal_context = TemporalContext::from_runtime(runtime_config);
    availability_slot_display_with_context(&temporal_context, slot)
}

pub(crate) fn display_timestamp(
    runtime_config: &RuntimeConfig,
    timestamp: DateTime<Utc>,
) -> String {
    let temporal_context = TemporalContext::from_runtime(runtime_config);
    format_local_timestamp(&temporal_context, timestamp)
}

fn occurrence_display_with_context(
    temporal_context: &TemporalContext,
    occurrence: &CalendarOccurrence,
) -> Result<CalendarOccurrenceDisplay, String> {
    let start_at = parse_utc_timestamp(&occurrence.start_at, "start_at")?;
    let end_at = parse_utc_timestamp(&occurrence.end_at, "end_at")?;
    let display_timezone = timezone_name(temporal_context);

    Ok(CalendarOccurrenceDisplay {
        occurrence_id: occurrence.occurrence_id.clone(),
        event_id: occurrence.event_id.clone(),
        series_event_id: occurrence.series_event_id.clone(),
        remote_uid: occurrence.remote_uid.clone(),
        summary: occurrence.summary.clone(),
        description: occurrence.description.clone(),
        location: occurrence.location.clone(),
        status: occurrence.status.clone(),
        organizer_name: occurrence.organizer_name.clone(),
        organizer_email: occurrence.organizer_email.clone(),
        all_day: occurrence.all_day,
        recurring: occurrence.recurring,
        override_instance: occurrence.override_instance,
        can_edit_series: occurrence.can_edit_series,
        attendee_count: occurrence.attendee_count,
        start_at_utc: occurrence.start_at.clone(),
        end_at_utc: occurrence.end_at.clone(),
        display_date: format_local_date(temporal_context, start_at),
        display_start: format_local_time(temporal_context, start_at, occurrence.all_day),
        display_end: format_local_time(temporal_context, end_at, occurrence.all_day),
        display_range: format_local_range(temporal_context, start_at, end_at, occurrence.all_day),
        display_timezone,
        event_timezone: occurrence.timezone.clone(),
    })
}

fn event_display_with_context(
    temporal_context: &TemporalContext,
    event: &CalendarEvent,
) -> Result<CalendarEventDisplay, String> {
    let start_at = parse_utc_timestamp(&event.start_at_utc, "start_at_utc")?;
    let end_at = parse_utc_timestamp(&event.end_at_utc, "end_at_utc")?;
    let display_timezone = timezone_name(temporal_context);

    Ok(CalendarEventDisplay {
        id: event.id.clone(),
        resource_id: event.resource_id.clone(),
        calendar_href: event.calendar_href.clone(),
        remote_href: event.remote_href.clone(),
        remote_uid: event.remote_uid.clone(),
        recurrence_id_utc: event.recurrence_id_utc.clone(),
        summary: event.summary.clone(),
        description: event.description.clone(),
        location: event.location.clone(),
        status: event.status.clone(),
        organizer_name: event.organizer_name.clone(),
        organizer_email: event.organizer_email.clone(),
        all_day: event.all_day,
        recurrence_rule: event.recurrence_rule.clone(),
        recurrence_exdates_json: event.recurrence_exdates_json.clone(),
        sequence: event.sequence,
        transparency: event.transparency.clone(),
        etag: event.etag.clone(),
        deleted: event.deleted,
        attendees: event.attendees.clone(),
        attendee_count: event.attendees.len(),
        start_at_utc: event.start_at_utc.clone(),
        end_at_utc: event.end_at_utc.clone(),
        display_date: format_local_date(temporal_context, start_at),
        display_start: format_local_time(temporal_context, start_at, event.all_day),
        display_end: format_local_time(temporal_context, end_at, event.all_day),
        display_range: format_local_range(temporal_context, start_at, end_at, event.all_day),
        display_timezone,
        event_timezone: event.timezone.clone(),
    })
}

fn availability_slot_display_with_context(
    temporal_context: &TemporalContext,
    slot: &CalendarAvailabilitySlot,
) -> Result<CalendarAvailabilitySlotDisplay, String> {
    let start_at = parse_utc_timestamp(&slot.start_at, "start_at")?;
    let end_at = parse_utc_timestamp(&slot.end_at, "end_at")?;
    let display_timezone = timezone_name(temporal_context);

    Ok(CalendarAvailabilitySlotDisplay {
        start_at_utc: slot.start_at.clone(),
        end_at_utc: slot.end_at.clone(),
        display_start: format_local_timestamp(temporal_context, start_at),
        display_end: format_local_timestamp(temporal_context, end_at),
        display_range: format!(
            "{} to {} ({display_timezone})",
            format_local_timestamp_without_zone(temporal_context, start_at),
            format_local_timestamp_without_zone(temporal_context, end_at),
        ),
        display_timezone,
    })
}

fn parse_utc_timestamp(value: &str, field: &str) -> Result<DateTime<Utc>, String> {
    DateTime::parse_from_rfc3339(value)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|error| format!("invalid {field} RFC3339 timestamp '{value}': {error}"))
}

fn timezone_name(temporal_context: &TemporalContext) -> String {
    match &temporal_context.timezone {
        TemporalTimezone::Named { timezone_name, .. } => timezone_name.clone(),
        TemporalTimezone::SystemLocal => "system local".to_string(),
    }
}

fn format_local_date(temporal_context: &TemporalContext, timestamp: DateTime<Utc>) -> String {
    match &temporal_context.timezone {
        TemporalTimezone::Named { timezone, .. } => timestamp
            .with_timezone(timezone)
            .format("%a, %b %-d, %Y")
            .to_string(),
        TemporalTimezone::SystemLocal => timestamp
            .with_timezone(&Local)
            .format("%a, %b %-d, %Y")
            .to_string(),
    }
}

fn format_local_time(
    temporal_context: &TemporalContext,
    timestamp: DateTime<Utc>,
    all_day: bool,
) -> String {
    if all_day {
        return "All day".to_string();
    }

    match &temporal_context.timezone {
        TemporalTimezone::Named { timezone, .. } => timestamp
            .with_timezone(timezone)
            .format("%-I:%M %p")
            .to_string(),
        TemporalTimezone::SystemLocal => timestamp
            .with_timezone(&Local)
            .format("%-I:%M %p")
            .to_string(),
    }
}

fn format_local_timestamp(temporal_context: &TemporalContext, timestamp: DateTime<Utc>) -> String {
    format!(
        "{} ({})",
        format_local_timestamp_without_zone(temporal_context, timestamp),
        timezone_name(temporal_context)
    )
}

fn format_local_timestamp_without_zone(
    temporal_context: &TemporalContext,
    timestamp: DateTime<Utc>,
) -> String {
    match &temporal_context.timezone {
        TemporalTimezone::Named { timezone, .. } => timestamp
            .with_timezone(timezone)
            .format("%a, %b %-d, %Y, %-I:%M %p")
            .to_string(),
        TemporalTimezone::SystemLocal => timestamp
            .with_timezone(&Local)
            .format("%a, %b %-d, %Y, %-I:%M %p")
            .to_string(),
    }
}

fn format_local_range(
    temporal_context: &TemporalContext,
    start_at: DateTime<Utc>,
    end_at: DateTime<Utc>,
    all_day: bool,
) -> String {
    let display_timezone = timezone_name(temporal_context);
    if all_day {
        let start_date = format_local_date(temporal_context, start_at);
        let end_date = format_local_date(temporal_context, end_at);
        if start_date == end_date {
            return format!("{start_date} (all day, {display_timezone})");
        }
        return format!("{start_date} to {end_date} (all day, {display_timezone})");
    }

    let start_date = format_local_date(temporal_context, start_at);
    let end_date = format_local_date(temporal_context, end_at);
    let start_time = format_local_time(temporal_context, start_at, false);
    let end_time = format_local_time(temporal_context, end_at, false);

    if start_date == end_date {
        format!("{start_date}, {start_time} to {end_time} ({display_timezone})")
    } else {
        format!("{start_date}, {start_time} to {end_date}, {end_time} ({display_timezone})")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono_tz::Tz;

    fn temporal_context(timezone_name: &str) -> TemporalContext {
        let timezone = timezone_name.parse::<Tz>().expect("timezone should parse");
        TemporalContext {
            now_utc: Utc::now(),
            timezone: TemporalTimezone::Named {
                timezone_name: timezone_name.to_string(),
                timezone,
            },
        }
    }

    #[test]
    fn occurrence_display_uses_runtime_timezone() {
        let temporal_context = temporal_context("Asia/Singapore");
        let occurrence = CalendarOccurrence {
            occurrence_id: "occ-1".to_string(),
            event_id: "event-1".to_string(),
            series_event_id: "event-1".to_string(),
            remote_uid: "uid-1".to_string(),
            calendar_href: "https://example.com/cal".to_string(),
            remote_href: "https://example.com/cal/event-1.ics".to_string(),
            summary: Some("Meeting".to_string()),
            description: None,
            location: None,
            status: Some("CONFIRMED".to_string()),
            organizer_name: None,
            organizer_email: None,
            start_at: "2026-04-01T02:00:00+00:00".to_string(),
            end_at: "2026-04-01T03:00:00+00:00".to_string(),
            timezone: Some("Asia/Singapore".to_string()),
            all_day: false,
            recurring: false,
            override_instance: false,
            can_edit_series: true,
            attendee_count: 0,
        };

        let display = occurrence_display_with_context(&temporal_context, &occurrence)
            .expect("display should format");

        assert_eq!(display.display_start, "10:00 AM");
        assert_eq!(display.display_end, "11:00 AM");
        assert_eq!(
            display.display_range,
            "Wed, Apr 1, 2026, 10:00 AM to 11:00 AM (Asia/Singapore)"
        );
    }
}
