//! Bounded growth: the retention window, snapshot cleanup and orphan
//! reconciliation (design/resident-ingest.md §7).
//!
//! Three invariants, in the order they matter:
//!
//! 1. **Rows before files.** A snapshot is unlinked only after the row that
//!    referenced it is committed as gone. A JPEG nobody references is an
//!    orphan; an observation whose snapshot file vanished is a broken record
//!    (`item-web` shows it as a card with no image).
//! 2. **One undeletable file is not a failed sweep.** Windows refuses to
//!    delete a file another process holds open, so every unlink is retried and
//!    then counted and skipped -- never propagated. The next pass tries again.
//! 3. **No heavy work under the store lock** (§3). Each pass reads what it
//!    needs from SQLite in one query, drops the lock, and only then walks the
//!    filesystem.
//!
//! The contract this module relies on: the snapshot of observation `id` is the
//! file `<snapshots_dir>/<id>.jpg` (`crate::write_snapshot`). The path stored
//! *in the row* is what readers resolve, and it may be relative (single-camera
//! CLI runs) or a `frigate://` reference with no local file at all -- so it is
//! never used as a deletion key. Files under a previous `snapshots_dir` are
//! therefore out of reach: only the configured directory is ever swept.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use chrono::{DateTime, Utc};

use crate::SharedStore;

/// How many times an unlink is retried before the file is left to the next
/// pass. Mirrors the health file's rename policy (§5/§7): on Windows a file
/// another process holds open cannot be deleted, and a reader must never be
/// able to break cleanup.
const DELETE_ATTEMPTS: u32 = 3;

/// Base delay between those retries; the nth retry waits `n * this`.
const DELETE_BACKOFF: Duration = Duration::from_millis(50);

/// What one pass did, as counts -- enough for the daemon to log a pass in one
/// line and for `--maintenance` to print a summary.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SweepReport {
    /// Observations removed for falling out of the retention window.
    pub rows_deleted: u64,
    /// Snapshot files removed (of expired rows, or orphans with no row left).
    pub files_deleted: u64,
    /// Bytes those files occupied, as last measured before the unlink.
    pub bytes_freed: u64,
    /// Files still there when the pass ended: held open by another process, or
    /// not a plain file. Never fatal; the next pass retries.
    pub files_kept: u64,
}

/// The snapshot file an observation owns.
pub fn snapshot_file(snapshots_dir: &Path, id: i64) -> PathBuf {
    snapshots_dir.join(format!("{id}.jpg"))
}

/// Drop observations last seen before `now - window`, together with the
/// snapshots they owned.
///
/// The rows go in one statement under the lock; the unlinks happen after it is
/// released. A row whose file cannot be deleted is still deleted: keeping the
/// row would preserve a reference to a file that may already be gone, which is
/// the worse of the two failures.
pub fn prune(
    store: &SharedStore,
    snapshots_dir: &Path,
    window: Duration,
    now: DateTime<Utc>,
) -> anyhow::Result<SweepReport> {
    let cutoff =
        now - chrono::Duration::from_std(window).context("retention window out of range")?;
    let expired = {
        let store = store.lock().expect("store mutex poisoned");
        store.expire_observations(cutoff)?
    };

    let mut report = SweepReport {
        rows_deleted: expired.len() as u64,
        ..Default::default()
    };
    remove_snapshots(snapshots_dir, expired, &mut report);
    remove_expired_raw(store, snapshots_dir, &mut report)?;
    Ok(report)
}

/// Delete snapshot files whose observation row is gone (the periodic pass).
///
/// Guard: a file is removed only when its id is **inside the range the database
/// has issued** (`<= max(id)`) and no longer in it. Anything beyond the maximum
/// id was not written by this program, so a `snapshots_dir` pointed at a folder
/// of unrelated pictures (`~/Pictures/2024.jpg`) cannot be emptied by accident.
/// The same guard means an *empty* observations table deletes nothing: an empty
/// database is far more likely to mean "wrong path" than "everything expired",
/// and the safe direction is to keep files.
pub fn reconcile(store: &SharedStore, snapshots_dir: &Path) -> anyhow::Result<SweepReport> {
    let mut report = SweepReport::default();
    remove_expired_raw(store, snapshots_dir, &mut report)?;
    let ids = {
        let store = store.lock().expect("store mutex poisoned");
        store.observation_ids()?
    };
    let Some(max_id) = ids.last().copied() else {
        tracing::info!(
            dir = %snapshots_dir.display(),
            "orphan scan skipped: no observations to compare against"
        );
        return Ok(report);
    };
    let live: HashSet<i64> = ids.into_iter().collect();

    let entries = match std::fs::read_dir(snapshots_dir) {
        Ok(entries) => entries,
        // Nothing has been written yet: an empty directory and a missing one
        // mean the same thing here.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(SweepReport::default()),
        Err(e) => {
            return Err(e).with_context(|| format!("scanning {}", snapshots_dir.display()));
        }
    };

    let mut orphans = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("scanning {}", snapshots_dir.display()))?;
        let Some(id) = orphan_candidate(&entry.file_name()) else {
            continue;
        };
        if id > max_id || live.contains(&id) {
            continue;
        }
        orphans.push(id);
    }
    tracing::debug!(orphans = orphans.len(), "orphan scan");
    remove_snapshots(snapshots_dir, orphans, &mut report);
    Ok(report)
}

fn remove_expired_raw(
    store: &SharedStore,
    root: &Path,
    report: &mut SweepReport,
) -> anyhow::Result<()> {
    let _commit = crate::assets::RAW_LIFECYCLE
        .lock()
        .map_err(|_| anyhow::anyhow!("raw lifecycle lock poisoned"))?;
    for path in crate::assets::expired_raw_files(store, root)? {
        match unlink(&path) {
            Unlinked::Gone(bytes) => {
                report.files_deleted += 1;
                report.bytes_freed += bytes;
            }
            Unlinked::Missing => {}
            Unlinked::Kept(error) => {
                report.files_kept += 1;
                tracing::warn!(error = %error, "raw file cleanup deferred");
            }
        }
    }
    Ok(())
}

/// Unlink one snapshot per id, recording what happened.
fn remove_snapshots(snapshots_dir: &Path, ids: Vec<i64>, report: &mut SweepReport) {
    for id in ids {
        let path = snapshot_file(snapshots_dir, id);
        match unlink(&path) {
            Unlinked::Gone(bytes) => {
                report.files_deleted += 1;
                report.bytes_freed += bytes;
            }
            Unlinked::Missing => {}
            Unlinked::Kept(error) => {
                report.files_kept += 1;
                tracing::warn!(
                    path = %path.display(),
                    error = %error,
                    "snapshot could not be deleted; leaving it for the next pass"
                );
            }
        }
    }
}

/// `<id>.jpg` or `<id>-<suffix>.jpg` -> the observation id it belongs to.
/// Snapshot modes produce: `12.jpg` (first), `12-ev34.jpg` (events), `12-g56.jpg` (detections).
/// Anything else in the directory (other extensions, non-numeric prefix, unreadable names)
/// is not ours and is left alone.
fn orphan_candidate(name: &std::ffi::OsStr) -> Option<i64> {
    let s = name.to_str()?.strip_suffix(".jpg")?;
    // Try to parse the whole name as a number first (backward compat with "12.jpg")
    if let Ok(id) = s.parse::<i64>() {
        return (id > 0).then_some(id);
    }
    // Otherwise try to parse the prefix before the first dash ("12-ev34" -> "12")
    let prefix = s.split('-').next()?;
    prefix.parse::<i64>().ok().filter(|id| *id > 0)
}

/// What happened to one file.
enum Unlinked {
    /// Gone; it held this many bytes.
    Gone(u64),
    /// There was nothing to delete. Normal: an observation fed by Frigate has
    /// no local snapshot, and a row with no pixels keeps `sample_snapshot`
    /// NULL.
    Missing,
    /// Still there: another process holds it open (the Windows case) or it is
    /// not a plain file.
    Kept(std::io::Error),
}

/// Delete one file, retrying a few times, and report the size it had.
fn unlink(path: &Path) -> Unlinked {
    let size = match std::fs::metadata(path) {
        Ok(meta) if meta.is_file() => meta.len(),
        // Not a plain file: the unlink below will fail and be reported. Reading
        // its "size" would be meaningless either way.
        Ok(_) => 0,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Unlinked::Missing,
        Err(e) => return Unlinked::Kept(e),
    };

    let mut last = None;
    for attempt in 1..=DELETE_ATTEMPTS {
        match std::fs::remove_file(path) {
            Ok(()) => return Unlinked::Gone(size),
            // A concurrent pass (or the operator) got there first: that is the
            // outcome we wanted.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Unlinked::Missing,
            Err(e) => {
                last = Some(e);
                std::thread::sleep(DELETE_BACKOFF * attempt);
            }
        }
    }
    Unlinked::Kept(last.expect("a failed retry loop always has an error"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use item_core::store::{DEFAULT_DEDUP_WINDOW, Store};
    use std::sync::{Arc, Mutex};

    /// A scratch directory that removes itself, with a store and a snapshots
    /// dir inside it.
    struct Sandbox(PathBuf);

    impl Sandbox {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "item-ingest-retention-{tag}-{}-{}",
                std::process::id(),
                chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn snapshots(&self) -> PathBuf {
            self.0.join("snapshots")
        }

        fn store(&self) -> SharedStore {
            Arc::new(Mutex::new(
                Store::open(self.0.join("items.db")).expect("store opens"),
            ))
        }

        /// Write a snapshot file of `bytes` bytes for `id`.
        fn snapshot(&self, id: i64, bytes: usize) -> PathBuf {
            let path = snapshot_file(&self.snapshots(), id);
            std::fs::create_dir_all(self.snapshots()).unwrap();
            std::fs::write(&path, vec![0u8; bytes]).unwrap();
            path
        }
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn day(n: i64) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 10, 12, 0, 0).unwrap() - chrono::Duration::days(n)
    }

    /// Record a sighting whose `last_seen` is `age_days` old.
    fn sighting(store: &SharedStore, label: &str, age_days: i64) -> i64 {
        let store = store.lock().unwrap();
        let (id, _, _) = store
            .record_sighting(
                "cam",
                "desk",
                label,
                day(age_days),
                Some(&format!("snapshots/{label}.jpg")),
                DEFAULT_DEDUP_WINDOW,
            )
            .unwrap();
        id
    }

    fn window() -> Duration {
        Duration::from_secs(90 * 24 * 3600)
    }

    #[test]
    fn pruning_takes_expired_rows_and_their_snapshots_only() {
        let sandbox = Sandbox::new("prune");
        let store = sandbox.store();
        let stale = sighting(&store, "stale", 100);
        let fresh = sighting(&store, "fresh", 10);
        let stale_file = sandbox.snapshot(stale, 2048);
        let fresh_file = sandbox.snapshot(fresh, 2048);

        let report = prune(&store, &sandbox.snapshots(), window(), day(0)).unwrap();

        assert_eq!(report.rows_deleted, 1);
        assert_eq!(report.files_deleted, 1);
        assert_eq!(report.bytes_freed, 2048);
        assert_eq!(report.files_kept, 0);
        assert!(!stale_file.exists(), "the expired row's snapshot is gone");
        assert!(fresh_file.exists(), "the live row's snapshot is untouched");
        assert_eq!(
            store.lock().unwrap().observation_ids().unwrap(),
            vec![fresh]
        );
    }

    /// §7's hard constraint, as a test: cleanup must never leave a row pointing
    /// at a file that is gone.
    #[test]
    fn no_surviving_row_points_at_a_deleted_snapshot() {
        let sandbox = Sandbox::new("dangling");
        let store = sandbox.store();
        for (index, age) in [1, 40, 100, 200].into_iter().enumerate() {
            let id = sighting(&store, &format!("thing{index}"), age);
            sandbox.snapshot(id, 128);
        }

        prune(&store, &sandbox.snapshots(), window(), day(0)).unwrap();

        let live = store.lock().unwrap().observation_ids().unwrap();
        assert_eq!(live.len(), 2, "two of the four rows expired: {live:?}");
        for id in live {
            assert!(
                snapshot_file(&sandbox.snapshots(), id).exists(),
                "observation {id} survived the sweep but its snapshot did not"
            );
        }
    }

    #[test]
    fn a_row_without_a_snapshot_file_is_not_an_error() {
        let sandbox = Sandbox::new("nosnap");
        let store = sandbox.store();
        sighting(&store, "frigate-fed", 200);

        let report = prune(&store, &sandbox.snapshots(), window(), day(0)).unwrap();
        assert_eq!(report.rows_deleted, 1);
        assert_eq!(report.files_deleted, 0, "there was no file to delete");
        assert_eq!(report.files_kept, 0);
    }

    /// The Windows case in §7/§10: a snapshot another process holds open must
    /// not abort the sweep. With `share_mode(0)` no other handle may delete the
    /// file, which is a real sharing violation rather than a proxy for one.
    #[cfg(windows)]
    #[test]
    fn a_file_held_by_another_process_does_not_abort_the_sweep() {
        use std::os::windows::fs::OpenOptionsExt;

        let sandbox = Sandbox::new("locked");
        let store = sandbox.store();
        let locked = sighting(&store, "locked", 200);
        let free = sighting(&store, "free", 200);
        // A row that outlives the sweep, so the orphan scan has an id range to
        // compare against (ids are reused once the table empties).
        let live = sighting(&store, "live", 1);
        assert!(live > locked);
        let locked_file = sandbox.snapshot(locked, 64);
        let free_file = sandbox.snapshot(free, 64);

        let held = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&locked_file)
            .expect("opening with no sharing");

        let report = prune(&store, &sandbox.snapshots(), window(), day(0)).unwrap();

        assert_eq!(report.rows_deleted, 2, "both expired rows go regardless");
        assert_eq!(report.files_deleted, 1, "the free file still goes");
        assert_eq!(report.files_kept, 1, "the held file is counted, not fatal");
        assert!(!free_file.exists());
        assert!(locked_file.exists(), "the held file is still there");

        drop(held);
        // Once the holder lets go, the next pass finishes the job.
        let second = reconcile(&store, &sandbox.snapshots()).unwrap();
        assert_eq!(second.files_deleted, 1);
        assert_eq!(second.files_kept, 0);
        assert!(!locked_file.exists(), "the next pass finishes the job");
    }

    #[test]
    fn reconciling_deletes_files_with_no_row_and_keeps_everything_else() {
        let sandbox = Sandbox::new("orphans");
        let store = sandbox.store();
        let orphan_id = sighting(&store, "gone", 200);
        let live = sighting(&store, "live", 1);
        let live_file = sandbox.snapshot(live, 16);
        let orphan = sandbox.snapshot(orphan_id, 32);
        // The state a failed unlink leaves behind: the row is gone from the
        // table, its file is still on disk.
        let expired = { store.lock().unwrap().expire_observations(day(90)).unwrap() };
        assert_eq!(expired, vec![orphan_id]);
        // Files that are not ours: another extension, a non-numeric name, and
        // an id the database has never issued.
        let foreign = [
            sandbox.snapshots().join("notes.txt"),
            sandbox.snapshots().join("cover.jpg"),
            sandbox.snapshot(live + 500, 8),
        ];
        for path in &foreign {
            std::fs::write(path, b"not a snapshot").unwrap();
        }

        let report = reconcile(&store, &sandbox.snapshots()).unwrap();

        assert_eq!(report.files_deleted, 1);
        assert_eq!(report.bytes_freed, 32);
        assert_eq!(report.files_kept, 0);
        assert!(!orphan.exists());
        assert!(live_file.exists(), "a referenced snapshot is never touched");
        for path in &foreign {
            assert!(path.exists(), "{} is not ours to delete", path.display());
        }
    }

    /// The guard that keeps a mis-pointed `snapshots_dir` from being emptied:
    /// with no rows to compare against, nothing is deleted.
    #[test]
    fn reconciling_an_empty_database_deletes_nothing() {
        let sandbox = Sandbox::new("empty");
        let store = sandbox.store();
        let file = sandbox.snapshot(1, 16);

        let report = reconcile(&store, &sandbox.snapshots()).unwrap();

        assert_eq!(report, SweepReport::default());
        assert!(file.exists(), "an empty database is not proof of an orphan");
    }

    #[test]
    fn a_missing_snapshots_directory_is_not_an_error() {
        let sandbox = Sandbox::new("nodir");
        let store = sandbox.store();
        sighting(&store, "live", 1);

        let report = reconcile(&store, &sandbox.snapshots()).unwrap();
        assert_eq!(report, SweepReport::default());
    }

    #[test]
    fn orphan_candidates_are_strictly_ours() {
        assert_eq!(orphan_candidate("12.jpg".as_ref()), Some(12));
        assert_eq!(orphan_candidate("12-ev34.jpg".as_ref()), Some(12));
        assert_eq!(orphan_candidate("12-g56.jpg".as_ref()), Some(12));
        assert_eq!(orphan_candidate("0.jpg".as_ref()), None, "ids start at 1");
        assert_eq!(orphan_candidate("0-ev1.jpg".as_ref()), None);
        assert_eq!(orphan_candidate("12.JPG".as_ref()), None);
        assert_eq!(orphan_candidate("12.png".as_ref()), None);
        assert_eq!(orphan_candidate("snapshot-12.jpg".as_ref()), None);
        assert_eq!(orphan_candidate(".jpg".as_ref()), None);
        assert_eq!(
            orphan_candidate("ev34-12.jpg".as_ref()),
            None,
            "id must be prefix"
        );
    }
}
