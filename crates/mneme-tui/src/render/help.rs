//! A modal, scrollable key guide. Its viewport never borrows an inspector's scroll.
use super::{DIM, INK, MINT, PANEL, s};
use crate::model::{App, View};
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind};
use ratatui::{
    Frame,
    layout::Rect,
    style::Style,
    text::Line,
    widgets::{Block, Clear, Paragraph, Wrap},
};

#[derive(Clone, Copy, Debug)]
pub(crate) struct Viewport {
    area: Rect,
    max: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Input {
    Pass,
    Consumed,
    Quit,
}

pub(crate) fn open(app: &mut App) {
    app.show_help = true;
    app.help_scroll = 0;
    app.help_viewport.borrow_mut().take();
    app.layout.borrow_mut().cancel_drag();
}

pub(crate) fn clamp(app: &mut App) {
    if app.show_help {
        if let Some(view) = *app.help_viewport.borrow() {
            app.help_scroll = app.help_scroll.min(view.max);
        }
    }
}

/// Called before generic dispatch, including background cancellation and reads.
pub(crate) fn input(app: &mut App, event: &Event, wheel_reports: usize) -> Input {
    if !app.show_help || app.source_menu.is_some() {
        return Input::Pass;
    }
    let view = *app.help_viewport.borrow();
    let max = view.map_or(0, |view| view.max);
    let page = view.map_or(1, |view| {
        usize::from(view.area.height).saturating_sub(1).max(1)
    });
    app.help_scroll = app.help_scroll.min(max);
    match event {
        Event::Key(key) if key.kind != KeyEventKind::Release => {
            if key.code == KeyCode::Char('q')
                || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
            {
                return Input::Quit;
            }
            match key.code {
                KeyCode::Esc | KeyCode::Char('?') => app.show_help = false,
                KeyCode::Down | KeyCode::Char('j') => {
                    app.help_scroll = app.help_scroll.saturating_add(1).min(max)
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    app.help_scroll = app.help_scroll.saturating_sub(1)
                }
                KeyCode::PageDown => {
                    app.help_scroll = app.help_scroll.saturating_add(page).min(max)
                }
                KeyCode::PageUp => app.help_scroll = app.help_scroll.saturating_sub(page),
                KeyCode::Home => app.help_scroll = 0,
                KeyCode::End => app.help_scroll = max,
                _ => {}
            }
        }
        Event::Mouse(mouse)
            if view.is_some_and(|view| view.area.contains((mouse.column, mouse.row).into())) =>
        {
            let amount = wheel_reports.max(1).saturating_mul(3);
            match mouse.kind {
                MouseEventKind::ScrollDown => {
                    app.help_scroll = app.help_scroll.saturating_add(amount).min(max)
                }
                MouseEventKind::ScrollUp => {
                    app.help_scroll = app.help_scroll.saturating_sub(amount)
                }
                _ => {}
            }
        }
        // Resize still invalidates the map geometry. The next draw clamps help.
        Event::Resize(_, _) => return Input::Pass,
        _ => {}
    }
    Input::Consumed
}

fn lines(view: View) -> Vec<Line<'static>> {
    let keys: &[&str] = match view {
        View::Map => &[
            "arrows / j k   choose a memory",
            ", / .          cycle all selectable memories",
            "click          select a marker or index row",
            "enter          follow connections",
            "i / e          open memory / edges",
            "/              search",
            "drag / ⇧arrows  pan",
            "wheel / +/-    zoom (+ also uses =)",
            "f / 0 / R      fit / 100% / recompose",
            "h              hide / show archived",
            "n / m          continue loading inventory",
            "esc / ⌫ / b    close pane / back",
            "s / t          scenes / touchstones",
        ],
        View::Scenes => &[
            "↑↓ / j k       choose a scene",
            "enter / i      read the exact edition",
            "/              search summaries",
            "o              recorded / occurred time",
            "esc            close edition, then Map",
            "s / t          map / touchstones",
        ],
        View::Touchstones => &[
            "↑↓ / j k       choose a touchstone",
            "enter / i      read annotation",
            "[ ] / ← →      choose historical reference",
            "c              read the exact target today",
            "n / r          next page / restart",
            "esc            close read, annotation, then Map",
            "t              map",
            "s              scenes",
        ],
    };
    let mut result: Vec<_> = keys.iter().map(|line| Line::raw(*line)).collect();
    if view != View::Touchstones {
        result.push(Line::raw("r              refresh"));
    }
    result.extend([
        Line::raw("tab            choose source"),
        Line::raw("C              configuration / read-source details (no read)"),
        Line::raw("↑↓ / j k       scroll an opened inspector"),
        Line::raw("wheel / PgUp/Dn scroll / page"),
        Line::raw("Home / End     first / last line"),
        Line::raw("? / esc        close this guide"),
        Line::raw("q / Ctrl-C     quit"),
        Line::default(),
    ]);
    match view {
        View::Map => result.extend([
            Line::from(s("◆ selected · ◇ core · × archived · • memory", MINT)),
            Line::from(s(
                "Islands group loaded links; names are existing tags.",
                DIM,
            )),
            Line::from(s("Position is not semantic distance.", DIM)),
            Line::from(s("Blank telemetry means no data, not no activity.", DIM)),
        ]),
        View::Scenes => result.extend([
            Line::from(s(
                "Exact editions never become their current successors.",
                DIM,
            )),
            Line::from(s(
                "Occurrence and recording are separate; dates are UTC.",
                DIM,
            )),
        ]),
        View::Touchstones => result.extend([
            Line::from(s(
                "Historical references are summary snapshots, not archived bodies.",
                DIM,
            )),
            Line::from(s(
                "Unchanged snapshot fields do not mean an unchanged body.",
                DIM,
            )),
            Line::from(s("Next page replaces this page.", DIM)),
        ]),
    }
    result
}

pub(super) fn draw(frame: &mut Frame<'_>, screen: Rect, app: &App) {
    let width = screen.width.min(76);
    let height = screen.height.min(27);
    let area = Rect::new(
        screen.x + (screen.width - width) / 2,
        screen.y + (screen.height - height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, area);
    let title = match app.view {
        View::Map => " Map keys ",
        View::Scenes => " Scene keys ",
        View::Touchstones => " Touchstone keys ",
    };
    let block = Block::bordered()
        .title(title)
        .border_style(Style::default().fg(MINT))
        .style(Style::default().bg(PANEL).fg(INK));
    let inside = block.inner(area);
    frame.render_widget(block, area);
    let body = Rect::new(
        inside.x,
        inside.y,
        inside.width,
        inside.height.saturating_sub(1),
    );
    let paragraph = Paragraph::new(lines(app.view)).wrap(Wrap { trim: false });
    let max = paragraph
        .line_count(body.width)
        .saturating_sub(usize::from(body.height));
    *app.help_viewport.borrow_mut() = Some(Viewport { area: body, max });
    frame.render_widget(paragraph.scroll((app.help_scroll.min(max) as u16, 0)), body);
    let controls = if inside.width >= 38 {
        "↑↓ scroll · PgUp/PgDn · Esc close"
    } else if inside.width >= 22 {
        "↑↓ scroll · Esc close"
    } else {
        "↑↓ · Esc close"
    };
    frame.render_widget(
        Paragraph::new(controls).style(Style::default().fg(MINT)),
        Rect::new(inside.x, inside.bottom().saturating_sub(1), inside.width, 1),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::demo_graph;
    use crossterm::event::{KeyEvent, MouseEvent};
    use ratatui::{Terminal, backend::TestBackend};
    use std::collections::HashSet;

    fn key(app: &mut App, code: KeyCode) -> Input {
        input(app, &Event::Key(KeyEvent::new(code, KeyModifiers::NONE)), 1)
    }

    fn render(app: &App, width: u16, height: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| super::draw(frame, frame.area(), app))
            .unwrap();
        let area = app.help_viewport.borrow().unwrap().area;
        (area.y..area.bottom())
            .map(|y| {
                (area.x..area.right())
                    .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn every_wrapped_help_line_is_reachable_at_normal_and_narrow_sizes() {
        for view in [View::Map, View::Scenes, View::Touchstones] {
            for (width, height) in [(22, 9), (38, 16), (78, 24), (98, 30)] {
                let mut app = App::new(vec![], String::new(), true);
                app.view = view;
                open(&mut app);
                let first = render(&app, width, height);
                let viewport = app.help_viewport.borrow().unwrap();
                let paragraph = Paragraph::new(lines(view)).wrap(Wrap { trim: false });
                let total = paragraph.line_count(viewport.area.width) as u16;
                let mut expected =
                    Terminal::new(TestBackend::new(viewport.area.width, total)).unwrap();
                expected
                    .draw(|frame| frame.render_widget(paragraph, frame.area()))
                    .unwrap();
                let expected: HashSet<_> = (0..total)
                    .map(|y| {
                        (0..viewport.area.width)
                            .map(|x| expected.backend().buffer()[(x, y)].symbol())
                            .collect::<String>()
                    })
                    .collect();
                let mut reached = HashSet::new();
                loop {
                    reached.extend(render(&app, width, height));
                    if app.help_scroll == viewport.max {
                        break;
                    }
                    assert_eq!(key(&mut app, KeyCode::Down), Input::Consumed);
                }
                assert!(
                    expected.is_subset(&reached),
                    "{view:?} at {width}x{height}: {:?}",
                    expected.difference(&reached).collect::<Vec<_>>()
                );
                key(&mut app, KeyCode::Home);
                assert_eq!(render(&app, width, height), first);
                key(&mut app, KeyCode::End);
                assert_eq!(app.help_scroll, viewport.max);
                key(&mut app, KeyCode::PageUp);
                assert!(app.help_scroll < viewport.max || viewport.max == 0);
                key(&mut app, KeyCode::PageDown);
                assert_eq!(app.help_scroll, viewport.max);
            }
        }
    }

    #[test]
    fn wheel_scrolls_help_without_moving_the_underlying_map_or_inspector() {
        let mut app = App::new(vec![], String::new(), true);
        app.graph = demo_graph();
        app.selected = 2;
        app.scroll = 7;
        open(&mut app);
        render(&app, 38, 16);
        let area = app.help_viewport.borrow().unwrap().area;
        let zoom = app.layout.borrow().zoom_factor();
        let center = app.layout.borrow().camera.center;
        let event = Event::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: area.x,
            row: area.y,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(input(&mut app, &event, 2), Input::Consumed);
        assert_eq!(app.help_scroll, 6);
        assert_eq!(app.selected, 2);
        assert_eq!(app.scroll, 7);
        assert_eq!(app.layout.borrow().zoom_factor(), zoom);
        assert_eq!(app.layout.borrow().camera.center, center);
        let mut outside = event;
        if let Event::Mouse(ref mut mouse) = outside {
            mouse.column = u16::MAX;
        }
        assert_eq!(input(&mut app, &outside, 1), Input::Consumed);
        assert_eq!(app.help_scroll, 6);
    }

    #[test]
    fn close_hint_fits_the_minimum_supported_terminal() {
        let mut app = App::new(vec![], String::new(), true);
        open(&mut app);
        let mut terminal = Terminal::new(TestBackend::new(24, 9)).unwrap();
        terminal
            .draw(|frame| crate::render::draw(frame, &app))
            .unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("Esc close"));
    }

    #[test]
    fn resize_clamps_the_view_and_reopening_starts_at_the_top() {
        let mut app = App::new(vec![], String::new(), true);
        open(&mut app);
        render(&app, 22, 9);
        key(&mut app, KeyCode::End);
        let old = app.help_scroll;
        assert_eq!(input(&mut app, &Event::Resize(120, 40), 1), Input::Pass);
        render(&app, 120, 40);
        clamp(&mut app);
        let max = app.help_viewport.borrow().unwrap().max;
        assert!(max < old);
        assert_eq!(app.help_scroll, max);
        key(&mut app, KeyCode::End);
        assert_eq!(app.help_scroll, max);
        key(&mut app, KeyCode::Esc);
        open(&mut app);
        assert_eq!(app.help_scroll, 0);
        assert!(app.help_viewport.borrow().is_none());
    }
}
