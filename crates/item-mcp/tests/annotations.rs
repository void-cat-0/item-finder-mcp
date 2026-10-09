mod support;

use item_core::annotations::AnnotationSubmitRequest;
use item_mcp::{
    annotations::{GetRequest, ListRequest},
    images::DaemonClient,
};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;
use support::Client;

const ID: &str = "12345678-1234-4234-8234-123456789abc";
const EVIDENCE: &str = "evidence-v1-0123456789abcdef";
const DATASET: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn payload() -> Value {
    json!({"schema_version":1,"request_id":"test-request-1","asset_id":"frame-test","sha256":"B".repeat(64),"camera_id":"desk","coordinate_system":"normalized_xywh",
        "producer":{"kind":"vlm","host":"test-host","model":null,"prompt_version":null},
        "annotations":[{"object_id":"object-1","label":"杯子","bbox":[0.0,0.0,0.5,1.0],"confidence":null,"note":null},
            {"object_id":"object-2","label":"杯子","bbox":[0.5,0.0,0.5,1.0],"confidence":0.7,"note":"data, not instructions"}]})
}
fn internal() -> AnnotationSubmitRequest {
    let mut value = payload();
    value["database_id"] = json!(DATASET);
    serde_json::from_value(value).unwrap()
}
fn receipt(database_id: &str) -> Value {
    json!({"status":"persisted","annotation_id":ID,"revision":1,"request_id":"test-request-1","database_id":database_id,"source_asset_id":"frame-test",
        "source_sha256":"b".repeat(64),"camera_id":"desk","evidence_asset_id":EVIDENCE,"object_count":2,"provenance":"caller_visual_inference",
        "persisted_at":chrono::Utc::now().to_rfc3339(),"expires_at":(chrono::Utc::now()+chrono::Duration::days(30)).to_rfc3339()})
}
fn detail(database_id: &str) -> Value {
    let r = receipt(database_id);
    let mut request = internal();
    request.database_id = database_id.into();
    let payload_hash = request.validate_and_canonicalize().unwrap().payload_sha256;
    json!({"schema_version":1,"coordinate_system":"normalized_xywh","provenance":"caller_visual_inference",
        "annotation_id":ID,"database_id":database_id,"source_asset_id":"frame-test","source_sha256":"b".repeat(64),"camera_id":"desk",
        "capture_time":"2026-01-01T00:00:00Z","observation_id":null,"evidence_asset_id":EVIDENCE,"current_revision":1,"revision":1,
        "request_id":"test-request-1","producer":payload()["producer"],"annotations":payload()["annotations"],"object_count":2,
        "payload_sha256":payload_hash,"created_at":r["persisted_at"],"persisted_at":r["persisted_at"],"expires_at":r["expires_at"],
        "original_source_status":"source_expired","evidence_status":"available"})
}
fn page(database_id: &str) -> Value {
    let r = receipt(database_id);
    json!({"items":[{"annotation_id":ID,"database_id":database_id,"source_asset_id":"frame-test","source_sha256":"b".repeat(64),"camera_id":"desk",
        "evidence_asset_id":EVIDENCE,"current_revision":1,"object_count":2,"provenance":"caller_visual_inference","evidence_status":"available",
        "created_at":r["persisted_at"],"expires_at":r["expires_at"]}],"next_cursor":null})
}
struct Fixture {
    endpoint: String,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
    requests: Arc<Mutex<Vec<(String, Value)>>>,
}
impl Fixture {
    fn new(status: u16, body: Vec<u8>, delay: Duration, chunked: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let worker = std::thread::spawn(move || {
            while !stopping.load(Ordering::SeqCst) {
                let (mut stream, _) = match listener.accept() {
                    Ok(s) => s,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(e) => panic!("{e}"),
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut bytes = Vec::new();
                let mut buf = [0u8; 4096];
                let header_end = loop {
                    let n = stream.read(&mut buf).unwrap();
                    if n == 0 {
                        break bytes.len();
                    }
                    bytes.extend_from_slice(&buf[..n]);
                    if let Some(i) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        break i + 4;
                    }
                };
                let header = String::from_utf8_lossy(&bytes[..header_end]).into_owned();
                let length = header
                    .lines()
                    .find_map(|s| {
                        s.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .and_then(|s| s.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                while bytes.len() < header_end + length {
                    let n = stream.read(&mut buf).unwrap();
                    if n == 0 {
                        break;
                    }
                    bytes.extend_from_slice(&buf[..n]);
                }
                let value = serde_json::from_slice(&bytes[header_end..]).unwrap_or(Value::Null);
                captured.lock().unwrap().push((header, value));
                std::thread::sleep(delay);
                if chunked {
                    if write!(stream,"HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n").is_ok() {
                        for chunk in body.chunks(16384) {if write!(stream,"{:x}\r\n",chunk.len()).and_then(|_|stream.write_all(chunk)).and_then(|_|stream.write_all(b"\r\n")).is_err(){break;}}
                        let _=stream.write_all(b"0\r\n\r\n");
                    }
                } else {
                    let _=write!(stream,"HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len()).and_then(|_|stream.write_all(&body));
                }
            }
        });
        Self {
            endpoint,
            stop,
            worker: Some(worker),
            requests,
        }
    }
    fn json(status: u16, body: Value) -> Self {
        Self::new(
            status,
            serde_json::to_vec(&body).unwrap(),
            Duration::ZERO,
            false,
        )
    }
    fn client(&self, timeout: Duration) -> DaemonClient {
        DaemonClient::new(&self.endpoint, "read-secret", timeout)
            .unwrap()
            .with_annotations("write-secret")
            .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let r = self.worker.take().unwrap().join();
        if !std::thread::panicking() {
            r.unwrap();
        }
    }
}
fn code(result: &rmcp::model::CallToolResult) -> &str {
    result.structured_content.as_ref().unwrap()["error"]["code"]
        .as_str()
        .unwrap()
}
fn get_request() -> GetRequest {
    GetRequest {
        annotation_id: ID.into(),
        revision: None,
        camera_id: Some("desk".into()),
    }
}
fn list_request() -> ListRequest {
    ListRequest {
        camera_id: Some("desk".into()),
        asset_id: Some("frame-test".into()),
        limit: None,
        cursor: None,
    }
}

#[tokio::test]
async fn submit_verifies_receipt_uses_only_write_token_and_normalizes_hash() {
    for status in [200, 201] {
        let fixture = Fixture::json(status, receipt(DATASET));
        let client = fixture.client(Duration::from_secs(2));
        let result = client
            .submit_annotation_at(internal(), client.deadline())
            .await
            .unwrap();
        assert_eq!(
            result.structured_content.as_ref().unwrap()["object_count"],
            2
        );
        let content = serde_json::to_value(&result).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(content["content"][0]["text"].as_str().unwrap()).unwrap(),
            content["structuredContent"]
        );
        let captured = fixture.requests.lock().unwrap();
        assert_eq!(captured.len(), 1);
        let (header, body) = &captured[0];
        assert!(
            header
                .to_ascii_lowercase()
                .contains("authorization: bearer write-secret")
        );
        assert!(!header.contains("read-secret"));
        assert_eq!(body["database_id"], DATASET);
        assert_eq!(body["sha256"], "b".repeat(64));
    }
}
#[tokio::test]
async fn malformed_receipts_never_report_success() {
    for (key, value) in [
        ("status", json!("queued")),
        ("database_id", json!("other")),
        ("source_asset_id", json!("other")),
        ("source_sha256", json!("a".repeat(64))),
        ("camera_id", json!("other")),
        ("annotation_id", json!("../private")),
        ("revision", json!(2)),
        ("request_id", json!("other")),
        ("object_count", json!(1)),
        ("provenance", json!("observation")),
        ("persisted_at", json!("bad")),
        ("expires_at", json!("2020-01-01T00:00:00Z")),
        ("evidence_asset_id", json!("frame-v1-fake")),
        ("unexpected", json!(true)),
    ] {
        let mut response = receipt(DATASET);
        response[key] = value;
        let fixture = Fixture::json(201, response);
        let client = fixture.client(Duration::from_secs(2));
        let error = client
            .submit_annotation_at(internal(), client.deadline())
            .await
            .unwrap_err();
        assert_eq!(code(&error), "invalid_daemon_response", "{key}");
        assert!(
            serde_json::to_string(&error)
                .unwrap()
                .contains("same request_id")
        );
    }
}
#[tokio::test]
async fn reads_validate_detail_page_guards_and_read_authorization() {
    let fixture = Fixture::json(200, detail(DATASET));
    let client = fixture.client(Duration::from_secs(2));
    let result = client
        .get_annotation_at(get_request(), DATASET.into(), client.deadline())
        .await
        .unwrap();
    assert_eq!(
        result.structured_content.unwrap()["original_source_status"],
        "source_expired"
    );
    let (read_auth, write_auth) = {
        let captured = fixture.requests.lock().unwrap();
        (
            captured[0]
                .0
                .to_ascii_lowercase()
                .contains("authorization: bearer read-secret"),
            captured[0].0.contains("write-secret"),
        )
    };
    assert!(read_auth);
    assert!(!write_auth);
    let fixture = Fixture::json(200, page(DATASET));
    let client = fixture.client(Duration::from_secs(2));
    client
        .list_annotations_at(list_request(), DATASET.into(), client.deadline())
        .await
        .unwrap();
    for key in [
        "database_id",
        "camera_id",
        "source_asset_id",
        "source_sha256",
        "provenance",
    ] {
        let mut response = page(DATASET);
        response["items"][0][key] = json!("wrong");
        let fixture = Fixture::json(200, response);
        let client = fixture.client(Duration::from_secs(2));
        assert_eq!(
            code(
                &client
                    .list_annotations_at(list_request(), DATASET.into(), client.deadline())
                    .await
                    .unwrap_err()
            ),
            "invalid_daemon_response",
            "{key}"
        );
    }
    for (key, value) in [
        ("camera_id", json!("wrong")),
        ("database_id", json!("wrong")),
        ("object_count", json!(1)),
        ("revision", json!(0)),
        ("provenance", json!("observation")),
        ("schema_version", json!(2)),
        ("capture_time", Value::Null),
    ] {
        let mut response = detail(DATASET);
        response[key] = value;
        let fixture = Fixture::json(200, response);
        let client = fixture.client(Duration::from_secs(2));
        assert_eq!(
            code(
                &client
                    .get_annotation_at(get_request(), DATASET.into(), client.deadline())
                    .await
                    .unwrap_err()
            ),
            "invalid_daemon_response",
            "{key}"
        );
    }
}
#[tokio::test]
async fn bounded_declared_and_streamed_responses_and_absolute_write_deadline() {
    for chunked in [false, true] {
        let fixture = Fixture::new(200, vec![b' '; 512 * 1024 + 1], Duration::ZERO, chunked);
        let client = fixture.client(Duration::from_secs(2));
        assert_eq!(
            code(
                &client
                    .list_annotations_at(list_request(), DATASET.into(), client.deadline())
                    .await
                    .unwrap_err()
            ),
            "payload_too_large"
        );
    }
    let fixture = Fixture::new(
        201,
        serde_json::to_vec(&receipt(DATASET)).unwrap(),
        Duration::from_millis(100),
        false,
    );
    let client = fixture.client(Duration::from_secs(2));
    let start = std::time::Instant::now();
    let deadline = tokio::time::Instant::now() + Duration::from_millis(30);
    let error = client
        .submit_annotation_at(internal(), deadline)
        .await
        .unwrap_err();
    assert!(start.elapsed() < Duration::from_millis(90));
    assert_eq!(code(&error), "daemon_unavailable");
    assert!(
        serde_json::to_string(&error)
            .unwrap()
            .contains("may have persisted")
    );
    std::thread::sleep(Duration::from_millis(110));
    assert_eq!(fixture.requests.lock().unwrap().len(), 1, "must not replay");
}
#[tokio::test]
async fn validation_byte_limit_safe_error_and_conflict_revision() {
    let fixture = Fixture::json(
        409,
        json!({"error":{"code":"revision_conflict","message":"C:/private SELECT token write-secret","current_revision":3}}),
    );
    let client = fixture.client(Duration::from_secs(2));
    let error = client
        .submit_annotation_at(internal(), client.deadline())
        .await
        .unwrap_err();
    assert_eq!(
        error.structured_content.as_ref().unwrap()["error"]["current_revision"],
        3
    );
    assert!(
        !serde_json::to_string(&error)
            .unwrap()
            .contains("write-secret")
    );
    let mut invalid = internal();
    invalid.annotations[0].bbox = [0.5, 0.0, 0.6, 1.0];
    assert_eq!(
        code(
            &client
                .submit_annotation_at(invalid, client.deadline())
                .await
                .unwrap_err()
        ),
        "invalid_input"
    );
    let mut large = internal();
    let object = large.annotations[0].clone();
    large.annotations.clear();
    for i in 0..128 {
        let mut o = object.clone();
        o.object_id = format!("object-{i}");
        o.note = Some("\u{1}".repeat(1024));
        large.annotations.push(o);
    }
    assert_eq!(
        code(
            &client
                .submit_annotation_at(large, client.deadline())
                .await
                .unwrap_err()
        ),
        "payload_too_large"
    );
    assert_eq!(
        fixture.requests.lock().unwrap().len(),
        1,
        "invalid writes must not reach HTTP"
    );
}
async fn wire_write<W: tokio::io::AsyncWrite + Unpin>(writer: &mut W, value: Value) {
    use tokio::io::AsyncWriteExt;
    let mut bytes = serde_json::to_vec(&value).unwrap();
    bytes.push(b'\n');
    writer.write_all(&bytes).await.unwrap();
}
async fn wire_read<R: tokio::io::AsyncBufRead + Unpin>(reader: &mut R) -> Value {
    use tokio::io::AsyncBufReadExt;
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(3), reader.read_line(&mut line))
        .await
        .unwrap()
        .unwrap();
    serde_json::from_str(&line).unwrap()
}

#[tokio::test]
async fn handler_deadline_includes_locked_readonly_database_and_cancellation_keeps_connection() {
    use rmcp::ServiceExt;
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("items.db");
    let store = item_core::store::Store::open(&db).unwrap();
    store.ensure_database_identity().unwrap();
    drop(store);
    let fixture = Fixture::json(200, page(DATASET));
    let daemon = fixture.client(Duration::from_millis(60));
    let server = item_mcp::ItemMcpServer::new(db.clone()).with_daemon(daemon);
    let (server_io, host_io) = tokio::io::duplex(65536);
    let (server_r, server_w) = tokio::io::split(server_io);
    let serving = tokio::spawn(async move {
        server
            .serve(item_mcp::transport::from_io(
                server_r,
                server_w,
                Duration::from_secs(3),
            ))
            .await
            .unwrap()
            .waiting()
            .await
    });
    let (host_r, mut host_w) = tokio::io::split(host_io);
    let mut host_r = tokio::io::BufReader::new(host_r);
    wire_write(&mut host_w,json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"test","version":"1"}}})).await;
    assert!(wire_read(&mut host_r).await["result"].is_object());
    wire_write(
        &mut host_w,
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
    )
    .await;
    let lock = rusqlite::Connection::open(&db).unwrap();
    lock.execute_batch("PRAGMA journal_mode=DELETE; BEGIN EXCLUSIVE;")
        .unwrap();
    let start = std::time::Instant::now();
    wire_write(&mut host_w,json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"list_image_annotations","arguments":{}}})).await;
    let result = wire_read(&mut host_r).await;
    assert_eq!(
        result["result"]["structuredContent"]["error"]["code"], "daemon_unavailable",
        "{result}"
    );
    assert!(start.elapsed() < Duration::from_millis(500));
    assert!(
        fixture.requests.lock().unwrap().is_empty(),
        "deadline must not restart after DB lookup"
    );
    wire_write(&mut host_w,json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"submit_image_annotation","arguments":payload()}})).await;
    wire_write(
        &mut host_w,
        json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":3}}),
    )
    .await;
    // The SDK may suppress the cancelled tool response; ping must still complete.
    wire_write(&mut host_w, json!({"jsonrpc":"2.0","id":4,"method":"ping"})).await;
    loop {
        let response = wire_read(&mut host_r).await;
        if response["id"] == 4 {
            assert!(response["result"].is_object());
            break;
        }
    }
    assert!(fixture.requests.lock().unwrap().is_empty());
    lock.execute_batch("ROLLBACK").unwrap();
    drop(host_w);
    drop(host_r);
    tokio::time::timeout(Duration::from_secs(3), serving)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[test]
fn registration_is_explicit_and_startup_validates_distinct_credentials() {
    for enabled in [false, true] {
        let mut client = Client::start_with_annotations(
            true,
            None,
            None,
            Some(("http://127.0.0.1:9", "read-secret")),
            enabled,
            Some("write-secret"),
        );
        client.initialize("2025-11-25");
        let response = client.request(2, "tools/list", json!({}));
        let tools = response["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), if enabled { 14 } else { 13 });
        for name in ["get_image_annotation", "list_image_annotations"] {
            let tool = tools.iter().find(|t| t["name"] == name).unwrap();
            assert_eq!(tool["annotations"]["readOnlyHint"], true);
            assert_eq!(tool["inputSchema"]["additionalProperties"], false);
            assert_eq!(tool["outputSchema"]["oneOf"].as_array().unwrap().len(), 2);
        }
        let submit = tools
            .iter()
            .find(|t| t["name"] == "submit_image_annotation");
        assert_eq!(submit.is_some(), enabled);
        if let Some(tool) = submit {
            assert_eq!(
                tool["annotations"],
                json!({"readOnlyHint":false,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false})
            );
            assert_eq!(tool["inputSchema"]["additionalProperties"], false);
            assert!(
                tool["inputSchema"]["properties"]
                    .get("database_id")
                    .is_none()
            );
        }
        let info = client.tool(3, "server_info", json!({}));
        assert_eq!(
            info["mode"],
            if enabled {
                "queries_camera_frames_and_annotations"
            } else {
                "queries_camera_frames_and_annotation_reads"
            }
        );
        client.finish();
    }
    for (endpoint, read, write) in [
        (None, None, None),
        (Some("http://127.0.0.1:9"), None, Some("write-secret")),
        (Some("http://127.0.0.1:9"), Some("read-secret"), None),
        (Some("http://127.0.0.1:9"), Some("read-secret"), Some("")),
        (
            Some("http://127.0.0.1:9"),
            Some("same-secret"),
            Some("same-secret"),
        ),
    ] {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_item-mcp"));
        command
            .arg("--enable-annotations")
            .env_remove("ITEM_DAEMON_ENDPOINT")
            .env_remove("ITEM_DAEMON_TOKEN")
            .env_remove("ITEM_DAEMON_ANNOTATION_TOKEN");
        if let Some(endpoint) = endpoint {
            command.arg("--daemon-endpoint").arg(endpoint);
        }
        if let Some(token) = read {
            command.env("ITEM_DAEMON_TOKEN", token);
        }
        if let Some(token) = write {
            command.env("ITEM_DAEMON_ANNOTATION_TOKEN", token);
        }
        let output = command.output().unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!stderr.contains("same-secret") && !stderr.contains("write-secret"));
    }
}
#[test]
fn readonly_identity_lookup_does_not_create_or_migrate_old_database() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("old.db");
    let store = item_core::store::Store::open(&db).unwrap();
    drop(store);
    let before = std::fs::read(&db).unwrap();
    let mut client = Client::start_with_annotations(
        true,
        Some(&db),
        None,
        Some(("http://127.0.0.1:9", "read-secret")),
        true,
        Some("write-secret"),
    );
    client.initialize("2025-11-25");
    client.tool_error(2, "list_image_annotations", json!({}), "schema_unavailable");
    client.tool_error(
        3,
        "get_image_annotation",
        json!({"annotation_id":ID}),
        "schema_unavailable",
    );
    client.tool_error(
        4,
        "submit_image_annotation",
        payload(),
        "schema_unavailable",
    );
    client.tool(5, "list_recorded_items", json!({}));
    client.finish();
    assert_eq!(before, std::fs::read(&db).unwrap());
}
