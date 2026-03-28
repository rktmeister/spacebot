//! Calendar domain types shared across sync, API, and tool layers.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Discovery and sync state for the configured remote calendar source.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct CalendarSourceState {
    pub source_id: String,
    pub provider_kind: String,
    pub base_url: Option<String>,
    pub principal_url: Option<String>,
    pub home_set_url: Option<String>,
    pub auth_kind: String,
    pub last_discovery_at: Option<String>,
    pub last_sync_at: Option<String>,
    pub last_successful_sync_at: Option<String>,
    pub last_error: Option<String>,
    pub sync_status: Option<String>,
}

/// Calendar collection discovered from the remote provider.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct CalendarCollection {
    pub href: String,
    pub display_name: Option<String>,
    pub description: Option<String>,
    pub color: Option<String>,
    pub timezone: Option<String>,
    pub ctag: Option<String>,
    pub sync_token: Option<String>,
    pub is_selected: bool,
    pub discovered_at: String,
    pub last_synced_at: Option<String>,
}

/// Persisted attendee metadata for a calendar event.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct CalendarAttendee {
    pub id: String,
    pub event_id: String,
    pub email: Option<String>,
    pub common_name: Option<String>,
    pub role: Option<String>,
    pub partstat: Option<String>,
    pub rsvp: bool,
    pub is_organizer: bool,
}

/// Stored VEVENT component derived from a synced ICS resource.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct CalendarEvent {
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
    pub start_at_utc: String,
    pub end_at_utc: String,
    pub timezone: Option<String>,
    pub all_day: bool,
    pub recurrence_rule: Option<String>,
    pub recurrence_exdates_json: Option<String>,
    pub sequence: i64,
    pub transparency: Option<String>,
    pub etag: Option<String>,
    pub raw_ics: String,
    pub deleted: bool,
    pub attendees: Vec<CalendarAttendee>,
}

impl CalendarEvent {
    pub fn is_recurring(&self) -> bool {
        self.recurrence_rule.is_some()
    }

    pub fn is_override(&self) -> bool {
        self.recurrence_id_utc.is_some()
    }
}

/// Range-query occurrence returned to the dashboard and tools.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct CalendarOccurrence {
    pub occurrence_id: String,
    pub event_id: String,
    pub series_event_id: String,
    pub remote_uid: String,
    pub calendar_href: String,
    pub remote_href: String,
    pub summary: Option<String>,
    pub description: Option<String>,
    pub location: Option<String>,
    pub status: Option<String>,
    pub organizer_name: Option<String>,
    pub organizer_email: Option<String>,
    pub start_at: String,
    pub end_at: String,
    pub timezone: Option<String>,
    pub all_day: bool,
    pub recurring: bool,
    pub override_instance: bool,
    pub can_edit_series: bool,
    pub attendee_count: usize,
}

/// Sync summary returned from discovery and refresh operations.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct CalendarSyncSummary {
    pub discovered_calendar_count: usize,
    pub selected_calendar_href: Option<String>,
    pub synced_resource_count: usize,
    pub created_event_count: usize,
    pub updated_event_count: usize,
    pub deleted_event_count: usize,
    pub sync_started_at: String,
    pub sync_finished_at: String,
    pub mode: String,
}

/// Operator-facing overview for the dashboard and API.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct CalendarOverview {
    pub configured: bool,
    pub enabled: bool,
    pub read_only: bool,
    pub provider_kind: String,
    pub auth_kind: String,
    pub selected_calendar_href: Option<String>,
    pub ics_export_url: Option<String>,
    pub source: Option<CalendarSourceState>,
    pub calendars: Vec<CalendarCollection>,
}

/// Editable event draft for proposals and apply flows.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema, JsonSchema)]
pub struct CalendarEventDraft {
    pub summary: String,
    pub description: Option<String>,
    pub location: Option<String>,
    pub start_at: String,
    pub end_at: String,
    pub timezone: Option<String>,
    pub all_day: bool,
    pub recurrence_rule: Option<String>,
    pub attendees: Vec<CalendarAttendeeInput>,
}

/// Attendee input accepted from tools and UI.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema, JsonSchema)]
pub struct CalendarAttendeeInput {
    pub email: String,
    pub common_name: Option<String>,
    pub role: Option<String>,
    pub partstat: Option<String>,
    #[serde(default)]
    pub rsvp: bool,
}

/// Stored proposal action.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum CalendarProposalAction {
    Create,
    Update,
    Delete,
}

impl CalendarProposalAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Update => "update",
            Self::Delete => "delete",
        }
    }
}

impl std::fmt::Display for CalendarProposalAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Proposal lifecycle state.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum CalendarProposalStatus {
    Pending,
    Applied,
    Failed,
    Cancelled,
    Expired,
}

impl CalendarProposalStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Applied => "applied",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Expired => "expired",
        }
    }
}

impl std::fmt::Display for CalendarProposalStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Persisted proposal shared between chat tools and dashboard UI.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct CalendarChangeProposal {
    pub id: String,
    pub action: CalendarProposalAction,
    pub status: CalendarProposalStatus,
    pub event_id: Option<String>,
    pub summary: String,
    pub diff: String,
    pub basis_etag: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub applied_at: Option<String>,
    pub error: Option<String>,
    pub draft: CalendarEventDraft,
}

/// Busy/available slot returned by the scheduling helper.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct CalendarAvailabilitySlot {
    pub start_at: String,
    pub end_at: String,
}

/// Internal materialized resource used during sync.
#[derive(Debug, Clone)]
pub struct SyncedCalendarResource {
    pub remote_href: String,
    pub etag: Option<String>,
    pub raw_ics: String,
    pub events: Vec<SyncedCalendarEvent>,
}

/// Internal VEVENT derived from a synced ICS resource.
#[derive(Debug, Clone)]
pub struct SyncedCalendarEvent {
    pub remote_uid: String,
    pub recurrence_id_utc: Option<String>,
    pub summary: Option<String>,
    pub description: Option<String>,
    pub location: Option<String>,
    pub status: Option<String>,
    pub organizer_name: Option<String>,
    pub organizer_email: Option<String>,
    pub start_at_utc: String,
    pub end_at_utc: String,
    pub timezone: Option<String>,
    pub all_day: bool,
    pub recurrence_rule: Option<String>,
    pub recurrence_exdates: Vec<String>,
    pub sequence: i64,
    pub transparency: Option<String>,
    pub attendees: Vec<CalendarAttendeeInput>,
}
