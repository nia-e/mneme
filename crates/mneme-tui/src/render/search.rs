//! A ranked retrieval list beside, never in place of, the retained graph.

use super::{DIM, GOLD, GRID, INK, LILAC, MINT, NIGHT, PANEL, ROSE, s, short, single, text_line};
use crate::model::{App, LensState, SearchResults, clean};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style, Stylize},
    text::Line,
    widgets::{Block, Borders, Paragraph, Wrap},
};

pub(super) fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let Some(search) = &app.search else { return };
    if search.detail.is_some() && area.width >= 90 {
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(48), Constraint::Percentage(52)])
            .split(area);
        results(frame, columns[0], search);
        memory(frame, columns[1], app, search, true);
    } else if search.detail.is_some() {
        memory(frame, area, app, search, false);
    } else {
        results(frame, area, search);
    }
}

fn results(frame: &mut Frame<'_>, area: Rect, search: &SearchResults) {
    if area.is_empty() {
        return;
    }
    let inset = area.width.min(2);
    let inner = Rect::new(area.x + inset, area.y, area.width - inset, area.height);
    text_line(
        frame,
        inner,
        Line::from(s(
            short(
                &format!("Search: {}", single(&search.query)),
                usize::from(inner.width),
            ),
            LILAC,
        )),
    );
    if area.height < 2 {
        return;
    }
    let coverage = if area.height < 4 && search.error.is_some() {
        // There is no separate error row at this height. Keep its route to the
        // complete diagnostic visible instead of silently dropping the failure.
        "Failed · C details".to_owned()
    } else {
        match search.state {
            LensState::Loading => "Searching… · map retained".to_owned(),
            LensState::Unavailable => "Search failed · map retained".to_owned(),
            LensState::Ready => format!(
                "{} ranked hits · max {}{}",
                search.page.nodes.len(),
                crate::data::QUERY_NODES,
                if search.page.partial {
                    " · partial"
                } else {
                    ""
                },
            ),
        }
    };
    text_line(
        frame,
        Rect::new(inner.x, inner.y + 1, inner.width, 1),
        Line::from(s(
            coverage,
            if search.page.partial || search.state == LensState::Unavailable {
                GOLD
            } else {
                DIM
            },
        )),
    );
    if area.height < 4 {
        return;
    }
    let bottom_rows = if search.error.is_some() || search.opening.is_some() {
        2
    } else {
        1
    };
    let field = Rect::new(
        area.x,
        area.y + 3,
        area.width,
        area.height.saturating_sub(3 + bottom_rows),
    );
    if search.page.nodes.is_empty() {
        let message = match search.state {
            LensState::Loading => "Finding relevant memories…",
            LensState::Unavailable => "No result page loaded.\nr retries · Esc returns to the map",
            LensState::Ready if search.page.partial => {
                "No hits in this partial response.\n/ try another query · Esc returns to the map"
            }
            LensState::Ready => "No hits returned.\n/ try another query · Esc returns to the map",
        };
        frame.render_widget(
            Paragraph::new(message)
                .style(Style::default().fg(DIM))
                .wrap(Wrap { trim: false }),
            Rect::new(inner.x, field.y, inner.width, field.height),
        );
    } else if !field.is_empty() {
        // Rank order is the query response order; neither layout nor IDs reorder it.
        let row_height = if field.height >= 10 { 3 } else { 2 };
        let rows = (usize::from(field.height) / row_height).max(1);
        let start = search
            .selected
            .saturating_sub(rows / 2)
            .min(search.page.nodes.len().saturating_sub(rows));
        for (row, (index, node)) in search
            .page
            .nodes
            .iter()
            .enumerate()
            .skip(start)
            .take(rows)
            .enumerate()
        {
            let y = field.y + (row * row_height) as u16;
            let height = (row_height as u16).min(field.bottom().saturating_sub(y));
            let rect = Rect::new(field.x, y, field.width, height);
            let selected = index == search.selected;
            frame.render_widget(
                Block::default().style(Style::default().bg(if selected { PANEL } else { NIGHT })),
                rect,
            );
            search.hits.borrow_mut().push((rect, index));
            let prefix = format!("{} {:02}  ", if selected { "›" } else { " " }, index + 1);
            let width = usize::from(rect.width).saturating_sub(Line::raw(&prefix).width());
            let title = if node.summary.is_empty() {
                "Untitled memory"
            } else {
                &node.summary
            };
            text_line(
                frame,
                rect,
                Line::from(vec![
                    s(prefix, if selected { MINT } else { DIM }),
                    s(short(title, width), if selected { MINT } else { INK }),
                ]),
            );
            if height > 1 {
                let tags = node
                    .tags
                    .iter()
                    .map(|tag| format!("#{}", single(tag)))
                    .collect::<Vec<_>>()
                    .join(" ");
                text_line(
                    frame,
                    Rect::new(rect.x + inset, y + 1, rect.width - inset, 1),
                    Line::from(s(short(&tags, usize::from(rect.width - inset)), DIM)),
                );
            }
        }
    }
    if let Some(error) = &search.error {
        text_line(
            frame,
            Rect::new(inner.x, area.bottom() - 2, inner.width, 1),
            Line::from(s(
                short(
                    &format!("! C details · {}", single(error)),
                    usize::from(inner.width),
                ),
                ROSE,
            )),
        );
    } else if search.opening.is_some() {
        text_line(
            frame,
            Rect::new(inner.x, area.bottom() - 2, inner.width, 1),
            Line::from(s("Opening selected memory…", DIM)),
        );
    }
    let note = if search.page.partial {
        "Partial results · not complete store coverage"
    } else {
        "Ranked retrieval · map unchanged"
    };
    text_line(
        frame,
        Rect::new(inner.x, area.bottom() - 1, inner.width, 1),
        Line::from(s(note, if search.page.partial { GOLD } else { DIM })),
    );
}

fn memory(frame: &mut Frame<'_>, area: Rect, app: &App, search: &SearchResults, divider: bool) {
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
    let Some(graph) = &search.detail else { return };
    let Some(node) = graph
        .nodes
        .iter()
        .find(|node| Some(node.id.as_str()) == graph.focus.as_deref())
    else {
        return;
    };
    let mut lines = vec![
        Line::from(s(single(&node.summary), MINT).add_modifier(Modifier::BOLD)),
        Line::from(s(
            node.tags
                .iter()
                .map(|tag| format!("#{}", single(tag)))
                .collect::<Vec<_>>()
                .join(" "),
            LILAC,
        )),
        Line::default(),
    ];
    if node.body.is_empty() {
        lines.push(Line::from(s("This memory has no body text.", DIM)));
    } else {
        lines.extend(
            clean(&node.body)
                .lines()
                .map(|line| Line::from(s(line, INK))),
        );
    }
    if graph.partial {
        lines.push(Line::default());
        lines.push(Line::from(s(
            "Partial read · body or links may be incomplete",
            GOLD,
        )));
    }
    lines.extend([
        Line::default(),
        Line::from(s("── PROVENANCE", DIM)),
        Line::from(s(
            if node.provenance.is_empty() {
                "Not supplied by this source".into()
            } else {
                single(&node.provenance)
            },
            DIM,
        )),
        Line::from(s(format!("id  {}", single(&node.id)), DIM)),
    ]);
    crate::detail_scroll::draw(frame, area, app, lines, Style::default());
}
