use crate::baseline::{median, BASELINE_WINDOW};
use std::fmt::Write;

#[derive(Debug, Clone, Copy)]
pub enum ExplainKind<'a> {
    Threshold { op: &'a str, value: f64 },
    Baseline { median: f64, z: f64 },
}

fn num(x: f64) -> String {
    let s = format!("{x:.2}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s == "-0" {
        "0".into()
    } else {
        s.into()
    }
}

pub fn stats_line(history: &[f64], current: f64, kind: ExplainKind<'_>) -> String {
    let window = &history[history.len().saturating_sub(BASELINE_WINDOW)..];
    match kind {
        ExplainKind::Baseline { median: med, z } => {
            let verb = if current < med { "Dropped" } else { "Jumped" };
            let tail = &window[window.len().saturating_sub(2)..];
            let last: Vec<String> = tail.iter().copied().chain([current]).map(num).collect();
            format!(
                "{verb} to {} from a {}-check median of {} (z {}). Last {} {}: {}.",
                num(current),
                window.len(),
                num(med),
                num(z),
                last.len(),
                if last.len() == 1 { "check" } else { "checks" },
                last.join(", ")
            )
        }
        ExplainKind::Threshold { op, value } => {
            let mut s = format!("Crossed {op} {}: {}", num(value), num(current));
            if let Some(prev) = window.last() {
                let _ = write!(
                    s,
                    ", previous {}, {}-check median {}",
                    num(*prev),
                    window.len(),
                    num(median(window))
                );
            }
            s.push('.');
            s
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn baseline_jump() {
        let h = [3.1, 3.0, 3.2];
        let s = stats_line(
            &h,
            14.2,
            ExplainKind::Baseline {
                median: 3.1,
                z: 4.7,
            },
        );
        assert_eq!(
            s,
            "Jumped to 14.2 from a 3-check median of 3.1 (z 4.7). Last 3 checks: 3, 3.2, 14.2."
        );
    }

    #[test]
    fn baseline_window_capped_at_thirty() {
        let h = vec![3.0; 45];
        let s = stats_line(
            &h,
            14.2,
            ExplainKind::Baseline {
                median: 3.0,
                z: 4.7,
            },
        );
        assert!(s.contains("30-check median"), "{s}");
    }

    #[test]
    fn baseline_drop() {
        let s = stats_line(
            &[50.0; 8],
            5.0,
            ExplainKind::Baseline {
                median: 50.0,
                z: -9.0,
            },
        );
        assert!(s.starts_with("Dropped to 5 from"), "{s}");
    }

    #[test]
    fn baseline_empty_history_lists_current_only() {
        let s = stats_line(
            &[],
            2.0,
            ExplainKind::Baseline {
                median: 1.0,
                z: 3.0,
            },
        );
        assert!(s.ends_with("Last 1 check: 2."), "{s}");
    }

    #[test]
    fn threshold_with_history() {
        let h = [3.1, 3.0, 3.2];
        let s = stats_line(
            &h,
            14.2,
            ExplainKind::Threshold {
                op: ">",
                value: 10.0,
            },
        );
        assert_eq!(s, "Crossed > 10: 14.2, previous 3.2, 3-check median 3.1.");
    }

    #[test]
    fn threshold_empty_history() {
        let s = stats_line(
            &[],
            14.2,
            ExplainKind::Threshold {
                op: ">",
                value: 10.0,
            },
        );
        assert_eq!(s, "Crossed > 10: 14.2.");
    }

    #[test]
    fn number_format_trims() {
        assert_eq!(num(3.0), "3");
        assert_eq!(num(1.23456), "1.23");
        assert_eq!(num(2.50), "2.5");
        assert_eq!(num(-0.001), "0");
    }
}
