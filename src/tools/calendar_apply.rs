//! Calendar proposal application tool.

use crate::calendar::CalendarService;
use rig::completion::ToolDefinition;
use rig::tool::Tool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct CalendarApplyTool {
    calendar_service: Arc<CalendarService>,
}

impl CalendarApplyTool {
    pub fn new(calendar_service: Arc<CalendarService>) -> Self {
        Self { calendar_service }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("calendar_apply failed: {0}")]
pub struct CalendarApplyError(String);

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CalendarApplyArgs {
    /// Proposal ID returned by calendar_create, calendar_update, or calendar_delete.
    pub proposal_id: String,
}

#[derive(Debug, Serialize)]
pub struct CalendarApplyOutput {
    pub success: bool,
    pub proposal: crate::calendar::CalendarChangeProposal,
}

impl Tool for CalendarApplyTool {
    const NAME: &'static str = "calendar_apply";

    type Error = CalendarApplyError;
    type Args = CalendarApplyArgs;
    type Output = CalendarApplyOutput;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: crate::prompts::text::get("tools/calendar_apply").to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "required": ["proposal_id"],
                "properties": {
                    "proposal_id": {
                        "type": "string",
                        "description": "Proposal ID returned by calendar_create, calendar_update, or calendar_delete."
                    }
                }
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let proposal = self
            .calendar_service
            .apply_proposal(&args.proposal_id)
            .await
            .map_err(|error| CalendarApplyError(error.to_string()))?;
        Ok(CalendarApplyOutput {
            success: true,
            proposal,
        })
    }
}
