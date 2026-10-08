//! One text viewport for opened memories, exact editions, annotations and edges.
//! Count with the renderer's wrapper, not bytes or explicit newline guesses.
use crate::model::App;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::Rect,
    style::Style,
    text::Line,
    widgets::{Paragraph, Wrap},
};

#[derive(Clone, Copy, Debug)]
pub(crate) struct Viewport {
    pub area: Rect,
    pub offset: usize,
    pub max: usize,
}

fn active(app: &App) -> bool {
    !app.editing
        && !app.show_help
        && app.source_menu.is_none()
        && (app.show_details || app.edge_lens.is_some())
}

pub(crate) fn draw(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    lines: Vec<Line<'static>>,
    style: Style,
) {
    if area.is_empty() {
        return;
    }
    // Counting each source line also gives source-line boundaries, so a large
    // document isn't trapped behind Paragraph's u16 scroll-offset ceiling.
    let counts: Vec<_> = lines
        .iter()
        .map(|line| {
            Paragraph::new(line.clone())
                .wrap(Wrap { trim: false })
                .line_count(area.width)
        })
        .collect();
    let total: usize = counts.iter().sum();
    let max = total.saturating_sub(usize::from(area.height));
    let offset = app.scroll.min(max);
    *app.detail_viewport.borrow_mut() = Some(Viewport { area, offset, max });
    let mut within = offset;
    let mut first = 0;
    while first < counts.len() && within >= counts[first] {
        within -= counts[first];
        first += 1;
    }
    // Individual lines are bounded by the native body/metadata read envelopes.
    // Retain the full usize document offset, only narrowing within a source line.
    let within = u16::try_from(within).unwrap_or(u16::MAX);
    frame.render_widget(
        Paragraph::new(lines.into_iter().skip(first).collect::<Vec<_>>())
            .style(style)
            .wrap(Wrap { trim: false })
            .scroll((within, 0)),
        area,
    );
}

pub(crate) fn clamp(app: &mut App) {
    if let Some(view) = *app.detail_viewport.borrow() {
        app.scroll = view.offset;
    }
}

fn move_by(app: &mut App, down: bool, amount: usize) {
    let Some(view) = *app.detail_viewport.borrow() else {
        return;
    };
    let current = app.scroll.min(view.max);
    app.scroll = if down {
        current.saturating_add(amount).min(view.max)
    } else {
        current.saturating_sub(amount)
    };
}

pub(crate) fn key(app: &mut App, key: KeyEvent) -> bool {
    if !active(app) || !key.modifiers.is_empty() || app.detail_viewport.borrow().is_none() {
        return false;
    }
    let page = app.detail_viewport.borrow().map_or(1, |view| {
        usize::from(view.area.height).saturating_sub(1).max(1)
    });
    match key.code {
        KeyCode::Down | KeyCode::Char('j') => move_by(app, true, 1),
        KeyCode::Up | KeyCode::Char('k') => move_by(app, false, 1),
        KeyCode::PageDown => move_by(app, true, page),
        KeyCode::PageUp => move_by(app, false, page),
        KeyCode::Home => app.scroll = 0,
        KeyCode::End => app.scroll = app.detail_viewport.borrow().unwrap().max,
        _ => return false,
    }
    true
}

pub(crate) fn mouse(app: &mut App, event: MouseEvent, reports: usize) -> bool {
    if !active(app)
        || event
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
    {
        return false;
    }
    let Some(view) = *app.detail_viewport.borrow() else {
        return false;
    };
    if !view.area.contains((event.column, event.row).into()) {
        return false;
    }
    let down = match event.kind {
        MouseEventKind::ScrollDown => true,
        MouseEventKind::ScrollUp => false,
        _ => return false,
    };
    app.layout.borrow_mut().cancel_drag();
    move_by(app, down, reports.saturating_mul(3));
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{model::demo_graph, render};
    use ratatui::{
        Terminal,
        backend::TestBackend,
        text::{Span, Text},
    };

    fn opened() -> App {
        let mut app = App::new(vec![], String::new(), true);
        app.graph = demo_graph();
        app.show_details = true;
        app
    }

    fn press(app: &mut App, code: KeyCode) {
        assert!(key(app, KeyEvent::new(code, KeyModifiers::NONE)));
    }

    fn paint(terminal: &mut Terminal<TestBackend>, app: &mut App) {
        terminal.draw(|frame| render::draw(frame, app)).unwrap();
        clamp(app);
    }

    #[test]
    fn wrapped_unicode_long_lines_and_blank_lines_match_the_renderers_pages() {
        let lines = vec![
            Line::from(vec![
                Span::raw("🦀 e\u{301} 界 ".repeat(17)),
                Span::raw(" tail"),
            ]),
            Line::default(),
            Line::raw("longword".repeat(70)),
            Line::raw("end of loaded body"),
        ];
        for width in [3, 7, 19, 43] {
            let mut app = opened();
            let mut actual = Terminal::new(TestBackend::new(width, 5)).unwrap();
            let mut expected = Terminal::new(TestBackend::new(width, 5)).unwrap();
            let paragraph = Paragraph::new(Text::from(lines.clone())).wrap(Wrap { trim: false });
            let count = paragraph.line_count(width);
            for offset in 0..=count {
                app.scroll = offset;
                actual
                    .draw(|frame| draw(frame, frame.area(), &app, lines.clone(), Style::default()))
                    .unwrap();
                let view = app.detail_viewport.borrow().unwrap();
                assert_eq!(view.max, count.saturating_sub(5));
                expected
                    .draw(|frame| {
                        frame.render_widget(
                            paragraph.clone().scroll((view.offset as u16, 0)),
                            frame.area(),
                        )
                    })
                    .unwrap();
                assert_eq!(
                    actual.backend().buffer(),
                    expected.backend().buffer(),
                    "width={width} offset={offset}"
                );
            }
        }
    }

    #[test]
    fn document_offsets_beyond_u16_reach_the_last_loaded_line() {
        let mut app = opened();
        app.scroll = usize::MAX;
        let lines: Vec<_> = (0..70_000).map(|i| Line::raw(format!("row {i}"))).collect();
        let mut terminal = Terminal::new(TestBackend::new(20, 3)).unwrap();
        terminal
            .draw(|frame| draw(frame, frame.area(), &app, lines, Style::default()))
            .unwrap();
        clamp(&mut app);
        assert_eq!(app.scroll, 69_997);
        assert_eq!(terminal.backend().buffer().content()[40].symbol(), "r");
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("row 69999"));
    }

    #[test]
    fn keyboard_pages_home_end_resize_and_selection_reset_use_actual_viewport() {
        let mut app = opened();
        app.graph.nodes[0].body = (0..80).map(|i| format!("body row {i}\n")).collect();
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        paint(&mut terminal, &mut app);
        let page = usize::from(app.detail_viewport.borrow().unwrap().area.height) - 1;
        press(&mut app, KeyCode::PageDown);
        assert_eq!(app.scroll, page);
        press(&mut app, KeyCode::Down);
        assert_eq!(app.scroll, page + 1);
        press(&mut app, KeyCode::Char('k'));
        assert_eq!(app.scroll, page);
        press(&mut app, KeyCode::End);
        let old_max = app.scroll;
        terminal.backend_mut().resize(130, 50);
        paint(&mut terminal, &mut app);
        assert!(app.scroll < old_max);
        let view = app.detail_viewport.borrow().unwrap();
        assert_eq!(app.scroll, view.max);
        press(&mut app, KeyCode::Home);
        assert_eq!(app.scroll, 0);
        press(&mut app, KeyCode::End);
        app.select_node(1);
        assert_eq!(app.scroll, 0);
        paint(&mut terminal, &mut app);
        assert_eq!(app.detail_viewport.borrow().unwrap().offset, 0);
    }

    #[test]
    fn wheel_is_pane_local_and_overlays_and_modified_navigation_keep_priority() {
        let mut app = opened();
        app.graph.nodes[0].body = "scrollable body\n".repeat(60);
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        paint(&mut terminal, &mut app);
        let view = app.detail_viewport.borrow().unwrap();
        let mut event = MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: view.area.x,
            row: view.area.y,
            modifiers: KeyModifiers::NONE,
        };
        assert!(mouse(&mut app, event, 2));
        assert_eq!(app.scroll, 6);
        event.column = 0;
        assert!(!mouse(&mut app, event, 1));
        assert_eq!(app.scroll, 6);
        event.column = view.area.x;
        event.kind = MouseEventKind::ScrollUp;
        assert!(mouse(&mut app, event, usize::MAX));
        assert_eq!(app.scroll, 0);
        for modifiers in [KeyModifiers::SHIFT, KeyModifiers::CONTROL] {
            assert!(!key(&mut app, KeyEvent::new(KeyCode::Down, modifiers)));
        }
        app.show_help = true;
        assert!(!mouse(&mut app, event, 1));
        assert!(!key(
            &mut app,
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)
        ));
        app.show_help = false;
        app.source_menu = Some(0);
        assert!(!mouse(&mut app, event, 1));
        app.source_menu = None;
        app.editing = true;
        assert!(!mouse(&mut app, event, 1));
        app.editing = false;
        app.show_details = false;
        assert!(!key(
            &mut app,
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)
        ));
        assert!(!mouse(&mut app, event, 1));
        terminal.backend_mut().resize(10, 5);
        paint(&mut terminal, &mut app);
        assert!(app.detail_viewport.borrow().is_none());
    }

    #[test]
    fn scene_editions_and_touchstone_annotations_share_the_opened_text_controls() {
        use crate::{
            model::View,
            scenes::{SceneAxis, demo_scenes},
            touchstones::{demo_detail, demo_touchstones},
        };
        for width in [50, 120] {
            for view in [View::Scenes, View::Touchstones] {
                let mut app = opened();
                app.view = view;
                match view {
                    View::Scenes => {
                        app.scenes = demo_scenes(SceneAxis::Recorded);
                        let mut exact = app.scenes.items[0].clone();
                        exact.body = "loaded edition text\n".repeat(60);
                        exact.body_partial = true;
                        app.scene_detail = Some(exact);
                    }
                    View::Touchstones => {
                        app.cabinet.page = demo_touchstones();
                        let mut annotation = demo_detail(&app.cabinet.page.items[0]);
                        annotation.node.body = "loaded annotation text\n".repeat(60);
                        annotation.body_partial = true;
                        app.cabinet.detail = Some(annotation);
                    }
                    View::Map => unreachable!(),
                }
                let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
                paint(&mut terminal, &mut app);
                press(&mut app, KeyCode::End);
                paint(&mut terminal, &mut app);
                assert!(app.scroll > 0);
                let text: String = terminal
                    .backend()
                    .buffer()
                    .content()
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect();
                assert!(
                    text.contains(if view == View::Scenes {
                        "CAPTURE PROVENANCE"
                    } else {
                        "exact target"
                    }),
                    "{view:?} width={width}: {text}"
                );
                press(&mut app, KeyCode::Home);
                paint(&mut terminal, &mut app);
                let view = app.detail_viewport.borrow().unwrap();
                assert_eq!(view.offset, 0);
                assert!(mouse(
                    &mut app,
                    MouseEvent {
                        kind: MouseEventKind::ScrollDown,
                        column: view.area.x,
                        row: view.area.y,
                        modifiers: KeyModifiers::NONE
                    },
                    1
                ));
                assert_eq!(app.scroll, 3);
            }
        }
    }
}
