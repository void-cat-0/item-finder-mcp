//! Shared wire types for daemon-owned image assets.
//!
//! The database remains the source of observation facts. These DTOs describe
//! the separately-owned asset boundary used by the daemon's internal API and
//! by read-only adapters such as item-mcp.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct RawAssetMetadata {
    pub asset_id: String,
    /// Persisted source identity; observation IDs alone are only database-local.
    pub database_id: String,
    pub kind: String,
    pub mime_type: String,
    pub bytes: u64,
    pub sha256: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub capture_time: Option<String>,
    pub observation_id: Option<i64>,
    pub camera_id: Option<String>,
    pub zone: Option<String>,
    pub status: String,
    pub provenance: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CameraFrameRequest {
    pub database_id: String,
    pub max_age_ms: u64,
    pub wait_timeout_ms: u64,
    pub max_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CameraFrameMetadata {
    pub asset: RawAssetMetadata,
    pub age_ms: u64,
    pub camera_state: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AssetErrorBody {
    pub error: AssetError,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AssetError {
    pub code: String,
    pub message: String,
}

impl AssetErrorBody {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            error: AssetError {
                code: code.into(),
                message: message.into(),
            },
        }
    }
}
