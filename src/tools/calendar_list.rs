//! Calendar occurrence listing tool for branches and cortex chat.

use crate::calendar::CalendarService;
use rig::completion::ToolDefinition;
use rig::tool::Tool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct CalendarListTool {
    calendar_service: Arc<CalendarService>,
}

impl CalendarListTool {
    pub fn new(calendar_service: Arc<CalendarService>) -> Self {
        Self { calendar_service }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("calendar_list failed: {0}")]
pub struct CalendarListError(String);

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CalendarListArgs {
    /// Range start in RFC3339 format. Defaults to now.
    pub start_at: Option<String>,
    /// Range end in RFC3339 format. Defaults to 7 days after `start_at`.
    pub end_at: Option<String>,
    /// Maximum number of occurrences to return.
    #[serde(default = "default_limit")]
    pub limit: i32,
}

fn default_limit() -> i32 {
    100
}

#[derive(Debug, Serialize)]
pub struct CalendarListOutput {
    pub success: bool,
    pub range_start: String,
    pub range_end: String,
    pub count: usize,
    pub occurrences: Vec<crate::calendar::CalendarOccurrence>,
}

impl Tool for CalendarListTool {
    const NAME: &'static str = "calendar_list";

    type Error = CalendarListError;
    type Args = CalendarListArgs;
    type Output = CalendarListOutput;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: crate::prompts::text::get("tools/calendar_list").to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "start_at": {
                        "type": "string",
                        "description": "Optional RFC3339 range start. Defaults to now."
                    },
                    "end_at": {
                        "type": "string",
                        "description": "Optional RFC3339 range end. Defaults to 7 days after start_at."
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of occurrences to return (default 100)."
                    }
                }
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let range_start = args
            .start_at
            .as_deref()
            .map(parse_utc_datetime)
            .transpose()?
            .unwrap_or_else(chrono::Utc::now);
        let range_end = args
            .end_at
            .as_deref()
            .map(parse_utc_datetime)
            .transpose()?
            .unwrap_or_else(|| range_start + chrono::Duration::days(7));
        if range_end <= range_start {
            return Err(CalendarListError(
                "end_at must be after start_at".to_string(),
            ));
        }

        let mut occurrences = self
            .calendar_service
            .list_occurrences(range_start, range_end)
            .await
            .map_err(|error| CalendarListError(error.to_string()))?;
        let limit = usize::try_from(args.limit.clamp(1, 500)).unwrap_or(100);
        occurrences.truncate(limit);

        Ok(CalendarListOutput {
            success: true,
            range_start: range_start.to_rfc3339(),
            range_end: range_end.to_rfc3339(),
            count: occurrences.len(),
            occurrences,
        })
    }
}

fn parse_utc_datetime(value: &str) -> Result<chrono::DateTime<chrono::Utc>, CalendarListError> {
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .map_err(|error| CalendarListError(format!("invalid RFC3339 datetime '{value}': {error}")))
}
