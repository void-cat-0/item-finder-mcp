//! The resident daemon: one process running the Frigate webhook server, the
//! optional RTSP preview and N camera threads (design/resident-ingest.md §3).
//!
//! Shutdown follows §3 exactly, because the order is the whole point:
//!
//! 1. stop accepting new requests (`with_graceful_shutdown`, and the stop flag
//!    is set first so the cameras stop pulling frames immediately);
//! 2. let the camera threads leave at a `step()` boundary, with a 10s budget --
//!    a wedged decoder must not hold the process hostage;
//! 3. write the final health snapshot so `updated_at` is the last word, then
//!    `PRAGMA wal_checkpoint(TRUNCATE)`;
//! 4. exit 0.
//!
//! Platform note: the stop sources differ. Windows has no `SIGTERM` at all, so
//! the console event (Ctrl-C) is the path there, while systemd/launchd send
//! `SIGTERM` on Unix. `tokio::signal::unix` does not exist off Unix, hence the
//! `#[cfg(unix)]` split -- see [`wait_for_signal`].

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use chrono::Utc;
use item_core::store::Store;

use crate::SharedStore;
use crate::config::{Config, RuntimeConfig};
use crate::health::{CameraMeta, HealthRegistry, HealthWriter};
use crate::lock::SingleInstance;
use crate::retention;
use crate::runtime::{self, CameraTask};
use crate::supervisor::Supervisor;

/// How long the camera threads get to leave before the daemon gives up (§3).
pub const SHUTDOWN_BUDGET: Duration = Duration::from_secs(10);

/// How often the health file is refreshed (§5).
pub const HEALTH_INTERVAL: Duration = Duration::from_secs(15);

/// How often a one-line "still alive" summary is logged (§5). Silence must
/// never be indistinguishable from success.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(300);

/// Resolution of the health/heartbeat/maintenance timers.
const TICK: Duration = Duration::from_millis(200);

/// Run the daemon until a stop signal arrives. Wires the stop flag to the
/// process signals; this is the entry point `--daemon` uses.
pub fn run(settings: RuntimeConfig, file: &Config) -> anyhow::Result<()> {
    let tasks = file.camera_tasks(&settings);
    run_with_stop(
        settings,
        tasks,
        Some(file),
        Arc::new(AtomicBool::new(false)),
    )
}

/// Same as [`run`], but the caller owns the stop flag. Tests flip it directly
/// instead of poking the process's signal handlers.
pub fn run_with_stop(
    settings: RuntimeConfig,
    tasks: Vec<CameraTask>,
    seed: Option<&Config>,
    stop: Arc<AtomicBool>,
) -> anyhow::Result<()> {
    settings.validate(seed).map_err(anyhow::Error::msg)?;
    reject_unsupported_sources(&tasks)?;
    // Validate and reserve both listeners before database, lock, health or camera
    // side effects. Disabled listeners are never bound.
    let bind = |address: &str| -> anyhow::Result<std::net::TcpListener> {
        let listener = std::net::TcpListener::bind(address).context("binding daemon listener")?;
        listener.set_nonblocking(true)?;
        Ok(listener)
    };
    let webhook_listener = settings
        .webhook_enabled
        .then(|| bind(&settings.listen))
        .transpose()?;
    let internal_listener = settings.internal_listen.as_deref().map(bind).transpose()?;

    if let Some(dir) = std::path::Path::new(&settings.db).parent()
        && !dir.as_os_str().is_empty()
    {
        std::fs::create_dir_all(dir).context("creating the database directory")?;
    }

    // Before anything opens the database: two writers on one WAL file is the
    // failure that corrupts data quietly (§1).
    //
    // This is the recoverable form (§8): a service manager restarting a killed
    // daemon must not be refused forever by the wreckage the kill left behind.
    // The health file's `updated_at` (§5) is read first and passed as evidence,
    // so a daemon that is alive but between lock heartbeats keeps its lock.
    let health_written_at = read_health_updated_at(&settings.health_file);
    let lock = SingleInstance::acquire_or_recover(&settings.lock_path(), health_written_at)
        .context("refusing to start a second ingest daemon")?;
    tracing::info!(
        lock = %lock.path().display(),
        pid = std::process::id(),
        recovered = lock.recovered().is_some(),
        "single-instance lock acquired"
    );
    if let Some(recovered) = lock.recovered() {
        // §8's "previous run died without shutdown", now an observed fact
        // rather than a guess: the lock stopped beating and no health file
        // contradicted it. The lock module already logged the previous holder.
        tracing::warn!(
            reason = recovered.reason(),
            "previous run died without shutdown"
        );
    }

    let store: SharedStore = Arc::new(Mutex::new(
        Store::open(&settings.db).context("opening sqlite store")?,
    ));
    // Annotation tables are opt-in data owned by the daemon, but their
    // migration/reconciliation must happen before any listener accepts writes,
    // including when annotation writes are disabled.
    {
        let guard = store.lock().expect("store mutex poisoned");
        guard
            .ensure_annotation_schema()
            .context("initializing annotation schema")?;
    }

    if let Some(file) = seed {
        let guard = store.lock().expect("store mutex poisoned");
        let seeded = file
            .seed_regions(&guard)
            .context("seeding camera regions")?;
        tracing::info!(
            cameras = file.camera.len(),
            regions = seeded,
            "config regions seeded"
        );
    }

    let registry = HealthRegistry::new();
    let camera_metas: Vec<CameraMeta> = tasks
        .iter()
        .map(|task| CameraMeta {
            id: task.camera_id.clone(),
            source: task
                .source
                .as_ref()
                .map(|s| s.describe())
                .unwrap_or_else(|| "webhook".to_string()),
        })
        .collect();
    let writer = Arc::new(Mutex::new(HealthWriter::new(
        &settings.health_file,
        &settings.detector,
        camera_metas.clone(),
    )));
    write_health(&writer, &registry);

    let asset_store = Some(
        crate::assets::AssetStore::new_with_ttls(
            &settings.snapshots_dir,
            settings.internal_max_asset_bytes,
            settings.internal_token.clone(),
            Arc::clone(&store),
            Duration::from_secs(settings.frame_asset_ttl_secs),
            Duration::from_millis(settings.frame_cache_ttl_ms),
        )
        .context("opening raw asset store")?
        .with_camera_status(
            if let Some(config) = seed {
                config
                    .camera
                    .iter()
                    .map(|camera| crate::assets::CameraRegistration {
                        camera_id: camera.id.clone(),
                        enabled: camera.enabled.unwrap_or(true),
                        source_kind: camera
                            .url
                            .as_deref()
                            .filter(|url| !url.is_empty())
                            .map(|_| "rtsp")
                            .or_else(|| camera.webcam.map(|_| "webcam"))
                            .unwrap_or("webhook")
                            .into(),
                    })
                    .collect()
            } else {
                tasks
                    .iter()
                    .map(|task| crate::assets::CameraRegistration {
                        camera_id: task.camera_id.clone(),
                        enabled: task.enabled,
                        source_kind: task
                            .source
                            .as_ref()
                            .map(|s| s.kind())
                            .unwrap_or("webhook")
                            .into(),
                    })
                    .collect()
            },
            registry.clone(),
            stop.clone(),
        ),
    );
    let annotations = crate::annotations::AnnotationRuntime::new(
        asset_store
            .as_ref()
            .expect("asset store initialized")
            .clone(),
        &settings,
    )
    .context("opening annotation runtime")?;
    let mut tasks = tasks;
    for task in &mut tasks {
        task.raw_assets = asset_store.clone();
        if let Some(assets) = &task.raw_assets
            && let Some(cache) = assets.frame_cache()
        {
            cache
                .register(&task.camera_id)
                .map_err(anyhow::Error::msg)?;
        }
    }

    let mut supervisor = Supervisor::new(Arc::clone(&stop)).with_health(registry.clone());
    supervisor.start(&store, tasks);
    tracing::info!("camera threads started");

    // The lock heartbeat (§8): a running holder keeps its file's mtime moving
    // so the next start can tell a live daemon from wreckage. It runs on its
    // own plain thread and shares only the path -- the `SingleInstance` guard
    // itself is owned here and released on drop.
    let lock_heartbeat_thread = {
        let stop = Arc::clone(&stop);
        let lock_path = lock.path().to_path_buf();
        std::thread::Builder::new()
            .name("lock-heartbeat".to_string())
            .spawn(move || {
                let mut until_beat = crate::lock::HEARTBEAT;
                while !stop.load(Ordering::SeqCst) {
                    std::thread::sleep(TICK);
                    if stop.load(Ordering::SeqCst) {
                        return;
                    }
                    until_beat = until_beat.saturating_sub(TICK);
                    if until_beat.is_zero() {
                        crate::lock::touch(&lock_path);
                        until_beat = crate::lock::HEARTBEAT;
                    }
                }
            })
            .context("spawning the lock heartbeat thread")?
    };

    // The health cadence runs on a plain thread so it keeps ticking while the
    // async side is parked on a signal.
    let health_thread = {
        let writer = Arc::clone(&writer);
        let registry = registry.clone();
        let stop = Arc::clone(&stop);
        std::thread::Builder::new()
            .name("health".to_string())
            .spawn(move || {
                let mut until_write = HEALTH_INTERVAL;
                let mut until_heartbeat = HEARTBEAT_INTERVAL;
                while !stop.load(Ordering::SeqCst) {
                    std::thread::sleep(TICK);
                    if stop.load(Ordering::SeqCst) {
                        return;
                    }
                    until_write = until_write.saturating_sub(TICK);
                    until_heartbeat = until_heartbeat.saturating_sub(TICK);
                    if until_write.is_zero() {
                        write_health(&writer, &registry);
                        until_write = HEALTH_INTERVAL;
                    }
                    if until_heartbeat.is_zero() {
                        log_heartbeat(&registry, &camera_metas);
                        until_heartbeat = HEARTBEAT_INTERVAL;
                    }
                }
            })
            .context("spawning the health writer thread")?
    };

    let app = build_router(&store, &settings);

    // Bounded growth (§7): WAL checkpoints, the retention sweep and the orphan
    // scan, all off when their interval is 0.
    let maintenance_thread = spawn_maintenance(
        Arc::clone(&store),
        &settings,
        Arc::clone(&stop),
        asset_store.clone(),
        Some(annotations.clone()),
    )
    .context("spawning the maintenance thread")?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let serve_result = runtime.block_on(async {
        let signal_stop = stop.clone();
        let signal_task = tokio::spawn(async move {
            wait_for_stop(signal_stop.clone()).await;
            signal_stop.store(true, Ordering::SeqCst);
        });
        let mut servers = tokio::task::JoinSet::new();
        if let Some(listener) = webhook_listener {
            let listener = tokio::net::TcpListener::from_std(listener)?;
            let addr = listener.local_addr()?;
            tracing::info!(%addr, "frigate webhook server listening");
            let stop = stop.clone();
            servers.spawn(async move {
                axum::serve(listener, app)
                    .with_graceful_shutdown(wait_for_flag(stop))
                    .await
            });
        }
        if let (Some(listener), Some(assets)) = (internal_listener, asset_store) {
            let listener = tokio::net::TcpListener::from_std(listener)?;
            let addr = listener.local_addr()?;
            let app = crate::assets::router(assets, addr)
                .merge(crate::annotations::router(annotations.clone(), addr));
            tracing::info!(%addr, "private raw asset server listening");
            let stop = stop.clone();
            servers.spawn(async move {
                axum::serve(listener, app)
                    .with_graceful_shutdown(wait_for_flag(stop))
                    .await
            });
        }
        let mut failure = None;
        if servers.is_empty() {
            wait_for_flag(stop.clone()).await;
        }
        while let Some(result) = servers.join_next().await {
            match result {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    failure = Some(anyhow::Error::from(e));
                }
                Err(e) => {
                    failure = Some(anyhow::Error::from(e));
                }
            }
            stop.store(true, Ordering::SeqCst);
        }
        signal_task.abort();
        failure.map_or(Ok(()), Err)
    });

    // From here on: nobody new is connecting, so drain the cameras.
    stop.store(true, Ordering::SeqCst);
    tracing::info!(
        budget_s = SHUTDOWN_BUDGET.as_secs(),
        "stopping: waiting for camera threads to reach a step boundary"
    );
    supervisor.request_stop();
    let outcomes = supervisor.join(Some(SHUTDOWN_BUDGET));
    for outcome in &outcomes {
        tracing::info!(
            camera = %outcome.camera_id,
            frames = outcome.frames,
            detections = outcome.detections,
            recorded = outcome.recorded,
            reconnects = outcome.reconnects,
            finished = outcome.finished,
            error = ?outcome.error,
            "camera stopped"
        );
    }

    let _ = health_thread.join();
    let _ = lock_heartbeat_thread.join();
    let _ = maintenance_thread.join();
    // One last snapshot: after this write, `updated_at` is the process's final
    // word, which is what P1's acceptance checks.
    write_health(&writer, &registry);
    {
        let store = store.lock().expect("store mutex poisoned");
        match store.checkpoint() {
            Ok(()) => tracing::info!("wal checkpointed"),
            Err(e) => tracing::warn!(error = %e, "wal checkpoint failed"),
        }
    }

    drop(lock);
    tracing::info!("ingest daemon stopped");
    serve_result
}

/// One periodic job. A zero interval means the job is off -- which is why the
/// countdown cannot simply be compared against zero: an off job would then fire
/// on every tick.
#[derive(Debug, Clone, Copy)]
struct Cadence {
    /// Zero = off.
    interval: Duration,
    until: Duration,
}

impl Cadence {
    /// First run one interval from now.
    fn new(interval: Duration) -> Self {
        Self {
            interval,
            until: interval,
        }
    }

    /// First run on the next tick -- for work that should happen at startup
    /// (a daemon that was down for a month prunes as soon as it comes back).
    fn due_now(interval: Duration) -> Self {
        Self {
            interval,
            until: Duration::ZERO,
        }
    }

    fn off() -> Self {
        Self {
            interval: Duration::ZERO,
            until: Duration::ZERO,
        }
    }

    /// Advance by `elapsed`; `true` when the job is due now.
    fn tick(&mut self, elapsed: Duration) -> bool {
        if self.interval.is_zero() {
            return false;
        }
        self.until = self.until.saturating_sub(elapsed);
        if self.until.is_zero() {
            self.until = self.interval;
            true
        } else {
            false
        }
    }
}

/// The health file's `updated_at`, as the lock takeover's evidence (§5/§8).
/// `None` when the file is missing or unreadable -- which must not veto a
/// takeover, or a first-ever start would refuse itself.
fn read_health_updated_at(path: &str) -> Option<chrono::DateTime<Utc>> {
    let text = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let parsed = chrono::DateTime::parse_from_rfc3339(value.get("updated_at")?.as_str()?).ok()?;
    Some(parsed.with_timezone(&Utc))
}

/// Checkpoints, pruning and the orphan scan on their own thread, so they keep
/// running while the async side is parked on a signal and never sit in the
/// frame path of a camera.
///
/// Every failure here is logged and survived: a locked database or an
/// undeletable file must not take the daemon down (§7).
fn spawn_maintenance(
    store: SharedStore,
    settings: &RuntimeConfig,
    stop: Arc<AtomicBool>,
    asset_store: Option<crate::assets::AssetStore>,
    annotations: Option<crate::annotations::AnnotationRuntime>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    let window = settings.retention_window();
    let snapshots_dir = PathBuf::from(&settings.snapshots_dir);
    let mut checkpoint = Cadence::new(Duration::from_secs(settings.checkpoint_secs));
    let mut sweep = match window {
        Some(_) => Cadence::due_now(Duration::from_secs(settings.sweep_secs)),
        None => Cadence::off(),
    };
    let mut reconcile = Cadence::new(Duration::from_secs(settings.reconcile_secs));
    let mut frames = Cadence::new(Duration::from_secs(1));
    let mut annotations_cadence = Cadence::due_now(Duration::from_secs(60));
    tracing::info!(
        checkpoint_secs = settings.checkpoint_secs,
        sweep_secs = settings.sweep_secs,
        reconcile_secs = settings.reconcile_secs,
        retention_days = settings.retention_days,
        snapshots_dir = %snapshots_dir.display(),
        "maintenance scheduled"
    );

    std::thread::Builder::new()
        .name("maintenance".to_string())
        .spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                std::thread::sleep(TICK);
                if stop.load(Ordering::SeqCst) {
                    return;
                }
                if let Some(runtime) = &annotations
                    && annotations_cadence.tick(TICK)
                    && let Err(error) = runtime.maintenance()
                {
                    tracing::warn!(error = ?error, "annotation cleanup deferred");
                }
                if frames.tick(TICK)
                    && let Some(assets) = &asset_store
                    && let Err(error) = assets.purge_expired_camera_assets()
                {
                    tracing::warn!(%error, "camera asset cleanup deferred");
                }
                if checkpoint.tick(TICK) {
                    checkpoint_wal(&store);
                }
                if sweep.tick(TICK)
                    && let Some(window) = window
                {
                    prune_once(&store, &snapshots_dir, window);
                }
                if reconcile.tick(TICK) {
                    reconcile_once(&store, &snapshots_dir);
                }
            }
        })
}

fn checkpoint_wal(store: &SharedStore) {
    let store = store.lock().expect("store mutex poisoned");
    match store.checkpoint() {
        Ok(()) => tracing::debug!("wal checkpointed"),
        Err(e) => tracing::warn!(error = %e, "wal checkpoint failed"),
    }
}

/// One retention pass. An empty pass logs at debug: at the default cadence it
/// is the common case, and the point of the log line is the pass that deleted
/// something.
fn prune_once(store: &SharedStore, snapshots_dir: &Path, window: Duration) {
    match retention::prune(store, snapshots_dir, window, Utc::now()) {
        Ok(report) if report.rows_deleted == 0 && report.files_deleted == 0 => {
            tracing::debug!(
                kept = report.files_kept,
                "retention sweep: nothing to prune"
            )
        }
        Ok(report) => tracing::info!(
            rows = report.rows_deleted,
            files = report.files_deleted,
            kib = report.bytes_freed / 1024,
            kept = report.files_kept,
            "retention sweep"
        ),
        Err(e) => tracing::warn!(error = %e, "retention sweep failed"),
    }
}

fn reconcile_once(store: &SharedStore, snapshots_dir: &Path) {
    match retention::reconcile(store, snapshots_dir) {
        Ok(report) if report.files_deleted == 0 => {
            tracing::debug!(kept = report.files_kept, "orphan scan: no orphans")
        }
        Ok(report) => tracing::info!(
            files = report.files_deleted,
            kib = report.bytes_freed / 1024,
            kept = report.files_kept,
            "orphan scan"
        ),
        Err(e) => tracing::warn!(error = %e, "orphan scan failed"),
    }
}

/// Refuse to start when a camera needs a source this build cannot open. That is
/// a configuration error, not a transient one, and retrying it forever would
/// only hide it (see `runtime::source_supported`).
fn reject_unsupported_sources(tasks: &[CameraTask]) -> anyhow::Result<()> {
    for task in tasks {
        if let Some(spec) = task.source.as_ref()
            && !runtime::source_supported(spec)
        {
            anyhow::bail!(
                "camera '{}' needs a {} source, which this build cannot open: \
                 rebuild with --features {}",
                task.camera_id,
                spec.kind(),
                spec.cargo_feature()
            );
        }
    }
    Ok(())
}

fn build_router(store: &SharedStore, settings: &RuntimeConfig) -> axum::Router {
    let state: crate::frigate::State = Arc::clone(store);
    let app = crate::frigate::router(state);

    // Shadow (not mut): with `rtsp` off there is nothing to merge.
    #[cfg(feature = "rtsp")]
    let app = match settings.preview_url.clone() {
        Some(url) => {
            // Credentials are part of the url; log only scheme+host.
            tracing::info!(
                target_url = %runtime::redact_url(&url),
                "web preview enabled at GET /preview"
            );
            app.merge(crate::preview::router(crate::preview::spawn_streamer(url)))
        }
        None => app,
    };

    // The preview is the only thing that reads the settings here.
    #[cfg(not(feature = "rtsp"))]
    let _ = settings;

    app
}

/// One "still alive" line per camera (§5). Without it, "ran six hours and
/// recorded nothing" and "working normally" look identical in the log.
fn log_heartbeat(registry: &HealthRegistry, cameras: &[CameraMeta]) {
    for meta in cameras {
        match registry.get(&meta.id) {
            Some(h) => tracing::info!(
                camera = %meta.id,
                state = h.state.as_str(),
                frames = h.frames,
                detections = h.detections,
                recorded = h.recorded,
                reconnects = h.reconnects,
                last_frame_age_s = ?h.last_frame_age.map(|age| age.as_secs()),
                "heartbeat"
            ),
            None => tracing::info!(camera = %meta.id, "heartbeat: no report yet"),
        }
    }
}

fn write_health(writer: &Mutex<HealthWriter>, registry: &HealthRegistry) {
    match writer.lock() {
        Ok(mut writer) => {
            // P2 makes this atomic and retries on Windows share violations; for
            // now a failure must not take the daemon down.
            if let Err(e) = writer.write(registry) {
                tracing::warn!(error = %e, path = %writer.path().display(), "health file write failed");
            }
        }
        Err(_) => tracing::warn!("health writer mutex poisoned"),
    }
}

/// Resolves when the process is asked to stop.
///
/// The stop sources differ by platform, and §8 requires each service manager's
/// own signal to work:
///
/// - Unix (systemd / launchd): `SIGTERM` and `SIGINT`, plus `SIGHUP` -- a
///   terminal hangup or a `launchctl kickstart -k` should also drain cleanly
///   rather than drop the process.
/// - Windows: the console control events. `Ctrl-C` is the interactive one;
///   `Ctrl-Break` is what many service wrappers and `GenerateConsoleCtrlEvent`
///   send; `Ctrl-Close`/`Ctrl-Logoff`/`Ctrl-Shutdown` are what a console window
///   closing, a user logging off, or the machine shutting down deliver. All of
///   them mean "stop now, while you still can", and all of them are available
///   through `tokio::signal::windows` -- no raw `SetConsoleCtrlHandler` needed.
async fn wait_for_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        match (
            signal(SignalKind::terminate()),
            signal(SignalKind::interrupt()),
            signal(SignalKind::hangup()),
        ) {
            (Ok(mut term), Ok(mut int), Ok(mut hup)) => {
                tokio::select! {
                    _ = term.recv() => tracing::info!("stop signal: SIGTERM"),
                    _ = int.recv() => tracing::info!("stop signal: SIGINT"),
                    _ = hup.recv() => tracing::info!("stop signal: SIGHUP"),
                }
            }
            _ => {
                // One source failing must not cost us the others; Ctrl-C is the
                // one every Unix console has.
                tracing::warn!(
                    "could not install every signal handler; falling back to Ctrl-C only"
                );
                let _ = tokio::signal::ctrl_c().await;
                tracing::info!("stop signal: Ctrl-C");
            }
        }
    }
    #[cfg(not(unix))]
    {
        use tokio::signal::windows;

        // Each listener is independent: one failing to install degrades to the
        // remaining sources rather than aborting startup.
        let mut ctrl_c = windows::ctrl_c().ok();
        let mut ctrl_break = windows::ctrl_break().ok();
        let mut ctrl_close = windows::ctrl_close().ok();
        let mut ctrl_logoff = windows::ctrl_logoff().ok();
        let mut ctrl_shutdown = windows::ctrl_shutdown().ok();

        // Await one optional listener, or never resolve when it is absent.
        macro_rules! stopped {
            ($listener:ident) => {
                async {
                    match $listener.as_mut() {
                        Some(l) => {
                            l.recv().await;
                        }
                        None => std::future::pending::<()>().await,
                    }
                }
            };
        }

        tokio::select! {
            _ = stopped!(ctrl_c) => tracing::info!("stop signal: Ctrl-C"),
            _ = stopped!(ctrl_break) => tracing::info!("stop signal: Ctrl-Break"),
            _ = stopped!(ctrl_close) => tracing::info!("stop signal: console close"),
            _ = stopped!(ctrl_logoff) => tracing::info!("stop signal: logoff"),
            _ = stopped!(ctrl_shutdown) => tracing::info!("stop signal: system shutdown"),
        }
    }
}

/// Either a process signal or someone flipping the flag (tests, and the
/// `--daemon` path when the flag is set by another task).
async fn wait_for_flag(stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn wait_for_stop(stop: Arc<AtomicBool>) {
    tokio::select! {
        _ = wait_for_signal() => {}
        _ = async move {
            while !stop.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        } => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CliOverrides, EnvOverrides};
    use crate::runtime::{DetectorSpec, SourceSpec};

    fn mock_task(id: &str) -> CameraTask {
        CameraTask {
            camera_id: id.into(),
            source: Some(SourceSpec::Mock {
                frames: u32::MAX,
                width: 8,
                height: 8,
            }),
            detector: DetectorSpec::Null,
            depth: crate::runtime::DepthSpec::None,
            detect_fps: 1.0,
            snapshot_dir: std::path::PathBuf::from("unused"),
            snapshots: crate::config::SnapshotMode::First,
            raw_assets: None,
            events_path: None,
            max_frames: 0,
            enabled: true,
        }
    }

    struct Sandbox(std::path::PathBuf);

    impl Sandbox {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "item-ingest-daemon-{tag}-{}-{}",
                std::process::id(),
                chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self, name: &str) -> std::path::PathBuf {
            self.0.join(name)
        }

        /// Settings pointing entirely inside the sandbox, with the web server
        /// off so the test needs no port.
        fn settings(&self) -> RuntimeConfig {
            let mut settings =
                RuntimeConfig::resolve(None, &CliOverrides::default(), &EnvOverrides::default())
                    .unwrap();
            settings.db = self.path("items.db").display().to_string();
            settings.snapshots_dir = self.path("snapshots").display().to_string();
            settings.health_file = self.path("health.json").display().to_string();
            settings.events_file = self.path("events.jsonl").display().to_string();
            settings.webhook_enabled = false;
            settings
        }
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn wait_for_file(path: &std::path::Path, within: Duration) -> bool {
        let deadline = std::time::Instant::now() + within;
        while std::time::Instant::now() < deadline {
            if path.exists() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    fn wait_until(within: Duration, mut check: impl FnMut() -> bool) -> bool {
        let deadline = std::time::Instant::now() + within;
        while std::time::Instant::now() < deadline {
            if check() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        check()
    }

    /// Count rows the way `item-web` reads them: a second, read-only connection
    /// while the daemon writes.
    fn count_rows(db: &std::path::Path) -> i64 {
        Store::open_read_only(db)
            .and_then(|store| store.count_observations())
            .unwrap_or(-1)
    }

    fn unused_address() -> String {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .to_string()
    }

    #[test]
    fn private_listener_validation_precedes_all_side_effects() {
        let sandbox = Sandbox::new("invalid-private");
        let mut settings = sandbox.settings();
        settings.internal_listen = Some("0.0.0.0:8480".into());
        settings.internal_token = Some("do-not-log".into());
        let result = run_with_stop(
            settings.clone(),
            vec![],
            None,
            Arc::new(AtomicBool::new(false)),
        );
        assert!(result.unwrap_err().to_string().contains("loopback"));
        assert!(!sandbox.path("items.db").exists());
        assert!(!sandbox.path("health.json").exists());
        assert!(!sandbox.path("ingest.lock").exists());
        settings.internal_listen = Some(unused_address());
        settings.internal_token = None;
        assert!(
            run_with_stop(
                settings.clone(),
                vec![],
                None,
                Arc::new(AtomicBool::new(false))
            )
            .is_err()
        );
        assert!(!sandbox.path("items.db").exists());
        assert!(!format!("{settings:?}").contains("do-not-log"));
        let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        settings.internal_listen = Some(held.local_addr().unwrap().to_string());
        settings.internal_token = Some("token".into());
        assert!(run_with_stop(settings, vec![], None, Arc::new(AtomicBool::new(false))).is_err());
        assert!(!sandbox.path("items.db").exists());
    }

    #[test]
    fn private_http_is_separate_concurrent_restartable_and_stops_with_webhook() {
        let sandbox = Sandbox::new("real-http");
        let db = Arc::new(Mutex::new(Store::open(sandbox.path("items.db")).unwrap()));
        let now = Utc::now();
        let owner = {
            let guard = db.lock().unwrap();
            let (id, _, _) = guard
                .record_sighting(
                    "cam",
                    "frame",
                    "cup",
                    now,
                    None,
                    item_core::store::DEFAULT_DEDUP_WINDOW,
                )
                .unwrap();
            guard.observation_by_id(id).unwrap().unwrap()
        };
        let raw = crate::assets::AssetStore::new(
            sandbox.path("snapshots"),
            crate::assets::DEFAULT_MAX_ASSET_BYTES,
            Some("http-secret".into()),
            db.clone(),
        )
        .unwrap();
        let metadata = raw.publish_rgb(&owner, now, &[45; 18], 3, 2).unwrap();
        // Only the durable manifest survives; the daemon receives no registry.
        drop(raw);
        let mut settings = sandbox.settings();
        settings.retention_days = 0;
        settings.webhook_enabled = true;
        settings.listen = unused_address();
        settings.internal_listen = Some(unused_address());
        settings.internal_token = Some("http-secret".into());
        let private = format!("http://{}", settings.internal_listen.as_ref().unwrap());
        let webhook = format!("http://{}", settings.listen);
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        for _ in 0..2 {
            let stop = Arc::new(AtomicBool::new(false));
            let worker = {
                let (settings, stop) = (settings.clone(), stop.clone());
                std::thread::spawn(move || run_with_stop(settings, vec![], None, stop))
            };
            assert!(wait_until(Duration::from_secs(10), || client
                .get(format!(
                    "{private}/internal/v1/assets/{}",
                    metadata.asset_id
                ))
                .bearer_auth("http-secret")
                .send()
                .is_ok_and(|r| r.status().is_success())));
            let response = client
                .get(format!(
                    "{private}/internal/v1/observations/{}/raw-asset",
                    owner.id
                ))
                .bearer_auth("http-secret")
                .send()
                .unwrap();
            assert_eq!(
                response
                    .json::<item_core::assets::RawAssetMetadata>()
                    .unwrap(),
                metadata
            );
            let response = client
                .get(format!(
                    "{private}/internal/v1/assets/{}/content",
                    metadata.asset_id
                ))
                .bearer_auth("http-secret")
                .send()
                .unwrap();
            assert_eq!(response.headers()["content-type"], "image/jpeg");
            assert_eq!(response.headers()["x-asset-id"], metadata.asset_id);
            assert_eq!(response.headers()["x-asset-sha256"], metadata.sha256);
            assert_eq!(
                crate::assets::hex_digest(&response.bytes().unwrap()),
                metadata.sha256
            );
            assert_eq!(
                client
                    .get(format!(
                        "{webhook}/internal/v1/assets/{}",
                        metadata.asset_id
                    ))
                    .bearer_auth("http-secret")
                    .send()
                    .unwrap()
                    .status(),
                reqwest::StatusCode::NOT_FOUND
            );
            // A live webhook and private listener share no routes or auth state.
            assert!(client.get(format!("{webhook}/")).send().is_ok());
            assert_eq!(
                client
                    .get(format!(
                        "{private}/internal/v1/assets/{}",
                        metadata.asset_id
                    ))
                    .send()
                    .unwrap()
                    .status(),
                reqwest::StatusCode::UNAUTHORIZED
            );
            for (header, value) in [
                ("host", "attacker.example"),
                ("origin", "http://attacker.example"),
            ] {
                assert_eq!(
                    client
                        .get(format!(
                            "{private}/internal/v1/assets/{}",
                            metadata.asset_id
                        ))
                        .header(header, value)
                        .bearer_auth("http-secret")
                        .send()
                        .unwrap()
                        .status(),
                    reqwest::StatusCode::FORBIDDEN
                );
            }
            let unavailable = client
                .get(format!("{private}/internal/v1/observations/999/raw-asset"))
                .bearer_auth("http-secret")
                .send()
                .unwrap();
            assert_eq!(unavailable.status(), reqwest::StatusCode::NOT_FOUND);
            assert_eq!(
                unavailable
                    .json::<item_core::assets::AssetErrorBody>()
                    .unwrap()
                    .error
                    .code,
                "not_found"
            );
            stop.store(true, Ordering::SeqCst);
            worker.join().unwrap().unwrap();
            assert!(client.get(format!("{private}/")).send().is_err());
            assert!(client.get(format!("{webhook}/")).send().is_err());
            assert!(!sandbox.path("ingest.lock").exists());
        }
    }

    #[test]
    fn a_cadence_that_is_off_never_fires() {
        let mut off = Cadence::off();
        for _ in 0..100 {
            assert!(
                !off.tick(TICK),
                "a zero interval must not mean 'every tick'"
            );
        }
    }

    #[test]
    fn a_cadence_fires_on_its_interval_and_not_before() {
        let mut cadence = Cadence::new(Duration::from_secs(1));
        assert!(!cadence.tick(Duration::from_millis(999)));
        assert!(cadence.tick(Duration::from_millis(1)));
        assert!(!cadence.tick(Duration::from_millis(999)));
        assert!(cadence.tick(Duration::from_millis(1)));
    }

    #[test]
    fn a_cadence_can_be_due_immediately() {
        let mut cadence = Cadence::due_now(Duration::from_secs(3600));
        assert!(cadence.tick(TICK), "startup work runs on the first tick");
        assert!(!cadence.tick(TICK));
        assert!(!cadence.tick(Duration::from_secs(3599)));
        assert!(cadence.tick(Duration::from_secs(1)));
    }

    /// P3's core promise, end to end: a daemon that comes back to a database
    /// holding rows older than its window prunes them -- and their snapshots --
    /// without being asked.
    #[test]
    fn the_daemon_prunes_what_is_past_the_window_as_soon_as_it_starts() {
        let sandbox = Sandbox::new("prune");
        let snapshots = sandbox.path("snapshots");
        std::fs::create_dir_all(&snapshots).unwrap();

        let stale_file = snapshots.join("1.jpg");
        {
            let store = Store::open(sandbox.path("items.db")).unwrap();
            let (id, _, _) = store
                .record_sighting(
                    "cam",
                    "desk",
                    "keys",
                    Utc::now() - chrono::Duration::days(2),
                    Some(&stale_file.display().to_string()),
                    item_core::store::DEFAULT_DEDUP_WINDOW,
                )
                .unwrap();
            assert_eq!(id, 1, "the snapshot file name comes from the row id");
            std::fs::write(&stale_file, b"jpeg-ish").unwrap();
        }

        let mut settings = sandbox.settings();
        settings.retention_days = 1;
        let stop = Arc::new(AtomicBool::new(false));
        let handle = {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || run_with_stop(settings, vec![mock_task("cam")], None, stop))
        };

        assert!(
            wait_until(Duration::from_secs(10), || count_rows(
                &sandbox.path("items.db")
            ) == 0),
            "the expired row must be gone"
        );
        // prune() deletes the row while holding the store lock and unlinks the
        // file only after releasing it, so the file lags the row by design:
        // waiting for the row is not proof the file is gone yet.
        assert!(
            wait_until(Duration::from_secs(10), || !stale_file.exists()),
            "and its snapshot with it"
        );

        stop.store(true, Ordering::SeqCst);
        handle
            .join()
            .expect("daemon thread")
            .expect("clean shutdown");
    }

    #[test]
    fn unsupported_sources_are_refused_before_the_daemon_starts() {
        let mut task = mock_task("cam");
        task.source = Some(SourceSpec::Webcam { index: 0 });
        match reject_unsupported_sources(&[task]) {
            Err(e) => assert!(
                e.to_string().contains("rebuild with --features camera"),
                "{e}"
            ),
            // Only a build that can actually open a webcam may accept this. The
            // constant is the point: reaching this arm at all means the build
            // has the feature. (A `const` block, clippy's suggestion, would be
            // evaluated while compiling and break the featureless build.)
            #[allow(clippy::assertions_on_constants)]
            Ok(()) => assert!(cfg!(feature = "camera"), "no camera feature, yet accepted"),
        }
    }

    #[test]
    fn a_daemon_run_stops_on_request_and_leaves_a_final_health_file() {
        let sandbox = Sandbox::new("stop");
        let health_path = sandbox.path("health.json");
        let lock_path = sandbox.path("ingest.lock");

        let stop = Arc::new(AtomicBool::new(false));
        let handle = {
            let settings = sandbox.settings();
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || run_with_stop(settings, vec![mock_task("cam")], None, stop))
        };

        assert!(
            wait_for_file(&health_path, Duration::from_secs(10)),
            "the daemon writes an initial health file"
        );
        assert!(lock_path.exists(), "the lock is held while running");

        let before: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&health_path).unwrap()).unwrap();

        stop.store(true, Ordering::SeqCst);
        let result = handle.join().expect("daemon thread must not panic");
        assert!(result.is_ok(), "clean shutdown returns Ok: {result:?}");

        assert!(!lock_path.exists(), "a clean stop releases the lock");
        let after: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&health_path).unwrap()).unwrap();
        assert!(
            after["updated_at"].as_str().unwrap() >= before["updated_at"].as_str().unwrap(),
            "the final snapshot must be the last word"
        );
        assert_eq!(after["cameras"][0]["state"], "stopped");
    }

    #[test]
    fn a_second_daemon_is_refused_while_the_first_holds_the_lock() {
        let sandbox = Sandbox::new("lock");
        let health_path = sandbox.path("health.json");

        let stop = Arc::new(AtomicBool::new(false));
        let handle = {
            let settings = sandbox.settings();
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || run_with_stop(settings, vec![mock_task("cam")], None, stop))
        };
        assert!(wait_for_file(&health_path, Duration::from_secs(10)));

        let second = run_with_stop(
            sandbox.settings(),
            vec![mock_task("cam")],
            None,
            Arc::new(AtomicBool::new(false)),
        );
        let message = format!("{:#}", second.expect_err("a second daemon must be refused"));
        assert!(message.contains("already running"), "{message}");
        assert!(
            message.contains(&std::process::id().to_string()),
            "the loser must be told which pid holds the lock: {message}"
        );

        stop.store(true, Ordering::SeqCst);
        let _ = handle.join();
    }

    /// §8's restart case, end to end at the daemon level: a killed daemon left
    /// a silent lock file, and the next start takes it over -- with a stale
    /// health file agreeing -- instead of being refused forever.
    #[test]
    fn a_daemon_takes_over_a_lock_left_by_a_killed_one() {
        let sandbox = Sandbox::new("takeover");
        let lock_path = sandbox.path("ingest.lock");
        let health_path = sandbox.path("health.json");

        // Wreckage: a lock file with both its heartbeat and its health file
        // long past §5's 90s rule.
        let mut settings = sandbox.settings();
        {
            let killed = SingleInstance::acquire(&lock_path).unwrap();
            std::mem::forget(killed); // a hard kill runs no Drop
        }
        // Backdate: the lock's mtime and the health stamp must both be old.
        let old = std::time::SystemTime::now() - Duration::from_secs(3600);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&lock_path)
            .unwrap()
            .set_modified(old)
            .unwrap();
        std::fs::write(
            &health_path,
            serde_json::json!({
                "schema": 1,
                "updated_at": (Utc::now() - chrono::Duration::seconds(3600)).to_rfc3339(),
            })
            .to_string(),
        )
        .unwrap();

        // A fresh run must start, and must replace the lock with its own.
        settings.webhook_enabled = false;
        let stop = Arc::new(AtomicBool::new(false));
        let handle = {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || run_with_stop(settings, vec![mock_task("cam")], None, stop))
        };

        assert!(
            wait_until(Duration::from_secs(10), || {
                std::fs::read_to_string(&lock_path)
                    .map(|body| body.contains(&format!("pid={}", std::process::id())))
                    .unwrap_or(false)
            }),
            "the new daemon must hold the lock it took over"
        );

        stop.store(true, Ordering::SeqCst);
        let result = handle.join().expect("daemon thread must not panic");
        assert!(
            result.is_ok(),
            "the takeover run shuts down cleanly: {result:?}"
        );
        assert!(!lock_path.exists(), "and releases the lock on the way out");
    }

    /// The opposite case: a fresh health stamp outvotes a silent lock, so a
    /// wedged-but-live daemon is not robbed of its lock by an impatient
    /// restart.
    #[test]
    fn a_daemon_does_not_steal_a_lock_whose_daemon_still_looks_alive() {
        let sandbox = Sandbox::new("noveto");
        let lock_path = sandbox.path("ingest.lock");
        let health_path = sandbox.path("health.json");

        {
            let wedged = SingleInstance::acquire(&lock_path).unwrap();
            std::mem::forget(wedged);
        }
        let old = std::time::SystemTime::now() - Duration::from_secs(3600);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&lock_path)
            .unwrap()
            .set_modified(old)
            .unwrap();
        // The health file was written moments ago: the daemon is alive, merely
        // between lock heartbeats.
        std::fs::write(
            &health_path,
            serde_json::json!({
                "schema": 1,
                "updated_at": Utc::now().to_rfc3339(),
            })
            .to_string(),
        )
        .unwrap();

        let err = run_with_stop(
            sandbox.settings(),
            vec![mock_task("cam")],
            None,
            Arc::new(AtomicBool::new(false)),
        )
        .expect_err("a live-looking daemon keeps its lock");
        let message = format!("{err:#}");
        assert!(message.contains("already running"), "{message}");
    }
}
