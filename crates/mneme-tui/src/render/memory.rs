//! Selection-owned text: memory bodies, link evidence and capture provenance.

use super::map::link_color;
use super::{DIM, GOLD, GRID, INK, LILAC, MINT, card_title, islands, s, short, single, text_line};
use crate::model::{App, Edge, LensState, clean};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style, Stylize},
    text::{Line, Text},
    widgets::{Block, Borders, Paragraph, Wrap},
};

/// The neighbor is the grammatical subject; `this` is the selected endpoint.
/// Stored arrows remain unchanged: derived analysis points at its source.
fn relationship_caption<'a>(edge: &'a Edge, selected: &str) -> &'a str {
    if edge.from == edge.to || (edge.from != selected && edge.to != selected) {
        return &edge.kind;
    }
    let outgoing = edge.from == selected;
    match (edge.kind.as_str(), outgoing) {
        ("supersedes", true) => "is superseded",
        ("supersedes", false) => "supersedes this",
        ("derived_from", true) => "sources this",
        ("derived_from", false) => "is sourced",
        ("associative", _) => "is associated",
        ("bridge", _) => "is bridged",
        ("transition", true) => "follows this",
        ("transition", false) => "leads to this",
        // Legacy/synthetic conflict labels are symmetric. Native contradictions
        // remain a cold-path overlay, not hot-path graph edges.
        ("contradicts", _) => "contradicts this",
        ("conflicts", _) => "conflicts with this",
        // Preserve unfamiliar names rather than inventing a semantic inverse.
        _ => &edge.kind,
    }
}

fn relationship_line(
    edge: &Edge,
    selected: &str,
    neighbor: &str,
    width: Option<u16>,
) -> Line<'static> {
    let arrow = if edge.from == selected { "→" } else { "←" };
    let caption = single(relationship_caption(edge, selected));
    // Spend width on the short relationship first, not on a long summary that
    // would hide its direction. Very small panes can wrap, never invert it.
    let neighbor_width = width
        .map(|width| {
            usize::from(width)
                .saturating_sub(Line::raw(&caption).width() + 3)
                .max(1)
        })
        .unwrap_or(usize::MAX);
    Line::from(vec![
        s(format!("{arrow} "), DIM),
        s(short(neighbor, neighbor_width), INK),
        s(format!(" {caption}"), link_color(&edge.kind, true)),
    ])
}

pub(super) fn inspector(frame: &mut Frame<'_>, area: Rect, app: &App) {
    if area.width < 4 || area.height == 0 {
        return;
    }
    frame.render_widget(
        Block::default()
            .borders(Borders::LEFT)
            .border_style(Style::default().fg(GRID)),
        area,
    );
    let area = Rect::new(
        area.x + 2,
        area.y,
        area.width.saturating_sub(2),
        area.height,
    );
    let inside = area;
    let Some(node) = app.selected_node() else {
        frame.render_widget(
            Paragraph::new(if app.hide_archived && !app.graph.nodes.is_empty() {
                "No selectable memories · archived hidden.\n\nh shows archived nodes; the loaded inventory is unchanged."
            } else {
                "A lens, not the whole sky.\n\nFind a thought with /, then follow its connections."
            })
            .style(Style::default().fg(DIM))
            .wrap(Wrap { trim: true }),
            inside,
        );
        return;
    };
    let hidden = app.hidden_node_ids();
    if app.edge_lens.is_some() {
        edge_inspector(frame, inside, app);
        return;
    }
    if !app.show_details {
        let groups = app.layout.borrow_mut().groups(&app.graph);
        let mut lines = vec![
            Line::from(s("i  open memory · e  edges", DIM)),
            Line::default(),
            Line::from(s(single(card_title(app, node)), MINT).add_modifier(Modifier::BOLD)),
            Line::default(),
            Line::from(s(
                node.tags
                    .iter()
                    .take(3)
                    .map(|tag| format!("#{}", single(tag)))
                    .collect::<Vec<_>>()
                    .join(" "),
                LILAC,
            )),
            Line::default(),
        ];
        if let Some(inventory) = &app.graph.inventory {
            if !inventory.complete || !inventory.topology_edges_complete {
                lines.insert(
                    3,
                    Line::from(s("Topology incomplete · m continue / r restart", GOLD)),
                );
            }
        }
        if let Some(group) = groups.group_for(&node.id) {
            if let Some(label) = &group.label {
                lines.push(Line::from(s(
                    format!("#{} · {} memories", single(label), group.members.len()),
                    islands::color(group.id, true),
                )));
                lines.push(Line::default());
            }
        }
        for edge in app
            .graph
            .edges
            .iter()
            .filter(|edge| edge.from == node.id || edge.to == node.id)
            .filter(|edge| {
                !hidden.contains(edge.from.as_str()) && !hidden.contains(edge.to.as_str())
            })
            .take(3)
        {
            let id = if edge.from == node.id {
                &edge.to
            } else {
                &edge.from
            };
            let index = app
                .graph
                .nodes
                .iter()
                .position(|n| &n.id == id)
                .map(|i| format!("{:02}", i + 1))
                .unwrap_or_else(|| single(id));
            lines.push(relationship_line(edge, &node.id, &index, None));
        }
        frame.render_widget(
            Paragraph::new(Text::from(lines)).wrap(Wrap { trim: true }),
            inside,
        );
        return;
    }
    let mut lines = vec![
        Line::from(s(single(card_title(app, node)), MINT).add_modifier(Modifier::BOLD)),
        Line::from(vec![
            s(
                if node.status.is_empty() {
                    "unknown".into()
                } else {
                    single(&node.status)
                },
                GOLD,
            ),
            s("  ", DIM),
            s(
                node.tags
                    .iter()
                    .map(|tag| format!("#{}", single(tag)))
                    .collect::<Vec<_>>()
                    .join(" "),
                LILAC,
            ),
        ]),
        Line::default(),
    ];
    if node.body.trim().is_empty() {
        lines.push(Line::from(s(
            "Summary only · focus to load the memory body.",
            DIM,
        )));
    } else {
        lines.extend(
            clean(&node.body)
                .lines()
                .map(|line| Line::from(s(line, INK))),
        );
    }
    lines.push(Line::default());
    let groups = app.layout.borrow_mut().groups(&app.graph);
    if let Some(group) = groups.group_for(&node.id) {
        lines.push(Line::from(s("── DISPLAY ISLAND", DIM)));
        lines.push(Line::from(s(
            format!(
                "{} · {} loaded memories",
                group
                    .label
                    .as_deref()
                    .map(single)
                    .unwrap_or_else(|| "Unnamed island".into()),
                group.members.len()
            ),
            islands::color(group.id, true),
        )));
        lines.push(Line::from(s("Grouped from loaded weighted links; any name comes from an existing tag. Not a stored community or semantic-distance claim.", DIM)));
        lines.push(Line::default());
    }
    lines.push(Line::from(s("── THREADS", DIM)));
    let edges: Vec<&Edge> = app
        .graph
        .edges
        .iter()
        .filter(|edge| edge.from == node.id || edge.to == node.id)
        .filter(|edge| !hidden.contains(edge.from.as_str()) && !hidden.contains(edge.to.as_str()))
        .collect();
    if edges.is_empty() {
        lines.push(Line::from(s(
            if app.hide_archived {
                "No visible relationships · archived hidden"
            } else {
                "No relationships in this view."
            },
            DIM,
        )));
    }
    for edge in edges {
        let other_id = if edge.from == node.id {
            &edge.to
        } else {
            &edge.from
        };
        let other = app
            .graph
            .nodes
            .iter()
            .find(|n| &n.id == other_id)
            .map(|n| {
                single(if n.summary.trim().is_empty() {
                    &n.id
                } else {
                    &n.summary
                })
            })
            .unwrap_or_else(|| single(other_id));
        lines.push(relationship_line(
            edge,
            &node.id,
            &other,
            Some(inside.width),
        ));
    }
    lines.push(Line::default());
    lines.push(Line::from(s("── PROVENANCE", DIM)));
    lines.push(Line::from(s(
        if node.provenance.trim().is_empty() {
            "Not supplied by this source".into()
        } else {
            single(&node.provenance)
        },
        DIM,
    )));
    lines.push(Line::from(s(format!("id  {}", single(&node.id)), DIM)));
    if let Some(confidence) = node.confidence.filter(|value| value.is_finite()) {
        lines.push(Line::from(s(
            format!("stored confidence  {confidence:.2}"),
            DIM,
        )));
    }
    crate::detail_scroll::draw(frame, inside, app, lines, Style::default());
}

fn edge_inspector(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let Some(lens) = &app.edge_lens else { return };
    let Some(node) = app.graph.nodes.iter().find(|node| node.id == lens.anchor) else {
        return;
    };
    let hidden = app.hidden_node_ids();
    let focused = lens.state == LensState::Ready;
    let edges: Vec<&Edge> = lens
        .graph
        .edges
        .iter()
        .filter(|edge| edge.from == node.id || edge.to == node.id)
        .filter(|edge| !hidden.contains(edge.from.as_str()) && !hidden.contains(edge.to.as_str()))
        .collect();
    let title = format!(
        "edges / {}",
        short(&node.summary, usize::from(area.width.saturating_sub(8)))
    );
    text_line(frame, area, Line::from(s(title, MINT)));
    if area.height < 2 {
        return;
    }
    let note = if app.hide_archived {
        format!(
            "{} visible edges · archived hidden{}",
            edges.len(),
            if lens.graph.partial || !focused {
                " · more may exist"
            } else {
                ""
            }
        )
    } else if lens.graph.partial || !focused {
        "loaded edges · more may exist".to_owned()
    } else {
        format!("{} loaded edges", edges.len())
    };
    text_line(
        frame,
        Rect::new(area.x, area.y + 1, area.width, 1),
        Line::from(s(note, if lens.graph.partial { GOLD } else { DIM })),
    );
    let mut lines = Vec::new();
    if !focused {
        lines.push(Line::from(s(
            if lens.state == LensState::Loading {
                "Loading this memory's edges…"
            } else {
                "Edges unavailable · e closes; reopen to retry"
            },
            GOLD,
        )));
        if !edges.is_empty() {
            lines.push(Line::default());
        }
    } else if edges.is_empty() {
        lines.push(Line::from(s(
            if app.hide_archived {
                "No visible edges · archived hidden"
            } else {
                "No edges returned"
            },
            DIM,
        )));
    }
    for edge in edges {
        let other_id = if edge.from == node.id {
            &edge.to
        } else {
            &edge.from
        };
        let other = app
            .scene_nodes()
            .into_iter()
            .find(|other| &other.id == other_id)
            .map(|other| {
                single(if other.summary.trim().is_empty() {
                    &other.id
                } else {
                    &other.summary
                })
            })
            .unwrap_or_else(|| format!("{} · not loaded", single(other_id)));
        let weight = if edge.weight.is_finite() {
            format!("{:.2}", edge.weight)
        } else {
            "not supplied".into()
        };
        lines.push(relationship_line(edge, &node.id, &other, Some(area.width)));
        lines.push(Line::from(s(format!("weight {weight}"), DIM)));
        lines.push(Line::default());
    }
    crate::detail_scroll::draw(
        frame,
        Rect::new(
            area.x,
            area.y + 2,
            area.width,
            area.height.saturating_sub(2),
        ),
        app,
        lines,
        Style::default(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Graph, InventoryState, Node};
    use ratatui::{Terminal, backend::TestBackend};

    fn edge(kind: &str) -> Edge {
        Edge {
            from: "source".into(),
            to: "target".into(),
            kind: kind.into(),
            weight: 0.7,
        }
    }

    #[test]
    fn archive_filter_masks_relationship_rows_but_not_the_loaded_graph() {
        for pane in [Pane::Compact, Pane::Details, Pane::Edges] {
            let mut app = pane_app("supersedes", 0, pane);
            app.graph.nodes[1].status = "archived".into();
            if let Some(lens) = &mut app.edge_lens {
                lens.graph.nodes[1].status = "archived".into();
            }
            assert!(rendered_inspector(&app).contains("is superseded"));
            app.toggle_archived();
            let text = rendered_inspector(&app);
            assert!(!text.contains("target memory"), "{pane:?}: {text}");
            assert!(!text.contains("is superseded"), "{pane:?}: {text}");
            if matches!(pane, Pane::Details | Pane::Edges) {
                assert!(text.contains("archived hidden"));
                assert!(text.contains("No visible"));
            }
            assert_eq!(app.graph.edges.len(), 1);
            assert_eq!(app.graph.nodes.len(), 2);
            app.toggle_archived();
            assert!(rendered_inspector(&app).contains("is superseded"));
        }
        let mut app = pane_app("supersedes", 0, Pane::Details);
        for node in &mut app.graph.nodes {
            node.status = "archived".into();
        }
        app.toggle_archived();
        let text = rendered_inspector_width(&app, 36);
        assert!(text.contains("No selectable memories"));
        assert!(text.contains("h shows archived nodes"));
        assert!(!text.contains("Find a thought"));
    }

    #[test]
    fn captions_follow_selected_endpoint_not_traversal_direction() {
        for (kind, source, target) in [
            ("supersedes", "is superseded", "supersedes this"),
            ("derived_from", "sources this", "is sourced"),
            ("associative", "is associated", "is associated"),
            ("transition", "follows this", "leads to this"),
            ("bridge", "is bridged", "is bridged"),
            ("future_relation", "future_relation", "future_relation"),
        ] {
            let edge = edge(kind);
            assert_eq!(relationship_caption(&edge, "source"), source);
            assert_eq!(relationship_caption(&edge, "target"), target);
            assert_eq!(relationship_caption(&edge, "unrelated"), kind);
            assert_eq!((edge.from.as_str(), edge.to.as_str()), ("source", "target"));
        }
        let mut loop_edge = edge("derived_from");
        loop_edge.to = loop_edge.from.clone();
        assert_eq!(relationship_caption(&loop_edge, "source"), "derived_from");
    }

    #[derive(Clone, Copy, Debug)]
    enum Pane {
        Compact,
        Details,
        Edges,
    }

    fn pane_app(kind: &str, selected: usize, pane: Pane) -> App {
        let mut app = App::new(vec![], String::new(), true);
        app.graph = Graph {
            nodes: ["source", "target"]
                .into_iter()
                .map(|id| Node {
                    id: id.into(),
                    summary: format!("{id} memory"),
                    status: "active".into(),
                    ..Default::default()
                })
                .collect(),
            edges: vec![edge(kind)],
            ..Default::default()
        };
        app.selected = selected;
        match pane {
            Pane::Compact => {}
            Pane::Details => app.show_details = true,
            Pane::Edges => {
                let anchor = app.selected_node().unwrap().id.clone();
                app.open_edge_lens(anchor, app.graph.clone(), LensState::Ready);
            }
        }
        app
    }

    fn rendered_pane(kind: &str, selected: usize, pane: Pane) -> String {
        rendered_inspector(&pane_app(kind, selected, pane))
    }

    fn rendered_inspector(app: &App) -> String {
        rendered_inspector_width(app, 100)
    }
    fn rendered_inspector_width(app: &App, width: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, 40)).unwrap();
        terminal
            .draw(|frame| inspector(frame, frame.area(), app))
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn existing_compact_controls_precede_summary_and_are_not_duplicated() {
        let hint = "i  open memory · e  edges";
        for selected in [0, 1] {
            let text = rendered_pane("supersedes", selected, Pane::Compact);
            let summary = if selected == 0 {
                "source memory"
            } else {
                "target memory"
            };
            assert!(text.find(hint).unwrap() < text.find(summary).unwrap());
            assert_eq!(text.matches(hint).count(), 1);
            for pane in [Pane::Details, Pane::Edges] {
                assert!(!rendered_pane("supersedes", selected, pane).contains(hint));
            }
        }
    }

    #[test]
    fn inventory_omits_duplicate_counts_and_success_banner_but_keeps_recovery_hint() {
        let mut app = App::new(vec![], String::new(), true);
        app.graph.nodes.push(Node {
            id: "source".into(),
            summary: "Selected memory".into(),
            ..Default::default()
        });
        for (complete, edges_complete) in [(true, true), (false, false), (true, false)] {
            app.graph.inventory = Some(InventoryState {
                complete,
                topology_edges_complete: edges_complete,
                summaries_loaded: 1,
                ..Default::default()
            });
            let text = rendered_inspector(&app);
            assert!(!text.contains("Topology scan complete"));
            assert!(!text.contains("not an atomic snapshot"));
            assert_eq!(
                text.contains("Topology incomplete · m continue / r restart"),
                !complete || !edges_complete
            );
            assert!(!text.contains("loaded IDs"));
            assert!(!text.contains("cards loaded"));
            assert!(!text.contains("map visible"));
        }
    }

    #[test]
    fn all_map_inspector_rows_put_neighbor_before_selected_relative_caption() {
        for (kind, source, target) in [
            ("supersedes", "is superseded", "supersedes this"),
            ("derived_from", "sources this", "is sourced"),
            ("associative", "is associated", "is associated"),
            ("transition", "follows this", "leads to this"),
            ("bridge", "is bridged", "is bridged"),
            ("contradicts", "contradicts this", "contradicts this"),
            ("conflicts", "conflicts with this", "conflicts with this"),
            ("future_relation", "future_relation", "future_relation"),
        ] {
            for pane in [Pane::Compact, Pane::Details, Pane::Edges] {
                for (selected, arrow, caption) in [(0, "→", source), (1, "←", target)] {
                    let text = rendered_pane(kind, selected, pane);
                    let expected = match pane {
                        Pane::Compact => format!("{arrow} {:02} {caption}", 2 - selected),
                        _ => format!(
                            "{arrow} {} memory {caption}",
                            if selected == 0 { "target" } else { "source" }
                        ),
                    };
                    assert!(text.contains(&expected), "{kind}, {pane:?}: {text}");
                    if matches!(pane, Pane::Edges) {
                        assert!(text.contains("weight 0.70"));
                    }
                }
            }
        }
    }
    #[test]
    fn narrow_rows_truncate_neighbor_not_relationship_and_keep_subject_first() {
        for (kind, outgoing, incoming) in [
            ("supersedes", "is superseded", "supersedes this"),
            ("derived_from", "sources this", "is sourced"),
            ("transition", "follows this", "leads to this"),
            ("associative", "is associated", "is associated"),
            ("bridge", "is bridged", "is bridged"),
            ("contradicts", "contradicts this", "contradicts this"),
            ("conflicts", "conflicts with this", "conflicts with this"),
        ] {
            for pane in [Pane::Compact, Pane::Details, Pane::Edges] {
                for (selected, caption) in [(0, outgoing), (1, incoming)] {
                    assert!(caption.split_whitespace().count() <= 3);
                    let mut app = pane_app(kind, selected, pane);
                    for node in &mut app.graph.nodes {
                        node.summary = format!(
                            "{} neighbor with an intentionally very long summary",
                            node.id
                        );
                    }
                    if let Some(lens) = app.edge_lens.as_mut() {
                        lens.graph = app.graph.clone();
                    }
                    let text = rendered_inspector_width(&app, 32);
                    let row = text
                        .lines()
                        .find(|line| line.contains(caption))
                        .unwrap_or_else(|| panic!("{kind}, {pane:?}: {text}"));
                    let arrow = if selected == 0 { "→" } else { "←" };
                    let row = row.trim_start_matches(|ch| ch == ' ' || ch == '│');
                    match pane {
                        Pane::Compact => assert!(
                            row.trim_start()
                                .starts_with(&format!("{arrow} {:02} {caption}", 2 - selected))
                        ),
                        _ => {
                            let neighbor = if selected == 0 { "target" } else { "source" };
                            assert!(
                                row.trim_start().starts_with(&format!("{arrow} {neighbor}")),
                                "{row}"
                            );
                            assert!(row.contains('…'));
                            assert!(row.trim_end().ends_with(caption));
                        }
                    }
                }
            }
        }
    }
    #[test]
    fn unloaded_neighbor_card_keeps_its_id_as_subject() {
        let mut compact = pane_app("derived_from", 0, Pane::Compact);
        compact.graph.nodes.pop();
        assert!(rendered_inspector(&compact).contains("→ target sources this"));
        for pane in [Pane::Details, Pane::Edges] {
            let mut app = pane_app("derived_from", 0, pane);
            app.graph.nodes[1].summary.clear();
            if let Some(lens) = app.edge_lens.as_mut() {
                lens.graph = app.graph.clone();
            }
            assert!(rendered_inspector(&app).contains("→ target sources this"));
        }
    }
}
