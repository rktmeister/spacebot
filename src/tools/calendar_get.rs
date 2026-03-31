//! Calendar event lookup tool.

use crate::calendar::CalendarService;
use crate::config::RuntimeConfig;
use crate::tools::calendar_display::{
    CalendarEventDisplay, display_timezone_label, event_display, guidance_summary,
};
use rig::completion::ToolDefinition;
use rig::tool::Tool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct CalendarGetTool {
    calendar_service: Arc<CalendarService>,
    runtime_config: Arc<RuntimeConfig>,
}

impl CalendarGetTool {
    pub fn new(calendar_service: Arc<CalendarService>, runtime_config: Arc<RuntimeConfig>) -> Self {
        Self {
            calendar_service,
            runtime_config,
        }
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
    pub display_timezone: String,
    pub summary: String,
    pub event: CalendarEventDisplay,
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
        let display_timezone = display_timezone_label(self.runtime_config.as_ref());
        let event =
            event_display(self.runtime_config.as_ref(), &event).map_err(CalendarGetError)?;

        Ok(CalendarGetOutput {
            success: true,
            display_timezone: display_timezone.clone(),
            summary: guidance_summary(&display_timezone),
            event,
        })
    }
}
