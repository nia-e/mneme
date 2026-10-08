//! Cell-aligned, non-semantic hints for groups computed from loaded links.

use super::map::{PlacedStar, cell_center};
use super::{s, short, text_line};
use crate::clusters::{DisplayGroupId, DisplayGroups};
use crate::model::Graph;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::text::Line;
use ratatui::widgets::canvas::{Context, Line as Stroke};

pub(super) struct Island {
    pub(super) id: DisplayGroupId,
    bounds: Rect,
    label: Option<(Rect, String)>,
}

pub(super) fn color(id: DisplayGroupId, bright: bool) -> Color {
    // Identity, not rank or selection, owns the accent. Keep all accents within
    // the mineral / mint / lilac family rather than assigning topic semantics.
    let palette = if bright {
        [
            (165, 188, 219),
            (163, 212, 196),
            (196, 179, 226),
            (152, 200, 218),
        ]
    } else {
        [(37, 49, 66), (32, 57, 53), (48, 42, 64), (31, 53, 65)]
    };
    let (r, g, b) = palette[(id.0 as usize) % palette.len()];
    Color::Rgb(r, g, b)
}

pub(super) fn place(
    groups: &DisplayGroups,
    placements: &[PlacedStar],
    graph: &Graph,
    width: u16,
    height: u16,
) -> Vec<Island> {
    if width < 28 || height < 8 {
        return Vec::new();
    }
    // Reserve every potential nearby numeric-label / halo cell, not only the
    // selected node's current labels. Selection must not shuffle island names.
    let mut occupied: Vec<_> = placements
        .iter()
        .map(|placed| {
            Rect::new(
                placed.marker.x.saturating_sub(3),
                placed.marker.y.saturating_sub(2),
                7,
                5,
            )
        })
        .collect();
    let mut islands = Vec::new();
    for group in &groups.groups {
        let markers: Vec<_> = placements
            .iter()
            .filter_map(|placed| {
                graph
                    .nodes
                    .get(placed.star.index)
                    .filter(|node| group.members.contains(&node.id))
                    .map(|_| placed.marker)
            })
            .collect();
        if markers.len() < 2 {
            continue;
        }
        let left = markers
            .iter()
            .map(|rect| rect.x)
            .min()
            .unwrap()
            .saturating_sub(2);
        let right = markers
            .iter()
            .map(|rect| rect.right())
            .max()
            .unwrap()
            .saturating_add(2)
            .min(width);
        let top = markers
            .iter()
            .map(|rect| rect.y)
            .min()
            .unwrap()
            .saturating_sub(2);
        let bottom = markers
            .iter()
            .map(|rect| rect.bottom())
            .max()
            .unwrap()
            .saturating_add(2)
            .min(height);
        let bounds = Rect::new(left, top, right - left, bottom - top);
        let label = group.label.as_ref().and_then(|label| {
            let title = format!("#{}  {}", short(label, 22), group.members.len());
            let label_width = Line::raw(&title).width().min(usize::from(width)) as u16;
            let x = ((u32::from(left) + u32::from(right)).saturating_sub(u32::from(label_width))
                / 2)
            .min(u32::from(width.saturating_sub(label_width))) as u16;
            // Try the space above and below the pocket before looking farther
            // out. A name is omitted rather than painted over a real memory.
            let rows = [
                top.checked_sub(1),
                Some(bottom),
                top.checked_sub(2),
                bottom.checked_add(1),
            ];
            let rect = rows
                .into_iter()
                .flatten()
                .map(|y| Rect::new(x, y, label_width, 1))
                .find(|rect| {
                    rect.bottom() <= height && !occupied.iter().any(|other| other.intersects(*rect))
                })?;
            occupied.push(rect);
            Some((rect, title))
        });
        islands.push(Island {
            id: group.id,
            bounds,
            label,
        });
    }
    islands
}

pub(super) fn boundaries(ctx: &mut Context<'_>, islands: &[Island], width: u16, height: u16) {
    for island in islands {
        let bounds = island.bounds;
        // Giant envelopes would make a false-looking territory claim. Small
        // open corners hint at a pocket without fencing the rest of the sky.
        if u32::from(bounds.width) * u32::from(bounds.height)
            > u32::from(width) * u32::from(height) / 2
        {
            continue;
        }
        let left = bounds.x;
        let right = bounds.right().saturating_sub(1);
        let top = bounds.y;
        let bottom = bounds.bottom().saturating_sub(1);
        for (x, y, dx, dy) in [
            (left, top, 1_i16, 1_i16),
            (right, top, -1, 1),
            (left, bottom, 1, -1),
            (right, bottom, -1, -1),
        ] {
            let center = cell_center(0, Rect::new(x, y, 1, 1), width, height);
            let across = cell_center(
                0,
                Rect::new(x.saturating_add_signed(dx), y, 1, 1),
                width,
                height,
            );
            let down = cell_center(
                0,
                Rect::new(x, y.saturating_add_signed(dy), 1, 1),
                width,
                height,
            );
            for end in [across, down] {
                ctx.draw(&Stroke {
                    x1: center.x,
                    y1: center.y,
                    x2: end.x,
                    y2: end.y,
                    color: color(island.id, false),
                });
            }
        }
    }
    ctx.layer();
}

pub(super) fn labels(frame: &mut Frame<'_>, sky: Rect, islands: &[Island]) {
    for island in islands {
        if let Some((rect, title)) = &island.label {
            text_line(
                frame,
                Rect::new(sky.x + rect.x, sky.y + rect.y, rect.width, 1),
                Line::from(s(title, color(island.id, true))),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::map::cell_center;
    use super::*;
    use crate::clusters::DisplayGroup;
    use crate::model::Node;

    fn placement(index: usize, x: u16, y: u16) -> PlacedStar {
        let marker = Rect::new(x, y, 1, 1);
        PlacedStar {
            star: cell_center(index, marker, 60, 20),
            marker,
            label: None,
        }
    }

    #[test]
    fn names_are_centered_in_cells_and_do_not_cover_marker_reservations() {
        let graph = Graph {
            nodes: vec![
                Node {
                    id: "a".into(),
                    ..Node::default()
                },
                Node {
                    id: "b".into(),
                    ..Node::default()
                },
            ],
            ..Graph::default()
        };
        let groups = DisplayGroups {
            lookup: Default::default(),
            groups: vec![DisplayGroup {
                id: DisplayGroupId(7),
                members: vec!["a".into(), "b".into()],
                label: Some("workshop".into()),
            }],
        };
        let placements = [placement(0, 20, 8), placement(1, 32, 12)];
        let islands = place(&groups, &placements, &graph, 60, 20);
        let (label, _) = islands[0].label.as_ref().unwrap();
        let center = f64::from(islands[0].bounds.x) + f64::from(islands[0].bounds.width) / 2.0;
        assert!((f64::from(label.x) + f64::from(label.width) / 2.0 - center).abs() <= 0.5);
        assert!(
            placements
                .iter()
                .all(|placed| !label.intersects(placed.marker))
        );
        let reversed = place(&groups, &[placements[1], placements[0]], &graph, 60, 20);
        assert_eq!(islands[0].label, reversed[0].label);
        assert_eq!(islands[0].bounds, reversed[0].bounds);
    }

    #[test]
    fn tiny_fields_omit_islands_and_unnamed_groups_stay_unnamed() {
        let groups = DisplayGroups {
            lookup: Default::default(),
            groups: vec![DisplayGroup {
                id: DisplayGroupId(0),
                members: vec!["a".into(), "b".into()],
                label: None,
            }],
        };
        let graph = Graph {
            nodes: vec![
                Node {
                    id: "a".into(),
                    ..Node::default()
                },
                Node {
                    id: "b".into(),
                    ..Node::default()
                },
            ],
            ..Graph::default()
        };
        assert!(place(&groups, &[], &graph, 10, 3).is_empty());
        let islands = place(
            &groups,
            &[placement(0, 10, 10), placement(1, 20, 10)],
            &graph,
            60,
            20,
        );
        assert!(islands[0].label.is_none());
    }
}
