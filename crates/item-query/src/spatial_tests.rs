use super::*;
use item_core::evidence::ReadControl;
use item_core::{DepthInput, ObservationEventInput, store::Store};
use std::{
    sync::{Arc, atomic::AtomicBool},
    time::Instant,
};
fn at(t: i64) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
        + Duration::seconds(t)
}
fn control() -> ReadControl {
    ReadControl::new(
        Instant::now() + std::time::Duration::from_secs(10),
        Arc::new(AtomicBool::new(false)),
    )
}
fn obs(s: &Store, label: &str, t: i64) -> i64 {
    s.record_sighting(
        "cam",
        "desk",
        label,
        at(t),
        None,
        std::time::Duration::from_secs(300),
    )
    .unwrap()
    .0
}
#[allow(clippy::too_many_arguments)]
fn ev(
    s: &Store,
    o: i64,
    label: &str,
    kind: &str,
    t: i64,
    reason: &str,
    session: Option<&str>,
    hits: i64,
    duration: Option<f64>,
    geometry: bool,
) -> i64 {
    let g = geometry.then(|| {
        s.record_geometry(
            o,
            &format!("event_{kind}_{t}"),
            at(t),
            [10.0, 10.0, 20.0, 20.0],
            Some(100),
            Some(100),
            "desk",
            "detector",
            Some(0.8),
            Some("C:/private.jpg"),
        )
        .unwrap()
    });
    s.append_observation_event(&ObservationEventInput {
        observation_id: o,
        camera_id: "cam".into(),
        zone: "desk".into(),
        label: label.into(),
        event_type: kind.into(),
        occurred_at: at(t),
        noticed_at: at(t),
        source: "detector".into(),
        session_id: session.map(str::to_owned),
        reason: reason.into(),
        hits,
        seen_for_s: duration,
        geometry_id: g,
    })
    .unwrap()
}
fn base() -> (Store, i64, i64, i64) {
    let s = Store::in_memory().unwrap();
    let t = obs(&s, "keys", 0);
    let c = obs(&s, "box", -1);
    let a = ev(
        &s,
        t,
        "keys",
        "disappeared",
        0,
        "missed_gap",
        Some("run"),
        2,
        Some(5.0),
        true,
    );
    ev(
        &s,
        c,
        "box",
        "appeared",
        -1,
        "new",
        Some("run"),
        2,
        Some(2.0),
        true,
    );
    ev(
        &s,
        c,
        "box",
        "observed",
        0,
        "present",
        Some("run"),
        3,
        Some(10.0),
        true,
    );
    (s, t, c, a)
}
fn configured() -> Priors {
    Priors::from_toml_str("[[container]]\nlabel='box'\ncamera='cam'\nzone='desk'\nrole='container'\nopening_state='open'\ninterior_length_m=[0.3,0.4]\ninterior_width_m=[0.3,0.4]\ninterior_height_m=[0.3,0.4]",format!("builtin:g4-priors-1+configured-default:sha256:{}","a".repeat(64))).unwrap()
}
fn lookup(s: &Store, t: i64, p: &Priors) -> SpatialLookup {
    let snap = s.read_snapshot(control()).unwrap();
    candidates_for_observation(&snap, t, None, SpatialOptions::default(), p).unwrap()
}
fn assert_resolved(x: &SpatialLookup) {
    for c in &x.candidates {
        assert!(c.evidence.len() <= 8);
        for e in &c.evidence {
            assert!(e.refs.len() <= 8);
            for r in &e.refs {
                match r {
                    EvidenceRef::Observation { id } => {
                        assert!(x.used_facts.observations.iter().any(|v| v.id == *id))
                    }
                    EvidenceRef::Event { id } => {
                        assert!(x.used_facts.events.iter().any(|v| v.id == *id))
                    }
                    EvidenceRef::Geometry { id } => {
                        assert!(x.used_facts.geometry.iter().any(|v| v.id == *id))
                    }
                    EvidenceRef::Depth { id } => {
                        assert!(x.used_facts.depth.iter().any(|v| v.id == *id))
                    }
                    EvidenceRef::PriorEntry { key, priors_ref } => assert!(
                        x.used_facts
                            .prior_entries
                            .iter()
                            .any(|v| v.key == *key && v.priors_ref == *priors_ref)
                    ),
                }
            }
        }
    }
}
#[test]
fn measured_container_has_full_real_selection_trace_and_resolvable_bundle() {
    let (s, t, c, a) = base();
    let x = lookup(&s, t, &configured());
    assert_eq!(x.candidates.len(), 1);
    let candidate = &x.candidates[0];
    assert_eq!(candidate.candidate.relation, "possibly_contained_in");
    assert_eq!(candidate.candidate.rule_version, RULE_VERSION);
    assert_ne!(
        candidate.trace.cover_geometry_event_id,
        candidate.candidate.cover_event_id
    );
    assert!(
        candidate
            .trace
            .persistence_event_ids
            .contains(&candidate.trace.cover_geometry_event_id)
    );
    assert_eq!(
        x.used_facts
            .observations
            .iter()
            .map(|o| o.id)
            .collect::<Vec<_>>(),
        vec![t, c]
    );
    assert!(x.used_facts.events.iter().any(|e| e.id == a));
    assert_eq!(x.used_facts.prior_entries.len(), 2);
    assert!(
        x.used_facts
            .geometry
            .iter()
            .all(|g| g.snapshot_ref.is_none())
    );
    assert_resolved(&x);
}
#[test]
fn legacy_relation_and_score_stay_unchanged_when_no_new_gate_triggers() {
    let (s, t, _, _) = base();
    let snapshot = s.read_snapshot(control()).unwrap();
    let target = snapshot.observation(t, None).unwrap();
    let anchor = snapshot
        .latest_departure_for_observation(&target)
        .unwrap()
        .unwrap();
    let scene = snapshot
        .scene_window(SceneFilter {
            camera: "cam",
            zone: "desk",
            session: "run",
            since: at(-120),
            until: at(120),
        })
        .unwrap();
    let legacy = crate::containment::find_candidates(
        &scene.events,
        "keys",
        Duration::seconds(120),
        &configured(),
    );
    let new =
        candidates_for_observation(&snapshot, t, None, SpatialOptions::default(), &configured())
            .unwrap();
    assert_eq!(legacy[0].score, new.candidates[0].candidate.score);
    assert_eq!(legacy[0].relation, new.candidates[0].candidate.relation);
    assert_eq!(new.anchor.unwrap().id, anchor.id);
    assert_eq!(legacy[0].rule_version, "g4-size-1");
}
#[test]
fn builtin_never_promotes_overlap_to_containment() {
    let (s, t, _, _) = base();
    let x = lookup(&s, t, Priors::builtin());
    assert_eq!(x.candidates[0].candidate.relation, "possibly_under");
    assert_eq!(x.used_facts.prior_entries[0].opening_state, Some("unclear"));
    assert_resolved(&x);
}
#[test]
fn latest_incomplete_anchor_does_not_fall_back() {
    let (s, t, _, a) = base();
    let newer = ev(
        &s,
        t,
        "keys",
        "disappeared",
        2,
        "missed_gap",
        None,
        2,
        None,
        false,
    );
    let x = lookup(&s, t, &configured());
    assert!(x.candidates.is_empty());
    assert_eq!(x.reason, Some("anchor_evidence_missing"));
    assert_eq!(x.anchor.unwrap().id, newer);
    assert_ne!(a, newer);
}
#[test]
fn direct_hit_after_anchor_suppresses_candidates() {
    let (s, t, _, _) = base();
    s.record_sighting(
        "cam",
        "desk",
        "keys",
        at(1),
        None,
        std::time::Duration::from_secs(300),
    )
    .unwrap();
    let x = lookup(&s, t, &configured());
    assert_eq!(x.reason, Some("target_seen_after_anchor"));
    assert!(x.candidates.is_empty());
}
#[test]
fn same_label_reappearance_outside_window_and_cross_zone_is_independent() {
    let (s, t, _, _) = base();
    let other = s
        .record_sighting(
            "cam",
            "other-zone",
            "KEYS",
            at(1000),
            None,
            std::time::Duration::from_secs(300),
        )
        .unwrap()
        .0;
    s.append_observation_event(&ObservationEventInput {
        observation_id: other,
        camera_id: "cam".into(),
        zone: "other-zone".into(),
        label: "KEYS".into(),
        event_type: "appeared".into(),
        occurred_at: at(1000),
        noticed_at: at(1000),
        source: "shutdown".into(),
        session_id: Some("run".into()),
        reason: "new".into(),
        hits: 1,
        seen_for_s: None,
        geometry_id: None,
    })
    .unwrap();
    let x = lookup(&s, t, &configured());
    assert_eq!(x.reason, Some("same_label_reappeared"));
}
#[test]
fn any_late_lifecycle_end_suppresses_without_revival() {
    for reason in ["missed_gap", "shutdown", "lost", "row-change"] {
        let (s, t, c, _) = base();
        ev(
            &s,
            c,
            "box",
            "disappeared",
            1000,
            reason,
            Some("run"),
            3,
            None,
            false,
        );
        ev(
            &s,
            c,
            "box",
            "appeared",
            1001,
            "new",
            Some("run"),
            3,
            None,
            false,
        );
        let x = lookup(&s, t, &configured());
        assert!(x.candidates.is_empty(), "{reason}");
        assert_eq!(x.diagnostic_counts.get("cover_lifecycle_ended"), Some(&1));
    }
}
#[test]
fn reopen_without_end_is_discontinuous_and_end_at_anchor_uses_legacy_gate() {
    let (s, t, c, _) = base();
    ev(
        &s,
        c,
        "box",
        "appeared",
        1000,
        "new",
        Some("run"),
        3,
        None,
        false,
    );
    let x = lookup(&s, t, &configured());
    assert!(x.candidates.is_empty());
    assert!(x.diagnostics.contains(&"cover_lifecycle_discontinuous"));
    let (s, t, c, _) = base();
    ev(
        &s,
        c,
        "box",
        "disappeared",
        0,
        "missed_gap",
        Some("run"),
        3,
        None,
        true,
    );
    let x = lookup(&s, t, &configured());
    assert!(x.candidates.is_empty());
    assert!(!x.diagnostics.contains(&"cover_lifecycle_ended"));
}
fn add_depth(s: &Store, g: i64, relation: &str, provider: &str, frame: &str) -> i64 {
    s.record_depth(g, provider, "relative", relation, Some(0.5), "good", None)
        .unwrap();
    let ids = s.depth_for_geometry(g).unwrap();
    let id = ids.last().unwrap().id; // writer's convenience legacy API lacks provenance, replace via typed input is unavailable here
    let _ = (frame, id);
    id
}
fn depth_input(relation: &str, provider: &str, frame: &str) -> DepthInput {
    DepthInput {
        provider: provider.into(),
        frame_id: frame.into(),
        response_id: "response1".into(),
        prompt_version: "v1".into(),
        backend_version: "v1".into(),
        mode: "relative".into(),
        relation_to_camera: relation.into(),
        relative_score: Some(0.5),
        quality: "usable".into(),
        model_ref: None,
        value_m: None,
        uncertainty_m: None,
        valid_fraction: None,
        coordinate_frame: None,
        calibration_ref: None,
    }
}
#[test]
fn depth_scan_limit_never_returns_positive_candidates() {
    let (s, t, _, a) = base();
    let snapshot = s.read_snapshot(control()).unwrap();
    let anchor = snapshot
        .latest_departure_for_observation(&snapshot.observation(t, None).unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(anchor.id, a);
    let g = anchor.geometry_id.unwrap();
    drop(snapshot);
    for _ in 0..5 {
        add_depth(&s, g, "nearer", "provider", "f");
    }
    let x = lookup(&s, t, &configured());
    assert!(!x.scan_complete);
    assert_eq!(x.reason, Some("evidence_scan_limit"));
    assert!(x.candidates.is_empty());
}
#[test]
fn selected_depth_ids_are_actual_and_provenance_gate_preserves_unknown() {
    let mut events = fixture_scene();
    events.sort_by_key(|e| match e.event.event_type.as_str() {
        "disappeared" => 0,
        "appeared" => 1,
        _ => 2,
    });
    let target_geo = events[0].geometry.as_ref().unwrap().id;
    let cover_geo = events[2].geometry.as_ref().unwrap().id;
    events[0].depth = vec![depth_evidence(
        71,
        target_geo,
        depth_input("nearer", "p", "frame"),
    )];
    events[2].depth = vec![depth_evidence(
        72,
        cover_geo,
        depth_input("farther", "p", "frame"),
    )];
    let x =
        crate::containment::evaluate(&events, "keys", Duration::seconds(120), &configured(), true);
    assert!(x.candidates.is_empty());
    assert_eq!(x.diagnostics.get("depth_conflict"), Some(&1));
    for change in ["provider", "frame", "mode"] {
        let mut e = events.clone();
        match change {
            "provider" => e[2].depth[0].provider = "other".into(),
            "frame" => e[2].depth[0].frame_id = "different".into(),
            _ => e[2].depth[0].mode = "metric".into(),
        };
        let result =
            crate::containment::evaluate(&e, "keys", Duration::seconds(120), &configured(), true);
        assert_eq!(result.candidates.len(), 1);
        assert_eq!(result.candidates[0].trace.target_depth_id, Some(71));
        assert_eq!(result.candidates[0].trace.cover_depth_id, Some(72));
        assert_eq!(
            result.candidates[0]
                .candidate
                .evidence
                .iter()
                .find(|c| c.kind == "depth")
                .unwrap()
                .state,
            "unknown"
        );
    }
}
#[test]
fn cpu_rule_evaluation_observes_cancellation_and_unsafe_provenance_stays_unknown() {
    let scene = fixture_scene();
    let result = crate::containment::evaluate_anchor_with_check(
        &scene,
        "keys",
        scene
            .iter()
            .find(|e| e.event.event_type == "disappeared")
            .unwrap()
            .event
            .id,
        Duration::seconds(120),
        Priors::builtin(),
        || Err(StoreError::ReadCancelled),
    );
    assert!(matches!(result, Err(StoreError::ReadCancelled)));
    for unsafe_value in [
        "C:/private.jpg",
        "https://provider.example",
        "Bearer-abc",
        "TOKEN123",
        "api_key_secret",
    ] {
        assert!(!crate::containment::safe_provenance(unsafe_value));
    }
}
#[test]
fn unsafe_priors_reference_is_not_exposed() {
    let (s, t, _, _) = base();
    let unsafe_priors = Priors::from_toml_str("", "C:/private.toml".into()).unwrap();
    let snap = s.read_snapshot(control()).unwrap();
    assert!(matches!(
        candidates_for_observation(&snap, t, None, SpatialOptions::default(), &unsafe_priors),
        Err(StoreError::InvalidInput(_))
    ));
}
fn fixture_scene() -> Vec<SceneEvent> {
    let (s, _, _, _) = base();
    let snap = s.read_snapshot(control()).unwrap();
    snap.scene_window(SceneFilter {
        camera: "cam",
        zone: "desk",
        session: "run",
        since: at(-120),
        until: at(120),
    })
    .unwrap()
    .events
    .into_iter()
    .collect()
}
fn depth_evidence(id: i64, g: i64, d: DepthInput) -> DepthEvidence {
    DepthEvidence {
        id,
        geometry_id: g,
        provider: d.provider,
        frame_id: d.frame_id,
        response_id: d.response_id,
        prompt_version: d.prompt_version,
        backend_version: d.backend_version,
        mode: d.mode,
        relation_to_camera: d.relation_to_camera,
        relative_score: d.relative_score,
        quality: d.quality,
        model_ref: None,
        value_m: None,
        uncertainty_m: None,
        valid_fraction: None,
        coordinate_frame: None,
        calibration_ref: None,
    }
}
#[test]
fn byte_tail_removal_preserves_resolvable_bundle_and_empty_has_no_used_facts() {
    let (s, t, _, _) = base();
    let mut x = lookup(&s, t, &configured());
    assert!(x.remove_tail_candidate());
    assert!(x.used_facts.events.is_empty());
    assert!(x.used_facts.observations.is_empty());
    assert!(x.used_facts.prior_entries.is_empty());
    assert!(x.candidates_truncated);
    assert_resolved(&x);
    assert!(!x.remove_tail_candidate());
}
#[test]
fn window_affects_qualification_and_temporal_score() {
    let (s, t, _, _) = base();
    let snap = s.read_snapshot(control()).unwrap();
    let a = candidates_for_observation(
        &snap,
        t,
        None,
        SpatialOptions {
            window_secs: 2,
            limit: 20,
        },
        &configured(),
    )
    .unwrap();
    let b = candidates_for_observation(
        &snap,
        t,
        None,
        SpatialOptions {
            window_secs: 120,
            limit: 20,
        },
        &configured(),
    )
    .unwrap();
    assert!(a.candidates[0].candidate.score < b.candidates[0].candidate.score);
    drop(snap);
    let c = obs(&s, "book", -1000);
    ev(
        &s,
        c,
        "book",
        "appeared",
        -1000,
        "new",
        Some("run"),
        2,
        Some(10.0),
        true,
    );
    let snap = s.read_snapshot(control()).unwrap();
    let wide = candidates_for_observation(
        &snap,
        t,
        None,
        SpatialOptions {
            window_secs: 3600,
            limit: 1,
        },
        Priors::builtin(),
    )
    .unwrap();
    assert!(wide.candidates_truncated);
    assert_eq!(wide.candidates.len(), 1);
    assert!(wide.scan_complete);
    assert_resolved(&wide);
}
