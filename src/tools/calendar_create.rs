//! Calendar event creation proposal tool.

use crate::calendar::{CalendarEventDraft, CalendarService};
use rig::completion::ToolDefinition;
use rig::tool::Tool;
use serde::Serialize;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct CalendarCreateTool {
    calendar_service: Arc<CalendarService>,
}

impl CalendarCreateTool {
    pub fn new(calendar_service: Arc<CalendarService>) -> Self {
        Self { calendar_service }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("calendar_create failed: {0}")]
pub struct CalendarCreateError(String);

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct CalendarCreateArgs {
    pub draft: CalendarEventDraft,
}

#[derive(Debug, Serialize)]
pub struct CalendarCreateOutput {
    pub success: bool,
    pub proposal: crate::calendar::CalendarChangeProposal,
}

impl Tool for CalendarCreateTool {
    const NAME: &'static str = "calendar_create";

    type Error = CalendarCreateError;
    type Args = CalendarCreateArgs;
    type Output = CalendarCreateOutput;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: crate::prompts::text::get("tools/calendar_create").to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "required": ["draft"],
                "properties": {
                    "draft": schemars::schema_for!(CalendarEventDraft),
                }
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let proposal = self
            .calendar_service
            .propose_create(args.draft)
            .await
            .map_err(|error| CalendarCreateError(error.to_string()))?;
        Ok(CalendarCreateOutput {
            success: true,
            proposal,
        })
    }
}
