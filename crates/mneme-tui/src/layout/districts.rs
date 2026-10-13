//! A sparse quotient of the loaded graph: districts have area, not importance.
//!
//! Distinct linked districts choose the hub. Member count reserves footprint;
//! it never votes for the center. A rooted spanning tree supplies stable nearby
//! seeds, then a bounded sparse pass shortens non-tree ties and eases collisions.

use super::{LayoutKey, Point, fraction, spacing};
use std::collections::{BTreeMap, VecDeque};

const STEPS: usize = 64;
const NEARBY: usize = 16;
const WORK_BUDGET: usize = 800_000;
const GOLDEN_ANGLE: f64 = 2.399_963_229_728_653;
const PACKING_CANDIDATES: usize = 512;
const PACKING_COMPARISONS: usize = 400_000;

struct Quotient {
    order: Vec<usize>,
    links: BTreeMap<(usize, usize), f64>,
    adjacent: Vec<Vec<usize>>,
    roots: Vec<usize>,
    root: usize,
    unique_hub: bool,
}

fn quotient(key: &LayoutKey, units: &[(u64, Vec<usize>)]) -> Quotient {
    // All subsequent accumulation is canonical, including hash collisions.
    let identity: Vec<_> = units
        .iter()
        .map(|(_, members)| members.iter().map(|&i| &key.ids[i]).min())
        .collect();
    let mut order: Vec<_> = (0..units.len()).collect();
    order.sort_by(|&a, &b| {
        units[a]
            .0
            .cmp(&units[b].0)
            .then(identity[a].cmp(&identity[b]))
    });
    let mut owner = vec![usize::MAX; key.ids.len()];
    for (district, &unit) in order.iter().enumerate() {
        for &member in &units[unit].1 {
            owner[member] = district;
        }
    }
    // The strongest observed tie summarizes each pair. Summing member edges
    // would quietly turn a large district into a gravitational heavyweight.
    let mut links = BTreeMap::<(usize, usize), f64>::new();
    for edge in &key.springs {
        let (a, b) = (owner[edge.a], owner[edge.b]);
        let weight = f64::from_bits(edge.weight);
        if a == b || a == usize::MAX || b == usize::MAX || !weight.is_finite() || weight <= 0.0 {
            continue;
        }
        links
            .entry((a.min(b), a.max(b)))
            .and_modify(|old| *old = old.max(weight.min(1.0)))
            .or_insert(weight.min(1.0));
    }
    let mut adjacent = vec![Vec::new(); units.len()];
    let mut strength = vec![0.0; units.len()];
    for (&(a, b), &weight) in &links {
        adjacent[a].push(b);
        adjacent[b].push(a);
        strength[a] += weight;
        strength[b] += weight;
    }
    let mut roots: Vec<_> = (0..units.len()).collect();
    roots.sort_by(|&a, &b| {
        adjacent[b]
            .len()
            .cmp(&adjacent[a].len())
            .then_with(|| strength[b].total_cmp(&strength[a]))
            .then(a.cmp(&b))
    });
    let unique_hub = adjacent[roots[0]].len() > adjacent[roots[1]].len()
        || strength[roots[0]] > strength[roots[1]];
    let root = key
        .focus
        .map(|focus| owner[focus])
        .filter(|&unit| unit != usize::MAX)
        .unwrap_or(roots[0]);
    roots.retain(|&i| i != root);
    roots.insert(0, root);

    Quotient {
        order,
        links,
        adjacent,
        roots,
        root,
        unique_hub,
    }
}

pub(super) fn centers(key: &LayoutKey, units: &[(u64, Vec<usize>)]) -> Vec<Point> {
    let center = Point {
        x: f64::from(key.width) * 0.5,
        y: f64::from(key.height),
    };
    if units.len() <= 1 || key.width == 0 || key.height == 0 {
        return vec![center; units.len()];
    }
    let Quotient {
        order,
        links,
        adjacent,
        roots,
        root,
        unique_hub,
    } = quotient(key, units);

    let gap = spacing(key.width, key.height, key.ids.len());
    let radii: Vec<_> = order
        .iter()
        .map(|&unit| gap * (units[unit].1.len().max(1) as f64).sqrt() * 0.27)
        .collect();
    if units.len() == 2 {
        // With only two peers there is no meaningful angular topology. Spend
        // the long axis instead of letting a hash squeeze them into the short
        // one; a focused pair also needs room on the focus's empty side.
        let horizontal = center.x >= center.y;
        let span = if horizontal {
            center.x * 2.0
        } else {
            center.y * 2.0
        };
        let weight = links.get(&(0, 1)).copied().unwrap_or(0.0);
        let distance = (radii[0] + radii[1] + gap * (1.2 - 0.55 * weight))
            .min(span * if key.focus.is_some() { 0.24 } else { 0.44 });
        let mut result = vec![center; 2];
        for i in 0..2 {
            let offset = if key.focus.is_some() {
                if i == root { 0.0 } else { distance }
            } else if i == 0 {
                -distance * 0.5
            } else {
                distance * 0.5
            };
            if horizontal {
                result[order[i]].x += offset;
            } else {
                result[order[i]].y += offset;
            }
        }
        return result;
    }
    let mut packing_budget = PACKING_COMPARISONS;
    if links.is_empty() && key.focus.is_none() {
        // With no quotient ties there is no central component to orbit. Pack
        // full footprints into a compact, irregular field; empty viewport space
        // is not a reason to put every district on a shelf at its outer edge.
        return organic_centers(center, &order, &radii, gap, units, &mut packing_budget, key);
    }
    let mut points = vec![Point::default(); units.len()];
    let mut parent = vec![usize::MAX; units.len()];
    let mut children = vec![Vec::new(); units.len()];
    let mut mass: Vec<_> = order
        .iter()
        .map(|&unit| units[unit].1.len().max(1) as f64)
        .collect();
    let mut angles = vec![(0.0, std::f64::consts::TAU); units.len()];
    let mut radial = vec![0.0; units.len()];
    let mut primary_extent = 0.0;
    let mut outside_area: f64 = 0.0;
    let mut component_number = 0;
    let outside_rotation = fraction(units[order[root]].0) * std::f64::consts::TAU;
    for &component_root in &roots {
        if parent[component_root] != usize::MAX {
            continue;
        }
        parent[component_root] = component_root;
        let mut traversal = Vec::new();
        let mut queue = VecDeque::from([component_root]);
        while let Some(a) = queue.pop_front() {
            traversal.push(a);
            for &b in &adjacent[a] {
                if parent[b] == usize::MAX {
                    parent[b] = a;
                    children[a].push(b);
                    queue.push_back(b);
                }
            }
        }
        for &a in traversal.iter().rev() {
            if a != component_root {
                mass[parent[a]] += mass[a];
            }
        }
        let rotation = fraction(units[order[component_root]].0) * std::f64::consts::TAU;
        for &a in &traversal {
            let (start, span) = angles[a];
            let child_mass: f64 = children[a].iter().map(|&b| mass[b]).sum();
            let mut next = start;
            for &b in &children[a] {
                let share = span * mass[b] / child_mass;
                angles[b] = (next, share);
                radial[b] = radial[a] + radii[a] + radii[b] + gap * 0.8;
                let angle = next + share * 0.5 + rotation;
                points[b] = Point {
                    x: angle.cos() * radial[b],
                    y: angle.sin() * radial[b],
                };
                next += share;
            }
        }
        let extent = traversal
            .iter()
            .map(|&i| points[i].x.hypot(points[i].y) + radii[i])
            .fold(0.0_f64, f64::max);
        if component_number == 0 {
            primary_extent = extent;
        } else {
            // Disconnected components start beyond the main component's rim;
            // the growing golden-angle field avoids an O(component_count²)
            // packing problem, without giving every isolate an enormous ring.
            let distance = primary_extent + extent + gap * 1.2 + outside_area.sqrt();
            let angle = component_number as f64 * GOLDEN_ANGLE + outside_rotation;
            for &i in &traversal {
                points[i].x += angle.cos() * distance;
                points[i].y += angle.sin() * distance;
            }
            outside_area += (extent + gap * 0.5).powi(2);
        }
        component_number += 1;
    }

    let seeds = points.clone();
    let mut forces = vec![Point::default(); units.len()];
    let mut sweep: Vec<_> = (0..units.len()).collect();
    let steps = solver_steps(units.len(), links.len());
    for _ in 0..steps {
        for ((force, point), seed) in forces.iter_mut().zip(&points).zip(&seeds) {
            *force = Point {
                x: (seed.x - point.x) * 0.035,
                y: (seed.y - point.y) * 0.035,
            };
        }
        for (&(a, b), &weight) in &links {
            let (dx, dy, distance) = separation(points[a], points[b], a, b);
            let rest = radii[a] + radii[b] + gap * (1.0 - 0.35 * weight);
            let pull = ((distance - rest) * 0.12).clamp(-gap * 0.4, gap * 0.4);
            let pull = pull / adjacent[a].len().max(adjacent[b].len()).max(1) as f64;
            add_pair(
                &mut forces,
                a,
                b,
                dx / distance * pull,
                dy / distance * pull,
            );
        }
        // A bounded x-neighborhood catches nearby collisions without an all-
        // pairs matrix or allowing one dense bucket to make the pass quadratic.
        sweep.sort_by(|&a, &b| points[a].x.total_cmp(&points[b].x).then(a.cmp(&b)));
        for (at, &a) in sweep.iter().enumerate() {
            for &b in sweep.iter().skip(at + 1).take(NEARBY) {
                let (dx, dy, distance) = separation(points[a], points[b], a, b);
                let overlap = radii[a] + radii[b] + gap * 0.35 - distance;
                if overlap > 0.0 {
                    let push = -overlap.min(gap) * 0.25;
                    add_pair(
                        &mut forces,
                        a,
                        b,
                        dx / distance * push,
                        dy / distance * push,
                    );
                }
            }
        }
        for i in 0..points.len() {
            if i == root {
                continue;
            }
            let scale = (gap * 0.18 / forces[i].x.hypot(forces[i].y).max(0.001)).min(1.0);
            points[i].x += forces[i].x * scale;
            points[i].y += forces[i].y * scale;
        }
    }
    if key.focus.is_none() && !unique_hub {
        // No centrality winner: balance districts equally, never weight this
        // translation by their member counts. In particular, two peer pockets
        // should flank the center rather than arbitrarily crown one of them.
        let average = super::mean(points.iter().copied());
        for point in &mut points {
            point.x -= average.x;
            point.y -= average.y;
        }
    }
    // Keep the topology hub/focus at the real center. Fitting by the node-weighted
    // centroid would let a huge peripheral district seize it again afterwards.
    let extent_x = points
        .iter()
        .zip(&radii)
        .map(|(point, radius)| point.x.abs() + radius)
        .fold(0.001_f64, f64::max);
    let extent_y = points
        .iter()
        .zip(&radii)
        .map(|(point, radius)| point.y.abs() + radius)
        .fold(0.001_f64, f64::max);
    let scale = (center.x * 0.88 / extent_x)
        .min(center.y * 0.88 / extent_y)
        .min(1.0);
    if key.focus.is_none()
        && !unique_hub
        && links.keys().any(|&(a, b)| {
            let distance = (points[a].x - points[b].x).hypot(points[a].y - points[b].y);
            distance * scale < radii[a] + radii[b] + gap * 0.35
        })
    {
        // A deep quotient tree can need much more span than its district
        // footprints. Shrinking only center distances would overlap their
        // still-full-size interiors and make final node packing scatter them.
        // Prefer a footprint-aware field to a topology-shaped hairball. Explicit
        // focus and an actual unique hub keep their center invariant instead.
        return organic_centers(center, &order, &radii, gap, units, &mut packing_budget, key);
    }
    let mut result = vec![center; units.len()];
    for (i, &unit) in order.iter().enumerate() {
        result[unit] = Point {
            x: center.x + points[i].x * scale,
            y: center.y + points[i].y * scale,
        };
    }
    result
}

/// Final placement moves complete settled interiors, never individual members.
/// Measure after local node packing: seed-radius guesses cannot bound that pass.
pub(super) fn arrange_settled(key: &LayoutKey, units: &[(u64, Vec<usize>)], points: &mut [Point]) {
    if units.len() <= 2 || key.width == 0 || key.height == 0 {
        return;
    }
    let topology = quotient(key, units);
    let center = Point {
        x: f64::from(key.width) * 0.5,
        y: f64::from(key.height),
    };
    let mut origins = Vec::with_capacity(units.len());
    let mut footprints = Vec::with_capacity(units.len());
    for &unit in &topology.order {
        let mut members = units[unit].1.clone();
        members.sort_unstable();
        let origin = key
            .focus
            .filter(|focus| members.contains(focus))
            .map_or_else(
                || super::mean(members.iter().map(|&i| points[i])),
                |focus| points[focus],
            );
        footprints.push(Point {
            x: members
                .iter()
                .map(|&i| (points[i].x - origin.x).abs())
                .fold(0.0_f64, f64::max),
            y: members
                .iter()
                .map(|&i| (points[i].y - origin.y).abs())
                .fold(0.0_f64, f64::max),
        });
        origins.push(origin);
    }
    // A changed link weight must not re-pack unrelated, already-separated
    // islands. Only repair actual final crowding, not harmless seed movement.
    let mut remaining = PACKING_COMPARISONS;
    if separated(&origins, &footprints, 0.0, &mut remaining) {
        return;
    }
    let preferred: Vec<_> = origins
        .iter()
        .map(|p| Point {
            x: p.x - center.x,
            y: p.y - center.y,
        })
        .collect();
    let pinned = (key.focus.is_some() || topology.unique_hub).then_some(topology.root);
    let mut priority = vec![usize::MAX; units.len()];
    if let Some(root) = pinned {
        priority[root] = 0;
        let mut queue = VecDeque::from([root]);
        while let Some(a) = queue.pop_front() {
            for &b in &topology.adjacent[a] {
                if priority[b] == usize::MAX {
                    priority[b] = priority[a] + 1;
                    queue.push_back(b);
                }
            }
        }
    }
    let radii = vec![0.0; units.len()];
    let packed = pack_field(
        PackingField {
            center,
            order: &topology.order,
            units,
            footprints: &footprints,
            radii: &radii,
            clearance: (spacing(key.width, key.height, key.ids.len()) * 0.35)
                .max(super::WORLD_CLEARANCE),
            preferred: Some(&preferred),
            pinned,
            priority: Some(&priority),
        },
        &mut remaining,
    );
    for (i, &unit) in topology.order.iter().enumerate() {
        let delta = Point {
            x: packed[unit].x - origins[i].x,
            y: packed[unit].y - origins[i].y,
        };
        for &member in &units[unit].1 {
            points[member].x += delta.x;
            points[member].y += delta.y;
        }
    }
}

/// Bounded broad phase; exhausting the proof budget means re-pack, not assume
/// separation. A large district cannot turn one crowded bucket quadratic.
fn separated(origins: &[Point], half: &[Point], clearance: f64, remaining: &mut usize) -> bool {
    let cell =
        (half.iter().map(|p| p.x.max(p.y)).fold(0.0_f64, f64::max) * 2.0 + clearance).max(0.001);
    let mut buckets = BTreeMap::<(i64, i64), Vec<usize>>::new();
    for (i, point) in origins.iter().enumerate() {
        let bucket = (
            (point.x / cell).floor() as i64,
            (point.y / cell).floor() as i64,
        );
        for x in bucket.0 - 1..=bucket.0 + 1 {
            for y in bucket.1 - 1..=bucket.1 + 1 {
                for &j in buckets.get(&(x, y)).into_iter().flatten() {
                    let Some(next) = remaining.checked_sub(1) else {
                        return false;
                    };
                    *remaining = next;
                    if (point.x - origins[j].x).abs() <= half[i].x + half[j].x + clearance
                        && (point.y - origins[j].y).abs() <= half[i].y + half[j].y + clearance
                    {
                        return false;
                    }
                }
            }
        }
        buckets.entry(bucket).or_default().push(i);
    }
    true
}

/// Search a finite, identity-rotated golden-angle field using full-size disks.
/// The candidate field grows with occupied area, not unused viewport area.
/// Neither this pass nor its fallback scales center distances after packing.
fn organic_centers(
    center: Point,
    order: &[usize],
    radii: &[f64],
    gap: f64,
    units: &[(u64, Vec<usize>)],
    remaining: &mut usize,
    key: &LayoutKey,
) -> Vec<Point> {
    let clearance = gap * 0.35;
    let footprints: Vec<_> = order
        .iter()
        .enumerate()
        .map(|(i, &unit)| {
            let interior = super::district_interior(key, &units[unit].1, radii[i]);
            // Multi-member interiors continue settling after seeding. Preserve
            // the old shelf pass's 0.9*gap inter-group breathing room, without
            // charging that whole margin to standalone notes.
            let slack = if units[unit].1.len() > 1 {
                gap * 0.275
            } else {
                0.0
            };
            Point {
                x: slack
                    + interior
                        .iter()
                        .map(|(_, p)| p.x.abs())
                        .fold(0.0_f64, f64::max),
                y: slack
                    + interior
                        .iter()
                        .map(|(_, p)| p.y.abs())
                        .fold(0.0_f64, f64::max),
            }
        })
        .collect();
    pack_field(
        PackingField {
            center,
            order,
            units,
            footprints: &footprints,
            radii,
            clearance,
            preferred: None,
            pinned: None,
            priority: None,
        },
        remaining,
    )
}

struct PackingField<'a> {
    center: Point,
    order: &'a [usize],
    units: &'a [(u64, Vec<usize>)],
    footprints: &'a [Point],
    radii: &'a [f64],
    clearance: f64,
    preferred: Option<&'a [Point]>,
    pinned: Option<usize>,
    priority: Option<&'a [usize]>,
}

fn pack_field(field: PackingField<'_>, remaining: &mut usize) -> Vec<Point> {
    let PackingField {
        center,
        order,
        units,
        footprints,
        radii,
        clearance,
        preferred,
        pinned,
        priority,
    } = field;
    let mut by_size: Vec<_> = (0..order.len()).collect();
    let envelopes: Vec<_> = footprints
        .iter()
        .zip(radii)
        .map(|(p, &r)| p.x.hypot(p.y).max(r))
        .collect();
    by_size.sort_by(|&a, &b| {
        let area = |i: usize| footprints[i].x.max(radii[i]) * footprints[i].y.max(radii[i]);
        (Some(b) == pinned)
            .cmp(&(Some(a) == pinned))
            .then_with(|| {
                priority
                    .map_or(0, |rank| rank[a])
                    .cmp(&priority.map_or(0, |rank| rank[b]))
            })
            .then_with(|| area(b).total_cmp(&area(a)))
            .then(a.cmp(&b))
    });
    let largest = envelopes.iter().copied().fold(0.0_f64, f64::max);
    // Every possible collision lies in this cell or one of its eight neighbors.
    // A comparison budget also bounds a pathological large/small bucket mix.
    let cell = (largest * 2.0 + clearance).max(0.001);
    let mut buckets = BTreeMap::<(i64, i64), Vec<usize>>::new();
    let mut points = vec![Point::default(); order.len()];
    let aspect = (center.x / center.y).sqrt();
    let mut area = 0.0_f64;
    let mut bounds = (0.0_f64, 0.0_f64, 0.0_f64, 0.0_f64);
    for i in by_size.iter().copied() {
        area += (envelopes[i] + clearance * 0.5).powi(2);
        let extent = (area / 0.50).sqrt();
        let rotation = fraction(units[order[i]].0) * std::f64::consts::TAU;
        let mut best: Option<((f64, f64), Point)> = None;
        for candidate in 0..if Some(i) == pinned {
            1
        } else {
            PACKING_CANDIDATES
        } {
            let angle = rotation + candidate as f64 * GOLDEN_ANGLE;
            let available_x = center.x - footprints[i].x.max(radii[i]) - clearance * 0.4;
            let available_y = center.y - footprints[i].y.max(radii[i]) - clearance * 0.4;
            if available_x < 0.0 || available_y < 0.0 {
                continue;
            }
            let (point, radius) = if let Some(seeds) = preferred {
                // Final repair starts at the actual settled origin, not the
                // global center. Small early steps resolve local overlap without
                // rebuilding the already meaningful quotient composition.
                let seed = if Some(i) == pinned {
                    Point::default()
                } else {
                    seeds[i]
                };
                let radius = extent * candidate as f64 / (PACKING_CANDIDATES - 1) as f64;
                (
                    Point {
                        x: seed.x + radius * angle.cos(),
                        y: seed.y + radius * angle.sin(),
                    },
                    radius,
                )
            } else {
                let dx = angle.cos() * aspect;
                let dy = angle.sin() / aspect;
                let limit = extent
                    .min(available_x / dx.abs().max(0.0001))
                    .min(available_y / dy.abs().max(0.0001));
                let radius = limit * (candidate as f64 / (PACKING_CANDIDATES - 1) as f64).sqrt();
                (
                    Point {
                        x: radius * dx,
                        y: radius * dy,
                    },
                    radius,
                )
            };
            if point.x.abs() > available_x || point.y.abs() > available_y {
                continue;
            }
            let bucket = (
                (point.x / cell).floor() as i64,
                (point.y / cell).floor() as i64,
            );
            let mut clear = true;
            'neighbors: for x in bucket.0 - 1..=bucket.0 + 1 {
                for y in bucket.1 - 1..=bucket.1 + 1 {
                    for &j in buckets.get(&(x, y)).into_iter().flatten() {
                        let Some(next) = remaining.checked_sub(1) else {
                            return ring_fallback(center, order, &envelopes, clearance, pinned);
                        };
                        *remaining = next;
                        let dx = (point.x - points[j].x).abs();
                        let dy = (point.y - points[j].y).abs();
                        if dx.hypot(dy) < radii[i] + radii[j] + clearance
                            || (dx < footprints[i].x + footprints[j].x + clearance
                                && dy < footprints[i].y + footprints[j].y + clearance)
                        {
                            clear = false;
                            break 'neighbors;
                        }
                    }
                }
            }
            if !clear {
                continue;
            }
            // Prefer a compact envelope with the viewport's aspect, rather
            // than pushing each new item out to consume the entire canvas.
            let half_x = footprints[i].x.max(radii[i]);
            let half_y = footprints[i].y.max(radii[i]);
            let width = bounds.1.max(point.x + half_x) - bounds.0.min(point.x - half_x);
            let height = bounds.3.max(point.y + half_y) - bounds.2.min(point.y - half_y);
            let compactness = (width / aspect).powi(2) + (height * aspect).powi(2);
            let score = if preferred.is_some() {
                (radius.powi(2), compactness)
            } else {
                (compactness + radius.powi(2) * 0.08, 0.0)
            };
            if best.is_none_or(|(old, _)| {
                score
                    .0
                    .total_cmp(&old.0)
                    .then_with(|| score.1.total_cmp(&old.1))
                    .is_lt()
            }) {
                best = Some((score, point));
            }
        }
        let Some((_, point)) = best else {
            return ring_fallback(center, order, &envelopes, clearance, pinned);
        };
        points[i] = point;
        bounds.0 = bounds.0.min(point.x - footprints[i].x.max(radii[i]));
        bounds.1 = bounds.1.max(point.x + footprints[i].x.max(radii[i]));
        bounds.2 = bounds.2.min(point.y - footprints[i].y.max(radii[i]));
        bounds.3 = bounds.3.max(point.y + footprints[i].y.max(radii[i]));
        buckets
            .entry((
                (point.x / cell).floor() as i64,
                (point.y / cell).floor() as i64,
            ))
            .or_default()
            .push(i);
    }
    let origin = if pinned.is_some() || preferred.is_some() {
        Point::default()
    } else {
        Point {
            x: (bounds.0 + bounds.1) * 0.5,
            y: (bounds.2 + bounds.3) * 0.5,
        }
    };
    let mut result = vec![center; order.len()];
    for (i, &unit) in order.iter().enumerate() {
        result[unit] = Point {
            x: center.x + points[i].x - origin.x,
            y: center.y + points[i].y - origin.y,
        };
    }
    result
}

/// Bounded non-shelf fallback: concentric stadium contours (circles in a square
/// viewport). Each contour is a fixed segment's distance level-set, so radial
/// clearance separates whole rings. Cap chord length and straight-line spacing
/// separate neighbors on a ring. An impossible viewport may overflow rather than
/// silently shrinking only center distances; the caller owns whole-field fit.
fn ring_fallback(
    center: Point,
    order: &[usize],
    radii: &[f64],
    clearance: f64,
    pinned: Option<usize>,
) -> Vec<Point> {
    let mut by_size: Vec<_> = (0..order.len()).collect();
    by_size.sort_by(|&a, &b| {
        (Some(b) == pinned)
            .cmp(&(Some(a) == pinned))
            .then_with(|| radii[b].total_cmp(&radii[a]))
            .then(a.cmp(&b))
    });
    let horizontal = center.x >= center.y;
    let aspect = center.x.max(center.y) / center.x.min(center.y);
    let area: f64 = radii
        .iter()
        .map(|radius| (radius * 2.0 + clearance).powi(2))
        .sum();
    let expected_radius = (area * 1.08 / (4.0 * (aspect - 1.0) + std::f64::consts::PI)).sqrt();
    let mut half_segment = expected_radius * (aspect - 1.0);
    // Tiny straight segments buy little area and can put their endpoints closer
    // than one footprint. Use a true circle instead in that near-square case.
    if half_segment * 2.0 < radii.iter().copied().fold(0.0_f64, f64::max) * 2.0 + clearance {
        half_segment = 0.0;
    }
    let mut result = vec![center; order.len()];
    let mut placed = 1;
    let mut previous = radii[by_size[0]];
    let mut radius = 0.0;
    let mut ring = 0;
    while placed < order.len() {
        let largest = radii[by_size[placed]];
        let pitch = largest * 2.0 + clearance;
        radius += previous + largest + clearance;
        let sites = ring_sites(radius, pitch, half_segment, ring, order.len() - placed);
        for point in sites.into_iter().take(order.len() - placed) {
            result[order[by_size[placed]]] = if horizontal {
                Point {
                    x: center.x + point.x,
                    y: center.y + point.y,
                }
            } else {
                Point {
                    x: center.x + point.y,
                    y: center.y + point.x,
                }
            };
            placed += 1;
        }
        previous = largest;
        ring += 1;
    }
    result
}

/// At most 4*remaining+2 sites are generated even for an enormous contour and
/// tiny remaining footprints. Reducing subdivisions only increases clearance.
fn ring_sites(
    radius: f64,
    pitch: f64,
    half_segment: f64,
    ring: usize,
    remaining: usize,
) -> Vec<Point> {
    let angle = 2.0 * (pitch / (radius * 2.0)).min(1.0).asin();
    let mut sites = Vec::new();
    if half_segment == 0.0 {
        let count = ((std::f64::consts::TAU / angle).floor().max(1.0) as usize).min(remaining);
        for i in 0..count {
            let theta =
                i as f64 * std::f64::consts::TAU / count as f64 + ring as f64 * GOLDEN_ANGLE;
            sites.push(Point {
                x: theta.cos() * radius,
                y: theta.sin() * radius,
            });
        }
    } else {
        let straight = ((half_segment * 2.0 / pitch).floor().max(1.0) as usize).min(remaining);
        let cap = ((std::f64::consts::PI / angle).floor().max(1.0) as usize).min(remaining);
        for i in 0..=straight {
            let x = -half_segment + half_segment * 2.0 * i as f64 / straight as f64;
            sites.push(Point { x, y: radius });
            sites.push(Point { x: -x, y: -radius });
        }
        for i in 1..cap {
            let theta = i as f64 * std::f64::consts::PI / cap as f64;
            sites.push(Point {
                x: half_segment + theta.sin() * radius,
                y: theta.cos() * radius,
            });
            sites.push(Point {
                x: -half_segment - theta.sin() * radius,
                y: -theta.cos() * radius,
            });
        }
        let rotate = ((fraction(ring as u64) * sites.len() as f64) as usize).min(sites.len() - 1);
        sites.rotate_left(rotate);
    }
    sites
}

fn solver_steps(units: usize, links: usize) -> usize {
    let per_step = units.saturating_mul(NEARBY + 1).saturating_add(links);
    STEPS.min(WORK_BUDGET / per_step.max(1))
}

fn separation(a: Point, b: Point, i: usize, j: usize) -> (f64, f64, f64) {
    let (dx, dy) = (b.x - a.x, b.y - a.y);
    let distance = dx.hypot(dy);
    if distance > 0.0001 {
        (dx, dy, distance)
    } else {
        let angle = fraction((i as u64).rotate_left(32) ^ j as u64) * std::f64::consts::TAU;
        (angle.cos() * 0.0001, angle.sin() * 0.0001, 0.0001)
    }
}

fn add_pair(forces: &mut [Point], a: usize, b: usize, x: f64, y: f64) {
    forces[a].x += x;
    forces[a].y += y;
    forces[b].x -= x;
    forces[b].y -= y;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clusters::DisplayLink;

    fn fixture(sizes: &[usize], edges: &[(usize, usize)]) -> (LayoutKey, Vec<(u64, Vec<usize>)>) {
        let mut ids = Vec::new();
        let mut units = Vec::new();
        for (district, &size) in sizes.iter().enumerate() {
            let mut members = Vec::new();
            for member in 0..size {
                members.push(ids.len());
                ids.push(format!("{district:06}-{member:06}"));
            }
            units.push((district as u64, members));
        }
        let springs = edges
            .iter()
            .map(|&(a, b)| DisplayLink {
                a: units[a].1[0],
                b: units[b].1[0],
                weight: 0.8_f64.to_bits(),
            })
            .collect();
        (
            LayoutKey {
                ids,
                springs,
                focus: None,
                width: 600,
                height: 200,
            },
            units,
        )
    }

    fn distance(a: Point, b: Point) -> f64 {
        (a.x - b.x).hypot(a.y - b.y)
    }

    #[test]
    fn small_topological_hub_not_large_periphery_owns_center() {
        let (key, units) = fixture(&[100, 2, 2, 2], &[(1, 0), (1, 2), (1, 3)]);
        let points = centers(&key, &units);
        assert_eq!(points[1], Point { x: 300.0, y: 200.0 });
        assert!(distance(points[0], points[1]) > 1.0);
    }

    #[test]
    fn linked_districts_are_closer_than_disconnected_districts() {
        let (key, units) = fixture(&[4, 4, 4, 4, 4], &[(0, 1), (0, 2), (0, 3)]);
        let points = centers(&key, &units);
        for neighbor in 1..4 {
            assert!(distance(points[0], points[neighbor]) < distance(points[0], points[4]));
        }
    }

    #[test]
    fn focus_overrides_hub_and_stays_exactly_at_center() {
        let (mut key, units) = fixture(&[12, 3, 3, 3, 2], &[(1, 0), (1, 2), (1, 3)]);
        key.focus = Some(units[4].1[0]);
        assert_eq!(centers(&key, &units)[4], Point { x: 300.0, y: 200.0 });
    }

    #[test]
    fn canonical_ids_ignore_unit_member_and_edge_order() {
        let (mut key, units) = fixture(&[8, 3, 3, 5, 2], &[(0, 1), (1, 2), (0, 2), (2, 3)]);
        let expected = centers(&key, &units);
        key.springs.reverse();
        let mut reordered = units.clone();
        reordered.reverse();
        for (_, members) in &mut reordered {
            members.reverse();
        }
        let mut actual = centers(&key, &reordered);
        actual.reverse();
        assert_eq!(actual, expected);
    }

    #[test]
    fn two_peer_districts_flank_center_without_a_fake_hub() {
        for edges in [vec![], vec![(0, 1)]] {
            let (key, units) = fixture(&[4, 4], &edges);
            let points = centers(&key, &units);
            let middle = super::super::mean(points.iter().copied());
            assert!((middle.x - 300.0).abs() < 0.001);
            assert!((middle.y - 200.0).abs() < 0.001);
            assert!(distance(points[0], points[1]) > 1.0);
        }
    }

    #[test]
    fn larger_footprint_needs_more_link_clearance() {
        let (mut key, units) = fixture(&[4, 4, 4], &[(0, 1), (0, 2)]);
        key.width = 2000;
        key.height = 1000;
        let small = centers(&key, &units);
        let (mut key, units) = fixture(&[4, 100, 4], &[(0, 1), (0, 2)]);
        key.width = 2000;
        key.height = 1000;
        let large = centers(&key, &units);
        // Radius uses sqrt(member count); compare to the other equal-sized
        // neighbor in the same field, not across differently scaled fields.
        assert!(distance(large[0], large[1]) > distance(large[0], large[2]));
        assert!((distance(small[0], small[1]) - distance(small[0], small[2])).abs() < 0.001);
    }

    #[test]
    fn large_fields_are_finite_and_relaxation_has_an_aggregate_budget() {
        let sizes = vec![1; 4096];
        let edges: Vec<_> = (1..sizes.len()).map(|i| (i - 1, i)).collect();
        let (key, units) = fixture(&sizes, &edges);
        for point in centers(&key, &units) {
            assert!(point.x.is_finite() && point.y.is_finite());
            assert!((0.0..=600.0).contains(&point.x));
            assert!((0.0..=400.0).contains(&point.y));
        }
        for (nodes, links) in [
            (1, 0),
            (4096, 4095),
            (100_000, 1_000_000),
            (usize::MAX, usize::MAX),
        ] {
            let steps = solver_steps(nodes, links);
            assert!(steps <= STEPS);
            assert!(
                steps.saturating_mul(nodes.saturating_mul(NEARBY + 1).saturating_add(links))
                    <= WORK_BUDGET
            );
        }
    }

    fn assert_footprint_clearance(key: &LayoutKey, units: &[(u64, Vec<usize>)], points: &[Point]) {
        let gap = spacing(key.width, key.height, key.ids.len());
        let radii: Vec<_> = units
            .iter()
            .map(|(_, members)| gap * (members.len() as f64).sqrt() * 0.27)
            .collect();
        for a in 0..units.len() {
            assert!(points[a].x.is_finite() && points[a].y.is_finite());
            for b in a + 1..units.len() {
                assert!(
                    distance(points[a], points[b]) + 1e-8 >= radii[a] + radii[b] + gap * 0.35,
                    "districts {a}/{b}: distance={}, required={}",
                    distance(points[a], points[b]),
                    radii[a] + radii[b] + gap * 0.35
                );
            }
        }
    }

    #[test]
    fn disconnected_packing_is_canonical_even_with_colliding_hashes() {
        let (key, mut units) = fixture(&[20, 8, 8, 2, 1, 1], &[]);
        units[2].0 = units[1].0;
        let expected = centers(&key, &units);
        units.reverse();
        for (_, members) in &mut units {
            members.reverse();
        }
        let mut actual = centers(&key, &units);
        actual.reverse();
        assert_eq!(actual, expected);
        assert_footprint_clearance(&key, &units, &centers(&key, &units));
    }

    #[test]
    fn disconnected_small_field_is_organic_not_shelves_and_keeps_full_footprints() {
        let sizes = [5, 3, 3, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1];
        for (width, height) in [(120, 36), (70, 35), (60, 50)] {
            let (mut key, units) = fixture(&sizes, &[]);
            key.width = width;
            key.height = height;
            let points = centers(&key, &units);
            assert_footprint_clearance(&key, &units, &points);
            let gap = spacing(width, height, key.ids.len());
            for (i, point) in points.iter().enumerate() {
                let radius = gap * (sizes[i] as f64).sqrt() * 0.27;
                assert!(point.x - radius >= 0.0 && point.x + radius <= f64::from(width));
                assert!(point.y - radius >= 0.0 && point.y + radius <= f64::from(height) * 2.0);
            }
            // Shelf placement gives equal-sized singleton members identical y.
            for a in 3..points.len() {
                for b in a + 1..points.len() {
                    assert!(
                        (points[a].y - points[b].y).abs() > 1e-8,
                        "{width}x{height}: small field fell back to shelves"
                    );
                }
            }
        }
    }

    #[test]
    fn packing_budget_exhaustion_uses_finite_ring_fallback() {
        let (key, units) = fixture(&[8, 4, 3, 2, 1, 1], &[]);
        let gap = spacing(key.width, key.height, key.ids.len());
        let radii: Vec<_> = units
            .iter()
            .map(|(_, members)| gap * (members.len() as f64).sqrt() * 0.27)
            .collect();
        let order: Vec<_> = (0..units.len()).collect();
        let center = Point { x: 300.0, y: 200.0 };
        let mut remaining = 1;
        let points = organic_centers(center, &order, &radii, gap, &units, &mut remaining, &key);
        assert_eq!(remaining, 0);
        assert_footprint_clearance(&key, &units, &points);
        let mut no_work = 0;
        assert_eq!(
            points,
            organic_centers(center, &order, &radii, gap, &units, &mut no_work, &key)
        );
        assert_eq!(no_work, 0);
    }

    #[test]
    fn impossible_nominal_footprints_remain_finite_without_center_only_shrink() {
        let (mut key, units) = fixture(&[118, 1, 1], &[]);
        key.width = 150;
        key.height = 12;
        let points = centers(&key, &units);
        assert_eq!(points, centers(&key, &units));
        assert_footprint_clearance(&key, &units, &points);
        // The largest full-size disk cannot fit this height. Do not pretend it
        // can by reducing only inter-district distances; anchors owns final fit.
        let radius = spacing(150, 12, 120) * 118.0_f64.sqrt() * 0.27;
        assert!(radius > 12.0);
    }

    #[test]
    fn final_arrangement_preserves_interior_shape_and_focus_or_hub() {
        for focus_group in [None, Some(2)] {
            let (mut key, units) = fixture(&[3, 2, 2, 1], &[(0, 1), (0, 2), (0, 3)]);
            key.focus = focus_group.map(|g| units[g].1[0]);
            let mut points: Vec<_> = (0..key.ids.len())
                .map(|i| Point {
                    x: 298.0 + (i % 3) as f64 * 0.5,
                    y: 198.0 + (i % 2) as f64 * 0.5,
                })
                .collect();
            let original = points.clone();
            arrange_settled(&key, &units, &mut points);
            for (_, members) in &units {
                for &a in members {
                    for &b in members {
                        assert!(
                            ((points[a].x - points[b].x) - (original[a].x - original[b].x)).abs()
                                < 1e-9
                        );
                        assert!(
                            ((points[a].y - points[b].y) - (original[a].y - original[b].y)).abs()
                                < 1e-9
                        );
                    }
                }
            }
            let origin = key.focus.map_or_else(
                || super::super::mean(units[0].1.iter().map(|&i| points[i])),
                |i| points[i],
            );
            assert_eq!(origin, Point { x: 300.0, y: 200.0 });
            let mut origins = Vec::new();
            let mut half = Vec::new();
            for (_, members) in &units {
                let c = super::super::mean(members.iter().map(|&i| points[i]));
                origins.push(c);
                half.push(Point {
                    x: members
                        .iter()
                        .map(|&i| (points[i].x - c.x).abs())
                        .fold(0.0_f64, f64::max),
                    y: members
                        .iter()
                        .map(|&i| (points[i].y - c.y).abs())
                        .fold(0.0_f64, f64::max),
                });
            }
            let mut budget = PACKING_COMPARISONS;
            assert!(separated(&origins, &half, 0.0, &mut budget));
            let mut reordered = units.clone();
            reordered.reverse();
            for (_, members) in &mut reordered {
                members.reverse();
            }
            let mut again = original;
            arrange_settled(&key, &reordered, &mut again);
            assert_eq!(again, points);
        }
    }

    #[test]
    fn already_separated_interiors_are_not_recomposed() {
        let (key, units) = fixture(&[2, 2, 2], &[]);
        let mut points = vec![Point::default(); key.ids.len()];
        for (i, (_, members)) in units.iter().enumerate() {
            for (j, &member) in members.iter().enumerate() {
                points[member] = Point {
                    x: 100.0 + i as f64 * 150.0 + j as f64,
                    y: 200.0 + j as f64,
                };
            }
        }
        let expected = points.clone();
        arrange_settled(&key, &units, &mut points);
        assert_eq!(points, expected);
    }

    #[test]
    fn ring_fallback_small_pin_cannot_underestimate_large_siblings() {
        let radii = [0.0, 30.0, 30.0, 30.0];
        let order = [0, 1, 2, 3];
        let center = Point { x: 110.0, y: 100.0 };
        let points = ring_fallback(center, &order, &radii, 7.0, Some(0));
        assert_eq!(points[0], center);
        for a in 0..points.len() {
            for b in a + 1..points.len() {
                assert!(distance(points[a], points[b]) + 1e-8 >= radii[a] + radii[b] + 7.0);
            }
        }
    }

    #[test]
    fn ring_contour_work_depends_on_remaining_items_not_coordinate_magnitude() {
        for half_segment in [0.0, 1.0e12] {
            for remaining in [1, 2, 7, 31] {
                let sites = ring_sites(1.0e12, 7.0, half_segment, 0, remaining);
                assert!(sites.len() <= 4 * remaining + 2);
                assert!(!sites.is_empty());
                for a in 0..sites.len() {
                    assert!(sites[a].x.is_finite() && sites[a].y.is_finite());
                    for b in a + 1..sites.len() {
                        assert!(distance(sites[a], sites[b]) >= 7.0);
                    }
                }
            }
        }
    }

    #[test]
    fn final_repair_keeps_clear_preferred_origins_in_place() {
        let (key, units) = fixture(&[1, 1, 1, 1], &[]);
        let order = [0, 1, 2, 3];
        let preferred = [
            Point { x: -80.0, y: -20.0 },
            Point { x: 70.0, y: 30.0 },
            Point { x: -80.0, y: -20.0 },
            Point { x: 30.0, y: -40.0 },
        ];
        let half = [Point::default(); 4];
        let radii = [0.0; 4];
        let center = Point { x: 300.0, y: 200.0 };
        let mut remaining = PACKING_COMPARISONS;
        let points = pack_field(
            PackingField {
                center,
                order: &order,
                units: &units,
                footprints: &half,
                radii: &radii,
                clearance: 7.0,
                preferred: Some(&preferred),
                pinned: None,
                priority: None,
            },
            &mut remaining,
        );
        for i in [0, 1, 3] {
            assert_eq!(
                points[i],
                Point {
                    x: center.x + preferred[i].x,
                    y: center.y + preferred[i].y
                }
            );
        }
        assert!(distance(points[0], points[2]) >= 7.0);
        assert!(distance(points[0], points[2]) < 11.0);
        assert!(remaining < PACKING_COMPARISONS);
        assert_eq!(key.ids.len(), points.len());
    }
}
