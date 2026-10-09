//! Single-instance guard (design/resident-ingest.md §1) and the stale-lock
//! takeover that lets a service manager restart a killed daemon (§8).
//!
//! Two writers on one WAL database is the failure that corrupts data quietly,
//! so the daemon takes an exclusive lock *before* it opens the store.
//!
//! The mechanism is an atomically created lock file: `create_new(true)`, which
//! is `O_EXCL` on Unix and `CREATE_NEW` on Windows, with the same "fail if it
//! already exists" semantics on both. Deliberately **not** a file lock --
//! `flock` and `LockFileEx` disagree about advisory vs mandatory locking and
//! about what happens across processes, and §1 rules that out.
//!
//! The file carries the holder's pid and start time. It does not survive a
//! hard kill (nothing runs `Drop`), so a stale file is possible. Two facts
//! together decide whether a file on disk is a live lock or wreckage:
//!
//! 1. **The holder's heartbeat.** A running daemon re-stamps the file's
//!    modification time on a fixed cadence ([`HEARTBEAT`]); a file whose mtime
//!    has not moved for [`STALE_AFTER`] belongs to a process that is gone.
//!    This is a file observation, not a process check -- `kill(pid, 0)` is not
//!    portable and Unix reuses pids, which §5 already rules out for liveness.
//! 2. **The health file's `updated_at`** (§5), when the caller passes it: a
//!    daemon that is alive but wedged between lock heartbeats would still be
//!    visibly alive there, and stealing its lock would be the one mistake this
//!    module must not make.
//!
//! [`SingleInstance::acquire_or_recover`] uses both; plain
//! [`SingleInstance::acquire`] stays strict (fail on any existing file) and is
//! what `--maintenance` uses.

use std::fs::OpenOptions;
use std::io::{Seek as _, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Utc};

/// How often a held lock re-stamps its own mtime.
pub const HEARTBEAT: Duration = Duration::from_secs(15);

/// How long a lock file may go without a heartbeat before it is treated as
/// wreckage. Three missed heartbeats: long enough that a busy or briefly-paged
/// daemon is never mistaken for a dead one, short enough that a service
/// manager's next restart attempt (typically a minute) succeeds.
pub const STALE_AFTER: Duration = Duration::from_secs(45);

#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error(
        "another item-ingest is already running: {holder} (lock file {path}); \
         stop it, or delete the file if that process is really gone"
    )]
    Held { path: PathBuf, holder: String },
    #[error("could not create the lock file {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Why a held lock was taken over. Reported so the daemon can log the
/// "previous run died without shutdown" line §8 asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recovered {
    /// The file's heartbeat had stopped: the process that wrote it is gone.
    HeartbeatStopped,
}

impl Recovered {
    pub fn reason(self) -> &'static str {
        match self {
            Recovered::HeartbeatStopped => "the lock file's heartbeat had stopped",
        }
    }
}

/// Holds the lock for as long as it is alive; `Drop` releases it, so a clean
/// exit never leaves a file the operator has to delete by hand.
#[derive(Debug)]
pub struct SingleInstance {
    path: PathBuf,
    /// Set when this instance took over a stale file rather than creating a
    /// fresh one; the daemon logs it as §8's "previous run died".
    recovered: Option<Recovered>,
}

/// Re-stamp the lock file's mtime and content so other processes can see the
/// holder is alive. Failure is not fatal: it only means a later process has
/// less evidence, never that this one stopped holding the lock.
///
/// Free function rather than a method so the daemon's heartbeat thread can call
/// it with only the path, without sharing the guard.
pub fn touch(path: &Path) {
    match OpenOptions::new().write(true).open(path) {
        Ok(mut file) => stamp(&mut file, Some(Utc::now())),
        Err(e) => tracing::debug!(
            path = %path.display(),
            error = %e,
            "lock heartbeat could not be written"
        ),
    }
}

impl SingleInstance {
    /// Take the lock, failing if any lock file exists -- the strict form used
    /// where a human is watching (`--maintenance`).
    pub fn acquire(path: &Path) -> Result<Self, LockError> {
        Self::try_acquire(path, None)
    }

    /// Take the lock, or take over a lock file whose holder is demonstrably
    /// gone (§8: the service manager restarts a killed daemon, and the entire
    /// restart would otherwise be refused forever by the wreckage).
    ///
    /// `health_updated_at` is the caller's read of `health.json`'s
    /// `updated_at` (§5). **A fresh health stamp vetoes the takeover**: the
    /// process may be alive, merely slow to heartbeat, and two daemons on one
    /// WAL file is the one outcome that must never happen. `None` (no readable
    /// health file) does not veto; the heartbeat alone is then the evidence.
    pub fn acquire_or_recover(
        path: &Path,
        health_updated_at: Option<DateTime<Utc>>,
    ) -> Result<Self, LockError> {
        Self::try_acquire(path, Some(health_updated_at))
    }

    fn try_acquire(
        path: &Path,
        health_updated_at: Option<Option<DateTime<Utc>>>,
    ) -> Result<Self, LockError> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).map_err(|source| LockError::Io {
                path: dir.to_path_buf(),
                source,
            })?;
        }

        match OpenOptions::new().write(true).create_new(true).open(path) {
            Ok(mut file) => {
                stamp(&mut file, None);
                Ok(Self {
                    path: path.to_path_buf(),
                    recovered: None,
                })
            }
            Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => {
                // The strict path never recovers.
                let Some(health) = health_updated_at else {
                    return Err(Self::held(path));
                };
                Self::recover(path, health)
            }
            Err(source) => Err(LockError::Io {
                path: path.to_path_buf(),
                source,
            }),
        }
    }

    /// Decide whether the existing file is wreckage, and if so, replace it.
    fn recover(path: &Path, health_updated_at: Option<DateTime<Utc>>) -> Result<Self, LockError> {
        let heartbeat_age = match std::fs::metadata(path).and_then(|m| m.modified()) {
            Ok(modified) => modified.elapsed().unwrap_or(Duration::ZERO),
            // Metadata we cannot read is not evidence of death.
            Err(_) => return Err(Self::held(path)),
        };
        if heartbeat_age < STALE_AFTER {
            return Err(Self::held(path));
        }

        // A live-but-wedged daemon: fresh health stamp, silent lock. Do not
        // touch it.
        if let Some(updated) = health_updated_at {
            let health_age = Utc::now()
                .signed_duration_since(updated)
                .to_std()
                .unwrap_or_default();
            if health_age < crate::health::STALE_AFTER {
                tracing::warn!(
                    path = %path.display(),
                    health_age_s = health_age.as_secs(),
                    "lock heartbeat stopped but the health file is fresh; \
                     refusing to take over a possibly-live daemon"
                );
                return Err(Self::held(path));
            }
        }

        // Both signals agree the holder is gone. Removing the file before
        // recreating it is what makes this a takeover rather than a second
        // holder: `create_new` guarantees we win the re-create race, and a
        // loser in that race gets the Held error.
        let holder = std::fs::read_to_string(path).unwrap_or_default();
        std::fs::remove_file(path).map_err(|source| LockError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        match OpenOptions::new().write(true).create_new(true).open(path) {
            Ok(mut file) => {
                stamp(&mut file, None);
                tracing::warn!(
                    path = %path.display(),
                    previous_holder = %holder.split_whitespace().collect::<Vec<_>>().join(" "),
                    reason = Recovered::HeartbeatStopped.reason(),
                    "previous run died without shutdown; taking over the lock"
                );
                Ok(Self {
                    path: path.to_path_buf(),
                    recovered: Some(Recovered::HeartbeatStopped),
                })
            }
            Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => {
                // Someone re-created it between our remove and open: they hold
                // it now, and we must not fight over it.
                Err(Self::held(path))
            }
            Err(source) => Err(LockError::Io {
                path: path.to_path_buf(),
                source,
            }),
        }
    }

    fn held(path: &Path) -> LockError {
        let holder = std::fs::read_to_string(path).unwrap_or_default();
        let holder = holder.split_whitespace().collect::<Vec<_>>().join(" ");
        LockError::Held {
            path: path.to_path_buf(),
            holder: if holder.is_empty() {
                "(no pid recorded)".to_string()
            } else {
                holder
            },
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// `Some(_)` when this instance took over a dead holder's file.
    pub fn recovered(&self) -> Option<Recovered> {
        self.recovered
    }

    /// Re-stamp this instance's lock file (see [`touch`]).
    pub fn heartbeat(&self) {
        touch(&self.path);
    }
}

/// Write the holder identity, and (for a heartbeat) the current time so the
/// file's own content carries the same evidence as its mtime.
fn stamp(file: &mut std::fs::File, heartbeat_at: Option<DateTime<Utc>>) {
    let _ = file.set_len(0);
    let _ = file.seek(std::io::SeekFrom::Start(0));
    let _ = writeln!(file, "pid={}", std::process::id());
    let _ = writeln!(file, "started_at={}", Utc::now().to_rfc3339());
    if let Some(at) = heartbeat_at {
        let _ = writeln!(file, "heartbeat={}", at.to_rfc3339());
    }
    let _ = file.flush();
}

impl Drop for SingleInstance {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static SEQ: AtomicUsize = AtomicUsize::new(0);

    fn temp_lock() -> PathBuf {
        std::env::temp_dir().join(format!(
            "item-ingest-lock-{}-{}.lock",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ))
    }

    /// Backdate a file's mtime, since tests cannot wait out a real 45s window.
    fn age_file(path: &Path, age: Duration) {
        let file = OpenOptions::new().write(true).open(path).unwrap();
        let when = std::time::SystemTime::now() - age;
        file.set_modified(when).unwrap();
    }

    #[test]
    fn a_second_acquire_names_the_holder_and_leaves_the_file_alone() {
        let path = temp_lock();
        let first = SingleInstance::acquire(&path).expect("first acquire succeeds");

        let err = SingleInstance::acquire(&path).expect_err("second acquire must be refused");
        let message = err.to_string();
        assert!(
            message.contains(&std::process::id().to_string()),
            "the loser must be told which pid holds it: {message}"
        );
        assert!(message.contains(&path.display().to_string()));

        // The loser must not have truncated or removed the winner's file.
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains(&format!("pid={}", std::process::id())));
        assert!(body.contains("started_at="));

        drop(first);
        assert!(!path.exists(), "a clean exit releases the lock");
        let _ = SingleInstance::acquire(&path).expect("the lock is reusable after release");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_lock_file_creates_its_own_directory() {
        let dir = std::env::temp_dir().join(format!("item-ingest-lockdir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("nested").join("ingest.lock");

        let guard = SingleInstance::acquire(&path).expect("creates parent directories");
        assert!(path.exists());
        drop(guard);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A heartbeat keeps the file fresh; that is the whole liveness signal.
    #[test]
    fn a_heartbeat_refreshes_the_file() {
        let path = temp_lock();
        let guard = SingleInstance::acquire(&path).unwrap();
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();

        age_file(&path, Duration::from_secs(600));
        assert!(
            std::fs::metadata(&path).unwrap().modified().unwrap() < before,
            "the test backdated it"
        );

        guard.heartbeat();

        let age = std::fs::metadata(&path)
            .unwrap()
            .modified()
            .unwrap()
            .elapsed()
            .unwrap();
        assert!(
            age < Duration::from_secs(5),
            "heartbeat re-stamped: {age:?}"
        );
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("heartbeat="), "and recorded when: {body}");

        let _ = std::fs::remove_file(&path);
    }

    /// The §8 restart case: a killed daemon leaves a silent lock file, and the
    /// next start takes it over instead of being refused forever.
    #[test]
    fn a_lock_whose_heartbeat_stopped_is_taken_over() {
        let path = temp_lock();
        {
            let killed = SingleInstance::acquire(&path).unwrap();
            // Simulate the hard kill: no Drop, and the file stops moving.
            std::mem::forget(killed);
        }
        age_file(&path, Duration::from_secs(600));

        let recovered = SingleInstance::acquire_or_recover(&path, None)
            .expect("a silent lock is wreckage, not a holder");
        assert_eq!(recovered.recovered(), Some(Recovered::HeartbeatStopped));
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(
            body.contains(&format!("pid={}", std::process::id())),
            "the file now names the new holder: {body}"
        );

        drop(recovered);
        let _ = std::fs::remove_file(&path);
    }

    /// A lock that is still beating must never be taken: two writers on one
    /// WAL file is the failure the module exists to prevent.
    #[test]
    fn a_live_lock_is_never_taken_over() {
        let path = temp_lock();
        let live = SingleInstance::acquire(&path).unwrap();

        let err = SingleInstance::acquire_or_recover(&path, None)
            .expect_err("a fresh heartbeat means the holder is alive");
        assert!(matches!(err, LockError::Held { .. }), "{err:?}");

        drop(live);
        let _ = std::fs::remove_file(&path);
    }

    /// The veto §5/§8 rely on: a fresh health stamp outvotes a silent lock, so
    /// a wedged-but-live daemon keeps its lock.
    #[test]
    fn a_fresh_health_file_vetoes_taking_over_a_silent_lock() {
        let path = temp_lock();
        {
            let wedged = SingleInstance::acquire(&path).unwrap();
            std::mem::forget(wedged);
        }
        age_file(&path, Duration::from_secs(600));

        let err = SingleInstance::acquire_or_recover(&path, Some(Utc::now()))
            .expect_err("a fresh health file says the daemon is alive");
        assert!(matches!(err, LockError::Held { .. }), "{err:?}");

        // A stale health stamp agrees with the silent heartbeat, and the
        // takeover proceeds.
        let old = Utc::now() - chrono::Duration::seconds(600);
        let recovered = SingleInstance::acquire_or_recover(&path, Some(old))
            .expect("both signals say the holder is gone");
        assert_eq!(recovered.recovered(), Some(Recovered::HeartbeatStopped));

        drop(recovered);
        let _ = std::fs::remove_file(&path);
    }

    /// The strict form is used by `--maintenance`, where a human is watching
    /// and an empty database is being rewritten: it never recovers.
    #[test]
    fn the_strict_acquire_refuses_even_a_stale_lock() {
        let path = temp_lock();
        {
            let killed = SingleInstance::acquire(&path).unwrap();
            std::mem::forget(killed);
        }
        age_file(&path, Duration::from_secs(600));

        assert!(
            SingleInstance::acquire(&path).is_err(),
            "strict acquire must not take over anything"
        );

        let _ = std::fs::remove_file(&path);
    }
}
