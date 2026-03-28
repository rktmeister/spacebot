//! Calendar event update proposal tool.

use crate::calendar::{CalendarEventDraft, CalendarService};
use rig::completion::ToolDefinition;
use rig::tool::Tool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct CalendarUpdateTool {
    calendar_service: Arc<CalendarService>,
}

impl CalendarUpdateTool {
    pub fn new(calendar_service: Arc<CalendarService>) -> Self {
        Self { calendar_service }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("calendar_update failed: {0}")]
pub struct CalendarUpdateError(String);

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CalendarUpdateArgs {
    /// Mirrored calendar event ID returned by `calendar_list`.
    pub event_id: String,
    pub draft: CalendarEventDraft,
}

#[derive(Debug, Serialize)]
pub struct CalendarUpdateOutput {
    pub success: bool,
    pub proposal: crate::calendar::CalendarChangeProposal,
}

impl Tool for CalendarUpdateTool {
    const NAME: &'static str = "calendar_update";

    type Error = CalendarUpdateError;
    type Args = CalendarUpdateArgs;
    type Output = CalendarUpdateOutput;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: crate::prompts::text::get("tools/calendar_update").to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "required": ["event_id", "draft"],
                "properties": {
                    "event_id": {
                        "type": "string",
                        "description": "Mirrored calendar event ID returned by calendar_list."
                    },
                    "draft": schemars::schema_for!(CalendarEventDraft),
                }
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let proposal = self
            .calendar_service
            .propose_update(&args.event_id, args.draft)
            .await
            .map_err(|error| CalendarUpdateError(error.to_string()))?;
        Ok(CalendarUpdateOutput {
            success: true,
            proposal,
        })
    }
}
