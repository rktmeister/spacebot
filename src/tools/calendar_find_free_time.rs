//! Calendar free-time discovery tool.

use crate::calendar::CalendarService;
use crate::config::RuntimeConfig;
use crate::tools::calendar_display::{
    CalendarAvailabilitySlotDisplay, availability_slot_display, display_timezone_label,
    guidance_summary,
};
use rig::completion::ToolDefinition;
use rig::tool::Tool;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct CalendarFindFreeTimeTool {
    calendar_service: Arc<CalendarService>,
    runtime_config: Arc<RuntimeConfig>,
}

impl CalendarFindFreeTimeTool {
    pub fn new(calendar_service: Arc<CalendarService>, runtime_config: Arc<RuntimeConfig>) -> Self {
        Self {
            calendar_service,
            runtime_config,
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("calendar_find_free_time failed: {0}")]
pub struct CalendarFindFreeTimeError(String);

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CalendarFindFreeTimeArgs {
    /// Range start in RFC3339 format.
    pub start_at: String,
    /// Range end in RFC3339 format.
    pub end_at: String,
    /// Minimum free slot duration in minutes.
    pub duration_minutes: i64,
}

#[derive(Debug, Serialize)]
pub struct CalendarFindFreeTimeOutput {
    pub success: bool,
    pub display_timezone: String,
    pub summary: String,
    pub slots: Vec<CalendarAvailabilitySlotDisplay>,
}

impl Tool for CalendarFindFreeTimeTool {
    const NAME: &'static str = "calendar_find_free_time";

    type Error = CalendarFindFreeTimeError;
    type Args = CalendarFindFreeTimeArgs;
    type Output = CalendarFindFreeTimeOutput;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: crate::prompts::text::get("tools/calendar_find_free_time").to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "required": ["start_at", "end_at", "duration_minutes"],
                "properties": {
                    "start_at": {
                        "type": "string",
                        "description": "RFC3339 range start."
                    },
                    "end_at": {
                        "type": "string",
                        "description": "RFC3339 range end."
                    },
                    "duration_minutes": {
                        "type": "integer",
                        "description": "Minimum desired free slot in minutes."
                    }
                }
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let start_at = parse_utc_datetime(&args.start_at)?;
        let end_at = parse_utc_datetime(&args.end_at)?;
        if end_at <= start_at || args.duration_minutes <= 0 {
            return Err(CalendarFindFreeTimeError(
                "end_at must be after start_at and duration_minutes must be positive".to_string(),
            ));
        }

        let slots = self
            .calendar_service
            .find_free_time(start_at, end_at, args.duration_minutes)
            .await
            .map_err(|error| CalendarFindFreeTimeError(error.to_string()))?;
        let display_timezone = display_timezone_label(self.runtime_config.as_ref());
        let slots = slots
            .iter()
            .map(|slot| availability_slot_display(self.runtime_config.as_ref(), slot))
            .collect::<Result<Vec<_>, _>>()
            .map_err(CalendarFindFreeTimeError)?;

        Ok(CalendarFindFreeTimeOutput {
            success: true,
            display_timezone: display_timezone.clone(),
            summary: guidance_summary(&display_timezone),
            slots,
        })
    }
}

fn parse_utc_datetime(
    value: &str,
) -> Result<chrono::DateTime<chrono::Utc>, CalendarFindFreeTimeError> {
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .map_err(|error| {
            CalendarFindFreeTimeError(format!("invalid RFC3339 datetime '{value}': {error}"))
        })
}
