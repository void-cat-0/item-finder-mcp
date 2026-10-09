//! Declarative camera/region config, plus the runtime settings it resolves to.
//!
//! Regions are rows in SQLite, so everything else -- webhook, camera loop,
//! query -- keeps reading from the store, not from TOML.
//!
//! Precedence, lowest to highest (design/resident-ingest.md §4):
//!
//! ```text
//! built-in defaults -> config.toml -> environment -> explicit CLI flags
//! ```
//!
//! Every field a config file may supply is therefore an `Option` on the CLI
//! side: clap's `default_value` makes "the user asked for X" and "X is the
//! default" indistinguishable, and that distinction is the whole job here.
//!
//! config.toml shape (`[daemon]`/`[webhook]` are optional; a bare `[[camera]]`
//! list keeps working exactly as before):
//!
//! ```toml
//! [daemon]
//! db = "data/items.db"
//! snapshots_dir = "data/snapshots"
//! health_file = "data/health.json"
//! rescan_config_secs = 0          # 0 = no hot reload
//! checkpoint_secs = 300
//! retention_days = 90             # 0 = keep observations forever
//! sweep_secs = 3600               # 0 = prune only on demand (--maintenance)
//! reconcile_secs = 604800         # 0 = never scan for orphan snapshot files
//!
//! [webhook]
//! enabled = true
//! listen = "127.0.0.1:8477"
//! preview_url = ""                # non-empty opens /preview (rtsp feature)
//!
//! [[camera]]
//! id = "living"
//! enabled = true
//! url = "rtsp://user:pass@192.168.1.64:554/Streaming/Channels/102"
//! detector = "yolo"               # overrides the global default
//! detect_fps = 1.0                # overrides the global default
//! targets = ""                    # only meaningful for detector = "vlm"
//!
//! [camera.regions]                # rects in that camera's pixel space
//! desk = [0.0, 360.0, 1280.0, 720.0]
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

/// How much to save for each detection: off (no snapshots), first (one JPEG
/// when an observation opens), events (first + one JPEG per appeared/disappeared
/// event), or detections (events + one JPEG per detection that updates an
/// observation). Default: first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SnapshotMode {
    Off,
    #[default]
    First,
    Events,
    Detections,
}

impl SnapshotMode {
    pub fn parse(s: &str) -> Result<Self, ConfigError> {
        match s.to_ascii_lowercase().as_str() {
            "off" => Ok(Self::Off),
            "first" => Ok(Self::First),
            "events" => Ok(Self::Events),
            "detections" => Ok(Self::Detections),
            _ => Err(ConfigError::Invalid(format!(
                "snapshots mode must be off, first, events, or detections; got '{s}'"
            ))),
        }
    }
}

use crate::runtime::{CameraTask, DetectorSpec, SourceSpec};

/// Built-in defaults: the lowest-precedence layer of the merge.
pub mod defaults {
    pub const DB: &str = "data/items.db";
    pub const SNAPSHOTS_DIR: &str = "data/snapshots";
    pub const HEALTH_FILE: &str = "data/health.json";
    /// The appeared/disappeared timeline (§6). An empty value in config.toml
    /// turns it off; see [`RuntimeConfig::events_path`].
    pub const EVENTS_FILE: &str = "data/events.jsonl";
    pub const LISTEN: &str = "127.0.0.1:8477";
    pub const CAMERA_ID: &str = "rtsp-0";
    pub const DETECTOR: &str = "yolo";
    pub const MODEL: &str = "models/yolov8n.onnx";
    pub const INPUT_SIZE: usize = 640;
    pub const CONF: f32 = 0.3;
    pub const VLM_TIMEOUT_SECS: u64 = 60;
    pub const VLM_COORDS: &str = "norm1000";
    pub const DEPTH_PROVIDER: &str = "none";
    pub const DEPTH_TIMEOUT_SECS: u64 = 30;
    pub const CHECKPOINT_SECS: u64 = 300;
    /// How long an observation is kept after its `last_seen`. **Read the hard
    /// constraint in `item-ingest::retention` before lowering this**: the v2
    /// rule engine (README Roadmap step 3) infers containment from
    /// disappearances, and the evidence it needs is exactly what this window
    /// deletes -- no later run can recreate it (design/resident-ingest.md §7).
    pub const RETENTION_DAYS: u64 = 90;
    /// How often expired observations are pruned.
    pub const SWEEP_SECS: u64 = 3600;
    /// How often the snapshots directory is scanned for files whose row is
    /// gone. Weekly: it is a repair pass, not the primary cleanup.
    pub const RECONCILE_SECS: u64 = 7 * 24 * 3600;
    pub const ANNOTATION_RETENTION_DAYS: u64 = 30;
    pub const ANNOTATION_MAX_SETS: u64 = 10_000;
    pub const ANNOTATION_MAX_REVISIONS: u64 = 50_000;
    pub const ANNOTATION_MAX_METADATA_BYTES: u64 = 512 * 1024 * 1024;
    pub const ANNOTATION_MAX_EVIDENCE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
    /// Grounding is one HTTP round trip per detection against a local LLM, so
    /// the VLM default is far below the local-detector one.
    pub const DETECT_FPS_VLM: f64 = 0.2;
    pub const DETECT_FPS_LOCAL: f64 = 1.0;
}

#[derive(Debug, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub daemon: DaemonConfig,
    #[serde(default)]
    pub webhook: WebhookConfig,
    #[serde(default)]
    pub camera: Vec<CameraConfig>,
}

#[derive(Deserialize, Default)]
pub struct DaemonConfig {
    pub db: Option<String>,
    pub snapshots_dir: Option<String>,
    /// Snapshot mode: "off", "first", "events", or "detections" (default: "first").
    pub snapshots: Option<String>,
    pub health_file: Option<String>,
    /// Where the appeared/disappeared timeline is appended; `""` = produce no
    /// events (design/resident-ingest.md §6).
    pub events_file: Option<String>,
    /// 0 = never reload config.toml while running (phase 2 feature).
    pub rescan_config_secs: Option<u64>,
    pub checkpoint_secs: Option<u64>,
    /// Days an observation is kept after its `last_seen`; 0 = keep forever.
    pub retention_days: Option<u64>,
    /// How often expired observations are pruned; 0 = only on demand
    /// (`item-ingest --maintenance`).
    pub sweep_secs: Option<u64>,
    /// How often to scan the snapshots directory for orphan files; 0 = never.
    pub reconcile_secs: Option<u64>,
    /// Optional loopback-only daemon asset API listener, e.g. 127.0.0.1:8480.
    pub internal_listen: Option<String>,
    /// Bearer token for the internal asset API. Required when the listener is enabled.
    pub internal_token: Option<String>,
    /// Maximum raw asset size served by the internal API.
    pub internal_max_asset_bytes: Option<u64>,
    /// Latest camera-frame cache lifetime in milliseconds (1..=5000).
    pub frame_cache_ttl_ms: Option<u64>,
    /// Frozen camera-frame asset lifetime in seconds (1..=86400).
    pub frame_asset_ttl_secs: Option<u64>,
    /// Enable authenticated annotation submissions. Defaults to false.
    pub annotations_enabled: Option<bool>,
    /// Annotation set retention in days (1..=365).
    pub annotation_retention_days: Option<u64>,
    /// Maximum retained annotation sets.
    pub annotation_max_sets: Option<u64>,
    /// Maximum retained annotation revisions.
    pub annotation_max_revisions: Option<u64>,
    /// Logical JSON metadata budget in bytes.
    pub annotation_max_metadata_bytes: Option<u64>,
    /// Physical evidence JPEG budget in bytes.
    pub annotation_max_evidence_bytes: Option<u64>,
}

#[derive(Debug, Deserialize, Default)]
pub struct WebhookConfig {
    pub enabled: Option<bool>,
    pub listen: Option<String>,
    pub preview_url: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CameraConfig {
    pub id: String,
    /// RTSP url (may embed credentials); absent for webhook-fed cameras.
    #[serde(default)]
    pub url: Option<String>,
    /// Local/USB camera index (feature `camera`). **Not portable**: DirectShow,
    /// V4L2 and AVFoundation enumerate different devices, so this is a
    /// per-machine value in a way `url` is not (design/resident-ingest.md §9).
    /// Ignored when `url` is also set.
    #[serde(default)]
    pub webcam: Option<u32>,
    /// `false` keeps the camera out of the daemon without deleting its regions.
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub detector: Option<String>,
    #[serde(default)]
    pub detect_fps: Option<f64>,
    /// Vocabulary for `detector = "vlm"`.
    #[serde(default)]
    pub targets: Option<String>,
    /// Optional ordinal depth provider: `none` or `relative-vlm`.
    #[serde(default)]
    pub depth_provider: Option<String>,
    #[serde(default)]
    pub depth_base_url: Option<String>,
    #[serde(default)]
    pub depth_model: Option<String>,
    #[serde(default)]
    pub depth_timeout: Option<u64>,
    /// name -> [x0, y0, x1, y1] in camera pixels; first match wins (BTreeMap
    /// order is alphabetical, so name zones deliberately if they overlap).
    #[serde(default)]
    pub regions: BTreeMap<String, [f32; 4]>,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("parsing {path}: {source}")]
    Parse {
        #[source]
        source: toml::de::Error,
        path: std::path::PathBuf,
    },
    #[error("store: {0}")]
    Store(#[from] item_core::store::StoreError),
    #[error("invalid configuration: {0}")]
    Invalid(String),
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path)?;
        toml::from_str(&text).map_err(|e| ConfigError::Parse {
            source: e,
            path: path.to_path_buf(),
        })
    }

    /// Upsert every camera's regions into the store. Idempotent; safe on
    /// every daemon start. Returns how many region rows were seeded.
    pub fn seed_regions(&self, store: &item_core::store::Store) -> Result<usize, ConfigError> {
        let mut n = 0;
        for cam in &self.camera {
            for (name, rect) in &cam.regions {
                store.upsert_region(&cam.id, name, *rect)?;
                n += 1;
            }
        }
        Ok(n)
    }

    /// One task per camera the daemon should drive: the enabled `[[camera]]`
    /// rows, in file order.
    ///
    /// A `url` drives a network camera and a `webcam` index drives a local one
    /// (`url` wins when both are set, since a network camera is the portable
    /// choice). A row with neither is webhook-fed: it gets `source: None`, which
    /// the supervisor reads as "no thread".
    pub fn camera_tasks(&self, settings: &RuntimeConfig) -> Vec<CameraTask> {
        self.camera
            .iter()
            .filter(|cam| cam.enabled.unwrap_or(true))
            .map(|cam| CameraTask {
                camera_id: cam.id.clone(),
                source: cam
                    .url
                    .as_deref()
                    .filter(|url| !url.is_empty())
                    .map(|url| SourceSpec::Rtsp {
                        url: url.to_string(),
                    })
                    .or_else(|| cam.webcam.map(|index| SourceSpec::Webcam { index })),
                detector: settings.detector_spec_for(Some(cam)),
                // `main` validates every task before opening the daemon. Keep
                // this historical shape for daemon callers, but never turn an
                // invalid provider into `None` or silently disable it.
                depth: settings
                    .depth_spec_for(Some(cam))
                    .expect("depth configuration must be validated before camera_tasks"),
                detect_fps: settings.detect_fps_for(Some(cam)),
                snapshot_dir: PathBuf::from(&settings.snapshots_dir),
                snapshots: settings.snapshots,
                raw_assets: None,
                events_path: settings.events_path(),
                max_frames: 0,
                enabled: true,
            })
            .collect()
    }
}

/// The CLI-layer values that are actually present: `None` means the flag was
/// not given, which is exactly what lets config.toml win over a default.
#[derive(Debug, Default, Clone)]
pub struct CliOverrides {
    pub db: Option<String>,
    pub snapshots_dir: Option<String>,
    pub snapshots: Option<String>,
    pub listen: Option<String>,
    pub camera_id: Option<String>,
    pub detector: Option<String>,
    pub model: Option<String>,
    pub input_size: Option<usize>,
    pub conf: Option<f32>,
    pub detect_fps: Option<f64>,
    pub vlm_base_url: Option<String>,
    pub vlm_model: Option<String>,
    pub vlm_timeout: Option<u64>,
    pub vlm_coords: Option<String>,
    pub targets: Option<String>,
    pub depth_provider: Option<String>,
    pub depth_base_url: Option<String>,
    pub depth_model: Option<String>,
    pub depth_timeout: Option<u64>,
    pub preview_url: Option<String>,
}

/// The environment layer. Same two variables item-web's ask bar reads, so one
/// shell setup can drive both sides.
#[derive(Default, Clone)]
pub struct EnvOverrides {
    pub internal_token: Option<String>,
    pub annotation_token: Option<String>,
    pub vlm_base_url: Option<String>,
    pub vlm_model: Option<String>,
    pub depth_base_url: Option<String>,
    pub depth_model: Option<String>,
}

impl EnvOverrides {
    pub fn from_env() -> Self {
        Self {
            internal_token: std::env::var("ITEM_DAEMON_TOKEN").ok(),
            annotation_token: std::env::var("ITEM_DAEMON_ANNOTATION_TOKEN").ok(),
            vlm_base_url: std::env::var("ITEM_VLM_BASE_URL").ok(),
            vlm_model: std::env::var("ITEM_VLM_MODEL").ok(),
            // ITEM_VLM_* is a compatibility fallback for the depth backend;
            // depth_spec_for only consumes it when relative-vlm is enabled.
            depth_base_url: std::env::var("ITEM_DEPTH_BASE_URL")
                .ok()
                .or_else(|| std::env::var("ITEM_VLM_BASE_URL").ok()),
            depth_model: std::env::var("ITEM_DEPTH_MODEL")
                .ok()
                .or_else(|| std::env::var("ITEM_VLM_MODEL").ok()),
        }
    }
}

/// The resolved settings: pure data, safe to log, safe to unit-test.
#[derive(Clone, PartialEq)]
pub struct RuntimeConfig {
    pub db: String,
    pub snapshots_dir: String,
    pub snapshots: SnapshotMode,
    pub health_file: String,
    /// The appeared/disappeared timeline path; empty string = off (§6).
    pub events_file: String,
    /// 0 = do not reload config.toml at runtime (phase 2).
    pub rescan_config_secs: u64,
    pub checkpoint_secs: u64,
    /// 0 = keep observations forever (retention off).
    pub retention_days: u64,
    /// How often to prune; 0 = only on demand.
    pub sweep_secs: u64,
    /// How often to look for orphan snapshot files; 0 = never.
    pub reconcile_secs: u64,
    pub internal_listen: Option<String>,
    pub internal_token: Option<String>,
    pub internal_max_asset_bytes: u64,
    pub frame_cache_ttl_ms: u64,
    pub frame_asset_ttl_secs: u64,
    pub annotations_enabled: bool,
    pub annotation_retention_days: u64,
    pub annotation_max_sets: u64,
    pub annotation_max_revisions: u64,
    pub annotation_max_metadata_bytes: u64,
    pub annotation_max_evidence_bytes: u64,
    pub annotation_token: Option<String>,
    pub webhook_enabled: bool,
    pub listen: String,
    /// `None` = the preview endpoint stays off.
    pub preview_url: Option<String>,
    pub camera_id: String,
    pub detector: String,
    /// `None` = derive from [`RuntimeConfig::detect_fps`].
    pub detect_fps: Option<f64>,
    pub targets: String,
    pub depth_provider: String,
    pub depth_base_url: Option<String>,
    pub depth_model: Option<String>,
    pub depth_timeout: u64,
    depth_provider_cli: bool,
    depth_base_url_cli: bool,
    depth_model_cli: bool,
    depth_timeout_cli: bool,
    depth_base_url_env: bool,
    depth_model_env: bool,
    pub model: String,
    pub input_size: usize,
    pub conf: f32,
    pub vlm_base_url: Option<String>,
    pub vlm_model: Option<String>,
    pub vlm_timeout: u64,
    pub vlm_coords: String,
}

// Redacted Debug is deliberate: resolved config is often printed in diagnostics.
impl std::fmt::Debug for RuntimeConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RuntimeConfig { redacted }")
    }
}
impl std::fmt::Debug for DaemonConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DaemonConfig { redacted }")
    }
}
impl std::fmt::Debug for EnvOverrides {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("EnvOverrides { redacted }")
    }
}

impl RuntimeConfig {
    /// Validate provider names and values before any database or camera work.
    pub fn validate(&self, file: Option<&Config>) -> Result<(), String> {
        if let Some(listen) = &self.internal_listen {
            let addr: std::net::SocketAddr = listen
                .parse()
                .map_err(|_| "internal_listen must be a valid socket address".to_string())?;
            if !addr.ip().is_loopback() {
                return Err("internal_listen must bind to a loopback address".into());
            }
            if self.internal_token.as_deref().is_none_or(|token| {
                token.trim().is_empty() || !token.bytes().all(|b| (0x21..=0x7e).contains(&b))
            }) {
                return Err("ITEM_DAEMON_TOKEN (or internal_token) is required when internal_listen is enabled and must contain visible ASCII".into());
            }
        }
        if self.internal_max_asset_bytes == 0
            || self.internal_max_asset_bytes > crate::assets::DEFAULT_MAX_ASSET_BYTES
        {
            return Err("internal_max_asset_bytes must be between 1 and 8388608".into());
        }
        if !(1..=5000).contains(&self.frame_cache_ttl_ms) {
            return Err("frame_cache_ttl_ms must be between 1 and 5000".into());
        }
        if !(1..=86400).contains(&self.frame_asset_ttl_secs) {
            return Err("frame_asset_ttl_secs must be between 1 and 86400".into());
        }
        if !(1..=365).contains(&self.annotation_retention_days) {
            return Err("annotation_retention_days must be between 1 and 365".into());
        }
        if self.annotation_max_sets == 0
            || self.annotation_max_revisions == 0
            || self.annotation_max_metadata_bytes == 0
            || self.annotation_max_evidence_bytes == 0
        {
            return Err("annotation quotas must be positive".into());
        }
        let valid_token = |token: Option<&String>| {
            token.is_some_and(|s| !s.is_empty() && s.bytes().all(|b| (0x21..=0x7e).contains(&b)))
        };
        if self.annotation_token.is_some() && !valid_token(self.annotation_token.as_ref()) {
            return Err("ITEM_DAEMON_ANNOTATION_TOKEN must contain visible ASCII".into());
        }
        if self.annotation_token.is_some() && self.annotation_token == self.internal_token {
            return Err("annotation token must differ from ITEM_DAEMON_TOKEN".into());
        }
        if self.annotations_enabled {
            if self.internal_listen.is_none() || !valid_token(self.annotation_token.as_ref()) {
                return Err(
                    "annotations_enabled requires internal_listen and ITEM_DAEMON_ANNOTATION_TOKEN"
                        .into(),
                );
            }
            if self.annotation_token == self.internal_token {
                return Err("annotation token must differ from ITEM_DAEMON_TOKEN".into());
            }
        }
        if self.webhook_enabled {
            self.listen
                .parse::<std::net::SocketAddr>()
                .map_err(|_| "bad webhook listen address".to_string())?;
        }
        self.depth_spec_for(None).map_err(|e| e.to_string())?;
        if let Some(file) = file {
            let mut ids = std::collections::HashSet::new();
            for camera in &file.camera {
                if camera.id.is_empty()
                    || camera.id.len() > 1024
                    || camera.id == "."
                    || camera.id == ".."
                    || camera.id.chars().any(|c| c.is_control())
                {
                    return Err(format!("camera '{}' has an invalid id", camera.id));
                }
                if !ids.insert(camera.id.as_str()) {
                    return Err(format!("duplicate camera id '{}'", camera.id));
                }
                self.depth_spec_for(Some(camera))
                    .map_err(|e| format!("camera '{}': {e}", camera.id))?;
            }
        }
        Ok(())
    }

    /// Merge the four layers. See the module doc for the precedence.
    ///
    /// `camera_id` deliberately does **not** come from config.toml: it is the
    /// identity the historical single-camera CLI paths attribute frames to, and
    /// changing it under an existing command line would rewrite where
    /// observations land. A `[[camera]]` row is consulted only once the id is
    /// known, to pick up that camera's `detector`/`detect_fps`/`targets`.
    pub fn resolve(
        file: Option<&Config>,
        cli: &CliOverrides,
        env: &EnvOverrides,
    ) -> Result<Self, ConfigError> {
        let daemon = file.map(|c| &c.daemon);
        let webhook = file.map(|c| &c.webhook);
        let row = cli
            .camera_id
            .as_deref()
            .and_then(|id| file.and_then(|c| c.camera.iter().find(|cam| cam.id == id)));
        // CLI values are global explicit overrides: a camera row must not undo
        // them. Environment is only the fallback layer for depth endpoints.
        let depth_provider_cli = cli.depth_provider.is_some();
        let depth_base_url_cli = cli.depth_base_url.is_some();
        let depth_model_cli = cli.depth_model.is_some();
        let depth_timeout_cli = cli.depth_timeout.is_some();
        let depth_provider_for_env = cli
            .depth_provider
            .as_deref()
            .or_else(|| row.and_then(|camera| camera.depth_provider.as_deref()))
            .unwrap_or(defaults::DEPTH_PROVIDER);
        let depth_env_base_url = env.depth_base_url.clone().or_else(|| {
            (depth_provider_for_env == "relative-vlm").then(|| env.vlm_base_url.clone())?
        });
        let depth_env_model = env.depth_model.clone().or_else(|| {
            (depth_provider_for_env == "relative-vlm").then(|| env.vlm_model.clone())?
        });
        let depth_base_url_env = depth_env_base_url.is_some();
        let depth_model_env = depth_env_model.is_some();

        Ok(Self {
            db: take(&cli.db, daemon.and_then(|d| d.db.as_deref()), defaults::DB),
            snapshots_dir: take(
                &cli.snapshots_dir,
                daemon.and_then(|d| d.snapshots_dir.as_deref()),
                defaults::SNAPSHOTS_DIR,
            ),
            snapshots: {
                let raw = cli
                    .snapshots
                    .as_deref()
                    .or_else(|| daemon.and_then(|d| d.snapshots.as_deref()))
                    .unwrap_or("first");
                SnapshotMode::parse(raw)?
            },
            health_file: daemon
                .and_then(|d| d.health_file.clone())
                .unwrap_or_else(|| defaults::HEALTH_FILE.to_string()),
            events_file: daemon
                .and_then(|d| d.events_file.clone())
                .unwrap_or_else(|| defaults::EVENTS_FILE.to_string()),
            rescan_config_secs: daemon.and_then(|d| d.rescan_config_secs).unwrap_or(0),
            checkpoint_secs: daemon
                .and_then(|d| d.checkpoint_secs)
                .unwrap_or(defaults::CHECKPOINT_SECS),
            retention_days: daemon
                .and_then(|d| d.retention_days)
                .unwrap_or(defaults::RETENTION_DAYS),
            sweep_secs: daemon
                .and_then(|d| d.sweep_secs)
                .unwrap_or(defaults::SWEEP_SECS),
            reconcile_secs: daemon
                .and_then(|d| d.reconcile_secs)
                .unwrap_or(defaults::RECONCILE_SECS),
            internal_listen: daemon
                .and_then(|d| d.internal_listen.clone())
                .filter(|value| !value.trim().is_empty()),
            internal_token: env
                .internal_token
                .clone()
                .or_else(|| daemon.and_then(|d| d.internal_token.clone()))
                .filter(|value| !value.is_empty()),
            internal_max_asset_bytes: daemon
                .and_then(|d| d.internal_max_asset_bytes)
                .unwrap_or(crate::assets::DEFAULT_MAX_ASSET_BYTES),
            frame_cache_ttl_ms: daemon.and_then(|d| d.frame_cache_ttl_ms).unwrap_or(5000),
            frame_asset_ttl_secs: daemon.and_then(|d| d.frame_asset_ttl_secs).unwrap_or(600),
            annotations_enabled: daemon.and_then(|d| d.annotations_enabled).unwrap_or(false),
            annotation_retention_days: daemon
                .and_then(|d| d.annotation_retention_days)
                .unwrap_or(defaults::ANNOTATION_RETENTION_DAYS),
            annotation_max_sets: daemon
                .and_then(|d| d.annotation_max_sets)
                .unwrap_or(defaults::ANNOTATION_MAX_SETS),
            annotation_max_revisions: daemon
                .and_then(|d| d.annotation_max_revisions)
                .unwrap_or(defaults::ANNOTATION_MAX_REVISIONS),
            annotation_max_metadata_bytes: daemon
                .and_then(|d| d.annotation_max_metadata_bytes)
                .unwrap_or(defaults::ANNOTATION_MAX_METADATA_BYTES),
            annotation_max_evidence_bytes: daemon
                .and_then(|d| d.annotation_max_evidence_bytes)
                .unwrap_or(defaults::ANNOTATION_MAX_EVIDENCE_BYTES),
            annotation_token: env
                .annotation_token
                .clone()
                .filter(|value| !value.is_empty()),
            webhook_enabled: webhook.and_then(|w| w.enabled).unwrap_or(true),
            listen: take(
                &cli.listen,
                webhook.and_then(|w| w.listen.as_deref()),
                defaults::LISTEN,
            ),
            // An empty string means "off", whether it came from TOML or a flag.
            preview_url: cli
                .preview_url
                .clone()
                .or_else(|| webhook.and_then(|w| w.preview_url.clone()))
                .filter(|u| !u.is_empty()),
            camera_id: cli
                .camera_id
                .clone()
                .unwrap_or_else(|| defaults::CAMERA_ID.to_string()),
            detector: take(
                &cli.detector,
                row.and_then(|c| c.detector.as_deref()),
                defaults::DETECTOR,
            ),
            detect_fps: cli.detect_fps.or_else(|| row.and_then(|c| c.detect_fps)),
            targets: take(
                &cli.targets,
                row.and_then(|c| c.targets.as_deref()),
                default_targets(),
            ),
            depth_provider: take(
                &cli.depth_provider,
                row.and_then(|c| c.depth_provider.as_deref()),
                defaults::DEPTH_PROVIDER,
            ),
            depth_base_url: cli
                .depth_base_url
                .clone()
                .or_else(|| depth_env_base_url.clone())
                .or_else(|| row.and_then(|c| c.depth_base_url.clone())),
            depth_model: cli
                .depth_model
                .clone()
                .or_else(|| depth_env_model.clone())
                .or_else(|| row.and_then(|c| c.depth_model.clone())),
            depth_timeout: cli
                .depth_timeout
                .or_else(|| row.and_then(|c| c.depth_timeout))
                .unwrap_or(defaults::DEPTH_TIMEOUT_SECS),
            depth_provider_cli,
            depth_base_url_cli,
            depth_model_cli,
            depth_timeout_cli,
            depth_base_url_env,
            depth_model_env,
            model: cli
                .model
                .clone()
                .unwrap_or_else(|| defaults::MODEL.to_string()),
            input_size: cli.input_size.unwrap_or(defaults::INPUT_SIZE),
            conf: cli.conf.unwrap_or(defaults::CONF),
            vlm_base_url: cli
                .vlm_base_url
                .clone()
                .or_else(|| env.vlm_base_url.clone()),
            vlm_model: cli.vlm_model.clone().or_else(|| env.vlm_model.clone()),
            vlm_timeout: cli.vlm_timeout.unwrap_or(defaults::VLM_TIMEOUT_SECS),
            vlm_coords: cli
                .vlm_coords
                .clone()
                .unwrap_or_else(|| defaults::VLM_COORDS.to_string()),
        })
    }

    /// How often detection runs: an explicit value wins; otherwise 0.2 for the
    /// VLM sidecar (one HTTP round trip per detection) and 1.0 for everything
    /// else. This is the historical `effective_detect_fps` rule, moved into the
    /// resolved config so it can be unit-tested.
    pub fn detect_fps(&self) -> f64 {
        self.detect_fps_for(None)
    }

    /// The same rule, but a `[[camera]]` row may override both the detector
    /// and the rate -- so a row that says `vlm` gets the VLM default even when
    /// the global default is `yolo`.
    pub fn detect_fps_for(&self, row: Option<&CameraConfig>) -> f64 {
        let name = row
            .and_then(|c| c.detector.as_deref())
            .unwrap_or(self.detector.as_str());
        row.and_then(|c| c.detect_fps)
            .or(self.detect_fps)
            .unwrap_or(if name == "vlm" {
                defaults::DETECT_FPS_VLM
            } else {
                defaults::DETECT_FPS_LOCAL
            })
    }

    /// The detector spec for one camera. A `[[camera]]` row may override the
    /// resolved `detector` and `targets`; the model path and the VLM endpoint
    /// come from the merged settings.
    pub fn detector_spec_for(&self, row: Option<&CameraConfig>) -> DetectorSpec {
        let name = row
            .and_then(|c| c.detector.as_deref())
            .unwrap_or(self.detector.as_str());
        let targets = row
            .and_then(|c| c.targets.as_deref())
            .unwrap_or(self.targets.as_str());

        match name {
            "vlm" => DetectorSpec::Vlm {
                base_url: self.vlm_base_url.clone().unwrap_or_default(),
                model: self.vlm_model.clone().unwrap_or_default(),
                targets: Self::vlm_targets(targets),
                timeout: Duration::from_secs(self.vlm_timeout),
                coords: self.vlm_coords.clone(),
            },
            _ => DetectorSpec::Yolo {
                model: PathBuf::from(&self.model),
                labels: crate::detector::COCO_LABELS
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
                input_size: self.input_size,
                conf: self.conf,
            },
        }
    }

    pub fn depth_spec_for(
        &self,
        row: Option<&CameraConfig>,
    ) -> std::result::Result<crate::runtime::DepthSpec, String> {
        let provider = if self.depth_provider_cli {
            self.depth_provider.as_str()
        } else {
            row.and_then(|c| c.depth_provider.as_deref())
                .unwrap_or(self.depth_provider.as_str())
        };
        match provider {
            "none" | "" => Ok(crate::runtime::DepthSpec::None),
            "relative-vlm" => {
                // CLI > environment > camera file > default. The resolved
                // values already include CLI/env layers; only consult a row
                // when neither explicit layer supplied a value.
                let base_url = if self.depth_base_url_cli || self.depth_base_url_env {
                    self.depth_base_url.clone()
                } else {
                    row.and_then(|c| c.depth_base_url.clone())
                        .or_else(|| self.depth_base_url.clone())
                }
                .unwrap_or_default();
                let model = if self.depth_model_cli || self.depth_model_env {
                    self.depth_model.clone()
                } else {
                    row.and_then(|c| c.depth_model.clone())
                        .or_else(|| self.depth_model.clone())
                }
                .unwrap_or_default();
                let timeout = if self.depth_timeout_cli {
                    self.depth_timeout
                } else {
                    row.and_then(|c| c.depth_timeout)
                        .unwrap_or(self.depth_timeout)
                };
                if timeout == 0 {
                    return Err("depth timeout must be greater than zero".into());
                }
                if base_url.trim().is_empty() {
                    return Err("relative-vlm depth requires a non-empty base URL".into());
                }
                if model.trim().is_empty() {
                    return Err("relative-vlm depth requires a non-empty model".into());
                }
                Ok(crate::runtime::DepthSpec::RelativeVlm {
                    base_url,
                    model,
                    timeout: Duration::from_secs(timeout),
                })
            }
            other => Err(format!(
                "unknown depth provider '{other}' (none|relative-vlm)"
            )),
        }
    }

    #[cfg(feature = "vlm")]
    fn vlm_targets(spec: &str) -> Vec<String> {
        crate::detector::vlm::parse_targets(spec)
    }

    /// No VLM backend in this build: the vocabulary is inert, and
    /// `runtime::build_detector` is where the missing feature gets reported.
    #[cfg(not(feature = "vlm"))]
    fn vlm_targets(_spec: &str) -> Vec<String> {
        Vec::new()
    }

    /// Make every path absolute, resolving relative paths against the **config
    /// file's directory** rather than the process cwd (design/resident-ingest.md
    /// §3). A service manager picks the cwd, so a daemon that trusted it would
    /// write snapshots where `item-web` cannot find them.
    ///
    /// `model` counts as one of these paths: it was cwd-relative at first, and
    /// running the daemon from a foreign cwd made every camera die at startup
    /// with "model load: File ... does not exist". The config's directory is the
    /// deployment root, so a shared model needs an absolute path.
    pub fn absolutize(&mut self, base: &Path) {
        self.db = absolute(base, &self.db);
        self.snapshots_dir = absolute(base, &self.snapshots_dir);
        self.health_file = absolute(base, &self.health_file);
        // An empty events_file means "off" and must stay empty: absolutizing it
        // would turn the off switch into the config directory.
        if !self.events_file.is_empty() {
            self.events_file = absolute(base, &self.events_file);
        }
        self.model = absolute(base, &self.model);
    }

    /// Where the event timeline goes for a camera, or `None` when events are
    /// off (`events_file = ""`). Same "empty string means off" rule as
    /// `preview_url`.
    pub fn events_path(&self) -> Option<PathBuf> {
        (!self.events_file.is_empty()).then(|| PathBuf::from(&self.events_file))
    }

    /// The single-instance lock lives next to the database: one writer per
    /// database, whatever the daemon happens to be called (§1).
    pub fn lock_path(&self) -> PathBuf {
        match Path::new(&self.db).parent() {
            Some(dir) if !dir.as_os_str().is_empty() => dir.join("ingest.lock"),
            _ => PathBuf::from("ingest.lock"),
        }
    }

    /// The retention window, or `None` when retention is off
    /// (`retention_days = 0`).
    pub fn retention_window(&self) -> Option<Duration> {
        (self.retention_days > 0)
            .then(|| Duration::from_secs(self.retention_days.saturating_mul(24 * 3600)))
    }
}

/// Absolute form of `path`, resolved against `base` when it is relative.
///
/// Lexical only (`std::path::absolute`): the target usually does not exist yet,
/// so `canonicalize` would fail, and resolving symlinks is not what callers
/// want here anyway.
fn absolute(base: &Path, path: &str) -> String {
    let path = Path::new(path);
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    std::path::absolute(&joined)
        .unwrap_or(joined)
        .to_string_lossy()
        .into_owned()
}

/// CLI value, else file value, else the built-in default.
fn take(cli: &Option<String>, file: Option<&str>, default: &str) -> String {
    cli.clone()
        .or_else(|| file.map(str::to_string))
        .unwrap_or_else(|| default.to_string())
}

#[cfg(feature = "vlm")]
fn default_targets() -> &'static str {
    crate::detector::vlm::DEFAULT_TARGETS
}

#[cfg(not(feature = "vlm"))]
fn default_targets() -> &'static str {
    ""
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Config {
        toml::from_str(text).expect("test config parses")
    }

    fn cli() -> CliOverrides {
        CliOverrides::default()
    }

    #[test]
    fn parses_and_seeds() {
        let text = r#"
[[camera]]
id = "living"
url = "rtsp://x@host/path"
[camera.regions]
desk = [100.0, 100.0, 700.0, 400.0]
"#;
        let cfg: Config = toml::from_str(text).unwrap();
        assert_eq!(cfg.camera.len(), 1);
        assert_eq!(cfg.camera[0].url.as_deref(), Some("rtsp://x@host/path"));
        let store = item_core::store::Store::in_memory().unwrap();
        assert_eq!(cfg.seed_regions(&store).unwrap(), 1);
        assert_eq!(
            store.zone_for_point("living", (200.0, 200.0)).unwrap(),
            "desk"
        );
    }

    #[test]
    fn an_empty_config_reproduces_the_historical_cli_defaults() {
        let cfg = RuntimeConfig::resolve(None, &cli(), &EnvOverrides::default()).unwrap();
        assert_eq!(cfg.db, "data/items.db");
        assert_eq!(cfg.snapshots_dir, "data/snapshots");
        assert_eq!(cfg.health_file, "data/health.json");
        assert_eq!(cfg.listen, "127.0.0.1:8477");
        assert_eq!(cfg.camera_id, "rtsp-0");
        assert_eq!(cfg.detector, "yolo");
        assert_eq!(cfg.detect_fps, None);
        assert_eq!(cfg.detect_fps(), 1.0);
        assert_eq!(cfg.model, "models/yolov8n.onnx");
        assert_eq!(cfg.input_size, 640);
        assert_eq!(cfg.conf, 0.3);
        assert_eq!(cfg.vlm_timeout, 60);
        assert_eq!(cfg.vlm_coords, "norm1000");
        assert!(cfg.webhook_enabled);
        assert_eq!(cfg.preview_url, None);
        assert_eq!(cfg.vlm_base_url, None);
        assert_eq!(cfg.checkpoint_secs, 300);
        assert_eq!(cfg.retention_days, 90);
        assert_eq!(
            cfg.retention_window(),
            Some(Duration::from_secs(90 * 24 * 3600)),
            "the default window is the one the roadmap needs (§7)"
        );
        assert_eq!(cfg.sweep_secs, 3600);
        assert_eq!(cfg.reconcile_secs, 7 * 24 * 3600);
        assert_eq!(cfg.events_file, "data/events.jsonl");
        assert_eq!(
            cfg.events_path(),
            Some(PathBuf::from("data/events.jsonl")),
            "the timeline is on by default (§6)"
        );
    }

    #[test]
    fn an_empty_events_file_turns_the_timeline_off() {
        let file = parse(
            r#"
[daemon]
events_file = ""
"#,
        );
        let cfg = RuntimeConfig::resolve(Some(&file), &cli(), &EnvOverrides::default()).unwrap();
        assert_eq!(
            cfg.events_path(),
            None,
            "empty string means off, like preview_url"
        );

        let mut cfg = cfg;
        cfg.absolutize(Path::new("/base"));
        assert_eq!(
            cfg.events_file, "",
            "absolutizing must not turn the off switch into a path"
        );
    }

    #[test]
    fn a_relative_events_file_is_resolved_against_the_config_directory() {
        let file = parse(
            r#"
[daemon]
events_file = "logs/events.jsonl"
"#,
        );
        let mut cfg =
            RuntimeConfig::resolve(Some(&file), &cli(), &EnvOverrides::default()).unwrap();
        cfg.absolutize(Path::new("/base"));
        assert!(
            cfg.events_path().unwrap().is_absolute(),
            "a service manager picks the cwd, so this must not stay relative"
        );
        assert!(cfg.events_file.ends_with("events.jsonl"));
    }

    #[test]
    fn file_beats_defaults() {
        let file = parse(
            r#"
[daemon]
db = "from-file.db"
snapshots_dir = "from-file-snaps"
health_file = "from-file-health.json"
checkpoint_secs = 42
retention_days = 30
sweep_secs = 60
reconcile_secs = 120

[webhook]
listen = "0.0.0.0:9000"
enabled = false

[[camera]]
id = "living"
detector = "vlm"
detect_fps = 0.5
targets = "keys,wallet"
"#,
        );
        let cfg = RuntimeConfig::resolve(Some(&file), &cli(), &EnvOverrides::default()).unwrap();
        assert_eq!(cfg.db, "from-file.db");
        assert_eq!(cfg.snapshots_dir, "from-file-snaps");
        assert_eq!(cfg.health_file, "from-file-health.json");
        assert_eq!(cfg.checkpoint_secs, 42);
        assert_eq!(cfg.retention_days, 30);
        assert_eq!(
            cfg.retention_window(),
            Some(Duration::from_secs(30 * 24 * 3600))
        );
        assert_eq!(cfg.sweep_secs, 60);
        assert_eq!(cfg.reconcile_secs, 120);
        assert_eq!(cfg.listen, "0.0.0.0:9000");
        assert!(!cfg.webhook_enabled);
        assert_eq!(
            cfg.detector, "yolo",
            "no --camera-id: no camera row matches"
        );
    }

    #[test]
    fn zero_retention_days_means_keep_everything() {
        let file = parse(
            r#"
[daemon]
retention_days = 0
sweep_secs = 0
reconcile_secs = 0
"#,
        );
        let cfg = RuntimeConfig::resolve(Some(&file), &cli(), &EnvOverrides::default()).unwrap();
        assert_eq!(cfg.retention_window(), None);
        assert_eq!(cfg.sweep_secs, 0, "0 is a value, not 'unset'");
        assert_eq!(cfg.reconcile_secs, 0);
    }

    #[test]
    fn cli_beats_file() {
        let file = parse(
            r#"
[daemon]
db = "from-file.db"
snapshots_dir = "from-file-snaps"

[webhook]
listen = "0.0.0.0:9000"

[[camera]]
id = "living"
detector = "vlm"
detect_fps = 0.5
"#,
        );
        let cli = CliOverrides {
            db: Some("from-cli.db".into()),
            snapshots_dir: Some("from-cli-snaps".into()),
            listen: Some("127.0.0.1:1234".into()),
            camera_id: Some("living".into()),
            detector: Some("yolo".into()),
            detect_fps: Some(3.0),
            ..Default::default()
        };
        let cfg = RuntimeConfig::resolve(Some(&file), &cli, &EnvOverrides::default()).unwrap();
        assert_eq!(cfg.db, "from-cli.db");
        assert_eq!(cfg.snapshots_dir, "from-cli-snaps");
        assert_eq!(cfg.listen, "127.0.0.1:1234");
        assert_eq!(cfg.detector, "yolo");
        assert_eq!(cfg.detect_fps(), 3.0);
    }

    #[test]
    fn a_matching_camera_row_supplies_what_the_cli_left_silent() {
        let file = parse(
            r#"
[[camera]]
id = "living"
detector = "vlm"
detect_fps = 0.5
targets = "keys,wallet"

[[camera]]
id = "garage"
detector = "yolo"
"#,
        );
        let cli = CliOverrides {
            camera_id: Some("living".into()),
            ..Default::default()
        };
        let cfg = RuntimeConfig::resolve(Some(&file), &cli, &EnvOverrides::default()).unwrap();
        assert_eq!(cfg.camera_id, "living");
        assert_eq!(cfg.detector, "vlm");
        assert_eq!(cfg.detect_fps(), 0.5);
        assert_eq!(cfg.targets, "keys,wallet");

        // An id that matches nothing must not silently borrow another camera.
        let cli = CliOverrides {
            camera_id: Some("nope".into()),
            ..Default::default()
        };
        let cfg = RuntimeConfig::resolve(Some(&file), &cli, &EnvOverrides::default()).unwrap();
        assert_eq!(cfg.detector, "yolo");
        assert_eq!(cfg.detect_fps(), 1.0);
    }

    #[test]
    fn camera_id_never_comes_from_the_file() {
        let file = parse(
            r#"
[[camera]]
id = "living"
"#,
        );
        let cfg = RuntimeConfig::resolve(Some(&file), &cli(), &EnvOverrides::default()).unwrap();
        assert_eq!(
            cfg.camera_id, "rtsp-0",
            "identity must stay stable for an unchanged command line"
        );
    }

    #[test]
    fn a_camera_row_can_name_a_network_camera_or_a_local_one() {
        let file = parse(
            r#"
[[camera]]
id = "desk"
webcam = 0

[[camera]]
id = "yard"
url = "rtsp://cam.local/stream"

[[camera]]
id = "both"
url = "rtsp://cam.local/other"
webcam = 1

[[camera]]
id = "disabled"
enabled = false
webcam = 2

[[camera]]
id = "fed-by-frigate"
"#,
        );
        let cfg = RuntimeConfig::resolve(Some(&file), &cli(), &EnvOverrides::default()).unwrap();
        let tasks = file.camera_tasks(&cfg);

        let by_id = |id: &str| {
            tasks
                .iter()
                .find(|t| t.camera_id == id)
                .unwrap_or_else(|| panic!("no task for {id}"))
        };
        assert_eq!(
            by_id("desk").source,
            Some(SourceSpec::Webcam { index: 0 }),
            "a webcam index drives a local camera"
        );
        assert_eq!(
            by_id("yard").source,
            Some(SourceSpec::Rtsp {
                url: "rtsp://cam.local/stream".into()
            })
        );
        assert_eq!(
            by_id("both").source,
            Some(SourceSpec::Rtsp {
                url: "rtsp://cam.local/other".into()
            }),
            "url wins when both are set: a network camera is the portable choice"
        );
        assert_eq!(by_id("fed-by-frigate").source, None);
        assert!(
            !tasks.iter().any(|t| t.camera_id == "disabled"),
            "enabled = false keeps a camera out of the daemon"
        );
        assert_eq!(tasks.len(), 4);
    }

    #[test]
    fn internal_token_env_precedence_validation_and_debug_are_safe() {
        let file = parse(
            r#"
[daemon]
internal_listen = "127.0.0.1:8480"
internal_token = "file-secret"
"#,
        );
        let env = EnvOverrides {
            internal_token: Some("env-secret".into()),
            ..Default::default()
        };
        let settings = RuntimeConfig::resolve(Some(&file), &cli(), &env).unwrap();
        assert_eq!(settings.internal_token.as_deref(), Some("env-secret"));
        assert!(settings.validate(Some(&file)).is_ok());
        for text in [
            format!("{file:?}"),
            format!("{settings:?}"),
            format!("{env:?}"),
        ] {
            assert!(!text.contains("file-secret"));
            assert!(!text.contains("env-secret"));
        }
        for invalid in ["", "   ", "bad\r\ntoken", "spaced token"] {
            let env = EnvOverrides {
                internal_token: Some(invalid.into()),
                ..Default::default()
            };
            let settings = RuntimeConfig::resolve(Some(&file), &cli(), &env).unwrap();
            assert!(settings.validate(Some(&file)).is_err());
        }
    }

    #[test]
    fn annotations_are_default_off_and_validate_separate_credentials() {
        let defaults = RuntimeConfig::resolve(None, &cli(), &EnvOverrides::default()).unwrap();
        assert!(!defaults.annotations_enabled);
        assert_eq!(defaults.annotation_retention_days, 30);
        assert_eq!(defaults.annotation_max_sets, 10_000);
        assert_eq!(defaults.annotation_max_revisions, 50_000);
        assert_eq!(defaults.annotation_max_metadata_bytes, 536_870_912);
        assert_eq!(defaults.annotation_max_evidence_bytes, 2_147_483_648);
        let file = parse(
            r#"
[daemon]
annotations_enabled = true
internal_listen = "127.0.0.1:8480"
internal_token = "read-secret"
"#,
        );
        for token in [
            None,
            Some("read-secret"),
            Some("bad token"),
            Some("bad\nsecret"),
        ] {
            let env = EnvOverrides {
                annotation_token: token.map(str::to_string),
                ..Default::default()
            };
            let settings = RuntimeConfig::resolve(Some(&file), &cli(), &env).unwrap();
            assert!(settings.validate(Some(&file)).is_err());
        }
        let env = EnvOverrides {
            annotation_token: Some("write-secret".into()),
            ..Default::default()
        };
        let mut settings = RuntimeConfig::resolve(Some(&file), &cli(), &env).unwrap();
        assert!(settings.validate(Some(&file)).is_ok());
        assert!(!format!("{settings:?}{env:?}{file:?}").contains("secret"));
        settings.internal_listen = None;
        assert!(settings.validate(None).is_err());
        settings.internal_listen = Some("127.0.0.1:8480".into());
        for days in [0, 366] {
            settings.annotation_retention_days = days;
            assert!(settings.validate(None).is_err());
        }
        settings.annotation_retention_days = 30;
        settings.annotation_max_evidence_bytes = 0;
        assert!(settings.validate(None).is_err());
    }

    #[test]
    fn env_sits_between_file_and_cli() {
        let env = EnvOverrides {
            internal_token: None,
            annotation_token: None,
            vlm_base_url: Some("http://from-env/v1".into()),
            vlm_model: Some("from-env-model".into()),
            depth_base_url: None,
            depth_model: None,
        };
        let cfg = RuntimeConfig::resolve(None, &cli(), &env).unwrap();
        assert_eq!(cfg.vlm_base_url.as_deref(), Some("http://from-env/v1"));
        assert_eq!(cfg.vlm_model.as_deref(), Some("from-env-model"));

        let cli = CliOverrides {
            vlm_base_url: Some("http://from-cli/v1".into()),
            vlm_model: Some("from-cli-model".into()),
            ..Default::default()
        };
        let cfg = RuntimeConfig::resolve(None, &cli, &env).unwrap();
        assert_eq!(cfg.vlm_base_url.as_deref(), Some("http://from-cli/v1"));
        assert_eq!(cfg.vlm_model.as_deref(), Some("from-cli-model"));
    }

    #[test]
    fn detect_fps_falls_back_on_the_detector_choice() {
        let cli = CliOverrides {
            detector: Some("vlm".into()),
            ..Default::default()
        };
        let cfg = RuntimeConfig::resolve(None, &cli, &EnvOverrides::default()).unwrap();
        assert_eq!(cfg.detect_fps(), 0.2);

        let cli = CliOverrides {
            detector: Some("yolo".into()),
            ..Default::default()
        };
        let cfg = RuntimeConfig::resolve(None, &cli, &EnvOverrides::default()).unwrap();
        assert_eq!(cfg.detect_fps(), 1.0);
    }

    #[test]
    fn depth_cli_beats_environment_and_camera_file() {
        let file = parse(
            r#"
[[camera]]
id = "cam"
depth_provider = "relative-vlm"
depth_base_url = "http://file/v1"
depth_model = "file-model"
depth_timeout = 10
"#,
        );
        let cli = CliOverrides {
            camera_id: Some("cam".into()),
            depth_provider: Some("relative-vlm".into()),
            depth_base_url: Some("http://cli/v1".into()),
            depth_model: Some("cli-model".into()),
            depth_timeout: Some(7),
            ..Default::default()
        };
        let env = EnvOverrides {
            internal_token: None,
            depth_base_url: Some("http://env/v1".into()),
            depth_model: Some("env-model".into()),
            ..Default::default()
        };
        let settings = RuntimeConfig::resolve(Some(&file), &cli, &env).unwrap();
        assert!(matches!(
            settings.depth_spec_for(Some(&file.camera[0])).unwrap(),
            crate::runtime::DepthSpec::RelativeVlm { base_url, model, timeout }
                if base_url == "http://cli/v1" && model == "cli-model" && timeout == Duration::from_secs(7)
        ));
    }

    #[test]
    fn unknown_depth_provider_and_zero_timeout_fail_validation() {
        let file = parse(
            r#"
[[camera]]
id = "cam"
depth_provider = "made-up"
"#,
        );
        let settings =
            RuntimeConfig::resolve(Some(&file), &cli(), &EnvOverrides::default()).unwrap();
        assert!(settings.validate(Some(&file)).is_err());
        let file = parse(
            r#"
[[camera]]
id = "cam"
depth_provider = "relative-vlm"
depth_base_url = "http://127.0.0.1/v1"
depth_model = "m"
depth_timeout = 0
"#,
        );
        let settings =
            RuntimeConfig::resolve(Some(&file), &cli(), &EnvOverrides::default()).unwrap();
        assert!(settings.validate(Some(&file)).is_err());
    }

    #[test]
    fn an_empty_preview_url_means_off() {
        let file = parse(
            r#"
[webhook]
preview_url = ""
"#,
        );
        let cfg = RuntimeConfig::resolve(Some(&file), &cli(), &EnvOverrides::default()).unwrap();
        assert_eq!(cfg.preview_url, None);

        let file = parse(
            r#"
[webhook]
preview_url = "rtsp://cam/stream"
"#,
        );
        let cfg = RuntimeConfig::resolve(Some(&file), &cli(), &EnvOverrides::default()).unwrap();
        assert_eq!(cfg.preview_url.as_deref(), Some("rtsp://cam/stream"));
    }
}
