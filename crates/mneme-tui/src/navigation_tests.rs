use super::*;

#[test]
fn startup_owner_warning_survives_inventory_success_without_marking_primary_unavailable() {
    let mut app = App::new(vec![], String::new(), false);
    let warning = "Global Tab owner unavailable · project remains selected · repair cli.json";
    app.event("Load paused: call/time/byte budget · n continues");
    apply_startup_warnings(&mut app, vec![warning.into()]);
    let mut pending = Some(PendingRead::Inventory {
        after: None,
        pages: 0,
        bytes: 0,
        started: Instant::now(),
        waiting: true,
    });
    apply_response(
        &mut app,
        &mut pending,
        Response::Inventory(InventoryPage {
            graph: Graph {
                inventory: Some(InventoryState {
                    complete: true,
                    topology_edges_complete: true,
                    ..Default::default()
                }),
                ..Default::default()
            },
            bytes: 10,
        }),
    );
    assert_eq!(app.events.front().map(String::as_str), Some(warning));
    assert!(app.error.is_none());
    assert!(render::preview(120, 36, &app).unwrap().contains(warning));
}

fn demo() -> App {
    let mut app = App::new(vec![], "original search".into(), true);
    app.graph = demo_graph();
    app.selected = 1;
    app.scroll = 3;
    app.last_refresh = Some(Instant::now());
    app
}
#[test]
fn demo_focus_and_back_restore_the_exact_view() {
    let mut app = demo();
    let id = app.selected_node().unwrap().id.clone();
    let refreshed = app.last_refresh;
    let mut pending = None;
    assert!(focus_selected(&mut app, None, &mut pending));
    assert_eq!(app.graph.focus.as_deref(), Some(id.as_str()));
    assert_eq!(app.graph.nodes.len(), 2);
    assert_eq!(app.graph.edges.len(), 1);
    assert!(app.can_go_back());
    app.query = "changed".into();
    back_key(&mut app, KeyCode::Backspace);
    assert_eq!(app.graph.nodes.len(), 12);
    assert_eq!(app.selected, 1);
    assert_eq!(app.query, "original search");
    assert_eq!(app.scroll, 3);
    assert_eq!(app.last_refresh, refreshed);
    assert!(!app.can_go_back());
}
#[test]
fn escape_dismisses_panes_before_going_back() {
    let mut app = demo();
    focus_selected(&mut app, None, &mut None);
    app.show_help = true;
    toggle_edge_lens(&mut app, None, &mut None);
    back_key(&mut app, KeyCode::Esc);
    assert!(!app.show_help);
    assert!(app.edge_lens.is_some());
    assert!(app.can_go_back());
    back_key(&mut app, KeyCode::Esc);
    assert!(app.edge_lens.is_none());
    assert!(app.can_go_back());
    app.show_details = true;
    back_key(&mut app, KeyCode::Esc);
    assert!(!app.show_details);
    assert!(app.can_go_back());
    back_key(&mut app, KeyCode::Esc);
    assert!(!app.can_go_back());
    assert_eq!(app.selected, 1);
}
#[test]
fn pending_focus_does_not_commit_a_history_entry() {
    let mut app = demo();
    app.demo = false;
    let (tx, mut rx) = mpsc::channel(1);
    let mut pending = None;
    assert!(focus_selected(&mut app, Some(&tx), &mut pending));
    assert!(matches!(rx.try_recv().unwrap(), Request::Focus(_)));
    assert!(pending.is_some());
    assert!(!app.can_go_back());
    assert!(!focus_selected(&mut app, Some(&tx), &mut pending));
    apply_response(
        &mut app,
        &mut pending,
        Response::Loaded(demo_neighborhood("demo-01")),
    );
    back_key(&mut app, KeyCode::Char('b'));
    assert_eq!(app.selected, 1);
}
#[test]
fn history_is_bounded_and_back_at_root_is_a_noop() {
    let mut app = demo();
    for _ in 0..25 {
        app.remember(app.visit());
    }
    assert_eq!(app.history.len(), 16);
    for _ in 0..30 {
        app.go_back();
    }
    assert!(!app.can_go_back());
    assert_eq!(app.graph.nodes.len(), 12);
}
#[test]
fn edge_lens_keeps_selection_history_and_base_and_restores_panes() {
    let mut app = demo();
    app.show_details = true;
    let refresh = app.last_refresh;
    let ids: Vec<_> = app.graph.nodes.iter().map(|n| n.id.clone()).collect();
    toggle_edge_lens(&mut app, None, &mut None);
    assert_eq!(app.edge_lens.as_ref().unwrap().anchor, "demo-01");
    assert_eq!(app.selected, 1);
    assert!(!app.can_go_back());
    assert!(!app.show_details);
    assert_eq!(app.scroll, 0);
    app.scroll = 8;
    toggle_edge_lens(&mut app, None, &mut None);
    assert_eq!(app.scroll, 3);
    assert!(app.show_details);
    assert_eq!(app.last_refresh, refresh);
    assert_eq!(
        app.graph
            .nodes
            .iter()
            .map(|n| n.id.clone())
            .collect::<Vec<_>>(),
        ids
    );
}

fn live_search() -> App {
    let mut app = demo();
    app.demo = false;
    app.graph.focus = None;
    app.graph.edges.clear();
    app
}

#[test]
fn live_lens_loads_unknown_neighbors_without_replacing_the_search() {
    let mut app = live_search();
    app.graph.nodes.truncate(2);
    let (tx, mut rx) = mpsc::channel(1);
    let mut pending = None;
    toggle_edge_lens(&mut app, Some(&tx), &mut pending);
    assert!(matches!(rx.try_recv().unwrap(), Request::Lens(id) if id == "demo-01"));
    assert_eq!(app.edge_lens.as_ref().unwrap().state, LensState::Loading);
    assert_eq!(app.selected, 1);
    let mut graph = demo_neighborhood("demo-01");
    graph.nodes.push(Node {
        id: "new-node".into(),
        summary: "New neighbor".into(),
        ..Node::default()
    });
    graph.edges.push(Edge {
        from: "demo-01".into(),
        to: "new-node".into(),
        kind: "associative".into(),
        weight: 0.5,
    });
    apply_response(&mut app, &mut pending, Response::Loaded(graph));
    assert_eq!(app.edge_lens.as_ref().unwrap().state, LensState::Ready);
    assert_eq!(app.graph.nodes.len(), 2);
    assert!(app.graph.edges.is_empty());
    assert_eq!(app.scene_nodes().len(), 3);
    assert_eq!(app.selected, 1);
    assert!(!app.can_go_back());
    back_key(&mut app, KeyCode::Esc);
    assert_eq!(app.scene_nodes().len(), 2);
    assert_eq!(app.scroll, 3);
}

#[test]
fn empty_lens_is_ready_not_an_unfetched_search() {
    let mut app = live_search();
    let (tx, mut rx) = mpsc::channel(1);
    let mut pending = None;
    toggle_edge_lens(&mut app, Some(&tx), &mut pending);
    rx.try_recv().unwrap();
    let graph = Graph {
        nodes: vec![app.selected_node().unwrap().clone()],
        focus: Some("demo-01".into()),
        ..Graph::default()
    };
    apply_response(&mut app, &mut pending, Response::Loaded(graph));
    let lens = app.edge_lens.as_ref().unwrap();
    assert_eq!(lens.state, LensState::Ready);
    assert!(lens.graph.edges.is_empty());
    assert_eq!(
        app.scene_nodes()
            .iter()
            .filter(|node| app.node_visible(&node.id))
            .count(),
        1
    );
    assert_eq!(app.graph.nodes.len(), 12);
}

#[test]
fn closing_discards_pending_lens_but_selection_keeps_anchor_read() {
    for close_with_selection in [false, true] {
        let mut app = live_search();
        let (tx, mut rx) = mpsc::channel(1);
        let mut pending = None;
        toggle_edge_lens(&mut app, Some(&tx), &mut pending);
        rx.try_recv().unwrap();
        if close_with_selection {
            change_selection(&mut app, true);
        } else {
            back_key(&mut app, KeyCode::Esc);
        }
        assert_eq!(app.edge_lens.is_some(), close_with_selection);
        assert!(rx.try_recv().is_err()); // selection never silently fetches
        apply_response(
            &mut app,
            &mut pending,
            Response::Loaded(demo_neighborhood("demo-01")),
        );
        assert_eq!(app.selected, if close_with_selection { 2 } else { 1 });
        assert_eq!(app.graph.nodes.len(), 12);
        assert!(app.graph.focus.is_none());
        assert!(app.graph.edges.is_empty());
        assert!(!app.can_go_back());
        assert_eq!(app.edge_lens.is_some(), close_with_selection);
        if close_with_selection {
            assert_eq!(app.edge_lens.as_ref().unwrap().state, LensState::Ready);
        }
        assert!(!app.busy);
    }
}

#[test]
fn pending_lens_blocks_another_fetch_and_query_then_clears_overlay() {
    let mut app = live_search();
    let (tx, mut rx) = mpsc::channel(1);
    let mut pending = None;
    toggle_edge_lens(&mut app, Some(&tx), &mut pending);
    rx.try_recv().unwrap();
    assert!(!query_field(
        &mut app,
        &tx,
        &mut pending,
        "new search".into()
    ));
    assert!(!focus_selected(&mut app, Some(&tx), &mut pending));
    assert!(matches!(pending, Some(PendingRead::EdgeLens(_))));
    assert!(rx.try_recv().is_err());
    apply_response(
        &mut app,
        &mut pending,
        Response::Loaded(demo_neighborhood("demo-01")),
    );
    assert!(query_field(
        &mut app,
        &tx,
        &mut pending,
        "new search".into()
    ));
    assert!(matches!(rx.try_recv().unwrap(), Request::Query(_)));
    assert!(app.edge_lens.is_none());
    apply_response(&mut app, &mut pending, Response::Loaded(Graph::default()));
    assert_eq!(app.query, "new search");
    assert!(app.graph.nodes.is_empty());
    assert!(!app.can_go_back());
}

#[test]
fn source_switch_drops_pending_lens_and_old_channel() {
    let mut app = live_search();
    app.targets = (0..2)
        .map(|i| Target {
            name: format!("source-{i}"),
            database: "project".into(),
            url: String::new(),
            ssh_mcp_port: 0,
            token_env: None,
            expected_db_id: None,
            expected_path: None,
        })
        .collect();
    let (tx, mut rx) = mpsc::channel(1);
    let mut pending = None;
    toggle_edge_lens(&mut app, Some(&tx), &mut pending);
    rx.try_recv().unwrap();
    reset_source(&mut app, &mut pending);
    // The event loop also drops the old response receiver, so only the new
    // worker can answer a subsequently submitted query.
    assert!(pending.is_none());
    assert_eq!(app.target, 1);
    assert!(app.edge_lens.is_none());
    assert!(app.graph.nodes.is_empty());
    apply_response(&mut app, &mut pending, Response::Loaded(demo_graph()));
    assert!(app.graph.nodes.is_empty());
    let (new_tx, mut new_rx) = mpsc::channel(1);
    assert!(query_field(
        &mut app,
        &new_tx,
        &mut pending,
        "new source".into()
    ));
    assert!(matches!(new_rx.try_recv().unwrap(), Request::Query(_)));
    apply_response(&mut app, &mut pending, Response::Loaded(demo_graph()));
    assert_eq!(app.query, "new source");
    assert_eq!(app.graph.nodes.len(), 12);
}

#[test]
fn failed_lens_preserves_field_and_dismissed_lens_ignores_failure() {
    for dismissed in [false, true] {
        let mut app = live_search();
        let (tx, mut rx) = mpsc::channel(1);
        let mut pending = None;
        toggle_edge_lens(&mut app, Some(&tx), &mut pending);
        rx.try_recv().unwrap();
        if dismissed {
            back_key(&mut app, KeyCode::Esc);
        }
        apply_response(&mut app, &mut pending, Response::Error("offline".into()));
        assert_eq!(app.graph.nodes.len(), 12);
        assert_eq!(app.selected, 1);
        assert!(!app.can_go_back());
        assert!(!app.busy);
        if dismissed {
            assert!(app.error.is_none());
            assert!(app.edge_lens.is_none());
        } else {
            assert_eq!(
                app.edge_lens.as_ref().unwrap().state,
                LensState::Unavailable
            );
            assert_eq!(app.error.as_deref(), Some("offline"));
        }
    }
}

#[test]
fn enter_from_lens_navigates_and_back_restores_base_not_an_overlay_cache() {
    let mut app = live_search();
    let (tx, mut rx) = mpsc::channel(1);
    let mut pending = None;
    toggle_edge_lens(&mut app, Some(&tx), &mut pending);
    rx.try_recv().unwrap();
    apply_response(
        &mut app,
        &mut pending,
        Response::Loaded(demo_neighborhood("demo-01")),
    );
    assert!(focus_selected(&mut app, Some(&tx), &mut pending));
    assert!(matches!(rx.try_recv().unwrap(), Request::Focus(_)));
    assert!(app.edge_lens.is_none());
    apply_response(
        &mut app,
        &mut pending,
        Response::Loaded(demo_neighborhood("demo-01")),
    );
    assert_eq!(app.graph.nodes.len(), 2);
    assert!(app.can_go_back());
    change_selection(&mut app, true);
    assert!(focus_selected(&mut app, Some(&tx), &mut pending));
    rx.try_recv().unwrap();
    apply_response(
        &mut app,
        &mut pending,
        Response::Loaded(demo_neighborhood("demo-00")),
    );
    back_key(&mut app, KeyCode::Char('b'));
    assert_eq!(app.graph.nodes.len(), 2);
    assert_eq!(app.selected, 1);
    back_key(&mut app, KeyCode::Char('b'));
    assert_eq!(app.graph.nodes.len(), 12);
    assert_eq!(app.selected, 1);
    assert_eq!(app.scroll, 3);
    assert!(app.edge_lens.is_none());
    assert!(!app.can_go_back());
}

#[test]
fn overlay_union_remains_bounded_and_does_not_accumulate_between_anchors() {
    let mut app = demo();
    let graph = Graph {
        nodes: (0..100)
            .map(|i| Node {
                id: format!("neighbor-{i}"),
                ..Node::default()
            })
            .collect(),
        edges: (0..100)
            .map(|i| Edge {
                from: "demo-01".into(),
                to: format!("neighbor-{i}"),
                kind: "associative".into(),
                weight: 0.5,
            })
            .collect(),
        ..Graph::default()
    };
    app.open_edge_lens("demo-01".into(), graph, LensState::Ready);
    let lens = app.edge_lens.as_ref().unwrap();
    assert_eq!(lens.graph.nodes.len(), 33);
    assert_eq!(lens.graph.edges.len(), 32);
    assert!(lens.graph.partial);
    assert_eq!(app.scene_nodes().len(), 45);
    change_selection(&mut app, true);
    assert_eq!(app.scene_nodes().len(), 45); // selection keeps the temporary graph
    toggle_edge_lens(&mut app, None, &mut None); // dismiss the old lens explicitly
    toggle_edge_lens(&mut app, None, &mut None); // new anchor, not accumulated cards
    assert!(app.edge_lens.as_ref().unwrap().graph.nodes.len() < 33);
    assert_eq!(app.scene_nodes().len(), 12);
}
fn edge(from: &str, to: &str, kind: &str, weight: f64) -> Edge {
    Edge {
        from: from.into(),
        to: to.into(),
        kind: kind.into(),
        weight,
    }
}

#[test]
fn fresh_lens_archive_status_updates_placeholder_without_replacing_card_or_world() {
    for initial_status in ["", "active"] {
        for hide in [false, true] {
            let mut app = live_search();
            app.graph.nodes.truncate(2);
            app.selected = 0;
            app.graph.nodes[0].status = initial_status.into();
            let original = app.graph.nodes[0].clone();
            let positions = app.layout.borrow_mut().world_positions(&app.graph);
            if hide {
                app.toggle_archived();
            }
            let (tx, mut rx) = mpsc::channel(1);
            let mut pending = None;
            toggle_edge_lens(&mut app, Some(&tx), &mut pending);
            assert!(matches!(rx.try_recv().unwrap(), Request::Lens(id) if id == original.id));
            let neighbor_id = app.graph.nodes[1].id.clone();
            apply_response(
                &mut app,
                &mut pending,
                Response::Loaded(Graph {
                    // Actual e reply: get(body=false) knows status, neighbors can
                    // contain cards with unknown status. Base presentation wins.
                    nodes: vec![
                        Node {
                            id: original.id.clone(),
                            status: "archived".into(),
                            ..Node::default()
                        },
                        Node {
                            id: neighbor_id,
                            ..Node::default()
                        },
                    ],
                    focus: Some(original.id.clone()),
                    ..Graph::default()
                }),
            );
            let mut expected = original;
            expected.status = "archived".into();
            let actual = &app.graph.nodes[0];
            assert_eq!(actual.id, expected.id);
            assert_eq!(actual.status, expected.status);
            assert_eq!(actual.summary, expected.summary);
            assert_eq!(actual.body, expected.body);
            assert_eq!(actual.tags, expected.tags);
            assert_eq!(
                app.graph.nodes[1].status, "active",
                "unknown neighbor must not erase known status"
            );
            assert_eq!(
                positions,
                app.layout.borrow_mut().world_positions(&app.graph)
            );
            assert!(pending.is_none());
            if hide {
                assert!(app.edge_lens.is_none());
                assert_eq!(app.selected, 1);
                assert!(!app.node_visible(&expected.id));
            } else {
                assert_eq!(app.selected_node().unwrap().status, "archived");
                assert!(app.edge_lens.is_some());
            }
        }
    }
    // Status propagation must remain inside the admitted current-lens guard.
    let mut app = live_search();
    let id = app.graph.nodes[0].id.clone();
    let (tx, mut rx) = mpsc::channel(1);
    let mut pending = None;
    toggle_edge_lens(&mut app, Some(&tx), &mut pending);
    rx.try_recv().unwrap();
    app.close_edge_lens();
    apply_response(
        &mut app,
        &mut pending,
        Response::Loaded(Graph {
            nodes: vec![Node {
                id,
                status: "archived".into(),
                ..Node::default()
            }],
            ..Graph::default()
        }),
    );
    assert_eq!(app.graph.nodes[0].status, "active");
}

#[test]
fn asymmetric_neighbors_reply_keeps_known_incident_edge_and_base_geometry() {
    for (kind, selected) in [("transition", 1), ("derived_from", 0), ("supersedes", 0)] {
        let mut app = live_search();
        app.graph.nodes.truncate(2);
        app.graph.edges = vec![edge("demo-00", "demo-01", kind, 0.7)];
        app.selected = selected;
        let id = app.selected_node().unwrap().id.clone();
        let base_positions = app.layout.borrow_mut().positions(&app.graph, 80, 29);
        let base_ids: Vec<_> = app.graph.nodes.iter().map(|node| node.id.clone()).collect();
        let (tx, mut rx) = mpsc::channel(1);
        let mut pending = None;
        toggle_edge_lens(&mut app, Some(&tx), &mut pending);
        assert!(matches!(rx.try_recv().unwrap(), Request::Lens(found) if found == id));
        assert_eq!(app.scene_edges().len(), 1);
        // The non-traversing endpoint legitimately returns no neighbors.
        let fresh = Graph {
            nodes: vec![app.selected_node().unwrap().clone()],
            focus: Some(id),
            ..Graph::default()
        };
        apply_response(&mut app, &mut pending, Response::Loaded(fresh));
        let lens = app.edge_lens.as_ref().unwrap();
        assert_eq!(lens.state, LensState::Ready);
        assert_eq!(lens.graph.nodes.len(), 2);
        assert_eq!(lens.graph.edges.len(), 1);
        assert_eq!(lens.graph.edges[0].kind, kind);
        assert_eq!(lens.graph.edges[0].weight, 0.7);
        assert!(app.node_visible("demo-00") && app.node_visible("demo-01"));
        assert_eq!(
            app.graph
                .nodes
                .iter()
                .map(|node| node.id.clone())
                .collect::<Vec<_>>(),
            base_ids
        );
        assert_eq!(app.graph.edges.len(), 1);
        assert_eq!(app.selected, selected);
        assert!(!app.can_go_back());
        assert_eq!(
            app.layout.borrow_mut().positions(&app.graph, 80, 29),
            base_positions
        );
        back_key(&mut app, KeyCode::Esc);
        assert_eq!(app.scene_edges().len(), 1);
        assert_eq!(
            app.layout.borrow_mut().positions(&app.graph, 80, 29),
            base_positions
        );
        assert!(rx.try_recv().is_err());
    }
}

#[test]
fn fresh_matching_edge_updates_lens_weight_without_duplicates_or_base_mutation() {
    let mut app = live_search();
    app.graph.nodes.truncate(2);
    app.graph.edges = vec![edge("demo-00", "demo-01", "transition", 0.2)];
    let (tx, mut rx) = mpsc::channel(1);
    let mut pending = None;
    toggle_edge_lens(&mut app, Some(&tx), &mut pending);
    rx.try_recv().unwrap();
    let mut fresh = app.graph.clone();
    fresh.nodes[0].summary = "fresh card".into();
    fresh.edges = vec![
        edge("demo-00", "demo-01", "transition", 0.9),
        edge("demo-00", "demo-01", "transition", 0.9),
        edge("demo-01", "demo-00", "transition", 0.6),
        edge("demo-00", "demo-01", "associative", 0.4),
    ];
    apply_response(&mut app, &mut pending, Response::Loaded(fresh));
    let lens = app.edge_lens.as_ref().unwrap();
    assert_eq!(lens.graph.edges.len(), 3);
    assert_eq!(lens.graph.edges[0].from, "demo-00");
    assert_eq!(lens.graph.edges[0].to, "demo-01");
    assert_eq!(lens.graph.edges[0].kind, "transition");
    assert_eq!(lens.graph.edges[0].weight, 0.9);
    assert_eq!(
        lens.graph
            .nodes
            .iter()
            .find(|node| node.id == "demo-00")
            .unwrap()
            .summary,
        "fresh card"
    );
    assert_eq!(app.graph.edges.len(), 1);
    assert_eq!(app.graph.edges[0].weight, 0.2);
    assert_ne!(app.graph.nodes[0].summary, "fresh card");
}

#[test]
fn merged_lens_prioritizes_known_edges_and_keeps_every_retained_endpoint_card() {
    let mut app = live_search();
    app.graph.nodes = (0..=10)
        .map(|i| Node {
            id: format!("known-{i}"),
            ..Node::default()
        })
        .collect();
    app.selected = 0;
    app.graph.edges = (1..=10)
        .map(|i| edge("known-0", &format!("known-{i}"), "transition", 0.5))
        .collect();
    let (tx, mut rx) = mpsc::channel(1);
    let mut pending = None;
    toggle_edge_lens(&mut app, Some(&tx), &mut pending);
    rx.try_recv().unwrap();
    let mut fresh = Graph {
        nodes: vec![app.graph.nodes[0].clone()],
        ..Graph::default()
    };
    fresh.nodes.extend((0..32).map(|i| Node {
        id: format!("new-{i}"),
        ..Node::default()
    }));
    fresh.edges = (0..32)
        .map(|i| edge("known-0", &format!("new-{i}"), "associative", 0.6))
        .collect();
    apply_response(&mut app, &mut pending, Response::Loaded(fresh));
    let lens = app.edge_lens.as_ref().unwrap();
    assert_eq!(lens.graph.edges.len(), 32);
    assert_eq!(lens.graph.nodes.len(), 33);
    assert!(lens.graph.partial);
    for (kept, known) in lens.graph.edges.iter().zip(&app.graph.edges) {
        assert_eq!(
            (&kept.from, &kept.to, &kept.kind),
            (&known.from, &known.to, &known.kind)
        );
    }
    for edge in &lens.graph.edges {
        assert!(lens.graph.nodes.iter().any(|node| node.id == edge.from));
        assert!(lens.graph.nodes.iter().any(|node| node.id == edge.to));
    }
    assert!(lens.graph.nodes.iter().any(|node| node.id == "new-21"));
    assert!(!lens.graph.nodes.iter().any(|node| node.id == "new-22"));
    assert_eq!(app.graph.nodes.len(), 11);
    assert_eq!(app.graph.edges.len(), 10);
    assert!(!app.can_go_back());
}

#[test]
fn incomplete_fresh_cards_never_leave_dangling_lens_edges() {
    let mut app = live_search();
    app.graph.edges = vec![edge("demo-00", "demo-01", "transition", 0.5)];
    let (tx, mut rx) = mpsc::channel(1);
    let mut pending = None;
    toggle_edge_lens(&mut app, Some(&tx), &mut pending);
    rx.try_recv().unwrap();
    let fresh = Graph {
        nodes: vec![app.selected_node().unwrap().clone()],
        edges: vec![edge("demo-01", "missing", "associative", 0.5)],
        ..Graph::default()
    };
    apply_response(&mut app, &mut pending, Response::Loaded(fresh));
    let lens = app.edge_lens.as_ref().unwrap();
    assert!(lens.graph.partial);
    assert_eq!(lens.graph.edges.len(), 1);
    assert_eq!(lens.graph.edges[0].kind, "transition");
    assert_eq!(lens.graph.nodes.len(), 2);
}

#[test]
fn failed_or_dismissed_lens_reply_never_replaces_known_incident_edges() {
    for dismissed in [false, true] {
        let mut app = live_search();
        app.graph.edges = vec![edge("demo-00", "demo-01", "transition", 0.7)];
        let (tx, mut rx) = mpsc::channel(1);
        let mut pending = None;
        toggle_edge_lens(&mut app, Some(&tx), &mut pending);
        rx.try_recv().unwrap();
        if dismissed {
            back_key(&mut app, KeyCode::Esc);
        }
        let reply = if dismissed {
            Response::Loaded(Graph::default())
        } else {
            Response::Error("offline".into())
        };
        apply_response(&mut app, &mut pending, reply);
        assert_eq!(app.scene_edges().len(), 1);
        assert_eq!(app.scene_edges()[0].weight, 0.7);
        assert_eq!(app.graph.edges.len(), 1);
        if dismissed {
            assert!(app.edge_lens.is_none());
        } else {
            assert_eq!(
                app.edge_lens.as_ref().unwrap().state,
                LensState::Unavailable
            );
        }
    }
}

#[test]
fn graph_refresh_follows_identity_not_new_rank() {
    let mut app = demo();
    let selected = app.selected_node().unwrap().id.clone();
    let mut graph = app.graph.clone();
    graph.nodes.reverse();
    let mut pending = Some(PendingRead::Refresh);
    apply_response(&mut app, &mut pending, Response::Loaded(graph));
    assert_eq!(app.selected_node().unwrap().id, selected);
    assert_eq!(app.selected, 10);
    assert!(!app.can_go_back());
}

#[test]
fn map_back_restores_geometry_not_just_node_ids() {
    let mut app = demo();
    let before = app.layout.borrow_mut().positions(&app.graph, 70, 26);
    focus_selected(&mut app, None, &mut None);
    let _ = app.layout.borrow_mut().positions(&app.graph, 70, 26);
    app.go_back();
    let after = app.layout.borrow_mut().positions(&app.graph, 70, 26);
    assert_eq!(before, after);
}

#[test]
fn source_switch_forgets_scene_and_geometry_state() {
    let mut app = demo();
    app.targets = vec![Target {
        name: "other".into(),
        database: "project".into(),
        url: "http://127.0.0.1:1".into(),
        ssh_mcp_port: 18766,
        token_env: None,
        expected_db_id: None,
        expected_path: None,
    }];
    switch_view(&mut app, None, &mut None);
    open_scene(&mut app, None, &mut None);
    reset_source(&mut app, &mut None);
    assert!(app.scene_detail.is_none());
    assert!(app.scenes.items.is_empty());
    assert!(!app.scenes_loaded);
    assert!(app.graph.nodes.is_empty());
}

#[test]
fn back_from_initial_scenes_requests_the_never_loaded_map() {
    let mut app = App::new(vec![], "context".into(), true);
    switch_view(&mut app, None, &mut None);
    assert!(back_key(&mut app, KeyCode::Char('b')));
    assert_eq!(app.view, View::Map);
    let (tx, mut rx) = mpsc::channel(1);
    initial_read(&mut app, &tx, &mut None);
    assert!(matches!(rx.try_recv().unwrap(), Request::Query(cue) if cue == "context"));
}
