//! Disposable replay/projection only: no retrieval, provider, storage or scoring.
//! Fixture scopes are supplied historical bindings, never current-applicability
//! decisions. The native conditional-preference rule is the sole scalar policy.

use mneme_engine::conditional_preference::{
    ConditionalPreference, Judgment, PreferenceDecision, decide_update,
};
use serde::{Deserialize, Serialize, de::IgnoredAny};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;

const MAX_INPUT_BYTES: usize = 1024 * 1024;
const MAX_PAYLOAD_BYTES: usize = 10 * 1024;
type Result<T> = std::result::Result<T, String>;
type CellKey = (String, String, String);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Fixture {
    schema_version: u32,
    cases: Vec<Case>,
    #[serde(default, rename = "private_scoring")]
    _private_scoring: Option<IgnoredAny>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    id: String,
    current: Current,
    cards: Vec<Card>,
    historical_cards: Vec<Card>,
    scopes: Vec<Scope>,
    events: Vec<Event>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Current {
    goal: String,
    intended_use: String,
    facts: Vec<Fact>,
    options: Vec<Fact>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Fact {
    id: String,
    text: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Card {
    id: String,
    memory_id: String,
    presentation_revision: String,
    summary: String,
    source_ref: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Scope {
    id: String,
    memory_id: String,
    presentation_revision: String,
    intended_use: String,
    condition: String,
    source_refs: Vec<String>,
    refines_scope_id: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ReportedJudgment {
    Helpful,
    ActivelyMisled,
    Unknown,
}

impl From<ReportedJudgment> for Judgment {
    fn from(value: ReportedJudgment) -> Self {
        match value {
            ReportedJudgment::Helpful => Self::Helpful,
            ReportedJudgment::ActivelyMisled => Self::ActivelyMisled,
            ReportedJudgment::Unknown => Self::Unknown,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Event {
    id: String,
    source_task_id: String,
    memory_id: String,
    presentation_revision: String,
    scope_id: String,
    source_ref: String,
    reported_judgment: ReportedJudgment,
    note: String,
}

#[derive(Debug, Serialize)]
struct Output {
    schema_version: u32,
    fixture_sha256: String,
    cases: Vec<CaseOutput>,
}

#[derive(Debug, PartialEq, Serialize)]
struct CaseOutput {
    id: String,
    ordinary: Ordinary,
    notes: Notes,
    annotations: Vec<Annotation>,
    replay: Vec<ReplayRow>,
    states: Vec<Annotation>,
    payload_bytes: PayloadBytes,
}

#[derive(Debug, PartialEq, Serialize)]
struct Ordinary {
    current: Current,
    cards: Vec<Card>,
}

#[derive(Debug, PartialEq, Serialize)]
struct Notes {
    scopes: Vec<Scope>,
    events: Vec<Event>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
struct Annotation {
    card_id: String,
    memory_id: String,
    presentation_revision: String,
    scope_id: String,
    intended_use: String,
    condition: String,
    source_refs: Vec<String>,
    refines_scope_id: Option<String>,
    value: f32,
}

#[derive(Debug, PartialEq, Serialize)]
struct ReplayRow {
    event_id: String,
    source_task_id: String,
    memory_id: String,
    presentation_revision: String,
    scope_id: String,
    outcome: &'static str,
    value: Option<f32>,
}

#[derive(Debug, PartialEq, Serialize)]
struct PayloadBytes {
    ordinary: usize,
    notes: usize,
    annotated: usize,
}

fn require(condition: bool, message: &str) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(message.into())
    }
}

fn text(value: &str, max_bytes: usize) -> Result<()> {
    require(
        !value.trim().is_empty() && value.len() <= max_bytes && !value.contains('\0'),
        "blank, NUL-containing or oversized text",
    )
}

fn id(value: &str) -> Result<()> {
    require(
        !value.is_empty()
            && value.len() <= 80
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-.:".contains(&b)),
        "invalid identifier or revision",
    )
}

fn facts(values: &[Fact], min: usize, max: usize) -> Result<()> {
    require(
        (min..=max).contains(&values.len()),
        "invalid fact/option count",
    )?;
    let mut ids = BTreeSet::new();
    for fact in values {
        id(&fact.id)?;
        text(&fact.text, 512)?;
        require(ids.insert(&fact.id), "duplicate fact/option ID")?;
    }
    Ok(())
}

fn validate(case: &Case) -> Result<()> {
    id(&case.id)?;
    text(&case.current.goal, 1024)?;
    text(&case.current.intended_use, 256)?;
    facts(&case.current.facts, 1, 12)?;
    facts(&case.current.options, 2, 5)?;
    require(case.cards.len() == 6, "expected six current cards")?;
    require(
        case.historical_cards.len() <= 6,
        "too many historical cards",
    )?;
    require(
        case.scopes.len() <= 24 && case.events.len() <= 24,
        "too many scopes/events",
    )?;
    let mut card_ids = BTreeSet::new();
    let mut versions = BTreeSet::new();
    let current_memories: BTreeSet<_> = case.cards.iter().map(|c| c.memory_id.as_str()).collect();
    require(current_memories.len() == 6, "duplicate current memory")?;
    for card in case.cards.iter().chain(&case.historical_cards) {
        id(&card.id)?;
        id(&card.memory_id)?;
        id(&card.presentation_revision)?;
        text(&card.summary, 700)?;
        text(&card.source_ref, 160)?;
        require(
            current_memories.contains(card.memory_id.as_str()),
            "historical memory lacks current card",
        )?;
        require(card_ids.insert(&card.id), "duplicate card ID")?;
        require(
            versions.insert((&card.memory_id, &card.presentation_revision)),
            "duplicate memory revision",
        )?;
    }
    let mut scopes = BTreeMap::new();
    for scope in &case.scopes {
        id(&scope.id)?;
        id(&scope.memory_id)?;
        id(&scope.presentation_revision)?;
        text(&scope.intended_use, 256)?;
        text(&scope.condition, 512)?;
        require(
            versions.contains(&(&scope.memory_id, &scope.presentation_revision)),
            "scope refers to undeclared memory revision",
        )?;
        require(
            scopes.insert(scope.id.as_str(), scope).is_none(),
            "duplicate scope ID",
        )?;
        require(
            (1..=4).contains(&scope.source_refs.len()),
            "invalid scope source count",
        )?;
        let mut refs = BTreeSet::new();
        for reference in &scope.source_refs {
            text(reference, 160)?;
            require(refs.insert(reference), "duplicate scope source reference")?;
        }
    }
    for scope in &case.scopes {
        let mut cursor = scope;
        let mut visited = BTreeSet::from([scope.id.as_str()]);
        while let Some(parent_id) = &cursor.refines_scope_id {
            id(parent_id)?;
            let parent = scopes
                .get(parent_id.as_str())
                .ok_or("missing refinement scope")?;
            require(visited.insert(parent_id), "cyclic refinement")?;
            require(
                parent.memory_id == scope.memory_id
                    && parent.presentation_revision == scope.presentation_revision
                    && parent.intended_use == scope.intended_use,
                "refinement changes memory, revision or intended use",
            )?;
            cursor = parent;
        }
    }
    let mut event_ids = BTreeMap::new();
    let mut tasks = BTreeMap::new();
    for event in &case.events {
        for value in [
            &event.id,
            &event.source_task_id,
            &event.memory_id,
            &event.presentation_revision,
            &event.scope_id,
        ] {
            id(value)?;
        }
        text(&event.source_ref, 160)?;
        text(&event.note, 700)?;
        let scope = scopes
            .get(event.scope_id.as_str())
            .ok_or("event refers to missing scope")?;
        require(
            scope.memory_id == event.memory_id
                && scope.presentation_revision == event.presentation_revision,
            "event/scope memory revision mismatch",
        )?;
        require(
            scope.source_refs.contains(&event.source_ref),
            "event source not bound to scope",
        )?;
        if let Some(previous) = event_ids.insert(&event.id, event) {
            require(previous == event, "changed duplicate event ID")?;
        }
        if let Some(previous) = tasks.insert((&event.source_task_id, &event.memory_id), event) {
            require(previous == event, "changed duplicate task/memory judgment")?;
        }
    }
    require(tasks.len() <= 18, "too many distinct task/memory judgments")?;
    for scope in &case.scopes {
        for reference in &scope.source_refs {
            require(
                case.events
                    .iter()
                    .any(|event| event.scope_id == scope.id && event.source_ref == *reference),
                "scope cites no matching event",
            )?;
        }
    }
    Ok(())
}

fn derive(case: Case) -> Result<CaseOutput> {
    validate(&case)?;
    let mut cells: BTreeMap<CellKey, ConditionalPreference> = BTreeMap::new();
    let mut seen = BTreeSet::new();
    let mut unique_events = Vec::new();
    let mut replay = Vec::new();
    for event in &case.events {
        let key = (
            event.memory_id.clone(),
            event.presentation_revision.clone(),
            event.scope_id.clone(),
        );
        let outcome = if !seen.insert((&event.source_task_id, &event.memory_id)) {
            "duplicate"
        } else {
            unique_events.push(event.clone());
            let occupied = cells
                .keys()
                .filter(|(memory, revision, _)| {
                    memory == &event.memory_id && revision == &event.presentation_revision
                })
                .count();
            match decide_update(
                cells.get(&key).copied(),
                occupied,
                event.reported_judgment.into(),
            )
            .map_err(|error| error.to_string())?
            {
                PreferenceDecision::NoChange => "unknown",
                PreferenceDecision::Update(value) => {
                    cells.insert(key.clone(), value);
                    "updated"
                }
                PreferenceDecision::Admit(value) => {
                    cells.insert(key.clone(), value);
                    "admitted"
                }
                PreferenceDecision::RejectFull => "rejected_full",
            }
        };
        replay.push(ReplayRow {
            event_id: event.id.clone(),
            source_task_id: event.source_task_id.clone(),
            memory_id: event.memory_id.clone(),
            presentation_revision: event.presentation_revision.clone(),
            scope_id: event.scope_id.clone(),
            outcome,
            value: cells.get(&key).map(|value| value.value()),
        });
    }
    let mut states = Vec::new();
    for (key, value) in cells {
        let scope = case
            .scopes
            .iter()
            .find(|scope| scope.id == key.2)
            .expect("validated scope");
        let card = case
            .cards
            .iter()
            .chain(&case.historical_cards)
            .find(|card| card.memory_id == key.0 && card.presentation_revision == key.1)
            .expect("validated card version");
        states.push(Annotation {
            card_id: card.id.clone(),
            memory_id: key.0,
            presentation_revision: key.1,
            scope_id: key.2,
            intended_use: scope.intended_use.clone(),
            condition: scope.condition.clone(),
            source_refs: scope.source_refs.clone(),
            refines_scope_id: scope.refines_scope_id.clone(),
            value: value.value(),
        });
    }
    let annotations = case
        .cards
        .iter()
        .flat_map(|card| {
            states
                .iter()
                .filter(|state| state.card_id == card.id)
                .cloned()
        })
        .collect::<Vec<_>>();
    let ordinary = Ordinary {
        current: case.current,
        cards: case.cards,
    };
    let notes = Notes {
        scopes: case.scopes,
        events: unique_events,
    };
    let payloads = [
        serde_json::json!({"current": &ordinary.current, "cards": &ordinary.cards}),
        serde_json::json!({"current": &ordinary.current, "cards": &ordinary.cards, "notes": &notes}),
        serde_json::json!({"current": &ordinary.current, "cards": &ordinary.cards, "notes": &notes, "annotations": &annotations}),
    ];
    let sizes = payloads
        .iter()
        .map(|value| {
            serde_json::to_vec(value)
                .map(|bytes| bytes.len())
                .map_err(|error| error.to_string())
        })
        .collect::<Result<Vec<_>>>()?;
    require(
        sizes.iter().all(|size| *size <= MAX_PAYLOAD_BYTES),
        "projected payload exceeds 10 KiB; do not truncate shared notes",
    )?;
    Ok(CaseOutput {
        id: case.id,
        ordinary,
        notes,
        annotations,
        replay,
        states,
        payload_bytes: PayloadBytes {
            ordinary: sizes[0],
            notes: sizes[1],
            annotated: sizes[2],
        },
    })
}

fn replay_fixture(bytes: &[u8]) -> Result<Output> {
    require(bytes.len() <= MAX_INPUT_BYTES, "fixture exceeds 1 MiB")?;
    let fixture: Fixture = serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
    require(fixture.schema_version == 1, "unsupported fixture schema")?;
    require(
        (1..=12).contains(&fixture.cases.len()),
        "invalid case count",
    )?;
    let mut ids = BTreeSet::new();
    let mut cases = Vec::new();
    for case in fixture.cases {
        require(ids.insert(case.id.clone()), "duplicate case ID")?;
        cases.push(derive(case)?);
    }
    Ok(Output {
        schema_version: 1,
        fixture_sha256: format!("{:x}", Sha256::digest(bytes)),
        cases,
    })
}

fn run() -> Result<Output> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    require(
        args.len() == 1,
        "usage: experience_ablation_v1 FIXTURE.json",
    )?;
    let mut bytes = Vec::new();
    std::fs::File::open(&args[0])
        .map_err(|error| error.to_string())?
        .take((MAX_INPUT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    replay_fixture(&bytes)
}

fn main() {
    match run().and_then(|output| serde_json::to_string(&output).map_err(|error| error.to_string()))
    {
        Ok(output) => println!("{output}"),
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
#[path = "experience_ablation_v1/tests.rs"]
mod tests;
