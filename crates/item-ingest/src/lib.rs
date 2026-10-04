//! item-ingest: turn camera frames / webhook events into observations.
//!
//! Pipeline stages (each behind a trait, so the optional native backends can
//! be swapped independently):
//!   FrameSource  -> Detector -> (NMS) -> Store::record_sighting
//!
//! The Frigate webhook path skips FrameSource+Detector entirely: Frigate has
//! already done decode+detect, we only deduplicate and persist its events.

pub mod annotate;
pub mod assets;
pub mod config;
pub mod daemon;
pub mod depth;
pub mod detector;
pub mod events;
pub mod frame_cache;
pub mod frigate;
pub mod health;
pub mod lock;
#[cfg(feature = "rtsp")]
pub mod preview;
pub mod retention;
pub mod runner;
pub mod runtime;
pub mod source;
pub mod supervisor;

use chrono::Utc;
use std::io::Write;
use thiserror::Error;

use item_core::geo::nms;
use item_core::store::Store;
use item_core::{DepthInput, Detection, FrameMeta, GeometryInput, Region, SightingInput};

#[derive(Debug, Error)]
pub enum IngestError {
    #[error(transparent)]
    Store(#[from] item_core::store::StoreError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, IngestError>;

/// What one surviving detection did to the store: the row it landed on, its
/// zone/label (the store's dedup identity), whether the row was opened by this
/// sighting, and the row's hit count after the merge.
///
/// This is what the event timeline needs and what used to be thrown away: the
/// `(id, is_new)` pair existed only to name the snapshot file, and callers saw
/// a bare count. `hits` is reported as of this sighting, so a later
/// disappearance can say how often the object was seen before it went
/// (docs/resident-ingest.md §6).
#[derive(Debug, Clone, PartialEq)]
pub struct Recorded {
    pub obs_id: i64,
    pub zone: String,
    pub label: String,
    pub is_new: bool,
    pub hits: i64,
    /// The surviving detector geometry that produced this sighting.
    pub bbox: [f32; 4],
    pub frame_width: u32,
    pub frame_height: u32,
    pub captured_at: chrono::DateTime<chrono::Utc>,
    pub geometry_id: i64,
}

/// Persist one frame's detections: NMS them, map centers to zones, record.
/// When `frame_rgb` + `snapshots_dir` are provided, snapshots are written
/// according to `snapshot_mode`: `Off` = none, `First` = one per new observation,
/// `Events` = First + one per event (written by runner), `Detections` = Events +
/// one per detection. Boxes live in pixels only; the database stays detection-free.
///
/// Returns one [`Recorded`] per surviving detection, in NMS order -- the count
/// is `len()`, and the identities are what the event log is built from.
pub fn ingest_detections(
    store: &impl StoreAccess,
    meta: &FrameMeta,
    dets: &[Detection],
    nms_iou_threshold: f32,
    min_confidence: f32,
    frame_rgb: Option<(&[u8], &std::path::Path)>,
    snapshot_mode: crate::config::SnapshotMode,
) -> Result<Vec<Recorded>> {
    ingest_detections_with_depth(
        store,
        meta,
        dets,
        nms_iou_threshold,
        min_confidence,
        frame_rgb,
        snapshot_mode,
        &[],
        None,
        None,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn ingest_detections_with_depth(
    store: &impl StoreAccess,
    meta: &FrameMeta,
    dets: &[Detection],
    nms_iou_threshold: f32,
    min_confidence: f32,
    frame_rgb: Option<(&[u8], &std::path::Path)>,
    snapshot_mode: crate::config::SnapshotMode,
    depth: &[crate::depth::DepthEstimate],
    depth_provider: Option<&str>,
    depth_model: Option<&str>,
    raw_assets: Option<&crate::assets::AssetStore>,
) -> Result<Vec<Recorded>> {
    // Detectors regress boxes past the frame edge for objects touching it
    // (YOLO's cx±w/2 routinely does), and the store rightly refuses geometry
    // outside the frame -- so clip here, once, before NMS/zones/geometry see
    // it. A box with nothing left inside the frame is dropped.
    let (fw, fh) = (meta.width as f32, meta.height as f32);
    let (owned_indices, owned): (Vec<usize>, Vec<Detection>) = dets
        .iter()
        .enumerate()
        .filter(|(_, d)| d.confidence >= min_confidence)
        .filter_map(|(i, d)| {
            let [x0, y0, x1, y1] = d.bbox;
            let bbox = [
                x0.clamp(0.0, fw),
                y0.clamp(0.0, fh),
                x1.clamp(0.0, fw),
                y1.clamp(0.0, fh),
            ];
            let usable =
                bbox.iter().all(|v| v.is_finite()) && bbox[2] > bbox[0] && bbox[3] > bbox[1];
            usable.then(|| (i, Detection { bbox, ..d.clone() }))
        })
        .unzip();
    let keep = nms(&owned, nms_iou_threshold);
    let survivors: Vec<&Detection> = keep.iter().map(|&i| &owned[i]).collect();
    // Zones come from the DB and only matter for annotated snapshots, so the
    // query stays on the snapshot-providing path.
    let regions: Vec<Region> = match frame_rgb {
        Some(_) => store.with_store(|s| s.regions_for(&meta.camera_id))?,
        None => Vec::new(),
    };

    let mut recorded = Vec::with_capacity(keep.len());
    for (hi, idx) in keep.into_iter().enumerate() {
        let det = &owned[idx];
        let zone = store.with_store(|s| s.zone_for_point(&meta.camera_id, det.center()))?;
        let source_index = owned_indices[idx];
        let depth_input = depth
            .iter()
            .find(|e| e.index == source_index)
            .and_then(|estimate| {
                depth_provider.map(|provider| DepthInput {
                    provider: provider.to_string(),
                    frame_id: estimate.frame_id.clone(),
                    response_id: estimate.response_id.clone(),
                    prompt_version: estimate.prompt_version.clone(),
                    backend_version: estimate.backend_version.clone(),
                    mode: "relative".into(),
                    relation_to_camera: estimate.relation_to_camera.as_str().into(),
                    relative_score: estimate.relative_score,
                    quality: estimate.quality.as_str().into(),
                    model_ref: depth_model.map(str::to_string),
                    value_m: None,
                    uncertainty_m: None,
                    valid_fraction: None,
                    coordinate_frame: None,
                    calibration_ref: None,
                })
            });
        let input = SightingInput {
            camera_id: meta.camera_id.clone(),
            zone: zone.clone(),
            label: det.label.clone(),
            seen_at: meta.captured_at,
            snapshot: meta.snapshot_path.clone(),
            window_seconds: item_core::store::DEFAULT_DEDUP_WINDOW.as_secs(),
            geometry: Some(GeometryInput {
                sample_kind: "last".into(),
                captured_at: meta.captured_at,
                bbox: det.bbox,
                frame_width: Some(meta.width),
                frame_height: Some(meta.height),
                zone: zone.clone(),
                source: "local_detector".into(),
                confidence: Some(det.confidence),
                snapshot_ref: meta.snapshot_path.clone(),
            }),
            depth: depth_input,
        };
        let (id, is_new, hits, geometry_id) =
            store.with_store(|s| s.record_sighting_with_geometry(&input))?;
        // The typed sighting transaction owns both bounded samples and depth;
        // the returned id identifies this input, even when it arrived late.
        let geometry_id = geometry_id.ok_or_else(|| {
            item_core::store::StoreError::InvalidInput(
                "sighting did not return input geometry".into(),
            )
        })?;
        // Raw assets are daemon-owned and never use the annotated snapshot path.
        // They are written once for a newly opened observation and registered only
        // after the JPEG has been fully encoded.
        if let Some(registry) = raw_assets
            && is_new
            && let Some((rgb, _)) = frame_rgb
            && let Some(owner) = store.with_store(|s| s.observation_by_id(id))?
            && let Err(error) =
                registry.publish_rgb(&owner, meta.captured_at, rgb, meta.width, meta.height)
        {
            tracing::warn!(error = %error, obs = id, "raw asset publication failed");
        }

        // Snapshot writing controlled by mode: Off = none, First = new obs only,
        // Events = new obs (runner writes event snapshots), Detections = every sighting.
        if let Some((rgb, dir)) = frame_rgb {
            use crate::config::SnapshotMode;
            let should_write = match snapshot_mode {
                SnapshotMode::Off => false,
                SnapshotMode::First => is_new,
                SnapshotMode::Events => is_new,
                SnapshotMode::Detections => true,
            };

            if should_write {
                match annotate::annotate(
                    rgb,
                    meta.width,
                    meta.height,
                    &survivors,
                    &regions,
                    Some(hi),
                ) {
                    Some(img) => {
                        let filename = match snapshot_mode {
                            SnapshotMode::Detections => format!("{id}-g{geometry_id}.jpg"),
                            _ => format!("{id}.jpg"),
                        };
                        match write_snapshot(dir, &filename, &img) {
                            Ok(rel) => {
                                if snapshot_mode == SnapshotMode::First {
                                    store.with_store(|s| s.set_sample_snapshot(id, &rel))?;
                                }
                            }
                            Err(e) => tracing::warn!(error = %e, obs = id, "snapshot write failed"),
                        }
                    }
                    None => {
                        tracing::warn!(obs = id, "snapshot skipped: rgb buffer shorter than frame")
                    }
                }
            }
        }
        recorded.push(Recorded {
            obs_id: id,
            zone,
            label: det.label.clone(),
            is_new,
            hits,
            bbox: det.bbox,
            frame_width: meta.width,
            frame_height: meta.height,
            captured_at: meta.captured_at,
            geometry_id,
        });
    }
    tracing::debug!(camera = %meta.camera_id, recorded = recorded.len(), "frame ingested");
    Ok(recorded)
}

/// Encode an (already annotated) RGB image -> JPEG at `{dir}/{filename}`,
/// return the stored path string.
fn write_snapshot(
    dir: &std::path::Path,
    filename: &str,
    img: &image::RgbImage,
) -> std::io::Result<String> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(filename);
    let mut out = std::io::BufWriter::new(std::fs::File::create(&path)?);
    let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 75);
    enc.encode(
        img,
        img.width(),
        img.height(),
        image::ExtendedColorType::Rgb8,
    )
    .map_err(|e| std::io::Error::other(e.to_string()))?;
    out.flush()?;
    Ok(path.display().to_string())
}

/// Convenience: shared handle so the webhook server and a camera loop can both
/// write. See `frigate::State` (rusqlite::Connection is not Sync).
pub type SharedStore = std::sync::Arc<std::sync::Mutex<Store>>;

/// Borrow only for database work, never frame decoding or raw JPEG publication.
pub trait StoreAccess {
    fn with_store<T>(&self, operation: impl FnOnce(&Store) -> T) -> T;
}
impl StoreAccess for Store {
    fn with_store<T>(&self, operation: impl FnOnce(&Store) -> T) -> T {
        operation(self)
    }
}
impl StoreAccess for SharedStore {
    fn with_store<T>(&self, operation: impl FnOnce(&Store) -> T) -> T {
        operation(&self.lock().expect("store mutex poisoned"))
    }
}

pub fn now() -> chrono::DateTime<Utc> {
    Utc::now()
}

#[cfg(test)]
mod tests {
    use super::annotate::palette_color;
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static TEMP_SEQ: AtomicUsize = AtomicUsize::new(0);

    /// Closed-loop smoke: a new observation's snapshot must exist and carry
    /// the burned-in box color at the box's top edge (JPEG q75 bleeds a
    /// little, so compare with tolerance instead of exact equality).
    #[test]
    fn snapshot_written_with_burned_in_boxes() {
        let dir = std::env::temp_dir().join(format!(
            "item-ingest-annot-{}-{}",
            std::process::id(),
            TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let store = Store::in_memory().unwrap();
        store
            .upsert_region("cam", "desk", [4.0, 4.0, 44.0, 44.0])
            .unwrap();
        let meta = FrameMeta {
            camera_id: "cam".into(),
            captured_at: now(),
            width: 64,
            height: 48,
            snapshot_path: None,
        };
        let dets = [Detection {
            label: "bottle".into(),
            confidence: 0.9,
            bbox: [8.0, 8.0, 40.0, 32.0],
        }];
        let rgb = vec![128u8; 64 * 48 * 3];

        let recorded = ingest_detections(
            &store,
            &meta,
            &dets,
            0.45,
            0.3,
            Some((&rgb, dir.as_path())),
            crate::config::SnapshotMode::First,
        )
        .unwrap();
        assert_eq!(recorded.len(), 1);
        assert_eq!(
            recorded[0],
            Recorded {
                obs_id: 1,
                zone: "desk".into(),
                label: "bottle".into(),
                is_new: true,
                hits: 1,
                bbox: [8.0, 8.0, 40.0, 32.0],
                frame_width: 64,
                frame_height: 48,
                captured_at: meta.captured_at,
                geometry_id: 2,
            },
            "the row identity the caller used to lose"
        );

        let obs = store.recent(None, 10).unwrap();
        assert_eq!(obs.len(), 1);
        assert_eq!(obs[0].zone, "desk");
        let snap = obs[0].sample_snapshot.clone().expect("snapshot attached");
        let img = image::open(&snap).unwrap().to_rgb8();

        let want = palette_color("bottle");
        let got = *img.get_pixel(24, 8); // middle of the box top edge
        let near =
            |a: image::Rgb<u8>, b: image::Rgb<u8>| (0..3).all(|i| a.0[i].abs_diff(b.0[i]) < 60);
        assert!(
            near(got, want),
            "box edge pixel {got:?} should look like {want:?}"
        );
        assert!(
            got.0.iter().any(|d| d.abs_diff(128) > 60),
            "edge pixel must differ from flat background"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A box regressed past the frame edge used to reach the store as-is,
    /// which refuses out-of-frame geometry -- and that error stopped the
    /// camera on the first object touching the edge.
    #[test]
    fn a_box_past_the_frame_edge_is_clipped_not_fatal() {
        let store = Store::in_memory().unwrap();
        let meta = FrameMeta {
            camera_id: "cam".into(),
            captured_at: now(),
            width: 64,
            height: 48,
            snapshot_path: None,
        };
        let dets = [
            Detection {
                label: "chair".into(),
                confidence: 0.9,
                bbox: [-3.5, 20.0, 70.25, 52.0],
            },
            Detection {
                label: "cup".into(),
                confidence: 0.9,
                bbox: [66.0, 10.0, 80.0, 20.0],
            },
        ];

        let recorded = ingest_detections(
            &store,
            &meta,
            &dets,
            0.45,
            0.3,
            None,
            crate::config::SnapshotMode::First,
        )
        .unwrap();

        assert_eq!(
            recorded.len(),
            1,
            "the box wholly outside the frame is dropped"
        );
        assert_eq!(recorded[0].label, "chair");
        assert_eq!(recorded[0].bbox, [0.0, 20.0, 64.0, 48.0]);
        let stored = store
            .geometry_by_id(recorded[0].geometry_id)
            .unwrap()
            .expect("geometry persisted");
        assert_eq!(stored.bbox, [0.0, 20.0, 64.0, 48.0]);
    }
}
