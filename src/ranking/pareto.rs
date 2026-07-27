//! Pareto front computation per T01-06 §16.12 step 3.
//!
//! A proposal `a` dominates `b` when `a` is at least as good in every
//! criterion and strictly better in at least one. The front is the set
//! of non-dominated proposals. The MVP runs this filter on the
//! aggregated judge criteria (correctness, completeness, fit, evidence,
//! clarity).
//!
//! Higher is better for every criterion. Ties break the dominance in
//! favour of the first proposal (the earlier index wins).

use crate::phases::judge::Aggregated;

/// Per-criterion quality vector extracted from an aggregated judge
/// score. The vector has the same five dimensions the judges emit so
/// callers do not have to re-derive the components.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QualityVector {
    /// 0..=10. Higher is better.
    pub correctness: f32,
    /// 0..=10.
    pub completeness: f32,
    /// 0..=10.
    pub fit: f32,
    /// 0..=10.
    pub evidence: f32,
    /// 0..=10.
    pub clarity: f32,
}

impl QualityVector {
    /// Build a `QualityVector` from an aggregated judge score.
    pub fn from_aggregated(a: &Aggregated) -> Self {
        Self {
            correctness: a.correctness,
            completeness: a.completeness,
            fit: a.fit,
            evidence: a.evidence,
            clarity: a.clarity,
        }
    }

    fn dims(&self) -> [f32; 5] {
        [
            self.correctness,
            self.completeness,
            self.fit,
            self.evidence,
            self.clarity,
        ]
    }
}

/// `a` dominates `b` when `a >= b` in every dimension and strictly `>`
/// in at least one. Higher is better.
pub fn dominates(a: &QualityVector, b: &QualityVector) -> bool {
    let ad = a.dims();
    let bd = b.dims();
    let mut all_ge = true;
    let mut any_gt = false;
    for i in 0..ad.len() {
        if ad[i] < bd[i] {
            all_ge = false;
            break;
        }
        if ad[i] > bd[i] {
            any_gt = true;
        }
    }
    all_ge && any_gt
}

/// Return the indices of proposals that are not dominated by any
/// other proposal in `vectors`. Ties broken in favour of the earlier
/// index. The returned indices are in ascending order.
pub fn pareto_front(vectors: &[QualityVector]) -> Vec<usize> {
    let n = vectors.len();
    let mut dominated_by: Vec<Option<usize>> = vec![None; n];
    for i in 0..n {
        if dominated_by[i].is_some() {
            continue;
        }
        for j in (i + 1)..n {
            if dominated_by[j].is_some() {
                continue;
            }
            if dominates(&vectors[i], &vectors[j]) {
                dominated_by[j] = Some(i);
            } else if dominates(&vectors[j], &vectors[i]) {
                dominated_by[i] = Some(j);
                break;
            }
        }
    }
    (0..n).filter(|i| dominated_by[*i].is_none()).collect()
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
    fn dominance_requires_strict_better_in_one() {
        let a = v(8.0, 8.0, 8.0, 8.0, 8.0);
        let b = v(8.0, 8.0, 8.0, 8.0, 8.0);
        assert!(!dominates(&a, &b));
    }

    #[test]
    fn dominance_strict_in_one_wins() {
        let a = v(9.0, 8.0, 8.0, 8.0, 8.0);
        let b = v(8.0, 8.0, 8.0, 8.0, 8.0);
        assert!(dominates(&a, &b));
        assert!(!dominates(&b, &a));
    }

    #[test]
    fn front_keeps_non_dominated() {
        let vs = vec![
            v(9.0, 8.0, 7.0, 6.0, 5.0),
            v(5.0, 6.0, 7.0, 8.0, 9.0),
            v(1.0, 1.0, 1.0, 1.0, 1.0),
        ];
        let front = pareto_front(&vs);
        // 0 and 1 are non-dominated; 2 is dominated by both.
        assert_eq!(front, vec![0, 1]);
    }

    #[test]
    fn front_keeps_all_when_distinct() {
        let vs = vec![
            v(9.0, 8.0, 7.0, 6.0, 5.0),
            v(8.0, 9.0, 7.0, 6.0, 5.0),
            v(7.0, 8.0, 9.0, 6.0, 5.0),
        ];
        let front = pareto_front(&vs);
        assert_eq!(front.len(), 3);
    }

    #[test]
    fn front_handles_empty() {
        let vs: Vec<QualityVector> = vec![];
        assert!(pareto_front(&vs).is_empty());
    }
}
