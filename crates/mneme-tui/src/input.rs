//! Map input uses the settled world, rendered hitboxes and small marker halos.
use crate::{
    model::{App, View},
    render::map::world_stars,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

pub(crate) fn map_key(app: &mut App, key: KeyEvent) -> bool {
    if app.view != View::Map || app.editing || app.show_help {
        return false;
    }
    let direction = match key.code {
        KeyCode::Left => Some((-1.0, 0.0)),
        KeyCode::Right => Some((1.0, 0.0)),
        KeyCode::Up | KeyCode::Char('k') => Some((0.0, 1.0)),
        KeyCode::Down | KeyCode::Char('j') => Some((0.0, -1.0)),
        KeyCode::Char('h') => {
            app.toggle_archived();
            return true;
        }
        KeyCode::Char(',') => {
            app.cycle_nodes(false);
            return true;
        }
        KeyCode::Char('.') => {
            app.cycle_nodes(true);
            return true;
        }
        _ => None,
    };
    let Some((dx, dy)) = direction else {
        return false;
    };
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        screen_order(app, dx != 0.0, dx > 0.0 || dy < 0.0);
    } else if key.modifiers.contains(KeyModifiers::SHIFT) {
        app.layout.borrow_mut().pan(dx * 12.0, dy * 8.0);
    } else if app.layout.borrow().camera.index_only && !app.layout.borrow().has_world() {
        // While the initial topology is still loading, these are index keys,
        // not permission to freeze an incidental nodes-before-edges layout.
        app.cycle_nodes(dx > 0.0 || dy < 0.0);
    } else {
        spatial(app, dx, dy);
    }
    true
}

fn spatial(app: &mut App, dx: f64, dy: f64) {
    if app.selected_node().is_none() {
        return;
    }
    let stars = world_stars(app);
    let Some(origin) = stars.iter().find(|star| star.index == app.selected) else {
        return;
    };
    let nodes = app.scene_nodes();
    let id = nodes[app.selected].id.as_str();
    let neighbors: std::collections::HashSet<&str> = app
        .scene_edges()
        .iter()
        .filter_map(|edge| {
            if edge.from == id {
                Some(edge.to.as_str())
            } else if edge.to == id {
                Some(edge.from.as_str())
            } else {
                None
            }
        })
        .collect();
    let choice = stars
        .iter()
        .filter(|star| star.index != app.selected && app.node_selectable(nodes[star.index]))
        .filter_map(|star| {
            let (x, y) = (star.x - origin.x, star.y - origin.y);
            let forward = x * dx + y * dy;
            if forward <= 0.001 {
                return None;
            }
            let sideways = (x * dy - y * dx).abs();
            let linked = neighbors.contains(nodes[star.index].id.as_str());
            Some((
                !linked,
                forward.hypot(sideways) + sideways * 2.0,
                star.index,
            ))
        })
        .min_by(|a, b| a.0.cmp(&b.0).then(a.1.total_cmp(&b.1)).then(a.2.cmp(&b.2)))
        .map(|choice| choice.2);
    if let Some(index) = choice {
        app.select_node(index);
    }
}

/// Exhaust the visible column/row order; edges have no say. Wrap makes every
/// rendered node reachable even when several share one screen coordinate.
fn screen_order(app: &mut App, horizontal: bool, forward: bool) {
    let nodes = app.scene_nodes();
    let mut visible = app.layout.borrow().camera.markers.clone();
    let hidden = app.hidden_node_ids();
    visible.retain(|(_, id)| !hidden.contains(id.as_str()));
    visible.sort_by(|(a, aid), (b, bid)| {
        let akey = if horizontal { (a.x, a.y) } else { (a.y, a.x) };
        let bkey = if horizontal { (b.x, b.y) } else { (b.y, b.x) };
        akey.cmp(&bkey).then(aid.cmp(bid))
    });
    visible.dedup_by(|a, b| a.1 == b.1);
    if visible.is_empty() {
        return;
    }
    let current = nodes.get(app.selected).map(|node| node.id.as_str());
    let index = visible
        .iter()
        .position(|(_, id)| Some(id.as_str()) == current);
    let next = index.map_or(if forward { 0 } else { visible.len() - 1 }, |i| {
        (i + if forward { 1 } else { visible.len() - 1 }) % visible.len()
    });
    let selected = nodes.iter().position(|node| node.id == visible[next].1);
    if let Some(index) = selected {
        app.select_node(index);
    }
}

/// Exact rendered targets win; nearby sky clicks then choose the closest
/// visible marker. Rows are about twice as tall as columns, hence dy² × 4.
fn hit_node(camera: &crate::layout::Camera, column: u16, row: u16) -> Option<String> {
    let distance = |x: u16, y: u16| {
        let dx = (i64::from(column) - i64::from(x)).unsigned_abs();
        let dy = (i64::from(row) - i64::from(y)).unsigned_abs();
        dx * dx + 4 * dy * dy
    };
    let markers: std::collections::HashMap<_, _> = camera
        .markers
        .iter()
        .filter(|(rect, _)| {
            camera
                .map_area
                .is_some_and(|area| area.contains((rect.x, rect.y).into()))
        })
        .map(|(rect, id)| (id.as_str(), (rect.x, rect.y)))
        .collect();
    if let Some((_, id)) = camera
        .hits
        .iter()
        .filter(|(rect, _)| rect.contains((column, row).into()))
        .map(|(_, id)| {
            (
                markers
                    .get(id.as_str())
                    .map_or(u64::MAX, |&(x, y)| distance(x, y)),
                id,
            )
        })
        .min()
    {
        return Some(id.clone());
    }
    if !camera
        .map_area
        .is_some_and(|area| area.contains((column, row).into()))
    {
        return None;
    }
    markers
        .into_iter()
        .filter(|(_, (x, y))| column.abs_diff(*x) <= 2 && row.abs_diff(*y) <= 1)
        .map(|(id, (x, y))| (distance(x, y), id))
        .min()
        .map(|(_, id)| id.to_owned())
}

pub(crate) fn mouse(app: &mut App, event: MouseEvent) {
    if crate::detail_scroll::mouse(app, event, 1) {
        return;
    }
    if app.view != View::Map || app.editing || app.show_help {
        app.layout.borrow_mut().cancel_drag();
        return;
    }
    match event.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            app.layout.borrow_mut().start_drag(event.column, event.row);
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            app.layout.borrow_mut().drag_to(event.column, event.row);
            return;
        }
        MouseEventKind::Up(MouseButton::Left) => {
            app.layout.borrow_mut().cancel_drag();
            return;
        }
        _ => return,
    }
    let id = hit_node(&app.layout.borrow().camera, event.column, event.row);
    let Some(id) = id else {
        return;
    };
    let index = app.scene_nodes().iter().position(|node| node.id == id);
    if let Some(index) = index {
        app.select_node(index);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        model::{Edge, Graph, LensState, Node},
        render,
    };
    use ratatui::{Terminal, backend::TestBackend, layout::Rect};

    fn app(count: usize) -> App {
        let mut app = App::new(vec![], String::new(), true);
        app.graph = Graph {
            nodes: (0..count)
                .map(|i| Node {
                    id: format!("node-{i:03}"),
                    summary: format!("Memory {i}"),
                    ..Node::default()
                })
                .collect(),
            ..Graph::default()
        };
        app
    }
    fn draw(app: &App, w: u16, h: u16) {
        Terminal::new(TestBackend::new(w, h))
            .unwrap()
            .draw(|frame| render::draw(frame, app))
            .unwrap();
    }
    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }
    fn pointer(app: &mut App, kind: MouseEventKind, column: u16, row: u16) {
        mouse(
            app,
            MouseEvent {
                kind,
                column,
                row,
                modifiers: KeyModifiers::NONE,
            },
        );
    }

    fn empty_map_cell(app: &App) -> (u16, u16) {
        let layout = app.layout.borrow();
        let area = layout.camera.map_area.unwrap();
        (area.y..area.bottom())
            .flat_map(|y| (area.x..area.right()).map(move |x| (x, y)))
            .find(|&(x, y)| hit_node(&layout.camera, x, y).is_none())
            .unwrap()
    }

    fn click_camera(markers: &[(u16, u16, &str)]) -> crate::layout::Camera {
        let mut camera = crate::layout::Camera::default();
        camera.map_area = Some(Rect::new(10, 10, 20, 10));
        camera.markers = markers
            .iter()
            .map(|&(x, y, id)| (Rect::new(x, y, 1, 1), id.to_owned()))
            .collect();
        camera.hits = camera.markers.clone();
        camera
    }

    #[test]
    fn nearby_sky_clicks_choose_the_closest_marker_with_cell_aspect_and_stable_ties() {
        let mut camera = click_camera(&[(14, 14, "a"), (18, 14, "b")]);
        assert_eq!(hit_node(&camera, 16, 14).as_deref(), Some("a"));
        assert_eq!(hit_node(&camera, 17, 14).as_deref(), Some("b"));
        camera.markers.reverse();
        camera.hits.reverse();
        assert_eq!(hit_node(&camera, 16, 14).as_deref(), Some("a"));
        let mut overlap = click_camera(&[(14, 14, "b"), (14, 14, "a")]);
        assert_eq!(hit_node(&overlap, 14, 14).as_deref(), Some("a"));
        overlap.hits.reverse();
        assert_eq!(hit_node(&overlap, 14, 14).as_deref(), Some("a"));
        // One row and two columns are the same visual distance. The nearby
        // diagonal remains eligible, but the closer horizontal node wins.
        let camera = click_camera(&[(14, 14, "a"), (16, 13, "b")]);
        assert_eq!(hit_node(&camera, 16, 14).as_deref(), Some("a"));
        assert_eq!(hit_node(&camera, 15, 14).as_deref(), Some("a"));
        let camera = click_camera(&[(14, 14, "a")]);
        assert_eq!(hit_node(&camera, 16, 15).as_deref(), Some("a"));
        assert!(
            hit_node(&camera, 17, 14).is_none(),
            "three columns is too far"
        );
        assert!(hit_node(&camera, 14, 16).is_none(), "two rows is too far");
    }

    #[test]
    fn exact_labels_and_index_rows_win_over_halos_and_exact_ties_use_marker_proximity() {
        let mut camera = click_camera(&[(14, 14, "a"), (17, 14, "b")]);
        camera.hits.push((Rect::new(16, 14, 2, 1), "a".into()));
        assert_eq!(
            hit_node(&camera, 16, 14).as_deref(),
            Some("a"),
            "an exact label wins over b's halo"
        );
        assert_eq!(
            hit_node(&camera, 17, 14).as_deref(),
            Some("b"),
            "an exact marker wins an overlapping label by proximity"
        );
        camera.hits.push((Rect::new(16, 14, 2, 1), "b".into()));
        assert_eq!(hit_node(&camera, 16, 14).as_deref(), Some("b"));
        camera.hits.reverse();
        assert_eq!(
            hit_node(&camera, 16, 14).as_deref(),
            Some("b"),
            "paint order cannot choose exact-hit ties"
        );
        camera.hits.push((Rect::new(32, 11, 10, 1), "a".into()));
        assert_eq!(hit_node(&camera, 40, 11).as_deref(), Some("a"));
        assert!(
            hit_node(&camera, 42, 11).is_none(),
            "index rows have no halo"
        );
        camera.map_area = None;
        assert_eq!(hit_node(&camera, 40, 11).as_deref(), Some("a"));
        assert!(
            hit_node(&camera, 15, 15).is_none(),
            "compact layouts have no sky halo"
        );
    }

    #[test]
    fn sky_halos_never_leak_outside_the_viewport_or_choose_offscreen_markers() {
        let camera = click_camera(&[(29, 11, "edge"), (9, 12, "offscreen"), (12, 12, "visible")]);
        assert!(
            hit_node(&camera, 30, 11).is_none(),
            "inspector space cannot borrow the edge node's halo"
        );
        assert_eq!(hit_node(&camera, 10, 12).as_deref(), Some("visible"));
        let camera = click_camera(&[(9, 12, "offscreen")]);
        assert!(hit_node(&camera, 10, 12).is_none());
        assert!(hit_node(&camera, u16::MAX, u16::MAX).is_none());
    }

    #[test]
    fn forgiving_click_still_starts_a_drag_and_selects_only_a_rendered_marker() {
        let mut app = app(240);
        draw(&app, 120, 36);
        let layout = app.layout.borrow().clone();
        let area = layout.camera.map_area.unwrap();
        let (point, id) = layout
            .camera
            .markers
            .iter()
            .filter(|(_, id)| id != &app.graph.nodes[0].id)
            .find_map(|(marker, id)| {
                let point = (marker.x + 1, marker.y);
                (area.contains(point.into())
                    && !layout
                        .camera
                        .hits
                        .iter()
                        .any(|(rect, _)| rect.contains(point.into()))
                    && layout.camera.markers.iter().all(|(other, other_id)| {
                        other_id == id
                            || point.0.abs_diff(other.x).pow(2)
                                + 4 * point.1.abs_diff(other.y).pow(2)
                                > 1
                    }))
                .then(|| (point, id.clone()))
            })
            .unwrap();
        let center = layout.camera.center.unwrap();
        pointer(
            &mut app,
            MouseEventKind::Down(MouseButton::Left),
            point.0,
            point.1,
        );
        assert_eq!(app.selected_node().unwrap().id, id);
        draw(&app, 120, 36);
        assert_eq!(app.layout.borrow().camera.center, Some(center));
        pointer(
            &mut app,
            MouseEventKind::Drag(MouseButton::Left),
            point.0 + 3,
            point.1 + 2,
        );
        assert_eq!(
            app.layout.borrow().camera.center,
            Some(crate::layout::Point {
                x: center.x - 3.0,
                y: center.y + 4.0
            })
        );
    }

    #[test]
    fn left_drag_grabs_the_world_from_markers_or_empty_sky_without_selection_snap() {
        for on_marker in [false, true] {
            let mut app = app(240);
            draw(&app, 120, 36);
            let before = world_stars(&app);
            let center = app.layout.borrow().camera.center.unwrap();
            let (start, expected) = if on_marker {
                let (rect, id) = app
                    .layout
                    .borrow()
                    .camera
                    .markers
                    .iter()
                    .find(|(_, id)| id != &app.graph.nodes[0].id)
                    .unwrap()
                    .clone();
                (
                    (rect.x, rect.y),
                    app.graph
                        .nodes
                        .iter()
                        .position(|node| node.id == id)
                        .unwrap(),
                )
            } else {
                (empty_map_cell(&app), 0)
            };
            pointer(
                &mut app,
                MouseEventKind::Down(MouseButton::Left),
                start.0,
                start.1,
            );
            assert_eq!(app.selected, expected);
            draw(&app, 120, 36);
            assert_eq!(
                app.layout.borrow().camera.center,
                Some(center),
                "press must not snap the camera"
            );
            pointer(
                &mut app,
                MouseEventKind::Drag(MouseButton::Left),
                start.0,
                start.1,
            );
            assert_eq!(app.layout.borrow().camera.center, Some(center));
            pointer(
                &mut app,
                MouseEventKind::Drag(MouseButton::Left),
                start.0 + 3,
                start.1 + 2,
            );
            let moved = crate::layout::Point {
                x: center.x - 3.0,
                y: center.y + 4.0,
            };
            assert_eq!(app.layout.borrow().camera.center, Some(moved));
            assert!(app.layout.borrow().camera.hits.is_empty());
            draw(&app, 120, 36);
            assert_eq!(app.layout.borrow().camera.center, Some(moved));
            assert_eq!(app.selected, expected);
            assert!(
                before
                    .iter()
                    .zip(world_stars(&app))
                    .all(|(a, b)| (a.index, a.x, a.y) == (b.index, b.x, b.y))
            );
            pointer(
                &mut app,
                MouseEventKind::Up(MouseButton::Left),
                start.0 + 3,
                start.1 + 2,
            );
            pointer(&mut app, MouseEventKind::Drag(MouseButton::Left), 0, 0);
            assert_eq!(
                app.layout.borrow().camera.center,
                Some(moved),
                "release ends the gesture"
            );
        }
    }

    #[test]
    fn drags_started_in_inspector_index_or_compact_layout_do_not_pan() {
        let mut app = app(240);
        draw(&app, 120, 36);
        let sky = app.layout.borrow().camera.map_area.unwrap();
        let row = app
            .layout
            .borrow()
            .camera
            .hits
            .iter()
            .find(|(rect, _)| rect.width > 4)
            .unwrap()
            .0;
        for start in [(sky.right() + 1, sky.y + 1), (row.x, row.y)] {
            pointer(
                &mut app,
                MouseEventKind::Down(MouseButton::Left),
                start.0,
                start.1,
            );
            draw(&app, 120, 36);
            let center = app.layout.borrow().camera.center;
            pointer(
                &mut app,
                MouseEventKind::Drag(MouseButton::Left),
                sky.x,
                sky.y,
            );
            assert_eq!(app.layout.borrow().camera.center, center);
        }
        draw(&app, 40, 24);
        assert!(app.layout.borrow().camera.map_area.is_none());
        let row = app.layout.borrow().camera.hits[0].0;
        pointer(
            &mut app,
            MouseEventKind::Down(MouseButton::Left),
            row.x,
            row.y,
        );
        let center = app.layout.borrow().camera.center;
        pointer(&mut app, MouseEventKind::Drag(MouseButton::Left), 0, 0);
        assert_eq!(app.layout.borrow().camera.center, center);
    }

    #[test]
    fn hidden_overlay_resized_or_reset_maps_cancel_old_drag_gestures() {
        for mode in 0..6 {
            let mut app = app(240);
            draw(&app, 120, 36);
            let start = empty_map_cell(&app);
            pointer(
                &mut app,
                MouseEventKind::Down(MouseButton::Left),
                start.0,
                start.1,
            );
            match mode {
                0 => {
                    app.view = View::Scenes;
                    draw(&app, 120, 36);
                    app.view = View::Map;
                }
                1 => {
                    app.show_help = true;
                    draw(&app, 120, 36);
                    app.show_help = false;
                }
                2 => {
                    app.editing = true;
                    draw(&app, 120, 36);
                    app.editing = false;
                }
                3 => draw(&app, 76, 30),
                4 => draw(&app, 20, 8),
                _ => app.layout.borrow_mut().reset(),
            }
            draw(&app, 120, 36);
            let center = app.layout.borrow().camera.center;
            pointer(
                &mut app,
                MouseEventKind::Drag(MouseButton::Left),
                start.0 + 4,
                start.1 + 3,
            );
            assert_eq!(
                app.layout.borrow().camera.center,
                center,
                "mode {mode} must cancel old gestures"
            );
        }
    }

    #[test]
    fn drag_coordinates_widen_before_subtraction_and_can_leave_the_viewport() {
        let mut app = app(240);
        draw(&app, 120, 36);
        let start = empty_map_cell(&app);
        let center = app.layout.borrow().camera.center.unwrap();
        pointer(
            &mut app,
            MouseEventKind::Down(MouseButton::Left),
            start.0,
            start.1,
        );
        pointer(
            &mut app,
            MouseEventKind::Drag(MouseButton::Left),
            u16::MAX,
            u16::MAX,
        );
        pointer(&mut app, MouseEventKind::Drag(MouseButton::Left), 0, 0);
        assert_eq!(
            app.layout.borrow().camera.center,
            Some(crate::layout::Point {
                x: center.x + f64::from(start.0),
                y: center.y - 2.0 * f64::from(start.1),
            })
        );
    }

    #[test]
    fn archive_toggle_reaches_unknowns_and_never_reflows_or_drops_loaded_nodes() {
        let mut app = app(8);
        for i in [0, 2, 6] {
            app.graph.nodes[i].status = "archived".into();
        }
        app.graph.nodes[1].status = "active".into();
        let before = world_stars(&app);
        assert!(!app.hide_archived);
        map_key(&mut app, key(KeyCode::Char('h'), KeyModifiers::NONE));
        assert_eq!(app.selected, 1);
        assert!(app.hide_archived);
        let mut reached = std::collections::BTreeSet::new();
        for _ in 0..16 {
            map_key(&mut app, key(KeyCode::Char('.'), KeyModifiers::NONE));
            reached.insert(app.selected);
        }
        assert_eq!(reached, [1, 3, 4, 5, 7].into_iter().collect());
        map_key(&mut app, key(KeyCode::Char(','), KeyModifiers::NONE));
        assert!(app.selected_node().is_some());
        let after = world_stars(&app);
        assert!(
            before
                .iter()
                .zip(after)
                .all(|(a, b)| (a.index, a.x, a.y) == (b.index, b.x, b.y))
        );
        assert_eq!(app.graph.nodes.len(), 8);
        assert!(
            app.node_visible("node-003"),
            "unloaded status stays visible"
        );
    }

    #[test]
    fn hidden_archives_are_not_arrow_screen_order_click_or_prefetch_targets() {
        let mut app = app(24);
        let stars = world_stars(&app);
        app.selected = stars
            .iter()
            .min_by(|a, b| a.x.total_cmp(&b.x))
            .unwrap()
            .index;
        let hidden = stars
            .iter()
            .max_by(|a, b| a.x.total_cmp(&b.x))
            .unwrap()
            .index;
        app.graph.nodes[hidden].status = "archived".into();
        app.graph.edges.push(Edge {
            from: app.graph.nodes[app.selected].id.clone(),
            to: app.graph.nodes[hidden].id.clone(),
            kind: "supersedes".into(),
            weight: 0.0,
        });
        app.toggle_archived();
        map_key(&mut app, key(KeyCode::Right, KeyModifiers::NONE));
        assert_ne!(app.selected, hidden, "linked hidden candidate cannot win");
        for width in [40, 120] {
            draw(&app, width, 36);
            let hidden_id = app.graph.nodes[hidden].id.clone();
            assert!(
                app.layout
                    .borrow()
                    .camera
                    .hits
                    .iter()
                    .all(|(_, id)| id != &hidden_id)
            );
            assert!(!render::near_viewport_ids(&app, 1).contains(&hidden_id));
            // Defend against a stale hitbox before the next frame too.
            app.layout
                .borrow_mut()
                .camera
                .markers
                .push((Rect::new(2, 2, 1, 1), hidden_id.clone()));
            app.layout
                .borrow_mut()
                .camera
                .hits
                .push((Rect::new(2, 2, 1, 1), hidden_id));
            pointer(&mut app, MouseEventKind::Down(MouseButton::Left), 2, 2);
            assert_ne!(app.selected, hidden);
            for _ in 0..24 {
                map_key(&mut app, key(KeyCode::Right, KeyModifiers::CONTROL));
                assert_ne!(app.selected, hidden);
                draw(&app, width, 36);
            }
        }
    }

    #[test]
    fn all_hidden_and_late_archive_status_have_safe_explicit_selection() {
        let mut app = app(3);
        for node in &mut app.graph.nodes {
            node.status = "archived".into();
        }
        map_key(&mut app, key(KeyCode::Char('h'), KeyModifiers::NONE));
        assert!(app.selected_node().is_none());
        for width in [40, 120] {
            draw(&app, width, 36);
            assert!(app.layout.borrow().camera.markers.is_empty());
            assert!(render::near_viewport_ids(&app, 1).is_empty());
            for code in [KeyCode::Right, KeyCode::Char('.'), KeyCode::Char(',')] {
                map_key(&mut app, key(code, KeyModifiers::NONE));
                assert!(app.selected_node().is_none());
            }
        }
        map_key(&mut app, key(KeyCode::Char('h'), KeyModifiers::NONE));
        assert!(app.selected_node().is_some());
        app.graph.nodes[1].status.clear();
        app.toggle_archived();
        assert_eq!(app.selected, 1);
        app.graph.nodes[1].status = "archived".into();
        app.normalize_map_selection();
        assert!(app.selected_node().is_none());
        app.graph.nodes[2].status = "active".into();
        app.normalize_map_selection();
        assert_eq!(app.selected, 2);
        let mut empty = App::new(vec![], String::new(), true);
        empty.toggle_archived();
        empty.cycle_nodes(true);
        assert!(empty.selected_node().is_none());
    }

    #[test]
    fn archive_filter_closes_hidden_anchor_lens_and_masks_extra_archived_cards() {
        let mut app = app(2);
        let anchor = app.graph.nodes[0].id.clone();
        app.open_edge_lens(
            anchor.clone(),
            Graph {
                nodes: vec![
                    app.graph.nodes[0].clone(),
                    Node {
                        id: "extra".into(),
                        status: "archived".into(),
                        ..Node::default()
                    },
                ],
                edges: vec![Edge {
                    from: anchor.clone(),
                    to: "extra".into(),
                    kind: "associative".into(),
                    weight: 1.0,
                }],
                ..Graph::default()
            },
            LensState::Ready,
        );
        app.toggle_archived();
        assert!(
            app.edge_lens.is_some(),
            "an active/unknown anchor lens stays open"
        );
        assert!(!app.node_visible("extra"));
        for _ in 0..5 {
            app.cycle_nodes(true);
            assert_ne!(app.selected_node().unwrap().id, "extra");
        }
        app.graph.nodes[0].status = "archived".into();
        app.normalize_map_selection(); // late status closes a now-hidden anchor
        assert!(app.edge_lens.is_none());
        assert_eq!(app.selected, 1);
    }

    #[test]
    fn cycling_reaches_every_loaded_island_without_reflow_and_keeps_selected_visible() {
        let mut app = app(240);
        draw(&app, 120, 36);
        let before = world_stars(&app);
        for i in 1..=240 {
            assert!(map_key(
                &mut app,
                key(KeyCode::Char('.'), KeyModifiers::NONE)
            ));
            assert_eq!(app.selected, i % 240);
            draw(&app, 120, 36);
            assert!(
                app.layout
                    .borrow()
                    .camera
                    .markers
                    .iter()
                    .any(|(_, id)| id == &app.selected_node().unwrap().id)
            );
        }
        let after = world_stars(&app);
        assert!(
            before
                .iter()
                .zip(after)
                .all(|(a, b)| (a.index, a.x, a.y) == (b.index, b.x, b.y))
        );
    }
    #[test]
    fn directional_selection_prefers_links_before_nearest_unrelated_node() {
        let mut app = app(24);
        let stars = world_stars(&app);
        let origin = stars.iter().min_by(|a, b| a.x.total_cmp(&b.x)).unwrap();
        app.selected = origin.index;
        let right = stars
            .iter()
            .filter(|s| s.x > origin.x)
            .max_by(|a, b| a.x.total_cmp(&b.x))
            .unwrap()
            .index;
        app.graph.edges.push(Edge {
            from: app.graph.nodes[origin.index].id.clone(),
            to: app.graph.nodes[right].id.clone(),
            kind: "supports".into(),
            weight: 0.0,
        });
        // Zero weight changes no layout spring, but remains a real navigable edge.
        map_key(&mut app, key(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(app.selected, right);
    }
    #[test]
    fn control_arrow_exhausts_columns_and_rows_including_coordinate_ties() {
        let mut app = app(5);
        let positions = [(2, 2), (2, 3), (2, 3), (3, 1), (3, 2)];
        for horizontal in [true, false] {
            let mut ordered: Vec<_> = positions.iter().copied().enumerate().collect();
            ordered.sort_by_key(|(i, (x, y))| {
                if horizontal {
                    (*x, *y, *i)
                } else {
                    (*y, *x, *i)
                }
            });
            app.selected = ordered[0].0;
            for expected in ordered.iter().skip(1).chain(ordered.iter().take(1)) {
                app.layout.borrow_mut().camera.markers = positions
                    .iter()
                    .enumerate()
                    .map(|(i, (x, y))| (Rect::new(*x, *y, 1, 1), app.graph.nodes[i].id.clone()))
                    .collect();
                map_key(
                    &mut app,
                    key(
                        if horizontal {
                            KeyCode::Right
                        } else {
                            KeyCode::Down
                        },
                        KeyModifiers::CONTROL,
                    ),
                );
                assert_eq!(app.selected, expected.0);
            }
        }
    }
    #[test]
    fn click_selects_exact_rendered_marker_and_stale_or_hidden_surfaces_do_not() {
        let mut app = app(240);
        draw(&app, 120, 36);
        let (rect, id) = app
            .layout
            .borrow()
            .camera
            .markers
            .iter()
            .find(|(_, id)| id != &app.graph.nodes[0].id)
            .unwrap()
            .clone();
        let event = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: rect.x,
            row: rect.y,
            modifiers: KeyModifiers::NONE,
        };
        mouse(&mut app, event);
        assert_eq!(app.selected_node().unwrap().id, id);
        app.layout.borrow_mut().invalidate_hits();
        app.selected = 0;
        mouse(&mut app, event);
        assert_eq!(app.selected, 0);
        draw(&app, 76, 30);
        let current = app.selected;
        app.view = View::Scenes;
        mouse(&mut app, event);
        assert_eq!(app.selected, current);
        draw(&app, 76, 30);
        assert!(app.layout.borrow().camera.hits.is_empty());
    }

    #[test]
    fn every_rendered_marker_label_and_index_row_click_returns_and_selects_its_card() {
        for (width, height) in [(120, 36), (76, 30), (40, 24)] {
            let mut app = app(240);
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| render::draw(frame, &app)).unwrap();
            let initial_layout = app.layout.borrow().clone();
            let hits = initial_layout.camera.hits.clone();
            let buffer = terminal.backend().buffer().clone();
            let mut kinds = [false; 3];
            let started = std::time::Instant::now();
            for (rect, id) in hits {
                let index = app
                    .graph
                    .nodes
                    .iter()
                    .position(|node| node.id == id)
                    .unwrap();
                let kind = match rect.width {
                    1 => {
                        assert!(matches!(buffer[(rect.x, rect.y)].symbol(), "◆" | "◇" | "•"));
                        0
                    }
                    2..=4 => {
                        let rendered: String = (rect.x..rect.right())
                            .map(|x| buffer[(x, rect.y)].symbol())
                            .collect();
                        assert_eq!(rendered, format!("{:02}", index + 1));
                        1
                    }
                    _ => 2,
                };
                kinds[kind] = true;
                // Restore the rendered snapshot: a selection deliberately
                // invalidates hits, and its next frame may move the camera.
                app.selected = 0;
                app.scroll = 17;
                *app.layout.borrow_mut() = initial_layout.clone();
                mouse(
                    &mut app,
                    MouseEvent {
                        kind: MouseEventKind::Down(MouseButton::Left),
                        column: rect.right() - 1,
                        row: rect.y,
                        modifiers: KeyModifiers::NONE,
                    },
                );
                assert_eq!(app.selected, index, "hit {rect:?} must select {id}");
                assert_eq!(app.scroll, 0);
                assert!(app.layout.borrow().camera.hits.is_empty());
                terminal.draw(|frame| render::draw(frame, &app)).unwrap();
                assert!(
                    app.layout
                        .borrow()
                        .camera
                        .markers
                        .iter()
                        .any(|(_, marked)| marked == &id)
                );
            }
            assert!(kinds[2], "every layout has clickable index rows");
            if width >= 53 {
                assert!(
                    kinds[0] && kinds[1],
                    "map markers and labels were exercised"
                );
            }
            assert!(started.elapsed() < std::time::Duration::from_secs(2));
        }
    }

    #[test]
    fn empty_space_and_non_press_mouse_events_do_not_select_or_invalidate_the_map() {
        let mut app = app(240);
        draw(&app, 120, 36);
        let hits = app.layout.borrow().camera.hits.clone();
        let (rect, _) = hits
            .iter()
            .find(|(_, id)| id != &app.graph.nodes[0].id)
            .unwrap();
        for kind in [
            MouseEventKind::Moved,
            MouseEventKind::Up(MouseButton::Left),
            MouseEventKind::Drag(MouseButton::Left),
            MouseEventKind::Down(MouseButton::Right),
            MouseEventKind::Down(MouseButton::Middle),
            MouseEventKind::ScrollUp,
            MouseEventKind::ScrollDown,
            MouseEventKind::ScrollLeft,
            MouseEventKind::ScrollRight,
        ] {
            mouse(
                &mut app,
                MouseEvent {
                    kind,
                    column: rect.x,
                    row: rect.y,
                    modifiers: KeyModifiers::NONE,
                },
            );
            assert_eq!(app.selected, 0);
            assert_eq!(app.layout.borrow().camera.hits, hits);
        }
        let empty = (0..36)
            .flat_map(|y| (0..120).map(move |x| (x, y)))
            .find(|&(x, y)| !hits.iter().any(|(rect, _)| rect.contains((x, y).into())))
            .unwrap();
        mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: empty.0,
                row: empty.1,
                modifiers: KeyModifiers::NONE,
            },
        );
        assert_eq!(app.selected, 0);
        assert_eq!(app.layout.borrow().camera.hits, hits);
    }

    #[test]
    fn modified_left_clicks_select_the_rendered_card_without_borrowing_the_layout_twice() {
        let mut app = app(240);
        for modifiers in [
            KeyModifiers::SHIFT,
            KeyModifiers::CONTROL,
            KeyModifiers::ALT,
        ] {
            app.selected = 0;
            draw(&app, 120, 36);
            let (rect, id) = app
                .layout
                .borrow()
                .camera
                .markers
                .iter()
                .find(|(_, id)| id != &app.graph.nodes[0].id)
                .unwrap()
                .clone();
            mouse(
                &mut app,
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: rect.x,
                    row: rect.y,
                    modifiers,
                },
            );
            assert_eq!(app.selected_node().unwrap().id, id);
        }
    }
    #[test]
    fn lens_extra_cards_are_selectable_and_focusable_without_moving_base() {
        let mut app = app(2);
        let before = world_stars(&app);
        app.open_edge_lens(
            "node-000".into(),
            Graph {
                nodes: vec![
                    app.graph.nodes[0].clone(),
                    Node {
                        id: "extra".into(),
                        ..Node::default()
                    },
                ],
                edges: vec![Edge {
                    from: "node-000".into(),
                    to: "extra".into(),
                    kind: "supports".into(),
                    weight: 0.5,
                }],
                ..Graph::default()
            },
            LensState::Ready,
        );
        app.cycle_nodes(true);
        app.cycle_nodes(true);
        assert_eq!(app.selected_node().unwrap().id, "extra");
        assert!(
            before
                .iter()
                .zip(world_stars(&app))
                .all(|(a, b)| (a.index, a.x, a.y) == (b.index, b.x, b.y))
        );
        draw(&app, 120, 36);
        assert!(
            app.layout
                .borrow()
                .camera
                .markers
                .iter()
                .any(|(_, id)| id == "extra")
        );
        app.close_edge_lens();
        assert_eq!(app.selected_node().unwrap().id, "node-000");
    }
    #[test]
    fn pan_changes_projection_only_and_card_hydration_changes_no_positions() {
        let mut app = app(240);
        draw(&app, 120, 36);
        let before = world_stars(&app);
        let camera = app.layout.borrow().camera.center;
        map_key(&mut app, key(KeyCode::Right, KeyModifiers::SHIFT));
        assert_ne!(camera, app.layout.borrow().camera.center);
        assert!(app.layout.borrow().camera.hits.is_empty());
        app.graph.nodes[10].summary = "hydrated card".into();
        app.graph.nodes[10].tags.push("hydrated tag".into());
        let after = world_stars(&app);
        assert!(
            before
                .iter()
                .zip(after)
                .all(|(a, b)| (a.index, a.x, a.y) == (b.index, b.x, b.y))
        );
        app.view = View::Touchstones;
        assert!(!map_key(&mut app, key(KeyCode::Right, KeyModifiers::NONE)));
    }
    #[test]
    fn compact_index_has_screen_order_clicks_and_adjacent_row_hydration() {
        let mut app = app(240);
        draw(&app, 40, 24);
        assert!(app.layout.borrow().camera.index_only);
        let first = app.selected;
        map_key(&mut app, key(KeyCode::Down, KeyModifiers::CONTROL));
        assert_ne!(app.selected, first);
        draw(&app, 40, 24);
        let ids = render::near_viewport_ids(&app, 1);
        assert!(ids.contains(&app.selected_node().unwrap().id));
        assert!(ids.len() >= app.layout.borrow().camera.index_ids.len());
        let (rect, id) = app.layout.borrow().camera.markers[0].clone();
        mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: rect.x,
                row: rect.y,
                modifiers: KeyModifiers::NONE,
            },
        );
        assert_eq!(app.selected_node().unwrap().id, id);
    }

    #[test]
    fn thousands_of_nodes_and_ten_thousand_edges_have_a_click_and_repaint_budget() {
        let mut app = app(3000);
        app.graph.edges = (0..10000)
            .map(|i| Edge {
                from: app.graph.nodes[i % 3000].id.clone(),
                to: app.graph.nodes[(i * 7 + 13) % 3000].id.clone(),
                kind: "supports".into(),
                weight: 0.5,
            })
            .collect();
        let started = std::time::Instant::now();
        draw(&app, 120, 36);
        let build = started.elapsed();
        let points = world_stars(&app);
        let started = std::time::Instant::now();
        for _ in 0..3 {
            let selected_id = &app.selected_node().unwrap().id;
            let (rect, id) = app
                .layout
                .borrow()
                .camera
                .markers
                .iter()
                .find(|(_, id)| id != selected_id)
                .unwrap()
                .clone();
            mouse(
                &mut app,
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: rect.x,
                    row: rect.y,
                    modifiers: KeyModifiers::NONE,
                },
            );
            assert_eq!(app.selected_node().unwrap().id, id);
            draw(&app, 120, 36);
        }
        let warm = started.elapsed();
        eprintln!("3000 nodes / 10000 edges: initial={build:?}, 3 click/repaint frames={warm:?}");
        assert!(
            warm < std::time::Duration::from_secs(2),
            "click/repaint frames must not rebuild an O(E²) topology: {warm:?}"
        );
        app.graph.nodes[42].summary = "hydrated independently".into();
        let after = world_stars(&app);
        assert!(
            points
                .iter()
                .zip(after)
                .all(|(a, b)| (a.index, a.x, a.y) == (b.index, b.x, b.y))
        );
    }
    #[test]
    fn initial_crawl_defers_composition_then_settles_with_links_or_pauses() {
        let mut app = app(240);
        app.graph.inventory = Some(crate::model::InventoryState {
            crawling: true,
            next: Some("continuation".into()),
            ..Default::default()
        });
        draw(&app, 120, 36);
        assert!(!app.layout.borrow().has_world());
        assert!(app.layout.borrow().camera.index_only);
        assert!(!app.layout.borrow().camera.index_ids.is_empty());
        let _ = render::near_viewport_ids(&app, 1);
        assert!(!app.layout.borrow().has_world()); // prefetch cannot commit an arbitrary initial map
        map_key(&mut app, key(KeyCode::Down, KeyModifiers::CONTROL));
        assert!(!app.layout.borrow().has_world());
        map_key(&mut app, key(KeyCode::Down, KeyModifiers::NONE));
        assert!(!app.layout.borrow().has_world());
        map_key(&mut app, key(KeyCode::Char('j'), KeyModifiers::NONE));
        assert!(!app.layout.borrow().has_world());
        app.graph.edges = (0..239)
            .map(|i| Edge {
                from: app.graph.nodes[i].id.clone(),
                to: app.graph.nodes[i + 1].id.clone(),
                kind: "supports".into(),
                weight: 0.5,
            })
            .collect();
        app.graph.inventory.as_mut().unwrap().crawling = false;
        draw(&app, 120, 36);
        assert!(app.layout.borrow().has_world());
        assert!(!app.layout.borrow().camera.index_only);
        let before = world_stars(&app);
        app.graph.inventory.as_mut().unwrap().crawling = true;
        app.graph.nodes.push(Node {
            id: "new-page-node".into(),
            ..Node::default()
        });
        draw(&app, 120, 36);
        assert!(!app.layout.borrow().camera.index_only);
        assert!(
            before
                .iter()
                .zip(world_stars(&app))
                .all(|(a, b)| (a.index, a.x, a.y) == (b.index, b.x, b.y))
        );
    }
}
