//! Runtime task descriptions: the pure-data layer that separates "what to run"
//! from "how to run it" (docs/resident-ingest.md §2).
//!
//! Nothing here touches the store or spawns threads. The only side-effecting
//! entry points are [`build_source`] / [`build_detector`], and their
//! feature-gated arms are the single place where a missing cargo feature turns
//! into either a warning (yolo -> `NullDetector`) or a hard error (vlm).

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::depth::{NoDepthProvider, RelativeDepthProvider};
use crate::detector::{Detector, NullDetector};
use crate::source::{FrameSource, MockSource};

// Re-export SnapshotMode so main.rs and other modules can use it without importing config
pub use crate::config::SnapshotMode;

/// How to reach one camera's frames.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceSpec {
    /// Synthetic frames -- unit tests and `--demo`; needs no cargo feature.
    Mock {
        frames: u32,
        width: u32,
        height: u32,
    },
    /// IP camera over RTSP (feature `rtsp`).
    Rtsp { url: String },
    /// Local/USB camera by index (feature `camera`). The index is **not**
    /// portable: DirectShow / V4L2 / AVFoundation enumerate different devices,
    /// so it must never be treated as a portable config value
    /// (docs/resident-ingest.md §9).
    Webcam { index: u32 },
}

impl SourceSpec {
    /// The cargo feature that makes this source usable (`""` when it always
    /// works). Deliberately distinct from [`SourceSpec::kind`], which is the
    /// log name: the feature is `camera`, the kind is `webcam`.
    pub fn cargo_feature(&self) -> &'static str {
        match self {
            SourceSpec::Mock { .. } => "",
            SourceSpec::Rtsp { .. } => "rtsp",
            SourceSpec::Webcam { .. } => "camera",
        }
    }

    /// `rtsp` / `webcam` / `mock` -- used in log lines and health output.
    pub fn kind(&self) -> &'static str {
        match self {
            SourceSpec::Mock { .. } => "mock",
            SourceSpec::Rtsp { .. } => "rtsp",
            SourceSpec::Webcam { .. } => "webcam",
        }
    }

    /// Same information, safe to log: credentials are stripped from RTSP urls.
    pub fn describe(&self) -> String {
        match self {
            SourceSpec::Mock { .. } => "mock".to_string(),
            SourceSpec::Rtsp { url } => redact_url(url),
            SourceSpec::Webcam { index } => format!("#{index}"),
        }
    }
}

/// Strip credentials from an RTSP url for safe logging
/// (rtsp://user:pass@host/path -> rtsp://host/path).
pub fn redact_url(url: &str) -> String {
    match url.split_once("://") {
        Some((scheme, rest)) => match rest.rsplit_once('@') {
            Some((_, host_path)) => format!("{scheme}://{host_path}"),
            None => url.to_string(),
        },
        None => url.to_string(),
    }
}

/// Which relative-depth evidence backend to run.
#[derive(Debug, Clone, PartialEq)]
pub enum DepthSpec {
    None,
    RelativeVlm {
        base_url: String,
        model: String,
        timeout: Duration,
    },
}

/// Which detection backend to run.
#[derive(Debug, Clone, PartialEq)]
pub enum DetectorSpec {
    /// Reports nothing; keeps the plumbing honest without a model.
    Null,
    /// Local ONNX YOLO (feature `yolo`).
    Yolo {
        model: PathBuf,
        labels: Vec<String>,
        input_size: usize,
        conf: f32,
    },
    /// Open-vocabulary grounding via an OpenAI-compatible sidecar (feature `vlm`).
    Vlm {
        base_url: String,
        model: String,
        targets: Vec<String>,
        timeout: Duration,
        /// `norm1000` (Qwen-VL convention) or `pixel`; parsed by the detector.
        coords: String,
    },
}

/// Everything needed to run one camera, as data.
#[derive(Debug, Clone)]
pub struct CameraTask {
    pub camera_id: String,
    /// `None` = this camera is fed by the webhook only (no local capture).
    pub source: Option<SourceSpec>,
    pub detector: DetectorSpec,
    pub depth: DepthSpec,
    pub detect_fps: f64,
    pub snapshot_dir: PathBuf,
    pub snapshots: SnapshotMode,
    /// Daemon-owned raw asset registry; `None` keeps standalone/test paths unchanged.
    pub raw_assets: Option<crate::assets::AssetStore>,
    /// Where this camera's appeared/disappeared timeline is appended, or `None`
    /// to run without one (docs/resident-ingest.md §6). A file rather than a
    /// handle so the daemon can hand the same path to every camera.
    pub events_path: Option<PathBuf>,
    /// Stop after this many frames; 0 = run until stopped.
    pub max_frames: u64,
    pub enabled: bool,
}

impl CameraTask {
    /// Interval between detections. Floored so a bogus `detect_fps` cannot spin
    /// the loop -- the historical loop used the same 0.05 floor.
    pub fn detect_interval(&self) -> Duration {
        Duration::from_secs_f64(1.0 / self.detect_fps.max(0.05))
    }
}

/// Can *this build* open that kind of source at all?
///
/// `false` is a configuration error, not a transient one: the daemon refuses to
/// start rather than retrying forever against a missing cargo feature.
pub fn source_supported(spec: &SourceSpec) -> bool {
    match spec {
        SourceSpec::Mock { .. } => true,
        SourceSpec::Rtsp { .. } => cfg!(feature = "rtsp"),
        SourceSpec::Webcam { .. } => cfg!(feature = "camera"),
    }
}

/// Build the frame source for a task.
///
/// Asking for a source whose cargo feature is off is a **hard error**: a
/// silently synthetic camera would be indistinguishable from a working one.
pub fn build_source(task: &CameraTask) -> Result<Box<dyn FrameSource>> {
    let spec = task
        .source
        .as_ref()
        .context("camera task has no frame source to open")?;
    match spec {
        SourceSpec::Mock {
            frames,
            width,
            height,
        } => Ok(Box::new(MockSource::new(
            task.camera_id.clone(),
            *frames,
            (*width, *height),
        ))),
        SourceSpec::Rtsp { url } => build_rtsp(task, url),
        SourceSpec::Webcam { index } => build_webcam(task, *index),
    }
}

#[cfg(feature = "rtsp")]
fn build_rtsp(task: &CameraTask, url: &str) -> Result<Box<dyn FrameSource>> {
    use crate::source::rtsp::RtspSource;
    Ok(Box::new(RtspSource::new(task.camera_id.clone(), url)?))
}

#[cfg(not(feature = "rtsp"))]
fn build_rtsp(_task: &CameraTask, _url: &str) -> Result<Box<dyn FrameSource>> {
    anyhow::bail!("rtsp source requires the 'rtsp' feature: rebuild with --features rtsp")
}

#[cfg(feature = "camera")]
fn build_webcam(task: &CameraTask, index: u32) -> Result<Box<dyn FrameSource>> {
    use crate::source::nokhwa::NokhwaSource;
    Ok(Box::new(NokhwaSource::new(task.camera_id.clone(), index)))
}

#[cfg(not(feature = "camera"))]
fn build_webcam(_task: &CameraTask, _index: u32) -> Result<Box<dyn FrameSource>> {
    anyhow::bail!("webcam source requires the 'camera' feature: rebuild with --features camera")
}

/// Build the detector for a spec.
///
/// A missing `yolo` feature degrades to `NullDetector` with a warning (the
/// pipeline still runs); a missing `vlm` feature hard-errors, because a
/// silently null VLM would be indistinguishable from a broken sidecar.
pub fn build_depth(spec: &DepthSpec) -> Result<Box<dyn RelativeDepthProvider>> {
    match spec {
        DepthSpec::None => Ok(Box::new(NoDepthProvider)),
        DepthSpec::RelativeVlm {
            base_url,
            model,
            timeout,
        } => build_relative_vlm(base_url, model, *timeout),
    }
}

#[cfg(feature = "relative-depth")]
fn build_relative_vlm(
    base_url: &str,
    model: &str,
    timeout: Duration,
) -> Result<Box<dyn RelativeDepthProvider>> {
    let provider = crate::depth::RelativeVlmProvider::new(base_url, model, timeout)
        .map_err(|e| anyhow::anyhow!("depth provider: {e}"))?;
    tracing::info!(base = %base_url, model = %model, timeout_s = timeout.as_secs(), "relative depth provider enabled");
    Ok(Box::new(provider))
}

#[cfg(not(feature = "relative-depth"))]
fn build_relative_vlm(
    _base_url: &str,
    _model: &str,
    _timeout: Duration,
) -> Result<Box<dyn RelativeDepthProvider>> {
    anyhow::bail!(
        "relative-vlm depth requires the 'relative-depth' feature: rebuild with --features relative-depth"
    )
}

/// Build the detector for a spec.
///
/// A missing `yolo` feature degrades to `NullDetector` with a warning (the
/// pipeline still runs); a missing `vlm` feature hard-errors, because a
/// silently null VLM would be indistinguishable from a broken sidecar.
pub fn build_detector(spec: &DetectorSpec) -> Result<Box<dyn Detector>> {
    match spec {
        DetectorSpec::Null => Ok(Box::new(NullDetector)),
        DetectorSpec::Yolo {
            model,
            labels,
            input_size,
            conf,
        } => build_yolo(model, labels, *input_size, *conf),
        DetectorSpec::Vlm {
            base_url,
            model,
            targets,
            timeout,
            coords,
        } => build_vlm(base_url, model, targets, *timeout, coords),
    }
}

#[cfg(feature = "yolo")]
fn build_yolo(
    model: &std::path::Path,
    labels: &[String],
    input_size: usize,
    conf: f32,
) -> Result<Box<dyn Detector>> {
    use crate::detector::yolo::YoloDetector;
    let det = YoloDetector::new(model, labels.to_vec(), input_size, conf)
        .map_err(|e| anyhow::anyhow!("model load: {e}"))?;
    tracing::info!(model = %model.display(), conf, "yolo detector enabled");
    Ok(Box::new(det))
}

#[cfg(not(feature = "yolo"))]
fn build_yolo(
    _model: &std::path::Path,
    _labels: &[String],
    _input_size: usize,
    _conf: f32,
) -> Result<Box<dyn Detector>> {
    tracing::warn!(
        "detector 'yolo' but built without the 'yolo' feature: camera loop runs \
         NullDetector (no observations); rebuild with --features yolo"
    );
    Ok(Box::new(NullDetector))
}

#[cfg(feature = "vlm")]
fn build_vlm(
    base_url: &str,
    model: &str,
    targets: &[String],
    timeout: Duration,
    coords: &str,
) -> Result<Box<dyn Detector>> {
    use crate::detector::vlm::{CoordMode, VlmGroundDetector};
    let coords: CoordMode = coords.parse().map_err(|e: String| anyhow::anyhow!("{e}"))?;
    let det = VlmGroundDetector::new(base_url, model, targets.to_vec(), timeout, coords)?;
    tracing::info!(
        base = %base_url,
        model = %model,
        ?targets,
        timeout_s = timeout.as_secs(),
        "vlm grounding detector enabled"
    );
    Ok(Box::new(det))
}

#[cfg(not(feature = "vlm"))]
fn build_vlm(
    _base_url: &str,
    _model: &str,
    _targets: &[String],
    _timeout: Duration,
    _coords: &str,
) -> Result<Box<dyn Detector>> {
    anyhow::bail!("detector 'vlm' requires the 'vlm' feature: rebuild with --features vlm")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(source: Option<SourceSpec>) -> CameraTask {
        CameraTask {
            camera_id: "test".into(),
            source,
            detector: DetectorSpec::Null,
            depth: DepthSpec::None,
            detect_fps: 1.0,
            snapshot_dir: PathBuf::from("unused"),
            snapshots: SnapshotMode::First,
            raw_assets: None,
            events_path: None,
            max_frames: 0,
            enabled: true,
        }
    }

    #[test]
    fn redact_url_strips_credentials_only() {
        assert_eq!(
            redact_url("rtsp://user:pass@192.168.1.64:554/Streaming/Channels/102"),
            "rtsp://192.168.1.64:554/Streaming/Channels/102"
        );
        assert_eq!(redact_url("rtsp://host/path"), "rtsp://host/path");
        assert_eq!(redact_url("not-a-url"), "not-a-url");
    }

    #[test]
    fn source_describe_never_leaks_credentials() {
        let spec = SourceSpec::Rtsp {
            url: "rtsp://user:pass@cam.local/stream".into(),
        };
        assert_eq!(spec.describe(), "rtsp://cam.local/stream");
        assert_eq!(spec.kind(), "rtsp");
        assert_eq!(SourceSpec::Webcam { index: 3 }.describe(), "#3");
    }

    #[test]
    fn detect_interval_is_floored() {
        let mut t = task(None);
        t.detect_fps = 2.0;
        assert_eq!(t.detect_interval(), Duration::from_millis(500));
        // A bogus/zero fps must not spin the loop.
        t.detect_fps = 0.0;
        assert_eq!(t.detect_interval(), Duration::from_secs_f64(1.0 / 0.05));
    }

    #[test]
    fn mock_source_and_null_detector_need_no_feature() {
        let t = task(Some(SourceSpec::Mock {
            frames: 1,
            width: 4,
            height: 4,
        }));
        let mut src = build_source(&t).expect("mock source builds featureless");
        assert!(src.next_frame().is_ok());

        let det = build_detector(&DetectorSpec::Null).expect("null detector builds");
        assert!(det.detect(&[0u8; 48], 4, 4).unwrap().is_empty());
    }

    #[test]
    fn a_task_without_a_source_is_an_error_not_a_silent_noop() {
        let err = match build_source(&task(None)) {
            Ok(_) => panic!("a task without a source must not build"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("no frame source"));
    }
}
