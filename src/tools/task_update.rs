//! Task update tool for branch and worker processes.

use crate::tasks::{TaskPriority, TaskStatus, TaskStore, TaskSubtask, UpdateTaskInput};
use crate::{AgentId, WorkerId};
use rig::completion::ToolDefinition;
use rig::tool::Tool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Clone)]
pub enum TaskUpdateScope {
    Branch,
    Worker(WorkerId),
}

#[derive(Debug, Clone)]
pub struct TaskUpdateTool {
    task_store: Arc<TaskStore>,
    agent_id: AgentId,
    scope: TaskUpdateScope,
}

impl TaskUpdateTool {
    pub fn for_branch(task_store: Arc<TaskStore>, agent_id: AgentId) -> Self {
        Self {
            task_store,
            agent_id,
            scope: TaskUpdateScope::Branch,
        }
    }

    pub fn for_worker(task_store: Arc<TaskStore>, agent_id: AgentId, worker_id: WorkerId) -> Self {
        Self {
            task_store,
            agent_id,
            scope: TaskUpdateScope::Worker(worker_id),
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("task_update failed: {0}")]
pub struct TaskUpdateError(String);

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TaskUpdateArgs {
    pub task_number: i32,
    pub title: Option<String>,
    pub description: Option<String>,
    pub status: Option<String>,
    pub priority: Option<String>,
    pub subtasks: Option<Vec<TaskSubtask>>,
    pub metadata: Option<serde_json::Value>,
    pub complete_subtask: Option<i32>,
    pub worker_id: Option<String>,
    pub approved_by: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct TaskUpdateOutput {
    pub success: bool,
    pub task_number: i64,
    pub status: String,
    pub message: String,
}

impl Tool for TaskUpdateTool {
    const NAME: &'static str = "task_update";

    type Error = TaskUpdateError;
    type Args = TaskUpdateArgs;
    type Output = TaskUpdateOutput;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: crate::prompts::text::get("tools/task_update").to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "task_number": { "type": "integer", "description": "Task number reference (#N)" },
                    "title": { "type": "string", "description": "Optional new title" },
                    "description": { "type": "string", "description": "Optional new description" },
                    "status": {
                        "type": "string",
                        "enum": crate::tasks::TaskStatus::ALL.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
                        "description": "Optional new status"
                    },
                    "priority": {
                        "type": "string",
                        "enum": crate::tasks::TaskPriority::ALL.iter().map(|p| p.to_string()).collect::<Vec<_>>(),
                        "description": "Optional new priority"
                    },
                    "subtasks": {
                        "type": "array",
                        "description": "Optional full replacement of subtask list",
                        "items": {
                            "type": "object",
                            "properties": {
                                "title": { "type": "string" },
                                "completed": { "type": "boolean" }
                            },
                            "required": ["title", "completed"]
                        }
                    },
                    "metadata": { "type": "object", "description": "Metadata object merged with current metadata" },
                    "complete_subtask": { "type": "integer", "description": "Subtask index to mark complete" },
                    "worker_id": { "type": "string", "description": "Optional worker ID to bind to this task" },
                    "approved_by": { "type": "string", "description": "Optional approver identifier" }
                },
                "required": ["task_number"]
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let task_number = i64::from(args.task_number);

        if let TaskUpdateScope::Worker(worker_id) = self.scope {
            let current = self
                .task_store
                .get_by_worker_id(&worker_id.to_string())
                .await
                .map_err(|error| TaskUpdateError(format!("{error}")))?;

            let Some(task) = current else {
                return Err(TaskUpdateError("worker is not assigned to a task".to_string()));
            };

            if task.task_number != task_number {
                return Err(TaskUpdateError(format!(
                    "worker {} can only update task #{}",
                    worker_id, task.task_number
                )));
            }
        }

        let status = args.status.as_deref().and_then(TaskStatus::parse);
        let priority = args.priority.as_deref().and_then(TaskPriority::parse);
        let complete_subtask = args
            .complete_subtask
            .and_then(|value| usize::try_from(value).ok());

        let updated = self
            .task_store
            .update(
                &self.agent_id,
                task_number,
                UpdateTaskInput {
                    title: args.title,
                    description: args.description,
                    status,
                    priority,
                    subtasks: args.subtasks,
                    metadata: args.metadata,
                    worker_id: args.worker_id,
                    clear_worker_id: false,
                    approved_by: args.approved_by,
                    complete_subtask,
                },
            )
            .await
            .map_err(|error| TaskUpdateError(format!("{error}")))?
            .ok_or_else(|| TaskUpdateError(format!("task #{} not found", task_number)))?;

        Ok(TaskUpdateOutput {
            success: true,
            task_number: updated.task_number,
            status: updated.status.to_string(),
            message: format!("Updated task #{}", updated.task_number),
        })
    }
}
