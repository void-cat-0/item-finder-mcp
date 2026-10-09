//! Conservative disappearance/cover candidate rules: G2 2-D matching, G4 size
//! and affordance evidence.
//!
//! These are deterministic hypotheses, not calibrated probabilities. The
//! strongest relation, `possibly_contained_in`, still only says "may be
//! inside"; it needs a container recorded as open with a measured interior the
//! target fits (see `affordance`). Missing depth or size evidence stays unknown.

use chrono::{DateTime, Duration, Utc};
use item_core::{DepthEvidence, ObservationGeometry, SceneEvent};
use serde::Serialize;

use crate::affordance::{Priors, assess, is_person_label};

pub const RULE_VERSION: &str = "g4-size-1";
pub const MIN_COVER_HITS: i64 = 2;
pub const MIN_COVER_PERSISTENCE: Duration = Duration::seconds(2);

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct EvidenceContribution {
    pub kind: &'static str,
    pub state: &'static str,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Candidate {
    pub target_event_id: i64,
    pub cover_event_id: i64,
    pub target_observation_id: i64,
    pub cover_observation_id: i64,
    pub target_geometry_id: Option<i64>,
    pub cover_geometry_id: Option<i64>,
    pub target_label: String,
    pub cover_label: String,
    /// When the target was last directly seen (its last hit) and when the
    /// cover appeared; the card's timeline is built from these two facts.
    pub target_last_hit: DateTime<Utc>,
    pub cover_appeared_at: DateTime<Utc>,
    pub camera_id: String,
    pub zone: String,
    pub relation: &'static str,
    /// Deterministic heuristic score. It is not a probability or calibrated
    /// confidence and should only be used to order candidates.
    pub score: f32,
    pub same_label_ambiguous: bool,
    pub rule_version: &'static str,
    /// Which size/container priors produced the G4 evidence.
    pub priors_ref: String,
    pub explanation: String,
    pub evidence: Vec<EvidenceContribution>,
}

/// Find conservative 2-D hypotheses from authoritative lifecycle events.
///
/// The slice may contain a bounded scene window. A candidate requires a target
/// `missed_gap` departure and a persistent, geometrically related cover in the
/// same camera, zone, and ingest session. The temporal window is measured from
/// the target's last hit (`occurred_at`), never from the delayed sweep time
/// (`noticed_at`).
pub fn find_candidates(
    events: &[SceneEvent],
    target_label: &str,
    window: Duration,
    priors: &Priors,
) -> Vec<Candidate> {
    evaluate(events, target_label, window, priors, false)
        .candidates
        .into_iter()
        .map(|c| c.candidate)
        .collect()
}

/// The trace is produced at the actual selection sites, never reconstructed from output IDs.
#[derive(Debug, Clone, Serialize)]
pub struct SelectionTrace {
    pub cover_geometry_event_id: i64,
    pub persistence_event_ids: Vec<i64>,
    pub target_depth_id: Option<i64>,
    pub cover_depth_id: Option<i64>,
}
#[derive(Debug, Clone)]
pub struct AuditedCandidate {
    pub candidate: Candidate,
    pub trace: SelectionTrace,
}
#[derive(Debug, Default)]
pub struct Evaluation {
    pub candidates: Vec<AuditedCandidate>,
    pub diagnostics: std::collections::BTreeMap<&'static str, usize>,
}

pub fn evaluate(
    events: &[SceneEvent],
    target_label: &str,
    window: Duration,
    priors: &Priors,
    strict_depth: bool,
) -> Evaluation {
    evaluate_with_check(
        events,
        target_label,
        window,
        priors,
        strict_depth,
        || Ok(()),
    )
    .expect("infallible rule evaluation")
}

pub fn evaluate_with_check(
    events: &[SceneEvent],
    target_label: &str,
    window: Duration,
    priors: &Priors,
    strict_depth: bool,
    check: impl Fn() -> item_core::store::Result<()>,
) -> item_core::store::Result<Evaluation> {
    evaluate_selected(
        events,
        target_label,
        None,
        window,
        priors,
        strict_depth,
        check,
    )
}

pub fn evaluate_anchor_with_check(
    events: &[SceneEvent],
    target_label: &str,
    anchor_id: i64,
    window: Duration,
    priors: &Priors,
    check: impl Fn() -> item_core::store::Result<()>,
) -> item_core::store::Result<Evaluation> {
    evaluate_selected(
        events,
        target_label,
        Some(anchor_id),
        window,
        priors,
        true,
        check,
    )
}

fn evaluate_selected(
    events: &[SceneEvent],
    target_label: &str,
    anchor_id: Option<i64>,
    window: Duration,
    priors: &Priors,
    strict_depth: bool,
    check: impl Fn() -> item_core::store::Result<()>,
) -> item_core::store::Result<Evaluation> {
    let window = window.max(Duration::zero());
    let mut out = Vec::new();
    let mut diagnostics = std::collections::BTreeMap::new();

    for target in events.iter().filter(|event| {
        is_target(event, target_label) && anchor_id.is_none_or(|id| event.event.id == id)
    }) {
        check()?;
        let Some(target_geo) = valid_geometry(target.geometry.as_ref()) else {
            continue;
        };
        let Some(session_id) = target.event.session_id.as_deref() else {
            continue;
        };
        if target_reappeared(events, target, session_id) {
            continue;
        }

        for cover_appearance in events.iter().filter(|event| {
            is_cover_appearance(event, target, session_id)
                && within_last_hit_window(event.event.occurred_at, target.event.occurred_at, window)
        }) {
            check()?;
            let Some((cover, persistence)) =
                cover_lifecycle(events, cover_appearance, target, window, strict_depth)
            else {
                continue;
            };
            let Some(cover_geo) = valid_geometry(cover.geometry.as_ref()) else {
                continue;
            };
            let Some(overlap) = geometry_overlap(target_geo, cover_geo) else {
                continue;
            };
            if overlap.coverage < 0.15 && overlap.iou < 0.05 {
                continue;
            }

            let depth = depth_compatibility(target, cover, target_geo, cover_geo, strict_depth);
            if depth.state == "conflicting" {
                *diagnostics.entry("depth_conflict").or_insert(0) += 1;
                continue;
            }
            let selected_depth_ids = (depth.target_depth_id, depth.cover_depth_id);

            let same_label_ambiguous = target.event.label.eq_ignore_ascii_case(&cover.event.label);
            let person = is_person_label(&cover.event.label);
            let container_prior = if person {
                None
            } else {
                priors.container(
                    &cover.event.label,
                    &target.event.camera_id,
                    &target_geo.zone,
                )
            };
            let cover_prior = container_prior.is_some();
            let assessment = container_prior.map(|container| {
                assess(
                    &target.event.label,
                    priors.object(&target.event.label),
                    container,
                )
            });
            // Identity has to be settled before a size fit can mean anything,
            // and a cover's geometry must at least reach over most of the
            // target's last box before "inside" is worth saying.
            let contained = assessment.as_ref().is_some_and(|a| a.promotable)
                && !same_label_ambiguous
                && overlap.coverage >= 0.5;
            let relation = if contained {
                "possibly_contained_in"
            } else if cover_prior {
                "possibly_under"
            } else {
                "possibly_occluded_by"
            };
            let temporal_distance = target
                .event
                .occurred_at
                .signed_duration_since(cover_appearance.event.occurred_at)
                .num_milliseconds()
                .unsigned_abs() as f32
                / window.num_milliseconds().max(1) as f32;
            let temporal_score = (1.0 - temporal_distance.min(1.0)).max(0.0);
            let overlap_score = (overlap.coverage.max(overlap.iou)).clamp(0.0, 1.0);
            let persistence_score = (persistence.seconds / 10.0).clamp(0.0, 1.0);
            let semantic_score = if cover_prior { 1.0 } else { 0.0 };
            let score = (temporal_score * 0.25
                + overlap_score * 0.35
                + persistence_score * 0.25
                + semantic_score * 0.15)
                .clamp(0.0, 1.0);

            let mut evidence = vec![
                EvidenceContribution {
                    kind: "temporal_proximity",
                    state: "supporting",
                    detail: {
                        let offset = cover_appearance
                            .event
                            .occurred_at
                            .signed_duration_since(target.event.occurred_at);
                        if offset < Duration::zero() {
                            format!(
                                "cover appeared {} before the target's last hit",
                                format_delta(-offset)
                            )
                        } else {
                            format!(
                                "cover appeared {} after the target's last hit",
                                format_delta(offset)
                            )
                        }
                    },
                },
                EvidenceContribution {
                    kind: "projected_overlap",
                    state: "supporting",
                    detail: format!(
                        "target coverage {:.0}%, IoU {:.0}%",
                        overlap.coverage * 100.0,
                        overlap.iou * 100.0
                    ),
                },
                EvidenceContribution {
                    kind: "persistence",
                    state: "supporting",
                    detail: format!(
                        "cover remained observable for {} with at least {} hits",
                        format_delta(Duration::milliseconds(
                            (persistence.seconds * 1000.0).round() as i64
                        )),
                        persistence.hits
                    ),
                },
                EvidenceContribution {
                    kind: "depth",
                    state: depth.state,
                    detail: depth.detail,
                },
            ];
            if same_label_ambiguous {
                evidence.push(EvidenceContribution {
                    kind: "identity",
                    state: "unknown",
                    detail: "cover and target have the same label; identity is ambiguous".into(),
                });
            }
            match &assessment {
                Some(assessment) => {
                    evidence.push(EvidenceContribution {
                        kind: "semantic_affordance",
                        state: assessment.affordance.state,
                        detail: assessment.affordance.detail.clone(),
                    });
                    if let Some(size) = &assessment.size {
                        evidence.push(EvidenceContribution {
                            kind: "size",
                            state: size.state,
                            detail: size.detail.clone(),
                        });
                    }
                    if assessment.promotable && !contained {
                        evidence.push(EvidenceContribution {
                            kind: "containment_gate",
                            state: "unknown",
                            detail: if same_label_ambiguous {
                                "the size fit is not used: target and cover identity are ambiguous"
                                    .into()
                            } else {
                                format!(
                                    "the cover reaches over only {:.0}% of the target's last box (containment needs 50%)",
                                    overlap.coverage * 100.0
                                )
                            },
                        });
                    }
                }
                None => evidence.push(EvidenceContribution {
                    kind: "semantic_affordance",
                    state: "unknown",
                    detail: if person {
                        "a person or hand can only occlude; relation remains occlusion".into()
                    } else {
                        "cover label has no container/cover prior; relation remains occlusion"
                            .into()
                    },
                }),
            }

            let explanation = if contained {
                format!(
                    "{} was last directly seen near {}; {} (recorded as an open container whose interior fits it) then covered {:.0}% of that image area and persisted. The {} may be inside it; this is a qualified hypothesis, not an observation.",
                    target.event.label,
                    target_geo.zone,
                    cover.event.label,
                    overlap.coverage * 100.0,
                    target.event.label,
                )
            } else {
                format!(
                    "{} was last directly seen near {}; {} then covered {:.0}% of that image area and persisted. This is a 2-D visibility hypothesis, not proof of containment.",
                    target.event.label,
                    target_geo.zone,
                    cover.event.label,
                    overlap.coverage * 100.0
                )
            };

            let trace = SelectionTrace {
                cover_geometry_event_id: cover.event.id,
                persistence_event_ids: persistence.event_ids.clone(),
                target_depth_id: selected_depth_ids.0,
                cover_depth_id: selected_depth_ids.1,
            };
            out.push(AuditedCandidate {
                trace,
                candidate: Candidate {
                    target_event_id: target.event.id,
                    cover_event_id: cover_appearance.event.id,
                    target_observation_id: target.event.observation_id,
                    cover_observation_id: cover.event.observation_id,
                    target_geometry_id: target.geometry.as_ref().map(|geometry| geometry.id),
                    cover_geometry_id: cover.geometry.as_ref().map(|geometry| geometry.id),
                    target_label: target.event.label.clone(),
                    cover_label: cover.event.label.clone(),
                    target_last_hit: target.event.occurred_at,
                    cover_appeared_at: cover_appearance.event.occurred_at,
                    camera_id: target.event.camera_id.clone(),
                    zone: target_geo.zone.clone(),
                    relation,
                    score,
                    same_label_ambiguous,
                    rule_version: RULE_VERSION,
                    priors_ref: priors.reference().to_owned(),
                    explanation,
                    evidence,
                },
            });
        }
    }

    out.sort_by(|left, right| {
        right
            .candidate
            .score
            .total_cmp(&left.candidate.score)
            .then_with(|| {
                left.candidate
                    .target_event_id
                    .cmp(&right.candidate.target_event_id)
            })
            .then_with(|| {
                left.candidate
                    .cover_event_id
                    .cmp(&right.candidate.cover_event_id)
            })
            .then_with(|| {
                left.candidate
                    .cover_geometry_id
                    .cmp(&right.candidate.cover_geometry_id)
            })
    });
    Ok(Evaluation {
        candidates: out,
        diagnostics,
    })
}

fn is_target(event: &SceneEvent, target_label: &str) -> bool {
    event.event.label.eq_ignore_ascii_case(target_label)
        && event.event.event_type == "disappeared"
        && event.event.reason == "missed_gap"
        && !excluded_event(event)
}

fn is_cover_appearance(event: &SceneEvent, target: &SceneEvent, session_id: &str) -> bool {
    event.event.event_type == "appeared"
        && event.event.camera_id == target.event.camera_id
        && event.event.zone == target.event.zone
        && event.event.session_id.as_deref() == Some(session_id)
        && event.event.observation_id != target.event.observation_id
        && !excluded_event(event)
}

fn excluded_event(event: &SceneEvent) -> bool {
    excluded_source(&event.event.source) || excluded_source(&event.event.reason)
}

fn excluded_source(source: &str) -> bool {
    matches!(
        source
            .to_ascii_lowercase()
            .replace([' ', '-'], "_")
            .as_str(),
        "lost" | "shutdown" | "rowchange" | "row_changed"
    )
}

fn target_reappeared(events: &[SceneEvent], target: &SceneEvent, session_id: &str) -> bool {
    events.iter().any(|event| {
        event.event.camera_id == target.event.camera_id
            && event.event.session_id.as_deref() == Some(session_id)
            && event.event.label.eq_ignore_ascii_case(&target.event.label)
            && event.event.occurred_at > target.event.occurred_at
            && event.event.event_type == "appeared"
            && !excluded_event(event)
    })
}

/// Select the lifecycle record that proves a cover persisted and the geometry
/// nearest the target's last hit, together with the persistence the whole
/// lifecycle shows. A later `disappeared` record is useful: it is the
/// immutable last/closed geometry for a pre-existing cover, and it carries the
/// cover's final hit count and residence time.
fn cover_lifecycle<'a>(
    events: &'a [SceneEvent],
    appearance: &'a SceneEvent,
    target: &SceneEvent,
    window: Duration,
    strict: bool,
) -> Option<(&'a SceneEvent, Persistence)> {
    let mut related: Vec<&SceneEvent> = events
        .iter()
        .filter(|event| {
            event.event.observation_id == appearance.event.observation_id
                && event.event.camera_id == appearance.event.camera_id
                && event.event.zone == appearance.event.zone
                && event.event.session_id == appearance.event.session_id
                && !excluded_event(event)
                && (!strict
                    || (event.event.occurred_at, event.event.id)
                        >= (appearance.event.occurred_at, appearance.event.id))
                && valid_geometry(event.geometry.as_ref()).is_some()
        })
        .collect();
    related.sort_by_key(|event| (event.event.occurred_at, event.event.id));

    let last = related.last().copied()?;
    let duration = last
        .event
        .occurred_at
        .signed_duration_since(appearance.event.occurred_at);
    let has_declared_persistence = related.iter().any(|event| {
        event.event.seen_for_s.unwrap_or_default() >= MIN_COVER_PERSISTENCE.num_seconds() as f64
            || event.event.hits >= MIN_COVER_HITS
    });
    let has_lifecycle_persistence = duration >= MIN_COVER_PERSISTENCE;
    let preexisting_nearby = appearance.event.occurred_at < target.event.occurred_at
        && target
            .event
            .occurred_at
            .signed_duration_since(appearance.event.occurred_at)
            <= window;
    let future_close_nearby = last.event.occurred_at >= target.event.occurred_at
        && last.event.occurred_at <= target.event.occurred_at + window;
    if !(has_declared_persistence || has_lifecycle_persistence) {
        return None;
    }
    if !preexisting_nearby && !future_close_nearby {
        return None;
    }
    if related.iter().any(|event| {
        event.event.event_type == "disappeared"
            && event.event.occurred_at <= target.event.occurred_at
            && event.event.occurred_at > appearance.event.occurred_at
    }) {
        return None;
    }

    // Only the actual extrema/declared witnesses influence persistence and score.
    let max_duration = related
        .iter()
        .filter(|e| e.event.seen_for_s.is_some())
        .max_by(|a, b| {
            a.event
                .seen_for_s
                .unwrap_or_default()
                .total_cmp(&b.event.seen_for_s.unwrap_or_default())
        });
    let max_hits = related.iter().max_by_key(|e| e.event.hits);
    let mut witness_ids = vec![appearance.event.id, last.event.id];
    if let Some(e) = max_duration {
        witness_ids.push(e.event.id);
    }
    if let Some(e) = max_hits {
        witness_ids.push(e.event.id);
    }
    witness_ids.sort_unstable();
    witness_ids.dedup();
    let persistence = Persistence {
        seconds: related
            .iter()
            .filter_map(|event| event.event.seen_for_s)
            .map(|seconds| seconds as f32)
            .fold(duration.num_milliseconds().max(0) as f32 / 1000.0, f32::max),
        hits: related
            .iter()
            .map(|event| event.event.hits)
            .max()
            .unwrap_or(0),
        event_ids: witness_ids,
    };

    // Prefer geometry at or just before the target, otherwise the appearance
    // geometry. This permits a closed/last sample without comparing frames.
    related
        .into_iter()
        .filter(|event| event.event.occurred_at <= target.event.occurred_at)
        .max_by_key(|event| event.event.occurred_at)
        .or_else(|| {
            events
                .iter()
                .find(|event| event.event.id == appearance.event.id)
        })
        .map(|geometry| (geometry, persistence))
}

#[derive(Debug, Clone)]
struct Persistence {
    seconds: f32,
    hits: i64,
    event_ids: Vec<i64>,
}

#[derive(Debug, Clone, Copy)]
struct Overlap {
    coverage: f32,
    iou: f32,
}

fn valid_geometry(geometry: Option<&ObservationGeometry>) -> Option<&ObservationGeometry> {
    let geometry = geometry?;
    let width = geometry.frame_width?;
    let height = geometry.frame_height?;
    if width == 0 || height == 0 || geometry.bbox.iter().any(|value| !value.is_finite()) {
        return None;
    }
    let [x0, y0, x1, y1] = geometry.bbox;
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    Some(geometry)
}

fn geometry_overlap(a: &ObservationGeometry, b: &ObservationGeometry) -> Option<Overlap> {
    if a.frame_width != b.frame_width || a.frame_height != b.frame_height {
        return None;
    }
    let ix0 = a.bbox[0].max(b.bbox[0]);
    let iy0 = a.bbox[1].max(b.bbox[1]);
    let ix1 = a.bbox[2].min(b.bbox[2]);
    let iy1 = a.bbox[3].min(b.bbox[3]);
    let intersection = (ix1 - ix0).max(0.0) * (iy1 - iy0).max(0.0);
    let area_a = (a.bbox[2] - a.bbox[0]) * (a.bbox[3] - a.bbox[1]);
    let area_b = (b.bbox[2] - b.bbox[0]) * (b.bbox[3] - b.bbox[1]);
    if area_a <= 0.0 || area_b <= 0.0 || !area_a.is_finite() || !area_b.is_finite() {
        return None;
    }
    let union = area_a + area_b - intersection;
    Some(Overlap {
        coverage: intersection / area_a,
        iou: if union > 0.0 {
            intersection / union
        } else {
            0.0
        },
    })
}

#[derive(Debug)]
struct DepthCompatibility {
    state: &'static str,
    detail: String,
    target_depth_id: Option<i64>,
    cover_depth_id: Option<i64>,
}

fn depth_compatibility(
    target: &SceneEvent,
    cover: &SceneEvent,
    target_geo: &ObservationGeometry,
    cover_geo: &ObservationGeometry,
    strict: bool,
) -> DepthCompatibility {
    if target_geo.captured_at != cover_geo.captured_at {
        return DepthCompatibility {
            state: "unknown",
            target_depth_id: None,
            cover_depth_id: None,
            detail: "depth not compared: target and cover geometry came from different frames"
                .into(),
        };
    }
    let (Some(target_depth), Some(cover_depth)) =
        (usable_depth(&target.depth), usable_depth(&cover.depth))
    else {
        return DepthCompatibility {
            state: "unknown",
            target_depth_id: None,
            cover_depth_id: None,
            detail: "no comparable same-frame relative depth evidence".into(),
        };
    };
    if target_depth.response_id.is_empty() || target_depth.response_id != cover_depth.response_id {
        return DepthCompatibility {
            state: "unknown",
            target_depth_id: Some(target_depth.id),
            cover_depth_id: Some(cover_depth.id),
            detail: "depth not compared: samples came from different provider responses".into(),
        };
    }
    if strict
        && (target_depth.mode != "relative"
            || cover_depth.mode != "relative"
            || !safe_provenance(&target_depth.frame_id)
            || target_depth.frame_id != cover_depth.frame_id
            || !safe_provenance(&target_depth.provider)
            || target_depth.provider != cover_depth.provider
            || !safe_provenance(&target_depth.response_id)
            || !safe_provenance(&target_depth.prompt_version)
            || !safe_provenance(&cover_depth.prompt_version)
            || !safe_provenance(&target_depth.backend_version)
            || !safe_provenance(&cover_depth.backend_version))
    {
        return DepthCompatibility {
            state: "unknown",
            detail: "depth provenance is missing or incompatible".into(),
            target_depth_id: Some(target_depth.id),
            cover_depth_id: Some(cover_depth.id),
        };
    }
    let target_farther = target_depth.relation_to_camera == "farther";
    let cover_nearer = cover_depth.relation_to_camera == "nearer";
    let target_nearer = target_depth.relation_to_camera == "nearer";
    let cover_farther = cover_depth.relation_to_camera == "farther";
    if target_nearer && cover_farther {
        DepthCompatibility {
            state: "conflicting",
            target_depth_id: Some(target_depth.id),
            cover_depth_id: Some(cover_depth.id),
            detail: "same-frame depth places the target in front of the cover".into(),
        }
    } else if target_farther && cover_nearer {
        DepthCompatibility {
            state: "supporting",
            target_depth_id: Some(target_depth.id),
            cover_depth_id: Some(cover_depth.id),
            detail: "same-frame depth places the cover in front of the target".into(),
        }
    } else {
        DepthCompatibility {
            state: "unknown",
            target_depth_id: Some(target_depth.id),
            cover_depth_id: Some(cover_depth.id),
            detail: "same-frame depth has no decisive ordering".into(),
        }
    }
}

/// Logical provenance only: references resembling paths, URLs or credentials are unusable.
pub fn safe_provenance(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    !value.is_empty()
        && value.len() <= 256
        && value
            .chars()
            .all(|c| c.is_alphanumeric() || "._-+".contains(c))
        && !["token", "password", "secret", "bearer", "api_key", "apikey"]
            .iter()
            .any(|s| lower.contains(s))
}

fn usable_depth(depth: &[DepthEvidence]) -> Option<&DepthEvidence> {
    depth.iter().find(|evidence| {
        evidence.quality != "invalid"
            && matches!(
                evidence.relation_to_camera.as_str(),
                "nearer" | "farther" | "same_plane" | "overlapping_depth"
            )
    })
}

fn within_last_hit_window(at: DateTime<Utc>, last_hit: DateTime<Utc>, window: Duration) -> bool {
    let delta = at.signed_duration_since(last_hit);
    delta >= -window && delta <= window
}

fn format_delta(delta: Duration) -> String {
    let seconds = delta.num_milliseconds() as f64 / 1000.0;
    format!("{seconds:.1}s")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use item_core::{ObservationEvent, ObservationEventInput, ObservationGeometry};

    fn at(seconds: i64) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 30, 12, 0, 0).unwrap() + Duration::seconds(seconds)
    }

    #[allow(clippy::too_many_arguments)]
    fn event(
        id: i64,
        observation_id: i64,
        label: &str,
        kind: &str,
        seconds: i64,
        reason: &str,
        session: Option<&str>,
        bbox: [f32; 4],
        hits: i64,
        seen_for_s: Option<f64>,
    ) -> SceneEvent {
        let timestamp = at(seconds);
        SceneEvent {
            event: ObservationEvent {
                id,
                observation_id,
                camera_id: "cam".into(),
                zone: "desk".into(),
                label: label.into(),
                event_type: kind.into(),
                occurred_at: timestamp,
                noticed_at: timestamp + Duration::seconds(30),
                source: "local_detector".into(),
                session_id: session.map(str::to_owned),
                reason: reason.into(),
                hits,
                seen_for_s,
                geometry_id: Some(id),
            },
            geometry: Some(ObservationGeometry {
                id,
                observation_id,
                sample_kind: kind.into(),
                captured_at: timestamp,
                bbox,
                frame_width: Some(100),
                frame_height: Some(100),
                zone: "desk".into(),
                source: "local_detector".into(),
                confidence: Some(0.9),
                snapshot_ref: None,
            }),
            depth: vec![],
        }
    }

    fn find(events: &[SceneEvent]) -> Vec<Candidate> {
        find_candidates(events, "keys", Duration::seconds(10), Priors::builtin())
    }

    fn find_with(events: &[SceneEvent], priors: &Priors) -> Vec<Candidate> {
        find_candidates(events, "keys", Duration::seconds(10), priors)
    }

    /// Keys vanish at t=0; a cover labelled `cover` was there from t=-1 and
    /// closed at t=3 with `cover_bbox` over the keys' [10,10,40,40] box.
    fn keys_then(cover: &str, cover_bbox: [f32; 4]) -> Vec<SceneEvent> {
        vec![
            event(
                1,
                1,
                "keys",
                "disappeared",
                0,
                "missed_gap",
                Some("s"),
                [10.0, 10.0, 40.0, 40.0],
                3,
                Some(3.0),
            ),
            event(
                2,
                2,
                cover,
                "appeared",
                -1,
                "appeared",
                Some("s"),
                cover_bbox,
                1,
                None,
            ),
            event(
                3,
                2,
                cover,
                "disappeared",
                3,
                "missed_gap",
                Some("s"),
                cover_bbox,
                3,
                Some(4.0),
            ),
        ]
    }

    fn priors(text: &str) -> Priors {
        Priors::from_toml_str(text, "test-priors".into()).unwrap()
    }

    const OPEN_SHOEBOX: &str = "[[container]]\nlabel = \"box\"\ncamera = \"cam\"\nrole = \"container\"\n\
        opening_state = \"open\"\ninterior_length_m = [0.28, 0.30]\ninterior_width_m = [0.17, 0.18]\n\
        interior_height_m = [0.09, 0.10]\n";

    fn evidence<'a>(candidate: &'a Candidate, kind: &str) -> Option<&'a EvidenceContribution> {
        candidate
            .evidence
            .iter()
            .find(|evidence| evidence.kind == kind)
    }

    #[test]
    fn an_open_measured_container_that_fits_promotes_to_possibly_contained_in() {
        let candidates = find_with(
            &keys_then("box", [5.0, 5.0, 50.0, 50.0]),
            &priors(OPEN_SHOEBOX),
        );
        assert_eq!(candidates.len(), 1);
        let candidate = &candidates[0];
        assert_eq!(candidate.relation, "possibly_contained_in");
        assert_eq!(candidate.rule_version, "g4-size-1");
        assert_eq!(candidate.priors_ref, "test-priors");
        assert_eq!(evidence(candidate, "size").unwrap().state, "supporting");
        assert_eq!(
            evidence(candidate, "semantic_affordance").unwrap().state,
            "supporting"
        );
        assert!(candidate.explanation.contains("may be inside"));
        assert!(candidate.explanation.contains("not an observation"));
    }

    #[test]
    fn builtin_priors_alone_never_claim_containment() {
        let candidates = find(&keys_then("box", [5.0, 5.0, 50.0, 50.0]));
        assert_eq!(candidates[0].relation, "possibly_under");
        assert_eq!(evidence(&candidates[0], "size").unwrap().state, "unknown");
        assert_eq!(
            evidence(&candidates[0], "semantic_affordance")
                .unwrap()
                .state,
            "unknown"
        );
        assert_eq!(
            candidates[0].priors_ref,
            crate::affordance::BUILTIN_PRIORS_REF
        );
    }

    #[test]
    fn closed_or_unclear_or_too_small_containers_stay_under() {
        let closed = OPEN_SHOEBOX.replace("\"open\"", "\"closed\"");
        let candidates = find_with(&keys_then("box", [5.0, 5.0, 50.0, 50.0]), &priors(&closed));
        assert_eq!(candidates[0].relation, "possibly_under");

        let unclear = OPEN_SHOEBOX.replace("\"open\"", "\"unclear\"");
        let candidates = find_with(&keys_then("box", [5.0, 5.0, 50.0, 50.0]), &priors(&unclear));
        assert_eq!(candidates[0].relation, "possibly_under");

        let ring_box = "[[container]]\nlabel = \"box\"\nrole = \"container\"\nopening_state = \"open\"\n\
            interior_length_m = [0.03, 0.04]\ninterior_width_m = [0.03, 0.04]\ninterior_height_m = [0.02, 0.03]\n\
            [[object]]\nlabel = \"keys\"\nlength_m = [0.08, 0.10]\n";
        let candidates = find_with(&keys_then("box", [5.0, 5.0, 50.0, 50.0]), &priors(ring_box));
        assert_eq!(
            candidates.len(),
            1,
            "a size conflict does not hide the cover hypothesis"
        );
        assert_eq!(candidates[0].relation, "possibly_under");
        assert_eq!(
            evidence(&candidates[0], "size").unwrap().state,
            "conflicting"
        );
    }

    #[test]
    fn the_container_scope_must_match_the_targets_camera() {
        let elsewhere = OPEN_SHOEBOX.replace("camera = \"cam\"", "camera = \"hall\"");
        let candidates = find_with(
            &keys_then("box", [5.0, 5.0, 50.0, 50.0]),
            &priors(&elsewhere),
        );
        assert_eq!(
            candidates[0].relation, "possibly_under",
            "the built-in box applies instead"
        );
    }

    #[test]
    fn a_cover_role_or_a_glancing_overlap_blocks_promotion() {
        let candidates = find_with(
            &keys_then("book", [5.0, 5.0, 50.0, 50.0]),
            &priors(OPEN_SHOEBOX),
        );
        assert_eq!(candidates[0].relation, "possibly_under");
        assert_eq!(
            evidence(&candidates[0], "semantic_affordance")
                .unwrap()
                .state,
            "conflicting"
        );
        assert!(evidence(&candidates[0], "size").is_none());

        // The box reaches over 20% of the keys' box: still a cover candidate,
        // but not enough to say "inside".
        let candidates = find_with(
            &keys_then("box", [34.0, 5.0, 60.0, 60.0]),
            &priors(OPEN_SHOEBOX),
        );
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].relation, "possibly_under");
        let gate = evidence(&candidates[0], "containment_gate").unwrap();
        assert!(gate.detail.contains("50%"), "{}", gate.detail);
    }

    #[test]
    fn a_file_entry_can_make_a_new_label_a_cover_but_a_person_stays_an_occluder() {
        let towel = "[[container]]\nlabel = \"towel\"\nrole = \"cover\"\n";
        let candidates = find_with(&keys_then("towel", [5.0, 5.0, 50.0, 50.0]), &priors(towel));
        assert_eq!(candidates[0].relation, "possibly_under");

        let candidates = find(&keys_then("towel", [5.0, 5.0, 50.0, 50.0]));
        assert_eq!(candidates[0].relation, "possibly_occluded_by");

        let candidates = find_with(
            &keys_then("hand", [5.0, 5.0, 50.0, 50.0]),
            &priors(OPEN_SHOEBOX),
        );
        assert_eq!(candidates[0].relation, "possibly_occluded_by");
    }

    #[test]
    fn same_label_identity_ambiguity_blocks_promotion() {
        let mut events = keys_then("box", [5.0, 5.0, 50.0, 50.0]);
        for event in &mut events {
            event.event.label = "box".into();
        }
        let both_boxes = format!(
            "{OPEN_SHOEBOX}[[object]]\nlabel = \"box\"\nlength_m = [0.05, 0.06]\nwidth_m = [0.05, 0.06]\nheight_m = [0.05, 0.06]\n"
        );
        let candidates =
            find_candidates(&events, "box", Duration::seconds(10), &priors(&both_boxes));
        assert_eq!(candidates.len(), 1);
        assert!(candidates[0].same_label_ambiguous);
        assert_eq!(candidates[0].relation, "possibly_under");
        assert!(
            evidence(&candidates[0], "containment_gate")
                .unwrap()
                .detail
                .contains("ambiguous")
        );
    }

    #[test]
    fn persistence_is_reported_from_the_whole_cover_lifecycle() {
        // The appearance row (hits 1, no dwell) supplies the geometry; the
        // later close row carries what the cover actually showed.
        let candidates = find(&keys_then("box", [5.0, 5.0, 50.0, 50.0]));
        let detail = &evidence(&candidates[0], "persistence").unwrap().detail;
        assert!(detail.contains("4.0s"), "{detail}");
        assert!(detail.contains("at least 3 hits"), "{detail}");
    }

    #[test]
    fn accepts_persistent_cover_using_last_geometry_without_containment_claim() {
        let events = vec![
            event(
                1,
                1,
                "keys",
                "disappeared",
                0,
                "missed_gap",
                Some("s"),
                [10.0, 10.0, 40.0, 40.0],
                3,
                Some(3.0),
            ),
            event(
                2,
                2,
                "box",
                "appeared",
                -1,
                "appeared",
                Some("s"),
                [5.0, 5.0, 50.0, 50.0],
                1,
                None,
            ),
            event(
                3,
                2,
                "box",
                "disappeared",
                3,
                "missed_gap",
                Some("s"),
                [4.0, 4.0, 55.0, 55.0],
                3,
                Some(4.0),
            ),
        ];
        let candidates = find(&events);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].relation, "possibly_under");
        assert!(!candidates[0].explanation.contains("inside"));
        assert!(candidates[0].cover_geometry_id.is_some());
    }

    #[test]
    fn rejects_wrong_session_reappearance_bad_geometry_and_shutdown() {
        let mut reappeared = event(
            2,
            2,
            "keys",
            "appeared",
            2,
            "appeared",
            Some("s"),
            [10.0, 10.0, 40.0, 40.0],
            1,
            None,
        );
        reappeared.event.camera_id = "cam".into();
        let mut cover = event(
            3,
            3,
            "box",
            "appeared",
            1,
            "appeared",
            Some("other"),
            [10.0, 10.0, 40.0, 40.0],
            3,
            Some(3.0),
        );
        cover.geometry.as_mut().unwrap().frame_width = None;
        let target = event(
            1,
            1,
            "keys",
            "disappeared",
            0,
            "missed_gap",
            Some("s"),
            [10.0, 10.0, 40.0, 40.0],
            2,
            Some(2.0),
        );
        assert!(find(&[target.clone(), cover]).is_empty());
        assert!(find(&[target, reappeared]).is_empty());

        let target = event(
            4,
            4,
            "keys",
            "disappeared",
            0,
            "shutdown",
            Some("s"),
            [10.0, 10.0, 40.0, 40.0],
            2,
            Some(2.0),
        );
        let cover = event(
            5,
            5,
            "box",
            "appeared",
            1,
            "appeared",
            Some("s"),
            [10.0, 10.0, 40.0, 40.0],
            3,
            Some(3.0),
        );
        assert!(find(&[target, cover]).is_empty());
    }

    #[test]
    fn overlap_alone_does_not_pass_persistence_or_make_under_relation() {
        let target = event(
            1,
            1,
            "keys",
            "disappeared",
            0,
            "missed_gap",
            Some("s"),
            [10.0, 10.0, 40.0, 40.0],
            2,
            Some(2.0),
        );
        let cover = event(
            2,
            2,
            "chair",
            "appeared",
            1,
            "appeared",
            Some("s"),
            [10.0, 10.0, 40.0, 40.0],
            1,
            None,
        );
        assert!(find(&[target, cover]).is_empty());
    }

    #[test]
    fn person_can_only_be_an_occluder() {
        let target = event(
            1,
            1,
            "keys",
            "disappeared",
            0,
            "missed_gap",
            Some("s"),
            [10.0, 10.0, 40.0, 40.0],
            2,
            Some(2.0),
        );
        let person = event(
            2,
            2,
            "person",
            "appeared",
            1,
            "appeared",
            Some("s"),
            [10.0, 10.0, 40.0, 40.0],
            1,
            Some(3.0),
        );
        let closed = event(
            3,
            2,
            "person",
            "disappeared",
            4,
            "missed_gap",
            Some("s"),
            [10.0, 10.0, 40.0, 40.0],
            3,
            Some(3.0),
        );
        let candidates = find(&[target, person, closed]);
        assert_eq!(candidates[0].relation, "possibly_occluded_by");
    }

    #[test]
    fn same_frame_depth_conflict_rejects_but_different_frames_are_unknown() {
        let mut target = event(
            1,
            1,
            "keys",
            "disappeared",
            0,
            "missed_gap",
            Some("s"),
            [10.0, 10.0, 40.0, 40.0],
            2,
            Some(2.0),
        );
        let mut cover = event(
            2,
            2,
            "box",
            "appeared",
            1,
            "appeared",
            Some("s"),
            [10.0, 10.0, 40.0, 40.0],
            1,
            Some(3.0),
        );
        target.depth.push(DepthEvidence {
            id: 1,
            geometry_id: 1,
            provider: "test".into(),
            frame_id: "frame-1".into(),
            prompt_version: "prompt-1".into(),
            backend_version: "backend-1".into(),
            mode: "relative".into(),
            relation_to_camera: "nearer".into(),
            relative_score: None,
            quality: "good".into(),
            response_id: "test-response".into(),
            model_ref: None,
            value_m: None,
            uncertainty_m: None,
            valid_fraction: None,
            coordinate_frame: None,
            calibration_ref: None,
        });
        cover.depth.push(DepthEvidence {
            id: 2,
            geometry_id: 2,
            provider: "test".into(),
            frame_id: "frame-1".into(),
            prompt_version: "prompt-1".into(),
            backend_version: "backend-1".into(),
            mode: "relative".into(),
            relation_to_camera: "farther".into(),
            relative_score: None,
            quality: "good".into(),
            response_id: "test-response".into(),
            model_ref: None,
            value_m: None,
            uncertainty_m: None,
            valid_fraction: None,
            coordinate_frame: None,
            calibration_ref: None,
        });
        cover.geometry.as_mut().unwrap().captured_at = at(0);
        let candidates = find(&[target.clone(), cover.clone()]);
        assert!(candidates.is_empty());
        cover.geometry.as_mut().unwrap().captured_at = at(2);
        let candidates = find(&[target, cover]);
        assert_eq!(candidates.len(), 1);
        assert_eq!(
            candidates[0]
                .evidence
                .iter()
                .find(|evidence| evidence.kind == "depth")
                .map(|evidence| evidence.state),
            Some("unknown")
        );
    }

    #[test]
    fn scene_events_from_read_only_store_are_usable() {
        let store = item_core::store::Store::in_memory().unwrap();
        let target_id = store
            .record_sighting(
                "cam",
                "desk",
                "keys",
                at(0),
                None,
                std::time::Duration::from_secs(300),
            )
            .unwrap()
            .0;
        let target_geo = store
            .record_geometry(
                target_id,
                "target_last",
                at(0),
                [10.0, 10.0, 40.0, 40.0],
                Some(100),
                Some(100),
                "desk",
                "local_detector",
                Some(0.9),
                None,
            )
            .unwrap();
        store
            .append_observation_event(&ObservationEventInput {
                observation_id: target_id,
                camera_id: "cam".into(),
                zone: "desk".into(),
                label: "keys".into(),
                event_type: "disappeared".into(),
                occurred_at: at(0),
                noticed_at: at(30),
                source: "local_detector".into(),
                session_id: Some("s".into()),
                reason: "missed_gap".into(),
                hits: 2,
                seen_for_s: Some(2.0),
                geometry_id: Some(target_geo),
            })
            .unwrap();
        let cover_id = store
            .record_sighting(
                "cam",
                "desk",
                "box",
                at(-1),
                None,
                std::time::Duration::from_secs(300),
            )
            .unwrap()
            .0;
        let cover_geo = store
            .record_geometry(
                cover_id,
                "cover_first",
                at(-1),
                [5.0, 5.0, 50.0, 50.0],
                Some(100),
                Some(100),
                "desk",
                "local_detector",
                Some(0.9),
                None,
            )
            .unwrap();
        store
            .append_observation_event(&ObservationEventInput {
                observation_id: cover_id,
                camera_id: "cam".into(),
                zone: "desk".into(),
                label: "box".into(),
                event_type: "appeared".into(),
                occurred_at: at(-1),
                noticed_at: at(-1),
                source: "local_detector".into(),
                session_id: Some("s".into()),
                reason: "appeared".into(),
                hits: 1,
                seen_for_s: None,
                geometry_id: Some(cover_geo),
            })
            .unwrap();
        let closed_geo = store
            .record_geometry(
                cover_id,
                "cover_closed",
                at(3),
                [4.0, 4.0, 55.0, 55.0],
                Some(100),
                Some(100),
                "desk",
                "local_detector",
                Some(0.9),
                None,
            )
            .unwrap();
        store
            .append_observation_event(&ObservationEventInput {
                observation_id: cover_id,
                camera_id: "cam".into(),
                zone: "desk".into(),
                label: "box".into(),
                event_type: "disappeared".into(),
                occurred_at: at(3),
                noticed_at: at(4),
                source: "local_detector".into(),
                session_id: Some("s".into()),
                reason: "missed_gap".into(),
                hits: 3,
                seen_for_s: Some(4.0),
                geometry_id: Some(closed_geo),
            })
            .unwrap();
        let events = store
            .scene_events_for_camera("cam", at(-10), at(10), 100)
            .unwrap();
        let candidates = find(&events);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].cover_observation_id, cover_id);
    }
}
