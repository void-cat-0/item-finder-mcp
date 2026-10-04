use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::Duration;

use item_mcp::images::{DaemonClient, ImagesRequest};
use rmcp::model::CallToolResult;
use serde_json::json;

fn request() -> ImagesRequest {
    serde_json::from_value(json!({"image_kind":"raw","asset_ids":["raw-test"]})).unwrap()
}

fn error_code(error: CallToolResult) -> String {
    error.structured_content.unwrap()["error"]["code"]
        .as_str()
        .unwrap()
        .into()
}

fn stub(response: String, delay: Duration) -> (String, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let worker = std::thread::spawn(move || {
        let (mut connection, _) = listener.accept().unwrap();
        connection
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut request = Vec::new();
        let mut chunk = [0; 4096];
        while !request.windows(4).any(|value| value == b"\r\n\r\n") {
            let count = connection.read(&mut chunk).unwrap();
            assert!(count > 0, "HTTP request closed before headers");
            request.extend_from_slice(&chunk[..count]);
        }
        std::thread::sleep(delay);
        let _ = connection.write_all(response.as_bytes());
    });
    (endpoint, worker)
}

#[tokio::test]
async fn metadata_is_bounded_even_without_content_length() {
    let body = "x".repeat(20 * 1024);
    let response = format!("HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n{body}");
    let (endpoint, worker) = stub(response, Duration::ZERO);
    let client = DaemonClient::new(&endpoint, "token", Duration::from_secs(2)).unwrap();
    assert_eq!(
        error_code(
            client
                .images(request(), "test-db".into())
                .await
                .unwrap_err()
        ),
        "payload_too_large"
    );
    worker.join().unwrap();
}

#[tokio::test]
async fn image_requests_have_a_complete_deadline() {
    let (endpoint, worker) = stub(String::new(), Duration::from_millis(150));
    let client = DaemonClient::new(&endpoint, "token", Duration::from_millis(30)).unwrap();
    assert_eq!(
        error_code(
            client
                .images(request(), "test-db".into())
                .await
                .unwrap_err()
        ),
        "daemon_unavailable"
    );
    worker.join().unwrap();
}

#[tokio::test]
async fn redirects_are_not_followed() {
    let (endpoint, worker) = stub("HTTP/1.1 302 Found\r\nLocation: http://192.0.2.1/private\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(), Duration::ZERO);
    let client = DaemonClient::new(&endpoint, "token", Duration::from_secs(2)).unwrap();
    assert_eq!(
        error_code(
            client
                .images(request(), "test-db".into())
                .await
                .unwrap_err()
        ),
        "invalid_daemon_response"
    );
    worker.join().unwrap();
}
