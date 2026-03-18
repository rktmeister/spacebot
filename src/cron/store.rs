//! Cron job CRUD storage (SQLite).

use crate::cron::scheduler::CronConfig;
use crate::error::Result;
use anyhow::Context as _;
use chrono::{DateTime, NaiveDateTime, SecondsFormat, Utc};
use sqlx::{Row as _, SqlitePool};
use std::collections::HashMap;

/// Cron job store for persistence.
#[derive(Debug)]
pub struct CronStore {
    pool: SqlitePool,
}

impl CronStore {
    /// Create a new cron store.
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Rewrite legacy SQLite timestamp strings into explicit RFC3339 UTC form.
    ///
    /// Older cron execution rows rely on SQLite's `CURRENT_TIMESTAMP`, which
    /// stores `YYYY-MM-DD HH:MM:SS` in UTC but without an explicit offset.
    /// Browsers commonly parse that as local time, shifting dashboard displays.
    pub async fn normalize_execution_timestamps(&self) -> Result<usize> {
        let rows = sqlx::query(
            r#"
            SELECT id, executed_at
            FROM cron_executions
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .context("failed to load cron execution timestamps for normalization")?;

        let mut updates = Vec::new();
        for row in rows {
            let execution_id: String = row.try_get("id")?;
            let executed_at: String = row.try_get("executed_at")?;
            let normalized = normalize_cron_timestamp_lossy(&executed_at);
            if normalized != executed_at {
                updates.push((execution_id, normalized));
            }
        }

        if updates.is_empty() {
            return Ok(0);
        }

        let mut transaction = self
            .pool
            .begin()
            .await
            .context("failed to begin cron timestamp normalization transaction")?;

        for (execution_id, normalized_timestamp) in &updates {
            sqlx::query(
                r#"
                UPDATE cron_executions
                SET executed_at = ?
                WHERE id = ?
                "#,
            )
            .bind(normalized_timestamp)
            .bind(execution_id)
            .execute(&mut *transaction)
            .await
            .context("failed to rewrite normalized cron execution timestamp")?;
        }

        transaction
            .commit()
            .await
            .context("failed to commit cron timestamp normalization transaction")?;

        Ok(updates.len())
    }

    /// Save a cron job configuration.
    pub async fn save(&self, config: &CronConfig) -> Result<()> {
        let active_start = config.active_hours.map(|h| h.0 as i64);
        let active_end = config.active_hours.map(|h| h.1 as i64);

        sqlx::query(
            r#"
            INSERT INTO cron_jobs (id, prompt, cron_expr, interval_secs, delivery_target, active_start_hour, active_end_hour, enabled, run_once, timeout_secs)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(id) DO UPDATE SET
                prompt = excluded.prompt,
                cron_expr = excluded.cron_expr,
                interval_secs = excluded.interval_secs,
                delivery_target = excluded.delivery_target,
                active_start_hour = excluded.active_start_hour,
                active_end_hour = excluded.active_end_hour,
                enabled = excluded.enabled,
                run_once = excluded.run_once,
                timeout_secs = excluded.timeout_secs
            "#
        )
        .bind(&config.id)
        .bind(&config.prompt)
        .bind(config.cron_expr.as_deref())
        .bind(config.interval_secs as i64)
        .bind(&config.delivery_target)
        .bind(active_start)
        .bind(active_end)
        .bind(config.enabled as i64)
        .bind(config.run_once as i64)
        .bind(config.timeout_secs.map(|t| t as i64))
        .execute(&self.pool)
        .await
        .context("failed to save cron job")?;

        Ok(())
    }

    /// Load all enabled cron job configurations.
    pub async fn load_all(&self) -> Result<Vec<CronConfig>> {
        let rows = sqlx::query(
            r#"
            SELECT id, prompt, cron_expr, interval_secs, delivery_target, active_start_hour, active_end_hour, enabled, run_once, timeout_secs
            FROM cron_jobs
            WHERE enabled = 1
            ORDER BY created_at ASC
            "#
        )
        .fetch_all(&self.pool)
        .await
        .context("failed to load cron jobs")?;

        let configs = rows
            .into_iter()
            .map(|row| CronConfig {
                id: row.try_get("id").unwrap_or_default(),
                prompt: row.try_get("prompt").unwrap_or_default(),
                cron_expr: row.try_get::<Option<String>, _>("cron_expr").ok().flatten(),
                interval_secs: row.try_get::<i64, _>("interval_secs").unwrap_or(3600) as u64,
                delivery_target: row.try_get("delivery_target").unwrap_or_default(),
                active_hours: {
                    let start: Option<i64> = row.try_get("active_start_hour").ok();
                    let end: Option<i64> = row.try_get("active_end_hour").ok();
                    match (start, end) {
                        (Some(s), Some(e)) if s != e => Some((s as u8, e as u8)),
                        _ => None,
                    }
                },
                enabled: row.try_get::<i64, _>("enabled").unwrap_or(1) != 0,
                run_once: row.try_get::<i64, _>("run_once").unwrap_or(0) != 0,
                timeout_secs: row
                    .try_get::<Option<i64>, _>("timeout_secs")
                    .ok()
                    .flatten()
                    .map(|t| t as u64),
            })
            .collect();

        Ok(configs)
    }

    /// Delete a cron job.
    pub async fn delete(&self, id: &str) -> Result<()> {
        sqlx::query("DELETE FROM cron_jobs WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await
            .context("failed to delete cron job")?;

        Ok(())
    }

    /// Update the enabled state of a cron job (used by circuit breaker).
    pub async fn update_enabled(&self, id: &str, enabled: bool) -> Result<()> {
        sqlx::query("UPDATE cron_jobs SET enabled = ? WHERE id = ?")
            .bind(enabled as i64)
            .bind(id)
            .execute(&self.pool)
            .await
            .context("failed to update cron job enabled state")?;

        Ok(())
    }

    /// Log a cron job execution result.
    pub async fn log_execution(
        &self,
        cron_id: &str,
        success: bool,
        result_summary: Option<&str>,
    ) -> Result<()> {
        let execution_id = uuid::Uuid::new_v4().to_string();
        let executed_at = utc_timestamp_rfc3339();

        sqlx::query(
            r#"
            INSERT INTO cron_executions (id, cron_id, executed_at, success, result_summary)
            VALUES (?, ?, ?, ?, ?)
            "#,
        )
        .bind(&execution_id)
        .bind(cron_id)
        .bind(&executed_at)
        .bind(success as i64)
        .bind(result_summary)
        .execute(&self.pool)
        .await
        .context("failed to log cron execution")?;

        Ok(())
    }

    /// Load all cron job configurations (including disabled).
    pub async fn load_all_unfiltered(&self) -> Result<Vec<CronConfig>> {
        let rows = sqlx::query(
            r#"
            SELECT id, prompt, cron_expr, interval_secs, delivery_target, active_start_hour, active_end_hour, enabled, run_once, timeout_secs
            FROM cron_jobs
            ORDER BY created_at ASC
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .context("failed to load cron jobs")?;

        let configs = rows
            .into_iter()
            .map(|row| CronConfig {
                id: row.try_get("id").unwrap_or_default(),
                prompt: row.try_get("prompt").unwrap_or_default(),
                cron_expr: row.try_get::<Option<String>, _>("cron_expr").ok().flatten(),
                interval_secs: row.try_get::<i64, _>("interval_secs").unwrap_or(3600) as u64,
                delivery_target: row.try_get("delivery_target").unwrap_or_default(),
                active_hours: {
                    let start: Option<i64> = row.try_get("active_start_hour").ok();
                    let end: Option<i64> = row.try_get("active_end_hour").ok();
                    match (start, end) {
                        (Some(s), Some(e)) if s != e => Some((s as u8, e as u8)),
                        _ => None,
                    }
                },
                enabled: row.try_get::<i64, _>("enabled").unwrap_or(1) != 0,
                run_once: row.try_get::<i64, _>("run_once").unwrap_or(0) != 0,
                timeout_secs: row
                    .try_get::<Option<i64>, _>("timeout_secs")
                    .ok()
                    .flatten()
                    .map(|t| t as u64),
            })
            .collect();

        Ok(configs)
    }

    /// Load execution history for a specific cron job.
    pub async fn load_executions(
        &self,
        cron_id: &str,
        limit: i64,
    ) -> Result<Vec<CronExecutionEntry>> {
        let rows = sqlx::query(
            r#"
            SELECT id, executed_at, success, result_summary
            FROM cron_executions
            WHERE cron_id = ?
            ORDER BY executed_at DESC
            LIMIT ?
            "#,
        )
        .bind(cron_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .context("failed to load cron executions")?;

        let entries = rows
            .into_iter()
            .map(|row| CronExecutionEntry {
                id: row.try_get("id").unwrap_or_default(),
                executed_at: normalize_cron_timestamp_lossy(
                    &row.try_get::<String, _>("executed_at").unwrap_or_default(),
                ),
                success: row.try_get::<i64, _>("success").unwrap_or(0) != 0,
                result_summary: row.try_get("result_summary").ok(),
            })
            .collect();

        Ok(entries)
    }

    /// Load recent execution history across all cron jobs.
    pub async fn load_all_executions(&self, limit: i64) -> Result<Vec<CronExecutionEntry>> {
        let rows = sqlx::query(
            r#"
            SELECT id, cron_id, executed_at, success, result_summary
            FROM cron_executions
            ORDER BY executed_at DESC
            LIMIT ?
            "#,
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .context("failed to load cron executions")?;

        let entries = rows
            .into_iter()
            .map(|row| CronExecutionEntry {
                id: row.try_get("id").unwrap_or_default(),
                executed_at: normalize_cron_timestamp_lossy(
                    &row.try_get::<String, _>("executed_at").unwrap_or_default(),
                ),
                success: row.try_get::<i64, _>("success").unwrap_or(0) != 0,
                result_summary: row.try_get("result_summary").ok(),
            })
            .collect();

        Ok(entries)
    }

    /// Get the most recent execution timestamp for each cron job.
    ///
    /// Returns a map of `cron_id -> last_executed_at` (UTC timestamp string).
    /// Used by the scheduler to anchor interval-based jobs to their last run
    /// time after a restart, avoiding skipped or duplicate firings.
    pub async fn last_execution_times(&self) -> Result<HashMap<String, String>> {
        let rows = sqlx::query(
            r#"
            SELECT cron_id, MAX(executed_at) as last_executed_at
            FROM cron_executions
            GROUP BY cron_id
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .context("failed to load last execution times")?;

        let mut map = HashMap::new();
        for row in rows {
            let cron_id: String = row.try_get("cron_id")?;
            let last: Option<String> = row.try_get("last_executed_at")?;
            if let Some(last) = last {
                map.insert(cron_id, normalize_cron_timestamp_lossy(&last));
            }
        }

        Ok(map)
    }

    /// Get execution stats for a cron job (success count, failure count, last execution).
    pub async fn get_execution_stats(&self, cron_id: &str) -> Result<CronExecutionStats> {
        let row = sqlx::query(
            r#"
            SELECT
                SUM(CASE WHEN success = 1 THEN 1 ELSE 0 END) as success_count,
                SUM(CASE WHEN success = 0 THEN 1 ELSE 0 END) as failure_count,
                MAX(executed_at) as last_executed_at
            FROM cron_executions
            WHERE cron_id = ?
            "#,
        )
        .bind(cron_id)
        .fetch_optional(&self.pool)
        .await
        .context("failed to load cron execution stats")?;

        if let Some(row) = row {
            let success_count: i64 = row.try_get("success_count").unwrap_or(0);
            let failure_count: i64 = row.try_get("failure_count").unwrap_or(0);
            let last_executed_at: Option<String> = row
                .try_get::<Option<String>, _>("last_executed_at")
                .ok()
                .flatten()
                .map(|value| normalize_cron_timestamp_lossy(&value));

            Ok(CronExecutionStats {
                success_count: success_count as u64,
                failure_count: failure_count as u64,
                last_executed_at,
            })
        } else {
            Ok(CronExecutionStats::default())
        }
    }
}

const LEGACY_SQLITE_TIMESTAMP_FORMAT: &str = "%Y-%m-%d %H:%M:%S";
const LEGACY_SQLITE_TIMESTAMP_FRACTIONAL_FORMAT: &str = "%Y-%m-%d %H:%M:%S%.f";

fn utc_timestamp_rfc3339() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn normalize_cron_timestamp(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Ok(parsed) = DateTime::parse_from_rfc3339(trimmed) {
        return Some(parsed.to_utc().to_rfc3339_opts(SecondsFormat::Secs, true));
    }

    for format in [
        LEGACY_SQLITE_TIMESTAMP_FORMAT,
        LEGACY_SQLITE_TIMESTAMP_FRACTIONAL_FORMAT,
    ] {
        if let Ok(parsed) = NaiveDateTime::parse_from_str(trimmed, format) {
            return Some(parsed.and_utc().to_rfc3339_opts(SecondsFormat::Secs, true));
        }
    }

    None
}

fn normalize_cron_timestamp_lossy(value: &str) -> String {
    normalize_cron_timestamp(value).unwrap_or_else(|| value.to_string())
}

/// Entry in the cron execution log.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CronExecutionEntry {
    pub id: String,
    pub executed_at: String,
    pub success: bool,
    pub result_summary: Option<String>,
}

/// Execution statistics for a cron job.
#[derive(Debug, Clone, serde::Serialize, Default)]
pub struct CronExecutionStats {
    pub success_count: u64,
    pub failure_count: u64,
    pub last_executed_at: Option<String>,
}
#[cfg(test)]
mod tests {
    use super::{normalize_cron_timestamp, normalize_cron_timestamp_lossy};

    #[test]
    fn normalizes_legacy_sqlite_timestamp_as_utc() {
        assert_eq!(
            normalize_cron_timestamp("2026-03-18 01:00:00").as_deref(),
            Some("2026-03-18T01:00:00Z")
        );
    }

    #[test]
    fn normalizes_rfc3339_offsets_to_utc() {
        assert_eq!(
            normalize_cron_timestamp("2026-03-18T09:00:00+08:00").as_deref(),
            Some("2026-03-18T01:00:00Z")
        );
    }

    #[test]
    fn lossy_normalizer_preserves_unparseable_values() {
        assert_eq!(
            normalize_cron_timestamp_lossy("not-a-timestamp"),
            "not-a-timestamp"
        );
    }
}
