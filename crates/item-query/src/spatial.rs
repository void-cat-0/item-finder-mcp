//! Conservative, bounded and auditable spatial lookup in a pinned read snapshot.
use crate::{
    affordance::{Extents, OpeningState, PriorSource, Priors, Role},
    containment::{Candidate, SelectionTrace, evaluate_anchor_with_check},
};
use chrono::{DateTime, Duration, Utc};
use item_core::store::{Result, StoreError};
use item_core::{
    DepthEvidence, Observation, ObservationEvent, ObservationGeometry, SceneEvent,
    evidence::{LifecycleCheck, ReadSnapshot, SceneFilter},
};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

#[cfg(test)]
#[path = "spatial_tests.rs"]
mod tests;
pub const RULE_VERSION: &str = "g4-size-mcp-1";
#[derive(Clone, Debug)]
pub struct SpatialOptions {
    pub window_secs: u32,
    pub limit: usize,
}
impl Default for SpatialOptions {
    fn default() -> Self {
        Self {
            window_secs: 120,
            limit: 20,
        }
    }
}
#[derive(Debug, Clone, Serialize)]
pub struct FactRef {
    pub kind: &'static str,
    pub id: i64,
}
#[derive(Debug, Clone, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EvidenceRef {
    Observation { id: i64 },
    Event { id: i64 },
    Geometry { id: i64 },
    Depth { id: i64 },
    PriorEntry { key: String, priors_ref: String },
}
#[derive(Debug, Clone, Serialize)]
pub struct AuditedContribution {
    pub kind: &'static str,
    pub state: &'static str,
    pub reason: Option<&'static str>,
    pub detail: String,
    pub refs: Vec<EvidenceRef>,
}
#[derive(Debug, Clone, Serialize)]
pub struct AuditedCandidate {
    pub candidate: Candidate,
    pub trace: SelectionTrace,
    pub refs: Vec<FactRef>,
    pub evidence: Vec<AuditedContribution>,
    pub lifecycle_check: LifecycleCheck,
}
#[derive(Debug, Clone, Serialize)]
pub struct PriorEntry {
    pub key: String,
    pub priors_ref: String,
    pub label: String,
    pub camera: Option<String>,
    pub zone: Option<String>,
    pub role: &'static str,
    pub opening_state: Option<&'static str>,
    pub extents: Option<[[Option<f32>; 2]; 3]>,
    pub usable_interior: Option<[[Option<f32>; 2]; 3]>,
    pub unit: &'static str,
    pub source: &'static str,
    pub uncertainty: &'static str,
}
#[derive(Debug, Clone, Serialize, Default)]
pub struct UsedFacts {
    pub observations: Vec<Observation>,
    pub events: Vec<ObservationEvent>,
    pub geometry: Vec<ObservationGeometry>,
    pub depth: Vec<DepthEvidence>,
    pub prior_entries: Vec<PriorEntry>,
}
#[derive(Debug, Clone, Serialize)]
pub struct SpatialScope {
    pub camera_id: String,
    pub zone: String,
    pub session_id: Option<String>,
    pub window_secs: u32,
    pub scene_event_limit: usize,
    pub depth_per_geometry_limit: usize,
    pub scene_since: Option<DateTime<Utc>>,
    pub scene_until: Option<DateTime<Utc>>,
    pub reappearance_checked_until: DateTime<Utc>,
}
#[derive(Debug, Clone, Serialize)]
pub struct SpatialChecks {
    pub same_label_reappearance: &'static str,
    pub checked_until: DateTime<Utc>,
    pub camera_id: String,
    pub session_id: Option<String>,
    pub label: String,
    pub zone_scope: &'static str,
}
#[derive(Debug, Clone, Serialize)]
pub struct SpatialLookup {
    pub database_id: Option<String>,
    pub as_of: DateTime<Utc>,
    pub direct_observation: Observation,
    pub anchor: Option<ObservationEvent>,
    pub scope: SpatialScope,
    pub candidates: Vec<AuditedCandidate>,
    pub used_facts: UsedFacts,
    pub availability: &'static str,
    pub reason: Option<&'static str>,
    pub scan_complete: bool,
    pub provenance_complete: bool,
    pub rule_version: &'static str,
    pub candidates_truncated: bool,
    pub diagnostics: Vec<&'static str>,
    pub diagnostic_counts: BTreeMap<&'static str, usize>,
    pub checks: SpatialChecks,
}

/// No writes, migration, daemon operations, or second version of the relationship rules.
pub fn candidates_for_observation(
    snapshot: &ReadSnapshot<'_>,
    observation_id: i64,
    camera_guard: Option<&str>,
    options: SpatialOptions,
    priors: &Priors,
) -> Result<SpatialLookup> {
    if !(1..=3600).contains(&options.window_secs) || !(1..=50).contains(&options.limit) {
        return Err(StoreError::InvalidInput("invalid spatial limits".into()));
    }
    if !safe_priors_reference(priors.reference()) {
        return Err(StoreError::InvalidInput(
            "spatial lookup requires a safe immutable priors reference".into(),
        ));
    }
    let target = snapshot.observation(observation_id, camera_guard)?;
    let mut lookup = empty(target, None, snapshot, options.window_secs);
    let caps = &snapshot.capabilities;
    if !caps.geometry.usable() || !caps.events.usable() {
        lookup.availability = "schema_unavailable";
        lookup.reason = caps
            .geometry
            .reason
            .filter(|_| !caps.geometry.usable())
            .or(caps.events.reason);
        lookup.scan_complete = false;
        return Ok(lookup);
    }
    let Some(anchor) = snapshot.latest_departure_for_observation(&lookup.direct_observation)?
    else {
        lookup.reason = Some("no_eligible_departure");
        return Ok(lookup);
    };
    lookup.scope.session_id = anchor.session_id.clone();
    lookup.checks.session_id = anchor.session_id.clone();
    lookup.anchor = Some(anchor.clone());
    let Some(session) = anchor.session_id.as_deref().filter(|s| !s.is_empty()) else {
        lookup.reason = Some("anchor_evidence_missing");
        return Ok(lookup);
    };
    let Some(geometry_id) = anchor.geometry_id else {
        lookup.reason = Some("anchor_evidence_missing");
        return Ok(lookup);
    };
    let Some(anchor_geo) = snapshot.geometry(geometry_id)? else {
        lookup.reason = Some("anchor_evidence_missing");
        return Ok(lookup);
    };
    if !eligible_event(&anchor, &lookup.direct_observation, snapshot.as_of)
        || !eligible_geometry(&anchor_geo, &anchor, snapshot.as_of)
    {
        lookup.reason = Some("anchor_evidence_missing");
        lookup.diagnostics.push("invalid_anchor_evidence");
        return Ok(lookup);
    }
    if lookup.direct_observation.last_seen > snapshot.as_of {
        lookup.reason = Some("future_observation");
        return Ok(lookup);
    };
    if lookup.direct_observation.last_seen > anchor.occurred_at {
        lookup.reason = Some("target_seen_after_anchor");
        return Ok(lookup);
    };
    if snapshot.same_label_appeared_after(&anchor)?.is_some() {
        lookup.reason = Some("same_label_reappeared");
        lookup.checks.same_label_reappearance = "same_label_reappeared";
        return Ok(lookup);
    }
    lookup.checks.same_label_reappearance = "no_later_same_label_appearance";
    let window = Duration::seconds(options.window_secs as i64);
    let since = anchor
        .occurred_at
        .checked_sub_signed(window)
        .ok_or(StoreError::InvalidInput("anchor time out of range".into()))?;
    let until = anchor
        .occurred_at
        .checked_add_signed(window.max(Duration::seconds(31)))
        .ok_or(StoreError::InvalidInput("anchor time out of range".into()))?
        .min(snapshot.as_of);
    lookup.scope.scene_since = Some(since);
    lookup.scope.scene_until = Some(until);
    let mut scene = snapshot.scene_window(SceneFilter {
        camera: &anchor.camera_id,
        zone: &anchor.zone,
        session,
        since,
        until,
    })?;
    lookup.provenance_complete = scene.provenance_complete;
    if !scene.scan_complete {
        lookup.availability = "partial";
        lookup.reason = Some("evidence_scan_limit");
        lookup.scan_complete = false;
        return Ok(lookup);
    };
    // Ownership and temporal checks precede pure geometry/depth rules. Invalid facts
    // never become silently normalized boxes or facts from a different observation.
    let mut invalid_scene = false;
    for event in &mut scene.events {
        snapshot.check()?;
        if event.event.noticed_at < event.event.occurred_at
            || event.event.noticed_at > snapshot.as_of
            || event.event.hits <= 0
            || event
                .event
                .seen_for_s
                .is_some_and(|s| !s.is_finite() || s < 0.0)
        {
            invalid_scene = true;
        }
        if event
            .geometry
            .as_ref()
            .is_some_and(|geometry| !eligible_geometry(geometry, &event.event, snapshot.as_of))
        {
            event.geometry = None;
            *lookup
                .diagnostic_counts
                .entry("invalid_geometry")
                .or_default() += 1;
        }
    }
    if invalid_scene {
        lookup.reason = Some("scene_evidence_invalid");
        lookup.diagnostics.push("scene_evidence_invalid");
        return Ok(lookup);
    };
    snapshot.check()?;
    let evaluation = evaluate_anchor_with_check(
        &scene.events,
        &lookup.direct_observation.label,
        anchor.id,
        window,
        priors,
        || snapshot.check(),
    )?;
    snapshot.check()?;
    lookup.diagnostic_counts.extend(evaluation.diagnostics);
    for mut audited in evaluation.candidates {
        snapshot.check()?;
        if audited.candidate.target_event_id != anchor.id {
            continue;
        }
        let appearance = scene
            .events
            .iter()
            .find(|e| e.event.id == audited.candidate.cover_event_id)
            .ok_or(StoreError::InvalidInput(
                "missing selected appearance".into(),
            ))?;
        let lifecycle = snapshot.cover_lifecycle_after(&appearance.event, &anchor)?;
        if lifecycle.result != "no_later_lifecycle_conflict" {
            *lookup
                .diagnostic_counts
                .entry(lifecycle.result)
                .or_default() += 1;
            continue;
        }
        // A stale foreign or inconsistent basic record is not an attributable cover.
        let cover = snapshot.observation(
            audited.candidate.cover_observation_id,
            Some(&anchor.camera_id),
        )?;
        if cover.zone != anchor.zone
            || cover.label != appearance.event.label
            || !eligible_event(&appearance.event, &cover, snapshot.as_of)
        {
            *lookup
                .diagnostic_counts
                .entry("cover_evidence_unavailable")
                .or_default() += 1;
            continue;
        }
        audited.candidate.rule_version = RULE_VERSION;
        audited.candidate.explanation="Qualified visibility hypothesis; not current presence or physical containment. Size and opening are configured priors; entry path is not measured.".into();
        let (evidence, _) = audited_evidence(&audited.candidate, &audited.trace, priors)?;
        let refs = flat_refs(&evidence);
        lookup.candidates.push(AuditedCandidate {
            candidate: audited.candidate,
            trace: audited.trace,
            refs,
            evidence,
            lifecycle_check: lifecycle,
        });
    }
    lookup.candidates.sort_by(|a, b| {
        b.candidate
            .score
            .total_cmp(&a.candidate.score)
            .then_with(|| {
                a.candidate
                    .target_event_id
                    .cmp(&b.candidate.target_event_id)
            })
            .then_with(|| a.candidate.cover_event_id.cmp(&b.candidate.cover_event_id))
            .then_with(|| {
                a.candidate
                    .cover_geometry_id
                    .cmp(&b.candidate.cover_geometry_id)
            })
    });
    lookup.candidates.dedup_by_key(|c| {
        (
            c.candidate.target_event_id,
            c.candidate.cover_event_id,
            c.candidate.cover_geometry_id,
        )
    });
    lookup.candidates_truncated = lookup.candidates.len() > options.limit;
    lookup.candidates.truncate(options.limit);
    lookup.used_facts = collect_used_facts(snapshot, &lookup.candidates, &scene.events, priors)?;
    lookup.availability = if !scene.provenance_complete && caps.depth.usable() {
        "partial"
    } else {
        "available"
    };
    lookup.reason = if lookup.candidates.is_empty() {
        Some("no_candidate_supported")
    } else if lookup.availability == "partial" {
        Some("optional_provenance_missing")
    } else {
        None
    };
    lookup.diagnostics = lookup.diagnostic_counts.keys().copied().collect();
    snapshot.check()?;
    Ok(lookup)
}
fn empty(
    target: Observation,
    anchor: Option<ObservationEvent>,
    snapshot: &ReadSnapshot<'_>,
    window: u32,
) -> SpatialLookup {
    SpatialLookup {
        database_id: snapshot.database_id.clone(),
        as_of: snapshot.as_of,
        scope: SpatialScope {
            camera_id: target.camera_id.clone(),
            zone: target.zone.clone(),
            session_id: None,
            window_secs: window,
            scene_event_limit: 500,
            depth_per_geometry_limit: 4,
            scene_since: None,
            scene_until: None,
            reappearance_checked_until: snapshot.as_of,
        },
        checks: SpatialChecks {
            same_label_reappearance: "not_checked",
            checked_until: snapshot.as_of,
            camera_id: target.camera_id.clone(),
            session_id: None,
            label: target.label.clone(),
            zone_scope: "all_zones_same_camera_session_label",
        },
        direct_observation: target,
        anchor,
        candidates: vec![],
        used_facts: UsedFacts::default(),
        availability: "not_available",
        reason: None,
        scan_complete: true,
        provenance_complete: snapshot.capabilities.depth_provenance_complete,
        rule_version: RULE_VERSION,
        candidates_truncated: false,
        diagnostics: vec![],
        diagnostic_counts: BTreeMap::new(),
    }
}
fn eligible_event(event: &ObservationEvent, obs: &Observation, as_of: DateTime<Utc>) -> bool {
    event.id > 0
        && event.observation_id == obs.id
        && event.camera_id == obs.camera_id
        && event.zone == obs.zone
        && event.label == obs.label
        && event.occurred_at <= event.noticed_at
        && event.noticed_at <= as_of
        && event.hits > 0
        && event.seen_for_s.is_none_or(|v| v.is_finite() && v >= 0.0)
}
fn eligible_geometry(g: &ObservationGeometry, e: &ObservationEvent, as_of: DateTime<Utc>) -> bool {
    let [x0, y0, x1, y1] = g.bbox;
    let dimensions = g.frame_width.zip(g.frame_height);
    g.id > 0
        && g.observation_id == e.observation_id
        && g.zone == e.zone
        && g.captured_at <= e.occurred_at
        && g.captured_at <= as_of
        && g.sample_kind.starts_with("event_")
        && g.bbox.iter().all(|v| v.is_finite())
        && dimensions.is_some_and(|(w, h)| {
            w > 0
                && h > 0
                && x0 >= 0.0
                && y0 >= 0.0
                && x1 > x0
                && y1 > y0
                && x1 <= w as f32
                && y1 <= h as f32
        })
}
fn fixed_detail(kind: &str, state: &str) -> &'static str {
    match kind {
        "temporal_proximity" => "Plausible lifecycle timing within the configured window.",
        "projected_overlap" => "Qualified image-plane overlap; not physical containment proof.",
        "persistence" => "Cover persistence and score use the cited lifecycle witnesses.",
        "depth" if state == "unknown" => "No decisive comparable ordinal depth was established.",
        "depth" => "Same-frame relative depth supports the cover being in front of the target.",
        "identity" => "Target and cover labels are equal; instance identity is ambiguous.",
        "semantic_affordance" => {
            "Configured cover/container role and opening prior; not a measured entry path."
        }
        "size" => {
            "Conservative three-axis interval fit using configured priors; the opening path was not checked."
        }
        _ => "Containment promotion gate did not establish sufficient geometry or identity.",
    }
}
fn safe_priors_reference(reference: &str) -> bool {
    reference == crate::affordance::BUILTIN_PRIORS_REF
        || reference
            .strip_prefix("builtin:g4-priors-1+configured-default:sha256:")
            .is_some_and(|s| {
                s.len() == 64
                    && s.bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            })
}
fn safe_prior_label(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 1024
        && !value.contains(['/', '\\', ':', '@'])
        && !value.chars().any(char::is_control)
}
fn audited_evidence(
    c: &Candidate,
    t: &SelectionTrace,
    p: &Priors,
) -> Result<(Vec<AuditedContribution>, Vec<PriorEntry>)> {
    let container = p
        .container(&c.cover_label, &c.camera_id, &c.zone)
        .filter(|_| !crate::affordance::is_person_label(&c.cover_label));
    let object = p.object(&c.target_label);
    let mut entries = vec![];
    if container.is_some_and(|c| {
        !safe_prior_label(&c.label)
            || c.camera
                .as_deref()
                .is_some_and(|s| s.len() > 1024 || s.chars().any(char::is_control))
            || c.zone
                .as_deref()
                .is_some_and(|s| s.len() > 1024 || s.chars().any(char::is_control))
    }) || object.is_some_and(|o| !safe_prior_label(&o.label))
    {
        return Err(StoreError::InvalidInput(
            "unsafe prior entry metadata".into(),
        ));
    }
    if let Some(prior) = container {
        entries.push(PriorEntry {
            key: container_key(prior),
            priors_ref: p.reference().into(),
            label: prior.label.clone(),
            camera: prior.camera.clone(),
            zone: prior.zone.clone(),
            role: match prior.role {
                Role::Container => "container",
                Role::Cover => "cover",
            },
            opening_state: Some(match prior.opening_state {
                OpeningState::Open => "open",
                OpeningState::Closed => "closed",
                OpeningState::Unclear => "unclear",
                OpeningState::NotApplicable => "not_applicable",
            }),
            extents: None,
            usable_interior: Some(extents(&prior.interior)),
            unit: "metres",
            source: prior_source(prior.source),
            uncertainty: if prior.source == PriorSource::Builtin {
                "high_class_prior"
            } else {
                "configured_not_measured_by_rule"
            },
        });
    }
    if let Some(prior) = object.filter(|_| container.is_some_and(|c| c.role == Role::Container)) {
        entries.push(PriorEntry {
            key: format!("object:{}", key_part(&prior.label)),
            priors_ref: p.reference().into(),
            label: prior.label.clone(),
            camera: None,
            zone: None,
            role: "object",
            opening_state: None,
            extents: Some(extents(&prior.extents)),
            usable_interior: None,
            unit: "metres",
            source: prior_source(prior.source),
            uncertainty: if prior.source == PriorSource::Builtin {
                "high_class_prior"
            } else {
                "configured_not_measured_by_rule"
            },
        });
    }
    let prior_refs = entries
        .iter()
        .map(|e| EvidenceRef::PriorEntry {
            key: e.key.clone(),
            priors_ref: e.priors_ref.clone(),
        })
        .collect::<Vec<_>>();
    let mut out = vec![];
    for e in &c.evidence {
        let mut refs=match e.kind {
            "temporal_proximity"=>vec![EvidenceRef::Event{id:c.target_event_id},EvidenceRef::Event{id:c.cover_event_id}],
            "projected_overlap"|"containment_gate"=>vec![EvidenceRef::Geometry{id:c.target_geometry_id.ok_or(StoreError::PayloadTooLarge)?},EvidenceRef::Geometry{id:c.cover_geometry_id.ok_or(StoreError::PayloadTooLarge)?},EvidenceRef::Event{id:t.cover_geometry_event_id}],
            "persistence"=>t.persistence_event_ids.iter().map(|id|EvidenceRef::Event{id:*id}).collect(),
            "depth"=>{ let mut r=vec![EvidenceRef::Geometry{id:c.target_geometry_id.ok_or(StoreError::PayloadTooLarge)?},EvidenceRef::Geometry{id:c.cover_geometry_id.ok_or(StoreError::PayloadTooLarge)?}]; if let Some(id)=t.target_depth_id {r.push(EvidenceRef::Depth{id});}
                if let Some(id)=t.cover_depth_id{r.push(EvidenceRef::Depth{id});}r },
            "identity"=>vec![EvidenceRef::Observation{id:c.target_observation_id},EvidenceRef::Observation{id:c.cover_observation_id}],
            "size"=>prior_refs.clone(),
            "semantic_affordance"=>prior_refs.iter().filter(|r|matches!(r,EvidenceRef::PriorEntry{key,..} if key.starts_with("container:"))).cloned().collect(),
            _=>vec![],
        };
        // The basic aggregate facts must also be resolvable for every candidate.
        if e.kind == "temporal_proximity" {
            refs.push(EvidenceRef::Observation {
                id: c.target_observation_id,
            });
            refs.push(EvidenceRef::Observation {
                id: c.cover_observation_id,
            });
        }
        refs.sort();
        refs.dedup();
        if refs.len() > 8 {
            return Err(StoreError::PayloadTooLarge);
        };
        out.push(AuditedContribution {
            kind: e.kind,
            state: e.state,
            reason: if e.kind == "depth" && e.state == "unknown" {
                Some("no_comparable_depth")
            } else {
                None
            },
            detail: fixed_detail(e.kind, e.state).into(),
            refs,
        });
    }
    if out.len() > 8 {
        return Err(StoreError::PayloadTooLarge);
    };
    Ok((out, entries))
}
fn extents(e: &Extents) -> [[Option<f32>; 2]; 3] {
    e.map(|v| v.map_or([None, None], |i| [Some(i.min), Some(i.max)]))
}
fn prior_source(s: PriorSource) -> &'static str {
    match s {
        PriorSource::Builtin => "builtin",
        PriorSource::File => "configured",
    }
}
fn key_part(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.') {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}
fn container_key(c: &crate::affordance::ContainerPrior) -> String {
    format!(
        "container:{}:{}:{}",
        c.camera
            .as_deref()
            .map(key_part)
            .unwrap_or_else(|| "*".into()),
        c.zone
            .as_deref()
            .map(key_part)
            .unwrap_or_else(|| "*".into()),
        key_part(&c.label)
    )
}
fn flat_refs(es: &[AuditedContribution]) -> Vec<FactRef> {
    let mut set = BTreeSet::new();
    for e in es {
        for r in &e.refs {
            match r {
                EvidenceRef::Observation { id } => {
                    set.insert(("observation", *id));
                }
                EvidenceRef::Event { id } => {
                    set.insert(("event", *id));
                }
                EvidenceRef::Geometry { id } => {
                    set.insert(("geometry", *id));
                }
                EvidenceRef::Depth { id } => {
                    set.insert(("depth", *id));
                }
                _ => {}
            }
        }
    }
    set.into_iter()
        .map(|(kind, id)| FactRef { kind, id })
        .collect()
}
fn collect_used_facts(
    snapshot: &ReadSnapshot<'_>,
    candidates: &[AuditedCandidate],
    scene: &[SceneEvent],
    priors: &Priors,
) -> Result<UsedFacts> {
    let mut facts = UsedFacts::default();
    let mut refs = BTreeSet::new();
    for c in candidates {
        for e in &c.evidence {
            refs.extend(e.refs.clone());
        }
    }
    for r in refs {
        snapshot.check()?;
        match r {
            EvidenceRef::Observation { id } => {
                facts.observations.push(snapshot.observation(id, None)?)
            }
            EvidenceRef::Event { id } => facts.events.push(
                scene
                    .iter()
                    .find(|e| e.event.id == id)
                    .ok_or(StoreError::InvalidInput("unresolved used event".into()))?
                    .event
                    .clone(),
            ),
            EvidenceRef::Geometry { id } => facts.geometry.push(
                scene
                    .iter()
                    .filter_map(|e| e.geometry.as_ref())
                    .find(|g| g.id == id)
                    .ok_or(StoreError::InvalidInput("unresolved used geometry".into()))?
                    .clone(),
            ),
            EvidenceRef::Depth { id } => {
                let d = scene
                    .iter()
                    .flat_map(|e| &e.depth)
                    .find(|d| d.id == id)
                    .ok_or(StoreError::InvalidInput("unresolved used depth".into()))?
                    .clone();
                facts.depth.push(d);
            }
            EvidenceRef::PriorEntry { key, .. } => {
                let entry = candidates
                    .iter()
                    .filter_map(|c| audited_evidence(&c.candidate, &c.trace, priors).ok())
                    .flat_map(|(_, entries)| entries)
                    .find(|e| e.key == key)
                    .ok_or(StoreError::InvalidInput("unresolved prior entry".into()))?;
                facts.prior_entries.push(entry);
            }
        }
    }
    Ok(facts)
}

/// Remove a complete tail candidate and every now-unused bundle entry. Used by
/// the adapter's response byte budget; decisive refs are never sliced.
impl SpatialLookup {
    pub fn remove_tail_candidate(&mut self) -> bool {
        if self.candidates.pop().is_none() {
            return false;
        }
        self.candidates_truncated = true;
        let refs = self
            .candidates
            .iter()
            .flat_map(|c| c.evidence.iter().flat_map(|e| e.refs.clone()))
            .collect::<BTreeSet<_>>();
        self.used_facts
            .observations
            .retain(|o| refs.contains(&EvidenceRef::Observation { id: o.id }));
        self.used_facts
            .events
            .retain(|e| refs.contains(&EvidenceRef::Event { id: e.id }));
        self.used_facts
            .geometry
            .retain(|g| refs.contains(&EvidenceRef::Geometry { id: g.id }));
        self.used_facts
            .depth
            .retain(|d| refs.contains(&EvidenceRef::Depth { id: d.id }));
        self.used_facts.prior_entries.retain(|p| {
            refs.contains(&EvidenceRef::PriorEntry {
                key: p.key.clone(),
                priors_ref: p.priors_ref.clone(),
            })
        });
        true
    }
}
