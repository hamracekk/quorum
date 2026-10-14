//! Gradient-boosted regression trees (squared loss) as a [`Learner`].
//!
//! Start from the mean label; each round fits a shallow tree to the residuals and adds it,
//! shrunk by `learning_rate`. Features are binned at training-set quantiles (histogram splits),
//! a leaf needs `min_leaf` samples, and labels are winsorised at the 2.5th/97.5th percentiles —
//! returns are heavy-tailed, and one outlier week should not shape the trees. Deterministic:
//! ties go to the lower feature and threshold.

use trader_core::strategies::{FEATURES, Learner, Row, VolTarget, XsLearner};

/// Boosting settings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BoostParams {
    pub trees: usize,
    pub depth: usize,
    pub learning_rate: f64,
    pub min_leaf: usize,
    pub bins: usize,
}

impl Default for BoostParams {
    fn default() -> Self {
        Self {
            trees: 100,
            depth: 3,
            learning_rate: 0.05,
            min_leaf: 20,
            bins: 32,
        }
    }
}

/// A tree node: a split or a leaf value.
#[derive(Debug, Clone, PartialEq)]
enum Node {
    Split {
        feature: usize,
        /// Go left when the value is at most this.
        threshold: f64,
        left: usize,
        right: usize,
    },
    Leaf(f64),
}

#[derive(Debug, Clone, PartialEq)]
struct Tree {
    nodes: Vec<Node>,
}

impl Tree {
    fn predict(&self, row: &Row) -> f64 {
        let mut at = 0;
        loop {
            match self.nodes.get(at) {
                Some(Node::Split {
                    feature,
                    threshold,
                    left,
                    right,
                }) => {
                    at = if row[*feature] <= *threshold {
                        *left
                    } else {
                        *right
                    }
                }
                Some(Node::Leaf(value)) => return *value,
                None => return 0.0,
            }
        }
    }
}

/// Gradient-boosted trees.
#[derive(Debug, Clone, Default)]
pub struct Gbdt {
    params: BoostParams,
    base: f64,
    trees: Vec<Tree>,
}

impl Gbdt {
    pub const fn new(params: BoostParams) -> Self {
        Self {
            params,
            base: 0.0,
            trees: Vec::new(),
        }
    }
}

/// The `num / den` quantile of sorted `values` (nearest rank, integer arithmetic).
fn quantile(sorted: &[f64], num: usize, den: usize) -> f64 {
    let Some(last) = sorted.len().checked_sub(1) else {
        return 0.0;
    };
    let rank = (num * last + den / 2) / den.max(1);
    sorted.get(rank.min(last)).copied().unwrap_or(0.0)
}

/// Candidate thresholds per feature: distinct training-set quantiles.
fn thresholds(rows: &[Row], bins: usize) -> Vec<Vec<f64>> {
    (0..FEATURES)
        .map(|j| {
            let mut values: Vec<f64> = rows
                .iter()
                .map(|r| r[j])
                .filter(|v| v.is_finite())
                .collect();
            values.sort_by(f64::total_cmp);
            let mut cuts: Vec<f64> = (1..bins).map(|b| quantile(&values, b, bins)).collect();
            cuts.dedup_by(|a, b| (*a - *b).abs() <= f64::EPSILON);
            cuts
        })
        .collect()
}

/// Grow one tree on `targets` over the samples in `index`.
struct Grower<'a> {
    rows: &'a [Row],
    targets: &'a [f64],
    cuts: &'a [Vec<f64>],
    params: BoostParams,
    nodes: Vec<Node>,
}

impl Grower<'_> {
    fn mean(&self, index: &[usize]) -> f64 {
        index.iter().map(|&i| self.targets[i]).sum::<f64>() / index.len().max(1) as f64
    }

    /// The best split of `index` as `(gain, feature, threshold)`, if any is allowed.
    fn best_split(&self, index: &[usize]) -> Option<(f64, usize, f64)> {
        let total: f64 = index.iter().map(|&i| self.targets[i]).sum();
        let n = index.len() as f64;
        let mut best: Option<(f64, usize, f64)> = None;
        for (feature, cuts) in self.cuts.iter().enumerate() {
            // Per bin: count and sum of targets (bin k: value <= cuts[k], last: above all).
            let mut count = vec![0usize; cuts.len() + 1];
            let mut sum = vec![0.0f64; cuts.len() + 1];
            for &i in index {
                let v = self.rows[i][feature];
                let bin = cuts.partition_point(|c| *c < v);
                count[bin] += 1;
                sum[bin] += self.targets[i];
            }
            let (mut left_n, mut left_sum) = (0usize, 0.0f64);
            for (k, threshold) in cuts.iter().enumerate() {
                left_n += count[k];
                left_sum += sum[k];
                let right_n = index.len() - left_n;
                if left_n < self.params.min_leaf || right_n < self.params.min_leaf {
                    continue;
                }
                let right_sum = total - left_sum;
                let gain = (left_sum * left_sum).mul_add(
                    1.0 / left_n as f64,
                    (right_sum * right_sum) / right_n as f64,
                ) - total * total / n;
                if gain > 1e-12 && best.is_none_or(|b| gain > b.0) {
                    best = Some((gain, feature, *threshold));
                }
            }
        }
        best
    }

    /// Grow the subtree for `index` at `depth`; returns its node index.
    fn grow(&mut self, index: &[usize], depth: usize) -> usize {
        let at = self.nodes.len();
        self.nodes.push(Node::Leaf(self.mean(index)));
        if depth >= self.params.depth {
            return at;
        }
        let Some((_, feature, threshold)) = self.best_split(index) else {
            return at;
        };
        let (left_idx, right_idx): (Vec<usize>, Vec<usize>) = index
            .iter()
            .partition(|&&i| self.rows[i][feature] <= threshold);
        let left = self.grow(&left_idx, depth + 1);
        let right = self.grow(&right_idx, depth + 1);
        self.nodes[at] = Node::Split {
            feature,
            threshold,
            left,
            right,
        };
        at
    }
}

impl Learner for Gbdt {
    fn name(&self) -> &'static str {
        "xs-boost"
    }

    fn fit(&mut self, samples: &[&(Row, f64)]) -> bool {
        self.trees.clear();
        let p = self.params;
        if samples.len() < p.min_leaf * 2 || p.trees == 0 {
            return false;
        }
        let rows: Vec<Row> = samples.iter().map(|s| s.0).collect();
        let mut labels: Vec<f64> = samples.iter().map(|s| s.1).collect();
        let mut sorted: Vec<f64> = labels.iter().copied().filter(|v| v.is_finite()).collect();
        sorted.sort_by(f64::total_cmp);
        let (low, high) = (quantile(&sorted, 1, 40), quantile(&sorted, 39, 40));
        for y in &mut labels {
            *y = if y.is_finite() {
                y.clamp(low, high)
            } else {
                0.0
            };
        }
        self.base = labels.iter().sum::<f64>() / labels.len() as f64;
        let cuts = thresholds(&rows, p.bins.max(2));
        let mut fitted = vec![self.base; labels.len()];
        let index: Vec<usize> = (0..labels.len()).collect();
        for _ in 0..p.trees {
            let residuals: Vec<f64> = labels.iter().zip(&fitted).map(|(y, f)| y - f).collect();
            let mut grower = Grower {
                rows: &rows,
                targets: &residuals,
                cuts: &cuts,
                params: p,
                nodes: Vec::new(),
            };
            grower.grow(&index, 0);
            let tree = Tree {
                nodes: grower.nodes,
            };
            for (f, row) in fitted.iter_mut().zip(&rows) {
                *f = p.learning_rate.mul_add(tree.predict(row), *f);
            }
            self.trees.push(tree);
        }
        true
    }

    fn predict(&self, row: &Row) -> f64 {
        self.trees.iter().fold(self.base, |acc, t| {
            self.params.learning_rate.mul_add(t.predict(row), acc)
        })
    }
}

/// The cross-sectional gradient-boosted strategy (`xs-boost`).
#[derive(Debug)]
pub struct XsBoost;

impl XsBoost {
    /// Hold the `top` instruments the trees rank highest, re-ranked every `rebalance`
    /// timestamps, trained on the last `window` labelled snapshots.
    pub fn strategy(
        params: BoostParams,
        top: usize,
        rebalance: usize,
        window: usize,
        sizing: VolTarget,
    ) -> XsLearner {
        XsLearner::new(Box::new(Gbdt::new(params)), top, rebalance, window, sizing)
    }
}

#[cfg(test)]
mod tests;
