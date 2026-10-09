use super::*;
fn control() -> ReadControl {
    ReadControl::new(
        Instant::now() + Duration::from_secs(10),
        Arc::new(AtomicBool::new(false)),
    )
}
#[test]
fn latest_departure_is_selected_before_ownership_validation() {
    let s = Store::in_memory().unwrap();
    let o = seed(&s);
    let g = geo(&s, o, "event_disappeared_1", "2026-01-01T00:00:00Z");
    event(&s, o, g, "2026-01-01T00:00:00Z");
    s.conn.execute("INSERT INTO observation_events(observation_id,camera_id,zone,label,event_type,occurred_at,noticed_at,source,session_id,reason,hits,geometry_id) VALUES(?1,'foreign','desk','keys','disappeared','2026-01-01T00:00:01Z','2026-01-01T00:00:01Z','detector','run','missed_gap',2,?2)",params![o,g]).unwrap();
    let snapshot = s.read_snapshot(control()).unwrap();
    let target = snapshot.observation(o, None).unwrap();
    let latest = snapshot
        .latest_departure_for_observation(&target)
        .unwrap()
        .unwrap();
    assert_eq!(latest.camera_id, "foreign");
    assert_eq!(latest.occurred_at.timestamp(), 1767225601);
}

#[test]
fn scene_depth_keeps_legacy_ascending_selection_order() {
    let s = Store::in_memory().unwrap();
    let o = seed(&s);
    let g = geo(&s, o, "event_disappeared_1", "2026-01-01T00:00:00Z");
    event(&s, o, g, "2026-01-01T00:00:00Z");
    depth(&s, g);
    depth(&s, g);
    let snapshot = s.read_snapshot(control()).unwrap();
    let scene = snapshot
        .scene_window(SceneFilter {
            camera: "cam",
            zone: "desk",
            session: "run",
            since: DateTime::parse_from_rfc3339("2025-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            until: snapshot.as_of,
        })
        .unwrap();
    assert!(scene.events[0].depth[0].id < scene.events[0].depth[1].id);
    let context = snapshot
        .observation_context(o, None, ContextLimits::default())
        .unwrap();
    assert!(context.depth[0].id > context.depth[1].id);
}
#[test]
fn controlled_open_and_busy_wait_respect_deadline_without_creating_missing_db() {
    let path = std::env::temp_dir().join(format!(
        "geometry-busy-{}-{}.db",
        std::process::id(),
        Utc::now().timestamp_nanos_opt().unwrap()
    ));
    assert!(Store::open_read_only_controlled(&path, control()).is_err());
    assert!(!path.exists());
    let writer = Store::open(&path).unwrap();
    seed(&writer);
    writer
        .conn
        .execute_batch("PRAGMA journal_mode=DELETE; BEGIN EXCLUSIVE")
        .unwrap();
    let c = ReadControl::new(
        Instant::now() + Duration::from_millis(40),
        Arc::new(AtomicBool::new(false)),
    );
    let start = Instant::now();
    let reader = Store::open_read_only_controlled(&path, c.clone()).unwrap();
    assert!(reader.read_snapshot(c).is_err());
    assert!(start.elapsed() < Duration::from_secs(1));
    writer.conn.execute_batch("ROLLBACK").unwrap();
    drop(reader);
    drop(writer);
    let _ = std::fs::remove_file(&path);
}
fn seed(s: &Store) -> i64 {
    s.record_sighting(
        "cam",
        "desk",
        "keys",
        DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc),
        None,
        Duration::from_secs(10),
    )
    .unwrap()
    .0
}
fn geo(s: &Store, obs: i64, kind: &str, time: &str) -> i64 {
    s.conn.execute("INSERT INTO observation_geometry(observation_id,sample_kind,captured_at,x0,y0,x1,y1,frame_width,frame_height,zone,source,snapshot_ref) VALUES (?1,?2,?3,1,1,5,5,10,10,'desk','detector','C:/secret.jpg')",params![obs,kind,time]).unwrap();
    s.conn.last_insert_rowid()
}
fn event(s: &Store, obs: i64, id: i64, time: &str) -> i64 {
    s.conn.execute("INSERT INTO observation_events(observation_id,camera_id,zone,label,event_type,occurred_at,noticed_at,source,session_id,reason,hits,geometry_id) VALUES (?1,'cam','desk','keys','disappeared',?2,?2,'detector','run','missed_gap',2,?3)",params![obs,time,id]).unwrap();
    s.conn.last_insert_rowid()
}
fn depth(s: &Store, g: i64) {
    s.conn.execute("INSERT INTO depth_evidence(geometry_id,provider,mode,relation_to_camera,quality) VALUES (?1,'provider','relative','nearer','usable')",[g]).unwrap();
}
#[test]
fn mixed_offsets_nanoseconds_are_ordered_by_instant() {
    let s = Store::in_memory().unwrap();
    let o = seed(&s);
    let old = geo(&s, o, "event_a", "2026-01-01T08:00:00.000000001+08:00");
    let new = geo(&s, o, "event_b", "2026-01-01T00:00:00.000000002Z");
    event(&s, o, old, "2026-01-01T08:00:00.000000001+08:00");
    let newest = event(&s, o, new, "2026-01-01T00:00:00.000000002Z");
    let snap = s.read_snapshot(control()).unwrap();
    let c = snap
        .observation_context(
            o,
            None,
            ContextLimits {
                max_geometry: 1,
                max_events: 1,
                max_depth: 1,
            },
        )
        .unwrap();
    assert_eq!(c.geometry[0].id, new);
    assert_eq!(c.events[0].id, newest);
    assert!(c.branches.geometry.truncated);
    assert!(c.branches.events.truncated);
    assert!(c.geometry[0].snapshot_ref.is_none());
    assert!(c.observation.sample_snapshot.is_none());
    assert_eq!(
        snap.latest_departure_for_observation(&c.observation)
            .unwrap()
            .unwrap()
            .id,
        newest
    );
}
#[test]
fn depth_budget_is_total_and_returned_geometry_scoped() {
    let s = Store::in_memory().unwrap();
    let o = seed(&s);
    let a = geo(&s, o, "event_a", "2026-01-01T00:00:00Z");
    let b = geo(&s, o, "event_b", "2026-01-01T00:00:01Z");
    for _ in 0..4 {
        depth(&s, a);
        depth(&s, b);
    }
    let snap = s.read_snapshot(control()).unwrap();
    let c = snap
        .observation_context(
            o,
            None,
            ContextLimits {
                max_geometry: 1,
                max_events: 1,
                max_depth: 2,
            },
        )
        .unwrap();
    assert_eq!(c.depth.len(), 2);
    assert!(c.branches.depth.truncated);
    assert!(c.depth.iter().all(|d| d.geometry_id == b));
    assert!(c.depth[0].id > c.depth[1].id);
}
#[test]
fn guard_precedes_broken_associated_facts() {
    let s = Store::in_memory().unwrap();
    let o = seed(&s);
    geo(&s, o, "last", "not-a-time");
    let snap = s.read_snapshot(control()).unwrap();
    assert!(matches!(
        snap.observation_context(o, Some("wrong"), ContextLimits::default()),
        Err(StoreError::CameraMismatch)
    ));
    assert!(matches!(
        snap.observation_context(o, None, ContextLimits::default()),
        Err(StoreError::Sql(_))
    ));
}
#[test]
fn schema_missing_and_empty_are_distinct_and_future_is_conservative() {
    let s = Store::in_memory().unwrap();
    let o = seed(&s);
    s.conn.execute_batch("DROP TABLE depth_evidence;DROP TABLE observation_events;DROP TABLE observation_geometry;PRAGMA user_version=1").unwrap();
    let snap = s.read_snapshot(control()).unwrap();
    let c = snap
        .observation_context(o, None, ContextLimits::default())
        .unwrap();
    assert_eq!(c.branches.geometry.reason, Some("table_missing"));
    assert_eq!(c.branches.events.availability, "schema_unavailable");
    drop(snap);
    s.conn.pragma_update(None, "user_version", 99).unwrap();
    let snap = s.read_snapshot(control()).unwrap();
    let c = snap
        .observation_context(o, None, ContextLimits::default())
        .unwrap();
    assert_eq!(c.branches.depth.reason, Some("unsupported_schema_version"));
}
#[test]
fn legacy_depth_optional_provenance_missing_is_partial_not_failure() {
    let s = Store::in_memory().unwrap();
    let o = seed(&s);
    let g = geo(&s, o, "last", "2026-01-01T00:00:00Z");
    depth(&s, g);
    for col in [
        "frame_id",
        "response_id",
        "prompt_version",
        "backend_version",
    ] {
        s.conn
            .execute_batch(&format!("ALTER TABLE depth_evidence DROP COLUMN {col}"))
            .unwrap();
    }
    let snap = s.read_snapshot(control()).unwrap();
    let c = snap
        .observation_context(o, None, ContextLimits::default())
        .unwrap();
    assert_eq!(c.depth.len(), 1);
    assert_eq!(c.branches.depth.reason, Some("optional_provenance_missing"));
    assert!(!snap.capabilities.depth_provenance_complete);
}
#[test]
fn required_columns_missing_do_not_swallow_sql_errors() {
    let s = Store::in_memory().unwrap();
    let o = seed(&s);
    s.conn
        .execute_batch("ALTER TABLE observation_geometry DROP COLUMN confidence")
        .unwrap();
    let snap = s.read_snapshot(control()).unwrap();
    assert_eq!(
        snap.observation_context(o, None, ContextLimits::default())
            .unwrap()
            .branches
            .geometry
            .reason,
        Some("required_columns_missing")
    );
    drop(snap);
    s.conn.execute_batch("DROP TABLE observations").unwrap();
    let snap = s.read_snapshot(control()).unwrap();
    assert!(matches!(snap.observation(o, None), Err(StoreError::Sql(_))));
}
#[test]
fn cancellation_deadline_and_oversize_necessary_fields_are_typed() {
    let s = Store::in_memory().unwrap();
    let o = seed(&s);
    let c = control();
    c.cancelled.store(true, Ordering::Relaxed);
    assert!(matches!(s.read_snapshot(c), Err(StoreError::ReadCancelled)));
    let expired = ReadControl::new(
        Instant::now() - Duration::from_secs(1),
        Arc::new(AtomicBool::new(false)),
    );
    assert!(matches!(
        s.read_snapshot(expired),
        Err(StoreError::ReadTimeout)
    ));
    s.conn
        .execute(
            "UPDATE observations SET label=?1 WHERE id=?2",
            params!["x".repeat(4097), o],
        )
        .unwrap();
    let snap = s.read_snapshot(control()).unwrap();
    assert!(matches!(
        snap.observation(o, None),
        Err(StoreError::PayloadTooLarge)
    ));
}
#[test]
fn sqlite_progress_interrupts_unbounded_scans() {
    let s = Store::in_memory().unwrap();
    let c = control();
    let snap = s.read_snapshot(c.clone()).unwrap();
    c.cancelled.store(true, Ordering::Relaxed);
    let value=snap.tx.query_row("WITH RECURSIVE x(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM x WHERE n<100000000) SELECT sum(n) FROM x",[],|r|r.get::<_,i64>(0));
    assert!(value.is_err());
    assert!(matches!(
        snap.checked(value.map_err(Into::into)),
        Err(StoreError::ReadCancelled)
    ));
}
#[test]
fn scene_scope_filters_before_limit_and_detects_501() {
    let s = Store::in_memory().unwrap();
    let o = seed(&s);
    let g = geo(&s, o, "event_disappeared", "2026-01-01T00:00:00Z");
    for _ in 0..501 {
        event(&s, o, g, "2026-01-01T00:00:00Z");
    }
    s.conn
        .execute("UPDATE observation_events SET zone='other' WHERE id>1", [])
        .unwrap();
    let snap = s.read_snapshot(control()).unwrap();
    let filter = SceneFilter {
        camera: "cam",
        zone: "desk",
        session: "run",
        since: DateTime::parse_from_rfc3339("2025-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc),
        until: snap.as_of,
    };
    assert!(snap.scene_window(filter.clone()).unwrap().scan_complete);
    assert_eq!(snap.scene_window(filter).unwrap().events.len(), 1);
    drop(snap);
    s.conn
        .execute("UPDATE observation_events SET zone='desk'", [])
        .unwrap();
    let snap = s.read_snapshot(control()).unwrap();
    let scene = snap
        .scene_window(SceneFilter {
            camera: "cam",
            zone: "desk",
            session: "run",
            since: DateTime::parse_from_rfc3339("2025-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            until: snap.as_of,
        })
        .unwrap();
    assert_eq!(scene.events.len(), 500);
    assert!(!scene.scan_complete);
}
#[test]
fn wal_snapshot_is_consistent_and_read_only_does_not_migrate() {
    let path = std::env::temp_dir().join(format!(
        "geometry-snapshot-{}-{}.db",
        std::process::id(),
        Utc::now().timestamp_nanos_opt().unwrap()
    ));
    let writer = Store::open(&path).unwrap();
    let o = seed(&writer);
    let reader = Store::open_read_only(&path).unwrap();
    let snap = reader.read_snapshot(control()).unwrap();
    writer
        .conn
        .execute("UPDATE observations SET hit_count=99 WHERE id=?1", [o])
        .unwrap();
    assert_eq!(snap.observation(o, None).unwrap().hit_count, 1);
    drop(snap);
    let snap = reader.read_snapshot(control()).unwrap();
    assert_eq!(snap.observation(o, None).unwrap().hit_count, 99);
    assert!(
        snap.tx
            .execute("UPDATE observations SET hit_count=77", [])
            .is_err()
    );
    drop(snap);
    assert_eq!(reader.schema_version().unwrap(), 2);
    assert!(reader.database_identity().unwrap().is_none());
    drop(reader);
    drop(writer);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(path.with_extension("db-wal"));
    let _ = std::fs::remove_file(path.with_extension("db-shm"));
}
