use super::*;
use item_core::{
    Observation,
    evidence::{ContextBranches as FactBranches, ContextRead, EvidenceBranch},
};

fn context() -> ContextResponse {
    let at = DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    ContextResponse::from_read(ContextRead {
        database_id: None,
        as_of: at,
        observation: Observation {
            id: 42,
            camera_id: "desk".into(),
            zone: "table".into(),
            label: "keys".into(),
            first_seen: at,
            last_seen: at,
            hit_count: 7,
            sample_snapshot: Some("C:/private/snapshot.jpg".into()),
        },
        geometry: vec![],
        events: vec![],
        depth: vec![],
        branches: FactBranches {
            geometry: EvidenceBranch::rows(0, false),
            events: EvidenceBranch::rows(0, false),
            depth: EvidenceBranch::rows(0, false),
        },
    })
}

#[test]
fn strict_null_unknown_fields_and_byte_guard_are_rejected() {
    for v in [
        serde_json::json!({"observation_id":42,"camera_id":null}),
        serde_json::json!({"observation_id":42,"include_images":false}),
        serde_json::json!({"observation_id":42,"max_geometry":null}),
    ] {
        assert!(serde_json::from_value::<ContextRequest>(v).is_err());
    }
    let mut request: ContextRequest =
        serde_json::from_value(serde_json::json!({"observation_id":42})).unwrap();
    assert_eq!(
        (request.max_geometry, request.max_events, request.max_depth),
        (8, 16, 16)
    );
    request.camera_id = Some("摄".repeat(342));
    assert!(request.validate().is_err());
    request.camera_id = Some("desk\n".into());
    assert!(request.validate().is_err());
    for v in [
        serde_json::json!({"observation_id":42,"priors_profile":"builtin"}),
        serde_json::json!({"observation_id":42,"window_secs":null}),
    ] {
        assert!(serde_json::from_value::<SpatialRequest>(v).is_err());
    }
}

#[test]
fn output_schema_has_typed_bundle_and_ref_variants() {
    let schema = output_schema::<SpatialResponse>();
    let success = &schema["oneOf"][0];
    assert!(
        success["required"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "used_facts")
    );
    let defs = &schema["$defs"];
    assert!(defs["CandidateJson"]["properties"]["evidence"].is_object());
    assert!(defs["EvidenceRefJson"]["oneOf"].as_array().unwrap().len() >= 5);
    assert_eq!(defs["UsedFactsJson"]["additionalProperties"], false);
}

#[test]
fn metadata_provenance_paths_are_redacted_not_promoted() {
    assert_eq!(logical(Some("C:/private/model")), (None, "redacted"));
    assert_eq!(logical(Some("Bearer-secret")), (None, "redacted"));
    assert_eq!(logical(Some("provider-v1")).1, "available");
    let result = context().finish().unwrap();
    let text = &result.content[0].as_text().unwrap().text;
    assert!(!text.contains("private"));
    assert_eq!(
        serde_json::from_str::<Value>(text).unwrap(),
        result.structured_content.unwrap()
    );
}

#[test]
fn encoding_is_bounded_and_context_truncation_is_explicit() {
    assert!(
        encode_limited(&"x".repeat(RESPONSE_BYTES + 1))
            .unwrap()
            .is_none()
    );
    let mut response = context();
    for id in 1..=32 {
        response.geometry.push(GeometryJson {
            geometry_id: id,
            observation_id: 42,
            sample_kind: "event_disappeared_1".into(),
            sample_mutability: "immutable_event_boundary".into(),
            captured_at: "2026-01-01T00:00:00Z".into(),
            coordinate_system: "image_pixels_xyxy",
            bbox: [0.0, 0.0, 20.0, 20.0],
            frame_width: Some(100),
            frame_height: Some(100),
            validity: "valid".into(),
            zone: "z".repeat(12000),
            source: Some("local_detector".into()),
            source_status: "available".into(),
            confidence: None,
            source_asset_id: None,
        });
    }
    let result = response.finish().unwrap();
    let value = result.structured_content.unwrap();
    assert_eq!(value["branches"]["geometry"]["truncated"], true);
    assert!(value["geometry"].as_array().unwrap().len() < 32);
    assert!(serde_json::to_vec(&value).unwrap().len() <= RESPONSE_BYTES);
    let mut response = context();
    response.observation.label = "x".repeat(RESPONSE_BYTES);
    let error = response.finish().unwrap_err();
    assert_eq!(
        error.structured_content.unwrap()["error"]["code"],
        "payload_too_large"
    );
}

#[test]
fn spatial_byte_truncation_keeps_only_resolvable_used_facts() {
    use item_core::{ObservationEventInput, evidence::ReadControl, store::Store};
    let store = Store::in_memory().unwrap();
    let at = DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let target = store
        .record_sighting(
            "desk",
            "table",
            "keys",
            at,
            None,
            std::time::Duration::from_secs(300),
        )
        .unwrap()
        .0;
    let event = |id, label: &str, kind: &str, time, hits| {
        let geometry = store
            .record_geometry(
                id,
                &format!("event_{kind}_{id}"),
                time,
                [10.0, 10.0, 30.0, 30.0],
                Some(100),
                Some(100),
                "table",
                "local_detector",
                None,
                None,
            )
            .unwrap();
        store
            .append_observation_event(&ObservationEventInput {
                observation_id: id,
                camera_id: "desk".into(),
                zone: "table".into(),
                label: label.into(),
                event_type: kind.into(),
                occurred_at: time,
                noticed_at: time,
                source: "local_detector".into(),
                session_id: Some("run".into()),
                reason: if kind == "disappeared" {
                    "missed_gap"
                } else {
                    "appeared"
                }
                .into(),
                hits,
                seen_for_s: Some(3.0),
                geometry_id: Some(geometry),
            })
            .unwrap();
    };
    event(target, "keys", "disappeared", at, 7);
    for i in 0..50 {
        let label = format!("{}-{i}", "c".repeat(3500));
        let time = at - chrono::Duration::seconds(1);
        let id = store
            .record_sighting(
                "desk",
                "table",
                &label,
                time,
                None,
                std::time::Duration::from_secs(300),
            )
            .unwrap()
            .0;
        event(id, &label, "appeared", time, 2);
    }
    let budget = RequestBudget::new();
    let snapshot = store
        .read_snapshot(ReadControl::new(budget.deadline, budget.cancelled.clone()))
        .unwrap();
    let priors = crate::priors::PriorsSnapshot::default();
    let lookup = item_query::spatial::candidates_for_observation(
        &snapshot,
        target,
        None,
        item_query::spatial::SpatialOptions {
            window_secs: 120,
            limit: 50,
        },
        priors.priors(),
    )
    .unwrap();
    assert_eq!(lookup.candidates.len(), 50);
    let mut oversized = lookup.clone();
    oversized.candidates.truncate(1);
    oversized.candidates[0].candidate.explanation = "x".repeat(RESPONSE_BYTES);
    assert_eq!(
        SpatialResponse::from_lookup(oversized, priors.profile(), 120, 1)
            .finish()
            .unwrap_err()
            .structured_content
            .unwrap()["error"]["code"],
        "payload_too_large"
    );
    let output = SpatialResponse::from_lookup(lookup, priors.profile(), 120, 50)
        .finish()
        .unwrap()
        .structured_content
        .unwrap();
    assert_eq!(output["candidates_truncated"], true);
    assert_eq!(output["scan_complete"], true);
    let candidates = output["candidates"].as_array().unwrap();
    assert!(!candidates.is_empty() && candidates.len() < 50);
    for candidate in candidates {
        for contribution in candidate["evidence"].as_array().unwrap() {
            for reference in contribution["refs"].as_array().unwrap() {
                let (array, key) = match reference["type"].as_str().unwrap() {
                    "observation" => ("observations", "observation_id"),
                    "event" => ("events", "event_id"),
                    "geometry" => ("geometry", "geometry_id"),
                    "depth" => ("depth", "depth_id"),
                    "prior_entry" => {
                        assert!(
                            output["used_facts"]["prior_entries"]
                                .as_array()
                                .unwrap()
                                .iter()
                                .any(|v| v["key"] == reference["key"])
                        );
                        continue;
                    }
                    kind => panic!("unexpected ref {kind}"),
                };
                assert!(
                    output["used_facts"][array]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|v| v[key] == reference["id"])
                );
            }
        }
    }
}

#[test]
fn dropped_request_budget_cancels_blocking_control() {
    let budget = RequestBudget::new();
    let flag = budget.cancelled.clone();
    drop(budget);
    assert!(flag.load(std::sync::atomic::Ordering::Relaxed));
}
