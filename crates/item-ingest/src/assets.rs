//! Immutable daemon-owned JPEGs, durable per-asset manifests and a private API.
//! IDs are opaque: clients must not construct or interpret them. No snapshot fallback.

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{Json, Path as AxumPath, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::Response;
use axum::routing::{get, post};
use image::ImageDecoder;
use item_core::Observation;
use item_core::assets::{
    AssetErrorBody, CameraFrameMetadata, CameraFrameRequest, RawAssetMetadata,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;

use crate::SharedStore;

pub const RAW_KIND: &str = "raw";
pub const RAW_MIME: &str = "image/jpeg";
pub const DEFAULT_MAX_ASSET_BYTES: u64 = 8 * 1024 * 1024;
const MAX_MANIFEST_BYTES: u64 = 16 * 1024;
const MAX_DIMENSION: u32 = 16384;
const MAX_PIXELS: u64 = 64_000_000;
const MAX_DIRECTORY_ENTRIES: usize = 100_000;
const MAX_READERS: usize = 4;
const MAX_CAMERA_ASSETS: usize = 128;
const MAX_CAMERA_ASSET_BYTES: u64 = 256 * 1024 * 1024;
// Only publication/unlink uses this lock, after JPEG work and DB transactions.
// It serializes the tiny commit window with retention (not HTTP reads).
pub(crate) static RAW_LIFECYCLE: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[derive(Clone)]
pub struct AssetStore {
    root: Arc<PathBuf>,
    annotation_root: Arc<PathBuf>,
    max_bytes: u64,
    database_id: Arc<str>,
    token: Option<Arc<str>>,
    db: SharedStore,
    readers: Arc<Semaphore>,
    camera_ttl: Duration,
    frame_cache: Option<crate::frame_cache::FrameCache>,
    camera_status: Option<Arc<CameraStatus>>,
}

#[derive(Clone)]
pub struct CameraRegistration {
    pub camera_id: String,
    pub enabled: bool,
    pub source_kind: String,
}

struct CameraStatus {
    cameras: Vec<CameraRegistration>,
    health: crate::health::HealthRegistry,
    started_at: String,
    stop: Arc<std::sync::atomic::AtomicBool>,
}

// Neither filesystem paths nor bearer credentials belong in diagnostics.
impl std::fmt::Debug for AssetStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AssetStore { redacted }")
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema: u32,
    label: String,
    metadata: RawAssetMetadata,
}

impl AssetStore {
    pub fn new(
        root: impl AsRef<Path>,
        max_bytes: u64,
        token: Option<String>,
        db: SharedStore,
    ) -> io::Result<Self> {
        Self::new_with_ttl(root, max_bytes, token, db, Duration::from_secs(600))
    }

    pub fn new_with_ttl(
        root: impl AsRef<Path>,
        max_bytes: u64,
        token: Option<String>,
        db: SharedStore,
        camera_ttl: Duration,
    ) -> io::Result<Self> {
        Self::new_with_ttls(
            root,
            max_bytes,
            token,
            db,
            camera_ttl,
            camera_ttl.min(Duration::from_secs(5)),
        )
    }

    pub fn new_with_ttls(
        root: impl AsRef<Path>,
        max_bytes: u64,
        token: Option<String>,
        db: SharedStore,
        camera_ttl: Duration,
        cache_ttl: Duration,
    ) -> io::Result<Self> {
        if max_bytes == 0
            || max_bytes > DEFAULT_MAX_ASSET_BYTES
            || camera_ttl.is_zero()
            || camera_ttl > Duration::from_secs(86400)
            || cache_ttl.is_zero()
            || cache_ttl > Duration::from_secs(5)
        {
            return Err(io::Error::other("invalid raw asset limits"));
        }
        let database_id = db
            .lock()
            .map_err(|_| io::Error::other("DB lock poisoned"))?
            .ensure_database_identity()
            .map_err(io::Error::other)?;
        std::fs::create_dir_all(root.as_ref())?;
        let canonical = std::fs::canonicalize(root)?;
        // Keep existing raw-only stores free of annotation side effects.
        // AnnotationRuntime creates this directory explicitly on startup.
        let annotation_root = canonical.join("annotation-evidence");
        {
            let _commit = RAW_LIFECYCLE
                .lock()
                .map_err(|_| io::Error::other("raw lifecycle lock poisoned"))?;
            for (index, entry) in std::fs::read_dir(&canonical)?.enumerate() {
                if index >= MAX_DIRECTORY_ENTRIES {
                    return Err(failure_io(AssetFailure::Busy));
                }
                let entry = entry?;
                let name = entry.file_name();
                let Some(name) = name.to_str() else { continue };
                if (raw_temp_name(name)
                    || name
                        .strip_suffix(".json")
                        .or_else(|| name.strip_suffix(".jpg"))
                        .and_then(frame_expiry)
                        .is_some_and(|deadline| deadline <= chrono::Utc::now().timestamp_millis()))
                    && entry.file_type()?.is_file()
                    && let Err(error) = std::fs::remove_file(entry.path())
                {
                    tracing::warn!(%error, "raw crash/expired camera asset cleanup deferred");
                }
            }
        }
        Ok(Self {
            root: Arc::new(canonical),
            annotation_root: Arc::new(annotation_root),
            max_bytes,
            database_id: Arc::from(database_id),
            token: token.filter(|s| !s.trim().is_empty()).map(Arc::from),
            db,
            readers: Arc::new(Semaphore::new(MAX_READERS)),
            camera_ttl,
            frame_cache: Some(crate::frame_cache::FrameCache::new(cache_ttl)),
            camera_status: None,
        })
    }

    pub(crate) fn initialize_annotation_directory(&self) -> io::Result<()> {
        self.check_root().map_err(failure_io)?;
        std::fs::create_dir_all(self.annotation_root.as_ref())?;
        if !std::fs::symlink_metadata(self.annotation_root.as_ref())?
            .file_type()
            .is_dir()
        {
            return Err(failure_io(AssetFailure::InvalidPath));
        }
        self.check_annotation_root().map_err(failure_io)
    }
    pub(crate) fn annotation_directory(&self) -> &Path {
        &self.annotation_root
    }
    pub(crate) fn database_id(&self) -> &str {
        &self.database_id
    }
    pub(crate) fn shared_store(&self) -> SharedStore {
        self.db.clone()
    }

    /// Tiny acceptance check under RAW_LIFECYCLE then the existing Store guard.
    /// No JPEG decode or filesystem writes occur here.
    pub(crate) fn accept_annotation_source(
        &self,
        metadata: &RawAssetMetadata,
        store: &item_core::store::Store,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<(), AssetFailure> {
        if metadata.database_id != self.database_id.as_ref() {
            return Err(AssetFailure::Changed);
        }
        if metadata.provenance == "daemon_camera_frame" {
            if frame_expiry(&metadata.asset_id).is_none_or(|ms| ms <= now.timestamp_millis()) {
                return Err(AssetFailure::Missing);
            }
        } else if metadata.provenance == "daemon_raw_asset" {
            let id = metadata.observation_id.ok_or(AssetFailure::Changed)?;
            let owner = store
                .observation_by_id(id)
                .map_err(|_| AssetFailure::Io)?
                .ok_or(AssetFailure::Missing)?;
            let manifest = self.manifest(&metadata.asset_id)?;
            if !belongs_to(&manifest, &owner, &self.database_id) {
                return Err(AssetFailure::Missing);
            }
        } else {
            return Err(AssetFailure::Changed);
        }
        Ok(())
    }

    pub fn with_camera_status(
        mut self,
        cameras: Vec<CameraRegistration>,
        health: crate::health::HealthRegistry,
        stop: Arc<std::sync::atomic::AtomicBool>,
    ) -> Self {
        self.camera_status = Some(Arc::new(CameraStatus {
            cameras,
            health,
            started_at: chrono::Utc::now().to_rfc3339(),
            stop,
        }));
        self
    }

    pub fn cameras(&self) -> Result<item_core::cameras::CamerasResponse, AssetFailure> {
        let status = self.camera_status.as_ref().ok_or(AssetFailure::Busy)?;
        let mut cameras = Vec::new();
        for camera in &status.cameras {
            let health = status.health.get(&camera.camera_id);
            let cache = self
                .frame_cache
                .as_ref()
                .and_then(|c| c.status(&camera.camera_id));
            let supported = camera.enabled && camera.source_kind != "webhook";
            let state = if !camera.enabled {
                "disabled".into()
            } else if !supported {
                "webhook_only".into()
            } else {
                cache
                    .as_ref()
                    .map(|s| s.0.clone())
                    .unwrap_or_else(|| "starting".into())
            };
            cameras.push(item_core::cameras::CameraInfo {
                camera_id: camera.camera_id.clone(),
                enabled: camera.enabled,
                source_kind: camera.source_kind.clone(),
                state,
                frame_supported: supported,
                frame_available: supported && cache.as_ref().is_some_and(|s| s.2),
                cached_frame_age_ms: cache.and_then(|s| s.1),
                frames: health.as_ref().map_or(0, |h| h.frames),
                detections: health.as_ref().map_or(0, |h| h.detections),
                recorded: health.as_ref().map_or(0, |h| h.recorded),
                reconnects: health.as_ref().map_or(0, |h| h.reconnects),
                has_error: health.is_some_and(|h| h.last_error.is_some()),
            });
        }
        cameras.sort_by(|a, b| a.camera_id.cmp(&b.camera_id));
        Ok(item_core::cameras::CamerasResponse {
            database_id: self.database_id.to_string(),
            observed_at: chrono::Utc::now().to_rfc3339(),
            cameras,
        })
    }

    pub fn health(&self) -> Result<item_core::cameras::DaemonHealthResponse, AssetFailure> {
        let status = self.camera_status.as_ref().ok_or(AssetFailure::Busy)?;
        let cameras = self.cameras()?;
        self.check_root()?;
        let mut count = 0;
        let mut bytes = 0u64;
        for (index, entry) in std::fs::read_dir(self.root.as_ref())
            .map_err(|_| AssetFailure::Io)?
            .enumerate()
        {
            if index >= MAX_DIRECTORY_ENTRIES {
                return Err(AssetFailure::Busy);
            }
            let entry = entry.map_err(|_| AssetFailure::Io)?;
            let name = entry.file_name();
            if let Some(id) = name.to_str().and_then(|s| s.strip_suffix(".jpg"))
                && frame_asset_id(id)
                && entry.file_type().map_err(|_| AssetFailure::Io)?.is_file()
            {
                count += 1;
                bytes = bytes.saturating_add(entry.metadata().map_err(|_| AssetFailure::Io)?.len());
            }
        }
        Ok(item_core::cameras::DaemonHealthResponse {
            database_id: cameras.database_id,
            observed_at: cameras.observed_at,
            started_at: status.started_at.clone(),
            daemon_state: if status.stop.load(std::sync::atomic::Ordering::SeqCst) {
                "stopping"
            } else {
                "running"
            }
            .into(),
            cameras: cameras.cameras,
            cache_bytes: self
                .frame_cache
                .as_ref()
                .map_or(0, |c| c.cache_bytes() as u64),
            cache_max_bytes: crate::frame_cache::MAX_TOTAL_BYTES as u64,
            cache_ttl_ms: self
                .frame_cache
                .as_ref()
                .map_or(0, |c| c.ttl().as_millis() as u64),
            frame_asset_count: count,
            frame_asset_bytes: bytes,
            frame_asset_max_count: MAX_CAMERA_ASSETS,
            frame_asset_max_bytes: MAX_CAMERA_ASSET_BYTES,
        })
    }

    pub fn with_frame_cache(mut self, cache: crate::frame_cache::FrameCache) -> Self {
        self.frame_cache = Some(cache);
        self
    }

    pub fn frame_cache(&self) -> Option<crate::frame_cache::FrameCache> {
        self.frame_cache.clone()
    }

    /// Idle maintenance removes only expired camera-frame files. Observation
    /// assets remain under the existing DB-driven retention policy.
    pub fn purge_expired_camera_assets(&self) -> io::Result<usize> {
        if let Some(cache) = &self.frame_cache {
            cache.purge_expired();
        }
        self.check_root().map_err(failure_io)?;
        let now = chrono::Utc::now().timestamp_millis();
        let mut removed = 0;
        let _guard = RAW_LIFECYCLE
            .lock()
            .map_err(|_| io::Error::other("raw lifecycle lock poisoned"))?;
        for (index, entry) in std::fs::read_dir(self.root.as_ref())?.enumerate() {
            if index >= MAX_DIRECTORY_ENTRIES {
                return Err(failure_io(AssetFailure::Busy));
            }
            let entry = entry?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Some(asset_id) = name
                .strip_suffix(".json")
                .or_else(|| name.strip_suffix(".jpg"))
            else {
                continue;
            };
            if frame_expiry(asset_id).is_some_and(|deadline| deadline <= now)
                && std::fs::remove_file(entry.path()).is_ok()
            {
                removed += 1;
            }
        }
        if let Some(cache) = &self.frame_cache {
            cache.purge_expired();
        }
        Ok(removed)
    }

    pub fn freeze_camera_frame(
        &self,
        camera_id: &str,
        frame: &crate::frame_cache::FrameSnapshot,
    ) -> Result<RawAssetMetadata, AssetFailure> {
        let mut guard = frame.asset.lock().map_err(|_| AssetFailure::Io)?;
        if let Some(metadata) = guard.as_ref() {
            if frame_expiry(&metadata.asset_id)
                .is_none_or(|deadline| deadline <= chrono::Utc::now().timestamp_millis())
            {
                return Err(AssetFailure::Missing);
            }
            return Ok(metadata.clone());
        }
        let metadata = self
            .publish_camera_rgb(
                camera_id,
                frame.captured_at,
                frame.rgb(),
                frame.width,
                frame.height,
            )
            .map_err(map_io)?;
        *guard = Some(metadata.clone());
        Ok(metadata)
    }

    /// `owner` is obtained from the sighting transaction, without reacquiring its
    /// DB mutex. Publication has no mutable index and never replaces either file.
    pub fn publish_camera_rgb(
        &self,
        camera_id: &str,
        captured_at: chrono::DateTime<chrono::Utc>,
        rgb: &[u8],
        width: u32,
        height: u32,
    ) -> io::Result<RawAssetMetadata> {
        dimensions(width, height).map_err(failure_io)?;
        if rgb.len() as u64 != u64::from(width) * u64::from(height) * 3 {
            return Err(io::Error::other("invalid RGB frame buffer"));
        }
        let mut bytes = LimitedWriter {
            bytes: Vec::new(),
            limit: self.max_bytes as usize,
        };
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, 85)
            .encode(rgb, width, height, image::ExtendedColorType::Rgb8)
            .map_err(|error| match error {
                image::ImageError::IoError(error) => error,
                other => io::Error::other(other),
            })?;
        self.publish_camera_jpeg(camera_id, captured_at, &bytes.bytes)
    }

    pub fn publish_camera_jpeg(
        &self,
        camera_id: &str,
        captured_at: chrono::DateTime<chrono::Utc>,
        bytes: &[u8],
    ) -> io::Result<RawAssetMetadata> {
        if camera_id.is_empty()
            || camera_id.len() > 1024
            || camera_id == "."
            || camera_id == ".."
            || camera_id.chars().any(|c| c.is_control())
        {
            return Err(io::Error::other("invalid camera id"));
        }
        if bytes.len() as u64 > self.max_bytes {
            return Err(failure_io(AssetFailure::TooLarge));
        }
        let (width, height) = jpeg_dimensions(bytes).map_err(failure_io)?;
        let sha256 = hex_digest(bytes);
        let expires =
            captured_at + chrono::Duration::from_std(self.camera_ttl).map_err(io::Error::other)?;
        let expires_at = expires.to_rfc3339();
        let owner_hash = hex_digest(
            &serde_json::to_vec(&(
                self.database_id.as_ref(),
                camera_id,
                captured_at.to_rfc3339(),
                &expires_at,
            ))
            .unwrap(),
        );
        let expiry_ms = expires.timestamp_millis();
        let asset_id = format!("frame-v1-{expiry_ms}-{owner_hash}-{sha256}");
        let metadata = RawAssetMetadata {
            asset_id: asset_id.clone(),
            database_id: self.database_id.to_string(),
            kind: RAW_KIND.into(),
            mime_type: RAW_MIME.into(),
            bytes: bytes.len() as u64,
            sha256: sha256.clone(),
            width: Some(width),
            height: Some(height),
            capture_time: Some(captured_at.to_rfc3339()),
            observation_id: None,
            camera_id: Some(camera_id.to_string()),
            zone: None,
            status: "available".into(),
            provenance: "daemon_camera_frame".into(),
            expires_at: Some(expires_at),
        };
        let manifest = serde_json::to_vec(&Manifest {
            schema: 1,
            label: String::new(),
            metadata: metadata.clone(),
        })?;
        if manifest.len() as u64 > MAX_MANIFEST_BYTES {
            return Err(io::Error::other("raw asset manifest too large"));
        }
        let _commit = RAW_LIFECYCLE
            .lock()
            .map_err(|_| io::Error::other("raw lifecycle lock poisoned"))?;
        self.check_root().map_err(failure_io)?;
        if expires <= chrono::Utc::now() {
            return Err(failure_io(AssetFailure::Missing));
        }
        let mut count = 0usize;
        let mut total_bytes = 0u64;
        let mut existing = false;
        for (index, entry) in std::fs::read_dir(self.root.as_ref())?.enumerate() {
            if index >= MAX_DIRECTORY_ENTRIES {
                return Err(failure_io(AssetFailure::Busy));
            }
            let entry = entry?;
            let name = entry.file_name();
            let Some(id) = name.to_str().and_then(|s| s.strip_suffix(".jpg")) else {
                continue;
            };
            if !frame_asset_id(id) {
                continue;
            }
            let file_type = entry.file_type()?;
            if !file_type.is_file() {
                return Err(failure_io(AssetFailure::InvalidPath));
            }
            if frame_expiry(id)
                .is_some_and(|deadline| deadline <= chrono::Utc::now().timestamp_millis())
                && std::fs::remove_file(entry.path()).is_ok()
            {
                let _ = std::fs::remove_file(self.root.join(format!("{id}.json")));
                continue;
            }
            count += 1;
            total_bytes = total_bytes.saturating_add(entry.metadata()?.len());
            existing |= id == asset_id;
        }
        if !existing
            && (count >= MAX_CAMERA_ASSETS
                || total_bytes.saturating_add(bytes.len() as u64) > MAX_CAMERA_ASSET_BYTES)
        {
            return Err(failure_io(AssetFailure::Busy));
        }
        self.publish_file(&format!("{asset_id}.jpg"), bytes)?;
        self.publish_file(&format!("{asset_id}.json"), &manifest)?;
        Ok(metadata)
    }

    pub fn publish_rgb(
        &self,
        owner: &Observation,
        captured_at: chrono::DateTime<chrono::Utc>,
        rgb: &[u8],
        width: u32,
        height: u32,
    ) -> io::Result<RawAssetMetadata> {
        dimensions(width, height).map_err(failure_io)?;
        if rgb.len() as u64 != u64::from(width) * u64::from(height) * 3 {
            return Err(io::Error::other("invalid RGB frame buffer"));
        }
        let mut bytes = LimitedWriter {
            bytes: Vec::new(),
            limit: self.max_bytes as usize,
        };
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, 85)
            .encode(rgb, width, height, image::ExtendedColorType::Rgb8)
            .map_err(|error| match error {
                image::ImageError::IoError(error) => error,
                other => io::Error::other(other),
            })?;
        self.publish_jpeg(owner, captured_at, &bytes.bytes)
    }

    /// Publish an immutable annotation evidence copy. Evidence lives in a
    /// private subdirectory so raw-frame/observation cleanup can never claim it.
    /// The source metadata is copied, but its expiry is not reused as evidence
    /// expiry; the caller supplies the annotation retention deadline.
    pub fn publish_annotation_evidence(
        &self,
        source: &RawAssetMetadata,
        bytes: &[u8],
        expires_at: &str,
    ) -> Result<RawAssetMetadata, AssetFailure> {
        if source.database_id != self.database_id.as_ref()
            || source.kind != RAW_KIND
            || source.mime_type != RAW_MIME
            || source.camera_id.is_none()
            || bytes.len() as u64 > self.max_bytes
        {
            return Err(AssetFailure::Changed);
        }
        let (width, height) = jpeg_dimensions(bytes)?;
        let sha256 = hex_digest(bytes);
        if source.sha256 != sha256 || source.width != Some(width) || source.height != Some(height) {
            return Err(AssetFailure::Changed);
        }
        let expiry = chrono::DateTime::parse_from_rfc3339(expires_at)
            .map_err(|_| AssetFailure::Changed)?
            .with_timezone(&chrono::Utc);
        if expiry <= chrono::Utc::now() {
            return Err(AssetFailure::Missing);
        }
        static EVIDENCE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nonce = format!(
            "{}-{}-{}-{}",
            std::process::id(),
            EVIDENCE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default(),
            sha256
        );
        let evidence_id = format!("evidence-v1-{}", hex_digest(nonce.as_bytes()));
        let metadata = RawAssetMetadata {
            asset_id: evidence_id.clone(),
            database_id: self.database_id.to_string(),
            kind: RAW_KIND.into(),
            mime_type: RAW_MIME.into(),
            bytes: bytes.len() as u64,
            sha256: sha256.clone(),
            width: Some(width),
            height: Some(height),
            capture_time: source.capture_time.clone(),
            observation_id: None,
            camera_id: source.camera_id.clone(),
            zone: None,
            status: "available".into(),
            provenance: "daemon_annotation_evidence".into(),
            expires_at: Some(expires_at.to_owned()),
        };
        let manifest = serde_json::to_vec(&Manifest {
            schema: 1,
            label: String::new(),
            metadata: metadata.clone(),
        })
        .map_err(|_| AssetFailure::Io)?;
        if manifest.len() as u64 > MAX_MANIFEST_BYTES {
            return Err(AssetFailure::TooLarge);
        }
        // The annotation operation mutex protects this directory from its
        // cleanup. Do not hold the source lifecycle or Store lock over disk IO.
        self.check_annotation_root()?;
        self.publish_file_at(&self.annotation_root, &format!("{evidence_id}.jpg"), bytes)
            .map_err(map_io)?;
        self.publish_file_at(
            &self.annotation_root,
            &format!("{evidence_id}.json"),
            &manifest,
        )
        .map_err(map_io)?;
        Ok(metadata)
    }

    pub fn publish_jpeg(
        &self,
        owner: &Observation,
        captured_at: chrono::DateTime<chrono::Utc>,
        bytes: &[u8],
    ) -> io::Result<RawAssetMetadata> {
        if owner.id <= 0 || captured_at < owner.first_seen || captured_at > owner.last_seen {
            return Err(io::Error::other("invalid raw asset ownership"));
        }
        if bytes.len() as u64 > self.max_bytes {
            return Err(failure_io(AssetFailure::TooLarge));
        }
        let (width, height) = jpeg_dimensions(bytes).map_err(failure_io)?;
        let sha256 = hex_digest(bytes);
        let capture_time = captured_at.to_rfc3339();
        let owner_hash = owner_digest(
            &self.database_id,
            owner.id,
            &owner.camera_id,
            &owner.zone,
            &owner.label,
            &capture_time,
        );
        let asset_id = format!("raw-v1-{}-{owner_hash}-{sha256}", owner.id);
        let metadata = RawAssetMetadata {
            asset_id: asset_id.clone(),
            database_id: self.database_id.to_string(),
            kind: RAW_KIND.into(),
            mime_type: RAW_MIME.into(),
            bytes: bytes.len() as u64,
            sha256,
            width: Some(width),
            height: Some(height),
            capture_time: Some(capture_time),
            observation_id: Some(owner.id),
            camera_id: Some(owner.camera_id.clone()),
            zone: Some(owner.zone.clone()),
            status: "available".into(),
            provenance: "daemon_raw_asset".into(),
            expires_at: None,
        };
        let manifest = serde_json::to_vec(&Manifest {
            schema: 1,
            label: owner.label.clone(),
            metadata: metadata.clone(),
        })?;
        if manifest.len() as u64 > MAX_MANIFEST_BYTES {
            return Err(io::Error::other("raw asset manifest too large"));
        }
        let _commit = RAW_LIFECYCLE
            .lock()
            .map_err(|_| io::Error::other("raw lifecycle lock poisoned"))?;
        let current = self.owner(owner.id).map_err(failure_io)?;
        if !belongs_to(
            &Manifest {
                schema: 1,
                label: owner.label.clone(),
                metadata: metadata.clone(),
            },
            &current,
            &self.database_id,
        ) {
            return Err(io::Error::other("raw observation expired during encoding"));
        }
        // A fully written JPEG becomes visible first; only a complete manifest
        // publishes its identity. Crash leftovers are never served without it.
        self.publish_file(&format!("{asset_id}.jpg"), bytes)?;
        self.publish_file(&format!("{asset_id}.json"), &manifest)?;
        Ok(metadata)
    }

    fn publish_file(&self, name: &str, bytes: &[u8]) -> io::Result<()> {
        self.publish_file_at(&self.root, name, bytes)
    }

    fn publish_file_at(&self, root: &Path, name: &str, bytes: &[u8]) -> io::Result<()> {
        let final_path = root.join(name);
        if final_path.exists() {
            let existing =
                bounded_read(root, &final_path, bytes.len() as u64).map_err(failure_io)?;
            return if existing == bytes {
                Ok(())
            } else {
                Err(io::Error::other("immutable raw asset already exists"))
            };
        }
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let temp = root.join(format!(
            ".raw-{}-{}.tmp",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let result = (|| {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp)?;
            file.write_all(bytes)?;
            file.sync_all()?;
            drop(file);
            // Unlike rename on Unix, hard_link is atomic and fails if the target
            // exists. The temporary file and final file are on the same volume.
            match std::fs::hard_link(&temp, &final_path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    let existing =
                        bounded_read(root, &final_path, bytes.len() as u64).map_err(failure_io)?;
                    if existing == bytes {
                        Ok(())
                    } else {
                        Err(io::Error::other("immutable raw asset conflict"))
                    }
                }
                Err(e) => Err(e),
            }
        })();
        let _ = std::fs::remove_file(temp);
        result
    }

    fn check_root(&self) -> Result<(), AssetFailure> {
        if std::fs::canonicalize(self.root.as_ref()).map_err(|_| AssetFailure::InvalidPath)?
            != *self.root
        {
            return Err(AssetFailure::InvalidPath);
        }
        Ok(())
    }

    fn check_annotation_root(&self) -> Result<(), AssetFailure> {
        self.check_root()?;
        if std::fs::canonicalize(self.annotation_root.as_ref())
            .map_err(|_| AssetFailure::InvalidPath)?
            != *self.annotation_root
        {
            return Err(AssetFailure::InvalidPath);
        }
        Ok(())
    }

    /// Read an annotation evidence asset after validating its independent
    /// manifest and JPEG hash. This path never consults source/frame expiry.
    pub fn annotation_evidence_content(
        &self,
        asset_id: &str,
    ) -> Result<(RawAssetMetadata, Vec<u8>), AssetFailure> {
        if !annotation_asset_id(asset_id) {
            return Err(AssetFailure::NotFound);
        }
        self.check_annotation_root()?;
        let manifest_bytes = bounded_read(
            &self.annotation_root,
            &self.annotation_root.join(format!("{asset_id}.json")),
            MAX_MANIFEST_BYTES,
        )?;
        let manifest: Manifest =
            serde_json::from_slice(&manifest_bytes).map_err(|_| AssetFailure::Changed)?;
        validate_annotation_manifest(&manifest, asset_id, self.database_id.as_ref())?;
        let mut metadata = manifest.metadata;
        // Evidence lifetime is DB-owned and may extend when another live set
        // shares this generation. Never trust a stale filesystem expiry.
        if let Some(db) = Some(self.db.clone()) {
            let guard = db.lock().map_err(|_| AssetFailure::Io)?;
            let Some((_registration, expiry)) = guard
                .annotation_evidence_by_id(asset_id, chrono::Utc::now())
                .map_err(|_| AssetFailure::Io)?
            else {
                return Err(AssetFailure::Missing);
            };
            metadata.expires_at = Some(expiry);
        }
        let bytes = bounded_read(
            &self.annotation_root,
            &self.annotation_root.join(format!("{asset_id}.jpg")),
            self.max_bytes,
        )?;
        if bytes.len() as u64 != metadata.bytes || hex_digest(&bytes) != metadata.sha256 {
            return Err(AssetFailure::Changed);
        }
        let (width, height) = jpeg_dimensions(&bytes)?;
        if metadata.width != Some(width) || metadata.height != Some(height) {
            return Err(AssetFailure::Changed);
        }
        Ok((metadata, bytes))
    }

    fn read_file(&self, path: &Path, limit: u64) -> Result<Vec<u8>, AssetFailure> {
        self.check_root()?;
        bounded_read(&self.root, path, limit)
    }

    fn manifest(&self, asset_id: &str) -> Result<Manifest, AssetFailure> {
        let observation_id = asset_observation_id(asset_id);
        let frame = frame_asset_id(asset_id);
        if observation_id.is_none() && !frame {
            return Err(AssetFailure::NotFound);
        }
        if frame
            && frame_expiry(asset_id)
                .is_some_and(|deadline| deadline <= chrono::Utc::now().timestamp_millis())
        {
            return Err(AssetFailure::Missing);
        }
        let bytes = self
            .read_file(
                &self.root.join(format!("{asset_id}.json")),
                MAX_MANIFEST_BYTES,
            )
            .map_err(|e| {
                if e == AssetFailure::Missing && frame {
                    AssetFailure::Missing
                } else if e == AssetFailure::Missing {
                    AssetFailure::NotFound
                } else {
                    e
                }
            })?;
        let manifest: Manifest =
            serde_json::from_slice(&bytes).map_err(|_| AssetFailure::Changed)?;
        if let Some(id) = observation_id {
            validate_manifest(&manifest, asset_id, id)?;
        } else {
            validate_camera_manifest(&manifest, asset_id, &self.database_id)?;
        }
        Ok(manifest)
    }

    fn owner(&self, id: i64) -> Result<Observation, AssetFailure> {
        if id <= 0 {
            return Err(AssetFailure::NotFound);
        }
        self.db
            .lock()
            .map_err(|_| AssetFailure::Io)?
            .observation_by_id(id)
            .map_err(|_| AssetFailure::Io)?
            .ok_or(AssetFailure::NotFound)
    }

    pub fn metadata(&self, asset_id: &str) -> Result<RawAssetMetadata, AssetFailure> {
        self.content(asset_id).map(|(metadata, _)| metadata)
    }

    pub fn content(&self, asset_id: &str) -> Result<(RawAssetMetadata, Vec<u8>), AssetFailure> {
        if annotation_asset_id(asset_id) {
            return self.annotation_evidence_content(asset_id);
        }
        let manifest = self.manifest(asset_id)?;
        if manifest.metadata.provenance == "daemon_camera_frame" {
            let metadata = manifest.metadata;
            if metadata
                .expires_at
                .as_deref()
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .is_some_and(|t| t <= chrono::Utc::now())
            {
                return Err(AssetFailure::Missing);
            }
            let bytes =
                self.read_file(&self.root.join(format!("{asset_id}.jpg")), self.max_bytes)?;
            if bytes.len() as u64 != metadata.bytes || hex_digest(&bytes) != metadata.sha256 {
                return Err(AssetFailure::Changed);
            }
            let (width, height) = jpeg_dimensions(&bytes)?;
            if metadata.width != Some(width) || metadata.height != Some(height) {
                return Err(AssetFailure::Changed);
            }
            if frame_expiry(asset_id)
                .is_none_or(|deadline| deadline <= chrono::Utc::now().timestamp_millis())
            {
                return Err(AssetFailure::Missing);
            }
            return Ok((metadata, bytes));
        }
        let owner = self
            .owner(manifest.metadata.observation_id.unwrap())
            .map_err(|e| {
                if e == AssetFailure::NotFound {
                    AssetFailure::Missing
                } else {
                    e
                }
            })?;
        if !belongs_to(&manifest, &owner, &self.database_id) {
            return Err(AssetFailure::Missing);
        }
        let metadata = manifest.metadata;
        let bytes = self.read_file(&self.root.join(format!("{asset_id}.jpg")), self.max_bytes)?;
        if bytes.len() as u64 != metadata.bytes || hex_digest(&bytes) != metadata.sha256 {
            return Err(AssetFailure::Changed);
        }
        let (width, height) = jpeg_dimensions(&bytes)?;
        if metadata.width != Some(width) || metadata.height != Some(height) {
            return Err(AssetFailure::Changed);
        }
        // No file IO or JPEG decode while holding the database mutex. Recheck
        // after reading to avoid returning an asset pruned during the operation.
        if !belongs_to(
            &Manifest {
                schema: manifest.schema,
                label: manifest.label,
                metadata: metadata.clone(),
            },
            &self.owner(owner.id).map_err(|_| AssetFailure::Missing)?,
            &self.database_id,
        ) {
            return Err(AssetFailure::Missing);
        }
        Ok((metadata, bytes))
    }

    pub fn observation_metadata(&self, id: i64) -> Result<RawAssetMetadata, AssetFailure> {
        let owner = self.owner(id)?;
        self.check_root()?;
        let prefix = format!("raw-v1-{id}-");
        let mut found: Option<String> = None;
        for (count, entry) in std::fs::read_dir(self.root.as_ref())
            .map_err(|_| AssetFailure::Io)?
            .enumerate()
        {
            if count >= MAX_DIRECTORY_ENTRIES {
                return Err(AssetFailure::Busy);
            }
            let entry = entry.map_err(|_| AssetFailure::Io)?;
            let name = entry.file_name();
            let Some(asset_id) = name.to_str().and_then(|s| s.strip_suffix(".json")) else {
                continue;
            };
            if !asset_id.starts_with(&prefix) {
                continue;
            }
            let manifest = self.manifest(asset_id)?;
            if belongs_to(&manifest, &owner, &self.database_id)
                && found.as_deref().is_none_or(|current| asset_id < current)
            {
                found = Some(asset_id.to_string());
            }
        }
        match found.as_deref() {
            Some(id) => self.metadata(id),
            None => Err(AssetFailure::Unavailable),
        }
    }

    pub(crate) fn authorized(&self, headers: &HeaderMap) -> bool {
        let Some(token) = self.token.as_deref() else {
            return false;
        };
        if headers.get_all(header::AUTHORIZATION).iter().count() != 1 {
            return false;
        }
        let Some(candidate) = headers
            .get(header::AUTHORIZATION)
            .and_then(|h| h.to_str().ok())
            .and_then(|s| s.strip_prefix("Bearer "))
        else {
            return false;
        };
        // Fixed-size digest comparison avoids a token-prefix timing oracle.
        let a = Sha256::digest(candidate.as_bytes());
        let b = Sha256::digest(token.as_bytes());
        a.iter().zip(b.iter()).fold(0u8, |d, (a, b)| d | (a ^ b)) == 0
    }
}

fn belongs_to(manifest: &Manifest, owner: &Observation, database_id: &str) -> bool {
    let m = &manifest.metadata;
    let capture = m
        .capture_time
        .as_deref()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok());
    m.database_id == database_id
        && m.observation_id == Some(owner.id)
        && m.camera_id.as_deref() == Some(&owner.camera_id)
        && m.zone.as_deref() == Some(&owner.zone)
        && manifest.label == owner.label
        && capture.is_some_and(|t| t >= owner.first_seen && t <= owner.last_seen)
}

fn validate_manifest(manifest: &Manifest, asset_id: &str, id: i64) -> Result<(), AssetFailure> {
    let m = &manifest.metadata;
    let (Some(camera), Some(zone), Some(capture), Some(width), Some(height)) =
        (&m.camera_id, &m.zone, &m.capture_time, m.width, m.height)
    else {
        return Err(AssetFailure::Changed);
    };
    dimensions(width, height)?;
    if manifest.schema != 1
        || m.database_id.len() != 64
        || !m
            .database_id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        || m.asset_id != asset_id
        || m.observation_id != Some(id)
        || m.kind != RAW_KIND
        || m.mime_type != RAW_MIME
        || m.status != "available"
        || m.provenance != "daemon_raw_asset"
        || m.expires_at.is_some()
        || m.bytes == 0
        || m.bytes > DEFAULT_MAX_ASSET_BYTES
        || chrono::DateTime::parse_from_rfc3339(capture).is_err()
        || asset_id
            != format!(
                "raw-v1-{id}-{}-{}",
                owner_digest(&m.database_id, id, camera, zone, &manifest.label, capture),
                m.sha256
            )
    {
        return Err(AssetFailure::Changed);
    }
    Ok(())
}

pub(crate) fn annotation_asset_id_public(id: &str) -> bool {
    annotation_asset_id(id)
}

fn annotation_asset_id(id: &str) -> bool {
    let Some(rest) = id.strip_prefix("evidence-v1-") else {
        return false;
    };
    (16..=128).contains(&rest.len()) && rest.bytes().all(|b| b.is_ascii_hexdigit())
}

fn validate_annotation_manifest(
    manifest: &Manifest,
    asset_id: &str,
    database_id: &str,
) -> Result<(), AssetFailure> {
    let m = &manifest.metadata;
    let (Some(camera), Some(_capture), Some(expiry), Some(width), Some(height)) = (
        &m.camera_id,
        &m.capture_time,
        &m.expires_at,
        m.width,
        m.height,
    ) else {
        return Err(AssetFailure::Changed);
    };
    let _expiry =
        chrono::DateTime::parse_from_rfc3339(expiry).map_err(|_| AssetFailure::Changed)?;
    if manifest.schema != 1
        || !manifest.label.is_empty()
        || m.asset_id != asset_id
        || m.database_id != database_id
        || m.kind != RAW_KIND
        || m.mime_type != RAW_MIME
        || m.status != "available"
        || m.provenance != "daemon_annotation_evidence"
        || m.observation_id.is_some()
        || m.zone.is_some()
        || camera.is_empty()
        || camera.len() > 1024
        || camera == "."
        || camera == ".."
        || camera.chars().any(char::is_control)
        || m.bytes == 0
        || m.bytes > DEFAULT_MAX_ASSET_BYTES
    {
        return Err(AssetFailure::Changed);
    }
    dimensions(width, height)
}

fn frame_expiry(id: &str) -> Option<i64> {
    if !frame_asset_id(id) {
        return None;
    }
    id.strip_prefix("frame-v1-")?
        .split('-')
        .next()?
        .parse()
        .ok()
}

fn frame_asset_id(id: &str) -> bool {
    let Some(rest) = id.strip_prefix("frame-v1-") else {
        return false;
    };
    let mut parts = rest.split('-');
    parts
        .next()
        .is_some_and(|s| s.parse::<i64>().is_ok_and(|n| n > 0 && n.to_string() == s))
        && parts
            .next()
            .is_some_and(|s| s.len() == 64 && s.bytes().all(|c| c.is_ascii_hexdigit()))
        && parts
            .next()
            .is_some_and(|s| s.len() == 64 && s.bytes().all(|c| c.is_ascii_hexdigit()))
        && parts.next().is_none()
}

fn validate_camera_manifest(
    manifest: &Manifest,
    asset_id: &str,
    database_id: &str,
) -> Result<(), AssetFailure> {
    let m = &manifest.metadata;
    let (Some(camera), Some(capture), Some(expiry), Some(width), Some(height)) = (
        &m.camera_id,
        &m.capture_time,
        &m.expires_at,
        m.width,
        m.height,
    ) else {
        return Err(AssetFailure::Changed);
    };
    dimensions(width, height)?;
    let expires =
        chrono::DateTime::parse_from_rfc3339(expiry).map_err(|_| AssetFailure::Changed)?;
    let capture_dt =
        chrono::DateTime::parse_from_rfc3339(capture).map_err(|_| AssetFailure::Changed)?;
    let owner_hash = hex_digest(
        &serde_json::to_vec(&(database_id, camera, capture, expiry))
            .map_err(|_| AssetFailure::Changed)?,
    );
    let expiry_ms = asset_id
        .strip_prefix("frame-v1-")
        .and_then(|s| s.split('-').next())
        .and_then(|s| s.parse::<i64>().ok())
        .ok_or(AssetFailure::Changed)?;
    if !frame_asset_id(asset_id)
        || manifest.schema != 1
        || !manifest.label.is_empty()
        || camera.is_empty()
        || camera.len() > 1024
        || camera == "."
        || camera == ".."
        || camera.chars().any(char::is_control)
        || m.status != "available"
        || m.asset_id != asset_id
        || m.database_id != database_id
        || m.kind != RAW_KIND
        || m.mime_type != RAW_MIME
        || m.provenance != "daemon_camera_frame"
        || m.observation_id.is_some()
        || m.zone.is_some()
        || m.bytes == 0
        || m.bytes > DEFAULT_MAX_ASSET_BYTES
        || expires <= capture_dt
        || expires.timestamp_millis() != expiry_ms
        || asset_id != format!("frame-v1-{expiry_ms}-{owner_hash}-{}", m.sha256)
    {
        return Err(AssetFailure::Changed);
    }
    Ok(())
}

fn owner_digest(
    database_id: &str,
    id: i64,
    camera: &str,
    zone: &str,
    label: &str,
    capture: &str,
) -> String {
    hex_digest(
        &serde_json::to_vec(&(database_id, id, camera, zone, label, capture))
            .expect("serialize owner"),
    )
}

/// Only recognized versioned names can ever reach the filesystem resolver.
pub fn asset_observation_id(id: &str) -> Option<i64> {
    let mut parts = id.strip_prefix("raw-v1-")?.split('-');
    let number = parts.next()?;
    let obs: i64 = number.parse().ok()?;
    if obs <= 0 || obs.to_string() != number {
        return None;
    }
    for _ in 0..2 {
        let hash = parts.next()?;
        if hash.len() != 64
            || !hash
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        {
            return None;
        }
    }
    parts.next().is_none().then_some(obs)
}

pub fn hex_digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn bounded_read(root: &Path, path: &Path, limit: u64) -> Result<Vec<u8>, AssetFailure> {
    let meta = std::fs::symlink_metadata(path).map_err(map_io)?;
    if !meta.file_type().is_file() {
        return Err(AssetFailure::InvalidPath);
    }
    let canonical = std::fs::canonicalize(path).map_err(map_io)?;
    if !canonical.starts_with(root) || canonical.parent() != Some(root) {
        return Err(AssetFailure::InvalidPath);
    }
    let file = std::fs::File::open(&canonical).map_err(map_io)?;
    let meta = file.metadata().map_err(map_io)?;
    if !meta.is_file() {
        return Err(AssetFailure::InvalidPath);
    }
    if meta.len() > limit {
        return Err(AssetFailure::TooLarge);
    }
    let mut bytes = Vec::new();
    file.take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(map_io)?;
    if bytes.len() as u64 > limit {
        return Err(AssetFailure::TooLarge);
    }
    Ok(bytes)
}

fn dimensions(width: u32, height: u32) -> Result<(), AssetFailure> {
    if width == 0
        || height == 0
        || width > MAX_DIMENSION
        || height > MAX_DIMENSION
        || u64::from(width) * u64::from(height) > MAX_PIXELS
    {
        return Err(AssetFailure::TooLarge);
    }
    Ok(())
}

fn jpeg_dimensions(bytes: &[u8]) -> Result<(u32, u32), AssetFailure> {
    // image/zune accepts a truncated entropy scan in permissive mode. Require
    // the JPEG framing markers before decoding so a partial camera write cannot
    // become an apparently valid immutable asset.
    if bytes.len() < 4 || !bytes.starts_with(&[0xff, 0xd8]) || !bytes.ends_with(&[0xff, 0xd9]) {
        return Err(AssetFailure::Changed);
    }
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    limits.max_alloc = Some(MAX_PIXELS * 4);
    let mut reader =
        image::ImageReader::with_format(std::io::Cursor::new(bytes), image::ImageFormat::Jpeg);
    reader.limits(limits);
    let decoder = reader.into_decoder().map_err(|_| AssetFailure::Changed)?;
    let (width, height) = decoder.dimensions();
    dimensions(width, height)?;
    // Validate the entire JPEG, not just its marker header, under decode limits.
    image::DynamicImage::from_decoder(decoder).map_err(|_| AssetFailure::Changed)?;
    Ok((width, height))
}

struct LimitedWriter {
    bytes: Vec<u8>,
    limit: usize,
}
impl Write for LimitedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            return Err(failure_io(AssetFailure::TooLarge));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn map_io(e: io::Error) -> AssetFailure {
    if let Some(failure) = e.get_ref().and_then(|e| e.downcast_ref::<AssetFailure>()) {
        return *failure;
    }
    if e.kind() == io::ErrorKind::NotFound {
        AssetFailure::Missing
    } else {
        AssetFailure::Io
    }
}
fn failure_io(e: AssetFailure) -> io::Error {
    io::Error::other(e)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssetFailure {
    NotFound,
    Unavailable,
    Missing,
    TooLarge,
    Changed,
    InvalidPath,
    Io,
    Busy,
    CameraNotFound,
    CameraOffline,
    FrameUnavailable,
    Stale,
}
impl std::fmt::Display for AssetFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "raw asset failure: {self:?}")
    }
}

impl std::error::Error for AssetFailure {}

impl AssetFailure {
    fn status(self) -> StatusCode {
        match self {
            Self::NotFound | Self::Unavailable => StatusCode::NOT_FOUND,
            Self::Missing => StatusCode::GONE,
            Self::TooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Self::Busy => StatusCode::SERVICE_UNAVAILABLE,
            Self::Changed | Self::InvalidPath | Self::Io => StatusCode::INTERNAL_SERVER_ERROR,
            Self::CameraNotFound => StatusCode::NOT_FOUND,
            Self::CameraOffline => StatusCode::SERVICE_UNAVAILABLE,
            Self::FrameUnavailable => StatusCode::SERVICE_UNAVAILABLE,
            Self::Stale => StatusCode::GATEWAY_TIMEOUT,
        }
    }
    fn body(self) -> AssetErrorBody {
        let (code, message) = match self {
            Self::NotFound => (
                "not_found",
                "The requested raw asset or observation was not found.",
            ),
            Self::Unavailable => (
                "raw_unavailable",
                "This observation has no raw image asset.",
            ),
            Self::Missing => ("source_expired", "The raw asset is no longer available."),
            Self::TooLarge => (
                "payload_too_large",
                "The raw asset exceeds the configured limits.",
            ),
            Self::Changed => (
                "integrity_error",
                "The raw asset failed integrity validation.",
            ),
            Self::InvalidPath => ("invalid_source", "The raw asset source is invalid."),
            Self::Io => ("internal", "The raw asset could not be read."),
            Self::Busy => ("busy", "The raw asset lookup exceeded its work budget."),
            Self::CameraNotFound => (
                "camera_not_found",
                "The requested camera is not configured.",
            ),
            Self::CameraOffline => ("camera_offline", "The requested camera is not running."),
            Self::FrameUnavailable => ("frame_unavailable", "No camera frame is available yet."),
            Self::Stale => (
                "frame_stale",
                "No frame newer than the requested age became available.",
            ),
        };
        AssetErrorBody::new(code, message)
    }
}

#[derive(Clone)]
struct ApiState {
    store: AssetStore,
    authority: String,
}

/// The actual bound address (including an ephemeral port) is the only allowed
/// Host/Origin authority; browsers are never granted cross-origin access.
pub fn router(store: AssetStore, address: std::net::SocketAddr) -> Router {
    Router::new()
        .route("/internal/v1/cameras", get(cameras_handler))
        .route("/internal/v1/health", get(health_handler))
        .route("/internal/v1/assets/{asset_id}", get(metadata_handler))
        .route(
            "/internal/v1/assets/{asset_id}/content",
            get(content_handler),
        )
        .route(
            "/internal/v1/observations/{id}/raw-asset",
            get(observation_handler),
        )
        .route(
            "/internal/v1/cameras/{camera_id}/frame",
            post(frame_handler),
        )
        .fallback(fallback_handler)
        .method_not_allowed_fallback(method_handler)
        .layer(axum::extract::DefaultBodyLimit::max(
            MAX_MANIFEST_BYTES as usize,
        ))
        .layer(axum::middleware::from_fn_with_state(
            ApiState {
                store: store.clone(),
                authority: address.to_string(),
            },
            authorize_request,
        ))
        .with_state(ApiState {
            store,
            authority: address.to_string(),
        })
}

async fn authorize_request(
    State(state): State<ApiState>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    if let Err((status, body)) = check_request(&state, request.headers()) {
        return error_response(status, body);
    }
    next.run(request).await
}

async fn fallback_handler() -> Response {
    error_response(StatusCode::NOT_FOUND, AssetFailure::NotFound.body())
}
async fn method_handler() -> Response {
    error_response(
        StatusCode::METHOD_NOT_ALLOWED,
        AssetErrorBody::new(
            "method_not_allowed",
            "The requested method is not supported.",
        ),
    )
}

fn check_request(
    state: &ApiState,
    headers: &HeaderMap,
) -> Result<(), (StatusCode, AssetErrorBody)> {
    if !state.store.authorized(headers) {
        return Err((
            StatusCode::UNAUTHORIZED,
            AssetErrorBody::new("unauthorized", "Internal asset access is unauthorized."),
        ));
    }
    if headers.get_all(header::HOST).iter().count() != 1
        || headers.get(header::HOST).and_then(|h| h.to_str().ok()) != Some(&state.authority)
        || headers.get_all(header::ORIGIN).iter().count() > 1
        || headers
            .get(header::ORIGIN)
            .is_some_and(|h| h.to_str().ok() != Some(&format!("http://{}", state.authority)))
    {
        return Err((
            StatusCode::FORBIDDEN,
            AssetErrorBody::new("forbidden", "Internal asset request origin is not allowed."),
        ));
    }
    Ok(())
}

async fn blocking_response<F>(state: ApiState, headers: HeaderMap, operation: F) -> Response
where
    F: FnOnce(AssetStore) -> Result<Response, AssetFailure> + Send + 'static,
{
    if let Err((status, body)) = check_request(&state, &headers) {
        return error_response(status, body);
    }
    // Reject excess work rather than queue an unbounded set of blocking reads.
    let Ok(permit) = state.store.readers.clone().try_acquire_owned() else {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            AssetErrorBody::new("busy", "Internal asset readers are busy."),
        );
    };
    match tokio::task::spawn_blocking(move || {
        let _permit = permit;
        operation(state.store)
    })
    .await
    {
        Ok(Ok(response)) => response,
        Ok(Err(e)) => error_response(e.status(), e.body()),
        Err(_) => error_response(AssetFailure::Io.status(), AssetFailure::Io.body()),
    }
}

fn frame_failure(error: crate::frame_cache::FrameLookupError) -> AssetFailure {
    match error {
        crate::frame_cache::FrameLookupError::CameraNotFound => AssetFailure::CameraNotFound,
        crate::frame_cache::FrameLookupError::CameraOffline => AssetFailure::CameraOffline,
        crate::frame_cache::FrameLookupError::FrameUnavailable => AssetFailure::FrameUnavailable,
        crate::frame_cache::FrameLookupError::FrameStale => AssetFailure::Stale,
    }
}

async fn frame_handler(
    State(state): State<ApiState>,
    headers: HeaderMap,
    AxumPath(camera_id): AxumPath<String>,
    request: Result<Json<CameraFrameRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err((status, body)) = check_request(&state, &headers) {
        return error_response(status, body);
    }
    let request = match request {
        Ok(Json(request)) => request,
        Err(error) => {
            return error_response(
                if error.status() == StatusCode::PAYLOAD_TOO_LARGE {
                    error.status()
                } else {
                    StatusCode::BAD_REQUEST
                },
                AssetErrorBody::new(
                    if error.status() == StatusCode::PAYLOAD_TOO_LARGE {
                        "payload_too_large"
                    } else {
                        "invalid_input"
                    },
                    "Provide a bounded valid camera frame request.",
                ),
            );
        }
    };
    let Ok(permit) = state.store.readers.clone().try_acquire_owned() else {
        return error_response(AssetFailure::Busy.status(), AssetFailure::Busy.body());
    };
    if request.database_id != state.store.database_id.as_ref() {
        return error_response(
            StatusCode::CONFLICT,
            AssetErrorBody::new(
                "dataset_mismatch",
                "The requested database identity does not match this daemon.",
            ),
        );
    }
    if !(1..=60_000).contains(&request.max_age_ms)
        || request.wait_timeout_ms > 10_000
        || request.max_bytes == 0
        || request.max_bytes > DEFAULT_MAX_ASSET_BYTES
    {
        return error_response(
            StatusCode::BAD_REQUEST,
            AssetErrorBody::new(
                "invalid_input",
                "Frame limits are outside the supported range.",
            ),
        );
    }
    let Some(cache) = state.store.frame_cache() else {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            AssetFailure::Unavailable.body(),
        );
    };
    let max_age = Duration::from_millis(request.max_age_ms);
    let wait = Duration::from_millis(request.wait_timeout_ms);
    let frame = match cache.wait_fresh(&camera_id, max_age, wait).await {
        Ok(frame) => frame,
        Err(e) => {
            let failure = match e {
                crate::frame_cache::FrameLookupError::CameraNotFound => {
                    AssetFailure::CameraNotFound
                }
                crate::frame_cache::FrameLookupError::CameraOffline => AssetFailure::CameraOffline,
                crate::frame_cache::FrameLookupError::FrameUnavailable => {
                    AssetFailure::FrameUnavailable
                }
                crate::frame_cache::FrameLookupError::FrameStale => AssetFailure::Stale,
            };
            return error_response(failure.status(), failure.body());
        }
    };
    let store = state.store.clone();
    let requested = request.max_bytes;
    match tokio::task::spawn_blocking(move || {
        let _permit = permit;
        cache
            .validate_snapshot(&camera_id, &frame, max_age)
            .map_err(frame_failure)?;
        let limited_store = AssetStore {
            max_bytes: store.max_bytes.min(requested),
            ..store
        };
        let metadata = limited_store.freeze_camera_frame(&camera_id, &frame)?;
        cache
            .validate_snapshot(&camera_id, &frame, max_age)
            .map_err(frame_failure)?;
        if metadata.bytes > requested {
            return Err(AssetFailure::TooLarge);
        }
        if frame_expiry(&metadata.asset_id)
            .is_none_or(|deadline| deadline <= chrono::Utc::now().timestamp_millis())
        {
            return Err(AssetFailure::Missing);
        }
        Ok(CameraFrameMetadata {
            asset: metadata,
            age_ms: frame.acquired_at.elapsed().as_millis() as u64,
            camera_state: "running".into(),
        })
    })
    .await
    {
        Ok(Ok(value)) => json_response(StatusCode::OK, &value),
        Ok(Err(e)) => error_response(e.status(), e.body()),
        Err(_) => error_response(StatusCode::INTERNAL_SERVER_ERROR, AssetFailure::Io.body()),
    }
}

async fn cameras_handler(State(state): State<ApiState>, headers: HeaderMap) -> Response {
    blocking_response(state, headers, |store| {
        store
            .cameras()
            .map(|value| json_response(StatusCode::OK, &value))
    })
    .await
}

async fn health_handler(State(state): State<ApiState>, headers: HeaderMap) -> Response {
    blocking_response(state, headers, |store| {
        store
            .health()
            .map(|value| json_response(StatusCode::OK, &value))
    })
    .await
}

async fn metadata_handler(
    State(state): State<ApiState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Response {
    blocking_response(state, headers, move |store| {
        store
            .metadata(&id)
            .map(|m| json_response(StatusCode::OK, &m))
    })
    .await
}
async fn observation_handler(
    State(state): State<ApiState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Response {
    blocking_response(state, headers, move |store| {
        let id = id
            .parse::<i64>()
            .ok()
            .filter(|id| *id > 0)
            .ok_or(AssetFailure::NotFound)?;
        store
            .observation_metadata(id)
            .map(|m| json_response(StatusCode::OK, &m))
    })
    .await
}
async fn content_handler(
    State(state): State<ApiState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Response {
    blocking_response(state, headers, move |store| {
        let (metadata, bytes) = store.content(&id)?;
        let mut response = Response::new(Body::from(bytes));
        let headers = response.headers_mut();
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(RAW_MIME));
        headers.insert(
            header::CONTENT_LENGTH,
            HeaderValue::from_str(&metadata.bytes.to_string()).unwrap(),
        );
        headers.insert(
            "x-asset-id",
            HeaderValue::from_str(&metadata.asset_id).unwrap(),
        );
        headers.insert(
            "x-asset-sha256",
            HeaderValue::from_str(&metadata.sha256).unwrap(),
        );
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        headers.insert(
            "x-content-type-options",
            HeaderValue::from_static("nosniff"),
        );
        Ok(response)
    })
    .await
}
fn json_response<T: Serialize>(status: StatusCode, value: &T) -> Response {
    let mut response = Response::new(Body::from(
        serde_json::to_vec(value).expect("asset response serialization"),
    ));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}
fn error_response(status: StatusCode, body: AssetErrorBody) -> Response {
    json_response(status, &body)
}

fn raw_temp_name(name: &str) -> bool {
    let Some(inner) = name
        .strip_prefix(".raw-")
        .and_then(|name| name.strip_suffix(".tmp"))
    else {
        return false;
    };
    let Some((pid, seq)) = inner.split_once('-') else {
        return false;
    };
    !pid.is_empty()
        && !seq.is_empty()
        && pid.bytes().all(|c| c.is_ascii_digit())
        && seq.bytes().all(|c| c.is_ascii_digit())
}

/// Retention reconciles only versioned raw names. Unlike legacy snapshots,
/// durable owner manifests allow safe cleanup even when the DB becomes empty.
/// The DB check is per owner and short; unlinks never hold its mutex.
pub(crate) fn expired_raw_files(db: &SharedStore, root: &Path) -> io::Result<Vec<PathBuf>> {
    let canonical = match std::fs::canonicalize(root) {
        Ok(root) => root,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let database_id = db
        .lock()
        .map_err(|_| io::Error::other("DB lock poisoned"))?
        .database_identity()
        .map_err(io::Error::other)?;
    let mut expired = Vec::new();
    for entry in std::fs::read_dir(&canonical)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if raw_temp_name(name) {
            // Caller holds RAW_LIFECYCLE: no publisher can own a live temp.
            expired.push(entry.path());
            continue;
        }
        let Some(asset_id) = name
            .strip_suffix(".json")
            .or_else(|| name.strip_suffix(".jpg"))
        else {
            continue;
        };
        let Some(id) = asset_observation_id(asset_id) else {
            continue;
        };
        let owner = db
            .lock()
            .map_err(|_| io::Error::other("DB lock poisoned"))?
            .observation_by_id(id)
            .map_err(io::Error::other)?;
        let manifest_path = canonical.join(format!("{asset_id}.json"));
        let live = owner.as_ref().is_some_and(|owner| {
            bounded_read(&canonical, &manifest_path, MAX_MANIFEST_BYTES)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Manifest>(&bytes).ok())
                .is_some_and(|m| {
                    validate_manifest(&m, asset_id, id).is_ok()
                        && database_id
                            .as_deref()
                            .is_some_and(|db| belongs_to(&m, owner, db))
                })
        });
        if !live {
            expired.push(entry.path());
        }
    }
    Ok(expired)
}

#[cfg(test)]
#[path = "assets_frame_tests.rs"]
mod frame_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::http::Request;
    use tower::ServiceExt;

    pub(super) fn fixture() -> (
        tempfile::TempDir,
        SharedStore,
        AssetStore,
        Observation,
        RawAssetMetadata,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(std::sync::Mutex::new(
            item_core::store::Store::in_memory().unwrap(),
        ));
        let now = chrono::Utc::now();
        let owner = {
            let db = db.lock().unwrap();
            let (id, _, _) = db
                .record_sighting(
                    "cam",
                    "frame",
                    "cup",
                    now,
                    None,
                    item_core::store::DEFAULT_DEDUP_WINDOW,
                )
                .unwrap();
            db.observation_by_id(id).unwrap().unwrap()
        };
        let store = AssetStore::new(
            dir.path(),
            DEFAULT_MAX_ASSET_BYTES,
            Some("secret".into()),
            db.clone(),
        )
        .unwrap();
        let m = store
            .publish_rgb(&owner, now, &[7; 3 * 2 * 3], 3, 2)
            .unwrap();
        (dir, db, store, owner, m)
    }

    #[test]
    fn durable_identity_integrity_and_no_overwrite() {
        let (dir, db, store, owner, m) = fixture();
        assert_eq!(asset_observation_id(&m.asset_id), Some(owner.id));
        assert!(m.asset_id.ends_with(&m.sha256));
        let restarted = AssetStore::new(dir.path(), DEFAULT_MAX_ASSET_BYTES, None, db).unwrap();
        assert_eq!(restarted.observation_metadata(owner.id).unwrap(), m);
        let bytes = store.content(&m.asset_id).unwrap().1;
        let again = store
            .publish_jpeg(&owner, owner.first_seen, &bytes)
            .unwrap();
        assert_eq!(again, m);
        std::fs::write(dir.path().join(format!("{}.jpg", m.asset_id)), b"corrupt").unwrap();
        assert_eq!(store.metadata(&m.asset_id), Err(AssetFailure::Changed));
        assert!(
            store
                .publish_jpeg(&owner, owner.first_seen, &bytes)
                .is_err()
        );
        assert_eq!(
            std::fs::read(dir.path().join(format!("{}.jpg", m.asset_id))).unwrap(),
            b"corrupt"
        );
    }

    #[test]
    fn identical_observation_ids_across_databases_cannot_share_assets() {
        let (dir, _db, first, owner, m) = fixture();
        let db = Arc::new(std::sync::Mutex::new(
            item_core::store::Store::in_memory().unwrap(),
        ));
        db.lock()
            .unwrap()
            .record_sighting(
                &owner.camera_id,
                &owner.zone,
                &owner.label,
                owner.first_seen,
                None,
                item_core::store::DEFAULT_DEDUP_WINDOW,
            )
            .unwrap();
        let second = AssetStore::new(
            dir.path(),
            DEFAULT_MAX_ASSET_BYTES,
            Some("second".into()),
            db.clone(),
        )
        .unwrap();
        assert_ne!(first.database_id, second.database_id);
        assert_eq!(second.metadata(&m.asset_id), Err(AssetFailure::Missing));
        assert_eq!(
            second.observation_metadata(owner.id),
            Err(AssetFailure::Unavailable)
        );
        let other = second
            .publish_rgb(&owner, owner.first_seen, &[7; 18], 3, 2)
            .unwrap();
        assert_eq!(other.sha256, m.sha256);
        assert_ne!(other.asset_id, m.asset_id);
        assert_eq!(second.metadata(&other.asset_id).unwrap(), other);
    }

    #[test]
    fn crash_temporaries_cleaned_on_start_and_reconcile() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(std::sync::Mutex::new(
            item_core::store::Store::in_memory().unwrap(),
        ));
        let temp = dir.path().join(".raw-123-456.tmp");
        let foreign = dir.path().join(".raw-not-mine.tmp");
        std::fs::write(&temp, b"partial").unwrap();
        std::fs::write(&foreign, b"foreign").unwrap();
        let assets =
            AssetStore::new(dir.path(), DEFAULT_MAX_ASSET_BYTES, None, db.clone()).unwrap();
        assert!(!temp.exists());
        assert!(foreign.exists());
        std::fs::write(&temp, b"partial again").unwrap();
        crate::retention::reconcile(&db, dir.path()).unwrap();
        assert!(!temp.exists());
        assert!(foreign.exists());
        assert_eq!(
            db.lock().unwrap().database_identity().unwrap().as_deref(),
            Some(assets.database_id.as_ref())
        );
    }

    #[test]
    fn expired_and_reused_observation_cannot_claim_old_asset() {
        let (_dir, db, store, owner, m) = fixture();
        db.lock()
            .unwrap()
            .expire_observations(chrono::Utc::now() + chrono::Duration::days(1))
            .unwrap();
        assert_eq!(store.metadata(&m.asset_id), Err(AssetFailure::Missing));
        let time = owner.first_seen + chrono::Duration::days(2);
        let (id, _, _) = db
            .lock()
            .unwrap()
            .record_sighting(
                "cam",
                "frame",
                "cup",
                time,
                None,
                item_core::store::DEFAULT_DEDUP_WINDOW,
            )
            .unwrap();
        assert_eq!(id, owner.id);
        assert_eq!(store.metadata(&m.asset_id), Err(AssetFailure::Missing));
        assert_eq!(
            store.observation_metadata(id),
            Err(AssetFailure::Unavailable)
        );
    }

    #[test]
    fn bounded_inputs_missing_files_and_debug_redaction() {
        let (dir, _db, store, owner, m) = fixture();
        assert!(!format!("{store:?}").contains("secret"));
        assert!(!format!("{store:?}").contains(&dir.path().display().to_string()));
        assert!(
            store
                .publish_rgb(&owner, owner.first_seen, &[], 65535, 65535)
                .is_err()
        );
        assert!(
            store
                .publish_jpeg(&owner, owner.first_seen, b"not jpeg")
                .is_err()
        );
        assert_eq!(store.metadata("../x"), Err(AssetFailure::NotFound));
        let jpeg = dir.path().join(format!("{}.jpg", m.asset_id));
        std::fs::remove_file(&jpeg).unwrap();
        assert_eq!(store.metadata(&m.asset_id), Err(AssetFailure::Missing));
        std::fs::File::create(&jpeg)
            .unwrap()
            .set_len(DEFAULT_MAX_ASSET_BYTES + 1)
            .unwrap();
        assert_eq!(store.metadata(&m.asset_id), Err(AssetFailure::TooLarge));
        std::fs::File::create(dir.path().join(format!("{}.json", m.asset_id)))
            .unwrap()
            .set_len(MAX_MANIFEST_BYTES + 1)
            .unwrap();
        assert_eq!(store.metadata(&m.asset_id), Err(AssetFailure::TooLarge));
    }

    #[tokio::test]
    async fn api_auth_origin_metadata_content_and_limits() {
        let (_dir, _db, store, owner, m) = fixture();
        let address = "127.0.0.1:8480".parse().unwrap();
        let app = router(store.clone(), address);
        let url = format!("/internal/v1/assets/{}", m.asset_id);
        for (auth, host, origin, status) in [
            (None, "127.0.0.1:8480", None, StatusCode::UNAUTHORIZED),
            (
                Some("Bearer wrong"),
                "127.0.0.1:8480",
                None,
                StatusCode::UNAUTHORIZED,
            ),
            (
                Some("Bearer secret"),
                "evil.test",
                None,
                StatusCode::FORBIDDEN,
            ),
            (
                Some("Bearer secret"),
                "127.0.0.1:8480",
                Some("http://evil.test"),
                StatusCode::FORBIDDEN,
            ),
            (
                Some("Bearer secret"),
                "127.0.0.1:8480",
                Some("null"),
                StatusCode::FORBIDDEN,
            ),
            (
                Some("Bearer secret"),
                "127.0.0.1:8480",
                Some("http://127.0.0.1:8480"),
                StatusCode::OK,
            ),
        ] {
            let mut req = Request::get(&url).header("host", host);
            if let Some(auth) = auth {
                req = req.header("authorization", auth);
            }
            if let Some(origin) = origin {
                req = req.header("origin", origin);
            }
            assert_eq!(
                app.clone()
                    .oneshot(req.body(Body::empty()).unwrap())
                    .await
                    .unwrap()
                    .status(),
                status
            );
        }
        for url in [
            url.clone(),
            format!("/internal/v1/observations/{}/raw-asset", owner.id),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::get(url)
                        .header("host", address.to_string())
                        .header("authorization", "Bearer secret")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                serde_json::from_slice::<RawAssetMetadata>(
                    &to_bytes(response.into_body(), 16384).await.unwrap()
                )
                .unwrap(),
                m
            );
        }
        let req = || {
            Request::get(format!("{url}/content"))
                .header("host", address.to_string())
                .header("authorization", "Bearer secret")
                .body(Body::empty())
                .unwrap()
        };
        let response = app.clone().oneshot(req()).await.unwrap();
        assert_eq!(response.headers()[header::CONTENT_TYPE], RAW_MIME);
        assert_eq!(response.headers()["x-asset-id"], m.asset_id);
        assert_eq!(response.headers()["x-asset-sha256"], m.sha256);
        assert_eq!(
            hex_digest(
                &to_bytes(response.into_body(), DEFAULT_MAX_ASSET_BYTES as usize)
                    .await
                    .unwrap()
            ),
            m.sha256
        );
        let _held = store
            .readers
            .clone()
            .acquire_many_owned(MAX_READERS as u32)
            .await
            .unwrap();
        assert_eq!(
            app.oneshot(req()).await.unwrap().status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        let no_token =
            AssetStore::new(store.root.as_ref(), store.max_bytes, None, store.db.clone()).unwrap();
        assert_eq!(
            router(no_token, address)
                .oneshot(req())
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
    }

    #[test]
    fn concurrent_camera_ingest_preserves_first_raw_and_snapshots_off() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(std::sync::Mutex::new(
            item_core::store::Store::in_memory().unwrap(),
        ));
        let assets = AssetStore::new(
            dir.path(),
            DEFAULT_MAX_ASSET_BYTES,
            Some("secret".into()),
            db.clone(),
        )
        .unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let threads: Vec<_> = (0..2)
            .map(|camera| {
                let (db, assets, barrier) = (db.clone(), assets.clone(), barrier.clone());
                std::thread::spawn(move || {
                    barrier.wait();
                    let meta = item_core::FrameMeta {
                        camera_id: format!("cam-{camera}"),
                        captured_at: chrono::Utc::now(),
                        width: 8,
                        height: 8,
                        snapshot_path: None,
                    };
                    let det = item_core::Detection {
                        label: "cup".into(),
                        confidence: 0.9,
                        bbox: [1., 1., 6., 6.],
                    };
                    let first = vec![camera as u8 * 200; 8 * 8 * 3];
                    let recorded = crate::ingest_detections_with_depth(
                        &db,
                        &meta,
                        std::slice::from_ref(&det),
                        0.45,
                        0.,
                        Some((&first, assets.root.as_path())),
                        crate::config::SnapshotMode::Off,
                        &[],
                        None,
                        None,
                        Some(&assets),
                    )
                    .unwrap();
                    let id = recorded[0].obs_id;
                    let raw = assets.observation_metadata(id).unwrap();
                    let later = vec![80; 8 * 8 * 3];
                    for _ in 0..3 {
                        let more = crate::ingest_detections_with_depth(
                            &db,
                            &meta,
                            std::slice::from_ref(&det),
                            0.45,
                            0.,
                            Some((&later, assets.root.as_path())),
                            crate::config::SnapshotMode::Off,
                            &[],
                            None,
                            None,
                            Some(&assets),
                        )
                        .unwrap();
                        assert!(!more[0].is_new);
                        assert_eq!(assets.observation_metadata(id).unwrap(), raw);
                        crate::retention::reconcile(&db, assets.root.as_path()).unwrap();
                    }
                    let bytes = assets.content(&raw.asset_id).unwrap().1;
                    let pixel = image::load_from_memory(&bytes)
                        .unwrap()
                        .to_rgb8()
                        .get_pixel(0, 0)
                        .0[0];
                    assert!(pixel.abs_diff(camera as u8 * 200) < 5);
                    assert!(!assets.root.join(format!("{id}.jpg")).exists());
                    raw
                })
            })
            .collect();
        let results: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        assert_ne!(results[0].asset_id, results[1].asset_id);
    }

    #[test]
    fn expired_owner_is_not_published_and_retention_removes_manifest_and_raw() {
        let (dir, db, store, owner, m) = fixture();
        let annotated = dir.path().join(format!("{}.jpg", owner.id));
        std::fs::write(&annotated, b"annotated").unwrap();
        let report = crate::retention::prune(
            &db,
            dir.path(),
            std::time::Duration::from_secs(1),
            chrono::Utc::now() + chrono::Duration::days(2),
        )
        .unwrap();
        assert_eq!(report.rows_deleted, 1);
        assert_eq!(report.files_deleted, 3);
        assert!(!annotated.exists());
        assert!(!dir.path().join(format!("{}.jpg", m.asset_id)).exists());
        assert!(!dir.path().join(format!("{}.json", m.asset_id)).exists());
        assert!(
            store
                .publish_rgb(&owner, owner.first_seen, &[8; 18], 3, 2)
                .is_err()
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn retention_racing_publication_is_serialized_at_commit_not_encoding() {
        let (dir, db, store, owner, m) = fixture();
        let commit = RAW_LIFECYCLE.lock().unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let worker = {
            let store = store.clone();
            let owner = owner.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                store.publish_rgb(&owner, owner.first_seen, &[120; 18], 3, 2)
            })
        };
        barrier.wait();
        // Even while a publisher is queued for its file commit, the DB mutex
        // remains obtainable and the expiry transaction is not blocked.
        db.lock()
            .unwrap()
            .expire_observations(chrono::Utc::now() + chrono::Duration::days(2))
            .unwrap();
        drop(commit);
        assert!(worker.join().unwrap().is_err());
        let report = crate::retention::reconcile(&db, dir.path()).unwrap();
        assert_eq!(report.files_deleted, 2);
        assert_eq!(store.metadata(&m.asset_id), Err(AssetFailure::NotFound));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_assets_and_replaced_root() {
        let (dir, _db, store, _owner, m) = fixture();
        let outside = tempfile::tempdir().unwrap();
        let path = dir.path().join(format!("{}.jpg", m.asset_id));
        std::fs::rename(&path, outside.path().join("outside.jpg")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("outside.jpg"), &path).unwrap();
        assert_eq!(store.metadata(&m.asset_id), Err(AssetFailure::InvalidPath));
    }
}
