//! Safe camera discovery and operational status for local daemon adapters.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CameraInfo {
    pub camera_id: String,
    pub enabled: bool,
    pub source_kind: String,
    pub state: String,
    pub frame_supported: bool,
    pub frame_available: bool,
    pub cached_frame_age_ms: Option<u64>,
    pub frames: u64,
    pub detections: u64,
    pub recorded: u64,
    pub reconnects: u64,
    pub has_error: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CamerasResponse {
    pub database_id: String,
    pub observed_at: String,
    pub cameras: Vec<CameraInfo>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DaemonHealthResponse {
    pub database_id: String,
    pub observed_at: String,
    pub started_at: String,
    pub daemon_state: String,
    pub cameras: Vec<CameraInfo>,
    pub cache_bytes: u64,
    pub cache_max_bytes: u64,
    pub cache_ttl_ms: u64,
    pub frame_asset_count: usize,
    pub frame_asset_bytes: u64,
    pub frame_asset_max_count: usize,
    pub frame_asset_max_bytes: u64,
}
