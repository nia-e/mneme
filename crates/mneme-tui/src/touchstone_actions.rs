//! One cabinet page, one authored annotation, one deliberately inspected citation.
use super::*;
use crate::touchstones::{ExactTarget, ReferenceResolution, TouchstoneDetail, TouchstonePage};

pub(super) fn view_is_unread(app: &App) -> bool {
    match app.view {
        View::Map => app.last_refresh.is_none(),
        View::Scenes => !app.scenes_loaded,
        View::Touchstones => !app.cabinet.loaded,
    }
}

fn pending_serves_view(app: &App, pending: &Option<PendingRead>) -> bool {
    match (app.view, pending) {
        (
            View::Map,
            Some(
                PendingRead::Inventory { .. }
                | PendingRead::Summaries { .. }
                | PendingRead::Query(_)
                | PendingRead::Navigate(_)
                | PendingRead::Refresh
                | PendingRead::EdgeLens(_),
            ),
        ) => true,
        (View::Scenes, Some(PendingRead::Scenes { .. } | PendingRead::Scene { .. })) => true,
        (View::Touchstones, Some(intent)) => {
            !cabinet_intent_is_stale(app, intent)
                && matches!(
                    intent,
                    PendingRead::Cabinet { .. }
                        | PendingRead::Annotation { .. }
                        | PendingRead::ExactTarget { .. }
                )
        }
        _ => false,
    }
}

fn generation(app: &App) -> u64 {
    app.cabinet.generation
}

pub(super) fn cabinet_intent_is_stale(app: &App, intent: &PendingRead) -> bool {
    let expected = match intent {
        PendingRead::Cabinet { generation }
        | PendingRead::Annotation { generation, .. }
        | PendingRead::ExactTarget { generation, .. } => *generation,
        _ => return false,
    };
    app.view != View::Touchstones || expected != generation(app)
}

pub(super) fn load_cabinet(
    app: &mut App,
    tx: Option<&mpsc::Sender<Request>>,
    pending: &mut Option<PendingRead>,
    after: Option<String>,
) -> bool {
    if app.busy {
        return false;
    }
    let intent = PendingRead::Cabinet {
        generation: generation(app),
    };
    if app.demo {
        *pending = Some(intent);
        let page = if after.is_some() {
            TouchstonePage::default()
        } else {
            touchstones::demo_touchstones()
        };
        apply_cabinet_page(app, pending, page);
        return true;
    }
    if let Some(tx) = tx {
        if request(app, tx, Request::Touchstones { after }) {
            *pending = Some(intent);
            return true;
        }
    }
    false
}

pub(super) fn leave_cabinet(
    app: &mut App,
    tx: Option<&mpsc::Sender<Request>>,
    pending: &mut Option<PendingRead>,
    view: View,
) {
    app.cabinet.invalidate();
    app.view = view;
    app.cabinet.detail = None;
    app.cabinet.current = None;
    app.show_details = false;
    app.scroll = 0;
    app.deferred_view_read = false;
    let unread = view_is_unread(app);
    if unread && !app.demo {
        if app.busy {
            app.deferred_view_read = !pending_serves_view(app, pending);
        } else if let Some(tx) = tx {
            initial_read(app, tx, pending);
        }
    }
}

pub(super) fn toggle_cabinet(
    app: &mut App,
    tx: Option<&mpsc::Sender<Request>>,
    pending: &mut Option<PendingRead>,
) {
    if app.view == View::Touchstones {
        leave_cabinet(app, tx, pending, View::Map);
        return;
    }
    app.deferred_view_read = false;
    app.cabinet.invalidate();
    app.close_edge_lens();
    app.show_details = false;
    app.scroll = 0;
    app.view = View::Touchstones;
    if !app.cabinet.loaded {
        if app.busy {
            app.deferred_view_read = true;
        } else {
            load_cabinet(app, tx, pending, None);
        }
    }
}

pub(super) fn cabinet_selection(app: &mut App, forward: bool) {
    let count = app.cabinet.page.items.len();
    if count == 0 {
        return;
    }
    app.cabinet.selected = (app.cabinet.selected + if forward { 1 } else { count - 1 }) % count;
    app.cabinet.invalidate();
    app.cabinet.detail = None;
    app.cabinet.current = None;
    app.cabinet.reference = 0;
    app.show_details = false;
    app.scroll = 0;
    app.error = None;
}

pub(super) fn cabinet_reference(app: &mut App, forward: bool) {
    let count = app
        .cabinet
        .detail
        .as_ref()
        .map_or(0, |detail| detail.references.len());
    if count == 0 {
        return;
    }
    app.cabinet.reference = (app.cabinet.reference + if forward { 1 } else { count - 1 }) % count;
    app.cabinet.invalidate();
    app.cabinet.current = None;
    app.scroll = 0;
    app.error = None;
}

pub(super) fn open_annotation(
    app: &mut App,
    tx: Option<&mpsc::Sender<Request>>,
    pending: &mut Option<PendingRead>,
) {
    if app.busy {
        return;
    }
    let Some(card) = app.cabinet.page.items.get(app.cabinet.selected).cloned() else {
        return;
    };
    if app
        .cabinet
        .detail
        .as_ref()
        .is_some_and(|detail| detail.owner == card.id)
    {
        app.show_details = true;
        app.scroll = 0;
        return;
    }
    let intent = PendingRead::Annotation {
        generation: generation(app),
        id: card.id.clone(),
    };
    if app.demo {
        *pending = Some(intent);
        apply_annotation(app, pending, touchstones::demo_detail(&card));
    } else if let Some(tx) = tx {
        if request(app, tx, Request::Touchstone { id: card.id }) {
            *pending = Some(intent);
        }
    }
}

pub(super) fn open_exact_target(
    app: &mut App,
    tx: Option<&mpsc::Sender<Request>>,
    pending: &mut Option<PendingRead>,
) {
    if app.busy {
        return;
    }
    let Some(reference) = app
        .cabinet
        .detail
        .as_ref()
        .and_then(|detail| detail.references.get(app.cabinet.reference))
        .cloned()
    else {
        return;
    };
    let intent = PendingRead::ExactTarget {
        generation: generation(app),
        db_id: reference.db_id.clone(),
        id: reference.id.clone(),
    };
    if app.demo {
        *pending = Some(intent);
        let node = (reference.resolution != ReferenceResolution::Absent).then(|| Node {
            id: reference.id,
            summary: if reference.resolution == ReferenceResolution::Unchanged { reference.summary } else { "The original scene, inspected today".into() },
            body: "This is a synthetic explicit read of the cited edition. Later editions have not replaced it.".into(),
            status: "Active".into(), ..Default::default()
        });
        apply_exact_target(
            app,
            pending,
            ExactTarget {
                db_id: reference.db_id,
                node,
                body_partial: false,
                summary_partial: false,
            },
        );
    } else if let Some(tx) = tx {
        if request(
            app,
            tx,
            Request::TouchstoneTarget {
                db_id: reference.db_id,
                id: reference.id,
            },
        ) {
            *pending = Some(intent);
        }
    }
}

/// Esc peels the current read, then the annotation, then leaves for the map.
/// An in-flight read is invalidated but not replaced until its serial reply arrives.
pub(super) fn cabinet_back(app: &mut App) -> bool {
    app.cabinet.invalidate();
    app.deferred_view_read = false;
    app.error = None;
    app.scroll = 0;
    if app.cabinet.current.take().is_some() {
        return false;
    }
    if app.cabinet.detail.take().is_some() {
        app.show_details = false;
        return false;
    }
    app.view = View::Map;
    app.show_details = false;
    let unread = app.last_refresh.is_none();
    if unread && app.busy {
        app.deferred_view_read = true;
        return false;
    }
    unread
}

pub(super) fn apply_cabinet_page(
    app: &mut App,
    pending: &mut Option<PendingRead>,
    page: TouchstonePage,
) {
    let Some(intent @ PendingRead::Cabinet { .. }) = pending.take() else {
        return;
    };
    if cabinet_intent_is_stale(app, &intent) {
        return;
    }
    app.cabinet.page = page;
    app.cabinet.selected = 0;
    app.cabinet.detail = None;
    app.cabinet.reference = 0;
    app.cabinet.current = None;
    app.cabinet.loaded = true;
    app.cabinet.invalidate();
    app.show_details = false;
    app.scroll = 0;
    app.error = None;
    app.event(format!(
        "Read {} touchstones · one shelf page",
        app.cabinet.page.items.len()
    ));
}

pub(super) fn apply_annotation(
    app: &mut App,
    pending: &mut Option<PendingRead>,
    detail: TouchstoneDetail,
) {
    let Some(intent @ PendingRead::Annotation { .. }) = pending.take() else {
        return;
    };
    if cabinet_intent_is_stale(app, &intent) {
        return;
    }
    let PendingRead::Annotation { id, .. } = intent else {
        unreachable!()
    };
    if detail.owner != id
        || !app
            .cabinet
            .page
            .items
            .get(app.cabinet.selected)
            .is_some_and(|card| card.id == id)
    {
        app.error = Some("Touchstone identity changed · r restarts this shelf".into());
        return;
    }
    app.cabinet.detail = Some(detail);
    app.cabinet.reference = 0;
    app.cabinet.current = None;
    app.show_details = true;
    app.scroll = 0;
    app.error = None;
}

pub(super) fn apply_exact_target(
    app: &mut App,
    pending: &mut Option<PendingRead>,
    target: ExactTarget,
) {
    let Some(intent @ PendingRead::ExactTarget { .. }) = pending.take() else {
        return;
    };
    if cabinet_intent_is_stale(app, &intent) {
        return;
    }
    let PendingRead::ExactTarget { db_id, id, .. } = intent else {
        unreachable!()
    };
    let reference = app
        .cabinet
        .detail
        .as_ref()
        .and_then(|detail| detail.references.get(app.cabinet.reference));
    if target.db_id != db_id
        || target.node.as_ref().is_some_and(|node| node.id != id)
        || !reference.is_some_and(|reference| reference.db_id == db_id && reference.id == id)
    {
        app.error = Some("Exact citation identity changed · c retries this reference".into());
        return;
    }
    app.cabinet.current = Some(target);
    app.show_details = true;
    app.scroll = 0;
    app.error = None;
}

#[cfg(test)]
mod tests {
    use super::*;
    fn app() -> App {
        let mut app = App::new(vec![], "memory".into(), true);
        app.graph = demo_graph();
        toggle_cabinet(&mut app, None, &mut None);
        app
    }
    #[test]
    fn secondary_view_keys_have_deterministic_destinations_not_entry_history() {
        for (start, with_s, expected) in [
            (View::Map, true, View::Scenes),
            (View::Scenes, true, View::Map),
            (View::Touchstones, true, View::Scenes),
            (View::Map, false, View::Touchstones),
            (View::Scenes, false, View::Touchstones),
            (View::Touchstones, false, View::Map),
        ] {
            let mut app = app();
            app.view = start;
            if with_s {
                switch_view(&mut app, None, &mut None);
            } else {
                toggle_cabinet(&mut app, None, &mut None);
            }
            assert_eq!(app.view, expected);
        }
        for entry in [View::Map, View::Scenes] {
            for with_s in [false, true] {
                let mut app = app();
                app.view = entry;
                let ids: Vec<_> = app.graph.nodes.iter().map(|n| n.id.clone()).collect();
                toggle_cabinet(&mut app, None, &mut None);
                assert_eq!(app.view, View::Touchstones);
                if with_s {
                    switch_view(&mut app, None, &mut None);
                } else {
                    toggle_cabinet(&mut app, None, &mut None);
                }
                assert_eq!(app.view, if with_s { View::Scenes } else { View::Map });
                assert_eq!(
                    app.graph
                        .nodes
                        .iter()
                        .map(|n| n.id.clone())
                        .collect::<Vec<_>>(),
                    ids
                );
            }
        }
    }

    #[test]
    fn cabinet_toggles_without_replacing_map_or_scenes() {
        let mut app = app();
        let ids: Vec<_> = app.graph.nodes.iter().map(|n| n.id.clone()).collect();
        toggle_cabinet(&mut app, None, &mut None);
        assert_eq!(app.view, View::Map);
        app.view = View::Scenes;
        toggle_cabinet(&mut app, None, &mut None);
        toggle_cabinet(&mut app, None, &mut None);
        assert_eq!(app.view, View::Map);
        assert_eq!(
            app.graph
                .nodes
                .iter()
                .map(|n| n.id.clone())
                .collect::<Vec<_>>(),
            ids
        );
    }
    #[test]
    fn failed_next_keeps_previous_page_cursor_and_selection() {
        let mut app = app();
        app.demo = false;
        app.cabinet.page.next = Some("next".into());
        let id = app.cabinet.page.items[0].id.clone();
        let (tx, mut rx) = mpsc::channel(1);
        let mut pending = None;
        assert!(load_cabinet(
            &mut app,
            Some(&tx),
            &mut pending,
            Some("next".into())
        ));
        assert!(
            matches!(rx.try_recv().unwrap(), Request::Touchstones { after: Some(cursor) } if cursor=="next")
        );
        apply_response(&mut app, &mut pending, Response::Error("offline".into()));
        assert_eq!(app.cabinet.page.items[0].id, id);
        assert_eq!(app.cabinet.page.next.as_deref(), Some("next"));
        assert!(app.error.is_some());
    }
    #[test]
    fn stale_page_success_and_failure_ignore_selection_and_view_changes() {
        for failure in [false, true] {
            let mut app = app();
            let mut pending = Some(PendingRead::Cabinet {
                generation: generation(&app),
            });
            cabinet_selection(&mut app, true);
            if failure {
                apply_response(&mut app, &mut pending, Response::Error("stale".into()));
            } else {
                apply_response(
                    &mut app,
                    &mut pending,
                    Response::Touchstones(TouchstonePage::default()),
                );
            }
            assert!(!app.cabinet.page.items.is_empty());
            assert!(app.error.is_none());
            let mut pending = Some(PendingRead::Cabinet {
                generation: generation(&app),
            });
            toggle_cabinet(&mut app, None, &mut pending);
            apply_response(&mut app, &mut pending, Response::Error("stale view".into()));
            assert!(app.error.is_none());
        }
    }
    #[test]
    fn exact_target_request_never_uses_an_episode_successor() {
        let mut app = app();
        open_annotation(&mut app, None, &mut None);
        app.demo = false;
        let reference = app.cabinet.detail.as_ref().unwrap().references[0].clone();
        let (tx, mut rx) = mpsc::channel(1);
        open_exact_target(&mut app, Some(&tx), &mut None);
        assert!(
            matches!(rx.try_recv().unwrap(), Request::TouchstoneTarget { db_id, id } if db_id==reference.db_id && id==reference.id)
        );
    }
    #[test]
    fn reference_changes_discard_stale_target_success_and_error() {
        for failure in [false, true] {
            let mut app = app();
            open_annotation(&mut app, None, &mut None);
            let reference = app.cabinet.detail.as_ref().unwrap().references[0].clone();
            let mut pending = Some(PendingRead::ExactTarget {
                generation: generation(&app),
                db_id: reference.db_id.clone(),
                id: reference.id,
            });
            cabinet_reference(&mut app, true);
            if failure {
                apply_response(
                    &mut app,
                    &mut pending,
                    Response::Error("stale target".into()),
                );
            } else {
                apply_response(
                    &mut app,
                    &mut pending,
                    Response::TouchstoneTarget(ExactTarget {
                        db_id: reference.db_id,
                        node: None,
                        body_partial: false,
                        summary_partial: false,
                    }),
                );
            }
            assert!(app.cabinet.current.is_none());
            assert!(app.error.is_none());
        }
    }
    #[test]
    fn escape_peels_current_annotation_then_map_even_while_busy() {
        let mut app = app();
        open_annotation(&mut app, None, &mut None);
        open_exact_target(&mut app, None, &mut None);
        app.busy = true;
        assert!(!back_key(&mut app, KeyCode::Esc));
        assert!(app.cabinet.detail.is_some());
        assert!(app.cabinet.current.is_none());
        assert!(!back_key(&mut app, KeyCode::Esc));
        assert!(app.cabinet.detail.is_none());
        assert!(!back_key(&mut app, KeyCode::Esc));
        assert_eq!(app.view, View::Map);
        assert!(app.deferred_view_read);
    }
    #[test]
    fn annotation_responses_ignore_changed_selection_and_view() {
        for failure in [false, true] {
            let mut app = app();
            let card = app.cabinet.page.items[0].clone();
            let mut pending = Some(PendingRead::Annotation {
                generation: generation(&app),
                id: card.id.clone(),
            });
            cabinet_selection(&mut app, true);
            if failure {
                apply_response(
                    &mut app,
                    &mut pending,
                    Response::Error("stale annotation".into()),
                );
            } else {
                apply_response(
                    &mut app,
                    &mut pending,
                    Response::Touchstone(touchstones::demo_detail(&card)),
                );
            }
            assert!(app.cabinet.detail.is_none());
            assert!(app.error.is_none());
        }
    }
    #[test]
    fn leaving_cabinet_closes_inspection_but_keeps_shelf() {
        let mut app = app();
        open_annotation(&mut app, None, &mut None);
        open_exact_target(&mut app, None, &mut None);
        toggle_cabinet(&mut app, None, &mut None);
        toggle_cabinet(&mut app, None, &mut None);
        assert!(app.cabinet.loaded);
        assert!(app.cabinet.detail.is_none());
        assert!(app.cabinet.current.is_none());
        assert!(!app.show_details);
    }
    #[test]
    fn source_change_resets_cabinet_page_selection_and_exact_read() {
        let mut app = app();
        open_annotation(&mut app, None, &mut None);
        open_exact_target(&mut app, None, &mut None);
        app.targets = (0..2)
            .map(|i| Target {
                name: i.to_string(),
                database: "project".into(),
                url: "http://example.invalid".into(),
                ssh_mcp_port: 1,
                token_env: None,
                expected_db_id: None,
                expected_path: None,
            })
            .collect();
        app.cabinet.page.next = Some("old cursor".into());
        let mut pending = Some(PendingRead::Cabinet {
            generation: generation(&app),
        });
        reset_source(&mut app, &mut pending);
        assert!(app.cabinet.page.items.is_empty());
        assert!(app.cabinet.page.next.is_none());
        assert!(app.cabinet.detail.is_none());
        assert!(app.cabinet.current.is_none());
        assert!(!app.cabinet.loaded);
        assert!(pending.is_none());
    }
    #[test]
    fn initial_cabinet_reads_shelf_and_escape_lazily_reads_map() {
        let mut app = app();
        app.demo = false;
        app.cabinet.loaded = false;
        let (tx, mut rx) = mpsc::channel(1);
        let mut pending = None;
        initial_read(&mut app, &tx, &mut pending);
        assert!(matches!(
            rx.try_recv().unwrap(),
            Request::Touchstones { after: None }
        ));
        app.busy = false;
        pending = None;
        assert!(back_key(&mut app, KeyCode::Esc));
        initial_read(&mut app, &tx, &mut pending);
        assert!(matches!(rx.try_recv().unwrap(), Request::Query(_)));
    }
    #[test]
    fn canceled_unread_cabinet_does_not_enqueue_an_unsolicited_map_query() {
        for esc in [false, true] {
            let mut app = app();
            toggle_cabinet(&mut app, None, &mut None);
            app.demo = false;
            app.last_refresh = Some(Instant::now());
            app.cabinet.loaded = false;
            app.remember(app.visit());
            let (tx, mut rx) = mpsc::channel(1);
            let mut pending = None;
            assert!(focus_selected(&mut app, Some(&tx), &mut pending));
            let Request::Focus(id) = rx.try_recv().unwrap() else {
                panic!("expected focus")
            };
            toggle_cabinet(&mut app, Some(&tx), &mut pending);
            assert!(app.deferred_view_read);
            if esc {
                assert!(!back_key(&mut app, KeyCode::Esc));
            } else {
                toggle_cabinet(&mut app, Some(&tx), &mut pending);
            }
            assert!(!app.deferred_view_read);
            apply_response(
                &mut app,
                &mut pending,
                Response::Loaded(demo_neighborhood(&id)),
            );
            assert_eq!(app.view, View::Map);
            assert_eq!(app.graph.focus.as_deref(), Some(id.as_str()));
            assert_eq!(app.history.len(), 2);
            assert!(rx.try_recv().is_err());
        }
    }
    #[test]
    fn failed_initial_map_read_is_not_auto_retried_after_canceled_cabinet_entry() {
        for esc in [false, true] {
            let mut app = App::new(vec![], "memory".into(), false);
            let (tx, mut rx) = mpsc::channel(1);
            let mut pending = None;
            initial_read(&mut app, &tx, &mut pending);
            assert!(matches!(rx.try_recv().unwrap(), Request::Query(_)));
            toggle_cabinet(&mut app, Some(&tx), &mut pending);
            if esc {
                back_key(&mut app, KeyCode::Esc);
            } else {
                toggle_cabinet(&mut app, Some(&tx), &mut pending);
            }
            apply_response(
                &mut app,
                &mut pending,
                Response::Error("initial Map unavailable".into()),
            );
            assert_eq!(app.view, View::Map);
            assert!(!app.deferred_view_read);
            assert!(app.error.is_some());
            assert!(rx.try_recv().is_err());
        }
    }
    #[test]
    fn demo_cabinet_has_consistent_changed_unchanged_and_missing_targets() {
        let mut app = app();
        assert_eq!(app.cabinet.page.items.len(), 3);
        for index in 0..3 {
            app.cabinet.selected = index;
            app.cabinet.detail = None;
            open_annotation(&mut app, None, &mut None);
            let detail = app.cabinet.detail.as_ref().unwrap();
            assert_eq!(
                app.cabinet.page.items[index].reference_count,
                detail.references.len()
            );
            let snapshot = detail.references[0].summary.clone();
            open_exact_target(&mut app, None, &mut None);
            let current = app.cabinet.current.as_ref().unwrap();
            match index {
                0 => assert_ne!(current.node.as_ref().unwrap().summary, snapshot),
                1 => assert_eq!(current.node.as_ref().unwrap().summary, snapshot),
                2 => assert!(current.node.is_none()),
                _ => unreachable!(),
            }
        }
    }
    #[test]
    fn empty_terminal_page_replaces_instead_of_accumulating() {
        let mut app = app();
        let mut pending = Some(PendingRead::Cabinet {
            generation: generation(&app),
        });
        apply_response(
            &mut app,
            &mut pending,
            Response::Touchstones(TouchstonePage::default()),
        );
        assert!(app.cabinet.loaded);
        assert!(app.cabinet.page.items.is_empty());
        assert!(app.cabinet.page.next.is_none());
    }
}
