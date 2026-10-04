use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use item_core::assets::{CameraFrameMetadata, CameraFrameRequest, RawAssetMetadata};
use item_mcp::images::{DaemonClient, FrameRequest, ImageKind, ImagesRequest};
use rmcp::model::CallToolResult;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct FakeDaemon {
    endpoint: String,
    requests: Arc<Mutex<Vec<(String, Value)>>>,
    task: tokio::task::JoinHandle<()>,
}

impl FakeDaemon {
    async fn start(mode: &'static str) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        let task = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let recorded = recorded.clone();
                tokio::spawn(async move {
                    let mut bytes = Vec::new();
                    let (header_end, length) = loop {
                        let mut buf = [0u8; 4096];
                        let n = stream.read(&mut buf).await.unwrap();
                        if n == 0 {
                            return;
                        }
                        bytes.extend_from_slice(&buf[..n]);
                        assert!(bytes.len() < 16384);
                        if let Some(end) = bytes.windows(4).position(|s| s == b"\r\n\r\n") {
                            let headers =
                                String::from_utf8_lossy(&bytes[..end]).to_ascii_lowercase();
                            let length = headers
                                .lines()
                                .find_map(|line| line.strip_prefix("content-length: "))
                                .map(|s| s.parse::<usize>().unwrap())
                                .unwrap_or(0);
                            break (end + 4, length);
                        }
                    };
                    while bytes.len() < header_end + length {
                        let mut buf = [0u8; 4096];
                        let n = stream.read(&mut buf).await.unwrap();
                        if n == 0 {
                            return;
                        }
                        bytes.extend_from_slice(&buf[..n]);
                    }
                    let headers = String::from_utf8_lossy(&bytes[..header_end]);
                    assert!(
                        headers
                            .to_ascii_lowercase()
                            .contains("authorization: bearer test-token\r\n")
                    );
                    let line = headers.lines().next().unwrap();
                    let method = line.split_whitespace().next().unwrap();
                    let path = line.split_whitespace().nth(1).unwrap().to_owned();
                    let body: Value = if length == 0 {
                        Value::Null
                    } else {
                        serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap()
                    };
                    recorded.lock().unwrap().push((path.clone(), body.clone()));
                    let image = image::RgbImage::from_pixel(3, 2, image::Rgb([10, 20, 30]));
                    let mut jpeg = Vec::new();
                    image::codecs::jpeg::JpegEncoder::new(&mut jpeg)
                        .encode_image(&image)
                        .unwrap();
                    if mode == "corrupt" {
                        // Keep the headers/SOF/SOS dimensions valid but remove the scan.
                        let sos = jpeg.windows(2).position(|s| s == [0xff, 0xda]).unwrap();
                        let segment_len =
                            u16::from_be_bytes([jpeg[sos + 2], jpeg[sos + 3]]) as usize;
                        jpeg.truncate(sos + 2 + segment_len);
                        assert!(
                            image::ImageReader::with_format(
                                std::io::Cursor::new(&jpeg),
                                image::ImageFormat::Jpeg
                            )
                            .into_dimensions()
                            .is_ok()
                        );
                    }
                    let now = chrono::Utc::now();
                    let capture = if mode == "stale" {
                        now - chrono::Duration::seconds(10)
                    } else {
                        now
                    };
                    let mut asset = RawAssetMetadata {
                        asset_id: "frame-test".into(),
                        database_id: "dataset".into(),
                        kind: "raw".into(),
                        mime_type: "image/jpeg".into(),
                        bytes: jpeg.len() as u64,
                        sha256: format!("{:x}", Sha256::digest(&jpeg)),
                        width: Some(3),
                        height: Some(2),
                        capture_time: Some(capture.to_rfc3339()),
                        observation_id: None,
                        camera_id: Some("摄像头/desk?x#y%z".into()),
                        zone: None,
                        status: "available".into(),
                        provenance: "daemon_camera_frame".into(),
                        expires_at: Some((now + chrono::Duration::seconds(60)).to_rfc3339()),
                    };
                    match mode {
                        "wrong_camera" => asset.camera_id = Some("different-camera".into()),
                        "wrong_dataset" => asset.database_id = "other-dataset".into(),
                        "observation" => asset.observation_id = Some(9),
                        "missing_expiry" => asset.expires_at = None,
                        "expired" => {
                            asset.capture_time =
                                Some((now - chrono::Duration::seconds(10)).to_rfc3339());
                            asset.expires_at =
                                Some((now - chrono::Duration::seconds(1)).to_rfc3339());
                        }
                        "expires_during_body" => {
                            asset.expires_at =
                                Some((now + chrono::Duration::milliseconds(200)).to_rfc3339())
                        }
                        _ => {}
                    }
                    let (status, mime, extra, data) = if path.ends_with("/content") {
                        if mode == "expires_during_body" {
                            tokio::time::sleep(Duration::from_millis(350)).await;
                        }
                        (
                            200,
                            "image/jpeg",
                            format!(
                                "x-asset-id: frame-test\r\nx-asset-sha256: {}\r\n",
                                asset.sha256
                            ),
                            jpeg,
                        )
                    } else if path.ends_with("/frame") {
                        assert_eq!(method, "POST");
                        let input: CameraFrameRequest = serde_json::from_value(body).unwrap();
                        assert_eq!(input.database_id, "dataset");
                        assert_eq!(input.max_age_ms, 2000);
                        assert_eq!(input.max_bytes, 4194304);
                        if mode == "timeout" {
                            tokio::time::sleep(Duration::from_millis(500)).await;
                        }
                        if mode == "offline" {
                            (503, "application/json", String::new(), serde_json::to_vec(&json!({"error":{"code":"camera_offline","message":"private path/token details"}})).unwrap())
                        } else {
                            (
                                200,
                                "application/json",
                                String::new(),
                                serde_json::to_vec(&CameraFrameMetadata {
                                    asset,
                                    age_ms: if mode == "stale" { 10000 } else { 0 },
                                    camera_state: if mode == "offline_success" {
                                        "stopped".into()
                                    } else {
                                        "running".into()
                                    },
                                })
                                .unwrap(),
                            )
                        }
                    } else {
                        (
                            200,
                            "application/json",
                            String::new(),
                            serde_json::to_vec(&asset).unwrap(),
                        )
                    };
                    let header = format!(
                        "HTTP/1.1 {status} Test\r\nContent-Type: {mime}\r\nContent-Length: {}\r\n{extra}Connection: close\r\n\r\n",
                        data.len()
                    );
                    if stream.write_all(header.as_bytes()).await.is_ok() {
                        let _ = stream.write_all(&data).await;
                    }
                });
            }
        });
        Self {
            endpoint,
            requests,
            task,
        }
    }

    fn client(&self, timeout: Duration) -> DaemonClient {
        DaemonClient::new(&self.endpoint, "test-token", timeout).unwrap()
    }
}

impl Drop for FakeDaemon {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn frame_request() -> FrameRequest {
    FrameRequest {
        camera_id: "摄像头/desk?x#y%z".into(),
        max_age_ms: None,
        wait_timeout_ms: None,
        max_bytes: None,
    }
}

fn code(result: &CallToolResult) -> String {
    result.structured_content.as_ref().unwrap()["error"]["code"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[tokio::test]
async fn current_frame_roundtrip_preserves_encoded_id_and_has_no_observation() {
    let daemon = FakeDaemon::start("valid").await;
    let client = daemon.client(Duration::from_secs(3));
    let result = client
        .camera_frame(frame_request(), "dataset".into())
        .await
        .unwrap();
    let metadata = result.structured_content.as_ref().unwrap();
    assert_eq!(metadata["source"], "daemon_camera_frame");
    assert_eq!(metadata["images"][0]["observation_id"], Value::Null);
    assert_eq!(metadata["age_ms"], 0);
    assert_eq!(metadata["camera_state"], "running");
    let text = serde_json::to_value(&result.content[0]).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(text["text"].as_str().unwrap()).unwrap(),
        *metadata
    );
    let image = serde_json::to_value(&result.content[1]).unwrap();
    let jpeg = base64::engine::general_purpose::STANDARD
        .decode(image["data"].as_str().unwrap())
        .unwrap();
    assert_eq!(
        format!("{:x}", Sha256::digest(&jpeg)),
        metadata["images"][0]["sha256"]
    );
    {
        let requests = daemon.requests.lock().unwrap();
        assert_eq!(
            requests[0].0,
            "/internal/v1/cameras/%E6%91%84%E5%83%8F%E5%A4%B4%2Fdesk%3Fx%23y%25z/frame"
        );
        assert_eq!(requests[0].1["database_id"], "dataset");
    }
    let reread = client
        .images(
            ImagesRequest {
                image_kind: ImageKind::Raw,
                observation_ids: vec![],
                asset_ids: vec!["frame-test".into()],
                max_images: None,
                max_bytes_each: None,
            },
            "dataset".into(),
        )
        .await
        .unwrap();
    assert_eq!(
        reread.structured_content.unwrap()["images"][0]["observation_id"],
        Value::Null
    );
}

#[tokio::test]
async fn invalid_inputs_never_send_a_freeze_request() {
    let daemon = FakeDaemon::start("valid").await;
    let client = daemon.client(Duration::from_secs(1));
    for id in ["", ".", "..", "bad\nname"] {
        let mut request = frame_request();
        request.camera_id = id.into();
        assert_eq!(
            code(
                &client
                    .camera_frame(request, "dataset".into())
                    .await
                    .unwrap_err()
            ),
            "invalid_input"
        );
    }
    for (age, wait, bytes) in [
        (0, 0, 1),
        (60001, 0, 1),
        (1, 10001, 1),
        (1, 0, 0),
        (1, 0, 8388609),
    ] {
        let mut request = frame_request();
        request.max_age_ms = Some(age);
        request.wait_timeout_ms = Some(wait);
        request.max_bytes = Some(bytes);
        assert_eq!(
            code(
                &client
                    .camera_frame(request, "dataset".into())
                    .await
                    .unwrap_err()
            ),
            "invalid_input"
        );
    }
    assert!(daemon.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn wrong_ownership_stale_and_expired_metadata_are_rejected_before_content() {
    for (mode, expected) in [
        ("wrong_camera", "invalid_daemon_response"),
        ("wrong_dataset", "dataset_mismatch"),
        ("observation", "invalid_daemon_response"),
        ("missing_expiry", "invalid_daemon_response"),
        ("stale", "frame_stale"),
        ("expired", "source_expired"),
        ("offline", "camera_offline"),
        ("offline_success", "invalid_daemon_response"),
    ] {
        let daemon = FakeDaemon::start(mode).await;
        let result = daemon
            .client(Duration::from_secs(2))
            .camera_frame(frame_request(), "dataset".into())
            .await
            .unwrap_err();
        assert_eq!(code(&result), expected, "{mode}");
        assert_eq!(daemon.requests.lock().unwrap().len(), 1, "{mode}");
        assert!(
            !serde_json::to_string(&result)
                .unwrap()
                .contains("private path/token")
        );
    }
}

#[tokio::test]
async fn complete_frame_deadline_and_mid_read_expiry_are_enforced() {
    let daemon = FakeDaemon::start("timeout").await;
    let began = std::time::Instant::now();
    let result = daemon
        .client(Duration::from_millis(100))
        .camera_frame(frame_request(), "dataset".into())
        .await
        .unwrap_err();
    assert_eq!(code(&result), "daemon_unavailable");
    assert!(began.elapsed() < Duration::from_millis(400));
    let daemon = FakeDaemon::start("expires_during_body").await;
    let result = daemon
        .client(Duration::from_secs(2))
        .camera_frame(frame_request(), "dataset".into())
        .await
        .unwrap_err();
    assert_eq!(code(&result), "source_expired");
}

#[tokio::test]
async fn jpeg_with_valid_headers_but_corrupt_scan_is_rejected() {
    let daemon = FakeDaemon::start("corrupt").await;
    let result = daemon
        .client(Duration::from_secs(2))
        .camera_frame(frame_request(), "dataset".into())
        .await
        .unwrap_err();
    assert_eq!(code(&result), "invalid_daemon_response");
}
