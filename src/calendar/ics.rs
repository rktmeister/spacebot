//! iCalendar parsing, occurrence expansion, and mutation helpers.

use crate::calendar::types::{
    CalendarAttendeeInput, CalendarEvent, CalendarEventDraft, CalendarOccurrence,
    SyncedCalendarEvent,
};

use anyhow::{Context as _, anyhow};
use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveDateTime, TimeZone, Utc};
use rrule::{RRuleSet, Tz};
use std::collections::{BTreeSet, HashMap};
use std::str::FromStr;

#[derive(Debug, Clone)]
pub struct EventScheduleContext {
    pub organizer_name: Option<String>,
    pub organizer_email: String,
}

#[derive(Debug, Clone)]
struct ParsedIcalDateTime {
    utc: DateTime<Utc>,
    timezone: Option<String>,
    all_day: bool,
    local_date: Option<NaiveDate>,
    local_datetime: Option<NaiveDateTime>,
}

#[derive(Debug, Default)]
struct SyncedEventBuilder {
    uid: Option<String>,
    summary: Option<String>,
    description: Option<String>,
    location: Option<String>,
    status: Option<String>,
    organizer_name: Option<String>,
    organizer_email: Option<String>,
    start: Option<ParsedIcalDateTime>,
    end: Option<ParsedIcalDateTime>,
    duration: Option<Duration>,
    recurrence_rule: Option<String>,
    recurrence_id: Option<ParsedIcalDateTime>,
    recurrence_exdates: Vec<String>,
    sequence: i64,
    transparency: Option<String>,
    attendees: Vec<CalendarAttendeeInput>,
}

impl SyncedEventBuilder {
    fn build(self) -> anyhow::Result<Option<SyncedCalendarEvent>> {
        let Some(uid) = self.uid else {
            return Ok(None);
        };
        let Some(start) = self.start else {
            return Ok(None);
        };
        let end = match (self.end, self.duration) {
            (Some(end), _) => end,
            (None, Some(duration)) => ParsedIcalDateTime {
                utc: start.utc + duration,
                timezone: start.timezone.clone(),
                all_day: start.all_day,
                local_date: None,
                local_datetime: start.local_datetime.map(|dt| dt + duration),
            },
            (None, None) if start.all_day => ParsedIcalDateTime {
                utc: start.utc + Duration::days(1),
                timezone: start.timezone.clone(),
                all_day: true,
                local_date: start.local_date.map(|date| date.succ_opt().unwrap_or(date)),
                local_datetime: None,
            },
            (None, None) => ParsedIcalDateTime {
                utc: start.utc + Duration::hours(1),
                timezone: start.timezone.clone(),
                all_day: false,
                local_date: None,
                local_datetime: start.local_datetime.map(|dt| dt + Duration::hours(1)),
            },
        };

        Ok(Some(SyncedCalendarEvent {
            remote_uid: uid,
            recurrence_id_utc: self.recurrence_id.map(|value| value.utc.to_rfc3339()),
            summary: self.summary,
            description: self.description,
            location: self.location,
            status: self.status,
            organizer_name: self.organizer_name,
            organizer_email: self.organizer_email,
            start_at_utc: start.utc.to_rfc3339(),
            end_at_utc: end.utc.to_rfc3339(),
            timezone: start.timezone.or(end.timezone),
            all_day: start.all_day,
            recurrence_rule: self.recurrence_rule,
            recurrence_exdates: self.recurrence_exdates,
            sequence: self.sequence,
            transparency: self.transparency,
            attendees: self.attendees,
        }))
    }
}

pub fn parse_calendar_events(raw_ics: &str) -> anyhow::Result<Vec<SyncedCalendarEvent>> {
    let lines = unfold_ical_lines(raw_ics);
    let mut events = Vec::new();
    let mut current: Option<SyncedEventBuilder> = None;
    let mut nested_component_depth = 0usize;

    for line in lines {
        let trimmed = line.trim();
        if trimmed.eq_ignore_ascii_case("BEGIN:VEVENT") {
            current = Some(SyncedEventBuilder::default());
            nested_component_depth = 0;
            continue;
        }
        if trimmed.eq_ignore_ascii_case("END:VEVENT") {
            if let Some(builder) = current.take()
                && let Some(event) = builder.build()?
            {
                events.push(event);
            }
            nested_component_depth = 0;
            continue;
        }

        let Some(builder) = current.as_mut() else {
            continue;
        };
        if trimmed.starts_with("BEGIN:") {
            nested_component_depth += 1;
            continue;
        }
        if trimmed.starts_with("END:") {
            nested_component_depth = nested_component_depth.saturating_sub(1);
            continue;
        }
        if nested_component_depth > 0 {
            continue;
        }
        let Some((name, params, value)) = parse_ical_property(trimmed) else {
            continue;
        };

        match name.as_str() {
            "UID" => builder.uid = Some(value.trim().to_string()),
            "SUMMARY" => builder.summary = Some(unescape_ical_text(&value)),
            "DESCRIPTION" => builder.description = Some(unescape_ical_text(&value)),
            "LOCATION" => builder.location = Some(unescape_ical_text(&value)),
            "STATUS" => builder.status = Some(value.trim().to_string()),
            "TRANSP" => builder.transparency = Some(value.trim().to_string()),
            "ORGANIZER" => {
                let (name, email) = parse_organizer(&params, &value);
                builder.organizer_name = name;
                builder.organizer_email = email;
            }
            "ATTENDEE" => {
                if let Some(attendee) = parse_attendee(&params, &value) {
                    builder.attendees.push(attendee);
                }
            }
            "DTSTART" => builder.start = parse_ical_datetime_value(&value, &params),
            "DTEND" => builder.end = parse_ical_datetime_value(&value, &params),
            "DURATION" => builder.duration = parse_ical_duration(&value),
            "RRULE" => builder.recurrence_rule = Some(value.trim().to_string()),
            "RECURRENCE-ID" => builder.recurrence_id = parse_ical_datetime_value(&value, &params),
            "EXDATE" => {
                for part in value.split(',') {
                    if let Some(parsed) = parse_ical_datetime_value(part.trim(), &params) {
                        builder.recurrence_exdates.push(parsed.utc.to_rfc3339());
                    }
                }
            }
            "SEQUENCE" => {
                builder.sequence = value.trim().parse::<i64>().unwrap_or_default();
            }
            _ => {}
        }
    }

    Ok(events)
}

pub fn expand_occurrences(
    events: &[CalendarEvent],
    range_start: DateTime<Utc>,
    range_end: DateTime<Utc>,
) -> anyhow::Result<Vec<CalendarOccurrence>> {
    let mut grouped = HashMap::<String, Vec<&CalendarEvent>>::new();
    for event in events {
        grouped
            .entry(event.remote_uid.clone())
            .or_default()
            .push(event);
    }

    let mut occurrences = Vec::new();
    for (_, series) in grouped {
        let Some(master) = series.iter().copied().find(|event| !event.is_override()) else {
            continue;
        };
        let overrides = series
            .iter()
            .copied()
            .filter_map(|event| {
                event
                    .recurrence_id_utc
                    .as_ref()
                    .map(|recurrence_id| (recurrence_id.clone(), event))
            })
            .collect::<HashMap<_, _>>();

        if !master.is_recurring() {
            maybe_push_occurrence(
                &mut occurrences,
                master,
                master,
                master.start_at_utc.as_str(),
                range_start,
                range_end,
            )?;
            continue;
        }

        let mut rrule_input = String::new();
        rrule_input.push_str(&format!(
            "{}\n",
            format_dtstart_line(
                master.start_at_utc.as_str(),
                master.timezone.as_deref(),
                master.all_day,
            )?
        ));
        if let Some(rule) = &master.recurrence_rule {
            rrule_input.push_str("RRULE:");
            rrule_input.push_str(rule);
            rrule_input.push('\n');
        }
        if let Some(exdates_json) = &master.recurrence_exdates_json {
            for exdate in serde_json::from_str::<Vec<String>>(exdates_json)
                .unwrap_or_default()
                .into_iter()
            {
                rrule_input.push_str(&format!(
                    "{}\n",
                    format_exdate_line(&exdate, master.timezone.as_deref(), master.all_day)?
                ));
            }
        }

        let tz = parse_rrule_timezone(master.timezone.as_deref());
        let set = RRuleSet::from_str(&rrule_input)
            .with_context(|| format!("failed to parse recurrence rule for {}", master.remote_uid))?
            .after(range_start.with_timezone(&tz))
            .before(range_end.with_timezone(&tz));
        let recurrence_result = set.all(4096);

        for occurrence_start in recurrence_result.dates {
            let occurrence_start_utc = occurrence_start.with_timezone(&Utc);
            let occurrence_key = occurrence_start_utc.to_rfc3339();
            if let Some(override_event) = overrides.get(&occurrence_key).copied() {
                maybe_push_occurrence(
                    &mut occurrences,
                    master,
                    override_event,
                    occurrence_key.as_str(),
                    range_start,
                    range_end,
                )?;
            } else {
                let duration = DateTime::parse_from_rfc3339(&master.end_at_utc)
                    .context("invalid master end_at_utc")?
                    .with_timezone(&Utc)
                    - DateTime::parse_from_rfc3339(&master.start_at_utc)
                        .context("invalid master start_at_utc")?
                        .with_timezone(&Utc);
                let occurrence_end_utc = occurrence_start_utc + duration;
                if occurrence_end_utc <= range_start || occurrence_start_utc >= range_end {
                    continue;
                }
                occurrences.push(CalendarOccurrence {
                    occurrence_id: format!("{}@{}", master.id, occurrence_key),
                    event_id: master.id.clone(),
                    series_event_id: master.id.clone(),
                    remote_uid: master.remote_uid.clone(),
                    calendar_href: master.calendar_href.clone(),
                    remote_href: master.remote_href.clone(),
                    summary: master.summary.clone(),
                    description: master.description.clone(),
                    location: master.location.clone(),
                    status: master.status.clone(),
                    organizer_name: master.organizer_name.clone(),
                    organizer_email: master.organizer_email.clone(),
                    start_at: occurrence_start_utc.to_rfc3339(),
                    end_at: occurrence_end_utc.to_rfc3339(),
                    timezone: master.timezone.clone(),
                    all_day: master.all_day,
                    recurring: true,
                    override_instance: false,
                    can_edit_series: true,
                    attendee_count: master.attendees.len(),
                });
            }
        }
    }

    occurrences.sort_by(|left, right| left.start_at.cmp(&right.start_at));
    Ok(occurrences)
}

fn maybe_push_occurrence(
    occurrences: &mut Vec<CalendarOccurrence>,
    master_event: &CalendarEvent,
    occurrence_event: &CalendarEvent,
    occurrence_key: &str,
    range_start: DateTime<Utc>,
    range_end: DateTime<Utc>,
) -> anyhow::Result<()> {
    let start_at = DateTime::parse_from_rfc3339(&occurrence_event.start_at_utc)
        .context("invalid occurrence start")?
        .with_timezone(&Utc);
    let end_at = DateTime::parse_from_rfc3339(&occurrence_event.end_at_utc)
        .context("invalid occurrence end")?
        .with_timezone(&Utc);
    if end_at <= range_start || start_at >= range_end {
        return Ok(());
    }

    occurrences.push(CalendarOccurrence {
        occurrence_id: format!("{}@{}", master_event.id, occurrence_key),
        event_id: occurrence_event.id.clone(),
        series_event_id: master_event.id.clone(),
        remote_uid: master_event.remote_uid.clone(),
        calendar_href: occurrence_event.calendar_href.clone(),
        remote_href: occurrence_event.remote_href.clone(),
        summary: occurrence_event.summary.clone(),
        description: occurrence_event.description.clone(),
        location: occurrence_event.location.clone(),
        status: occurrence_event.status.clone(),
        organizer_name: occurrence_event.organizer_name.clone(),
        organizer_email: occurrence_event.organizer_email.clone(),
        start_at: start_at.to_rfc3339(),
        end_at: end_at.to_rfc3339(),
        timezone: occurrence_event.timezone.clone(),
        all_day: occurrence_event.all_day,
        recurring: master_event.is_recurring(),
        override_instance: occurrence_event.is_override(),
        can_edit_series: true,
        attendee_count: occurrence_event.attendees.len(),
    });
    Ok(())
}

pub fn build_new_event_resource(
    draft: &CalendarEventDraft,
    uid: &str,
    sequence: i64,
    schedule: Option<&EventScheduleContext>,
) -> anyhow::Result<String> {
    let start = parse_user_datetime(&draft.start_at, draft.timezone.as_deref(), draft.all_day)?;
    let end = parse_user_datetime(&draft.end_at, draft.timezone.as_deref(), draft.all_day)?;
    ensure_valid_event_range(&start, &end)?;
    let scheduled_event = schedule.is_some() && !draft.attendees.is_empty();
    let attendees = build_outbound_attendees(None, draft, scheduled_event);

    let mut lines = vec![
        "BEGIN:VCALENDAR".to_string(),
        "VERSION:2.0".to_string(),
        "PRODID:-//Spacebot//Calendar//EN".to_string(),
        "CALSCALE:GREGORIAN".to_string(),
    ];
    lines.extend([
        "BEGIN:VEVENT".to_string(),
        format!("UID:{uid}"),
        format!("DTSTAMP:{}", format_utc_ical(Utc::now())),
        format!("LAST-MODIFIED:{}", format_utc_ical(Utc::now())),
        format!("SEQUENCE:{}", sequence.max(0)),
        format_ical_text_property("SUMMARY", &draft.summary),
        format_dt_line("DTSTART", &start),
        format_dt_line("DTEND", &end),
    ]);

    if let Some(description) = &draft.description
        && !description.trim().is_empty()
    {
        lines.push(format_ical_text_property("DESCRIPTION", description));
    }
    if let Some(location) = &draft.location
        && !location.trim().is_empty()
    {
        lines.push(format_ical_text_property("LOCATION", location));
    }
    if let Some(rule) = &draft.recurrence_rule
        && !rule.trim().is_empty()
    {
        lines.push(format!("RRULE:{}", rule.trim()));
    }
    if let Some(schedule) = schedule
        && scheduled_event
    {
        lines.push(format_organizer_property(schedule));
    }
    lines.extend(attendees.iter().map(format_attendee_property));

    lines.push("STATUS:CONFIRMED".to_string());
    lines.push("TRANSP:OPAQUE".to_string());
    lines.push("END:VEVENT".to_string());
    lines.push("END:VCALENDAR".to_string());

    Ok(join_ical_lines(lines))
}

pub fn update_existing_resource(
    raw_ics: &str,
    current_event: &CalendarEvent,
    draft: &CalendarEventDraft,
    schedule: Option<&EventScheduleContext>,
) -> anyhow::Result<String> {
    let document = split_calendar_document(raw_ics);
    let mut updated_blocks = Vec::with_capacity(document.segments.len());
    let mut replaced = false;

    let current_start = parse_user_datetime(
        &current_event.start_at_utc,
        current_event.timezone.as_deref(),
        current_event.all_day,
    )?;
    let requested_start =
        parse_user_datetime(&draft.start_at, draft.timezone.as_deref(), draft.all_day)?;
    let requested_end =
        parse_user_datetime(&draft.end_at, draft.timezone.as_deref(), draft.all_day)?;
    ensure_valid_event_range(&requested_start, &requested_end)?;
    let recurring_time_change = current_event.is_recurring()
        && (current_start.utc != requested_start.utc
            || current_event.all_day != draft.all_day
            || current_event.timezone != draft.timezone);

    if recurring_time_change && document.override_count > 0 {
        return Err(anyhow!(
            "updating the start time or timezone of a recurring series with overridden occurrences is not supported in v1"
        ));
    }

    for segment in document.segments {
        match segment {
            CalendarSegment::Lines(lines) => {
                updated_blocks.extend(rewrite_calendar_lines(&lines));
            }
            CalendarSegment::Event {
                lines,
                uid,
                recurrence_id,
            } => {
                if !replaced
                    && uid.as_deref() == Some(current_event.remote_uid.as_str())
                    && recurrence_id.is_none()
                {
                    updated_blocks.extend(rewrite_master_event_block(
                        &lines,
                        current_event,
                        draft,
                        &requested_start,
                        &requested_end,
                        schedule,
                    )?);
                    replaced = true;
                } else {
                    updated_blocks.extend(lines);
                }
            }
        }
    }

    if !replaced {
        return Err(anyhow!(
            "failed to locate target VEVENT in remote ICS resource"
        ));
    }

    Ok(join_ical_lines(updated_blocks))
}

pub fn build_scheduling_message(raw_ics: &str, method: &str) -> anyhow::Result<String> {
    let document = split_calendar_document(raw_ics);
    let mut updated_blocks = Vec::with_capacity(document.segments.len());
    let mut calendar_method_written = false;

    for segment in document.segments {
        match segment {
            CalendarSegment::Lines(lines) => updated_blocks.extend(
                rewrite_calendar_lines_with_method(&lines, method, &mut calendar_method_written),
            ),
            CalendarSegment::Event { lines, .. } => updated_blocks.extend(lines),
        }
    }

    Ok(join_ical_lines(updated_blocks))
}

pub fn build_cancelled_scheduling_message(
    raw_ics: &str,
    current_event: &CalendarEvent,
    schedule: &EventScheduleContext,
) -> anyhow::Result<String> {
    let document = split_calendar_document(raw_ics);
    let mut updated_blocks = Vec::with_capacity(document.segments.len());
    let mut replaced = false;
    let mut calendar_method_written = false;

    for segment in document.segments {
        match segment {
            CalendarSegment::Lines(lines) => updated_blocks.extend(
                rewrite_calendar_lines_with_method(&lines, "CANCEL", &mut calendar_method_written),
            ),
            CalendarSegment::Event {
                lines,
                uid,
                recurrence_id,
            } => {
                if !replaced
                    && uid.as_deref() == Some(current_event.remote_uid.as_str())
                    && recurrence_id.is_none()
                {
                    updated_blocks.extend(rewrite_cancelled_master_event_block(
                        &lines,
                        current_event,
                        schedule,
                    )?);
                    replaced = true;
                } else {
                    updated_blocks.extend(lines);
                }
            }
        }
    }

    if !replaced {
        return Err(anyhow!(
            "failed to locate target VEVENT in remote ICS resource"
        ));
    }

    Ok(join_ical_lines(updated_blocks))
}

pub fn export_resources_to_ics(resources: &[String]) -> String {
    let mut timezone_blocks = BTreeSet::new();
    let mut events = Vec::new();

    for resource in resources {
        let document = split_calendar_document(resource);
        for segment in document.segments {
            match segment {
                CalendarSegment::Lines(lines) => {
                    let mut current_block = Vec::new();
                    let mut current_name: Option<String> = None;
                    for line in lines {
                        let trimmed = line.trim();
                        if let Some(name) = trimmed.strip_prefix("BEGIN:")
                            && name != "VCALENDAR"
                        {
                            current_name = Some(name.to_string());
                            current_block = vec![line.clone()];
                            continue;
                        }
                        if let Some(name) = trimmed.strip_prefix("END:")
                            && current_name.as_deref() == Some(name)
                        {
                            current_block.push(line.clone());
                            if name == "VTIMEZONE" {
                                timezone_blocks.insert(join_ical_lines(current_block.clone()));
                            }
                            current_name = None;
                            current_block.clear();
                            continue;
                        }
                        if current_name.is_some() {
                            current_block.push(line.clone());
                        }
                    }
                }
                CalendarSegment::Event { lines, .. } => events.push(join_ical_lines(lines)),
            }
        }
    }

    let mut output = vec![
        "BEGIN:VCALENDAR".to_string(),
        "VERSION:2.0".to_string(),
        "PRODID:-//Spacebot//Calendar Export//EN".to_string(),
        "CALSCALE:GREGORIAN".to_string(),
        "METHOD:PUBLISH".to_string(),
    ];
    for block in timezone_blocks {
        output.extend(unfold_ical_lines(&block));
    }
    for event in events {
        output.extend(unfold_ical_lines(&event));
    }
    output.push("END:VCALENDAR".to_string());
    join_ical_lines(output)
}

#[derive(Debug)]
enum CalendarSegment {
    Lines(Vec<String>),
    Event {
        lines: Vec<String>,
        uid: Option<String>,
        recurrence_id: Option<String>,
    },
}

#[derive(Debug)]
struct ParsedCalendarDocument {
    segments: Vec<CalendarSegment>,
    override_count: usize,
}

fn split_calendar_document(raw_ics: &str) -> ParsedCalendarDocument {
    let lines = unfold_ical_lines(raw_ics);
    let mut segments = Vec::new();
    let mut outside_lines = Vec::new();
    let mut current_event = Vec::new();
    let mut inside_event = false;
    let mut override_count = 0usize;

    for line in lines {
        let trimmed = line.trim();
        if trimmed.eq_ignore_ascii_case("BEGIN:VEVENT") {
            if !outside_lines.is_empty() {
                segments.push(CalendarSegment::Lines(std::mem::take(&mut outside_lines)));
            }
            inside_event = true;
            current_event.push(line.clone());
            continue;
        }

        if inside_event {
            current_event.push(line.clone());
            if trimmed.eq_ignore_ascii_case("END:VEVENT") {
                let uid = current_event.iter().find_map(|line| {
                    parse_ical_property(line).and_then(|(name, _, value)| {
                        (name == "UID").then_some(value.trim().to_string())
                    })
                });
                let recurrence_id = current_event.iter().find_map(|line| {
                    parse_ical_property(line).and_then(|(name, params, value)| {
                        if name != "RECURRENCE-ID" {
                            return None;
                        }
                        parse_ical_datetime_value(&value, &params)
                            .map(|parsed| parsed.utc.to_rfc3339())
                    })
                });
                if recurrence_id.is_some() {
                    override_count += 1;
                }
                segments.push(CalendarSegment::Event {
                    lines: std::mem::take(&mut current_event),
                    uid,
                    recurrence_id,
                });
                inside_event = false;
            }
            continue;
        }

        outside_lines.push(line);
    }

    if !outside_lines.is_empty() {
        segments.push(CalendarSegment::Lines(outside_lines));
    }

    ParsedCalendarDocument {
        segments,
        override_count,
    }
}

fn rewrite_master_event_block(
    lines: &[String],
    current_event: &CalendarEvent,
    draft: &CalendarEventDraft,
    requested_start: &ParsedIcalDateTime,
    requested_end: &ParsedIcalDateTime,
    schedule: Option<&EventScheduleContext>,
) -> anyhow::Result<Vec<String>> {
    let scheduled_event = schedule.is_some() && !draft.attendees.is_empty();

    let mut result = Vec::new();
    let mut inserted = false;
    for line in lines {
        let trimmed = line.trim();
        if trimmed.eq_ignore_ascii_case("BEGIN:VEVENT") {
            result.push(line.clone());
            continue;
        }
        if trimmed.eq_ignore_ascii_case("END:VEVENT") {
            if !inserted {
                result.extend(build_replacement_properties(
                    current_event,
                    draft,
                    requested_start,
                    requested_end,
                    schedule,
                ));
                inserted = true;
            }
            result.push(line.clone());
            continue;
        }

        let Some((name, _, _)) = parse_ical_property(trimmed) else {
            result.push(line.clone());
            continue;
        };

        if name == "UID" {
            result.push(line.clone());
            if !inserted {
                result.extend(build_replacement_properties(
                    current_event,
                    draft,
                    requested_start,
                    requested_end,
                    schedule,
                ));
                inserted = true;
            }
            continue;
        }

        if is_managed_rewrite_property(&name, scheduled_event) {
            continue;
        }

        result.push(line.clone());
    }

    if !inserted {
        return Err(anyhow!("failed to rewrite VEVENT block"));
    }

    Ok(result)
}

fn build_replacement_properties(
    current_event: &CalendarEvent,
    draft: &CalendarEventDraft,
    requested_start: &ParsedIcalDateTime,
    requested_end: &ParsedIcalDateTime,
    schedule: Option<&EventScheduleContext>,
) -> Vec<String> {
    let scheduled_event = schedule.is_some() && !draft.attendees.is_empty();
    let attendees = build_outbound_attendees(Some(current_event), draft, scheduled_event);
    let mut lines = vec![
        format!("LAST-MODIFIED:{}", format_utc_ical(Utc::now())),
        format!("SEQUENCE:{}", current_event.sequence + 1),
        format_ical_text_property("SUMMARY", &draft.summary),
        format_dt_line("DTSTART", requested_start),
        format_dt_line("DTEND", requested_end),
    ];

    if let Some(description) = &draft.description
        && !description.trim().is_empty()
    {
        lines.push(format_ical_text_property("DESCRIPTION", description));
    }
    if let Some(location) = &draft.location
        && !location.trim().is_empty()
    {
        lines.push(format_ical_text_property("LOCATION", location));
    }
    if let Some(rule) = current_event
        .recurrence_rule
        .as_ref()
        .or(draft.recurrence_rule.as_ref())
    {
        lines.push(format!("RRULE:{}", rule.trim()));
    }
    if let Some(schedule) = schedule
        && scheduled_event
    {
        lines.push(format_organizer_property(schedule));
    }
    lines.extend(attendees.iter().map(format_attendee_property));
    lines.push(format!(
        "STATUS:{}",
        current_event.status.as_deref().unwrap_or("CONFIRMED")
    ));
    lines.push(format!(
        "TRANSP:{}",
        current_event.transparency.as_deref().unwrap_or("OPAQUE")
    ));
    lines
}

fn rewrite_cancelled_master_event_block(
    lines: &[String],
    current_event: &CalendarEvent,
    schedule: &EventScheduleContext,
) -> anyhow::Result<Vec<String>> {
    let mut result = Vec::new();
    let mut inserted = false;

    for line in lines {
        let trimmed = line.trim();
        if trimmed.eq_ignore_ascii_case("BEGIN:VEVENT") {
            result.push(line.clone());
            continue;
        }
        if trimmed.eq_ignore_ascii_case("END:VEVENT") {
            if !inserted {
                result.extend(build_cancelled_properties(current_event, schedule));
                inserted = true;
            }
            result.push(line.clone());
            continue;
        }

        let Some((name, _, _)) = parse_ical_property(trimmed) else {
            result.push(line.clone());
            continue;
        };

        if name == "UID" {
            result.push(line.clone());
            if !inserted {
                result.extend(build_cancelled_properties(current_event, schedule));
                inserted = true;
            }
            continue;
        }

        if matches!(
            name.as_str(),
            "STATUS" | "LAST-MODIFIED" | "SEQUENCE" | "ORGANIZER"
        ) {
            continue;
        }

        result.push(line.clone());
    }

    if !inserted {
        return Err(anyhow!("failed to rewrite VEVENT block"));
    }

    Ok(result)
}

fn build_cancelled_properties(
    current_event: &CalendarEvent,
    schedule: &EventScheduleContext,
) -> Vec<String> {
    vec![
        format!("LAST-MODIFIED:{}", format_utc_ical(Utc::now())),
        format!("SEQUENCE:{}", current_event.sequence + 1),
        format_organizer_property(schedule),
        "STATUS:CANCELLED".to_string(),
    ]
}

fn is_managed_rewrite_property(name: &str, scheduled_event: bool) -> bool {
    matches!(
        name,
        "SUMMARY"
            | "DESCRIPTION"
            | "LOCATION"
            | "DTSTART"
            | "DTEND"
            | "DURATION"
            | "STATUS"
            | "TRANSP"
            | "LAST-MODIFIED"
            | "SEQUENCE"
            | "ATTENDEE"
            | "RRULE"
    ) || (scheduled_event && name == "ORGANIZER")
}

fn rewrite_calendar_lines(lines: &[String]) -> Vec<String> {
    lines
        .iter()
        .filter(|line| {
            parse_ical_property(line)
                .map(|(name, _, _)| name != "METHOD")
                .unwrap_or(true)
        })
        .cloned()
        .collect()
}

fn rewrite_calendar_lines_with_method(
    lines: &[String],
    method: &str,
    method_inserted: &mut bool,
) -> Vec<String> {
    let mut rewritten = rewrite_calendar_lines(lines);

    if !*method_inserted {
        insert_calendar_method(&mut rewritten, method);
        *method_inserted = true;
    }

    rewritten
}

fn insert_calendar_method(lines: &mut Vec<String>, method: &str) {
    let mut insert_at = 0usize;

    for (index, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.eq_ignore_ascii_case("BEGIN:VCALENDAR") {
            insert_at = index + 1;
            continue;
        }
        if matches!(
            parse_ical_property(trimmed).map(|(name, _, _)| name),
            Some(name) if matches!(name.as_str(), "VERSION" | "PRODID" | "CALSCALE")
        ) {
            insert_at = index + 1;
            continue;
        }
        if trimmed.eq_ignore_ascii_case("BEGIN:VEVENT") {
            break;
        }
    }

    lines.insert(insert_at, format!("METHOD:{method}"));
}

fn build_outbound_attendees(
    current_event: Option<&CalendarEvent>,
    draft: &CalendarEventDraft,
    scheduled_event: bool,
) -> Vec<CalendarAttendeeInput> {
    let current_attendees = current_event
        .map(|event| {
            event
                .attendees
                .iter()
                .filter_map(|attendee| {
                    attendee
                        .email
                        .as_ref()
                        .map(|email| (normalize_attendee_email_key(email), attendee))
                })
                .collect::<HashMap<_, _>>()
        })
        .unwrap_or_default();

    draft
        .attendees
        .iter()
        .map(|attendee| {
            let existing = current_attendees
                .get(&normalize_attendee_email_key(&attendee.email))
                .copied();
            CalendarAttendeeInput {
                email: attendee.email.trim().to_string(),
                common_name: trimmed_non_empty(attendee.common_name.as_deref())
                    .or_else(|| existing.and_then(|value| value.common_name.clone())),
                role: trimmed_non_empty(attendee.role.as_deref())
                    .or_else(|| existing.and_then(|value| value.role.clone()))
                    .or_else(|| scheduled_event.then_some("REQ-PARTICIPANT".to_string())),
                partstat: trimmed_non_empty(attendee.partstat.as_deref())
                    .or_else(|| existing.and_then(|value| value.partstat.clone()))
                    .or_else(|| scheduled_event.then_some("NEEDS-ACTION".to_string())),
                rsvp: if scheduled_event {
                    true
                } else {
                    attendee.rsvp || existing.is_some_and(|value| value.rsvp)
                },
            }
        })
        .collect()
}

fn normalize_attendee_email_key(email: &str) -> String {
    email.trim().to_ascii_lowercase()
}

fn trimmed_non_empty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn parse_user_datetime(
    value: &str,
    timezone: Option<&str>,
    all_day: bool,
) -> anyhow::Result<ParsedIcalDateTime> {
    let normalized_timezone = normalize_timezone_label(timezone);

    if all_day {
        let date = NaiveDate::parse_from_str(value.trim(), "%Y-%m-%d")
            .or_else(|_| DateTime::parse_from_rfc3339(value).map(|dt| dt.date_naive()))
            .with_context(|| format!("invalid all-day date '{value}'"))?;
        let tz = parse_rrule_timezone(normalized_timezone.as_deref());
        let local = tz
            .with_ymd_and_hms(date.year(), date.month(), date.day(), 0, 0, 0)
            .single()
            .ok_or_else(|| anyhow!("invalid all-day local datetime"))?;
        return Ok(ParsedIcalDateTime {
            utc: local.with_timezone(&Utc),
            timezone: normalized_timezone,
            all_day: true,
            local_date: Some(date),
            local_datetime: None,
        });
    }

    if let Ok(parsed) = DateTime::parse_from_rfc3339(value.trim()) {
        let utc = parsed.with_timezone(&Utc);
        if let Some(tz_name) = normalized_timezone {
            let tz = parse_rrule_timezone(Some(&tz_name));
            let local = utc.with_timezone(&tz).naive_local();
            return Ok(ParsedIcalDateTime {
                utc,
                timezone: Some(tz_name),
                all_day: false,
                local_date: None,
                local_datetime: Some(local),
            });
        }
        return Ok(ParsedIcalDateTime {
            utc,
            timezone: Some("UTC".to_string()),
            all_day: false,
            local_date: None,
            local_datetime: Some(utc.naive_utc()),
        });
    }

    let naive = NaiveDateTime::parse_from_str(value.trim(), "%Y-%m-%dT%H:%M:%S")
        .or_else(|_| NaiveDateTime::parse_from_str(value.trim(), "%Y-%m-%dT%H:%M"))
        .with_context(|| format!("invalid datetime '{value}'"))?;
    let tz = parse_rrule_timezone(normalized_timezone.as_deref());
    let local = tz
        .from_local_datetime(&naive)
        .single()
        .ok_or_else(|| anyhow!("ambiguous or invalid local datetime '{value}'"))?;
    Ok(ParsedIcalDateTime {
        utc: local.with_timezone(&Utc),
        timezone: normalized_timezone,
        all_day: false,
        local_date: None,
        local_datetime: Some(naive),
    })
}

fn parse_organizer(
    params: &HashMap<String, String>,
    value: &str,
) -> (Option<String>, Option<String>) {
    let name = params.get("CN").map(|name| unescape_ical_text(name));
    let email = value
        .trim()
        .strip_prefix("MAILTO:")
        .or_else(|| value.trim().strip_prefix("mailto:"))
        .map(str::to_string)
        .or_else(|| {
            let trimmed = value.trim();
            (!trimmed.is_empty()).then_some(trimmed.to_string())
        });
    (name, email)
}

fn parse_attendee(params: &HashMap<String, String>, value: &str) -> Option<CalendarAttendeeInput> {
    let email = value
        .trim()
        .strip_prefix("MAILTO:")
        .or_else(|| value.trim().strip_prefix("mailto:"))
        .map(str::to_string)?;
    Some(CalendarAttendeeInput {
        email,
        common_name: params.get("CN").map(|value| unescape_ical_text(value)),
        role: params.get("ROLE").cloned(),
        partstat: params.get("PARTSTAT").cloned(),
        rsvp: params
            .get("RSVP")
            .is_some_and(|value| value.eq_ignore_ascii_case("TRUE")),
    })
}

fn parse_ical_duration(value: &str) -> Option<Duration> {
    let mut sign = 1i64;
    let mut remaining = value.trim();
    if let Some(rest) = remaining.strip_prefix('-') {
        sign = -1;
        remaining = rest;
    }
    let remaining = remaining.strip_prefix('P')?;
    let mut days = 0i64;
    let mut hours = 0i64;
    let mut minutes = 0i64;
    let mut seconds = 0i64;
    let mut buffer = String::new();
    let mut in_time = false;

    for character in remaining.chars() {
        if character == 'T' {
            in_time = true;
            continue;
        }
        if character.is_ascii_digit() {
            buffer.push(character);
            continue;
        }
        let value = buffer.parse::<i64>().ok()?;
        buffer.clear();
        match (character, in_time) {
            ('D', false) => days = value,
            ('H', true) => hours = value,
            ('M', true) => minutes = value,
            ('S', true) => seconds = value,
            _ => return None,
        }
    }

    let duration = Duration::days(days)
        + Duration::hours(hours)
        + Duration::minutes(minutes)
        + Duration::seconds(seconds);
    Some(if sign < 0 { -duration } else { duration })
}

fn ensure_valid_event_range(
    start: &ParsedIcalDateTime,
    end: &ParsedIcalDateTime,
) -> anyhow::Result<()> {
    if end.utc <= start.utc {
        return Err(anyhow!("calendar event end must be after start"));
    }
    Ok(())
}

fn format_dtstart_line(
    start_at_utc: &str,
    timezone: Option<&str>,
    all_day: bool,
) -> anyhow::Result<String> {
    let parsed = parse_user_datetime(start_at_utc, timezone, all_day)?;
    Ok(format_dt_line("DTSTART", &parsed))
}

fn format_exdate_line(
    exdate_utc: &str,
    timezone: Option<&str>,
    all_day: bool,
) -> anyhow::Result<String> {
    let parsed = parse_user_datetime(exdate_utc, timezone, all_day)?;
    Ok(format_dt_line("EXDATE", &parsed))
}

fn format_dt_line(name: &str, value: &ParsedIcalDateTime) -> String {
    if value.all_day {
        return format!(
            "{name};VALUE=DATE:{}",
            value
                .local_date
                .unwrap_or_else(|| value.utc.date_naive())
                .format("%Y%m%d")
        );
    }

    if let (Some(timezone), Some(local_datetime)) = (
        canonical_tzid(value.timezone.as_deref()),
        &value.local_datetime,
    ) && timezone != "UTC"
    {
        return format!(
            "{name};TZID={timezone}:{}",
            local_datetime.format("%Y%m%dT%H%M%S")
        );
    }

    format!("{name}:{}", format_utc_ical(value.utc))
}

fn format_utc_ical(value: DateTime<Utc>) -> String {
    value.format("%Y%m%dT%H%M%SZ").to_string()
}

fn format_ical_text_property(name: &str, value: &str) -> String {
    format!("{name}:{}", escape_ical_text(value))
}

fn format_organizer_property(schedule: &EventScheduleContext) -> String {
    let mut property = String::from("ORGANIZER");
    if let Some(name) = schedule.organizer_name.as_deref()
        && !name.trim().is_empty()
    {
        property.push_str(";CN=");
        property.push_str(&escape_ical_param(name));
    }
    property.push(':');
    property.push_str("MAILTO:");
    property.push_str(schedule.organizer_email.trim());
    property
}

fn format_attendee_property(attendee: &CalendarAttendeeInput) -> String {
    let mut property = String::from("ATTENDEE");
    if let Some(name) = &attendee.common_name
        && !name.trim().is_empty()
    {
        property.push_str(";CN=");
        property.push_str(&escape_ical_param(name));
    }
    if let Some(role) = &attendee.role
        && !role.trim().is_empty()
    {
        property.push_str(";ROLE=");
        property.push_str(role.trim());
    }
    if let Some(partstat) = &attendee.partstat
        && !partstat.trim().is_empty()
    {
        property.push_str(";PARTSTAT=");
        property.push_str(partstat.trim());
    }
    if attendee.rsvp {
        property.push_str(";RSVP=TRUE");
    }
    property.push(':');
    property.push_str("MAILTO:");
    property.push_str(attendee.email.trim());
    property
}

fn escape_ical_text(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace(',', "\\,")
        .replace(';', "\\;")
}

fn escape_ical_param(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn join_ical_lines(lines: Vec<String>) -> String {
    let mut output = String::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        output.push_str(&line);
        output.push_str("\r\n");
    }
    output
}

pub fn unfold_ical_lines(content: &str) -> Vec<String> {
    let mut unfolded: Vec<String> = Vec::new();
    for raw_line in content.replace("\r\n", "\n").split('\n') {
        if matches!(raw_line.chars().next(), Some(' ' | '\t'))
            && let Some(previous) = unfolded.last_mut()
        {
            previous.push_str(raw_line.trim_start());
            continue;
        }
        unfolded.push(raw_line.to_string());
    }
    unfolded
}

pub fn parse_ical_property(line: &str) -> Option<(String, HashMap<String, String>, String)> {
    let (name_part, value) = line.split_once(':')?;
    let mut segments = name_part.split(';');
    let name = segments.next()?.trim().to_ascii_uppercase();
    let params = segments
        .filter_map(|segment| {
            let (key, value) = segment.split_once('=')?;
            Some((key.trim().to_ascii_uppercase(), value.trim().to_string()))
        })
        .collect::<HashMap<_, _>>();
    Some((name, params, value.to_string()))
}

fn parse_ical_datetime_value(
    value: &str,
    params: &HashMap<String, String>,
) -> Option<ParsedIcalDateTime> {
    let timezone = normalize_timezone_label(params.get("TZID").map(String::as_str));
    let normalized = value.trim();
    let value_type = params.get("VALUE").map(String::as_str);

    if value_type == Some("DATE") || normalized.len() == 8 {
        let date = NaiveDate::parse_from_str(normalized, "%Y%m%d").ok()?;
        let tz = parse_rrule_timezone(timezone.as_deref());
        let local = tz
            .with_ymd_and_hms(date.year(), date.month(), date.day(), 0, 0, 0)
            .single()?;
        return Some(ParsedIcalDateTime {
            utc: local.with_timezone(&Utc),
            timezone,
            all_day: true,
            local_date: Some(date),
            local_datetime: None,
        });
    }

    if normalized.ends_with('Z') {
        let date_time =
            NaiveDateTime::parse_from_str(normalized.trim_end_matches('Z'), "%Y%m%dT%H%M%S")
                .ok()?;
        let utc = Utc.from_utc_datetime(&date_time);
        return Some(ParsedIcalDateTime {
            utc,
            timezone: Some("UTC".to_string()),
            all_day: false,
            local_date: None,
            local_datetime: Some(date_time),
        });
    }

    let date_time = NaiveDateTime::parse_from_str(normalized, "%Y%m%dT%H%M%S").ok()?;
    let tz = parse_rrule_timezone(timezone.as_deref());
    let local = tz.from_local_datetime(&date_time).single()?;
    Some(ParsedIcalDateTime {
        utc: local.with_timezone(&Utc),
        timezone,
        all_day: false,
        local_date: None,
        local_datetime: Some(date_time),
    })
}

fn parse_rrule_timezone(timezone: Option<&str>) -> Tz {
    canonical_tzid(timezone)
        .and_then(|value| value.parse::<chrono_tz::Tz>().ok().map(Into::into))
        .unwrap_or(Tz::UTC)
}

pub fn normalize_timezone_label(timezone: Option<&str>) -> Option<String> {
    let timezone = timezone?.trim();
    if timezone.is_empty() {
        return None;
    }
    if timezone.eq_ignore_ascii_case("UTC") {
        return Some("UTC".to_string());
    }
    if let Ok(parsed) = timezone.parse::<chrono_tz::Tz>() {
        return Some(parsed.to_string());
    }
    windows_timezone_to_iana(timezone)
        .map(str::to_string)
        .or_else(|| Some(timezone.to_string()))
}

fn canonical_tzid(timezone: Option<&str>) -> Option<String> {
    let timezone = normalize_timezone_label(timezone)?;
    if timezone == "UTC" || timezone.parse::<chrono_tz::Tz>().is_ok() {
        return Some(timezone);
    }
    None
}

fn windows_timezone_to_iana(timezone: &str) -> Option<&'static str> {
    match timezone {
        "UTC" => Some("UTC"),
        "Singapore Standard Time" => Some("Asia/Singapore"),
        "SE Asia Standard Time" => Some("Asia/Bangkok"),
        "China Standard Time" => Some("Asia/Shanghai"),
        "Tokyo Standard Time" => Some("Asia/Tokyo"),
        "India Standard Time" => Some("Asia/Kolkata"),
        "Pakistan Standard Time" => Some("Asia/Karachi"),
        "Arabian Standard Time" => Some("Asia/Dubai"),
        "Arab Standard Time" => Some("Asia/Riyadh"),
        "GMT Standard Time" => Some("Europe/London"),
        "W. Europe Standard Time" => Some("Europe/Berlin"),
        "Romance Standard Time" => Some("Europe/Paris"),
        "Turkey Standard Time" => Some("Europe/Istanbul"),
        "Russian Standard Time" => Some("Europe/Moscow"),
        "South Africa Standard Time" => Some("Africa/Johannesburg"),
        "Eastern Standard Time" => Some("America/New_York"),
        "Central Standard Time" => Some("America/Chicago"),
        "Mountain Standard Time" => Some("America/Denver"),
        "US Mountain Standard Time" => Some("America/Phoenix"),
        "Pacific Standard Time" => Some("America/Los_Angeles"),
        "Alaskan Standard Time" => Some("America/Anchorage"),
        "Hawaiian Standard Time" => Some("Pacific/Honolulu"),
        "AUS Eastern Standard Time" => Some("Australia/Sydney"),
        "E. Australia Standard Time" => Some("Australia/Brisbane"),
        "Cen. Australia Standard Time" => Some("Australia/Adelaide"),
        "W. Australia Standard Time" => Some("Australia/Perth"),
        "New Zealand Standard Time" => Some("Pacific/Auckland"),
        _ => None,
    }
}

fn unescape_ical_text(value: &str) -> String {
    value
        .replace("\\n", "\n")
        .replace("\\N", "\n")
        .replace("\\,", ",")
        .replace("\\;", ";")
        .replace("\\\\", "\\")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calendar::types::CalendarEvent;

    fn invite_schedule() -> EventScheduleContext {
        EventScheduleContext {
            organizer_name: Some("Spacebot".to_string()),
            organizer_email: "bot@example.com".to_string(),
        }
    }

    fn recurring_event(timezone: Option<&str>) -> CalendarEvent {
        CalendarEvent {
            id: "event-1".to_string(),
            resource_id: "resource-1".to_string(),
            calendar_href: "https://example.com/cal/".to_string(),
            remote_href: "https://example.com/cal/event-1.ics".to_string(),
            remote_uid: "uid-1".to_string(),
            recurrence_id_utc: None,
            summary: Some("Recurring".to_string()),
            description: None,
            location: None,
            status: Some("CONFIRMED".to_string()),
            organizer_name: None,
            organizer_email: None,
            start_at_utc: "2026-04-07T13:30:00+00:00".to_string(),
            end_at_utc: "2026-04-07T14:00:00+00:00".to_string(),
            timezone: timezone.map(str::to_string),
            all_day: false,
            recurrence_rule: Some(
                "FREQ=WEEKLY;UNTIL=20260804T063000Z;INTERVAL=2;BYDAY=TU;WKST=MO".to_string(),
            ),
            recurrence_exdates_json: None,
            sequence: 0,
            transparency: None,
            etag: None,
            raw_ics: String::new(),
            deleted: false,
            attendees: Vec::new(),
        }
    }

    #[test]
    fn expand_occurrences_tolerates_non_iana_timezone_names() {
        let start = Utc
            .with_ymd_and_hms(2026, 4, 1, 0, 0, 0)
            .single()
            .expect("valid range start");
        let end = Utc
            .with_ymd_and_hms(2026, 4, 15, 0, 0, 0)
            .single()
            .expect("valid range end");

        let occurrences = expand_occurrences(
            &[recurring_event(Some("SE Asia Standard Time"))],
            start,
            end,
        )
        .expect("invalid timezone names should fall back to UTC recurrence expansion");

        assert!(!occurrences.is_empty());
        assert_eq!(occurrences[0].remote_uid, "uid-1");
    }

    #[test]
    fn parse_calendar_events_maps_windows_timezone_ids_to_iana() {
        let raw_ics = indoc::indoc! {"
            BEGIN:VCALENDAR
            VERSION:2.0
            BEGIN:VEVENT
            UID:test-windows-tz
            SUMMARY:Windows timezone event
            DTSTART;TZID=Singapore Standard Time:20260309T163000
            DTEND;TZID=Singapore Standard Time:20260309T170000
            END:VEVENT
            END:VCALENDAR
        "};

        let events = parse_calendar_events(raw_ics).expect("raw ICS should parse");

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].timezone.as_deref(), Some("Asia/Singapore"));
        assert_eq!(events[0].start_at_utc, "2026-03-09T08:30:00+00:00");
        assert_eq!(events[0].end_at_utc, "2026-03-09T09:00:00+00:00");
    }

    #[test]
    fn parse_user_datetime_maps_windows_timezone_ids_to_iana() {
        let parsed = parse_user_datetime(
            "2026-03-09T08:30:00+00:00",
            Some("Singapore Standard Time"),
            false,
        )
        .expect("RFC3339 input should parse");

        assert_eq!(parsed.timezone.as_deref(), Some("Asia/Singapore"));
        assert_eq!(
            parsed
                .local_datetime
                .expect("local datetime should be populated")
                .to_string(),
            "2026-03-09 16:30:00"
        );
        assert_eq!(
            format_dt_line("DTSTART", &parsed),
            "DTSTART;TZID=Asia/Singapore:20260309T163000"
        );
    }

    #[test]
    fn parse_calendar_events_ignores_nested_valarm_fields() {
        let raw_ics = indoc::indoc! {"
            BEGIN:VCALENDAR
            VERSION:2.0
            BEGIN:VEVENT
            UID:test-valarm
            SUMMARY:Invite with alarm
            DESCRIPTION:Join here https://teams.microsoft.com/meet/test
            LOCATION:Microsoft Teams Meeting
            DTSTART;TZID=Singapore Standard Time:20260309T163000
            DTEND;TZID=Singapore Standard Time:20260309T170000
            BEGIN:VALARM
            DESCRIPTION:REMINDER
            TRIGGER:-PT15M
            END:VALARM
            END:VEVENT
            END:VCALENDAR
        "};

        let events = parse_calendar_events(raw_ics).expect("raw ICS should parse");

        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].description.as_deref(),
            Some("Join here https://teams.microsoft.com/meet/test")
        );
        assert_eq!(
            events[0].location.as_deref(),
            Some("Microsoft Teams Meeting")
        );
    }

    #[test]
    fn build_new_event_resource_keeps_organizer_and_omits_method_for_invites() {
        let draft = CalendarEventDraft {
            summary: "Team sync".to_string(),
            description: Some("Agenda".to_string()),
            location: None,
            start_at: "2026-03-30T09:00".to_string(),
            end_at: "2026-03-30T10:00".to_string(),
            timezone: Some("Asia/Singapore".to_string()),
            all_day: false,
            recurrence_rule: None,
            attendees: vec![CalendarAttendeeInput {
                email: "alice@example.com".to_string(),
                common_name: Some("Alice".to_string()),
                role: None,
                partstat: None,
                rsvp: false,
            }],
        };

        let raw_ics =
            build_new_event_resource(&draft, "uid-team-sync", 0, Some(&invite_schedule()))
                .expect("invite resource should build");

        assert!(!raw_ics.contains("METHOD:"));
        assert!(raw_ics.contains("ORGANIZER;CN=Spacebot:MAILTO:bot@example.com\r\n"));
        assert!(raw_ics.contains(
            "ATTENDEE;CN=Alice;ROLE=REQ-PARTICIPANT;PARTSTAT=NEEDS-ACTION;RSVP=TRUE:MAILTO:alice@example.com\r\n"
        ));
    }

    #[test]
    fn update_existing_resource_preserves_attendee_partstat_for_invites() {
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
            start_at_utc: "2026-03-30T01:00:00+00:00".to_string(),
            end_at_utc: "2026-03-30T02:00:00+00:00".to_string(),
            timezone: Some("Asia/Singapore".to_string()),
            all_day: false,
            recurrence_rule: None,
            recurrence_exdates_json: None,
            sequence: 2,
            transparency: Some("OPAQUE".to_string()),
            etag: Some("\"etag-1\"".to_string()),
            raw_ics: indoc::indoc! {"
                BEGIN:VCALENDAR
                VERSION:2.0
                PRODID:-//Spacebot//Calendar//EN
                CALSCALE:GREGORIAN
                METHOD:REQUEST
                BEGIN:VEVENT
                UID:uid-team-sync
                DTSTAMP:20260329T010000Z
                LAST-MODIFIED:20260329T010000Z
                SEQUENCE:2
                SUMMARY:Team sync
                DTSTART;TZID=Asia/Singapore:20260330T090000
                DTEND;TZID=Asia/Singapore:20260330T100000
                ATTENDEE;CN=Alice;ROLE=REQ-PARTICIPANT;PARTSTAT=ACCEPTED;RSVP=TRUE:MAILTO:alice@example.com
                STATUS:CONFIRMED
                TRANSP:OPAQUE
                END:VEVENT
                END:VCALENDAR
            "}
            .replace('\n', "\r\n"),
            deleted: false,
            attendees: vec![crate::calendar::CalendarAttendee {
                id: "attendee-1".to_string(),
                event_id: "event-1".to_string(),
                email: Some("alice@example.com".to_string()),
                common_name: Some("Alice".to_string()),
                role: Some("REQ-PARTICIPANT".to_string()),
                partstat: Some("ACCEPTED".to_string()),
                rsvp: true,
                is_organizer: false,
            }],
        };
        let draft = CalendarEventDraft {
            summary: "Team sync updated".to_string(),
            description: Some("Agenda updated".to_string()),
            location: None,
            start_at: "2026-03-30T09:30".to_string(),
            end_at: "2026-03-30T10:30".to_string(),
            timezone: Some("Asia/Singapore".to_string()),
            all_day: false,
            recurrence_rule: None,
            attendees: vec![CalendarAttendeeInput {
                email: "alice@example.com".to_string(),
                common_name: Some("Alice".to_string()),
                role: None,
                partstat: None,
                rsvp: false,
            }],
        };

        let updated = update_existing_resource(
            &current_event.raw_ics,
            &current_event,
            &draft,
            Some(&invite_schedule()),
        )
        .expect("invite update should build");

        assert!(!updated.contains("METHOD:"));
        assert!(updated.contains("SEQUENCE:3\r\n"));
        assert!(updated.contains("ORGANIZER;CN=Spacebot:MAILTO:bot@example.com\r\n"));
        assert!(updated.contains("PARTSTAT=ACCEPTED"));
    }

    #[test]
    fn build_scheduling_message_adds_request_method_for_email_delivery() {
        let stored = indoc::indoc! {"
            BEGIN:VCALENDAR
            VERSION:2.0
            PRODID:-//Spacebot//Calendar//EN
            CALSCALE:GREGORIAN
            BEGIN:VEVENT
            UID:uid-team-sync
            DTSTAMP:20260329T010000Z
            LAST-MODIFIED:20260329T010000Z
            SEQUENCE:0
            SUMMARY:Team sync
            DTSTART;TZID=Asia/Singapore:20260330T090000
            DTEND;TZID=Asia/Singapore:20260330T100000
            ORGANIZER;CN=Spacebot:MAILTO:bot@example.com
            ATTENDEE;CN=Alice;ROLE=REQ-PARTICIPANT;PARTSTAT=NEEDS-ACTION;RSVP=TRUE:MAILTO:alice@example.com
            STATUS:CONFIRMED
            TRANSP:OPAQUE
            END:VEVENT
            END:VCALENDAR
        "}
        .replace('\n', "\r\n");

        let request = build_scheduling_message(&stored, "REQUEST")
            .expect("request scheduling ICS should build");

        assert!(request.contains("METHOD:REQUEST\r\n"));
        assert!(request.contains("ORGANIZER;CN=Spacebot:MAILTO:bot@example.com\r\n"));
    }

    #[test]
    fn build_cancelled_scheduling_message_sets_cancel_method_and_status() {
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
            start_at_utc: "2026-03-30T01:00:00+00:00".to_string(),
            end_at_utc: "2026-03-30T02:00:00+00:00".to_string(),
            timezone: Some("Asia/Singapore".to_string()),
            all_day: false,
            recurrence_rule: None,
            recurrence_exdates_json: None,
            sequence: 2,
            transparency: Some("OPAQUE".to_string()),
            etag: Some("\"etag-1\"".to_string()),
            raw_ics: indoc::indoc! {"
                BEGIN:VCALENDAR
                VERSION:2.0
                PRODID:-//Spacebot//Calendar//EN
                CALSCALE:GREGORIAN
                BEGIN:VEVENT
                UID:uid-team-sync
                DTSTAMP:20260329T010000Z
                LAST-MODIFIED:20260329T010000Z
                SEQUENCE:2
                SUMMARY:Team sync
                DTSTART;TZID=Asia/Singapore:20260330T090000
                DTEND;TZID=Asia/Singapore:20260330T100000
                ATTENDEE;CN=Alice;ROLE=REQ-PARTICIPANT;PARTSTAT=ACCEPTED;RSVP=TRUE:MAILTO:alice@example.com
                STATUS:CONFIRMED
                TRANSP:OPAQUE
                END:VEVENT
                END:VCALENDAR
            "}
            .replace('\n', "\r\n"),
            deleted: false,
            attendees: vec![crate::calendar::CalendarAttendee {
                id: "attendee-1".to_string(),
                event_id: "event-1".to_string(),
                email: Some("alice@example.com".to_string()),
                common_name: Some("Alice".to_string()),
                role: Some("REQ-PARTICIPANT".to_string()),
                partstat: Some("ACCEPTED".to_string()),
                rsvp: true,
                is_organizer: false,
            }],
        };

        let cancelled = build_cancelled_scheduling_message(
            &current_event.raw_ics,
            &current_event,
            &invite_schedule(),
        )
        .expect("cancel scheduling ICS should build");

        assert!(cancelled.contains("METHOD:CANCEL\r\n"));
        assert!(cancelled.contains("SEQUENCE:3\r\n"));
        assert!(cancelled.contains("ORGANIZER;CN=Spacebot:MAILTO:bot@example.com\r\n"));
        assert!(cancelled.contains("STATUS:CANCELLED\r\n"));
    }

    #[test]
    fn build_new_event_resource_rejects_non_positive_ranges() {
        let draft = CalendarEventDraft {
            summary: "Bad range".to_string(),
            description: None,
            location: None,
            start_at: "2026-03-30T09:00".to_string(),
            end_at: "2026-03-30T09:00".to_string(),
            timezone: Some("Asia/Singapore".to_string()),
            all_day: false,
            recurrence_rule: None,
            attendees: Vec::new(),
        };

        let error = build_new_event_resource(&draft, "uid-bad-range", 0, None)
            .expect_err("range should fail");
        assert!(
            error
                .to_string()
                .contains("calendar event end must be after start")
        );
    }

    #[test]
    fn format_dt_line_uses_utc_for_unknown_timezone_names() {
        let parsed = parse_user_datetime(
            "2026-04-07T13:30:00+00:00",
            Some("Completely Made Up Timezone"),
            false,
        )
        .expect("RFC3339 input should parse");

        assert_eq!(
            format_dt_line("DTSTART", &parsed),
            "DTSTART:20260407T133000Z"
        );
    }
}
