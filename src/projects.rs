//! Project workspace tracking: repos, worktrees, and project-level configuration.

pub mod git;
pub mod store;

pub use store::{
    CreateProjectInput, CreateRepoInput, CreateWorktreeInput, Project, ProjectRepo, ProjectStatus,
    ProjectStore, ProjectWorktree, UpdateProjectInput,
};

/// Refresh the sandbox allowlist with all active project root paths.
///
/// Queries all active projects for the agent and injects their root paths
/// into the sandbox config. Takes effect immediately for subsequent subprocess
/// calls. Should be called after project create/delete/scan.
pub async fn refresh_sandbox_project_paths(
    project_store: &ProjectStore,
    agent_id: &str,
    sandbox: &crate::sandbox::Sandbox,
) {
    let projects = match project_store
        .list_projects(agent_id, Some(ProjectStatus::Active))
        .await
    {
        Ok(projects) => projects,
        Err(error) => {
            tracing::warn!(%error, "failed to list projects for sandbox refresh");
            return;
        }
    };

    let paths: Vec<std::path::PathBuf> = projects
        .iter()
        .map(|project| std::path::PathBuf::from(&project.root_path))
        .collect();

    sandbox.refresh_project_paths(paths);
}
