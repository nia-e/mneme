use super::*;
use crate::read_source::{ReadProvenance, example};
use ratatui::backend::TestBackend;

fn frame(app: &App, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|f| render::draw(f, app)).unwrap();
    (0..height)
        .map(|y| {
            (0..width)
                .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn snapshot_app(view: View) -> App {
    let mut app = App::new(vec![], String::new(), false);
    app.graph = demo_graph();
    app.graph.read_source = example();
    app.scenes = scenes::demo_scenes(SceneAxis::Recorded);
    app.scenes.read_source = example();
    app.scenes_loaded = true;
    app.cabinet.page = touchstones::demo_touchstones();
    app.cabinet.page.read_source = example();
    app.cabinet.loaded = true;
    app.view = view;
    app
}

#[test]
fn every_view_keeps_snapshot_warning_at_minimum_size_during_busy_error_and_poll() {
    for view in [View::Map, View::Scenes, View::Touchstones] {
        let mut app = snapshot_app(view);
        for busy in [false, true] {
            app.busy = busy;
            let rendered = frame(&app, 24, 9);
            assert!(
                rendered
                    .lines()
                    .next()
                    .unwrap()
                    .contains("SNAPSHOT · read-only"),
                "{view:?}: {rendered}"
            );
        }
        apply_response(
            &mut app,
            &mut None,
            Response::Error("owner unavailable".into()),
        );
        assert!(frame(&app, 24, 9).contains("SNAPSHOT · read-only"));
        for state in ["reading", "idle", "live-looking telemetry"] {
            apply_response(
                &mut app,
                &mut None,
                Response::Activity(Activity {
                    state: state.into(),
                    feed: true,
                    ..Default::default()
                }),
            );
            assert_eq!(app.displayed_read_source(), example());
            assert!(frame(&app, 24, 9).contains("SNAPSHOT · read-only"));
        }
    }
}

#[test]
fn wide_snapshot_header_names_age_device_generation_and_narrow_mixed_is_explicit() {
    let mut app = snapshot_app(View::Map);
    let rendered = frame(&app, 120, 24);
    let header = rendered.lines().next().unwrap();
    for expected in [
        "SNAPSHOT · read-only",
        "old",
        "device test-device",
        "gen generation-a",
    ] {
        assert!(header.contains(expected), "{header}");
    }
    app.graph.read_source.merge(&ReadProvenance::default());
    let rendered = frame(&app, 24, 9);
    assert!(rendered.contains("SNAPSHOT · read-only"));
    assert!(rendered.contains("mixed"));
}

#[test]
fn active_view_aggregation_ignores_unread_placeholders_but_keeps_loaded_unknowns() {
    let mut app = App::new(vec![], String::new(), false);
    // A default graph is not an unknown read competing with the lens.
    let mut graph = demo_neighborhood("demo-00");
    graph.read_source = example();
    app.open_edge_lens("demo-00".into(), graph, LensState::Ready);
    assert_eq!(app.displayed_read_source(), example());
    app.graph = demo_graph();
    assert!(app.displayed_read_source().mixed);

    app.view = View::Scenes;
    app.scene_detail = Some(scenes::Scene {
        read_source: example(),
        ..Default::default()
    });
    assert_eq!(app.displayed_read_source(), example());
    app.scenes_loaded = true; // A successfully read empty page still counts.
    assert!(app.displayed_read_source().mixed);

    app.view = View::Touchstones;
    app.cabinet.current = Some(touchstones::ExactTarget {
        read_source: example(),
        db_id: "owner".into(),
        node: None,
        body_partial: false,
        summary_partial: false,
    });
    assert_eq!(app.displayed_read_source(), example());
    app.cabinet.loaded = true;
    assert!(app.displayed_read_source().mixed);
}

#[test]
fn scene_and_touchstone_details_merge_with_page_and_exact_target_without_cross_view_leaks() {
    let mut app = snapshot_app(View::Scenes);
    app.scene_detail = Some(app.scenes.items[0].clone());
    assert!(app.displayed_read_source().mixed); // Detail was an independent unknown read.
    app.scene_detail.as_mut().unwrap().read_source = example();
    assert_eq!(app.displayed_read_source(), example());
    app.view = View::Touchstones;
    app.cabinet.detail = Some(touchstones::demo_detail(&app.cabinet.page.items[0]));
    assert!(app.displayed_read_source().mixed);
    app.cabinet.detail.as_mut().unwrap().read_source = example();
    assert_eq!(app.displayed_read_source(), example());
    app.cabinet.current = Some(touchstones::ExactTarget {
        read_source: ReadProvenance::default(),
        db_id: "other-owner".into(),
        node: None,
        body_partial: false,
        summary_partial: false,
    });
    assert!(app.displayed_read_source().mixed);
    app.view = View::Map;
    assert_eq!(app.displayed_read_source(), example());
}

#[test]
fn snapshot_history_restores_and_source_reset_clears_every_retained_component() {
    let mut app = snapshot_app(View::Map);
    let visit = app.visit();
    app.graph = demo_neighborhood("demo-01");
    app.remember(visit);
    assert!(app.displayed_read_source().snapshot.is_none());
    app.go_back();
    assert_eq!(app.displayed_read_source(), example());
    let graph = neighborhood(&app.graph, "demo-00");
    assert_eq!(graph.read_source, example());
    app.open_edge_lens("demo-00".into(), graph, LensState::Loading);
    assert_eq!(app.displayed_read_source(), example());
    app.scene_detail = Some(scenes::Scene {
        read_source: example(),
        ..Default::default()
    });
    app.cabinet.current = Some(touchstones::ExactTarget {
        read_source: example(),
        db_id: "owner".into(),
        node: None,
        body_partial: false,
        summary_partial: false,
    });
    let mut pending = Some(PendingRead::Refresh);
    reset_source(&mut app, &mut pending);
    assert!(pending.is_none());
    assert!(app.history.is_empty());
    assert!(app.edge_lens.is_none());
    for view in [View::Map, View::Scenes, View::Touchstones] {
        app.view = view;
        assert_eq!(app.displayed_read_source(), ReadProvenance::default());
        assert!(!frame(&app, 24, 9).contains("SNAPSHOT"));
    }
}

fn lens_graph(source: ReadProvenance, edges: &[(&str, &str)]) -> Graph {
    Graph {
        read_source: source,
        nodes: ["anchor", "old", "new"]
            .into_iter()
            .map(|id| Node {
                id: id.into(),
                status: "active".into(),
                ..Default::default()
            })
            .collect(),
        edges: edges
            .iter()
            .map(|(from, to)| Edge {
                from: (*from).into(),
                to: (*to).into(),
                kind: "associative".into(),
                weight: 0.5,
            })
            .collect(),
        ..Default::default()
    }
}

#[test]
fn lens_only_mixes_old_provenance_when_an_old_value_survives() {
    let mut app = App::new(vec![], String::new(), false);
    let old = lens_graph(ReadProvenance::default(), &[("old", "anchor")]);
    app.open_edge_lens("anchor".into(), old.clone(), LensState::Loading);
    app.edge_lens
        .as_mut()
        .unwrap()
        .merge_graph(lens_graph(example(), &[("old", "anchor")]));
    assert_eq!(app.edge_lens.as_ref().unwrap().graph.read_source, example());

    app.open_edge_lens("anchor".into(), old.clone(), LensState::Loading);
    app.edge_lens
        .as_mut()
        .unwrap()
        .merge_graph(lens_graph(example(), &[("anchor", "new")]));
    assert!(app.edge_lens.as_ref().unwrap().graph.read_source.mixed);

    app.open_edge_lens("anchor".into(), old, LensState::Loading);
    let mut fresh = lens_graph(example(), &[("old", "anchor")]);
    fresh.nodes.retain(|node| node.id != "old");
    app.edge_lens.as_mut().unwrap().merge_graph(fresh);
    assert!(app.edge_lens.as_ref().unwrap().graph.read_source.mixed);
}

#[test]
fn admitted_lens_status_update_retains_source_after_lens_closes_but_stale_reply_does_not() {
    let mut app = App::new(vec![], String::new(), false);
    app.graph = lens_graph(ReadProvenance::default(), &[("anchor", "new")]);
    app.open_edge_lens(
        "anchor".into(),
        neighborhood(&app.graph, "anchor"),
        LensState::Loading,
    );
    apply_response(
        &mut app,
        &mut Some(PendingRead::EdgeLens("stale".into())),
        Response::Loaded(lens_graph(example(), &[("anchor", "new")])),
    );
    assert!(app.graph.read_source.snapshot.is_none());
    apply_response(
        &mut app,
        &mut Some(PendingRead::EdgeLens("anchor".into())),
        Response::Loaded(lens_graph(example(), &[("anchor", "new")])),
    );
    app.close_edge_lens();
    assert_eq!(app.graph.read_source.snapshot, example().snapshot);
    assert!(app.graph.read_source.mixed);
}

fn inventory_page(id: &str, next: Option<&str>, source: ReadProvenance) -> InventoryPage {
    InventoryPage {
        bytes: 10,
        graph: Graph {
            read_source: source,
            nodes: vec![Node {
                id: id.into(),
                ..Default::default()
            }],
            inventory: Some(InventoryState {
                db_id: "owner".into(),
                next: next.map(str::to_owned),
                bytes: 10,
                ..Default::default()
            }),
            ..Default::default()
        },
    }
}

fn inventory_pending(after: Option<&str>) -> Option<PendingRead> {
    Some(PendingRead::Inventory {
        after: after.map(str::to_owned),
        pages: 0,
        bytes: 0,
        started: Instant::now(),
        waiting: true,
    })
}

#[test]
fn inventory_provenance_commits_only_with_accepted_page_and_restarts_replace_it() {
    let mut app = App::new(vec![], String::new(), false);
    apply_inventory(
        &mut app,
        &mut inventory_pending(None),
        inventory_page("a", Some("cursor"), ReadProvenance::default()),
    );
    apply_inventory(
        &mut app,
        &mut inventory_pending(Some("wrong")),
        inventory_page("b", None, example()),
    );
    assert!(app.graph.read_source.snapshot.is_none());
    apply_inventory(
        &mut app,
        &mut inventory_pending(Some("cursor")),
        inventory_page("b", None, example()),
    );
    assert!(app.graph.read_source.mixed);
    assert_eq!(app.graph.read_source.snapshot, example().snapshot);
    apply_inventory(
        &mut app,
        &mut inventory_pending(None),
        inventory_page("a", None, example()),
    );
    assert_eq!(app.graph.read_source, example());
}

#[test]
fn hydration_only_adds_provenance_for_admitted_retained_cards() {
    let mut app = App::new(vec![], String::new(), false);
    apply_inventory(
        &mut app,
        &mut inventory_pending(None),
        inventory_page("a", None, ReadProvenance::default()),
    );
    let page = || InventorySummaries {
        read_source: example(),
        nodes: vec![Node {
            id: "a".into(),
            summary: "hydrated".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    apply_summaries(&mut app, &mut None, page());
    assert!(app.graph.read_source.snapshot.is_none());
    apply_summaries(
        &mut app,
        &mut Some(PendingRead::Summaries {
            ids: vec!["wrong".into()],
        }),
        page(),
    );
    assert!(app.graph.read_source.snapshot.is_none());
    apply_summaries(
        &mut app,
        &mut Some(PendingRead::Summaries {
            ids: vec!["a".into()],
        }),
        page(),
    );
    assert_eq!(app.graph.nodes[0].summary, "hydrated");
    assert_eq!(app.graph.read_source.snapshot, example().snapshot);
    assert!(app.graph.read_source.mixed);
}
