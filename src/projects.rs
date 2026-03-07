//! Project workspace tracking: repos, worktrees, and project-level configuration.

pub mod git;
pub mod store;

pub use store::{
    CreateProjectInput, CreateRepoInput, CreateWorktreeInput, Project, ProjectRepo, ProjectStatus,
    ProjectStore, ProjectWorktree, UpdateProjectInput,
};
