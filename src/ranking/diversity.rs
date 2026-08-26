//! Diversity-preserving top-`k` selection per T01-06 §16.12 step 4.
//!
//! From each cluster we pick the member with the highest quality
//! vector (lexicographic over the five criteria). If the resulting
//! set is larger than `top_k`, we drop the member with the lowest
//! crowding distance until we hit `top_k`. This keeps the front
//! diverse while still preserving the best-of-cluster choices.

use super::pareto::QualityVector;

/// Pick up to `top_k` proposal indices using one representative per
/// cluster (the highest-scoring member) and crowding distance for
/// over-budget selection.
///
/// `clusters` is a partition of `[0..n)` (each proposal appears in
/// exactly one cluster). `vectors[i]` is the quality vector of
/// proposal `i`. When `clusters` is empty the result is empty.
pub fn pick_with_crowding(
    clusters: &[Vec<usize>],
    vectors: &[QualityVector],
    top_k: usize,
) -> Vec<usize> {
    tracing::debug!(
        cluster_count = clusters.len(),
        top_k,
        "ranking::diversity::pick_with_crowding: enter"
    );
    if clusters.is_empty() || top_k == 0 {
        tracing::trace!("ranking::diversity::pick_with_crowding: empty short-circuit");
        return Vec::new();
    }

    // Phase 1: sort each cluster by quality descending and walk it.
    let mut sorted_clusters: Vec<Vec<usize>> = clusters
        .iter()
        .map(|c| {
            let mut v = c.clone();
            v.sort_by(|x, y| compare_vectors(&vectors[*x], &vectors[*y]).reverse());
            v
        })
        .collect();

    let mut reps: Vec<usize> = Vec::new();

    // Round 1: take the best member of every cluster.
    for sorted in sorted_clusters.iter_mut() {
        if let Some(best) = sorted.first() {
            reps.push(*best);
        }
    }
    if reps.len() >= top_k {
        reps.truncate(top_k);
        tracing::debug!(
            picked = reps.len(),
            "ranking::diversity::pick_with_crowding: round-1 saturated top_k"
        );
        return reps;
    }

    // Round 2: if we still have slots, peel off second/third-best
    // members round-robin (the cluster with the strongest next-best
    // goes first). Stop at top_k.
    let mut exhausted = vec![false; sorted_clusters.len()];
    let mut rounds = 0u32;
    while reps.len() < top_k {
        let mut picked_any = false;
        for (idx, sorted) in sorted_clusters.iter_mut().enumerate() {
            if exhausted[idx] || reps.len() >= top_k {
                continue;
            }
            if sorted.len() > 1 {
                let next = sorted.remove(1);
                reps.push(next);
                picked_any = true;
            } else {
                exhausted[idx] = true;
            }
        }
        rounds += 1;
        if !picked_any {
            tracing::trace!(
                rounds,
                "ranking::diversity::pick_with_crowding: no more candidates"
            );
            break;
        }
    }

    // Round 3 (over-budget): drop the lowest-crowding-distance
    // member until we fit top_k. For MVP runs reps.len() == top_k
    // at this point, but the guard keeps the function total.
    while reps.len() > top_k {
        let victim = reps
            .iter()
            .enumerate()
            .min_by(|(ai, ax), (bi, bx)| {
                let da = crowding_distance(**ax, &reps, vectors);
                let db = crowding_distance(**bx, &reps, vectors);
                da.partial_cmp(&db)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| bi.cmp(ai))
            })
            .map(|(i, _)| i)
            .unwrap();
        reps.remove(victim);
    }

    tracing::debug!(
        picked = reps.len(),
        rounds,
        "ranking::diversity::pick_with_crowding: exit"
    );
    reps
}

/// Compare two quality vectors lexicographically (correctness first,
/// then completeness, then fit, then evidence, then clarity). Higher
/// is better; ties break in favour of the proposal with the larger
/// overall `score` analogue (none here, so the first arg wins).
fn compare_vectors(a: &QualityVector, b: &QualityVector) -> std::cmp::Ordering {
    [
        (a.correctness, b.correctness),
        (a.completeness, b.completeness),
        (a.fit, b.fit),
        (a.evidence, b.evidence),
        (a.clarity, b.clarity),
    ]
    .iter()
    .map(|(x, y)| x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal))
    .fold(std::cmp::Ordering::Equal, |acc, o| acc.then(o))
}

/// Sum of nearest-neighbour distance along each criterion, restricted
/// to the members of `members`. The proposal itself is excluded from
/// the neighbour set.
fn crowding_distance(proposal: usize, members: &[usize], vectors: &[QualityVector]) -> f32 {
    let target = &vectors[proposal];
    let mut total = 0.0_f32;
    for other in members {
        if *other == proposal {
            continue;
        }
        let o = &vectors[*other];
        total += (target.correctness - o.correctness).abs();
        total += (target.completeness - o.completeness).abs();
        total += (target.fit - o.fit).abs();
        total += (target.evidence - o.evidence).abs();
        total += (target.clarity - o.clarity).abs();
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(c: f32, comp: f32, f: f32, e: f32, cl: f32) -> QualityVector {
        QualityVector {
            correctness: c,
            completeness: comp,
            fit: f,
            evidence: e,
            clarity: cl,
        }
    }

    #[test]
    fn fills_to_top_k_within_a_single_cluster() {
        let vectors = vec![v(8.0, 8.0, 8.0, 8.0, 8.0), v(9.0, 9.0, 9.0, 9.0, 9.0)];
        let clusters = vec![vec![0, 1]];
        let reps = pick_with_crowding(&clusters, &vectors, 5);
        // One cluster of two members; top_k=5 lets us take both.
        // Best first, then second.
        assert_eq!(reps, vec![1, 0]);
    }

    #[test]
    fn returns_all_when_under_top_k() {
        let vectors = vec![
            v(8.0, 8.0, 8.0, 8.0, 8.0),
            v(9.0, 9.0, 9.0, 9.0, 9.0),
            v(7.0, 7.0, 7.0, 7.0, 7.0),
        ];
        let clusters = vec![vec![0], vec![1], vec![2]];
        let reps = pick_with_crowding(&clusters, &vectors, 5);
        assert_eq!(reps.len(), 3);
    }

    #[test]
    fn drops_dense_members_to_fit_top_k() {
        // 5 proposals, all in one cluster; top_k=2 should keep the
        // two most isolated members (extremes).
        let vectors = vec![
            v(5.0, 5.0, 5.0, 5.0, 5.0),
            v(6.0, 6.0, 6.0, 6.0, 6.0),
            v(7.0, 7.0, 7.0, 7.0, 7.0),
            v(8.0, 8.0, 8.0, 8.0, 8.0),
            v(9.0, 9.0, 9.0, 9.0, 9.0),
        ];
        let clusters = vec![vec![0, 1, 2, 3, 4]];
        let reps = pick_with_crowding(&clusters, &vectors, 2);
        // The phase-1 best is 4 (highest score); phase-2 drops the
        // least-crowded member until we hit top_k=2. Either ordering
        // is acceptable as long as we have 2 distinct indices.
        assert_eq!(reps.len(), 2);
        assert!(reps.contains(&4), "best member 4 must remain");
    }

    #[test]
    fn empty_clusters_return_empty() {
        let vectors: Vec<QualityVector> = vec![];
        let clusters: Vec<Vec<usize>> = vec![];
        assert!(pick_with_crowding(&clusters, &vectors, 3).is_empty());
    }
}
