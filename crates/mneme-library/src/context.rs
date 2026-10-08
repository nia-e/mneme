//! Projection of the current owner-context envelope. Source selection is
//! deliberately outside this module: semantic and episodic lanes travel together.
use mneme_present::{
    EpisodeReferenceRetrieval, TouchstoneRetrieval, validate_episode_card_metadata,
    validate_touchstone_card_metadata,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{MAX_CONTEXT_BYTES, SourceFailure};

pub(super) const MAX_ITEMS: usize = 13;
pub(super) const EPISODE_TARGET: usize = 2;
// A packet resource ceiling, not a limit on relevant scenes.
pub(super) const EPISODE_MAX: usize = MAX_ITEMS;

fn refused(message: &str) -> SourceFailure {
    SourceFailure::Refused(format!("owner context {message}"))
}

fn valid_id(value: &Value) -> bool {
    value
        .as_str()
        .is_some_and(|id| !id.is_empty() && id.len() <= 80)
}

fn valid_card(card: &Value) -> bool {
    let summary = &card["summary"];
    valid_id(&card["id"])
        && card["rank"]
            .as_u64()
            .is_some_and(|rank| rank > 0 && rank <= u16::MAX.into())
        && summary["text"].as_str().is_some_and(|text| {
            text.len() <= 2048
                && summary["source_bytes"].as_u64().is_some_and(|bytes| {
                    bytes <= u32::MAX.into()
                        && bytes >= text.len() as u64
                        && (summary["complete"] != true || bytes == text.len() as u64)
                })
        })
        && summary["complete"].is_boolean()
        && card["content_trust"] == "untrusted"
        && card.get("body").is_none()
}

fn valid_episode(card: &Value, lexical_searched: bool) -> bool {
    valid_card(card)
        && card["kind"] == "episode"
        && card["edition_id"] == card["id"]
        && validate_episode_card_metadata(card, lexical_searched).is_ok()
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct LaneOmissions {
    bounded_window_budget: u32,
    further_tail_unknown: bool,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct NativeOmissions {
    core: LaneOmissions,
    primary: LaneOmissions,
    expansion: LaneOmissions,
    episodic: LaneOmissions,
}

pub(super) fn validate(value: &Value, requested_capacity: usize) -> Result<(), SourceFailure> {
    if !(1..=256).contains(&requested_capacity) {
        return Err(refused("request capacity outside native bounds"));
    }
    if value["schema"] != "mneme.context.v7" {
        return Err(refused("schema is unsupported (expected mneme.context.v7)"));
    }
    if serde_json::to_vec(value)
        .map_err(|_| refused("cannot be encoded"))?
        .len()
        > MAX_CONTEXT_BYTES
        || !value["partial"].is_boolean()
    {
        return Err(refused("exceeds 32 KiB or has no partial flag"));
    }
    let mut semantic_count = 0;
    if value.get("probationary").is_some()
        || value["omitted"].get("probationary").is_some()
        || value["usage"].get("probationary").is_some()
        || value["retrieval"]["lanes"].get("probationary").is_some()
    {
        return Err(refused("obsolete probationary lane"));
    }
    let omissions: NativeOmissions = serde_json::from_value(value["omitted"].clone())
        .map_err(|_| refused("native omissions invalid"))?;
    if serde_json::to_value(omissions).ok().as_ref() != Some(&value["omitted"]) {
        return Err(refused("native omissions noncanonical"));
    }
    for lane in ["core", "primary", "expansions"] {
        let cards = value[lane]
            .as_array()
            .ok_or_else(|| refused("semantic lanes missing"))?;
        semantic_count += cards.len();
        if cards.iter().any(|card| {
            !valid_card(card)
                || card.get("kind").is_some()
                || card.get("occurrence_contexts").is_some()
                || card.get("recording_session").is_some()
                || card.get("origins").is_some()
                || validate_touchstone_card_metadata(card).is_err()
        }) {
            return Err(refused("semantic card invalid or body disclosed"));
        }
    }
    if semantic_count > requested_capacity {
        return Err(refused(
            "semantic window exceeds requested discovery capacity",
        ));
    }
    let touchstones: TouchstoneRetrieval =
        serde_json::from_value(value["touchstone_retrieval"].clone())
            .map_err(|_| refused("touchstone coverage invalid"))?;
    if touchstones.validate().is_err()
        || serde_json::to_value(&touchstones).ok().as_ref() != Some(&value["touchstone_retrieval"])
    {
        return Err(refused("touchstone coverage invalid or noncanonical"));
    }
    if !touchstones.searched
        && ["core", "primary", "expansions"].iter().any(|lane| {
            value[*lane]
                .as_array()
                .expect("validated semantic lane")
                .iter()
                .any(|card| card.get("touchstone").is_some())
        })
    {
        return Err(refused(
            "touchstone card without searched touchstone coverage",
        ));
    }
    {
        let cards = value["episodes"]
            .as_array()
            .ok_or_else(|| refused("episodic lane missing"))?;
        let coverage = &value["episodic_retrieval"];
        let state = coverage["state"].as_str();
        if cards.len() > requested_capacity
            || cards.iter().any(|card| {
                !valid_episode(card, state == Some("searched")) || card.get("touchstone").is_some()
            })
        {
            return Err(refused("episode card invalid or body disclosed"));
        }
        let references: EpisodeReferenceRetrieval =
            serde_json::from_value(value["episode_reference_retrieval"].clone())
                .map_err(|_| refused("episode reference coverage invalid"))?;
        if serde_json::to_value(&references).ok().as_ref()
            != Some(&value["episode_reference_retrieval"])
        {
            return Err(refused("episode reference coverage is noncanonical"));
        }
        if value["episode_reference_retrieval"]["state"] != "searched"
            && cards.iter().any(|card| {
                card["origins"].as_array().is_some_and(|origins| {
                    origins.iter().any(|origin| origin["kind"] == "reference")
                })
            })
        {
            return Err(refused(
                "reference card without searched reference coverage",
            ));
        }
        if coverage["mode"] != "lexical"
            || !matches!(
                state,
                Some("searched" | "not_searched" | "not_searched_tag_filter" | "unavailable")
            )
            || !coverage["cue_normalized"].is_boolean()
            || !coverage["cue_truncated"].is_boolean()
            || (state == Some("unavailable")
                && !matches!(
                    coverage["unavailable_reason"].as_str(),
                    Some("adapter_unsupported" | "store_not_upgraded")
                ))
            || value["omitted"]["episodic"]["bounded_window_budget"]
                .as_u64()
                .is_none()
            || !value["omitted"]["episodic"]["further_tail_unknown"].is_boolean()
        {
            return Err(refused("episodic coverage invalid"));
        }
    }
    Ok(())
}

/// Facet identifiers are local to the verified source, not new library routes.
/// Refuse the whole source rather than widening access or falling back on an
/// untrusted card's database identity.
pub(super) fn validate_source(
    value: &Value,
    requested_capacity: usize,
    expected_db: &str,
) -> Result<(), SourceFailure> {
    validate(value, requested_capacity)?;
    for lane in ["core", "primary", "expansions"] {
        for card in value[lane].as_array().expect("validated semantic lane") {
            if let Some(view) = card.get("touchstone") {
                if view["references"]
                    .as_array()
                    .expect("validated touchstone references")
                    .iter()
                    .any(|row| row["db_id"] != expected_db)
                {
                    return Err(refused("touchstone reference database identity mismatch"));
                }
            }
        }
    }
    Ok(())
}

pub(super) fn coverage(value: &Value) -> Value {
    let mut coverage = value["episodic_retrieval"].clone();
    coverage["omitted"] = value["omitted"]["episodic"].clone();
    coverage
}

/// Losslessly factor repeated native reports; per-source state/tail/stop stay
/// inline and its typed index resolves the complete exact native counters.
pub(super) fn intern_reference_reports(coverage: &mut [Value]) -> Vec<Value> {
    let mut canonical_reports = std::collections::BTreeMap::new();
    let mut reports = Vec::new();
    for source in coverage {
        let Some(report) = source.get("episode_reference_retrieval").cloned() else {
            continue;
        };
        let typed: EpisodeReferenceRetrieval =
            serde_json::from_value(report.clone()).expect("validated native reference report");
        let bytes = serde_json::to_vec(&typed).expect("typed native report serializes");
        let index = *canonical_reports.entry(bytes).or_insert_with(|| {
            let index = reports.len();
            reports.push(report.clone());
            index
        });
        let mut reference = json!({"report_index":index,"state":report["state"],
            "further_tail_unknown":report["further_tail_unknown"]});
        if let Some(stop) = report.get("stop_reason") {
            reference["stop_reason"] = stop.clone();
        }
        source["episode_reference_retrieval"] = reference;
    }
    reports
}

/// Preserve complete native reports while factoring repeated source work. A
/// reference is only an encoding detail: no counter or caveat is discarded.
pub(super) fn intern_touchstone_reports(coverage: &mut [Value]) -> Vec<Value> {
    intern_reports(coverage, "touchstone_retrieval", None)
}

pub(super) fn intern_omission_reports(coverage: &mut [Value]) -> Vec<Value> {
    intern_reports(coverage, "native_omitted", None)
}

/// The lexical state remains visible beside each pinned owner identity. The
/// index resolves the exact native cue flags, unavailable reason and omissions.
pub(super) fn intern_episodic_reports(coverage: &mut [Value]) -> Vec<Value> {
    intern_reports(coverage, "episodic", Some("state"))
}

fn intern_reports(coverage: &mut [Value], key: &str, inline_field: Option<&str>) -> Vec<Value> {
    let mut canonical_reports = std::collections::BTreeMap::new();
    let mut reports = Vec::new();
    for source in coverage {
        let Some(report) = source.get(key).cloned() else {
            continue;
        };
        let bytes = serde_json::to_vec(&report).expect("validated native report");
        let index = *canonical_reports.entry(bytes).or_insert_with(|| {
            let index = reports.len();
            reports.push(report.clone());
            index
        });
        source[key] = json!({"report_index":index});
        if let Some(field) = inline_field {
            source[key][field] = report[field].clone();
        }
    }
    reports
}

/// Reject optional cards which cannot fit even alone with the complete source
/// metadata. Do this before the final item cap so a large first origin union
/// cannot consume a slot or cause smaller later scenes to be popped first.
pub(super) fn remove_unfit_cards(result: &mut Value, considered: [usize; 2]) {
    let mut alone = result.clone();
    alone["primary"] = json!([]);
    alone["episodes"] = json!([]);
    let mut primary = std::mem::take(result["primary"].as_array_mut().expect("semantic lane"));
    let semantic_before = primary.len();
    primary.retain(|card| {
        alone["primary"] = json!([card]);
        update_omissions(&mut alone, considered);
        serde_json::to_vec(&alone)
            .expect("library packet serializes")
            .len()
            <= MAX_CONTEXT_BYTES
    });
    alone["primary"] = json!([]);
    let mut episodes = std::mem::take(result["episodes"].as_array_mut().expect("episode lane"));
    let before = episodes.len();
    episodes.retain(|card| {
        alone["episodes"] = json!([card]);
        update_omissions(&mut alone, considered);
        serde_json::to_vec(&alone)
            .expect("library packet serializes")
            .len()
            <= MAX_CONTEXT_BYTES
    });
    result["episodes"] = json!(episodes);
    result["primary"] = json!(primary);
    if result["episodes"].as_array().expect("episode lane").len() != before
        || result["primary"].as_array().expect("semantic lane").len() != semantic_before
    {
        mark_truncated(result, considered);
    }
}

pub(super) fn trim_result_pool(result: &mut Value, total: usize) {
    let mut primary = std::mem::take(result["primary"].as_array_mut().expect("semantic lane"));
    let mut episodes = std::mem::take(result["episodes"].as_array_mut().expect("episode lane"));
    trim_pool(&mut primary, &mut episodes, total);
    result["primary"] = json!(primary);
    result["episodes"] = json!(episodes);
}

pub(super) fn trim_result_items(result: &mut Value) {
    let mut primary = std::mem::take(result["primary"].as_array_mut().expect("semantic lane"));
    let mut episodes = std::mem::take(result["episodes"].as_array_mut().expect("episode lane"));
    trim_items(&mut primary, &mut episodes);
    result["primary"] = json!(primary);
    result["episodes"] = json!(episodes);
}

pub(super) fn project(card: &Value, project: &str, db: &str, source: &Value, lane: &str) -> Value {
    let mut out = json!({
        "project_id":project, "db_id":db, "source":source,
        "id":card["id"], "summary":card["summary"]["text"],
        "summary_complete":card["summary"]["complete"],
        "summary_source_bytes":card["summary"]["source_bytes"],
        "content_trust":"untrusted", "lane":lane, "source_rank":card["rank"],
        "kind":if lane == "episodic" { "episode" } else { "semantic" },
    });
    if lane == "episodic" {
        for key in [
            "episode_id",
            "edition_id",
            "revision",
            "current_edition_id",
            "occurred",
            "recorded_at",
            "edition_recorded_at",
            "thread",
            "recording_session",
            "origins",
        ] {
            out[key] = card[key].clone();
        }
        if let Some(contexts) = card.get("occurrence_contexts") {
            out["occurrence_contexts"] = contexts.clone();
        }
    }
    // The compact native view is indivisible: keep its exact historical summary
    // prefixes, statuses, origin identities and omission count together.
    if let Some(touchstone) = card.get("touchstone") {
        out["touchstone"] = touchstone.clone();
    }
    out
}

/// Union views only for the same immutable account in the same logical store.
/// First route and observed head remain pinned; conflicting duplicates are
/// explicitly refused rather than silently repaired or substituted with a head.
pub(super) fn deduplicate_episodes(episodes: &mut Vec<Value>) -> Vec<(String, String)> {
    let mut positions = std::collections::BTreeMap::new();
    let mut unique: Vec<Value> = Vec::new();
    let mut refused = Vec::new();
    for card in episodes.drain(..) {
        let key = (
            card["db_id"]
                .as_str()
                .expect("enrolled database identity")
                .to_owned(),
            card["episode_id"]
                .as_str()
                .expect("validated episode identity")
                .to_owned(),
            card["edition_id"]
                .as_str()
                .expect("validated edition identity")
                .to_owned(),
        );
        if let Some(&index) = positions.get(&key) {
            let first = &mut unique[index];
            let mut metadata =
                validate_episode_card_metadata(first, true).expect("validated owner metadata");
            let other =
                validate_episode_card_metadata(&card, true).expect("validated owner metadata");
            // Repacking changes a view, not the account: compatible bounded
            // prefixes with the same source length keep the first rendering.
            let first_text = first["summary"].as_str().expect("validated summary text");
            let other_text = card["summary"].as_str().expect("validated summary text");
            let same_summary = first["summary_source_bytes"] == card["summary_source_bytes"]
                && (first_text.starts_with(other_text) || other_text.starts_with(first_text));
            if !same_summary || metadata.merge_origins(&other).is_err() {
                refused.push((
                    card["project_id"]
                        .as_str()
                        .expect("enrolled project identity")
                        .to_owned(),
                    key.0,
                ));
                continue;
            }
            first["origins"] =
                serde_json::to_value(metadata.origins()).expect("typed origins serialize");
        } else {
            positions.insert(key, unique.len());
            unique.push(card);
        }
    }
    *episodes = unique;
    refused
}

fn episode_reservation(available: usize, total: usize) -> usize {
    available.min(match total {
        0 | 1 => 0,
        2 => 1,
        _ => EPISODE_TARGET,
    })
}

/// Pool and final packing share the same soft episodic target. A one-card
/// budget prefers ordinary semantic recall; two cards give each lane one slot.
fn trim(primary: &mut Vec<Value>, episodes: &mut Vec<Value>, total: usize, episode_max: usize) {
    let reserved = episode_reservation(episodes.len(), total);
    let semantic_slots = total - reserved;
    primary.truncate(semantic_slots);
    episodes.truncate(episode_max.min(total.saturating_sub(primary.len())));
}

pub(super) fn trim_pool(primary: &mut Vec<Value>, episodes: &mut Vec<Value>, total: usize) {
    trim(primary, episodes, total, total);
}

pub(super) fn trim_items(primary: &mut Vec<Value>, episodes: &mut Vec<Value>) {
    trim(primary, episodes, MAX_ITEMS, EPISODE_MAX);
}

pub(super) fn update_omissions(result: &mut Value, considered: [usize; 2]) {
    for (index, lane) in ["primary", "episodes"].iter().enumerate() {
        let omitted = considered[index].saturating_sub(result[*lane].as_array().unwrap().len());
        result["omitted"][*lane] = json!(omitted);
        if omitted > 0 {
            result["partial"] = json!(true);
        }
    }
}

pub(super) fn mark_truncated(result: &mut Value, considered: [usize; 2]) {
    result["context_truncated"] = json!(true);
    result["partial"] = json!(true);
    update_omissions(result, considered);
}
