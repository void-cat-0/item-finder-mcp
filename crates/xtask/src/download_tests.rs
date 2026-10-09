use super::*;

use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use tempfile::TempDir;

const CHUNK: u64 = 8;
const DATA_LEN: usize = 29;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Behavior {
    Normal,
    IgnoreRange,
    MalformedRange,
    FailRange,
    FailConnection,
    TruncatedFull,
    Gzip,
    Worker200,
    Worker416,
}

#[derive(Clone, Debug)]
struct RequestRecord {
    #[allow(dead_code)]
    path: String,
    range: Option<(u64, u64)>,
    accept_encoding: Option<String>,
    if_range: Option<String>,
}

struct ServerState {
    data: Vec<u8>,
    behavior: Behavior,
    etag: Option<String>,
    fail_start: u64,
    delay: Duration,
    requests: Vec<RequestRecord>,
    active: usize,
    max_active: usize,
    paths: HashMap<String, String>,
}

struct Fixture {
    addr: SocketAddr,
    state: Arc<Mutex<ServerState>>,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl Fixture {
    fn new(data: Vec<u8>) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind fixture");
        let addr = listener.local_addr().expect("fixture address");
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let state = Arc::new(Mutex::new(ServerState {
            data,
            behavior: Behavior::Normal,
            etag: Some("\"fixture-v1\"".to_owned()),
            fail_start: CHUNK * 2,
            delay: Duration::ZERO,
            requests: Vec::new(),
            active: 0,
            max_active: 0,
            paths: HashMap::new(),
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_state = Arc::clone(&state);
        let thread_stop = Arc::clone(&stop);
        let join = thread::spawn(move || serve(listener, thread_state, thread_stop));
        Self {
            addr,
            state,
            stop,
            join: Some(join),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    fn set_behavior(&self, behavior: Behavior) {
        self.state.lock().unwrap().behavior = behavior;
    }

    fn set_etag(&self, etag: Option<&str>) {
        self.state.lock().unwrap().etag = etag.map(str::to_owned);
    }

    fn set_delay(&self, delay: Duration) {
        self.state.lock().unwrap().delay = delay;
    }

    #[allow(dead_code)]
    fn set_path_etag(&self, path: &str, etag: &str) {
        self.state
            .lock()
            .unwrap()
            .paths
            .insert(path.to_owned(), etag.to_owned());
    }

    fn requests(&self) -> Vec<RequestRecord> {
        self.state.lock().unwrap().requests.clone()
    }

    fn clear_requests(&self) {
        self.state.lock().unwrap().requests.clear();
    }

    fn max_active(&self) -> usize {
        self.state.lock().unwrap().max_active
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = TcpStream::connect(self.addr);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn serve(listener: TcpListener, state: Arc<Mutex<ServerState>>, stop: Arc<AtomicBool>) {
    let mut handlers = Vec::new();
    while !stop.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, _)) => {
                if stop.load(Ordering::Acquire) {
                    break;
                }
                let _ = stream.set_nonblocking(false);
                let thread_state = Arc::clone(&state);
                handlers.push(thread::spawn(move || {
                    handle_connection(stream, thread_state)
                }));
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(2));
            }
            Err(_) => break,
        }
    }
    for handler in handlers {
        let _ = handler.join();
    }
}

fn handle_connection(mut stream: TcpStream, state: Arc<Mutex<ServerState>>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    let Some((path, headers)) = read_request(&mut stream) else {
        return;
    };
    let range = headers
        .get("range")
        .and_then(|value| parse_range_header(value));
    let accept_encoding = headers.get("accept-encoding").cloned();
    let if_range = headers.get("if-range").cloned();

    let (plan, delay, close) = {
        let mut locked = state.lock().unwrap();
        locked.requests.push(RequestRecord {
            path: path.clone(),
            range,
            accept_encoding,
            if_range,
        });
        locked.active += 1;
        locked.max_active = locked.max_active.max(locked.active);
        let etag = locked
            .paths
            .get(&path)
            .cloned()
            .or_else(|| locked.etag.clone());
        let close = locked.behavior == Behavior::FailConnection
            && range.is_some_and(|(start, _)| start == locked.fail_start);
        let plan = response_plan(&locked, range, etag);
        (plan, locked.delay, close)
    };

    let (plan, delay, close) = (plan, delay, close);
    if !delay.is_zero() {
        thread::sleep(delay);
    }
    if close {
        let _ = stream.shutdown(Shutdown::Both);
    } else {
        write_response(&mut stream, &plan);
    }
    let mut locked = state.lock().unwrap();
    locked.active = locked.active.saturating_sub(1);
}

struct ResponsePlan {
    status: u16,
    reason: &'static str,
    body: Vec<u8>,
    declared_length: Option<u64>,
    content_range: Option<String>,
    etag: Option<String>,
    content_encoding: Option<&'static str>,
}

fn response_plan(
    state: &ServerState,
    range: Option<(u64, u64)>,
    etag: Option<String>,
) -> ResponsePlan {
    let len = state.data.len() as u64;
    let full = |body: Vec<u8>, content_encoding: Option<&'static str>| ResponsePlan {
        status: 200,
        reason: "OK",
        body,
        declared_length: None,
        content_range: None,
        etag: etag.clone(),
        content_encoding,
    };

    match state.behavior {
        Behavior::IgnoreRange | Behavior::Worker200 => full(state.data.clone(), None),
        Behavior::TruncatedFull => {
            let body = if state.data.is_empty() {
                Vec::new()
            } else {
                state.data[..state.data.len() - 1].to_vec()
            };
            let mut plan = full(body, None);
            plan.declared_length = Some(len);
            plan
        }
        Behavior::Gzip => full(state.data.clone(), Some("gzip")),
        Behavior::Worker416 => {
            if range.is_some() {
                ResponsePlan {
                    status: 416,
                    reason: "Range Not Satisfiable",
                    body: Vec::new(),
                    declared_length: None,
                    content_range: Some(format!("bytes */{len}")),
                    etag,
                    content_encoding: None,
                }
            } else {
                full(state.data.clone(), None)
            }
        }
        Behavior::FailRange => {
            if range.is_some_and(|(start, _)| start == state.fail_start) {
                ResponsePlan {
                    status: 503,
                    reason: "Service Unavailable",
                    body: b"interrupted".to_vec(),
                    declared_length: None,
                    content_range: None,
                    etag,
                    content_encoding: None,
                }
            } else {
                ranged_response(&state.data, range, etag, false)
            }
        }
        Behavior::MalformedRange => ranged_response(&state.data, range, etag, true),
        Behavior::Normal | Behavior::FailConnection => {
            ranged_response(&state.data, range, etag, false)
        }
    }
}

fn ranged_response(
    data: &[u8],
    range: Option<(u64, u64)>,
    etag: Option<String>,
    malformed: bool,
) -> ResponsePlan {
    let Some((start, requested_end)) = range else {
        return ResponsePlan {
            status: 200,
            reason: "OK",
            body: data.to_vec(),
            declared_length: None,
            content_range: None,
            etag,
            content_encoding: None,
        };
    };
    let len = data.len() as u64;
    if start >= len || requested_end < start {
        return ResponsePlan {
            status: 416,
            reason: "Range Not Satisfiable",
            body: Vec::new(),
            declared_length: None,
            content_range: Some(format!("bytes */{len}")),
            etag,
            content_encoding: None,
        };
    }
    let end = requested_end.min(len - 1);
    let body = data[start as usize..=end as usize].to_vec();
    let content_range = if malformed {
        format!("bytes {}-{}/{}", start, end.saturating_sub(1), len)
    } else {
        format!("bytes {start}-{end}/{len}")
    };
    ResponsePlan {
        status: 206,
        reason: "Partial Content",
        body,
        declared_length: None,
        content_range: Some(content_range),
        etag,
        content_encoding: None,
    }
}

fn write_response(stream: &mut TcpStream, plan: &ResponsePlan) {
    let mut response = format!(
        "HTTP/1.1 {} {}\r\nConnection: close\r\nAccept-Ranges: bytes\r\nContent-Length: {}\r\n",
        plan.status,
        plan.reason,
        plan.declared_length.unwrap_or(plan.body.len() as u64)
    );
    if let Some(content_range) = &plan.content_range {
        response.push_str(&format!("Content-Range: {content_range}\r\n"));
    }
    if let Some(etag) = &plan.etag {
        response.push_str(&format!("ETag: {etag}\r\n"));
    }
    if let Some(encoding) = plan.content_encoding {
        response.push_str(&format!("Content-Encoding: {encoding}\r\n"));
    }
    response.push_str("\r\n");
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.write_all(&plan.body);
}

fn read_request(stream: &mut TcpStream) -> Option<(String, HashMap<String, String>)> {
    let mut bytes = Vec::with_capacity(1024);
    let mut buf = [0u8; 1024];
    loop {
        let count = stream.read(&mut buf).ok()?;
        if count == 0 {
            return None;
        }
        bytes.extend_from_slice(&buf[..count]);
        if bytes.len() > 32 * 1024 {
            return None;
        }
        if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    let text = std::str::from_utf8(&bytes).ok()?;
    let mut lines = text.split("\r\n");
    let request_line = lines.next()?;
    let mut request_parts = request_line.split_whitespace();
    let _method = request_parts.next()?;
    let path = request_parts.next()?.to_owned();
    let mut headers = HashMap::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        let (name, value) = line.split_once(':')?;
        headers.insert(name.to_ascii_lowercase(), value.trim().to_owned());
    }
    Some((path, headers))
}

fn parse_range_header(value: &str) -> Option<(u64, u64)> {
    let value = value.strip_prefix("bytes=")?;
    let (start, end) = value.split_once('-')?;
    Some((start.parse().ok()?, end.parse().ok()?))
}

fn data() -> Vec<u8> {
    (0..DATA_LEN)
        .map(|index| index.wrapping_mul(37).wrapping_add(11) as u8)
        .collect()
}

fn config(workers: usize, chunk_bytes: u64) -> DownloadConfig {
    DownloadConfig {
        workers,
        chunk_bytes,
        force: false,
    }
}

fn force_config(workers: usize, chunk_bytes: u64) -> DownloadConfig {
    DownloadConfig {
        workers,
        chunk_bytes,
        force: true,
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn ranges(records: &[RequestRecord]) -> Vec<(u64, u64)> {
    records.iter().filter_map(|record| record.range).collect()
}

fn full_gets(records: &[RequestRecord]) -> usize {
    records
        .iter()
        .filter(|record| record.range.is_none())
        .count()
}

fn temp_root() -> TempDir {
    tempfile::tempdir().expect("temporary download root")
}

#[test]
fn concurrent_ranges_produce_exact_archive_and_sha() {
    let payload = data();
    let expected_sha = sha256_hex(&payload);
    let fixture = Fixture::new(payload.clone());
    fixture.set_delay(Duration::from_millis(35));
    let root = temp_root();

    let archive = download_archive(
        root.path(),
        "concurrent",
        &[fixture.url("/archive")],
        Some(&expected_sha),
        config(4, CHUNK),
    )
    .expect("download archive");

    assert_eq!(fs::read(&archive).unwrap(), payload);
    assert_eq!(sha256_hex(&fs::read(&archive).unwrap()), expected_sha);
    let records = fixture.requests();
    assert!(ranges(&records).len() >= 3, "records: {records:?}");
    assert!(fixture.max_active() >= 2, "requests were not concurrent");
    assert!(records.iter().all(|record| {
        record.range.is_none() || record.accept_encoding.as_deref() == Some("identity")
    }));
    assert!(records.iter().any(|record| {
        record
            .if_range
            .as_deref()
            .is_some_and(|value| value == "\"fixture-v1\"")
    }));
}

#[test]
fn interrupted_range_retry_reuses_completed_chunks() {
    let fixture = Fixture::new(data());
    fixture.set_behavior(Behavior::FailRange);
    let root = temp_root();

    let first = download_archive(
        root.path(),
        "interrupted",
        &[fixture.url("/archive")],
        None,
        config(4, CHUNK),
    );
    assert!(first.is_err());
    let first_ranges = ranges(&fixture.requests());
    assert!(first_ranges.contains(&(CHUNK * 2, CHUNK * 3 - 1)));

    fixture.set_behavior(Behavior::Normal);
    fixture.clear_requests();
    let archive = download_archive(
        root.path(),
        "interrupted",
        &[fixture.url("/archive")],
        None,
        config(4, CHUNK),
    )
    .expect("retry download");
    assert_eq!(fs::read(archive).unwrap(), data());
    let retry_ranges = ranges(&fixture.requests());
    assert!(retry_ranges.contains(&(CHUNK * 2, CHUNK * 3 - 1)));
    assert!(!retry_ranges.contains(&(CHUNK, CHUNK * 2 - 1)));
}

#[test]
fn changed_etag_does_not_mix_partial_cache() {
    let fixture = Fixture::new(data());
    fixture.set_behavior(Behavior::FailRange);
    let root = temp_root();
    assert!(
        download_archive(
            root.path(),
            "etag-change",
            &[fixture.url("/archive")],
            None,
            config(4, CHUNK),
        )
        .is_err()
    );

    fixture.set_behavior(Behavior::Normal);
    fixture.set_etag(Some("\"fixture-v2\""));
    fixture.clear_requests();
    let archive = download_archive(
        root.path(),
        "etag-change",
        &[fixture.url("/archive")],
        None,
        config(4, CHUNK),
    )
    .expect("download with changed validator");
    assert_eq!(fs::read(archive).unwrap(), data());
    let retry = fixture.requests();
    assert!(ranges(&retry).contains(&(CHUNK, CHUNK * 2 - 1)));
    assert!(retry.iter().any(|record| {
        record
            .if_range
            .as_deref()
            .is_some_and(|value| value == "\"fixture-v2\"")
    }));
}

#[test]
fn no_validator_disallows_cross_run_resume() {
    let fixture = Fixture::new(data());
    fixture.set_etag(None);
    fixture.set_behavior(Behavior::FailRange);
    let root = temp_root();
    assert!(
        download_archive(
            root.path(),
            "no-validator",
            &[fixture.url("/archive")],
            None,
            config(4, CHUNK),
        )
        .is_err()
    );

    fixture.set_behavior(Behavior::Normal);
    fixture.clear_requests();
    let archive = download_archive(
        root.path(),
        "no-validator",
        &[fixture.url("/archive")],
        None,
        config(4, CHUNK),
    )
    .expect("full restart without validator");
    assert_eq!(fs::read(archive).unwrap(), data());
    assert!(ranges(&fixture.requests()).contains(&(CHUNK, CHUNK * 2 - 1)));
}

#[test]
fn ignored_range_response_falls_back_to_full_get() {
    let payload = data();
    let fixture = Fixture::new(payload.clone());
    fixture.set_behavior(Behavior::IgnoreRange);
    let root = temp_root();
    let archive = download_archive(
        root.path(),
        "ignored-range",
        &[fixture.url("/archive")],
        Some(&sha256_hex(&payload)),
        config(3, CHUNK),
    )
    .expect("full fallback");
    assert_eq!(fs::read(archive).unwrap(), payload);
    assert!(full_gets(&fixture.requests()) >= 1);
}

#[test]
fn malformed_range_response_falls_back_to_full_get() {
    let payload = data();
    let fixture = Fixture::new(payload.clone());
    fixture.set_behavior(Behavior::MalformedRange);
    let root = temp_root();
    let archive = download_archive(
        root.path(),
        "malformed-range",
        &[fixture.url("/archive")],
        None,
        config(3, CHUNK),
    )
    .expect("malformed range fallback");
    assert_eq!(fs::read(archive).unwrap(), payload);
    assert!(full_gets(&fixture.requests()) >= 1);
}

#[test]
fn worker_200_and_416_responses_use_safe_full_fallback() {
    for (name, behavior) in [
        ("worker-200", Behavior::Worker200),
        ("worker-416", Behavior::Worker416),
    ] {
        let payload = data();
        let fixture = Fixture::new(payload.clone());
        fixture.set_behavior(behavior);
        let root = temp_root();
        let archive = download_archive(
            root.path(),
            name,
            &[fixture.url("/archive")],
            None,
            config(3, CHUNK),
        )
        .expect("worker fallback");
        assert_eq!(fs::read(archive).unwrap(), payload);
        assert!(full_gets(&fixture.requests()) >= 1);
    }
}

#[test]
fn small_file_shorter_than_probe_is_accepted() {
    let payload = vec![3, 1, 4];
    let fixture = Fixture::new(payload.clone());
    let root = temp_root();
    let archive = download_archive(
        root.path(),
        "short-probe",
        &[fixture.url("/tiny")],
        None,
        config(3, CHUNK),
    )
    .expect("small archive");
    assert_eq!(fs::read(archive).unwrap(), payload);
}

#[test]
fn truncated_full_response_is_rejected() {
    let fixture = Fixture::new(data());
    fixture.set_behavior(Behavior::TruncatedFull);
    let root = temp_root();
    let result = download_archive(
        root.path(),
        "truncated",
        &[fixture.url("/archive")],
        None,
        config(3, CHUNK),
    );
    assert!(result.is_err());
}

#[test]
fn compressed_response_is_rejected() {
    let fixture = Fixture::new(data());
    fixture.set_behavior(Behavior::Gzip);
    let root = temp_root();
    let result = download_archive(
        root.path(),
        "gzip",
        &[fixture.url("/archive")],
        None,
        config(3, CHUNK),
    );
    assert!(result.is_err());
}

#[test]
fn pinned_sha_mismatch_is_cleaned_before_correct_retry() {
    let payload = data();
    let fixture = Fixture::new(payload.clone());
    let root = temp_root();
    let wrong_sha = sha256_hex(b"wrong payload");
    let right_sha = sha256_hex(&payload);
    assert!(
        download_archive(
            root.path(),
            "sha-mismatch",
            &[fixture.url("/archive")],
            Some(&wrong_sha),
            config(3, CHUNK),
        )
        .is_err()
    );

    fixture.clear_requests();
    let archive = download_archive(
        root.path(),
        "sha-mismatch",
        &[fixture.url("/archive")],
        Some(&right_sha),
        config(3, CHUNK),
    )
    .expect("retry after sha mismatch");
    assert_eq!(fs::read(archive).unwrap(), payload);
    assert!(!fixture.requests().is_empty());
}

#[test]
fn cache_reuse_avoids_network_and_force_refreshes() {
    let fixture = Fixture::new(data());
    let root = temp_root();
    let archive = download_archive(
        root.path(),
        "reuse",
        &[fixture.url("/archive")],
        None,
        config(3, CHUNK),
    )
    .expect("initial download");
    assert_eq!(fs::read(&archive).unwrap(), data());

    fixture.clear_requests();
    let reused = download_archive(
        root.path(),
        "reuse",
        &[fixture.url("/archive")],
        None,
        config(3, CHUNK),
    )
    .expect("cache reuse");
    assert_eq!(reused, archive);
    assert!(fixture.requests().is_empty());

    let _ = download_archive(
        root.path(),
        "reuse",
        &[fixture.url("/archive")],
        None,
        force_config(3, CHUNK),
    )
    .expect("forced refresh");
    assert!(!fixture.requests().is_empty());
}

#[test]
fn corrupt_completed_cache_is_redownloaded() {
    let fixture = Fixture::new(data());
    let root = temp_root();
    let archive = download_archive(
        root.path(),
        "corrupt-cache",
        &[fixture.url("/archive")],
        None,
        config(3, CHUNK),
    )
    .expect("initial download");
    fs::write(&archive, b"corrupt").unwrap();
    fixture.clear_requests();

    let repaired = download_archive(
        root.path(),
        "corrupt-cache",
        &[fixture.url("/archive")],
        None,
        config(3, CHUNK),
    )
    .expect("repair corrupted cache");
    assert_eq!(fs::read(repaired).unwrap(), data());
    assert!(!fixture.requests().is_empty());
}

#[test]
fn different_url_invalidates_partial_cache() {
    let fixture = Fixture::new(data());
    fixture.set_behavior(Behavior::FailRange);
    let root = temp_root();
    assert!(
        download_archive(
            root.path(),
            "url-change",
            &[fixture.url("/one")],
            None,
            config(4, CHUNK),
        )
        .is_err()
    );

    fixture.set_behavior(Behavior::Normal);
    fixture.clear_requests();
    let archive = download_archive(
        root.path(),
        "url-change",
        &[fixture.url("/two")],
        None,
        config(4, CHUNK),
    )
    .expect("download from changed url");
    assert_eq!(fs::read(archive).unwrap(), data());
    assert!(ranges(&fixture.requests()).contains(&(CHUNK, CHUNK * 2 - 1)));
}

#[test]
fn different_chunk_size_invalidates_partial_cache() {
    let fixture = Fixture::new(data());
    fixture.set_behavior(Behavior::FailRange);
    let root = temp_root();
    assert!(
        download_archive(
            root.path(),
            "chunk-change",
            &[fixture.url("/archive")],
            None,
            config(4, CHUNK),
        )
        .is_err()
    );

    fixture.set_behavior(Behavior::Normal);
    fixture.clear_requests();
    let archive = download_archive(
        root.path(),
        "chunk-change",
        &[fixture.url("/archive")],
        None,
        config(4, 4),
    )
    .expect("download with changed chunk size");
    assert_eq!(fs::read(archive).unwrap(), data());
    assert!(ranges(&fixture.requests()).contains(&(4, 7)));
}

#[test]
fn invalid_config_is_rejected_and_default_is_valid() {
    let fixture = Fixture::new(data());
    let root = temp_root();
    for invalid in [
        DownloadConfig {
            workers: 0,
            chunk_bytes: CHUNK,
            force: false,
        },
        DownloadConfig {
            workers: 2,
            chunk_bytes: 0,
            force: false,
        },
    ] {
        assert!(
            download_archive(
                root.path(),
                "invalid-config",
                &[fixture.url("/archive")],
                None,
                invalid,
            )
            .is_err()
        );
    }
    let defaults = DownloadConfig::default();
    assert!(defaults.workers > 0);
    assert!(defaults.chunk_bytes > 0);
}
