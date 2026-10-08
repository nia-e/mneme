//! A small durable envelope around read-only requests. This is not an auth boundary:
//! the coordinator exposes only request/finish, not its files or CLI controls.
use super::{Result, fixture, require};
use mneme_app::episode::PreparedEpisode;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

pub const MAX_CALLS: usize = 12;
pub const MAX_RESPONSE_BYTES: usize = 8192;
pub const MAX_REQUEST_BYTES: usize = 4096;
const MAX_ANSWER_WORDS: usize = 250;
const DEFAULT_RESERVATION: usize = 2048;
const MIN_RESERVATION: usize = 256;

#[derive(Serialize, Deserialize)]
struct State {
    schema: String,
    fixture_sha256: String,
    case: String,
    calls: usize,
    response_bytes: usize,
    exhausted: bool,
    final_packet: Option<Value>,
    trace: Vec<Value>,
}

struct Lock(PathBuf);
impl Lock {
    fn acquire(dir: &Path) -> Result<Self> {
        let path = dir.join("request.lock");
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        Ok(Self(path))
    }
}
impl Drop for Lock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn load(dir: &Path) -> Result<State> {
    let state: State = serde_json::from_slice(&fs::read(dir.join("session.json"))?)?;
    require(
        state.schema == "mneme.episodic-v1.actor.v1",
        "wrong actor session schema",
    )?;
    require(
        state.fixture_sha256 == fixture::fingerprint(),
        "fixture changed during actor session",
    )?;
    require(
        state.calls <= MAX_CALLS && state.response_bytes <= MAX_RESPONSE_BYTES,
        "invalid actor counters",
    )?;
    Ok(state)
}

fn save(dir: &Path, state: &State) -> Result<()> {
    let pending = dir.join("session.pending");
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&pending)?;
    file.write_all(&serde_json::to_vec_pretty(state)?)?;
    file.sync_all()?;
    fs::rename(&pending, dir.join("session.json"))?;
    Ok(())
}

fn case(id: &str) -> Result<Value> {
    fixture::assessment()["qualitative"]["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["id"] == id)
        .cloned()
        .ok_or_else(|| "unknown actor case".into())
}

pub fn initialize(dir: &Path, case_id: &str) -> Result<Value> {
    let question = case(case_id)?["question"].clone();
    fs::create_dir(dir)?; // Never reset a previous actor's counters.
    let _lock = Lock::acquire(dir)?;
    save(
        dir,
        &State {
            schema: "mneme.episodic-v1.actor.v1".into(),
            fixture_sha256: fixture::fingerprint(),
            case: case_id.into(),
            calls: 0,
            response_bytes: 0,
            exhausted: false,
            final_packet: None,
            trace: Vec::new(),
        },
    )?;
    Ok(json!({
        "question":question,"as_of_ms":fixture::corpus()["as_of_ms"],
        "context":"You are recalling Rin's synthetic experiences. Read only through these operations; do not inspect coordinator files, fixture contents, source code, other answers or live stores.",
        "budget":{"memory_calls":MAX_CALLS,"tool_response_bytes":MAX_RESPONSE_BYTES,"answer_words":MAX_ANSWER_WORDS},
        "request_envelope":{"request":"one operation below","max_response_bytes":"optional 256..4096; default min(2048, remaining budget). Oversize results return a charged error, not partial JSON. Narrow limit/max_bytes."},
        "operations":[
            {"action":"list","axis":"recorded|occurred (default recorded)","order":"newest_first|oldest_first","from":"optional epoch milliseconds","through":"optional epoch milliseconds","thread":"optional exact thread","limit":"1..4, use 2 or 3","after":"optional returned cursor"},
            {"action":"search","cue":"plain lexical text over CURRENT EPISODE SUMMARIES, not semantic paraphrase matching","thread":"optional exact thread","limit":"1..4"},
            {"action":"get","episode_id":"returned root ID","edition_id":"optional exact historical edition","body":true,"max_bytes":"1..1024, use 512","offset":"optional byte offset"},
            {"action":"history","episode_id":"returned root ID","limit":"1..4","after":"optional returned cursor"},
            {"action":"references","anchor":"returned exact episode edition or semantic node ID; both incoming/outgoing episode-incident edges","limit":"1..4","after":"optional returned cursor"},
            {"action":"semantic_search","text":"retrieve current lessons with reference hashing embeddings; inspect probationary status","limit":"1..4, default 3"},
            {"action":"exact","id":"returned node ID; exact edition or semantic lesson","max_bytes":"1..1024, default 512"}
        ],
        "final_packet":{"answer":"concise recollection","episode_ids":["actual returned IDs"],"lesson_ids":["actual returned IDs if relevant"],"uncertainty":"optional; total prose <=250 words"},
        "note":"No lesson is mandatory. Distinguish then from now and planned from completed. A historical analogy is not evidence of a new incident's cause. Timeline cursors are keyset positions, not snapshots."
    }))
}

fn bounded_usize(value: &Value, name: &str, default: usize, maximum: usize) -> Result<usize> {
    let n = match value.get(name) {
        None => default,
        Some(value) => usize::try_from(value.as_u64().ok_or("limit must be an integer")?)?,
    };
    require(n > 0 && n <= maximum, "actor read bound exceeded")?;
    Ok(n)
}

async fn execute(request: &Value) -> Result<Value> {
    let object = request.as_object().ok_or("request must be an object")?;
    let action = request["action"].as_str().ok_or("action is required")?;
    match action {
        "semantic_search" => {
            require(
                object
                    .keys()
                    .all(|k| ["action", "text", "limit"].contains(&k.as_str())),
                "unknown semantic-search field",
            )?;
            let text = request["text"].as_str().ok_or("text is required")?;
            require(
                !text.trim().is_empty() && text.len() <= 1024,
                "text must be 1..1024 bytes",
            )?;
            let limit = bounded_usize(request, "limit", 3, 4)?;
            fixture::Fixture::load()
                .await?
                .semantic_search(text, limit)
                .await
        }
        "exact" => {
            require(
                object
                    .keys()
                    .all(|k| ["action", "id", "max_bytes"].contains(&k.as_str())),
                "unknown exact-read field",
            )?;
            let id = fixture::parse_id(&request["id"])?;
            let max_bytes = bounded_usize(request, "max_bytes", 512, 1024)?;
            fixture::Fixture::load().await?.exact(id, max_bytes).await
        }
        "list" | "search" | "get" | "history" | "references" => {
            let mut request = request.clone();
            if action != "get" {
                request["limit"] = json!(bounded_usize(&request, "limit", 3, 4)?);
            } else if request["body"] == true {
                request["max_bytes"] = json!(bounded_usize(&request, "max_bytes", 512, 1024)?);
            }
            let prepared = PreparedEpisode::parse(&request)?;
            require(!prepared.is_mutation(), "actor cannot mutate memory")?;
            let fixture = fixture::Fixture::load().await?;
            prepared
                .run(&fixture.memory, fixture.store.db_id(), None)
                .await
        }
        _ => Err(
            "unsupported actor read action; writes and coordinator controls are unavailable".into(),
        ),
    }
}

pub async fn request(dir: &Path, envelope: Value) -> Result<Value> {
    let _lock = Lock::acquire(dir)?;
    let mut state = load(dir)?;
    require(state.final_packet.is_none(), "actor already finished")?;
    if state.exhausted
        || state.calls >= MAX_CALLS
        || MAX_RESPONSE_BYTES - state.response_bytes < MIN_RESERVATION
    {
        state.exhausted = true;
        save(dir, &state)?;
        return Err(
            "actor memory envelope exhausted; no read executed; finish with available evidence"
                .into(),
        );
    }
    let remaining = MAX_RESPONSE_BYTES - state.response_bytes;
    let parsed = (|| -> Result<(Value, usize)> {
        require(
            serde_json::to_vec(&envelope)?.len() <= MAX_REQUEST_BYTES,
            "request exceeds 4096 bytes",
        )?;
        let fields = envelope.as_object().ok_or("envelope must be an object")?;
        require(
            fields
                .keys()
                .all(|k| ["request", "max_response_bytes"].contains(&k.as_str())),
            "unknown request-envelope field",
        )?;
        let request = envelope
            .get("request")
            .ok_or("request is required")?
            .clone();
        let reservation = bounded_usize(
            &envelope,
            "max_response_bytes",
            DEFAULT_RESERVATION.min(remaining),
            4096,
        )?;
        require(
            reservation >= MIN_RESERVATION && reservation <= remaining,
            "response reservation does not fit remaining envelope",
        )?;
        Ok((request, reservation))
    })();
    let (result, reservation) = match parsed {
        Ok((request, reserved)) => (execute(&request).await, reserved),
        Err(error) => (Err(error), DEFAULT_RESERVATION.min(remaining)),
    };
    let mut response = match result {
        Ok(result) => json!({"ok":true,"result":result}),
        Err(error) => {
            let message: String = error.to_string().chars().take(120).collect();
            json!({"ok":false,"error":message})
        }
    };
    if serde_json::to_vec(&response)?.len() > reservation {
        response = json!({"ok":false,"error":"response exceeds reserved bytes; narrow limit/max_bytes or request a larger reservation"});
    }
    let bytes = serde_json::to_vec(&response)?.len();
    require(
        bytes <= reservation && bytes <= remaining,
        "internal response-envelope error",
    )?;
    state.calls += 1;
    state.response_bytes += bytes;
    state.trace.push(json!({"request":envelope,"response":response,"response_bytes":bytes,"reserved_bytes":reservation}));
    save(dir, &state)?; // Commit accounting before any answer is printed.
    Ok(response)
}

pub fn finish(dir: &Path, packet: Value) -> Result<Value> {
    let _lock = Lock::acquire(dir)?;
    let mut state = load(dir)?;
    require(state.final_packet.is_none(), "actor already finished")?;
    let object = packet.as_object().ok_or("final packet must be an object")?;
    require(
        object
            .keys()
            .all(|k| ["answer", "episode_ids", "lesson_ids", "uncertainty"].contains(&k.as_str())),
        "unknown final-packet field",
    )?;
    let answer = packet["answer"].as_str().ok_or("answer is required")?;
    require(!answer.trim().is_empty(), "answer is empty")?;
    let uncertainty = match packet.get("uncertainty") {
        None => "",
        Some(value) => value.as_str().ok_or("uncertainty must be text")?,
    };
    let words = answer.split_whitespace().count() + uncertainty.split_whitespace().count();
    require(words <= MAX_ANSWER_WORDS, "final prose exceeds 250 words")?;
    for field in ["episode_ids", "lesson_ids"] {
        let ids = packet[field]
            .as_array()
            .ok_or("reference arrays are required")?;
        require(ids.len() <= 12, "too many final references")?;
        for id in ids {
            fixture::parse_id(id)?;
        }
    }
    state.final_packet = Some(packet);
    save(dir, &state)?;
    Ok(
        json!({"finished":true,"answer_words":words,"memory_calls":state.calls,"tool_response_bytes":state.response_bytes,"assessment":"coordinator-only; no automatic qualitative grade"}),
    )
}

pub fn artifact(dir: &Path) -> Result<Value> {
    let _lock = Lock::acquire(dir)?;
    let state = load(dir)?;
    Ok(
        json!({"actor_kind":"request_driven_unclassified","case":state.case,"state":state,
              "claims":{"fresh_actor_evidence":"coordinator must attach actual actor provenance; this driver cannot establish it",
                        "qualitative_grade":"not computed","provider_tokens":null,"provider_cost":null}}),
    )
}
