//! A small observatory: quiet geometry, real links, and an honest instrument strip.
//!
//! Layout is deliberately deterministic. A redraw must not rearrange somebody's map.

use crate::model::{App, View, clean};
use ratatui::backend::TestBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::canvas::{Canvas, Line as Stroke, Points};
use ratatui::widgets::{Block, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use std::fmt::Write as _;

const NIGHT: Color = Color::Rgb(12, 17, 27);
const PANEL: Color = Color::Rgb(16, 23, 35);
const INK: Color = Color::Rgb(226, 235, 239);
const DIM: Color = Color::Rgb(113, 137, 153);
const GRID: Color = Color::Rgb(32, 48, 65);
const MINT: Color = Color::Rgb(119, 237, 192);
const LILAC: Color = Color::Rgb(190, 162, 250);
const BLUE: Color = Color::Rgb(116, 185, 229);
const GOLD: Color = Color::Rgb(233, 197, 129);
const ROSE: Color = Color::Rgb(237, 137, 160);

pub(crate) mod help;
mod islands;
pub(crate) mod map;
mod memory;
mod scenes;
mod touchstones;

#[cfg(test)]
use crate::model::{Edge, LensState};
#[cfg(test)]
use map::{Star, cell_center, draw_edges, marker_rect, placed_stars, star_field};
#[cfg(test)]
use ratatui::widgets::canvas::Circle;
#[cfg(test)]
use std::collections::HashMap;

#[cfg(test)]
mod projection_tests;

pub(crate) fn near_viewport_ids(app: &App, margin: u16) -> Vec<String> {
    let mut ids = map::near_viewport_ids(app, f64::from(margin));
    let selected = app.selected_node().map(|node| node.id.as_str());
    ids.sort_by(|a, b| {
        (Some(a.as_str()) != selected)
            .cmp(&(Some(b.as_str()) != selected))
            .then(a.cmp(b))
    });
    ids
}

pub fn draw(frame: &mut Frame<'_>, app: &App) {
    app.detail_viewport.borrow_mut().take();
    app.source_menu_hits.borrow_mut().clear();
    app.layout.borrow_mut().invalidate_hits();
    app.layout.borrow_mut().set_map_area(None);
    let screen = frame.area();
    frame.render_widget(
        Block::default().style(Style::default().bg(NIGHT).fg(INK)),
        screen,
    );
    if screen.width < 24 || screen.height < 9 {
        app.layout.borrow_mut().cancel_drag();
        frame.render_widget(
            Paragraph::new("MNEME\nA little more sky, please.\nResize · q to leave")
                .style(Style::default().fg(MINT))
                .wrap(Wrap { trim: false }),
            screen,
        );
        return;
    }
    let screen = Rect::new(screen.x + 1, screen.y, screen.width - 2, screen.height);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(2),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(screen);
    header(frame, rows[0], app);
    match app.view {
        View::Scenes => scenes::draw(frame, rows[1], app),
        View::Touchstones => touchstones::draw(frame, rows[1], app),
        View::Map => map::draw(frame, rows[1], app),
    }
    activity(frame, rows[2], app);
    footer(frame, rows[3], app);
    if app.source_menu.is_some() {
        crate::source_picker::draw(frame, screen, app);
        app.layout.borrow_mut().invalidate_hits();
        app.layout.borrow_mut().set_map_area(None);
    }
    if app.show_help && app.source_menu.is_none() {
        help::draw(frame, screen, app);
    }
    let mut layout = app.layout.borrow_mut();
    if app.view != View::Map
        || app.editing
        || app.show_help
        || app.source_menu.is_some()
        || layout.camera.map_area.is_none()
    {
        layout.cancel_drag();
    }
}

fn text_line(frame: &mut Frame<'_>, area: Rect, line: Line<'_>) {
    if area.width > 0 && area.height > 0 {
        frame.render_widget(
            Paragraph::new(line),
            Rect::new(area.x, area.y, area.width, 1),
        );
    }
}

fn s(text: impl Into<String>, color: Color) -> Span<'static> {
    Span::styled(text.into(), Style::default().fg(color))
}

fn single(text: &str) -> String {
    clean(text).replace(['\n', '\t'], " ")
}

pub(crate) fn short(text: &str, columns: usize) -> String {
    let text = single(text);
    if Line::raw(&text).width() <= columns {
        return text;
    }
    let mut result = String::new();
    for ch in text.chars() {
        let mut next = result.clone();
        next.push(ch);
        if Line::raw(&next).width() + 1 > columns {
            break;
        }
        result = next;
    }
    if columns > 0 {
        result.push('…');
    }
    result
}

fn card_title<'a>(app: &App, node: &'a crate::model::Node) -> &'a str {
    if !node.summary.is_empty() {
        &node.summary
    } else if app
        .graph
        .inventory
        .as_ref()
        .is_some_and(|inventory| !inventory.hydrated.contains(&node.id))
    {
        "Card not loaded yet"
    } else {
        "Untitled memory"
    }
}

fn timestamp(ms: u64) -> String {
    mneme_present::format_timestamp_millis(u128::from(ms))
}

fn header(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let source = app
        .targets
        .get(app.target)
        .map(|t| single(&t.name))
        .unwrap_or_else(|| "memory".into());
    let status = if app.demo {
        "DEMO"
    } else if app.error.is_some() {
        "unavailable"
    } else {
        &source
    };
    let status_color = if app.demo {
        GOLD
    } else if app.error.is_some() {
        ROSE
    } else {
        DIM
    };
    let (brand, divider) = if area.width < 48 {
        ("◌ mneme", " · ")
    } else {
        ("◌  m n e m e", "   /   ")
    };
    let reserved = Line::raw(brand).width()
        + Line::raw(divider).width()
        + usize::from(area.width > 48 && app.busy) * 9;
    text_line(
        frame,
        area,
        Line::from(vec![
            s(brand, MINT).add_modifier(Modifier::BOLD),
            s(divider, GRID),
            s(
                short(status, usize::from(area.width).saturating_sub(reserved)),
                status_color,
            ),
        ]),
    );
    if area.width > 48 && app.busy {
        let badge = "reading  ";
        text_line(
            frame,
            Rect::new(
                area.right() - badge.len() as u16,
                area.y,
                badge.len() as u16,
                1,
            ),
            Line::from(s(badge, if app.busy { MINT } else { GRID })),
        );
    }
    // A fine instrument rule, not another dashboard box.
    if area.height > 1 {
        let tabs = vec![
            s("  map", if app.view == View::Map { MINT } else { DIM }),
            s("   /   ", GRID),
            s("scenes", if app.view == View::Scenes { MINT } else { DIM }),
            s("   /   ", GRID),
            s(
                "touchstones",
                if app.view == View::Touchstones {
                    MINT
                } else {
                    DIM
                },
            ),
            s("  ", GRID),
        ];
        let used = Line::from(tabs.clone()).width();
        let mut tabs = tabs;
        tabs.push(s(
            "─".repeat(usize::from(area.width).saturating_sub(used)),
            GRID,
        ));
        text_line(
            frame,
            Rect::new(area.x, area.y + 1, area.width, 1),
            Line::from(tabs),
        );
    }
}

fn activity(frame: &mut Frame<'_>, area: Rect, app: &App) {
    if area.height == 0 || area.width < 2 {
        return;
    }
    let label_width = if area.width >= 60 { 22 } else { 16 };
    let waveform = Rect::new(
        area.x,
        area.y,
        area.width.saturating_sub(label_width),
        area.height.saturating_sub(1),
    );
    let mut points = Vec::new();
    let history = &app.activity_samples;
    let capacity = usize::from(waveform.width).max(1);
    let shown = history.len().min(capacity);
    let maximum = history
        .iter()
        .rev()
        .take(shown)
        .map(|sample| {
            sample
                .returns
                .saturating_add(sample.in_flight)
                .saturating_add(sample.backend_jobs)
        })
        .max()
        .unwrap_or(0)
        .max(1) as f64;
    for (i, sample) in history
        .iter()
        .skip(history.len().saturating_sub(shown))
        .enumerate()
    {
        let value = (sample
            .returns
            .saturating_add(sample.in_flight)
            .saturating_add(sample.backend_jobs)) as f64;
        points.push(((capacity - shown + i) as f64, value / maximum));
    }
    if !app.demo && !points.is_empty() && waveform.height > 0 {
        frame.render_widget(
            Canvas::default()
                .background_color(NIGHT)
                .marker(Marker::Braille)
                .x_bounds([0.0, f64::from(waveform.width)])
                .y_bounds([-0.15, 1.1])
                .paint(|ctx| {
                    // Only the observed span gets a baseline; unobserved time is blank.
                    for pair in points.windows(2) {
                        ctx.draw(&Stroke {
                            x1: pair[0].0,
                            y1: pair[0].1,
                            x2: pair[1].0,
                            y2: pair[1].1,
                            color: MINT,
                        });
                    }
                    ctx.draw(&Points {
                        coords: &points,
                        color: MINT,
                    });
                }),
            waveform,
        );
    } else {
        text_line(
            frame,
            waveform,
            Line::from(s("─".repeat(usize::from(waveform.width)), GRID)),
        );
    }
    let status = if app.demo {
        "demo · no telemetry".to_string()
    } else if app.error.is_some() {
        "source unavailable".into()
    } else if let Some(activity) = &app.activity {
        if activity.feed {
            format!(
                "{} returns{}",
                activity.returns,
                if activity.missed { " · gap" } else { "" }
            )
        } else {
            format!("{} · sampled", single(&activity.state))
        }
    } else {
        "awaiting sample".into()
    };
    text_line(
        frame,
        Rect::new(waveform.right(), area.y, label_width.min(area.width), 1),
        Line::from(s(
            status,
            if app.demo {
                GOLD
            } else if app.error.is_some() {
                ROSE
            } else {
                DIM
            },
        )),
    );
    if area.height >= 2 {
        let status = app
            .activity
            .as_ref()
            .map(|a| format!("{} req · {} jobs", a.in_flight, a.backend_jobs))
            .unwrap_or_default();
        text_line(
            frame,
            Rect::new(waveform.right(), area.y + 1, label_width.min(area.width), 1),
            Line::from(s(status, DIM)),
        );
    }
    if area.height >= 3 {
        let event = app
            .error
            .as_ref()
            .map(|e| (format!("! {}", single(e)), ROSE))
            .or_else(|| {
                app.events
                    .front()
                    .map(|e| (format!("↳ {}", single(e)), DIM))
            });
        if let Some((event, color)) = event {
            text_line(
                frame,
                Rect::new(area.x + 1, area.bottom() - 1, area.width - 1, 1),
                Line::from(s(event, color)),
            );
        }
    }
}

fn footer(frame: &mut Frame<'_>, area: Rect, app: &App) {
    if area.height == 0 || app.show_help || app.source_menu.is_some() {
        return;
    }
    if app.editing {
        text_line(
            frame,
            area,
            Line::from(vec![
                s(" / ", MINT),
                s(single(&app.input), INK),
                s("▏", MINT),
            ]),
        );
    } else {
        let pane_open = app.show_details
            || app.edge_lens.is_some()
            || app.show_help
            || app.cabinet.detail.is_some();
        let can_back = app.view == View::Map && app.can_go_back();
        let back = if pane_open && can_back && area.width >= 40 {
            "esc close · b ← back  "
        } else if pane_open {
            "esc close  "
        } else if can_back {
            "esc ← back  "
        } else {
            ""
        };
        let remaining = usize::from(area.width).saturating_sub(Line::raw(back).width());
        let candidates: &[&str] = if app.show_details || app.edge_lens.is_some() {
            if app.view == View::Touchstones {
                &["↑↓ scroll  t map  ? keys  q quit", "t map  ?  q"]
            } else {
                &["↑↓ scroll  ? keys  q quit", "↑↓ scroll  ?  q", "?  q"]
            }
        } else if app.view == View::Touchstones {
            &[
                "↵ annotation  c today  t map  ? keys  q quit",
                "↵ annotation  t map  ?  q",
                "t map  ?  q",
            ]
        } else if app.view == View::Scenes {
            &[
                "↵ edition  / search  ? keys  q quit",
                "↵ edition  / search  ?  q",
                "↵ read  / find  ?  q",
                "/ find  ?  q",
            ]
        } else {
            &[
                "↵ follow  / search  ? keys  q quit",
                "↵ follow  / search  ?  q",
                "↵ follow  / find  ?  q",
                "/ find  ?  q",
            ]
        };
        let keys = candidates
            .iter()
            .find(|keys| Line::raw(**keys).width() <= remaining)
            .copied()
            .unwrap_or("?  q");
        text_line(frame, area, Line::from(vec![s(back, MINT), s(keys, LILAC)]));
    }
}

/// Render a noninteractive, ANSI-colored frame without modifying the terminal.
pub fn preview(width: u16, height: u16, app: &App) -> std::io::Result<String> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| draw(frame, app)).unwrap();
    let buffer = terminal.backend().buffer();
    let mut output = String::new();
    let mut previous = None;
    for row in 0..height {
        let mut column = 0;
        while column < width {
            let cell = &buffer[(column, row)];
            let style = (cell.fg, cell.bg, cell.modifier);
            if previous != Some(style) {
                output.push_str("\x1b[0m");
                ansi_color(&mut output, cell.fg, false);
                ansi_color(&mut output, cell.bg, true);
                if cell.modifier.contains(Modifier::BOLD) {
                    output.push_str("\x1b[1m");
                }
                previous = Some(style);
            }
            output.push_str(cell.symbol());
            column = column.saturating_add(Line::raw(cell.symbol()).width().max(1) as u16);
        }
        output.push_str("\x1b[0m\n");
        previous = None;
    }
    Ok(output)
}

fn ansi_color(output: &mut String, color: Color, background: bool) {
    let code = if background { 48 } else { 38 };
    match color {
        Color::Rgb(r, g, b) => {
            let _ = write!(output, "\x1b[{code};2;{r};{g};{b}m");
        }
        Color::Indexed(i) => {
            let _ = write!(output, "\x1b[{code};5;{i}m");
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Activity, Node, demo_graph};

    fn app() -> App {
        let mut app = App::new(Vec::new(), "a thought worth returning to".into(), true);
        app.graph = demo_graph();
        app
    }

    fn plain_frame(width: u16, height: u16, app: &App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn wide_demo_is_labeled_and_has_constellation_and_inspector() {
        let text = plain_frame(110, 34, &app());
        assert!(text.contains("DEMO"));
        assert!(text.contains("nodes ·"));
        assert!(text.contains("↵ follow"));
        assert!(!text.contains("i  open memory"));
        assert!(text.contains("demo · no telemetry"));
        assert!(text.contains("follows this"));
        assert!(text.contains("A memory is a promise"));
    }

    #[test]
    fn narrow_layout_and_tiny_terminal_do_not_panic() {
        for (width, height) in [
            (100, 30),
            (60, 24),
            (40, 16),
            (24, 9),
            (10, 3),
            (1, 1),
            (0, 0),
        ] {
            let text = plain_frame(width, height, &app());
            if width >= 60 {
                assert!(text.contains("nodes ·"));
            }
        }
    }

    #[test]
    fn empty_map_distinguishes_loading_unavailable_unread_empty_search_and_partial() {
        let mut app = App::new(vec![], String::new(), false);
        for (busy, error, loaded, query, partial, expected) in [
            (true, false, false, "", false, "Reading this source"),
            (false, true, false, "", false, "Map unavailable"),
            (false, false, false, "", false, "Map not loaded"),
            (false, false, true, "", false, "No memories in this field"),
            (false, false, true, "a cue", false, "No memories match"),
            (false, false, true, "a cue", true, "partial view"),
        ] {
            app.busy = busy;
            app.error = error.then(|| "Connection refused".into());
            app.last_refresh = loaded.then(std::time::Instant::now);
            app.query = query.into();
            app.graph.partial = partial;
            for (width, height) in [(120, 36), (60, 24), (40, 16)] {
                let text = plain_frame(width, height, &app);
                assert!(text.contains(expected), "{width}x{height}: {text}");
                if error || !loaded || partial {
                    assert!(!text.contains("No memories in this field"));
                }
            }
            // Even the minimum layout keeps the real source failure in its header.
            let text = plain_frame(24, 9, &app);
            if error {
                assert!(text.contains("unavailable"));
            }
        }
    }

    #[test]
    fn persistent_chrome_keeps_primary_actions_without_the_instruction_inventory() {
        for view in [View::Map, View::Scenes, View::Touchstones] {
            let mut app = app();
            app.view = view;
            app.scenes = crate::scenes::demo_scenes(app.scene_axis);
            app.scenes_loaded = true;
            app.cabinet.page = crate::touchstones::demo_touchstones();
            app.cabinet.loaded = true;
            for width in [120, 80, 40, 24] {
                let text = plain_frame(width, 30, &app);
                let footer = text.lines().last().unwrap();
                assert!(footer.contains('?') && footer.contains('q'));
                assert!(!footer.contains("tab source"));
                assert!(!footer.contains("zoom"));
                assert!(!text.contains("Authored meaning"));
                assert!(!text.contains("changes time axis"));
                assert!(!text.lines().next().unwrap().contains("observatory"));
            }
        }
    }

    #[test]
    fn modal_footer_is_quiet_and_pane_actions_fit_small_screens() {
        for width in [24, 40, 80, 120] {
            let mut app = app();
            app.remember(app.visit());
            app.show_details = true;
            for view in [View::Map, View::Scenes, View::Touchstones] {
                app.view = view;
                let text = plain_frame(width, 30, &app);
                let footer = text.lines().last().unwrap();
                assert!(
                    footer.contains('?') && footer.contains('q'),
                    "{view:?} {width}: {footer}"
                );
                if view == View::Touchstones {
                    assert!(footer.contains("t map"));
                }
            }
            for help in [true, false] {
                app.show_help = help;
                app.source_menu = (!help).then_some(0);
                let text = plain_frame(width, 36, &app);
                assert!(text.lines().last().unwrap().trim().is_empty());
            }
        }
    }

    #[test]
    fn selected_memory_stays_visible_in_world_and_index() {
        let mut app = app();
        app.graph.nodes.extend((12..33).map(|i| Node {
            id: format!("test-{i}"),
            summary: format!("Memory number {i}"),
            ..Node::default()
        }));
        app.selected = 32;
        let stars = star_field(&app, 70, 17);
        assert!(stars.len() <= app.graph.nodes.len());
        assert!(stars.iter().any(|star| star.index == 32));
        let text = plain_frame(100, 30, &app);
        assert!(text.contains("33 Memory number 32"));
    }

    #[test]
    fn instrument_counts_clipped_links_not_only_visible_endpoints() {
        let mut app = app();
        app.graph.nodes.extend((12..240).map(|i| Node {
            id: format!("hidden-{i}"),
            summary: format!("Memory {i}"),
            ..Node::default()
        }));
        app.graph.edges.clear();
        let field = star_field(&app, 73, 27);
        let hidden = (0..app.graph.nodes.len())
            .find(|i| !field.iter().any(|star| star.index == *i))
            .unwrap();
        let visible_a = app.graph.nodes[field[0].index].id.clone();
        let visible_b = app.graph.nodes[field[1].index].id.clone();
        app.graph.edges = vec![
            Edge {
                from: visible_a.clone(),
                to: visible_b,
                kind: "supports".into(),
                weight: 0.5,
            },
            Edge {
                from: visible_a,
                to: app.graph.nodes[hidden].id.clone(),
                kind: "supports".into(),
                weight: 0.5,
            },
        ];
        let text = plain_frame(110, 34, &app);
        assert!(text.contains("visible/loaded"), "{text}");
        assert!(text.contains("2 links drawn"), "{text}");
    }

    #[test]
    fn layout_is_stable_across_ticks_and_uses_no_synthetic_live_activity() {
        let mut app = app();
        let before = star_field(&app, 60, 15);
        app.tick = 400;
        let after = star_field(&app, 60, 15);
        assert!(
            before
                .iter()
                .zip(after)
                .all(|(a, b)| a.index == b.index && a.x == b.x && a.y == b.y)
        );
        app.demo = false;
        let text = plain_frame(110, 34, &app);
        assert!(text.contains("awaiting sample"));
        assert!(!text.contains("idle"));
        app.activity = Some(Activity {
            state: "idle".into(),
            in_flight: 2,
            backend_jobs: 1,
            ..Activity::default()
        });
        let text = plain_frame(110, 34, &app);
        assert!(text.contains("idle · sampled"));
        assert!(text.contains("2 req · 1 jobs"));
    }

    #[test]
    fn help_and_input_are_readable_and_untrusted_controls_are_not_emitted() {
        let mut app = app();
        app.show_help = true;
        assert!(plain_frame(100, 30, &app).contains("Map keys"));
        app.show_help = false;
        app.editing = true;
        app.input = "hello\u{1b}[31m".into();
        let text = plain_frame(100, 30, &app);
        assert!(!text.contains('\u{1b}'));
        assert!(text.contains("hello[31m"));
        assert!(preview(100, 30, &app).unwrap().contains("\x1b[38;2;"));
    }

    #[test]
    fn touchstone_keys_name_map_and_scenes_at_every_footer_width() {
        let mut app = app();
        app.view = View::Touchstones;
        for width in [120, 60, 40, 24] {
            let text = plain_frame(width, 30, &app);
            let footer = text.lines().last().unwrap();
            assert!(footer.contains("t map"), "width {width}: {footer}");
            assert!(!footer.contains("t return"));
        }
        app.show_help = true;
        let text = plain_frame(100, 30, &app);
        assert!(text.contains("t              map"));
        assert!(text.contains("s              scenes"));
    }

    #[test]
    fn back_hint_is_visible_and_respects_open_panes() {
        let mut app = app();
        assert!(!plain_frame(100, 30, &app).contains("esc ← back"));
        let visit = app.visit();
        app.remember(visit);
        for (width, height) in [(100, 30), (60, 24)] {
            let text = plain_frame(width, height, &app);
            assert!(text.contains("esc ← back"));
            assert!(text.contains("/ search"));
        }
        app.open_edge_lens("demo-00".into(), app.graph.clone(), LensState::Ready);
        let text = plain_frame(60, 24, &app);
        assert!(text.contains("esc close"));
        assert!(text.contains("b ← back"));
        assert!(!text.contains("esc ← back"));
    }

    fn edge_app() -> App {
        let mut app = app();
        app.graph.nodes.truncate(3);
        app.graph.nodes[0].summary = "Selected memory".into();
        app.graph.nodes[1].summary = "The outgoing neighbor".into();
        app.graph.nodes[2].summary = "The incoming neighbor".into();
        app.graph.edges = vec![
            Edge {
                from: app.graph.nodes[0].id.clone(),
                to: app.graph.nodes[1].id.clone(),
                kind: "supports".into(),
                weight: 0.75,
            },
            Edge {
                from: app.graph.nodes[2].id.clone(),
                to: app.graph.nodes[0].id.clone(),
                kind: "contradicts".into(),
                weight: 0.25,
            },
        ];
        app.open_edge_lens("demo-00".into(), app.graph.clone(), LensState::Ready);
        app
    }

    #[test]
    fn edge_pane_names_both_directions_types_weights_and_neighbors() {
        let mut app = edge_app();
        let text = plain_frame(120, 34, &app);
        assert!(text.contains("→ The outgoing neighbor supports"));
        assert!(
            text.lines()
                .any(|line| line.contains("← The incoming") && line.contains("contradicts this"))
        );
        assert!(text.contains("weight 0.75"));
        assert!(text.contains("weight 0.25"));
        assert!(text.contains("The outgoing neighbor"));
        assert!(text.contains("The incoming"));
        // Medium terminals keep the sky. The unchanged lower pane scrolls.
        let text = plain_frame(76, 30, &app);
        assert!(text.contains("The outgoing neighbor"));
        assert!(text.contains("nodes ·"));
        app.scroll = 3;
        let text = plain_frame(76, 30, &app);
        assert!(text.contains("The incoming neighbor"));
        assert!(text.contains("The outgoing neighbor")); // Everything fits: no blank overscroll.
        assert_eq!(app.detail_viewport.borrow().unwrap().offset, 0);
        let text = plain_frame(76, 24, &app);
        assert!(text.contains("The incoming neighbor"));
        assert!(!text.contains("The outgoing neighbor"));
        assert!(text.contains("2 loaded edges"));
    }

    #[test]
    fn edge_pane_distinguishes_loading_empty_partial_and_failed_neighborhoods() {
        let mut app = edge_app();
        let lens = app.edge_lens.as_mut().unwrap();
        lens.graph.edges.clear();
        lens.state = LensState::Loading;
        let text = plain_frame(76, 30, &app);
        assert!(text.contains("Loading this memory's edges…"));
        assert!(!text.contains("No edges returned"));

        app.edge_lens.as_mut().unwrap().state = LensState::Ready;
        assert!(plain_frame(76, 30, &app).contains("No edges returned"));
        app.edge_lens.as_mut().unwrap().graph.partial = true;
        let text = plain_frame(76, 30, &app);
        assert!(text.contains("loaded edges · more may exist"));
        assert!(text.contains("No edges returned"));

        app.edge_lens.as_mut().unwrap().state = LensState::Unavailable;
        let text = plain_frame(76, 30, &app);
        assert!(text.contains("Edges unavailable"));
        assert!(!text.contains("No edges returned"));
    }

    #[test]
    fn masking_keeps_base_positions_and_reserves_hidden_numeric_labels() {
        let mut app = app();
        app.selected = 1;
        for (width, height) in [(80, 29), (74, 15), (48, 11)] {
            let before = placed_stars(&app, width, height);
            app.open_edge_lens(
                "demo-01".into(),
                crate::model::demo_neighborhood("demo-01"),
                LensState::Ready,
            );
            let during = placed_stars(&app, width, height);
            assert_eq!(during.len(), before.len());
            for (a, b) in before.iter().zip(&during) {
                assert_eq!(a.star.index, b.star.index);
                assert_eq!((a.star.x, a.star.y), (b.star.x, b.star.y));
                assert_eq!(a.marker, b.marker);
                assert_eq!(a.label, b.label);
            }
            assert!(
                during
                    .iter()
                    .filter(|p| app.node_visible(&app.graph.nodes[p.star.index].id))
                    .count()
                    <= 2
            );
            assert!(during.iter().any(|p| p.star.index == app.selected));
            app.close_edge_lens();
            let after = placed_stars(&app, width, height);
            assert!(
                before
                    .iter()
                    .zip(after)
                    .all(|(a, b)| a.marker == b.marker && a.label == b.label)
            );
        }
    }

    #[test]
    fn opening_and_closing_lens_preserves_actual_screen_cells_at_both_layouts() {
        let mut app = app();
        app.selected = 1;
        for (width, height) in [(120, 36), (76, 30)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| draw(frame, &app)).unwrap();
            let before = terminal.backend().buffer().clone();
            // The graph occupies the same rectangle before and during the lens.
            let sky_width = if width == 120 { 80 } else { 74 };
            let sky_height = if height == 36 { 29 } else { 15 };
            let placement = placed_stars(&app, sky_width, sky_height);
            app.open_edge_lens(
                "demo-01".into(),
                crate::model::demo_neighborhood("demo-01"),
                LensState::Ready,
            );
            terminal.draw(|frame| draw(frame, &app)).unwrap();
            let during = terminal.backend().buffer();
            for p in placement
                .iter()
                .filter(|p| app.node_visible(&app.graph.nodes[p.star.index].id))
            {
                let (x, y) = (p.marker.x + 1, p.marker.y + 2);
                assert!(matches!(before[(x, y)].symbol(), "◆" | "◇" | "•"));
                assert_eq!(before[(x, y)].symbol(), during[(x, y)].symbol());
                if let Some(label) = p.label {
                    for dx in 0..label.width {
                        let (x, y) = (label.x + dx + 1, label.y + 2);
                        let number = format!("{:02}", p.star.index + 1);
                        assert_eq!(
                            before[(x, y)].symbol(),
                            &number[usize::from(dx)..usize::from(dx) + 1]
                        );
                        assert_eq!(before[(x, y)].symbol(), during[(x, y)].symbol());
                    }
                }
            }
            app.close_edge_lens();
            terminal.draw(|frame| draw(frame, &app)).unwrap();
            assert_eq!(&before, terminal.backend().buffer());
        }
    }

    #[test]
    fn new_neighbor_slots_do_not_relayout_or_relabel_base_nodes() {
        let mut app = app();
        app.graph.focus = None;
        app.graph.edges.clear();
        app.selected = 1;
        let before = placed_stars(&app, 80, 29);
        let mut graph = crate::model::demo_neighborhood("demo-01");
        graph.nodes[0].id = "new-neighbor".into();
        graph.nodes[1].id = "demo-01".into();
        graph.edges[0].from = "new-neighbor".into();
        graph.edges[0].to = "demo-01".into();
        app.open_edge_lens("demo-01".into(), graph, LensState::Ready);
        let during = placed_stars(&app, 80, 29);
        assert_eq!(during.len(), before.len() + 1);
        for (a, b) in before.iter().zip(&during) {
            assert_eq!(a.star.index, b.star.index);
            assert_eq!(a.marker, b.marker);
            assert_eq!(a.label, b.label);
        }
        let new = during.last().unwrap();
        assert_eq!(new.star.index, app.graph.nodes.len());
        assert!(before.iter().all(|old| !old.marker.intersects(new.marker)
            && old.label.is_none_or(|rect| !rect.intersects(new.marker))));
        app.close_edge_lens();
        assert_eq!(app.scene_nodes().len(), 12);
    }
    fn large_world_visual_fixture() -> App {
        let mut app = app();
        app.graph = crate::model::Graph {
            nodes: (0..240)
                .map(|i| Node {
                    id: format!("fixture-{i:03}"),
                    summary: format!("Memory {i}: a thread worth following"),
                    tags: vec![format!("district-{}", i / 12)],
                    tags_complete: true,
                    ..Node::default()
                })
                .collect(),
            edges: (0..240)
                .filter(|i| i % 12 != 11)
                .map(|i| Edge {
                    from: format!("fixture-{i:03}"),
                    to: format!("fixture-{:03}", i + 1),
                    kind: "supports".into(),
                    weight: 0.75,
                })
                .collect(),
            ..crate::model::Graph::default()
        };
        app
    }

    fn assert_large_fixture_district_bounds(app: &App) {
        let points = app.layout.borrow_mut().world_positions(&app.graph);
        let groups = app.layout.borrow_mut().groups(&app.graph);
        assert_eq!(groups.groups.len(), 20);
        let bounds: Vec<_> = groups
            .groups
            .iter()
            .map(|group| {
                assert_eq!(group.members.len(), 12);
                let members: Vec<_> = group
                    .members
                    .iter()
                    .map(|id| {
                        points[app
                            .graph
                            .nodes
                            .iter()
                            .position(|node| &node.id == id)
                            .unwrap()]
                    })
                    .collect();
                (
                    members
                        .iter()
                        .map(|point| point.x)
                        .fold(f64::INFINITY, f64::min),
                    members
                        .iter()
                        .map(|point| point.x)
                        .fold(f64::NEG_INFINITY, f64::max),
                    members
                        .iter()
                        .map(|point| point.y)
                        .fold(f64::INFINITY, f64::min),
                    members
                        .iter()
                        .map(|point| point.y)
                        .fold(f64::NEG_INFINITY, f64::max),
                )
            })
            .collect();
        for (i, a) in bounds.iter().enumerate() {
            for (j, b) in bounds.iter().enumerate().skip(i + 1) {
                assert!(
                    a.1 < b.0 || b.1 < a.0 || a.3 < b.2 || b.3 < a.2,
                    "districts {i}/{j} overlap: {a:?} / {b:?}"
                );
            }
        }
    }

    #[test]
    fn disconnected_large_fixture_keeps_districts_visually_separate() {
        assert_large_fixture_district_bounds(&large_world_visual_fixture());
    }

    #[test]
    fn thin_connected_quotient_does_not_crush_district_interiors() {
        let mut app = large_world_visual_fixture();
        app.graph.edges.extend((1..20).map(|district| Edge {
            from: format!("fixture-{:03}", district * 12 - 1),
            to: format!("fixture-{:03}", district * 12),
            kind: "associative".into(),
            weight: 0.001,
        }));
        assert_large_fixture_district_bounds(&app);
    }

    #[test]
    #[ignore = "manual rendered-fixture capture; no live memory source"]
    fn capture_large_world_visual_fixture() {
        let path = std::env::var("MNEME_TUI_CAPTURE")
            .expect("set MNEME_TUI_CAPTURE for a disposable ANSI frame");
        std::fs::write(
            path,
            preview(120, 36, &large_world_visual_fixture()).unwrap(),
        )
        .unwrap();
    }
}
