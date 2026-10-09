//! Authenticated annotation protocol adapter. Never opens a writable database.
use std::collections::BTreeSet;
use std::sync::Arc;

use item_core::annotations::{
    AnnotationDetail, AnnotationInput, AnnotationListPage, AnnotationProducer, AnnotationReceipt,
    AnnotationSubmitRequest,
};
use rmcp::{model::CallToolResult, schemars};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::images::{DaemonClient, bounded_body, valid_asset_id, valid_camera_id};
use crate::query::tool_error;

const MAX_READ: usize = 512 * 1024;
const MAX_WRITE: usize = 256 * 1024;
const MAX_REVISION: u32 = 100;

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SubmitRequest {
    pub schema_version: u32,
    /// Dataset-scoped idempotency key. Retry uncertain results with this same key and payload.
    pub request_id: String,
    pub asset_id: String,
    pub sha256: String,
    pub camera_id: String,
    pub coordinate_system: String,
    pub producer: AnnotationProducer,
    pub annotations: Vec<AnnotationInput>,
    pub annotation_id: Option<String>,
    pub expected_revision: Option<u32>,
}

impl SubmitRequest {
    pub fn into_internal(self, database_id: String) -> AnnotationSubmitRequest {
        AnnotationSubmitRequest {
            schema_version: self.schema_version,
            database_id,
            request_id: self.request_id,
            asset_id: self.asset_id,
            sha256: self.sha256,
            camera_id: self.camera_id,
            coordinate_system: self.coordinate_system,
            producer: self.producer,
            annotations: self.annotations,
            annotation_id: self.annotation_id,
            expected_revision: self.expected_revision,
        }
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetRequest {
    pub annotation_id: String,
    pub revision: Option<u32>,
    pub camera_id: Option<String>,
}
impl GetRequest {
    pub fn validate(&self) -> Result<(), CallToolResult> {
        if !valid_annotation_id(&self.annotation_id)
            || self
                .revision
                .is_some_and(|r| !(1..=MAX_REVISION).contains(&r))
            || self
                .camera_id
                .as_deref()
                .is_some_and(|s| !valid_camera_id(s))
        {
            return Err(tool_error(
                "invalid_input",
                "Provide a valid annotation ID, revision 1–100 and exact camera guard.",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListRequest {
    pub camera_id: Option<String>,
    /// Exact original source asset ID, not the independently retained evidence ID.
    pub asset_id: Option<String>,
    pub limit: Option<u32>,
    /// Opaque cursor tied to the dataset and filters. Pagination is not a snapshot.
    pub cursor: Option<String>,
}
impl ListRequest {
    pub fn validate(&self) -> Result<(), CallToolResult> {
        if self
            .camera_id
            .as_deref()
            .is_some_and(|s| !valid_camera_id(s))
            || self.asset_id.as_deref().is_some_and(|s| !valid_asset_id(s))
            || !(1..=50).contains(&self.limit.unwrap_or(20))
            || self.cursor.as_deref().is_some_and(|s| !valid_cursor(s))
        {
            return Err(tool_error(
                "invalid_input",
                "Provide exact camera/source filters, limit 1–50 and a bounded opaque cursor.",
            ));
        }
        Ok(())
    }
}

pub fn output_schema<T: schemars::JsonSchema + 'static>() -> Arc<rmcp::model::JsonObject> {
    let mut success = (*rmcp::handler::server::tool::schema_for_output::<T>()).clone();
    let defs = success.remove("$defs");
    let mut schema = json!({"type":"object", "oneOf":[success, {
        "type":"object", "additionalProperties":false, "required":["error"],
        "properties":{"error":{"type":"object", "additionalProperties":false,
            "required":["code","message"], "properties":{
                "code":{"type":"string"},"message":{"type":"string"},
                "current_revision":{"type":"integer","minimum":1,"maximum":100}
            }}}
    }]})
    .as_object()
    .unwrap()
    .clone();
    if let Some(defs) = defs {
        schema.insert("$defs".into(), defs);
    }
    Arc::new(schema)
}

impl DaemonClient {
    pub async fn submit_annotation_at(
        &self,
        mut request: AnnotationSubmitRequest,
        deadline: tokio::time::Instant,
    ) -> Result<CallToolResult, CallToolResult> {
        validate_submit(&request)?;
        request = request
            .validate_and_canonicalize()
            .map_err(|_| tool_error("invalid_input", "Invalid annotation contract."))?
            .request;
        let body = serde_json::to_vec(&request).map_err(|_| invalid_response())?;
        if body.len() > MAX_WRITE {
            return Err(tool_error(
                "payload_too_large",
                "Annotation body exceeds 256 KiB.",
            ));
        }
        let _permit = self.annotation_permits.try_acquire().map_err(|_| {
            tool_error(
                "busy",
                "Annotation submission is busy. Retry later with the same request_id.",
            )
        })?;
        let work = async {
            let response = self
                .annotation_request(
                    reqwest::Method::POST,
                    &["internal", "v1", "annotations"],
                    true,
                )?
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body)
                .send()
                .await
                .map_err(|_| uncertain_error())?;
            let status = response.status();
            let bytes = bounded_body(response, MAX_READ)
                .await
                .map_err(uncertain_context)?;
            if !matches!(status.as_u16(), 200 | 201) {
                return Err(annotation_error(status.as_u16(), &bytes, true));
            }
            let receipt: AnnotationReceipt =
                serde_json::from_slice(&bytes).map_err(|_| invalid_write_response())?;
            validate_receipt(&receipt, &request).map_err(|_| invalid_write_response())?;
            structured(receipt)
        };
        let result = tokio::time::timeout_at(deadline, work)
            .await
            .unwrap_or_else(|_| Err(uncertain_error()));
        if tokio::time::Instant::now() >= deadline {
            return Err(uncertain_error());
        }
        result
    }

    pub async fn get_annotation_at(
        &self,
        request: GetRequest,
        database_id: String,
        deadline: tokio::time::Instant,
    ) -> Result<CallToolResult, CallToolResult> {
        request.validate()?;
        let work = async {
            let mut query = vec![("database_id", database_id.clone())];
            if let Some(r) = request.revision {
                query.push(("revision", r.to_string()));
            }
            if let Some(camera) = &request.camera_id {
                query.push(("camera_id", camera.clone()));
            }
            let response = self
                .annotation_request(
                    reqwest::Method::GET,
                    &["internal", "v1", "annotations", &request.annotation_id],
                    false,
                )?
                .query(&query)
                .send()
                .await
                .map_err(|_| read_timeout_error())?;
            let status = response.status();
            let bytes = bounded_body(response, MAX_READ).await?;
            if status.as_u16() != 200 {
                return Err(annotation_error(status.as_u16(), &bytes, false));
            }
            let detail: AnnotationDetail =
                serde_json::from_slice(&bytes).map_err(|_| invalid_response())?;
            validate_detail(&detail, &request, &database_id)?;
            structured(detail)
        };
        let result = tokio::time::timeout_at(deadline, work)
            .await
            .unwrap_or_else(|_| Err(read_timeout_error()));
        if tokio::time::Instant::now() >= deadline {
            return Err(read_timeout_error());
        }
        result
    }

    pub async fn list_annotations_at(
        &self,
        request: ListRequest,
        database_id: String,
        deadline: tokio::time::Instant,
    ) -> Result<CallToolResult, CallToolResult> {
        request.validate()?;
        let work = async {
            let mut query = vec![
                ("database_id", database_id.clone()),
                ("limit", request.limit.unwrap_or(20).to_string()),
            ];
            if let Some(camera) = &request.camera_id {
                query.push(("camera_id", camera.clone()));
            }
            if let Some(asset) = &request.asset_id {
                query.push(("asset_id", asset.clone()));
            }
            if let Some(cursor) = &request.cursor {
                query.push(("cursor", cursor.clone()));
            }
            let response = self
                .annotation_request(
                    reqwest::Method::GET,
                    &["internal", "v1", "annotations"],
                    false,
                )?
                .query(&query)
                .send()
                .await
                .map_err(|_| read_timeout_error())?;
            let status = response.status();
            let bytes = bounded_body(response, MAX_READ).await?;
            if status.as_u16() != 200 {
                return Err(annotation_error(status.as_u16(), &bytes, false));
            }
            let page: AnnotationListPage =
                serde_json::from_slice(&bytes).map_err(|_| invalid_response())?;
            validate_page(&page, &request, &database_id)?;
            structured(page)
        };
        let result = tokio::time::timeout_at(deadline, work)
            .await
            .unwrap_or_else(|_| Err(read_timeout_error()));
        if tokio::time::Instant::now() >= deadline {
            return Err(read_timeout_error());
        }
        result
    }
}

fn structured<T: Serialize>(value: T) -> Result<CallToolResult, CallToolResult> {
    Ok(CallToolResult::structured(
        serde_json::to_value(value).map_err(|_| invalid_response())?,
    ))
}

// Supplemental protocol checks also defend against malformed daemon successes.
pub fn validate_submit(r: &AnnotationSubmitRequest) -> Result<(), CallToolResult> {
    if r.schema_version != 1
        || r.coordinate_system != "normalized_xywh"
        || !valid_key(&r.request_id, 128)
        || !valid_asset_id(&r.asset_id)
        || !valid_hash(&r.sha256, false)
        || !valid_camera_id(&r.camera_id)
        || r.annotation_id
            .as_deref()
            .is_some_and(|s| !valid_annotation_id(s))
        || r.annotation_id.is_some() != r.expected_revision.is_some()
        || r.expected_revision
            .is_some_and(|n| !(1..MAX_REVISION).contains(&n))
        || !valid_producer(&r.producer)
        || !valid_objects(&r.annotations)
    {
        return Err(tool_error(
            "invalid_input",
            "Invalid annotation schema, source, revision, producer or object batch.",
        ));
    }
    Ok(())
}
fn valid_producer(p: &AnnotationProducer) -> bool {
    p.kind == "vlm"
        && !p.host.trim().is_empty()
        && p.host.len() <= 256
        && p.model.as_ref().is_none_or(|s| s.len() <= 256)
        && p.prompt_version.as_ref().is_none_or(|s| s.len() <= 256)
}
fn valid_objects(objects: &[AnnotationInput]) -> bool {
    let mut ids = BTreeSet::new();
    objects.len() <= 128
        && objects.iter().all(|o| {
            let [x, y, w, h] = o.bbox;
            valid_key(&o.object_id, 64)
                && ids.insert(&o.object_id)
                && !o.label.trim().is_empty()
                && o.label.len() <= 256
                && o.note.as_ref().is_none_or(|s| s.len() <= 1024)
                && o.confidence
                    .is_none_or(|n| n.is_finite() && (0.0..=1.0).contains(&n))
                && o.bbox.iter().all(|n| n.is_finite())
                && x >= 0.0
                && y >= 0.0
                && w > 0.0
                && h > 0.0
                && x + w <= 1.0
                && y + h <= 1.0
        })
}
fn valid_key(s: &str, max: usize) -> bool {
    !s.is_empty()
        && s.len() <= max
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
}
fn valid_annotation_id(s: &str) -> bool {
    s.len() == 36
        && s.bytes().enumerate().all(|(i, c)| {
            if [8, 13, 18, 23].contains(&i) {
                c == b'-'
            } else {
                c.is_ascii_hexdigit() && !c.is_ascii_uppercase()
            }
        })
}
fn valid_cursor(s: &str) -> bool {
    valid_key(s, 4096)
}
fn valid_hash(s: &str, lower: bool) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|c| c.is_ascii_hexdigit() && (!lower || !c.is_ascii_uppercase()))
}
fn valid_evidence_id(s: &str) -> bool {
    valid_asset_id(s)
        && s.strip_prefix("evidence-v1-").is_some_and(|s| {
            !s.is_empty()
                && s.bytes()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        })
}
fn time(s: &str) -> Result<chrono::DateTime<chrono::FixedOffset>, CallToolResult> {
    if s.len() > 64 {
        return Err(invalid_response());
    }
    chrono::DateTime::parse_from_rfc3339(s).map_err(|_| invalid_response())
}
fn times(created: &str, persisted: &str, expires: &str) -> Result<(), CallToolResult> {
    let created = time(created)?;
    let persisted = time(persisted)?;
    let expires = time(expires)?;
    if created > persisted
        || persisted >= expires
        || persisted > chrono::Utc::now() + chrono::Duration::seconds(1)
        || expires <= chrono::Utc::now()
    {
        return Err(invalid_response());
    }
    Ok(())
}
fn validate_receipt(
    r: &AnnotationReceipt,
    request: &AnnotationSubmitRequest,
) -> Result<(), CallToolResult> {
    if r.status != "persisted"
        || !valid_annotation_id(&r.annotation_id)
        || r.database_id != request.database_id
        || r.source_asset_id != request.asset_id
        || r.source_sha256 != request.sha256
        || !valid_hash(&r.source_sha256, true)
        || r.camera_id != request.camera_id
        || r.request_id != request.request_id
        || r.object_count as usize != request.annotations.len()
        || r.provenance != "caller_visual_inference"
        || !valid_evidence_id(&r.evidence_asset_id)
        || r.revision != request.expected_revision.map_or(1, |n| n + 1)
        || request
            .annotation_id
            .as_ref()
            .is_some_and(|id| id != &r.annotation_id)
    {
        return Err(invalid_response());
    }
    times(&r.persisted_at, &r.persisted_at, &r.expires_at)
}
fn validate_detail(
    d: &AnnotationDetail,
    request: &GetRequest,
    database_id: &str,
) -> Result<(), CallToolResult> {
    if d.annotation_id != request.annotation_id
        || d.database_id != database_id
        || request
            .camera_id
            .as_ref()
            .is_some_and(|c| c != &d.camera_id)
        || d.revision != request.revision.unwrap_or(d.current_revision)
        || !(1..=MAX_REVISION).contains(&d.current_revision)
        || d.revision == 0
        || d.revision > d.current_revision
        || d.schema_version != 1
        || d.coordinate_system != "normalized_xywh"
        || !valid_key(&d.request_id, 128)
        || !valid_hash(&d.payload_sha256, true)
        || !valid_asset_id(&d.source_asset_id)
        || !valid_hash(&d.source_sha256, true)
        || !valid_camera_id(&d.camera_id)
        || !valid_evidence_id(&d.evidence_asset_id)
        || d.provenance != "caller_visual_inference"
        || !valid_producer(&d.producer)
        || !valid_objects(&d.annotations)
        || d.object_count as usize != d.annotations.len()
        || d.observation_id.is_some_and(|n| n <= 0)
        || !matches!(
            d.evidence_status.as_str(),
            "available" | "missing" | "integrity_error"
        )
        || !matches!(
            d.original_source_status.as_str(),
            "available" | "missing" | "source_expired" | "integrity_error"
        )
    {
        return Err(invalid_response());
    }
    if time(&d.capture_time)? > time(&d.created_at)? {
        return Err(invalid_response());
    }
    times(&d.created_at, &d.persisted_at, &d.expires_at)?;
    let payload = AnnotationSubmitRequest {
        schema_version: d.schema_version,
        database_id: d.database_id.clone(),
        request_id: d.request_id.clone(),
        asset_id: d.source_asset_id.clone(),
        sha256: d.source_sha256.clone(),
        camera_id: d.camera_id.clone(),
        coordinate_system: d.coordinate_system.clone(),
        producer: d.producer.clone(),
        annotations: d.annotations.clone(),
        annotation_id: (d.revision > 1).then(|| d.annotation_id.clone()),
        expected_revision: (d.revision > 1).then(|| d.revision - 1),
    };
    let validated = payload
        .validate_and_canonicalize()
        .map_err(|_| invalid_response())?;
    if validated.payload_sha256 != d.payload_sha256 {
        return Err(invalid_response());
    }
    Ok(())
}
fn validate_page(
    page: &AnnotationListPage,
    request: &ListRequest,
    database_id: &str,
) -> Result<(), CallToolResult> {
    if page.items.len() > request.limit.unwrap_or(20) as usize
        || page
            .next_cursor
            .as_deref()
            .is_some_and(|s| !valid_cursor(s))
        || page.next_cursor.is_some()
            && (page.items.is_empty() || page.next_cursor == request.cursor)
    {
        return Err(invalid_response());
    }
    let mut ids = BTreeSet::new();
    let mut previous: Option<(chrono::DateTime<chrono::FixedOffset>, &str)> = None;
    for s in &page.items {
        if !ids.insert(&s.annotation_id)
            || !valid_annotation_id(&s.annotation_id)
            || s.database_id != database_id
            || !valid_camera_id(&s.camera_id)
            || request
                .camera_id
                .as_ref()
                .is_some_and(|c| c != &s.camera_id)
            || request
                .asset_id
                .as_ref()
                .is_some_and(|a| a != &s.source_asset_id)
            || !valid_asset_id(&s.source_asset_id)
            || !valid_hash(&s.source_sha256, true)
            || !valid_evidence_id(&s.evidence_asset_id)
            || s.provenance != "caller_visual_inference"
            || !(1..=MAX_REVISION).contains(&s.current_revision)
            || s.object_count > 128
            || !matches!(
                s.evidence_status.as_str(),
                "available" | "missing" | "integrity_error"
            )
        {
            return Err(invalid_response());
        }
        times(&s.created_at, &s.created_at, &s.expires_at)?;
        let current = (time(&s.created_at)?, s.annotation_id.as_str());
        if previous.is_some_and(|p| p.0 < current.0 || p.0 == current.0 && p.1 >= current.1) {
            return Err(invalid_response());
        }
        previous = Some(current);
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ErrorBody {
    error: ErrorDetail,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ErrorDetail {
    code: String,
    message: String,
    #[serde(default)]
    current_revision: Option<u32>,
}
fn annotation_error(status: u16, bytes: &[u8], write: bool) -> CallToolResult {
    let Ok(body) = serde_json::from_slice::<ErrorBody>(bytes) else {
        return if write {
            invalid_write_response()
        } else {
            invalid_response()
        };
    };
    if body.error.message.len() > 1024
        || body
            .error
            .current_revision
            .is_some_and(|n| !(1..=MAX_REVISION).contains(&n))
    {
        return invalid_response();
    }
    let (expected, message) = match body.error.code.as_str() {
        "invalid_input" => (400, "Invalid annotation input or cursor."),
        "unauthorized" => (401, "Daemon annotation access was denied."),
        "forbidden" | "annotation_disabled" => {
            (403, "Daemon annotation submission is not permitted.")
        }
        "dataset_mismatch" => (
            409,
            "The daemon and query database do not share the same dataset.",
        ),
        "camera_mismatch" => (409, "The annotation source belongs to another camera."),
        "source_hash_mismatch" => (409, "The annotation source hash does not match."),
        "idempotency_conflict" => (
            409,
            "This request_id was already used with a different payload.",
        ),
        "revision_conflict" => (
            409,
            "The annotation revision changed; inspect current_revision.",
        ),
        "integrity_error" => (409, "Annotation evidence failed integrity validation."),
        "quota_exceeded" => (409, "Annotation retention quota is exhausted."),
        "not_found" => (
            404,
            "The requested annotation, revision or source was not found.",
        ),
        "source_expired" | "annotation_expired" => {
            (410, "The requested source or annotation has expired.")
        }
        "payload_too_large" => (413, "The annotation payload exceeds the allowed size."),
        "busy" => (503, "The daemon annotation service is busy."),
        "schema_unavailable" => (503, "The daemon annotation schema is unavailable."),
        "internal" => {
            if status != 500 {
                return if write {
                    invalid_write_response()
                } else {
                    invalid_response()
                };
            }
            return if write {
                uncertain_error()
            } else {
                read_timeout_error()
            };
        }
        _ => {
            return if write {
                invalid_write_response()
            } else {
                invalid_response()
            };
        }
    };
    if status != expected
        || body.error.current_revision.is_some() && body.error.code != "revision_conflict"
    {
        return if write {
            invalid_write_response()
        } else {
            invalid_response()
        };
    }
    let mut value = json!({"error":{"code":body.error.code,"message":message}});
    if let Some(revision) = body.error.current_revision {
        value["error"]["current_revision"] = json!(revision);
    }
    CallToolResult::structured_error(value)
}
pub fn read_timeout_error() -> CallToolResult {
    tool_error(
        "daemon_unavailable",
        "The daemon annotation service is unavailable or timed out.",
    )
}
pub fn uncertain_error() -> CallToolResult {
    tool_error(
        "daemon_unavailable",
        "Annotation submission result is uncertain and may have persisted. Retry only the identical payload with the same request_id, or read the annotation; do not create a new key.",
    )
}
pub fn cancelled_write_error() -> CallToolResult {
    tool_error(
        "cancelled",
        "Annotation submission was cancelled but may have persisted. Retry only the identical payload with the same request_id, or read the annotation; do not create a new key.",
    )
}
fn invalid_response() -> CallToolResult {
    tool_error(
        "invalid_daemon_response",
        "The daemon returned invalid or inconsistent annotation data.",
    )
}
fn invalid_write_response() -> CallToolResult {
    tool_error(
        "invalid_daemon_response",
        "The daemon returned an invalid annotation receipt. Persistence is uncertain; retry only the identical payload with the same request_id or read the annotation.",
    )
}
fn uncertain_context(error: CallToolResult) -> CallToolResult {
    if error
        .structured_content
        .as_ref()
        .and_then(|s| s["error"]["code"].as_str())
        == Some("daemon_unavailable")
    {
        uncertain_error()
    } else {
        invalid_write_response()
    }
}
