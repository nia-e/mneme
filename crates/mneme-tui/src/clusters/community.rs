//! Sparse, bounded grouping of the already loaded display links.

use super::DisplayLink;
use std::collections::BTreeMap;

// Canvas decoration must finish even when modularity keeps finding tiny gains.
// Every sweep visits each node and each adjacency once; it never scans all pairs.
const MAX_SWEEPS: usize = 16;
const MAX_MERGE_ROUNDS: usize = 16;

/// Deterministic local weighted-modularity improvement, not a semantic census.
///
/// Storage is O(nodes + loaded links). After canonicalizing links, work is at
/// most MAX_SWEEPS local sweeps plus MAX_MERGE_ROUNDS coarsening rounds, each
/// O(nodes + links * log(nodes)). Communities and their members are returned in
/// canonical node order, including singletons for the caller to omit from its
/// display groups.
pub(super) fn sparse_groups(n: usize, links: &[DisplayLink]) -> Vec<Vec<usize>> {
    // Usually already canonicalized by DisplayTopology. Retaining that contract
    // here also makes edge order, direction and duplicate relation kinds unable
    // to change floating-point accumulation order or inflate connectivity.
    let mut edges = BTreeMap::<(usize, usize), f64>::new();
    for link in links {
        let weight = f64::from_bits(link.weight);
        if link.a >= n || link.b >= n || link.a == link.b || !weight.is_finite() || weight <= 0.0 {
            continue;
        }
        let weight = weight.min(1.0);
        edges
            .entry((link.a.min(link.b), link.a.max(link.b)))
            .and_modify(|old| *old = old.max(weight))
            .or_insert(weight);
    }
    // Modularity is invariant under uniform scaling. This keeps even an all-
    // subnormal-weight graph meaningful rather than imposing a weight cutoff.
    let scale = edges.values().copied().fold(0.0, f64::max);
    let mut adjacent = vec![Vec::new(); n];
    let mut degree = vec![0.0; n];
    for ((a, b), weight) in edges {
        let weight = weight / scale;
        adjacent[a].push((b, weight));
        adjacent[b].push((a, weight));
        degree[a] += weight;
        degree[b] += weight;
    }
    let total_degree: f64 = degree.iter().sum();
    let mut assignment: Vec<_> = (0..n).collect();
    let mut group_degree = degree.clone();
    if total_degree > 0.0 {
        for _ in 0..MAX_SWEEPS {
            let mut changed = false;
            for node in 0..n {
                if adjacent[node].is_empty() {
                    continue;
                }
                let old = assignment[node];
                let mut into = BTreeMap::<usize, f64>::new();
                for &(neighbor, weight) in &adjacent[node] {
                    *into.entry(assignment[neighbor]).or_default() += weight;
                }
                // Temporarily remove this node. The candidate score differs
                // from the modularity gain by the same positive factor for all
                // candidates, so compare scores without squaring total weight.
                group_degree[old] = (group_degree[old] - degree[node]).max(0.0);
                let fraction = degree[node] / total_degree;
                let mut best = old;
                let mut best_score =
                    into.get(&old).copied().unwrap_or(0.0) - fraction * group_degree[old];
                // Relative roundoff tolerance, not a minimum edge weight.
                let tolerance = 16.0 * f64::EPSILON * degree[node];
                for (&candidate, &weight) in &into {
                    let score = weight - fraction * group_degree[candidate];
                    // Sorted candidates break genuine gain ties canonically;
                    // retain the existing group on a neutral move, preventing
                    // label churn and flat-objective oscillation.
                    if score > best_score + tolerance {
                        best = candidate;
                        best_score = score;
                    }
                }
                assignment[node] = best;
                group_degree[best] += degree[node];
                changed |= best != old;
            }
            if !changed {
                break;
            }
        }
        merge_communities(&adjacent, &degree, total_degree, &mut assignment);
    }

    // A node moving out can disconnect its old group. Split such leftovers in
    // one sparse pass: no disconnected nodes acquire a phantom shared group.
    let mut seen = vec![false; n];
    let mut groups = Vec::new();
    for seed in 0..n {
        if seen[seed] {
            continue;
        }
        seen[seed] = true;
        let mut members = vec![seed];
        let mut cursor = 0;
        while cursor < members.len() {
            let node = members[cursor];
            for &(neighbor, _) in &adjacent[node] {
                if !seen[neighbor] && assignment[neighbor] == assignment[seed] {
                    seen[neighbor] = true;
                    members.push(neighbor);
                }
            }
            cursor += 1;
        }
        members.sort_unstable();
        groups.push(members);
    }
    groups
}

/// Moving one node cannot escape every local optimum: an ordinary path can
/// freeze as pairs. Move whole communities too, but only across loaded links
/// whose aggregate weight makes their merge a positive modularity improvement.
fn merge_communities(
    adjacent: &[Vec<(usize, f64)>],
    degree: &[f64],
    total_degree: f64,
    assignment: &mut [usize],
) {
    let n = assignment.len();
    let mut group_degree = vec![0.0; n];
    for _ in 0..MAX_MERGE_ROUNDS {
        group_degree.fill(0.0);
        for (node, &group) in assignment.iter().enumerate() {
            group_degree[group] += degree[node];
        }
        let mut between = BTreeMap::<(usize, usize), f64>::new();
        for (node, neighbors) in adjacent.iter().enumerate() {
            for &(neighbor, weight) in neighbors {
                if node >= neighbor {
                    continue; // Count every undirected loaded link only once.
                }
                let (a, b) = (assignment[node], assignment[neighbor]);
                if a != b {
                    *between.entry((a.min(b), a.max(b))).or_default() += weight;
                }
            }
        }
        let mut candidates: Vec<_> = between
            .into_iter()
            .filter_map(|((a, b), weight)| {
                let gain = weight - (group_degree[a] / total_degree) * group_degree[b];
                let tolerance = 16.0 * f64::EPSILON * group_degree[a].min(group_degree[b]);
                (gain > tolerance).then_some((gain, a, b))
            })
            .collect();
        if candidates.is_empty() {
            break;
        }
        candidates.sort_by(|a, b| {
            b.0.total_cmp(&a.0)
                .then_with(|| a.1.cmp(&b.1))
                .then_with(|| a.2.cmp(&b.2))
        });
        let mut merged = vec![false; n];
        let mut target: Vec<_> = (0..n).collect();
        for (_, a, b) in candidates {
            // Disjoint pairs preserve each candidate's positive-gain proof
            // throughout this round. Rebuild sparse aggregate edges next round
            // instead of using stale gains after a neighboring merge.
            if !merged[a] && !merged[b] {
                target[b] = a;
                merged[a] = true;
                merged[b] = true;
            }
        }
        for group in assignment.iter_mut() {
            *group = target[*group];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(a: usize, b: usize, weight: f64) -> DisplayLink {
        DisplayLink {
            a,
            b,
            weight: weight.to_bits(),
        }
    }

    fn bridged_pockets() -> Vec<DisplayLink> {
        let mut links = Vec::new();
        for start in [0, 70] {
            for a in start..start + 70 {
                for b in a + 1..start + 70 {
                    links.push(link(a, b, 0.9));
                }
            }
        }
        links.push(link(69, 70, 0.001));
        links
    }

    #[test]
    fn large_weighted_pockets_do_not_collapse_across_a_weak_bridge() {
        let groups = sparse_groups(143, &bridged_pockets());
        assert_eq!(
            groups,
            vec![
                (0..70).collect::<Vec<_>>(),
                (70..140).collect(),
                vec![140],
                vec![141],
                vec![142],
            ]
        );
    }

    #[test]
    fn direction_order_and_duplicate_relations_do_not_change_partition() {
        let mut links = bridged_pockets();
        let expected = sparse_groups(143, &links);
        links.reverse();
        for edge in &mut links {
            std::mem::swap(&mut edge.a, &mut edge.b);
        }
        links.extend([link(0, 1, 0.8), link(70, 69, 0.0001)]);
        assert_eq!(sparse_groups(143, &links), expected);
    }

    #[test]
    fn missing_or_invalid_links_do_not_invent_groups() {
        let links = [
            link(0, 1, 0.0),
            link(1, 2, -1.0),
            link(2, 3, f64::NAN),
            link(3, 4, f64::INFINITY),
            link(4, 4, 1.0),
            link(5, 500, 1.0),
        ];
        let singles: Vec<_> = (0..140).map(|i| vec![i]).collect();
        assert_eq!(sparse_groups(140, &links), singles);
        assert_eq!(sparse_groups(140, &[]), singles);
        assert!(sparse_groups(0, &links).is_empty());
    }

    #[test]
    fn positive_tiny_links_remain_weighted_connections() {
        let mut links = bridged_pockets();
        let expected = sparse_groups(140, &links);
        for edge in &mut links {
            edge.weight = (f64::from_bits(edge.weight) * 1e-300).to_bits();
        }
        assert_eq!(sparse_groups(140, &links), expected);
        assert_eq!(
            sparse_groups(3, &[link(0, 1, f64::from_bits(1))]),
            vec![vec![0, 1], vec![2]]
        );
    }

    #[test]
    fn disjoint_short_paths_coarsen_into_whole_neighborhoods() {
        let links: Vec<_> = (0..20)
            .flat_map(|group| {
                (0..11).map(move |offset| link(group * 12 + offset, group * 12 + offset + 1, 0.75))
            })
            .collect();
        let expected: Vec<Vec<_>> = (0..20)
            .map(|group| (group * 12..(group + 1) * 12).collect())
            .collect();
        assert_eq!(sparse_groups(240, &links), expected);
    }

    #[test]
    fn every_node_occurs_once_and_every_nonsingleton_is_connected() {
        let links: Vec<_> = (0..149)
            .map(|i| link(i, i + 1, if i % 7 == 0 { 0.01 } else { 0.9 }))
            .collect();
        let groups = sparse_groups(153, &links);
        let mut members: Vec<_> = groups.iter().flatten().copied().collect();
        members.sort_unstable();
        assert_eq!(members, (0..153).collect::<Vec<_>>());
        // In a path, connected subsets are precisely contiguous ranges.
        assert!(
            groups
                .iter()
                .all(|group| { group.windows(2).all(|pair| pair[1] == pair[0] + 1) })
        );
        assert!(groups.iter().any(|group| group.len() > 1));
        assert!(groups.iter().any(|group| group == &[152]));
    }
}
