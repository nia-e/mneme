//! Bounded loaded-link groups and a settled, warm-started display composition.
//! Repaints, selection, activity and edge lenses never run the solver.

mod districts;

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

    /// Deliberate local recomposition; late topology never moves an established
    /// field by itself. Retain group identity, but drop warm-start geometry.
    pub(crate) fn recompose(&mut self) {
        self.field = None;
        self.world = None;
        self.camera = Camera::default();
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

/// Camera transforms do not change solver inputs. Coordinates use terminal
/// columns horizontally and twice terminal rows vertically (roughly square).
#[derive(Clone, Debug)]
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
    zoom: f64,
}

impl Default for Camera {
    fn default() -> Self {
        Self {
            center: None,
            viewport: (0, 0),
            index_only: false,
            index_ids: Vec::new(),
            hits: Vec::new(),
            markers: Vec::new(),
            map_area: None,
            drag: None,
            selected: None,
            zoom: 1.0,
        }
    }
}

const MIN_ZOOM: f64 = 1.0e-6;
const MAX_ZOOM: f64 = 8.0;

/// Inverse of the renderer's 2x4 Braille-dot raster cell choice. The cursor
/// anchors the same screen-space cell center used for marker/halo placement.
pub(crate) fn map_cell_center(column: u16, row: u16, width: u16, height: u16) -> Point {
    let w = f64::from(width.max(1));
    let h = f64::from(height.max(1)) * 2.0;
    Point {
        x: (f64::from(column) * 2.0 + 0.5) * w / (w * 2.0 - 1.0),
        y: h - (f64::from(row) * 4.0 + 1.5) * h / (h * 2.0 - 1.0),
    }
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
            x: drag.center.x - f64::from(dx) / self.camera.zoom,
            y: drag.center.y + 2.0 * f64::from(dy) / self.camera.zoom,
        };
        if self.camera.center != Some(center) {
            self.camera.center = Some(center);
            self.invalidate_hits();
        }
    }

    pub(crate) fn zoom_factor(&self) -> f64 {
        self.camera.zoom
    }

    fn next_zoom(&self, factor: f64) -> Option<f64> {
        if !factor.is_finite() || factor <= 0.0 {
            return None;
        }
        let zoom = (self.camera.zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        (zoom != self.camera.zoom).then_some(zoom)
    }

    fn set_camera_transform(&mut self, center: Point, zoom: f64) -> bool {
        if !center.x.is_finite()
            || !center.y.is_finite()
            || !zoom.is_finite()
            || !(MIN_ZOOM..=MAX_ZOOM).contains(&zoom)
            || (self.camera.center == Some(center) && self.camera.zoom == zoom)
        {
            return false;
        }
        self.camera.center = Some(center);
        self.camera.zoom = zoom;
        self.cancel_drag();
        self.invalidate_hits();
        true
    }

    /// Zoom around the viewport center; this never changes world coordinates.
    pub(crate) fn zoom_by(&mut self, factor: f64) -> bool {
        let Some(center) = self.camera.center else {
            return false;
        };
        let Some(zoom) = self.next_zoom(factor) else {
            return false;
        };
        if self.camera.viewport.0 == 0 || self.camera.viewport.1 == 0 {
            return false;
        }
        self.set_camera_transform(center, zoom)
    }

    /// Wheel zoom is accepted only in the current map, and keeps the exact
    /// world point under the cursor's Braille cell center stationary on screen.
    pub(crate) fn zoom_at(&mut self, factor: f64, column: u16, row: u16) -> bool {
        let (Some(area), Some(center)) = (self.camera.map_area, self.camera.center) else {
            return false;
        };
        if area.width == 0
            || area.height == 0
            || (area.width, area.height) != self.camera.viewport
            || !area.contains((column, row).into())
        {
            return false;
        }
        let Some(zoom) = self.next_zoom(factor) else {
            return false;
        };
        let screen = map_cell_center(column - area.x, row - area.y, area.width, area.height);
        let dx = screen.x - f64::from(area.width) * 0.5;
        let dy = screen.y - f64::from(area.height);
        let inverse_delta = self.camera.zoom.recip() - zoom.recip();
        self.set_camera_transform(
            Point {
                x: center.x + dx * inverse_delta,
                y: center.y + dy * inverse_delta,
            },
            zoom,
        )
    }

    /// 100% scale, keeping the current viewport center rather than recomposing.
    pub(crate) fn reset_zoom(&mut self) -> bool {
        let Some(center) = self.camera.center else {
            return false;
        };
        self.set_camera_transform(center, 1.0)
    }

    /// Fit the supplied loaded scene only; no graph solve, topology read, or
    /// provider fetch. Reject an invalid/unrepresentable fit as a whole rather
    /// than silently omitting a far or nonfinite point from the bounds.
    pub(crate) fn fit_points(&mut self, points: &[Point]) -> bool {
        let (width, height) = self.camera.viewport;
        if points.is_empty()
            || width == 0
            || height == 0
            || points.iter().any(|p| !p.x.is_finite() || !p.y.is_finite())
        {
            return false;
        }
        let (mut left, mut right, mut bottom, mut top) = (
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
        );
        for point in points {
            left = left.min(point.x);
            right = right.max(point.x);
            bottom = bottom.min(point.y);
            top = top.max(point.y);
        }
        // Halving before subtracting/adding avoids overflowing finite endpoints.
        let half_x = right * 0.5 - left * 0.5;
        let half_y = top * 0.5 - bottom * 0.5;
        let center = Point {
            x: left * 0.5 + right * 0.5,
            y: bottom * 0.5 + top * 0.5,
        };
        // Five screen-space units keep the existing halo and numeric labels off
        // the rim. Tiny viewports retain a positive proportional interior.
        let room_x = (f64::from(width) * 0.5 - 5.0).max(f64::from(width) * 0.1);
        let room_y = (f64::from(height) - 5.0).max(f64::from(height) * 0.2);
        let zoom = (if half_x > 0.0 { room_x / half_x } else { 1.0 })
            .min(if half_y > 0.0 { room_y / half_y } else { 1.0 })
            .min(1.0);
        if zoom < MIN_ZOOM {
            return false;
        }
        self.set_camera_transform(center, zoom)
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
                let half_x = (f64::from(width) / 2.0 - 5.0).max(0.0) / self.camera.zoom;
                let half_y = (f64::from(height) - 5.0).max(0.0) / self.camera.zoom;
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
                x: (p.x - center.x) * self.camera.zoom + f64::from(width) / 2.0,
                y: (p.y - center.y) * self.camera.zoom + f64::from(height),
            })
            .collect()
    }

    pub(crate) fn pan(&mut self, dx: f64, dy: f64) {
        if !dx.is_finite() || !dy.is_finite() {
            return;
        }
        if let Some(center) = self.camera.center {
            self.set_camera_transform(
                Point {
                    x: center.x + dx / self.camera.zoom,
                    y: center.y + dy / self.camera.zoom,
                },
                self.camera.zoom,
            );
        }
    }
}

const WORLD_CLEARANCE: f64 = 7.0;
/// Bounded local search, followed by an O(1) vacant-envelope fallback. Failure
/// to find a near slot changes placement, never loaded-node coverage.
struct Packing {
    cells: std::collections::HashMap<(i32, i32), Vec<Point>>,
    max_x: Option<f64>,
    clearance: f64,
}
impl Default for Packing {
    fn default() -> Self {
        Self::with_clearance(WORLD_CLEARANCE)
    }
}
impl Packing {
    fn with_clearance(clearance: f64) -> Self {
        Self {
            cells: Default::default(),
            max_x: None,
            clearance,
        }
    }
    fn occupy(&mut self, point: Point) {
        self.cells
            .entry((
                (point.x / self.clearance).floor() as i32,
                (point.y / self.clearance).floor() as i32,
            ))
            .or_default()
            .push(point);
        self.max_x = Some(self.max_x.map_or(point.x, |x| x.max(point.x)));
    }
    fn place(&mut self, origin: Point) -> Point {
        self.place_with_step(origin, self.clearance)
    }
    fn place_near(&mut self, origin: Point) -> Point {
        // Settled interiors already have useful geometry. Search for the
        // smallest local correction before falling back to a vacant envelope.
        self.place_with_step(origin, self.clearance * 0.25)
    }
    fn place_with_step(&mut self, origin: Point, step: f64) -> Point {
        let mut candidate = origin;
        for attempt in 0..128 {
            let cell = (
                (candidate.x / self.clearance).floor() as i32,
                (candidate.y / self.clearance).floor() as i32,
            );
            let free = (-1..=1).all(|dx| {
                (-1..=1).all(|dy| {
                    self.cells
                        .get(&(cell.0 + dx, cell.1 + dy))
                        .is_none_or(|neighbors| {
                            neighbors.iter().all(|point| {
                                (point.x - candidate.x).hypot(point.y - candidate.y)
                                    >= self.clearance
                            })
                        })
                })
            });
            if free {
                self.occupy(candidate);
                return candidate;
            }
            let radius = step * ((attempt + 1) as f64).sqrt();
            let angle = (attempt + 1) as f64 * 2.399963229728653;
            candidate = Point {
                x: origin.x + radius * angle.cos(),
                y: origin.y + radius * angle.sin(),
            };
        }
        candidate = Point {
            x: self.max_x.unwrap_or(origin.x) + self.clearance * 2.0,
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

/// Display groups and standalone notes are equally real placement units.
fn layout_units(key: &LayoutKey, groups: &DisplayGroups) -> Vec<(u64, Vec<usize>)> {
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
    for (_, members) in &mut units {
        members.sort_unstable();
    }
    units.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.first().cmp(&b.1.first())));
    units
}

/// Compose district centers, then give each district a small irregular interior.
/// No corner-assigned global hash anchors or expansion into spare canvas area.
fn anchors(key: &LayoutKey, groups: &DisplayGroups) -> Vec<Point> {
    let width = f64::from(key.width);
    let height = 2.0 * f64::from(key.height);
    let center = Point {
        x: width * 0.5,
        y: height * 0.5,
    };
    let units = layout_units(key, groups);
    let mut points = vec![center; key.ids.len()];
    let centers = districts::centers(key, &units);
    let focus_unit = key.focus.and_then(|focus| {
        units
            .iter()
            .position(|(_, members)| members.contains(&focus))
    });
    for (unit_index, (_, members)) in units.iter().enumerate() {
        let origin = centers[unit_index];
        let radius =
            spacing(key.width, key.height, key.ids.len()) * (members.len() as f64).sqrt() * 0.27;
        for (index, offset) in district_interior(key, members, radius) {
            points[index] = Point {
                x: origin.x + offset.x,
                y: origin.y + offset.y,
            };
        }
        if focus_unit == Some(unit_index) {
            if let Some(focus) = key.focus {
                points[focus] = center;
            }
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
    // The quotient layout owns the origin. A node-weighted recenter here
    // would turn a large peripheral district into the hub again.
    let average = center;
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
/// Shared exact seed geometry: district packing measures the same recentered
/// square-hash interior that anchors places, rather than assuming a radius is an
/// enclosing disk. Focus is fixed at its district origin, not in the mean.
fn district_interior(key: &LayoutKey, members: &[usize], radius: f64) -> Vec<(usize, Point)> {
    let mut points: Vec<_> = members
        .iter()
        .map(|&index| {
            let hash = stable_hash(&key.ids[index]);
            (
                index,
                Point {
                    x: (fraction(hash) - 0.5) * radius * 2.0,
                    y: (fraction(hash ^ 0x9e3779b97f4a7c15) - 0.5) * radius * 2.0,
                },
            )
        })
        .collect();
    points.sort_by_key(|(index, _)| *index);
    let average = mean(
        points
            .iter()
            .filter(|(index, _)| key.focus != Some(*index))
            .map(|(_, point)| *point),
    );
    for (index, point) in &mut points {
        if key.focus == Some(*index) {
            *point = Point::default();
        } else {
            point.x -= average.x;
            point.y -= average.y;
        }
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
    if previous.is_none() && key.ids.len() > 2 {
        let units = layout_units(key, groups);
        if units.len() > 2 || key.ids.len() > 128 {
            // Give crowded interiors the same minimum marker spacing at every
            // field size, not only beyond the force-work cutoff. Keep the
            // specialized one/two-unit composition unchanged for small fields.
            // A global node-by-node spill scatters later groups into earlier
            // ones after their centers were carefully separated.
            for (_, members) in &units {
                if key.ids.len() <= 128 && members.len() <= 2 {
                    continue;
                }
                let clearance = if key.ids.len() > 128 {
                    WORLD_CLEARANCE
                } else {
                    (spacing * 0.48).min(WORLD_CLEARANCE)
                };
                let mut occupied = Packing::with_clearance(clearance);
                if let Some(focus) = key.focus.filter(|focus| members.contains(focus)) {
                    occupied.occupy(points[focus]);
                }
                for &index in members {
                    if key.focus != Some(index) {
                        points[index] = occupied.place_near(points[index]);
                    }
                }
            }
        }
        // Measure the actual settled interiors, then translate whole districts.
        // Nothing may pack individual nodes again after this final clearance pass.
        districts::arrange_settled(key, &units, &mut points);
    } else if key.ids.len() > 128 {
        // Keep the existing warm-start path; it does not recompose districts.
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

    #[test]
    fn initial_district_packing_keeps_focus_and_marker_clearance_on_both_size_paths() {
        for count in [96, 156] {
            let focus_index = count - 25;
            let graph = Graph {
                nodes: (0..count)
                    .map(|i| Node {
                        id: format!("packed-{i:03}"),
                        ..Node::default()
                    })
                    .collect(),
                edges: (0..count)
                    .filter(|i| i % 12 != 11)
                    .map(|i| Edge {
                        from: format!("packed-{i:03}"),
                        to: format!("packed-{:03}", i + 1),
                        kind: "associative".into(),
                        weight: 0.8,
                    })
                    .collect(),
                // Not the first member or first district in canonical order.
                focus: Some(format!("packed-{focus_index:03}")),
                ..Graph::default()
            };
            let mut cache = LayoutCache::default();
            let points = cache.world_positions(&graph);
            let (width, height) = LayoutCache::world_size(graph.nodes.len());
            let center = Point {
                x: f64::from(width) * 0.5,
                y: f64::from(height),
            };
            assert!((points[focus_index].x - center.x).abs() < 1e-9);
            assert!((points[focus_index].y - center.y).abs() < 1e-9);
            let clearance = if count > 128 {
                WORLD_CLEARANCE
            } else {
                (spacing(width, height, count) * 0.48).min(WORLD_CLEARANCE)
            };
            let mut checked_groups = 0;
            for group in cache.groups(&graph).groups {
                // Small-field pairs retain their weight-responsive geometry;
                // minimum packing clearance applies to the larger interiors.
                if count <= 128 && group.members.len() <= 2 {
                    continue;
                }
                checked_groups += 1;
                let members: Vec<_> = group
                    .members
                    .iter()
                    .map(|id| graph.nodes.iter().position(|node| &node.id == id).unwrap())
                    .collect();
                for (i, &a) in members.iter().enumerate() {
                    for &b in &members[i + 1..] {
                        assert!(
                            (points[a].x - points[b].x).hypot(points[a].y - points[b].y)
                                >= clearance - 1e-9
                        );
                    }
                }
            }
            assert!(checked_groups > 0);
            assert_eq!(points, cache.world_positions(&graph));
        }
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
    fn explicit_recompose_applies_late_links_once_and_keeps_group_identity() {
        let mut graph = pockets();
        let links = std::mem::take(&mut graph.edges);
        let mut cache = LayoutCache::default();
        let initial = cache.world_positions(&graph);
        graph.edges = links;
        assert_eq!(cache.world_positions(&graph), initial);
        let groups = cache.groups(&graph);
        cache.camera.center = Some(Point {
            x: -100.0,
            y: -100.0,
        });
        cache.recompose();
        assert_eq!(cache.camera.center, None);
        let recomposed = cache.world_positions(&graph);
        assert_ne!(recomposed, initial);
        assert_eq!(cache.groups(&graph), groups);
        assert_eq!(cache.world_positions(&graph), recomposed);
        assert_eq!(cache.solves, 2);
    }

    #[test]
    fn settling_does_not_recenter_the_world_on_a_large_peripheral_district() {
        use crate::clusters::{DisplayGroup, DisplayGroupId};
        let graph = Graph {
            nodes: (0..46)
                .map(|i| Node {
                    id: format!("node-{i:03}"),
                    ..Node::default()
                })
                .collect(),
            edges: [(40, 0), (40, 42), (40, 44)]
                .into_iter()
                .map(|(a, b)| Edge {
                    from: format!("node-{a:03}"),
                    to: format!("node-{b:03}"),
                    weight: 0.4,
                    kind: "supports".into(),
                })
                .collect(),
            ..Graph::default()
        };
        let groups = DisplayGroups {
            groups: [(0, 40), (40, 42), (42, 44), (44, 46)]
                .into_iter()
                .enumerate()
                .map(|(i, (a, b))| DisplayGroup {
                    id: DisplayGroupId(i as u64),
                    members: graph.nodes[a..b].iter().map(|n| n.id.clone()).collect(),
                    label: None,
                })
                .collect(),
            lookup: Default::default(),
        };
        let points = settle(&LayoutKey::new(&graph, 240, 80), &groups, None);
        let distances: Vec<_> = [(0, 40), (40, 42), (42, 44), (44, 46)]
            .into_iter()
            .map(|(a, b)| {
                let center = mean(points[a..b].iter().copied());
                (center.x - 120.0).hypot(center.y - 80.0)
            })
            .collect();
        assert!(
            distances[1] < distances[0]
                && distances[1] < distances[2]
                && distances[1] < distances[3],
            "district distances from center: {distances:?}"
        );
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

    fn camera_fixture() -> (LayoutCache, Graph, Vec<Point>) {
        let graph = pair(0.75);
        let mut cache = LayoutCache::default();
        let points = cache.world_positions(&graph);
        cache.project(&graph, &points, None, 80, 24);
        cache.set_map_area(Some(ratatui::layout::Rect::new(10, 5, 80, 24)));
        (cache, graph, points)
    }

    #[test]
    fn camera_zoom_anchors_exact_braille_cell_world_point() {
        let (mut cache, graph, points) = camera_fixture();
        for (column, row, factor) in [(10, 5, 2.0), (89, 28, 0.25), (47, 12, 1.5)] {
            let screen = map_cell_center(column - 10, row - 5, 80, 24);
            let center = cache.camera.center.unwrap();
            let world = Point {
                x: center.x + (screen.x - 40.0) / cache.zoom_factor(),
                y: center.y + (screen.y - 24.0) / cache.zoom_factor(),
            };
            assert!(cache.zoom_at(factor, column, row));
            let projected = cache.project(&graph, &[world], None, 80, 24)[0];
            assert!((projected.x - screen.x).abs() < 1e-9);
            assert!((projected.y - screen.y).abs() < 1e-9);
        }
        assert_eq!(cache.world_positions(&graph), points);
        assert_eq!(cache.solves, 1);
        let center = cache.camera.center;
        assert!(!cache.zoom_at(2.0, 9, 12));
        assert!(!cache.zoom_at(2.0, 90, 12));
        assert!(!cache.zoom_at(2.0, 12, 29));
        assert_eq!(cache.camera.center, center);
    }

    #[test]
    fn zoomed_drag_pan_and_selection_reveal_use_screen_space_distances() {
        let (mut cache, graph, points) = camera_fixture();
        assert!(cache.zoom_by(2.0));
        let origin = cache.camera.center.unwrap();
        cache.start_drag(20, 15);
        cache.drag_to(25, 18);
        assert_eq!(
            cache.camera.center,
            Some(Point {
                x: origin.x - 2.5,
                y: origin.y + 3.0
            })
        );
        cache.cancel_drag();
        let before = cache.project(&graph, &points, None, 80, 24);
        cache.pan(12.0, -8.0);
        let after = cache.project(&graph, &points, None, 80, 24);
        for (a, b) in before.iter().zip(after) {
            assert!((a.x - b.x - 12.0).abs() < 1e-9);
            assert!((b.y - a.y - 8.0).abs() < 1e-9);
        }
        cache.camera.center = Some(Point::default());
        cache.zoom_by(0.25);
        let selected = Point { x: 60.0, y: 0.0 };
        cache.project(&graph, &[selected], Some(("far", selected)), 80, 24);
        assert_eq!(cache.camera.center, Some(Point::default()));
        cache.zoom_by(4.0);
        let selected = Point { x: 60.0, y: 70.0 };
        let projected = cache.project(&graph, &[selected], Some(("new", selected)), 80, 24)[0];
        assert_eq!(projected, Point { x: 75.0, y: 43.0 });
        assert_eq!(cache.world_positions(&graph), points);
        assert_eq!(cache.solves, 1);
    }

    #[test]
    fn fit_covers_all_loaded_points_with_screen_margin_without_zooming_in() {
        let (mut cache, graph, world) = camera_fixture();
        let points = [
            Point {
                x: -1.0e6,
                y: -500_000.0,
            },
            Point {
                x: 1.0e6,
                y: 500_000.0,
            },
            Point { x: 0.0, y: 10.0 },
        ];
        assert!(cache.fit_points(&points));
        assert!((MIN_ZOOM..=1.0).contains(&cache.zoom_factor()));
        for point in cache.project(&graph, &points, None, 80, 24) {
            assert!((5.0 - 1e-8..=75.0 + 1e-8).contains(&point.x));
            assert!((5.0 - 1e-8..=43.0 + 1e-8).contains(&point.y));
        }
        assert_eq!(cache.world_positions(&graph), world);
        assert_eq!(cache.solves, 1);
        let singleton = Point {
            x: 10_000.0,
            y: -4000.0,
        };
        assert!(cache.fit_points(&[singleton]));
        assert_eq!(cache.zoom_factor(), 1.0);
        assert_eq!(cache.camera.center, Some(singleton));
        assert_eq!(
            cache.project(&graph, &[singleton], None, 80, 24),
            [Point { x: 40.0, y: 24.0 }]
        );
        assert!(!cache.fit_points(&[singleton]));
    }

    #[test]
    fn invalid_or_unready_camera_operations_are_whole_operation_noops() {
        let mut empty = LayoutCache::default();
        assert_eq!(empty.zoom_factor(), 1.0);
        assert!(!empty.zoom_by(2.0));
        assert!(!empty.zoom_at(2.0, 0, 0));
        assert!(!empty.fit_points(&[Point::default()]));
        let (mut cache, _, _) = camera_fixture();
        let center = cache.camera.center;
        for factor in [0.0, -1.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(!cache.zoom_by(factor));
            assert!(!cache.zoom_at(factor, 20, 10));
        }
        assert!(!cache.fit_points(&[]));
        assert!(!cache.fit_points(&[
            Point::default(),
            Point {
                x: f64::NAN,
                y: 0.0
            }
        ]));
        assert!(!cache.fit_points(&[
            Point {
                x: -f64::MAX,
                y: 0.0
            },
            Point {
                x: f64::MAX,
                y: 0.0
            }
        ]));
        assert_eq!(cache.camera.center, center);
        assert_eq!(cache.zoom_factor(), 1.0);
        assert!(cache.zoom_by(f64::MAX));
        assert_eq!(cache.zoom_factor(), MAX_ZOOM);
        assert!(!cache.zoom_by(2.0));
        assert!(cache.zoom_by(f64::MIN_POSITIVE));
        assert_eq!(cache.zoom_factor(), MIN_ZOOM);
        assert!(!cache.zoom_by(0.5));
    }

    #[test]
    fn zoom_changes_cancel_drag_invalidate_hits_and_clone_exactly_for_back() {
        let (mut cache, graph, world) = camera_fixture();
        let marker = ratatui::layout::Rect::new(20, 10, 1, 1);
        cache.camera.hits.push((marker, "a".into()));
        cache.camera.markers.push((marker, "a".into()));
        cache.camera.index_ids.push("a".into());
        cache.start_drag(20, 10);
        assert!(cache.zoom_by(1.75));
        assert!(cache.camera.drag.is_none());
        assert!(
            cache.camera.hits.is_empty()
                && cache.camera.markers.is_empty()
                && cache.camera.index_ids.is_empty()
        );
        cache.pan(8.0, -3.0);
        let expected = cache.project(&graph, &world, None, 80, 24);
        let saved = cache.clone();
        let center = cache.camera.center;
        assert!(cache.reset_zoom());
        assert_eq!(cache.zoom_factor(), 1.0);
        assert_eq!(cache.camera.center, center);
        cache = saved;
        assert_eq!(cache.zoom_factor(), 1.75);
        assert_eq!(cache.project(&graph, &world, None, 80, 24), expected);
        assert_eq!(cache.world_positions(&graph), world);
        assert_eq!(cache.solves, 1);
        cache.recompose();
        assert_eq!(cache.zoom_factor(), 1.0);
        assert_eq!(cache.camera.center, None);
        cache.zoom_by(2.0);
        cache.reset();
        assert_eq!(cache.zoom_factor(), 1.0);
    }
}
