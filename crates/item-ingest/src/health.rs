//! The liveness file (design/resident-ingest.md §5).
//!
//! A file rather than an HTTP endpoint: `item-web` has no HTTP client, and 15s
//! granularity is plenty for "is the ingest daemon alive". The only field that
//! decides liveness is `updated_at` -- `pid` is advisory, because Windows would
//! need `OpenProcess`, Unix pids get reused, and neither is a portable std API.
//!
//! Writes are atomic: serialize into `<path>.tmp`, then rename over the target,
//! so a reader never sees half a file. On Windows a reader holding the target
//! open makes that rename fail with a sharing violation (Unix does not care), so
//! the rename is retried and, as a last resort, the target is deleted first --
//! the contract in §5 is that the file may be briefly missing or stale, and
//! every reader must tolerate that.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::Serialize;

use crate::runner::CameraHealth;

/// Bumped whenever the shape below changes; readers must check it.
pub const SCHEMA: u32 = 1;

/// §5's liveness rule: an `updated_at` older than this means the process is
/// dead or hung. It is the **only** liveness signal -- `pid` is advisory
/// (Windows needs `OpenProcess`, Unix reuses pids), so nothing may decide on
/// it.
pub const STALE_AFTER: Duration = Duration::from_secs(90);

/// How long ago the health file was last written, or `None` when it cannot be
/// read at all. §5 requires every reader to tolerate a missing or stale file,
/// so this never fails loudly.
pub fn age_of(path: &Path) -> Option<Duration> {
    let text = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let updated = DateTime::parse_from_rfc3339(value.get("updated_at")?.as_str()?).ok()?;
    let age = Utc::now().signed_duration_since(updated.with_timezone(&Utc));
    // A timestamp in the future (clock skew) reads as "just now", not as an
    // error.
    Some(age.to_std().unwrap_or_default())
}

/// One camera, as the outside world sees it.
#[derive(Debug, Clone, Serialize)]
pub struct CameraEntry {
    pub id: String,
    pub state: String,
    /// Already redacted: credentials never reach this file.
    pub source: String,
    pub reconnects: u64,
    pub frames: u64,
    pub detections: u64,
    pub recorded: u64,
    pub last_frame_at: Option<String>,
    pub last_frame_age_s: Option<u64>,
    pub last_error: Option<String>,
    pub inference_ms_ewma: f64,
}

/// The whole file, as serialized.
#[derive(Debug, Clone, Serialize)]
pub struct HealthSnapshot {
    pub schema: u32,
    pub pid: u32,
    pub started_at: String,
    pub updated_at: String,
    pub seq: u64,
    pub detector_default: String,
    pub cameras: Vec<CameraEntry>,
}

/// Camera threads publish here; the writer task reads.
#[derive(Clone, Default)]
pub struct HealthRegistry {
    inner: Arc<Mutex<BTreeMap<String, CameraHealth>>>,
}

impl HealthRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn publish(&self, camera_id: &str, health: CameraHealth) {
        if let Ok(mut map) = self.inner.lock() {
            map.insert(camera_id.to_string(), health);
        }
    }

    pub fn get(&self, camera_id: &str) -> Option<CameraHealth> {
        self.inner.lock().ok()?.get(camera_id).cloned()
    }
}

/// The camera identity the writer needs, kept after the tasks themselves have
/// moved into the supervisor.
#[derive(Debug, Clone)]
pub struct CameraMeta {
    pub id: String,
    pub source: String,
}

/// Writes `health.json` on a cadence and once more on shutdown.
pub struct HealthWriter {
    path: PathBuf,
    started_at: DateTime<Utc>,
    seq: u64,
    detector_default: String,
    cameras: Vec<CameraMeta>,
}

impl HealthWriter {
    pub fn new(
        path: impl Into<PathBuf>,
        detector_default: impl Into<String>,
        cameras: Vec<CameraMeta>,
    ) -> Self {
        Self {
            path: path.into(),
            started_at: Utc::now(),
            seq: 0,
            detector_default: detector_default.into(),
            cameras,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// Build the current snapshot without touching the disk.
    pub fn snapshot(&self, registry: &HealthRegistry) -> HealthSnapshot {
        let updated_at = Utc::now();
        HealthSnapshot {
            schema: SCHEMA,
            pid: std::process::id(),
            started_at: self.started_at.to_rfc3339(),
            updated_at: updated_at.to_rfc3339(),
            seq: self.seq,
            detector_default: self.detector_default.clone(),
            cameras: self
                .cameras
                .iter()
                .map(|meta| camera_entry(meta, registry.get(&meta.id), updated_at))
                .collect(),
        }
    }

    pub fn write(&mut self, registry: &HealthRegistry) -> std::io::Result<()> {
        self.seq += 1;
        let snapshot = self.snapshot(registry);
        if let Some(dir) = self.path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        let json = serde_json::to_string_pretty(&snapshot).map_err(std::io::Error::other)?;
        write_atomic(&self.path, json.as_bytes())
    }

    /// The cameras this file describes; the daemon's heartbeat log walks them.
    pub fn cameras(&self) -> &[CameraMeta] {
        &self.cameras
    }
}

/// How many times a failing `rename` is retried before the fallback.
const RENAME_ATTEMPTS: u32 = 3;

/// Base delay between those retries; the nth retry waits `n * this`.
const RENAME_BACKOFF: Duration = Duration::from_millis(50);

/// Replace `path` with `bytes`, atomically where the platform allows it.
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = tmp_path(path);
    {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        // Not fatal if the OS declines to flush: the rename is what matters.
        let _ = file.sync_all();
    }

    let mut last_error = None;
    for attempt in 1..=RENAME_ATTEMPTS {
        match std::fs::rename(&tmp, path) {
            Ok(()) => return Ok(()),
            Err(e) => {
                last_error = Some(e);
                std::thread::sleep(RENAME_BACKOFF * attempt);
            }
        }
    }

    // A reader that refuses to share DELETE (the Windows case) still yields to
    // a delete: the file is briefly absent, which readers must already tolerate.
    if std::fs::remove_file(path).is_ok() && std::fs::rename(&tmp, path).is_ok() {
        return Ok(());
    }

    let _ = std::fs::remove_file(&tmp);
    Err(last_error.unwrap_or_else(|| std::io::Error::other("rename failed")))
}

/// `health.json` -> `health.json.tmp`, in the same directory so the rename
/// stays on one filesystem.
fn tmp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    path.with_file_name(name)
}

fn camera_entry(
    meta: &CameraMeta,
    health: Option<CameraHealth>,
    now: DateTime<Utc>,
) -> CameraEntry {
    let Some(h) = health else {
        // The camera thread has not reported yet: it exists, but has no facts.
        return CameraEntry {
            id: meta.id.clone(),
            state: "starting".to_string(),
            source: meta.source.clone(),
            reconnects: 0,
            frames: 0,
            detections: 0,
            recorded: 0,
            last_frame_at: None,
            last_frame_age_s: None,
            last_error: None,
            inference_ms_ewma: 0.0,
        };
    };

    CameraEntry {
        id: meta.id.clone(),
        state: h.state.as_str().to_string(),
        source: meta.source.clone(),
        reconnects: h.reconnects,
        frames: h.frames,
        detections: h.detections,
        recorded: h.recorded,
        last_frame_at: h
            .last_frame_age
            .map(|age| (now - ChronoDuration::from_std(age).unwrap_or_default()).to_rfc3339()),
        last_frame_age_s: h.last_frame_age.map(|age| age.as_secs()),
        last_error: h.last_error.clone(),
        inference_ms_ewma: (h.inference_ms_ewma * 10.0).round() / 10.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::CameraState;
    use std::time::Duration;

    fn health(frames: u64) -> CameraHealth {
        CameraHealth {
            state: CameraState::Running,
            frames,
            detections: 2,
            recorded: 1,
            reconnects: 3,
            last_frame_age: Some(Duration::from_secs(2)),
            last_error: None,
            inference_ms_ewma: 284.14,
        }
    }

    #[test]
    fn a_camera_that_has_not_reported_yet_still_appears() {
        let writer = HealthWriter::new(
            "unused.json",
            "yolo",
            vec![CameraMeta {
                id: "living".into(),
                source: "rtsp://cam.local/stream".into(),
            }],
        );
        let snap = writer.snapshot(&HealthRegistry::new());
        assert_eq!(snap.schema, SCHEMA);
        assert_eq!(snap.cameras.len(), 1);
        assert_eq!(snap.cameras[0].state, "starting");
        assert_eq!(snap.cameras[0].frames, 0);
    }

    #[test]
    fn published_health_reaches_the_snapshot_in_the_documented_shape() {
        let registry = HealthRegistry::new();
        registry.publish("living", health(210_344));
        let writer = HealthWriter::new(
            "unused.json",
            "yolo",
            vec![CameraMeta {
                id: "living".into(),
                source: "rtsp://cam.local/stream".into(),
            }],
        );

        let snap = writer.snapshot(&registry);
        let cam = &snap.cameras[0];
        assert_eq!(cam.state, "running");
        assert_eq!(cam.frames, 210_344);
        assert_eq!(cam.reconnects, 3);
        assert_eq!(cam.last_frame_age_s, Some(2));
        assert!(
            cam.last_frame_at.is_some(),
            "age must also be rendered as a timestamp"
        );
        assert_eq!(cam.inference_ms_ewma, 284.1);
    }

    #[test]
    fn writing_produces_parseable_json_and_bumps_seq() {
        let dir = std::env::temp_dir().join(format!("item-ingest-health-{}", std::process::id()));
        let path = dir.join("health.json");
        let registry = HealthRegistry::new();
        registry.publish("cam", health(7));

        let mut writer = HealthWriter::new(
            &path,
            "yolo",
            vec![CameraMeta {
                id: "cam".into(),
                source: "mock".into(),
            }],
        );
        writer.write(&registry).unwrap();
        writer.write(&registry).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["schema"], SCHEMA);
        assert_eq!(value["seq"], 2);
        assert_eq!(value["pid"], std::process::id());
        assert_eq!(value["cameras"][0]["frames"], 7);
        assert_eq!(value["detector_default"], "yolo");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_write_leaves_no_temp_file_behind() {
        let dir = std::env::temp_dir().join(format!("item-ingest-atomic-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("health.json");
        let registry = HealthRegistry::new();
        let mut writer = HealthWriter::new(
            &path,
            "yolo",
            vec![CameraMeta {
                id: "cam".into(),
                source: "mock".into(),
            }],
        );

        writer.write(&registry).unwrap();
        writer.write(&registry).unwrap();

        assert!(
            !dir.join("health.json.tmp").exists(),
            "the temp file is renamed away, never left behind"
        );
        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(value["seq"], 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// §5's contract: a reader may hold the file, and the write must still land.
    /// On Windows a reader that refuses to share DELETE makes `rename` fail, so
    /// `write_atomic` retries and then falls back to delete + rename.
    #[test]
    fn a_reader_holding_the_file_does_not_block_the_write() {
        let dir = std::env::temp_dir().join(format!("item-ingest-reader-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("health.json");
        let registry = HealthRegistry::new();
        let mut writer = HealthWriter::new(
            &path,
            "yolo",
            vec![CameraMeta {
                id: "cam".into(),
                source: "mock".into(),
            }],
        );
        writer.write(&registry).unwrap();

        let held = std::fs::File::open(&path).expect("a reader can hold the file");
        writer.write(&registry).unwrap();
        drop(held);

        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(value["seq"], 2);
        assert!(!dir.join("health.json.tmp").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The failure path: a target that cannot be replaced (here a directory)
    /// must be reported, and the temp file must not be left lying around.
    #[test]
    fn an_impossible_rename_is_reported_and_cleans_up() {
        let dir =
            std::env::temp_dir().join(format!("item-ingest-badrename-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let target = dir.join("health.json");
        std::fs::create_dir_all(&target).unwrap();

        let err = write_atomic(&target, b"{}").unwrap_err();
        assert!(!err.to_string().is_empty());
        assert!(!tmp_path(&target).exists(), "the temp file is cleaned up");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// §5: readers must tolerate a missing or unparseable file rather than
    /// treat it as fatal.
    #[test]
    fn a_health_file_that_is_missing_or_unreadable_has_no_age() {
        assert_eq!(age_of(Path::new("definitely-not-there.json")), None);

        let dir = std::env::temp_dir().join(format!("item-ingest-age-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("health.json");
        std::fs::write(&path, b"not json").unwrap();
        assert_eq!(age_of(&path), None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_age_of_a_snapshot_tells_fresh_from_stale() {
        let dir = std::env::temp_dir().join(format!("item-ingest-fresh-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("health.json");
        let registry = HealthRegistry::new();
        let mut writer = HealthWriter::new(&path, "yolo", Vec::new());
        writer.write(&registry).unwrap();

        let age = age_of(&path).expect("a written file has an age");
        assert!(
            age < STALE_AFTER,
            "a just-written file is not stale: {age:?}"
        );

        let stale = serde_json::json!({
            "updated_at": (Utc::now() - ChronoDuration::seconds(3600)).to_rfc3339(),
        });
        std::fs::write(&path, stale.to_string()).unwrap();
        assert!(
            age_of(&path).unwrap() > STALE_AFTER,
            "an hour-old snapshot is what the liveness rule rejects"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
