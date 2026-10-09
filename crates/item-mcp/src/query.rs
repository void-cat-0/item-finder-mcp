use std::path::PathBuf;
use std::sync::{Arc, atomic::AtomicBool};
use std::time::Instant;

use chrono::{DateTime, SecondsFormat, Utc};
use item_core::Observation;
use item_core::store::{ObservationFilter, RecordedItemSummary, Store};
use rmcp::model::CallToolResult;
use rmcp::schemars;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::Semaphore;

pub const DIRECT_NOTE: &str = "Direct sighting records describe where items were last recorded, not their current location. Labels may include detector or webhook false positives.";

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InventoryRequest {
    /// Exact camera ID; omit to search all cameras.
    pub camera: Option<String>,
    /// Exact zone name; omit to search all zones.
    pub zone: Option<String>,
    /// Number of distinct recorded labels, from 1 to 500; default 200.
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListRequest {
    /// Literal label substring, not a SQL or wildcard pattern.
    pub label: Option<String>,
    pub camera: Option<String>,
    pub zone: Option<String>,
    /// Inclusive lower bound on last_seen, as RFC3339 with timezone.
    pub since: Option<String>,
    /// Inclusive upper bound on last_seen, as RFC3339 with timezone.
    pub until: Option<String>,
    /// Number of observations, from 1 to 500; default 20.
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LocateRequest {
    /// An English question parsed by a keyword heuristic; explicit label is preferred.
    pub query: Option<String>,
    /// Explicit literal label substring, preferred over query. Use list_recorded_items to discover labels.
    pub label: Option<String>,
    pub camera: Option<String>,
    pub zone: Option<String>,
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetRequest {
    pub observation_id: i64,
    /// Optional exact camera ID guard for multi-camera callers.
    pub camera_id: Option<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ObservationJson {
    pub observation_id: i64,
    pub camera_id: String,
    pub zone: String,
    pub label: String,
    pub first_seen: String,
    pub last_seen: String,
    pub hit_count: i64,
}

impl From<Observation> for ObservationJson {
    fn from(o: Observation) -> Self {
        Self {
            observation_id: o.id,
            camera_id: o.camera_id,
            zone: o.zone,
            label: o.label,
            first_seen: timestamp(o.first_seen),
            last_seen: timestamp(o.last_seen),
            hit_count: o.hit_count,
        }
    }
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct RecordedItemJson {
    pub label: String,
    pub observation_count: i64,
    pub last_seen: String,
    pub cameras: Vec<String>,
    pub zones: Vec<String>,
}

impl From<RecordedItemSummary> for RecordedItemJson {
    fn from(item: RecordedItemSummary) -> Self {
        Self {
            label: item.label,
            observation_count: item.observation_count,
            last_seen: timestamp(item.last_seen),
            cameras: item.cameras,
            zones: item.zones,
        }
    }
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct InventoryResponse {
    pub items: Vec<RecordedItemJson>,
    pub truncated: bool,
    pub limit: i64,
    pub source: &'static str,
    pub note: &'static str,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ListResponse {
    pub observations: Vec<ObservationJson>,
    pub limit: i64,
    pub time_filter: &'static str,
    pub source: &'static str,
    pub note: &'static str,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct LocateResponse {
    pub keyword: String,
    pub label_source: &'static str,
    pub direct: Vec<ObservationJson>,
    pub limit: i64,
    pub source: &'static str,
    pub note: &'static str,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct GetResponse {
    pub observation: ObservationJson,
    pub source: &'static str,
    pub detail_level: &'static str,
    pub note: &'static str,
}

#[cfg(test)]
#[path = "query_budget_tests.rs"]
mod budget_tests;

#[derive(Debug, Clone)]
pub(crate) struct Database {
    path: PathBuf,
    permits: Arc<Semaphore>,
}

impl Database {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            permits: Arc::new(Semaphore::new(4)),
        }
    }

    pub async fn read<T, F>(&self, operation: F) -> Result<T, CallToolResult>
    where
        T: Send + 'static,
        F: FnOnce(&Store) -> item_core::store::Result<T> + Send + 'static,
    {
        let permit = self
            .permits
            .clone()
            .acquire_owned()
            .await
            .map_err(|error| {
                tracing::error!(%error, "database query limiter unavailable");
                tool_error("internal", "The query service is unavailable.")
            })?;
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            // Keep the permit with the work even if the caller cancels its await.
            let _permit = permit;
            let store = Store::open_read_only(path)?;
            operation(&store)
        })
        .await
        .map_err(|error| {
            tracing::error!(%error, "database query task failed");
            tool_error("internal", "The query could not be completed.")
        })?
        .map_err(|error| {
            tracing::warn!(%error, "read-only database query failed");
            tool_error(
                "database_unavailable",
                "The configured database could not be read. Check the database configuration and ingest service.",
            )
        })
    }

    /// Read-side geometry admission. The deadline starts before semaphore admission;
    /// the permit is moved into blocking work and remains held through cancellation.
    pub async fn read_snapshot<T, F>(
        &self,
        deadline: Instant,
        cancelled: Arc<AtomicBool>,
        operation: F,
    ) -> Result<T, CallToolResult>
    where
        T: Send + 'static,
        F: FnOnce(&item_core::evidence::ReadSnapshot<'_>) -> item_core::store::Result<T>
            + Send
            + 'static,
    {
        let wait = deadline.saturating_duration_since(Instant::now());
        let permit = tokio::time::timeout(wait, self.permits.clone().acquire_owned())
            .await
            .map_err(|_| tool_error("timeout", "The geometry request exceeded its deadline."))?
            .map_err(|_| tool_error("internal", "The query service is unavailable."))?;
        let path = self.path.clone();
        let task = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let control = item_core::evidence::ReadControl::new(deadline, cancelled);
            let store = Store::open_read_only_controlled(path, control.clone())?;
            let snapshot = store.read_snapshot(control)?;
            operation(&snapshot)
        });
        tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), task)
            .await
            .map_err(|_| tool_error("timeout", "The geometry request exceeded its deadline."))?
            .map_err(|_| tool_error("internal", "The geometry query could not be completed."))?
            .map_err(store_error)
    }
}

pub(crate) fn store_error(error: item_core::store::StoreError) -> CallToolResult {
    use item_core::store::StoreError;
    match error {
        StoreError::ReadTimeout => {
            tool_error("timeout", "The geometry request exceeded its deadline.")
        }
        StoreError::ReadCancelled => tool_error("cancelled", "The geometry request was cancelled."),
        StoreError::PayloadTooLarge => tool_error(
            "payload_too_large",
            "Necessary database evidence exceeds the response budget.",
        ),
        StoreError::ObservationNotFound => {
            tool_error("not_found", "No observation with this ID is recorded.")
        }
        StoreError::CameraMismatch => tool_error(
            "camera_mismatch",
            "The observation does not belong to the requested camera.",
        ),
        StoreError::InvalidInput(_) => {
            tool_error("invalid_input", "The geometry request is invalid.")
        }
        _ => tool_error(
            "database_unavailable",
            "The configured database could not be read.",
        ),
    }
}

pub(crate) fn tool_error(code: &'static str, message: &'static str) -> CallToolResult {
    CallToolResult::structured_error(json!({"error":{"code":code,"message":message}}))
}

pub(crate) fn limit(value: Option<i64>, default: i64) -> Result<i64, CallToolResult> {
    match value.unwrap_or(default) {
        value @ 1..=500 => Ok(value),
        _ => Err(tool_error(
            "invalid_input",
            "limit must be between 1 and 500.",
        )),
    }
}

pub(crate) fn text(value: Option<String>) -> Result<Option<String>, CallToolResult> {
    value
        .map(|value| {
            if value.trim().is_empty() || value.chars().count() > 1024 || value.chars().any(char::is_control) {
                return Err(tool_error(
                    "invalid_input",
                    "Text filters must be nonempty, at most 1024 characters, and contain no control characters.",
                ));
            }
            Ok(value.to_owned())
        })
        .transpose()
}

fn date(value: Option<String>) -> Result<Option<DateTime<Utc>>, CallToolResult> {
    value
        .map(|value| {
            if value.len() > 64 {
                return Err(tool_error(
                    "invalid_input",
                    "Time bounds must be RFC3339 timestamps with timezone.",
                ));
            }
            DateTime::parse_from_rfc3339(&value)
                .map(|date| date.with_timezone(&Utc))
                .map_err(|_| {
                    tool_error(
                        "invalid_input",
                        "Time bounds must be RFC3339 timestamps with timezone.",
                    )
                })
        })
        .transpose()
}

impl ListRequest {
    pub(crate) fn filter(self) -> Result<ObservationFilter, CallToolResult> {
        let since = date(self.since)?;
        let until = date(self.until)?;
        if matches!((&since, &until), (Some(since), Some(until)) if since > until) {
            return Err(tool_error(
                "invalid_input",
                "since must not be later than until.",
            ));
        }
        Ok(ObservationFilter {
            label: text(self.label)?,
            camera: text(self.camera)?,
            zone: text(self.zone)?,
            since,
            until,
            limit: limit(self.limit, 20)?,
        })
    }
}

fn timestamp(date: DateTime<Utc>) -> String {
    date.to_rfc3339_opts(SecondsFormat::AutoSi, true)
}
