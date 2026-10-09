//! P3's acceptance criterion, as a test (design/resident-ingest.md §10):
//!
//! > 造 1 万行 + 1 万张图，跑清理后行数与文件数同时下降且无"指向已删文件的
//! > sample_snapshot"；Windows 下遇到被占用文件不中断清理
//!
//! So: ten thousand observations with ten thousand snapshot files, a sweep, and
//! then the invariant that matters -- no surviving row may reference a file the
//! sweep deleted. One file is held open with no sharing on Windows, which is
//! the case §7 says must be counted and survived rather than propagated.
//!
//! This is deliberately an integration test: it goes through the same public
//! entry points the daemon's maintenance thread and `--maintenance` use, on a
//! real file-backed database.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use item_core::store::{DEFAULT_DEDUP_WINDOW, Store};
use item_ingest::SharedStore;
use item_ingest::retention::{prune, snapshot_file};

/// The scale the design document asks for.
const EXPIRED: i64 = 10_000;
/// Rows inside the window: the sweep must leave these -- and their files --
/// exactly as they were.
const LIVE: i64 = 100;
const WINDOW_DAYS: u64 = 90;

struct Sandbox(PathBuf);

impl Sandbox {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "item-ingest-p3-acceptance-{}-{}",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn db(&self) -> PathBuf {
        self.0.join("items.db")
    }

    fn snapshots(&self) -> PathBuf {
        self.0.join("snapshots")
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Write `{id}.jpg` for every id, as `write_snapshot` does.
fn seed_snapshots(dir: &Path, ids: &[i64]) -> Vec<PathBuf> {
    std::fs::create_dir_all(dir).unwrap();
    ids.iter()
        .map(|id| {
            let path = snapshot_file(dir, *id);
            std::fs::write(&path, b"jpeg-ish bytes").unwrap();
            path
        })
        .collect()
}

/// Record `count` sightings `age_days` old, one per label, and return their ids
/// with the snapshot path each row stores.
fn seed_rows(store: &Store, count: i64, age_days: i64, tag: &str) -> Vec<i64> {
    let seen_at: DateTime<Utc> = Utc::now() - chrono::Duration::days(age_days);
    let mut ids = Vec::with_capacity(count as usize);
    for index in 0..count {
        let label = format!("{tag}-{index}");
        let (id, is_new, _) = store
            .record_sighting(
                "cam",
                "desk",
                &label,
                seen_at,
                Some(&format!("snapshots/{label}.jpg")),
                DEFAULT_DEDUP_WINDOW,
            )
            .unwrap();
        assert!(is_new, "every label is distinct, so every row is new");
        ids.push(id);
    }
    ids
}

/// How many snapshot files are in the directory right now.
fn count_files(dir: &Path) -> usize {
    match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
            .count(),
        Err(_) => 0,
    }
}

/// Ignored by default: at the document's scale the seeding alone is ~7s (ten
/// thousand single-row commits, each one an fsync) and the sweep another ~2s,
/// which is a 20x slowdown of a test suite that otherwise runs in half a
/// second. Run it when touching retention:
///
/// ```text
/// cargo test -p item-ingest --test retention_acceptance -- --ignored --nocapture
/// ```
#[test]
#[ignore = "10k rows + 10k files: run explicitly when touching retention (§10)"]
fn ten_thousand_rows_and_files_are_pruned_without_leaving_dangling_references() {
    let sandbox = Sandbox::new();
    let started = Instant::now();

    let store: SharedStore = Arc::new(Mutex::new(Store::open(sandbox.db()).unwrap()));
    let (expired_ids, live_ids) = {
        let store = store.lock().unwrap();
        let expired = seed_rows(&store, EXPIRED, WINDOW_DAYS as i64 + 10, "stale");
        let live = seed_rows(&store, LIVE, 1, "fresh");
        (expired, live)
    };
    assert_eq!(
        store.lock().unwrap().count_observations().unwrap(),
        EXPIRED + LIVE
    );
    let mut all_files = seed_snapshots(&sandbox.snapshots(), &expired_ids);
    all_files.extend(seed_snapshots(&sandbox.snapshots(), &live_ids));
    assert_eq!(count_files(&sandbox.snapshots()), (EXPIRED + LIVE) as usize);

    let seeded = started.elapsed();

    // Windows: hold one expiring file with no sharing at all, so the unlink
    // fails for real. The sweep must count it and carry on (§7/§10).
    #[cfg(windows)]
    let held = {
        use std::os::windows::fs::OpenOptionsExt;
        let path = snapshot_file(&sandbox.snapshots(), expired_ids[0]);
        Some(
            std::fs::OpenOptions::new()
                .read(true)
                .share_mode(0)
                .open(&path)
                .expect("opening with no sharing"),
        )
    };

    let window = Duration::from_secs(WINDOW_DAYS * 24 * 3600);
    let report = prune(&store, &sandbox.snapshots(), window, Utc::now()).unwrap();
    let swept = started.elapsed();

    // Rows and files both go down, by exactly the number that expired.
    assert_eq!(report.rows_deleted, EXPIRED as u64);
    #[cfg(not(windows))]
    assert_eq!(report.files_deleted, EXPIRED as u64);
    #[cfg(windows)]
    {
        assert_eq!(
            report.files_deleted,
            EXPIRED as u64 - 1,
            "the held file is the only one left behind"
        );
        assert_eq!(report.files_kept, 1, "counted, not fatal");
    }

    let survivors = store.lock().unwrap().observation_ids().unwrap();
    assert_eq!(survivors.len(), LIVE as usize);
    assert_eq!(survivors, live_ids);

    // §10's invariant: every surviving row still has its snapshot on disk.
    for id in &survivors {
        assert!(
            snapshot_file(&sandbox.snapshots(), *id).is_file(),
            "observation {id} survived but its snapshot did not"
        );
    }
    #[cfg(windows)]
    let expected_files = LIVE as usize + 1; // the live ones, plus the held orphan
    #[cfg(not(windows))]
    let expected_files = LIVE as usize;
    assert_eq!(count_files(&sandbox.snapshots()), expected_files);

    #[cfg(windows)]
    drop(held);

    // The orphans the sweep could not take are what the reconcile pass is for.
    let orphans = item_ingest::retention::reconcile(&store, &sandbox.snapshots()).unwrap();
    #[cfg(windows)]
    {
        assert_eq!(orphans.files_deleted, 1, "the held file, now free");
        assert_eq!(count_files(&sandbox.snapshots()), LIVE as usize);
    }
    #[cfg(not(windows))]
    assert_eq!(orphans.files_deleted, 0, "nothing was left behind");

    let remaining = store.lock().unwrap().count_observations().unwrap();
    assert_eq!(remaining, LIVE);

    println!(
        "seeded {EXPIRED}+{LIVE} rows and files in {:?}, swept in {:?}, {remaining} rows remain",
        seeded,
        swept - seeded
    );
}
