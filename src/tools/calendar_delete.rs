//! Calendar event deletion proposal tool.

use crate::calendar::CalendarService;
use rig::completion::ToolDefinition;
use rig::tool::Tool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct CalendarDeleteTool {
    calendar_service: Arc<CalendarService>,
}

impl CalendarDeleteTool {
    pub fn new(calendar_service: Arc<CalendarService>) -> Self {
        Self { calendar_service }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("calendar_delete failed: {0}")]
pub struct CalendarDeleteError(String);

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CalendarDeleteArgs {
    /// Mirrored calendar event ID returned by `calendar_list`.
    pub event_id: String,
}

#[derive(Debug, Serialize)]
pub struct CalendarDeleteOutput {
    pub success: bool,
    pub proposal: crate::calendar::CalendarChangeProposal,
}

impl Tool for CalendarDeleteTool {
    const NAME: &'static str = "calendar_delete";

    type Error = CalendarDeleteError;
    type Args = CalendarDeleteArgs;
    type Output = CalendarDeleteOutput;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: crate::prompts::text::get("tools/calendar_delete").to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "required": ["event_id"],
                "properties": {
                    "event_id": {
                        "type": "string",
                        "description": "Mirrored calendar event ID returned by calendar_list."
                    }
                }
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let proposal = self
            .calendar_service
            .propose_delete(&args.event_id)
            .await
            .map_err(|error| CalendarDeleteError(error.to_string()))?;
        Ok(CalendarDeleteOutput {
            success: true,
            proposal,
        })
    }
}
