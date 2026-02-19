use super::state::ApiState;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Deserialize)]
pub(super) struct TaskListQuery {
    agent_id: String,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    priority: Option<String>,
    #[serde(default = "default_task_limit")]
    limit: i64,
}

#[derive(Deserialize)]
pub(super) struct TaskGetQuery {
    agent_id: String,
}

#[derive(Deserialize)]
pub(super) struct CreateTaskRequest {
    agent_id: String,
    title: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    priority: Option<String>,
    #[serde(default)]
    subtasks: Vec<crate::tasks::TaskSubtask>,
    #[serde(default)]
    metadata: Option<serde_json::Value>,
    #[serde(default)]
    source_memory_id: Option<String>,
    #[serde(default)]
    created_by: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct UpdateTaskRequest {
    agent_id: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    priority: Option<String>,
    #[serde(default)]
    subtasks: Option<Vec<crate::tasks::TaskSubtask>>,
    #[serde(default)]
    metadata: Option<serde_json::Value>,
    #[serde(default)]
    complete_subtask: Option<usize>,
    #[serde(default)]
    worker_id: Option<String>,
    #[serde(default)]
    approved_by: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct DeleteTaskQuery {
    agent_id: String,
}

#[derive(Serialize)]
pub(super) struct TaskListResponse {
    tasks: Vec<crate::tasks::Task>,
}

#[derive(Serialize)]
pub(super) struct TaskResponse {
    task: crate::tasks::Task,
}

#[derive(Serialize)]
pub(super) struct TaskActionResponse {
    success: bool,
    message: String,
}

fn default_task_limit() -> i64 {
    20
}

pub(super) async fn list_tasks(
    State(state): State<Arc<ApiState>>,
    Query(query): Query<TaskListQuery>,
) -> Result<Json<TaskListResponse>, StatusCode> {
    let stores = state.task_stores.load();
    let store = stores.get(&query.agent_id).ok_or(StatusCode::NOT_FOUND)?;

    let status = query
        .status
        .as_deref()
        .and_then(crate::tasks::TaskStatus::parse);
    let priority = query
        .priority
        .as_deref()
        .and_then(crate::tasks::TaskPriority::parse);

    let tasks = store
        .list(&query.agent_id, status, priority, query.limit)
        .await
        .map_err(|error| {
            tracing::warn!(%error, agent_id = %query.agent_id, "failed to list tasks");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    Ok(Json(TaskListResponse { tasks }))
}

pub(super) async fn get_task(
    State(state): State<Arc<ApiState>>,
    Path(number): Path<i64>,
    Query(query): Query<TaskGetQuery>,
) -> Result<Json<TaskResponse>, StatusCode> {
    let stores = state.task_stores.load();
    let store = stores.get(&query.agent_id).ok_or(StatusCode::NOT_FOUND)?;

    let task = store
        .get_by_number(&query.agent_id, number)
        .await
        .map_err(|error| {
            tracing::warn!(%error, agent_id = %query.agent_id, task_number = number, "failed to get task");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .ok_or(StatusCode::NOT_FOUND)?;

    Ok(Json(TaskResponse { task }))
}

pub(super) async fn create_task(
    State(state): State<Arc<ApiState>>,
    Json(request): Json<CreateTaskRequest>,
) -> Result<Json<TaskResponse>, StatusCode> {
    let stores = state.task_stores.load();
    let store = stores.get(&request.agent_id).ok_or(StatusCode::NOT_FOUND)?;

    let status = request
        .status
        .as_deref()
        .and_then(crate::tasks::TaskStatus::parse)
        .unwrap_or(crate::tasks::TaskStatus::Backlog);
    let priority = request
        .priority
        .as_deref()
        .and_then(crate::tasks::TaskPriority::parse)
        .unwrap_or(crate::tasks::TaskPriority::Medium);

    let task = store
        .create(crate::tasks::CreateTaskInput {
            agent_id: request.agent_id.clone(),
            title: request.title,
            description: request.description,
            status,
            priority,
            subtasks: request.subtasks,
            metadata: request.metadata.unwrap_or_else(|| serde_json::json!({})),
            source_memory_id: request.source_memory_id,
            created_by: request.created_by.unwrap_or_else(|| "human".to_string()),
        })
        .await
        .map_err(|error| {
            tracing::warn!(%error, agent_id = %request.agent_id, "failed to create task");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    Ok(Json(TaskResponse { task }))
}

pub(super) async fn update_task(
    State(state): State<Arc<ApiState>>,
    Path(number): Path<i64>,
    Json(request): Json<UpdateTaskRequest>,
) -> Result<Json<TaskResponse>, StatusCode> {
    let stores = state.task_stores.load();
    let store = stores.get(&request.agent_id).ok_or(StatusCode::NOT_FOUND)?;

    let status = request
        .status
        .as_deref()
        .and_then(crate::tasks::TaskStatus::parse);
    let priority = request
        .priority
        .as_deref()
        .and_then(crate::tasks::TaskPriority::parse);

    let task = store
        .update(
            &request.agent_id,
            number,
            crate::tasks::UpdateTaskInput {
                title: request.title,
                description: request.description,
                status,
                priority,
                subtasks: request.subtasks,
                metadata: request.metadata,
                worker_id: request.worker_id,
                clear_worker_id: false,
                approved_by: request.approved_by,
                complete_subtask: request.complete_subtask,
            },
        )
        .await
        .map_err(|error| {
            tracing::warn!(%error, agent_id = %request.agent_id, task_number = number, "failed to update task");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .ok_or(StatusCode::NOT_FOUND)?;

    Ok(Json(TaskResponse { task }))
}

pub(super) async fn delete_task(
    State(state): State<Arc<ApiState>>,
    Path(number): Path<i64>,
    Query(query): Query<DeleteTaskQuery>,
) -> Result<Json<TaskActionResponse>, StatusCode> {
    let stores = state.task_stores.load();
    let store = stores.get(&query.agent_id).ok_or(StatusCode::NOT_FOUND)?;

    let deleted = store
        .delete(&query.agent_id, number)
        .await
        .map_err(|error| {
            tracing::warn!(%error, agent_id = %query.agent_id, task_number = number, "failed to delete task");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    if !deleted {
        return Err(StatusCode::NOT_FOUND);
    }

    Ok(Json(TaskActionResponse {
        success: true,
        message: format!("Task #{number} deleted"),
    }))
}

pub(super) async fn approve_task(
    State(state): State<Arc<ApiState>>,
    Path(number): Path<i64>,
    Json(request): Json<UpdateTaskRequest>,
) -> Result<Json<TaskResponse>, StatusCode> {
    let stores = state.task_stores.load();
    let store = stores.get(&request.agent_id).ok_or(StatusCode::NOT_FOUND)?;

    let task = store
        .update(
            &request.agent_id,
            number,
            crate::tasks::UpdateTaskInput {
                status: Some(crate::tasks::TaskStatus::Ready),
                approved_by: request.approved_by,
                ..Default::default()
            },
        )
        .await
        .map_err(|error| {
            tracing::warn!(%error, agent_id = %request.agent_id, task_number = number, "failed to approve task");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .ok_or(StatusCode::NOT_FOUND)?;

    Ok(Json(TaskResponse { task }))
}

pub(super) async fn execute_task(
    State(state): State<Arc<ApiState>>,
    Path(number): Path<i64>,
    Json(request): Json<UpdateTaskRequest>,
) -> Result<Json<TaskResponse>, StatusCode> {
    let stores = state.task_stores.load();
    let store = stores.get(&request.agent_id).ok_or(StatusCode::NOT_FOUND)?;

    let task = store
        .update(
            &request.agent_id,
            number,
            crate::tasks::UpdateTaskInput {
                status: Some(crate::tasks::TaskStatus::InProgress),
                ..Default::default()
            },
        )
        .await
        .map_err(|error| {
            tracing::warn!(%error, agent_id = %request.agent_id, task_number = number, "failed to execute task");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .ok_or(StatusCode::NOT_FOUND)?;

    Ok(Json(TaskResponse { task }))
}
