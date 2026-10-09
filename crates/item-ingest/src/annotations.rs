//! Bounded authenticated annotation runtime. The operation mutex serializes
//! publication/commit/cleanup; two permits bound callers including lock wait.
//! Lock order is annotation operation -> RAW_LIFECYCLE -> Store. JPEG work is
//! never performed while holding Store or source lifecycle locks.
use crate::assets::{AssetFailure, AssetStore, RAW_LIFECYCLE};
use crate::config::RuntimeConfig;
use axum::{
    Router,
    body::{Body, to_bytes},
    extract::{Path, Query, Request, State},
    http::{HeaderMap, StatusCode, header},
    response::Response,
    routing::{get, post},
};
use chrono::Utc;
use item_core::annotations::*;
use item_core::store::StoreError;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

const BODY_LIMIT: usize = 256 * 1024;
const RESPONSE_LIMIT: usize = 512 * 1024;
const DEADLINE: Duration = Duration::from_secs(15);
#[derive(Clone)]
pub struct AnnotationRuntime {
    assets: AssetStore,
    enabled: bool,
    token: Option<Arc<str>>,
    quota: AnnotationQuota,
    permits: Arc<Semaphore>,
    readers: Arc<Semaphore>,
    operation: Arc<Mutex<()>>,
}
#[derive(Clone)]
struct Api {
    runtime: AnnotationRuntime,
    authority: String,
}
#[derive(Debug, Clone)]
pub(crate) struct Failure {
    code: &'static str,
    current_revision: Option<u32>,
}
impl Failure {
    fn new(code: &'static str) -> Self {
        Self {
            code,
            current_revision: None,
        }
    }
    fn response(&self) -> Response {
        let status = match self.code {
            "invalid_input" => StatusCode::BAD_REQUEST,
            "unauthorized" => StatusCode::UNAUTHORIZED,
            "forbidden" | "annotation_disabled" => StatusCode::FORBIDDEN,
            "not_found" => StatusCode::NOT_FOUND,
            "source_expired" | "annotation_expired" => StatusCode::GONE,
            "payload_too_large" => StatusCode::PAYLOAD_TOO_LARGE,
            "busy" | "schema_unavailable" => StatusCode::SERVICE_UNAVAILABLE,
            "internal" => StatusCode::INTERNAL_SERVER_ERROR,
            _ => StatusCode::CONFLICT,
        };
        let message = match self.code {
            "revision_conflict" => {
                "The annotation revision changed; read the current revision before retrying."
            }
            "internal" => {
                "The annotation operation failed; retry the same request_id to determine its outcome."
            }
            "busy" => "Annotation work is busy; retry with the same request_id.",
            "annotation_disabled" => "Annotation submissions are disabled.",
            _ => "The annotation request could not be completed.",
        };
        json(
            status,
            &serde_json::json!({"error": AnnotationError { code: self.code.into(), message: message.into(), current_revision: self.current_revision }}),
        )
    }
}
impl From<StoreError> for Failure {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::AnnotationIdempotencyConflict => Self::new("idempotency_conflict"),
            StoreError::AnnotationRevisionConflict { current_revision } => Self {
                code: "revision_conflict",
                current_revision: Some(current_revision),
            },
            StoreError::AnnotationNotFound => Self::new("not_found"),
            StoreError::AnnotationExpired => Self::new("annotation_expired"),
            StoreError::AnnotationQuota(_) => Self::new("quota_exceeded"),
            StoreError::AnnotationSchemaUnavailable => Self::new("schema_unavailable"),
            StoreError::InvalidInput(ref message) if message == "camera_mismatch" => {
                Self::new("camera_mismatch")
            }
            StoreError::InvalidInput(_) => Self::new("invalid_input"),
            StoreError::Busy => Self::new("busy"),
            _ => Self::new("internal"),
        }
    }
}
impl From<AssetFailure> for Failure {
    fn from(e: AssetFailure) -> Self {
        Self::new(match e {
            AssetFailure::Missing => "source_expired",
            AssetFailure::Changed | AssetFailure::InvalidPath => "integrity_error",
            AssetFailure::NotFound | AssetFailure::Unavailable => "not_found",
            AssetFailure::TooLarge => "payload_too_large",
            AssetFailure::Busy => "busy",
            _ => "internal",
        })
    }
}
fn json<T: Serialize>(status: StatusCode, value: &T) -> Response {
    let bytes = match serde_json::to_vec(value) {
        Ok(b) if b.len() <= RESPONSE_LIMIT => b,
        _ => return Failure::new("payload_too_large").response(),
    };
    let mut response = Response::new(Body::from(bytes));
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    response
}
impl AnnotationRuntime {
    pub fn new(assets: AssetStore, settings: &RuntimeConfig) -> std::io::Result<Self> {
        assets.initialize_annotation_directory()?;
        let quota = AnnotationQuota {
            max_sets: settings.annotation_max_sets,
            max_revisions: settings.annotation_max_revisions,
            max_metadata_bytes: settings.annotation_max_metadata_bytes,
            max_evidence_bytes: settings.annotation_max_evidence_bytes,
            retention: chrono::Duration::days(settings.annotation_retention_days as i64),
            ..AnnotationQuota::default()
        };
        let runtime = Self {
            assets,
            enabled: settings.annotations_enabled,
            token: settings.annotation_token.clone().map(Arc::from),
            quota,
            permits: Arc::new(Semaphore::new(2)),
            readers: Arc::new(Semaphore::new(4)),
            operation: Arc::new(Mutex::new(())),
        };
        runtime
            .assets
            .shared_store()
            .lock()
            .map_err(|_| std::io::Error::other("annotation store unavailable"))?
            .ensure_annotation_schema()
            .map_err(std::io::Error::other)?;
        runtime
            .physical_bytes(Instant::now() + DEADLINE)
            .map_err(|_| std::io::Error::other("annotation evidence reconciliation failed"))?;
        Ok(runtime)
    }
    fn submit(
        &self,
        validated: ValidatedAnnotationRequest,
        deadline: Instant,
    ) -> Result<(AnnotationReceipt, bool), Failure> {
        let _operation = lock_until(&self.operation, deadline)?;
        let request = &validated.request;
        if request.database_id != self.assets.database_id() {
            return Err(Failure::new("dataset_mismatch"));
        }
        let db = self.assets.shared_store();
        // Idempotency always precedes source/revision/expiry/quota checks.
        if let Some(receipt) = lock_until(&db, deadline)?.annotation_receipt(
            &request.request_id,
            &validated.payload_sha256,
            Utc::now(),
        )? {
            return Ok((receipt, true));
        }
        if request.asset_id.starts_with("evidence-v1-") {
            return Err(Failure::new("invalid_input"));
        }
        let prior = if let Some(id) = request.annotation_id.as_deref() {
            let detail =
                lock_until(&db, deadline)?.annotation_detail(id, None, None, Utc::now())?;
            if detail.source_asset_id != request.asset_id || detail.source_sha256 != request.sha256
            {
                return Err(Failure::new("source_hash_mismatch"));
            }
            if detail.camera_id != request.camera_id {
                return Err(Failure::new("camera_mismatch"));
            }
            Some(detail)
        } else {
            None
        };
        let (source, bytes) = if let Some(detail) = &prior {
            let (mut retained, bytes) = self
                .assets
                .annotation_evidence_content(&detail.evidence_asset_id)?;
            retained.asset_id = detail.source_asset_id.clone();
            retained.observation_id = detail.observation_id;
            (retained, bytes)
        } else {
            self.assets.content(&request.asset_id)?
        };
        if source.database_id != request.database_id {
            return Err(Failure::new("dataset_mismatch"));
        }
        if source.camera_id.as_deref() != Some(request.camera_id.as_str()) {
            return Err(Failure::new("camera_mismatch"));
        }
        if source.sha256 != request.sha256 {
            return Err(Failure::new("source_hash_mismatch"));
        }
        check_deadline(deadline)?;
        let expires = if let Some(detail) = &prior {
            chrono::DateTime::parse_from_rfc3339(&detail.expires_at)
                .map_err(|_| Failure::new("integrity_error"))?
                .with_timezone(&Utc)
        } else {
            Utc::now() + self.quota.retention
        };
        // Serialize all publishers so the physical directory is the complete
        // quota accounting (includes failed deletes and abandoned temp files).
        let evidence = if let Some(detail) = &prior {
            self.assets
                .annotation_evidence_content(&detail.evidence_asset_id)?
                .0
        } else {
            let reusable = {
                let store = lock_until(&db, deadline)?;
                store.reusable_annotation_evidence(
                    &request.database_id,
                    &source.asset_id,
                    &source.sha256,
                    &request.camera_id,
                    Utc::now(),
                )?
            };
            if let Some((registration, _expiry)) = reusable {
                self.assets
                    .annotation_evidence_content(&registration.evidence_asset_id)?
                    .0
            } else {
                let physical = self.physical_bytes(deadline)?;
                if physical.saturating_add(source.bytes) > self.quota.max_evidence_bytes {
                    return Err(Failure::new("quota_exceeded"));
                }
                self.assets
                    .publish_annotation_evidence(&source, &bytes, &expires.to_rfc3339())?
            }
        };
        check_deadline(deadline)?;
        let _source_guard = lock_until(&RAW_LIFECYCLE, deadline)?;
        let store = lock_until(&db, deadline)?;
        let accepted_at = Utc::now();
        if prior.is_none() {
            self.assets
                .accept_annotation_source(&source, &store, accepted_at)?;
        } else {
            store.annotation_detail(
                request.annotation_id.as_deref().unwrap(),
                None,
                None,
                accepted_at,
            )?;
        }
        let registration = AnnotationEvidenceRegistration {
            database_id: source.database_id.clone(),
            evidence_asset_id: evidence.asset_id.clone(),
            source_asset_id: source.asset_id.clone(),
            source_sha256: source.sha256.clone(),
            camera_id: request.camera_id.clone(),
            capture_time: source
                .capture_time
                .clone()
                .ok_or_else(|| Failure::new("integrity_error"))?,
            observation_id: source.observation_id,
            sha256: source.sha256.clone(),
            bytes: source.bytes,
            width: source
                .width
                .ok_or_else(|| Failure::new("integrity_error"))?,
            height: source
                .height
                .ok_or_else(|| Failure::new("integrity_error"))?,
            status: "available".into(),
            accepted_at: accepted_at.to_rfc3339(),
        };
        let receipt =
            store.persist_annotation(&validated, &registration, accepted_at, &self.quota)?;
        Ok((receipt, false))
    }
    fn detail(
        &self,
        id: &str,
        query: DetailQuery,
        deadline: Instant,
    ) -> Result<AnnotationDetail, Failure> {
        if query.database_id != self.assets.database_id() {
            return Err(Failure::new("dataset_mismatch"));
        }
        if id.is_empty() || id.len() > 128 || !id.is_ascii() || query.revision == Some(0) {
            return Err(Failure::new("invalid_input"));
        }
        let db = self.assets.shared_store();
        let mut detail = lock_until(&db, deadline)?.annotation_detail(
            id,
            query.revision,
            query.camera_id.as_deref(),
            Utc::now(),
        )?;
        detail.original_source_status = self
            .assets
            .metadata(&detail.source_asset_id)
            .map(|_| "available")
            .unwrap_or("source_expired")
            .into();
        detail.evidence_status = self.evidence_status(&detail.evidence_asset_id).into();
        check_deadline(deadline)?;
        Ok(detail)
    }
    fn evidence_status(&self, id: &str) -> &'static str {
        match self.assets.annotation_evidence_content(id) {
            Ok(_) => "available",
            Err(AssetFailure::Changed | AssetFailure::InvalidPath) => "integrity_error",
            _ => "missing",
        }
    }
    fn list(
        &self,
        filter: AnnotationListFilter,
        deadline: Instant,
    ) -> Result<AnnotationListPage, Failure> {
        if filter.database_id != self.assets.database_id() {
            return Err(Failure::new("dataset_mismatch"));
        }
        let db = self.assets.shared_store();
        let mut page = lock_until(&db, deadline)?.list_annotations(&filter, Utc::now())?;
        for item in &mut page.items {
            check_deadline(deadline)?;
            item.evidence_status = self.evidence_status(&item.evidence_asset_id).into();
        }
        Ok(page)
    }
    pub(crate) fn maintenance(&self) -> Result<(), Failure> {
        let _op = self
            .operation
            .try_lock()
            .map_err(|_| Failure::new("busy"))?;
        let now = Utc::now();
        let db = self.assets.shared_store();
        let report = db
            .lock()
            .map_err(|_| Failure::new("internal"))?
            .expire_annotations(now, self.quota.tombstone_retention)
            .map_err(Failure::from)?;
        if report.expired_sets > 0 || report.deleted_tombstones > 0 {
            let _ = self.reconcile_evidence_files(now);
        }
        Ok(())
    }
    fn reconcile_evidence_files(&self, now: chrono::DateTime<Utc>) -> Result<(), Failure> {
        for (n, entry) in std::fs::read_dir(self.assets.annotation_directory())
            .map_err(|_| Failure::new("internal"))?
            .enumerate()
        {
            if n >= 100_000 {
                return Err(Failure::new("busy"));
            }
            let entry = entry.map_err(|_| Failure::new("internal"))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(id) = name
                .strip_suffix(".jpg")
                .or_else(|| name.strip_suffix(".json"))
            else {
                continue;
            };
            if !crate::assets::annotation_asset_id_public(id) {
                continue;
            }
            let active = self
                .assets
                .shared_store()
                .lock()
                .map_err(|_| Failure::new("internal"))?
                .annotation_evidence_by_id(id, now)
                .map_err(Failure::from)?
                .is_some();
            if !active
                && entry
                    .metadata()
                    .map_err(|_| Failure::new("internal"))?
                    .len()
                    > 0
            {
                let _ = std::fs::remove_file(entry.path());
            }
        }
        Ok(())
    }
    fn physical_bytes(&self, deadline: Instant) -> Result<u64, Failure> {
        let mut total = 0u64;
        for (n, entry) in std::fs::read_dir(self.assets.annotation_directory())
            .map_err(|_| Failure::new("internal"))?
            .enumerate()
        {
            check_deadline(deadline)?;
            if n >= 100_000 {
                return Err(Failure::new("busy"));
            }
            let entry = entry.map_err(|_| Failure::new("internal"))?;
            if !entry
                .file_type()
                .map_err(|_| Failure::new("internal"))?
                .is_file()
            {
                return Err(Failure::new("integrity_error"));
            }
            total = total.saturating_add(
                entry
                    .metadata()
                    .map_err(|_| Failure::new("internal"))?
                    .len(),
            );
        }
        Ok(total)
    }
}
fn check_deadline(deadline: Instant) -> Result<(), Failure> {
    if Instant::now() >= deadline {
        Err(Failure::new("busy"))
    } else {
        Ok(())
    }
}
fn lock_until<T>(
    mutex: &Mutex<T>,
    deadline: Instant,
) -> Result<std::sync::MutexGuard<'_, T>, Failure> {
    loop {
        match mutex.try_lock() {
            Ok(g) => return Ok(g),
            Err(std::sync::TryLockError::Poisoned(_)) => return Err(Failure::new("internal")),
            Err(_) => {
                check_deadline(deadline)?;
                std::thread::sleep(Duration::from_millis(2));
            }
        }
    }
}
fn authorized(token: Option<&str>, headers: &HeaderMap) -> bool {
    let Some(token) = token else {
        return false;
    };
    if headers.get_all(header::AUTHORIZATION).iter().count() != 1 {
        return false;
    }
    let Some(value) = headers
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
    else {
        return false;
    };
    let a = Sha256::digest(value.as_bytes());
    let b = Sha256::digest(token.as_bytes());
    a.iter().zip(b.iter()).fold(0u8, |v, (a, b)| v | (a ^ b)) == 0
}
fn origin(api: &Api, headers: &HeaderMap) -> bool {
    headers.get_all(header::HOST).iter().count() == 1
        && headers.get(header::HOST).and_then(|h| h.to_str().ok()) == Some(api.authority.as_str())
        && headers.get_all(header::ORIGIN).iter().count() <= 1
        && headers
            .get(header::ORIGIN)
            .is_none_or(|h| h.to_str().ok() == Some(format!("http://{}", api.authority).as_str()))
}
pub fn router(runtime: AnnotationRuntime, address: std::net::SocketAddr) -> Router {
    Router::new()
        .route(
            "/internal/v1/annotations",
            post(submit_handler).get(list_handler),
        )
        .route("/internal/v1/annotations/{id}", get(detail_handler))
        .layer(axum::extract::DefaultBodyLimit::disable())
        .with_state(Api {
            runtime,
            authority: address.to_string(),
        })
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DetailQuery {
    database_id: String,
    revision: Option<u32>,
    camera_id: Option<String>,
}
fn read_auth(api: &Api, headers: &HeaderMap) -> Result<(), Failure> {
    if !api.runtime.assets.authorized(headers) {
        return Err(Failure::new("unauthorized"));
    }
    if !origin(api, headers) {
        return Err(Failure::new("forbidden"));
    }
    Ok(())
}
async fn detail_handler(
    State(api): State<Api>,
    headers: HeaderMap,
    Path(id): Path<String>,
    query: Result<Query<DetailQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    if let Err(e) = read_auth(&api, &headers) {
        return e.response();
    }
    let Ok(Query(query)) = query else {
        return Failure::new("invalid_input").response();
    };
    let Ok(permit) = api.runtime.readers.clone().try_acquire_owned() else {
        return Failure::new("busy").response();
    };
    let deadline = Instant::now() + DEADLINE;
    match tokio::task::spawn_blocking(move || {
        let _permit = permit;
        api.runtime.detail(&id, query, deadline)
    })
    .await
    {
        Ok(Ok(value)) => json(StatusCode::OK, &value),
        Ok(Err(e)) => e.response(),
        Err(_) => Failure::new("internal").response(),
    }
}
async fn list_handler(
    State(api): State<Api>,
    headers: HeaderMap,
    query: Result<Query<AnnotationListFilter>, axum::extract::rejection::QueryRejection>,
) -> Response {
    if let Err(e) = read_auth(&api, &headers) {
        return e.response();
    }
    let Ok(Query(filter)) = query else {
        return Failure::new("invalid_input").response();
    };
    let Ok(permit) = api.runtime.readers.clone().try_acquire_owned() else {
        return Failure::new("busy").response();
    };
    let deadline = Instant::now() + DEADLINE;
    match tokio::task::spawn_blocking(move || {
        let _permit = permit;
        api.runtime.list(filter, deadline)
    })
    .await
    {
        Ok(Ok(value)) => json(StatusCode::OK, &value),
        Ok(Err(e)) => e.response(),
        Err(_) => Failure::new("internal").response(),
    }
}
async fn submit_handler(State(api): State<Api>, request: Request) -> Response {
    // This handler receives the raw body; authentication precedes collection,
    // parsing and duplicate-key detection, even for oversized/invalid bodies.
    if !authorized(api.runtime.token.as_deref(), request.headers()) {
        return Failure::new("unauthorized").response();
    }
    if !origin(&api, request.headers()) {
        return Failure::new("forbidden").response();
    }
    if !api.runtime.enabled {
        return Failure::new("annotation_disabled").response();
    }
    if request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        .is_none_or(|v| {
            !v.split(';')
                .next()
                .is_some_and(|v| v.trim().eq_ignore_ascii_case("application/json"))
        })
    {
        return Failure::new("invalid_input").response();
    }
    let Ok(permit) = api.runtime.permits.clone().try_acquire_owned() else {
        return Failure::new("busy").response();
    };
    let deadline = Instant::now() + DEADLINE;
    let body = match tokio::time::timeout_at(
        tokio::time::Instant::from_std(deadline),
        to_bytes(request.into_body(), BODY_LIMIT),
    )
    .await
    {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(_)) => return Failure::new("payload_too_large").response(),
        Err(_) => return Failure::new("busy").response(),
    };
    let validated = match parse_annotation_request_json(&body) {
        Ok(v) => v,
        Err(_) => return Failure::new("invalid_input").response(),
    };
    match tokio::task::spawn_blocking(move || {
        let _permit = permit;
        api.runtime.submit(validated, deadline)
    })
    .await
    {
        Ok(Ok((receipt, replay))) => json(
            if replay {
                StatusCode::OK
            } else {
                StatusCode::CREATED
            },
            &receipt,
        ),
        Ok(Err(e)) => e.response(),
        Err(_) => Failure::new("internal").response(),
    }
}
