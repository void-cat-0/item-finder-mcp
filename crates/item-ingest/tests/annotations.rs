//! Native-dependency-free tests of the actual daemon routers and durable files.
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use chrono::Utc;
use item_core::{
    annotations::AnnotationReceipt,
    assets::RawAssetMetadata,
    store::{DEFAULT_DEDUP_WINDOW, Store},
};
use item_ingest::{
    annotations::{self, AnnotationRuntime},
    assets::AssetStore,
    config::{CliOverrides, EnvOverrides, RuntimeConfig},
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tower::ServiceExt;
struct Fixture {
    _dir: tempfile::TempDir,
    assets: AssetStore,
    runtime: AnnotationRuntime,
    db: item_ingest::SharedStore,
    address: std::net::SocketAddr,
    camera: RawAssetMetadata,
    observed: RawAssetMetadata,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(Mutex::new(
            Store::open(dir.path().join("db.sqlite")).unwrap(),
        ));
        let assets = AssetStore::new_with_ttl(
            dir.path().join("images"),
            8 * 1024 * 1024,
            Some("read-secret".into()),
            db.clone(),
            Duration::from_millis(250),
        )
        .unwrap();
        let now = Utc::now();
        let (id, _, _) = db
            .lock()
            .unwrap()
            .record_sighting("desk", "table", "cup", now, None, DEFAULT_DEDUP_WINDOW)
            .unwrap();
        let owner = db.lock().unwrap().observation_by_id(id).unwrap().unwrap();
        let observed = assets
            .publish_rgb(&owner, now, &[90; 4 * 4 * 3], 4, 4)
            .unwrap();
        let camera = assets
            .publish_camera_rgb("laptop", now, &[170; 4 * 4 * 3], 4, 4)
            .unwrap();
        let mut settings =
            RuntimeConfig::resolve(None, &CliOverrides::default(), &EnvOverrides::default())
                .unwrap();
        settings.annotations_enabled = true;
        settings.annotation_token = Some("write-secret".into());
        let runtime = AnnotationRuntime::new(assets.clone(), &settings).unwrap();
        Self {
            _dir: dir,
            assets,
            runtime,
            db,
            address: "127.0.0.1:8480".parse().unwrap(),
            camera,
            observed,
        }
    }
    fn app(&self) -> axum::Router {
        item_ingest::assets::router(self.assets.clone(), self.address)
            .merge(annotations::router(self.runtime.clone(), self.address))
    }
    fn payload(&self, source: &RawAssetMetadata, key: &str) -> Value {
        json!({"schema_version":1,"database_id":source.database_id,"request_id":key,"asset_id":source.asset_id,"sha256":source.sha256,"camera_id":source.camera_id,"coordinate_system":"normalized_xywh","producer":{"kind":"vlm","host":"test"},"annotations":[{"object_id":"one","label":"cup","bbox":[0.0,0.0,0.5,1.0]},{"object_id":"two","label":"cup","bbox":[0.5,0.0,0.5,1.0]}]})
    }
    async fn call(
        &self,
        method: &str,
        path: &str,
        token: &str,
        body: String,
    ) -> (StatusCode, Vec<u8>) {
        let response = self
            .app()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .header("host", self.address.to_string())
                    .header("authorization", format!("Bearer {token}"))
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        (
            response.status(),
            to_bytes(response.into_body(), 9 * 1024 * 1024)
                .await
                .unwrap()
                .to_vec(),
        )
    }
    async fn post(&self, v: &Value) -> (StatusCode, Value) {
        let (s, b) = self
            .call(
                "POST",
                "/internal/v1/annotations",
                "write-secret",
                v.to_string(),
            )
            .await;
        (s, serde_json::from_slice(&b).unwrap())
    }
    async fn detail(&self, id: &str) -> (StatusCode, Value) {
        let (s, b) = self
            .call(
                "GET",
                &format!(
                    "/internal/v1/annotations/{id}?database_id={}",
                    self.camera.database_id
                ),
                "read-secret",
                String::new(),
            )
            .await;
        (s, serde_json::from_slice(&b).unwrap())
    }
}
#[tokio::test]
async fn dual_sources_auth_guards_duplicate_keys_and_atomic_validation() {
    let f = Fixture::new();
    let body = f.payload(&f.camera, "auth");
    assert_eq!(
        f.call(
            "POST",
            "/internal/v1/annotations",
            "read-secret",
            "not json".into()
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        f.call(
            "GET",
            "/internal/v1/annotations?database_id=x",
            "write-secret",
            String::new()
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    let duplicate = body.to_string().replacen(
        "\"schema_version\":1",
        "\"schema_version\":1,\"schema_version\":1",
        1,
    );
    assert_eq!(
        f.call(
            "POST",
            "/internal/v1/annotations",
            "write-secret",
            duplicate
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let (status, b) = f
        .call(
            "POST",
            "/internal/v1/annotations",
            "write-secret",
            " ".repeat(256 * 1024 + 1),
        )
        .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        serde_json::from_slice::<Value>(&b).unwrap()["error"]["code"],
        "payload_too_large"
    );
    let mut bad = body.clone();
    bad["camera_id"] = json!("desk");
    assert_eq!(f.post(&bad).await.1["error"]["code"], "camera_mismatch");
    bad = body.clone();
    bad["database_id"] = json!("wrong");
    assert_eq!(f.post(&bad).await.1["error"]["code"], "dataset_mismatch");
    bad = body.clone();
    bad["sha256"] = json!("0".repeat(64));
    assert_eq!(
        f.post(&bad).await.1["error"]["code"],
        "source_hash_mismatch"
    );
    bad = body.clone();
    bad["annotations"][1]["object_id"] = json!("one");
    assert_eq!(f.post(&bad).await.0, StatusCode::BAD_REQUEST);
    let (s, receipt) = f.post(&body).await;
    assert_eq!(s, StatusCode::CREATED);
    assert_eq!(receipt["object_count"], 2);
    let (s, obs) = f.post(&f.payload(&f.observed, "observation")).await;
    assert_eq!(s, StatusCode::CREATED);
    assert_ne!(receipt["annotation_id"], obs["annotation_id"]);
    let id = receipt["annotation_id"].as_str().unwrap();
    assert_eq!(
        f.call(
            "GET",
            &format!(
                "/internal/v1/annotations/{id}?database_id={}&camera_id=desk",
                f.camera.database_id
            ),
            "read-secret",
            String::new()
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    let (_, detail) = f.detail(id).await;
    assert_eq!(detail["annotations"].as_array().unwrap().len(), 2);
    let e = receipt["evidence_asset_id"].as_str().unwrap();
    let (s, b) = f
        .call(
            "GET",
            &format!("/internal/v1/assets/{e}"),
            "read-secret",
            String::new(),
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    let m: Value = serde_json::from_slice(&b).unwrap();
    assert_eq!(m["provenance"], "daemon_annotation_evidence");
    assert!(m["observation_id"].is_null());
    assert!(m["zone"].is_null());
}
#[tokio::test]
async fn expired_source_replay_revision_concurrency_and_restart_keep_evidence() {
    let f = Fixture::new();
    let body = f.payload(&f.camera, "stable");
    let (s, r) = f.post(&body).await;
    assert_eq!(s, StatusCode::CREATED);
    let receipt: AnnotationReceipt = serde_json::from_value(r.clone()).unwrap();
    tokio::time::sleep(Duration::from_millis(260)).await;
    f.assets.purge_expired_camera_assets().unwrap();
    let (s, replay) = f.post(&body).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(replay, r);
    let mut changed = body.clone();
    changed["annotations"] = json!([]);
    assert_eq!(
        f.post(&changed).await.1["error"]["code"],
        "idempotency_conflict"
    );
    let mut left = body.clone();
    left["annotation_id"] = json!(receipt.annotation_id);
    left["expected_revision"] = json!(1);
    left["request_id"] = json!("revision-a");
    let mut right = left.clone();
    right["request_id"] = json!("revision-b");
    right["annotations"] = json!([]);
    let (a, b) = tokio::join!(f.post(&left), f.post(&right));
    assert!(
        (a.0 == StatusCode::CREATED && b.1["error"]["code"] == "revision_conflict")
            || (b.0 == StatusCode::CREATED && a.1["error"]["code"] == "revision_conflict")
    );
    let (s, detail) = f.detail(&receipt.annotation_id).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(detail["current_revision"], 2);
    assert_eq!(detail["original_source_status"], "source_expired");
    assert_eq!(detail["evidence_status"], "available");
    let metadata = f.assets.metadata(&receipt.evidence_asset_id).unwrap();
    assert_eq!(metadata.sha256, f.camera.sha256);
    let restarted = AssetStore::new(
        f._dir.path().join("images"),
        8 * 1024 * 1024,
        Some("read-secret".into()),
        f.db.clone(),
    )
    .unwrap();
    assert_eq!(
        restarted
            .content(&receipt.evidence_asset_id)
            .unwrap()
            .0
            .sha256,
        f.camera.sha256
    );
    std::fs::write(
        f._dir
            .path()
            .join("images/annotation-evidence")
            .join(format!("{}.jpg", receipt.evidence_asset_id)),
        b"corrupt",
    )
    .unwrap();
    let (_, detail) = f.detail(&receipt.annotation_id).await;
    assert_eq!(detail["evidence_status"], "integrity_error");
    assert_eq!(detail["current_revision"], 2);
}
