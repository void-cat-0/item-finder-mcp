use std::collections::{BTreeMap, BTreeSet};
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use item_core::assets::{
    AssetErrorBody, CameraFrameMetadata, CameraFrameRequest, RawAssetMetadata,
};
use reqwest::{Client, Response, Url};
use rmcp::model::{CallToolResult, ContentBlock};
use rmcp::schemars;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;

use crate::query::tool_error;

const MAX_IMAGE_BYTES: usize = 8 * 1024 * 1024;
const MAX_ENCODED_BYTES: usize = 16 * 1024 * 1024;
const MAX_METADATA_BYTES: usize = 16 * 1024;
const MAX_CAMERA_ID_BYTES: usize = 1024;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ImageKind {
    Raw,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ImagesRequest {
    /// Only unannotated raw JPEGs are supported. Never falls back to detector evidence.
    pub image_kind: ImageKind,
    /// Positive observation IDs; at most 32 selectors in total.
    #[serde(default)]
    pub observation_ids: Vec<i64>,
    /// Opaque asset IDs returned by the daemon, including frozen camera frames; paths and URLs are forbidden.
    #[serde(default)]
    pub asset_ids: Vec<String>,
    /// Number of images, 1–8; default 4. Newest capture first, then asset ID.
    pub max_images: Option<usize>,
    /// Maximum JPEG bytes per image, 1–8388608; default 4194304.
    pub max_bytes_each: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FrameRequest {
    /// Exact camera ID, including non-ASCII IDs; 1–1024 UTF-8 bytes, without control characters. `.` and `..` are reserved and rejected because they cannot safely address one URL path segment.
    pub camera_id: String,
    /// Maximum age of the frame at freeze time, 1–60000 milliseconds; default 2000.
    #[schemars(range(min = 1, max = 60000))]
    pub max_age_ms: Option<u64>,
    /// Wait for an eligible frame, 0–10000 milliseconds; default 3000.
    #[schemars(range(min = 0, max = 10000))]
    pub wait_timeout_ms: Option<u64>,
    /// Maximum JPEG bytes, 1–8388608; default 4194304.
    #[schemars(range(min = 1, max = 8388608))]
    pub max_bytes: Option<u64>,
}

impl FrameRequest {
    pub fn validate(&self) -> Result<(), CallToolResult> {
        if !valid_camera_id(&self.camera_id)
            || !(1..=60000).contains(&self.max_age_ms.unwrap_or(2000))
            || self.wait_timeout_ms.unwrap_or(3000) > 10000
            || !(1..=MAX_IMAGE_BYTES as u64).contains(&self.max_bytes.unwrap_or(4194304))
        {
            return Err(tool_error(
                "invalid_input",
                "Provide an exact camera_id of 1–1024 UTF-8 bytes without controls, max_age_ms 1–60000, wait_timeout_ms 0–10000, and max_bytes 1–8388608.",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ImageResult {
    pub asset_id: String,
    pub database_id: String,
    pub image_kind: &'static str,
    pub mime_type: String,
    pub bytes: u64,
    pub sha256: String,
    pub width: u32,
    pub height: u32,
    pub capture_time: String,
    /// Null for independent camera frames; never invents a database observation.
    pub observation_id: Option<i64>,
    pub camera_id: Option<String>,
    pub zone: Option<String>,
    pub provenance: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    /// Index into the MCP result content array; index zero is the metadata text.
    pub content_index: usize,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ImagesResponse {
    pub images: Vec<ImageResult>,
    pub truncated: bool,
    pub source: &'static str,
    pub note: &'static str,
    /// Frame age at the daemon's freeze time, not at response delivery or later rereads.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub age_ms: Option<u64>,
    /// Camera state reported at freeze time; it does not guarantee continuing availability.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub camera_state: Option<String>,
}

pub fn output_schema() -> Arc<rmcp::model::JsonObject> {
    let mut success = (*rmcp::handler::server::tool::schema_for_output::<ImagesResponse>()).clone();
    let definitions = success.remove("$defs");
    let mut schema = serde_json::json!({
        "type": "object",
        "oneOf": [success, {
            "type": "object", "required": ["error"], "additionalProperties": false,
            "properties": {"error": {
                "type": "object", "required": ["code", "message"], "additionalProperties": false,
                "properties": {"code": {"type": "string"}, "message": {"type": "string"}}
            }}
        }]
    })
    .as_object()
    .unwrap()
    .clone();
    if let Some(definitions) = definitions {
        schema.insert("$defs".into(), definitions);
    }
    Arc::new(schema)
}

#[derive(Clone)]
pub struct DaemonClient {
    client: Client,
    endpoint: Url,
    authorization: reqwest::header::HeaderValue,
    permits: Arc<Semaphore>,
    cpu_permits: Arc<Semaphore>,
    timeout: Duration,
}

impl DaemonClient {
    pub fn new(endpoint: &str, token: &str, timeout: Duration) -> anyhow::Result<Self> {
        let endpoint = Url::parse(endpoint)?;
        let host = endpoint.host_str().unwrap_or("").trim_matches(['[', ']']);
        let loopback = host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback());
        anyhow::ensure!(
            endpoint.scheme() == "http"
                && loopback
                && endpoint.username().is_empty()
                && endpoint.password().is_none()
                && endpoint.query().is_none()
                && endpoint.fragment().is_none()
                && endpoint.path() == "/",
            "daemon endpoint must be an HTTP numeric loopback origin"
        );
        anyhow::ensure!(!token.trim().is_empty(), "daemon token must not be empty");
        anyhow::ensure!(!timeout.is_zero(), "daemon timeout must be positive");
        let mut authorization = reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| anyhow::anyhow!("invalid daemon token"))?;
        authorization.set_sensitive(true);
        let client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(timeout)
            .build()?;
        Ok(Self {
            client,
            endpoint,
            authorization,
            permits: Arc::new(Semaphore::new(4)),
            cpu_permits: Arc::new(Semaphore::new(4)),
            timeout,
        })
    }

    /// Start before database identity lookup so all response generation shares this budget.
    pub fn deadline(&self) -> tokio::time::Instant {
        tokio::time::Instant::now() + self.timeout
    }

    pub async fn images(
        &self,
        request: ImagesRequest,
        database_id: String,
    ) -> Result<CallToolResult, CallToolResult> {
        self.images_at(request, database_id, self.deadline()).await
    }

    pub async fn images_at(
        &self,
        request: ImagesRequest,
        database_id: String,
        deadline: tokio::time::Instant,
    ) -> Result<CallToolResult, CallToolResult> {
        let (max_images, max_bytes_each) = validate(&request)?;
        self.admitted(
            deadline,
            self.read_images(request, database_id, max_images, max_bytes_each),
        )
        .await
    }

    pub async fn camera_frame(
        &self,
        request: FrameRequest,
        database_id: String,
    ) -> Result<CallToolResult, CallToolResult> {
        self.camera_frame_at(request, database_id, self.deadline())
            .await
    }

    pub async fn camera_frame_at(
        &self,
        request: FrameRequest,
        database_id: String,
        deadline: tokio::time::Instant,
    ) -> Result<CallToolResult, CallToolResult> {
        request.validate()?;
        self.admitted(deadline, self.freeze_frame(request, database_id))
            .await
    }

    async fn admitted<F>(
        &self,
        deadline: tokio::time::Instant,
        work: F,
    ) -> Result<CallToolResult, CallToolResult>
    where
        F: Future<Output = Result<CallToolResult, CallToolResult>>,
    {
        let _permit = self
            .permits
            .try_acquire()
            .map_err(|_| tool_error("daemon_busy", "The image service is busy. Retry later."))?;
        let result = tokio::time::timeout_at(deadline, work)
            .await
            .map_err(|_| timeout_error())?;
        if tokio::time::Instant::now() >= deadline {
            return Err(timeout_error());
        }
        result
    }

    async fn freeze_frame(
        &self,
        request: FrameRequest,
        database_id: String,
    ) -> Result<CallToolResult, CallToolResult> {
        let body = CameraFrameRequest {
            database_id: database_id.clone(),
            max_age_ms: request.max_age_ms.unwrap_or(2000),
            wait_timeout_ms: request.wait_timeout_ms.unwrap_or(3000),
            max_bytes: request.max_bytes.unwrap_or(4194304),
        };
        let mut url = self.endpoint.clone();
        // URL's segment encoder preserves the exact ID, including '/', '%', '?' and Unicode.
        url.path_segments_mut()
            .map_err(|_| invalid_response())?
            .clear()
            .extend(["internal", "v1", "cameras", &request.camera_id, "frame"]);
        let started = chrono::Utc::now();
        let response = self.send(self.client.post(url).json(&body)).await?;
        let bytes = bounded_body(response, MAX_METADATA_BYTES).await?;
        let frame: CameraFrameMetadata =
            serde_json::from_slice(&bytes).map_err(|_| invalid_response())?;
        validate_metadata(&frame.asset)?;
        check_database(&frame.asset, &database_id)?;
        if frame.asset.provenance != "daemon_camera_frame"
            || frame.asset.camera_id.as_deref() != Some(request.camera_id.as_str())
            || frame.camera_state != "running"
        {
            return Err(invalid_response());
        }
        let capture =
            chrono::DateTime::parse_from_rfc3339(frame.asset.capture_time.as_deref().unwrap())
                .map_err(|_| invalid_response())?;
        // Age is measured at freeze. A delayed metadata transfer may itself take time,
        // but cannot make a frame older than the request start minus its age budget.
        if frame.age_ms > body.max_age_ms
            || capture < started - chrono::Duration::milliseconds(body.max_age_ms as i64 + 1)
        {
            return Err(tool_error(
                "frame_stale",
                "The daemon frame exceeds the requested capture age.",
            ));
        }
        if capture > chrono::Utc::now() + chrono::Duration::seconds(1)
            || frame.age_ms
                > (chrono::Utc::now() - capture.with_timezone(&chrono::Utc))
                    .num_milliseconds()
                    .max(0) as u64
                    + 1000
        {
            return Err(invalid_response());
        }
        self.render_images(
            vec![frame.asset],
            body.max_bytes as usize,
            false,
            Some((frame.age_ms, frame.camera_state)),
        )
        .await
    }

    async fn read_images(
        &self,
        request: ImagesRequest,
        database_id: String,
        max_images: usize,
        max_bytes_each: usize,
    ) -> Result<CallToolResult, CallToolResult> {
        let mut metadata = Vec::new();
        for id in request.observation_ids.into_iter().collect::<BTreeSet<_>>() {
            let asset = self
                .metadata(&format!("internal/v1/observations/{id}/raw-asset"))
                .await?;
            if asset.observation_id != Some(id) || asset.provenance != "daemon_raw_asset" {
                return Err(invalid_response());
            }
            metadata.push(asset);
        }
        for id in request.asset_ids.into_iter().collect::<BTreeSet<_>>() {
            let asset = self.metadata(&format!("internal/v1/assets/{id}")).await?;
            if asset.asset_id != id {
                return Err(invalid_response());
            }
            metadata.push(asset);
        }
        let mut unique = BTreeMap::<String, RawAssetMetadata>::new();
        for asset in metadata {
            check_database(&asset, &database_id)?;
            if let Some(previous) = unique.insert(asset.asset_id.clone(), asset.clone())
                && previous != asset
            {
                return Err(invalid_response());
            }
        }
        let mut metadata: Vec<_> = unique.into_values().collect();
        metadata.sort_by(|a, b| {
            let a_time = a
                .capture_time
                .as_deref()
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok());
            let b_time = b
                .capture_time
                .as_deref()
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok());
            b_time
                .cmp(&a_time)
                .then_with(|| a.asset_id.cmp(&b.asset_id))
        });
        let truncated = metadata.len() > max_images;
        metadata.truncate(max_images);
        self.render_images(metadata, max_bytes_each, truncated, None)
            .await
    }

    // Internal renderer never reacquires HTTP admission or starts a fresh deadline.
    async fn render_images(
        &self,
        metadata: Vec<RawAssetMetadata>,
        max_bytes_each: usize,
        mut truncated: bool,
        frame: Option<(u64, String)>,
    ) -> Result<CallToolResult, CallToolResult> {
        let mut content = Vec::new();
        let mut images = Vec::new();
        let mut encoded_bytes = 0usize;
        for asset in metadata {
            check_expiry(&asset)?;
            if asset.bytes > max_bytes_each as u64 {
                return Err(tool_error(
                    "payload_too_large",
                    "A requested raw image exceeds the requested byte limit.",
                ));
            }
            let encoded_len = (asset.bytes as usize).div_ceil(3) * 4;
            if encoded_bytes + encoded_len > MAX_ENCODED_BYTES {
                truncated = true;
                break;
            }
            let response = self
                .get(&format!("internal/v1/assets/{}/content", asset.asset_id))
                .await?;
            if response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|h| h.to_str().ok())
                != Some("image/jpeg")
                || response
                    .headers()
                    .get("x-asset-id")
                    .and_then(|h| h.to_str().ok())
                    != Some(asset.asset_id.as_str())
                || response
                    .headers()
                    .get("x-asset-sha256")
                    .and_then(|h| h.to_str().ok())
                    != Some(asset.sha256.as_str())
            {
                return Err(invalid_response());
            }
            let bytes = bounded_body(response, max_bytes_each).await?;
            let expected = asset.clone();
            let cpu_permit = self.cpu_permit()?;
            let (dimensions, encoded) = tokio::task::spawn_blocking(move || {
                // A cancelled blocking task keeps its permit until the bounded work finishes.
                let _permit = cpu_permit;
                if !bytes.starts_with(&[0xff, 0xd8]) || !bytes.ends_with(&[0xff, 0xd9]) {
                    return Err(invalid_response());
                }
                let hash = format!("{:x}", Sha256::digest(&bytes));
                let reader = image::ImageReader::with_format(
                    std::io::Cursor::new(&bytes),
                    image::ImageFormat::Jpeg,
                );
                let dimensions = reader.into_dimensions().map_err(|_| invalid_response())?;
                if dimensions.0 == 0
                    || dimensions.1 == 0
                    || dimensions.0 > 16384
                    || dimensions.1 > 16384
                    || u64::from(dimensions.0) * u64::from(dimensions.1) > 64_000_000
                    || bytes.len() as u64 != expected.bytes
                    || hash != expected.sha256
                    || Some(dimensions.0) != expected.width
                    || Some(dimensions.1) != expected.height
                {
                    return Err(invalid_response());
                }
                // Header-only validation is insufficient: decode the complete scan while
                // retaining the bounded CPU permit, but never retain the decoded pixels.
                image::ImageReader::with_format(
                    std::io::Cursor::new(&bytes),
                    image::ImageFormat::Jpeg,
                )
                .decode()
                .map_err(|_| invalid_response())?;
                check_expiry(&expected)?;
                Ok((
                    dimensions,
                    base64::engine::general_purpose::STANDARD.encode(bytes),
                ))
            })
            .await
            .map_err(|_| invalid_response())??;
            check_expiry(&asset)?;
            encoded_bytes += encoded_len;
            images.push(ImageResult {
                asset_id: asset.asset_id,
                database_id: asset.database_id,
                image_kind: "raw",
                mime_type: asset.mime_type,
                bytes: asset.bytes,
                sha256: asset.sha256,
                width: dimensions.0,
                height: dimensions.1,
                capture_time: asset.capture_time.unwrap(),
                observation_id: asset.observation_id,
                camera_id: asset.camera_id,
                zone: asset.zone,
                provenance: asset.provenance,
                expires_at: asset.expires_at,
                content_index: content.len() + 1,
            });
            content.push(ContentBlock::image(encoded, "image/jpeg"));
        }
        let is_frame = frame.is_some();
        let (age_ms, camera_state) = match frame {
            Some((age, state)) => (Some(age), Some(state)),
            None => (None, None),
        };
        let response = ImagesResponse {
            images,
            truncated,
            age_ms,
            camera_state,
            source: if is_frame {
                "daemon_camera_frame"
            } else {
                "daemon_raw_asset"
            },
            note: if is_frame {
                "Immutable unannotated JPEG captured at capture_time. age_ms and camera_state describe the daemon freeze time, not delivery time or current presence. The expiring opaque asset can be reread with get_item_images without capturing again. No observation or visual inference is created. content_index identifies the image block."
            } else {
                "Raw evidence is an unannotated captured frame, not a visual inference or proof of current presence. Image blocks follow metadata order; content_index identifies each block. Independent camera frame assets have no observation and can be read only until expires_at."
            },
        };
        // JSON/text generation can copy megabytes; keep it off the async executor and
        // inside the same deadline and CPU budget as JPEG validation/base64 encoding.
        let cpu_permit = self.cpu_permit()?;
        tokio::task::spawn_blocking(move || {
            let _permit = cpu_permit;
            for image in &response.images {
                check_expiry_value(image.expires_at.as_deref())?;
            }
            let mut result = CallToolResult::structured(
                serde_json::to_value(response).map_err(|_| invalid_response())?,
            );
            result.content.extend(content);
            Ok(result)
        })
        .await
        .map_err(|_| invalid_response())?
    }

    fn cpu_permit(&self) -> Result<tokio::sync::OwnedSemaphorePermit, CallToolResult> {
        self.cpu_permits.clone().try_acquire_owned().map_err(|_| {
            tool_error(
                "daemon_busy",
                "The image validation service is busy. Retry later.",
            )
        })
    }

    async fn metadata(&self, path: &str) -> Result<RawAssetMetadata, CallToolResult> {
        let response = self.get(path).await?;
        let bytes = bounded_body(response, MAX_METADATA_BYTES).await?;
        let asset: RawAssetMetadata =
            serde_json::from_slice(&bytes).map_err(|_| invalid_response())?;
        validate_metadata(&asset)?;
        Ok(asset)
    }

    async fn get(&self, path: &str) -> Result<Response, CallToolResult> {
        let url = self.endpoint.join(path).map_err(|_| invalid_response())?;
        self.send(self.client.get(url)).await
    }

    async fn send(&self, request: reqwest::RequestBuilder) -> Result<Response, CallToolResult> {
        let response = request
            .header(reqwest::header::AUTHORIZATION, self.authorization.clone())
            .send()
            .await
            .map_err(|_| {
                tool_error(
                    "daemon_unavailable",
                    "The daemon image service could not be reached.",
                )
            })?;
        if response.status().is_success() {
            return Ok(response);
        }
        let status = response.status();
        let bytes = bounded_body(response, MAX_METADATA_BYTES).await?;
        let body: Option<AssetErrorBody> = serde_json::from_slice(&bytes).ok();
        let code = body.as_ref().map(|b| b.error.code.as_str());
        Err(match (status.as_u16(), code) {
            (401 | 403, _) => tool_error("unauthorized", "Daemon image access was denied."),
            (_, Some("camera_not_found")) => {
                tool_error("camera_not_found", "The requested camera was not found.")
            }
            (_, Some("camera_offline")) => {
                tool_error("camera_offline", "The requested camera is offline.")
            }
            (_, Some("frame_unavailable")) => tool_error(
                "frame_unavailable",
                "No eligible camera frame is available.",
            ),
            (_, Some("frame_stale")) => tool_error(
                "frame_stale",
                "The camera frame exceeds the requested capture age.",
            ),
            (_, Some("source_expired")) => {
                tool_error("source_expired", "The raw image is missing or expired.")
            }
            (_, Some("dataset_mismatch")) => tool_error(
                "dataset_mismatch",
                "The daemon and query database do not share the same source identity.",
            ),
            (_, Some("raw_unavailable")) => tool_error(
                "raw_unavailable",
                "No raw image is registered for this observation.",
            ),
            (404, _) => tool_error(
                "not_found",
                "The requested observation or raw asset was not found.",
            ),
            (410, _) => tool_error("source_expired", "The raw image is missing or expired."),
            (413, _) => tool_error(
                "payload_too_large",
                "The raw image exceeds the daemon size limit.",
            ),
            (429 | 503, _) => tool_error(
                "daemon_unavailable",
                "The daemon image service is unavailable. Retry later.",
            ),
            _ => invalid_response(),
        })
    }
}

fn check_database(asset: &RawAssetMetadata, database_id: &str) -> Result<(), CallToolResult> {
    if asset.database_id != database_id {
        return Err(tool_error(
            "dataset_mismatch",
            "The daemon and query database do not share the same source identity.",
        ));
    }
    Ok(())
}

fn validate_metadata(asset: &RawAssetMetadata) -> Result<(), CallToolResult> {
    if !valid_asset_id(&asset.asset_id)
        || asset.kind != "raw"
        || asset.mime_type != "image/jpeg"
        || asset.status != "available"
        || asset.bytes == 0
        || asset.bytes > MAX_IMAGE_BYTES as u64
        || asset.sha256.len() != 64
        || !asset.sha256.bytes().all(|c| c.is_ascii_hexdigit())
        || asset.width.is_none_or(|w| w == 0 || w > 16384)
        || asset.height.is_none_or(|h| h == 0 || h > 16384)
        || u64::from(asset.width.unwrap_or(0)) * u64::from(asset.height.unwrap_or(0)) > 64_000_000
        || asset
            .capture_time
            .as_deref()
            .is_none_or(|s| chrono::DateTime::parse_from_rfc3339(s).is_err())
    {
        return Err(invalid_response());
    }
    match asset.provenance.as_str() {
        "daemon_raw_asset"
            if asset.observation_id.is_some_and(|id| id > 0) && asset.expires_at.is_none() => {}
        "daemon_camera_frame"
            if asset.observation_id.is_none()
                && asset.zone.is_none()
                && asset.camera_id.as_deref().is_some_and(valid_camera_id)
                && asset.expires_at.is_some() =>
        {
            let expiry = chrono::DateTime::parse_from_rfc3339(asset.expires_at.as_deref().unwrap())
                .map_err(|_| invalid_response())?;
            let capture =
                chrono::DateTime::parse_from_rfc3339(asset.capture_time.as_deref().unwrap())
                    .map_err(|_| invalid_response())?;
            if expiry <= capture {
                return Err(invalid_response());
            }
        }
        _ => return Err(invalid_response()),
    }
    check_expiry(asset)
}

fn check_expiry(asset: &RawAssetMetadata) -> Result<(), CallToolResult> {
    check_expiry_value(asset.expires_at.as_deref())
}

fn check_expiry_value(expiry: Option<&str>) -> Result<(), CallToolResult> {
    if let Some(expiry) = expiry {
        let expiry =
            chrono::DateTime::parse_from_rfc3339(expiry).map_err(|_| invalid_response())?;
        if expiry <= chrono::Utc::now() {
            return Err(tool_error(
                "source_expired",
                "The raw image is missing or expired.",
            ));
        }
    }
    Ok(())
}

pub fn validate(request: &ImagesRequest) -> Result<(usize, usize), CallToolResult> {
    let count = request.observation_ids.len() + request.asset_ids.len();
    let max_images = request.max_images.unwrap_or(4);
    let max_bytes_each = request.max_bytes_each.unwrap_or(4 * 1024 * 1024);
    if count == 0
        || count > 32
        || request.observation_ids.iter().any(|id| *id <= 0)
        || request.asset_ids.iter().any(|id| !valid_asset_id(id))
        || !(1..=8).contains(&max_images)
        || !(1..=MAX_IMAGE_BYTES).contains(&max_bytes_each)
    {
        return Err(tool_error(
            "invalid_input",
            "Provide 1–32 positive observation IDs or opaque asset IDs, max_images 1–8, and max_bytes_each 1–8388608.",
        ));
    }
    Ok((max_images, max_bytes_each))
}

fn valid_camera_id(id: &str) -> bool {
    !id.is_empty()
        && id != "."
        && id != ".."
        && id.len() <= MAX_CAMERA_ID_BYTES
        && !id.chars().any(char::is_control)
}

fn valid_asset_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 192
        && id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
}

async fn bounded_body(mut response: Response, max: usize) -> Result<Vec<u8>, CallToolResult> {
    if response
        .content_length()
        .is_some_and(|length| length > max as u64)
    {
        return Err(tool_error(
            "payload_too_large",
            "The daemon response exceeds the allowed size.",
        ));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| {
        tool_error(
            "daemon_unavailable",
            "The daemon image response was interrupted.",
        )
    })? {
        if bytes.len() + chunk.len() > max {
            return Err(tool_error(
                "payload_too_large",
                "The daemon response exceeds the allowed size.",
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

pub(crate) fn timeout_error() -> CallToolResult {
    tool_error("daemon_unavailable", "The daemon image request timed out.")
}

fn invalid_response() -> CallToolResult {
    tool_error(
        "invalid_daemon_response",
        "The daemon returned invalid or inconsistent raw asset data.",
    )
}
