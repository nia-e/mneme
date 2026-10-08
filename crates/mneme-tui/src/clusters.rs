//! Derived groups in the *loaded display graph*, not stored semantic communities.
//! Direction and relation kind remain facts of the drawn links/inspector; grouping
//! only summarizes their finite positive weights as an undirected visual aid.

use crate::model::Graph;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct DisplayGroupId(pub(crate) u64);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DisplayGroup {
    pub(crate) id: DisplayGroupId,
    pub(crate) members: Vec<String>,
    /// A distinctive existing tag, never a generated topic or truth claim.
    pub(crate) label: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct DisplayGroups {
    pub(crate) groups: Vec<DisplayGroup>,
    pub(crate) lookup: BTreeMap<String, usize>,
}

impl DisplayGroups {
    pub(crate) fn group_for(&self, id: &str) -> Option<&DisplayGroup> {
        if let Some(index) = self.lookup.get(id) {
            return self.groups.get(*index);
        }
        if !self.lookup.is_empty() {
            return None;
        }
        self.groups.iter().find(|group| {
            group
                .members
                .binary_search_by(|member| member.as_str().cmp(id))
                .is_ok()
        })
    }

    pub(crate) fn with_labels(mut self, graph: &Graph) -> Self {
        // Labels are metadata, never geometry/cache identity. Inventory pages
        // without a completed census cannot establish an outside denominator.
        if graph
            .inventory
            .as_ref()
            .is_some_and(|state| !state.complete)
        {
            for group in &mut self.groups {
                group.label = None;
            }
            return self;
        }
        // An incomplete card is unknown, not an empty tag set. Never pool
        // clipped prefixes or sanitized strings as supposedly exact tags.
        // Conservative whole-card uncertainty also bounds naming work.
        let tags: BTreeMap<_, Option<BTreeSet<_>>> = graph
            .nodes
            .iter()
            .map(|node| {
                let complete = node.tags_complete
                    && node.tags.len() <= 16
                    && node.tags.iter().all(|tag| {
                        tag.chars().take(81).count() <= 80 && !tag.chars().any(char::is_control)
                    });
                let tags = complete.then(|| {
                    node.tags
                        .iter()
                        .filter(|tag| !tag.trim().is_empty())
                        .map(String::as_str)
                        .collect()
                });
                (node.id.as_str(), tags)
            })
            .collect();
        let unknown = tags.values().filter(|set| set.is_none()).count();
        let mut all: BTreeMap<&str, usize> = BTreeMap::new();
        for set in tags.values().flatten() {
            for tag in set {
                *all.entry(tag).or_default() += 1;
            }
        }
        for group in &mut self.groups {
            group.label = None;
            let inside = group.members.len();
            let outside = tags.len().saturating_sub(inside);
            if inside == 0 || outside == 0 {
                continue; // A sole group cannot demonstrate specificity.
            }
            let mut candidates: BTreeMap<&str, usize> = BTreeMap::new();
            let mut unknown_inside = 0;
            for member in &group.members {
                if let Some(Some(set)) = tags.get(member.as_str()) {
                    for tag in set {
                        *candidates.entry(tag).or_default() += 1;
                    }
                } else {
                    unknown_inside += 1;
                }
            }
            let unknown_outside = unknown.saturating_sub(unknown_inside);
            group.label = candidates
                .into_iter()
                .filter_map(|(tag, count)| {
                    // Display heuristic, not statistical significance: at least
                    // two exact authored supporters and half the group; inside
                    // lower prevalence must be at least twice outside's upper
                    // prevalence (all unknown outside cards may have this tag).
                    let outside_upper =
                        all.get(tag).copied().unwrap_or(0).saturating_sub(count) + unknown_outside;
                    let lower = count as u128 * outside as u128;
                    let upper = outside_upper as u128 * inside as u128;
                    (count >= 2 && count * 2 >= inside && lower > upper && lower >= 2 * upper)
                        .then(|| (tag, count, lower - upper))
                })
                .max_by(|a, b| a.2.cmp(&b.2).then(a.1.cmp(&b.1)).then_with(|| b.0.cmp(a.0)))
                .map(|(tag, _, _)| tag.trim().to_owned());
        }
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DisplayLink {
    pub(crate) a: usize,
    pub(crate) b: usize,
    pub(crate) weight: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct DisplayTopology {
    pub(crate) ids: Vec<String>,
    pub(crate) links: Vec<DisplayLink>,
}

impl DisplayTopology {
    pub(crate) fn new(graph: &Graph) -> Self {
        let mut ids: Vec<_> = graph.nodes.iter().map(|n| n.id.clone()).collect();
        ids.sort();
        ids.dedup();
        let mut weights: BTreeMap<(usize, usize), u64> = BTreeMap::new();
        for edge in &graph.edges {
            let (Ok(a), Ok(b)) = (ids.binary_search(&edge.from), ids.binary_search(&edge.to))
            else {
                continue;
            };
            let weight = if edge.weight.is_finite() {
                edge.weight.clamp(0.0, 1.0)
            } else {
                0.0
            };
            if a == b || weight <= 0.0 {
                continue;
            }
            let (a, b) = (a.min(b), a.max(b));
            weights
                .entry((a, b))
                .and_modify(|old| *old = (*old).max(weight.to_bits()))
                .or_insert(weight.to_bits());
        }
        let links = weights
            .into_iter()
            .map(|((a, b), weight)| DisplayLink { a, b, weight })
            .collect();
        Self { ids, links }
    }

    pub(crate) fn groups(&self, previous: &DisplayGroups) -> DisplayGroups {
        let n = self.ids.len();
        let mut members: Vec<Vec<usize>> = (0..n).map(|i| vec![i]).collect();
        let mut degrees = vec![0.0; n];
        // Large skeletons must not allocate an n² matrix merely to decorate
        // the canvas. Use loaded-link components there; small neighborhoods keep
        // the finer weighted modularity composition below.
        if n > 128 {
            let mut parent: Vec<_> = (0..n).collect();
            fn root(parent: &mut [usize], mut i: usize) -> usize {
                while parent[i] != i {
                    parent[i] = parent[parent[i]];
                    i = parent[i];
                }
                i
            }
            for link in &self.links {
                let a = root(&mut parent, link.a);
                let b = root(&mut parent, link.b);
                parent[b] = a;
            }
            let mut components: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
            for i in 0..n {
                components.entry(root(&mut parent, i)).or_default().push(i);
            }
            members = components.into_values().collect();
        }
        let matrix_n = if n > 128 { 0 } else { n };
        let mut between = vec![vec![0.0; matrix_n]; matrix_n];
        let mut total = 0.0;
        for link in &self.links {
            let weight = f64::from_bits(link.weight);
            degrees[link.a] += weight;
            degrees[link.b] += weight;
            if matrix_n > 0 {
                between[link.a][link.b] = weight;
                between[link.b][link.a] = weight;
            }
            total += weight;
        }
        // Greedy weighted modularity, deterministically tied by canonical node
        // order. At most n-1 merges and n² candidates per merge; never backend
        // whole-store community detection, inference, or persistent writes.
        if total > 0.0 && matrix_n > 0 {
            for _ in 0..n.saturating_sub(1).min(64) {
                let mut best = None;
                let mut gain = 1e-10;
                for a in 0..n {
                    if members[a].is_empty() {
                        continue;
                    }
                    for b in a + 1..n {
                        if members[b].is_empty() {
                            continue;
                        }
                        let delta =
                            between[a][b] / total - degrees[a] * degrees[b] / (2.0 * total * total);
                        if delta > gain + 1e-12 {
                            gain = delta;
                            best = Some((a, b));
                        }
                    }
                }
                let Some((a, b)) = best else { break };
                let moved = std::mem::take(&mut members[b]);
                members[a].extend(moved);
                members[a].sort_unstable();
                degrees[a] += degrees[b];
                degrees[b] = 0.0;
                for k in 0..n {
                    if k == a || k == b {
                        continue;
                    }
                    between[a][k] += between[b][k];
                    between[k][a] = between[a][k];
                    between[b][k] = 0.0;
                    between[k][b] = 0.0;
                }
                between[a][b] = 0.0;
                between[b][a] = 0.0;
            }
        }
        let fresh: Vec<Vec<String>> = members
            .into_iter()
            .filter(|m| m.len() > 1)
            .map(|m| m.into_iter().map(|i| self.ids[i].clone()).collect())
            .collect();
        let mut assigned = vec![None; fresh.len()];
        let mut used = BTreeSet::new();
        let mut matches = Vec::new();
        let old_members: BTreeMap<_, _> = previous
            .groups
            .iter()
            .enumerate()
            .flat_map(|(index, group)| group.members.iter().map(move |id| (id.as_str(), index)))
            .collect();
        for (index, group) in fresh.iter().enumerate() {
            let mut overlaps: BTreeMap<usize, usize> = BTreeMap::new();
            for member in group {
                if let Some(old) = old_members.get(member.as_str()) {
                    *overlaps.entry(*old).or_default() += 1;
                }
            }
            for (old_index, shared) in overlaps {
                let old = &previous.groups[old_index];
                let union = group.len() + old.members.len() - shared;
                matches.push((
                    group == &old.members,
                    shared,
                    shared as f64 / union as f64,
                    old.id,
                    index,
                ));
            }
        }
        matches.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then(b.1.cmp(&a.1))
                .then(b.2.total_cmp(&a.2))
                .then(a.3.cmp(&b.3))
                .then(a.4.cmp(&b.4))
        });
        for (_, _, _, old, index) in matches {
            if assigned[index].is_none() && used.insert(old) {
                assigned[index] = Some(old);
            }
        }
        let groups: Vec<DisplayGroup> = fresh
            .into_iter()
            .enumerate()
            .map(|(index, members)| {
                let id = assigned[index].unwrap_or_else(|| {
                    let mut hash = 0xcbf29ce484222325u64;
                    for member in &members {
                        for byte in member.bytes().chain(std::iter::once(0)) {
                            hash = (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3);
                        }
                    }
                    let mut id = DisplayGroupId(hash);
                    while !used.insert(id) {
                        id.0 = id.0.wrapping_add(1);
                    }
                    id
                });
                DisplayGroup {
                    id,
                    members,
                    label: None,
                }
            })
            .collect();
        let lookup = groups
            .iter()
            .enumerate()
            .flat_map(|(index, group)| group.members.iter().cloned().map(move |id| (id, index)))
            .collect();
        DisplayGroups { groups, lookup }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Edge, Node};

    fn pockets() -> Graph {
        let nodes = ["a", "b", "c", "d", "e", "f", "island"]
            .into_iter()
            .map(|id| Node {
                id: id.into(),
                tags_complete: true,
                tags: vec![
                    "shared".into(),
                    if id <= "c" { "rust" } else { "garden" }.into(),
                ],
                ..Node::default()
            })
            .collect();
        let edges = [
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
        .collect();
        Graph {
            nodes,
            edges,
            ..Graph::default()
        }
    }

    #[test]
    fn weighted_pockets_split_and_isolates_are_not_invented_groups() {
        let graph = pockets();
        let groups = DisplayTopology::new(&graph)
            .groups(&DisplayGroups::default())
            .with_labels(&graph);
        assert_eq!(groups.groups.len(), 2);
        assert_eq!(groups.group_for("a").unwrap().members, ["a", "b", "c"]);
        assert_eq!(groups.group_for("d").unwrap().members, ["d", "e", "f"]);
        assert!(groups.group_for("island").is_none());
        assert_eq!(
            groups.group_for("a").unwrap().label.as_deref(),
            Some("rust")
        );
        assert_eq!(
            groups.group_for("d").unwrap().label.as_deref(),
            Some("garden")
        );
    }

    #[test]
    fn order_direction_and_duplicate_kinds_do_not_change_groups() {
        let mut graph = pockets();
        let first = DisplayTopology::new(&graph);
        graph.nodes.reverse();
        graph.edges.reverse();
        for edge in &mut graph.edges {
            std::mem::swap(&mut edge.from, &mut edge.to);
        }
        graph.edges.push(Edge {
            kind: "derived_from".into(),
            weight: 0.001,
            ..graph.edges[0].clone()
        });
        let second = DisplayTopology::new(&graph);
        assert_eq!(first, second);
        assert_eq!(
            first.groups(&DisplayGroups::default()),
            second.groups(&DisplayGroups::default())
        );
    }

    #[test]
    fn refresh_matching_keeps_ids_as_members_are_added_removed_or_split() {
        let mut graph = pockets();
        let old = DisplayTopology::new(&graph).groups(&DisplayGroups::default());
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
        let fresh = DisplayTopology::new(&graph).groups(&old);
        assert_eq!(
            old.group_for("a").unwrap().id,
            fresh.group_for("a").unwrap().id
        );
        assert_eq!(
            old.group_for("d").unwrap().id,
            fresh.group_for("d").unwrap().id
        );
        graph
            .edges
            .retain(|edge| edge.from != "a" && edge.to != "a");
        let split = DisplayTopology::new(&graph).groups(&fresh);
        assert_eq!(
            fresh.group_for("b").unwrap().id,
            split.group_for("b").unwrap().id
        );
        let ids: BTreeSet<_> = split.groups.iter().map(|g| g.id).collect();
        assert_eq!(ids.len(), split.groups.len());
    }

    #[test]
    fn no_ubiquitous_empty_or_control_label_and_labels_follow_current_cards() {
        let mut graph = pockets();
        for node in &mut graph.nodes {
            node.tags = vec!["everywhere".into(), "  ".into(), "\u{1b}\n".into()];
        }
        let topology = DisplayTopology::new(&graph);
        let groups = topology.groups(&DisplayGroups::default());
        assert!(
            groups
                .clone()
                .with_labels(&graph)
                .groups
                .iter()
                .all(|g| g.label.is_none())
        );
        for node in &mut graph.nodes {
            node.tags = vec!["everywhere".into()];
        }
        for node in graph.nodes.iter_mut().take(3) {
            node.tags = vec!["rust".into()];
        }
        let labels = groups.with_labels(&graph);
        assert_eq!(
            labels.group_for("a").unwrap().label.as_deref(),
            Some("rust")
        );
        assert!(
            labels
                .groups
                .iter()
                .filter_map(|g| g.label.as_ref())
                .all(|s| !s.chars().any(char::is_control))
        );
    }

    fn pair_field(total: usize, supporters: usize) -> Graph {
        Graph {
            nodes: (0..total)
                .map(|i| Node {
                    id: format!("node-{i:03}"),
                    tags: if i < supporters {
                        vec!["rust".into()]
                    } else {
                        vec![]
                    },
                    tags_complete: true,
                    ..Node::default()
                })
                .collect(),
            edges: vec![Edge {
                from: "node-000".into(),
                to: "node-001".into(),
                kind: "associative".into(),
                weight: 0.9,
            }],
            ..Graph::default()
        }
    }

    fn pair_label(graph: &Graph) -> Option<String> {
        DisplayTopology::new(graph)
            .groups(&DisplayGroups::default())
            .with_labels(graph)
            .group_for("node-000")
            .unwrap()
            .label
            .clone()
    }

    #[test]
    fn eighty_to_one_hundred_percent_tags_are_not_distinctive_pair_names() {
        for supporters in 8..=10 {
            assert_eq!(pair_label(&pair_field(10, supporters)), None);
        }
        assert_eq!(pair_label(&pair_field(10, 2)).as_deref(), Some("rust"));
        assert_eq!(pair_label(&pair_field(2, 2)), None);
    }

    #[test]
    fn unknown_outside_cards_cannot_manufacture_specificity() {
        let mut graph = pair_field(10, 2);
        for node in graph.nodes.iter_mut().skip(2) {
            node.tags_complete = false; // Unloaded or unavailable viewport summaries.
        }
        assert_eq!(pair_label(&graph), None);
        // Normal bounded summary delivery can reveal enough authored absence.
        for node in graph.nodes.iter_mut().skip(2).take(4) {
            node.tags_complete = true;
        }
        assert_eq!(pair_label(&graph).as_deref(), Some("rust"));
        graph.nodes[2].tags.push("rust".into());
        assert_eq!(pair_label(&graph), None); // Known + unknown outside, not either alone.
        for node in &mut graph.nodes {
            node.tags_complete = false;
        }
        assert_eq!(pair_label(&graph), None);
    }

    #[test]
    fn incomplete_census_and_tag_excerpts_stay_uncertain() {
        let mut graph = pair_field(10, 2);
        graph.inventory = Some(crate::model::InventoryState::default());
        assert_eq!(pair_label(&graph), None);
        graph.inventory.as_mut().unwrap().complete = true;
        assert_eq!(pair_label(&graph).as_deref(), Some("rust"));
        graph.nodes[0].tags_complete = false; // Backend tag truncation.
        assert_eq!(pair_label(&graph), None);
        graph.nodes[0].tags_complete = true;
        graph.nodes[0]
            .tags
            .extend((0..16).map(|i| format!("extra-{i}")));
        assert_eq!(pair_label(&graph), None); // Local tag-count cap.
        graph.nodes[0].tags = vec![format!("{}a", "x".repeat(80))];
        graph.nodes[1].tags = vec![format!("{}b", "x".repeat(80))];
        assert_eq!(pair_label(&graph), None); // No shared clipped prefix.
    }

    #[test]
    fn exact_shared_tags_and_ties_are_deterministic_not_topic_inference() {
        let mut graph = pair_field(10, 0);
        graph.nodes[0].tags = vec!["directory".into()];
        graph.nodes[1].tags = vec!["directories".into()];
        assert_eq!(pair_label(&graph), None);
        for node in graph.nodes.iter_mut().take(2) {
            node.tags = vec!["zebra".into(), "alpha".into(), "alpha".into()];
        }
        assert_eq!(pair_label(&graph).as_deref(), Some("alpha"));
        graph.nodes.reverse();
        for node in &mut graph.nodes {
            node.tags.reverse();
        }
        assert_eq!(pair_label(&graph).as_deref(), Some("alpha"));
    }

    #[test]
    fn two_members_do_not_name_a_large_group() {
        let mut graph = pair_field(10, 2);
        let groups = DisplayGroups {
            groups: vec![DisplayGroup {
                id: DisplayGroupId(1),
                members: graph.nodes.iter().take(5).map(|n| n.id.clone()).collect(),
                label: None,
            }],
            ..DisplayGroups::default()
        };
        assert!(groups.clone().with_labels(&graph).groups[0].label.is_none());
        for node in graph.nodes.iter_mut().skip(5) {
            node.tags.push("rust".into());
        }
        assert!(groups.with_labels(&graph).groups[0].label.is_none());
    }

    #[test]
    fn bad_weights_and_oversized_graph_are_bounded() {
        let mut graph = pockets();
        for edge in &mut graph.edges {
            edge.weight = f64::NAN;
        }
        assert!(
            DisplayTopology::new(&graph)
                .groups(&DisplayGroups::default())
                .groups
                .is_empty()
        );
        for i in 0..100 {
            graph.nodes.push(Node {
                id: format!("extra-{i}"),
                ..Node::default()
            });
        }
        for _ in 0..100 {
            graph.edges.push(Edge {
                from: "a".into(),
                to: "b".into(),
                kind: "associative".into(),
                weight: 0.9,
            });
        }
        let topology = DisplayTopology::new(&graph);
        assert_eq!(topology.ids.len(), graph.nodes.len());
    }
}
