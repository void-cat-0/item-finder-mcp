//! item-core: shared domain model and persistence for the item-finders pipeline.
//!
//! Data flow: detections (per frame) -> observations (tracked object x zone,
//! deduplicated over a time window) -> queried via text search / VLM sidecar.

pub mod annotation_store;
pub mod annotations;
pub mod assets;
pub mod cameras;
pub mod evidence;
pub mod geo;
pub mod model;
pub mod store;

pub use model::{
    DepthEvidence, DepthInput, Detection, FrameMeta, GeometryInput, Observation, ObservationEvent,
    ObservationEventInput, ObservationGeometry, Region, SceneEvent, SightingInput, Trajectory,
};
