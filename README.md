# camera-latency-test

A Rust tool to estimate RGB camera latency by displaying a fast-changing on-screen stimulus and correlating camera-observed transitions with display transition timestamps.

## Features

- Rust implementation with static, explicit data flow.
- GUI stimulus window (egui/eframe) designed for high-frequency visual transitions.
- Two measurement methods:
  - `luma-step`: alternating dark/light transitions.
  - `quad-code`: 2x2 coded quadrants using 4 grayscale nibbles (16-bit cyclic code space) for robust code-distance latency.
- Shared timing epoch for stimulus and camera event timestamps.
- Millisecond-level latency report with summary stats.
- CSV and JSON export.
- JSON stdout mode for piping into scripts.
- Python-callable API (feature-gated via `python`).

## Quick start

```bash
cargo run --release -- \
  --camera-index 0 \
  --stimulus-hz 30 \
  --duration-s 10 \
  --method quad-code \
  --luma-threshold 128 \
  --mode offline \
  --offline-runs 5 \
  --csv-out samples.csv \
  --json-out report.json \
  --json-stdout
```

## CLI options

- `--camera-index <N>` camera device index.
- `--stimulus-hz <HZ>` transition frequency.
- `--duration-s <SECONDS>` test duration.
- `--method <luma-step|quad-code>` detection strategy.
- `--luma-threshold <VALUE>` threshold for dark/light detection.
- `--state-space <N>` cyclic state space size for `quad-code` (default 65536; should be >=50000 for long-delay robustness).
- `--max-forward-jump-ms <MS>` reject decoded forward code jumps larger than this bound.
- `--warmup-s <SECONDS>` warmup period before latency sampling starts.
- `--mode <online|offline>` selects realtime or batch flow.
- `--offline-runs <N>` number of runs when in offline mode.
- `--csv-out <PATH>` write per-sample CSV.
- `--json-out <PATH>` write full JSON report.
- `--json-stdout` print report JSON to stdout.

## Python interoperability

Build Python extension module:

```bash
cargo build --release --features python
```

Exposed API:

- `compute_latency_samples(display_ms: list[float], camera_ms: list[float]) -> LatencyReport`

## CI artifacts

GitHub Actions on `main` builds release binaries and publishes artifacts for:

- Linux amd64 (`x86_64-unknown-linux-gnu`)
- macOS Apple Silicon (`aarch64-apple-darwin`)

## Modes

- **online**: continuous measurement with realtime latency stats shown in the GUI.
- **offline**: runs multiple fixed-duration measurements and reports aggregated results at the end.

## Measurement notes

- Latency is computed from code-distance between the currently displayed code and the latest decoded camera code, not wallclock correlation.
- Torn-frame candidates are filtered out when top/bottom ROI decodes disagree; the UI and JSON output include torn-frame ratio.
- Repeated frames are ignored, backward jumps are discarded, and forward jumps beyond the configured threshold are rejected.

## Notes

- Auto-exposure/auto-white-balance can increase jitter and false detections; disable when possible.
- Target camera FPS up to 120 is supported conceptually; actual results depend on hardware and driver behavior.
- Cross-platform runtime behavior depends on camera backend support from `nokhwa`.
