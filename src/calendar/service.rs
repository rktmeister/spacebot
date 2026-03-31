//! Calendar sync orchestration and proposal application.

use crate::calendar::caldav::CalDavClient;
use crate::calendar::google_meet::GoogleMeetClient;
use crate::calendar::ics::{
    EventScheduleContext, build_new_event_resource, expand_occurrences, export_resources_to_ics,
    normalize_timezone_label, update_existing_resource,
};
use crate::calendar::invite_email::{
    InviteEmail, build_cancel_invite_email, build_request_invite_email,
};
use crate::calendar::store::CalendarStore;
use crate::calendar::types::{
    CalendarAvailabilitySlot, CalendarChangeProposal, CalendarCollection, CalendarEvent,
    CalendarEventDraft, CalendarOverview, CalendarProposalAction, CalendarProposalStatus,
    CalendarSourceState, CalendarSyncSummary,
};
use crate::config::{CalendarAuthKind, CalendarConfig, CalendarProviderKind, RuntimeConfig};
use crate::error::Result;
use crate::messaging::email::EmailAdapter;

use anyhow::{Context as _, anyhow};
use chrono::{DateTime, Duration, Utc};
use std::sync::Arc;
use tokio::sync::Notify;

const DEFAULT_SOURCE_ID: &str = "default";
const GOOGLE_MEET_LINE_PREFIX: &str = "Google Meet: ";

#[derive(Debug)]
pub struct CalendarService {
    store: Arc<CalendarStore>,
    runtime_config: Arc<RuntimeConfig>,
    sync_notify: Notify,
}

impl CalendarService {
    pub fn new(store: Arc<CalendarStore>, runtime_config: Arc<RuntimeConfig>) -> Arc<Self> {
        Arc::new(Self {
            store,
            runtime_config,
            sync_notify: Notify::new(),
        })
    }

    pub fn start(self: &Arc<Self>) {
        let service = self.clone();
        tokio::spawn(async move {
            if let Err(error) = service.sync_now().await {
                tracing::warn!(%error, "initial calendar sync failed");
            }

            loop {
                let config = service.runtime_config.calendar.load().as_ref().clone();
                let sleep_secs = config.sync_interval_secs.max(30);
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_secs(sleep_secs)) => {},
                    _ = service.sync_notify.notified() => {},
                }

                if !service.runtime_config.calendar.load().enabled {
                    continue;
                }

                if let Err(error) = service.sync_now().await {
                    tracing::warn!(%error, "background calendar sync failed");
                }
            }
        });
    }

    pub fn request_sync(&self) {
        self.sync_notify.notify_one();
    }

    pub async fn overview(&self, ics_export_url: Option<String>) -> Result<CalendarOverview> {
        let config = self.runtime_config.calendar.load().as_ref().clone();
        let source = self.store.load_source_state(DEFAULT_SOURCE_ID).await?;
        let calendars = self.store.list_calendars(DEFAULT_SOURCE_ID).await?;

        Ok(CalendarOverview {
            configured: config.base_url.is_some()
                && config.username.is_some()
                && config.password.is_some(),
            enabled: config.enabled,
            read_only: config.read_only,
            provider_kind: config.provider_kind.to_string(),
            auth_kind: config.auth_kind.to_string(),
            selected_calendar_href: config.selected_calendar_href,
            ics_export_url,
            source,
            calendars,
        })
    }

    pub async fn sync_now(&self) -> Result<CalendarSyncSummary> {
        let sync_started_at = Utc::now().to_rfc3339();
        let config = self.runtime_config.calendar.load().as_ref().clone();
        if !config.enabled {
            return Ok(CalendarSyncSummary {
                discovered_calendar_count: 0,
                selected_calendar_href: config.selected_calendar_href,
                synced_resource_count: 0,
                created_event_count: 0,
                updated_event_count: 0,
                deleted_event_count: 0,
                sync_started_at: sync_started_at.clone(),
                sync_finished_at: sync_started_at,
                mode: "disabled".to_string(),
            });
        }

        let client = self.build_client(&config)?;
        self.store
            .save_source_state(&CalendarSourceState {
                source_id: DEFAULT_SOURCE_ID.to_string(),
                provider_kind: config.provider_kind.to_string(),
                base_url: config.base_url.clone(),
                principal_url: None,
                home_set_url: None,
                auth_kind: config.auth_kind.to_string(),
                last_discovery_at: None,
                last_sync_at: Some(sync_started_at.clone()),
                last_successful_sync_at: None,
                last_error: None,
                sync_status: Some("syncing".to_string()),
            })
            .await?;

        let existing_calendars = self.store.list_calendars(DEFAULT_SOURCE_ID).await?;
        let outcome: Result<CalendarSyncSummary> = async {
            let discovery = client.discover().await?;
            let discovered_at = Utc::now().to_rfc3339();
            let selected_href = config
                .selected_calendar_href
                .as_deref()
                .map(|href| client.resolve_href(href))
                .transpose()?;
            let calendars = discovery
                .calendars
                .iter()
                .map(|calendar| CalendarCollection {
                    href: calendar.href.clone(),
                    display_name: calendar.display_name.clone(),
                    description: calendar.description.clone(),
                    color: calendar.color.clone(),
                    timezone: normalize_timezone_label(calendar.timezone.as_deref()),
                    ctag: calendar.ctag.clone(),
                    sync_token: calendar.sync_token.clone(),
                    is_selected: selected_href.as_deref() == Some(calendar.href.as_str()),
                    discovered_at: discovered_at.clone(),
                    last_synced_at: None,
                })
                .collect::<Vec<_>>();

            self.store
                .replace_discovered_calendars(
                    DEFAULT_SOURCE_ID,
                    &calendars,
                    selected_href.as_deref(),
                )
                .await?;

            self.store
                .save_source_state(&CalendarSourceState {
                    source_id: DEFAULT_SOURCE_ID.to_string(),
                    provider_kind: config.provider_kind.to_string(),
                    base_url: config.base_url.clone(),
                    principal_url: discovery.principal_url.clone(),
                    home_set_url: discovery.home_set_url.clone(),
                    auth_kind: config.auth_kind.to_string(),
                    last_discovery_at: Some(discovered_at.clone()),
                    last_sync_at: Some(sync_started_at.clone()),
                    last_successful_sync_at: None,
                    last_error: None,
                    sync_status: Some("discovered".to_string()),
                })
                .await?;

            let Some(selected_href) = selected_href else {
                let sync_finished_at = Utc::now().to_rfc3339();
                self.store
                    .save_source_state(&CalendarSourceState {
                        source_id: DEFAULT_SOURCE_ID.to_string(),
                        provider_kind: config.provider_kind.to_string(),
                        base_url: config.base_url.clone(),
                        principal_url: discovery.principal_url,
                        home_set_url: discovery.home_set_url,
                        auth_kind: config.auth_kind.to_string(),
                        last_discovery_at: Some(discovered_at),
                        last_sync_at: Some(sync_started_at.clone()),
                        last_successful_sync_at: Some(sync_finished_at.clone()),
                        last_error: None,
                        sync_status: Some("awaiting_selection".to_string()),
                    })
                    .await?;

                return Ok(CalendarSyncSummary {
                    discovered_calendar_count: calendars.len(),
                    selected_calendar_href: None,
                    synced_resource_count: 0,
                    created_event_count: 0,
                    updated_event_count: 0,
                    deleted_event_count: 0,
                    sync_started_at: sync_started_at.clone(),
                    sync_finished_at,
                    mode: "discovery".to_string(),
                });
            };

            let current_sync_token = previous_sync_token(&existing_calendars, &selected_href);
            let delta = client
                .sync_calendar(&selected_href, current_sync_token.as_deref())
                .await?;
            let sync_finished_at = Utc::now().to_rfc3339();
            let apply_result = self
                .store
                .apply_sync_delta(crate::calendar::store::ApplySyncDeltaParams {
                    calendar_href: &selected_href,
                    resources: &delta.resources,
                    deleted_hrefs: &delta.deleted_hrefs,
                    sync_token: delta.sync_token.as_deref(),
                    ctag: delta.ctag.as_deref(),
                    full_refresh: delta.mode == "full",
                    synced_at: &sync_finished_at,
                })
                .await?;

            self.store
                .save_source_state(&CalendarSourceState {
                    source_id: DEFAULT_SOURCE_ID.to_string(),
                    provider_kind: config.provider_kind.to_string(),
                    base_url: config.base_url.clone(),
                    principal_url: discovery.principal_url,
                    home_set_url: discovery.home_set_url,
                    auth_kind: config.auth_kind.to_string(),
                    last_discovery_at: Some(discovered_at),
                    last_sync_at: Some(sync_started_at.clone()),
                    last_successful_sync_at: Some(sync_finished_at.clone()),
                    last_error: None,
                    sync_status: Some("ready".to_string()),
                })
                .await?;

            Ok(CalendarSyncSummary {
                discovered_calendar_count: calendars.len(),
                selected_calendar_href: Some(selected_href),
                synced_resource_count: apply_result.synced_resource_count,
                created_event_count: apply_result.created_event_count,
                updated_event_count: apply_result.updated_event_count,
                deleted_event_count: apply_result.deleted_event_count,
                sync_started_at: sync_started_at.clone(),
                sync_finished_at,
                mode: delta.mode.to_string(),
            })
        }
        .await;

        if let Err(error) = &outcome {
            self.store
                .save_source_state(&CalendarSourceState {
                    source_id: DEFAULT_SOURCE_ID.to_string(),
                    provider_kind: config.provider_kind.to_string(),
                    base_url: config.base_url.clone(),
                    principal_url: None,
                    home_set_url: None,
                    auth_kind: config.auth_kind.to_string(),
                    last_discovery_at: None,
                    last_sync_at: Some(sync_started_at.clone()),
                    last_successful_sync_at: None,
                    last_error: Some(error.to_string()),
                    sync_status: Some("failed".to_string()),
                })
                .await?;
        }

        outcome
    }

    pub async fn list_occurrences(
        &self,
        start_at: DateTime<Utc>,
        end_at: DateTime<Utc>,
    ) -> Result<Vec<crate::calendar::types::CalendarOccurrence>> {
        let selected_href = self.selected_calendar_href()?;
        let events = self.store.list_active_events(&selected_href).await?;
        expand_occurrences(&events, start_at, end_at).map_err(Into::into)
    }

    pub async fn get_event(&self, event_id: &str) -> Result<Option<CalendarEvent>> {
        self.store.get_event(event_id).await
    }

    pub async fn find_free_time(
        &self,
        start_at: DateTime<Utc>,
        end_at: DateTime<Utc>,
        duration_minutes: i64,
    ) -> Result<Vec<CalendarAvailabilitySlot>> {
        let duration = Duration::minutes(duration_minutes.max(1));
        let mut occurrences = self.list_occurrences(start_at, end_at).await?;
        occurrences.sort_by(|left, right| left.start_at.cmp(&right.start_at));

        let mut cursor = start_at;
        let mut slots = Vec::new();
        for occurrence in occurrences {
            let busy_start = DateTime::parse_from_rfc3339(&occurrence.start_at)
                .with_context(|| {
                    format!(
                        "calendar occurrence '{}' has invalid start_at '{}'",
                        occurrence.occurrence_id, occurrence.start_at
                    )
                })?
                .with_timezone(&Utc);
            let busy_end = DateTime::parse_from_rfc3339(&occurrence.end_at)
                .with_context(|| {
                    format!(
                        "calendar occurrence '{}' has invalid end_at '{}'",
                        occurrence.occurrence_id, occurrence.end_at
                    )
                })?
                .with_timezone(&Utc);
            if busy_start - cursor >= duration {
                slots.push(CalendarAvailabilitySlot {
                    start_at: cursor.to_rfc3339(),
                    end_at: busy_start.to_rfc3339(),
                });
            }
            if busy_end > cursor {
                cursor = busy_end;
            }
        }

        if end_at - cursor >= duration {
            slots.push(CalendarAvailabilitySlot {
                start_at: cursor.to_rfc3339(),
                end_at: end_at.to_rfc3339(),
            });
        }

        Ok(slots)
    }

    pub async fn propose_create(
        &self,
        draft: CalendarEventDraft,
    ) -> Result<CalendarChangeProposal> {
        let default_timezone = self.default_event_timezone();
        let draft = normalize_draft_timezone(draft, default_timezone.as_deref());
        self.store
            .create_change_proposal(
                CalendarProposalAction::Create,
                None,
                &format!("Create event '{}'", draft.summary),
                &render_create_diff(&draft),
                None,
                &draft,
            )
            .await
    }

    pub async fn propose_update(
        &self,
        event_id: &str,
        draft: CalendarEventDraft,
    ) -> Result<CalendarChangeProposal> {
        let default_timezone = self.default_event_timezone();
        let draft = normalize_draft_timezone(draft, default_timezone.as_deref());
        let current = self
            .store
            .get_event(event_id)
            .await?
            .ok_or_else(|| anyhow!("calendar event '{event_id}' not found"))?;
        self.store
            .create_change_proposal(
                CalendarProposalAction::Update,
                Some(event_id),
                &format!(
                    "Update event '{}'",
                    current.summary.as_deref().unwrap_or("Untitled event")
                ),
                &render_update_diff(&current, &draft),
                current.etag.as_deref(),
                &draft,
            )
            .await
    }

    pub async fn propose_delete(&self, event_id: &str) -> Result<CalendarChangeProposal> {
        let current = self
            .store
            .get_event(event_id)
            .await?
            .ok_or_else(|| anyhow!("calendar event '{event_id}' not found"))?;
        let draft = CalendarEventDraft {
            summary: current
                .summary
                .clone()
                .unwrap_or_else(|| "Untitled event".to_string()),
            description: current.description.clone(),
            location: current.location.clone(),
            start_at: current.start_at_utc.clone(),
            end_at: current.end_at_utc.clone(),
            timezone: current.timezone.clone(),
            all_day: current.all_day,
            recurrence_rule: current.recurrence_rule.clone(),
            attendees: current
                .attendees
                .iter()
                .filter_map(|attendee| {
                    attendee.email.as_ref().map(|email| {
                        crate::calendar::types::CalendarAttendeeInput {
                            email: email.clone(),
                            common_name: attendee.common_name.clone(),
                            role: attendee.role.clone(),
                            partstat: attendee.partstat.clone(),
                            rsvp: attendee.rsvp,
                        }
                    })
                })
                .collect(),
        };
        self.store
            .create_change_proposal(
                CalendarProposalAction::Delete,
                Some(event_id),
                &format!(
                    "Delete event '{}'",
                    current.summary.as_deref().unwrap_or("Untitled event")
                ),
                &render_delete_diff(&current),
                current.etag.as_deref(),
                &draft,
            )
            .await
    }

    pub async fn apply_proposal(&self, proposal_id: &str) -> Result<CalendarChangeProposal> {
        let config = self.runtime_config.calendar.load().as_ref().clone();
        if config.read_only {
            return Err(anyhow!("calendar is configured read-only").into());
        }

        let proposal = self
            .store
            .get_change_proposal(proposal_id)
            .await?
            .ok_or_else(|| anyhow!("calendar proposal '{proposal_id}' not found"))?;
        if proposal.status != CalendarProposalStatus::Pending {
            return Err(anyhow!("calendar proposal '{proposal_id}' is not pending").into());
        }

        let client = self.build_client(&config)?;
        let outcome: Result<CalendarChangeProposal> = async {
            let mut expected_create_uid = None;
            let mut expected_deleted_resource_href = None;
            let mut force_full_sync_calendar_href = None;
            let mut mailer = None;
            let mut pending_invites = Vec::new();
            match proposal.action {
                CalendarProposalAction::Create => {
                    let selected_href = self.selected_calendar_href()?;
                    let uid = uuid::Uuid::new_v4().to_string();
                    let remote_href =
                        format!("{}/{}.ics", selected_href.trim_end_matches('/'), uid);
                    let draft = self
                        .prepare_draft_for_remote_write(&config, None, &proposal.draft)
                        .await?;
                    let schedule =
                        scheduling_context(&config, draft.attendees.len(), None::<&CalendarEvent>)?;
                    let raw_ics = build_new_event_resource(&draft, &uid, 0, schedule.as_ref())?;
                    client
                        .put_resource(&remote_href, &raw_ics, None, true)
                        .await?;
                    if let Some(schedule) = schedule.as_ref() {
                        if mailer.is_none() {
                            mailer = self.build_invite_mailer(&config)?;
                        }
                        if let Some(invite_email) =
                            build_request_invite_email(&draft, schedule, &raw_ics, false)?
                        {
                            pending_invites.push(invite_email);
                        }
                    }
                    expected_create_uid = Some((selected_href, uid));
                }
                CalendarProposalAction::Update => {
                    let event_id = proposal
                        .event_id
                        .as_deref()
                        .ok_or_else(|| anyhow!("update proposal missing event_id"))?;
                    let current = self
                        .store
                        .get_event(event_id)
                        .await?
                        .ok_or_else(|| anyhow!("target calendar event no longer exists"))?;
                    let draft = self
                        .prepare_draft_for_remote_write(&config, Some(&current), &proposal.draft)
                        .await?;
                    let schedule = scheduling_context(&config, draft.attendees.len(), Some(&current))?;
                    let updated_ics = update_existing_resource(
                        &current.raw_ics,
                        &current,
                        &draft,
                        schedule.as_ref(),
                    )?;
                    client
                        .put_resource(
                            &current.remote_href,
                            &updated_ics,
                            proposal.basis_etag.as_deref().or(current.etag.as_deref()),
                            false,
                        )
                        .await?;
                    if let Some(schedule) = schedule.as_ref() {
                        if mailer.is_none() {
                            mailer = self.build_invite_mailer(&config)?;
                        }
                        if let Some(invite_email) =
                            build_request_invite_email(&draft, schedule, &updated_ics, true)?
                        {
                            pending_invites.push(invite_email);
                        }
                    }
                }
                CalendarProposalAction::Delete => {
                    let event_id = proposal
                        .event_id
                        .as_deref()
                        .ok_or_else(|| anyhow!("delete proposal missing event_id"))?;
                    let current = self
                        .store
                        .get_event(event_id)
                        .await?
                        .ok_or_else(|| anyhow!("target calendar event no longer exists"))?;
                    let etag = proposal.basis_etag.as_deref().or(current.etag.as_deref());
                    let schedule =
                        scheduling_context(&config, current.attendees.len(), Some(&current))?;
                    client.delete_resource(&current.remote_href, etag).await?;
                    if let Some(schedule) = schedule.as_ref() {
                        if mailer.is_none() {
                            mailer = self.build_invite_mailer(&config)?;
                        }
                        if let Some(invite_email) = build_cancel_invite_email(&current, schedule)? {
                            pending_invites.push(invite_email);
                        }
                    }
                    expected_deleted_resource_href = Some(current.remote_href.clone());
                    force_full_sync_calendar_href = Some(current.calendar_href.clone());
                }
            }

            if let Some(calendar_href) = force_full_sync_calendar_href {
                self.store.clear_calendar_sync_token(&calendar_href).await?;
            }
            self.sync_now().await?;
            if let Some((calendar_href, remote_uid)) = expected_create_uid {
                self.store
                    .find_series_master(&calendar_href, &remote_uid)
                    .await?
                    .ok_or_else(|| {
                        anyhow!(
                            "calendar create applied remotely but did not appear in the local mirror"
                        )
                    })?;
            }
            if let Some(remote_href) = expected_deleted_resource_href {
                if self.store.has_active_resource(&remote_href).await? {
                    return Err(anyhow!(
                        "calendar delete applied remotely but the event still appears in the local mirror"
                    )
                    .into());
                }
            }
            self.send_pending_invites(mailer.as_ref(), pending_invites)
                .await?;
            let applied_at = Utc::now().to_rfc3339();
            self.store
                .update_proposal_status(
                    proposal_id,
                    CalendarProposalStatus::Applied,
                    None,
                    Some(&applied_at),
                )
                .await?;
            self.store
                .get_change_proposal(proposal_id)
                .await?
                .ok_or_else(|| anyhow!("proposal disappeared after apply"))
                .map_err(crate::Error::from)
        }
        .await;

        if let Err(error) = &outcome {
            self.store
                .update_proposal_status(
                    proposal_id,
                    CalendarProposalStatus::Failed,
                    Some(&error.to_string()),
                    None,
                )
                .await?;
        }

        outcome
    }

    pub async fn export_ics(&self, token: &str) -> Result<String> {
        let config = self.runtime_config.calendar.load().as_ref().clone();
        let expected = config
            .ics_export_token
            .as_deref()
            .ok_or_else(|| anyhow!("ICS export is not configured"))?;
        if expected != token {
            return Err(anyhow!("invalid ICS export token").into());
        }
        let selected_href = self.selected_calendar_href()?;
        let resources = self.store.load_export_resources(&selected_href).await?;
        Ok(export_resources_to_ics(&resources))
    }

    fn build_client(&self, config: &CalendarConfig) -> Result<CalDavClient> {
        if config.provider_kind != CalendarProviderKind::CalDav {
            return Err(anyhow!("only CalDAV is implemented in v1").into());
        }
        if config.auth_kind != CalendarAuthKind::Basic {
            return Err(anyhow!("only basic-auth CalDAV is implemented in v1").into());
        }
        let base_url = config
            .base_url
            .as_deref()
            .ok_or_else(|| anyhow!("calendar.base_url is required"))?;
        let username = config
            .username
            .as_deref()
            .ok_or_else(|| anyhow!("calendar.username is required"))?;
        let password = config
            .password
            .as_deref()
            .ok_or_else(|| anyhow!("calendar.password is required"))?;
        CalDavClient::new(base_url, username, password).map_err(Into::into)
    }

    fn selected_calendar_href(&self) -> Result<String> {
        let config = self.runtime_config.calendar.load().as_ref().clone();
        let href = config
            .selected_calendar_href
            .clone()
            .ok_or_else(|| anyhow!("calendar.selected_calendar_href is not configured"))?;
        if href.contains("://") {
            return Ok(href);
        }

        let client = self.build_client(&config)?;
        client.resolve_href(&href).map_err(Into::into)
    }

    fn default_event_timezone(&self) -> Option<String> {
        self.runtime_config
            .user_timezone
            .load()
            .as_ref()
            .clone()
            .or_else(|| self.runtime_config.cron_timezone.load().as_ref().clone())
            .map(|timezone| timezone.trim().to_string())
            .filter(|timezone| !timezone.is_empty())
    }

    fn build_google_meet_client(&self, config: &CalendarConfig) -> Result<GoogleMeetClient> {
        let client_id = trimmed_config_value(config.google_meet_client_id.as_deref())
            .ok_or_else(|| anyhow!("calendar.google_meet_client_id is required"))?;
        let client_secret = trimmed_config_value(config.google_meet_client_secret.as_deref())
            .ok_or_else(|| anyhow!("calendar.google_meet_client_secret is required"))?;
        let refresh_token = trimmed_config_value(config.google_meet_refresh_token.as_deref())
            .ok_or_else(|| anyhow!("calendar.google_meet_refresh_token is required"))?;
        GoogleMeetClient::new(
            client_id,
            client_secret,
            refresh_token,
            config.google_meet_token_url.as_deref(),
            config.google_meet_access_type,
        )
        .map_err(Into::into)
    }

    async fn create_google_meet_link(&self, config: &CalendarConfig) -> Result<String> {
        let client = self.build_google_meet_client(config)?;
        client.create_space().await.map_err(Into::into)
    }

    async fn prepare_draft_for_remote_write(
        &self,
        config: &CalendarConfig,
        current_event: Option<&CalendarEvent>,
        draft: &CalendarEventDraft,
    ) -> Result<CalendarEventDraft> {
        let existing_meet_url = extract_managed_google_meet_url(draft.description.as_deref())
            .or_else(|| {
                current_event
                    .and_then(|event| extract_managed_google_meet_url(event.description.as_deref()))
            });
        let user_description = strip_managed_google_meet_line(draft.description.as_deref());
        let meeting_url = if let Some(url) = existing_meet_url {
            Some(url)
        } else if config.google_meet_enabled && !draft.attendees.is_empty() {
            Some(self.create_google_meet_link(config).await?)
        } else {
            None
        };

        let mut prepared = draft.clone();
        prepared.description = render_description_with_google_meet(
            user_description.as_deref(),
            meeting_url.as_deref(),
        );
        Ok(prepared)
    }

    fn build_invite_mailer(&self, config: &CalendarConfig) -> Result<Option<EmailAdapter>> {
        if !config.smtp_invites_enabled {
            return Ok(None);
        }

        let email_config = self
            .runtime_config
            .email
            .load()
            .as_ref()
            .clone()
            .ok_or_else(|| {
                anyhow!("calendar.smtp_invites_enabled requires messaging.email SMTP configuration")
            })?;

        if !email_config.enabled {
            return Err(anyhow!(
                "calendar.smtp_invites_enabled requires messaging.email.enabled = true"
            )
            .into());
        }

        if !email_config.allow_outbound {
            return Err(anyhow!(
                "calendar.smtp_invites_enabled requires messaging.email.allow_outbound = true"
            )
            .into());
        }

        EmailAdapter::from_config(&email_config).map(Some)
    }

    async fn send_pending_invites(
        &self,
        mailer: Option<&EmailAdapter>,
        pending_invites: Vec<InviteEmail>,
    ) -> Result<()> {
        let Some(mailer) = mailer else {
            return Ok(());
        };

        let mut failures = Vec::new();
        for invite in pending_invites {
            for recipient in &invite.recipients {
                if let Err(error) = mailer
                    .send_email(
                        recipient,
                        &invite.subject,
                        invite.body.clone(),
                        None,
                        Vec::new(),
                        Some((
                            invite.attachment_name.clone(),
                            invite.attachment_bytes.clone(),
                            invite.attachment_mime_type.clone(),
                        )),
                    )
                    .await
                {
                    failures.push(format!("{recipient}: {error}"));
                }
            }
        }

        if failures.is_empty() {
            return Ok(());
        }

        Err(anyhow!(
            "calendar event was written, but SMTP invite delivery failed: {}",
            failures.join("; ")
        )
        .into())
    }
}

fn scheduling_context(
    config: &CalendarConfig,
    attendee_count: usize,
    current_event: Option<&CalendarEvent>,
) -> Result<Option<EventScheduleContext>> {
    if attendee_count == 0 {
        return Ok(None);
    }

    let organizer_name = current_event
        .and_then(|event| trimmed_config_value(event.organizer_name.as_deref()))
        .map(str::to_string)
        .or_else(|| trimmed_config_value(config.organizer_name.as_deref()).map(str::to_string));
    let organizer_email = current_event
        .and_then(|event| trimmed_config_value(event.organizer_email.as_deref()))
        .map(str::to_string)
        .or_else(|| trimmed_config_value(config.organizer_email.as_deref()).map(str::to_string))
        .ok_or_else(|| {
            anyhow!("calendar.organizer_email is required for attendee-bearing events")
        })?;

    Ok(Some(EventScheduleContext {
        organizer_name,
        organizer_email,
    }))
}

fn trimmed_config_value(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn extract_managed_google_meet_url(description: Option<&str>) -> Option<String> {
    description
        .into_iter()
        .flat_map(str::lines)
        .find_map(|line| {
            let trimmed = line.trim();
            trimmed
                .strip_prefix(GOOGLE_MEET_LINE_PREFIX)
                .map(str::trim)
                .filter(|value| is_google_meet_url(value))
                .map(ToOwned::to_owned)
        })
}

fn strip_managed_google_meet_line(description: Option<&str>) -> Option<String> {
    let Some(description) = description else {
        return None;
    };

    let mut cleaned = Vec::new();
    let mut previous_blank = false;
    for line in description.lines() {
        let trimmed = line.trim();
        if trimmed
            .strip_prefix(GOOGLE_MEET_LINE_PREFIX)
            .is_some_and(|value| is_google_meet_url(value.trim()))
        {
            continue;
        }

        let blank = trimmed.is_empty();
        if blank && (cleaned.is_empty() || previous_blank) {
            continue;
        }

        cleaned.push(line.trim_end().to_string());
        previous_blank = blank;
    }

    while cleaned.last().is_some_and(|line| line.trim().is_empty()) {
        cleaned.pop();
    }

    (!cleaned.is_empty()).then_some(cleaned.join("\n"))
}

fn render_description_with_google_meet(
    base_description: Option<&str>,
    meeting_url: Option<&str>,
) -> Option<String> {
    match (
        base_description
            .map(str::trim)
            .filter(|value| !value.is_empty()),
        meeting_url.map(str::trim).filter(|value| !value.is_empty()),
    ) {
        (Some(description), Some(url)) => {
            Some(format!("{description}\n\n{GOOGLE_MEET_LINE_PREFIX}{url}"))
        }
        (Some(description), None) => Some(description.to_string()),
        (None, Some(url)) => Some(format!("{GOOGLE_MEET_LINE_PREFIX}{url}")),
        (None, None) => None,
    }
}

fn is_google_meet_url(value: &str) -> bool {
    value
        .trim()
        .strip_prefix("https://meet.google.com/")
        .is_some()
}

fn render_create_diff(draft: &CalendarEventDraft) -> String {
    format!(
        "Create '{}'\nStart: {}\nEnd: {}\nTimezone: {}",
        draft.summary,
        draft.start_at,
        draft.end_at,
        draft.timezone.as_deref().unwrap_or("UTC"),
    )
}

fn render_update_diff(current: &CalendarEvent, draft: &CalendarEventDraft) -> String {
    let mut lines = vec![format!(
        "Title: {} -> {}",
        current.summary.as_deref().unwrap_or("Untitled event"),
        draft.summary
    )];
    lines.push(format!(
        "Start: {} -> {}",
        current.start_at_utc, draft.start_at
    ));
    lines.push(format!("End: {} -> {}", current.end_at_utc, draft.end_at));
    if current.location != draft.location {
        lines.push(format!(
            "Location: {} -> {}",
            current.location.as_deref().unwrap_or("(none)"),
            draft.location.as_deref().unwrap_or("(none)"),
        ));
    }
    if current.description != draft.description {
        lines.push("Description updated".to_string());
    }
    if current.timezone != draft.timezone {
        lines.push(format!(
            "Timezone: {} -> {}",
            current.timezone.as_deref().unwrap_or("UTC"),
            draft.timezone.as_deref().unwrap_or("UTC"),
        ));
    }
    lines.join("\n")
}

fn render_delete_diff(current: &CalendarEvent) -> String {
    format!(
        "Delete '{}'\nStart: {}\nEnd: {}",
        current.summary.as_deref().unwrap_or("Untitled event"),
        current.start_at_utc,
        current.end_at_utc,
    )
}

fn previous_sync_token(
    existing_calendars: &[CalendarCollection],
    selected_href: &str,
) -> Option<String> {
    existing_calendars
        .iter()
        .find(|calendar| calendar.href == selected_href)
        .and_then(|calendar| calendar.sync_token.clone())
}

fn normalize_draft_timezone(
    mut draft: CalendarEventDraft,
    default_timezone: Option<&str>,
) -> CalendarEventDraft {
    let missing_timezone = draft
        .timezone
        .as_deref()
        .map(str::trim)
        .is_none_or(|timezone| timezone.is_empty());

    draft.timezone = if missing_timezone {
        normalize_timezone_label(default_timezone)
    } else {
        normalize_timezone_label(draft.timezone.as_deref())
    };

    draft
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calendar::CalendarAttendee;

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

    #[test]
    fn previous_sync_token_uses_local_calendar_state() {
        let calendars = vec![
            calendar_collection("https://example.com/calendars/other/", Some("token-other")),
            calendar_collection("https://example.com/calendars/main/", Some("token-local")),
        ];

        assert_eq!(
            previous_sync_token(&calendars, "https://example.com/calendars/main/").as_deref(),
            Some("token-local")
        );
    }

    #[test]
    fn previous_sync_token_returns_none_for_new_calendar() {
        let calendars = vec![calendar_collection(
            "https://example.com/calendars/other/",
            Some("token-other"),
        )];

        assert_eq!(
            previous_sync_token(&calendars, "https://example.com/calendars/main/"),
            None
        );
    }

    #[test]
    fn normalize_draft_timezone_uses_default_when_missing() {
        let draft = CalendarEventDraft {
            summary: "ERP update".to_string(),
            description: None,
            location: None,
            start_at: "2026-03-30T09:00:00".to_string(),
            end_at: "2026-03-30T10:00:00".to_string(),
            timezone: None,
            all_day: false,
            recurrence_rule: None,
            attendees: Vec::new(),
        };

        let normalized = normalize_draft_timezone(draft, Some("Asia/Singapore"));
        assert_eq!(normalized.timezone.as_deref(), Some("Asia/Singapore"));
    }

    #[test]
    fn normalize_draft_timezone_preserves_explicit_timezone() {
        let draft = CalendarEventDraft {
            summary: "ERP update".to_string(),
            description: None,
            location: None,
            start_at: "2026-03-30T09:00:00".to_string(),
            end_at: "2026-03-30T10:00:00".to_string(),
            timezone: Some("UTC".to_string()),
            all_day: false,
            recurrence_rule: None,
            attendees: Vec::new(),
        };

        let normalized = normalize_draft_timezone(draft, Some("Asia/Singapore"));
        assert_eq!(normalized.timezone.as_deref(), Some("UTC"));
    }

    #[test]
    fn normalize_draft_timezone_canonicalizes_known_windows_timezone_names() {
        let draft = CalendarEventDraft {
            summary: "ERP update".to_string(),
            description: None,
            location: None,
            start_at: "2026-03-30T09:00:00".to_string(),
            end_at: "2026-03-30T10:00:00".to_string(),
            timezone: Some("Singapore Standard Time".to_string()),
            all_day: false,
            recurrence_rule: None,
            attendees: Vec::new(),
        };

        let normalized = normalize_draft_timezone(draft, Some("Asia/Singapore"));
        assert_eq!(normalized.timezone.as_deref(), Some("Asia/Singapore"));
    }

    #[test]
    fn scheduling_context_prefers_current_event_organizer() {
        let config = CalendarConfig {
            organizer_name: Some("Config Name".to_string()),
            organizer_email: Some("config@example.com".to_string()),
            ..CalendarConfig::default()
        };
        let current_event = CalendarEvent {
            id: "event-1".to_string(),
            resource_id: "resource-1".to_string(),
            calendar_href: "https://example.com/cal/".to_string(),
            remote_href: "https://example.com/cal/event-1.ics".to_string(),
            remote_uid: "uid-1".to_string(),
            recurrence_id_utc: None,
            summary: Some("Team sync".to_string()),
            description: None,
            location: None,
            status: Some("CONFIRMED".to_string()),
            organizer_name: Some("Current Name".to_string()),
            organizer_email: Some("current@example.com".to_string()),
            start_at_utc: "2026-03-30T01:00:00+00:00".to_string(),
            end_at_utc: "2026-03-30T02:00:00+00:00".to_string(),
            timezone: Some("UTC".to_string()),
            all_day: false,
            recurrence_rule: None,
            recurrence_exdates_json: None,
            sequence: 0,
            transparency: Some("OPAQUE".to_string()),
            etag: None,
            raw_ics: String::new(),
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

        let schedule = scheduling_context(&config, 1, Some(&current_event))
            .expect("schedule context should build")
            .expect("attendee-bearing events should schedule");

        assert_eq!(schedule.organizer_name.as_deref(), Some("Current Name"));
        assert_eq!(schedule.organizer_email, "current@example.com");
    }

    #[test]
    fn scheduling_context_requires_organizer_email_for_attendees() {
        let error = scheduling_context(&CalendarConfig::default(), 1, None::<&CalendarEvent>)
            .expect_err("attendee-bearing events should require organizer_email");

        assert!(
            error
                .to_string()
                .contains("calendar.organizer_email is required for attendee-bearing events")
        );
    }

    #[test]
    fn managed_google_meet_line_round_trips_cleanly() {
        let description = "Agenda line\n\nGoogle Meet: https://meet.google.com/abc-mnop-xyz\n";

        assert_eq!(
            extract_managed_google_meet_url(Some(description)).as_deref(),
            Some("https://meet.google.com/abc-mnop-xyz")
        );
        assert_eq!(
            strip_managed_google_meet_line(Some(description)).as_deref(),
            Some("Agenda line")
        );
        assert_eq!(
            render_description_with_google_meet(
                Some("Agenda line"),
                Some("https://meet.google.com/abc-mnop-xyz"),
            )
            .as_deref(),
            Some("Agenda line\n\nGoogle Meet: https://meet.google.com/abc-mnop-xyz")
        );
    }
}
