//! Calendar API handlers for sync, range queries, proposals, and ICS export.

use super::state::ApiState;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::IntoResponse;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Deserialize, utoipa::ToSchema, utoipa::IntoParams)]
pub(super) struct CalendarAgentQuery {
    pub agent_id: String,
}

#[derive(Debug, Deserialize, utoipa::ToSchema, utoipa::IntoParams)]
pub(super) struct CalendarOccurrencesQuery {
    pub agent_id: String,
    pub start_at: String,
    pub end_at: String,
}

#[derive(Debug, Deserialize, utoipa::ToSchema, utoipa::IntoParams)]
pub(super) struct CalendarEventQuery {
    pub agent_id: String,
}

#[derive(Debug, Deserialize, utoipa::ToSchema, utoipa::IntoParams)]
pub(super) struct CalendarFreeTimeQuery {
    pub agent_id: String,
    pub start_at: String,
    pub end_at: String,
    pub duration_minutes: i64,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub(super) struct CalendarSyncRequest {
    pub agent_id: String,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub(super) struct CalendarCreateProposalRequest {
    pub agent_id: String,
    pub draft: crate::calendar::CalendarEventDraft,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub(super) struct CalendarUpdateProposalRequest {
    pub agent_id: String,
    pub event_id: String,
    pub draft: crate::calendar::CalendarEventDraft,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub(super) struct CalendarDeleteProposalRequest {
    pub agent_id: String,
    pub event_id: String,
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub(super) struct CalendarApplyProposalRequest {
    pub agent_id: String,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(super) struct CalendarOccurrencesResponse {
    pub occurrences: Vec<crate::calendar::CalendarOccurrence>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(super) struct CalendarEventResponse {
    pub event: crate::calendar::CalendarEvent,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(super) struct CalendarFreeTimeResponse {
    pub slots: Vec<crate::calendar::CalendarAvailabilitySlot>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(super) struct CalendarSyncResponse {
    pub summary: crate::calendar::CalendarSyncSummary,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub(super) struct CalendarProposalResponse {
    pub proposal: crate::calendar::CalendarChangeProposal,
}

fn get_calendar_service(
    state: &ApiState,
    agent_id: &str,
) -> Result<Arc<crate::calendar::CalendarService>, StatusCode> {
    let services = state.calendar_services.load();
    services.get(agent_id).cloned().ok_or(StatusCode::NOT_FOUND)
}

fn get_runtime_config(
    state: &ApiState,
    agent_id: &str,
) -> Result<Arc<crate::config::RuntimeConfig>, StatusCode> {
    let configs = state.runtime_configs.load();
    configs.get(agent_id).cloned().ok_or(StatusCode::NOT_FOUND)
}

fn parse_utc_datetime(value: &str, field: &str) -> Result<DateTime<Utc>, StatusCode> {
    DateTime::parse_from_rfc3339(value)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|error| {
            tracing::warn!(%error, field, value, "invalid calendar datetime");
            StatusCode::BAD_REQUEST
        })
}

fn calendar_error_status(error: &crate::Error) -> StatusCode {
    let message = error.to_string().to_ascii_lowercase();
    if message.contains("not found") {
        StatusCode::NOT_FOUND
    } else if message.contains("read-only")
        || message.contains("not pending")
        || message.contains("invalid")
        || message.contains("required")
        || message.contains("not configured")
        || message.contains("not supported")
    {
        StatusCode::BAD_REQUEST
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    }
}

fn build_public_base_url(headers: &HeaderMap) -> Option<String> {
    let host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())?;
    let scheme = headers
        .get("x-forwarded-proto")
        .or_else(|| headers.get("x-forwarded-protocol"))
        .and_then(|value| value.to_str().ok())
        .unwrap_or("http");
    Some(format!("{}://{}", scheme.trim_end_matches("://"), host))
}

fn build_ics_export_url(
    runtime_config: &crate::config::RuntimeConfig,
    public_base_url: Option<String>,
    agent_id: &str,
) -> Option<String> {
    let base_url = public_base_url?;
    let calendar = runtime_config.calendar.load();
    let token = calendar.ics_export_token.as_deref()?;
    calendar.selected_calendar_href.as_ref()?;
    Some(format!(
        "{}/calendar/ics/{}/{}.ics",
        base_url.trim_end_matches('/'),
        urlencoding::encode(agent_id),
        urlencoding::encode(token),
    ))
}

/// `GET /agents/calendar/overview` — current calendar configuration and sync state.
#[utoipa::path(
    get,
    path = "/agents/calendar/overview",
    params(
        ("agent_id" = String, Query, description = "Agent ID"),
    ),
    responses(
        (status = 200, body = crate::calendar::CalendarOverview),
        (status = 404, description = "Agent not found"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "calendar",
)]
pub(super) async fn calendar_overview(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Query(query): Query<CalendarAgentQuery>,
) -> Result<Json<crate::calendar::CalendarOverview>, StatusCode> {
    let service = get_calendar_service(&state, &query.agent_id)?;
    let runtime_config = get_runtime_config(&state, &query.agent_id)?;
    let ics_export_url = build_ics_export_url(
        &runtime_config,
        build_public_base_url(&headers),
        &query.agent_id,
    );

    service
        .overview(ics_export_url)
        .await
        .map(Json)
        .map_err(|error| {
            let status = calendar_error_status(&error);
            tracing::warn!(%error, agent_id = %query.agent_id, "failed to fetch calendar overview");
            status
        })
}

/// `GET /agents/calendar/events` — list occurrences in a time range.
#[utoipa::path(
    get,
    path = "/agents/calendar/events",
    params(CalendarOccurrencesQuery),
    responses(
        (status = 200, body = CalendarOccurrencesResponse),
        (status = 400, description = "Invalid datetime range"),
        (status = 404, description = "Agent not found"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "calendar",
)]
pub(super) async fn calendar_events(
    State(state): State<Arc<ApiState>>,
    Query(query): Query<CalendarOccurrencesQuery>,
) -> Result<Json<CalendarOccurrencesResponse>, StatusCode> {
    let service = get_calendar_service(&state, &query.agent_id)?;
    let start_at = parse_utc_datetime(&query.start_at, "start_at")?;
    let end_at = parse_utc_datetime(&query.end_at, "end_at")?;
    if end_at <= start_at {
        return Err(StatusCode::BAD_REQUEST);
    }

    service
        .list_occurrences(start_at, end_at)
        .await
        .map(|occurrences| Json(CalendarOccurrencesResponse { occurrences }))
        .map_err(|error| {
            let status = calendar_error_status(&error);
            tracing::warn!(%error, agent_id = %query.agent_id, "failed to list calendar occurrences");
            status
        })
}

/// `GET /agents/calendar/events/{event_id}` — fetch one mirrored calendar event.
#[utoipa::path(
    get,
    path = "/agents/calendar/events/{event_id}",
    params(
        ("event_id" = String, Path, description = "Calendar event ID"),
        ("agent_id" = String, Query, description = "Agent ID"),
    ),
    responses(
        (status = 200, body = CalendarEventResponse),
        (status = 404, description = "Event or agent not found"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "calendar",
)]
pub(super) async fn calendar_event(
    State(state): State<Arc<ApiState>>,
    Path(event_id): Path<String>,
    Query(query): Query<CalendarEventQuery>,
) -> Result<Json<CalendarEventResponse>, StatusCode> {
    let service = get_calendar_service(&state, &query.agent_id)?;
    let event = service.get_event(&event_id).await.map_err(|error| {
        let status = calendar_error_status(&error);
        tracing::warn!(%error, agent_id = %query.agent_id, event_id, "failed to fetch calendar event");
        status
    })?;

    event
        .map(|event| Json(CalendarEventResponse { event }))
        .ok_or(StatusCode::NOT_FOUND)
}

/// `GET /agents/calendar/free-time` — find available slots within a range.
#[utoipa::path(
    get,
    path = "/agents/calendar/free-time",
    params(CalendarFreeTimeQuery),
    responses(
        (status = 200, body = CalendarFreeTimeResponse),
        (status = 400, description = "Invalid datetime range"),
        (status = 404, description = "Agent not found"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "calendar",
)]
pub(super) async fn calendar_free_time(
    State(state): State<Arc<ApiState>>,
    Query(query): Query<CalendarFreeTimeQuery>,
) -> Result<Json<CalendarFreeTimeResponse>, StatusCode> {
    let service = get_calendar_service(&state, &query.agent_id)?;
    let start_at = parse_utc_datetime(&query.start_at, "start_at")?;
    let end_at = parse_utc_datetime(&query.end_at, "end_at")?;
    if end_at <= start_at || query.duration_minutes <= 0 {
        return Err(StatusCode::BAD_REQUEST);
    }

    service
        .find_free_time(start_at, end_at, query.duration_minutes)
        .await
        .map(|slots| Json(CalendarFreeTimeResponse { slots }))
        .map_err(|error| {
            let status = calendar_error_status(&error);
            tracing::warn!(%error, agent_id = %query.agent_id, "failed to compute calendar free time");
            status
        })
}

/// `POST /agents/calendar/sync` — trigger a sync immediately.
#[utoipa::path(
    post,
    path = "/agents/calendar/sync",
    request_body = CalendarSyncRequest,
    responses(
        (status = 200, body = CalendarSyncResponse),
        (status = 404, description = "Agent not found"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "calendar",
)]
pub(super) async fn calendar_sync(
    State(state): State<Arc<ApiState>>,
    Json(request): Json<CalendarSyncRequest>,
) -> Result<Json<CalendarSyncResponse>, StatusCode> {
    let service = get_calendar_service(&state, &request.agent_id)?;
    service
        .sync_now()
        .await
        .map(|summary| Json(CalendarSyncResponse { summary }))
        .map_err(|error| {
            let status = calendar_error_status(&error);
            tracing::warn!(%error, agent_id = %request.agent_id, "failed to sync calendar");
            status
        })
}

/// `POST /agents/calendar/proposals/create` — propose a new calendar event.
#[utoipa::path(
    post,
    path = "/agents/calendar/proposals/create",
    request_body = CalendarCreateProposalRequest,
    responses(
        (status = 200, body = CalendarProposalResponse),
        (status = 404, description = "Agent not found"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "calendar",
)]
pub(super) async fn calendar_propose_create(
    State(state): State<Arc<ApiState>>,
    Json(request): Json<CalendarCreateProposalRequest>,
) -> Result<Json<CalendarProposalResponse>, StatusCode> {
    let service = get_calendar_service(&state, &request.agent_id)?;
    service
        .propose_create(request.draft)
        .await
        .map(|proposal| Json(CalendarProposalResponse { proposal }))
        .map_err(|error| {
            let status = calendar_error_status(&error);
            tracing::warn!(%error, agent_id = %request.agent_id, "failed to create calendar proposal");
            status
        })
}

/// `POST /agents/calendar/proposals/update` — propose an event update.
#[utoipa::path(
    post,
    path = "/agents/calendar/proposals/update",
    request_body = CalendarUpdateProposalRequest,
    responses(
        (status = 200, body = CalendarProposalResponse),
        (status = 404, description = "Agent or event not found"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "calendar",
)]
pub(super) async fn calendar_propose_update(
    State(state): State<Arc<ApiState>>,
    Json(request): Json<CalendarUpdateProposalRequest>,
) -> Result<Json<CalendarProposalResponse>, StatusCode> {
    let service = get_calendar_service(&state, &request.agent_id)?;
    service
        .propose_update(&request.event_id, request.draft)
        .await
        .map(|proposal| Json(CalendarProposalResponse { proposal }))
        .map_err(|error| {
            let status = calendar_error_status(&error);
            tracing::warn!(%error, agent_id = %request.agent_id, event_id = %request.event_id, "failed to update calendar proposal");
            status
        })
}

/// `POST /agents/calendar/proposals/delete` — propose an event deletion.
#[utoipa::path(
    post,
    path = "/agents/calendar/proposals/delete",
    request_body = CalendarDeleteProposalRequest,
    responses(
        (status = 200, body = CalendarProposalResponse),
        (status = 404, description = "Agent or event not found"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "calendar",
)]
pub(super) async fn calendar_propose_delete(
    State(state): State<Arc<ApiState>>,
    Json(request): Json<CalendarDeleteProposalRequest>,
) -> Result<Json<CalendarProposalResponse>, StatusCode> {
    let service = get_calendar_service(&state, &request.agent_id)?;
    service
        .propose_delete(&request.event_id)
        .await
        .map(|proposal| Json(CalendarProposalResponse { proposal }))
        .map_err(|error| {
            let status = calendar_error_status(&error);
            tracing::warn!(%error, agent_id = %request.agent_id, event_id = %request.event_id, "failed to delete calendar proposal");
            status
        })
}

/// `POST /agents/calendar/proposals/{proposal_id}/apply` — apply a pending proposal.
#[utoipa::path(
    post,
    path = "/agents/calendar/proposals/{proposal_id}/apply",
    params(
        ("proposal_id" = String, Path, description = "Calendar proposal ID"),
    ),
    request_body = CalendarApplyProposalRequest,
    responses(
        (status = 200, body = CalendarProposalResponse),
        (status = 404, description = "Agent or proposal not found"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "calendar",
)]
pub(super) async fn calendar_apply_proposal(
    State(state): State<Arc<ApiState>>,
    Path(proposal_id): Path<String>,
    Json(request): Json<CalendarApplyProposalRequest>,
) -> Result<Json<CalendarProposalResponse>, StatusCode> {
    let service = get_calendar_service(&state, &request.agent_id)?;
    service
        .apply_proposal(&proposal_id)
        .await
        .map(|proposal| Json(CalendarProposalResponse { proposal }))
        .map_err(|error| {
            let status = calendar_error_status(&error);
            tracing::warn!(%error, agent_id = %request.agent_id, proposal_id, "failed to apply calendar proposal");
            status
        })
}

pub(super) async fn export_calendar_ics(
    State(state): State<Arc<ApiState>>,
    Path((agent_id, token)): Path<(String, String)>,
) -> Result<impl IntoResponse, StatusCode> {
    let service = get_calendar_service(&state, &agent_id)?;
    let body = service.export_ics(&token).await.map_err(|error| {
        let status = calendar_error_status(&error);
        tracing::warn!(%error, agent_id = %agent_id, "failed to export calendar ICS");
        status
    })?;

    Ok((
        [(header::CONTENT_TYPE, "text/calendar; charset=utf-8")],
        body,
    ))
}
