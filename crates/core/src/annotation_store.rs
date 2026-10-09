//! SQLite persistence for caller visual-inference annotations.
use chrono::{DateTime, Duration, Utc};
use rusqlite::{OptionalExtension, params, params_from_iter, types::Value};
use sha2::{Digest, Sha256};

use crate::annotations::*;
use crate::store::{Result, Store, StoreError};

fn ts(value: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map(|v| v.with_timezone(&Utc))
        .map_err(|e| StoreError::InvalidInput(format!("invalid annotation timestamp: {e}")))
}

fn json<T: serde::Serialize>(value: &T) -> Result<String> {
    serde_json::to_string(value).map_err(|e| StoreError::InvalidInput(e.to_string()))
}

fn uuid_like(seed: &str) -> String {
    let mut h = Sha256::new();
    h.update(seed.as_bytes());
    let b = h.finalize();
    format!(
        "{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-4{:x}{:x}-8{:x}{:x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0],
        b[1],
        b[2],
        b[3],
        b[4],
        b[5],
        b[6],
        b[7],
        b[8],
        b[9],
        b[10] & 0x0f,
        b[11] & 0x0f,
        b[12] & 0x0f,
        b[13] & 0x0f,
        b[14],
        b[15],
        b[16],
        b[17],
        b[18],
        b[19]
    )
}

fn cursor(created: &str, id: &str) -> String {
    let mut out = String::new();
    for b in created.bytes().chain(std::iter::once(0)).chain(id.bytes()) {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

fn parse_cursor(value: &str) -> Result<(String, String)> {
    if value.is_empty()
        || value.len() > 4096
        || !value.len().is_multiple_of(2)
        || !value.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err(StoreError::InvalidInput("invalid annotation cursor".into()));
    }
    let mut bytes = Vec::new();
    let raw = value.as_bytes();
    for index in (0..raw.len()).step_by(2) {
        let hi = (raw[index] as char)
            .to_digit(16)
            .ok_or_else(|| StoreError::InvalidInput("invalid annotation cursor".into()))?;
        let lo = (raw[index + 1] as char)
            .to_digit(16)
            .ok_or_else(|| StoreError::InvalidInput("invalid annotation cursor".into()))?;
        bytes.push((hi * 16 + lo) as u8);
    }
    let Some(nul) = bytes.iter().position(|b| *b == 0) else {
        return Err(StoreError::InvalidInput("invalid annotation cursor".into()));
    };
    let created = String::from_utf8(bytes[..nul].to_vec())
        .map_err(|_| StoreError::InvalidInput("invalid annotation cursor".into()))?;
    let id = String::from_utf8(bytes[nul + 1..].to_vec())
        .map_err(|_| StoreError::InvalidInput("invalid annotation cursor".into()))?;
    if created.is_empty() || id.is_empty() {
        return Err(StoreError::InvalidInput("invalid annotation cursor".into()));
    }
    Ok((created, id))
}

impl Store {
    /// Install annotation tables independently from the observation schema.
    pub fn ensure_annotation_schema(&self) -> Result<()> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS annotation_schema (version INTEGER PRIMARY KEY, installed_at TEXT NOT NULL);
             INSERT OR IGNORE INTO annotation_schema(version, installed_at) VALUES (1, datetime('now'));
             CREATE TABLE IF NOT EXISTS annotation_evidence (
               evidence_asset_id TEXT PRIMARY KEY, database_id TEXT NOT NULL,
               source_asset_id TEXT NOT NULL, source_sha256 TEXT NOT NULL,
               camera_id TEXT NOT NULL, capture_time TEXT NOT NULL, observation_id INTEGER,
               sha256 TEXT NOT NULL, bytes INTEGER NOT NULL, width INTEGER NOT NULL,
               height INTEGER NOT NULL, status TEXT NOT NULL, accepted_at TEXT NOT NULL,
               expires_at TEXT, active_refs INTEGER NOT NULL DEFAULT 0);
             CREATE INDEX IF NOT EXISTS idx_annotation_evidence_source
               ON annotation_evidence(database_id, source_asset_id, source_sha256, camera_id);
             CREATE TABLE IF NOT EXISTS annotation_sets (
               annotation_id TEXT PRIMARY KEY, database_id TEXT NOT NULL,
               source_asset_id TEXT NOT NULL, source_sha256 TEXT NOT NULL,
               camera_id TEXT NOT NULL, capture_time TEXT NOT NULL, observation_id INTEGER,
               evidence_asset_id TEXT NOT NULL, current_revision INTEGER NOT NULL,
               created_at TEXT NOT NULL, expires_at TEXT NOT NULL);
             CREATE INDEX IF NOT EXISTS idx_annotation_sets_list
               ON annotation_sets(database_id, created_at DESC, annotation_id ASC);
             CREATE INDEX IF NOT EXISTS idx_annotation_sets_source
               ON annotation_sets(database_id, camera_id, source_asset_id, created_at DESC);
             CREATE TABLE IF NOT EXISTS annotation_revisions (
               annotation_id TEXT NOT NULL REFERENCES annotation_sets(annotation_id) ON DELETE CASCADE,
               revision INTEGER NOT NULL, schema_version INTEGER NOT NULL,
               coordinate_system TEXT NOT NULL, producer_json TEXT NOT NULL,
               annotations_json TEXT NOT NULL, object_count INTEGER NOT NULL,
               payload_sha256 TEXT NOT NULL, request_id TEXT NOT NULL,
               persisted_at TEXT NOT NULL, metadata_bytes INTEGER NOT NULL,
               PRIMARY KEY(annotation_id, revision));
             CREATE INDEX IF NOT EXISTS idx_annotation_revisions_request
               ON annotation_revisions(request_id);
             CREATE TABLE IF NOT EXISTS annotation_requests (
               request_id TEXT PRIMARY KEY, database_id TEXT NOT NULL,
               payload_sha256 TEXT NOT NULL, receipt_json TEXT NOT NULL,
               annotation_id TEXT NOT NULL, revision INTEGER NOT NULL,
               created_at TEXT NOT NULL, tombstone_deadline TEXT NOT NULL);
             CREATE INDEX IF NOT EXISTS idx_annotation_requests_deadline
               ON annotation_requests(tombstone_deadline);",
        )?;
        Ok(())
    }

    pub fn annotation_receipt(
        &self,
        request_id: &str,
        payload_sha256: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<AnnotationReceipt>> {
        self.ensure_annotation_schema()?;
        let row: Option<(String, String, String)> = self.conn.query_row(
            "SELECT payload_sha256, receipt_json, tombstone_deadline FROM annotation_requests WHERE request_id=?1",
            [request_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        ).optional()?;
        let Some((hash, receipt_json, deadline)) = row else {
            return Ok(None);
        };
        if hash != payload_sha256 {
            return Err(StoreError::AnnotationIdempotencyConflict);
        }
        let receipt: AnnotationReceipt = serde_json::from_str(&receipt_json)
            .map_err(|e| StoreError::InvalidInput(e.to_string()))?;
        if ts(&receipt.expires_at)? <= now || ts(&deadline)? <= now {
            return Err(StoreError::AnnotationExpired);
        }
        Ok(Some(receipt))
    }

    pub fn register_annotation_evidence(
        &self,
        evidence: &AnnotationEvidenceRegistration,
        now: DateTime<Utc>,
    ) -> Result<()> {
        self.ensure_annotation_schema()?;
        if evidence.database_id.is_empty()
            || evidence.evidence_asset_id.is_empty()
            || evidence.capture_time.is_empty()
            || evidence.bytes == 0
            || evidence.width == 0
            || evidence.height == 0
        {
            return Err(StoreError::InvalidInput(
                "invalid annotation evidence metadata".into(),
            ));
        }
        ts(&evidence.capture_time)?;
        self.conn.execute(
            "INSERT INTO annotation_evidence
             (evidence_asset_id,database_id,source_asset_id,source_sha256,camera_id,capture_time,observation_id,sha256,bytes,width,height,status,accepted_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)
             ON CONFLICT(evidence_asset_id) DO UPDATE SET status=excluded.status, accepted_at=excluded.accepted_at
             WHERE annotation_evidence.sha256=excluded.sha256 AND annotation_evidence.bytes=excluded.bytes",
            params![evidence.evidence_asset_id,evidence.database_id,evidence.source_asset_id,evidence.source_sha256,
                evidence.camera_id,evidence.capture_time,evidence.observation_id,evidence.sha256,evidence.bytes as i64,
                evidence.width as i64,evidence.height as i64,evidence.status,
                if evidence.accepted_at.is_empty() { now.to_rfc3339() } else { evidence.accepted_at.clone() }],
        )?;
        Ok(())
    }

    fn evidence_registration(
        row: &rusqlite::Row<'_>,
    ) -> rusqlite::Result<(AnnotationEvidenceRegistration, String, i64)> {
        Ok((
            AnnotationEvidenceRegistration {
                database_id: row.get(0)?,
                evidence_asset_id: row.get(1)?,
                source_asset_id: row.get(2)?,
                source_sha256: row.get(3)?,
                camera_id: row.get(4)?,
                capture_time: row.get(5)?,
                observation_id: row.get(6)?,
                sha256: row.get(7)?,
                bytes: row.get::<_, i64>(8)? as u64,
                width: row.get::<_, i64>(9)? as u32,
                height: row.get::<_, i64>(10)? as u32,
                status: row.get(11)?,
                accepted_at: row.get(12)?,
            },
            row.get(13)?,
            row.get(14)?,
        ))
    }

    /// Return an active, integrity-registered evidence generation by opaque ID.
    pub fn annotation_evidence_by_id(
        &self,
        evidence_asset_id: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<(AnnotationEvidenceRegistration, String)>> {
        self.ensure_annotation_schema()?;
        self.conn.query_row(
            "SELECT database_id,evidence_asset_id,source_asset_id,source_sha256,camera_id,capture_time,observation_id,sha256,bytes,width,height,status,accepted_at,expires_at,active_refs
             FROM annotation_evidence WHERE evidence_asset_id=?1 AND active_refs>0 AND expires_at>?2",
            params![evidence_asset_id, now.to_rfc3339()], Self::evidence_registration,
        ).optional().map_err(StoreError::from).map(|row| row.map(|(registration, expiry, _)| (registration, expiry)))
    }

    /// Add one active annotation-set reference to an existing live evidence generation.
    pub fn retain_annotation_evidence(
        &self,
        evidence_asset_id: &str,
        expires_at: &str,
    ) -> Result<()> {
        self.ensure_annotation_schema()?;
        ts(expires_at)?;
        let changed = self.conn.execute(
            "UPDATE annotation_evidence SET active_refs=active_refs+1, expires_at=MAX(COALESCE(expires_at,?2),?2) WHERE evidence_asset_id=?1",
            params![evidence_asset_id, expires_at],
        )?;
        if changed == 0 {
            return Err(StoreError::AnnotationNotFound);
        }
        Ok(())
    }

    /// Find the latest live evidence generation for an exact source identity.
    pub fn reusable_annotation_evidence(
        &self,
        database_id: &str,
        source_asset_id: &str,
        source_sha256: &str,
        camera_id: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<(AnnotationEvidenceRegistration, String)>> {
        self.ensure_annotation_schema()?;
        self.conn.query_row(
            "SELECT database_id,evidence_asset_id,source_asset_id,source_sha256,camera_id,capture_time,observation_id,sha256,bytes,width,height,status,accepted_at,expires_at,active_refs
             FROM annotation_evidence WHERE database_id=?1 AND source_asset_id=?2 AND source_sha256=?3 AND camera_id=?4 AND active_refs>0 AND expires_at>?5
             ORDER BY expires_at DESC, evidence_asset_id ASC LIMIT 1",
            params![database_id, source_asset_id, source_sha256, camera_id, now.to_rfc3339()], Self::evidence_registration,
        ).optional().map_err(StoreError::from).map(|row| row.map(|(registration, expiry, _)| (registration, expiry)))
    }

    pub fn persist_annotation(
        &self,
        validated: &ValidatedAnnotationRequest,
        evidence: &AnnotationEvidenceRegistration,
        now: DateTime<Utc>,
        quota: &AnnotationQuota,
    ) -> Result<AnnotationReceipt> {
        self.ensure_annotation_schema()?;
        let req = &validated.request;
        if let Some(receipt) =
            self.annotation_receipt(&req.request_id, &validated.payload_sha256, now)?
        {
            return Ok(receipt);
        }
        if req.database_id != evidence.database_id
            || req.asset_id != evidence.source_asset_id
            || !req.sha256.eq_ignore_ascii_case(&evidence.source_sha256)
            || req.camera_id != evidence.camera_id
        {
            return Err(StoreError::InvalidInput(
                "annotation source/evidence mismatch".into(),
            ));
        }
        let tx = self.conn.unchecked_transaction()?;
        let current: Option<(u32, String, String)> = req.annotation_id.as_deref().map(|id| tx.query_row(
            "SELECT current_revision, expires_at, source_asset_id FROM annotation_sets WHERE annotation_id=?1 AND database_id=?2",
            params![id, req.database_id], |r| Ok((r.get::<_, i64>(0).map(|v| v as u32)?, r.get(1)?, r.get(2)?)),
        ).optional()).transpose()?.flatten();
        let (annotation_id, revision, expires_at, created_at) =
            if let Some((current, expiry, source)) = current {
                if source != req.asset_id {
                    return Err(StoreError::InvalidInput("source_mismatch".into()));
                }
                if current != req.expected_revision.unwrap_or(0) {
                    return Err(StoreError::AnnotationRevisionConflict {
                        current_revision: current,
                    });
                }
                if ts(&expiry)? <= now {
                    return Err(StoreError::AnnotationExpired);
                }
                (
                    req.annotation_id.clone().unwrap(),
                    current + 1,
                    expiry,
                    tx.query_row(
                        "SELECT created_at FROM annotation_sets WHERE annotation_id=?1",
                        [&req.annotation_id.clone().unwrap()],
                        |r| r.get(0),
                    )?,
                )
            } else {
                if req.annotation_id.is_some() {
                    return Err(StoreError::AnnotationNotFound);
                }
                let id = uuid_like(&format!(
                    "{}:{}:{}",
                    req.request_id,
                    now.to_rfc3339(),
                    req.sha256
                ));
                let expiry = (now + quota.retention).to_rfc3339();
                (id, 1, expiry, now.to_rfc3339())
            };
        let producer_json = json(&req.producer)?;
        let annotations_json = json(&req.annotations)?;
        let metadata_bytes = validated.canonical_json.len() as u64;
        let set_count: i64 = tx.query_row(
            "SELECT COUNT(*) FROM annotation_sets WHERE expires_at>?1",
            [now.to_rfc3339()],
            |r| r.get(0),
        )?;
        let revision_count: i64 =
            tx.query_row("SELECT COUNT(*) FROM annotation_revisions", [], |r| {
                r.get(0)
            })?;
        let metadata_count: i64 = tx.query_row(
            "SELECT COALESCE(SUM(metadata_bytes),0) FROM annotation_revisions",
            [],
            |r| r.get(0),
        )?;
        let evidence_count: i64 = tx.query_row(
            "SELECT COALESCE(SUM(bytes),0) FROM annotation_evidence WHERE active_refs>0",
            [],
            |r| r.get(0),
        )?;
        if req.annotation_id.is_none() && set_count as u64 >= quota.max_sets {
            return Err(StoreError::AnnotationQuota("max_sets".into()));
        }
        if revision_count as u64 >= quota.max_revisions
            || metadata_count as u64 + metadata_bytes > quota.max_metadata_bytes
            || evidence_count as u64 + evidence.bytes > quota.max_evidence_bytes
        {
            return Err(StoreError::AnnotationQuota("capacity".into()));
        }
        let evidence_expiry = (now + quota.retention).to_rfc3339();
        tx.execute(
            "INSERT INTO annotation_evidence(evidence_asset_id,database_id,source_asset_id,source_sha256,camera_id,capture_time,observation_id,sha256,bytes,width,height,status,accepted_at,expires_at,active_refs)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,0)
             ON CONFLICT(evidence_asset_id) DO UPDATE SET expires_at=MAX(COALESCE(annotation_evidence.expires_at,?14),?14),status=excluded.status
             WHERE annotation_evidence.sha256=excluded.sha256 AND annotation_evidence.bytes=excluded.bytes",
            params![evidence.evidence_asset_id,evidence.database_id,evidence.source_asset_id,evidence.source_sha256,evidence.camera_id,
                evidence.capture_time,evidence.observation_id,evidence.sha256,evidence.bytes as i64,evidence.width as i64,evidence.height as i64,
                evidence.status,if evidence.accepted_at.is_empty(){now.to_rfc3339()}else{evidence.accepted_at.clone()},evidence_expiry],
        )?;
        if req.annotation_id.is_none() {
            tx.execute("INSERT INTO annotation_sets(annotation_id,database_id,source_asset_id,source_sha256,camera_id,capture_time,observation_id,evidence_asset_id,current_revision,created_at,expires_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,1,?9,?10)", params![annotation_id,req.database_id,req.asset_id,req.sha256.to_ascii_lowercase(),req.camera_id,evidence.capture_time,evidence.observation_id,evidence.evidence_asset_id,created_at,expires_at])?;
            tx.execute("UPDATE annotation_evidence SET active_refs=active_refs+1 WHERE evidence_asset_id=?1", [&evidence.evidence_asset_id])?;
        } else {
            let old_evidence: String = tx.query_row(
                "SELECT evidence_asset_id FROM annotation_sets WHERE annotation_id=?1",
                [&annotation_id],
                |r| r.get(0),
            )?;
            tx.execute("UPDATE annotation_sets SET current_revision=?2,evidence_asset_id=?3 WHERE annotation_id=?1", params![annotation_id,revision,evidence.evidence_asset_id])?;
            if old_evidence != evidence.evidence_asset_id {
                tx.execute("UPDATE annotation_evidence SET active_refs=MAX(active_refs-1,0) WHERE evidence_asset_id=?1", [&old_evidence])?;
                tx.execute("UPDATE annotation_evidence SET active_refs=active_refs+1 WHERE evidence_asset_id=?1", [&evidence.evidence_asset_id])?;
            }
        }
        tx.execute("INSERT INTO annotation_revisions(annotation_id,revision,schema_version,coordinate_system,producer_json,annotations_json,object_count,payload_sha256,request_id,persisted_at,metadata_bytes) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)", params![annotation_id,revision,req.schema_version,req.coordinate_system,producer_json,annotations_json,req.annotations.len() as i64,validated.payload_sha256,req.request_id,now.to_rfc3339(),metadata_bytes as i64])?;
        let receipt = AnnotationReceipt {
            status: "persisted".into(),
            annotation_id: annotation_id.clone(),
            revision,
            request_id: req.request_id.clone(),
            database_id: req.database_id.clone(),
            source_asset_id: req.asset_id.clone(),
            source_sha256: req.sha256.to_ascii_lowercase(),
            camera_id: req.camera_id.clone(),
            evidence_asset_id: evidence.evidence_asset_id.clone(),
            object_count: req.annotations.len() as u32,
            provenance: ANNOTATION_PROVENANCE.into(),
            persisted_at: now.to_rfc3339(),
            expires_at: expires_at.clone(),
        };
        let receipt_json = json(&receipt)?;
        let tombstone = (ts(&expires_at)? + quota.tombstone_retention).to_rfc3339();
        tx.execute("INSERT INTO annotation_requests(request_id,database_id,payload_sha256,receipt_json,annotation_id,revision,created_at,tombstone_deadline) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)", params![req.request_id,req.database_id,validated.payload_sha256,receipt_json,annotation_id,revision,now.to_rfc3339(),tombstone])?;
        tx.commit()?;
        Ok(receipt)
    }

    pub fn annotation_detail(
        &self,
        annotation_id: &str,
        revision: Option<u32>,
        camera_guard: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<AnnotationDetail> {
        self.ensure_annotation_schema()?;
        let set: (String,String,String,String,String,Option<i64>,String,u32,String,String) = self.conn.query_row(
            "SELECT database_id,source_asset_id,source_sha256,camera_id,capture_time,observation_id,evidence_asset_id,current_revision,created_at,expires_at FROM annotation_sets WHERE annotation_id=?1",
            [annotation_id], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get::<_,i64>(7).map(|v|v as u32)?,r.get(8)?,r.get(9)?)),
        ).map_err(|e| match e { rusqlite::Error::QueryReturnedNoRows => StoreError::AnnotationNotFound, other => StoreError::Sql(other) })?;
        if camera_guard.is_some_and(|camera| camera != set.3) {
            return Err(StoreError::InvalidInput("camera_mismatch".into()));
        }
        if ts(&set.9)? <= now {
            return Err(StoreError::AnnotationExpired);
        }
        let rev = revision.unwrap_or(set.7);
        let row: (u32,String,String,String,u32,String,String,String) = self.conn.query_row(
            "SELECT schema_version,coordinate_system,producer_json,annotations_json,object_count,payload_sha256,request_id,persisted_at FROM annotation_revisions WHERE annotation_id=?1 AND revision=?2",
            params![annotation_id,rev], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get::<_,i64>(4).map(|v|v as u32)?,r.get(5)?,r.get(6)?,r.get(7)?)),
        ).map_err(|e| match e { rusqlite::Error::QueryReturnedNoRows => StoreError::AnnotationNotFound, other => StoreError::Sql(other) })?;
        let producer =
            serde_json::from_str(&row.2).map_err(|e| StoreError::InvalidInput(e.to_string()))?;
        let annotations =
            serde_json::from_str(&row.3).map_err(|e| StoreError::InvalidInput(e.to_string()))?;
        Ok(AnnotationDetail {
            annotation_id: annotation_id.into(),
            database_id: set.0,
            source_asset_id: set.1,
            source_sha256: set.2,
            camera_id: set.3,
            capture_time: set.4,
            observation_id: set.5,
            evidence_asset_id: set.6,
            current_revision: set.7,
            revision: rev,
            schema_version: row.0,
            coordinate_system: row.1,
            producer,
            annotations,
            object_count: row.4,
            payload_sha256: row.5,
            request_id: row.6,
            persisted_at: row.7,
            created_at: set.8,
            expires_at: set.9,
            provenance: ANNOTATION_PROVENANCE.into(),
            original_source_status: "available".into(),
            evidence_status: "available".into(),
        })
    }

    pub fn list_annotations(
        &self,
        filter: &AnnotationListFilter,
        now: DateTime<Utc>,
    ) -> Result<AnnotationListPage> {
        self.ensure_annotation_schema()?;
        if filter.limit == 0 || filter.limit > 50 {
            return Err(StoreError::InvalidInput(
                "annotation limit must be 1..50".into(),
            ));
        }
        let mut sql = String::from(
            "SELECT s.annotation_id,s.database_id,s.source_asset_id,s.source_sha256,s.camera_id,
                    s.evidence_asset_id,s.current_revision,r.object_count,s.created_at,s.expires_at
             FROM annotation_sets s JOIN annotation_revisions r
               ON r.annotation_id=s.annotation_id AND r.revision=s.current_revision
             WHERE s.database_id=?1 AND s.expires_at>?2",
        );
        let mut values = vec![
            Value::Text(filter.database_id.clone()),
            Value::Text(now.to_rfc3339()),
        ];
        let mut next = 3;
        if let Some(camera) = &filter.camera_id {
            sql.push_str(&format!(" AND s.camera_id=?{next}"));
            values.push(Value::Text(camera.clone()));
            next += 1;
        }
        if let Some(asset) = &filter.asset_id {
            sql.push_str(&format!(" AND s.source_asset_id=?{next}"));
            values.push(Value::Text(asset.clone()));
            next += 1;
        }
        if let Some(value) = &filter.cursor {
            let (created, id) = parse_cursor(value)?;
            sql.push_str(&format!(" AND (s.created_at < ?{next} OR (s.created_at = ?{next} AND s.annotation_id > ?{}))", next + 1));
            values.push(Value::Text(created));
            values.push(Value::Text(id));
            next += 2;
        }
        sql.push_str(&format!(
            " ORDER BY s.created_at DESC,s.annotation_id ASC LIMIT ?{next}"
        ));
        values.push(Value::Integer((filter.limit + 1) as i64));
        let mut stmt = self.conn.prepare(&sql)?;
        let mut rows = stmt.query(params_from_iter(values))?;
        let mut items = Vec::new();
        while let Some(row) = rows.next()? {
            items.push(AnnotationSummary {
                annotation_id: row.get(0)?,
                database_id: row.get(1)?,
                source_asset_id: row.get(2)?,
                source_sha256: row.get(3)?,
                camera_id: row.get(4)?,
                evidence_asset_id: row.get(5)?,
                current_revision: row.get::<_, i64>(6)? as u32,
                object_count: row.get::<_, i64>(7)? as u32,
                evidence_status: "available".into(),
                provenance: ANNOTATION_PROVENANCE.into(),
                created_at: row.get(8)?,
                expires_at: row.get(9)?,
            });
        }
        let has_more = items.len() > filter.limit as usize;
        items.truncate(filter.limit as usize);
        let next_cursor = has_more.then(|| {
            let last = items.last().expect("limit is positive");
            cursor(&last.created_at, &last.annotation_id)
        });
        Ok(AnnotationListPage { items, next_cursor })
    }

    pub fn expire_annotations(
        &self,
        now: DateTime<Utc>,
        tombstone_retention: Duration,
    ) -> Result<AnnotationMaintenance> {
        self.ensure_annotation_schema()?;
        let tx = self.conn.unchecked_transaction()?;
        let expired: Vec<String> = {
            let mut stmt =
                tx.prepare("SELECT annotation_id FROM annotation_sets WHERE expires_at<=?1")?;
            stmt.query_map([now.to_rfc3339()], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?
        };
        let mut report = AnnotationMaintenance::default();
        for id in expired {
            let (rev, evidence): (i64, String) = tx.query_row(
                "SELECT (SELECT COUNT(*) FROM annotation_revisions WHERE annotation_id=s.annotation_id),s.evidence_asset_id FROM annotation_sets s WHERE s.annotation_id=?1",
                [&id], |r| Ok((r.get(0)?, r.get(1)?),),
            )?;
            tx.execute("UPDATE annotation_evidence SET active_refs=MAX(active_refs-1,0) WHERE evidence_asset_id=?1", [&evidence])?;
            tx.execute("DELETE FROM annotation_sets WHERE annotation_id=?1", [&id])?;
            report.expired_sets += 1;
            report.expired_revisions += rev as u64;
        }
        report.deleted_tombstones = tx.execute(
            "DELETE FROM annotation_requests WHERE tombstone_deadline<=?1",
            [now.to_rfc3339()],
        )? as u64;
        let _ = tombstone_retention;
        report.orphaned_evidence = tx.execute(
            "DELETE FROM annotation_evidence WHERE active_refs<=0 AND expires_at IS NOT NULL AND expires_at<=?1",
            [now.to_rfc3339()],
        )? as u64;
        tx.commit()?;
        Ok(report)
    }
}
