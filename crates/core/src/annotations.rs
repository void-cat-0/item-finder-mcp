//! Shared annotation wire contract, validation, and canonicalization.
//!
//! The daemon is the sole writer.  This module deliberately contains no HTTP,
//! filesystem, or daemon dependencies so MCP and ingest can share these types.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use thiserror::Error;

pub const ANNOTATION_SCHEMA_VERSION: u32 = 1;
pub const ANNOTATION_PROVENANCE: &str = "caller_visual_inference";
pub const MAX_ANNOTATIONS: usize = 128;
pub const MAX_REQUEST_ID_BYTES: usize = 128;
pub const MAX_LABEL_BYTES: usize = 256;
pub const MAX_OBJECT_ID_BYTES: usize = 64;
pub const MAX_NOTE_BYTES: usize = 1024;
pub const MAX_PRODUCER_TEXT_BYTES: usize = 256;

/// One object in a complete annotation revision.  `bbox` is normalized
/// `[x, y, width, height]`, with the source image's top-left as origin.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AnnotationInput {
    pub object_id: String,
    pub label: String,
    pub bbox: [f64; 4],
    #[serde(default)]
    pub confidence: Option<f64>,
    #[serde(default)]
    pub note: Option<String>,
}

/// Compatibility aliases used by callers that call an object a label.
pub type ImageAnnotation = AnnotationInput;
pub type AnnotationObject = AnnotationInput;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AnnotationProducer {
    pub kind: String,
    pub host: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub prompt_version: Option<String>,
}

/// Create a set, or append a complete revision to an existing set.  New sets
/// omit both `annotation_id` and `expected_revision`; revisions provide both.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AnnotationSubmitRequest {
    pub schema_version: u32,
    pub request_id: String,
    pub database_id: String,
    pub asset_id: String,
    pub sha256: String,
    pub camera_id: String,
    pub coordinate_system: String,
    pub producer: AnnotationProducer,
    pub annotations: Vec<AnnotationInput>,
    #[serde(default)]
    pub annotation_id: Option<String>,
    #[serde(default)]
    pub expected_revision: Option<u32>,
}

pub type ImageAnnotationRequest = AnnotationSubmitRequest;
pub type AnnotationRequest = AnnotationSubmitRequest;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AnnotationReceipt {
    pub status: String,
    pub annotation_id: String,
    pub revision: u32,
    pub request_id: String,
    pub database_id: String,
    pub source_asset_id: String,
    pub source_sha256: String,
    pub camera_id: String,
    pub evidence_asset_id: String,
    pub object_count: u32,
    pub provenance: String,
    pub persisted_at: String,
    pub expires_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AnnotationError {
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_revision: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AnnotationDetail {
    pub annotation_id: String,
    pub database_id: String,
    pub source_asset_id: String,
    pub source_sha256: String,
    pub camera_id: String,
    pub capture_time: String,
    pub observation_id: Option<i64>,
    pub evidence_asset_id: String,
    pub current_revision: u32,
    pub revision: u32,
    pub schema_version: u32,
    pub coordinate_system: String,
    pub producer: AnnotationProducer,
    pub annotations: Vec<AnnotationInput>,
    pub object_count: u32,
    pub payload_sha256: String,
    pub request_id: String,
    pub persisted_at: String,
    pub created_at: String,
    pub expires_at: String,
    pub provenance: String,
    pub original_source_status: String,
    pub evidence_status: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AnnotationSummary {
    pub annotation_id: String,
    pub database_id: String,
    pub source_asset_id: String,
    pub source_sha256: String,
    pub camera_id: String,
    pub evidence_asset_id: String,
    pub current_revision: u32,
    pub object_count: u32,
    pub evidence_status: String,
    pub provenance: String,
    pub created_at: String,
    pub expires_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AnnotationListFilter {
    pub database_id: String,
    #[serde(default)]
    pub camera_id: Option<String>,
    #[serde(default)]
    pub asset_id: Option<String>,
    #[serde(default = "default_annotation_limit")]
    pub limit: u32,
    #[serde(default)]
    pub cursor: Option<String>,
}

fn default_annotation_limit() -> u32 {
    20
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AnnotationListPage {
    pub items: Vec<AnnotationSummary>,
    pub next_cursor: Option<String>,
}

/// Metadata needed to register or reuse an immutable evidence JPEG.  The
/// daemon writes the file; core records its identity and lifecycle atomically.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnnotationEvidenceRegistration {
    pub database_id: String,
    pub evidence_asset_id: String,
    pub source_asset_id: String,
    pub source_sha256: String,
    pub camera_id: String,
    pub capture_time: String,
    pub observation_id: Option<i64>,
    pub sha256: String,
    pub bytes: u64,
    pub width: u32,
    pub height: u32,
    pub status: String,
    pub accepted_at: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AnnotationQuota {
    pub max_sets: u64,
    pub max_revisions: u64,
    pub max_metadata_bytes: u64,
    pub max_evidence_bytes: u64,
    pub max_tombstones: u64,
    pub retention: chrono::Duration,
    pub tombstone_retention: chrono::Duration,
}

impl Default for AnnotationQuota {
    fn default() -> Self {
        Self {
            max_sets: 10_000,
            max_revisions: 50_000,
            max_metadata_bytes: 512 * 1024 * 1024,
            max_evidence_bytes: 2 * 1024 * 1024 * 1024,
            max_tombstones: 100_000,
            retention: chrono::Duration::days(30),
            tombstone_retention: chrono::Duration::days(7),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AnnotationMaintenance {
    pub expired_sets: u64,
    pub expired_revisions: u64,
    pub deleted_tombstones: u64,
    pub orphaned_evidence: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ValidatedAnnotationRequest {
    pub request: AnnotationSubmitRequest,
    pub canonical_json: String,
    pub payload_sha256: String,
}

#[derive(Debug, Error)]
pub enum AnnotationValidationError {
    #[error("invalid annotation input: {0}")]
    Invalid(String),
    #[error("invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
}

impl AnnotationSubmitRequest {
    pub fn validate_and_canonicalize(
        &self,
    ) -> Result<ValidatedAnnotationRequest, AnnotationValidationError> {
        validate_request(self)?;
        let mut normalized = self.clone();
        normalized.sha256 = normalized.sha256.to_ascii_lowercase();
        for object in &mut normalized.annotations {
            for coordinate in &mut object.bbox {
                if *coordinate == 0.0 {
                    *coordinate = 0.0;
                }
            }
            if let Some(confidence) = object.confidence.as_mut()
                && *confidence == 0.0
            {
                *confidence = 0.0;
            }
        }
        let canonical_json = serde_json::to_string(&normalized)
            .map_err(|e| AnnotationValidationError::Invalid(e.to_string()))?;
        let payload_sha256 = hex_sha256(canonical_json.as_bytes());
        Ok(ValidatedAnnotationRequest {
            request: normalized,
            canonical_json,
            payload_sha256,
        })
    }
}

/// Parse an annotation request while rejecting duplicate keys at every JSON
/// object depth.  `serde_json::from_slice` alone accepts the last duplicate.
pub fn parse_annotation_request_json(
    bytes: &[u8],
) -> Result<ValidatedAnnotationRequest, AnnotationValidationError> {
    let strict = serde_json::from_slice::<StrictValue>(bytes)?;
    let request: AnnotationSubmitRequest = serde_json::from_value(strict.0)?;
    request.validate_and_canonicalize()
}

fn validate_request(request: &AnnotationSubmitRequest) -> Result<(), AnnotationValidationError> {
    let invalid = |message: &str| Err(AnnotationValidationError::Invalid(message.to_owned()));
    if request.schema_version != ANNOTATION_SCHEMA_VERSION {
        return invalid("unsupported schema_version");
    }
    if !valid_token(&request.request_id, MAX_REQUEST_ID_BYTES) {
        return invalid("request_id must be 1..128 ASCII letters, digits, '-' or '_'");
    }
    if request.database_id.is_empty() || request.asset_id.is_empty() || request.camera_id.is_empty()
    {
        return invalid("database_id, asset_id and camera_id are required");
    }
    if request.sha256.len() != 64 || !request.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
        return invalid("sha256 must be 64 hexadecimal characters");
    }
    if request.coordinate_system != "normalized_xywh" {
        return invalid("coordinate_system must be normalized_xywh");
    }
    if request.annotations.len() > MAX_ANNOTATIONS {
        return invalid("annotations must contain at most 128 objects");
    }
    if request.annotation_id.is_some() != request.expected_revision.is_some() {
        return invalid("annotation_id and expected_revision must be supplied together");
    }
    if let Some(id) = &request.annotation_id
        && (id.is_empty() || id.len() > 128 || !id.is_ascii())
    {
        return invalid("annotation_id is invalid");
    }
    if request.producer.kind != "vlm" || request.producer.host.is_empty() {
        return invalid("producer kind/host is invalid");
    }
    for value in [
        &request.producer.host,
        request.producer.model.as_deref().unwrap_or(""),
        request.producer.prompt_version.as_deref().unwrap_or(""),
    ] {
        if value.len() > MAX_PRODUCER_TEXT_BYTES || !value.is_char_boundary(value.len()) {
            return invalid("producer text is too long");
        }
    }
    for object in &request.annotations {
        if object.object_id.is_empty()
            || object.object_id.len() > MAX_OBJECT_ID_BYTES
            || !valid_object_id(&object.object_id)
        {
            return invalid("object_id is invalid");
        }
        if object.label.is_empty() || object.label.len() > MAX_LABEL_BYTES {
            return invalid("label is invalid");
        }
        if object
            .note
            .as_ref()
            .is_some_and(|v| v.len() > MAX_NOTE_BYTES)
        {
            return invalid("note is too long");
        }
        let [x, y, width, height] = object.bbox;
        if ![x, y, width, height].iter().all(|v| v.is_finite())
            || x < 0.0
            || y < 0.0
            || width <= 0.0
            || height <= 0.0
            || x + width > 1.0
            || y + height > 1.0
        {
            return invalid("bbox must be finite normalized_xywh inside the image");
        }
        if object
            .confidence
            .is_some_and(|v| !v.is_finite() || !(0.0..=1.0).contains(&v))
        {
            return invalid("confidence must be finite and between 0 and 1");
        }
    }
    let mut ids = HashSet::new();
    if request
        .annotations
        .iter()
        .any(|object| !ids.insert(&object.object_id))
    {
        return invalid("object_id must be unique within a revision");
    }
    Ok(())
}

fn valid_token(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}
fn valid_object_id(value: &str) -> bool {
    valid_token(value, MAX_OBJECT_ID_BYTES)
}
fn hex_sha256(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Small recursive serde value used solely to detect duplicate object keys.
struct StrictValue(Value);
impl<'de> Deserialize<'de> for StrictValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = StrictValue;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a JSON value")
            }
            fn visit_bool<E>(self, v: bool) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(StrictValue(Value::Bool(v)))
            }
            fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(StrictValue(Value::Number(v.into())))
            }
            fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(StrictValue(Value::Number(v.into())))
            }
            fn visit_f64<E>(self, v: f64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                serde_json::Number::from_f64(v)
                    .map(|n| StrictValue(Value::Number(n)))
                    .ok_or_else(|| E::custom("non-finite number"))
            }
            fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(StrictValue(Value::String(v.to_owned())))
            }
            fn visit_string<E>(self, v: String) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(StrictValue(Value::String(v)))
            }
            fn visit_none<E>(self) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(StrictValue(Value::Null))
            }
            fn visit_unit<E>(self) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(StrictValue(Value::Null))
            }
            fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                let mut out = Vec::new();
                while let Some(v) = seq.next_element::<StrictValue>()? {
                    out.push(v.0);
                }
                Ok(StrictValue(Value::Array(out)))
            }
            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                let mut out = Map::new();
                let mut keys = HashSet::new();
                while let Some(key) = map.next_key::<String>()? {
                    if !keys.insert(key.clone()) {
                        return Err(serde::de::Error::custom(format!(
                            "duplicate JSON key: {key}"
                        )));
                    }
                    out.insert(key, map.next_value::<StrictValue>()?.0);
                }
                Ok(StrictValue(Value::Object(out)))
            }
        }
        deserializer.deserialize_any(V)
    }
}
