use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, clap::ValueEnum)]
pub enum MeasurementMethod {
    /// Alternate full-screen black/white state and detect by luma threshold.
    LumaStep,
    /// Use a 2x2 coded pattern that changes every transition for more robust matching.
    QuadCode,
}

#[cfg_attr(feature = "python", pyo3::pyclass)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LatencySample {
    pub transition_id: u64,
    pub display_ms: f64,
    pub camera_ms: f64,
    pub latency_ms: f64,
}

#[cfg_attr(feature = "python", pyo3::pyclass)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SummaryStats {
    pub count: usize,
    pub mean_ms: f64,
    pub median_ms: f64,
    pub p95_ms: f64,
    pub min_ms: f64,
    pub max_ms: f64,
    pub stddev_ms: f64,
}

#[cfg_attr(feature = "python", pyo3::pyclass)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LatencyReport {
    pub samples: Vec<LatencySample>,
    pub stats: Option<SummaryStats>,
    pub dropped_display_events: usize,
    pub dropped_camera_events: usize,
}

#[derive(Debug, Clone)]
pub struct MeasurementRun {
    pub display_events_ms: Vec<(u64, f64)>,
    pub camera_events_ms: Vec<(u64, f64)>,
}

pub fn build_report(run: &MeasurementRun) -> LatencyReport {
    let mut samples = Vec::new();
    let mut cam_idx = 0usize;
    let mut dropped_display_events = 0usize;

    for (id, display_ms) in &run.display_events_ms {
        while cam_idx < run.camera_events_ms.len() && run.camera_events_ms[cam_idx].0 < *id {
            cam_idx += 1;
        }

        if cam_idx >= run.camera_events_ms.len() {
            dropped_display_events += 1;
            continue;
        }

        let (cam_id, cam_ms) = run.camera_events_ms[cam_idx];
        if cam_id == *id {
            samples.push(LatencySample {
                transition_id: *id,
                display_ms: *display_ms,
                camera_ms: cam_ms,
                latency_ms: cam_ms - *display_ms,
            });
            cam_idx += 1;
        } else {
            dropped_display_events += 1;
        }
    }

    let dropped_camera_events = run.camera_events_ms.len().saturating_sub(samples.len());
    let stats = summarize(&samples);
    LatencyReport {
        samples,
        stats,
        dropped_display_events,
        dropped_camera_events,
    }
}

pub fn build_report_from_ms(display_ms: Vec<f64>, camera_ms: Vec<f64>) -> LatencyReport {
    let display_events_ms = display_ms
        .into_iter()
        .enumerate()
        .map(|(i, ms)| (i as u64, ms))
        .collect();
    let camera_events_ms = camera_ms
        .into_iter()
        .enumerate()
        .map(|(i, ms)| (i as u64, ms))
        .collect();
    build_report(&MeasurementRun {
        display_events_ms,
        camera_events_ms,
    })
}

pub fn summarize(samples: &[LatencySample]) -> Option<SummaryStats> {
    if samples.is_empty() {
        return None;
    }

    let mut latencies: Vec<f64> = samples.iter().map(|s| s.latency_ms).collect();
    latencies.sort_by(f64::total_cmp);
    let count = latencies.len();
    let mean_ms = latencies.iter().sum::<f64>() / count as f64;
    let median_ms = percentile(&latencies, 0.5);
    let p95_ms = percentile(&latencies, 0.95);
    let min_ms = latencies[0];
    let max_ms = latencies[count - 1];
    let variance = latencies
        .iter()
        .map(|v| {
            let d = *v - mean_ms;
            d * d
        })
        .sum::<f64>()
        / count as f64;

    Some(SummaryStats {
        count,
        mean_ms,
        median_ms,
        p95_ms,
        min_ms,
        max_ms,
        stddev_ms: variance.sqrt(),
    })
}

fn percentile(sorted: &[f64], pct: f64) -> f64 {
    if sorted.len() == 1 {
        return sorted[0];
    }
    let rank = pct.clamp(0.0, 1.0) * (sorted.len() - 1) as f64;
    let lo = rank.floor() as usize;
    let hi = rank.ceil() as usize;
    if lo == hi {
        return sorted[lo];
    }
    let w = rank - lo as f64;
    sorted[lo] * (1.0 - w) + sorted[hi] * w
}

#[cfg(test)]
mod tests {
    use super::*;
    use approx::assert_relative_eq;

    #[test]
    fn aligns_transition_ids() {
        let run = MeasurementRun {
            display_events_ms: vec![(0, 10.0), (1, 20.0), (2, 30.0)],
            camera_events_ms: vec![(0, 40.0), (2, 65.0)],
        };

        let report = build_report(&run);
        assert_eq!(report.samples.len(), 2);
        assert_eq!(report.dropped_display_events, 1);
        assert_relative_eq!(report.samples[0].latency_ms, 30.0);
        assert_relative_eq!(report.samples[1].latency_ms, 35.0);
    }
}
