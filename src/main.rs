use std::fs::File;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use camera_latency_test::analysis::{
    build_report, LatencyReport, MeasurementMethod, MeasurementRun, SummaryStats,
};
use camera_latency_test::stimulus::{decode_quad_code, state_for};
use clap::Parser;
use crossbeam_channel::{unbounded, Receiver, Sender};
use eframe::egui;
use nokhwa::pixel_format::RgbFormat;
use nokhwa::utils::{CameraIndex, RequestedFormat, RequestedFormatType};
use nokhwa::Camera;
use serde::Serialize;

#[derive(clap::ValueEnum, Debug, Clone, Copy, Serialize)]
enum RunMode {
    /// Continuous run with rolling latency shown in real time.
    Online,
    /// Execute multiple fixed-duration runs and report results at the end.
    Offline,
}

#[derive(Parser, Debug, Clone)]
#[command(author, version, about)]
struct Cli {
    /// Camera index to open.
    #[arg(long, default_value_t = 0)]
    camera_index: u32,
    /// Transition frequency in Hz.
    #[arg(long, default_value_t = 30.0)]
    stimulus_hz: f64,
    /// Run duration in seconds.
    #[arg(long, default_value_t = 10.0)]
    duration_s: f64,
    /// Detection method.
    #[arg(long, value_enum, default_value_t = MeasurementMethod::QuadCode)]
    method: MeasurementMethod,
    /// Luma threshold used for transition decoding.
    #[arg(long, default_value_t = 128.0)]
    luma_threshold: f32,
    /// Run mode.
    #[arg(long, value_enum, default_value_t = RunMode::Offline)]
    mode: RunMode,
    /// Number of runs when mode is offline.
    #[arg(long, default_value_t = 5)]
    offline_runs: u32,
    /// Optional CSV output path.
    #[arg(long)]
    csv_out: Option<PathBuf>,
    /// Optional JSON output path.
    #[arg(long)]
    json_out: Option<PathBuf>,
    /// Print report as JSON to stdout.
    #[arg(long, default_value_t = false)]
    json_stdout: bool,
}

#[derive(Debug, Clone, Copy)]
struct DisplayEvent {
    run: u32,
    id: u64,
    ms: f64,
}

#[derive(Debug, Clone, Copy)]
struct CameraEvent {
    run: u32,
    id: u64,
    ms: f64,
}

#[derive(Clone)]
struct SharedState {
    epoch: Instant,
    current_transition_id: u64,
    current_run: u32,
    capture_active: bool,
    display_events: Vec<DisplayEvent>,
    camera_events: Vec<CameraEvent>,
    run_started: bool,
    run_finished: bool,
}

impl SharedState {
    fn new() -> Self {
        Self {
            epoch: Instant::now(),
            current_transition_id: 0,
            current_run: 0,
            capture_active: false,
            display_events: Vec::new(),
            camera_events: Vec::new(),
            run_started: false,
            run_finished: false,
        }
    }
}

#[derive(Serialize)]
struct ResultEnvelope {
    mode: RunMode,
    offline_runs: u32,
    report: LatencyReport,
    per_run_stats: Vec<Option<SummaryStats>>,
    notice: String,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let state = Arc::new(Mutex::new(SharedState::new()));

    let (stop_tx, stop_rx) = unbounded();
    let camera_state = Arc::clone(&state);
    let cli_for_camera = cli.clone();

    let camera_thread = thread::spawn(move || {
        if let Err(err) = camera_capture_loop(cli_for_camera, camera_state, stop_rx) {
            eprintln!("camera loop error: {err:#}");
        }
    });

    let native_options = eframe::NativeOptions::default();
    let gui_state = Arc::clone(&state);
    let cli_for_gui = cli.clone();
    let stop_tx_for_app = stop_tx.clone();

    eframe::run_native(
        "Camera Latency Test",
        native_options,
        Box::new(move |_cc| {
            Box::new(App::new(
                cli_for_gui.clone(),
                Arc::clone(&gui_state),
                stop_tx_for_app.clone(),
            ))
        }),
    )
    .map_err(|e| anyhow::anyhow!("failed to run GUI: {e}"))?;

    let _ = stop_tx.send(());
    let _ = camera_thread.join();

    let locked = state.lock().expect("state lock poisoned").clone();
    let (report, per_run_stats) = build_mode_report(&cli, &locked);
    persist_results(&cli, &report, &per_run_stats)?;

    Ok(())
}

fn build_mode_report(cli: &Cli, state: &SharedState) -> (LatencyReport, Vec<Option<SummaryStats>>) {
    match cli.mode {
        RunMode::Online => {
            let report = build_report_for_run(state, 0);
            let stats = vec![report.stats.clone()];
            (report, stats)
        }
        RunMode::Offline => {
            let runs = cli.offline_runs.max(1);
            let mut per_run_stats = Vec::with_capacity(runs as usize);
            let mut samples = Vec::new();
            let mut dropped_display_events = 0usize;
            let mut dropped_camera_events = 0usize;

            for run in 0..runs {
                let report = build_report_for_run(state, run);
                per_run_stats.push(report.stats.clone());
                dropped_display_events += report.dropped_display_events;
                dropped_camera_events += report.dropped_camera_events;
                samples.extend(report.samples);
            }

            let stats = camera_latency_test::analysis::summarize(&samples);
            (
                LatencyReport {
                    samples,
                    stats,
                    dropped_display_events,
                    dropped_camera_events,
                },
                per_run_stats,
            )
        }
    }
}

fn build_report_for_run(state: &SharedState, run: u32) -> LatencyReport {
    let run = MeasurementRun {
        display_events_ms: state
            .display_events
            .iter()
            .filter(|e| e.run == run)
            .map(|e| (e.id, e.ms))
            .collect(),
        camera_events_ms: state
            .camera_events
            .iter()
            .filter(|e| e.run == run)
            .map(|e| (e.id, e.ms))
            .collect(),
    };
    build_report(&run)
}

fn persist_results(
    cli: &Cli,
    report: &LatencyReport,
    per_run_stats: &[Option<SummaryStats>],
) -> Result<()> {
    if let Some(path) = &cli.csv_out {
        let mut wtr = csv::Writer::from_path(path)?;
        for sample in &report.samples {
            wtr.serialize(sample)?;
        }
        wtr.flush()?;
    }

    if let Some(path) = &cli.json_out {
        let file = File::create(path)?;
        let envelope = ResultEnvelope {
            mode: cli.mode,
            offline_runs: cli.offline_runs.max(1),
            report: report.clone(),
            per_run_stats: per_run_stats.to_vec(),
            notice: "Auto-exposure can destabilize edge detection. Disable AE if possible.".into(),
        };
        serde_json::to_writer_pretty(file, &envelope)?;
    }

    if cli.json_stdout {
        let envelope = ResultEnvelope {
            mode: cli.mode,
            offline_runs: cli.offline_runs.max(1),
            report: report.clone(),
            per_run_stats: per_run_stats.to_vec(),
            notice: "Auto-exposure can destabilize edge detection. Disable AE if possible.".into(),
        };
        println!("{}", serde_json::to_string_pretty(&envelope)?);
    }

    Ok(())
}

fn camera_capture_loop(
    cli: Cli,
    state: Arc<Mutex<SharedState>>,
    stop_rx: Receiver<()>,
) -> Result<()> {
    let mut camera = Camera::new(
        CameraIndex::Index(cli.camera_index),
        RequestedFormat::new::<RgbFormat>(RequestedFormatType::AbsoluteHighestFrameRate),
    )?;
    camera.open_stream()?;

    let mut last_seen_run: Option<u32> = None;
    let mut last_seen_id: Option<u64> = None;

    loop {
        if stop_rx.try_recv().is_ok() {
            break;
        }

        let frame = match camera.frame() {
            Ok(frame) => frame,
            Err(_) => continue,
        };

        let image = match frame.decode_image::<RgbFormat>() {
            Ok(img) => img,
            Err(_) => continue,
        };

        let (capture_active, run_started, run_finished, current_run) = {
            let guard = state.lock().expect("state lock poisoned");
            (
                guard.capture_active,
                guard.run_started,
                guard.run_finished,
                guard.current_run,
            )
        };

        if !capture_active || !run_started || run_finished {
            continue;
        }

        if last_seen_run != Some(current_run) {
            last_seen_run = Some(current_run);
            last_seen_id = None;
        }

        let id = detect_transition_id(
            cli.method,
            cli.luma_threshold,
            image.as_raw(),
            image.width() as usize,
            image.height() as usize,
            last_seen_id,
        );

        if let Some(id) = id {
            let mut guard = state.lock().expect("state lock poisoned");
            if !guard.capture_active || guard.current_run != current_run || guard.run_finished {
                continue;
            }
            if guard.camera_events.last().map(|e| (e.run, e.id)) != Some((current_run, id)) {
                let ms = guard.epoch.elapsed().as_secs_f64() * 1000.0;
                guard.camera_events.push(CameraEvent {
                    run: current_run,
                    id,
                    ms,
                });
                last_seen_id = Some(id);
            }
        }
    }

    Ok(())
}

fn detect_transition_id(
    method: MeasurementMethod,
    luma_threshold: f32,
    rgb: &[u8],
    width: usize,
    height: usize,
    previous_id: Option<u64>,
) -> Option<u64> {
    if width < 4 || height < 4 {
        return None;
    }

    let sample_quad = |qx: usize, qy: usize| -> f32 {
        let x = if qx == 0 { width / 4 } else { (3 * width) / 4 };
        let y = if qy == 0 {
            height / 4
        } else {
            (3 * height) / 4
        };
        let idx = (y * width + x) * 3;
        if idx + 2 >= rgb.len() {
            return 0.0;
        }
        let r = rgb[idx] as f32;
        let g = rgb[idx + 1] as f32;
        let b = rgb[idx + 2] as f32;
        0.2126 * r + 0.7152 * g + 0.0722 * b
    };

    match method {
        MeasurementMethod::LumaStep => {
            let luma = sample_quad(0, 0);
            let bit = if luma > luma_threshold { 1 } else { 0 };
            match previous_id {
                None => Some(bit),
                Some(prev) if prev % 2 == bit => None,
                Some(prev) => Some(prev + 1),
            }
        }
        MeasurementMethod::QuadCode => {
            let lumas = [
                sample_quad(0, 0),
                sample_quad(1, 0),
                sample_quad(0, 1),
                sample_quad(1, 1),
            ];
            decode_quad_code(lumas, luma_threshold, previous_id)
        }
    }
}

struct App {
    cli: Cli,
    state: Arc<Mutex<SharedState>>,
    stop_tx: Sender<()>,
    last_tick: Instant,
    period: Duration,
    run_started_at: Instant,
    pause_until: Option<Instant>,
    done_sent: bool,
}

impl App {
    fn new(cli: Cli, state: Arc<Mutex<SharedState>>, stop_tx: Sender<()>) -> Self {
        let hz = cli.stimulus_hz.max(1.0);
        Self {
            cli,
            state,
            stop_tx,
            last_tick: Instant::now(),
            period: Duration::from_secs_f64(1.0 / hz),
            run_started_at: Instant::now(),
            pause_until: None,
            done_sent: false,
        }
    }

    fn rolling_online_report(&self) -> LatencyReport {
        let locked = self.state.lock().expect("state lock poisoned").clone();
        build_report_for_run(&locked, 0)
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        {
            let mut guard = self.state.lock().expect("state lock poisoned");
            if !guard.run_started {
                guard.run_started = true;
                guard.capture_active = true;
                guard.epoch = Instant::now();
                self.run_started_at = Instant::now();
            }
        }

        let is_offline = matches!(self.cli.mode, RunMode::Offline);
        if is_offline {
            if let Some(until) = self.pause_until {
                if Instant::now() >= until {
                    let mut guard = self.state.lock().expect("state lock poisoned");
                    guard.current_run += 1;
                    guard.current_transition_id = 0;
                    guard.capture_active = true;
                    self.run_started_at = Instant::now();
                    self.last_tick = Instant::now();
                    self.pause_until = None;
                }
            } else {
                let elapsed = self.run_started_at.elapsed().as_secs_f64();
                if elapsed >= self.cli.duration_s {
                    let mut guard = self.state.lock().expect("state lock poisoned");
                    guard.capture_active = false;
                    if guard.current_run + 1 >= self.cli.offline_runs.max(1) {
                        guard.run_finished = true;
                        if !self.done_sent {
                            let _ = self.stop_tx.send(());
                            self.done_sent = true;
                        }
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    } else {
                        self.pause_until = Some(Instant::now() + Duration::from_millis(800));
                    }
                }
            }
        }

        {
            let mut guard = self.state.lock().expect("state lock poisoned");
            if guard.capture_active
                && !guard.run_finished
                && self.last_tick.elapsed() >= self.period
            {
                let id = guard.current_transition_id;
                let ms = guard.epoch.elapsed().as_secs_f64() * 1000.0;
                let run = guard.current_run;
                guard.display_events.push(DisplayEvent { run, id, ms });
                guard.current_transition_id += 1;
                self.last_tick = Instant::now();
            }
        }

        let (current_run, current_id, capture_active) = {
            let guard = self.state.lock().expect("state lock poisoned");
            (
                guard.current_run,
                guard.current_transition_id,
                guard.capture_active,
            )
        };
        let stimulus = state_for(self.cli.method, current_id);

        egui::CentralPanel::default().show(ctx, |ui| {
            let rect = ui.max_rect();
            let painter = ui.painter();

            let [c0, c1, c2, c3] = stimulus.colors;
            let half_w = rect.width() / 2.0;
            let half_h = rect.height() / 2.0;
            let x = rect.left();
            let y = rect.top();

            painter.rect_filled(
                egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2(half_w, half_h)),
                0.0,
                egui::Color32::from_rgb(c0[0], c0[1], c0[2]),
            );
            painter.rect_filled(
                egui::Rect::from_min_size(egui::pos2(x + half_w, y), egui::vec2(half_w, half_h)),
                0.0,
                egui::Color32::from_rgb(c1[0], c1[1], c1[2]),
            );
            painter.rect_filled(
                egui::Rect::from_min_size(egui::pos2(x, y + half_h), egui::vec2(half_w, half_h)),
                0.0,
                egui::Color32::from_rgb(c2[0], c2[1], c2[2]),
            );
            painter.rect_filled(
                egui::Rect::from_min_size(
                    egui::pos2(x + half_w, y + half_h),
                    egui::vec2(half_w, half_h),
                ),
                0.0,
                egui::Color32::from_rgb(c3[0], c3[1], c3[2]),
            );

            let mut text = format!(
                "Mode: {:?} | Method: {:?}\nRun {} | Transition #{}",
                self.cli.mode,
                self.cli.method,
                current_run + 1,
                stimulus.transition_id
            );

            if matches!(self.cli.mode, RunMode::Online) {
                let report = self.rolling_online_report();
                if let Some(stats) = report.stats {
                    text.push_str(&format!(
                        "\nRealtime latency (ms): mean {:.2}, p95 {:.2}, n={}",
                        stats.mean_ms, stats.p95_ms, stats.count
                    ));
                } else {
                    text.push_str("\nRealtime latency: waiting for detections...");
                }
            } else if !capture_active {
                text.push_str("\nSettling between runs...");
            }

            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                text,
                egui::TextStyle::Heading.resolve(ui.style()),
                egui::Color32::RED,
            );
        });

        ctx.request_repaint();
    }
}
