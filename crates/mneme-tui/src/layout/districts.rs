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

pub(super) fn centers(key: &LayoutKey, units: &[(u64, Vec<usize>)]) -> Vec<Point> {
    let center = Point {
        x: f64::from(key.width) * 0.5,
        y: f64::from(key.height),
    };
    if units.len() <= 1 || key.width == 0 || key.height == 0 {
        return vec![center; units.len()];
    }
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
    if links.is_empty() && key.focus.is_none() {
        // With no quotient ties there is no central component to orbit. Pack
        // actual footprint-sized shelves across the rectangle instead of
        // fitting an expanding orbital field and crushing its center spacing.
        return rectangular_centers(center, &order, &radii, gap);
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
        // Prefer a readable rectangle to a topology-shaped hairball. Explicit
        // focus and an actual unique hub keep their center invariant instead.
        return rectangular_centers(center, &order, &radii, gap);
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

fn rectangular_centers(center: Point, order: &[usize], radii: &[f64], gap: f64) -> Vec<Point> {
    let mut by_size: Vec<_> = (0..order.len()).collect();
    by_size.sort_by(|&a, &b| radii[b].total_cmp(&radii[a]).then(a.cmp(&b)));
    let side: Vec<_> = radii
        .iter()
        .map(|radius| radius * 2.0 + gap * 0.9)
        .collect();
    let width = (center.x * 2.0 - gap * 0.5).max(side[by_size[0]]);
    let mut rows: Vec<(Vec<usize>, f64, f64)> = Vec::new();
    for i in by_size {
        if rows
            .last()
            .is_none_or(|(_, used, _)| used + side[i] > width)
        {
            rows.push((Vec::new(), 0.0, side[i]));
        }
        let row = rows.last_mut().expect("row initialized");
        row.0.push(i);
        row.1 += side[i];
    }
    let height: f64 = rows.iter().map(|(_, _, height)| height).sum();
    let available_height = center.y * 2.0 - gap * 0.5;
    let scale = (available_height / height).min(1.0);
    let row_gap = (available_height - height).max(0.0) / rows.len().saturating_sub(1).max(1) as f64;
    let field_height = height + row_gap * rows.len().saturating_sub(1) as f64;
    let mut y = -field_height * 0.5;
    let mut result = vec![center; order.len()];
    for (members, used, height) in rows {
        let column_gap = (width - used) / members.len().saturating_sub(1).max(1) as f64;
        let field_width = used + column_gap * members.len().saturating_sub(1) as f64;
        let mut x = -field_width * 0.5;
        for i in members {
            result[order[i]] = Point {
                x: center.x + (x + side[i] * 0.5) * scale,
                y: center.y + (y + height * 0.5) * scale,
            };
            x += side[i] + column_gap;
        }
        y += height + row_gap;
    }
    result
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
}
