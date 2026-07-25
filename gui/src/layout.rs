//! 1-D label placement: given each channel's ideal vertical position (from its
//! frequency), assign non-overlapping row positions that stay as close to the
//! ideals as possible. Neighbours are pushed apart; the leader lines (drawn
//! elsewhere) bridge the residual offset back to the true frequency.
//!
//! Uses cluster merging: adjacent labels that would overlap are grouped and the
//! group is centered on the mean of its members' ideal positions, so an isolated
//! label keeps its ideal spot and only crowded ones move.

/// Assign a `y` (row center) for each input, given each item's `ideal` center,
/// a per-row `line_h`, and the usable vertical range `[top, bottom]`.
/// Returns `y` in the original input order.
pub fn place(ideals: &[f32], line_h: f32, top: f32, bottom: f32) -> Vec<f32> {
    let n = ideals.len();
    if n == 0 {
        return Vec::new();
    }

    // sort item indices by ideal position
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|&a, &b| ideals[a].partial_cmp(&ideals[b]).unwrap());

    // Each cluster: sum of member ideals, member count, and the sorted member
    // item-indices. `top` of a cluster = mean_ideal - count*line_h/2.
    struct Cluster {
        sum: f32,
        members: Vec<usize>,
    }
    impl Cluster {
        fn count(&self) -> f32 {
            self.members.len() as f32
        }
        fn top(&self, line_h: f32) -> f32 {
            self.sum / self.count() - self.count() * line_h / 2.0
        }
        fn bottom(&self, line_h: f32) -> f32 {
            self.top(line_h) + self.count() * line_h
        }
    }

    let mut stack: Vec<Cluster> = Vec::new();
    for &idx in &order {
        let mut cur = Cluster { sum: ideals[idx], members: vec![idx] };
        // merge while the previous cluster would overlap this one
        while let Some(prev) = stack.last() {
            if prev.bottom(line_h) > cur.top(line_h) {
                let mut prev = stack.pop().unwrap();
                prev.sum += cur.sum;
                prev.members.extend(cur.members.drain(..));
                cur = prev;
            } else {
                break;
            }
        }
        stack.push(cur);
    }

    // Shift everything to fit within [top, bottom] (best effort).
    let first_top = stack.first().unwrap().top(line_h);
    let last_bottom = stack.last().unwrap().bottom(line_h);
    let mut shift = 0.0;
    if first_top + shift < top {
        shift = top - first_top;
    }
    if last_bottom + shift > bottom {
        shift = (bottom - last_bottom).min(shift.max(top - first_top));
    }

    let mut y = vec![0.0f32; n];
    for cluster in &stack {
        let t = cluster.top(line_h) + shift;
        for (k, &idx) in cluster.members.iter().enumerate() {
            y[idx] = t + k as f32 * line_h + line_h / 2.0;
        }
    }
    y
}

#[cfg(test)]
mod tests {
    use super::*;

    fn min_gap(ys: &mut Vec<f32>) -> f32 {
        ys.sort_by(|a, b| a.partial_cmp(b).unwrap());
        ys.windows(2).map(|w| w[1] - w[0]).fold(f32::MAX, f32::min)
    }

    #[test]
    fn isolated_labels_keep_their_ideal() {
        let y = place(&[100.0, 400.0, 700.0], 14.0, 0.0, 1000.0);
        assert_eq!(y, vec![100.0, 400.0, 700.0]);
    }

    #[test]
    fn crowded_labels_are_spread_without_overlap_and_ordered() {
        let ideals = [500.0, 505.0, 508.0]; // within one line height
        let y = place(&ideals, 14.0, 0.0, 1000.0);
        // order preserved
        assert!(y[0] < y[1] && y[1] < y[2]);
        // no overlap
        let mut yy = y.clone();
        assert!(min_gap(&mut yy) >= 14.0 - 1e-3);
        // centered near the mean of ideals (~504.3)
        let center = (y[0] + y[2]) / 2.0;
        assert!((center - 504.33).abs() < 1.0);
    }

    #[test]
    fn respects_top_bound() {
        let y = place(&[2.0, 6.0], 14.0, 0.0, 1000.0);
        assert!(y[0] >= 14.0 / 2.0 - 1e-3, "first row must fit below top");
    }
}
