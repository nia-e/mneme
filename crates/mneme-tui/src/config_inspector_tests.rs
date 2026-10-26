use super::*;
use crate::config_inspector::Input;
use crossterm::event::{KeyEvent, MouseEvent};
use mneme_config::{DiscoveryContributor, DiscoveryReport, FieldDiscoveryReport};
use ratatui::{Terminal, backend::TestBackend};
use std::sync::Arc;

fn key(code: KeyCode) -> Event {
    Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn app() -> App {
    let mut app = App::new(vec![], "unchanged query".into(), false);
    app.graph = demo_graph();
    app.config_report = Some(Arc::new(demo_config_report()));
    app
}

fn draw(app: &App, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| render::draw(frame, app)).unwrap();
    terminal
        .backend()
        .buffer()
        .content
        .chunks(usize::from(width))
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

fn press(app: &mut App, code: KeyCode) {
    assert_eq!(config_inspector::input(app, &key(code), 1), Input::Consumed);
}

#[test]
fn inspector_consumes_underlying_reads_navigation_editing_and_mouse_input() {
    for view in [View::Map, View::Scenes, View::Touchstones] {
        for editing in [false, true] {
            let mut app = app();
            app.view = view;
            app.editing = editing;
            app.input = "unfinished search".into();
            app.selected = 2;
            app.scroll = 7;
            app.show_details = true;
            app.busy = true;
            app.deferred_view_read = true;
            config_inspector::open(&mut app);
            draw(&app, 100, 24);
            for code in [
                KeyCode::Enter,
                KeyCode::Tab,
                KeyCode::BackTab,
                KeyCode::Backspace,
                KeyCode::Down,
                KeyCode::Up,
                KeyCode::Left,
                KeyCode::Right,
                KeyCode::Char('/'),
                KeyCode::Char('e'),
                KeyCode::Char('r'),
                KeyCode::Char('s'),
                KeyCode::Char('t'),
                KeyCode::Char('n'),
                KeyCode::Char('m'),
                KeyCode::Char('b'),
                KeyCode::Char('i'),
                KeyCode::Char('o'),
                KeyCode::Char('j'),
                KeyCode::Char('k'),
                KeyCode::Char('h'),
                KeyCode::Char('?'),
                KeyCode::Char('+'),
                KeyCode::Char('-'),
                KeyCode::Char('0'),
                KeyCode::Char('f'),
            ] {
                press(&mut app, code);
                assert!(app.config_inspector.is_some());
                assert_eq!(app.view, view);
                assert_eq!(app.editing, editing);
                assert_eq!(app.input, "unfinished search");
                assert_eq!(app.selected, 2);
                assert_eq!(app.scroll, 7);
                assert_eq!(app.query, "unchanged query");
                assert!(app.show_details && app.busy && app.deferred_view_read);
                assert!(app.source_menu.is_none());
                assert!(app.events.is_empty());
            }
            for event in [
                Event::Mouse(MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: 99,
                    row: 23,
                    modifiers: KeyModifiers::NONE,
                }),
                Event::Mouse(MouseEvent {
                    kind: MouseEventKind::Drag(MouseButton::Left),
                    column: 2,
                    row: 2,
                    modifiers: KeyModifiers::NONE,
                }),
                Event::Resize(24, 9),
                Event::Paste("not a query".into()),
            ] {
                assert_eq!(
                    config_inspector::input(&mut app, &event, 1),
                    Input::Consumed
                );
                assert_eq!(app.selected, 2);
                assert_eq!(app.scroll, 7);
                assert_eq!(app.input, "unfinished search");
            }
        }
    }
}

#[test]
fn inspector_dismissal_quit_and_key_release_stay_modal() {
    let mut app = app();
    assert_eq!(
        config_inspector::input(&mut app, &key(KeyCode::Enter), 1),
        Input::Pass
    );
    for close in [KeyCode::Esc, KeyCode::Char('c'), KeyCode::Char('C')] {
        config_inspector::open(&mut app);
        let mut release = KeyEvent::new(close, KeyModifiers::NONE);
        release.kind = KeyEventKind::Release;
        assert_eq!(
            config_inspector::input(&mut app, &Event::Key(release), 1),
            Input::Consumed
        );
        assert!(app.config_inspector.is_some());
        press(&mut app, close);
        assert!(app.config_inspector.is_none());
    }
    config_inspector::open(&mut app);
    for event in [
        key(KeyCode::Char('q')),
        Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
    ] {
        assert_eq!(config_inspector::input(&mut app, &event, 1), Input::Quit);
    }
}

#[test]
fn inspector_open_discards_old_queue_and_modal_gate_does_not_restore_selection() {
    let mut app = app();
    let selected = app.graph.nodes[3].id.clone();
    let mut queued = Some((
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        Some(selected.clone()),
    ));
    open_config_inspector(&mut app, &mut queued);
    assert!(queued.is_none());
    for code in [KeyCode::Enter, KeyCode::Char('e')] {
        queued = Some((
            KeyEvent::new(code, KeyModifiers::NONE),
            Some(selected.clone()),
        ));
        for barrier in [true, false] {
            assert!(queued_input(&mut app, barrier, &mut queued).is_none());
            assert!(queued.is_some());
            assert_eq!(app.selected, 0);
        }
    }
    press(&mut app, KeyCode::Esc);
    assert_eq!(
        queued_input(&mut app, false, &mut queued),
        Some(key(KeyCode::Char('e')))
    );
    assert_eq!(app.selected, 3);
}

#[test]
fn every_overlay_blocks_deferred_and_background_read_admission() {
    let mut app = app();
    assert!(memory_reads_visible(&app));
    app.show_help = true;
    assert!(!memory_reads_visible(&app));
    app.show_help = false;
    app.source_menu = Some(0);
    assert!(!memory_reads_visible(&app));
    app.source_menu = None;
    config_inspector::open(&mut app);
    assert!(!memory_reads_visible(&app));
    press(&mut app, KeyCode::Esc);
    assert!(memory_reads_visible(&app));
}

#[test]
fn central_dispatch_does_not_send_any_owner_read_while_inspecting() {
    let mut app = app();
    app.error = Some("retained source failure".into());
    config_inspector::open(&mut app);
    let (tx, mut rx) = mpsc::channel(2);
    for request_kind in [
        Request::Inventory { after: None },
        Request::Summaries {
            ids: vec!["id".into()],
        },
        Request::Query("query".into()),
        Request::Focus("id".into()),
        Request::Lens("id".into()),
        Request::Scenes {
            axis: SceneAxis::Recorded,
            cue: None,
        },
        Request::Scene {
            episode_id: "episode".into(),
            edition_id: "edition".into(),
        },
        Request::Touchstones { after: None },
        Request::Touchstone { id: "id".into() },
        Request::TouchstoneTarget {
            db_id: "db".into(),
            id: "id".into(),
        },
        Request::Poll,
    ] {
        assert!(!request(&mut app, &tx, request_kind));
        assert!(matches!(
            rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        assert!(!app.busy);
        assert_eq!(app.error.as_deref(), Some("retained source failure"));
    }
    press(&mut app, KeyCode::Esc);
    assert!(request(&mut app, &tx, Request::Poll));
    assert!(matches!(rx.try_recv(), Ok(Request::Poll)));
}

#[test]
fn active_search_and_detail_failure_causes_are_reachable_without_owner_reads() {
    for detail in [false, true] {
        for (width, height) in [(100, 30), (24, 9)] {
            let mut app = app();
            let map_ids: Vec<_> = app.graph.nodes.iter().map(|node| node.id.clone()).collect();
            let mut search = SearchResults::new("a search".into());
            search.saved_error = Some("HISTORICAL_ONLY".into());
            search.opening = detail.then(|| "selected".into());
            app.search = Some(search);
            let mut pending = Some(if detail {
                PendingRead::SearchDetail("selected".into())
            } else {
                PendingRead::Query("a search".into())
            });
            let cause = format!(
                "CAUSE_START {} CAUSE_END",
                (0..40)
                    .map(|i| format!("cause segment{i:02} "))
                    .collect::<String>()
            );
            apply_search_error(&mut app, &mut pending, &cause);
            config_inspector::open(&mut app);
            let (tx, mut rx) = mpsc::channel(2);
            assert!(!request(&mut app, &tx, Request::Poll));
            assert!(matches!(
                rx.try_recv(),
                Err(mpsc::error::TryRecvError::Empty)
            ));

            let mut seen = draw(&app, width, height);
            let mut previous = seen.clone();
            for _ in 0..300 {
                press(&mut app, KeyCode::PageDown);
                let current = draw(&app, width, height);
                seen.push_str(&current);
                if current == previous {
                    break;
                }
                previous = current;
            }
            assert!(seen.contains("CAUSE_START"), "{width}x{height}: {seen}");
            assert!(seen.contains("CAUSE_END"), "{width}x{height}: {seen}");
            assert!(!seen.contains("HISTORICAL_ONLY"));
            assert_eq!(
                app.search.as_ref().unwrap().error.as_deref(),
                Some(cause.as_str())
            );
            assert_eq!(
                app.graph
                    .nodes
                    .iter()
                    .map(|node| node.id.clone())
                    .collect::<Vec<_>>(),
                map_ids
            );
        }
    }
}

#[test]
fn inspector_hides_inactive_search_errors_and_deduplicates_active_causes() {
    let mut app = app();
    let mut search = SearchResults::new("a search".into());
    search.error = Some("SHARED_CAUSE".into());
    app.search = Some(search);
    app.error = Some("SHARED_CAUSE".into());
    config_inspector::open(&mut app);
    assert_eq!(draw(&app, 100, 50).matches("SHARED_CAUSE").count(), 1);
    for view in [View::Scenes, View::Touchstones] {
        app.view = view;
        app.error = Some("CURRENT_CAUSE".into());
        let rendered = draw(&app, 100, 50);
        assert!(!rendered.contains("SHARED_CAUSE"));
        assert!(rendered.contains("CURRENT_CAUSE"));
    }
}

#[test]
fn keyboard_reaches_every_long_provenance_entry_at_normal_and_narrow_sizes() {
    let mut report = demo_config_report();
    let store = report.stores.values_mut().next().unwrap();
    store.fields.clear();
    store.fields.insert(
        "permissions.read".into(),
        FieldDiscoveryReport {
            effective: Some(false.into()),
            contributors: (0..64)
                .map(|index| DiscoveryContributor {
                    publisher_db_id: format!("P{index:03}"),
                    value: Some(format!("V{index:03} {}", "long evidence value ".repeat(5)).into()),
                    witness: vec![format!(
                        "{} W{index:03}",
                        "branch with a long witness ".repeat(5)
                    )],
                })
                .collect(),
            ..Default::default()
        },
    );
    for (width, height) in [(100, 30), (36, 16), (24, 9)] {
        let mut app = app();
        app.config_report = Some(Arc::new(report.clone()));
        config_inspector::open(&mut app);
        press(&mut app, KeyCode::Right);
        let first = draw(&app, width, height);
        press(&mut app, KeyCode::End);
        let last = draw(&app, width, height);
        assert!(
            last.contains("W063"),
            "last witness unreachable at {width}x{height}: {last}"
        );
        press(&mut app, KeyCode::PageUp);
        assert_ne!(draw(&app, width, height), last);
        press(&mut app, KeyCode::Home);
        assert_eq!(draw(&app, width, height), first);
        let mut observed = first.clone();
        let mut previous = first;
        let mut reached_end = false;
        for _ in 0..2000 {
            press(&mut app, KeyCode::PageDown);
            let current = draw(&app, width, height);
            observed.push_str(&current);
            if current == previous {
                reached_end = true;
                break;
            }
            previous = current;
        }
        assert!(
            reached_end,
            "scrolling did not terminate at {width}x{height}"
        );
        for index in 0..64 {
            for prefix in ["P", "V", "W"] {
                let marker = format!("{prefix}{index:03}");
                assert!(
                    observed.contains(&marker),
                    "{marker} unreachable at {width}x{height}"
                );
            }
        }
        press(&mut app, KeyCode::Left);
        assert!(draw(&app, width, height).contains("Configuration evidence"));
    }
}

#[test]
fn inspecting_picker_identity_does_not_switch_the_active_source() {
    let mut app = app();
    let id = app
        .config_report
        .as_ref()
        .unwrap()
        .stores
        .keys()
        .next()
        .unwrap()
        .clone();
    let target = |name: &str, id: &str| Target {
        name: name.into(),
        database: name.into(),
        url: "http://127.0.0.1:1".into(),
        ssh_mcp_port: 18766,
        token_env: None,
        expected_db_id: Some(id.into()),
        expected_path: None,
        configured: None,
    };
    app.targets = vec![
        target("active source", "other-id"),
        target("inspected source", &id),
    ];
    app.sources = app
        .targets
        .iter()
        .cloned()
        .map(SourceChoice::routed)
        .collect();
    app.target = 0;
    app.source_menu = Some(1);
    config_inspector::open(&mut app);
    assert!(draw(&app, 100, 30).contains("Database ID:"));
    for code in [
        KeyCode::Enter,
        KeyCode::Right,
        KeyCode::Tab,
        KeyCode::Left,
        KeyCode::BackTab,
    ] {
        press(&mut app, code);
        assert_eq!(app.target, 0);
        assert_eq!(app.source_menu, Some(1));
    }
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.target, 0);
    assert_eq!(app.source_menu, Some(1));
}

#[test]
fn report_only_zero_store_rows_keep_unavailable_evidence_and_navigation() {
    let mut app = app();
    app.config_report = Some(Arc::new(DiscoveryReport {
        unavailable: [("missing-owner".into(), "UNAVAILABLE_NOT_EMPTY".into())].into(),
        ..Default::default()
    }));
    config_inspector::open(&mut app);
    let first = draw(&app, 100, 40);
    assert!(first.contains("0 stores"));
    assert!(first.contains("UNAVAILABLE_NOT_EMPTY"));
    assert!(first.contains("no selection snapshot"));
    for code in [
        KeyCode::Right,
        KeyCode::Tab,
        KeyCode::Left,
        KeyCode::BackTab,
        KeyCode::Enter,
    ] {
        press(&mut app, code);
        assert_eq!(draw(&app, 100, 40), first);
    }
}

#[tokio::test]
async fn inspector_startup_never_creates_worker_and_first_explicit_route_can_open() {
    let mut app = app();
    app.targets.push(Target {
        name: "first".into(),
        database: "first".into(),
        url: "http://127.0.0.1:1".into(),
        ssh_mcp_port: 18766,
        token_env: None,
        expected_db_id: Some("identity".into()),
        expected_path: None,
        configured: None,
    });
    app.sources = app
        .targets
        .iter()
        .cloned()
        .map(SourceChoice::routed)
        .collect();
    source_picker::open(&mut app);
    config_inspector::open(&mut app);
    assert!(
        initial_worker(&app).is_none(),
        "even a routable default is metadata-only at startup"
    );
    press(&mut app, KeyCode::Esc);
    let chosen = source_picker::key(&mut app, KeyCode::Enter).unwrap();
    assert_eq!(chosen, 0);
    assert!(source_connection_required(&app, chosen, false));
    assert!(!source_connection_required(&app, chosen, true));
}

#[test]
fn admission_activation_and_resolution_are_displayed_as_distinct_facts() {
    let mut app = app();
    let config = mneme_config::Config::parse(
        r#"
schema = "mneme.config.v1"
[stores.example]
id = "01ARZ3NDEKTSV4RRFFQ69G5FAV"
owner = { url = "http://127.0.0.1:1", db = "example" }
active = { workspace = "any" }
permissions = { read = false }
"#,
    )
    .unwrap();
    app.config_selection = Some(Arc::new(
        mneme_config::select(
            &config,
            &mneme_config::SelectionContext::new(std::env::temp_dir().canonicalize().unwrap()),
        )
        .unwrap(),
    ));
    config_inspector::open(&mut app);
    press(&mut app, KeyCode::Right);
    let rendered = draw(&app, 100, 35);
    for text in [
        "Admission: admitted",
        "Activation: active",
        "read=false",
        "Resolution: resolved",
        "CONFLICT",
    ] {
        assert!(rendered.contains(text), "missing {text}: {rendered}");
    }
    app.config_selection = None;
    let rendered = draw(&app, 100, 35);
    assert!(rendered.contains("Admission: not resolved"));
    assert!(rendered.contains("Activation: not resolved"));
}

#[test]
fn browse_admission_is_not_presented_as_automatic_activation_or_connectivity() {
    let mut app = app();
    let config = mneme_config::Config::parse(
        r#"
schema = "mneme.config.v1"
[stores.example]
id = "01ARZ3NDEKTSV4RRFFQ69G5FAV"
owner = { url = "http://127.0.0.1:1", db = "example" }
active = { enabled = false, workspace = "project", roots = ["/another/device"] }
"#,
    )
    .unwrap();
    let mut context =
        mneme_config::SelectionContext::new(std::env::temp_dir().canonicalize().unwrap());
    context.purpose = mneme_config::SelectionPurpose::Browse;
    app.config_selection = Some(Arc::new(mneme_config::select(&config, &context).unwrap()));
    Arc::make_mut(app.config_report.as_mut().unwrap())
        .stores
        .values_mut()
        .next()
        .unwrap()
        .blocked = true;

    config_inspector::open(&mut app);
    let overview = draw(&app, 120, 40);
    assert!(overview.contains("explicit read-only browsing"));
    assert!(overview.contains("automatic memory unchanged"));
    press(&mut app, KeyCode::Right);
    let details = draw(&app, 120, 40);
    for text in [
        "Browse admission: admitted",
        "Automatic activation: not evaluated for browsing",
        "Browse operations: read=true (read-only)",
        "Automatic policy resolution: blocked",
        "Policy diagnostics are not an owner connectivity test.",
        "[auto-blocked]",
    ] {
        assert!(details.contains(text), "missing {text}: {details}");
    }
    assert!(!details.contains("Activation: inactive"));
    assert!(!details.contains("core=false"));
}

#[test]
fn untrusted_report_text_never_reaches_terminal_as_control_sequences() {
    let mut report = demo_config_report();
    let store = report.stores.values_mut().next().unwrap();
    store.label = "Visible\x1b[31m label\x07".into();
    let field = store.fields.get_mut("permissions.read").unwrap();
    field.contributors[0].publisher_db_id = "publisher\x1b[2J\x07".into();
    field.contributors[0].witness = vec!["path\x1b[1H\x00".into()];
    report
        .unavailable
        .insert("owner\x1b[2J".into(), "reason\x07\x00".into());
    let mut app = app();
    app.config_report = Some(Arc::new(report));
    app.error = Some("error\x1b[2J\x07".into());
    config_inspector::open(&mut app);
    for code in [KeyCode::Home, KeyCode::End, KeyCode::Right, KeyCode::End] {
        press(&mut app, code);
        let rendered = draw(&app, 100, 50);
        assert!(rendered.chars().all(|ch| !ch.is_control() || ch == '\n'));
    }
}

#[test]
fn bounded_out_values_do_not_claim_the_configuration_was_unresolved() {
    let mut report = demo_config_report();
    let field = report
        .stores
        .values_mut()
        .next()
        .unwrap()
        .fields
        .get_mut("permissions.read")
        .unwrap();
    field.effective = None;
    field.contributors[0].value = None;
    report.omitted_details = 2;
    let mut app = app();
    app.config_report = Some(Arc::new(report));
    config_inspector::open(&mut app);
    press(&mut app, KeyCode::Right);
    let rendered = draw(&app, 100, 50);
    assert!(rendered.contains("Effective: not present in report"));
    assert!(rendered.contains("Value: not present in report"));
    assert!(!rendered.contains("Effective: not resolved"));
    assert!(rendered.contains("Report omitted 2 details"));
}
