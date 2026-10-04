use super::*;
use axum::body::to_bytes;
use axum::http::Request;
use tower::ServiceExt;

fn fixture(ttl: Duration) -> (tempfile::TempDir, AssetStore) {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(std::sync::Mutex::new(
        item_core::store::Store::in_memory().unwrap(),
    ));
    let store = AssetStore::new_with_ttls(
        dir.path(),
        DEFAULT_MAX_ASSET_BYTES,
        Some("secret".into()),
        db,
        ttl,
        Duration::from_secs(5),
    )
    .unwrap();
    (dir, store)
}

fn jpeg() -> Vec<u8> {
    let mut bytes = Vec::new();
    image::codecs::jpeg::JpegEncoder::new(&mut bytes)
        .encode(&[10; 18], 3, 2, image::ExtendedColorType::Rgb8)
        .unwrap();
    bytes
}

fn frame_request(store: &AssetStore) -> CameraFrameRequest {
    CameraFrameRequest {
        database_id: store.database_id.to_string(),
        max_age_ms: 2000,
        wait_timeout_ms: 0,
        max_bytes: DEFAULT_MAX_ASSET_BYTES,
    }
}

async fn post(store: &AssetStore, payload: String, token: &str) -> (StatusCode, serde_json::Value) {
    let response = router(store.clone(), "127.0.0.1:8480".parse().unwrap())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/internal/v1/cameras/cam/frame")
                .header("host", "127.0.0.1:8480")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(payload))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 16384).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn camera_freeze_and_reread_expire_without_observation_or_renewal() {
    let (dir, store) = fixture(Duration::from_secs(2));
    let cache = store.frame_cache().unwrap();
    cache.register("cam").unwrap();
    let token = cache.set_state("cam", "running");
    assert!(cache.publish(
        "cam",
        token,
        vec![10; 18],
        3,
        2,
        chrono::Utc::now(),
        std::time::Instant::now()
    ));
    let frame = cache
        .wait_fresh("cam", Duration::from_secs(1), Duration::ZERO)
        .await
        .unwrap();
    let first = store.freeze_camera_frame("cam", &frame).unwrap();
    let again = store.freeze_camera_frame("cam", &frame).unwrap();
    assert_eq!(first, again);
    assert!(first.observation_id.is_none());
    assert!(first.zone.is_none());
    assert_eq!(store.metadata(&first.asset_id).unwrap(), first);
    assert!(store.db.lock().unwrap().recent(None, 1).unwrap().is_empty());
    let restarted = AssetStore::new_with_ttl(
        dir.path(),
        DEFAULT_MAX_ASSET_BYTES,
        Some("secret".into()),
        store.db.clone(),
        Duration::from_secs(2),
    )
    .unwrap();
    assert_eq!(restarted.metadata(&first.asset_id).unwrap(), first);
    tokio::time::sleep(Duration::from_millis(2100)).await;
    assert_eq!(
        store.freeze_camera_frame("cam", &frame),
        Err(AssetFailure::Missing)
    );
    assert_eq!(store.metadata(&first.asset_id), Err(AssetFailure::Missing));
    assert_eq!(store.purge_expired_camera_assets().unwrap(), 2);
    assert!(!dir.path().join(format!("{}.jpg", first.asset_id)).exists());
    assert_eq!(store.metadata(&first.asset_id), Err(AssetFailure::Missing));
}

#[test]
fn camera_asset_quota_never_evicts_live_images_and_duplicate_is_allowed() {
    let (_dir, store) = fixture(Duration::from_secs(600));
    let bytes = jpeg();
    let now = chrono::Utc::now();
    let mut first = None;
    for i in 0..MAX_CAMERA_ASSETS {
        let asset = store
            .publish_camera_jpeg(
                "cam",
                now + chrono::Duration::microseconds(i as i64),
                &bytes,
            )
            .unwrap();
        first.get_or_insert(asset);
    }
    let first = first.unwrap();
    assert_eq!(
        store.publish_camera_jpeg("cam", now, &bytes).unwrap(),
        first
    );
    let error = store
        .publish_camera_jpeg("cam", now + chrono::Duration::seconds(1), &bytes)
        .unwrap_err();
    assert_eq!(map_io(error), AssetFailure::Busy);
    assert!(store.content(&first.asset_id).is_ok());
}

#[tokio::test]
async fn post_is_authenticated_bounded_and_dataset_checked_before_freeze() {
    let (_dir, store) = fixture(Duration::from_secs(600));
    let cache = store.frame_cache().unwrap();
    cache.register("cam").unwrap();
    let token = cache.set_state("cam", "running");
    cache.publish(
        "cam",
        token,
        vec![10; 18],
        3,
        2,
        chrono::Utc::now(),
        std::time::Instant::now(),
    );
    let payload = serde_json::to_string(&frame_request(&store)).unwrap();
    assert_eq!(
        post(&store, "x".repeat(20000), "wrong").await.0,
        StatusCode::UNAUTHORIZED
    );
    let too_big = post(&store, "x".repeat(20000), "secret").await;
    assert_eq!(too_big.0, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(too_big.1["error"]["code"], "payload_too_large");
    assert_eq!(
        post(&store, "{".into(), "secret").await.1["error"]["code"],
        "invalid_input"
    );
    let mut wrong = frame_request(&store);
    wrong.database_id = "wrong".into();
    assert_eq!(
        post(&store, serde_json::to_string(&wrong).unwrap(), "secret")
            .await
            .1["error"]["code"],
        "dataset_mismatch"
    );
    assert_eq!(std::fs::read_dir(store.root.as_ref()).unwrap().count(), 0);
    let permit = store.readers.clone().acquire_many_owned(4).await.unwrap();
    assert_eq!(
        post(&store, payload.clone(), "secret").await.1["error"]["code"],
        "busy"
    );
    drop(permit);
    let mut small = frame_request(&store);
    small.max_bytes = 1;
    assert_eq!(
        post(&store, serde_json::to_string(&small).unwrap(), "secret")
            .await
            .1["error"]["code"],
        "payload_too_large"
    );
    assert_eq!(std::fs::read_dir(store.root.as_ref()).unwrap().count(), 0);
    let result = post(&store, payload.clone(), "secret").await;
    assert_eq!(result.0, StatusCode::OK, "{result:?}");
    cache.set_state("cam", "stopped");
    assert_eq!(
        post(&store, payload, "secret").await.1["error"]["code"],
        "camera_offline"
    );
}

#[test]
fn startup_removes_expired_frame_files_and_read_rejects_other_dataset() {
    let (dir, store) = fixture(Duration::from_secs(2));
    let metadata = store
        .publish_camera_jpeg("cam", chrono::Utc::now(), &jpeg())
        .unwrap();
    let db = Arc::new(std::sync::Mutex::new(
        item_core::store::Store::in_memory().unwrap(),
    ));
    let other = AssetStore::new(
        dir.path(),
        DEFAULT_MAX_ASSET_BYTES,
        Some("secret".into()),
        db,
    )
    .unwrap();
    assert_eq!(
        other.metadata(&metadata.asset_id),
        Err(AssetFailure::Changed)
    );
    std::thread::sleep(Duration::from_millis(2100));
    let restarted = AssetStore::new(
        dir.path(),
        DEFAULT_MAX_ASSET_BYTES,
        Some("secret".into()),
        store.db.clone(),
    )
    .unwrap();
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    assert_eq!(
        restarted.metadata(&metadata.asset_id),
        Err(AssetFailure::Missing)
    );
}
