use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::Duration;

pub(super) const DEFAULT_DOWNLOAD_WORKERS: usize = 4;
pub(super) const DEFAULT_DOWNLOAD_CHUNK_MIB: u64 = 8;
const MAX_DOWNLOAD_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_REDIRECTS: u32 = 10;
const HTTP_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone, Copy)]
pub(super) struct DownloadConfig {
    pub(super) workers: usize,
    pub(super) chunk_bytes: u64,
    pub(super) force: bool,
}

impl Default for DownloadConfig {
    fn default() -> Self {
        Self {
            workers: DEFAULT_DOWNLOAD_WORKERS,
            chunk_bytes: DEFAULT_DOWNLOAD_CHUNK_MIB * 1024 * 1024,
            force: false,
        }
    }
}

impl DownloadConfig {
    pub(super) fn new(workers: usize, chunk_mib: u64, force: bool) -> Result<Self> {
        if !(1..=32).contains(&workers) {
            bail!("download workers must be between 1 and 32");
        }
        if !(1..=128).contains(&chunk_mib) {
            bail!("download chunk size must be between 1 and 128 MiB");
        }
        Ok(Self {
            workers,
            chunk_bytes: chunk_mib * 1024 * 1024,
            force,
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", content = "value")]
pub(super) enum Validator {
    Etag(String),
    LastModified(String),
}

impl Validator {
    fn as_header(&self) -> &str {
        match self {
            Self::Etag(v) | Self::LastModified(v) => v,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct DownloadState {
    url: String,
    expected_sha256: Option<String>,
    total_bytes: u64,
    chunk_bytes: u64,
    validator: Option<Validator>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ChunkReceipt {
    index: u64,
    length: u64,
    sha256: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct ArchiveReceipt {
    url: String,
    expected_sha256: Option<String>,
    length: u64,
    sha256: String,
}

#[derive(Debug, Clone, Copy)]
struct RangeSpec {
    start: u64,
    end: u64,
    total: u64,
}

#[derive(Debug)]
enum RangeFailure {
    Fallback(String),
    Retry(anyhow::Error),
}

impl RangeFailure {
    fn fallback(message: impl Into<String>) -> Self {
        Self::Fallback(message.into())
    }
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(MAX_REDIRECTS)
        .timeout_connect(Some(HTTP_TIMEOUT))
        .timeout_recv_response(Some(HTTP_TIMEOUT))
        .timeout_recv_body(Some(HTTP_TIMEOUT))
        .build()
        .new_agent()
}

fn header_text(resp: &ureq::http::Response<ureq::Body>, name: &str) -> Option<String> {
    resp.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

fn response_validator(resp: &ureq::http::Response<ureq::Body>) -> Option<Validator> {
    if let Some(etag) = header_text(resp, "etag")
        && !etag.trim_start().starts_with("W/")
        && !etag.is_empty()
    {
        return Some(Validator::Etag(etag));
    }
    header_text(resp, "last-modified")
        .filter(|v| !v.is_empty())
        .map(Validator::LastModified)
}

fn identity_response(resp: &ureq::http::Response<ureq::Body>) -> Result<(), RangeFailure> {
    if header_text(resp, "content-encoding").is_some_and(|v| !v.eq_ignore_ascii_case("identity")) {
        return Err(RangeFailure::fallback(
            "server returned a compressed response",
        ));
    }
    Ok(())
}

fn parse_content_range(value: &str) -> Result<RangeSpec> {
    let rest = value
        .strip_prefix("bytes ")
        .context("invalid Content-Range prefix")?;
    let (range, total) = rest
        .split_once('/')
        .context("invalid Content-Range separator")?;
    let (start, end) = range
        .split_once('-')
        .context("invalid Content-Range bounds")?;
    let start = start
        .parse::<u64>()
        .context("invalid Content-Range start")?;
    let end = end.parse::<u64>().context("invalid Content-Range end")?;
    let total = total
        .parse::<u64>()
        .context("invalid Content-Range total")?;
    if start > end || total == 0 || end >= total {
        bail!("invalid Content-Range bytes {start}-{end}/{total}");
    }
    Ok(RangeSpec { start, end, total })
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension(format!(
        "{}tmp",
        path.extension().and_then(|x| x.to_str()).unwrap_or("")
    ));
    fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("publishing {}", path.display()))?;
    Ok(())
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    atomic_write(path, serde_json::to_vec_pretty(value)?.as_slice())
}

fn sha256_hex_digest(digest: sha2::digest::Output<Sha256>) -> String {
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

fn url_key(url: &str) -> String {
    sha256_hex_digest(Sha256::digest(url.as_bytes()))
}

fn read_receipt(path: &Path) -> Option<ChunkReceipt> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

fn chunk_paths(dir: &Path, index: u64) -> (PathBuf, PathBuf) {
    (
        dir.join(format!("chunk-{index:08}.part")),
        dir.join(format!("chunk-{index:08}.bin")),
    )
}

fn receipt_path(dir: &Path, index: u64) -> PathBuf {
    dir.join(format!("chunk-{index:08}.json"))
}

fn expected_chunk_length(total: u64, chunk: u64, index: u64) -> u64 {
    total.min((index + 1) * chunk) - index * chunk
}

fn valid_chunk(dir: &Path, index: u64, expected_len: u64) -> bool {
    let (_, path) = chunk_paths(dir, index);
    let Some(receipt) = read_receipt(&receipt_path(dir, index)) else {
        return false;
    };
    if receipt.index != index || receipt.length != expected_len {
        return false;
    }
    let Ok(meta) = fs::metadata(&path) else {
        return false;
    };
    if meta.len() != expected_len {
        return false;
    }
    let Ok((len, hash)) = hash_file(&path) else {
        return false;
    };
    len == expected_len && hash.eq_ignore_ascii_case(&receipt.sha256)
}

fn hash_file(path: &Path) -> Result<(u64, String)> {
    let mut input = BufReader::new(File::open(path)?);
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    let mut len = 0;
    loop {
        let n = input.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        len += n as u64;
    }
    Ok((len, sha256_hex_digest(hasher.finalize())))
}

fn remove_owned_files(dir: &Path) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    let meta = fs::symlink_metadata(dir)?;
    if !meta.is_dir() {
        bail!(
            "download cache path is not an owned directory: {}",
            dir.display()
        );
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let m = fs::symlink_metadata(&path)?;
        if m.file_type().is_symlink() {
            bail!(
                "refusing to remove symlink in download cache: {}",
                path.display()
            );
        }
        if m.is_dir() {
            remove_owned_files(&path)?;
            fs::remove_dir(&path)?;
        } else {
            fs::remove_file(path)?;
        }
    }
    Ok(())
}

fn clear_partial(dir: &Path) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    remove_owned_files(dir)
}

fn acquire_lock(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(std::fs::TryLockError::WouldBlock) => thread::sleep(Duration::from_millis(100)),
            Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
        }
    }
}

fn validate_archive(
    path: &Path,
    receipt_path: &Path,
    url: &str,
    expected: Option<&str>,
) -> Result<bool> {
    let Ok(receipt) =
        serde_json::from_slice::<ArchiveReceipt>(&fs::read(receipt_path).unwrap_or_default())
    else {
        return Ok(false);
    };
    if receipt.url != url || receipt.expected_sha256.as_deref() != expected {
        return Ok(false);
    }
    let (len, hash) = hash_file(path)?;
    Ok(len == receipt.length
        && hash.eq_ignore_ascii_case(&receipt.sha256)
        && expected.is_none_or(|want| hash.eq_ignore_ascii_case(want)))
}

fn request(
    url: &str,
    start: u64,
    end: u64,
    validator: Option<&Validator>,
) -> Result<ureq::http::Response<ureq::Body>> {
    let range = format!("bytes={start}-{end}");
    let mut req = agent()
        .get(url)
        .header("Range", &range)
        .header("Accept-Encoding", "identity");
    if let Some(v) = validator {
        req = req.header("If-Range", v.as_header());
    }
    req.call().with_context(|| format!("GET {url} ({range})"))
}

fn stream_body(
    resp: &mut ureq::http::Response<ureq::Body>,
    output: &mut File,
    expected: u64,
) -> Result<(u64, String), RangeFailure> {
    identity_response(resp)?;
    if let Some(value) = header_text(resp, "content-length") {
        let length = value
            .parse::<u64>()
            .map_err(|_| RangeFailure::fallback("invalid Content-Length"))?;
        if length != expected {
            return Err(RangeFailure::fallback(format!(
                "Content-Length {length} does not match {expected}"
            )));
        }
    }
    let mut reader = resp.body_mut().as_reader();
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    let mut count = 0u64;
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|e| RangeFailure::Retry(anyhow!(e).context("reading response body")))?;
        if n == 0 {
            break;
        }
        count = count.saturating_add(n as u64);
        if count > expected {
            return Err(RangeFailure::fallback(
                "response body exceeds requested range",
            ));
        }
        output
            .write_all(&buf[..n])
            .map_err(|e| RangeFailure::Retry(anyhow!(e).context("writing download chunk")))?;
        hasher.update(&buf[..n]);
    }
    if count != expected {
        return Err(RangeFailure::fallback(format!(
            "response body truncated: expected {expected}, got {count}"
        )));
    }
    Ok((count, sha256_hex_digest(hasher.finalize())))
}

fn download_chunk(
    url: &str,
    dir: &Path,
    state: &DownloadState,
    index: u64,
) -> std::result::Result<(), RangeFailure> {
    let start = index * state.chunk_bytes;
    let end = state.total_bytes.min(start + state.chunk_bytes) - 1;
    let mut response =
        request(url, start, end, state.validator.as_ref()).map_err(RangeFailure::Retry)?;
    if response.status().as_u16() != 206 {
        return Err(match response.status().as_u16() {
            200 | 416 => {
                RangeFailure::fallback(format!("range request returned HTTP {}", response.status()))
            }
            _ => RangeFailure::Retry(anyhow!("range request returned HTTP {}", response.status())),
        });
    }
    let spec = parse_content_range(
        &header_text(&response, "content-range")
            .ok_or_else(|| RangeFailure::fallback("range response missing Content-Range"))?,
    )
    .map_err(|e| RangeFailure::fallback(e.to_string()))?;
    if spec.start != start || spec.end != end || spec.total != state.total_bytes {
        return Err(RangeFailure::fallback(
            "range response does not match requested chunk",
        ));
    }
    if state.validator.is_some() && response_validator(&response) != state.validator {
        return Err(RangeFailure::fallback(
            "remote validator changed during download",
        ));
    }
    let expected = end - start + 1;
    let (part, final_path) = chunk_paths(dir, index);
    let mut file = File::create(&part).map_err(|e| RangeFailure::Retry(anyhow!(e)))?;
    let (length, sha256) = stream_body(&mut response, &mut file, expected)?;
    file.sync_all()
        .map_err(|e| RangeFailure::Retry(anyhow!(e).context("syncing download chunk")))?;
    drop(file);
    fs::rename(&part, &final_path)
        .map_err(|e| RangeFailure::Retry(anyhow!(e).context("publishing download chunk")))?;
    write_json(
        &receipt_path(dir, index),
        &ChunkReceipt {
            index,
            length,
            sha256,
        },
    )
    .map_err(RangeFailure::Retry)?;
    Ok(())
}

fn probe(url: &str, chunk_bytes: u64, validator: Option<&Validator>) -> Result<Probe> {
    if chunk_bytes == 0 {
        bail!("download chunk size must be greater than zero");
    }
    let end = chunk_bytes - 1;
    let mut response = request(url, 0, end, validator).map_err(|e| anyhow!(e))?;
    if response.status().as_u16() == 200 {
        return Ok(Probe::Full);
    }
    if response.status().as_u16() != 206 {
        return Ok(Probe::Full);
    }
    identity_response(&response).map_err(|e| anyhow!("{e:?}"))?;
    let Ok(spec) =
        parse_content_range(&header_text(&response, "content-range").unwrap_or_default())
    else {
        return Ok(Probe::Full);
    };
    let Some(expected_end) = spec.total.checked_sub(1).map(|last| end.min(last)) else {
        return Ok(Probe::Full);
    };
    if spec.start != 0 || spec.end != expected_end {
        return Ok(Probe::Full);
    }
    let expected = spec.end + 1;
    let content_length =
        header_text(&response, "content-length").and_then(|v| v.parse::<u64>().ok());
    if content_length.is_some_and(|v| v != expected) {
        return Ok(Probe::Full);
    }
    let mut bytes = Vec::with_capacity(expected as usize);
    let mut reader = response.body_mut().as_reader().take(expected + 1);
    reader.read_to_end(&mut bytes)?;
    if bytes.len() as u64 != expected {
        return Ok(Probe::Full);
    }
    Ok(Probe::Range {
        total: spec.total,
        validator: response_validator(&response),
        bytes,
    })
}

#[derive(Debug)]
enum Probe {
    Full,
    Range {
        total: u64,
        validator: Option<Validator>,
        bytes: Vec<u8>,
    },
}

fn full_download(url: &str, dir: &Path, expected: Option<&str>) -> Result<PathBuf> {
    let archive = dir.join("archive.bin");
    let part = dir.join("archive.part");
    let mut response = agent()
        .get(url)
        .header("Accept-Encoding", "identity")
        .call()
        .with_context(|| format!("GET {url}"))?;
    if !response.status().is_success() {
        bail!("GET {url} -> HTTP {}", response.status());
    }
    identity_response(&response).map_err(|e| anyhow!("{e:?}"))?;
    let declared_length = header_text(&response, "content-length")
        .map(|value| value.parse::<u64>().context("invalid Content-Length"))
        .transpose()?;
    let mut file = File::create(&part)?;
    let mut hasher = Sha256::new();
    let mut reader = response.body_mut().as_reader();
    let mut buf = [0u8; 64 * 1024];
    let mut length = 0u64;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        length += n as u64;
        if length > MAX_DOWNLOAD_BYTES {
            drop(file);
            let _ = fs::remove_file(&part);
            bail!("download exceeds {MAX_DOWNLOAD_BYTES} bytes");
        }
        file.write_all(&buf[..n])?;
        hasher.update(&buf[..n]);
    }
    file.sync_all()?;
    drop(file);
    let sha256 = sha256_hex_digest(hasher.finalize());
    if declared_length.is_some_and(|declared| declared != length) {
        let _ = fs::remove_file(&part);
        bail!(
            "full response length mismatch: declared {:?}, received {length}",
            declared_length
        );
    }
    if expected.is_some_and(|want| !sha256.eq_ignore_ascii_case(want)) {
        let _ = fs::remove_file(&part);
        bail!(
            "sha256 mismatch: expected {}, got {sha256}",
            expected.unwrap()
        );
    }
    fs::rename(&part, &archive)?;
    write_json(
        &dir.join("archive.json"),
        &ArchiveReceipt {
            url: url.to_owned(),
            expected_sha256: expected.map(str::to_owned),
            length,
            sha256,
        },
    )?;
    Ok(archive)
}

fn assemble(dir: &Path, state: &DownloadState, expected: Option<&str>) -> Result<PathBuf> {
    let tmp = dir.join("archive.part");
    let archive = dir.join("archive.bin");
    let mut out = BufWriter::new(File::create(&tmp)?);
    let mut hasher = Sha256::new();
    let mut length = 0u64;
    for index in 0..state.total_bytes.div_ceil(state.chunk_bytes) {
        let expected_len = expected_chunk_length(state.total_bytes, state.chunk_bytes, index);
        if !valid_chunk(dir, index, expected_len) {
            bail!("chunk {index} is incomplete or corrupt");
        }
        let (_, path) = chunk_paths(dir, index);
        let mut input = BufReader::new(File::open(path)?);
        let mut buf = [0u8; 64 * 1024];
        loop {
            let n = input.read(&mut buf)?;
            if n == 0 {
                break;
            }
            out.write_all(&buf[..n])?;
            hasher.update(&buf[..n]);
            length += n as u64;
        }
    }
    out.flush()?;
    out.get_ref().sync_all()?;
    let sha256 = sha256_hex_digest(hasher.finalize());
    if length != state.total_bytes {
        let _ = fs::remove_file(&tmp);
        bail!("assembled archive length mismatch");
    }
    if expected.is_some_and(|want| !sha256.eq_ignore_ascii_case(want)) {
        let _ = fs::remove_file(&tmp);
        for index in 0..state.total_bytes.div_ceil(state.chunk_bytes) {
            let (part, final_path) = chunk_paths(dir, index);
            let _ = fs::remove_file(part);
            let _ = fs::remove_file(final_path);
            let _ = fs::remove_file(receipt_path(dir, index));
        }
        bail!(
            "sha256 mismatch: expected {}, got {sha256}",
            expected.unwrap()
        );
    }
    drop(out);
    fs::rename(&tmp, &archive)?;
    write_json(
        &dir.join("archive.json"),
        &ArchiveReceipt {
            url: state.url.clone(),
            expected_sha256: expected.map(str::to_owned),
            length,
            sha256,
        },
    )?;
    Ok(archive)
}

fn download_ranged(
    url: &str,
    dir: &Path,
    expected: Option<&str>,
    config: DownloadConfig,
) -> Result<PathBuf> {
    fs::create_dir_all(dir)?;
    let archive = dir.join("archive.bin");
    if archive.is_file()
        && !config.force
        && validate_archive(&archive, &dir.join("archive.json"), url, expected)?
    {
        println!("  reusing verified archive cache");
        return Ok(archive);
    }
    if archive.exists() {
        fs::remove_file(&archive)?;
        let _ = fs::remove_file(dir.join("archive.json"));
    }

    let state_path = dir.join("state.json");
    let existing = fs::read(&state_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<DownloadState>(&bytes).ok())
        .filter(|s| {
            s.url == url
                && s.expected_sha256.as_deref() == expected
                && s.chunk_bytes == config.chunk_bytes
                && s.total_bytes > 0
                && s.total_bytes <= MAX_DOWNLOAD_BYTES
        });
    let (state, first_probe) = match existing {
        Some(old) if old.validator.is_some() => {
            match probe(url, config.chunk_bytes, old.validator.as_ref()) {
                Ok(Probe::Range {
                    total,
                    validator,
                    bytes,
                }) if total == old.total_bytes && validator == old.validator => (old, Some(bytes)),
                Ok(Probe::Full) => return full_download(url, dir, expected),
                Ok(Probe::Range {
                    total,
                    validator,
                    bytes,
                }) => {
                    clear_partial(dir)?;
                    if total > MAX_DOWNLOAD_BYTES {
                        bail!("download exceeds {MAX_DOWNLOAD_BYTES} bytes");
                    }
                    let state = DownloadState {
                        url: url.to_owned(),
                        expected_sha256: expected.map(str::to_owned),
                        total_bytes: total,
                        chunk_bytes: config.chunk_bytes,
                        validator,
                    };
                    (state, Some(bytes))
                }
                Err(_) => return full_download(url, dir, expected),
            }
        }
        Some(_) => {
            clear_partial(dir)?;
            match probe(url, config.chunk_bytes, None)? {
                Probe::Full => return full_download(url, dir, expected),
                Probe::Range {
                    total,
                    validator,
                    bytes,
                } => {
                    let state = DownloadState {
                        url: url.to_owned(),
                        expected_sha256: expected.map(str::to_owned),
                        total_bytes: total,
                        chunk_bytes: config.chunk_bytes,
                        validator,
                    };
                    (state, Some(bytes))
                }
            }
        }
        None => {
            clear_partial(dir)?;
            match probe(url, config.chunk_bytes, None)? {
                Probe::Full => return full_download(url, dir, expected),
                Probe::Range {
                    total,
                    validator,
                    bytes,
                } => {
                    if total > MAX_DOWNLOAD_BYTES {
                        bail!("download exceeds {MAX_DOWNLOAD_BYTES} bytes");
                    }
                    let state = DownloadState {
                        url: url.to_owned(),
                        expected_sha256: expected.map(str::to_owned),
                        total_bytes: total,
                        chunk_bytes: config.chunk_bytes,
                        validator,
                    };
                    (state, Some(bytes))
                }
            }
        }
    };
    if config.workers == 0 || config.workers > 32 {
        bail!("download workers must be between 1 and 32");
    }
    if config.chunk_bytes == 0 || config.chunk_bytes > 128 * 1024 * 1024 {
        bail!("download chunk size must be between 1 and 128 MiB");
    }
    if state.total_bytes > MAX_DOWNLOAD_BYTES {
        bail!("download exceeds {MAX_DOWNLOAD_BYTES} bytes");
    }
    write_json(&state_path, &state)?;
    if let Some(bytes) = first_probe {
        let (part, final_path) = chunk_paths(dir, 0);
        let mut file = File::create(&part)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(part, final_path)?;
        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        write_json(
            &receipt_path(dir, 0),
            &ChunkReceipt {
                index: 0,
                length: bytes.len() as u64,
                sha256: sha256_hex_digest(hasher.finalize()),
            },
        )?;
    }

    let chunks = state.total_bytes.div_ceil(state.chunk_bytes);
    let queue = Arc::new(Mutex::new(VecDeque::new()));
    let mut reused = 0u64;
    for index in 0..chunks {
        if valid_chunk(
            dir,
            index,
            expected_chunk_length(state.total_bytes, state.chunk_bytes, index),
        ) {
            reused += 1;
        } else {
            queue.lock().unwrap().push_back(index);
        }
    }
    let pending = chunks - reused;
    println!(
        "  ranged download: {chunks} chunks, {reused} reused, {pending} workers (up to {})",
        config.workers
    );
    let queue = Arc::clone(&queue);
    let state = Arc::new(state);
    let failed = Arc::new(AtomicBool::new(false));
    let first_error = Arc::new(Mutex::new(None::<String>));
    let fallback = Arc::new(AtomicBool::new(false));
    let mut handles = Vec::new();
    for _ in 0..config.workers.min(chunks.max(1) as usize) {
        let queue = Arc::clone(&queue);
        let state = Arc::clone(&state);
        let dir = dir.to_path_buf();
        let url = url.to_owned();
        let failed = Arc::clone(&failed);
        let first_error = Arc::clone(&first_error);
        let fallback = Arc::clone(&fallback);
        handles.push(thread::spawn(move || {
            loop {
                if failed.load(Ordering::Acquire) {
                    break;
                }
                let Some(index) = queue.lock().unwrap().pop_front() else {
                    break;
                };
                match download_chunk(&url, &dir, &state, index) {
                    Ok(()) => {}
                    Err(RangeFailure::Fallback(message)) => {
                        fallback.store(true, Ordering::Release);
                        failed.store(true, Ordering::Release);
                        *first_error.lock().unwrap() = Some(message);
                    }
                    Err(RangeFailure::Retry(error)) => {
                        failed.store(true, Ordering::Release);
                        *first_error.lock().unwrap() = Some(error.to_string());
                    }
                }
            }
        }));
    }
    for handle in handles {
        handle
            .join()
            .map_err(|_| anyhow!("download worker panicked"))?;
    }
    if fallback.load(Ordering::Acquire) {
        clear_partial(dir)?;
        return full_download(url, dir, expected);
    }
    if let Some(error) = first_error.lock().unwrap().clone() {
        bail!("range download failed: {error}");
    }
    assemble(dir, &state, expected)
}

pub(super) fn download_archive(
    root: &Path,
    key: &str,
    urls: &[String],
    expected: Option<&str>,
    config: DownloadConfig,
) -> Result<PathBuf> {
    if key.is_empty() || key == "." || key == ".." || key.contains('/') || key.contains('\\') {
        bail!("invalid download cache key");
    }
    if urls.is_empty() {
        bail!("no download URL available");
    }
    let base = root.join(".downloads");
    fs::create_dir_all(&base)?;
    let key_dir = base.join(key);
    if key_dir.exists() && !fs::symlink_metadata(&key_dir)?.is_dir() {
        bail!("download cache key is not an owned directory");
    }
    let lock = acquire_lock(&base.join(format!("{key}.lock")))?;
    let _lock = lock;
    if config.force && key_dir.exists() {
        remove_owned_files(&key_dir)?;
        fs::remove_dir(&key_dir)?;
    }
    fs::create_dir_all(&key_dir)?;
    let mut last_error = None;
    for url in urls {
        let dir = key_dir.join(url_key(url));
        match download_ranged(url, &dir, expected, config) {
            Ok(path) => return Ok(path),
            Err(error) => {
                println!("  fetch candidate failed: {error:#}");
                last_error = Some(error);
            }
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow!("no download URL available")))
}

#[cfg(test)]
#[path = "download_tests.rs"]
mod tests;
