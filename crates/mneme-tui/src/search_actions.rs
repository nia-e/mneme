//! Search is a ranked list over one owner, not a replacement graph or a history entry.
use super::*;

pub(super) fn apply_search_page(
    app: &mut App,
    pending: &mut Option<PendingRead>,
    mut page: SearchPage,
) {
    let Some(PendingRead::Query(query)) = pending.take() else {
        return;
    };
    let Some(search) = app.search.as_mut().filter(|search| search.query == query) else {
        return;
    };
    page.partial |= page.nodes.len() > data::QUERY_NODES;
    page.nodes.truncate(data::QUERY_NODES);
    search.page = page;
    search.state = LensState::Ready;
    search.selected = 0;
    search.error = None;
    if app.view == View::Map {
        app.deferred_view_read = false;
    }
}

pub(super) fn open_search_hit(
    app: &mut App,
    tx: Option<&mpsc::Sender<Request>>,
    pending: &mut Option<PendingRead>,
) -> bool {
    let Some(id) = app
        .search
        .as_ref()
        .and_then(|search| search.selected_node())
        .map(|node| node.id.clone())
    else {
        return false;
    };
    let Some(tx) = tx else { return false };
    if request(app, tx, Request::Focus(id.clone())) {
        let search = app.search.as_mut().unwrap();
        search.opening = Some(id.clone());
        search.detail = None;
        search.error = None;
        search.scroll = 0;
        *pending = Some(PendingRead::SearchDetail(id));
        true
    } else {
        false
    }
}

pub(super) fn apply_search_detail(app: &mut App, id: &str, graph: Graph) {
    let Some(search) = app
        .search
        .as_mut()
        .filter(|search| search.opening.as_deref() == Some(id))
    else {
        return;
    };
    search.opening = None;
    // The transport validates this identity. Keep the UI transition tied to the
    // selected row as well, including canceled or dismissed detail requests.
    if search.selected_node().is_some_and(|node| node.id == id)
        && graph.focus.as_deref() == Some(id)
    {
        search.detail = Some(graph);
        search.error = None;
        search.scroll = 0;
    }
}

pub(super) fn apply_search_error(app: &mut App, pending: &mut Option<PendingRead>, error: &str) {
    let intent = pending.take();
    let Some(search) = app.search.as_mut() else {
        return;
    };
    match intent {
        Some(PendingRead::Query(query)) if query == search.query => {
            search.state = LensState::Unavailable;
            search.error = Some(clean(error));
            if app.view == View::Map {
                app.deferred_view_read = false;
            }
        }
        Some(PendingRead::SearchDetail(id)) if search.opening.as_ref() == Some(&id) => {
            search.opening = None;
            search.error = Some(clean(error));
        }
        _ => {}
    }
}

/// Returns true only when dismissing a startup --query needs the initial map read.
pub(super) fn search_back(app: &mut App) -> bool {
    let Some(search) = app.search.as_mut() else {
        return false;
    };
    if search.detail.take().is_some() || search.opening.take().is_some() {
        search.scroll = 0;
        search.error = None;
        return false;
    }
    app.error = app.search.take().and_then(|search| search.saved_error);
    app.editing = false;
    app.deferred_view_read = app.last_refresh.is_none() && !app.demo;
    app.deferred_view_read
}

pub(super) fn search_select(app: &mut App, index: usize) {
    let Some(search) = app.search.as_mut() else {
        return;
    };
    if index < search.page.nodes.len() {
        search.selected = index;
        search.detail = None;
        search.opening = None;
        search.error = None;
        search.scroll = 0;
    }
}

pub(super) fn search_move(app: &mut App, forward: bool, amount: usize) {
    let Some(search) = app.search.as_ref() else {
        return;
    };
    let count = search.page.nodes.len();
    if count == 0 {
        return;
    }
    let index = if forward {
        search.selected.saturating_add(amount).min(count - 1)
    } else {
        search.selected.saturating_sub(amount)
    };
    search_select(app, index);
}

/// Search owns its keys; map shortcuts must not pan/filter the hidden graph.
pub(super) fn search_key(
    app: &mut App,
    key: event::KeyEvent,
    tx: Option<&mpsc::Sender<Request>>,
    pending: &mut Option<PendingRead>,
) -> bool {
    if !app.search_visible() {
        return false;
    }
    if detail_scroll::key(app, key) {
        return true;
    }
    match key.code {
        KeyCode::Esc | KeyCode::Backspace | KeyCode::Char('b') => {
            if search_back(app) && !app.busy {
                if let Some(tx) = tx {
                    initial_map(app, tx, pending);
                }
            }
        }
        KeyCode::Enter | KeyCode::Char('i') => {
            open_search_hit(app, tx, pending);
        }
        KeyCode::Down | KeyCode::Char('j') => search_move(app, true, 1),
        KeyCode::Up | KeyCode::Char('k') => search_move(app, false, 1),
        KeyCode::PageDown => search_move(app, true, 8),
        KeyCode::PageUp => search_move(app, false, 8),
        KeyCode::Home => search_select(app, 0),
        KeyCode::End => search_move(app, true, usize::MAX),
        KeyCode::Char('/') => {
            app.editing = true;
            app.input.clear();
        }
        KeyCode::Char('r') => {
            if let Some(tx) = tx {
                let query = app.search.as_ref().unwrap().query.clone();
                query_field(app, tx, pending, query);
            }
        }
        KeyCode::Char('q' | '?' | 'C') | KeyCode::Tab => return false,
        _ => {}
    }
    true
}

pub(super) fn search_mouse(app: &mut App, mouse: event::MouseEvent, reports: usize) -> bool {
    if !app.search_visible()
        || app.editing
        || app.show_help
        || app.config_inspector.is_some()
        || app.source_menu.is_some()
    {
        return false;
    }
    if detail_scroll::mouse(app, mouse, reports) {
        return true;
    }
    if app.search.as_ref().unwrap().detail.is_some() {
        return true;
    }
    match mouse.kind {
        MouseEventKind::ScrollDown => search_move(app, true, reports),
        MouseEventKind::ScrollUp => search_move(app, false, reports),
        MouseEventKind::Down(MouseButton::Left) => {
            let selected = app
                .search
                .as_ref()
                .unwrap()
                .hits
                .borrow()
                .iter()
                .find(|(area, _)| area.contains((mouse.column, mouse.row).into()))
                .map(|(_, index)| *index);
            if let Some(index) = selected {
                search_select(app, index);
            }
        }
        _ => {}
    }
    true
}
