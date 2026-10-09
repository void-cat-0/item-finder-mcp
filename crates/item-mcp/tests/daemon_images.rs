mod support;

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use base64::Engine;
use chrono::TimeZone;
use item_core::store::{DEFAULT_DEDUP_WINDOW, Store};
use item_ingest::assets::AssetStore;
use item_ingest::config::{CliOverrides, EnvOverrides, RuntimeConfig};
use serde_json::json;
use support::Client;

#[test]
fn real_daemon_raw_asset_survives_restart_and_stdio_queries_survive_daemon_stop() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("items.db");
    let root = dir.path().join("snapshots");
    let store = Store::open(&db_path).unwrap();
    let capture_time = chrono::Utc.with_ymd_and_hms(2026, 10, 9, 10, 0, 0).unwrap();
    let (id, _, _) = store
        .record_sighting(
            "fixture-cam",
            "desk",
            "keys",
            capture_time,
            None,
            DEFAULT_DEDUP_WINDOW,
        )
        .unwrap();
    let owner = store.observation_by_id(id).unwrap().unwrap();
    let shared = Arc::new(Mutex::new(store));
    let asset = AssetStore::new(
        &root,
        8 * 1024 * 1024,
        Some("fixture-secret".into()),
        shared.clone(),
    )
    .unwrap();
    let metadata = asset
        .publish_rgb(&owner, capture_time, &[10; 18], 3, 2)
        .unwrap();
    drop(asset);
    drop(shared);

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let endpoint = format!("http://{addr}");
    let mut settings =
        RuntimeConfig::resolve(None, &CliOverrides::default(), &EnvOverrides::default()).unwrap();
    settings.db = db_path.display().to_string();
    settings.snapshots_dir = root.display().to_string();
    settings.health_file = dir.path().join("health.json").display().to_string();
    settings.events_file = String::new();
    settings.webhook_enabled = false;
    settings.internal_listen = Some(addr.to_string());
    settings.internal_token = Some("fixture-secret".into());
    settings.retention_days = 0;
    let stop = Arc::new(AtomicBool::new(false));
    let daemon_stop = stop.clone();
    let daemon = std::thread::spawn(move || {
        item_ingest::daemon::run_with_stop(settings, vec![], None, daemon_stop)
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    while std::net::TcpStream::connect(addr).is_err() {
        assert!(Instant::now() < deadline, "daemon listener did not start");
        std::thread::sleep(Duration::from_millis(20));
    }
    let mut client = Client::start_with_daemon(
        true,
        Some(&db_path),
        None,
        Some((&endpoint, "fixture-secret")),
    );
    client.initialize("2025-11-25");
    let cameras = client.tool(20, "list_cameras", json!({}));
    assert_eq!(cameras["cameras"].as_array().unwrap().len(), 0);
    let health = client.tool(21, "get_health", json!({}));
    assert_eq!(health["daemon_state"], "running");
    let response = client.request(
        2,
        "tools/call",
        json!({"name":"get_item_images","arguments":{"image_kind":"raw","observation_ids":[id]}}),
    );
    assert_eq!(response["result"]["isError"], false, "{response}");
    assert_eq!(
        response["result"]["structuredContent"]["images"][0]["asset_id"],
        metadata.asset_id
    );
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(response["result"]["content"][1]["data"].as_str().unwrap())
        .unwrap();
    assert_eq!(bytes.len() as u64, metadata.bytes);
    client.tool(
        3,
        "get_item_images",
        json!({"image_kind":"raw","asset_ids":[metadata.asset_id]}),
    );
    stop.store(true, Ordering::SeqCst);
    daemon.join().unwrap().unwrap();
    client.tool_error(
        4,
        "get_item_images",
        json!({"image_kind":"raw","observation_ids":[id]}),
        "daemon_unavailable",
    );
    let observation = client.tool(5, "get_observation", json!({"observation_id":id}));
    assert_eq!(observation["observation"]["label"], "keys");
    client.finish();
}
