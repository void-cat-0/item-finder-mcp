mod support;

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread::JoinHandle;

use base64::Engine;
use item_core::assets::RawAssetMetadata;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use support::Client;

struct Daemon {
    endpoint: String,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    jpeg: Vec<u8>,
    _dir: tempfile::TempDir,
    db: std::path::PathBuf,
}

impl Daemon {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("items.db");
        let store = item_core::store::Store::open(&db).unwrap();
        let database_id = store.ensure_database_identity().unwrap();
        drop(store);
        let image = image::RgbImage::from_pixel(3, 2, image::Rgb([10, 20, 30]));
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut jpeg)
            .encode_image(&image)
            .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let worker_jpeg = jpeg.clone();
        let frame_capture = chrono::Utc::now().to_rfc3339();
        let frame_expiry = (chrono::Utc::now() + chrono::Duration::seconds(60)).to_rfc3339();
        let thread = std::thread::spawn(move || {
            while !worker_stop.load(Ordering::SeqCst) {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => panic!("accept: {error}"),
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                    .unwrap();
                let mut request = Vec::new();
                let mut buf = [0; 4096];
                while !request.windows(4).any(|s| s == b"\r\n\r\n") {
                    let count = stream.read(&mut buf).unwrap();
                    if count == 0 {
                        break;
                    }
                    request.extend_from_slice(&buf[..count]);
                }
                let request = String::from_utf8(request).unwrap();
                let path = request.split_whitespace().nth(1).unwrap();
                let authorized = request
                    .to_ascii_lowercase()
                    .contains("authorization: bearer test-secret\r\n");
                let (status, mime, headers, body) = if !authorized {
                    (401, "application/json", String::new(), json!({"error":{"code":"unauthorized","message":"secret must never be relayed"}}).to_string().into_bytes())
                } else if path.contains("/observations/2/") {
                    (
                        404,
                        "application/json",
                        String::new(),
                        json!({"error":{"code":"raw_unavailable","message":"raw missing"}})
                            .to_string()
                            .into_bytes(),
                    )
                } else if path.contains("/observations/99/") {
                    (
                        404,
                        "application/json",
                        String::new(),
                        json!({"error":{"code":"not_found","message":"unknown"}})
                            .to_string()
                            .into_bytes(),
                    )
                } else if path.ends_with("/frame") {
                    let metadata = RawAssetMetadata {
                        database_id: database_id.clone(),
                        asset_id: "frame-test".into(),
                        kind: "raw".into(),
                        mime_type: "image/jpeg".into(),
                        bytes: worker_jpeg.len() as u64,
                        sha256: format!("{:x}", Sha256::digest(&worker_jpeg)),
                        width: Some(3),
                        height: Some(2),
                        capture_time: Some(frame_capture.clone()),
                        observation_id: None,
                        camera_id: Some("test-cam".into()),
                        zone: None,
                        status: "available".into(),
                        provenance: "daemon_camera_frame".into(),
                        expires_at: Some(frame_expiry.clone()),
                    };
                    (
                        200,
                        "application/json",
                        String::new(),
                        serde_json::to_vec(
                            &json!({"asset":metadata,"age_ms":10,"camera_state":"running"}),
                        )
                        .unwrap(),
                    )
                } else if path.ends_with("/content") {
                    let asset = path.split('/').nth(4).unwrap();
                    let hash = if asset == "bad-hash" {
                        "0".repeat(64)
                    } else {
                        format!("{:x}", Sha256::digest(&worker_jpeg))
                    };
                    (
                        200,
                        "image/jpeg",
                        format!("x-asset-id: {asset}\r\nx-asset-sha256: {hash}\r\n"),
                        worker_jpeg.clone(),
                    )
                } else {
                    let id = if path.contains("/observations/") {
                        "raw-test"
                    } else {
                        path.rsplit('/').next().unwrap()
                    };
                    let conflicting = path.contains("/observations/3/");
                    let is_frame = id == "frame-test";
                    let is_evidence = id.starts_with("evidence-v1-");
                    let metadata = RawAssetMetadata {
                        database_id: database_id.clone(),
                        asset_id: id.into(),
                        kind: "raw".into(),
                        mime_type: "image/jpeg".into(),
                        bytes: worker_jpeg.len() as u64,
                        sha256: if id == "bad-hash" {
                            "0".repeat(64)
                        } else {
                            format!("{:x}", Sha256::digest(&worker_jpeg))
                        },
                        width: Some(3),
                        height: Some(2),
                        capture_time: Some(if is_frame {
                            frame_capture.clone()
                        } else {
                            "2026-10-08T10:00:00Z".into()
                        }),
                        observation_id: if is_frame || is_evidence {
                            None
                        } else {
                            Some(if conflicting { 3 } else { 1 })
                        },
                        camera_id: Some("test-cam".into()),
                        zone: if is_frame || is_evidence {
                            None
                        } else {
                            Some("desk".into())
                        },
                        status: "available".into(),
                        provenance: if is_frame {
                            "daemon_camera_frame".into()
                        } else if is_evidence {
                            "daemon_annotation_evidence".into()
                        } else {
                            "daemon_raw_asset".into()
                        },
                        expires_at: if is_frame || is_evidence {
                            Some(frame_expiry.clone())
                        } else {
                            None
                        },
                    };
                    (
                        200,
                        "application/json",
                        String::new(),
                        serde_json::to_vec(&metadata).unwrap(),
                    )
                };
                write!(stream, "HTTP/1.1 {status} Test\r\nContent-Type: {mime}\r\nContent-Length: {}\r\n{headers}Connection: close\r\n\r\n", body.len()).unwrap();
                stream.write_all(&body).unwrap();
            }
        });
        Self {
            endpoint,
            stop,
            thread: Some(thread),
            jpeg,
            _dir: dir,
            db,
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let result = self.thread.take().unwrap().join();
        if !std::thread::panicking() {
            result.unwrap();
        }
    }
}

#[test]
fn stdio_returns_verified_raw_image_and_metadata_and_recovers_from_errors() {
    let daemon = Daemon::new();
    let mut client = Client::start_with_daemon(
        true,
        Some(&daemon.db),
        None,
        Some((&daemon.endpoint, "test-secret")),
    );
    client.initialize("2025-11-25");
    let response = client.request(2, "tools/call", json!({"name":"get_item_images","arguments":{"image_kind":"raw","observation_ids":[1],"asset_ids":["raw-test"]}}));
    let result = &response["result"];
    assert_eq!(result["isError"], false, "{response}");
    let metadata = &result["structuredContent"];
    assert_eq!(metadata["images"].as_array().unwrap().len(), 1);
    assert_eq!(metadata["images"][0]["content_index"], 1);
    assert_eq!(metadata["images"][0]["image_kind"], "raw");
    assert_eq!(metadata["truncated"], false);
    assert_eq!(
        serde_json::from_str::<Value>(result["content"][0]["text"].as_str().unwrap()).unwrap(),
        *metadata
    );
    assert_eq!(result["content"][1]["type"], "image");
    assert_eq!(result["content"][1]["mimeType"], "image/jpeg");
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(result["content"][1]["data"].as_str().unwrap())
            .unwrap(),
        daemon.jpeg
    );
    client.tool_error(
        3,
        "get_item_images",
        json!({"image_kind":"raw","observation_ids":[2]}),
        "raw_unavailable",
    );
    client.tool_error(
        4,
        "get_item_images",
        json!({"image_kind":"raw","observation_ids":[99]}),
        "not_found",
    );
    client.tool_error(
        5,
        "get_item_images",
        json!({"image_kind":"raw","asset_ids":["bad-hash"]}),
        "invalid_daemon_response",
    );
    client.tool_error(
        6,
        "get_item_images",
        json!({"image_kind":"raw","observation_ids":[1],"max_bytes_each":10}),
        "payload_too_large",
    );
    client.tool_error(
        7,
        "get_item_images",
        json!({"image_kind":"raw","asset_ids":["../secret"]}),
        "invalid_input",
    );
    let frame = client.tool(8, "get_camera_frame", json!({"camera_id":"test-cam"}));
    assert_eq!(frame["source"], "daemon_camera_frame");
    assert_eq!(frame["age_ms"], 10);
    assert_eq!(frame["camera_state"], "running");
    assert_eq!(frame["images"][0]["observation_id"], Value::Null);
    assert_eq!(frame["images"][0]["zone"], Value::Null);
    assert!(frame["images"][0]["expires_at"].is_string());
    assert_eq!(frame["images"][0]["provenance"], "daemon_camera_frame");
    client.tool_error(
        10,
        "get_item_images",
        json!({"image_kind":"raw","asset_ids":["frame-test"],"camera_id":"other-camera"}),
        "camera_mismatch",
    );
    let reread = client.tool(
        11,
        "get_item_images",
        json!({"image_kind":"raw","asset_ids":["frame-test"],"camera_id":"test-cam"}),
    );
    assert_eq!(reread["images"][0]["observation_id"], Value::Null);
    assert_eq!(
        reread["images"][0]["expires_at"],
        frame["images"][0]["expires_at"]
    );
    client.tool_error(
        8,
        "get_item_images",
        json!({"image_kind":"raw","observation_ids":[1,3]}),
        "invalid_daemon_response",
    );
    client.tool(9, "server_info", json!({}));
    client.finish();
    let other = tempfile::tempdir().unwrap();
    let other_db = other.path().join("items.db");
    let store = item_core::store::Store::open(&other_db).unwrap();
    store.ensure_database_identity().unwrap();
    drop(store);
    let mut client = Client::start_with_daemon(
        true,
        Some(&other_db),
        None,
        Some((&daemon.endpoint, "test-secret")),
    );
    client.initialize("2025-11-25");
    client.tool_error(
        2,
        "get_item_images",
        json!({"image_kind":"raw","observation_ids":[1]}),
        "dataset_mismatch",
    );
    client.finish();
}

#[test]
fn annotation_evidence_images_are_verified_independently_and_camera_guard_is_exact() {
    let daemon = Daemon::new();
    let mut client = Client::start_with_daemon(
        true,
        Some(&daemon.db),
        None,
        Some((&daemon.endpoint, "test-secret")),
    );
    client.initialize("2025-11-25");
    let id = "evidence-v1-1234567890abcdef";
    let first = client.tool(
        2,
        "get_item_images",
        json!({"image_kind":"raw","asset_ids":[id],"camera_id":"test-cam"}),
    );
    assert_eq!(
        first["images"][0]["provenance"],
        "daemon_annotation_evidence"
    );
    assert_eq!(first["images"][0]["observation_id"], Value::Null);
    assert_eq!(first["images"][0]["zone"], Value::Null);
    client.tool_error(
        3,
        "get_item_images",
        json!({"image_kind":"raw","asset_ids":[id],"camera_id":"other"}),
        "camera_mismatch",
    );
    let second = client.tool(
        4,
        "get_item_images",
        json!({"image_kind":"raw","asset_ids":[id]}),
    );
    assert_eq!(first["images"][0]["sha256"], second["images"][0]["sha256"]);
    assert_eq!(
        first["images"][0]["expires_at"],
        second["images"][0]["expires_at"]
    );
    client.tool_error(
        5,
        "get_item_images",
        json!({"image_kind":"raw","asset_ids":["evidence-v1-NOTHEX"]}),
        "invalid_daemon_response",
    );
    client.finish();
}

#[test]
fn image_access_requires_daemon_and_token_without_breaking_basic_queries() {
    let daemon = Daemon::new();
    let mut client = Client::start_with_daemon(
        true,
        Some(&daemon.db),
        None,
        Some((&daemon.endpoint, "wrong")),
    );
    client.initialize("2025-11-25");
    client.tool_error(
        2,
        "get_item_images",
        json!({"image_kind":"raw","observation_ids":[1]}),
        "unauthorized",
    );
    client.tool(3, "server_info", json!({}));
    client.finish();
    let mut client = Client::new(true);
    client.initialize("2025-11-25");
    client.tool_error(
        2,
        "get_item_images",
        json!({"image_kind":"raw","observation_ids":[1]}),
        "daemon_unavailable",
    );
    client.tool_error(
        3,
        "get_item_images",
        json!({"image_kind":"raw"}),
        "invalid_input",
    );
    client.tool(4, "server_info", json!({}));
    client.finish();
}

#[test]
fn daemon_endpoint_rejects_remote_urls_and_missing_tokens() {
    use item_mcp::images::DaemonClient;
    use std::time::Duration;
    for endpoint in [
        "https://127.0.0.1:8480",
        "http://example.com",
        "http://0.0.0.0",
        "http://127.0.0.1/assets",
        "http://user@127.0.0.1",
        "http://127.0.0.1/?token=secret",
    ] {
        assert!(DaemonClient::new(endpoint, "token", Duration::from_secs(1)).is_err());
    }
    assert!(DaemonClient::new("http://127.0.0.1", "", Duration::from_secs(1)).is_err());
}
