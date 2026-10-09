mod support;

use item_core::store::{DEFAULT_DEDUP_WINDOW, Store};
use serde_json::json;
use support::Client;

struct Fixture {
    _dir: tempfile::TempDir,
    db: std::path::PathBuf,
    keys: i64,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("items.db");
        let store = Store::open(&db).unwrap();
        let seen = chrono::DateTime::parse_from_rfc3339("2026-10-01T10:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let (keys, _, _) = store
            .record_sighting(
                "desk",
                "tray",
                "keys",
                seen,
                Some("private/snapshot.jpg"),
                DEFAULT_DEDUP_WINDOW,
            )
            .unwrap();
        store
            .record_sighting(
                "desk",
                "tray",
                "keys",
                seen + chrono::Duration::minutes(1),
                None,
                DEFAULT_DEDUP_WINDOW,
            )
            .unwrap();
        store
            .record_sighting(
                "desk",
                "drawer",
                "keys",
                seen + chrono::Duration::minutes(2),
                None,
                DEFAULT_DEDUP_WINDOW,
            )
            .unwrap();
        store
            .record_sighting(
                "hall",
                "shelf",
                "keys",
                seen + chrono::Duration::minutes(3),
                None,
                DEFAULT_DEDUP_WINDOW,
            )
            .unwrap();
        store
            .record_sighting(
                "desk",
                "tray",
                "cup_50%",
                seen + chrono::Duration::minutes(4),
                None,
                DEFAULT_DEDUP_WINDOW,
            )
            .unwrap();
        store
            .record_sighting(
                "desk",
                "tray",
                "cupx50foo",
                seen + chrono::Duration::minutes(5),
                None,
                DEFAULT_DEDUP_WINDOW,
            )
            .unwrap();
        drop(store);
        Self {
            _dir: dir,
            db,
            keys,
        }
    }
}

#[test]
fn query_tools_read_inventory_and_direct_sightings_without_writing() {
    let fixture = Fixture::new();
    let before = std::fs::read(&fixture.db).unwrap();
    let mut client = Client::with_db(&fixture.db);
    client.initialize("2025-11-25");
    let inventory = client.tool(2, "list_recorded_items", json!({}));
    let keys = inventory["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["label"] == "keys")
        .unwrap();
    assert_eq!(keys["observation_count"], 3);
    assert_eq!(keys["cameras"], json!(["desk", "hall"]));
    assert_eq!(keys["zones"], json!(["drawer", "shelf", "tray"]));
    assert_eq!(inventory["truncated"], false);
    let inventory = client.tool(3, "list_recorded_items", json!({"limit":1}));
    assert_eq!(inventory["truncated"], true);
    assert_eq!(inventory["items"].as_array().unwrap().len(), 1);
    let inventory = client.tool(
        4,
        "list_recorded_items",
        json!({"camera":"hall","zone":"shelf","limit":1}),
    );
    assert_eq!(inventory["items"][0]["label"], "keys");
    assert_eq!(inventory["items"][0]["observation_count"], 1);
    assert_eq!(inventory["truncated"], false);

    let result = client.tool(
        5,
        "locate_item",
        json!({"query":"where are my keys?","camera":"desk","zone":"tray","limit":1}),
    );
    assert_eq!(result["keyword"], "keys");
    assert_eq!(result["label_source"], "query_keyword_heuristic");
    assert_eq!(result["direct"][0]["observation_id"], fixture.keys);
    assert_eq!(result["direct"][0]["hit_count"], 2);
    assert!(result.get("hypotheses").is_none());
    let result = client.tool(
        6,
        "locate_item",
        json!({"label":"cup_50%","query":"where are my keys?"}),
    );
    assert_eq!(result["label_source"], "explicit_label");
    assert_eq!(result["direct"].as_array().unwrap().len(), 1);
    assert_eq!(result["direct"][0]["label"], "cup_50%");

    let result = client.tool(7,"list_observations",json!({"camera":"desk","zone":"tray","since":"2026-10-01T11:01:00+01:00","until":"2026-10-01T10:01:00Z"}));
    assert_eq!(result["observations"].as_array().unwrap().len(), 1);
    assert_eq!(result["observations"][0]["observation_id"], fixture.keys);
    assert_eq!(
        result["observations"][0]["last_seen"],
        "2026-10-01T10:01:00Z"
    );
    let result = client.tool(8, "get_observation", json!({"observation_id":fixture.keys}));
    assert_eq!(result["observation"]["label"], "keys");
    assert_eq!(result["detail_level"], "basic");
    assert!(!result.to_string().contains("snapshot"));
    let result = client.tool(9, "locate_item", json!({"label":"not recorded"}));
    assert_eq!(result["direct"], json!([]));
    client.tool_error(
        10,
        "get_observation",
        json!({"observation_id":99999}),
        "not_found",
    );
    client.finish();
    assert_eq!(
        std::fs::read(&fixture.db).unwrap(),
        before,
        "MCP mutated the database"
    );
}

#[test]
fn missing_database_can_appear_without_restarting_and_empty_is_not_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("not-yet-created.db");
    let mut client = Client::with_db(&db);
    client.initialize("2025-03-26");
    client.tool_error(2, "list_recorded_items", json!({}), "database_unavailable");
    let info = client.tool(3, "server_info", json!({}));
    assert_eq!(info["mode"], "queries_only");
    assert!(!db.exists(), "query created missing database");
    let store = Store::open(&db).unwrap();
    let result = client.tool(4, "list_observations", json!({}));
    assert_eq!(result["observations"], json!([]));
    let result = client.tool(5, "list_recorded_items", json!({}));
    assert_eq!(result["items"], json!([]));
    store
        .record_sighting(
            "desk",
            "tray",
            "keys",
            chrono::Utc::now(),
            None,
            DEFAULT_DEDUP_WINDOW,
        )
        .unwrap();
    let result = client.tool(6, "locate_item", json!({"label":"keys"}));
    assert_eq!(
        result["direct"][0]["label"], "keys",
        "reader sees live WAL writes"
    );
    client.finish();
}

#[test]
fn validation_errors_are_distinct_from_database_and_not_found_errors() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("missing.db");
    let mut client = Client::with_db(&db);
    client.initialize("2025-11-25");
    let invalid = [
        ("locate_item", json!({})),
        ("locate_item", json!({"label":"  "})),
        ("list_observations", json!({"camera":"\n"})),
        ("list_observations", json!({"since":"not a date"})),
        (
            "list_observations",
            json!({"since":"2026-10-02T00:00:00Z","until":"2026-10-01T00:00:00Z"}),
        ),
        ("list_observations", json!({"limit":0})),
        ("list_recorded_items", json!({"limit":501})),
        ("get_observation", json!({"observation_id":-1})),
    ];
    for (index, (name, args)) in invalid.into_iter().enumerate() {
        client.tool_error(index as i64 + 2, name, args, "invalid_input");
    }
    client.tool_error(
        10,
        "get_observation",
        json!({"observation_id":1}),
        "database_unavailable",
    );
    assert!(!db.exists());
    client.finish();
}

#[test]
fn old_schema_and_cli_over_environment_are_supported() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("v1.db");
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch("PRAGMA user_version=1;
        CREATE TABLE observations (id INTEGER PRIMARY KEY,camera_id TEXT NOT NULL,zone TEXT NOT NULL,label TEXT NOT NULL,first_seen TEXT NOT NULL,last_seen TEXT NOT NULL,hit_count INTEGER NOT NULL,sample_snapshot TEXT);
        INSERT INTO observations VALUES (1,'desk','tray','keys','2026-10-01T10:00:00Z','2026-10-01T10:00:00Z',2,NULL);").unwrap();
    drop(conn);
    let before = std::fs::read(&db).unwrap();
    let missing = dir.path().join("wrong-env-db");
    let mut client = Client::start(true, Some(&db), Some(&missing));
    client.initialize("2024-11-05");
    let result = client.tool(2, "list_recorded_items", json!({}));
    assert_eq!(result["items"][0]["label"], "keys");
    let result = client.tool(3, "get_observation", json!({"observation_id":1}));
    assert_eq!(result["observation"]["hit_count"], 2);
    client.finish();
    assert_eq!(
        std::fs::read(&db).unwrap(),
        before,
        "reader migrated old schema"
    );
    assert!(!missing.exists());
    let mut client = Client::start(false, None, Some(&db));
    client.initialize("2025-03-26");
    let result = client.tool(2, "locate_item", json!({"label":"keys"}));
    assert_eq!(result["direct"][0]["camera_id"], "desk");
    client.finish();
}

#[test]
fn busy_database_reports_unavailability_then_recovers() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("locked.db");
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch("CREATE TABLE observations (id INTEGER PRIMARY KEY,camera_id TEXT NOT NULL,zone TEXT NOT NULL,label TEXT NOT NULL,first_seen TEXT NOT NULL,last_seen TEXT NOT NULL,hit_count INTEGER NOT NULL,sample_snapshot TEXT);
        BEGIN EXCLUSIVE;").unwrap();
    let mut client = Client::with_db(&db);
    client.initialize("2025-11-25");
    client.tool_error(2, "list_observations", json!({}), "database_unavailable");
    conn.execute_batch("ROLLBACK").unwrap();
    let result = client.tool(3, "list_observations", json!({}));
    assert_eq!(result["observations"], json!([]));
    client.finish();
}

#[test]
fn malformed_database_is_not_reported_as_an_empty_log() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("broken.db");
    std::fs::write(&db, b"not a sqlite database").unwrap();
    let mut client = Client::with_db(&db);
    client.initialize("2025-11-25");
    client.tool_error(2, "list_observations", json!({}), "database_unavailable");
    client.finish();
}
