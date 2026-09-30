//! Reciprocal rank fusion of two best-first id lists (BZR-182).
//! Rank is 1-based. Recall does not call this.

use std::collections::{BTreeMap, HashSet};

pub const RRF_K: u32 = 60;

#[derive(Debug, Clone, PartialEq)]
pub struct Fused {
    pub id: String,
    pub score: f64,
}

/// Fuse `lexical` and `dense`. An id's score is the sum of
/// `1 / (RRF_K as f64 + rank)` over each list that contains it.
/// The first occurrence in a list is its rank. A later duplicate in that
/// same list is ignored. An empty id is ignored.
/// Argument order does not change the result.
/// Output is score descending, then id ascending (`str` byte order).
pub fn fuse(lexical: &[&str], dense: &[&str]) -> Vec<Fused> {
    let mut scores: BTreeMap<&str, f64> = BTreeMap::new();
    for list in [lexical, dense] {
        let mut seen: HashSet<&str> = HashSet::new();
        for (idx, &id) in list.iter().enumerate() {
            if id.is_empty() || !seen.insert(id) {
                continue;
            }
            let rank = (idx + 1) as f64;
            *scores.entry(id).or_insert(0.0) += 1.0 / (f64::from(RRF_K) + rank);
        }
    }
    let mut fused: Vec<Fused> = scores
        .into_iter()
        .map(|(id, score)| Fused {
            id: id.to_owned(),
            score,
        })
        .collect();
    fused.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.id.cmp(&b.id)));
    fused
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(got: f64, want: f64) -> bool {
        (got - want).abs() < 1e-12
    }

    #[test]
    fn both_lists_sum_and_order() {
        assert_eq!(RRF_K, 60);
        let got = fuse(&["a", "b"], &["b", "c"]);
        let ids: Vec<&str> = got.iter().map(|f| f.id.as_str()).collect();
        assert_eq!(ids, ["b", "a", "c"]);
        assert!(close(got[0].score, 1.0 / 61.0 + 1.0 / 62.0));
        assert!(close(got[1].score, 1.0 / 61.0));
        assert!(close(got[2].score, 1.0 / 62.0));
        assert_eq!(fuse(&["b", "c"], &["a", "b"]), got);
    }

    #[test]
    fn empty_is_empty() {
        assert!(fuse(&[], &[]).is_empty());
        assert!(fuse(&[""], &[]).is_empty());
    }

    #[test]
    fn duplicate_in_one_list_keeps_first_rank() {
        let got = fuse(&["a", "a"], &[]);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, "a");
        assert!(close(got[0].score, 1.0 / 61.0));
    }

    #[test]
    fn tie_breaks_by_id_ascending() {
        let got = fuse(&["b"], &["a"]);
        let ids: Vec<&str> = got.iter().map(|f| f.id.as_str()).collect();
        assert_eq!(ids, ["a", "b"]);
        assert!(close(got[0].score, 1.0 / 61.0));
        assert!(close(got[1].score, 1.0 / 61.0));
    }
}
