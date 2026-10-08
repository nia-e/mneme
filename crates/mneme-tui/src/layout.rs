//! Bounded loaded-link groups and a settled, warm-started display composition.
//! Repaints, selection, activity and edge lenses never run the solver.

use crate::clusters::{DisplayGroups, DisplayLink, DisplayTopology};
use crate::model::Graph;

const RELAXATION_STEPS: usize = 192;
const REFRESH_STEPS: usize = 64;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Point {
    pub x: f64,
    pub y: f64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LayoutKey {
    ids: Vec<String>,
    springs: Vec<DisplayLink>,
    focus: Option<usize>,
    width: u16,
    height: u16,
}
impl LayoutKey {
    fn new(graph: &Graph, width: u16, height: u16) -> Self {
        let topology = DisplayTopology::new(graph);
        let focus = graph
            .focus
            .as_ref()
            .and_then(|id| topology.ids.binary_search(id).ok());
        Self {
            ids: topology.ids,
            springs: topology.links,
            focus,
            width,
            height,
        }
    }
}

#[derive(Clone, Debug)]
struct WorldField {
    stamp: u64,
    ids: Vec<String>,
    points: Vec<Point>,
    origin: Point,
}

/// One current bounded field and its display-only groups; clone for exact Back.
#[derive(Clone, Debug, Default)]
pub(crate) struct LayoutCache {
    field: Option<(LayoutKey, Vec<Point>)>,
    topology: Option<DisplayTopology>,
    topology_stamp: Option<u64>,
    world: Option<WorldField>,
    display_groups: DisplayGroups,
    pub(crate) camera: Camera,
    #[cfg(test)]
    solves: usize,
}
impl LayoutCache {
    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }

    fn update_groups(&mut self, topology: DisplayTopology) {
        if self.topology.as_ref() != Some(&topology) {
            self.display_groups = topology.groups(&self.display_groups);
            self.topology = Some(topology);
        }
    }

    /// Current existing tags may change labels without changing spatial identity.
    pub(crate) fn groups(&mut self, graph: &Graph) -> DisplayGroups {
        let stamp = topology_stamp(graph);
        if self.topology_stamp != Some(stamp) {
            self.update_groups(DisplayTopology::new(graph));
            self.topology_stamp = Some(stamp);
        }
        self.display_groups.clone().with_labels(graph)
    }

    /// Positions align with the bounded base graph, never the visible lens subset.
    pub(crate) fn positions(&mut self, graph: &Graph, width: u16, height: u16) -> Vec<Point> {
        let key = LayoutKey::new(graph, width, height);
        self.update_groups(DisplayTopology {
            ids: key.ids.clone(),
            links: key.springs.clone(),
        });
        self.topology_stamp = Some(topology_stamp(graph));
        if self.field.as_ref().is_none_or(|(cached, _)| *cached != key) {
            let positions = settle(&key, &self.display_groups, self.field.as_ref());
            self.field = Some((key, positions));
            #[cfg(test)]
            {
                self.solves += 1;
            }
        }
        let (key, positions) = self.field.as_ref().expect("field initialized");
        graph
            .nodes
            .iter()
            .map(|node| positions[key.ids.binary_search(&node.id).expect("base node in key")])
            .collect()
    }
}

/// Camera translations do not change solver inputs. Coordinates use terminal
/// columns horizontally and twice terminal rows vertically (roughly square).
#[derive(Clone, Debug, Default)]
pub(crate) struct Camera {
    pub(crate) center: Option<Point>,
    pub(crate) viewport: (u16, u16),
    pub(crate) index_only: bool,
    pub(crate) index_ids: Vec<String>,
    pub(crate) hits: Vec<(ratatui::layout::Rect, String)>,
    pub(crate) markers: Vec<(ratatui::layout::Rect, String)>,
    pub(crate) map_area: Option<ratatui::layout::Rect>,
    drag: Option<MapDrag>,
    selected: Option<String>,
}

#[derive(Clone, Copy, Debug)]
struct MapDrag {
    area: ratatui::layout::Rect,
    start: (u16, u16),
    center: Point,
}

impl LayoutCache {
    /// Only the graph's actual sky accepts a new drag, never inspector/index.
    pub(crate) fn set_map_area(&mut self, area: Option<ratatui::layout::Rect>) {
        self.camera.map_area = area;
        if let Some(area) = area {
            if self.camera.drag.is_some_and(|drag| drag.area != area) {
                self.cancel_drag();
            }
        }
    }

    pub(crate) fn cancel_drag(&mut self) {
        self.camera.drag = None;
    }

    pub(crate) fn start_drag(&mut self, column: u16, row: u16) {
        self.cancel_drag();
        if let (Some(area), Some(center)) = (self.camera.map_area, self.camera.center) {
            if area.contains((column, row).into()) {
                self.camera.drag = Some(MapDrag {
                    area,
                    start: (column, row),
                    center,
                });
            }
        }
    }

    pub(crate) fn drag_to(&mut self, column: u16, row: u16) {
        let Some(drag) = self.camera.drag else {
            return;
        };
        if self.camera.map_area != Some(drag.area) {
            self.cancel_drag();
            return;
        }
        // Widen before subtracting: drags can leave the sky, including past
        // its top/left edge. Absolute positions also permit event coalescing.
        let dx = i32::from(column) - i32::from(drag.start.0);
        let dy = i32::from(row) - i32::from(drag.start.1);
        // Anchor to the press, not the previous report: event coalescing has
        // exactly the same result, without cumulative floating-point drift.
        let center = Point {
            x: drag.center.x - f64::from(dx),
            y: drag.center.y + 2.0 * f64::from(dy),
        };
        if self.camera.center != Some(center) {
            self.camera.center = Some(center);
            self.invalidate_hits();
        }
    }

    pub(crate) fn invalidate_hits(&mut self) {
        self.camera.hits.clear();
        self.camera.markers.clear();
        self.camera.index_ids.clear();
    }

    pub(crate) fn world_size(count: usize) -> (u16, u16) {
        // Keep label and halo breathing room as the loaded field grows, rather
        // than compressing another hundred nodes into the same terminal.
        let width = (count.max(20) as f64 * 220.0 * 1.65).sqrt().ceil();
        (
            width.min(f64::from(u16::MAX)) as u16,
            (width / 3.3).ceil().min(f64::from(u16::MAX)) as u16,
        )
    }

    pub(crate) fn has_world(&self) -> bool {
        self.world.is_some()
    }

    pub(crate) fn world_positions(&mut self, graph: &Graph) -> Vec<Point> {
        if graph.nodes.is_empty() {
            return Vec::new();
        }
        let stamp = topology_stamp(graph);
        if let Some(world) = &self.world {
            if world.stamp == stamp {
                return world.points.clone();
            }
        }
        let (width, height) = Self::world_size(graph.nodes.len());
        let next_ids: std::collections::HashSet<_> =
            graph.nodes.iter().map(|node| node.id.as_str()).collect();
        let append_only = self
            .world
            .as_ref()
            .is_some_and(|world| world.ids.iter().all(|id| next_ids.contains(id.as_str())));
        let (origin, points) = if append_only {
            let world = self.world.as_ref().unwrap();
            let origin = world.origin;
            let retained: std::collections::HashMap<&str, Point> = world
                .ids
                .iter()
                .map(String::as_str)
                .zip(world.points.iter().copied())
                .collect();
            let mut near: std::collections::HashMap<&str, (f64, Point)> =
                std::collections::HashMap::new();
            for edge in &graph.edges {
                for (new, old) in [(&edge.from, &edge.to), (&edge.to, &edge.from)] {
                    if !retained.contains_key(new.as_str()) {
                        if let Some(&point) = retained.get(old.as_str()) {
                            let weight = if edge.weight.is_finite() {
                                edge.weight.clamp(0.0, 1.0)
                            } else {
                                0.0
                            };
                            if near
                                .get(new.as_str())
                                .is_none_or(|(known, _)| weight > *known)
                            {
                                near.insert(new.as_str(), (weight, point));
                            }
                        }
                    }
                }
            }
            let mut occupied = Packing::default();
            for &point in &world.points {
                occupied.occupy(point);
            }
            let points = graph
                .nodes
                .iter()
                .map(|node| {
                    if let Some(&point) = retained.get(node.id.as_str()) {
                        return point;
                    }
                    let hash = stable_hash(&node.id);
                    let initial = near
                        .get(node.id.as_str())
                        .map(|(_, point)| {
                            let angle = fraction(hash) * std::f64::consts::TAU;
                            Point {
                                x: point.x + 12.0 * angle.cos(),
                                y: point.y + 12.0 * angle.sin(),
                            }
                        })
                        .unwrap_or(Point {
                            x: origin.x + (fraction(hash) - 0.5) * f64::from(width) * 0.88,
                            y: origin.y
                                + (fraction(hash ^ 0x9e3779b97f4a7c15) - 0.5)
                                    * 2.0
                                    * f64::from(height)
                                    * 0.88,
                        });
                    occupied.place(initial)
                })
                .collect();
            self.update_groups(DisplayTopology::new(graph));
            self.topology_stamp = Some(stamp);
            (origin, points)
        } else {
            (
                Point {
                    x: f64::from(width) / 2.0,
                    y: f64::from(height),
                },
                self.positions(graph, width, height),
            )
        };
        self.world = Some(WorldField {
            stamp,
            ids: graph.nodes.iter().map(|node| node.id.clone()).collect(),
            points,
            origin,
        });
        self.world.as_ref().unwrap().points.clone()
    }

    pub(crate) fn project(
        &mut self,
        graph: &Graph,
        points: &[Point],
        selected: Option<(&str, Point)>,
        width: u16,
        height: u16,
    ) -> Vec<Point> {
        let (world_width, world_height) = Self::world_size(graph.nodes.len());
        let resized = self.camera.viewport != (width, height);
        self.camera.viewport = (width, height);
        let initial_center = self.world.as_ref().map_or(
            Point {
                x: f64::from(world_width) / 2.0,
                y: f64::from(world_height),
            },
            |world| world.origin,
        );
        let center = self.camera.center.get_or_insert(initial_center);
        if let Some((id, point)) = selected {
            if resized || self.camera.selected.as_deref() != Some(id) {
                let half_x = (f64::from(width) / 2.0 - 5.0).max(0.0);
                let half_y = (f64::from(height) - 5.0).max(0.0);
                // A press still selects its node, but grabbing an already
                // visible marker must not snap the world underneath the hand.
                if self.camera.drag.is_none() {
                    center.x = center.x.clamp(point.x - half_x, point.x + half_x);
                    center.y = center.y.clamp(point.y - half_y, point.y + half_y);
                }
                self.camera.selected = Some(id.to_owned());
            }
        }
        points
            .iter()
            .map(|p| Point {
                x: p.x - center.x + f64::from(width) / 2.0,
                y: p.y - center.y + f64::from(height),
            })
            .collect()
    }

    pub(crate) fn pan(&mut self, dx: f64, dy: f64) {
        if let Some(center) = &mut self.camera.center {
            center.x += dx;
            center.y += dy;
        }
        self.invalidate_hits();
    }
}

const WORLD_CLEARANCE: f64 = 7.0;
/// Bounded local search, followed by an O(1) vacant-envelope fallback. Failure
/// to find a near slot changes placement, never loaded-node coverage.
#[derive(Default)]
struct Packing {
    cells: std::collections::HashMap<(i32, i32), Vec<Point>>,
    max_x: Option<f64>,
}
impl Packing {
    fn occupy(&mut self, point: Point) {
        self.cells
            .entry((
                (point.x / WORLD_CLEARANCE).floor() as i32,
                (point.y / WORLD_CLEARANCE).floor() as i32,
            ))
            .or_default()
            .push(point);
        self.max_x = Some(self.max_x.map_or(point.x, |x| x.max(point.x)));
    }
    fn place(&mut self, origin: Point) -> Point {
        let mut candidate = origin;
        for attempt in 0..128 {
            let cell = (
                (candidate.x / WORLD_CLEARANCE).floor() as i32,
                (candidate.y / WORLD_CLEARANCE).floor() as i32,
            );
            let free = (-1..=1).all(|dx| {
                (-1..=1).all(|dy| {
                    self.cells
                        .get(&(cell.0 + dx, cell.1 + dy))
                        .is_none_or(|neighbors| {
                            neighbors.iter().all(|point| {
                                (point.x - candidate.x).hypot(point.y - candidate.y)
                                    >= WORLD_CLEARANCE
                            })
                        })
                })
            });
            if free {
                self.occupy(candidate);
                return candidate;
            }
            let radius = WORLD_CLEARANCE * ((attempt + 1) as f64).sqrt();
            let angle = (attempt + 1) as f64 * 2.399963229728653;
            candidate = Point {
                x: origin.x + radius * angle.cos(),
                y: origin.y + radius * angle.sin(),
            };
        }
        candidate = Point {
            x: self.max_x.unwrap_or(origin.x) + WORLD_CLEARANCE * 2.0,
            y: origin.y,
        };
        self.occupy(candidate);
        candidate
    }
}

/// Cheap, allocation-free structural check. Card summaries/tags/bodies do not
/// contribute, so lazy hydration cannot rebuild topology or rerun the solver.
fn topology_stamp(graph: &Graph) -> u64 {
    let mut stamp = 0xcbf29ce484222325_u64;
    let mut feed = |bytes: &[u8]| {
        for &byte in bytes {
            stamp = (stamp ^ u64::from(byte)).wrapping_mul(0x100000001b3);
        }
        stamp = stamp.wrapping_mul(0x100000001b3);
    };
    for node in &graph.nodes {
        feed(node.id.as_bytes());
    }
    for edge in &graph.edges {
        feed(edge.from.as_bytes());
        feed(edge.to.as_bytes());
        feed(&edge.weight.to_bits().to_le_bytes());
    }
    feed(graph.focus.as_deref().unwrap_or("").as_bytes());
    stamp
}

pub(crate) fn stable_hash(value: &str) -> u64 {
    value.bytes().fold(0xcbf29ce484222325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
    })
}
fn fraction(mut bits: u64) -> f64 {
    bits = (bits ^ (bits >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    bits = (bits ^ (bits >> 27)).wrapping_mul(0x94d049bb133111eb);
    bits ^= bits >> 31;
    (bits >> 11) as f64 / ((1u64 << 53) - 1) as f64
}
fn spacing(width: u16, height: u16, node_count: usize) -> f64 {
    let width = f64::from(width);
    let height = 2.0 * f64::from(height);
    let span_x = width - 2.0 * 3.0_f64.min(width * 0.2);
    let span_y = height - 2.0 * 3.0_f64.min(height * 0.2);
    ((span_x * span_y / node_count.max(1) as f64).sqrt())
        .min(span_x.min(span_y) * 0.45)
        .max(0.25)
}
pub(crate) fn link_distance(weight: f64, width: u16, height: u16, node_count: usize) -> f64 {
    let weight = if weight.is_finite() {
        weight.clamp(0.0, 1.0)
    } else {
        0.0
    };
    spacing(width, height, node_count) * (1.25 - 0.55 * weight)
}

/// Compose district centers across the full padded rectangle, then give each
/// district a small irregular interior. No corner-assigned global hash anchors.
fn anchors(key: &LayoutKey, groups: &DisplayGroups) -> Vec<Point> {
    let width = f64::from(key.width);
    let height = 2.0 * f64::from(key.height);
    let center = Point {
        x: width * 0.5,
        y: height * 0.5,
    };
    let mut units: Vec<(u64, Vec<usize>)> = groups
        .groups
        .iter()
        .map(|group| {
            (
                group.id.0,
                group
                    .members
                    .iter()
                    .filter_map(|id| key.ids.binary_search(id).ok())
                    .collect(),
            )
        })
        .collect();
    for (index, id) in key.ids.iter().enumerate() {
        if groups.group_for(id).is_none() {
            units.push((stable_hash(id), vec![index]));
        }
    }
    units.sort_by_key(|unit| unit.0);
    let mut points = vec![center; key.ids.len()];
    let columns = ((units.len().max(1) as f64 * width.max(1.0) / height.max(1.0))
        .sqrt()
        .ceil() as usize)
        .clamp(1, units.len().max(1));
    let rows = units.len().max(1).div_ceil(columns);
    let step_x = width * 0.88 / columns as f64;
    let step_y = height * 0.88 / rows as f64;
    let focus_unit = key.focus.and_then(|focus| {
        units
            .iter()
            .position(|(_, members)| members.contains(&focus))
    });
    for (unit_index, (_, members)) in units.iter().enumerate() {
        let row = unit_index / columns;
        let row_count = (units.len() - row * columns).min(columns);
        let origin = if units.len() > 6 {
            if focus_unit == Some(unit_index) {
                center
            } else {
                Point {
                    x: center.x + (unit_index % columns) as f64 * step_x
                        - (row_count - 1) as f64 * step_x * 0.5
                        + (fraction(units[unit_index].0) - 0.5) * step_x * 0.32,
                    y: center.y + row as f64 * step_y - (rows - 1) as f64 * step_y * 0.5
                        + (fraction(units[unit_index].0 ^ 0x9e3779b97f4a7c15) - 0.5)
                            * step_y
                            * 0.32,
                }
            }
        } else if let Some(focused) = focus_unit {
            if focused == unit_index || units.len() == 1 {
                center
            } else {
                let index = unit_index - usize::from(unit_index > focused);
                let angle = std::f64::consts::TAU * index as f64 / (units.len() - 1) as f64
                    + if width < height {
                        std::f64::consts::FRAC_PI_2
                    } else {
                        0.0
                    };
                let spread = if units.len() == 2 { 0.24 } else { 0.34 };
                Point {
                    x: center.x + angle.cos() * width * spread,
                    y: center.y + angle.sin() * height * spread,
                }
            }
        } else {
            Point {
                x: center.x + (unit_index % columns) as f64 * step_x
                    - (row_count - 1) as f64 * step_x * 0.5
                    + (fraction(units[unit_index].0) - 0.5) * step_x * 0.32,
                y: center.y + row as f64 * step_y - (rows - 1) as f64 * step_y * 0.5
                    + (fraction(units[unit_index].0 ^ 0x9e3779b97f4a7c15) - 0.5) * step_y * 0.32,
            }
        };
        let radius =
            spacing(key.width, key.height, key.ids.len()) * (members.len() as f64).sqrt() * 0.27;
        for &index in members {
            let hash = stable_hash(&key.ids[index]);
            points[index] = Point {
                x: origin.x + (fraction(hash) - 0.5) * radius * 2.0,
                y: origin.y + (fraction(hash ^ 0x9e3779b97f4a7c15) - 0.5) * radius * 2.0,
            };
        }
        if focus_unit == Some(unit_index) {
            if let Some(focus) = key.focus {
                points[focus] = center;
            }
        }
        let movable: Vec<_> = members
            .iter()
            .copied()
            .filter(|i| key.focus != Some(*i))
            .collect();
        let average = mean(movable.iter().map(|&i| points[i]));
        for &index in &movable {
            points[index].x += origin.x - average.x;
            points[index].y += origin.y - average.y;
        }
    }
    if key.ids.len() == 2 {
        // A two-node field needs actual link-length elbow room rather than a
        // district interior; no other district should be stretched by focus.
        let half = spacing(key.width, key.height, 2) * 0.75;
        points[0] = Point {
            x: center.x - half,
            y: center.y,
        };
        points[1] = Point {
            x: center.x + half,
            y: center.y,
        };
    }
    let average = if key.focus.is_some() {
        center
    } else {
        mean(points.iter().copied())
    };
    let margin_x = 3.0_f64.min(width * 0.2);
    let margin_y = 3.0_f64.min(height * 0.2);
    let extent_x = points
        .iter()
        .map(|p| (p.x - average.x).abs())
        .fold(0.0_f64, f64::max);
    let extent_y = points
        .iter()
        .map(|p| (p.y - average.y).abs())
        .fold(0.0_f64, f64::max);
    let scale = ((width * 0.5 - margin_x) / extent_x.max(0.001))
        .min((height * 0.5 - margin_y) / extent_y.max(0.001))
        .min(1.0);
    for point in &mut points {
        point.x = center.x + (point.x - average.x) * scale;
        point.y = center.y + (point.y - average.y) * scale;
    }
    points
}
fn mean(points: impl Iterator<Item = Point>) -> Point {
    let (sum, count) = points.fold((Point::default(), 0), |(sum, count), p| {
        (
            Point {
                x: sum.x + p.x,
                y: sum.y + p.y,
            },
            count + 1,
        )
    });
    if count == 0 {
        Point::default()
    } else {
        Point {
            x: sum.x / count as f64,
            y: sum.y / count as f64,
        }
    }
}

fn settle(
    key: &LayoutKey,
    groups: &DisplayGroups,
    previous: Option<&(LayoutKey, Vec<Point>)>,
) -> Vec<Point> {
    let width = f64::from(key.width);
    let height = 2.0 * f64::from(key.height);
    let center = Point {
        x: width * 0.5,
        y: height * 0.5,
    };
    if key.ids.len() <= 1 || width == 0.0 || height == 0.0 {
        return vec![center; key.ids.len()];
    }
    let margin_x = 3.0_f64.min(width * 0.2);
    let margin_y = 3.0_f64.min(height * 0.2);
    let spacing = spacing(key.width, key.height, key.ids.len());
    let mut anchors = anchors(key, groups);
    let mut retained = vec![None; key.ids.len()];
    if let Some((old, points)) = previous {
        for (index, id) in key.ids.iter().enumerate() {
            if let Ok(old_index) = old.ids.binary_search(id) {
                let point = points[old_index];
                let point = Point {
                    x: if old.width > 0 {
                        point.x / f64::from(old.width) * width
                    } else {
                        center.x
                    },
                    y: if old.height > 0 {
                        point.y / (2.0 * f64::from(old.height)) * height
                    } else {
                        center.y
                    },
                };
                retained[index] = Some(point);
                anchors[index] = point;
            }
        }
        // Newly arrived memories begin by their strongest already visible tie,
        // not at a new global seed that would kick every old memory aside.
        for index in 0..key.ids.len() {
            if retained[index].is_some() {
                continue;
            }
            let neighbor = key
                .springs
                .iter()
                .filter_map(|link| {
                    let other = if link.a == index {
                        link.b
                    } else if link.b == index {
                        link.a
                    } else {
                        return None;
                    };
                    retained[other].map(|point| (link.weight, point))
                })
                .max_by_key(|candidate| candidate.0);
            if let Some((_, point)) = neighbor {
                let angle = fraction(stable_hash(&key.ids[index])) * std::f64::consts::TAU;
                anchors[index] = Point {
                    x: (point.x + angle.cos() * spacing * 0.7).clamp(margin_x, width - margin_x),
                    y: (point.y + angle.sin() * spacing * 0.7).clamp(margin_y, height - margin_y),
                };
            }
        }
    }
    if previous.is_none() && key.ids.len() == 2 {
        if let Some(focus) = key.focus {
            // Pinning the focus must not halve a small district's seeded link
            // lengths. Give its direct neighbors enough initial elbow room that
            // stronger springs shorten ties rather than expanding collisions.
            for spring in &key.springs {
                let other = if spring.a == focus {
                    spring.b
                } else if spring.b == focus {
                    spring.a
                } else {
                    continue;
                };
                let (dx, dy, distance) = separation(center, anchors[other], focus, other);
                let minimum = spacing * 1.30;
                if distance < minimum {
                    anchors[other] = Point {
                        x: (center.x + dx / distance * minimum).clamp(margin_x, width - margin_x),
                        y: (center.y + dy / distance * minimum).clamp(margin_y, height - margin_y),
                    };
                }
            }
        }
    }
    let mut points = anchors.clone();
    if let Some(focus) = key.focus {
        points[focus] = center;
    }
    let mut forces = vec![Point::default(); points.len()];
    let pairs = points.len().saturating_mul(points.len().saturating_sub(1)) / 2;
    let steps = (if previous.is_some() {
        REFRESH_STEPS
    } else {
        RELAXATION_STEPS
    })
    .min((2_000_000 / pairs.max(1)).max(2));
    let movement_cap = spacing * 0.45;
    let group_ids: Vec<_> = key
        .ids
        .iter()
        .map(|id| groups.group_for(id).map(|group| group.id))
        .collect();
    for step in 0..steps {
        for ((force, point), anchor) in forces.iter_mut().zip(&points).zip(&anchors) {
            *force = Point {
                x: (anchor.x - point.x) * 0.30,
                y: (anchor.y - point.y) * 0.30,
            };
        }
        for a in 0..points.len() {
            let comparison_window = (2_000_000 / (steps * points.len()).max(1)).max(1);
            for b in a + 1
                ..points
                    .len()
                    .min(a.saturating_add(comparison_window).saturating_add(1))
            {
                let (dx, dy, distance) = separation(points[a], points[b], a, b);
                let same_group = group_ids[a].is_some() && group_ids[a] == group_ids[b];
                let clearance = spacing * if same_group { 0.48 } else { 0.82 };
                let repulsion = (clearance - distance).max(0.0) * 0.45;
                let force = Point {
                    x: dx / distance * repulsion,
                    y: dy / distance * repulsion,
                };
                forces[a].x -= force.x;
                forces[a].y -= force.y;
                forces[b].x += force.x;
                forces[b].y += force.y;
            }
        }
        for spring in &key.springs {
            let weight = f64::from_bits(spring.weight);
            let (dx, dy, distance) =
                separation(points[spring.a], points[spring.b], spring.a, spring.b);
            let same_group =
                group_ids[spring.a].is_some() && group_ids[spring.a] == group_ids[spring.b];
            let rest = link_distance(weight, key.width, key.height, key.ids.len());
            // Cross-group links remain visible without annexing their districts.
            let strength = (0.025 + 0.065 * weight) * if same_group { 1.0 } else { 0.25 };
            let attraction = ((distance - rest) * strength).clamp(-spacing * 0.4, spacing * 0.4);
            let force = Point {
                x: dx / distance * attraction,
                y: dy / distance * attraction,
            };
            forces[spring.a].x += force.x;
            forces[spring.a].y += force.y;
            forces[spring.b].x -= force.x;
            forces[spring.b].y -= force.y;
        }
        let progress = step as f64 / (steps - 1) as f64;
        let max_step = spacing * (0.2 - 0.18 * progress);
        for (index, (point, force)) in points.iter_mut().zip(&forces).enumerate() {
            if key.focus == Some(index) {
                continue;
            }
            let dx = force.x * 0.3;
            let dy = force.y * 0.3;
            let scale = (max_step / dx.hypot(dy).max(f64::EPSILON)).min(1.0);
            point.x = (point.x + dx * scale).clamp(margin_x, width - margin_x);
            point.y = (point.y + dy * scale).clamp(margin_y, height - margin_y);
            if let Some(origin) = retained[index] {
                let dx = point.x - origin.x;
                let dy = point.y - origin.y;
                let scale = (movement_cap / dx.hypot(dy).max(f64::EPSILON)).min(1.0);
                point.x = origin.x + dx * scale;
                point.y = origin.y + dy * scale;
            }
        }
    }
    if key.focus.is_none() {
        // Balance initial composition without stretching distances. Explicit
        // focus stays centered; shift only the other nodes by a common vector.
        let average = mean(
            points
                .iter()
                .enumerate()
                .filter(|(i, _)| key.focus != Some(*i))
                .map(|(_, p)| *p),
        );
        let mut dx = center.x - average.x;
        let mut dy = center.y - average.y;
        for (i, point) in points.iter().enumerate() {
            if key.focus == Some(i) {
                continue;
            }
            dx = dx.clamp(margin_x - point.x, width - margin_x - point.x);
            dy = dy.clamp(margin_y - point.y, height - margin_y - point.y);
        }
        for (i, point) in points.iter_mut().enumerate() {
            if key.focus != Some(i) {
                point.x += dx;
                point.y += dy;
            }
        }
    }
    if key.ids.len() > 128 {
        // Local packing catches close pairs beyond the force-work budget.
        let mut occupied = Packing::default();
        for point in &mut points {
            *point = occupied.place(*point);
        }
    }
    points
}
fn separation(a: Point, b: Point, a_index: usize, b_index: usize) -> (f64, f64, f64) {
    let dx = b.x - a.x;
    let dy = b.y - a.y;
    let distance = dx.hypot(dy);
    if distance > 0.001 {
        (dx, dy, distance)
    } else {
        let angle = fraction((a_index as u64) << 32 | b_index as u64) * std::f64::consts::TAU;
        (angle.cos() * 0.001, angle.sin() * 0.001, 0.001)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{App, Edge, LensState, Node, demo_graph, demo_neighborhood};

    fn pair(weight: f64) -> Graph {
        Graph {
            nodes: ["a", "b"]
                .into_iter()
                .map(|id| Node {
                    id: id.into(),
                    ..Node::default()
                })
                .collect(),
            edges: vec![Edge {
                from: "a".into(),
                to: "b".into(),
                kind: "associative".into(),
                weight,
            }],
            ..Graph::default()
        }
    }

    fn distance(points: &[Point]) -> f64 {
        (points[0].x - points[1].x).hypot(points[0].y - points[1].y)
    }

    #[test]
    fn repaint_and_presentational_changes_reuse_the_settled_field() {
        let mut app = App::new(vec![], String::new(), true);
        app.graph = demo_graph();
        let first = app.layout.borrow_mut().positions(&app.graph, 90, 24);
        for tick in 0..20 {
            app.tick = tick;
            app.selected = tick as usize % app.graph.nodes.len();
            app.graph.note = format!("Refreshed {tick}");
            app.graph.nodes[0].summary = format!("A changed title {tick}");
            let id = app.selected_node().unwrap().id.clone();
            app.open_edge_lens(id.clone(), demo_neighborhood(&id), LensState::Ready);
            assert_eq!(first, app.layout.borrow_mut().positions(&app.graph, 90, 24));
            app.close_edge_lens();
        }
        assert_eq!(app.layout.borrow().solves, 1);
    }

    #[test]
    fn stronger_ties_settle_closer_without_normalizing_the_result() {
        for focus in [None, Some("a".into())] {
            let mut graph = pair(0.1);
            graph.focus = focus;
            let mut cache = LayoutCache::default();
            let weak = distance(&cache.positions(&graph, 90, 30));
            graph.edges[0].weight = 0.9;
            let strong = distance(&cache.positions(&graph, 90, 30));
            // Weight is a guide, not a mandate to shrink every pair by a fixed
            // percentage at the expense of the seeded composition.
            assert!(
                strong < weak,
                "focus={:?}, weak={weak}, strong={strong}",
                graph.focus
            );
            assert_eq!(cache.solves, 2);
        }
    }

    #[test]
    fn sparse_fields_keep_islands_off_the_rim_and_edges_do_not_remap_them() {
        let mut graph = Graph {
            nodes: (0..16)
                .map(|i| Node {
                    id: format!("island-{i:02}"),
                    ..Node::default()
                })
                .collect(),
            edges: vec![Edge {
                from: "island-00".into(),
                to: "island-01".into(),
                kind: "associative".into(),
                weight: 0.1,
            }],
            ..Graph::default()
        };
        for (width, height) in [(90, 24), (60, 15)] {
            for focus in [None, Some("island-00".into())] {
                graph.focus = focus;
                graph.edges[0].weight = 0.1;
                let before = LayoutCache::default().positions(&graph, width, height);
                let on_rim = before
                    .iter()
                    .skip(2)
                    .filter(|p| {
                        p.x <= 4.0
                            || p.x >= f64::from(width) - 4.0
                            || p.y <= 4.0
                            || p.y >= f64::from(height) * 2.0 - 4.0
                    })
                    .count();
                assert!(
                    on_rim <= 4,
                    "{width}×{height}: {on_rim} isolated nodes on rim"
                );
                graph.edges[0].weight = 0.9;
                let after = LayoutCache::default().positions(&graph, width, height);
                let local_scale = spacing(width, height, graph.nodes.len());
                for (a, b) in before.iter().zip(&after).skip(2) {
                    let shift = (a.x - b.x).hypot(a.y - b.y);
                    assert!(
                        shift < local_scale * 0.75,
                        "one changed link moved an unrelated island by {shift}"
                    );
                }
            }
        }
    }

    #[test]
    fn input_order_and_relation_direction_do_not_rotate_the_field() {
        let mut graph = demo_graph();
        let mut cache = LayoutCache::default();
        let first = cache.positions(&graph, 90, 24);
        graph.nodes.reverse();
        graph.edges.reverse();
        for edge in &mut graph.edges {
            std::mem::swap(&mut edge.from, &mut edge.to);
        }
        let mut reversed = cache.positions(&graph, 90, 24);
        reversed.reverse();
        assert_eq!(first, reversed);
        assert_eq!(cache.solves, 1);
        assert_eq!(first, {
            let mut independent = LayoutCache::default().positions(&graph, 90, 24);
            independent.reverse();
            independent
        });
    }

    #[test]
    fn only_geometry_topology_and_explicit_focus_invalidate_the_cache() {
        let mut graph = pair(0.5);
        let mut cache = LayoutCache::default();
        cache.positions(&graph, 90, 24);
        cache.positions(&graph, 91, 24);
        cache.positions(&graph, 91, 25);
        graph.focus = Some("a".into());
        let points = cache.positions(&graph, 91, 25);
        assert_eq!(points[0], Point { x: 45.5, y: 25.0 });
        graph.edges.clear();
        cache.positions(&graph, 91, 25);
        graph.nodes.push(Node {
            id: "c".into(),
            ..Node::default()
        });
        cache.positions(&graph, 91, 25);
        assert_eq!(cache.solves, 6);
    }

    #[test]
    fn disconnected_and_sloppy_weights_stay_finite_and_inside_fixed_bounds() {
        for weight in [
            0.0,
            -0.0,
            -1.0,
            2.0,
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
        ] {
            let graph = pair(weight);
            let mut cache = LayoutCache::default();
            for (width, height) in [(0, 0), (0, 10), (1, 1), (90, 24), (u16::MAX, u16::MAX)] {
                let first = cache.positions(&graph, width, height);
                for point in &first {
                    assert!(point.x.is_finite() && point.y.is_finite());
                    assert!((0.0..=f64::from(width)).contains(&point.x));
                    assert!(
                        (0.0..=2.0 * f64::from(height)).contains(&point.y),
                        "size={width}x{height}, point={point:?}, weight={weight}"
                    );
                }
                let solves = cache.solves;
                assert_eq!(first, cache.positions(&graph, width, height));
                assert_eq!(solves, cache.solves);
            }
        }
        let no_edge = Graph {
            edges: vec![],
            ..pair(0.0)
        };
        assert_eq!(
            LayoutCache::default().positions(&no_edge, 90, 24),
            LayoutCache::default().positions(&pair(0.0), 90, 24)
        );
    }

    #[test]
    fn a_single_node_is_centered_and_empty_graph_is_empty() {
        let mut graph = pair(0.0);
        graph.nodes.truncate(1);
        assert_eq!(
            LayoutCache::default().positions(&graph, 91, 23),
            vec![Point { x: 45.5, y: 23.0 }]
        );
        assert!(
            LayoutCache::default()
                .positions(&Graph::default(), 91, 23)
                .is_empty()
        );
    }

    #[test]
    fn large_world_contains_every_loaded_node_and_resize_does_not_solve() {
        let graph = Graph {
            nodes: (0..240)
                .map(|i| Node {
                    id: format!("node-{i:03}"),
                    ..Node::default()
                })
                .collect(),
            ..Graph::default()
        };
        let mut cache = LayoutCache::default();
        let points = cache.world_positions(&graph);
        assert_eq!(points.len(), 240);
        assert!(LayoutCache::world_size(240).0 > 120);
        let first = cache.project(
            &graph,
            &points,
            Some((&graph.nodes[0].id, points[0])),
            80,
            24,
        );
        let _ = cache.project(
            &graph,
            &points,
            Some((&graph.nodes[0].id, points[0])),
            100,
            28,
        );
        let _ = cache.project(
            &graph,
            &points,
            Some((&graph.nodes[0].id, points[0])),
            80,
            24,
        );
        cache.pan(12.0, 0.0);
        assert_eq!(points, cache.world_positions(&graph));
        assert_eq!(cache.solves, 1);
        assert_ne!(
            first,
            cache.project(
                &graph,
                &points,
                Some((&graph.nodes[0].id, points[0])),
                80,
                24
            )
        );
    }

    #[test]
    fn repeated_pointer_panning_changes_only_projection_not_solver_or_world() {
        let graph = pair(0.75);
        let mut cache = LayoutCache::default();
        let world = cache.world_positions(&graph);
        cache.project(&graph, &world, Some(("a", world[0])), 80, 24);
        let center = cache.camera.center;
        cache.set_map_area(Some(ratatui::layout::Rect::new(1, 2, 80, 24)));
        cache.start_drag(8, 8);
        cache.project(&graph, &world, Some(("b", world[1])), 80, 24);
        assert_eq!(
            cache.camera.center, center,
            "selection during a grab cannot snap"
        );
        for column in 9..50 {
            cache.drag_to(column, 12);
            assert_eq!(cache.world_positions(&graph), world);
            cache.project(&graph, &world, Some(("b", world[1])), 80, 24);
            assert_eq!(cache.solves, 1);
        }
        let moved = cache.camera.center;
        cache.cancel_drag();
        cache.drag_to(0, 0);
        assert_eq!(cache.camera.center, moved);
    }

    #[test]
    fn incremental_nodes_and_late_edges_preserve_every_existing_world_point() {
        let mut graph = Graph {
            nodes: (0..200)
                .map(|i| Node {
                    id: format!("node-{i:03}"),
                    ..Node::default()
                })
                .collect(),
            ..Graph::default()
        };
        let mut cache = LayoutCache::default();
        let first = cache.world_positions(&graph);
        cache.project(&graph, &first, Some((&graph.nodes[0].id, first[0])), 80, 24);
        let camera = cache.camera.center;
        graph.nodes.extend((200..264).map(|i| Node {
            id: format!("node-{i:03}"),
            ..Node::default()
        }));
        graph.edges.push(Edge {
            from: "node-000".into(),
            to: "node-200".into(),
            kind: "supports".into(),
            weight: 0.75,
        });
        let appended = cache.world_positions(&graph);
        assert_eq!(&appended[..200], &first);
        assert_eq!(appended.len(), 264);
        cache.project(
            &graph,
            &appended,
            Some((&graph.nodes[0].id, appended[0])),
            80,
            24,
        );
        assert_eq!(cache.camera.center, camera);
        graph.edges.extend((0..263).map(|i| Edge {
            from: format!("node-{i:03}"),
            to: format!("node-{:03}", i + 1),
            kind: "supports".into(),
            weight: 0.75,
        }));
        assert_eq!(appended, cache.world_positions(&graph));
        assert_eq!(cache.solves, 1);
        assert!(cache.groups(&graph).group_for("node-000").is_some());
        cache.reset();
        assert_eq!(cache.world_positions(&graph).len(), 264);
        assert_eq!(cache.solves, 1); // deliberate reset permits a fresh full composition
    }

    fn pockets() -> Graph {
        Graph {
            nodes: ["a", "b", "c", "d", "e", "f"]
                .into_iter()
                .map(|id| Node {
                    id: id.into(),
                    ..Node::default()
                })
                .collect(),
            edges: [
                ("a", "b", 0.9),
                ("b", "c", 0.9),
                ("a", "c", 0.9),
                ("d", "e", 0.9),
                ("e", "f", 0.9),
                ("d", "f", 0.9),
                ("c", "d", 0.02),
            ]
            .into_iter()
            .map(|(from, to, weight)| Edge {
                from: from.into(),
                to: to.into(),
                kind: "associative".into(),
                weight,
            })
            .collect(),
            ..Graph::default()
        }
    }

    #[test]
    fn districts_are_compact_balanced_and_padded_across_aspect_ratios() {
        for (width, height) in [(90, 24), (60, 15), (150, 12), (28, 36), (12, 6)] {
            let graph = pockets();
            let mut cache = LayoutCache::default();
            let points = cache.positions(&graph, width, height);
            let groups = cache.groups(&graph);
            assert_eq!(groups.groups.len(), 2);
            let centroids: Vec<_> =
                groups
                    .groups
                    .iter()
                    .map(|group| {
                        mean(group.members.iter().map(|id| {
                            points[graph.nodes.iter().position(|n| &n.id == id).unwrap()]
                        }))
                    })
                    .collect();
            let between = (centroids[0].x - centroids[1].x).hypot(centroids[0].y - centroids[1].y);
            for (group, center) in groups.groups.iter().zip(&centroids) {
                let radius = group
                    .members
                    .iter()
                    .map(|id| {
                        let point = points[graph.nodes.iter().position(|n| &n.id == id).unwrap()];
                        (point.x - center.x).hypot(point.y - center.y)
                    })
                    .fold(0.0_f64, f64::max);
                assert!(
                    radius < between * 0.65,
                    "{width}x{height}: district radius={radius}, separation={between}"
                );
            }
            let center = mean(points.iter().copied());
            assert!((center.x - f64::from(width) * 0.5).abs() < f64::from(width) * 0.08);
            assert!((center.y - f64::from(height)).abs() < f64::from(height) * 0.16);
            for point in &points {
                let mx = 3.0_f64.min(f64::from(width) * 0.2);
                let my = 3.0_f64.min(2.0 * f64::from(height) * 0.2);
                assert!((mx - 1e-6..=f64::from(width) - mx + 1e-6).contains(&point.x));
                assert!((my - 1e-6..=2.0 * f64::from(height) - my + 1e-6).contains(&point.y));
            }
        }
    }

    #[test]
    fn live_topology_refresh_warm_starts_survivors_and_retains_group_identity() {
        let mut graph = pockets();
        let mut cache = LayoutCache::default();
        let before = cache.positions(&graph, 90, 24);
        let groups = cache.groups(&graph);
        graph.nodes.push(Node {
            id: "new".into(),
            ..Node::default()
        });
        graph.edges.push(Edge {
            from: "a".into(),
            to: "new".into(),
            kind: "associative".into(),
            weight: 0.8,
        });
        graph.edges[0].weight = 0.85;
        let after = cache.positions(&graph, 90, 24);
        for (a, b) in before.iter().zip(&after) {
            let shift = (a.x - b.x).hypot(a.y - b.y);
            assert!(
                shift <= spacing(90, 24, graph.nodes.len()) * 0.45 + 1e-6,
                "survivor jumped {shift}"
            );
        }
        assert_eq!(
            groups.group_for("a").unwrap().id,
            cache.groups(&graph).group_for("a").unwrap().id
        );
        assert_eq!(after, cache.positions(&graph, 90, 24));
        assert_eq!(cache.solves, 2);
        graph.nodes.pop();
        graph.edges.pop();
        let removed = cache.positions(&graph, 90, 24);
        assert_eq!(removed.len(), before.len());
        for (a, b) in after.iter().zip(&removed) {
            assert!(
                (a.x - b.x).hypot(a.y - b.y) <= spacing(90, 24, graph.nodes.len()) * 0.45 + 1e-6
            );
        }
    }

    #[test]
    fn resize_preserves_normalized_home_and_clone_restores_exact_field() {
        let graph = pockets();
        let mut cache = LayoutCache::default();
        let before = cache.positions(&graph, 90, 24);
        let mut saved = cache.clone();
        let after = cache.positions(&graph, 120, 30);
        for (a, b) in before.iter().zip(&after) {
            let expected = Point {
                x: a.x / 90.0 * 120.0,
                y: a.y / 48.0 * 60.0,
            };
            assert!(
                (expected.x - b.x).hypot(expected.y - b.y)
                    <= spacing(120, 30, graph.nodes.len()) * 0.45 + 1e-6
            );
        }
        assert_eq!(before, saved.positions(&graph, 90, 24));
        assert_eq!(saved.solves, 1);
        cache.reset();
        assert_eq!(before, cache.positions(&graph, 90, 24));
        assert_eq!(cache.solves, 1);
    }

    #[test]
    fn group_labels_follow_cards_without_running_or_moving_layout() {
        let mut graph = pockets();
        let mut cache = LayoutCache::default();
        let before = cache.positions(&graph, 90, 24);
        let groups = cache.groups(&graph);
        assert!(groups.groups.iter().all(|g| g.label.is_none()));
        for node in graph.nodes.iter_mut().take(3) {
            node.tags.push("rust".into());
            node.tags_complete = true;
        }
        assert!(cache.groups(&graph).group_for("a").unwrap().label.is_none());
        for node in &mut graph.nodes {
            node.tags_complete = true;
        }
        assert_eq!(
            cache
                .groups(&graph)
                .group_for("a")
                .unwrap()
                .label
                .as_deref(),
            Some("rust")
        );
        assert_eq!(
            groups.group_for("a").unwrap().id,
            cache.groups(&graph).group_for("a").unwrap().id
        );
        assert_eq!(before, cache.positions(&graph, 90, 24));
        assert_eq!(cache.solves, 1);
    }

    #[test]
    fn focused_and_repeatedly_refreshed_fields_do_not_drift_to_a_corner() {
        for (width, height) in [(90, 24), (150, 12), (28, 36)] {
            for focus in [None, Some("a".to_owned())] {
                let mut graph = pockets();
                graph.focus = focus;
                let mut cache = LayoutCache::default();
                for iteration in 0..30 {
                    graph.edges[0].weight = if iteration % 2 == 0 { 0.85 } else { 0.95 };
                    let points = cache.positions(&graph, width, height);
                    let center = mean(points.iter().copied());
                    assert!(
                        (center.x - f64::from(width) * 0.5).abs() <= f64::from(width) * 0.15,
                        "{width}x{height}: off-center x={center:?}"
                    );
                    assert!(
                        (center.y - f64::from(height)).abs() <= 2.0 * f64::from(height) * 0.15,
                        "{width}x{height}: off-center y={center:?}"
                    );
                    if graph.focus.is_some() {
                        assert_eq!(
                            points[0],
                            Point {
                                x: f64::from(width) * 0.5,
                                y: f64::from(height)
                            }
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn actual_hub_demo_has_separate_compact_district_bounds_and_centered_focus() {
        let graph = demo_graph();
        for (width, height) in [(90, 24), (70, 25), (60, 15), (150, 12), (28, 36)] {
            let mut cache = LayoutCache::default();
            let points = cache.positions(&graph, width, height);
            let groups = cache.groups(&graph);
            assert_eq!(
                groups.groups.len(),
                3,
                "the demo must exercise three detected groups"
            );
            let focus = graph
                .nodes
                .iter()
                .position(|n| Some(&n.id) == graph.focus.as_ref())
                .unwrap();
            assert_eq!(
                points[focus],
                Point {
                    x: f64::from(width) * 0.5,
                    y: f64::from(height)
                }
            );
            let bounds: Vec<_> = groups
                .groups
                .iter()
                .map(|group| {
                    let points: Vec<_> = group
                        .members
                        .iter()
                        .map(|id| points[graph.nodes.iter().position(|n| &n.id == id).unwrap()])
                        .collect();
                    let left = points.iter().map(|p| p.x).fold(f64::INFINITY, f64::min);
                    let right = points.iter().map(|p| p.x).fold(f64::NEG_INFINITY, f64::max);
                    let bottom = points.iter().map(|p| p.y).fold(f64::INFINITY, f64::min);
                    let top = points.iter().map(|p| p.y).fold(f64::NEG_INFINITY, f64::max);
                    (left, right, bottom, top)
                })
                .collect();
            let padding = spacing(width, height, graph.nodes.len()) * 0.12;
            for a in 0..bounds.len() {
                for b in a + 1..bounds.len() {
                    let a = bounds[a];
                    let b = bounds[b];
                    assert!(
                        a.1 + padding < b.0
                            || b.1 + padding < a.0
                            || a.3 + padding < b.2
                            || b.3 + padding < a.2,
                        "{width}x{height}: demo group bounds interleave: {a:?} / {b:?}"
                    );
                }
            }
            let center = mean(points.iter().copied());
            assert!((center.x - f64::from(width) * 0.5).abs() < f64::from(width) * 0.10);
            assert!((center.y - f64::from(height)).abs() < 2.0 * f64::from(height) * 0.10);
        }
    }
}
