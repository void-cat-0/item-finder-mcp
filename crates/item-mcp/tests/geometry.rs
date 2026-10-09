mod support;

use chrono::{DateTime, Duration, TimeZone, Utc};
use item_core::ObservationEventInput;
use item_core::store::Store;
use serde_json::{Value, json};
use support::Client;

struct Fixture {
    _dir: tempfile::TempDir,
    db: std::path::PathBuf,
    target: i64,
}

fn at(seconds: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 2, 12, 0, 0).unwrap() + Duration::seconds(seconds)
}

fn lifecycle(
    store: &Store,
    observation_id: i64,
    label: &str,
    event_type: &str,
    seconds: i64,
    bbox: [f32; 4],
    hits: i64,
) -> i64 {
    let geometry = store
        .record_geometry(
            observation_id,
            &format!("event_{event_type}_{seconds}"),
            at(seconds),
            bbox,
            Some(1280),
            Some(720),
            "desk",
            "local_detector",
            Some(0.8),
            None,
        )
        .unwrap();
    store
        .append_observation_event(&ObservationEventInput {
            observation_id,
            camera_id: "desk".into(),
            zone: "desk".into(),
            label: label.into(),
            event_type: event_type.into(),
            occurred_at: at(seconds),
            noticed_at: at(seconds + 1),
            source: "local_detector".into(),
            session_id: Some("run-1".into()),
            reason: if event_type == "appeared" {
                "appeared".into()
            } else {
                "missed_gap".into()
            },
            hits,
            seen_for_s: (event_type == "disappeared").then_some(4.0),
            geometry_id: Some(geometry),
        })
        .unwrap();
    geometry
}

impl Fixture {
    fn scene() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("items.db");
        let store = Store::open(&db).unwrap();
        let window = std::time::Duration::from_secs(300);
        let target = store
            .record_sighting("desk", "desk", "keys", at(0), None, window)
            .unwrap()
            .0;
        lifecycle(
            &store,
            target,
            "keys",
            "appeared",
            -20,
            [600.0, 400.0, 660.0, 440.0],
            1,
        );
        let cover = store
            .record_sighting("desk", "desk", "box", at(-1), None, window)
            .unwrap()
            .0;
        lifecycle(
            &store,
            cover,
            "box",
            "appeared",
            -1,
            [560.0, 360.0, 720.0, 480.0],
            2,
        );
        lifecycle(
            &store,
            target,
            "keys",
            "disappeared",
            0,
            [600.0, 400.0, 660.0, 440.0],
            20,
        );
        Self {
            _dir: dir,
            db,
            target,
        }
    }

    fn with_cover_closure() -> Self {
        let fixture = Self::scene();
        let store = Store::open(&fixture.db).unwrap();
        let cover = store
            .observations_filtered(&item_core::store::ObservationFilter {
                label: Some("box".into()),
                limit: 1,
                ..Default::default()
            })
            .unwrap()[0]
            .id;
        lifecycle(
            &store,
            cover,
            "box",
            "disappeared",
            5,
            [560.0, 360.0, 720.0, 480.0],
            6,
        );
        fixture
    }
}

fn call(client: &mut Client, id: i64, name: &str, args: Value) -> Value {
    client.tool(id, name, args)
}

#[test]
fn context_and_spatial_hypotheses_return_safe_metadata_without_daemon() {
    let fixture = Fixture::scene();
    let mut client = Client::with_db(&fixture.db);
    client.initialize("2025-11-25");

    let context = call(
        &mut client,
        2,
        "get_observation_context",
        json!({"observation_id":fixture.target,"camera_id":"desk"}),
    );
    assert_eq!(context["contract_version"], "geometry-mcp-v1");
    assert_eq!(context["source"], "database_observation_facts");
    assert_eq!(context["observation"]["observation_id"], fixture.target);
    assert_eq!(context["source_assets"]["status"], "not_checked");
    assert!(context["geometry"].is_array());
    assert!(context["events"].is_array());
    assert_eq!(context["depth_scope"], "returned_geometry_only");
    assert!(context.to_string().len() < 256 * 1024);

    let hypotheses = call(
        &mut client,
        3,
        "get_spatial_hypotheses",
        json!({"observation_id":fixture.target,"camera_id":"desk"}),
    );
    assert_eq!(hypotheses["contract_version"], "geometry-mcp-v1");
    assert_eq!(hypotheses["source"], "derived_spatial_hypotheses");
    let context_as_of = DateTime::parse_from_rfc3339(context["as_of"].as_str().unwrap()).unwrap();
    let hypotheses_as_of =
        DateTime::parse_from_rfc3339(hypotheses["as_of"].as_str().unwrap()).unwrap();
    assert!(hypotheses_as_of >= context_as_of);
    assert_eq!(hypotheses["scope"]["camera_id"], "desk");
    assert_eq!(hypotheses["scope"]["zone"], "desk");
    assert_eq!(hypotheses["scope"]["session_id"], "run-1");
    assert_eq!(hypotheses["priors"]["priors_ref"], "builtin:g4-priors-1");
    assert!(hypotheses["used_facts"]["prior_entries"].is_array());
    assert!(hypotheses["candidates"].is_array());
    assert!(
        hypotheses["note"]
            .as_str()
            .unwrap()
            .contains("current presence")
    );
    client.finish();
}

#[test]
fn positive_candidate_evidence_refs_are_closed_and_no_raw_paths_leak() {
    let fixture = Fixture::scene();
    let mut client = Client::with_db(&fixture.db);
    client.initialize("2025-11-25");
    let output = call(
        &mut client,
        2,
        "get_spatial_hypotheses",
        json!({"observation_id":fixture.target,"camera_id":"desk","limit":50}),
    );
    let candidates = output["candidates"].as_array().unwrap();
    assert!(!candidates.is_empty(), "{output}");
    let candidate = &candidates[0];
    assert_eq!(candidate["status"], "candidate");
    assert_eq!(
        candidate["score_semantics"],
        "deterministic_heuristic_not_probability"
    );
    assert!(
        candidate["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["refs"].is_array())
    );
    assert!(candidate["lifecycle_check"]["result"].is_string());
    assert!(
        !output["used_facts"]["observations"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        !output["used_facts"]["events"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        !output["used_facts"]["geometry"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let fact_ids = |kind: &str| -> std::collections::HashSet<String> {
        let field = match kind {
            "observations" => "observation_id",
            "events" => "event_id",
            "geometry" => "geometry_id",
            "depth" => "depth_id",
            _ => unreachable!(),
        };
        output["used_facts"][kind]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| {
                v.get(field)
                    .and_then(Value::as_i64)
                    .map(|id| id.to_string())
            })
            .collect()
    };
    let observations = fact_ids("observations");
    let events = fact_ids("events");
    let geometry = fact_ids("geometry");
    let depth = fact_ids("depth");
    let prior_entries: std::collections::HashSet<(String, String)> =
        output["used_facts"]["prior_entries"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| {
                Some((
                    v["key"].as_str()?.to_owned(),
                    v["priors_ref"].as_str()?.to_owned(),
                ))
            })
            .collect();
    for contribution in candidate["evidence"].as_array().unwrap() {
        for reference in contribution["refs"].as_array().unwrap() {
            let kind = reference["type"].as_str().unwrap();
            let found = match kind {
                "observation" => reference["id"]
                    .as_i64()
                    .is_some_and(|id| observations.contains(&id.to_string())),
                "event" => reference["id"]
                    .as_i64()
                    .is_some_and(|id| events.contains(&id.to_string())),
                "geometry" => reference["id"]
                    .as_i64()
                    .is_some_and(|id| geometry.contains(&id.to_string())),
                "depth" => reference["id"]
                    .as_i64()
                    .is_some_and(|id| depth.contains(&id.to_string())),
                "prior_entry" => reference["key"]
                    .as_str()
                    .zip(reference["priors_ref"].as_str())
                    .is_some_and(|(key, reference)| {
                        prior_entries.contains(&(key.to_owned(), reference.to_owned()))
                    }),
                _ => false,
            };
            assert!(found, "unresolved {kind} ref {reference}: {output}");
        }
    }
    let text = output.to_string();
    assert!(!text.contains("snapshot.jpg"));
    assert!(!text.contains("C:/"));
    client.finish();
}

#[test]
fn cover_closure_suppresses_stale_positive_hypothesis() {
    let fixture = Fixture::with_cover_closure();
    let mut client = Client::with_db(&fixture.db);
    client.initialize("2025-11-25");
    let output = call(
        &mut client,
        2,
        "get_spatial_hypotheses",
        json!({"observation_id":fixture.target,"camera_id":"desk"}),
    );
    assert!(
        output["candidates"].as_array().unwrap().is_empty(),
        "{output}"
    );
    assert_eq!(output["reason"], "no_candidate_supported");
    assert!(
        output["diagnostic_counts"]
            .get("cover_lifecycle_ended")
            .is_some()
    );
    client.finish();
}

#[test]
fn strict_inputs_reject_null_optional_and_invalid_limits_then_recover() {
    let fixture = Fixture::scene();
    let mut client = Client::with_db(&fixture.db);
    client.initialize("2025-11-25");
    let response = client.request(
        2,
        "tools/call",
        json!({"name":"get_observation_context","arguments":{"observation_id":fixture.target,"camera_id":null}}),
    );
    assert_eq!(response["result"]["isError"], true);
    assert!(
        response["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("null")
    );
    client.tool_error(
        3,
        "get_spatial_hypotheses",
        json!({"observation_id":fixture.target,"window_secs":0}),
        "invalid_input",
    );
    let output = call(
        &mut client,
        4,
        "get_observation_context",
        json!({"observation_id":fixture.target}),
    );
    assert_eq!(output["observation"]["observation_id"], fixture.target);
    client.finish();
}

#[test]
fn old_schema_exposes_basic_observation_but_marks_evidence_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("old.db");
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch(
        "CREATE TABLE observations (id INTEGER PRIMARY KEY, camera_id TEXT NOT NULL, zone TEXT NOT NULL, label TEXT NOT NULL, first_seen TEXT NOT NULL, last_seen TEXT NOT NULL, hit_count INTEGER NOT NULL, sample_snapshot TEXT); INSERT INTO observations VALUES (1,'desk','desk','keys','2026-10-02T12:00:00Z','2026-10-02T12:00:00Z',1,NULL); PRAGMA user_version=1;",
    ).unwrap();
    drop(conn);
    let mut client = Client::with_db(&db);
    client.initialize("2025-11-25");
    let context = call(
        &mut client,
        2,
        "get_observation_context",
        json!({"observation_id":1}),
    );
    assert_eq!(context["observation"]["label"], "keys");
    assert_eq!(
        context["branches"]["geometry"]["availability"],
        "schema_unavailable"
    );
    assert_eq!(
        context["branches"]["events"]["availability"],
        "schema_unavailable"
    );
    let hypotheses = call(
        &mut client,
        3,
        "get_spatial_hypotheses",
        json!({"observation_id":1}),
    );
    assert_eq!(hypotheses["availability"], "schema_unavailable");
    assert_eq!(hypotheses["candidates"], json!([]));
    client.finish();
}
