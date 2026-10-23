use super::*;
use serde_json::{Value, json};
use std::fs;
use std::sync::atomic::AtomicU64;

const FIXTURE: &[u8] =
    include_bytes!("../../../../integrations/codex/fixtures/session-activity.json");
static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

struct Sandbox {
    base: PathBuf,
    workspace: PathBuf,
    root: PathBuf,
    manifest: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let base = std::env::temp_dir().join(format!(
            "mneme-session-activity-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&base).unwrap();
        let base = fs::canonicalize(base).unwrap();
        let workspace = base.join("workspace");
        let root = base.join("activity");
        let manifest = base.join("activity.json");
        fs::create_dir(&workspace).unwrap();
        fs::create_dir(&root).unwrap();
        let this = Self {
            base,
            workspace,
            root,
            manifest,
        };
        this.configure(&this.root, &[(&this.workspace, "codex")]);
        this
    }

    fn configure(&self, root: &Path, bindings: &[(&Path, &str)]) {
        let bindings = bindings
            .iter()
            .map(|(path, source)| json!({"path": path, "source": source}))
            .collect::<Vec<_>>();
        fs::write(
            &self.manifest,
            serde_json::to_vec(&json!({
                "schema": MANIFEST_SCHEMA,
                "root": root,
                "workspaces": bindings,
                "device": null,
            }))
            .unwrap(),
        )
        .unwrap();
    }

    fn record(&self, id: &str) -> Value {
        let mut record: Value = serde_json::from_slice(FIXTURE).unwrap();
        record["session_id"] = json!(id);
        record["workspace"] = json!(self.workspace);
        record["cwd"] = json!(self.workspace);
        record
    }

    fn write_value(&self, id: &str, record: &Value) -> PathBuf {
        let path = self.root.join(record_filename("codex", id));
        fs::write(&path, serde_json::to_vec(record).unwrap()).unwrap();
        path
    }

    fn add(&self, id: &str) -> PathBuf {
        self.write_value(id, &self.record(id))
    }

    fn report(&self, session: Option<&str>, details: bool) -> ActivityReport {
        report(
            &self.manifest,
            &self.workspace,
            ActivityRequest {
                session: session.map(str::to_owned),
                details,
            },
        )
        .unwrap()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.base).unwrap();
    }
}

fn issue(report: &ActivityReport, expected: CoverageIssue) -> bool {
    report
        .coverage
        .issues
        .get(&expected)
        .copied()
        .unwrap_or_default()
        > 0
}

fn snapshot(path: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn visit(root: &Path, at: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in fs::read_dir(at).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path).unwrap();
            if metadata.is_dir() {
                files.insert(
                    path.strip_prefix(root).unwrap().to_owned(),
                    b"directory".to_vec(),
                );
                visit(root, &path, files);
            } else if metadata.file_type().is_symlink() {
                files.insert(
                    path.strip_prefix(root).unwrap().to_owned(),
                    fs::read_link(&path)
                        .unwrap()
                        .as_os_str()
                        .as_encoded_bytes()
                        .to_vec(),
                );
            } else {
                files.insert(
                    path.strip_prefix(root).unwrap().to_owned(),
                    fs::read(path).unwrap(),
                );
            }
        }
    }
    let mut files = BTreeMap::new();
    visit(path, path, &mut files);
    files
}

#[test]
fn shared_fixture_preserves_distinct_evidence_and_native_generation() {
    let record = decode_record(FIXTURE).unwrap();
    assert_eq!(record.schema, RECORD_SCHEMA);
    assert!(record.events.iter().any(|event| matches!(&event.origin.generation, Some(Generation::Id(id)) if id == "0123456789abcdef0123456789abcdef")));
    let view = project(record, true);
    let human = render_human(&ActivityReport {
        schema: REPORT_SCHEMA,
        status: ActivityStatus::Ready,
        scope: None,
        sessions: vec![view],
        coverage: Coverage::default(),
    });
    assert!(human.contains("consumed_for_emission"));
    assert!(human.contains("packing_limit"));
    assert!(human.contains("maintenance_verified"));
    assert!(human.contains("actor_reported_capture"));
    assert!(human.contains("No retained observation: host admission"));
    assert!(!human.contains("save_confirmed"));
    assert!(human.contains("(UTC)"));
}

fn human_lane<'a>(human: &'a str, label: &str) -> &'a str {
    human
        .lines()
        .find(|line| line.trim_start().starts_with(label))
        .unwrap_or_else(|| panic!("missing {label} lane in {human}"))
}

fn human_for_record(record: SessionRecord) -> String {
    render_human(&ActivityReport {
        schema: REPORT_SCHEMA,
        status: ActivityStatus::Ready,
        scope: None,
        sessions: vec![project(record, false)],
        coverage: Coverage::default(),
    })
}

#[test]
fn concise_single_observation_prose_preserves_each_evidence_stage() {
    let human = human_for_record(decode_record(FIXTURE).unwrap());
    println!("{human}");
    let core = human_lane(&human, "Startup core:");
    assert!(core.contains("1 note") && core.contains("prepared") && core.contains("for project"));
    assert!(!core.contains("saved") && !core.contains("admitted"));

    let recall = human_lane(&human, "Recall:");
    assert!(recall.contains("skipped") && recall.contains("disabled mode"));
    let selection = human_lane(&human, "Recall selection:");
    assert!(
        selection.contains("1 note")
            && selection.contains("selected")
            && selection.contains("for project")
    );
    assert!(
        !selection.contains("admitted")
            && !selection.contains("emitted")
            && !selection.contains("used")
    );
    let delivery = human_lane(&human, "Delivery:");
    assert!(delivery.contains("1 note") && delivery.contains("consumed for emission"));
    assert!(!delivery.contains("admitted") && !delivery.contains("used"));
    let output = human_lane(&human, "Hook output:");
    assert!(output.contains("omitted") && output.contains("packing limit"));
    assert!(!output.contains("prepared") && !output.contains("admitted"));
    let recording = human_lane(&human, "Recording:");
    assert!(recording.contains("maintenance verified") && recording.contains("for project"));
    assert!(!recording.contains("save"));
    let checkpoint = human_lane(&human, "Checkpoint:");
    assert!(checkpoint.contains("capture reported by actor") && checkpoint.contains("unverified"));
    assert!(!checkpoint.contains("save confirmed"));
    assert!(human.contains("No retained observation: host admission"));
    assert!(!human.contains("Host admission: 0"));
    for line in [
        core, recall, selection, delivery, output, recording, checkpoint,
    ] {
        assert!(
            !line.contains("1 observation"),
            "unnecessary single-event boilerplate: {line}"
        );
        assert!(
            !line.contains("latest receipt count"),
            "unnecessary receipt jargon: {line}"
        );
    }
}

#[test]
fn native_core_ok_means_prepared_and_missing_note_counts_do_not_become_zero() {
    let fixture = decode_record(FIXTURE).unwrap();
    let mut core = fixture
        .events
        .iter()
        .find(|event| event.kind == EventKind::Core)
        .unwrap()
        .clone();
    core.outcome = "ok".into();
    let mut record = fixture.clone();
    record.events = vec![core.clone()];
    let human = human_for_record(record);
    let line = human_lane(&human, "Startup core:");
    assert!(line.contains("1 note prepared") && line.contains("for project"));
    assert!(!line.contains(": ok"));

    for (kind, outcome, label) in [
        (EventKind::Core, "ok", "Startup core:"),
        (EventKind::RecallSelection, "selected", "Recall selection:"),
        (EventKind::DeliveryConsumed, "consumed", "Delivery:"),
        (EventKind::HookOutput, "prepared", "Hook output:"),
    ] {
        let mut unknown = core.clone();
        unknown.kind = kind;
        unknown.outcome = outcome.into();
        unknown.count = None;
        unknown.nodes.clear();
        let mut record = fixture.clone();
        record.events = vec![unknown];
        let human = human_for_record(record);
        let line = human_lane(&human, label);
        assert!(line.contains("note count unknown"), "{line}");
        assert!(
            !line.contains("0 notes") && !line.contains("no notes"),
            "{line}"
        );
    }
}

#[test]
fn actor_reported_capture_is_unverified_without_an_explicit_reason() {
    let mut record = decode_record(FIXTURE).unwrap();
    let mut checkpoint = record
        .events
        .iter()
        .find(|event| event.kind == EventKind::Checkpoint)
        .unwrap()
        .clone();
    checkpoint.reason = None;
    record.events = vec![checkpoint];
    let human = human_for_record(record);
    let line = human_lane(&human, "Checkpoint:");
    assert!(line.contains("capture reported by actor"));
    assert!(line.contains("unverified"));
    assert!(!line.contains("save confirmed") && !line.contains("capture confirmed"));
}

#[test]
fn negative_admission_observation_is_limited_to_the_admitted_transcript_slice() {
    let mut record = decode_record(FIXTURE).unwrap();
    let mut admission = record.events[0].clone();
    admission.kind = EventKind::HostAdmission;
    admission.outcome = "not_admitted".into();
    admission.reason = None;
    record.events = vec![admission];
    let human = human_for_record(record);
    let line = human_lane(&human, "Host admission:");
    assert!(line.contains("admission not observed") && line.contains("transcript slice"));
    assert!(!line.contains("not admitted"));
    assert!(!line.contains("never") && !line.contains("rejected") && !line.contains("failed"));
}

#[test]
fn multiple_observations_show_latest_note_count_without_inventing_a_total() {
    let mut record = decode_record(FIXTURE).unwrap();
    let prototype = record
        .events
        .iter()
        .find(|event| event.kind == EventKind::Core)
        .unwrap()
        .clone();
    let mut first = prototype.clone();
    first.key = "first-seven".into();
    first.count = Some(7);
    first.omitted = 6;
    let mut omitted = prototype.clone();
    omitted.key = "second-omitted".into();
    omitted.at += 1;
    omitted.outcome = "omitted".into();
    omitted.reason = Some("packing_limit".into());
    omitted.count = Some(0);
    omitted.nodes.clear();
    let mut latest = prototype;
    latest.key = "latest-eleven".into();
    latest.at = record.last_observed;
    latest.count = Some(11);
    latest.omitted = 10;
    record.events = vec![first, omitted, latest];
    let human = human_for_record(record);
    let core = human_lane(&human, "Startup core:");
    assert!(core.contains("11 notes prepared") && core.contains("for project"));
    assert!(core.contains("latest of 3 observations"));
    assert!(core.contains("2 prepared") && core.contains("1 omitted"));
    assert!(!core.contains("18 notes") && !core.contains("total"));
}

#[test]
fn save_receipt_count_is_referenced_ids_not_a_fabricated_saved_note_count() {
    let mut record = decode_record(FIXTURE).unwrap();
    let mut saved = record
        .events
        .iter()
        .find(|event| event.kind == EventKind::Recording)
        .unwrap()
        .clone();
    saved.outcome = "save_confirmed".into();
    saved.count = Some(2);
    // A native commit receipt may identify an episode and its edition rather
    // than two separately saved notes. The formatter must not recategorize it.
    saved.nodes = vec!["episode-id".into(), "edition-id".into()];
    record.events = vec![saved];
    let human = human_for_record(record);
    let recording = human_lane(&human, "Recording:");
    assert!(recording.contains("save confirmed") && recording.contains("for project"));
    assert!(
        recording.contains('2')
            && recording.contains("referenced")
            && recording.to_lowercase().contains("ids")
    );
    assert!(!recording.contains("notes saved") && !recording.contains("2 notes"));
}

#[test]
fn strict_record_rejects_payloads_versions_types_depth_and_byte_overflow() {
    let fixture: Value = serde_json::from_slice(FIXTURE).unwrap();
    for level in ["record", "event", "origin", "owner"] {
        let mut bad = fixture.clone();
        let target = match level {
            "record" => &mut bad,
            "event" => &mut bad["events"][0],
            "origin" => &mut bad["events"][0]["origin"],
            _ => &mut bad["events"][1]["owner"],
        };
        target["body"] = json!("secret payload never admitted");
        assert!(
            matches!(
                decode_record(&serde_json::to_vec(&bad).unwrap()),
                Err(CoverageIssue::Malformed)
            ),
            "{level}"
        );
    }
    let mut bad = fixture.clone();
    bad["schema"] = json!("mneme.session-activity.v99");
    assert!(matches!(
        decode_record(&serde_json::to_vec(&bad).unwrap()),
        Err(CoverageIssue::Unsupported)
    ));
    for invalid in [
        json!(true),
        json!(-1),
        json!(1.25),
        json!({"unexpected": 1}),
    ] {
        let mut bad = fixture.clone();
        bad["events"][0]["origin"]["generation"] = invalid;
        assert!(matches!(
            decode_record(&serde_json::to_vec(&bad).unwrap()),
            Err(CoverageIssue::Malformed)
        ));
    }
    let deep = format!(
        "{}0{}",
        "[".repeat(MAX_JSON_DEPTH + 1),
        "]".repeat(MAX_JSON_DEPTH + 1)
    );
    assert!(matches!(
        decode_record(deep.as_bytes()),
        Err(CoverageIssue::Malformed)
    ));
    assert!(matches!(
        decode_record(&vec![b' '; MAX_RECORD_BYTES + 1]),
        Err(CoverageIssue::Oversize)
    ));
}

#[test]
fn duplicate_json_fields_are_malformed_instead_of_last_value_wins() {
    let fixture = std::str::from_utf8(FIXTURE).unwrap();
    for (needle, duplicate) in [
        (
            "\"schema\":",
            "\"schema\":\"mneme.session-activity.v0\",\"schema\":",
        ),
        ("\"kind\":", "\"kind\":\"lifecycle\",\"kind\":"),
        ("\"turn\":", "\"turn\":null,\"turn\":"),
        ("\"name\":", "\"name\":\"other\",\"name\":"),
    ] {
        let bad = fixture.replacen(needle, duplicate, 1);
        assert_ne!(bad, fixture, "fixture must contain {needle}");
        assert!(
            matches!(decode_record(bad.as_bytes()), Err(CoverageIssue::Malformed)),
            "{needle}"
        );
    }
}

#[test]
fn explicit_nullable_receipt_fields_are_required_but_manifest_device_is_optional() {
    let fixture: Value = serde_json::from_slice(FIXTURE).unwrap();
    for (level, field) in [
        ("record", "device"),
        ("event", "reason"),
        ("event", "owner"),
        ("event", "count"),
        ("event", "selection_digest"),
        ("origin", "turn"),
        ("origin", "generation"),
        ("origin", "context_generation"),
        ("origin", "attempt"),
    ] {
        let mut omitted = fixture.clone();
        let target = match level {
            "record" => &mut omitted,
            "event" => &mut omitted["events"][0],
            _ => &mut omitted["events"][0]["origin"],
        };
        assert!(target.as_object_mut().unwrap().remove(field).is_some());
        assert!(
            matches!(
                decode_record(&serde_json::to_vec(&omitted).unwrap()),
                Err(CoverageIssue::Malformed)
            ),
            "{level}.{field}"
        );
    }
    let mut explicit_null = fixture;
    explicit_null["device"] = Value::Null;
    explicit_null["events"][0]["origin"]["turn"] = Value::Null;
    assert!(decode_record(&serde_json::to_vec(&explicit_null).unwrap()).is_ok());

    let manifest: Manifest = serde_json::from_value(json!({
        "schema": MANIFEST_SCHEMA,
        "root": "/activity/example",
        "workspaces": [],
    }))
    .unwrap();
    assert!(
        manifest.device.is_none(),
        "manifest device is optional, unlike fixed receipt fields"
    );
    #[cfg(unix)]
    {
        let sandbox = Sandbox::new();
        sandbox.add("optional-manifest-device");
        let mut manifest: Value =
            serde_json::from_slice(&fs::read(&sandbox.manifest).unwrap()).unwrap();
        manifest.as_object_mut().unwrap().remove("device");
        fs::write(&sandbox.manifest, serde_json::to_vec(&manifest).unwrap()).unwrap();
        assert_eq!(sandbox.report(None, false).status, ActivityStatus::Ready);
    }
}

#[test]
fn summary_keeps_save_and_maintenance_counts_and_latest_owner_qualification() {
    let mut record = decode_record(FIXTURE).unwrap();
    let prototype = record
        .events
        .iter()
        .find(|event| event.kind == EventKind::Recording)
        .unwrap()
        .clone();
    let owner = |id: &str| EventOwner {
        name: id.to_owned(),
        id: id.to_owned(),
        db: "project".into(),
    };
    let mut saved = prototype.clone();
    saved.key = "saved-first".into();
    saved.at = record.first_observed + 2;
    saved.outcome = "save_confirmed".into();
    saved.owner = Some(owner("saved-owner"));
    saved.count = Some(3);

    let mut latest = prototype.clone();
    latest.key = "maintenance-latest".into();
    latest.at = record.last_observed;
    latest.owner = Some(owner("latest-owner"));
    latest.count = Some(0);

    let mut delayed = prototype;
    delayed.key = "saved-delayed".into();
    delayed.at = record.first_observed + 1;
    delayed.outcome = "save_confirmed".into();
    delayed.owner = Some(owner("delayed-owner"));
    delayed.count = Some(7);
    record.events = vec![saved, latest, delayed];

    let projected = project(record, false);
    let lane = &projected.summary[0];
    assert_eq!(lane.observed_events, 3);
    // These are observed outcome counts, not a fabricated total of saved nodes.
    assert_eq!(lane.outcomes.get("save_confirmed"), Some(&2));
    assert_eq!(lane.outcomes.get("maintenance_verified"), Some(&1));
    assert_eq!(lane.last_outcome.as_deref(), Some("maintenance_verified"));
    assert_eq!(lane.last_count, Some(0));
    assert_eq!(lane.last_owner.as_ref().unwrap().id, "latest-owner");
    assert_eq!(lane.last_at, projected.last_observed);
}

#[test]
fn strict_record_rejects_unqualified_nodes_bad_codes_and_event_identity() {
    let fixture: Value = serde_json::from_slice(FIXTURE).unwrap();
    for (pointer, replacement) in [
        ("/events/1/owner", Value::Null),
        (
            "/events/0/outcome",
            json!("Exception: secret provider text"),
        ),
        ("/events/0/reason", json!("two words")),
        ("/events/0/kind", json!("future_unknown_kind")),
        ("/events/0/at", json!(0)),
        ("/events/1/nodes/0", json!("not an identifier")),
        ("/events/0/origin/turn", json!("control\ntext")),
    ] {
        let mut bad = fixture.clone();
        *bad.pointer_mut(pointer).unwrap() = replacement;
        assert!(
            matches!(
                decode_record(&serde_json::to_vec(&bad).unwrap()),
                Err(CoverageIssue::Malformed)
            ),
            "{pointer}"
        );
    }
    let mut duplicate = fixture;
    duplicate["events"][1]["key"] = duplicate["events"][0]["key"].clone();
    assert!(matches!(
        decode_record(&serde_json::to_vec(&duplicate).unwrap()),
        Err(CoverageIssue::Malformed)
    ));
}

#[test]
fn delayed_append_does_not_replace_later_observed_lane_state() {
    let mut record = decode_record(FIXTURE).unwrap();
    let mut earlier = record.events[0].clone();
    earlier.key = "earlier-delayed".into();
    earlier.at = record.first_observed;
    earlier.outcome = "resume".into();
    record.events[0].at = record.last_observed;
    record.events[0].outcome = "end".into();
    record.events.push(earlier);
    let projected = project(record, false);
    let lifecycle = projected
        .summary
        .iter()
        .find(|lane| lane.kind == EventKind::Lifecycle)
        .unwrap();
    assert_eq!(lifecycle.last_outcome.as_deref(), Some("end"));
    assert_eq!(lifecycle.observed_events, 2);
}

#[test]
fn same_second_lifecycle_conflict_is_ambiguous_in_both_append_orders() {
    let fixture = decode_record(FIXTURE).unwrap();
    let mut startup = fixture.events[0].clone();
    startup.at = fixture.last_observed;
    let mut end = startup.clone();
    end.key = "same-second-end".into();
    end.outcome = "end".into();
    let calendar = mneme_present::format_timestamp_millis(u128::from(startup.at) * 1000);
    for events in [
        vec![startup.clone(), end.clone()],
        vec![end.clone(), startup.clone()],
    ] {
        let mut record = fixture.clone();
        record.events = events;
        let projected = project(record, false);
        let lane = &projected.summary[0];
        assert_eq!(
            lane.latest_status,
            LatestObservationStatus::AmbiguousTimestamp
        );
        assert!(lane.last_outcome.is_none());
        assert!(lane.last_reason.is_none());
        assert!(lane.last_owner.is_none());
        assert!(lane.last_count.is_none());
        assert_eq!(lane.last_at, startup.at);
        assert_eq!(lane.outcomes.get("startup"), Some(&1));
        assert_eq!(lane.outcomes.get("end"), Some(&1));
        let encoded = serde_json::to_value(lane).unwrap();
        assert_eq!(encoded["latest_status"], "ambiguous_timestamp");
        assert!(encoded["last_outcome"].is_null());
        let human = render_human(&ActivityReport {
            schema: REPORT_SCHEMA,
            status: ActivityStatus::Ready,
            scope: None,
            sessions: vec![projected],
            coverage: Coverage::default(),
        });
        assert!(human.contains(&format!("latest observation ambiguous at {calendar}")));
        assert!(!human.contains("; latest startup"));
        assert!(!human.contains("; latest end"));
    }
}

#[test]
fn any_conflicting_same_second_summary_fact_clears_all_latest_scalars() {
    let fixture = decode_record(FIXTURE).unwrap();
    let mut first = fixture
        .events
        .iter()
        .find(|event| event.kind == EventKind::Recording)
        .unwrap()
        .clone();
    first.at = fixture.last_observed;
    first.outcome = "save_confirmed".into();
    first.reason = Some("verified".into());
    first.count = Some(3);
    for changed in [
        "outcome",
        "reason",
        "reason_absent",
        "owner_name",
        "owner_id",
        "owner_db",
        "owner_absent",
        "count",
        "count_absent",
    ] {
        let mut conflicting = first.clone();
        conflicting.key = format!("conflicting-{changed}");
        match changed {
            "outcome" => conflicting.outcome = "maintenance_verified".into(),
            "reason" => conflicting.reason = Some("other_reason".into()),
            "reason_absent" => conflicting.reason = None,
            "owner_name" => conflicting.owner.as_mut().unwrap().name = "other-name".into(),
            "owner_id" => conflicting.owner.as_mut().unwrap().id = "other-id".into(),
            "owner_db" => conflicting.owner.as_mut().unwrap().db = "other-db".into(),
            "owner_absent" => conflicting.owner = None,
            "count" => conflicting.count = Some(4),
            "count_absent" => conflicting.count = None,
            _ => unreachable!(),
        }
        for reverse in [false, true] {
            let mut older = first.clone();
            older.key = "older-delayed".into();
            older.at -= 1;
            let mut agreeing = first.clone();
            agreeing.key = "same-time-agreeing".into();
            let mut record = fixture.clone();
            record.events = if reverse {
                vec![conflicting.clone(), first.clone(), agreeing, older]
            } else {
                vec![first.clone(), conflicting.clone(), agreeing, older]
            };
            let projected = project(record, false);
            let lane = &projected.summary[0];
            assert_eq!(
                lane.latest_status,
                LatestObservationStatus::AmbiguousTimestamp,
                "{changed}; reverse={reverse}"
            );
            assert!(lane.last_outcome.is_none(), "{changed}");
            assert!(lane.last_reason.is_none(), "{changed}");
            assert!(lane.last_owner.is_none(), "{changed}");
            assert!(lane.last_count.is_none(), "{changed}");
            assert_eq!(lane.observed_events, 4);
            assert_eq!(lane.outcomes.values().sum::<u64>(), 4);
        }
    }
}

#[test]
fn equivalent_same_second_summary_facts_remain_observed_despite_provenance_differences() {
    let mut record = decode_record(FIXTURE).unwrap();
    let first = record
        .events
        .iter()
        .find(|event| event.kind == EventKind::Core)
        .unwrap()
        .clone();
    let mut second = first.clone();
    second.key = "equivalent-core-observation".into();
    second.origin.turn = Some("another-turn".into());
    second.origin.generation = Some(Generation::Id("another-generation".into()));
    second.origin.attempt = Some("another-attempt".into());
    second.nodes = vec!["01ARZ3NDEKTSV4RRFFQ69G5FAW".into()];
    record.events = vec![first.clone(), second];
    let projected = project(record, true);
    let lane = &projected.summary[0];
    assert_eq!(lane.latest_status, LatestObservationStatus::Observed);
    assert_eq!(lane.last_outcome.as_deref(), Some(first.outcome.as_str()));
    assert_eq!(lane.last_reason, first.reason);
    assert_eq!(lane.last_count, first.count);
    assert_eq!(
        lane.last_owner.as_ref().unwrap().id,
        first.owner.as_ref().unwrap().id
    );
    assert_eq!(lane.outcomes.get("prepared"), Some(&2));
    let details = projected.details.unwrap();
    assert_ne!(details[0].origin.attempt, details[1].origin.attempt);
    assert_ne!(details[0].nodes, details[1].nodes);
}

#[test]
fn strictly_later_observation_resolves_ambiguity_and_earlier_receipts_cannot_restore_it() {
    let fixture = decode_record(FIXTURE).unwrap();
    let startup = fixture.events[0].clone();
    let mut end = startup.clone();
    end.key = "same-second-end".into();
    end.outcome = "end".into();
    let mut later = startup.clone();
    later.key = "later-resume".into();
    later.at += 1;
    later.outcome = "resume".into();
    let mut delayed = startup.clone();
    delayed.key = "delayed-startup".into();
    for events in [
        vec![startup.clone(), end.clone(), later.clone(), delayed.clone()],
        vec![later.clone(), startup.clone(), end.clone(), delayed.clone()],
    ] {
        let mut record = fixture.clone();
        record.events = events;
        let projected = project(record, false);
        let lane = &projected.summary[0];
        assert_eq!(lane.latest_status, LatestObservationStatus::Observed);
        assert_eq!(lane.last_outcome.as_deref(), Some("resume"));
        assert_eq!(lane.last_at, later.at);
        assert_eq!(lane.outcomes.get("startup"), Some(&2));
        assert_eq!(lane.outcomes.get("end"), Some(&1));
        assert_eq!(lane.outcomes.get("resume"), Some(&1));
    }
}

#[test]
fn selector_validation_is_bounded_and_non_sensitive() {
    let sandbox = Sandbox::new();
    for selector in [
        String::new(),
        "bad\nselector".to_owned(),
        "x".repeat(MAX_SESSION_SELECTOR_BYTES + 1),
    ] {
        let err = report(
            &sandbox.manifest,
            &sandbox.workspace,
            ActivityRequest {
                session: Some(selector.clone()),
                details: false,
            },
        )
        .unwrap_err();
        assert_eq!(err.code(), "invalid_session_selector");
        if !selector.is_empty() {
            assert!(!err.to_string().contains(&selector));
        }
    }
}

#[cfg(unix)]
mod local {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn report_and_refusals_do_not_write_sidecar_or_control_files() {
        let sandbox = Sandbox::new();
        sandbox.add("observed-session");
        fs::write(
            sandbox.base.join("control-ledger.json"),
            b"unrelated sentinel",
        )
        .unwrap();
        let before = snapshot(&sandbox.base);
        assert_eq!(sandbox.report(None, false).status, ActivityStatus::Ready);
        assert_eq!(
            sandbox.report(Some("observed-session"), true).status,
            ActivityStatus::Ready
        );
        assert_eq!(
            sandbox.report(Some("absent-session"), true).status,
            ActivityStatus::SessionNotFound
        );
        assert_eq!(snapshot(&sandbox.base), before);
    }

    #[test]
    fn human_report_is_readable_without_hiding_provenance_or_unusable_selectors() {
        let sandbox = Sandbox::new();
        let id = "fixture-session-a17c9e-full";
        sandbox.add(id);
        let summary = sandbox.report(None, false);
        assert!(summary.coverage.complete_catalog);
        let session = &summary.sessions[0];
        let short_selector = format!("codex:{}", session.display_id);
        let full_selector = format!("codex:{id}");
        assert_ne!(short_selector, full_selector);
        let calendar =
            mneme_present::format_timestamp_millis(u128::from(session.first_observed) * 1000);
        assert!(calendar.starts_with("2026-10-23T"));
        let human = render_human(&summary);
        assert!(human.contains(&calendar));
        assert!(human.contains("(UTC)"));
        assert!(!human.contains(&session.first_observed.to_string()));
        assert!(human.contains(&short_selector));
        assert!(!human.contains(&full_selector));
        assert!(human.contains("Startup core"));
        assert!(human.contains("maintenance verified"));
        assert!(human.contains("packing limit"));
        assert!(human.contains("No retained observation: host admission"));
        assert!(!human.contains("owner-example"));
        assert!(!human.contains("01ARZ3NDEKTSV4RRFFQ69G5FAV"));
        assert!(!human.contains("origin="));

        let detailed = render_human(&sandbox.report(None, true));
        assert!(detailed.contains(&full_selector));
        assert!(detailed.contains("owner=project/owner-example db=project"));
        assert!(detailed.contains("01ARZ3NDEKTSV4RRFFQ69G5FAV"));
        assert!(detailed.contains("recall_selection selected"));
        assert!(detailed.contains("reason=packing_limit"));
        assert!(detailed.contains("origin="));
        assert!(detailed.contains("0123456789abcdef0123456789abcdef"));
        assert!(detailed.contains("attempt-1"));

        let direct = sandbox.report(Some(id), false);
        assert_eq!(direct.coverage.scan, ScanKind::Direct);
        assert!(render_human(&direct).contains(&full_selector));
        fs::write(
            sandbox
                .root
                .join(record_filename("codex", "unreadable-session")),
            b"{",
        )
        .unwrap();
        let partial = sandbox.report(None, false);
        assert!(!partial.coverage.complete_catalog);
        let partial_human = render_human(&partial);
        assert!(partial_human.contains(&full_selector));
        assert!(partial_human.contains("catalog coverage is incomplete"));
    }

    #[test]
    fn chooser_never_substitutes_latest_and_full_id_wins_prefix_collision() {
        let sandbox = Sandbox::new();
        sandbox.add("abcdef");
        sandbox.add("abcdef-longer");
        let chooser = sandbox.report(None, false);
        assert_eq!(chooser.sessions.len(), 2);
        assert_eq!(chooser.status, ActivityStatus::Ready);
        assert_eq!(
            sandbox.report(Some("abc"), false).status,
            ActivityStatus::AmbiguousSession
        );
        for selector in ["abcdef", "codex:abcdef"] {
            let exact = sandbox.report(Some(selector), false);
            assert_eq!(exact.status, ActivityStatus::Ready);
            assert_eq!(exact.coverage.scan, ScanKind::Direct);
            assert_eq!(exact.sessions.len(), 1);
            assert_eq!(exact.sessions[0].session_id, "abcdef");
        }
        assert_ne!(
            chooser.sessions[0].display_id,
            chooser.sessions[1].display_id
        );
        assert!(
            chooser
                .sessions
                .iter()
                .all(|s| s.selector == format!("codex:{}", s.session_id))
        );
    }

    #[test]
    fn prefixes_match_stored_ids_not_digest_filenames() {
        let sandbox = Sandbox::new();
        sandbox.add("host-native-id");
        let filename = record_filename("codex", "host-native-id");
        let hash_prefix = &filename[6..14];
        assert_eq!(
            sandbox.report(Some(hash_prefix), false).status,
            ActivityStatus::SessionNotFound
        );
        let prefix = sandbox.report(Some("host-native"), false);
        assert_eq!(prefix.status, ActivityStatus::Ready);
        assert_eq!(prefix.sessions[0].session_id, "host-native-id");
    }

    #[test]
    fn prefix_display_stays_unique_against_the_whole_covered_scope() {
        let sandbox = Sandbox::new();
        sandbox.add("abcdef-selected");
        sandbox.add("abcdef-other");
        let selected = sandbox.report(Some("abcdef-s"), false);
        assert_eq!(selected.status, ActivityStatus::Ready);
        assert!(selected.coverage.complete_catalog);
        assert_eq!(selected.sessions.len(), 1);
        assert!(!"abcdef-other".starts_with(&selected.sessions[0].display_id));
    }

    #[test]
    fn source_namespaces_never_satisfy_each_others_selector() {
        let sandbox = Sandbox::new();
        sandbox.add("shared-host-id");
        for id in ["shared-host-id", "foreign-only-host-id"] {
            let mut foreign = sandbox.record(id);
            foreign["source"] = json!("other");
            fs::write(
                sandbox.root.join(record_filename("other", id)),
                serde_json::to_vec(&foreign).unwrap(),
            )
            .unwrap();
        }
        let shared = sandbox.report(Some("codex:shared-host-id"), false);
        assert_eq!(shared.status, ActivityStatus::Ready);
        assert_eq!(shared.sessions.len(), 1);
        assert_eq!(shared.sessions[0].source, "codex");
        let foreign = sandbox.report(Some("foreign-only"), false);
        assert_eq!(foreign.status, ActivityStatus::SessionNotFound);
        assert!(foreign.sessions.is_empty());
        let wrong_qualification = sandbox.report(Some("other:shared-host-id"), false);
        assert_eq!(wrong_qualification.status, ActivityStatus::SessionNotFound);
        assert!(wrong_qualification.sessions.is_empty());
    }

    #[test]
    fn historical_scope_and_filename_identity_are_independently_checked() {
        let sandbox = Sandbox::new();
        let mut wrong_scope = sandbox.record("other-workspace");
        wrong_scope["workspace"] = json!(sandbox.base.join("not-this-workspace"));
        sandbox.write_value("other-workspace", &wrong_scope);
        let scoped = sandbox.report(Some("other-workspace"), false);
        assert!(scoped.sessions.is_empty());
        assert!(issue(&scoped, CoverageIssue::ScopeMismatch));
        sandbox.write_value("filename-id", &sandbox.record("different-stored-id"));
        let wrong_id = sandbox.report(Some("filename-id"), false);
        assert!(wrong_id.sessions.is_empty());
        assert!(issue(&wrong_id, CoverageIssue::IdentityMismatch));
    }

    #[test]
    fn normal_subdirectory_and_nearest_binding_work_but_nested_markers_refuse() {
        let sandbox = Sandbox::new();
        let sub = sandbox.workspace.join("src");
        fs::create_dir(&sub).unwrap();
        sandbox.add("parent-session");
        let normal = report(&sandbox.manifest, &sub, ActivityRequest::default()).unwrap();
        assert_eq!(normal.status, ActivityStatus::Ready);
        for marker in [".git", ".mneme"] {
            fs::write(
                sub.join(marker),
                b"boundary marker; no content should be read",
            )
            .unwrap();
            let refused = report(&sandbox.manifest, &sub, ActivityRequest::default()).unwrap();
            assert_eq!(refused.status, ActivityStatus::WorkspaceBoundary);
            fs::remove_file(sub.join(marker)).unwrap();
        }
        fs::write(sub.join(".git"), b"gitdir: /not/read").unwrap();
        sandbox.configure(
            &sandbox.root,
            &[(&sandbox.workspace, "codex"), (&sub, "codex")],
        );
        let nested = report(&sandbox.manifest, &sub, ActivityRequest::default()).unwrap();
        assert_eq!(nested.scope.unwrap().workspace, sub.to_str().unwrap());
        assert!(nested.sessions.is_empty());
    }

    #[test]
    fn unrelated_missing_workspace_does_not_disable_the_current_binding() {
        let sandbox = Sandbox::new();
        sandbox.add("current-session");
        let deleted = sandbox.base.join("deleted-worktree");
        sandbox.configure(
            &sandbox.root,
            &[(&sandbox.workspace, "codex"), (&deleted, "codex")],
        );
        let result = sandbox.report(None, false);
        assert_eq!(result.status, ActivityStatus::Ready);
        assert_eq!(
            result.scope.unwrap().workspace,
            sandbox.workspace.to_str().unwrap()
        );
        assert_eq!(result.sessions[0].session_id, "current-session");
        assert!(!deleted.exists());
    }

    #[test]
    fn symlink_binding_grants_no_scope_and_nested_worktree_cannot_fall_back() {
        let sandbox = Sandbox::new();
        let nested = sandbox.workspace.join("worktree");
        fs::create_dir(&nested).unwrap();
        let alias = sandbox.base.join("worktree-alias");
        symlink(&nested, &alias).unwrap();
        sandbox.configure(&sandbox.root, &[(&alias, "codex")]);
        let unbound = report(&sandbox.manifest, &alias, ActivityRequest::default()).unwrap();
        assert_eq!(unbound.status, ActivityStatus::WorkspaceUnbound);
        assert!(unbound.sessions.is_empty());

        // Canonical cwd points to nested, not the alias. The independently
        // admitted parent is nevertheless blocked by this worktree marker.
        fs::write(nested.join(".git"), b"gitdir: /not/read").unwrap();
        sandbox.configure(
            &sandbox.root,
            &[(&sandbox.workspace, "codex"), (&alias, "codex")],
        );
        let refused = report(&sandbox.manifest, &alias, ActivityRequest::default()).unwrap();
        assert_eq!(refused.status, ActivityStatus::WorkspaceBoundary);
        assert!(refused.sessions.is_empty());
    }

    #[test]
    fn duplicate_bindings_and_unknown_manifest_fields_fail_closed() {
        let sandbox = Sandbox::new();
        sandbox.configure(
            &sandbox.root,
            &[(&sandbox.workspace, "codex"), (&sandbox.workspace, "codex")],
        );
        assert_eq!(
            sandbox.report(None, false).status,
            ActivityStatus::InvalidManifest
        );
        sandbox.configure(&sandbox.root, &[(&sandbox.workspace, "codex")]);
        let mut manifest: Value =
            serde_json::from_slice(&fs::read(&sandbox.manifest).unwrap()).unwrap();
        manifest["owner_endpoint"] = json!("secret endpoint must never be used");
        fs::write(&sandbox.manifest, serde_json::to_vec(&manifest).unwrap()).unwrap();
        let refused = sandbox.report(None, true);
        assert_eq!(refused.status, ActivityStatus::InvalidManifest);
        assert!(
            !serde_json::to_string(&refused)
                .unwrap()
                .contains("secret endpoint")
        );
    }

    #[test]
    fn duplicate_manifest_fields_are_not_last_value_wins() {
        let sandbox = Sandbox::new();
        let manifest = fs::read_to_string(&sandbox.manifest).unwrap();
        let duplicated = manifest.replacen("\"root\":", "\"root\":\"/unadmitted\",\"root\":", 1);
        assert_ne!(duplicated, manifest);
        fs::write(&sandbox.manifest, duplicated).unwrap();
        assert_eq!(
            sandbox.report(None, false).status,
            ActivityStatus::InvalidManifest
        );
    }

    #[test]
    fn root_manifest_record_and_ancestor_symlinks_are_not_followed() {
        let sandbox = Sandbox::new();
        let good = sandbox.add("safe-session");
        let bad = sandbox
            .root
            .join(record_filename("codex", "linked-session"));
        symlink(&good, &bad).unwrap();
        let linked = sandbox.report(Some("linked-session"), true);
        assert!(linked.sessions.is_empty());
        assert!(issue(&linked, CoverageIssue::UnsafePath));

        let link_manifest = sandbox.base.join("linked-manifest.json");
        symlink(&sandbox.manifest, &link_manifest).unwrap();
        let refused = report(
            &link_manifest,
            &sandbox.workspace,
            ActivityRequest::default(),
        )
        .unwrap();
        assert!(refused.sessions.is_empty());
        assert!(issue(&refused, CoverageIssue::UnsafePath));

        let link_root = sandbox.base.join("linked-root");
        symlink(&sandbox.root, &link_root).unwrap();
        sandbox.configure(&link_root, &[(&sandbox.workspace, "codex")]);
        let refused = sandbox.report(None, false);
        assert!(refused.sessions.is_empty());
        assert!(issue(&refused, CoverageIssue::UnsafePath));

        sandbox.configure(&sandbox.root, &[(&sandbox.workspace, "codex")]);
        let link_parent = sandbox.base.join("linked-parent");
        symlink(&sandbox.base, &link_parent).unwrap();
        let refused = report(
            &link_parent.join("activity.json"),
            &sandbox.workspace,
            ActivityRequest::default(),
        )
        .unwrap();
        assert!(refused.sessions.is_empty());
        assert!(issue(&refused, CoverageIssue::UnsafePath));
    }

    #[test]
    fn fifo_record_is_refused_without_waiting_for_a_writer() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let sandbox = Sandbox::new();
        let path = sandbox.root.join(record_filename("codex", "fifo-session"));
        let name = CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: name is a valid NUL-terminated path within this owned disposable
        // directory. mkfifo creates no descriptor or borrowed state to retain.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let result = sandbox.report(Some("fifo-session"), false);
        assert_eq!(result.status, ActivityStatus::SessionUnavailable);
        assert!(result.sessions.is_empty());
        assert!(issue(&result, CoverageIssue::UnsafePath));
    }

    #[test]
    fn prepared_service_pins_scope_and_open_root_not_mutable_manifest_or_path() {
        let sandbox = Sandbox::new();
        sandbox.add("original-session");
        let prepared =
            super::super::local::PreparedActivity::new(&sandbox.manifest, &sandbox.workspace)
                .unwrap();
        let retained = sandbox.base.join("retained-original-root");
        fs::rename(&sandbox.root, &retained).unwrap();
        fs::create_dir(&sandbox.root).unwrap();
        sandbox.add("replacement-session");
        let other_workspace = sandbox.base.join("other-workspace");
        fs::create_dir(&other_workspace).unwrap();
        sandbox.configure(&sandbox.root, &[(&other_workspace, "codex")]);
        let pinned = prepared.report(ActivityRequest::default()).unwrap();
        assert_eq!(pinned.status, ActivityStatus::Ready);
        assert_eq!(
            pinned.scope.unwrap().workspace,
            sandbox.workspace.to_str().unwrap()
        );
        assert_eq!(pinned.sessions.len(), 1);
        assert_eq!(pinned.sessions[0].session_id, "original-session");
        let repeat = prepared.report(ActivityRequest::default()).unwrap();
        assert_eq!(
            repeat.sessions.len(),
            1,
            "enumeration must have independent cursor per report"
        );
        assert_eq!(repeat.sessions[0].session_id, "original-session");
    }

    #[test]
    fn prepared_missing_configuration_stays_missing_until_restart() {
        let sandbox = Sandbox::new();
        fs::remove_file(&sandbox.manifest).unwrap();
        let prepared =
            super::super::local::PreparedActivity::new(&sandbox.manifest, &sandbox.workspace)
                .unwrap();
        sandbox.configure(&sandbox.root, &[(&sandbox.workspace, "codex")]);
        sandbox.add("newly-configured");
        assert_eq!(
            prepared.report(ActivityRequest::default()).unwrap().status,
            ActivityStatus::NotConfigured
        );
        assert_eq!(sandbox.report(None, false).status, ActivityStatus::Ready);
    }

    #[test]
    fn malformed_unsupported_oversize_and_unknown_entries_are_coverage_not_empty_history() {
        let sandbox = Sandbox::new();
        fs::write(
            sandbox.root.join(record_filename("codex", "broken")),
            b"{ secret corrupt payload",
        )
        .unwrap();
        let mut future = sandbox.record("future");
        future["schema"] = json!("mneme.session-activity.v99");
        sandbox.write_value("future", &future);
        fs::write(
            sandbox.root.join(record_filename("codex", "huge")),
            vec![b' '; MAX_RECORD_BYTES + 1],
        )
        .unwrap();
        fs::write(sandbox.root.join("unexpected-file"), b"not a session").unwrap();
        for (name, bytes) in [
            (".activity.lock", b"diagnostic lock".as_slice()),
            (".activity-quota.json", b"diagnostic quota".as_slice()),
            (".activity-stage", b"diagnostic scratch".as_slice()),
        ] {
            fs::write(sandbox.root.join(name), bytes).unwrap();
        }
        let before = snapshot(&sandbox.base);
        let result = sandbox.report(None, true);
        for expected in [
            CoverageIssue::Malformed,
            CoverageIssue::Unsupported,
            CoverageIssue::Oversize,
            CoverageIssue::UnknownEntry,
        ] {
            assert!(issue(&result, expected), "{expected:?}");
        }
        assert!(result.coverage.partial);
        assert!(result.sessions.is_empty());
        assert!(
            !serde_json::to_string(&result)
                .unwrap()
                .contains("secret corrupt")
        );
        assert_eq!(snapshot(&sandbox.base), before);
    }

    #[test]
    fn complete_catalog_orders_by_observed_time_not_filesystem_mtime() {
        let sandbox = Sandbox::new();
        let mut newer = sandbox.record("observed-newer");
        newer["last_observed"] = json!(1792779999u64);
        sandbox.write_value("observed-newer", &newer);
        // The older observation is written after the newer one. The reader may
        // use mtime for candidate priority, but not for final historical order.
        std::thread::sleep(std::time::Duration::from_millis(5));
        sandbox.add("observed-older");
        let result = sandbox.report(None, false);
        assert!(result.coverage.complete_catalog);
        assert_eq!(result.sessions[0].session_id, "observed-newer");
    }

    #[test]
    fn incomplete_catalog_requires_full_id_but_direct_lookup_still_works() {
        let sandbox = Sandbox::new();
        sandbox.add("known-full-host-session");
        for index in 0..=MAX_DIRECTORY_ENTRIES {
            fs::write(sandbox.root.join(format!("unknown-{index:05}")), b"").unwrap();
        }
        let partial = sandbox.report(Some("known-full"), false);
        assert_eq!(partial.status, ActivityStatus::FullIdRequired);
        assert!(!partial.coverage.complete_catalog);
        assert!(issue(&partial, CoverageIssue::DirectoryLimit));
        assert!(partial.coverage.inspected_entries <= MAX_DIRECTORY_ENTRIES);
        assert!(
            partial
                .sessions
                .iter()
                .all(|s| s.display_id == s.session_id)
        );
        let direct = sandbox.report(Some("codex:known-full-host-session"), false);
        assert_eq!(direct.status, ActivityStatus::Ready);
        assert_eq!(direct.coverage.scan, ScanKind::Direct);
        assert_eq!(direct.coverage.inspected_entries, 0);
        assert_eq!(direct.sessions[0].session_id, "known-full-host-session");
    }

    #[test]
    fn small_record_catalog_over_128_entries_is_complete_under_byte_budget() {
        let sandbox = Sandbox::new();
        for index in 0..160 {
            let id = format!("small-session-{index:04}-host");
            let mut record = sandbox.record(&id);
            record["events"] = json!([]);
            record["evicted_events"] = json!(0);
            sandbox.write_value(&id, &record);
        }
        let result = sandbox.report(Some("small-session-0159-"), false);
        assert_eq!(result.status, ActivityStatus::Ready);
        assert!(result.coverage.complete_catalog);
        assert_eq!(result.coverage.verified_records, 160);
        assert!(!issue(&result, CoverageIssue::ReadLimit));
        assert_eq!(result.sessions.len(), 1);
        assert_eq!(result.sessions[0].session_id, "small-session-0159-host");
        assert!(result.coverage.read_budget_used <= MAX_READ_BYTES);
    }

    #[test]
    fn aggregate_read_budget_refuses_prefix_resolution_but_not_direct_identity() {
        let sandbox = Sandbox::new();
        let mut large = sandbox.record("aggregate-session-0000");
        let prototype = large["events"][0].clone();
        large["events"] = json!([]);
        let mut event_count = 0;
        while serde_json::to_vec(&large).unwrap().len() < 60 * 1024 {
            let mut event = prototype.clone();
            event["key"] = json!(format!("event-{event_count:04}-{}", "x".repeat(200)));
            large["events"].as_array_mut().unwrap().push(event);
            event_count += 1;
        }
        assert!(serde_json::to_vec(&large).unwrap().len() <= MAX_RECORD_BYTES);
        assert!(decode_record(&serde_json::to_vec(&large).unwrap()).is_ok());
        for index in 0..140 {
            let id = format!("aggregate-session-{index:04}");
            large["session_id"] = json!(id);
            sandbox.write_value(&id, &large);
        }
        let partial = sandbox.report(Some("aggregate-session-"), false);
        assert_eq!(partial.status, ActivityStatus::FullIdRequired);
        assert!(!partial.coverage.complete_catalog);
        assert!(issue(&partial, CoverageIssue::ReadLimit));
        assert!(partial.coverage.read_budget_used <= MAX_READ_BYTES);
        assert!(partial.coverage.read_bytes <= partial.coverage.read_budget_used);
        assert!(partial.coverage.verified_records < 140);
        let direct = sandbox.report(Some("codex:aggregate-session-0000"), false);
        assert_eq!(direct.status, ActivityStatus::Ready);
        assert_eq!(direct.coverage.scan, ScanKind::Direct);
        assert_eq!(direct.sessions[0].session_id, "aggregate-session-0000");
    }

    #[test]
    fn concurrent_atomic_replacement_yields_one_complete_record_edition() {
        let sandbox = Sandbox::new();
        let id = "atomically-replaced-session";
        let original = sandbox.record(id);
        let mut replacement = original.clone();
        let mut end = original["events"][0].clone();
        end["key"] = json!("later-lifecycle-end");
        end["outcome"] = json!("end");
        end["at"] = json!(original["last_observed"].as_u64().unwrap() + 1);
        replacement["last_observed"] = end["at"].clone();
        replacement["events"].as_array_mut().unwrap().push(end);
        let original_bytes = serde_json::to_vec(&original).unwrap();
        let replacement_bytes = serde_json::to_vec(&replacement).unwrap();
        let target = sandbox.write_value(id, &original);
        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            let writer = scope.spawn(|| {
                barrier.wait();
                for index in 0..300 {
                    let staging = sandbox.root.join(format!(".test-stage-{index}"));
                    fs::write(
                        &staging,
                        if index % 2 == 0 {
                            &original_bytes
                        } else {
                            &replacement_bytes
                        },
                    )
                    .unwrap();
                    fs::rename(staging, &target).unwrap();
                    std::thread::yield_now();
                }
            });
            barrier.wait();
            for _ in 0..80 {
                let result = sandbox.report(Some(id), true);
                assert_eq!(result.status, ActivityStatus::Ready);
                assert!(!result.coverage.partial);
                let session = &result.sessions[0];
                let details = session.details.as_ref().unwrap();
                let lifecycle = session
                    .summary
                    .iter()
                    .find(|lane| lane.kind == EventKind::Lifecycle)
                    .unwrap();
                match details.len() {
                    8 => {
                        assert_eq!(
                            session.last_observed,
                            original["last_observed"].as_u64().unwrap()
                        );
                        assert_eq!(lifecycle.last_outcome.as_deref(), Some("startup"));
                    }
                    9 => {
                        assert_eq!(
                            session.last_observed,
                            replacement["last_observed"].as_u64().unwrap()
                        );
                        assert_eq!(lifecycle.last_outcome.as_deref(), Some("end"));
                    }
                    count => panic!("mixed or partial record with {count} events"),
                }
            }
            writer.join().unwrap();
        });
    }

    #[test]
    fn cancellation_and_output_bounds_preserve_honest_omissions() {
        let sandbox = Sandbox::new();
        for index in 0..90 {
            sandbox.add(&format!("session-{index:04}-long-host-native-id"));
        }
        let cancelled = report_with_cancel(
            &sandbox.manifest,
            &sandbox.workspace,
            ActivityRequest::default(),
            &AtomicBool::new(true),
        )
        .unwrap();
        assert_eq!(cancelled.status, ActivityStatus::Cancelled);
        assert!(issue(&cancelled, CoverageIssue::Cancelled));
        let bounded = sandbox.report(None, true);
        assert!(serde_json::to_vec(&bounded).unwrap().len() < MAX_OUTPUT_BYTES);
        assert!(render_human(&bounded).len() < MAX_OUTPUT_BYTES);
        assert!(issue(&bounded, CoverageIssue::OutputLimit));
        assert!(
            bounded.coverage.omitted_sessions > 0
                || bounded.sessions.iter().any(|s| s.details_omitted > 0)
        );
        assert!(bounded.coverage.read_bytes <= MAX_READ_BYTES);
    }
}
