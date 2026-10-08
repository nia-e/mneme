//! Each page commits independently. Aggregate bursts yield to input; continuation
//! is explicit once time, bytes or call budgets are exhausted.
use super::*;
const BURST_PAGES: usize = 64;
const BURST_BYTES: usize = 8 * 1024 * 1024;
const FIELD_BYTES: usize = 16 * 1024 * 1024;
const BURST_TIME: Duration = Duration::from_secs(15);

/// Remove only warnings that this workflow just resolved, not unrelated errors
/// or the demo/source distinction. Successful reads need no new status line.
fn clear_load_warning(app: &mut App) {
    app.events.retain(|event| {
        !event.starts_with("Load paused:") && !event.starts_with("16 MiB cache limit")
    });
}

pub(super) fn initial_map(
    app: &mut App,
    tx: &mpsc::Sender<Request>,
    pending: &mut Option<PendingRead>,
) {
    if app.query.trim().is_empty() {
        load_inventory(app, tx, pending, None);
    } else {
        query_field(app, tx, pending, app.query.clone());
    }
}
fn load_inventory(
    app: &mut App,
    tx: &mpsc::Sender<Request>,
    pending: &mut Option<PendingRead>,
    after: Option<String>,
) {
    if request(
        app,
        tx,
        Request::Inventory {
            after: after.clone(),
        },
    ) {
        if let Some(state) = app.graph.inventory.as_mut() {
            state.crawling = true;
        }
        *pending = Some(PendingRead::Inventory {
            after,
            pages: 0,
            bytes: 0,
            started: Instant::now(),
            waiting: true,
        });
    }
}
pub(super) fn continue_inventory(
    app: &mut App,
    tx: &mpsc::Sender<Request>,
    pending: &mut Option<PendingRead>,
) {
    if app.busy {
        return;
    }
    let Some(state) = &app.graph.inventory else {
        load_inventory(app, tx, pending, None);
        return;
    };
    if let Some(after) = &state.next {
        if state.bytes >= FIELD_BYTES {
            app.event("16 MiB cache limit · Enter opens memory · r restarts");
            return;
        }
        load_inventory(app, tx, pending, Some(after.clone()));
    } else {
        app.graph.inventory.as_mut().unwrap().background_paused = false;
        app.events
            .retain(|event| !event.starts_with("Load paused:"));
    }
}
pub(super) fn advance_inventory(
    app: &mut App,
    tx: &mpsc::Sender<Request>,
    pending: &mut Option<PendingRead>,
) {
    if app.busy || app.view != View::Map {
        return;
    }
    let req = match pending.as_ref() {
        Some(PendingRead::Inventory {
            after,
            waiting: false,
            ..
        }) => Request::Inventory {
            after: after.clone(),
        },
        _ => return,
    };
    if request(app, tx, req) {
        match pending.as_mut() {
            Some(PendingRead::Inventory { waiting, .. }) => *waiting = true,
            _ => {}
        }
    }
}
pub(super) fn apply_inventory(
    app: &mut App,
    pending: &mut Option<PendingRead>,
    page: InventoryPage,
) {
    let Some(PendingRead::Inventory {
        after,
        pages,
        bytes,
        started,
        ..
    }) = pending.take()
    else {
        return;
    };
    if let Some(state) = app.graph.inventory.as_mut() {
        state.crawling = false;
    }
    let graph = page.graph;
    let Some(state) = graph.inventory.as_ref() else {
        return;
    };
    if after.is_none() {
        // Do not clear a retained field until a valid first page arrived.
        app.close_edge_lens();
        app.layout.borrow_mut().reset();
        app.selected = 0;
        app.graph = graph;
        app.query.clear();
        app.history.clear();
    } else {
        let Some(previous) = app.graph.inventory.as_ref() else {
            return;
        };
        if previous.db_id != state.db_id || previous.next != after {
            app.error =
                Some("Inventory cursor changed · loaded memories kept · r refreshes".into());
            return;
        }
        // Canonical pages may change between reads but must not loop backwards.
        if let (Some(last), Some(first)) = (app.graph.nodes.last(), graph.nodes.first()) {
            if first.id <= last.id {
                app.error = Some(
                    "Inventory cursor moved backwards · loaded memories kept · r refreshes".into(),
                );
                return;
            }
        }
        if previous.bytes.saturating_add(page.bytes) > FIELD_BYTES {
            app.event("16 MiB cache limit · Enter opens memory · r restarts");
            return;
        }
        let bytes = previous.bytes + page.bytes;
        app.graph.nodes.extend(graph.nodes);
        // Native cursor order already gives each stored edge once. Do not add
        // an O(E²) deduplication pass to every lightweight topology page.
        app.graph.edges.extend(graph.edges);
        app.graph.partial = graph.partial;
        let current = app.graph.inventory.as_mut().unwrap();
        current.next = state.next.clone();
        current.complete = state.complete;
        current.bytes = bytes;
        current.topology_edges_complete = state.topology_edges_complete;
        current.background_paused = false;
    }
    clear_load_warning(app);
    if after.is_none() {
        app.events
            .retain(|event| !event.contains(" summaries unavailable · r refreshes"));
    }
    app.last_refresh = Some(Instant::now());
    app.deferred_view_read = false;
    app.error = None;
    let state = app.graph.inventory.as_ref().unwrap();
    let more = state.next.clone();
    let field_bytes = state.bytes;
    let pages = pages + 1;
    let bytes = bytes + page.bytes;
    app.graph.note = if more.is_some() {
        "Topology incomplete · n continues".into()
    } else {
        String::new()
    };
    if more.is_some()
        && pages < BURST_PAGES
        && bytes < BURST_BYTES
        && started.elapsed() < BURST_TIME
        && field_bytes < FIELD_BYTES
    {
        app.graph.inventory.as_mut().unwrap().crawling = true;
        *pending = Some(PendingRead::Inventory {
            after: more,
            pages,
            bytes,
            started,
            waiting: false,
        });
    } else {
        app.graph.inventory.as_mut().unwrap().crawling = false;
        if more.is_some() {
            app.event("Load paused: call/time/byte budget · n continues");
        }
    }
}

/// Select viewport + one-screen prefetch margin in one batched call. The cache
/// records attempts before dispatch, so missing/error replies cannot spin retry
/// loops. A refresh starts a new independent view and deliberately resets cache.
pub(super) fn hydrate_viewport(
    app: &mut App,
    tx: &mpsc::Sender<Request>,
    pending: &mut Option<PendingRead>,
) {
    if app.busy || pending.is_some() || app.view != View::Map || app.editing || app.show_help {
        return;
    }
    let Some(state) = app.graph.inventory.as_ref() else {
        return;
    };
    if state.bytes >= FIELD_BYTES || state.background_paused {
        return;
    }
    let mut ids = render::near_viewport_ids(app, 1);
    ids.retain(|id| {
        !state.summary_attempted.contains(id) && app.graph.nodes.iter().any(|node| &node.id == id)
    });
    ids.truncate(64);
    if ids.is_empty() {
        return;
    }
    if request(app, tx, Request::Summaries { ids: ids.clone() }) {
        app.graph
            .inventory
            .as_mut()
            .unwrap()
            .summary_attempted
            .extend(ids.iter().cloned());
        *pending = Some(PendingRead::Summaries { ids });
    }
}
/// Cancellation is not a failure or a delivered cache entry. Remove the exact
/// canceled batch's attempted IDs and pause background work until n or refresh.
pub(super) fn pause_background(app: &mut App, pending: &mut Option<PendingRead>) -> bool {
    if !matches!(
        pending,
        Some(PendingRead::Inventory { .. } | PendingRead::Summaries { .. })
    ) {
        return false;
    }
    if let Some(state) = app.graph.inventory.as_mut() {
        if let Some(PendingRead::Summaries { ids }) = pending.as_ref() {
            for id in ids {
                state.summary_attempted.remove(id);
            }
        }
        state.background_paused = true;
        state.crawling = false;
    }
    *pending = None;
    app.busy = false;
    true
}
pub(super) fn apply_summaries(
    app: &mut App,
    pending: &mut Option<PendingRead>,
    page: InventorySummaries,
) {
    let Some(PendingRead::Summaries { ids }) = pending.take() else {
        return;
    };
    let Some(state) = app.graph.inventory.as_mut() else {
        return;
    };
    if page.bytes.saturating_add(state.bytes) > FIELD_BYTES {
        app.event("16 MiB cache limit · Enter opens memory · r restarts");
        return;
    }
    if page.nodes.iter().any(|node| !ids.contains(&node.id))
        || page.missing.iter().any(|id| !ids.contains(id))
    {
        app.error = Some("Summary IDs changed · topology kept · r refreshes".into());
        return;
    }
    state.bytes += page.bytes;
    for fresh in page.nodes {
        if let Some(card) = app.graph.nodes.iter_mut().find(|node| node.id == fresh.id) {
            state.hydrated.insert(fresh.id.clone());
            *card = fresh;
        }
    }
    state.summaries_loaded = state.hydrated.len();
    if !page.missing.is_empty() {
        app.event(format!(
            "{} summaries unavailable · r refreshes",
            page.missing.len()
        ));
    }
    app.error = None;
}

#[cfg(test)]
mod tests {
    use super::*;
    fn page(start: usize, count: usize, next: Option<&str>) -> InventoryPage {
        InventoryPage {
            bytes: count * 100 + 10,
            graph: Graph {
                nodes: (start..start + count)
                    .map(|n| Node {
                        id: format!("{n:026}"),
                        summary: "native".into(),
                        ..Default::default()
                    })
                    .collect(),
                inventory: Some(InventoryState {
                    db_id: "owner".into(),
                    next: next.map(str::to_owned),
                    complete: next.is_none(),
                    ..Default::default()
                }),
                ..Default::default()
            },
        }
    }
    fn intent(after: Option<&str>) -> Option<PendingRead> {
        Some(PendingRead::Inventory {
            after: after.map(str::to_owned),
            pages: 0,
            bytes: 0,
            started: Instant::now(),
            waiting: true,
        })
    }
    #[test]
    fn completed_reads_are_quiet_and_keep_coverage_state() {
        let mut app = App::new(vec![], String::new(), false);
        apply_inventory(&mut app, &mut intent(None), page(0, 128, None));
        assert!(app.events.is_empty());
        assert!(app.graph.note.is_empty());
        assert!(app.graph.inventory.as_ref().unwrap().complete);
        let (tx, _rx) = mpsc::channel(1);
        continue_inventory(&mut app, &tx, &mut None);
        assert!(app.events.is_empty());
    }
    #[test]
    fn incomplete_and_budget_paused_reads_keep_actionable_status() {
        let mut app = App::new(vec![], String::new(), false);
        let mut pending = Some(PendingRead::Inventory {
            after: None,
            pages: 63,
            bytes: 0,
            started: Instant::now(),
            waiting: true,
        });
        apply_inventory(&mut app, &mut pending, page(0, 64, Some("one")));
        assert_eq!(app.graph.note, "Topology incomplete · n continues");
        assert_eq!(
            app.events.front().map(String::as_str),
            Some("Load paused: call/time/byte budget · n continues")
        );
        app.event("Edges unavailable");
        apply_inventory(&mut app, &mut intent(Some("one")), page(64, 64, None));
        assert_eq!(
            app.events.iter().map(String::as_str).collect::<Vec<_>>(),
            vec!["Edges unavailable"]
        );
        assert!(app.graph.note.is_empty());
    }
    #[test]
    fn native_pages_append_128_and_failed_continuation_keeps_field() {
        let mut app = App::new(vec![], String::new(), false);
        apply_response(
            &mut app,
            &mut intent(None),
            Response::Inventory(page(0, 64, Some("one"))),
        );
        apply_response(
            &mut app,
            &mut intent(Some("one")),
            Response::Inventory(page(64, 64, Some("two"))),
        );
        assert_eq!(app.graph.nodes.len(), 128);
        apply_response(
            &mut app,
            &mut intent(Some("two")),
            Response::Error("gone".into()),
        );
        assert_eq!(app.graph.nodes.len(), 128);
        assert_eq!(
            app.graph.inventory.as_ref().unwrap().next.as_deref(),
            Some("two")
        );
    }
    #[test]
    fn empty_page_advances_and_wrong_scope_or_overlapping_page_preserves_field() {
        let mut app = App::new(vec![], String::new(), false);
        apply_inventory(&mut app, &mut intent(None), page(0, 64, Some("one")));
        apply_inventory(&mut app, &mut intent(Some("one")), page(64, 0, Some("two")));
        assert_eq!(
            app.graph.inventory.as_ref().unwrap().next.as_deref(),
            Some("two")
        );
        let mut wrong = page(64, 1, None);
        wrong.graph.inventory.as_mut().unwrap().db_id = "other".into();
        apply_inventory(&mut app, &mut intent(Some("two")), wrong);
        assert_eq!(app.graph.nodes.len(), 64);
        apply_inventory(&mut app, &mut intent(Some("two")), page(63, 2, None));
        assert_eq!(app.graph.nodes.len(), 64);
    }
    #[test]
    fn hydration_updates_only_matching_cards_and_missing_keeps_topology() {
        let mut app = App::new(vec![], String::new(), false);
        apply_inventory(&mut app, &mut intent(None), page(0, 128, None));
        app.graph.edges.push(Edge {
            from: format!("{:026}", 0),
            to: format!("{:026}", 1),
            kind: "derived_from".into(),
            weight: 0.5,
        });
        let ids = vec![format!("{:026}", 0), format!("{:026}", 1)];
        let fresh = Node {
            id: ids[0].clone(),
            summary: String::new(),
            status: "archived".into(),
            ..Default::default()
        };
        apply_summaries(
            &mut app,
            &mut Some(PendingRead::Summaries { ids: ids.clone() }),
            InventorySummaries {
                nodes: vec![fresh],
                missing: vec![ids[1].clone()],
                bytes: 100,
            },
        );
        assert_eq!(app.graph.nodes.len(), 128);
        assert_eq!(app.graph.edges.len(), 1);
        let state = app.graph.inventory.as_ref().unwrap();
        assert!(state.hydrated.contains(&ids[0]));
        assert!(!state.hydrated.contains(&ids[1]));
        assert_eq!(state.summaries_loaded, 1);
        assert_eq!(app.graph.nodes[0].status, "archived");
    }
    #[test]
    fn later_summary_tag_edits_and_uncertainty_replace_census_evidence_without_reflow() {
        let mut app = App::new(vec![], String::new(), false);
        apply_inventory(&mut app, &mut intent(None), page(0, 3, None));
        for (i, node) in app.graph.nodes.iter_mut().enumerate() {
            node.tags_complete = true;
            if i < 2 {
                node.tags = vec!["directories".into()];
            }
        }
        let first = app.graph.nodes[0].id.clone();
        let outside = app.graph.nodes[2].id.clone();
        app.graph.edges.push(Edge {
            from: first.clone(),
            to: app.graph.nodes[1].id.clone(),
            kind: "associative".into(),
            weight: 0.9,
        });
        let before = app.layout.borrow_mut().positions(&app.graph, 90, 24);
        assert!(
            app.layout
                .borrow_mut()
                .groups(&app.graph)
                .group_for(&first)
                .unwrap()
                .label
                .is_some()
        );
        for (tags, complete, named) in [
            (vec!["directories".into()], true, false), // Authored edit makes it ubiquitous.
            (vec![], false, false), // New truncation cannot preserve stale known absence.
            (vec![], true, true),   // A complete later read establishes absence again.
        ] {
            apply_summaries(
                &mut app,
                &mut Some(PendingRead::Summaries {
                    ids: vec![outside.clone()],
                }),
                InventorySummaries {
                    nodes: vec![Node {
                        id: outside.clone(),
                        tags,
                        tags_complete: complete,
                        ..Default::default()
                    }],
                    bytes: 100,
                    ..Default::default()
                },
            );
            let groups = app.layout.borrow_mut().groups(&app.graph);
            assert_eq!(groups.group_for(&first).unwrap().label.is_some(), named);
            assert_eq!(
                app.layout.borrow_mut().positions(&app.graph, 90, 24),
                before
            );
        }
    }
    #[test]
    fn viewport_batches_do_not_repeat_failed_or_successful_attempts() {
        let mut app = App::new(vec![], String::new(), false);
        apply_inventory(&mut app, &mut intent(None), page(0, 128, None));
        let (tx, mut rx) = mpsc::channel(1);
        let mut pending = None;
        hydrate_viewport(&mut app, &tx, &mut pending);
        let Request::Summaries { ids } = rx.try_recv().unwrap() else {
            panic!("expected viewport batch")
        };
        assert!(!ids.is_empty() && ids.len() <= 64);
        apply_response(
            &mut app,
            &mut pending,
            Response::Error("failed once".into()),
        );
        hydrate_viewport(&mut app, &tx, &mut pending);
        if let Ok(Request::Summaries { ids: next }) = rx.try_recv() {
            assert!(next.iter().all(|id| !ids.contains(id)));
        }
        assert_eq!(app.graph.nodes.len(), 128);
    }
    #[test]
    fn user_open_preempts_crawl_without_losing_pages_or_selection() {
        let mut app = App::new(vec![], String::new(), false);
        apply_inventory(&mut app, &mut intent(None), page(0, 128, Some("more")));
        app.selected = 17;
        app.busy = true;
        let mut pending = intent(Some("more"));
        assert!(pause_background(&mut app, &mut pending));
        let (tx, mut rx) = mpsc::channel(1);
        assert!(focus_selected(&mut app, Some(&tx), &mut pending));
        assert!(matches!(rx.try_recv().unwrap(),Request::Focus(id) if id==format!("{:026}",17)));
        assert_eq!(app.graph.nodes.len(), 128);
        assert_eq!(
            app.graph.inventory.as_ref().unwrap().next.as_deref(),
            Some("more")
        );
    }
    #[test]
    fn canceled_summary_batch_can_resume_but_never_spins_while_paused() {
        let mut app = App::new(vec![], String::new(), false);
        apply_inventory(&mut app, &mut intent(None), page(0, 128, None));
        let (tx, mut rx) = mpsc::channel(1);
        let mut pending = None;
        hydrate_viewport(&mut app, &tx, &mut pending);
        let Request::Summaries { ids } = rx.try_recv().unwrap() else {
            panic!("summary request")
        };
        assert!(pause_background(&mut app, &mut pending));
        let state = app.graph.inventory.as_ref().unwrap();
        assert!(ids.iter().all(|id| !state.summary_attempted.contains(id)));
        hydrate_viewport(&mut app, &tx, &mut pending);
        assert!(rx.try_recv().is_err());
        continue_inventory(&mut app, &tx, &mut pending);
        hydrate_viewport(&mut app, &tx, &mut pending);
        assert!(matches!(rx.try_recv().unwrap(),Request::Summaries {ids:again} if again==ids));
    }
    #[test]
    fn initial_map_uses_inventory_search_remains_explicit_and_bursts_stop() {
        let mut app = App::new(vec![], String::new(), false);
        let (tx, mut rx) = mpsc::channel(1);
        let mut pending = None;
        initial_map(&mut app, &tx, &mut pending);
        assert!(matches!(
            rx.try_recv().unwrap(),
            Request::Inventory { after: None }
        ));
        app.busy = false;
        app.query = "cue".into();
        initial_map(&mut app, &tx, &mut pending);
        assert!(matches!(rx.try_recv().unwrap(),Request::Query(q) if q=="cue"));
        pending = Some(PendingRead::Inventory {
            after: None,
            pages: 63,
            bytes: 0,
            started: Instant::now(),
            waiting: true,
        });
        apply_inventory(&mut app, &mut pending, page(0, 64, Some("one")));
        assert!(pending.is_none());
    }
}
