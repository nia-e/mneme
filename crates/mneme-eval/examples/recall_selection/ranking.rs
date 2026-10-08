//! Pure post-admission ordering. Neither relevance nor diversity can add a node
//! that retrieval did not admit, and no score crosses the primary/probationary
//! lane boundary.

use std::collections::HashSet;

pub const MMR_LAMBDA: f32 = 0.7;
pub const MENU_PREFIXES: [usize; 5] = [1, 2, 4, 8, 12];

/// Return indices in descending score order, with source order as the stable
/// tie-break. Non-finite model output is rejected rather than silently ranked.
pub fn ranked(scores: &[f32]) -> Result<Vec<usize>, String> {
    if scores.iter().any(|score| !score.is_finite()) {
        return Err("non-finite reranker score".into());
    }
    let mut order: Vec<_> = (0..scores.len()).collect();
    order.sort_by(|&a, &b| scores[b].total_cmp(&scores[a]).then(a.cmp(&b)));
    Ok(order)
}

/// Rank-normalized relevance is based on a *policy ordering*, not raw BGE
/// logits. This makes current+MMR and BGE+MMR comparable without pretending
/// their scores share a scale. For N>1, top=1, bottom=0.
fn normalized_relevance(rank: usize, len: usize) -> f32 {
    if len < 2 {
        1.0
    } else {
        (len - 1 - rank) as f32 / (len - 1) as f32
    }
}

fn cosine(a: &[f32], b: &[f32]) -> Result<f32, String> {
    if a.len() != b.len() || a.is_empty() || a.iter().chain(b).any(|x| !x.is_finite()) {
        return Err("invalid embedding vector".into());
    }
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let aa: f32 = a.iter().map(|x| x * x).sum();
    let bb: f32 = b.iter().map(|x| x * x).sum();
    if aa == 0.0 || bb == 0.0 {
        return Err("zero embedding vector".into());
    }
    Ok((dot / (aa.sqrt() * bb.sqrt())).clamp(-1.0, 1.0))
}

/// Greedy MMR over the same frozen pool. `base_order` is a permutation of
/// `embeddings` indices. The first pick is always the base winner; later picks
/// maximize .7 * rank-normalized relevance - .3 * maximum selected cosine.
pub fn mmr(base_order: &[usize], embeddings: &[Vec<f32>]) -> Result<Vec<usize>, String> {
    if base_order.len() != embeddings.len() {
        return Err("MMR ordering and embedding count differ".into());
    }
    let n = base_order.len();
    let mut seen = HashSet::new();
    if base_order.iter().any(|&i| i >= n || !seen.insert(i)) {
        return Err("MMR base ordering is not a permutation".into());
    }
    let mut similarity: Vec<Vec<f32>> = vec![vec![0.0_f32; n]; n];
    for i in 0..n {
        for j in 0..i {
            similarity[i][j] = cosine(&embeddings[i], &embeddings[j])?.clamp(0.0, 1.0);
            similarity[j][i] = similarity[i][j];
        }
    }
    let mut rank = vec![0; n];
    for (r, &index) in base_order.iter().enumerate() {
        rank[index] = r;
    }
    let mut output: Vec<usize> = Vec::with_capacity(n);
    let mut selected = vec![false; n];
    while output.len() < n {
        let next = (0..n)
            .filter(|&i| !selected[i])
            .max_by(|&a, &b| {
                let value = |i: usize| {
                    let redundancy: f32 = output
                        .iter()
                        .map(|&j: &usize| similarity[i][j])
                        .fold(0.0_f32, f32::max);
                    MMR_LAMBDA * normalized_relevance(rank[i], n) - (1.0 - MMR_LAMBDA) * redundancy
                };
                value(a)
                    .total_cmp(&value(b))
                    .then_with(|| rank[b].cmp(&rank[a]))
            })
            .expect("unselected index remains");
        selected[next] = true;
        output.push(next);
    }
    Ok(output)
}

/// Distinct complete prefix sets from all policies. A menu option denotes a
/// set, not an invented source rank or a change to the packer's order.
pub fn menu(orders: &[(&str, &[usize])], cap: usize) -> Vec<(String, Vec<usize>)> {
    let mut result = Vec::new();
    let mut seen = HashSet::new();
    for &(policy, order) in orders {
        for prefix in MENU_PREFIXES {
            let mut indices = order[..prefix.min(cap).min(order.len())].to_vec();
            indices.sort_unstable();
            if seen.insert(indices.clone()) {
                result.push((format!("{policy}:{prefix}"), indices));
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mmr_can_prefer_complement_to_duplicate() {
        let order = [0, 1, 2, 3];
        let vectors = vec![vec![1., 0.], vec![1., 0.], vec![0., 1.], vec![0., 1.]];
        assert_eq!(mmr(&order, &vectors).unwrap()[..2], [0, 2]);
    }

    #[test]
    fn menu_deduplicates_identical_sets() {
        let a = [0, 1, 2];
        let b = [0, 2, 1];
        let result = menu(&[("a", &a), ("b", &b)], 12);
        assert_eq!(result.len(), 4); // {0}, {0,1}, {0,2}, {0,1,2}
    }
}
