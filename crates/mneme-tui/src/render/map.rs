//! The stable map: cell-correct geometry, links, glyphs and an accessible index.

use super::{
    BLUE, DIM, GOLD, GRID, INK, LILAC, MINT, NIGHT, PANEL, card_title, islands, memory, s, short,
    text_line,
};
use crate::layout::{Point, stable_hash};
use crate::model::{App, Edge};
use ratatui::widgets::canvas::{Canvas, Circle, Context, Line as Stroke, Points};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    symbols::Marker,
    text::Line,
    widgets::{Paragraph, Wrap},
};
use std::collections::HashMap;

pub(super) fn draw(frame: &mut Frame<'_>, body: Rect, app: &App) {
    app.layout.borrow_mut().camera.index_only = false;
    if body.width >= 84 {
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(68), Constraint::Percentage(32)])
            .split(body);
        constellation(frame, columns[0], app);
        let right = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(4)])
            .split(columns[1]);
        memory::inspector(frame, right[0], app);
        index(frame, right[1], app);
    } else if body.width >= 53 && body.height >= 12 {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(68), Constraint::Percentage(32)])
            .split(body);
        constellation(frame, rows[0], app);
        if app.edge_lens.is_some() {
            // Keep the sky's exact rectangle; give the existing lower pane to
            // readable edge text instead of replacing the whole constellation.
            memory::inspector(frame, rows[1], app);
        } else {
            let columns = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(43), Constraint::Percentage(57)])
                .split(rows[1]);
            index(frame, columns[0], app);
            memory::inspector(frame, columns[1], app);
        }
    } else {
        app.layout.borrow_mut().camera.index_only = true;
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(43), Constraint::Percentage(57)])
            .split(body);
        index(frame, rows[0], app);
        memory::inspector(frame, rows[1], app);
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Star {
    pub(crate) index: usize,
    pub(crate) x: f64,
    pub(crate) y: f64,
}

/// Stable world positions for every loaded node, including temporary lens cards.
/// Lens additions seek vacant world-space slots; they never displace base nodes.
pub(crate) fn world_stars(app: &App) -> Vec<Star> {
    let positions = app.layout.borrow_mut().world_positions(&app.graph);
    let mut stars: Vec<_> = positions
        .into_iter()
        .enumerate()
        .map(|(index, p)| Star {
            index,
            x: p.x,
            y: p.y,
        })
        .collect();
    let nodes = app.scene_nodes();
    let anchor = app
        .edge_lens
        .as_ref()
        .and_then(|lens| {
            stars
                .iter()
                .find(|star| nodes[star.index].id == lens.anchor)
        })
        .copied();
    let center = anchor.unwrap_or(Star {
        index: 0,
        x: 0.0,
        y: 0.0,
    });
    for (index, node) in nodes.iter().enumerate().skip(app.graph.nodes.len()) {
        let angle = (stable_hash(&node.id) % 10000) as f64 / 10000.0 * std::f64::consts::TAU;
        for step in 0..stars.len().saturating_mul(8) + 96 {
            let radius = 12.0 + (step / 12) as f64 * 8.0;
            let angle = angle + (step % 12) as f64 * std::f64::consts::TAU / 12.0;
            let star = Star {
                index,
                x: center.x + radius * angle.cos(),
                y: center.y + radius * angle.sin(),
            };
            if stars
                .iter()
                .all(|other| (star.x - other.x).hypot(star.y - other.y) >= 7.0)
            {
                stars.push(star);
                break;
            }
        }
    }
    stars
}

fn projected_stars(app: &App, width: u16, height: u16) -> Vec<Star> {
    let stars = world_stars(app);
    let points: Vec<_> = stars
        .iter()
        .map(|star| Point {
            x: star.x,
            y: star.y,
        })
        .collect();
    let selected = app.selected_node().and_then(|node| {
        stars
            .iter()
            .find(|star| star.index == app.selected)
            .map(|star| {
                (
                    node.id.as_str(),
                    Point {
                        x: star.x,
                        y: star.y,
                    },
                )
            })
    });
    let projected = app
        .layout
        .borrow_mut()
        .project(&app.graph, &points, selected, width, height);
    stars
        .into_iter()
        .zip(projected)
        .map(|(star, point)| Star {
            index: star.index,
            x: point.x,
            y: point.y,
        })
        .collect()
}

/// Cards are fetched in bounded batches; topology and navigation already know
/// these IDs. This margin includes near-offscreen nodes without crawling cards.
pub(crate) fn near_viewport_ids(app: &App, margin: f64) -> Vec<String> {
    let (width, height) = app.layout.borrow().camera.viewport;
    let nodes = app.scene_nodes();
    if app.layout.borrow().camera.index_only {
        let visible: Vec<_> = nodes
            .iter()
            .filter(|node| app.node_visible_node(node))
            .collect();
        let camera = &app.layout.borrow().camera;
        let first = camera
            .index_ids
            .first()
            .and_then(|id| visible.iter().position(|node| &node.id == id))
            .unwrap_or(0);
        let rows = camera.index_ids.len().max(1);
        let padding = (rows as f64 * margin).ceil() as usize;
        return visible
            .into_iter()
            .skip(first.saturating_sub(padding))
            .take(rows.saturating_add(2 * padding))
            .map(|node| node.id.clone())
            .collect();
    }
    let margin_x = f64::from(width) * margin;
    let margin_y = 2.0 * f64::from(height) * margin;
    projected_stars(app, width, height)
        .into_iter()
        .filter(|star| {
            app.node_selectable(nodes[star.index])
                && (star.index == app.selected
                    || (star.x >= -margin_x
                        && star.x <= f64::from(width) + margin_x
                        && star.y >= -margin_y
                        && star.y <= 2.0 * f64::from(height) + margin_y
                        && app.node_visible_node(nodes[star.index])))
        })
        .map(|star| nodes[star.index].id.clone())
        .collect()
}

pub(super) fn star_field(app: &App, width: u16, height: u16) -> Vec<Star> {
    projected_stars(app, width, height)
        .into_iter()
        .filter(|star| {
            width > 0
                && height > 0
                && star.x >= 0.0
                && star.x < f64::from(width)
                && star.y > 0.0
                && star.y <= 2.0 * f64::from(height)
        })
        .collect()
}

#[derive(Clone, Copy, Debug)]
pub(super) struct PlacedStar {
    pub(super) star: Star,
    pub(super) marker: Rect,
    pub(super) label: Option<Rect>,
}

/// Use the same 2×4 dot raster projection as Ratatui's Braille canvas, not
/// independently rounded terminal-cell coordinates (which drift by a row).
pub(super) fn marker_rect(star: Star, width: u16, height: u16) -> Rect {
    if width == 0 || height == 0 {
        return Rect::default();
    }
    let w = f64::from(width);
    let h = f64::from(height) * 2.0;
    let dot_x = (star.x.clamp(0.0, w) * (w * 2.0 - 1.0) / w).round() as u32;
    let dot_y = ((h - star.y.clamp(0.0, h)) * (h * 2.0 - 1.0) / h).round() as u32;
    Rect::new((dot_x / 2) as u16, (dot_y / 4) as u16, 1, 1)
}

/// Draw links and halos around the center of the chosen glyph cell. Sharing
/// this inverse also aligns neighbors introduced by a temporary edge lens.
pub(super) fn cell_center(index: usize, marker: Rect, width: u16, height: u16) -> Star {
    let w = f64::from(width.max(1));
    let h = f64::from(height.max(1)) * 2.0;
    Star {
        index,
        x: (f64::from(marker.x) * 2.0 + 0.5) * w / (w * 2.0 - 1.0),
        y: h - (f64::from(marker.y) * 4.0 + 1.5) * h / (h * 2.0 - 1.0),
    }
}

fn label_rect(
    index: usize,
    marker: Rect,
    width: u16,
    height: u16,
    occupied: &[Rect],
) -> Option<Rect> {
    let label_width = (format!("{:02}", index + 1).len() as u16).min(width);
    let (px, py) = (marker.x, marker.y);
    [
        Rect::new(px.saturating_add(2), py, label_width, 1),
        Rect::new(px.saturating_sub(label_width + 1), py, label_width, 1),
        Rect::new(
            px.saturating_sub(label_width / 2),
            py.saturating_add(1),
            label_width,
            1,
        ),
        Rect::new(
            px.saturating_sub(label_width / 2),
            py.saturating_sub(1),
            label_width,
            1,
        ),
    ]
    .into_iter()
    .find(|candidate| {
        candidate.right() <= width
            && candidate.bottom() <= height
            && !occupied
                .iter()
                .any(|previous| previous.intersects(*candidate))
    })
}

/// Resolve the entire base scene, including hidden labels, before applying the
/// lens. Otherwise a label can jump into space freed by a hidden neighbor.
pub(super) fn placed_stars(app: &App, width: u16, height: u16) -> Vec<PlacedStar> {
    let mut stars = star_field(app, width, height);
    // Labels have stable priority by identity, never selection. An ordinary
    // selection cannot move another marker or make its numeric label jump.
    stars.sort_by_key(|star| star.index);
    let mut occupied: Vec<_> = stars
        .iter()
        .map(|star| marker_rect(*star, width, height))
        .collect();
    let mut placed = Vec::new();
    for star in stars {
        let marker = marker_rect(star, width, height);
        let label = label_rect(star.index, marker, width, height, &occupied);
        occupied.extend(label);
        placed.push(PlacedStar {
            star: cell_center(star.index, marker, width, height),
            marker,
            label,
        });
    }
    placed
}

pub(super) fn link_color(kind: &str, bright: bool) -> Color {
    match (kind, bright) {
        ("derived_from" | "derivation" | "supports" | "causal", true) => MINT,
        ("transition" | "sequence" | "temporal", true) => BLUE,
        ("supersedes" | "contradicts" | "conflicts", true) => GOLD,
        (_, true) => LILAC,
        ("derived_from" | "derivation" | "supports" | "causal", false) => Color::Rgb(54, 108, 97),
        ("transition" | "sequence" | "temporal", false) => Color::Rgb(52, 89, 117),
        ("supersedes" | "contradicts" | "conflicts", false) => Color::Rgb(111, 92, 61),
        (_, false) => Color::Rgb(94, 79, 121),
    }
}

#[cfg(test)]
pub(super) fn draw_edges(
    ctx: &mut Context<'_>,
    edges: &[Edge],
    by_id: &HashMap<&str, Star>,
    selected_id: Option<&str>,
) {
    draw_edges_clipped(ctx, edges, by_id, selected_id, None);
}

fn draw_edges_clipped(
    ctx: &mut Context<'_>,
    edges: &[Edge],
    by_id: &HashMap<&str, Star>,
    selected_id: Option<&str>,
    bounds: Option<(f64, f64)>,
) {
    // Terminal cells have one foreground color, not alpha. Paint quiet links
    // first, then selected ones, on the same layer: their dots accumulate while
    // the bright color wins at crossings. Separate layers would erase dim dots.
    for bright in [false, true] {
        for edge in edges {
            let selected =
                selected_id == Some(edge.from.as_str()) || selected_id == Some(edge.to.as_str());
            if selected != bright {
                continue;
            }
            if let (Some(a), Some(b)) = (by_id.get(edge.from.as_str()), by_id.get(edge.to.as_str()))
            {
                let segment = bounds.map_or(Some((*a, *b)), |(width, height)| {
                    clip_segment(*a, *b, width, height)
                });
                let Some((a, b)) = segment else {
                    continue;
                };
                ctx.draw(&Stroke {
                    x1: a.x,
                    y1: a.y,
                    x2: b.x,
                    y2: b.y,
                    color: link_color(&edge.kind, bright),
                });
            }
        }
    }
    ctx.layer();
}

/// Clip before rasterizing: an offscreen world-spanning edge must not ask the
/// Braille painter to walk thousands of invisible cells on every repaint.
fn clip_segment(a: Star, b: Star, width: f64, height: f64) -> Option<(Star, Star)> {
    let (dx, dy) = (b.x - a.x, b.y - a.y);
    let (mut start, mut end) = (0.0_f64, 1.0_f64);
    for (p, q) in [
        (-dx, a.x),
        (dx, width - a.x),
        (-dy, a.y),
        (dy, height - a.y),
    ] {
        if p.abs() < f64::EPSILON {
            if q < 0.0 {
                return None;
            }
        } else {
            let t = q / p;
            if p < 0.0 {
                start = start.max(t);
            } else {
                end = end.min(t);
            }
        }
    }
    (start <= end).then_some((
        Star {
            x: a.x + start * dx,
            y: a.y + start * dy,
            ..a
        },
        Star {
            x: a.x + end * dx,
            y: a.y + end * dy,
            ..b
        },
    ))
}

fn constellation(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let nodes = app.scene_nodes();
    let count = nodes.len();
    let inside = area;
    if inside.height < 2 || inside.width < 8 {
        return;
    }
    let sky = Rect::new(inside.x, inside.y, inside.width, inside.height - 1);
    if app
        .graph
        .inventory
        .as_ref()
        .is_some_and(|state| state.crawling && !state.background_paused)
        && app.error.is_none()
        && !app.layout.borrow().has_world()
    {
        // Native topology is nodes-first, links-later. Do not animate a series
        // of incidental partial layouts. Compose once the active scan ends;
        // the index stays selectable while the honest progress view is up.
        app.layout.borrow_mut().camera.index_only = true;
        frame.render_widget(Paragraph::new(format!("Loading topology…\n{count} node IDs · {} stored links\nThe index is available; the map settles when this scan stops.", app.graph.edges.len())).style(Style::default().fg(DIM)).alignment(ratatui::layout::Alignment::Center).wrap(Wrap { trim:true }), Rect::new(sky.x+2,sky.y+sky.height/2,sky.width.saturating_sub(4),3.min(sky.height)));
        text_line(
            frame,
            Rect::new(
                inside.x + 1,
                inside.bottom() - 1,
                inside.width.saturating_sub(1),
                1,
            ),
            Line::from(s(
                format!("{count} node IDs loaded · topology scan in progress"),
                DIM,
            )),
        );
        return;
    }
    app.layout.borrow_mut().set_map_area(Some(sky));
    let placements = placed_stars(app, sky.width, sky.height);
    let visible: Vec<_> = placements
        .iter()
        .filter(|p| app.node_visible_node(nodes[p.star.index]))
        .collect();
    let mut by_id: HashMap<&str, Star> = projected_stars(app, sky.width, sky.height)
        .into_iter()
        .filter(|star| app.node_visible_node(nodes[star.index]))
        .map(|star| (nodes[star.index].id.as_str(), star))
        .collect();
    for p in &visible {
        by_id.insert(nodes[p.star.index].id.as_str(), p.star);
    }
    let selected_id = app.selected_node().map(|n| n.id.as_str());
    let groups = app.layout.borrow_mut().groups(&app.graph);
    // Resolve names against the full base field. Masking a lens must not move
    // island labels or allow them to cover a newly hidden marker.
    let mut island_marks = islands::place(&groups, &placements, &app.graph, sky.width, sky.height);
    if app.hide_archived {
        let shown_groups: std::collections::BTreeSet<_> = visible
            .iter()
            .filter_map(|p| {
                groups
                    .group_for(&nodes[p.star.index].id)
                    .map(|group| group.id)
            })
            .collect();
        // Keep full-field label reservations, then mask archive-only pockets.
        island_marks.retain(|island| shown_groups.contains(&island.id));
    }
    let dust: Vec<(f64, f64)> = (0..17)
        .map(|i| {
            (
                f64::from((i * 29 + 3) % u32::from(sky.width)),
                f64::from((i * 17 + 1) % (u32::from(sky.height) * 2)),
            )
        })
        .collect();
    let canvas = Canvas::default()
        .background_color(NIGHT)
        .marker(Marker::Braille)
        .x_bounds([0.0, f64::from(sky.width)])
        .y_bounds([0.0, f64::from(sky.height) * 2.0])
        .paint(|ctx| {
            ctx.draw(&Points {
                coords: &dust,
                color: GRID,
            });
            ctx.layer();
            if app.edge_lens.is_none() {
                islands::boundaries(ctx, &island_marks, sky.width, sky.height);
            }
            draw_edges_clipped(
                ctx,
                app.scene_edges(),
                &by_id,
                selected_id,
                Some((f64::from(sky.width), 2.0 * f64::from(sky.height))),
            );
            for placed in &visible {
                let star = placed.star;
                let node = nodes[star.index];
                let pulse = if app.demo {
                    0.0
                } else {
                    app.pulses
                        .get(&node.id)
                        .map(|at| (1.0 - at.elapsed().as_secs_f64() / 2.0).clamp(0.0, 1.0))
                        .unwrap_or(0.0)
                };
                if pulse > 0.0 {
                    ctx.draw(&Circle {
                        x: star.x,
                        y: star.y,
                        radius: 2.0 + (1.0 - pulse) * 5.0,
                        color: Color::Rgb(
                            (50.0 + pulse * 60.0) as u8,
                            (80.0 + pulse * 157.0) as u8,
                            (76.0 + pulse * 116.0) as u8,
                        ),
                    });
                }
                if star.index == app.selected {
                    ctx.draw(&Circle {
                        x: star.x,
                        y: star.y,
                        radius: 1.8,
                        color: MINT,
                    });
                    ctx.draw(&Circle {
                        x: star.x,
                        y: star.y,
                        radius: 3.0,
                        color: Color::Rgb(45, 83, 79),
                    });
                }
            }
        });
    frame.render_widget(canvas, sky);

    // Hide only after all base glyph and label positions have been reserved.
    for placed in &visible {
        let star = placed.star;
        let node = nodes[star.index];
        let selected = star.index == app.selected;
        let core = node.tags.iter().any(|tag| tag == "core");
        let archived = node.status == "archived";
        let marker = Rect::new(sky.x + placed.marker.x, sky.y + placed.marker.y, 1, 1);
        app.layout
            .borrow_mut()
            .camera
            .hits
            .push((marker, node.id.clone()));
        app.layout
            .borrow_mut()
            .camera
            .markers
            .push((marker, node.id.clone()));
        text_line(
            frame,
            marker,
            Line::from(s(
                if archived {
                    "×"
                } else if selected {
                    "◆"
                } else if core {
                    "◇"
                } else {
                    "•"
                },
                if archived {
                    if core { GOLD } else { DIM }
                } else if selected {
                    MINT
                } else if core {
                    GOLD
                } else {
                    groups
                        .group_for(&node.id)
                        .map_or(INK, |group| islands::color(group.id, true))
                },
            )),
        );
        if let Some(label) = placed.label {
            app.layout.borrow_mut().camera.hits.push((
                Rect::new(sky.x + label.x, sky.y + label.y, label.width, label.height),
                node.id.clone(),
            ));
            frame.render_widget(
                Paragraph::new(format!("{:02}", star.index + 1)).style(
                    Style::default()
                        .fg(if selected { MINT } else { DIM })
                        .bg(NIGHT),
                ),
                Rect::new(sky.x + label.x, sky.y + label.y, label.width, label.height),
            );
        }
    }
    if app.edge_lens.is_none() {
        islands::labels(frame, sky, &island_marks);
    }
    if count == 0 {
        let message = if app.busy {
            "opening the lens…"
        } else {
            "No memories in this field.  / search another thought"
        };
        frame.render_widget(
            Paragraph::new(message)
                .style(Style::default().fg(DIM))
                .alignment(ratatui::layout::Alignment::Center)
                .wrap(Wrap { trim: true }),
            Rect::new(
                sky.x + 2,
                sky.y + sky.height / 2,
                sky.width.saturating_sub(4),
                2.min(sky.height),
            ),
        );
    }
    let partial = app
        .edge_lens
        .as_ref()
        .map_or(app.graph.partial, |lens| lens.graph.partial);
    let drawn_edges = app
        .scene_edges()
        .iter()
        .filter(|edge| {
            by_id
                .get(edge.from.as_str())
                .zip(by_id.get(edge.to.as_str()))
                .is_some_and(|(a, b)| {
                    clip_segment(*a, *b, f64::from(sky.width), 2.0 * f64::from(sky.height))
                        .is_some()
                })
        })
        .count();
    let links = if drawn_edges == app.scene_edges().len() {
        format!("{drawn_edges} links drawn")
    } else {
        format!("{drawn_edges}/{} links drawn", app.scene_edges().len())
    };
    let node_count = if visible.len() == count {
        format!("{count} nodes · all visible")
    } else {
        format!("{}/{} nodes · visible/loaded", visible.len(), count)
    };
    let more = app
        .graph
        .inventory
        .as_ref()
        .is_some_and(|inventory| inventory.next.is_some());
    let state = if app.hide_archived {
        "archived hidden · "
    } else {
        ""
    };
    let warning = match (partial, more) {
        (true, true) => "partial · n more · ",
        (true, false) => "partial · ",
        (false, true) => "n more · ",
        (false, false) => "",
    };
    let warning = format!("{warning}{state}");
    let note = format!("{warning}{node_count} · {links}");
    // Ordinary island labels are deliberately best-effort: one visible member,
    // a clipped pocket or crowded labels can suppress them. The selected named
    // group gets priority in this existing strip instead. Never cover a node or
    // reshuffle the composition merely to make its name fit.
    let selected_group = selected_id
        .and_then(|id| groups.group_for(id))
        .filter(|group| group.label.is_some());
    let line = if let Some(group) = selected_group {
        let coverage = format!(
            "{}/{} nodes · {drawn_edges}/{} links drawn · visible/loaded",
            visible.len(),
            count,
            app.scene_edges().len()
        );
        selected_group_note(
            group,
            coverage,
            inside.width.saturating_sub(1),
            &warning,
            partial,
        )
    } else {
        Line::from(s(note, if partial { GOLD } else { DIM }))
    };
    text_line(
        frame,
        Rect::new(
            inside.x + 1,
            inside.bottom() - 1,
            inside.width.saturating_sub(1),
            1,
        ),
        line,
    );
}

fn selected_group_note(
    group: &crate::clusters::DisplayGroup,
    coverage: String,
    width: u16,
    warning: &str,
    partial: bool,
) -> Line<'static> {
    let available = usize::from(width).saturating_sub(Line::raw(warning).width());
    let name_columns = available
        .saturating_sub(Line::raw(&coverage).width() + 5)
        .clamp(8, 22)
        .min(available.saturating_sub(4));
    Line::from(vec![
        s(warning, GOLD),
        s("◆ ", MINT),
        s(
            format!(
                "#{}",
                short(
                    group.label.as_deref().expect("named selected group"),
                    name_columns
                )
            ),
            islands::color(group.id, true),
        ),
        s(format!(" · {coverage}"), if partial { GOLD } else { DIM }),
    ])
}

fn index(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let nodes = app.scene_nodes();
    let visible: Vec<_> = nodes
        .iter()
        .enumerate()
        .filter(|(_, node)| app.node_visible_node(node))
        .collect();
    let selected = if app.selected_node().is_none() {
        0
    } else {
        app.selected + 1
    };
    text_line(
        frame,
        area,
        Line::from(s(
            format!(
                "  {selected} / {}{}",
                nodes.len(),
                if app.hide_archived {
                    " · archived hidden (h)"
                } else {
                    ""
                }
            ),
            GRID,
        )),
    );
    let inside = Rect::new(
        area.x,
        area.y + 1,
        area.width,
        area.height.saturating_sub(1),
    );
    if visible.is_empty() && app.hide_archived && !nodes.is_empty() {
        text_line(
            frame,
            inside,
            Line::from(s("All loaded nodes hidden · h shows archived", DIM)),
        );
    }
    let rows = usize::from(inside.height);
    if rows == 0 {
        return;
    }
    let selected_row = visible
        .iter()
        .position(|(i, _)| *i == app.selected)
        .unwrap_or(0);
    let start = selected_row
        .saturating_sub(rows / 2)
        .min(visible.len().saturating_sub(rows));
    for (row, (i, node)) in visible.into_iter().skip(start).take(rows).enumerate() {
        let chosen = i == app.selected;
        let spans = vec![
            s(if chosen { "› " } else { "  " }, MINT),
            s(format!("{:02} ", i + 1), if chosen { MINT } else { DIM }),
            s(
                short(
                    card_title(app, node),
                    usize::from(inside.width.saturating_sub(6)),
                ),
                if chosen { INK } else { DIM },
            ),
        ];
        let row_rect = Rect::new(inside.x, inside.y + row as u16, inside.width, 1);
        let mut layout = app.layout.borrow_mut();
        layout.camera.hits.push((row_rect, node.id.clone()));
        layout.camera.index_ids.push(node.id.clone());
        if layout.camera.index_only {
            layout.camera.markers.push((row_rect, node.id.clone()));
            layout.camera.viewport = (inside.width, inside.height);
        }
        drop(layout);
        frame.render_widget(
            Paragraph::new(Line::from(spans)).style(Style::default().bg(if chosen {
                PANEL
            } else {
                NIGHT
            })),
            Rect::new(inside.x, inside.y + row as u16, inside.width, 1),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Graph, LensState, Node};
    use ratatui::{Terminal, backend::TestBackend};

    fn named_group() -> App {
        let mut app = App::new(vec![], String::new(), true);
        app.graph = Graph {
            nodes: ["a", "b", "outside"]
                .into_iter()
                .map(|id| Node {
                    id: id.into(),
                    tags_complete: true,
                    tags: vec![
                        if id == "outside" {
                            "elsewhere"
                        } else {
                            "workshop"
                        }
                        .into(),
                    ],
                    ..Node::default()
                })
                .collect(),
            edges: vec![Edge {
                from: "a".into(),
                to: "b".into(),
                kind: "supports".into(),
                weight: 0.75,
            }],
            ..Graph::default()
        };
        app
    }

    fn frame(app: &App, width: u16, height: u16) -> ratatui::buffer::Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| constellation(frame, Rect::new(0, 0, width, height), app))
            .unwrap();
        terminal.backend().buffer().clone()
    }
    fn row(buffer: &ratatui::buffer::Buffer, y: u16) -> String {
        (0..buffer.area.width)
            .map(|x| buffer[(x, y)].symbol())
            .collect()
    }

    #[test]
    fn archive_shape_and_core_color_are_independent_of_edges_and_selection() {
        let mut app = named_group();
        app.graph.nodes = [
            ("active-core", "active", true),
            ("archived-core", "archived", true),
            ("archived-island", "archived", false),
            ("unknown", "", false),
            ("unrecognized", "unknown", false),
            ("active", "active", false),
        ]
        .into_iter()
        .map(|(id, status, core)| Node {
            id: id.into(),
            status: status.into(),
            tags: if core { vec!["core".into()] } else { vec![] },
            ..Node::default()
        })
        .collect();
        // An active successor with many outgoing AND incoming supersession
        // links is still active. An archived isolated node still has ×.
        app.graph.edges = (1..6)
            .flat_map(|i| {
                [
                    Edge {
                        from: "active-core".into(),
                        to: app.graph.nodes[i].id.clone(),
                        kind: "supersedes".into(),
                        weight: 0.0,
                    },
                    Edge {
                        to: "active-core".into(),
                        from: app.graph.nodes[i].id.clone(),
                        kind: "supersedes".into(),
                        weight: 0.0,
                    },
                ]
            })
            .filter(|e| e.from != "archived-island" && e.to != "archived-island")
            .collect();
        for selected in 0..app.graph.nodes.len() {
            app.selected = selected;
            app.layout.borrow_mut().invalidate_hits();
            let buffer = frame(&app, 180, 70);
            let marker = app
                .layout
                .borrow()
                .camera
                .markers
                .iter()
                .find(|(_, id)| *id == app.graph.nodes[selected].id)
                .unwrap()
                .0;
            let cell = &buffer[(marker.x, marker.y)];
            if app.graph.nodes[selected].status == "archived" {
                assert_eq!(cell.symbol(), "×");
                assert_eq!(cell.fg, if selected == 1 { GOLD } else { DIM });
            } else {
                assert_eq!(cell.symbol(), "◆");
                assert_eq!(cell.fg, MINT);
            }
        }
        app.selected = 5;
        app.layout.borrow_mut().invalidate_hits();
        let buffer = frame(&app, 180, 70);
        for id in ["active-core", "unknown", "unrecognized"] {
            let marker = app
                .layout
                .borrow()
                .camera
                .markers
                .iter()
                .find(|(_, found)| found == id)
                .unwrap()
                .0;
            let cell = &buffer[(marker.x, marker.y)];
            if id == "active-core" {
                assert_eq!(cell.symbol(), "◇");
                assert_eq!(cell.fg, GOLD);
            } else {
                assert_eq!(cell.symbol(), "•");
            }
        }
    }

    #[test]
    fn hiding_archives_masks_drawn_links_and_targets_without_changing_inventory_or_world() {
        let mut app = named_group();
        app.graph.nodes[1].status = "archived".into();
        app.graph.nodes[0].status = "active".into();
        let before = app.layout.borrow_mut().world_positions(&app.graph);
        frame(&app, 120, 36);
        let center = app.layout.borrow().camera.center;
        app.toggle_archived();
        let buffer = frame(&app, 120, 36);
        assert_eq!(center, app.layout.borrow().camera.center);
        {
            let layout = app.layout.borrow();
            assert!(layout.camera.markers.iter().all(|(_, id)| id != "b"));
            assert!(layout.camera.hits.iter().all(|(_, id)| id != "b"));
        }
        assert!(row(&buffer, 35).contains("archived hidden"));
        assert!(row(&buffer, 35).contains("0/1 links drawn"));
        assert_eq!(app.graph.nodes.len(), 3);
        assert_eq!(app.graph.edges.len(), 1);
        assert_eq!(before, app.layout.borrow_mut().world_positions(&app.graph));
    }

    #[test]
    fn selected_name_survives_suppressed_island_labels_without_moving_nodes() {
        let app = named_group();
        let before = app.layout.borrow_mut().world_positions(&app.graph);
        let buffer = frame(&app, 24, 7);
        let placements = placed_stars(&app, 24, 6);
        let groups = app.layout.borrow_mut().groups(&app.graph);
        assert!(islands::place(&groups, &placements, &app.graph, 24, 6).is_empty());
        assert!(row(&buffer, 6).contains("◆ #workshop"));
        assert_eq!(before, app.layout.borrow_mut().world_positions(&app.graph));
        for placed in &placements {
            assert!(matches!(
                buffer[(placed.marker.x, placed.marker.y)].symbol(),
                "◆" | "•" | "◇"
            ));
        }
    }

    #[test]
    fn panning_past_other_group_members_keeps_selected_name_and_geometry() {
        let mut app = named_group();
        let before = app.layout.borrow_mut().world_positions(&app.graph);
        let horizontal = (before[0].x - before[1].x).abs() >= (before[0].y - before[1].y).abs();
        app.selected = if horizontal {
            usize::from(before[1].x < before[0].x)
        } else {
            usize::from(before[1].y < before[0].y)
        };
        let _ = frame(&app, 60, 17);
        let point = before[app.selected];
        let center = app.layout.borrow().camera.center.unwrap();
        let target = if horizontal {
            Point {
                x: point.x - 30.0 + 1.0,
                y: point.y,
            }
        } else {
            Point {
                x: point.x,
                y: point.y - 16.0 + 1.0,
            }
        };
        app.layout
            .borrow_mut()
            .pan(target.x - center.x, target.y - center.y);
        let buffer = frame(&app, 60, 17);
        let placements = placed_stars(&app, 60, 16);
        assert_eq!(placements.iter().filter(|p| p.star.index < 2).count(), 1);
        let groups = app.layout.borrow_mut().groups(&app.graph);
        assert!(islands::place(&groups, &placements, &app.graph, 60, 16).is_empty());
        let strip = row(&buffer, 16);
        assert!(strip.contains("◆ #workshop"), "{strip}");
        assert!(strip.contains("nodes"));
        assert!(strip.contains("links drawn"));
        assert_eq!(before, app.layout.borrow_mut().world_positions(&app.graph));
    }

    #[test]
    fn crowded_candidate_rows_do_not_hide_the_selected_name() {
        let mut app = named_group();
        app.graph.nodes.push(Node {
            id: "another-outside".into(),
            ..Node::default()
        });
        let groups = app.layout.borrow_mut().groups(&app.graph);
        let group = groups.group_for("a").unwrap();
        let placements: Vec<_> = [(20, 8), (32, 12), (26, 4), (26, 16)]
            .into_iter()
            .enumerate()
            .map(|(index, (x, y))| {
                let marker = Rect::new(x, y, 1, 1);
                PlacedStar {
                    star: cell_center(index, marker, 60, 20),
                    marker,
                    label: None,
                }
            })
            .collect();
        let marks = islands::place(&groups, &placements, &app.graph, 60, 20);
        assert_eq!(marks.len(), 1);
        let mut terminal = Terminal::new(TestBackend::new(60, 21)).unwrap();
        terminal
            .draw(|frame| {
                for placed in &placements {
                    text_line(
                        frame,
                        placed.marker,
                        Line::raw(if placed.star.index == 0 { "◆" } else { "•" }),
                    );
                }
                islands::labels(frame, Rect::new(0, 0, 60, 20), &marks);
            })
            .unwrap();
        let before = terminal.backend().buffer().clone();
        assert!(!(0..20).any(|y| row(&before, y).contains("#workshop")));
        terminal
            .draw(|frame| {
                for placed in &placements {
                    text_line(
                        frame,
                        placed.marker,
                        Line::raw(if placed.star.index == 0 { "◆" } else { "•" }),
                    );
                }
                islands::labels(frame, Rect::new(0, 0, 60, 20), &marks);
                text_line(
                    frame,
                    Rect::new(0, 20, 60, 1),
                    selected_group_note(
                        group,
                        "4/4 nodes · 1/1 links drawn · visible/loaded".into(),
                        60,
                        "",
                        false,
                    ),
                );
            })
            .unwrap();
        let after = terminal.backend().buffer();
        assert!(row(after, 20).contains("◆ #workshop"));
        assert!(row(after, 20).contains("links drawn"));
        for placed in &placements {
            assert_eq!(
                before[(placed.marker.x, placed.marker.y)],
                after[(placed.marker.x, placed.marker.y)]
            );
        }
    }

    #[test]
    fn selected_name_is_bounded_and_also_available_in_edge_lens() {
        let mut app = named_group();
        for node in app.graph.nodes.iter_mut().take(2) {
            node.tags = vec![
                "a-very-long-selected-cluster-name-that-must-not-eat-the-instrument-strip".into(),
            ];
        }
        app.open_edge_lens("a".into(), app.graph.clone(), LensState::Ready);
        let buffer = frame(&app, 80, 21);
        let strip = row(&buffer, 20);
        assert!(strip.contains("◆ #a-very-long-selected"), "{strip}");
        assert!(strip.contains('…'));
        assert!(strip.contains("nodes"));
        assert!(strip.contains("links drawn"));
    }

    #[test]
    fn unnamed_groups_get_no_invented_badge_but_named_selection_survives_offscreen_pan() {
        let mut app = named_group();
        for node in &mut app.graph.nodes {
            node.tags.clear();
        }
        let buffer = frame(&app, 60, 17);
        assert!(!row(&buffer, 16).contains("◆ #"));
        for node in app.graph.nodes.iter_mut().take(2) {
            node.tags = vec!["workshop".into()];
        }
        let _ = frame(&app, 60, 17);
        app.layout.borrow_mut().pan(1000.0, 1000.0);
        let buffer = frame(&app, 60, 17);
        assert!(row(&buffer, 16).contains("◆ #workshop"));
        assert!(
            app.layout
                .borrow()
                .camera
                .markers
                .iter()
                .all(|(_, id)| id != "a")
        );
    }

    #[test]
    fn narrow_selected_strip_keeps_actual_partial_and_continuation_warning() {
        let mut app = named_group();
        app.graph.partial = true;
        app.graph.inventory = Some(crate::model::InventoryState {
            next: Some("actual-cursor".into()),
            ..Default::default()
        });
        let buffer = frame(&app, 40, 17);
        let strip = row(&buffer, 16);
        assert!(strip.contains("partial"), "{strip}");
        assert!(strip.contains("n more"), "{strip}");
        assert!(!strip.contains("◆ #workshop"), "{strip}");
        app.graph.partial = false;
        app.graph.inventory.as_mut().unwrap().next = None;
        app.graph.inventory.as_mut().unwrap().complete = true;
        let strip = row(&frame(&app, 40, 17), 16);
        assert!(!strip.contains("partial") && !strip.contains("n more"));
        assert!(!strip.contains("unknown") && !strip.contains("snapshot"));
        assert!(strip.contains("◆ #workshop"), "{strip}");
    }
}
