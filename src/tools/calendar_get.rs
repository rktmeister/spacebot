//! Calendar event lookup tool.

use crate::calendar::CalendarService;
use rig::completion::ToolDefinition;
use rig::tool::Tool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct CalendarGetTool {
    calendar_service: Arc<CalendarService>,
}

impl CalendarGetTool {
    pub fn new(calendar_service: Arc<CalendarService>) -> Self {
        Self { calendar_service }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("calendar_get failed: {0}")]
pub struct CalendarGetError(String);

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CalendarGetArgs {
    /// Mirrored calendar event ID returned by `calendar_list`.
    pub event_id: String,
}

#[derive(Debug, Serialize)]
pub struct CalendarGetOutput {
    pub success: bool,
    pub event: crate::calendar::CalendarEvent,
}

impl Tool for CalendarGetTool {
    const NAME: &'static str = "calendar_get";

    type Error = CalendarGetError;
    type Args = CalendarGetArgs;
    type Output = CalendarGetOutput;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: crate::prompts::text::get("tools/calendar_get").to_string(),
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
        let event = self
            .calendar_service
            .get_event(&args.event_id)
            .await
            .map_err(|error| CalendarGetError(error.to_string()))?
            .ok_or_else(|| {
                CalendarGetError(format!("calendar event '{}' not found", args.event_id))
            })?;

        Ok(CalendarGetOutput {
            success: true,
            event,
        })
    }
}
