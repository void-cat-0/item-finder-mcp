//! SQLite persistence: region config + observation log.
//!
//! Vector embeddings (sqlite-vec) are deliberately NOT wired in yet: at
//! personal scale a LIKE/label lookup over observations carries the query
//! layer, and the embedding tables should be a rebuildable cache anyway.

use std::path::Path;
use std::time::Duration;

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use thiserror::Error;

use crate::model::{
    DepthEvidence, DepthInput, GeometryInput, Observation, ObservationEvent, ObservationEventInput,
    ObservationGeometry, Region, SightingInput,
};

#[derive(Debug, Error)]
pub enum StoreError {
    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
    /// SQLite declined to finish a WAL checkpoint because another connection
    /// held the database. The data is safe; the WAL simply was not folded back.
    #[error("database busy: another connection held it, so the work did not complete")]
    Busy,
    /// A dedup window chrono cannot represent. Unreachable from this crate's
    /// call sites (all pass [`DEFAULT_DEDUP_WINDOW`]), but a public API owes
    /// the caller an error rather than a panic.
    #[error("dedup window {0:?} is out of range")]
    Window(Duration),
    #[error("unsupported database schema version {0}; this binary supports up to version 2")]
    SchemaVersion(i64),
    #[error("invalid persistence input: {0}")]
    InvalidInput(String),
}

pub type Result<T> = std::result::Result<T, StoreError>;

/// Two sightings of the same label+zone within this window are merged into one
/// observation instead of inserting a new row.
pub const DEFAULT_DEDUP_WINDOW: Duration = Duration::from_secs(300);

/// How long a statement waits for another connection's lock before failing.
/// Readers (`item-web`) and the writer (the daemon) share one WAL file, and a
/// burst of reads must not turn one of the daemon's writes into an error.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// The most rows [`Store::recent`] will return in one call, whatever the
/// caller asks for. The UI shows 200 at a time; nothing needs the whole table.
pub const MAX_LIMIT: i64 = 500;

/// Escape LIKE's wildcards so the user's text matches itself. The escape
/// character itself has to go first, or it would double-escape the additions.
fn like_literal(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

#[derive(Debug, Clone)]
pub struct ObservationFilter {
    pub label: Option<String>,
    pub camera: Option<String>,
    pub zone: Option<String>,
    /// Inclusive UTC bounds on the last actual sighting, not the whole span.
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
    pub limit: i64,
}

impl Default for ObservationFilter {
    fn default() -> Self {
        Self {
            label: None,
            camera: None,
            zone: None,
            since: None,
            until: None,
            limit: 20,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RecordedItemsFilter {
    pub camera: Option<String>,
    pub zone: Option<String>,
    pub limit: i64,
}

impl Default for RecordedItemsFilter {
    fn default() -> Self {
        Self {
            camera: None,
            zone: None,
            limit: 200,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RecordedItemSummary {
    pub label: String,
    pub observation_count: i64,
    pub last_seen: DateTime<Utc>,
    pub cameras: Vec<String>,
    pub zones: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct RecordedItemsPage {
    pub items: Vec<RecordedItemSummary>,
    pub truncated: bool,
}

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", true)?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        Self::migrate(&conn)?;
        Ok(Self { conn })
    }

    pub fn in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.pragma_update(None, "foreign_keys", true)?;
        Self::migrate(&conn)?;
        Ok(Self { conn })
    }

    /// Open an existing database strictly read-only (no migrate, no WAL
    /// change) -- the web server reads the loop's data while the ingest
    /// daemon writes it; SQLite WAL makes concurrent readers safe.
    ///
    /// Fails when the file does not exist: callers that want "empty until the
    /// first ingest run" must say so themselves, because a read-write open
    /// here would create and migrate a database, which is exactly the writer
    /// role this mode exists to avoid (`item-web`).
    pub fn open_read_only(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        conn.pragma_update(None, "foreign_keys", true)?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        Ok(Self { conn })
    }

    /// Opt-in writer operation for daemon raw assets. Does not alter the
    /// observation schema version, and is never called by a read-only adapter.
    pub fn ensure_database_identity(&self) -> Result<String> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS asset_database_identity (
                singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
                database_id TEXT NOT NULL
             );
             INSERT OR IGNORE INTO asset_database_identity(singleton, database_id)
             VALUES (1, lower(hex(randomblob(32))));",
        )?;
        let id: String = tx.query_row(
            "SELECT database_id FROM asset_database_identity WHERE singleton = 1",
            [],
            |r| r.get(0),
        )?;
        if id.len() != 64
            || !id
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(StoreError::InvalidInput(
                "invalid database asset identity".into(),
            ));
        }
        tx.commit()?;
        Ok(id)
    }

    /// Read existing identity without creating tables or migrating a legacy DB.
    pub fn database_identity(&self) -> Result<Option<String>> {
        let exists: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='asset_database_identity')",
            [], |r| r.get(0),
        )?;
        if !exists {
            return Ok(None);
        }
        let id: Option<String> = self
            .conn
            .query_row(
                "SELECT database_id FROM asset_database_identity WHERE singleton=1",
                [],
                |r| r.get(0),
            )
            .optional()?;
        if id.as_ref().is_some_and(|id| {
            id.len() != 64
                || !id
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        }) {
            return Err(StoreError::InvalidInput(
                "invalid database asset identity".into(),
            ));
        }
        Ok(id)
    }

    /// `PRAGMA user_version`: 2 once lifecycle/geometry evidence exists. A
    /// read-only opener never migrates, so a reader uses this to tell an older
    /// database (no evidence tables yet) apart from a real read failure.
    pub fn schema_version(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))?)
    }

    /// Fold the WAL back into the main database file. Called on daemon
    /// shutdown so a stopped process leaves one compact file
    /// (docs/resident-ingest.md §3/§7). `wal_checkpoint` returns a row, so
    /// `execute_batch` cannot be used here.
    ///
    /// The `busy` column is the whole point of reading the row: a `1` there
    /// means SQLite gave up on a reader and the WAL is still on disk, so
    /// reporting `Ok` would be a claim the process cannot make.
    pub fn checkpoint(&self) -> Result<()> {
        let busy: i64 = self
            .conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| r.get(0))?;
        if busy != 0 {
            return Err(StoreError::Busy);
        }
        Ok(())
    }

    fn migrate(conn: &Connection) -> Result<()> {
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version > 2 {
            return Err(StoreError::SchemaVersion(version));
        }
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS regions (
                 id         INTEGER PRIMARY KEY,
                 camera_id  TEXT NOT NULL,
                 name       TEXT NOT NULL,
                 x0 REAL NOT NULL, y0 REAL NOT NULL, x1 REAL NOT NULL, y1 REAL NOT NULL,
                 UNIQUE(camera_id, name)
             );
             CREATE TABLE IF NOT EXISTS observations (
                 id              INTEGER PRIMARY KEY,
                 camera_id       TEXT NOT NULL,
                 zone            TEXT NOT NULL,
                 label           TEXT NOT NULL,
                 first_seen      TEXT NOT NULL,
                 last_seen       TEXT NOT NULL,
                 hit_count       INTEGER NOT NULL DEFAULT 1,
                 sample_snapshot TEXT
             );
             CREATE INDEX IF NOT EXISTS idx_obs_lookup
                 ON observations (label, zone, camera_id, last_seen DESC);
             -- The retention sweep deletes by `last_seen` alone, which the
             -- lookup index above cannot serve (label is its leading column).
                 CREATE INDEX IF NOT EXISTS idx_obs_last_seen
                     ON observations (last_seen);
                 CREATE TABLE IF NOT EXISTS observation_geometry (
                     id              INTEGER PRIMARY KEY,
                     observation_id  INTEGER NOT NULL REFERENCES observations(id) ON DELETE CASCADE,
                     sample_kind     TEXT NOT NULL,
                     captured_at     TEXT NOT NULL,
                     x0              REAL NOT NULL,
                     y0              REAL NOT NULL,
                     x1              REAL NOT NULL,
                     y1              REAL NOT NULL,
                     frame_width     INTEGER,
                     frame_height    INTEGER,
                     zone            TEXT NOT NULL,
                     source          TEXT NOT NULL,
                     confidence      REAL,
                     snapshot_ref    TEXT,
                     UNIQUE(observation_id, sample_kind)
                 );
                 CREATE INDEX IF NOT EXISTS idx_geometry_camera_time
                     ON observation_geometry (captured_at, observation_id);
                 CREATE TABLE IF NOT EXISTS observation_events (
                     id              INTEGER PRIMARY KEY,
                     observation_id  INTEGER NOT NULL REFERENCES observations(id) ON DELETE CASCADE,
                     camera_id       TEXT NOT NULL,
                     zone            TEXT NOT NULL,
                     label           TEXT NOT NULL,
                     event_type      TEXT NOT NULL,
                     occurred_at     TEXT NOT NULL,
                     noticed_at      TEXT NOT NULL,
                     source          TEXT NOT NULL,
                     session_id      TEXT,
                     reason          TEXT NOT NULL,
                     hits            INTEGER NOT NULL,
                     seen_for_s      REAL,
                     geometry_id     INTEGER REFERENCES observation_geometry(id) ON DELETE SET NULL
                 );
                 CREATE INDEX IF NOT EXISTS idx_events_camera_time
                     ON observation_events (camera_id, occurred_at);
                 CREATE TABLE IF NOT EXISTS depth_evidence (
                     id                INTEGER PRIMARY KEY,
                     geometry_id       INTEGER NOT NULL REFERENCES observation_geometry(id) ON DELETE CASCADE,
                     provider          TEXT NOT NULL,
                     frame_id          TEXT NOT NULL DEFAULT '',
                     response_id       TEXT NOT NULL DEFAULT '',
                     prompt_version    TEXT NOT NULL DEFAULT '',
                     backend_version   TEXT NOT NULL DEFAULT '',
                     mode              TEXT NOT NULL,
                     relation_to_camera TEXT NOT NULL,
                     relative_score   REAL,
                     quality           TEXT NOT NULL,
                     model_ref         TEXT,
                     value_m           REAL,
                     uncertainty_m     REAL,
                     valid_fraction    REAL,
                     coordinate_frame TEXT,
                     calibration_ref   TEXT
                 );
                 CREATE INDEX IF NOT EXISTS idx_depth_geometry
                     ON depth_evidence (geometry_id);",
        )?;
        let columns = tx
            .prepare("PRAGMA table_info(depth_evidence)")?
            .query_map([], |r| r.get::<_, String>(1))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (name, sql) in [
            (
                "frame_id",
                "ALTER TABLE depth_evidence ADD COLUMN frame_id TEXT NOT NULL DEFAULT ''",
            ),
            (
                "response_id",
                "ALTER TABLE depth_evidence ADD COLUMN response_id TEXT NOT NULL DEFAULT ''",
            ),
            (
                "prompt_version",
                "ALTER TABLE depth_evidence ADD COLUMN prompt_version TEXT NOT NULL DEFAULT ''",
            ),
            (
                "backend_version",
                "ALTER TABLE depth_evidence ADD COLUMN backend_version TEXT NOT NULL DEFAULT ''",
            ),
        ] {
            if !columns.iter().any(|column| column == name) {
                tx.execute(sql, [])?;
            }
        }
        tx.pragma_update(None, "user_version", 2i64)?;
        tx.commit()?;
        Ok(())
    }

    // ---- regions -----------------------------------------------------------

    pub fn upsert_region(&self, camera_id: &str, name: &str, rect: [f32; 4]) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO regions (camera_id, name, x0, y0, x1, y1)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(camera_id, name) DO UPDATE SET
                 x0 = excluded.x0, y0 = excluded.y0, x1 = excluded.x1, y1 = excluded.y1",
            params![camera_id, name, rect[0], rect[1], rect[2], rect[3]],
        )?;
        let id: i64 = self.conn.query_row(
            "SELECT id FROM regions WHERE camera_id = ?1 AND name = ?2",
            params![camera_id, name],
            |r| r.get(0),
        )?;
        Ok(id)
    }

    pub fn regions_for(&self, camera_id: &str) -> Result<Vec<Region>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, camera_id, name, x0, y0, x1, y1 FROM regions WHERE camera_id = ?1
             ORDER BY name",
        )?;
        let rows = stmt.query_map(params![camera_id], |r| {
            Ok(Region {
                id: r.get(0)?,
                camera_id: r.get(1)?,
                name: r.get(2)?,
                rect: [r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?],
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// First region whose rect contains `point`, if any.
    pub fn zone_for_point(&self, camera_id: &str, point: (f32, f32)) -> Result<String> {
        Ok(self
            .regions_for(camera_id)?
            .into_iter()
            .find(|rg| rg.contains(point))
            .map(|rg| rg.name)
            .unwrap_or_else(|| "frame".into()))
    }

    // ---- observations ------------------------------------------------------

    /// Merge with the latest observation of (camera, zone, label) whose span
    /// reaches within `window` of `seen_at`, otherwise insert a fresh row.
    /// Returns (row id, is_new, hit_count): the caller writes a snapshot
    /// exactly when is_new, and uses `hit_count` for the disappearance events
    /// of docs/resident-ingest.md §6, which report how many times the object
    /// was seen before it went.
    ///
    /// Two properties the merge has to keep, because `[first_seen, last_seen]`
    /// is read as a span (event `seen_for_s`, `item-web`'s ordering, and the
    /// retention clock, which follows `last_seen`):
    ///
    /// - **Sightings arrive in any order.** `seen_at` comes from the caller --
    ///   a webhook's `frame_time`, a camera's wall clock -- so a late or
    ///   backdated sighting must not drag `last_seen` behind `first_seen`.
    ///   The two bounds widen the span; they never move it backwards.
    /// - **The window is symmetric.** A sighting more than `window` *older*
    ///   than a row's `first_seen` opens its own row instead of stretching
    ///   that row across the gap (a two-day-old Frigate event must not become
    ///   part of today's observation).
    pub fn record_sighting(
        &self,
        camera_id: &str,
        zone: &str,
        label: &str,
        seen_at: DateTime<Utc>,
        snapshot: Option<&str>,
        window: Duration,
    ) -> Result<(i64, bool, i64)> {
        let window = chrono::Duration::from_std(window).map_err(|_| StoreError::Window(window))?;
        let earliest = (seen_at - window).to_rfc3339();
        let latest = (seen_at + window).to_rfc3339();
        let existing: Option<i64> = self
            .conn
            .query_row(
                "SELECT id FROM observations
                 WHERE camera_id = ?1 AND zone = ?2 AND label = ?3
                   AND last_seen >= ?4 AND first_seen <= ?5
                 ORDER BY last_seen DESC LIMIT 1",
                params![camera_id, zone, label, earliest, latest],
                |r| r.get(0),
            )
            .optional()?;

        if let Some(id) = existing {
            // `RETURNING hit_count` reads back the value the same statement
            // just wrote, so the count costs no extra round trip. `max`/`min`
            // are SQLite's scalar two-argument forms; the RFC3339 text they
            // compare orders chronologically (same reason the window test
            // above can be a text comparison).
            let hits: i64 = self.conn.query_row(
                "UPDATE observations
                 SET last_seen  = max(last_seen, ?2),
                     first_seen = min(first_seen, ?2),
                     hit_count = hit_count + 1,
                     sample_snapshot = COALESCE(sample_snapshot, ?3)
                 WHERE id = ?1
                 RETURNING hit_count",
                params![id, seen_at.to_rfc3339(), snapshot],
                |r| r.get(0),
            )?;
            return Ok((id, false, hits));
        }

        self.conn.execute(
            "INSERT INTO observations
                 (camera_id, zone, label, first_seen, last_seen, hit_count, sample_snapshot)
             VALUES (?1, ?2, ?3, ?4, ?4, 1, ?5)",
            params![camera_id, zone, label, seen_at.to_rfc3339(), snapshot],
        )?;
        Ok((self.conn.last_insert_rowid(), true, 1))
    }

    /// Record a sighting, bounded geometry, and optional relative depth as one
    /// transaction. `first` and `last` remain the only moving samples.
    pub fn record_sighting_with_geometry(
        &self,
        input: &SightingInput,
    ) -> Result<(i64, bool, i64, Option<i64>)> {
        self.validate_sighting(input)?;
        let window = Duration::from_secs(input.window_seconds);
        let window = chrono::Duration::from_std(window).map_err(|_| StoreError::Window(window))?;
        let earliest = (input.seen_at - window).to_rfc3339();
        let latest = (input.seen_at + window).to_rfc3339();
        let seen = input.seen_at.to_rfc3339();
        let tx = self.conn.unchecked_transaction()?;
        let existing: Option<i64> = tx
            .query_row(
                "SELECT id FROM observations
                 WHERE camera_id = ?1 AND zone = ?2 AND label = ?3
                   AND last_seen >= ?4 AND first_seen <= ?5
                 ORDER BY last_seen DESC LIMIT 1",
                params![
                    &input.camera_id,
                    &input.zone,
                    &input.label,
                    earliest,
                    latest
                ],
                |r| r.get(0),
            )
            .optional()?;
        let (id, is_new, hits) = if let Some(id) = existing {
            let hits: i64 = tx.query_row(
                "UPDATE observations SET last_seen=max(last_seen, ?2),
                    first_seen=min(first_seen, ?2), hit_count=hit_count+1,
                    sample_snapshot=COALESCE(sample_snapshot, ?3)
                 WHERE id=?1 RETURNING hit_count",
                params![id, seen, &input.snapshot],
                |r| r.get(0),
            )?;
            (id, false, hits)
        } else {
            tx.execute(
                "INSERT INTO observations
                 (camera_id,zone,label,first_seen,last_seen,hit_count,sample_snapshot)
                 VALUES (?1,?2,?3,?4,?4,1,?5)",
                params![
                    &input.camera_id,
                    &input.zone,
                    &input.label,
                    seen,
                    &input.snapshot
                ],
            )?;
            (tx.last_insert_rowid(), true, 1)
        };
        let mut geometry_id = None;
        if let Some(geometry) = &input.geometry {
            self.validate_geometry(geometry)?;
            if let Some(depth) = &input.depth {
                validate_depth(depth)?;
            }
            let mut samples = vec![geometry.clone()];
            if geometry.sample_kind == "last" {
                let mut first = geometry.clone();
                first.sample_kind = "first".into();
                samples.insert(0, first);
            }
            for sample in &samples {
                let (sample_id, changed) = upsert_bounded_geometry(
                    &tx,
                    id,
                    sample,
                    sample.sample_kind == "last" && sample.source == "frigate",
                )?;
                if sample.sample_kind == geometry.sample_kind {
                    geometry_id = Some(sample_id);
                }
                if changed
                    && sample.sample_kind == geometry.sample_kind
                    && let Some(depth) = &input.depth
                {
                    insert_depth(&tx, sample_id, depth)?;
                }
            }
        }
        tx.commit()?;
        Ok((id, is_new, hits, geometry_id))
    }

    fn validate_sighting(&self, input: &SightingInput) -> Result<()> {
        if input.camera_id.is_empty()
            || input.zone.is_empty()
            || input.label.is_empty()
            || input.seen_at.timestamp_nanos_opt().is_none()
            || input.window_seconds == 0
        {
            return Err(StoreError::InvalidInput(
                "sighting identity/timestamp/window is invalid".into(),
            ));
        }
        if input.depth.is_some() && input.geometry.is_none() {
            return Err(StoreError::InvalidInput("depth requires geometry".into()));
        }
        Ok(())
    }

    /// Attach (or replace) an observation's snapshot path after the fact.
    pub fn set_sample_snapshot(&self, id: i64, path: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE observations SET sample_snapshot = ?2 WHERE id = ?1",
            params![id, path],
        )?;
        Ok(())
    }

    /// Attach a snapshot to a geometry sample (for event or per-detection snapshots).
    pub fn update_geometry_snapshot(&self, geometry_id: i64, path: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE observation_geometry SET snapshot_ref = ?2 WHERE id = ?1",
            params![geometry_id, path],
        )?;
        Ok(())
    }

    /// The stored snapshot path of one observation, if any. The web layer
    /// resolves it to a file; `frigate://` refs come back verbatim.
    ///
    /// `optional()` rather than `.ok()`: "this row has no snapshot" and "the
    /// database is broken" must not look the same to the caller.
    pub fn snapshot_path(&self, id: i64) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT sample_snapshot FROM observations WHERE id = ?1",
                params![id],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten())
    }

    /// Most recent observations, optionally filtered by a label substring.
    /// This is the query layer's data source.
    ///
    /// The filter is a **literal** substring (case-insensitive for ASCII):
    /// `%` and `_` are escaped, so a search for `50%` finds `50%` rather than
    /// everything. `limit` is clamped to at least 1 -- SQLite reads
    /// `LIMIT -1` as "no limit", which would turn a typo into a full table
    /// dump -- and at most [`MAX_LIMIT`].
    pub fn recent(&self, label_like: Option<&str>, limit: i64) -> Result<Vec<Observation>> {
        self.observations_filtered(&ObservationFilter {
            label: label_like.map(str::to_owned),
            limit,
            ..Default::default()
        })
    }

    /// Filters apply before the limit. Time bounds refer to last_seen and accept
    /// equivalent RFC3339 offsets already stored by older ingest versions.
    pub fn observations_filtered(&self, filter: &ObservationFilter) -> Result<Vec<Observation>> {
        if matches!((&filter.since, &filter.until), (Some(since), Some(until)) if since > until) {
            return Err(StoreError::InvalidInput("since is later than until".into()));
        }
        let pattern = filter.label.as_deref().map(like_literal);
        let since = filter.since.map(|date| date.to_rfc3339());
        let until = filter.until.map(|date| date.to_rfc3339());
        let mut stmt = self.conn.prepare(
            "SELECT id, camera_id, zone, label, first_seen, last_seen, hit_count, sample_snapshot
             FROM observations
             WHERE (?1 IS NULL OR label LIKE '%' || ?1 || '%' ESCAPE '\\')
               AND (?2 IS NULL OR camera_id = ?2)
               AND (?3 IS NULL OR zone = ?3)
               AND (?4 IS NULL OR julianday(last_seen) >= julianday(?4))
               AND (?5 IS NULL OR julianday(last_seen) <= julianday(?5))
             ORDER BY julianday(last_seen) DESC, id DESC LIMIT ?6",
        )?;
        let rows = stmt.query_map(
            params![
                pattern,
                filter.camera,
                filter.zone,
                since,
                until,
                filter.limit.clamp(1, MAX_LIMIT)
            ],
            observation_row,
        )?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn observation_by_id(&self, id: i64) -> Result<Option<Observation>> {
        if id <= 0 {
            return Err(StoreError::InvalidInput(
                "observation id must be positive".into(),
            ));
        }
        Ok(self.conn.query_row(
            "SELECT id, camera_id, zone, label, first_seen, last_seen, hit_count, sample_snapshot
             FROM observations WHERE id = ?1",
            params![id],
            observation_row,
        ).optional()?)
    }

    /// Aggregate the full filtered observation log, not a recent-row sample.
    /// Distinct labels retain their original spelling and case.
    pub fn recorded_items(&self, filter: &RecordedItemsFilter) -> Result<RecordedItemsPage> {
        let limit = filter.limit.clamp(1, MAX_LIMIT) as usize;
        let tx = self.conn.unchecked_transaction()?;
        let summaries = {
            let mut stmt = tx.prepare(
                "SELECT label, COUNT(*) FROM observations
                 WHERE (?1 IS NULL OR camera_id = ?1) AND (?2 IS NULL OR zone = ?2)
                 GROUP BY label
                 ORDER BY MAX(julianday(last_seen)) DESC, label ASC LIMIT ?3",
            )?;
            stmt.query_map(
                params![filter.camera, filter.zone, limit as i64 + 1],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let truncated = summaries.len() > limit;
        let mut items = Vec::new();
        {
            let mut stmt = tx.prepare(
                "SELECT camera_id, zone, last_seen FROM observations
                 WHERE label = ?1 AND (?2 IS NULL OR camera_id = ?2) AND (?3 IS NULL OR zone = ?3)
                 ORDER BY julianday(last_seen) DESC, id DESC",
            )?;
            for (label, observation_count) in summaries.into_iter().take(limit) {
                let mut cameras = std::collections::BTreeSet::new();
                let mut zones = std::collections::BTreeSet::new();
                let mut last_seen = None;
                let rows = stmt.query_map(params![label, filter.camera, filter.zone], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        parse_ts_value(row.get(2)?)?,
                    ))
                })?;
                for row in rows {
                    let (camera, zone, seen) = row?;
                    last_seen.get_or_insert(seen);
                    cameras.insert(camera);
                    zones.insert(zone);
                }
                let last_seen = last_seen
                    .ok_or_else(|| StoreError::InvalidInput("inventory row disappeared".into()))?;
                items.push(RecordedItemSummary {
                    label,
                    observation_count,
                    last_seen,
                    cameras: cameras.into_iter().collect(),
                    zones: zones.into_iter().collect(),
                });
            }
        }
        tx.commit()?;
        Ok(RecordedItemsPage { items, truncated })
    }

    // ---- geometry and lifecycle evidence -----------------------------------

    /// Insert or update one bounded geometry sample. Prefer the typed input API;
    /// this argument-heavy form remains for callers compiled against G1's first
    /// draft.
    #[allow(clippy::too_many_arguments)]
    pub fn record_geometry(
        &self,
        observation_id: i64,
        sample_kind: &str,
        captured_at: DateTime<Utc>,
        bbox: [f32; 4],
        frame_width: Option<u32>,
        frame_height: Option<u32>,
        zone: &str,
        source: &str,
        confidence: Option<f32>,
        snapshot_ref: Option<&str>,
    ) -> Result<i64> {
        let input = GeometryInput {
            sample_kind: sample_kind.to_owned(),
            captured_at,
            bbox,
            frame_width,
            frame_height,
            zone: zone.to_owned(),
            source: source.to_owned(),
            confidence,
            snapshot_ref: snapshot_ref.map(str::to_owned),
        };
        self.validate_geometry(&input)?;
        if sample_kind == "first" || sample_kind == "last" {
            let tx = self.conn.unchecked_transaction()?;
            let (id, _) = upsert_bounded_geometry(&tx, observation_id, &input, false)?;
            tx.commit()?;
            return Ok(id);
        }
        if sample_kind == "last" {
            self.conn.execute(
                "INSERT INTO observation_geometry
                     (observation_id, sample_kind, captured_at, x0, y0, x1, y1,
                      frame_width, frame_height, zone, source, confidence, snapshot_ref)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
                 ON CONFLICT(observation_id, sample_kind) DO UPDATE SET
                     captured_at = excluded.captured_at,
                     x0 = excluded.x0, y0 = excluded.y0,
                     x1 = excluded.x1, y1 = excluded.y1,
                     frame_width = excluded.frame_width,
                     frame_height = excluded.frame_height,
                     zone = excluded.zone, source = excluded.source,
                     confidence = excluded.confidence,
                     snapshot_ref = excluded.snapshot_ref",
                params![
                    observation_id,
                    sample_kind,
                    captured_at.to_rfc3339(),
                    bbox[0],
                    bbox[1],
                    bbox[2],
                    bbox[3],
                    frame_width.map(i64::from),
                    frame_height.map(i64::from),
                    zone,
                    source,
                    confidence,
                    snapshot_ref,
                ],
            )?;
        } else {
            self.conn.execute(
                "INSERT OR IGNORE INTO observation_geometry
                     (observation_id, sample_kind, captured_at, x0, y0, x1, y1,
                      frame_width, frame_height, zone, source, confidence, snapshot_ref)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                params![
                    observation_id,
                    sample_kind,
                    captured_at.to_rfc3339(),
                    bbox[0],
                    bbox[1],
                    bbox[2],
                    bbox[3],
                    frame_width.map(i64::from),
                    frame_height.map(i64::from),
                    zone,
                    source,
                    confidence,
                    snapshot_ref,
                ],
            )?;
        }
        Ok(self.conn.query_row(
            "SELECT id FROM observation_geometry
             WHERE observation_id = ?1 AND sample_kind = ?2",
            params![observation_id, sample_kind],
            |r| r.get(0),
        )?)
    }

    pub fn record_geometry_input(&self, observation_id: i64, input: &GeometryInput) -> Result<i64> {
        self.record_geometry(
            observation_id,
            &input.sample_kind,
            input.captured_at,
            input.bbox,
            input.frame_width,
            input.frame_height,
            &input.zone,
            &input.source,
            input.confidence,
            input.snapshot_ref.as_deref(),
        )
    }

    fn validate_geometry(&self, input: &GeometryInput) -> Result<()> {
        let [x0, y0, x1, y1] = input.bbox;
        if input.sample_kind.is_empty() || !input.captured_at.timestamp_nanos_opt().is_some() {
            return Err(StoreError::InvalidInput(
                "geometry kind/timestamp is invalid".into(),
            ));
        }
        if ![x0, y0, x1, y1].iter().all(|value| value.is_finite())
            || x0 < 0.0
            || y0 < 0.0
            || x1 < x0
            || y1 < y0
            || input.frame_width == Some(0)
            || input.frame_height == Some(0)
            || input.frame_width.is_some_and(|w| x1 > w as f32)
            || input.frame_height.is_some_and(|h| y1 > h as f32)
        {
            return Err(StoreError::InvalidInput(
                "geometry bbox/frame dimensions are invalid".into(),
            ));
        }
        if input
            .confidence
            .is_some_and(|value| !value.is_finite() || !(0.0..=1.0).contains(&value))
        {
            return Err(StoreError::InvalidInput(
                "geometry confidence is invalid".into(),
            ));
        }
        if input.zone.is_empty() || input.source.is_empty() {
            return Err(StoreError::InvalidInput(
                "geometry zone/source must be non-empty".into(),
            ));
        }
        Ok(())
    }

    /// Copy a geometry sample into an immutable event-boundary sample. The
    /// caller uses the returned id in the event row; the original `last` sample
    /// may continue to move without changing historical evidence.
    pub fn copy_geometry(&self, geometry_id: i64, sample_kind: &str) -> Result<i64> {
        let tx = self.conn.unchecked_transaction()?;
        let id = copy_geometry_tx(&tx, geometry_id, sample_kind)?;
        tx.commit()?;
        Ok(id)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record_depth(
        &self,
        geometry_id: i64,
        provider: &str,
        mode: &str,
        relation_to_camera: &str,
        relative_score: Option<f32>,
        quality: &str,
        model_ref: Option<&str>,
    ) -> Result<i64> {
        if provider.is_empty()
            || mode != "relative"
            || !["nearer", "middle", "farther", "unknown"].contains(&relation_to_camera)
            || !["good", "degraded", "invalid"].contains(&quality)
            || relative_score
                .is_some_and(|value| !value.is_finite() || !(0.0..=1.0).contains(&value))
        {
            return Err(StoreError::InvalidInput("relative depth is invalid".into()));
        }
        let exists: Option<i64> = self
            .conn
            .query_row(
                "SELECT id FROM observation_geometry WHERE id = ?1",
                params![geometry_id],
                |row| row.get(0),
            )
            .optional()?;
        if exists.is_none() {
            return Err(StoreError::InvalidInput(
                "depth geometry does not exist".into(),
            ));
        }
        self.conn.execute(
            "INSERT INTO depth_evidence
                 (geometry_id, provider, response_id, mode, relation_to_camera, relative_score,
                  quality, model_ref)
             VALUES (?1, ?2, '', ?3, ?4, ?5, ?6, ?7)",
            params![
                geometry_id,
                provider,
                mode,
                relation_to_camera,
                relative_score,
                quality,
                model_ref,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Freeze the referenced geometry and append its lifecycle event atomically.
    /// The copy is immutable even if the source is the moving `last` sample.
    pub fn freeze_and_append_observation_event(
        &self,
        event: &ObservationEventInput,
    ) -> Result<i64> {
        validate_event(event)?;
        let tx = self.conn.unchecked_transaction()?;
        let geometry_id = if let Some(source_id) = event.geometry_id {
            let observation_id: i64 = tx
                .query_row(
                    "SELECT observation_id FROM observation_geometry WHERE id = ?1",
                    params![source_id],
                    |r| r.get(0),
                )
                .optional()?
                .ok_or_else(|| StoreError::InvalidInput("event geometry does not exist".into()))?;
            if observation_id != event.observation_id {
                return Err(StoreError::InvalidInput(
                    "event geometry belongs to another observation".into(),
                ));
            }
            let suffix: i64 = tx.query_row(
                "SELECT COALESCE(MAX(id), 0) + 1 FROM observation_geometry",
                [],
                |r| r.get(0),
            )?;
            let kind = format!(
                "event_{}_{}_{suffix}",
                event.event_type,
                event.occurred_at.timestamp_nanos_opt().unwrap_or_default()
            );
            let copied = copy_geometry_tx(&tx, source_id, &kind)?;
            let source_kind: String = tx.query_row(
                "SELECT sample_kind FROM observation_geometry WHERE id=?1",
                params![source_id],
                |row| row.get(0),
            )?;
            if source_kind.starts_with("ingress_last_") {
                tx.execute(
                    "DELETE FROM observation_geometry WHERE id=?1",
                    params![source_id],
                )?;
            }
            copied
        } else {
            0
        };
        let id = insert_event(&tx, event, (geometry_id != 0).then_some(geometry_id))?;
        tx.commit()?;
        Ok(id)
    }

    pub fn append_observation_event(&self, event: &ObservationEventInput) -> Result<i64> {
        validate_event(event)?;
        let geometry_id = self.validate_event_references(event)?;
        insert_event(&self.conn, event, geometry_id)
    }

    fn validate_event_references(&self, event: &ObservationEventInput) -> Result<Option<i64>> {
        let identity: (String, String, String) = self
            .conn
            .query_row(
                "SELECT camera_id, zone, label FROM observations WHERE id = ?1",
                params![event.observation_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?
            .ok_or_else(|| StoreError::InvalidInput("event observation does not exist".into()))?;
        if identity
            != (
                event.camera_id.clone(),
                event.zone.clone(),
                event.label.clone(),
            )
        {
            return Err(StoreError::InvalidInput(
                "event identity does not match observation".into(),
            ));
        }
        if let Some(geometry_id) = event.geometry_id {
            let owner: Option<i64> = self
                .conn
                .query_row(
                    "SELECT observation_id FROM observation_geometry WHERE id = ?1",
                    params![geometry_id],
                    |row| row.get(0),
                )
                .optional()?;
            if owner != Some(event.observation_id) {
                return Err(StoreError::InvalidInput(
                    "event geometry does not belong to observation".into(),
                ));
            }
            Ok(Some(geometry_id))
        } else {
            Ok(None)
        }
    }

    pub fn geometry_by_id(&self, geometry_id: i64) -> Result<Option<ObservationGeometry>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, observation_id, sample_kind, captured_at,
                        x0, y0, x1, y1, frame_width, frame_height,
                        zone, source, confidence, snapshot_ref
                 FROM observation_geometry WHERE id = ?1",
                params![geometry_id],
                geometry_row,
            )
            .optional()?)
    }

    pub fn geometry_for_observation(
        &self,
        observation_id: i64,
    ) -> Result<Vec<ObservationGeometry>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, observation_id, sample_kind, captured_at,
                    x0, y0, x1, y1, frame_width, frame_height,
                    zone, source, confidence, snapshot_ref
             FROM observation_geometry
             WHERE observation_id = ?1
             ORDER BY id",
        )?;
        let rows = stmt.query_map(params![observation_id], geometry_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn scene_events(
        &self,
        camera_id: Option<&str>,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<crate::model::SceneEvent>> {
        let events = match camera_id {
            Some(camera) => self.events_for_camera(camera, since, until, limit)?,
            None => self.events_between(since, until, limit)?,
        };
        events
            .into_iter()
            .map(|event| {
                let geometry = event
                    .geometry_id
                    .map(|id| self.geometry_by_id(id))
                    .transpose()?
                    .flatten();
                let depth = geometry
                    .as_ref()
                    .map(|g| self.depth_for_geometry(g.id))
                    .transpose()?
                    .unwrap_or_default();
                Ok(crate::model::SceneEvent {
                    event,
                    geometry,
                    depth,
                })
            })
            .collect()
    }

    pub fn scene_events_for_camera(
        &self,
        camera_id: &str,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<crate::model::SceneEvent>> {
        let events = self.events_for_camera(camera_id, since, until, limit)?;
        events
            .into_iter()
            .map(|event| {
                let geometry = event
                    .geometry_id
                    .map(|id| self.geometry_by_id(id))
                    .transpose()?
                    .flatten();
                let depth = geometry
                    .as_ref()
                    .map(|g| self.depth_for_geometry(g.id))
                    .transpose()?
                    .unwrap_or_default();
                Ok(crate::model::SceneEvent {
                    event,
                    geometry,
                    depth,
                })
            })
            .collect()
    }

    pub fn events_between(
        &self,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<ObservationEvent>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, observation_id, camera_id, zone, label, event_type,
                    occurred_at, noticed_at, source, session_id, reason,
                    hits, seen_for_s, geometry_id
             FROM observation_events
             WHERE occurred_at >= ?1 AND occurred_at <= ?2
             ORDER BY occurred_at ASC, id ASC
             LIMIT ?3",
        )?;
        let rows = stmt.query_map(
            params![
                since.to_rfc3339(),
                until.to_rfc3339(),
                limit.clamp(1, MAX_LIMIT)
            ],
            event_row,
        )?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn events_for_camera(
        &self,
        camera_id: &str,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<ObservationEvent>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, observation_id, camera_id, zone, label, event_type,
                    occurred_at, noticed_at, source, session_id, reason,
                    hits, seen_for_s, geometry_id
             FROM observation_events
             WHERE camera_id = ?1 AND occurred_at >= ?2 AND occurred_at <= ?3
             ORDER BY occurred_at ASC, id ASC
             LIMIT ?4",
        )?;
        let rows = stmt.query_map(
            params![
                camera_id,
                since.to_rfc3339(),
                until.to_rfc3339(),
                limit.clamp(1, MAX_LIMIT)
            ],
            event_row,
        )?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn depth_for_geometry(&self, geometry_id: i64) -> Result<Vec<DepthEvidence>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, geometry_id, provider, frame_id, response_id, prompt_version,
                    backend_version, mode, relation_to_camera, relative_score, quality,
                    model_ref, value_m, uncertainty_m, valid_fraction, coordinate_frame,
                    calibration_ref
             FROM depth_evidence WHERE geometry_id = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map(params![geometry_id], depth_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    // ---- retention (docs/resident-ingest.md §7) ---------------------------

    /// Delete every observation last seen before `cutoff`, returning the ids
    /// that went away. The caller deletes those snapshots **after** this
    /// returns: a JPEG left behind is an orphan, a row pointing at a deleted
    /// file is a broken observation, and only one of those is acceptable.
    ///
    /// One statement (`DELETE ... RETURNING`), so the returned ids are exactly
    /// the rows the delete removed. The iterator must be drained -- SQLite
    /// applies the deletion as the rows are stepped.
    ///
    /// `last_seen` is stored as RFC3339 text and compared as text, which orders
    /// correctly for the fixed-offset format chrono writes; the dedup window in
    /// [`Store::record_sighting`] already relies on the same property.
    pub fn expire_observations(&self, cutoff: DateTime<Utc>) -> Result<Vec<i64>> {
        let mut stmt = self
            .conn
            .prepare("DELETE FROM observations WHERE last_seen < ?1 RETURNING id")?;
        let rows = stmt.query_map(params![cutoff.to_rfc3339()], |r| r.get::<_, i64>(0))?;
        let mut ids = Vec::new();
        for row in rows {
            ids.push(row?);
        }
        Ok(ids)
    }

    /// Every observation id, ascending. Read once up front so the orphan scan
    /// can decide what a snapshot file may belong to without holding the store
    /// lock while it walks the directory (§3: no heavy work under the lock).
    pub fn observation_ids(&self) -> Result<Vec<i64>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id FROM observations ORDER BY id")?;
        let rows = stmt.query_map([], |r| r.get::<_, i64>(0))?;
        let mut ids = Vec::new();
        for row in rows {
            ids.push(row?);
        }
        Ok(ids)
    }

    pub fn count_observations(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM observations", [], |r| r.get(0))?)
    }

    /// Rewrite the database file, reclaiming the space deleted rows left
    /// behind. Deliberately never automatic: it holds an exclusive lock for as
    /// long as the rewrite takes, so the daemon only does it on request
    /// (`item-ingest --maintenance`, §7).
    pub fn vacuum(&self) -> Result<()> {
        self.conn.execute_batch("VACUUM")?;
        Ok(())
    }
}

fn validate_depth(input: &DepthInput) -> Result<()> {
    if input.provider.is_empty()
        || input.frame_id.is_empty()
        || input.response_id.is_empty()
        || input.prompt_version.is_empty()
        || input.backend_version.is_empty()
        || input.mode != "relative"
        || !["nearer", "middle", "farther", "unknown"].contains(&input.relation_to_camera.as_str())
        || !["good", "degraded", "invalid"].contains(&input.quality.as_str())
        || input
            .relative_score
            .is_some_and(|v| !v.is_finite() || !(0.0..=1.0).contains(&v))
        || input.value_m.is_some()
        || input.uncertainty_m.is_some()
        || input.valid_fraction.is_some()
        || input.coordinate_frame.is_some()
        || input.calibration_ref.is_some()
    {
        return Err(StoreError::InvalidInput(
            "relative depth/provenance is invalid".into(),
        ));
    }
    Ok(())
}

fn validate_event(event: &ObservationEventInput) -> Result<()> {
    if event.observation_id <= 0
        || event.camera_id.is_empty()
        || event.zone.is_empty()
        || event.label.is_empty()
        || event.source.is_empty()
        || !["appeared", "disappeared", "observed"].contains(&event.event_type.as_str())
        || event.occurred_at > event.noticed_at
        || event.hits <= 0
        || event.seen_for_s.is_some_and(|v| !v.is_finite() || v < 0.0)
        || event.occurred_at.timestamp_nanos_opt().is_none()
        || event.noticed_at.timestamp_nanos_opt().is_none()
    {
        return Err(StoreError::InvalidInput(
            "event identity/timestamps/values are invalid".into(),
        ));
    }
    Ok(())
}

fn upsert_bounded_geometry(
    conn: &Connection,
    observation_id: i64,
    input: &GeometryInput,
    preserve_late_last: bool,
) -> Result<(i64, bool)> {
    if input.sample_kind != "first" && input.sample_kind != "last" {
        return Err(StoreError::InvalidInput(
            "only first/last geometry can be recorded".into(),
        ));
    }
    let existing: Option<(i64, String)> = conn.query_row(
        "SELECT id,captured_at FROM observation_geometry WHERE observation_id=?1 AND sample_kind=?2",
        params![observation_id, input.sample_kind],
        |r| Ok((r.get(0)?,r.get(1)?)),
    ).optional()?;
    if let Some((id, timestamp)) = &existing {
        let timestamp = parse_ts_value(timestamp.clone())?;
        let changes_bound = if input.sample_kind == "first" {
            input.captured_at < timestamp
        } else {
            input.captured_at > timestamp
        };
        if !changes_bound {
            if preserve_late_last && input.sample_kind == "last" {
                let late_kind = format!(
                    "ingress_last_{}",
                    input.captured_at.timestamp_nanos_opt().unwrap_or_default()
                );
                conn.execute(
                    "INSERT OR IGNORE INTO observation_geometry
                     (observation_id,sample_kind,captured_at,x0,y0,x1,y1,frame_width,frame_height,zone,source,confidence,snapshot_ref)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
                    params![observation_id, late_kind, input.captured_at.to_rfc3339(), input.bbox[0], input.bbox[1], input.bbox[2], input.bbox[3], input.frame_width, input.frame_height, input.zone, input.source, input.confidence, input.snapshot_ref],
                )?;
                let ingress_id: i64 = conn.query_row(
                    "SELECT id FROM observation_geometry WHERE observation_id=?1 AND sample_kind=?2",
                    params![observation_id, late_kind],
                    |row| row.get(0),
                )?;
                return Ok((ingress_id, true));
            }
            return Ok((*id, false));
        }
    }
    conn.execute(
        "INSERT INTO observation_geometry
         (observation_id,sample_kind,captured_at,x0,y0,x1,y1,frame_width,frame_height,zone,source,confidence,snapshot_ref)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)
         ON CONFLICT(observation_id,sample_kind) DO UPDATE SET
         captured_at=excluded.captured_at,x0=excluded.x0,y0=excluded.y0,x1=excluded.x1,y1=excluded.y1,
         frame_width=excluded.frame_width,frame_height=excluded.frame_height,zone=excluded.zone,
         source=excluded.source,confidence=excluded.confidence,snapshot_ref=excluded.snapshot_ref",
        params![observation_id,input.sample_kind,input.captured_at.to_rfc3339(),
            input.bbox[0],input.bbox[1],input.bbox[2],input.bbox[3],input.frame_width,input.frame_height,
            input.zone,input.source,input.confidence,input.snapshot_ref],
    )?;
    let id = match existing {
        Some((id, _)) => id,
        None => conn.last_insert_rowid(),
    };
    conn.execute(
        "DELETE FROM depth_evidence WHERE geometry_id=?1",
        params![id],
    )?;
    Ok((id, true))
}

fn insert_depth(conn: &Connection, geometry_id: i64, input: &DepthInput) -> Result<i64> {
    conn.execute(
        "INSERT INTO depth_evidence
         (geometry_id,provider,frame_id,response_id,prompt_version,backend_version,mode,
          relation_to_camera,relative_score,quality,model_ref,value_m,uncertainty_m,
          valid_fraction,coordinate_frame,calibration_ref)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)",
        params![
            geometry_id,
            input.provider,
            input.frame_id,
            input.response_id,
            input.prompt_version,
            input.backend_version,
            input.mode,
            input.relation_to_camera,
            input.relative_score,
            input.quality,
            input.model_ref,
            input.value_m,
            input.uncertainty_m,
            input.valid_fraction,
            input.coordinate_frame,
            input.calibration_ref,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

fn copy_geometry_tx(conn: &Connection, source_id: i64, kind: &str) -> Result<i64> {
    if kind == "first" || kind == "last" || kind.is_empty() {
        return Err(StoreError::InvalidInput(
            "event copy requires a non-moving sample kind".into(),
        ));
    }
    let source_observation: i64 = conn.query_row(
        "SELECT observation_id FROM observation_geometry WHERE id=?1",
        params![source_id],
        |r| r.get(0),
    )?;
    let existing: Option<i64> = conn
        .query_row(
            "SELECT id FROM observation_geometry WHERE observation_id=?1 AND sample_kind=?2",
            params![source_observation, kind],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(id) = existing {
        return Ok(id);
    }
    conn.execute(
        "INSERT INTO observation_geometry
         (observation_id,sample_kind,captured_at,x0,y0,x1,y1,frame_width,frame_height,zone,source,confidence,snapshot_ref)
         SELECT observation_id,?2,captured_at,x0,y0,x1,y1,frame_width,frame_height,zone,source,confidence,snapshot_ref
         FROM observation_geometry WHERE id=?1",params![source_id,kind],
    )?;
    let id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO depth_evidence
         (geometry_id,provider,frame_id,response_id,prompt_version,backend_version,mode,
          relation_to_camera,relative_score,quality,model_ref,value_m,uncertainty_m,
          valid_fraction,coordinate_frame,calibration_ref)
         SELECT ?2,provider,frame_id,response_id,prompt_version,backend_version,mode,
          relation_to_camera,relative_score,quality,model_ref,value_m,uncertainty_m,
          valid_fraction,coordinate_frame,calibration_ref
         FROM depth_evidence WHERE geometry_id=?1",
        params![source_id, id],
    )?;
    Ok(id)
}

fn insert_event(
    conn: &Connection,
    event: &ObservationEventInput,
    geometry_id: Option<i64>,
) -> Result<i64> {
    let identity: (String, String, String) = conn.query_row(
        "SELECT camera_id,zone,label FROM observations WHERE id=?1",
        params![event.observation_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    if identity
        != (
            event.camera_id.clone(),
            event.zone.clone(),
            event.label.clone(),
        )
    {
        return Err(StoreError::InvalidInput(
            "event identity does not match observation".into(),
        ));
    }
    conn.execute(
        "INSERT INTO observation_events
         (observation_id,camera_id,zone,label,event_type,occurred_at,noticed_at,source,session_id,reason,hits,seen_for_s,geometry_id)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
        params![event.observation_id,event.camera_id,event.zone,event.label,event.event_type,
            event.occurred_at.to_rfc3339(),event.noticed_at.to_rfc3339(),event.source,event.session_id,
            event.reason,event.hits,event.seen_for_s,geometry_id],
    )?;
    Ok(conn.last_insert_rowid())
}

fn parse_ts_value(s: String) -> rusqlite::Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(&s)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })
}

fn observation_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Observation> {
    Ok(Observation {
        id: r.get(0)?,
        camera_id: r.get(1)?,
        zone: r.get(2)?,
        label: r.get(3)?,
        first_seen: parse_ts_value(r.get(4)?)?,
        last_seen: parse_ts_value(r.get(5)?)?,
        hit_count: r.get(6)?,
        sample_snapshot: r.get(7)?,
    })
}

fn geometry_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<ObservationGeometry> {
    Ok(ObservationGeometry {
        id: r.get(0)?,
        observation_id: r.get(1)?,
        sample_kind: r.get(2)?,
        captured_at: parse_ts_value(r.get(3)?)?,
        bbox: [r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?],
        frame_width: r.get::<_, Option<i64>>(8)?.map(|v| v as u32),
        frame_height: r.get::<_, Option<i64>>(9)?.map(|v| v as u32),
        zone: r.get(10)?,
        source: r.get(11)?,
        confidence: r.get(12)?,
        snapshot_ref: r.get(13)?,
    })
}

fn event_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<ObservationEvent> {
    Ok(ObservationEvent {
        id: r.get(0)?,
        observation_id: r.get(1)?,
        camera_id: r.get(2)?,
        zone: r.get(3)?,
        label: r.get(4)?,
        event_type: r.get(5)?,
        occurred_at: parse_ts_value(r.get(6)?)?,
        noticed_at: parse_ts_value(r.get(7)?)?,
        source: r.get(8)?,
        session_id: r.get(9)?,
        reason: r.get(10)?,
        hits: r.get(11)?,
        seen_for_s: r.get(12)?,
        geometry_id: r.get(13)?,
    })
}

fn depth_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<DepthEvidence> {
    Ok(DepthEvidence {
        id: r.get(0)?,
        geometry_id: r.get(1)?,
        provider: r.get(2)?,
        frame_id: r.get(3)?,
        response_id: r.get(4)?,
        prompt_version: r.get(5)?,
        backend_version: r.get(6)?,
        mode: r.get(7)?,
        relation_to_camera: r.get(8)?,
        relative_score: r.get(9)?,
        quality: r.get(10)?,
        model_ref: r.get(11)?,
        value_m: r.get(12)?,
        uncertainty_m: r.get(13)?,
        valid_fraction: r.get(14)?,
        coordinate_frame: r.get(15)?,
        calibration_ref: r.get(16)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn ts(mins: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 2, 10, mins, 0).unwrap()
    }

    fn insert_query_row(
        store: &Store,
        camera: &str,
        zone: &str,
        label: &str,
        seen: &str,
        hits: i64,
    ) -> i64 {
        store
            .conn
            .execute(
                "INSERT INTO observations (camera_id,zone,label,first_seen,last_seen,hit_count)
             VALUES (?1,?2,?3,?4,?4,?5)",
                params![camera, zone, label, seen, hits],
            )
            .unwrap();
        store.conn.last_insert_rowid()
    }

    #[test]
    fn database_identity_is_opt_in_stable_and_database_specific() {
        let first = Store::in_memory().unwrap();
        let second = Store::in_memory().unwrap();
        assert_eq!(first.database_identity().unwrap(), None);
        let schema = first.schema_version().unwrap();
        let id = first.ensure_database_identity().unwrap();
        assert_eq!(id.len(), 64);
        assert_eq!(first.ensure_database_identity().unwrap(), id);
        assert_eq!(first.database_identity().unwrap(), Some(id.clone()));
        assert_ne!(second.ensure_database_identity().unwrap(), id);
        assert_eq!(first.schema_version().unwrap(), schema);
        first.conn.pragma_update(None, "query_only", true).unwrap();
        assert_eq!(first.database_identity().unwrap(), Some(id));
        assert!(first.ensure_database_identity().is_err());
    }

    #[test]
    fn filtered_observations_apply_filters_before_limit_and_use_time_offsets() {
        let store = Store::in_memory().unwrap();
        let target = insert_query_row(
            &store,
            "desk",
            "tray",
            "keys_50%",
            "2026-09-02T11:00:00+01:00",
            3,
        );
        insert_query_row(
            &store,
            "desk",
            "elsewhere",
            "keys_50%",
            "2026-09-02T12:00:00Z",
            1,
        );
        insert_query_row(
            &store,
            "other",
            "tray",
            "keys_50%",
            "2026-09-02T13:00:00Z",
            1,
        );
        insert_query_row(
            &store,
            "desk",
            "tray",
            "keysx50zz",
            "2026-09-02T14:00:00Z",
            1,
        );
        let filter = ObservationFilter {
            label: Some("_50%".into()),
            camera: Some("desk".into()),
            zone: Some("tray".into()),
            since: Some(ts(0)),
            until: Some(ts(0)),
            limit: 1,
        };
        let rows = store.observations_filtered(&filter).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, target);
        assert_eq!(rows[0].last_seen, ts(0));
        assert!(
            store
                .observations_filtered(&ObservationFilter {
                    since: Some(ts(1)),
                    until: Some(ts(0)),
                    ..Default::default()
                })
                .is_err()
        );
    }

    #[test]
    fn basic_observation_lookup_is_stable_and_does_not_hide_errors() {
        let store = Store::in_memory().unwrap();
        let first = insert_query_row(&store, "desk", "tray", "keys", "2026-09-02T10:00:00Z", 3);
        let second = insert_query_row(
            &store,
            "desk",
            "tray",
            "cup",
            "2026-09-02T10:00:00+00:00",
            1,
        );
        assert_eq!(
            store
                .recent(None, 2)
                .unwrap()
                .iter()
                .map(|o| o.id)
                .collect::<Vec<_>>(),
            vec![second, first]
        );
        let row = store.observation_by_id(first).unwrap().unwrap();
        assert_eq!(row.label, "keys");
        assert_eq!(row.hit_count, 3);
        assert!(store.observation_by_id(999).unwrap().is_none());
        assert!(store.observation_by_id(0).is_err());
        store.conn.execute_batch("DROP TABLE observations").unwrap();
        assert!(store.observation_by_id(first).is_err());
    }

    #[test]
    fn inventory_aggregates_all_rows_then_limits_labels_and_counts_observations() {
        let store = Store::in_memory().unwrap();
        for _ in 0..510 {
            insert_query_row(&store, "desk", "tray", "keys", "2026-09-02T10:00:00Z", 10);
        }
        insert_query_row(&store, "hall", "shelf", "keys", "2026-09-02T10:01:00Z", 100);
        insert_query_row(&store, "hall", "shelf", "Keys", "2026-09-02T10:00:00Z", 1);
        let page = store
            .recorded_items(&RecordedItemsFilter {
                limit: 1,
                ..Default::default()
            })
            .unwrap();
        assert!(page.truncated);
        assert_eq!(page.items[0].label, "keys");
        assert_eq!(page.items[0].observation_count, 511);
        assert_eq!(page.items[0].cameras, vec!["desk", "hall"]);
        assert_eq!(page.items[0].zones, vec!["shelf", "tray"]);
        assert_eq!(page.items[0].last_seen, ts(1));
        let page = store
            .recorded_items(&RecordedItemsFilter {
                camera: Some("desk".into()),
                zone: Some("tray".into()),
                limit: 1,
            })
            .unwrap();
        assert!(!page.truncated);
        assert_eq!(page.items[0].observation_count, 510);
        let all = store
            .recorded_items(&RecordedItemsFilter::default())
            .unwrap();
        assert_eq!(all.items.len(), 2, "case variants remain original labels");
        let empty = store
            .recorded_items(&RecordedItemsFilter {
                camera: Some("missing".into()),
                ..Default::default()
            })
            .unwrap();
        assert!(empty.items.is_empty());
        assert!(!empty.truncated);
    }

    #[test]
    fn sightings_merge_within_window_then_split_after() {
        let s = Store::in_memory().unwrap();
        let (id1, new1, hits1) = s
            .record_sighting(
                "cam1",
                "entrance",
                "keys",
                ts(0),
                None,
                DEFAULT_DEDUP_WINDOW,
            )
            .unwrap();
        assert!(new1);
        assert_eq!(hits1, 1, "a fresh row has seen the object once");
        // +1 min: merged into the same observation
        let (id2, new2, hits2) = s
            .record_sighting(
                "cam1",
                "entrance",
                "keys",
                ts(1),
                None,
                DEFAULT_DEDUP_WINDOW,
            )
            .unwrap();
        assert!(!new2);
        assert_eq!(id1, id2);
        assert_eq!(
            hits2, 2,
            "the merge reports the count it just wrote (§6's `hits`)"
        );
        // +10 min: outside the 5-min window -> a new observation
        let (id3, new3, hits3) = s
            .record_sighting(
                "cam1",
                "entrance",
                "keys",
                ts(11),
                None,
                DEFAULT_DEDUP_WINDOW,
            )
            .unwrap();
        assert!(new3);
        assert_ne!(id1, id3);
        assert_eq!(hits3, 1, "the new row starts its own count");

        let obs = s.recent(Some("keys"), 10).unwrap();
        assert_eq!(obs.len(), 2);
        assert_eq!(obs[0].hit_count, 1);
        assert_eq!(obs[1].hit_count, 2); // merged row, newest first
    }

    #[test]
    fn zones_from_region_rects() {
        let s = Store::in_memory().unwrap();
        s.upsert_region("cam1", "entrance", [0.0, 0.0, 200.0, 400.0])
            .unwrap();
        assert_eq!(
            s.zone_for_point("cam1", (100.0, 300.0)).unwrap(),
            "entrance"
        );
        assert_eq!(s.zone_for_point("cam1", (999.0, 999.0)).unwrap(), "frame");
    }

    /// The rule the config documents ("first match wins, alphabetical"): an
    /// overlapping pair of zones must resolve the same way on every run, not
    /// whichever row the query planner happens to return first.
    #[test]
    fn overlapping_zones_resolve_by_name() {
        let s = Store::in_memory().unwrap();
        // Seeded out of order on purpose: insertion order must not decide.
        s.upsert_region("cam1", "sofa", [0.0, 0.0, 100.0, 100.0])
            .unwrap();
        s.upsert_region("cam1", "desk", [0.0, 0.0, 100.0, 100.0])
            .unwrap();
        assert_eq!(s.zone_for_point("cam1", (50.0, 50.0)).unwrap(), "desk");
    }

    #[test]
    fn a_backdated_sighting_widens_the_span_instead_of_inverting_it() {
        let s = Store::in_memory().unwrap();
        // A sighting at 10:05, then one that arrives late for 10:04: the same
        // window, so they merge -- and the span must stay the right way round.
        let (first, _, _) = s
            .record_sighting("cam1", "desk", "keys", ts(5), None, DEFAULT_DEDUP_WINDOW)
            .unwrap();
        let (second, is_new, hits) = s
            .record_sighting("cam1", "desk", "keys", ts(4), None, DEFAULT_DEDUP_WINDOW)
            .unwrap();
        assert_eq!(second, first, "within the window: one observation");
        assert!(!is_new);
        assert_eq!(hits, 2);

        let row = s.recent(Some("keys"), 10).unwrap().remove(0);
        assert_eq!(row.first_seen, ts(4), "the span widens to the earlier hit");
        assert_eq!(row.last_seen, ts(5));
        assert!(
            row.last_seen >= row.first_seen,
            "a late sighting must not drag last_seen behind first_seen: {row:?}"
        );
    }

    /// The other half of the same rule: the window is symmetric, so a sighting
    /// that is *far* older than an existing row opens its own row instead of
    /// stretching that one across the gap (a Frigate event whose `frame_time`
    /// is two days old must not become part of today's observation).
    #[test]
    fn a_sighting_far_outside_the_window_opens_its_own_row() {
        let s = Store::in_memory().unwrap();
        let today = ts(10);
        let two_days_ago = today - chrono::Duration::days(2);
        let (fresh, _, _) = s
            .record_sighting("cam1", "desk", "keys", today, None, DEFAULT_DEDUP_WINDOW)
            .unwrap();

        let (old, is_new, hits) = s
            .record_sighting(
                "cam1",
                "desk",
                "keys",
                two_days_ago,
                None,
                DEFAULT_DEDUP_WINDOW,
            )
            .unwrap();
        assert!(is_new, "outside the window on the far side: a separate row");
        assert_ne!(old, fresh);
        assert_eq!(hits, 1);

        let rows = s.recent(Some("keys"), 10).unwrap();
        assert_eq!(rows.len(), 2);
        let backdated = rows.iter().find(|o| o.id == old).unwrap();
        assert_eq!(backdated.first_seen, two_days_ago);
        assert_eq!(
            backdated.last_seen, two_days_ago,
            "its span is its own sighting, not today's"
        );
        assert_eq!(
            rows[0].id, fresh,
            "the newest last_seen still sorts first: {rows:?}"
        );
    }

    /// A database fault must not be answered with an INSERT: `.ok()` used to
    /// turn any error into "no existing row", which quietly forks an
    /// observation (new id, restarted hit count) instead of reporting.
    #[test]
    fn a_broken_query_is_an_error_not_a_new_row() {
        let s = Store::in_memory().unwrap();
        s.conn
            .execute_batch("DROP TABLE observations")
            .expect("the test breaks the table on purpose");
        assert!(
            s.record_sighting("cam1", "desk", "keys", ts(0), None, DEFAULT_DEDUP_WINDOW)
                .is_err(),
            "a missing table must surface, not silently insert"
        );
        assert!(s.snapshot_path(1).is_err());
    }

    /// The label filter is text, not a pattern: `%` and `_` are ordinary
    /// characters. And `limit` cannot be used to dump the whole table.
    #[test]
    fn the_label_filter_is_literal_and_the_limit_is_bounded() {
        let s = Store::in_memory().unwrap();
        for (index, label) in ["50% tint", "50_cent", "plain"].iter().enumerate() {
            s.record_sighting(
                "cam1",
                "desk",
                label,
                ts(index as u32),
                None,
                DEFAULT_DEDUP_WINDOW,
            )
            .unwrap();
        }
        assert_eq!(s.recent(Some("%"), 10).unwrap().len(), 1, "literal % only");
        assert_eq!(
            s.recent(Some("_"), 10).unwrap().len(),
            1,
            "literal _, not a single-character wildcard"
        );
        assert_eq!(s.recent(Some("50%"), 10).unwrap().len(), 1);
        assert_eq!(
            s.recent(Some("pla"), 10).unwrap().len(),
            1,
            "a plain substring still matches"
        );

        assert_eq!(
            s.recent(None, -1).unwrap().len(),
            1,
            "SQLite reads LIMIT -1 as 'no limit'; the clamp makes it one row"
        );
    }

    #[test]
    fn snapshot_attached_after_insert() {
        let s = Store::in_memory().unwrap();
        let (id, _, _) = s
            .record_sighting("cam1", "desk", "keys", ts(0), None, DEFAULT_DEDUP_WINDOW)
            .unwrap();
        assert!(
            s.recent(Some("keys"), 1).unwrap()[0]
                .sample_snapshot
                .is_none()
        );
        s.set_sample_snapshot(id, "snapshots/1.jpg").unwrap();
        assert_eq!(
            s.recent(Some("keys"), 1).unwrap()[0]
                .sample_snapshot
                .as_deref(),
            Some("snapshots/1.jpg")
        );
    }

    #[test]
    fn the_sweep_deletes_only_what_is_past_the_cutoff_and_names_what_it_took() {
        let s = Store::in_memory().unwrap();
        let (old_id, _, _) = s
            .record_sighting(
                "cam1",
                "desk",
                "keys",
                ts(0),
                Some("snapshots/1.jpg"),
                DEFAULT_DEDUP_WINDOW,
            )
            .unwrap();
        let (new_id, _, _) = s
            .record_sighting("cam1", "desk", "cup", ts(10), None, DEFAULT_DEDUP_WINDOW)
            .unwrap();

        let expired = s.expire_observations(ts(5)).unwrap();
        assert_eq!(
            expired,
            vec![old_id],
            "the sweep must report exactly the rows it deleted, so the caller can delete those files"
        );
        assert_eq!(s.observation_ids().unwrap(), vec![new_id]);
        assert_eq!(s.count_observations().unwrap(), 1);
        assert_eq!(s.recent(None, 10).unwrap()[0].label, "cup");
    }

    #[test]
    fn a_row_exactly_at_the_cutoff_survives() {
        let s = Store::in_memory().unwrap();
        let (id, _, _) = s
            .record_sighting("cam1", "desk", "keys", ts(5), None, DEFAULT_DEDUP_WINDOW)
            .unwrap();
        assert!(
            s.expire_observations(ts(5)).unwrap().is_empty(),
            "the window is `last_seen < cutoff`, so a row on the boundary is kept"
        );
        assert_eq!(s.observation_ids().unwrap(), vec![id]);
    }

    #[test]
    fn vacuum_leaves_the_surviving_rows_alone() {
        let s = Store::in_memory().unwrap();
        s.record_sighting("cam1", "desk", "cup", ts(0), None, DEFAULT_DEDUP_WINDOW)
            .unwrap();
        let (fresh, _, _) = s
            .record_sighting("cam1", "desk", "keys", ts(10), None, DEFAULT_DEDUP_WINDOW)
            .unwrap();
        s.expire_observations(ts(5)).unwrap();

        s.vacuum().unwrap();
        assert_eq!(s.count_observations().unwrap(), 1);
        assert_eq!(s.observation_ids().unwrap(), vec![fresh]);
        assert_eq!(s.recent(None, 10).unwrap()[0].label, "keys");
    }

    fn geometry(kind: &str, at: DateTime<Utc>) -> GeometryInput {
        GeometryInput {
            sample_kind: kind.into(),
            captured_at: at,
            bbox: [10.0, 20.0, 60.0, 80.0],
            frame_width: Some(100),
            frame_height: Some(100),
            zone: "desk".into(),
            source: "test".into(),
            confidence: Some(0.9),
            snapshot_ref: None,
        }
    }

    fn depth() -> DepthInput {
        DepthInput {
            provider: "test-depth".into(),
            frame_id: "frame-1".into(),
            response_id: "response-1".into(),
            prompt_version: "prompt-1".into(),
            backend_version: "backend-1".into(),
            mode: "relative".into(),
            relation_to_camera: "nearer".into(),
            relative_score: Some(0.8),
            quality: "good".into(),
            model_ref: Some("model".into()),
            value_m: None,
            uncertainty_m: None,
            valid_fraction: None,
            coordinate_frame: None,
            calibration_ref: None,
        }
    }

    fn event_for(obs: i64, geometry_id: Option<i64>) -> ObservationEventInput {
        ObservationEventInput {
            observation_id: obs,
            camera_id: "cam1".into(),
            zone: "desk".into(),
            label: "keys".into(),
            event_type: "observed".into(),
            occurred_at: ts(1),
            noticed_at: ts(2),
            source: "test".into(),
            session_id: Some("session".into()),
            reason: "test".into(),
            hits: 1,
            seen_for_s: None,
            geometry_id,
        }
    }

    #[test]
    fn typed_sighting_persists_first_last_and_depth_atomically() {
        let s = Store::in_memory().unwrap();
        let input = SightingInput {
            camera_id: "cam1".into(),
            zone: "desk".into(),
            label: "keys".into(),
            seen_at: ts(1),
            snapshot: None,
            window_seconds: 300,
            geometry: Some(geometry("last", ts(1))),
            depth: Some(depth()),
        };
        let (obs, is_new, hits, last) = s.record_sighting_with_geometry(&input).unwrap();
        assert!(is_new);
        assert_eq!(hits, 1);
        let last = last.unwrap();
        let geometries = s.geometry_for_observation(obs).unwrap();
        assert_eq!(
            geometries
                .iter()
                .map(|g| g.sample_kind.as_str())
                .collect::<Vec<_>>(),
            vec!["first", "last"]
        );
        assert_eq!(s.depth_for_geometry(last).unwrap().len(), 1);
    }

    #[test]
    fn invalid_depth_rolls_back_observation_and_geometry() {
        let s = Store::in_memory().unwrap();
        let mut bad = depth();
        bad.relation_to_camera = "near".into();
        let input = SightingInput {
            camera_id: "cam1".into(),
            zone: "desk".into(),
            label: "keys".into(),
            seen_at: ts(1),
            snapshot: None,
            window_seconds: 300,
            geometry: Some(geometry("last", ts(1))),
            depth: Some(bad),
        };
        assert!(s.record_sighting_with_geometry(&input).is_err());
        assert_eq!(s.count_observations().unwrap(), 0);
    }

    #[test]
    fn observed_event_checks_identity_and_geometry_owner() {
        let s = Store::in_memory().unwrap();
        let (obs, _, _) = s
            .record_sighting("cam1", "desk", "keys", ts(1), None, DEFAULT_DEDUP_WINDOW)
            .unwrap();
        let geometry_id = s
            .record_geometry(
                obs,
                "last",
                ts(1),
                [1.0, 1.0, 2.0, 2.0],
                None,
                None,
                "desk",
                "test",
                None,
                None,
            )
            .unwrap();
        let id = s
            .append_observation_event(&event_for(obs, Some(geometry_id)))
            .unwrap();
        assert!(id > 0);
        let mut mismatch = event_for(obs, Some(geometry_id));
        mismatch.label = "wallet".into();
        assert!(s.append_observation_event(&mismatch).is_err());
        let (other, _, _) = s
            .record_sighting("cam1", "desk", "wallet", ts(1), None, DEFAULT_DEDUP_WINDOW)
            .unwrap();
        let mut foreign = event_for(other, Some(geometry_id));
        foreign.label = "wallet".into();
        assert!(s.append_observation_event(&foreign).is_err());
    }

    #[test]
    fn copied_geometry_keeps_depth_evidence() {
        let s = Store::in_memory().unwrap();
        let input = SightingInput {
            camera_id: "cam1".into(),
            zone: "desk".into(),
            label: "keys".into(),
            seen_at: ts(1),
            snapshot: None,
            window_seconds: 300,
            geometry: Some(geometry("last", ts(1))),
            depth: Some(depth()),
        };
        let (_, _, _, last) = s.record_sighting_with_geometry(&input).unwrap();
        let copied = s.copy_geometry(last.unwrap(), "event_observed_1").unwrap();
        assert_eq!(s.depth_for_geometry(copied).unwrap().len(), 1);
    }
}
