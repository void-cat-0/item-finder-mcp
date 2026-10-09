//! Bounded, non-migrating evidence reads in one pinned SQLite snapshot.
use crate::{
    model::*,
    store::{Result, Store, StoreError},
};
use chrono::{DateTime, Utc};
use rusqlite::{
    OptionalExtension, Row, Transaction, params,
    types::{Type, ValueRef},
};
use serde::Serialize;
use std::{
    collections::BTreeSet,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

#[cfg(test)]
#[path = "evidence_tests.rs"]
mod tests;

pub const MAX_FIELD_BYTES: usize = 4096;
pub const MAX_ROW_BYTES: usize = 32768;
#[derive(Clone, Debug)]
pub struct ReadControl {
    pub deadline: Instant,
    pub cancelled: Arc<AtomicBool>,
}
impl ReadControl {
    pub fn new(deadline: Instant, cancelled: Arc<AtomicBool>) -> Self {
        Self {
            deadline,
            cancelled,
        }
    }
    pub fn check(&self) -> Result<()> {
        if self.cancelled.load(Ordering::Relaxed) {
            Err(StoreError::ReadCancelled)
        } else if Instant::now() >= self.deadline {
            Err(StoreError::ReadTimeout)
        } else {
            Ok(())
        }
    }
}
#[derive(Clone, Debug, Serialize)]
pub struct EvidenceBranch {
    pub availability: &'static str,
    pub reason: Option<&'static str>,
    pub returned_count: usize,
    pub truncated: bool,
}
impl EvidenceBranch {
    pub fn rows(count: usize, truncated: bool) -> Self {
        Self {
            availability: if truncated {
                "partial"
            } else if count == 0 {
                "not_available"
            } else {
                "available"
            },
            reason: if truncated {
                Some("count_limit")
            } else if count == 0 {
                Some("no_rows")
            } else {
                None
            },
            returned_count: count,
            truncated,
        }
    }
    fn unavailable(reason: &'static str) -> Self {
        Self {
            availability: "schema_unavailable",
            reason: Some(reason),
            returned_count: 0,
            truncated: false,
        }
    }
    pub fn usable(&self) -> bool {
        self.availability != "schema_unavailable"
    }
}
#[derive(Clone, Debug)]
pub struct EvidenceCapabilities {
    pub version: i64,
    pub geometry: EvidenceBranch,
    pub events: EvidenceBranch,
    pub depth: EvidenceBranch,
    pub depth_provenance_complete: bool,
    depth_columns: BTreeSet<String>,
}
#[derive(Clone, Copy, Debug)]
pub struct ContextLimits {
    pub max_geometry: usize,
    pub max_events: usize,
    pub max_depth: usize,
}
impl Default for ContextLimits {
    fn default() -> Self {
        Self {
            max_geometry: 8,
            max_events: 16,
            max_depth: 16,
        }
    }
}
#[derive(Clone, Debug)]
pub struct ContextBranches {
    pub geometry: EvidenceBranch,
    pub events: EvidenceBranch,
    pub depth: EvidenceBranch,
}
#[derive(Clone, Debug)]
pub struct ContextRead {
    pub database_id: Option<String>,
    pub as_of: DateTime<Utc>,
    pub observation: Observation,
    pub geometry: Vec<ObservationGeometry>,
    pub events: Vec<ObservationEvent>,
    pub depth: Vec<DepthEvidence>,
    pub branches: ContextBranches,
}
#[derive(Clone, Debug)]
pub struct SceneRead {
    pub events: Vec<SceneEvent>,
    pub scan_complete: bool,
    pub provenance_complete: bool,
}
#[derive(Clone, Debug)]
pub struct SceneFilter<'a> {
    pub camera: &'a str,
    pub zone: &'a str,
    pub session: &'a str,
    pub since: DateTime<Utc>,
    pub until: DateTime<Utc>,
}
#[derive(Clone, Debug, Serialize)]
pub struct LifecycleCheck {
    pub result: &'static str,
    pub event_id: Option<i64>,
    pub checked_until: DateTime<Utc>,
}

pub struct ReadSnapshot<'a> {
    tx: Transaction<'a>,
    pub as_of: DateTime<Utc>,
    pub database_id: Option<String>,
    pub capabilities: EvidenceCapabilities,
    control: ReadControl,
    previous_length_limit: i32,
}
impl Store {
    /// Open without migrations or a preliminary PRAGMA query. SQLite lock waits
    /// and scans use the request budget from the first possible statement.
    /// OS-level file-open latency itself cannot be interrupted by SQLite hooks.
    pub fn open_read_only_controlled(
        path: impl AsRef<std::path::Path>,
        control: ReadControl,
    ) -> Result<Self> {
        control.check()?;
        let conn = rusqlite::Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        control.check()?;
        conn.busy_timeout(
            control
                .deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(100)),
        )?;
        let progress = control.clone();
        conn.progress_handler(1000, Some(move || progress.check().is_err()))?;
        control.check()?;
        Ok(Self { conn })
    }
    pub fn read_snapshot(&self, control: ReadControl) -> Result<ReadSnapshot<'_>> {
        control.check()?;
        self.conn.busy_timeout(
            control
                .deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(100)),
        )?;
        let progress_control = control.clone();
        self.conn
            .progress_handler(1000, Some(move || progress_control.check().is_err()))?;
        self.conn.create_scalar_function(
            "evidence_instant",
            1,
            rusqlite::functions::FunctionFlags::SQLITE_UTF8
                | rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC,
            |ctx| {
                let value = ctx.get_raw(0).as_str()?;
                let parsed = DateTime::parse_from_rfc3339(value)
                    .map_err(|e| rusqlite::Error::UserFunctionError(Box::new(e)))?;
                // Sortable binary key: signed seconds biased to u64 plus exact
                // nanoseconds. Unlike julianday/unixepoch or nanosecond i64, this
                // preserves chrono's entire range and sub-microsecond precision.
                let mut key = Vec::with_capacity(12);
                key.extend_from_slice(&((parsed.timestamp() as u64) ^ (1u64 << 63)).to_be_bytes());
                key.extend_from_slice(&parsed.timestamp_subsec_nanos().to_be_bytes());
                Ok(key)
            },
        )?;
        let previous_length_limit = self.conn.set_limit(
            rusqlite::limits::Limit::SQLITE_LIMIT_LENGTH,
            MAX_ROW_BYTES as i32,
        )?;
        let result = (|| {
            let tx = self.conn.unchecked_transaction()?;
            // The first read pins the WAL snapshot before capability/identity reads.
            let version: i64 = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
            // Reading sqlite_schema explicitly establishes the data snapshot;
            // PRAGMA user_version alone is not the consistency guarantee.
            let _: i64 = tx.query_row("SELECT count(*) FROM sqlite_schema", [], |r| r.get(0))?;
            let capabilities = capabilities(&tx, version)?;
            let database_id =
                if table_columns(&tx, "asset_database_identity")?.contains("database_id") {
                    tx.query_row(
                        "SELECT database_id FROM asset_database_identity WHERE singleton=1",
                        [],
                        |r| bounded_text(r, 0),
                    )
                    .optional()?
                } else {
                    None
                };
            if database_id.as_ref().is_some_and(|s| {
                s.len() != 64
                    || !s
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            }) {
                return Err(StoreError::InvalidInput("invalid database identity".into()));
            }
            control.check()?;
            Ok(ReadSnapshot {
                tx,
                as_of: Utc::now(),
                database_id,
                capabilities,
                control: control.clone(),
                previous_length_limit,
            })
        })();
        if result.is_err() {
            let _ = self.conn.progress_handler(0, None::<fn() -> bool>);
            let _ = self.conn.set_limit(
                rusqlite::limits::Limit::SQLITE_LIMIT_LENGTH,
                previous_length_limit,
            );
        }
        control.check()?;
        result.map_err(read_error)
    }
}
impl Drop for ReadSnapshot<'_> {
    fn drop(&mut self) {
        let _ = self.tx.progress_handler(0, None::<fn() -> bool>);
        let _ = self.tx.set_limit(
            rusqlite::limits::Limit::SQLITE_LIMIT_LENGTH,
            self.previous_length_limit,
        );
    }
}
impl ReadSnapshot<'_> {
    pub fn check(&self) -> Result<()> {
        self.control.check()
    }
    pub fn checked<T>(&self, value: Result<T>) -> Result<T> {
        self.check()?;
        value.map_err(read_error)
    }
    pub fn observation(&self, id: i64, guard: Option<&str>) -> Result<Observation> {
        self.check()?;
        if id <= 0
            || guard
                .is_some_and(|s| s.is_empty() || s.len() > 1024 || s.chars().any(char::is_control))
        {
            return Err(StoreError::InvalidInput(
                "invalid observation selector".into(),
            ));
        }
        let row = self
            .checked(
                self.tx
                    .query_row(
                        &format!("SELECT {OBS} FROM observations WHERE id=?1"),
                        [id],
                        observation_row,
                    )
                    .optional()
                    .map_err(Into::into),
            )?
            .ok_or(StoreError::ObservationNotFound)?;
        if guard.is_some_and(|s| s != row.camera_id) {
            return Err(StoreError::CameraMismatch);
        }
        Ok(row)
    }
    pub fn observation_context(
        &self,
        id: i64,
        guard: Option<&str>,
        limits: ContextLimits,
    ) -> Result<ContextRead> {
        if !(1..=32).contains(&limits.max_geometry)
            || !(1..=64).contains(&limits.max_events)
            || !(1..=64).contains(&limits.max_depth)
        {
            return Err(StoreError::InvalidInput("invalid context limits".into()));
        }
        let observation = self.observation(id, guard)?;
        let mut geometry = vec![];
        let mut events = vec![];
        let mut depth = vec![];
        let mut branches = ContextBranches {
            geometry: self.capabilities.geometry.clone(),
            events: self.capabilities.events.clone(),
            depth: self.capabilities.depth.clone(),
        };
        if branches.geometry.usable() {
            geometry = self.query_geometry(&format!("SELECT {GEO} FROM observation_geometry WHERE observation_id=?1 ORDER BY evidence_instant(captured_at) DESC,id DESC LIMIT ?2"), params![id, (limits.max_geometry+1) as i64])?;
            let truncated = geometry.len() > limits.max_geometry;
            geometry.truncate(limits.max_geometry);
            branches.geometry = EvidenceBranch::rows(geometry.len(), truncated);
        }
        if branches.events.usable() {
            events = self.query_events(&format!("SELECT {EVENT} FROM observation_events WHERE observation_id=?1 AND camera_id=?2 ORDER BY evidence_instant(occurred_at) DESC,id DESC LIMIT ?3"), params![id, observation.camera_id, (limits.max_events+1) as i64])?;
            let truncated = events.len() > limits.max_events;
            events.truncate(limits.max_events);
            branches.events = EvidenceBranch::rows(events.len(), truncated);
        }
        if branches.depth.usable() {
            // Each query is SQL bounded; total allocation never exceeds max_depth+1.
            for geo in &geometry {
                let remaining = limits.max_depth + 1 - depth.len();
                if remaining == 0 {
                    break;
                }
                depth.extend(self.depth(geo.id, remaining)?);
            }
            let truncated = depth.len() > limits.max_depth;
            depth.truncate(limits.max_depth);
            branches.depth = EvidenceBranch::rows(depth.len(), truncated);
            if !self.capabilities.depth_provenance_complete && !truncated {
                branches.depth.availability = "partial";
                branches.depth.reason = Some("optional_provenance_missing");
            }
        }
        Ok(ContextRead {
            database_id: self.database_id.clone(),
            as_of: self.as_of,
            observation,
            geometry,
            events,
            depth,
            branches,
        })
    }
    pub fn latest_departure_for_observation(
        &self,
        observation: &Observation,
    ) -> Result<Option<ObservationEvent>> {
        let mut rows = self.query_events(&format!("SELECT {EVENT} FROM observation_events WHERE observation_id=?1 AND event_type='disappeared' AND reason='missed_gap' AND evidence_instant(occurred_at)<=evidence_instant(?2) ORDER BY evidence_instant(occurred_at) DESC,id DESC LIMIT 1"),params![observation.id,self.as_of.to_rfc3339()])?;
        Ok(rows.pop())
    }
    pub fn geometry(&self, id: i64) -> Result<Option<ObservationGeometry>> {
        Ok(self
            .query_geometry(
                &format!("SELECT {GEO} FROM observation_geometry WHERE id=?1 LIMIT 1"),
                [id],
            )?
            .pop())
    }
    pub fn depth(&self, id: i64, limit: usize) -> Result<Vec<DepthEvidence>> {
        self.depth_ordered(id, limit, true)
    }
    fn depth_ordered(&self, id: i64, limit: usize, descending: bool) -> Result<Vec<DepthEvidence>> {
        self.check()?;
        if !self.capabilities.depth.usable() {
            return Ok(vec![]);
        }
        let optional = |name: &str| {
            if self.capabilities.depth_columns.contains(name) {
                name.to_owned()
            } else {
                "NULL".into()
            }
        };
        let sql = format!(
            "SELECT id,geometry_id,provider,{},{},{},{},mode,relation_to_camera,relative_score,quality,model_ref,value_m,uncertainty_m,valid_fraction,coordinate_frame,calibration_ref FROM depth_evidence WHERE geometry_id=?1 ORDER BY id {} LIMIT ?2",
            optional("frame_id"),
            optional("response_id"),
            optional("prompt_version"),
            optional("backend_version"),
            if descending { "DESC" } else { "ASC" }
        );
        self.checked((|| {
            let mut stmt = self.tx.prepare(&sql)?;
            let rows = stmt
                .query_map(params![id, limit.min(65) as i64], depth_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })())
    }
    pub fn scene_window(&self, filter: SceneFilter<'_>) -> Result<SceneRead> {
        let events=self.query_events(&format!("SELECT {EVENT} FROM observation_events WHERE camera_id=?1 AND zone=?2 AND session_id=?3 AND evidence_instant(occurred_at)>=evidence_instant(?4) AND evidence_instant(occurred_at)<=evidence_instant(?5) ORDER BY evidence_instant(occurred_at),id LIMIT 501"),params![filter.camera,filter.zone,filter.session,filter.since.to_rfc3339(),filter.until.to_rfc3339()])?;
        let mut complete = events.len() <= 500;
        let mut out = Vec::with_capacity(events.len().min(500));
        for event in events.into_iter().take(500) {
            self.check()?;
            let geometry = if let Some(id) = event.geometry_id {
                self.geometry(id)?
            } else {
                None
            };
            let mut depth = if let Some(geo) = &geometry {
                self.depth_ordered(geo.id, 5, false)?
            } else {
                vec![]
            };
            if depth.len() > 4 {
                complete = false;
                depth.truncate(4);
            }
            out.push(SceneEvent {
                event,
                geometry,
                depth,
            });
        }
        Ok(SceneRead {
            events: out,
            scan_complete: complete,
            provenance_complete: self.capabilities.depth_provenance_complete,
        })
    }
    pub fn same_label_appeared_after(
        &self,
        anchor: &ObservationEvent,
    ) -> Result<Option<ObservationEvent>> {
        Ok(self.query_events(&format!("SELECT {EVENT} FROM observation_events WHERE camera_id=?1 AND session_id=?2 AND label=?3 COLLATE NOCASE AND event_type='appeared' AND evidence_instant(occurred_at)>evidence_instant(?4) AND evidence_instant(occurred_at)<=evidence_instant(?5) ORDER BY evidence_instant(occurred_at),id LIMIT 1"),params![anchor.camera_id,anchor.session_id,anchor.label,anchor.occurred_at.to_rfc3339(),self.as_of.to_rfc3339()])?.pop())
    }
    pub fn cover_lifecycle_after(
        &self,
        appearance: &ObservationEvent,
        anchor: &ObservationEvent,
    ) -> Result<LifecycleCheck> {
        let malformed=self.query_events(&format!("SELECT {EVENT} FROM observation_events WHERE observation_id=?1 AND session_id=?2 AND evidence_instant(occurred_at)>evidence_instant(?3) AND evidence_instant(occurred_at)<=evidence_instant(?4) AND (camera_id<>?5 OR zone<>?6 OR label<>?7 OR evidence_instant(noticed_at)<evidence_instant(occurred_at) OR evidence_instant(noticed_at)>evidence_instant(?4) OR hits<=0 OR seen_for_s<0) LIMIT 1"),params![appearance.observation_id,appearance.session_id,anchor.occurred_at.to_rfc3339(),self.as_of.to_rfc3339(),appearance.camera_id,appearance.zone,appearance.label])?;
        if let Some(event) = malformed.first() {
            return Ok(LifecycleCheck {
                result: "cover_evidence_unavailable",
                event_id: Some(event.id),
                checked_until: self.as_of,
            });
        }
        let rows=self.query_events(&format!("SELECT {EVENT} FROM observation_events WHERE observation_id=?1 AND camera_id=?2 AND zone=?3 AND session_id=?4 AND event_type IN ('disappeared','appeared') AND evidence_instant(occurred_at)>evidence_instant(?5) AND evidence_instant(occurred_at)<=evidence_instant(?6) AND (evidence_instant(occurred_at)>evidence_instant(?7) OR (evidence_instant(occurred_at)=evidence_instant(?7) AND id>?8)) ORDER BY CASE event_type WHEN 'disappeared' THEN 0 ELSE 1 END,evidence_instant(occurred_at),id LIMIT 1"),params![appearance.observation_id,appearance.camera_id,appearance.zone,appearance.session_id,anchor.occurred_at.to_rfc3339(),self.as_of.to_rfc3339(),appearance.occurred_at.to_rfc3339(),appearance.id])?;
        let event = rows.first();
        Ok(LifecycleCheck {
            result: match event {
                None => "no_later_lifecycle_conflict",
                Some(e) if e.event_type == "disappeared" => "cover_lifecycle_ended",
                _ => "cover_lifecycle_discontinuous",
            },
            event_id: event.map(|e| e.id),
            checked_until: self.as_of,
        })
    }
    fn query_geometry(
        &self,
        sql: &str,
        params: impl rusqlite::Params,
    ) -> Result<Vec<ObservationGeometry>> {
        self.check()?;
        self.checked((|| {
            let mut s = self.tx.prepare(sql)?;
            Ok(s.query_map(params, geometry_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })())
    }
    fn query_events(
        &self,
        sql: &str,
        params: impl rusqlite::Params,
    ) -> Result<Vec<ObservationEvent>> {
        self.check()?;
        self.checked((|| {
            let mut s = self.tx.prepare(sql)?;
            Ok(s.query_map(params, event_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })())
    }
}
fn read_error(e: StoreError) -> StoreError {
    match e {
        StoreError::Sql(rusqlite::Error::SqliteFailure(ref code, _))
            if code.code == rusqlite::ErrorCode::TooBig =>
        {
            StoreError::PayloadTooLarge
        }
        e => e,
    }
}
fn table_columns(tx: &Transaction<'_>, table: &str) -> Result<BTreeSet<String>> {
    // Only library constants reach this quoted identifier.
    let mut s = tx.prepare(&format!("PRAGMA table_info(\"{table}\")"))?;
    Ok(s.query_map([], |r| bounded_text(r, 1))?
        .collect::<rusqlite::Result<BTreeSet<_>>>()?)
}
fn capabilities(tx: &Transaction<'_>, version: i64) -> Result<EvidenceCapabilities> {
    let g = table_columns(tx, "observation_geometry")?;
    let e = table_columns(tx, "observation_events")?;
    let d = table_columns(tx, "depth_evidence")?;
    let branch = |cols: &BTreeSet<String>, required: &[&str]| {
        if !(1..=2).contains(&version) {
            EvidenceBranch::unavailable("unsupported_schema_version")
        } else if cols.is_empty() {
            EvidenceBranch::unavailable("table_missing")
        } else if version != 2 {
            EvidenceBranch::unavailable("unsupported_schema_version")
        } else if required.iter().any(|c| !cols.contains(*c)) {
            EvidenceBranch::unavailable("required_columns_missing")
        } else {
            EvidenceBranch::rows(0, false)
        }
    };
    let geometry = branch(
        &g,
        &[
            "id",
            "observation_id",
            "sample_kind",
            "captured_at",
            "x0",
            "y0",
            "x1",
            "y1",
            "frame_width",
            "frame_height",
            "zone",
            "source",
            "confidence",
        ],
    );
    let events = branch(
        &e,
        &[
            "id",
            "observation_id",
            "camera_id",
            "zone",
            "label",
            "event_type",
            "occurred_at",
            "noticed_at",
            "source",
            "session_id",
            "reason",
            "hits",
            "seen_for_s",
            "geometry_id",
        ],
    );
    let depth = branch(
        &d,
        &[
            "id",
            "geometry_id",
            "provider",
            "mode",
            "relation_to_camera",
            "relative_score",
            "quality",
            "model_ref",
            "value_m",
            "uncertainty_m",
            "valid_fraction",
            "coordinate_frame",
            "calibration_ref",
        ],
    );
    let complete = depth.usable()
        && [
            "frame_id",
            "response_id",
            "prompt_version",
            "backend_version",
        ]
        .iter()
        .all(|s| d.contains(*s));
    Ok(EvidenceCapabilities {
        version,
        geometry,
        events,
        depth,
        depth_provenance_complete: complete,
        depth_columns: d,
    })
}
fn bounded_text(row: &Row<'_>, i: usize) -> rusqlite::Result<String> {
    let text = row.get_ref(i)?.as_str()?;
    if text.len() > MAX_FIELD_BYTES {
        return Err(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_TOOBIG),
            None,
        ));
    }
    Ok(text.to_owned())
}
fn optional_text(row: &Row<'_>, i: usize) -> rusqlite::Result<Option<String>> {
    if matches!(row.get_ref(i)?, ValueRef::Null) {
        Ok(None)
    } else {
        bounded_text(row, i).map(Some)
    }
}
fn timestamp(row: &Row<'_>, i: usize) -> rusqlite::Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(&bounded_text(row, i)?)
        .map(|t| t.with_timezone(&Utc))
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(i, Type::Text, Box::new(e)))
}
const OBS: &str = "id,camera_id,zone,label,first_seen,last_seen,hit_count";
const GEO: &str = "id,observation_id,sample_kind,captured_at,x0,y0,x1,y1,frame_width,frame_height,zone,source,confidence";
const EVENT: &str = "id,observation_id,camera_id,zone,label,event_type,occurred_at,noticed_at,source,session_id,reason,hits,seen_for_s,geometry_id";
fn observation_row(r: &Row<'_>) -> rusqlite::Result<Observation> {
    Ok(Observation {
        id: r.get(0)?,
        camera_id: bounded_text(r, 1)?,
        zone: bounded_text(r, 2)?,
        label: bounded_text(r, 3)?,
        first_seen: timestamp(r, 4)?,
        last_seen: timestamp(r, 5)?,
        hit_count: r.get(6)?,
        sample_snapshot: None,
    })
}
fn geometry_row(r: &Row<'_>) -> rusqlite::Result<ObservationGeometry> {
    Ok(ObservationGeometry {
        id: r.get(0)?,
        observation_id: r.get(1)?,
        sample_kind: bounded_text(r, 2)?,
        captured_at: timestamp(r, 3)?,
        bbox: [r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?],
        frame_width: r.get(8)?,
        frame_height: r.get(9)?,
        zone: bounded_text(r, 10)?,
        source: bounded_text(r, 11)?,
        confidence: r.get(12)?,
        snapshot_ref: None,
    })
}
fn event_row(r: &Row<'_>) -> rusqlite::Result<ObservationEvent> {
    Ok(ObservationEvent {
        id: r.get(0)?,
        observation_id: r.get(1)?,
        camera_id: bounded_text(r, 2)?,
        zone: bounded_text(r, 3)?,
        label: bounded_text(r, 4)?,
        event_type: bounded_text(r, 5)?,
        occurred_at: timestamp(r, 6)?,
        noticed_at: timestamp(r, 7)?,
        source: bounded_text(r, 8)?,
        session_id: optional_text(r, 9)?,
        reason: bounded_text(r, 10)?,
        hits: r.get(11)?,
        seen_for_s: r.get(12)?,
        geometry_id: r.get(13)?,
    })
}
fn depth_row(r: &Row<'_>) -> rusqlite::Result<DepthEvidence> {
    Ok(DepthEvidence {
        id: r.get(0)?,
        geometry_id: r.get(1)?,
        provider: bounded_text(r, 2)?,
        frame_id: optional_text(r, 3)?.unwrap_or_default(),
        response_id: optional_text(r, 4)?.unwrap_or_default(),
        prompt_version: optional_text(r, 5)?.unwrap_or_default(),
        backend_version: optional_text(r, 6)?.unwrap_or_default(),
        mode: bounded_text(r, 7)?,
        relation_to_camera: bounded_text(r, 8)?,
        relative_score: r.get(9)?,
        quality: bounded_text(r, 10)?,
        model_ref: optional_text(r, 11)?,
        value_m: r.get(12)?,
        uncertainty_m: r.get(13)?,
        valid_fraction: r.get(14)?,
        coordinate_frame: optional_text(r, 15)?,
        calibration_ref: optional_text(r, 16)?,
    })
}
