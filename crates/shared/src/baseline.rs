pub const BASELINE_NEED: usize = 7;
pub const BASELINE_WINDOW: usize = 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Up,
    Down,
}

#[derive(Debug, Clone, PartialEq)]
pub enum BaselineVerdict {
    Learning {
        have: usize,
        need: usize,
    },
    Normal {
        z: f64,
        median: f64,
        mad: f64,
    },
    Outlier {
        z: f64,
        median: f64,
        mad: f64,
        direction: Direction,
    },
}

#[must_use]
pub fn median(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        return 0.0;
    }
    let mut v = xs.to_vec();
    v.sort_by(f64::total_cmp);
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        f64::midpoint(v[n / 2 - 1], v[n / 2])
    }
}

// MAD over the last 30 points, no seasonality; hour/weekday buckets would be the next step if
// weekly patterns cause false positives.
#[must_use]
pub fn baseline_verdict(history: &[f64], current: f64, z_threshold: f64) -> BaselineVerdict {
    if history.len() < BASELINE_NEED {
        return BaselineVerdict::Learning {
            have: history.len(),
            need: BASELINE_NEED,
        };
    }
    let window = &history[history.len().saturating_sub(BASELINE_WINDOW)..];
    let med = median(window);
    let devs: Vec<f64> = window.iter().map(|x| (x - med).abs()).collect();
    let mad = median(&devs);
    // A flat series has no spread; fall back to a relative scale so it can still flag large moves.
    let scale = if mad > 0.0 {
        mad
    } else if med != 0.0 {
        0.1 * med.abs()
    } else {
        1.0
    };
    let delta = current - med;
    let z = delta / (1.4826 * scale);
    if z.abs() >= z_threshold && delta.abs() >= 0.02 * med.abs() {
        let direction = if delta > 0.0 {
            Direction::Up
        } else {
            Direction::Down
        };
        BaselineVerdict::Outlier {
            z,
            median: med,
            mad,
            direction,
        }
    } else {
        BaselineVerdict::Normal {
            z,
            median: med,
            mad,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MEDIUM: f64 = 3.5;

    fn is_outlier(v: &BaselineVerdict) -> bool {
        matches!(v, BaselineVerdict::Outlier { .. })
    }

    #[test]
    fn median_odd_even_empty() {
        assert_eq!(median(&[3.0, 1.0, 2.0]), 2.0);
        assert_eq!(median(&[4.0, 1.0, 2.0, 3.0]), 2.5);
        assert_eq!(median(&[]), 0.0);
    }

    #[test]
    fn constant_series_small_change_normal_large_change_outlier() {
        let h = vec![10.0; 10];
        assert!(matches!(
            baseline_verdict(&h, 10.4, MEDIUM),
            BaselineVerdict::Normal { .. }
        ));
        assert!(!is_outlier(&baseline_verdict(&h, 15.0, MEDIUM))); // 50% < ~52%
        assert!(is_outlier(&baseline_verdict(&h, 16.0, MEDIUM)));
    }

    #[test]
    fn constant_series_thresholds_scale_with_sensitivity() {
        let h = vec![100.0; 10];
        // High 2.5 -> ~37%, Low 5.0 -> ~74%
        assert!(!is_outlier(&baseline_verdict(&h, 136.0, 2.5)));
        assert!(is_outlier(&baseline_verdict(&h, 138.0, 2.5)));
        assert!(!is_outlier(&baseline_verdict(&h, 173.0, 5.0)));
        assert!(is_outlier(&baseline_verdict(&h, 175.0, 5.0)));
    }

    #[test]
    fn slow_drift_is_normal() {
        let h: Vec<f64> = (0..20).map(|i| 100.0 + i as f64).collect();
        assert!(matches!(
            baseline_verdict(&h, 120.0, MEDIUM),
            BaselineVerdict::Normal { .. }
        ));
    }

    #[test]
    fn single_spike_outlier_up() {
        let h = [3.0, 3.2, 2.9, 3.1, 3.0, 3.3, 3.1, 2.8, 3.2, 3.0];
        match baseline_verdict(&h, 14.2, MEDIUM) {
            BaselineVerdict::Outlier { direction, z, .. } => {
                assert_eq!(direction, Direction::Up);
                assert!(z > MEDIUM);
            }
            v => panic!("{v:?}"),
        }
    }

    #[test]
    fn drop_outlier_down() {
        let h = [50.0, 52.0, 49.0, 51.0, 50.0, 53.0, 51.0, 48.0];
        match baseline_verdict(&h, 5.0, MEDIUM) {
            BaselineVerdict::Outlier { direction, z, .. } => {
                assert_eq!(direction, Direction::Down);
                assert!(z < 0.0);
            }
            v => panic!("{v:?}"),
        }
    }

    #[test]
    fn zero_mad_with_zero_median_uses_unit_scale() {
        let h = vec![0.0; 8];
        match baseline_verdict(&h, 6.0, MEDIUM) {
            BaselineVerdict::Outlier { z, median, mad, .. } => {
                assert!((z - 6.0 / 1.4826).abs() < 1e-9);
                assert_eq!((median, mad), (0.0, 0.0));
            }
            v => panic!("{v:?}"),
        }
        assert!(matches!(
            baseline_verdict(&h, 1.0, MEDIUM),
            BaselineVerdict::Normal { .. }
        ));
    }

    #[test]
    fn below_noise_floor_is_normal() {
        // Tight spread so |z| is huge, yet the change is under 2% of the median.
        let h = [1000.0, 1000.1, 999.9, 1000.0, 1000.1, 999.9, 1000.0, 1000.0];
        assert!(matches!(
            baseline_verdict(&h, 1010.0, MEDIUM),
            BaselineVerdict::Normal { .. }
        ));
        assert!(is_outlier(&baseline_verdict(&h, 1030.0, MEDIUM)));
    }

    #[test]
    fn learning_until_enough_points() {
        let h = [1.0; 6];
        assert_eq!(
            baseline_verdict(&h, 1.0, MEDIUM),
            BaselineVerdict::Learning { have: 6, need: 7 }
        );
        assert!(!matches!(
            baseline_verdict(&[1.0; 7], 1.0, MEDIUM),
            BaselineVerdict::Learning { .. }
        ));
    }

    #[test]
    fn window_uses_last_thirty_only() {
        let mut h = vec![1000.0; 20];
        h.extend(vec![10.0; 30]);
        match baseline_verdict(&h, 10.0, MEDIUM) {
            BaselineVerdict::Normal { median, .. } => assert_eq!(median, 10.0),
            v => panic!("{v:?}"),
        }
    }
}
