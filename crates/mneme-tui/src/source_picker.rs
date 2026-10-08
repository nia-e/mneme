//! Metadata-only source selection. Opening the menu never opens a connection.
use crate::model::App;
use crossterm::event::{KeyCode, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Clear, Paragraph},
};

pub(crate) fn open(app: &mut App) {
    let mut route = 0;
    let selected = app
        .sources
        .iter()
        .position(|source| {
            if source.route.is_some() {
                let current = route;
                route += 1;
                current == app.target
            } else {
                false
            }
        })
        .unwrap_or(0);
    app.source_menu = Some(selected);
    app.show_help = false;
    app.layout.borrow_mut().cancel_drag();
}
fn choose(app: &mut App, index: usize) -> Option<usize> {
    let source = app.sources.get(index)?;
    if source.route.is_none() {
        app.event(
            source
                .unavailable_reason
                .clone()
                .unwrap_or_else(|| "No owner route configured for this source".into()),
        );
        return None;
    }
    let target = app.sources[..index]
        .iter()
        .filter(|source| source.route.is_some())
        .count();
    app.source_menu = None;
    app.source_menu_hits.borrow_mut().clear();
    Some(target)
}
pub(crate) fn key(app: &mut App, key: KeyCode) -> Option<usize> {
    let selected = app.source_menu?;
    let count = app.sources.len();
    match key {
        KeyCode::Esc | KeyCode::Tab | KeyCode::Char('q') => {
            app.source_menu = None;
            app.source_menu_hits.borrow_mut().clear();
        }
        KeyCode::Down | KeyCode::Right | KeyCode::Char('j') if count > 0 => {
            app.source_menu = Some((selected + 1) % count)
        }
        KeyCode::Up | KeyCode::Left | KeyCode::Char('k') if count > 0 => {
            app.source_menu = Some((selected + count - 1) % count)
        }
        KeyCode::Enter => return choose(app, selected),
        _ => {}
    }
    None
}
pub(crate) fn mouse(app: &mut App, mouse: MouseEvent) -> Option<usize> {
    if mouse.kind != MouseEventKind::Down(MouseButton::Left) {
        return None;
    }
    let index = app
        .source_menu_hits
        .borrow()
        .iter()
        .find(|(rect, _)| rect.contains((mouse.column, mouse.row).into()))
        .map(|(_, i)| *i)?;
    app.source_menu = Some(index);
    choose(app, index)
}
fn clipped(text: &str, width: u16) -> String {
    let text = crate::model::clean(text).replace(['\n', '\t'], " ");
    let mut result = String::new();
    for c in text.chars() {
        if Line::raw(format!("{result}{c}")).width() > usize::from(width.saturating_sub(1)) {
            result.push('…');
            break;
        }
        result.push(c);
    }
    result
}
pub(crate) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    app.source_menu_hits.borrow_mut().clear();
    if area.width == 0 || area.height == 0 {
        return;
    }
    let Some(selected) = app.source_menu else {
        return;
    };
    let width = area.width.min(66);
    let height = area
        .height
        .min((app.sources.len() as u16).saturating_add(6))
        .max(1);
    let panel = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, panel);
    frame.render_widget(
        Block::bordered().title(" Known sources ").style(
            Style::default()
                .bg(Color::Rgb(16, 23, 35))
                .fg(Color::Rgb(226, 235, 239)),
        ),
        panel,
    );
    let inner = Rect::new(
        panel.x + 1,
        panel.y + 1,
        panel.width.saturating_sub(2),
        panel.height.saturating_sub(2),
    );
    if inner.height == 0 {
        return;
    }
    let capacity = usize::from(inner.height.saturating_sub(3));
    let start = selected.saturating_sub(capacity.saturating_sub(1));
    app.source_menu_hits.borrow_mut().clear();
    for (row, (index, source)) in app
        .sources
        .iter()
        .enumerate()
        .skip(start)
        .take(capacity)
        .enumerate()
    {
        let text = format!(
            "{} {}{}",
            if index == selected { "›" } else { " " },
            source.name,
            if source.route.is_none() {
                " · unavailable"
            } else {
                ""
            }
        );
        let rect = Rect::new(inner.x, inner.y + row as u16, inner.width, 1);
        frame.render_widget(
            Paragraph::new(clipped(&text, inner.width)).style(Style::default().fg(
                if index == selected {
                    Color::Rgb(119, 237, 192)
                } else {
                    Color::Rgb(226, 235, 239)
                },
            )),
            rect,
        );
        app.source_menu_hits.borrow_mut().push((rect, index));
    }
    let reason = app
        .sources
        .get(selected)
        .and_then(|source| source.unavailable_reason.as_deref())
        .unwrap_or("Configured route · availability checked only when selected");
    if inner.height >= 3 {
        frame.render_widget(
            Paragraph::new(clipped(reason, inner.width))
                .style(Style::default().fg(Color::Rgb(233, 197, 129))),
            Rect::new(inner.x, inner.y + inner.height - 2, inner.width, 1),
        );
        frame.render_widget(
            Paragraph::new(Line::from(Span::raw("↑↓ select · Enter open · Esc cancel"))),
            Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{SourceChoice, Target};
    fn app() -> App {
        let target = Target {
            name: "project (alias)".into(),
            database: "alias".into(),
            url: "http://127.0.0.1:1".into(),
            ssh_mcp_port: 18766,
            token_env: None,
            expected_db_id: Some("identity".into()),
            expected_path: None,
        };
        let mut app = App::new(vec![target], String::new(), false);
        app.sources.push(SourceChoice::unavailable(
            "Archived project",
            "No configured owner route",
        ));
        app
    }
    #[test]
    fn keyboard_visits_unavailable_and_cancel_does_not_switch() {
        let mut app = app();
        open(&mut app);
        key(&mut app, KeyCode::Down);
        assert_eq!(app.source_menu, Some(1));
        assert_eq!(key(&mut app, KeyCode::Enter), None);
        assert_eq!(app.target, 0);
        assert_eq!(app.source_menu, Some(1));
        key(&mut app, KeyCode::Esc);
        assert_eq!(app.source_menu, None);
        assert_eq!(app.targets[0].expected_db_id.as_deref(), Some("identity"));
    }
    #[test]
    fn picker_is_centered_and_keeps_unavailable_visible_when_narrow() {
        for width in [24, 36, 100] {
            let mut app = app();
            open(&mut app);
            key(&mut app, KeyCode::Down);
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 16)).unwrap();
            terminal.draw(|f| crate::render::draw(f, &app)).unwrap();
            let hits = app.source_menu_hits.borrow();
            assert_eq!(hits.len(), 2);
            assert_eq!(hits[0].0.x, if width == 100 { 18 } else { 2 });
            let buffer = terminal.backend().buffer();
            let text = buffer
                .content
                .iter()
                .map(|c| c.symbol())
                .collect::<String>();
            assert!(text.contains("Known sources"));
            assert!(text.contains("Archived project"));
            assert!(app.layout.borrow().camera.hits.is_empty());
        }
    }
    #[test]
    fn opening_picker_discards_queued_pre_modal_enter() {
        let mut app = app();
        let mut queued = Some((
            crossterm::event::KeyEvent::new(KeyCode::Enter, crossterm::event::KeyModifiers::NONE),
            Some("selected-memory".into()),
        ));
        crate::open_source_picker(&mut app, &mut queued);
        assert!(queued.is_none());
        key(&mut app, KeyCode::Down);
        // A cancellation reply can now clear its barrier, but has no key to replay.
        assert_eq!(app.source_menu, Some(1));
        assert_eq!(app.target, 0);
    }
    #[test]
    fn explicit_selection_keeps_route_identity_and_resets_all_view_caches() {
        let mut app = app();
        let mut next = app.targets[0].clone();
        next.name = "global (other alias)".into();
        next.expected_db_id = Some("other-identity".into());
        app.targets.push(next.clone());
        app.sources.push(SourceChoice::routed(next));
        app.graph = crate::demo_graph();
        app.remember(app.visit());
        app.scenes_loaded = true;
        app.cabinet.loaded = true;
        app.show_details = true;
        app.activity_samples
            .push_back(crate::model::Activity::default());
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 24)).unwrap();
        terminal.draw(|f| crate::render::draw(f, &app)).unwrap();
        open(&mut app);
        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::Down);
        let target = key(&mut app, KeyCode::Enter).unwrap();
        assert_eq!(target, 1);
        crate::reset_source(&mut app, &mut None);
        app.target = target;
        assert!(app.graph.nodes.is_empty());
        assert!(app.history.is_empty());
        assert!(!app.scenes_loaded && !app.cabinet.loaded);
        assert!(app.activity_samples.is_empty());
        assert!(app.layout.borrow().camera.hits.is_empty());
        assert_eq!(
            app.targets[app.target].expected_db_id.as_deref(),
            Some("other-identity")
        );
    }
    #[test]
    fn small_menu_scrolls_to_every_row_and_zero_area_clears_hits() {
        let mut app = app();
        for i in 0..64 {
            app.sources.push(SourceChoice::unavailable(
                format!("Archive {i}"),
                "No live owner route",
            ));
        }
        open(&mut app);
        for _ in 0..65 {
            key(&mut app, KeyCode::Down);
        }
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(36, 9)).unwrap();
        terminal.draw(|f| crate::render::draw(f, &app)).unwrap();
        assert!(
            app.source_menu_hits
                .borrow()
                .iter()
                .any(|(_, index)| *index == 65)
        );
        terminal
            .draw(|f| draw(f, Rect::new(0, 0, 0, 0), &app))
            .unwrap();
        assert!(app.source_menu_hits.borrow().is_empty());
    }
    #[test]
    fn click_uses_current_modal_rows_and_unavailable_does_not_switch() {
        let mut app = app();
        open(&mut app);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 20)).unwrap();
        terminal.draw(|f| crate::render::draw(f, &app)).unwrap();
        let rows = app.source_menu_hits.borrow().clone();
        let click = |row: Rect| MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: row.x,
            row: row.y,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        assert_eq!(mouse(&mut app, click(rows[1].0)), None);
        assert_eq!(app.source_menu, Some(1));
        assert_eq!(app.target, 0);
        assert_eq!(mouse(&mut app, click(rows[0].0)), Some(0));
        assert_eq!(app.source_menu, None);
    }
}
