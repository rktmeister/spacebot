//! Calendar persistence and proposal storage (SQLite).

use crate::calendar::types::{
    CalendarAttendee, CalendarAttendeeInput, CalendarChangeProposal, CalendarCollection,
    CalendarEvent, CalendarEventDraft, CalendarProposalAction, CalendarProposalStatus,
    CalendarSourceState, SyncedCalendarResource,
};
use crate::error::Result;

use anyhow::Context as _;
use sqlx::{Row as _, SqlitePool};
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct CalendarStore {
    pool: SqlitePool,
}

#[derive(Debug, Clone)]
pub struct ApplySyncResult {
    pub created_event_count: usize,
    pub updated_event_count: usize,
    pub deleted_event_count: usize,
    pub synced_resource_count: usize,
}

pub struct ApplySyncDeltaParams<'a> {
    pub calendar_href: &'a str,
    pub resources: &'a [SyncedCalendarResource],
    pub deleted_hrefs: &'a [String],
    pub sync_token: Option<&'a str>,
    pub ctag: Option<&'a str>,
    pub full_refresh: bool,
    pub synced_at: &'a str,
}

impl CalendarStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub async fn load_source_state(&self, source_id: &str) -> Result<Option<CalendarSourceState>> {
        let row = sqlx::query(
            r#"
            SELECT source_id, provider_kind, base_url, principal_url, home_set_url, auth_kind,
                   last_discovery_at, last_sync_at, last_successful_sync_at, last_error, sync_status
            FROM calendar_sources
            WHERE source_id = ?
            "#,
        )
        .bind(source_id)
        .fetch_optional(&self.pool)
        .await
        .context("failed to load calendar source state")?;

        row.map(|row| row_to_source_state(&row)).transpose()
    }

    pub async fn save_source_state(&self, state: &CalendarSourceState) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO calendar_sources (
                source_id, provider_kind, base_url, principal_url, home_set_url, auth_kind,
                last_discovery_at, last_sync_at, last_successful_sync_at, last_error, sync_status
            )
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(source_id) DO UPDATE SET
                provider_kind = excluded.provider_kind,
                base_url = excluded.base_url,
                principal_url = excluded.principal_url,
                home_set_url = excluded.home_set_url,
                auth_kind = excluded.auth_kind,
                last_discovery_at = excluded.last_discovery_at,
                last_sync_at = excluded.last_sync_at,
                last_successful_sync_at = excluded.last_successful_sync_at,
                last_error = excluded.last_error,
                sync_status = excluded.sync_status
            "#,
        )
        .bind(&state.source_id)
        .bind(&state.provider_kind)
        .bind(&state.base_url)
        .bind(&state.principal_url)
        .bind(&state.home_set_url)
        .bind(&state.auth_kind)
        .bind(&state.last_discovery_at)
        .bind(&state.last_sync_at)
        .bind(&state.last_successful_sync_at)
        .bind(&state.last_error)
        .bind(&state.sync_status)
        .execute(&self.pool)
        .await
        .context("failed to save calendar source state")?;

        Ok(())
    }

    pub async fn replace_discovered_calendars(
        &self,
        source_id: &str,
        calendars: &[CalendarCollection],
        selected_calendar_href: Option<&str>,
    ) -> Result<()> {
        let mut transaction = self
            .pool
            .begin()
            .await
            .context("failed to begin calendar discovery transaction")?;

        for calendar in calendars {
            sqlx::query(
                r#"
                INSERT INTO calendar_calendars (
                    href, source_id, display_name, description, color, timezone, ctag, sync_token,
                    is_selected, discovered_at, last_synced_at
                )
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                ON CONFLICT(href) DO UPDATE SET
                    source_id = excluded.source_id,
                    display_name = excluded.display_name,
                    description = excluded.description,
                    color = excluded.color,
                    timezone = excluded.timezone,
                    ctag = excluded.ctag,
                    sync_token = COALESCE(calendar_calendars.sync_token, excluded.sync_token),
                    is_selected = excluded.is_selected,
                    discovered_at = excluded.discovered_at,
                    last_synced_at = COALESCE(excluded.last_synced_at, calendar_calendars.last_synced_at)
                "#,
            )
            .bind(&calendar.href)
            .bind(source_id)
            .bind(&calendar.display_name)
            .bind(&calendar.description)
            .bind(&calendar.color)
            .bind(&calendar.timezone)
            .bind(&calendar.ctag)
            .bind(&calendar.sync_token)
            .bind(
                selected_calendar_href
                    .map(|selected| selected == calendar.href)
                    .unwrap_or(calendar.is_selected) as i64,
            )
            .bind(&calendar.discovered_at)
            .bind(&calendar.last_synced_at)
            .execute(&mut *transaction)
            .await
            .context("failed to save discovered calendar")?;
        }

        if let Some(selected) = selected_calendar_href {
            sqlx::query(
                r#"
                UPDATE calendar_calendars
                SET is_selected = CASE WHEN href = ? THEN 1 ELSE 0 END
                WHERE source_id = ?
                "#,
            )
            .bind(selected)
            .bind(source_id)
            .execute(&mut *transaction)
            .await
            .context("failed to update selected calendar")?;
        }

        transaction
            .commit()
            .await
            .context("failed to commit calendar discovery transaction")?;

        Ok(())
    }

    pub async fn list_calendars(&self, source_id: &str) -> Result<Vec<CalendarCollection>> {
        let rows = sqlx::query(
            r#"
            SELECT href, display_name, description, color, timezone, ctag, sync_token,
                   is_selected, discovered_at, last_synced_at
            FROM calendar_calendars
            WHERE source_id = ?
            ORDER BY is_selected DESC, COALESCE(display_name, href) ASC
            "#,
        )
        .bind(source_id)
        .fetch_all(&self.pool)
        .await
        .context("failed to list calendars")?;

        rows.iter().map(row_to_calendar).collect()
    }

    pub async fn apply_sync_delta(
        &self,
        params: ApplySyncDeltaParams<'_>,
    ) -> Result<ApplySyncResult> {
        let ApplySyncDeltaParams {
            calendar_href,
            resources,
            deleted_hrefs,
            sync_token,
            ctag,
            full_refresh,
            synced_at,
        } = params;
        let mut transaction = self
            .pool
            .begin()
            .await
            .context("failed to begin calendar sync transaction")?;

        sqlx::query(
            r#"
            UPDATE calendar_calendars
            SET sync_token = ?, ctag = COALESCE(?, ctag), last_synced_at = ?
            WHERE href = ?
            "#,
        )
        .bind(sync_token)
        .bind(ctag)
        .bind(synced_at)
        .bind(calendar_href)
        .execute(&mut *transaction)
        .await
        .context("failed to update calendar sync metadata")?;

        let mut created_event_count = 0usize;
        let mut updated_event_count = 0usize;
        let mut deleted_event_count = 0usize;

        for remote_href in deleted_hrefs {
            let changed = sqlx::query(
                r#"
                UPDATE calendar_resources
                SET deleted = 1, updated_at = ?
                WHERE calendar_href = ? AND remote_href = ?
                "#,
            )
            .bind(synced_at)
            .bind(calendar_href)
            .bind(remote_href)
            .execute(&mut *transaction)
            .await
            .context("failed to mark calendar resource deleted")?
            .rows_affected() as usize;
            deleted_event_count += changed;
        }

        let known_hrefs = if full_refresh {
            Some(
                sqlx::query(
                    r#"
                    SELECT remote_href
                    FROM calendar_resources
                    WHERE calendar_href = ?
                    "#,
                )
                .bind(calendar_href)
                .fetch_all(&mut *transaction)
                .await
                .context("failed to load existing calendar resources")?
                .into_iter()
                .filter_map(|row| row.try_get::<String, _>("remote_href").ok())
                .collect::<Vec<_>>(),
            )
        } else {
            None
        };

        if let Some(existing_hrefs) = known_hrefs {
            let current_hrefs = resources
                .iter()
                .map(|resource| resource.remote_href.as_str())
                .collect::<std::collections::HashSet<_>>();
            for existing in existing_hrefs {
                if current_hrefs.contains(existing.as_str()) {
                    continue;
                }
                let changed = sqlx::query(
                    r#"
                    UPDATE calendar_resources
                    SET deleted = 1, updated_at = ?
                    WHERE calendar_href = ? AND remote_href = ?
                    "#,
                )
                .bind(synced_at)
                .bind(calendar_href)
                .bind(&existing)
                .execute(&mut *transaction)
                .await
                .context("failed to mark stale calendar resource deleted")?
                .rows_affected() as usize;
                deleted_event_count += changed;
            }
        }

        for resource in resources {
            let existing_row = sqlx::query(
                r#"
                SELECT id
                FROM calendar_resources
                WHERE remote_href = ?
                "#,
            )
            .bind(&resource.remote_href)
            .fetch_optional(&mut *transaction)
            .await
            .context("failed to load existing calendar resource")?;

            let is_existing = existing_row.is_some();
            let resource_id = existing_row
                .as_ref()
                .and_then(|row| row.try_get::<String, _>("id").ok())
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

            sqlx::query(
                r#"
                INSERT INTO calendar_resources (
                    id, calendar_href, remote_href, etag, raw_ics, deleted, created_at, updated_at
                )
                VALUES (?, ?, ?, ?, ?, 0, ?, ?)
                ON CONFLICT(remote_href) DO UPDATE SET
                    calendar_href = excluded.calendar_href,
                    etag = excluded.etag,
                    raw_ics = excluded.raw_ics,
                    deleted = 0,
                    updated_at = excluded.updated_at
                "#,
            )
            .bind(&resource_id)
            .bind(calendar_href)
            .bind(&resource.remote_href)
            .bind(&resource.etag)
            .bind(&resource.raw_ics)
            .bind(synced_at)
            .bind(synced_at)
            .execute(&mut *transaction)
            .await
            .context("failed to upsert calendar resource")?;

            sqlx::query(
                r#"
                DELETE FROM calendar_attendees
                WHERE event_id IN (
                    SELECT id FROM calendar_events WHERE resource_id = ?
                )
                "#,
            )
            .bind(&resource_id)
            .execute(&mut *transaction)
            .await
            .context("failed to delete stale calendar attendees")?;

            sqlx::query("DELETE FROM calendar_events WHERE resource_id = ?")
                .bind(&resource_id)
                .execute(&mut *transaction)
                .await
                .context("failed to delete stale calendar events")?;

            if is_existing {
                updated_event_count += resource.events.len();
            } else {
                created_event_count += resource.events.len();
            }

            for event in &resource.events {
                let event_id = uuid::Uuid::new_v4().to_string();
                let exdates_json = if event.recurrence_exdates.is_empty() {
                    None
                } else {
                    Some(
                        serde_json::to_string(&event.recurrence_exdates)
                            .context("failed to serialize recurrence exdates")?,
                    )
                };

                sqlx::query(
                    r#"
                    INSERT INTO calendar_events (
                        id, resource_id, calendar_href, remote_uid, recurrence_id_utc, summary,
                        description, location, status, organizer_name, organizer_email,
                        start_at_utc, end_at_utc, timezone, all_day, recurrence_rule,
                        recurrence_exdates_json, sequence, transparency, created_at, updated_at
                    )
                    VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                    "#,
                )
                .bind(&event_id)
                .bind(&resource_id)
                .bind(calendar_href)
                .bind(&event.remote_uid)
                .bind(&event.recurrence_id_utc)
                .bind(&event.summary)
                .bind(&event.description)
                .bind(&event.location)
                .bind(&event.status)
                .bind(&event.organizer_name)
                .bind(&event.organizer_email)
                .bind(&event.start_at_utc)
                .bind(&event.end_at_utc)
                .bind(&event.timezone)
                .bind(event.all_day as i64)
                .bind(&event.recurrence_rule)
                .bind(&exdates_json)
                .bind(event.sequence)
                .bind(&event.transparency)
                .bind(synced_at)
                .bind(synced_at)
                .execute(&mut *transaction)
                .await
                .context("failed to insert calendar event")?;

                for attendee in &event.attendees {
                    self.insert_attendee(&mut transaction, &event_id, attendee)
                        .await
                        .context("failed to insert calendar attendee")?;
                }
            }
        }

        transaction
            .commit()
            .await
            .context("failed to commit calendar sync transaction")?;

        Ok(ApplySyncResult {
            created_event_count,
            updated_event_count,
            deleted_event_count,
            synced_resource_count: resources.len(),
        })
    }

    pub async fn clear_calendar_sync_token(&self, calendar_href: &str) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE calendar_calendars
            SET sync_token = NULL
            WHERE href = ?
            "#,
        )
        .bind(calendar_href)
        .execute(&self.pool)
        .await
        .context("failed to clear calendar sync token")?;

        Ok(())
    }

    async fn insert_attendee(
        &self,
        transaction: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        event_id: &str,
        attendee: &CalendarAttendeeInput,
    ) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO calendar_attendees (
                id, event_id, email, common_name, role, partstat, rsvp, is_organizer
            )
            VALUES (?, ?, ?, ?, ?, ?, ?, 0)
            "#,
        )
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(event_id)
        .bind(&attendee.email)
        .bind(&attendee.common_name)
        .bind(&attendee.role)
        .bind(&attendee.partstat)
        .bind(attendee.rsvp as i64)
        .execute(&mut **transaction)
        .await
        .context("failed to insert attendee")?;

        Ok(())
    }

    pub async fn list_active_events(&self, calendar_href: &str) -> Result<Vec<CalendarEvent>> {
        let rows = sqlx::query(
            r#"
            SELECT e.id, e.resource_id, e.calendar_href, r.remote_href, e.remote_uid,
                   e.recurrence_id_utc, e.summary, e.description, e.location, e.status,
                   e.organizer_name, e.organizer_email, e.start_at_utc, e.end_at_utc,
                   e.timezone, e.all_day, e.recurrence_rule, e.recurrence_exdates_json,
                   e.sequence, e.transparency, r.etag, r.raw_ics, r.deleted
            FROM calendar_events e
            JOIN calendar_resources r ON r.id = e.resource_id
            WHERE e.calendar_href = ? AND r.deleted = 0
            ORDER BY e.start_at_utc ASC
            "#,
        )
        .bind(calendar_href)
        .fetch_all(&self.pool)
        .await
        .context("failed to list calendar events")?;

        let event_ids = rows
            .iter()
            .filter_map(|row| row.try_get::<String, _>("id").ok())
            .collect::<Vec<_>>();
        let attendees = self.load_attendees(&event_ids).await?;

        rows.iter()
            .map(|row| row_to_event(row, &attendees))
            .collect::<Result<Vec<_>>>()
    }

    pub async fn get_event(&self, event_id: &str) -> Result<Option<CalendarEvent>> {
        let row = sqlx::query(
            r#"
            SELECT e.id, e.resource_id, e.calendar_href, r.remote_href, e.remote_uid,
                   e.recurrence_id_utc, e.summary, e.description, e.location, e.status,
                   e.organizer_name, e.organizer_email, e.start_at_utc, e.end_at_utc,
                   e.timezone, e.all_day, e.recurrence_rule, e.recurrence_exdates_json,
                   e.sequence, e.transparency, r.etag, r.raw_ics, r.deleted
            FROM calendar_events e
            JOIN calendar_resources r ON r.id = e.resource_id
            WHERE e.id = ?
            "#,
        )
        .bind(event_id)
        .fetch_optional(&self.pool)
        .await
        .context("failed to fetch calendar event")?;

        let Some(row) = row else {
            return Ok(None);
        };

        let attendees = self.load_attendees(&[event_id.to_string()]).await?;
        Ok(Some(row_to_event(&row, &attendees)?))
    }

    pub async fn find_series_master(
        &self,
        calendar_href: &str,
        remote_uid: &str,
    ) -> Result<Option<CalendarEvent>> {
        let row = sqlx::query(
            r#"
            SELECT e.id, e.resource_id, e.calendar_href, r.remote_href, e.remote_uid,
                   e.recurrence_id_utc, e.summary, e.description, e.location, e.status,
                   e.organizer_name, e.organizer_email, e.start_at_utc, e.end_at_utc,
                   e.timezone, e.all_day, e.recurrence_rule, e.recurrence_exdates_json,
                   e.sequence, e.transparency, r.etag, r.raw_ics, r.deleted
            FROM calendar_events e
            JOIN calendar_resources r ON r.id = e.resource_id
            WHERE e.calendar_href = ? AND e.remote_uid = ? AND e.recurrence_id_utc IS NULL
            LIMIT 1
            "#,
        )
        .bind(calendar_href)
        .bind(remote_uid)
        .fetch_optional(&self.pool)
        .await
        .context("failed to load recurring series master")?;

        let Some(row) = row else {
            return Ok(None);
        };
        let event_id = row.try_get::<String, _>("id")?;
        let attendees = self.load_attendees(&[event_id]).await?;
        Ok(Some(row_to_event(&row, &attendees)?))
    }

    pub async fn load_export_resources(&self, calendar_href: &str) -> Result<Vec<String>> {
        let rows = sqlx::query(
            r#"
            SELECT raw_ics
            FROM calendar_resources
            WHERE calendar_href = ? AND deleted = 0
            ORDER BY remote_href ASC
            "#,
        )
        .bind(calendar_href)
        .fetch_all(&self.pool)
        .await
        .context("failed to load ICS export resources")?;

        Ok(rows
            .into_iter()
            .filter_map(|row| row.try_get::<String, _>("raw_ics").ok())
            .collect())
    }

    pub async fn has_active_resource(&self, remote_href: &str) -> Result<bool> {
        let row = sqlx::query(
            r#"
            SELECT 1
            FROM calendar_resources
            WHERE remote_href = ? AND deleted = 0
            LIMIT 1
            "#,
        )
        .bind(remote_href)
        .fetch_optional(&self.pool)
        .await
        .context("failed to check calendar resource state")?;

        Ok(row.is_some())
    }

    pub async fn create_change_proposal(
        &self,
        action: CalendarProposalAction,
        event_id: Option<&str>,
        summary: &str,
        diff: &str,
        basis_etag: Option<&str>,
        draft: &CalendarEventDraft,
    ) -> Result<CalendarChangeProposal> {
        let proposal = CalendarChangeProposal {
            id: uuid::Uuid::new_v4().to_string(),
            action,
            status: CalendarProposalStatus::Pending,
            event_id: event_id.map(str::to_string),
            summary: summary.to_string(),
            diff: diff.to_string(),
            basis_etag: basis_etag.map(str::to_string),
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            applied_at: None,
            error: None,
            draft: draft.clone(),
        };

        sqlx::query(
            r#"
            INSERT INTO calendar_change_proposals (
                id, action, status, event_id, summary, diff, basis_etag, draft_json,
                created_at, updated_at, applied_at, error
            )
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(&proposal.id)
        .bind(proposal.action.as_str())
        .bind(proposal.status.as_str())
        .bind(&proposal.event_id)
        .bind(&proposal.summary)
        .bind(&proposal.diff)
        .bind(&proposal.basis_etag)
        .bind(serde_json::to_string(&proposal.draft).context("failed to serialize proposal draft")?)
        .bind(&proposal.created_at)
        .bind(&proposal.updated_at)
        .bind(&proposal.applied_at)
        .bind(&proposal.error)
        .execute(&self.pool)
        .await
        .context("failed to create calendar change proposal")?;

        Ok(proposal)
    }

    pub async fn get_change_proposal(
        &self,
        proposal_id: &str,
    ) -> Result<Option<CalendarChangeProposal>> {
        let row = sqlx::query(
            r#"
            SELECT id, action, status, event_id, summary, diff, basis_etag, draft_json,
                   created_at, updated_at, applied_at, error
            FROM calendar_change_proposals
            WHERE id = ?
            "#,
        )
        .bind(proposal_id)
        .fetch_optional(&self.pool)
        .await
        .context("failed to load calendar change proposal")?;

        row.map(|row| row_to_proposal(&row)).transpose()
    }

    pub async fn update_proposal_status(
        &self,
        proposal_id: &str,
        status: CalendarProposalStatus,
        error: Option<&str>,
        applied_at: Option<&str>,
    ) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE calendar_change_proposals
            SET status = ?, updated_at = ?, applied_at = ?, error = ?
            WHERE id = ?
            "#,
        )
        .bind(status.as_str())
        .bind(chrono::Utc::now().to_rfc3339())
        .bind(applied_at)
        .bind(error)
        .bind(proposal_id)
        .execute(&self.pool)
        .await
        .context("failed to update calendar proposal status")?;

        Ok(())
    }

    async fn load_attendees(
        &self,
        event_ids: &[String],
    ) -> Result<HashMap<String, Vec<CalendarAttendee>>> {
        if event_ids.is_empty() {
            return Ok(HashMap::new());
        }

        let placeholders = std::iter::repeat_n("?", event_ids.len())
            .collect::<Vec<_>>()
            .join(", ");
        let query = format!(
            "SELECT id, event_id, email, common_name, role, partstat, rsvp, is_organizer \
             FROM calendar_attendees WHERE event_id IN ({placeholders})"
        );

        let mut statement = sqlx::query(&query);
        for event_id in event_ids {
            statement = statement.bind(event_id);
        }

        let rows = statement
            .fetch_all(&self.pool)
            .await
            .context("failed to load calendar attendees")?;

        let mut grouped = HashMap::new();
        for row in rows {
            let attendee = CalendarAttendee {
                id: row.try_get("id")?,
                event_id: row.try_get("event_id")?,
                email: row.try_get("email")?,
                common_name: row.try_get("common_name")?,
                role: row.try_get("role")?,
                partstat: row.try_get("partstat")?,
                rsvp: row.try_get::<i64, _>("rsvp")? != 0,
                is_organizer: row.try_get::<i64, _>("is_organizer")? != 0,
            };
            grouped
                .entry(attendee.event_id.clone())
                .or_insert_with(Vec::new)
                .push(attendee);
        }

        Ok(grouped)
    }
}

fn row_to_source_state(row: &sqlx::sqlite::SqliteRow) -> Result<CalendarSourceState> {
    Ok(CalendarSourceState {
        source_id: row.try_get("source_id")?,
        provider_kind: row.try_get("provider_kind")?,
        base_url: row.try_get("base_url")?,
        principal_url: row.try_get("principal_url")?,
        home_set_url: row.try_get("home_set_url")?,
        auth_kind: row.try_get("auth_kind")?,
        last_discovery_at: row.try_get("last_discovery_at")?,
        last_sync_at: row.try_get("last_sync_at")?,
        last_successful_sync_at: row.try_get("last_successful_sync_at")?,
        last_error: row.try_get("last_error")?,
        sync_status: row.try_get("sync_status")?,
    })
}

fn row_to_calendar(row: &sqlx::sqlite::SqliteRow) -> Result<CalendarCollection> {
    Ok(CalendarCollection {
        href: row.try_get("href")?,
        display_name: row.try_get("display_name")?,
        description: row.try_get("description")?,
        color: row.try_get("color")?,
        timezone: row.try_get("timezone")?,
        ctag: row.try_get("ctag")?,
        sync_token: row.try_get("sync_token")?,
        is_selected: row.try_get::<i64, _>("is_selected")? != 0,
        discovered_at: row.try_get("discovered_at")?,
        last_synced_at: row.try_get("last_synced_at")?,
    })
}

fn row_to_event(
    row: &sqlx::sqlite::SqliteRow,
    attendees: &HashMap<String, Vec<CalendarAttendee>>,
) -> Result<CalendarEvent> {
    let event_id: String = row.try_get("id")?;
    Ok(CalendarEvent {
        id: event_id.clone(),
        resource_id: row.try_get("resource_id")?,
        calendar_href: row.try_get("calendar_href")?,
        remote_href: row.try_get("remote_href")?,
        remote_uid: row.try_get("remote_uid")?,
        recurrence_id_utc: row.try_get("recurrence_id_utc")?,
        summary: row.try_get("summary")?,
        description: row.try_get("description")?,
        location: row.try_get("location")?,
        status: row.try_get("status")?,
        organizer_name: row.try_get("organizer_name")?,
        organizer_email: row.try_get("organizer_email")?,
        start_at_utc: row.try_get("start_at_utc")?,
        end_at_utc: row.try_get("end_at_utc")?,
        timezone: row.try_get("timezone")?,
        all_day: row.try_get::<i64, _>("all_day")? != 0,
        recurrence_rule: row.try_get("recurrence_rule")?,
        recurrence_exdates_json: row.try_get("recurrence_exdates_json")?,
        sequence: row.try_get("sequence")?,
        transparency: row.try_get("transparency")?,
        etag: row.try_get("etag")?,
        raw_ics: row.try_get("raw_ics")?,
        deleted: row.try_get::<i64, _>("deleted")? != 0,
        attendees: attendees.get(&event_id).cloned().unwrap_or_default(),
    })
}

fn row_to_proposal(row: &sqlx::sqlite::SqliteRow) -> Result<CalendarChangeProposal> {
    let action = match row.try_get::<String, _>("action")?.as_str() {
        "create" => CalendarProposalAction::Create,
        "update" => CalendarProposalAction::Update,
        "delete" => CalendarProposalAction::Delete,
        other => return Err(anyhow::anyhow!("unknown calendar proposal action '{other}'").into()),
    };
    let status = match row.try_get::<String, _>("status")?.as_str() {
        "pending" => CalendarProposalStatus::Pending,
        "applied" => CalendarProposalStatus::Applied,
        "failed" => CalendarProposalStatus::Failed,
        "cancelled" => CalendarProposalStatus::Cancelled,
        "expired" => CalendarProposalStatus::Expired,
        other => return Err(anyhow::anyhow!("unknown calendar proposal status '{other}'").into()),
    };

    Ok(CalendarChangeProposal {
        id: row.try_get("id")?,
        action,
        status,
        event_id: row.try_get("event_id")?,
        summary: row.try_get("summary")?,
        diff: row.try_get("diff")?,
        basis_etag: row.try_get("basis_etag")?,
        draft: serde_json::from_str(
            &row.try_get::<String, _>("draft_json")
                .context("missing calendar proposal draft json")?,
        )
        .context("failed to deserialize calendar proposal draft")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
        applied_at: row.try_get("applied_at")?,
        error: row.try_get("error")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn setup_store() -> CalendarStore {
        let pool = SqlitePool::connect("sqlite::memory:")
            .await
            .expect("failed to create in-memory pool");
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("failed to run migrations");
        CalendarStore::new(pool)
    }

    fn calendar_collection(href: &str, sync_token: Option<&str>) -> CalendarCollection {
        CalendarCollection {
            href: href.to_string(),
            display_name: Some("Calendar".to_string()),
            description: None,
            color: None,
            timezone: Some("UTC".to_string()),
            ctag: Some("ctag-1".to_string()),
            sync_token: sync_token.map(str::to_string),
            is_selected: true,
            discovered_at: "2026-03-28T09:00:00Z".to_string(),
            last_synced_at: Some("2026-03-28T09:00:00Z".to_string()),
        }
    }

    async fn seed_source(store: &CalendarStore) {
        store
            .save_source_state(&CalendarSourceState {
                source_id: "default".to_string(),
                provider_kind: "caldav".to_string(),
                base_url: Some("https://example.com/caldav".to_string()),
                principal_url: None,
                home_set_url: None,
                auth_kind: "basic".to_string(),
                last_discovery_at: None,
                last_sync_at: None,
                last_successful_sync_at: None,
                last_error: None,
                sync_status: Some("discovered".to_string()),
            })
            .await
            .expect("failed to seed calendar source");
    }

    #[tokio::test]
    async fn discovery_refresh_preserves_existing_sync_token_until_sync_applies() {
        let store = setup_store().await;
        let href = "https://example.com/calendars/main/";
        seed_source(&store).await;

        store
            .replace_discovered_calendars(
                "default",
                &[calendar_collection(href, Some("token-old"))],
                Some(href),
            )
            .await
            .expect("failed to save initial discovery");

        store
            .replace_discovered_calendars(
                "default",
                &[calendar_collection(href, Some("token-new-from-discovery"))],
                Some(href),
            )
            .await
            .expect("failed to refresh discovery");

        let calendars = store
            .list_calendars("default")
            .await
            .expect("failed to list calendars");

        assert_eq!(calendars.len(), 1);
        assert_eq!(calendars[0].sync_token.as_deref(), Some("token-old"));
    }

    #[tokio::test]
    async fn apply_sync_delta_advances_sync_token() {
        let store = setup_store().await;
        let href = "https://example.com/calendars/main/";
        seed_source(&store).await;

        store
            .replace_discovered_calendars(
                "default",
                &[calendar_collection(href, Some("token-old"))],
                Some(href),
            )
            .await
            .expect("failed to save initial discovery");

        store
            .apply_sync_delta(ApplySyncDeltaParams {
                calendar_href: href,
                resources: &[],
                deleted_hrefs: &[],
                sync_token: Some("token-new"),
                ctag: Some("ctag-2"),
                full_refresh: false,
                synced_at: "2026-03-28T09:05:00Z",
            })
            .await
            .expect("failed to apply sync delta");

        let calendars = store
            .list_calendars("default")
            .await
            .expect("failed to list calendars");

        assert_eq!(calendars.len(), 1);
        assert_eq!(calendars[0].sync_token.as_deref(), Some("token-new"));
        assert_eq!(calendars[0].ctag.as_deref(), Some("ctag-2"));
        assert_eq!(
            calendars[0].last_synced_at.as_deref(),
            Some("2026-03-28T09:05:00Z")
        );
    }

    #[tokio::test]
    async fn clear_calendar_sync_token_resets_selected_calendar_cursor() {
        let store = setup_store().await;
        let href = "https://example.com/calendars/main/";
        seed_source(&store).await;

        store
            .replace_discovered_calendars(
                "default",
                &[calendar_collection(href, Some("token-old"))],
                Some(href),
            )
            .await
            .expect("failed to save initial discovery");

        store
            .clear_calendar_sync_token(href)
            .await
            .expect("failed to clear sync token");

        let calendars = store
            .list_calendars("default")
            .await
            .expect("failed to list calendars");

        assert_eq!(calendars.len(), 1);
        assert_eq!(calendars[0].sync_token, None);
    }
}
