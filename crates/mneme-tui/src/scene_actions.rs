//! Scene navigation keeps a selected account's root and exact edition separate.
use super::*;
use crate::scenes::{Scene, SceneAxis, ScenePage};

pub(super) fn initial_read(
    app: &mut App,
    tx: &mpsc::Sender<Request>,
    pending: &mut Option<PendingRead>,
) {
    if app.view == View::Touchstones {
        load_cabinet(app, Some(tx), pending, None);
    } else if app.view == View::Scenes {
        load_scenes(
            app,
            Some(tx),
            pending,
            app.scene_axis,
            app.scene_query.clone(),
        );
    } else {
        initial_map(app, tx, pending);
    }
}

pub(super) fn load_scenes(
    app: &mut App,
    tx: Option<&mpsc::Sender<Request>>,
    pending: &mut Option<PendingRead>,
    axis: SceneAxis,
    cue: String,
) -> bool {
    if app.busy {
        return false;
    }
    if app.demo {
        // The occurrence index excludes unknown dates, but lexical search does
        // not: sorting a result window must not become an occurrence filter.
        let mut page = scenes::demo_scenes(if cue.trim().is_empty() {
            axis
        } else {
            SceneAxis::Recorded
        });
        page.axis = axis;
        if !cue.trim().is_empty() {
            page.items
                .retain(|scene| scene.summary.to_lowercase().contains(&cue.to_lowercase()));
            page.cue = Some(cue.clone());
            scenes::sort_scenes(&mut page.items, axis);
        }
        *pending = Some(PendingRead::Scenes { axis, cue });
        apply_scene_page(app, pending, page);
        return true;
    }
    let Some(tx) = tx else {
        return false;
    };
    let req = Request::Scenes {
        axis,
        cue: (!cue.trim().is_empty()).then(|| cue.clone()),
    };
    if request(app, tx, req) {
        *pending = Some(PendingRead::Scenes { axis, cue });
        true
    } else {
        false
    }
}

pub(super) fn switch_view(
    app: &mut App,
    tx: Option<&mpsc::Sender<Request>>,
    pending: &mut Option<PendingRead>,
) {
    if app.view == View::Touchstones {
        leave_cabinet(app, tx, pending, View::Scenes);
        return;
    }
    if app.busy {
        return;
    }
    app.close_edge_lens();
    app.show_details = false;
    app.scroll = 0;
    match app.view {
        View::Map => {
            app.view = View::Scenes;
            if !app.scenes_loaded {
                load_scenes(app, tx, pending, app.scene_axis, app.scene_query.clone());
            }
        }
        View::Touchstones => unreachable!(),
        View::Scenes => {
            app.view = View::Map;
            if app.last_refresh.is_none() && !app.demo {
                if let Some(tx) = tx {
                    initial_map(app, tx, pending);
                }
            }
        }
    }
}

pub(super) fn open_scene(
    app: &mut App,
    tx: Option<&mpsc::Sender<Request>>,
    pending: &mut Option<PendingRead>,
) {
    let Some(scene) = app.selected_scene().cloned() else {
        return;
    };
    if app.scene_detail.as_ref().is_some_and(|detail| {
        detail.episode_id == scene.episode_id
            && (app.show_details || detail.edition_id == scene.edition_id)
    }) {
        // A retained historical detail remains that exact edition, even if r
        // observed a newer head. Closing it permits an explicit new open.
        if app.show_details {
            app.scene_detail = None;
        }
        app.show_details = !app.show_details;
        app.scroll = 0;
        return;
    }
    if app.demo {
        app.scene_detail = Some(scene);
        app.show_details = true;
        app.scroll = 0;
    } else if let Some(tx) = tx {
        let episode_id = scene.episode_id;
        let edition_id = scene.edition_id;
        if request(
            app,
            tx,
            Request::Scene {
                episode_id: episode_id.clone(),
                edition_id: edition_id.clone(),
            },
        ) {
            *pending = Some(PendingRead::Scene {
                episode_id,
                edition_id,
            });
        }
    }
}

pub(super) fn apply_scene_page(app: &mut App, pending: &mut Option<PendingRead>, page: ScenePage) {
    let Some(PendingRead::Scenes { axis, cue }) = pending.take() else {
        return;
    };
    if page.axis != axis {
        app.error = Some("Scene response has a different ordering; keeping the last view".into());
        return;
    }
    let previous = app.selected_scene().map(|scene| scene.episode_id.clone());
    app.scene_selected = previous
        .as_ref()
        .and_then(|id| page.items.iter().position(|scene| &scene.episode_id == id))
        .unwrap_or_else(|| app.scene_selected.min(page.items.len().saturating_sub(1)));
    app.scenes = page;
    app.scene_axis = axis;
    app.scene_query = cue;
    app.scenes_loaded = true;
    app.scene_refreshed = Some(Instant::now());
    let has_selection = app.selected_scene().is_some();
    if let Some(detail) = app.scene_detail.as_mut() {
        // Borrow fields separately: a new head is observation metadata, not a
        // replacement for the immutable edition/body that was actually read.
        let header = app.scenes.items.get(app.scene_selected);
        if let Some(header) = header.filter(|header| header.episode_id == detail.episode_id) {
            detail.current_edition_id = header.current_edition_id.clone();
        } else {
            app.scene_detail = None;
            app.show_details = false;
        }
    } else if !has_selection {
        app.show_details = false;
    }
    app.scroll = 0;
    app.error = None;
    app.event(format!(
        "Read {} scenes · {} time",
        app.scenes.items.len(),
        axis.as_str()
    ));
}

pub(super) fn apply_scene_detail(app: &mut App, pending: &mut Option<PendingRead>, scene: Scene) {
    let Some(PendingRead::Scene {
        episode_id,
        edition_id,
    }) = pending.take()
    else {
        return;
    };
    if scene.episode_id != episode_id || scene.edition_id != edition_id {
        app.error = Some("Scene identity changed; keeping the selected account".into());
        return;
    }
    if app.view != View::Scenes
        || !app.selected_scene().is_some_and(|selected| {
            selected.episode_id == episode_id && selected.edition_id == edition_id
        })
    {
        return;
    }
    app.scene_detail = Some(scene);
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
        switch_view(&mut app, None, &mut None);
        app
    }
    #[test]
    fn demo_occurrence_search_keeps_unknown_dates_but_index_excludes_them() {
        let mut app = app();
        load_scenes(
            &mut app,
            None,
            &mut None,
            SceneAxis::Occurred,
            String::new(),
        );
        assert!(
            app.scenes
                .items
                .iter()
                .all(|scene| scene.occurred != scenes::SceneOccurrence::Unknown)
        );
        load_scenes(
            &mut app,
            None,
            &mut None,
            SceneAxis::Occurred,
            "bridge repair".into(),
        );
        assert_eq!(app.scenes.items.len(), 1);
        assert_eq!(
            app.scenes.items[0].occurred,
            scenes::SceneOccurrence::Unknown
        );
        assert_eq!(app.scenes.axis, SceneAxis::Occurred);
    }
    #[test]
    fn scene_view_switches_without_replacing_the_map() {
        let mut app = app();
        assert_eq!(app.view, View::Scenes);
        assert_eq!(app.scenes.items.len(), 12);
        let ids: Vec<_> = app.graph.nodes.iter().map(|n| n.id.clone()).collect();
        change_selection(&mut app, true);
        open_scene(&mut app, None, &mut None);
        assert!(app.show_details);
        switch_view(&mut app, None, &mut None);
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
    fn stale_detail_does_not_follow_new_selection() {
        let mut app = app();
        let scene = app.selected_scene().unwrap().clone();
        let mut pending = Some(PendingRead::Scene {
            episode_id: scene.episode_id.clone(),
            edition_id: scene.edition_id.clone(),
        });
        change_selection(&mut app, true);
        apply_response(&mut app, &mut pending, Response::Scene(scene));
        assert!(app.scene_detail.is_none());
    }
    #[test]
    fn refresh_preserves_root_selection_and_read_edition() {
        let mut app = app();
        app.scene_selected = 2;
        open_scene(&mut app, None, &mut None);
        let exact = app.scene_detail.as_ref().unwrap().edition_id.clone();
        let mut page = app.scenes.clone();
        page.items[2].edition_id = "new-edition".into();
        page.items[2].current_edition_id = "new-edition".into();
        page.items.reverse();
        let mut pending = Some(PendingRead::Scenes {
            axis: app.scene_axis,
            cue: String::new(),
        });
        apply_response(&mut app, &mut pending, Response::Scenes(page));
        assert_eq!(app.scene_selected, 9);
        assert_eq!(app.scene_detail.as_ref().unwrap().edition_id, exact);
        assert_eq!(
            app.scene_detail.as_ref().unwrap().current_edition_id,
            "new-edition"
        );
        assert!(app.show_details);
    }
    #[test]
    fn failures_keep_scene_window_and_axis() {
        let mut app = app();
        let before = app.selected_scene().unwrap().episode_id.clone();
        let mut pending = Some(PendingRead::Scenes {
            axis: SceneAxis::Occurred,
            cue: "different".into(),
        });
        apply_response(
            &mut app,
            &mut pending,
            Response::Error("unavailable".into()),
        );
        assert_eq!(app.scene_axis, SceneAxis::Recorded);
        assert_eq!(app.selected_scene().unwrap().episode_id, before);
        assert_eq!(app.scene_query, "");
    }
    #[test]
    fn exact_read_request_uses_displayed_edition() {
        let mut app = app();
        app.demo = false;
        let (tx, mut rx) = mpsc::channel(1);
        let scene = app.selected_scene().unwrap().clone();
        open_scene(&mut app, Some(&tx), &mut None);
        assert!(
            matches!(rx.try_recv().unwrap(), Request::Scene { episode_id, edition_id } if episode_id == scene.episode_id && edition_id == scene.edition_id)
        );
    }
    #[test]
    fn stale_detail_failure_does_not_poison_new_selection() {
        let mut app = app();
        let scene = app.selected_scene().unwrap().clone();
        let mut pending = Some(PendingRead::Scene {
            episode_id: scene.episode_id,
            edition_id: scene.edition_id,
        });
        app.busy = true;
        change_selection(&mut app, true);
        apply_response(
            &mut app,
            &mut pending,
            Response::Error("old edition unavailable".into()),
        );
        assert!(app.error.is_none());
        assert!(!app.busy);
        assert!(pending.is_none());
    }
}
