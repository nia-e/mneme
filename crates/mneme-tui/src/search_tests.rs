use super::*;
use crossterm::event::{KeyEvent, MouseEvent};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn map() -> App {
    let mut app = App::new(vec![], String::new(), false);
    app.graph = demo_graph();
    app.graph.inventory = Some(InventoryState {
        complete: true,
        topology_edges_complete: true,
        ..Default::default()
    });
    app.selected = 3;
    app.scroll = 4;
    app.show_details = true;
    app.last_refresh = Some(Instant::now());
    render::preview(120, 36, &app).unwrap();
    app.layout.borrow_mut().zoom_by(1.5);
    app.layout.borrow_mut().pan(4.0, -2.0);
    app.remember(app.visit());
    app
}

fn page() -> SearchPage {
    let graph = demo_graph();
    SearchPage {
        nodes: [8, 3, 10]
            .into_iter()
            .map(|index| graph.nodes[index].clone())
            .collect(),
        ..Default::default()
    }
}

fn begin(
    app: &mut App,
    pending: &mut Option<PendingRead>,
    tx: &mpsc::Sender<Request>,
    rx: &mut mpsc::Receiver<Request>,
    query: &str,
) {
    assert!(query_field(app, tx, pending, query.into()));
    assert!(matches!(rx.try_recv().unwrap(), Request::Query(value) if value == query));
    assert!(app.search_visible());
    assert_eq!(app.search.as_ref().unwrap().state, LensState::Loading);
}

#[test]
fn ranked_search_and_all_back_keys_retain_the_exact_underlying_map() {
    for back in [KeyCode::Esc, KeyCode::Backspace, KeyCode::Char('b')] {
        let mut app = map();
        app.error = Some("retained owner warning".into());
        let graph = format!("{:?}", app.graph);
        let layout = format!("{:?}", app.layout.borrow());
        let refreshed = app.last_refresh;
        let (tx, mut rx) = mpsc::channel(2);
        let mut pending = None;
        begin(&mut app, &mut pending, &tx, &mut rx, "deepseek");
        apply_response(&mut app, &mut pending, Response::Search(page()));
        assert_eq!(
            app.search
                .as_ref()
                .unwrap()
                .page
                .nodes
                .iter()
                .map(|node| node.id.as_str())
                .collect::<Vec<_>>(),
            ["demo-08", "demo-03", "demo-10"]
        );
        search_move(&mut app, true, 2);
        assert_eq!(app.search.as_ref().unwrap().selected, 2);
        assert!(!back_key(&mut app, back));
        assert!(app.search.is_none());
        assert_eq!(format!("{:?}", app.graph), graph);
        assert_eq!(format!("{:?}", app.layout.borrow()), layout);
        assert_eq!(app.selected, 3);
        assert_eq!(app.scroll, 4);
        assert!(app.show_details);
        assert_eq!(app.history.len(), 1);
        assert_eq!(app.last_refresh, refreshed);
        assert_eq!(app.error.as_deref(), Some("retained owner warning"));
        assert!(rx.try_recv().is_err());
    }
}

#[test]
fn opening_a_hit_keeps_result_rank_and_body_scroll_out_of_the_map() {
    let mut app = map();
    let graph = format!("{:?}", app.graph);
    let center = app.layout.borrow().camera.center;
    let zoom = app.layout.borrow().zoom_factor();
    let (tx, mut rx) = mpsc::channel(2);
    let mut pending = None;
    begin(&mut app, &mut pending, &tx, &mut rx, "deepseek");
    apply_response(&mut app, &mut pending, Response::Search(page()));
    search_select(&mut app, 1);
    assert!(open_search_hit(&mut app, Some(&tx), &mut pending));
    assert!(matches!(rx.try_recv().unwrap(), Request::Focus(id) if id == "demo-03"));
    assert!(matches!(&pending, Some(PendingRead::SearchDetail(id)) if id == "demo-03"));
    let mut detail = demo_neighborhood("demo-03");
    detail.nodes[0].body = (0..80).map(|i| format!("Body line {i}\n")).collect();
    apply_response(&mut app, &mut pending, Response::Loaded(detail));
    render::preview(100, 25, &app).unwrap();
    assert!(detail_scroll::key(&mut app, key(KeyCode::PageDown)));
    assert!(app.search.as_ref().unwrap().scroll > 0);
    assert_eq!(app.scroll, 4);
    assert!(!back_key(&mut app, KeyCode::Backspace));
    assert!(app.search.as_ref().unwrap().detail.is_none());
    assert_eq!(app.search.as_ref().unwrap().selected, 1);
    assert!(!back_key(&mut app, KeyCode::Char('b')));
    assert!(app.search.is_none());
    assert_eq!(format!("{:?}", app.graph), graph);
    assert_eq!(app.layout.borrow().camera.center, center);
    assert_eq!(app.layout.borrow().zoom_factor(), zoom);
    assert_eq!(app.history.len(), 1);
}

#[test]
fn late_query_success_and_failure_cannot_resurrect_dismissed_results() {
    for response in [
        Response::Search(page()),
        Response::Error("unavailable".into()),
    ] {
        let mut app = map();
        app.error = Some("map warning".into());
        let graph = format!("{:?}", app.graph);
        let (tx, mut rx) = mpsc::channel(2);
        let mut pending = None;
        begin(&mut app, &mut pending, &tx, &mut rx, "deepseek");
        assert!(app.busy);
        assert!(!back_key(&mut app, KeyCode::Esc));
        assert!(app.search.is_none());
        apply_response(&mut app, &mut pending, response);
        assert!(app.search.is_none());
        assert!(!app.busy);
        assert!(pending.is_none());
        assert_eq!(format!("{:?}", app.graph), graph);
        assert_eq!(app.error.as_deref(), Some("map warning"));
    }
}

#[test]
fn dismissed_or_changed_hit_ignores_late_detail_and_its_error() {
    for response in [
        Response::Loaded(demo_neighborhood("demo-08")),
        Response::Error("late body failure".into()),
    ] {
        let mut app = map();
        let (tx, mut rx) = mpsc::channel(2);
        let mut pending = None;
        begin(&mut app, &mut pending, &tx, &mut rx, "deepseek");
        apply_response(&mut app, &mut pending, Response::Search(page()));
        assert!(open_search_hit(&mut app, Some(&tx), &mut pending));
        rx.try_recv().unwrap();
        assert!(!back_key(&mut app, KeyCode::Esc));
        assert!(app.search.is_some());
        search_select(&mut app, 2);
        apply_response(&mut app, &mut pending, response);
        let search = app.search.as_ref().unwrap();
        assert!(search.detail.is_none());
        assert!(search.error.is_none());
        assert!(search.opening.is_none());
        assert_eq!(search.selected, 2);
        assert_eq!(app.selected, 3);
    }
}

#[test]
fn research_refresh_and_failed_search_do_not_replace_map_or_back_history() {
    let mut app = map();
    let graph = format!("{:?}", app.graph);
    let (tx, mut rx) = mpsc::channel(2);
    let mut pending = None;
    begin(&mut app, &mut pending, &tx, &mut rx, "first");
    apply_response(&mut app, &mut pending, Response::Search(page()));
    begin(&mut app, &mut pending, &tx, &mut rx, "second");
    apply_response(
        &mut app,
        &mut pending,
        Response::Error("source unavailable".into()),
    );
    let search = app.search.as_ref().unwrap();
    assert_eq!(search.state, LensState::Unavailable);
    assert_eq!(search.error.as_deref(), Some("source unavailable"));
    assert!(search.page.nodes.is_empty());
    assert!(search_key(
        &mut app,
        key(KeyCode::Char('r')),
        Some(&tx),
        &mut pending
    ));
    assert!(matches!(rx.try_recv().unwrap(), Request::Query(query) if query == "second"));
    apply_response(
        &mut app,
        &mut pending,
        Response::Search(SearchPage::default()),
    );
    assert_eq!(app.search.as_ref().unwrap().state, LensState::Ready);
    assert!(app.search.as_ref().unwrap().error.is_none());
    assert!(app.search.as_ref().unwrap().page.nodes.is_empty());
    assert_eq!(format!("{:?}", app.graph), graph);
    assert_eq!(app.history.len(), 1);
}

#[test]
fn partial_ranked_page_is_bounded_without_becoming_the_inventory() {
    let mut app = map();
    let (tx, mut rx) = mpsc::channel(2);
    let mut pending = None;
    begin(&mut app, &mut pending, &tx, &mut rx, "many");
    let nodes = (0..20)
        .map(|index| Node {
            id: format!("rank-{index}"),
            ..Default::default()
        })
        .collect();
    apply_response(
        &mut app,
        &mut pending,
        Response::Search(SearchPage {
            nodes,
            ..Default::default()
        }),
    );
    let search = app.search.as_ref().unwrap();
    assert_eq!(search.page.nodes.len(), data::QUERY_NODES);
    assert_eq!(search.page.nodes[0].id, "rank-0");
    assert_eq!(search.page.nodes.last().unwrap().id, "rank-15");
    assert!(search.page.partial);
    assert_eq!(app.graph.nodes.len(), 12);
    assert!(app.graph.inventory.is_some());
}

#[test]
fn startup_query_dismissal_starts_full_inventory_not_the_same_query_again() {
    let mut app = App::new(vec![], "deepseek".into(), false);
    let (tx, mut rx) = mpsc::channel(2);
    let mut pending = None;
    initial_map(&mut app, &tx, &mut pending);
    assert!(matches!(rx.try_recv().unwrap(), Request::Query(query) if query == "deepseek"));
    assert!(app.query.is_empty());
    assert!(app.search_visible());
    assert!(!view_is_unread(&app));
    apply_response(&mut app, &mut pending, Response::Search(page()));
    assert!(app.last_refresh.is_none());
    assert!(search_key(
        &mut app,
        key(KeyCode::Esc),
        Some(&tx),
        &mut pending
    ));
    assert!(app.search.is_none());
    assert!(matches!(
        rx.try_recv().unwrap(),
        Request::Inventory { after: None }
    ));
    assert!(matches!(
        pending,
        Some(PendingRead::Inventory { after: None, .. })
    ));
}

#[test]
fn startup_dismissal_before_reply_defers_inventory_and_ignores_late_results() {
    let mut app = App::new(vec![], "deepseek".into(), false);
    let (tx, mut rx) = mpsc::channel(2);
    let mut pending = None;
    initial_map(&mut app, &tx, &mut pending);
    rx.try_recv().unwrap();
    assert!(search_key(
        &mut app,
        key(KeyCode::Backspace),
        Some(&tx),
        &mut pending
    ));
    assert!(app.search.is_none());
    assert!(app.deferred_view_read);
    assert!(rx.try_recv().is_err());
    apply_response(&mut app, &mut pending, Response::Search(page()));
    assert!(!app.busy);
    assert!(app.search.is_none());
    assert!(app.deferred_view_read);
    assert!(view_is_unread(&app));
    initial_read(&mut app, &tx, &mut pending);
    assert!(matches!(
        rx.try_recv().unwrap(),
        Request::Inventory { after: None }
    ));
}

#[test]
fn search_owns_list_navigation_mouse_and_hidden_map_shortcuts() {
    let mut app = map();
    let center = app.layout.borrow().camera.center;
    let zoom = app.layout.borrow().zoom_factor();
    let (tx, mut rx) = mpsc::channel(2);
    let mut pending = None;
    begin(&mut app, &mut pending, &tx, &mut rx, "deepseek");
    apply_response(&mut app, &mut pending, Response::Search(page()));
    for code in [
        KeyCode::Char('q'),
        KeyCode::Char('?'),
        KeyCode::Char('C'),
        KeyCode::Tab,
    ] {
        assert!(!search_key(&mut app, key(code), Some(&tx), &mut pending));
    }
    for code in [
        KeyCode::Char('+'),
        KeyCode::Char('h'),
        KeyCode::Char('s'),
        KeyCode::Char('n'),
    ] {
        assert!(search_key(&mut app, key(code), Some(&tx), &mut pending));
    }
    assert_eq!(app.view, View::Map);
    assert!(!app.hide_archived);
    assert!(search_key(
        &mut app,
        key(KeyCode::End),
        Some(&tx),
        &mut pending
    ));
    assert_eq!(app.search.as_ref().unwrap().selected, 2);
    assert!(search_key(
        &mut app,
        key(KeyCode::Home),
        Some(&tx),
        &mut pending
    ));
    render::preview(120, 36, &app).unwrap();
    let hit = app.search.as_ref().unwrap().hits.borrow()[1].0;
    assert!(search_mouse(
        &mut app,
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: hit.x,
            row: hit.y,
            modifiers: KeyModifiers::NONE,
        },
        1
    ));
    assert_eq!(app.search.as_ref().unwrap().selected, 1);
    assert_eq!(app.selected, 3);
    assert_eq!(app.layout.borrow().camera.center, center);
    assert_eq!(app.layout.borrow().zoom_factor(), zoom);
    assert!(rx.try_recv().is_err());
}

#[test]
fn search_neither_hydrates_nor_advances_the_hidden_inventory() {
    let mut app = map();
    app.graph.inventory.as_mut().unwrap().complete = false;
    app.graph.inventory.as_mut().unwrap().next = Some("next".into());
    let (tx, mut rx) = mpsc::channel(2);
    let mut pending = None;
    begin(&mut app, &mut pending, &tx, &mut rx, "deepseek");
    apply_response(&mut app, &mut pending, Response::Search(page()));
    hydrate_viewport(&mut app, &tx, &mut pending);
    assert!(rx.try_recv().is_err());
    pending = Some(PendingRead::Inventory {
        after: Some("next".into()),
        pages: 1,
        bytes: 100,
        started: Instant::now(),
        waiting: false,
    });
    advance_inventory(&mut app, &tx, &mut pending);
    assert!(rx.try_recv().is_err());
    assert!(!app.busy);
}

#[test]
fn switching_owner_discards_results_and_late_answers() {
    let mut app = map();
    let (tx, mut rx) = mpsc::channel(2);
    let mut pending = None;
    begin(&mut app, &mut pending, &tx, &mut rx, "deepseek");
    reset_source(&mut app, &mut pending);
    assert!(app.search.is_none());
    assert!(app.query.is_empty());
    assert!(app.graph.nodes.is_empty());
    apply_response(&mut app, &mut pending, Response::Search(page()));
    assert!(app.search.is_none());
    assert!(app.graph.nodes.is_empty());
}

#[test]
fn displayed_search_provenance_excludes_the_hidden_map_and_returns_on_close() {
    let mut app = map();
    let snapshot = read_source::example();
    app.graph.read_source = snapshot.clone();
    let (tx, mut rx) = mpsc::channel(2);
    let mut pending = None;
    begin(&mut app, &mut pending, &tx, &mut rx, "deepseek");
    apply_response(&mut app, &mut pending, Response::Search(page()));
    assert_eq!(
        app.displayed_read_source(),
        read_source::ReadProvenance::default()
    );
    assert!(open_search_hit(&mut app, Some(&tx), &mut pending));
    rx.try_recv().unwrap();
    let mut detail = demo_neighborhood("demo-08");
    detail.read_source = snapshot.clone();
    apply_response(&mut app, &mut pending, Response::Loaded(detail));
    let displayed = app.displayed_read_source();
    assert_eq!(displayed.snapshot, snapshot.snapshot);
    assert!(displayed.mixed);
    back_key(&mut app, KeyCode::Esc);
    assert_eq!(
        app.displayed_read_source(),
        read_source::ReadProvenance::default()
    );
    back_key(&mut app, KeyCode::Esc);
    assert_eq!(app.displayed_read_source(), snapshot);
}
