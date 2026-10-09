//! Event observation: the appeared/disappeared timeline as an append-only JSONL
//! file (design/resident-ingest.md §6).
//!
//! This is the evidence the v2 events table will be designed against, so it is
//! deliberately produced **without touching the schema**: nothing here talks to
//! `item-core`, and the whole timeline lives in a camera thread's memory until
//! it is written out.
//!
//! Three invariants:
//!
//! 1. **The file is an observation, not the truth.** When the v2 events table
//!    lands it becomes authoritative and this JSONL drops to a debugging
//!    artifact (§6). Nothing may read it back as state.
//! 2. **A lost event is better than a wrong one.** The in-memory table is keyed
//!    on the observation id: when the id changes under a key, that is reported
//!    as "the old one disappeared, a new one appeared" rather than guessed at.
//!    Coarser than reality, never false (the §6 drift note).
//! 3. **Writing must never take a camera down.** Every file operation here
//!    logs and continues; a rotation that cannot rename (Windows, a `tail`
//!    holding the file) simply leaves the log to grow until the next write.
//!
//! `covered` / `moved` are not produced: they need spatial relations (who
//! occluded whom, a container moving), which is v2 plus the rule engine. The
//! format accommodates them by keeping `event` a plain string rather than an
//! enum -- adding one costs no schema change and no change to this module.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::Serialize;

/// The event kinds this stage produces. The other two §6 names to come
/// (`covered`, `moved`) need v2; see the module doc for why they are strings.
pub const APPEARED: &str = "appeared";
pub const DISAPPEARED: &str = "disappeared";
/// A marker line written once per process start, so a disappearance never has
/// to be read across a restart boundary: after this line the table was empty,
/// and any `appeared` following it is genuinely new.
pub const DAEMON_STARTED: &str = "daemon_started";

/// Rotate at 64MB and keep two generations behind it (§6): a week of events has
/// to be bounded before it can be considered for a table.
pub const MAX_BYTES: u64 = 64 * 1024 * 1024;
/// `events.jsonl` -> `events.jsonl.1` -> `events.jsonl.2`; the third is dropped.
pub const MAX_GENERATIONS: u32 = 2;

/// How many times a rotation rename is retried before giving up for this write.
/// Mirrors the health file's rename policy (§5) and the sweep's unlink policy
/// (§7): on Windows another process holding the file open makes the rename
/// fail, and a log rotation must never be the thing that breaks a daemon.
const RENAME_ATTEMPTS: u32 = 3;
/// Base delay between those retries; the nth retry waits `n * this`.
const RENAME_BACKOFF: Duration = Duration::from_millis(50);

/// The smallest disappearance gap. Below this, a detection that is merely
/// throttled or briefly missed would read as an object leaving the room.
pub const MIN_GAP: Duration = Duration::from_secs(30);
/// The gap is this many detection intervals when that is longer than
/// [`MIN_GAP`] (§6: `max(30s, 3/detect_fps)`).
const GAP_INTERVALS: u32 = 3;

/// One line of the event log, in the §6 shape:
///
/// ```json
/// {"ts":"2026-09-09T02:31:02Z","camera":"living","zone":"desk","label":"keys",
///  "obs_id":87,"event":"disappeared","hits":64,"seen_for_s":128.4}
/// ```
///
/// `seen_for_s` is present only on `disappeared`: §6's own example carries it
/// only there, and it means **how long the object was observed** -- first hit
/// to last hit -- which is what §10 compares against the row's
/// `last_seen - first_seen`. (It is not the quiet time before the sweep: an
/// object still in view at shutdown is closed by the flush and would then read
/// as `seen_for_s: 0.7`.) The appeared line has no duration to report yet.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Event {
    pub ts: String,
    pub camera: String,
    pub zone: String,
    pub label: String,
    pub obs_id: i64,
    pub event: String,
    pub hits: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seen_for_s: Option<f64>,
    /// Exact source-frame geometry and lifecycle metadata used by SQLite G1.
    /// These fields stay out of the legacy JSONL shape for compatibility.
    #[serde(skip)]
    pub occurred_at: DateTime<Utc>,
    #[serde(skip)]
    pub noticed_at: DateTime<Utc>,
    #[serde(skip)]
    pub reason: String,
    #[serde(skip)]
    pub geometry_id: Option<i64>,
    #[serde(skip)]
    pub bbox: Option<[f32; 4]>,
    #[serde(skip)]
    pub frame_width: Option<u32>,
    #[serde(skip)]
    pub frame_height: Option<u32>,
}

impl Event {
    /// A marker line: process start. `obs_id` is 0 because no observation is
    /// involved; nothing should read a marker's id.
    fn marker(ts: DateTime<Utc>, camera: &str, event: &str) -> Self {
        Self {
            ts: ts.to_rfc3339(),
            camera: camera.to_string(),
            zone: String::new(),
            label: String::new(),
            obs_id: 0,
            event: event.to_string(),
            hits: 0,
            seen_for_s: None,
            occurred_at: ts,
            noticed_at: ts,
            reason: "marker".into(),
            geometry_id: None,
            bbox: None,
            frame_width: None,
            frame_height: None,
        }
    }

    fn appeared(
        ts: DateTime<Utc>,
        camera: &str,
        zone: &str,
        label: &str,
        obs_id: i64,
        hits: i64,
    ) -> Self {
        Self {
            ts: ts.to_rfc3339(),
            camera: camera.to_string(),
            zone: zone.to_string(),
            label: label.to_string(),
            obs_id,
            event: APPEARED.to_string(),
            hits,
            seen_for_s: None,
            occurred_at: ts,
            noticed_at: ts,
            reason: "appeared".into(),
            geometry_id: None,
            bbox: None,
            frame_width: None,
            frame_height: None,
        }
    }

    fn disappeared(
        ts: DateTime<Utc>,
        camera: &str,
        zone: &str,
        label: &str,
        obs_id: i64,
        hits: i64,
        seen_for: Duration,
    ) -> Self {
        Self {
            ts: ts.to_rfc3339(),
            camera: camera.to_string(),
            zone: zone.to_string(),
            label: label.to_string(),
            obs_id,
            event: DISAPPEARED.to_string(),
            hits,
            // The same rounding the health file applies before writing a
            // measured millisecond value: one decimal, no float noise.
            seen_for_s: Some((seen_for.as_secs_f64() * 10.0).round() / 10.0),
            // The legacy constructor represents a synthetic event whose last
            // hit and noticed time are the same; Tracker replaces these with
            // the real values before persistence.
            occurred_at: ts,
            noticed_at: ts,
            reason: "missed_gap".into(),
            geometry_id: None,
            bbox: None,
            frame_width: None,
            frame_height: None,
        }
    }

    /// One JSONL line, newline included.
    fn line(&self) -> Result<String, serde_json::Error> {
        Ok(format!("{}\n", serde_json::to_string(self)?))
    }
}

/// The append-only log on disk.
///
/// Deliberately **not** a held-open file handle: this opens, appends, flushes
/// and closes on every write. A long-lived handle is what makes rotation's
/// rename fail on Windows (the same sharing violation §5 and §7 retry around),
/// and the write rate here is one line per detection, not per frame.
pub struct EventLog {
    path: PathBuf,
    /// Bytes currently in the live file, so rotation needs no `metadata` call
    /// per write and a restart can adopt an existing file's size.
    bytes: u64,
}

impl EventLog {
    /// Open (or adopt) the log at `path`.
    ///
    /// An existing file is **appended to**, never truncated: §6's whole point is
    /// a log you can `jq`/`grep` over a long stretch of time. Its current size
    /// is adopted so rotation accounts for what is already there, and a
    /// `daemon_started` marker separates this process's events from the last
    /// one's.
    pub fn open(path: impl Into<PathBuf>, camera: &str) -> std::io::Result<Self> {
        let path = path.into();
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let mut log = Self { path, bytes };
        if let Err(e) = log.append(&Event::marker(Utc::now(), camera, DAEMON_STARTED)) {
            // A log that cannot be created at startup is still not fatal: the
            // camera loop and the database are the product, this is evidence.
            tracing::warn!(
                path = %log.path.display(),
                error = %e,
                "event log could not be opened; events will be lost"
            );
        }
        Ok(log)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one event, rotating first when the live file is over the cap.
    ///
    /// Errors are the caller's to log and drop; nothing in the ingest path may
    /// fail because an event could not be written.
    pub fn write(&mut self, event: &Event) -> std::io::Result<()> {
        if self.bytes >= MAX_BYTES {
            self.rotate();
        }
        self.append(event)
    }

    /// Append a batch, rotating once up front if needed. Used at shutdown,
    /// where several keys disappear at the same instant.
    pub fn write_all(&mut self, events: &[Event]) -> std::io::Result<()> {
        if self.bytes >= MAX_BYTES {
            self.rotate();
        }
        for event in events {
            self.append(event)?;
        }
        Ok(())
    }

    fn append(&mut self, event: &Event) -> std::io::Result<()> {
        use std::io::Write as _;

        let line = event.line().map_err(std::io::Error::other)?;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        file.write_all(line.as_bytes())?;
        // Flushed before the handle closes so a reader (jq, tail) sees whole
        // lines: a JSONL row split across a buffer boundary is not a row.
        file.flush()?;
        self.bytes += line.len() as u64;
        Ok(())
    }

    /// `events.jsonl` -> `.1` -> `.2`, dropping what `.2` held (§6).
    ///
    /// A failure here is logged and forgotten: the file keeps growing and the
    /// next write tries again, which is strictly better than dropping events or
    /// failing a camera step.
    fn rotate(&mut self) {
        for generation in (1..=MAX_GENERATIONS).rev() {
            let from = self.generation_path(generation - 1);
            if !from.exists() {
                continue;
            }
            let to = self.generation_path(generation);
            if generation == MAX_GENERATIONS {
                // The oldest generation is simply dropped; a leftover from a
                // previous failure is not an error.
                let _ = std::fs::remove_file(&to);
            }
            if let Err(e) = rename_with_retry(&from, &to) {
                tracing::warn!(
                    from = %from.display(),
                    to = %to.display(),
                    error = %e,
                    "event log rotation failed; the log keeps growing until the next attempt"
                );
                return;
            }
        }
        self.bytes = 0;
    }

    /// The live file is generation 0; `.1`, `.2` are the histories.
    fn generation_path(&self, generation: u32) -> PathBuf {
        if generation == 0 {
            return self.path.clone();
        }
        let mut name = self.path.file_name().unwrap_or_default().to_os_string();
        name.push(format!(".{generation}"));
        self.path.with_file_name(name)
    }
}

/// Rename, retrying the Windows sharing-violation case (§5's policy).
fn rename_with_retry(from: &Path, to: &Path) -> std::io::Result<()> {
    let mut last = None;
    for attempt in 1..=RENAME_ATTEMPTS {
        match std::fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(e) => {
                last = Some(e);
                std::thread::sleep(RENAME_BACKOFF * attempt);
            }
        }
    }
    Err(last.unwrap_or_else(|| std::io::Error::other("rename failed")))
}

/// One sighting as the timeline sees it: what the store reported for one
/// detection. A struct rather than a row of positional arguments because these
/// values are easy to transpose and hard to read at a call site.
#[derive(Debug, Clone, PartialEq)]
pub struct Sighting<'a> {
    pub camera_id: &'a str,
    pub zone: &'a str,
    pub label: &'a str,
    pub obs_id: i64,
    /// The store opened this row on this sighting.
    pub is_new: bool,
    /// The row's hit count as of this sighting.
    pub hits: i64,
    pub bbox: [f32; 4],
    pub frame_width: u32,
    pub frame_height: u32,
    pub geometry_id: i64,
}

/// What the in-memory table knew about one key: the observation it currently
/// maps to, when it was **first** seen on that row, when it was last hit, and
/// the hit count at that moment.
///
/// `first_hit_at` is what `seen_for_s` is measured from. That is the residency
/// duration §10's acceptance compares against `last_seen - first_seen`, and it
/// is the number §6's example implies (`hits: 64` at roughly half-second
/// sampling is ~128s, matching `seen_for_s: 128.4`). Measuring from the *last*
/// hit instead would report the quiet time before a sweep -- always ~0 for an
/// object still in view at shutdown, which reads as nonsense.
#[derive(Debug, Clone, PartialEq)]
struct Tracked {
    obs_id: i64,
    first_hit_at: DateTime<Utc>,
    last_hit_at: DateTime<Utc>,
    hits_at_last_hit: i64,
    geometry_id: i64,
    bbox: [f32; 4],
    frame_width: u32,
    frame_height: u32,
}

/// The per-camera in-memory timeline (§6).
///
/// Keyed on `(camera_id, zone, label)` -- the same tuple the store deduplicates
/// on -- and holding only what a disappearance needs. It is not persisted and
/// not queried: if it drifts from the database, the `obs_id` check makes the
/// report coarser rather than wrong.
///
/// A `BTreeMap` rather than a `HashMap` so a sweep visiting several keys
/// produces its events in a stable order, which is what makes the JSONL
/// diffable in tests.
#[derive(Debug)]
pub struct Tracker {
    gap: Duration,
    entries: BTreeMap<(String, String, String), Tracked>,
}

impl Tracker {
    /// The gap is `max(30s, 3 detection intervals)` (§6): three missed
    /// detections in a row is the signal, but never less than half a minute, or
    /// ordinary throttling would look like an object leaving.
    pub fn new(detect_interval: Duration) -> Self {
        Self {
            gap: MIN_GAP.max(detect_interval * GAP_INTERVALS),
            entries: BTreeMap::new(),
        }
    }

    pub fn gap(&self) -> Duration {
        self.gap
    }

    /// How many keys are currently open. Used in tests and log lines.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Fold one sighting into the table and return the events it implies.
    ///
    /// A first sighting (or one whose observation id changed) produces
    /// `appeared`; an id change also produces the old key's `disappeared`.
    /// Merged sightings produce nothing -- the object is still there, and a
    /// timeline of "still there" twice a second is not a timeline.
    ///
    /// `is_new` is accepted but deliberately **not** trusted as the arrival
    /// signal: it describes the store's view of the row, while what this table
    /// needs is "is this a row I have not seen on this key". Comparing `obs_id`
    /// answers exactly that and stays correct if the store's dedup rules ever
    /// change.
    pub fn record(&mut self, sighting: &Sighting<'_>, now: DateTime<Utc>) -> Vec<Event> {
        let Sighting {
            camera_id,
            zone,
            label,
            obs_id,
            is_new: _,
            hits,
            bbox,
            frame_width,
            frame_height,
            geometry_id,
        } = *sighting;
        let key = (camera_id.to_string(), zone.to_string(), label.to_string());
        let mut events = Vec::new();

        if let Some(previous) = self.entries.get(&key)
            && previous.obs_id != obs_id
        {
            // §6's drift case: the store opened a different row under a key we
            // were already tracking (the dedup window's edge, or a row deleted
            // by hand). We cannot know whether the old object left or was
            // merely renamed, so report both facts rather than invent a
            // merge.
            let mut departure = self.disappearance(&key, previous, now, "row_changed");
            departure.reason = "row_changed".into();
            events.push(departure);
        }

        let appeared = self.entries.get(&key).map(|previous| previous.obs_id) != Some(obs_id);
        // A merge must keep the row's original first-hit time: `seen_for_s` is
        // the residency duration, so only a *new* row starts a new clock.
        let first_hit_at = match self.entries.get(&key) {
            Some(previous) if !appeared => previous.first_hit_at,
            _ => now,
        };
        self.entries.insert(
            key.clone(),
            Tracked {
                obs_id,
                first_hit_at,
                last_hit_at: now,
                hits_at_last_hit: hits,
                geometry_id,
                bbox,
                frame_width,
                frame_height,
            },
        );
        if appeared {
            let mut arrival = Event::appeared(now, &key.0, &key.1, &key.2, obs_id, hits);
            arrival.geometry_id = Some(geometry_id);
            arrival.bbox = Some(bbox);
            arrival.frame_width = Some(frame_width);
            arrival.frame_height = Some(frame_height);
            events.push(arrival);
        }
        events
    }

    /// Close every key that has not been hit for longer than the gap.
    pub fn sweep(&mut self, now: DateTime<Utc>) -> Vec<Event> {
        let stale: Vec<(String, String, String)> = self
            .entries
            .iter()
            .filter(|(_, tracked)| {
                now.signed_duration_since(tracked.last_hit_at)
                    .to_std()
                    .unwrap_or_default()
                    > self.gap
            })
            .map(|(key, _)| key.clone())
            .collect();
        self.close(stale, now, "missed_gap")
    }

    /// Close **every** open key, gap or not.
    ///
    /// Shutdown calls this (§3: before the final health write). A disappearing
    /// object is only noticed at the *next* scan, and there is no next scan once
    /// the process exits -- without this, the last real disappearance of every
    /// run would be silently lost.
    pub fn flush_all(&mut self, now: DateTime<Utc>) -> Vec<Event> {
        let all: Vec<(String, String, String)> = self.entries.keys().cloned().collect();
        self.close(all, now, "shutdown")
    }

    /// Remove `keys` from the table and report each as a disappearance.
    fn close(
        &mut self,
        keys: Vec<(String, String, String)>,
        now: DateTime<Utc>,
        reason: &str,
    ) -> Vec<Event> {
        let mut events = Vec::with_capacity(keys.len());
        for key in keys {
            let Some(tracked) = self.entries.remove(&key) else {
                continue;
            };
            events.push(self.disappearance(&key, &tracked, now, reason));
        }
        events
    }

    fn disappearance(
        &self,
        key: &(String, String, String),
        tracked: &Tracked,
        now: DateTime<Utc>,
        reason: &str,
    ) -> Event {
        // The event's timestamp is when the absence was *noticed* (`now`, the
        // sweep). `seen_for_s` is how long the object was actually observed:
        // first hit to last hit, which is the number §10 checks against the
        // row's `last_seen - first_seen`.
        let seen_for = tracked
            .last_hit_at
            .signed_duration_since(tracked.first_hit_at)
            .to_std()
            .unwrap_or_default();
        let mut event = Event::disappeared(
            now,
            &key.0,
            &key.1,
            &key.2,
            tracked.obs_id,
            tracked.hits_at_last_hit,
            seen_for,
        );
        event.occurred_at = tracked.last_hit_at;
        event.noticed_at = now;
        event.reason = reason.to_string();
        event.geometry_id = Some(tracked.geometry_id);
        event.bbox = Some(tracked.bbox);
        event.frame_width = Some(tracked.frame_width);
        event.frame_height = Some(tracked.frame_height);
        event
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-09-09T02:31:02Z")
            .unwrap()
            .with_timezone(&Utc)
            + chrono::Duration::seconds(secs)
    }

    fn sandbox(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "item-ingest-events-{tag}-{}-{}",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// One sighting, for the tracker tests: `(camera, zone, label, obs_id,
    /// is_new, hits)` reads better than a struct literal ten times over.
    fn sighting<'a>(
        camera: &'a str,
        zone: &'a str,
        label: &'a str,
        obs_id: i64,
        is_new: bool,
        hits: i64,
    ) -> Sighting<'a> {
        Sighting {
            camera_id: camera,
            zone,
            label,
            obs_id,
            is_new,
            hits,
            bbox: [10.0, 10.0, 20.0, 20.0],
            frame_width: 100,
            frame_height: 100,
            geometry_id: obs_id,
        }
    }

    /// §6's example line, reproduced exactly. If this test changes, the file
    /// format changed and v2's table design would be reading a different shape.
    #[test]
    fn a_disappearance_line_matches_the_documented_shape() {
        let event = Event::disappeared(
            at(0),
            "living",
            "desk",
            "keys",
            87,
            64,
            Duration::from_millis(128_400),
        );
        assert_eq!(
            event.line().unwrap(),
            "{\"ts\":\"2026-09-09T02:31:02+00:00\",\"camera\":\"living\",\"zone\":\"desk\",\
             \"label\":\"keys\",\"obs_id\":87,\"event\":\"disappeared\",\"hits\":64,\
             \"seen_for_s\":128.4}\n"
        );
    }

    #[test]
    fn an_arrival_carries_no_seen_for_field() {
        let event = Event::appeared(at(0), "living", "desk", "keys", 1, 1);
        let line = event.line().unwrap();
        assert!(
            !line.contains("seen_for_s"),
            "the field is omitted, not null: {line}"
        );
        assert!(line.contains("\"hits\":1"));
    }

    #[test]
    fn seen_for_is_rounded_to_one_decimal() {
        // 90s whole, and a value whose float form is famously noisy.
        let whole = Event::disappeared(at(0), "c", "z", "l", 1, 1, Duration::from_secs(90));
        assert!(whole.line().unwrap().contains("\"seen_for_s\":90.0"));

        let noisy = Event::disappeared(
            at(0),
            "c",
            "z",
            "l",
            1,
            1,
            Duration::from_nanos(128_400_000_000),
        );
        assert!(noisy.line().unwrap().contains("\"seen_for_s\":128.4"));
    }

    #[test]
    fn writing_appends_parseable_lines_and_adopts_an_existing_file() {
        let dir = sandbox("append");
        let path = dir.join("events.jsonl");

        let mut log = EventLog::open(&path, "cam").unwrap();
        log.write(&Event::appeared(at(0), "cam", "desk", "keys", 1, 1))
            .unwrap();

        // A second process start must not truncate: it adopts the size and
        // marks itself.
        let mut second = EventLog::open(&path, "cam").unwrap();
        second
            .write(&Event::disappeared(
                at(10),
                "cam",
                "desk",
                "keys",
                1,
                4,
                Duration::from_secs(10),
            ))
            .unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<serde_json::Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).expect("every line is standalone JSON"))
            .collect();
        assert_eq!(
            lines.len(),
            4,
            "start marker + appeared + start + disappeared"
        );
        assert_eq!(lines[0]["event"], DAEMON_STARTED);
        assert_eq!(lines[1]["event"], APPEARED);
        assert_eq!(lines[2]["event"], DAEMON_STARTED);
        assert_eq!(lines[3]["event"], DISAPPEARED);
        assert_eq!(lines[3]["seen_for_s"], 10.0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rotation_moves_the_live_file_aside_and_keeps_two_generations() {
        let dir = sandbox("rotate");
        let path = dir.join("events.jsonl");
        let mut log = EventLog::open(&path, "cam").unwrap();
        // Pretend the live file is already at the cap, without writing 64MB.
        log.bytes = MAX_BYTES;

        log.write(&Event::appeared(at(0), "cam", "desk", "keys", 1, 1))
            .unwrap();

        assert!(path.exists(), "the new live file exists");
        assert!(
            dir.join("events.jsonl.1").exists(),
            "the marker line was rotated aside"
        );
        let rotated = std::fs::read_to_string(dir.join("events.jsonl.1")).unwrap();
        assert!(
            rotated.contains(DAEMON_STARTED),
            "generation .1 is the old content"
        );
        let live = std::fs::read_to_string(&path).unwrap();
        assert!(!live.contains(DAEMON_STARTED), "the live file starts fresh");
        assert!(live.contains(APPEARED));

        // Two more rotations: `.1` and `.2` exist, and nothing beyond them.
        for _ in 0..2 {
            log.bytes = MAX_BYTES;
            log.write(&Event::appeared(at(0), "cam", "desk", "keys", 1, 1))
                .unwrap();
        }
        assert!(dir.join("events.jsonl.1").exists());
        assert!(dir.join("events.jsonl.2").exists());
        assert!(
            !dir.join("events.jsonl.3").exists(),
            "two generations is the cap (§6)"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The Windows case: when a rotation cannot rename -- because something
    /// else holds the destination -- the write must still land and no error may
    /// reach the caller. The log simply keeps growing until a later attempt
    /// succeeds.
    #[cfg(windows)]
    #[test]
    fn a_rotation_that_cannot_rename_does_not_fail_the_write() {
        use std::os::windows::fs::OpenOptionsExt;

        let dir = sandbox("locked-rotate");
        let path = dir.join("events.jsonl");
        let mut log = EventLog::open(&path, "cam").unwrap();
        // Two rotations would need to move `.1` -> `.2`; hold `.2` so the
        // second move cannot happen. (Holding the *live* file would block the
        // append itself, which is a different failure and not this test's
        // subject: appends are permitted, renames are not.)
        std::fs::write(dir.join("events.jsonl.1"), "old\n").unwrap();
        let blocked = dir.join("events.jsonl.2");
        std::fs::write(&blocked, "older\n").unwrap();
        let held = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&blocked)
            .expect("opening with no sharing");

        log.bytes = MAX_BYTES;
        log.write(&Event::appeared(at(0), "cam", "desk", "keys", 1, 1))
            .expect("a failed rotation must not surface as an error");

        // The event landed in the live file regardless.
        let live = std::fs::read_to_string(&path).unwrap();
        assert!(
            live.contains(APPEARED),
            "the event was written even though rotation could not finish"
        );
        drop(held);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_gap_is_three_detection_intervals_with_a_thirty_second_floor() {
        assert_eq!(
            Tracker::new(Duration::from_secs(1)).gap(),
            MIN_GAP,
            "1s interval -> 3s, so the floor wins"
        );
        assert_eq!(
            Tracker::new(Duration::from_secs(20)).gap(),
            Duration::from_secs(60),
            "20s interval -> 60s beats the floor"
        );
    }

    #[test]
    fn a_first_sighting_appears_and_repeats_stay_silent() {
        let mut tracker = Tracker::new(Duration::from_secs(1));

        let events = tracker.record(&sighting("cam", "desk", "keys", 7, true, 1), at(0));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, APPEARED);
        assert_eq!(events[0].obs_id, 7);

        // Merged sightings: present, but not news.
        let events = tracker.record(&sighting("cam", "desk", "keys", 7, false, 2), at(1));
        assert!(events.is_empty(), "a merge is not an event");
        assert_eq!(tracker.len(), 1);
    }

    #[test]
    fn a_key_not_seen_for_longer_than_the_gap_disappears() {
        let mut tracker = Tracker::new(Duration::from_secs(1)); // gap = 30s
        tracker.record(&sighting("cam", "desk", "keys", 7, true, 1), at(0));
        tracker.record(&sighting("cam", "desk", "keys", 7, false, 40), at(20));

        assert!(
            tracker.sweep(at(40)).is_empty(),
            "20s < 30s gap: still there"
        );
        let events = tracker.sweep(at(60));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, DISAPPEARED);
        assert_eq!(
            events[0].hits, 40,
            "the hit count at the LAST sighting, not at the sweep"
        );
        assert_eq!(
            events[0].seen_for_s,
            Some(20.0),
            "residency: first hit at t+0 to last hit at t+20, not the 40s quiet time"
        );
        assert_eq!(
            events[0].ts,
            at(60).to_rfc3339(),
            "but the event is stamped when the absence was noticed"
        );
        assert!(tracker.is_empty());
    }

    /// The residency semantics §10 checks: `seen_for_s` must equal the span the
    /// observations cover, so a merge keeps the original first hit instead of
    /// restarting the clock.
    #[test]
    fn seen_for_is_the_residency_from_the_first_hit_not_the_last() {
        let mut tracker = Tracker::new(Duration::from_secs(1));
        // Five merges over 80s, all one row.
        for step in 0..5 {
            tracker.record(
                &sighting("cam", "desk", "keys", 7, step == 0, step + 1),
                at(step * 20),
            );
        }
        let events = tracker.sweep(at(200)); // well past the gap
        assert_eq!(
            events[0].seen_for_s,
            Some(80.0),
            "first hit t+0, last hit t+80 -- the residency, not the quiet time"
        );
        assert_eq!(events[0].hits, 5);

        // A NEW row on the same key restarts it.
        tracker.record(&sighting("cam", "desk", "keys", 9, true, 1), at(300));
        tracker.record(&sighting("cam", "desk", "keys", 9, false, 2), at(310));
        let events = tracker.sweep(at(400));
        assert_eq!(
            events[0].seen_for_s,
            Some(10.0),
            "a fresh observation does not inherit the old one's span"
        );
    }

    /// §6's drift handling: an id change is reported as the old one going and a
    /// new one arriving. Coarser than reality, never a fabricated merge.
    #[test]
    fn an_id_change_reports_a_disappearance_and_an_arrival() {
        let mut tracker = Tracker::new(Duration::from_secs(1));
        tracker.record(&sighting("cam", "desk", "keys", 7, true, 5), at(0));

        let events = tracker.record(&sighting("cam", "desk", "keys", 12, true, 1), at(5));
        assert_eq!(events.len(), 2, "old gone, new here: {events:?}");
        assert_eq!(events[0].event, DISAPPEARED);
        assert_eq!(events[0].obs_id, 7);
        assert_eq!(events[0].hits, 5);
        assert_eq!(events[1].event, APPEARED);
        assert_eq!(events[1].obs_id, 12);
        assert_eq!(tracker.len(), 1, "the key now tracks the new row");
    }

    #[test]
    fn different_zones_are_different_keys() {
        let mut tracker = Tracker::new(Duration::from_secs(1));
        tracker.record(&sighting("cam", "desk", "keys", 1, true, 1), at(0));
        tracker.record(&sighting("cam", "shelf", "keys", 2, true, 1), at(1));
        assert_eq!(tracker.len(), 2, "(zone, label) is the identity");

        // The store would have split these too: its dedup key includes zone.
        let events = tracker.sweep(at(60));
        assert_eq!(events.len(), 2, "both zones time out");
    }

    /// Shutdown: everything open is reported, gap or not -- there is no next
    /// scan once the process exits, so the last real disappearance would
    /// otherwise be lost (§3/§6).
    #[test]
    fn flushing_reports_even_keys_hit_a_moment_ago() {
        let mut tracker = Tracker::new(Duration::from_secs(1));
        tracker.record(&sighting("cam", "desk", "keys", 1, true, 1), at(100));
        tracker.record(&sighting("cam", "shelf", "cup", 2, true, 9), at(100));

        let events = tracker.flush_all(at(101));
        assert_eq!(events.len(), 2, "both keys, well inside the gap");
        assert!(events.iter().all(|e| e.event == DISAPPEARED));
        assert!(tracker.is_empty());
        assert!(
            tracker.flush_all(at(102)).is_empty(),
            "a second flush is a no-op"
        );
    }
}
