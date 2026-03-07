//! REST API handlers for project, repo, and worktree management.

use super::state::ApiState;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::projects::store::{
    CreateProjectInput, CreateRepoInput, CreateWorktreeInput, ProjectStatus, ProjectWithRelations,
    UpdateProjectInput,
};

// ---------------------------------------------------------------------------
// Query / request types
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub(super) struct AgentQuery {
    agent_id: String,
}

#[derive(Deserialize)]
pub(super) struct ProjectListQuery {
    agent_id: String,
    #[serde(default)]
    status: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct CreateProjectRequest {
    agent_id: String,
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    icon: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    root_path: String,
    #[serde(default)]
    settings: Option<serde_json::Value>,
    /// When true, scan root_path for git repos and register them automatically.
    #[serde(default = "default_true")]
    auto_discover: bool,
}

#[derive(Deserialize)]
pub(super) struct UpdateProjectRequest {
    agent_id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    icon: Option<String>,
    #[serde(default)]
    tags: Option<Vec<String>>,
    #[serde(default)]
    settings: Option<serde_json::Value>,
    #[serde(default)]
    status: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct CreateRepoRequest {
    agent_id: String,
    name: String,
    path: String,
    #[serde(default)]
    remote_url: Option<String>,
    #[serde(default)]
    default_branch: Option<String>,
    #[serde(default)]
    description: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct CreateWorktreeRequest {
    agent_id: String,
    repo_id: String,
    branch: String,
    #[serde(default)]
    worktree_name: Option<String>,
    #[serde(default)]
    start_point: Option<String>,
}

// ---------------------------------------------------------------------------
// Response types
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub(super) struct ProjectListResponse {
    projects: Vec<crate::projects::Project>,
}

#[derive(Serialize)]
pub(super) struct ProjectResponse {
    #[serde(flatten)]
    project: ProjectWithRelations,
}

#[derive(Serialize)]
pub(super) struct RepoResponse {
    repo: crate::projects::ProjectRepo,
}

#[derive(Serialize)]
pub(super) struct WorktreeResponse {
    worktree: crate::projects::ProjectWorktree,
}

#[derive(Serialize)]
pub(super) struct ActionResponse {
    success: bool,
    message: String,
}

#[derive(Serialize)]
pub(super) struct DiskUsageResponse {
    total_bytes: u64,
    entries: Vec<DiskUsageEntry>,
}

#[derive(Serialize)]
pub(super) struct DiskUsageEntry {
    name: String,
    bytes: u64,
    is_dir: bool,
}

fn default_true() -> bool {
    true
}

/// Discover worktrees for all repos in a project and register any new ones.
async fn discover_and_register_worktrees(
    store: &Arc<crate::projects::ProjectStore>,
    project_id: &str,
    root: &std::path::Path,
) {
    let repos = match store.list_repos(project_id).await {
        Ok(repos) => repos,
        Err(error) => {
            tracing::warn!(%error, "failed to list repos for worktree discovery");
            return;
        }
    };

    for repo in &repos {
        let repo_abs_path = root.join(&repo.path);
        if !repo_abs_path.is_dir() {
            continue;
        }
        match crate::projects::git::list_worktrees(&repo_abs_path).await {
            Ok(discovered) => {
                for worktree in discovered {
                    // Compute relative path from project root.
                    let relative_path = worktree
                        .path
                        .strip_prefix(root)
                        .map(|p| p.to_string_lossy().to_string())
                        .unwrap_or_else(|_| {
                            worktree
                                .path
                                .file_name()
                                .map(|n| n.to_string_lossy().to_string())
                                .unwrap_or_default()
                        });

                    let name = worktree
                        .path
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default();

                    // Skip if already registered.
                    if store
                        .get_worktree_by_path(project_id, &relative_path)
                        .await
                        .ok()
                        .flatten()
                        .is_some()
                    {
                        continue;
                    }

                    if let Err(error) = store
                        .create_worktree(CreateWorktreeInput {
                            project_id: project_id.to_string(),
                            repo_id: repo.id.clone(),
                            name,
                            path: relative_path,
                            branch: worktree.branch,
                            created_by: "scan".into(),
                        })
                        .await
                    {
                        tracing::warn!(%error, "failed to register discovered worktree");
                    }
                }
            }
            Err(error) => {
                tracing::warn!(%error, repo = %repo.name, "failed to discover worktrees for repo");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// GET /agents/projects — list projects for an agent.
pub(super) async fn list_projects(
    State(state): State<Arc<ApiState>>,
    Query(query): Query<ProjectListQuery>,
) -> Result<Json<ProjectListResponse>, StatusCode> {
    let stores = state.project_stores.load();
    let store = stores.get(&query.agent_id).ok_or(StatusCode::NOT_FOUND)?;

    let status = query.status.as_deref().and_then(ProjectStatus::parse);

    let projects = store
        .list_projects(&query.agent_id, status)
        .await
        .map_err(|error| {
            tracing::error!(%error, "failed to list projects");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    Ok(Json(ProjectListResponse { projects }))
}

/// POST /agents/projects — create a new project.
pub(super) async fn create_project(
    State(state): State<Arc<ApiState>>,
    Json(request): Json<CreateProjectRequest>,
) -> Result<Json<ProjectResponse>, StatusCode> {
    let stores = state.project_stores.load();
    let store = stores.get(&request.agent_id).ok_or(StatusCode::NOT_FOUND)?;

    let project = store
        .create_project(CreateProjectInput {
            agent_id: request.agent_id.clone(),
            name: request.name,
            description: request.description.unwrap_or_default(),
            icon: request.icon.unwrap_or_default(),
            tags: request.tags,
            root_path: request.root_path.clone(),
            settings: request
                .settings
                .unwrap_or(serde_json::Value::Object(Default::default())),
        })
        .await
        .map_err(|error| {
            tracing::error!(%error, "failed to create project");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    // Auto-discover repos and worktrees if requested.
    if request.auto_discover {
        let root = std::path::PathBuf::from(&request.root_path);
        if root.is_dir() {
            match crate::projects::git::discover_repos(&root).await {
                Ok(discovered) => {
                    for repo in discovered {
                        if let Err(error) = store
                            .create_repo(CreateRepoInput {
                                project_id: project.id.clone(),
                                name: repo.name,
                                path: repo.relative_path,
                                remote_url: repo.remote_url,
                                default_branch: repo.default_branch,
                                description: String::new(),
                            })
                            .await
                        {
                            tracing::warn!(%error, "failed to register discovered repo");
                        }
                    }
                }
                Err(error) => {
                    tracing::warn!(%error, "failed to discover repos in project root");
                }
            }

            // Discover worktrees for all registered repos.
            discover_and_register_worktrees(store, &project.id, &root).await;
        }
    }

    // Reload with relations.
    let full = store
        .get_project_with_relations(&request.agent_id, &project.id)
        .await
        .map_err(|error| {
            tracing::error!(%error, "failed to load project with relations");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .ok_or(StatusCode::NOT_FOUND)?;

    Ok(Json(ProjectResponse { project: full }))
}

/// GET /agents/projects/{id} — get a project with repos and worktrees.
pub(super) async fn get_project(
    State(state): State<Arc<ApiState>>,
    Path(project_id): Path<String>,
    Query(query): Query<AgentQuery>,
) -> Result<Json<ProjectResponse>, StatusCode> {
    let stores = state.project_stores.load();
    let store = stores.get(&query.agent_id).ok_or(StatusCode::NOT_FOUND)?;

    let project = store
        .get_project_with_relations(&query.agent_id, &project_id)
        .await
        .map_err(|error| {
            tracing::error!(%error, "failed to get project");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .ok_or(StatusCode::NOT_FOUND)?;

    Ok(Json(ProjectResponse { project }))
}

/// PUT /agents/projects/{id} — update a project.
pub(super) async fn update_project(
    State(state): State<Arc<ApiState>>,
    Path(project_id): Path<String>,
    Json(request): Json<UpdateProjectRequest>,
) -> Result<Json<ProjectResponse>, StatusCode> {
    let stores = state.project_stores.load();
    let store = stores.get(&request.agent_id).ok_or(StatusCode::NOT_FOUND)?;

    let status = request.status.as_deref().and_then(ProjectStatus::parse);

    store
        .update_project(
            &request.agent_id,
            &project_id,
            UpdateProjectInput {
                name: request.name,
                description: request.description,
                icon: request.icon,
                tags: request.tags,
                settings: request.settings,
                status,
            },
        )
        .await
        .map_err(|error| {
            tracing::error!(%error, "failed to update project");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .ok_or(StatusCode::NOT_FOUND)?;

    // Reload with relations.
    let full = store
        .get_project_with_relations(&request.agent_id, &project_id)
        .await
        .map_err(|error| {
            tracing::error!(%error, "failed to reload project");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .ok_or(StatusCode::NOT_FOUND)?;

    Ok(Json(ProjectResponse { project: full }))
}

/// DELETE /agents/projects/{id} — delete a project (DB records only).
pub(super) async fn delete_project(
    State(state): State<Arc<ApiState>>,
    Path(project_id): Path<String>,
    Query(query): Query<AgentQuery>,
) -> Result<Json<ActionResponse>, StatusCode> {
    let stores = state.project_stores.load();
    let store = stores.get(&query.agent_id).ok_or(StatusCode::NOT_FOUND)?;

    let deleted = store
        .delete_project(&query.agent_id, &project_id)
        .await
        .map_err(|error| {
            tracing::error!(%error, "failed to delete project");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    if !deleted {
        return Err(StatusCode::NOT_FOUND);
    }

    Ok(Json(ActionResponse {
        success: true,
        message: "project deleted".into(),
    }))
}

/// POST /agents/projects/{id}/scan — re-scan project root for repos and worktrees.
pub(super) async fn scan_project(
    State(state): State<Arc<ApiState>>,
    Path(project_id): Path<String>,
    Query(query): Query<AgentQuery>,
) -> Result<Json<ProjectResponse>, StatusCode> {
    let stores = state.project_stores.load();
    let store = stores.get(&query.agent_id).ok_or(StatusCode::NOT_FOUND)?;

    let project = store
        .get_project(&query.agent_id, &project_id)
        .await
        .map_err(|error| {
            tracing::error!(%error, "failed to get project for scan");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .ok_or(StatusCode::NOT_FOUND)?;

    let root = std::path::PathBuf::from(&project.root_path);
    if !root.is_dir() {
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }

    // Discover repos.
    match crate::projects::git::discover_repos(&root).await {
        Ok(discovered) => {
            for repo in discovered {
                // Skip if already registered.
                if store
                    .get_repo_by_path(&project_id, &repo.relative_path)
                    .await
                    .ok()
                    .flatten()
                    .is_some()
                {
                    continue;
                }
                if let Err(error) = store
                    .create_repo(CreateRepoInput {
                        project_id: project_id.clone(),
                        name: repo.name,
                        path: repo.relative_path,
                        remote_url: repo.remote_url,
                        default_branch: repo.default_branch,
                        description: String::new(),
                    })
                    .await
                {
                    tracing::warn!(%error, "failed to register discovered repo during scan");
                }
            }
        }
        Err(error) => {
            tracing::warn!(%error, "failed to discover repos during scan");
        }
    }

    // Discover worktrees for each known repo.
    discover_and_register_worktrees(store, &project_id, &root).await;

    // Reload with relations.
    let full = store
        .get_project_with_relations(&query.agent_id, &project_id)
        .await
        .map_err(|error| {
            tracing::error!(%error, "failed to reload project after scan");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .ok_or(StatusCode::NOT_FOUND)?;

    Ok(Json(ProjectResponse { project: full }))
}

/// POST /agents/projects/{id}/repos — add a repo to a project.
pub(super) async fn create_repo(
    State(state): State<Arc<ApiState>>,
    Path(project_id): Path<String>,
    Json(request): Json<CreateRepoRequest>,
) -> Result<Json<RepoResponse>, StatusCode> {
    let stores = state.project_stores.load();
    let store = stores.get(&request.agent_id).ok_or(StatusCode::NOT_FOUND)?;

    // Verify project exists.
    store
        .get_project(&request.agent_id, &project_id)
        .await
        .map_err(|error| {
            tracing::error!(%error, "failed to verify project");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .ok_or(StatusCode::NOT_FOUND)?;

    let repo = store
        .create_repo(CreateRepoInput {
            project_id,
            name: request.name,
            path: request.path,
            remote_url: request.remote_url.unwrap_or_default(),
            default_branch: request.default_branch.unwrap_or_else(|| "main".into()),
            description: request.description.unwrap_or_default(),
        })
        .await
        .map_err(|error| {
            tracing::error!(%error, "failed to create repo");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    Ok(Json(RepoResponse { repo }))
}

/// DELETE /agents/projects/{project_id}/repos/{repo_id} — remove a repo.
pub(super) async fn delete_repo(
    State(state): State<Arc<ApiState>>,
    Path((_, repo_id)): Path<(String, String)>,
    Query(query): Query<AgentQuery>,
) -> Result<Json<ActionResponse>, StatusCode> {
    let stores = state.project_stores.load();
    let store = stores.get(&query.agent_id).ok_or(StatusCode::NOT_FOUND)?;

    let deleted = store.delete_repo(&repo_id).await.map_err(|error| {
        tracing::error!(%error, "failed to delete repo");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    if !deleted {
        return Err(StatusCode::NOT_FOUND);
    }

    Ok(Json(ActionResponse {
        success: true,
        message: "repo removed".into(),
    }))
}

/// POST /agents/projects/{id}/worktrees — create a worktree.
pub(super) async fn create_worktree(
    State(state): State<Arc<ApiState>>,
    Path(project_id): Path<String>,
    Json(request): Json<CreateWorktreeRequest>,
) -> Result<Json<WorktreeResponse>, StatusCode> {
    let stores = state.project_stores.load();
    let store = stores.get(&request.agent_id).ok_or(StatusCode::NOT_FOUND)?;

    // Look up the project and repo.
    let project = store
        .get_project(&request.agent_id, &project_id)
        .await
        .map_err(|error| {
            tracing::error!(%error, "failed to get project");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .ok_or(StatusCode::NOT_FOUND)?;

    let repo = store
        .get_repo(&request.repo_id)
        .await
        .map_err(|error| {
            tracing::error!(%error, "failed to get repo");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .ok_or(StatusCode::NOT_FOUND)?;

    let root = std::path::PathBuf::from(&project.root_path);
    let repo_abs_path = root.join(&repo.path);

    // Determine worktree name and path.
    let worktree_name = request
        .worktree_name
        .unwrap_or_else(|| request.branch.replace('/', "-"));
    let worktree_abs_path = root.join(&worktree_name);

    // Create the git worktree.
    crate::projects::git::create_worktree(
        &repo_abs_path,
        &worktree_abs_path,
        &request.branch,
        request.start_point.as_deref(),
    )
    .await
    .map_err(|error| {
        tracing::error!(%error, "failed to create git worktree");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    // Register in the database.
    let worktree = store
        .create_worktree(CreateWorktreeInput {
            project_id,
            repo_id: request.repo_id,
            name: worktree_name.clone(),
            path: worktree_name,
            branch: request.branch,
            created_by: "user".into(),
        })
        .await
        .map_err(|error| {
            tracing::error!(%error, "failed to register worktree");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    Ok(Json(WorktreeResponse { worktree }))
}

/// DELETE /agents/projects/{project_id}/worktrees/{worktree_id} — remove a worktree.
pub(super) async fn delete_worktree(
    State(state): State<Arc<ApiState>>,
    Path((project_id, worktree_id)): Path<(String, String)>,
    Query(query): Query<AgentQuery>,
) -> Result<Json<ActionResponse>, StatusCode> {
    let stores = state.project_stores.load();
    let store = stores.get(&query.agent_id).ok_or(StatusCode::NOT_FOUND)?;

    // Look up worktree and project for the git removal.
    let worktree = store
        .get_worktree(&worktree_id)
        .await
        .map_err(|error| {
            tracing::error!(%error, "failed to get worktree");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .ok_or(StatusCode::NOT_FOUND)?;

    let project = store
        .get_project(&query.agent_id, &project_id)
        .await
        .map_err(|error| {
            tracing::error!(%error, "failed to get project");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .ok_or(StatusCode::NOT_FOUND)?;

    let repo = store
        .get_repo(&worktree.repo_id)
        .await
        .map_err(|error| {
            tracing::error!(%error, "failed to get repo");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .ok_or(StatusCode::NOT_FOUND)?;

    // Run `git worktree remove`.
    let root = std::path::PathBuf::from(&project.root_path);
    let repo_abs_path = root.join(&repo.path);
    let worktree_abs_path = root.join(&worktree.path);

    if let Err(error) =
        crate::projects::git::remove_worktree(&repo_abs_path, &worktree_abs_path).await
    {
        tracing::warn!(%error, "git worktree remove failed, deleting DB record anyway");
    }

    // Delete from database.
    store.delete_worktree(&worktree_id).await.map_err(|error| {
        tracing::error!(%error, "failed to delete worktree record");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    Ok(Json(ActionResponse {
        success: true,
        message: "worktree removed".into(),
    }))
}

/// GET /agents/projects/{id}/disk-usage — calculate disk usage for a project.
pub(super) async fn disk_usage(
    State(state): State<Arc<ApiState>>,
    Path(project_id): Path<String>,
    Query(query): Query<AgentQuery>,
) -> Result<Json<DiskUsageResponse>, StatusCode> {
    let stores = state.project_stores.load();
    let store = stores.get(&query.agent_id).ok_or(StatusCode::NOT_FOUND)?;

    let project = store
        .get_project(&query.agent_id, &project_id)
        .await
        .map_err(|error| {
            tracing::error!(%error, "failed to get project for disk usage");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .ok_or(StatusCode::NOT_FOUND)?;

    let root = std::path::PathBuf::from(&project.root_path);
    if !root.is_dir() {
        return Ok(Json(DiskUsageResponse {
            total_bytes: 0,
            entries: Vec::new(),
        }));
    }

    let mut entries = Vec::new();
    let mut total_bytes: u64 = 0;

    let mut dir_entries = tokio::fs::read_dir(&root).await.map_err(|error| {
        tracing::error!(%error, "failed to read project root for disk usage");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    while let Ok(Some(entry)) = dir_entries.next_entry().await {
        let name = entry.file_name().to_string_lossy().to_string();
        let metadata = match entry.metadata().await {
            Ok(m) => m,
            Err(_) => continue,
        };
        let is_dir = metadata.is_dir();
        let bytes = if is_dir {
            // For directories, approximate with a quick du.
            dir_size(&entry.path()).await
        } else {
            metadata.len()
        };
        total_bytes += bytes;
        entries.push(DiskUsageEntry {
            name,
            bytes,
            is_dir,
        });
    }

    entries.sort_by(|a, b| b.bytes.cmp(&a.bytes));

    Ok(Json(DiskUsageResponse {
        total_bytes,
        entries,
    }))
}

/// Recursively calculate directory size. Best-effort — skips entries it can't read.
async fn dir_size(path: &std::path::Path) -> u64 {
    let mut total: u64 = 0;
    let mut stack = vec![path.to_path_buf()];

    while let Some(current) = stack.pop() {
        let mut entries = match tokio::fs::read_dir(&current).await {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let metadata = match entry.metadata().await {
                Ok(m) => m,
                Err(_) => continue,
            };
            if metadata.is_dir() {
                stack.push(entry.path());
            } else {
                total += metadata.len();
            }
        }
    }

    total
}
