pub mod analysis;
pub mod stimulus;

pub use analysis::{LatencyReport, LatencySample, MeasurementMethod, MeasurementRun, SummaryStats};

#[cfg(feature = "python")]
use pyo3::prelude::*;

#[cfg(feature = "python")]
#[pyfunction]
fn compute_latency_samples(
    display_ms: Vec<f64>,
    camera_ms: Vec<f64>,
) -> PyResult<analysis::LatencyReport> {
    Ok(analysis::build_report_from_ms(display_ms, camera_ms))
}

#[cfg(feature = "python")]
#[pymodule]
fn camera_latency_test(_py: Python<'_>, m: &PyModule) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(compute_latency_samples, m)?)?;
    m.add_class::<analysis::LatencySample>()?;
    m.add_class::<analysis::SummaryStats>()?;
    m.add_class::<analysis::LatencyReport>()?;
    Ok(())
}
