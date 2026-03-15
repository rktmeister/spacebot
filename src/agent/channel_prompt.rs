//! System prompt assembly and temporal context for channels.
//!
//! Contains `TemporalContext` for timezone-aware timestamps and
//! all the prompt-building methods that assemble the channel's
//! system prompt from identity, memory bulletin, skills, status, etc.

use chrono::{DateTime, Local, Utc};
use chrono_tz::Tz;

/// Debounce window for retriggers: coalesce rapid branch/worker completions
/// into a single retrigger instead of firing one per event.
pub(crate) const RETRIGGER_DEBOUNCE_MS: u64 = 500;

/// Maximum retriggers allowed since the last real user message. Prevents
/// infinite retrigger cascades where each retrigger spawns more work.
pub(crate) const MAX_RETRIGGERS_PER_TURN: usize = 3;

/// Max LLM turns for retrigger relay. Retriggers are simple relay tasks —
/// the LLM just needs to call the reply tool once. A low cap avoids wasting
/// tokens on retries when the model struggles with the retrigger format.
pub(crate) const RETRIGGER_MAX_TURNS: usize = 3;

#[derive(Debug, Clone)]
pub(crate) enum TemporalTimezone {
    Named { timezone_name: String, timezone: Tz },
    SystemLocal,
}

#[derive(Debug, Clone)]
pub(crate) struct TemporalContext {
    pub(crate) now_utc: DateTime<Utc>,
    pub(crate) timezone: TemporalTimezone,
}

impl TemporalContext {
    pub(crate) fn from_runtime(runtime_config: &crate::config::RuntimeConfig) -> Self {
        let now_utc = Utc::now();
        let user_timezone = runtime_config.user_timezone.load().as_ref().clone();
        let cron_timezone = runtime_config.cron_timezone.load().as_ref().clone();

        Self {
            now_utc,
            timezone: Self::resolve_timezone_from_names(user_timezone, cron_timezone),
        }
    }

    pub(crate) fn resolve_timezone_from_names(
        user_timezone: Option<String>,
        cron_timezone: Option<String>,
    ) -> TemporalTimezone {
        if let Some(timezone_name) = user_timezone {
            match timezone_name.parse::<Tz>() {
                Ok(timezone) => {
                    return TemporalTimezone::Named {
                        timezone_name,
                        timezone,
                    };
                }
                Err(_) => {
                    let cron_timezone_candidate =
                        cron_timezone.as_deref().unwrap_or("none configured");
                    tracing::warn!(
                        timezone = %timezone_name,
                        cron_timezone = %cron_timezone_candidate,
                        "invalid runtime timezone for channel temporal context, will try cron_timezone then fall back to system local"
                    );
                }
            }
        }

        if let Some(timezone_name) = cron_timezone {
            match timezone_name.parse::<Tz>() {
                Ok(timezone) => {
                    return TemporalTimezone::Named {
                        timezone_name,
                        timezone,
                    };
                }
                Err(error) => {
                    tracing::warn!(
                        timezone = %timezone_name,
                        error = %error,
                        "invalid cron_timezone for channel temporal context, falling back to system local"
                    );
                }
            }
        }

        TemporalTimezone::SystemLocal
    }

    pub(crate) fn format_timestamp(&self, timestamp: DateTime<Utc>) -> String {
        let utc_timestamp = timestamp.format("%Y-%m-%d %H:%M:%S UTC").to_string();
        let display = self.format_display_timestamp(timestamp);
        format!("{display}; UTC {utc_timestamp}")
    }

    pub(crate) fn format_date_line(&self, timestamp: DateTime<Utc>) -> String {
        match &self.timezone {
            TemporalTimezone::Named {
                timezone_name,
                timezone,
            } => {
                let local_timestamp = timestamp.with_timezone(timezone);
                format!(
                    "{} ({timezone_name})",
                    local_timestamp.format("%a, %d %b %Y")
                )
            }
            TemporalTimezone::SystemLocal => {
                let local_timestamp = timestamp.with_timezone(&Local);
                format!("{} (system local)", local_timestamp.format("%a, %d %b %Y"))
            }
        }
    }

    pub(crate) fn format_display_timestamp(&self, timestamp: DateTime<Utc>) -> String {
        match &self.timezone {
            TemporalTimezone::Named {
                timezone_name,
                timezone,
            } => {
                let local_timestamp = timestamp.with_timezone(timezone);
                format!(
                    "{} ({}, UTC{})",
                    local_timestamp.format("%Y-%m-%d %H:%M:%S"),
                    timezone_name,
                    local_timestamp.format("%:z")
                )
            }
            TemporalTimezone::SystemLocal => {
                let local_timestamp = timestamp.with_timezone(&Local);
                format!(
                    "{} (system local, UTC{})",
                    local_timestamp.format("%Y-%m-%d %H:%M:%S"),
                    local_timestamp.format("%:z")
                )
            }
        }
    }

    pub(crate) fn current_time_line(&self) -> String {
        self.format_timestamp(self.now_utc)
    }

    pub(crate) fn current_date_line(&self) -> String {
        self.format_date_line(self.now_utc)
    }
}

#[cfg(test)]
mod tests {
    use super::{TemporalContext, TemporalTimezone};
    use chrono::TimeZone as _;

    #[test]
    fn render_current_date_line_formats_named_timezone() {
        let timezone = "Asia/Singapore"
            .parse()
            .expect("Asia/Singapore should parse");
        let context = TemporalContext {
            now_utc: chrono::Utc
                .with_ymd_and_hms(2026, 3, 15, 1, 0, 0)
                .single()
                .expect("valid UTC timestamp"),
            timezone: TemporalTimezone::Named {
                timezone_name: "Asia/Singapore".to_string(),
                timezone,
            },
        };

        assert_eq!(
            context.current_date_line(),
            "Sun, 15 Mar 2026 (Asia/Singapore)"
        );
    }
}
