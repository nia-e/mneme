//! A readable chronology of native episodes and their exact immutable editions.

use super::{
    BLUE, DIM, GOLD, GRID, INK, LILAC, MINT, NIGHT, PANEL, s, short, single, text_line, timestamp,
};
use crate::model::{App, clean};
use crate::scenes::{Scene, SceneAxis, SceneOccurrence};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::text::{Line, Text};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

pub(super) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    if area.width >= 88 {
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(56), Constraint::Percentage(44)])
            .split(area);
        chronology(frame, columns[0], app);
        inspector(frame, columns[1], app, true);
    } else if app.show_details {
        // A body deserves the full width on a narrow terminal; do not crush it
        // into a two-column postcard or move the user's chronological selection.
        inspector(frame, area, app, false);
    } else if area.width >= 60 && area.height >= 17 {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(64), Constraint::Percentage(36)])
            .split(area);
        chronology(frame, rows[0], app);
        inspector(frame, rows[1], app, false);
    } else {
        chronology(frame, area, app);
    }
}

fn occurrence(occurred: &SceneOccurrence) -> String {
    match occurred {
        SceneOccurrence::Unknown => "time unknown".into(),
        SceneOccurrence::Point { at } => timestamp(*at),
        SceneOccurrence::Range { start, end } => {
            format!("{} → {} (interval)", timestamp(*start), timestamp(*end))
        }
    }
}

fn list_time(scene: &Scene, axis: SceneAxis) -> String {
    match axis {
        SceneAxis::Recorded => timestamp(scene.recorded_at),
        SceneAxis::Occurred => match &scene.occurred {
            SceneOccurrence::Range { start, end } => {
                format!("interval {} → {}", timestamp(*start), timestamp(*end))
            }
            occurred => occurrence(occurred),
        },
    }
}

fn context_preview(scene: &Scene) -> String {
    let contexts = scene
        .occurrence_contexts
        .iter()
        .take(2)
        .map(|context| {
            format!(
                "{}:{}",
                single(&context.namespace),
                single(context.label.as_deref().unwrap_or(&context.key))
            )
        })
        .collect::<Vec<_>>()
        .join(" · ");
    if scene.occurrence_contexts.len() > 2 {
        format!("{contexts} · +{}", scene.occurrence_contexts.len() - 2)
    } else {
        contexts
    }
}

fn chronology(frame: &mut Frame<'_>, area: Rect, app: &App) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    text_line(
        frame,
        area,
        Line::from(vec![
            s("  SCENES", LILAC).add_modifier(Modifier::BOLD),
            s(format!("  /  {} ↓", app.scenes.axis.as_str()), DIM),
        ]),
    );
    if area.height < 2 {
        return;
    }
    let query = if app.scene_query.is_empty() {
        "Newest first · o changes time axis".into()
    } else {
        format!("Find: {} · o changes time axis", single(&app.scene_query))
    };
    text_line(
        frame,
        Rect::new(area.x + 2, area.y + 1, area.width.saturating_sub(2), 1),
        Line::from(s(query, DIM)),
    );
    if area.height < 4 {
        return;
    }
    let field = Rect::new(
        area.x,
        area.y + 3,
        area.width,
        area.height.saturating_sub(4),
    );
    if app.scenes.items.is_empty() {
        let message = if app.busy {
            "Opening the scene lens…"
        } else if !app.scenes_loaded && app.error.is_some() {
            "Scenes unavailable · r retries this source"
        } else if !app.scenes_loaded {
            "Scenes not loaded · r reads this source"
        } else if !app.scene_query.trim().is_empty() {
            "No scenes match this summary search.
/ try another cue"
        } else if app.scenes.axis == SceneAxis::Occurred {
            "No scenes with known occurrence time.
o shows recorded scenes"
        } else {
            "No scenes returned. / find an episode"
        };
        frame.render_widget(
            Paragraph::new(message)
                .style(Style::default().fg(DIM))
                .wrap(Wrap { trim: true }),
            Rect::new(
                field.x + 2,
                field.y,
                field.width.saturating_sub(4),
                field.height,
            ),
        );
    } else {
        // Real time is textual, not a fake proportional timeline: unknown dates
        // and intervals remain visible without inventing positions or duration.
        let row_height: usize = if field.height >= 8 { 4 } else { 2 };
        let rows = (usize::from(field.height) / row_height).max(1);
        let start = app
            .scene_selected
            .saturating_sub(rows / 2)
            .min(app.scenes.items.len().saturating_sub(rows));
        for (row, (index, scene)) in app
            .scenes
            .items
            .iter()
            .enumerate()
            .skip(start)
            .take(rows)
            .enumerate()
        {
            let chosen = index == app.scene_selected;
            let y = field.y + (row * row_height) as u16;
            let height = (row_height as u16).min(field.bottom().saturating_sub(y));
            let rect = Rect::new(field.x, y, field.width, height);
            frame.render_widget(
                Block::default().style(Style::default().bg(if chosen { PANEL } else { NIGHT })),
                rect,
            );
            text_line(
                frame,
                rect,
                Line::from(vec![
                    s(if chosen { "› " } else { "  " }, MINT),
                    s("│ ", if chosen { MINT } else { GRID }),
                    s(
                        short(
                            &list_time(scene, app.scenes.axis),
                            usize::from(rect.width.saturating_sub(4)),
                        ),
                        if chosen { BLUE } else { DIM },
                    ),
                ]),
            );
            if height > 1 {
                text_line(
                    frame,
                    Rect::new(rect.x + 4, y + 1, rect.width.saturating_sub(4), 1),
                    Line::from(s(
                        short(&scene.summary, usize::from(rect.width.saturating_sub(5))),
                        if chosen { MINT } else { INK },
                    )),
                );
            }
            if row_height == 4 && height > 2 {
                let contexts = context_preview(scene);
                let note = if contexts.is_empty() {
                    format!("r{} · no occurrence context", scene.revision)
                } else {
                    format!("r{} · {contexts}", scene.revision)
                };
                text_line(
                    frame,
                    Rect::new(rect.x + 4, y + 2, rect.width.saturating_sub(4), 1),
                    Line::from(s(
                        short(&note, usize::from(rect.width.saturating_sub(5))),
                        DIM,
                    )),
                );
            }
        }
    }
    let selected = if app.scenes.items.is_empty() {
        0
    } else {
        app.scene_selected + 1
    };
    let coverage = if !app.scenes_loaded {
        if app.busy {
            "  Awaiting scene response".into()
        } else if app.error.is_some() {
            "  Scenes unavailable · no page loaded".into()
        } else {
            "  Scenes not loaded".into()
        }
    } else {
        format!(
            "  {selected}/{} scenes · {}{}",
            app.scenes.items.len(),
            if app.scenes.next.is_some() {
                "more available"
            } else {
                "loaded page"
            },
            if app.scenes.partial {
                " · partial"
            } else {
                ""
            }
        )
    };
    text_line(
        frame,
        Rect::new(area.x, area.bottom() - 1, area.width, 1),
        Line::from(s(
            coverage,
            if app.scenes.partial || app.scenes.next.is_some() {
                GOLD
            } else {
                DIM
            },
        )),
    );
}

fn selected_scene(app: &App) -> Option<(&Scene, bool)> {
    let selected = app.scenes.items.get(app.scene_selected)?;
    match &app.scene_detail {
        // A refresh may discover a new head after this edition was opened.
        // Root identity pins the inspector; the exact body stays historical.
        Some(detail) if detail.episode_id == selected.episode_id => Some((detail, true)),
        _ => Some((selected, false)),
    }
}

fn inspector(frame: &mut Frame<'_>, area: Rect, app: &App, divider: bool) {
    if area.width < 4 || area.height == 0 {
        return;
    }
    if divider {
        frame.render_widget(
            Block::default()
                .borders(Borders::LEFT)
                .border_style(Style::default().fg(GRID)),
            area,
        );
    }
    let area = Rect::new(
        area.x + 2,
        area.y,
        area.width.saturating_sub(3),
        area.height,
    );
    let Some((scene, exact)) = selected_scene(app) else {
        frame.render_widget(
            Paragraph::new(
                "An account of what happened.\n\n/ find a scene · o recorded / occurred",
            )
            .style(Style::default().fg(DIM))
            .wrap(Wrap { trim: true }),
            area,
        );
        return;
    };
    let mut lines = vec![
        Line::from(s(single(&scene.summary), MINT).add_modifier(Modifier::BOLD)),
        Line::from(s(
            format!(
                "revision {}  /  {}",
                scene.revision,
                if exact {
                    "exact edition loaded"
                } else {
                    "edition header"
                }
            ),
            LILAC,
        )),
        Line::default(),
        Line::from(vec![
            s("occurred  ", DIM),
            s(occurrence(&scene.occurred), BLUE),
        ]),
        Line::from(vec![
            s("recorded  ", DIM),
            s(timestamp(scene.recorded_at), INK),
        ]),
    ];
    if scene.edition_recorded_at != scene.recorded_at {
        lines.push(Line::from(vec![
            s("edition recorded  ", DIM),
            s(timestamp(scene.edition_recorded_at), INK),
        ]));
    }
    if !scene.occurrence_contexts.is_empty() {
        lines.push(Line::default());
        lines.push(Line::from(s("OCCURRENCE CONTEXT", DIM)));
        for context in &scene.occurrence_contexts {
            let identity = format!("{}:{}", single(&context.namespace), single(&context.key));
            lines.push(Line::from(s(identity, LILAC)));
            if let Some(label) = &context.label {
                if label != &context.key {
                    lines.push(Line::from(s(format!("  {}", single(label)), INK)));
                }
            }
        }
    }
    if let Some(thread) = &scene.thread {
        lines.push(Line::from(vec![
            s("thread  ", DIM),
            s(single(thread), LILAC),
        ]));
    }
    lines.push(Line::default());
    if !app.show_details {
        if scene.current_edition_id != scene.edition_id {
            lines.push(Line::from(s(
                "A later edition exists; this selection stays exact.",
                GOLD,
            )));
        }
        lines.push(Line::from(s("↵ / i  open this edition", DIM)));
    } else {
        if !exact {
            lines.push(Line::from(s(
                if app.busy {
                    "Reading this exact edition…"
                } else {
                    "Body not loaded · Enter retries this exact edition"
                },
                if app.busy { DIM } else { GOLD },
            )));
        } else if scene.body.is_empty() {
            lines.push(Line::from(s("This edition has an empty body.", DIM)));
        } else {
            lines.extend(
                clean(&scene.body)
                    .lines()
                    .map(|line| Line::from(s(line, INK))),
            );
        }
        if exact && scene.body_partial {
            lines.push(Line::from(s("… body excerpt · byte-bounded", GOLD)));
        }
        lines.push(Line::default());
        lines.push(Line::from(s("EXACT EDITION", DIM)));
        lines.push(Line::from(vec![
            s("episode  ", DIM),
            s(single(&scene.episode_id), INK),
        ]));
        lines.push(Line::from(vec![
            s("edition  ", DIM),
            s(single(&scene.edition_id), INK),
        ]));
        if scene.current_edition_id == scene.edition_id {
            lines.push(Line::from(s("This is the current edition.", DIM)));
        } else {
            lines.push(Line::from(s(
                "A later edition exists; the body above is not replaced.",
                GOLD,
            )));
            lines.push(Line::from(vec![
                s("current  ", DIM),
                s(single(&scene.current_edition_id), GOLD),
            ]));
        }
        lines.push(Line::default());
        lines.push(Line::from(s("CAPTURE PROVENANCE", DIM)));
        if !exact {
            lines.push(Line::from(s("Not loaded with this edition header.", DIM)));
        } else {
            lines.extend(source_lines(&scene.source));
        }
    }
    if let Some(cue) = &app.scenes.cue {
        lines.push(Line::default());
        lines.push(Line::from(s(single(cue), GOLD)));
    }
    if app.show_details {
        crate::detail_scroll::draw(frame, area, app, lines, Style::default());
    } else {
        frame.render_widget(
            Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }),
            area,
        );
    }
}

fn source_lines(source: &str) -> Vec<Line<'static>> {
    if let Ok(serde_json::Value::Object(fields)) = serde_json::from_str(source) {
        fields
            .into_iter()
            .map(|(key, value)| {
                let value = value
                    .as_str()
                    .map(single)
                    .unwrap_or_else(|| single(&value.to_string()));
                Line::from(vec![s(format!("{}  ", single(&key)), DIM), s(value, INK)])
            })
            .collect()
    } else {
        vec![Line::from(s(
            if source.is_empty() {
                "Not supplied by this source".into()
            } else {
                single(source)
            },
            DIM,
        ))]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::View;
    use crate::scenes::{SceneContext, ScenePage};
    use ratatui::{Terminal, backend::TestBackend};

    fn scene_app() -> App {
        let scene = Scene {
            episode_id: "episode-root".into(),
            edition_id: "historical-edition".into(),
            current_edition_id: "current-edition".into(),
            revision: 2,
            summary: "A tunnel flooded; we changed the route".into(),
            occurred: SceneOccurrence::Range {
                start: 1_759_316_400_000,
                end: 1_759_320_000_000,
            },
            recorded_at: 1_759_323_600_000,
            edition_recorded_at: 1_759_327_200_000,
            occurrence_contexts: vec![SceneContext {
                namespace: "place".into(),
                key: "north-tunnel".into(),
                label: Some("Northern tunnel".into()),
            }],
            body: "The historical body, not the current account.".into(),
            body_partial: true,
            source: r#"{"kind":"session","key":"capture-9"}"#.into(),
            thread: Some("expedition".into()),
        };
        let mut app = App::new(vec![], String::new(), true);
        app.view = View::Scenes;
        app.scenes_loaded = true;
        app.scenes = ScenePage {
            items: vec![scene.clone()],
            axis: SceneAxis::Occurred,
            partial: true,
            next: Some("cursor".into()),
            cue: None,
        };
        app.scene_detail = Some(scene);
        app
    }
    fn plain(width: u16, height: u16, app: &App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| super::super::draw(frame, app))
            .unwrap();
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
    fn times_are_checked_utc_milliseconds_and_unknown_is_not_zero() {
        assert_eq!(timestamp(0), "1970-01-01 00:00 UTC");
        assert_eq!(timestamp(1_000), "1970-01-01 00:00:01 UTC");
        assert_eq!(timestamp(1_001), "1970-01-01 00:00:01.001 UTC");
        assert_eq!(timestamp(60_000), "1970-01-01 00:01 UTC");
        assert!(timestamp(u64::MAX).starts_with("unsupported time"));
        assert_eq!(occurrence(&SceneOccurrence::Unknown), "time unknown");
        assert!(
            occurrence(&SceneOccurrence::Range {
                start: 0,
                end: 60_000
            })
            .contains("interval")
        );
    }

    #[test]
    fn unavailable_or_unread_scenes_are_not_a_successful_empty_page() {
        let mut app = App::new(vec![], String::new(), false);
        app.view = View::Scenes;
        let text = plain(120, 36, &app);
        assert!(text.contains("Scenes not loaded"));
        assert!(!text.contains("No scenes returned"));
        assert!(!text.contains("loaded page"));
        app.error = Some("Owner does not advertise episode reads".into());
        let text = plain(120, 36, &app);
        assert!(text.contains("Scenes unavailable"));
        assert!(!text.contains("No scenes returned"));
        assert!(!text.contains("0/0 scenes"));
        app.error = None;
        app.busy = true;
        assert!(plain(120, 36, &app).contains("Awaiting scene response"));
        app.busy = false;
        app.scenes_loaded = true;
        let text = plain(120, 36, &app);
        assert!(text.contains("No scenes returned"));
        assert!(text.contains("0/0 scenes · loaded page"));
    }

    #[test]
    fn empty_occurrence_index_explains_excluded_unknowns_but_search_does_not() {
        let mut app = scene_app();
        app.scenes.items.clear();
        app.scene_detail = None;
        app.scenes.next = None;
        app.scenes.partial = false;
        for (width, height) in [(120, 36), (40, 16)] {
            let text = plain(width, height, &app);
            // Wrapping must preserve the meaning on a narrow terminal.
            let words = text.split_whitespace().collect::<Vec<_>>().join(" ");
            assert!(words.contains("No scenes with known occurrence time."));
            assert!(words.contains("o shows recorded scenes"));
            assert!(!words.contains("No scenes returned"));
        }
        app.scene_query = "tunnel".into();
        let text = plain(120, 36, &app);
        assert!(text.contains("No scenes match this summary search"));
        assert!(!text.contains("known occurrence time"));
        // Lexical search can return an unknown occurrence even on this axis.
        let mut unknown = scene_app().scenes.items.remove(0);
        unknown.occurred = SceneOccurrence::Unknown;
        app.scenes.items.push(unknown);
        let text = plain(120, 36, &app);
        assert!(text.contains("time unknown"));
        assert!(!text.contains("No scenes with known"));
    }

    #[test]
    fn chronological_view_keeps_context_and_capture_and_exact_editions_distinct() {
        let mut app = scene_app();
        let text = plain(120, 40, &app);
        assert!(text.contains("SCENES"));
        assert!(text.contains("occurred ↓"));
        assert!(text.contains("more available"));
        assert!(text.contains("Northern tunnel"));
        app.show_details = true;
        let text = plain(140, 44, &app);
        assert!(text.contains("OCCURRENCE CONTEXT"));
        assert!(text.contains("CAPTURE PROVENANCE"));
        assert!(text.contains("historical-edition"));
        assert!(text.contains("current-edition"));
        assert!(text.contains("The historical body, not the current account."));
        assert!(text.contains("body excerpt"));
        assert!(text.contains("capture-9"));
    }

    #[test]
    fn stale_detail_never_substitutes_a_different_episode() {
        let mut app = scene_app();
        app.show_details = true;
        app.scene_detail.as_mut().unwrap().episode_id = "wrong-episode".into();
        let text = plain(120, 40, &app);
        assert!(text.contains("Body not loaded"));
        assert!(!text.contains("The historical body"));
        assert!(!text.contains("capture-9"));
    }

    #[test]
    fn refresh_new_head_keeps_an_already_read_historical_body_visible() {
        let mut app = scene_app();
        app.show_details = true;
        app.scenes.items[0].edition_id = "current-edition".into();
        app.scenes.items[0].summary = "A newer account of this expedition".into();
        let text = plain(140, 44, &app);
        assert!(text.contains("The historical body, not the current account."));
        assert!(text.contains("historical-edition"));
        assert!(text.contains("current-edition"));
        assert!(text.contains("A later edition exists"));
        assert!(!text.contains("Body not loaded"));
    }

    #[test]
    fn scene_browser_handles_tiny_narrow_scrolled_and_untrusted_text() {
        let mut app = scene_app();
        app.scenes.items[0].summary.push_str("\u{1b}[31m");
        for details in [false, true] {
            app.show_details = details;
            for (width, height) in [
                (140, 44),
                (90, 30),
                (76, 30),
                (40, 16),
                (24, 9),
                (10, 3),
                (1, 1),
                (0, 0),
            ] {
                let text = plain(width, height, &app);
                assert!(!text.contains('\u{1b}'));
                if width >= 76 && !details {
                    assert!(text.contains("SCENES"));
                }
            }
        }
        app.scroll = usize::MAX;
        let _ = plain(76, 30, &app);
    }

    #[test]
    fn scene_preview_uses_the_production_ansi_renderer() {
        let mut app = scene_app();
        app.scenes = crate::scenes::demo_scenes(SceneAxis::Recorded);
        let preview = super::super::preview(120, 36, &app).unwrap();
        assert!(preview.contains("\x1b[38;2;"));
        assert!(preview.contains("SCENES"));
    }
}
