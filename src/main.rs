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
    Online,
    Offline,
}

#[derive(Parser, Debug, Clone)]
#[command(author, version, about)]
struct Cli {
    #[arg(long, default_value_t = 0)]
    camera_index: u32,
    #[arg(long, default_value_t = 30.0)]
    stimulus_hz: f64,
    #[arg(long, default_value_t = 10.0)]
    duration_s: f64,
    #[arg(long, value_enum, default_value_t = MeasurementMethod::QuadCode)]
    method: MeasurementMethod,
    #[arg(long, default_value_t = 128.0)]
    luma_threshold: f32,
    #[arg(long, value_enum, default_value_t = RunMode::Offline)]
    mode: RunMode,
    #[arg(long, default_value_t = 5)]
    offline_runs: u32,
    #[arg(long)]
    csv_out: Option<PathBuf>,
    #[arg(long)]
    json_out: Option<PathBuf>,
    #[arg(long, default_value_t = false)]
    json_stdout: bool,
}

#[derive(Debug, Clone, Copy)]
struct RoiNorm {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

impl RoiNorm {
    fn clamp(self) -> Self {
        let x = self.x.clamp(0.0, 1.0);
        let y = self.y.clamp(0.0, 1.0);
        let max_w = (1.0 - x).max(0.0);
        let max_h = (1.0 - y).max(0.0);
        Self {
            x,
            y,
            w: self.w.clamp(0.01, max_w),
            h: self.h.clamp(0.01, max_h),
        }
    }
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
struct PreviewFrame {
    width: usize,
    height: usize,
    rgb: Vec<u8>,
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
    latest_preview: Option<PreviewFrame>,
    detection_roi: Option<RoiNorm>,
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
            latest_preview: None,
            detection_roi: None,
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
    let camera_cli = cli.clone();
    let camera_thread = thread::spawn(move || {
        if let Err(err) = camera_capture_loop(camera_cli, camera_state, stop_rx) {
            eprintln!("camera loop error: {err:#}");
        }
    });

    let gui_state = Arc::clone(&state);
    let gui_cli = cli.clone();
    let gui_stop_tx = stop_tx.clone();
    eframe::run_native(
        "Camera Latency Test",
        eframe::NativeOptions::default(),
        Box::new(move |_cc| {
            Box::new(App::new(
                gui_cli.clone(),
                Arc::clone(&gui_state),
                gui_stop_tx.clone(),
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
            (report.clone(), vec![report.stats])
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

            (
                LatencyReport {
                    stats: camera_latency_test::analysis::summarize(&samples),
                    samples,
                    dropped_display_events,
                    dropped_camera_events,
                },
                per_run_stats,
            )
        }
    }
}

fn build_report_for_run(state: &SharedState, run: u32) -> LatencyReport {
    let measurement_run = MeasurementRun {
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
    build_report(&measurement_run)
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

    let envelope = ResultEnvelope {
        mode: cli.mode,
        offline_runs: cli.offline_runs.max(1),
        report: report.clone(),
        per_run_stats: per_run_stats.to_vec(),
        notice: "Select ROI in camera view so the detector only tracks the displayed pattern. Disable AE if possible.".into(),
    };

    if let Some(path) = &cli.json_out {
        let file = File::create(path)?;
        serde_json::to_writer_pretty(file, &envelope)?;
    }
    if cli.json_stdout {
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

        let full_w = image.width() as usize;
        let full_h = image.height() as usize;
        let raw = image.as_raw();

        let (capture_active, run_started, run_finished, current_run, roi) = {
            let mut guard = state.lock().expect("state lock poisoned");
            guard.latest_preview = Some(build_preview(raw, full_w, full_h, 640));
            (
                guard.capture_active,
                guard.run_started,
                guard.run_finished,
                guard.current_run,
                guard.detection_roi,
            )
        };

        if !capture_active || !run_started || run_finished {
            continue;
        }

        if last_seen_run != Some(current_run) {
            last_seen_run = Some(current_run);
            last_seen_id = None;
        }

        let Some(roi) = roi else {
            continue;
        };

        let id = detect_transition_id(
            cli.method,
            cli.luma_threshold,
            raw,
            full_w,
            full_h,
            roi,
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

fn build_preview(raw: &[u8], width: usize, height: usize, max_w: usize) -> PreviewFrame {
    if width <= max_w {
        return PreviewFrame {
            width,
            height,
            rgb: raw.to_vec(),
        };
    }

    let scale = max_w as f32 / width as f32;
    let out_w = max_w;
    let out_h = ((height as f32 * scale).round() as usize).max(1);
    let mut out = vec![0u8; out_w * out_h * 3];

    for oy in 0..out_h {
        for ox in 0..out_w {
            let ix = ((ox as f32 / out_w as f32) * width as f32).floor() as usize;
            let iy = ((oy as f32 / out_h as f32) * height as f32).floor() as usize;
            let sx = ix.min(width - 1);
            let sy = iy.min(height - 1);
            let src = (sy * width + sx) * 3;
            let dst = (oy * out_w + ox) * 3;
            out[dst..dst + 3].copy_from_slice(&raw[src..src + 3]);
        }
    }

    PreviewFrame {
        width: out_w,
        height: out_h,
        rgb: out,
    }
}

fn detect_transition_id(
    method: MeasurementMethod,
    luma_threshold: f32,
    rgb: &[u8],
    width: usize,
    height: usize,
    roi: RoiNorm,
    previous_id: Option<u64>,
) -> Option<u64> {
    if width < 4 || height < 4 {
        return None;
    }

    let roi = roi.clamp();
    let x0 = (roi.x * width as f32).floor() as usize;
    let y0 = (roi.y * height as f32).floor() as usize;
    let x1 = ((roi.x + roi.w) * width as f32).ceil() as usize;
    let y1 = ((roi.y + roi.h) * height as f32).ceil() as usize;

    let rw = x1.saturating_sub(x0).max(2);
    let rh = y1.saturating_sub(y0).max(2);

    let sample = |qx: usize, qy: usize| -> f32 {
        let x = if qx == 0 {
            x0 + rw / 4
        } else {
            x0 + (3 * rw) / 4
        };
        let y = if qy == 0 {
            y0 + rh / 4
        } else {
            y0 + (3 * rh) / 4
        };
        let x = x.min(width - 1);
        let y = y.min(height - 1);
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
            let luma = sample(0, 0);
            let bit = if luma > luma_threshold { 1 } else { 0 };
            match previous_id {
                None => Some(bit),
                Some(prev) if prev % 2 == bit => None,
                Some(prev) => Some(prev + 1),
            }
        }
        MeasurementMethod::QuadCode => {
            let lumas = [sample(0, 0), sample(1, 0), sample(0, 1), sample(1, 1)];
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
    preview_texture: Option<egui::TextureHandle>,
    roi_drag_start: Option<egui::Pos2>,
    mask_roi_in_preview: bool,
}

impl App {
    fn new(cli: Cli, state: Arc<Mutex<SharedState>>, stop_tx: Sender<()>) -> Self {
        Self {
            period: Duration::from_secs_f64(1.0 / cli.stimulus_hz.max(1.0)),
            cli,
            state,
            stop_tx,
            last_tick: Instant::now(),
            run_started_at: Instant::now(),
            pause_until: None,
            done_sent: false,
            preview_texture: None,
            roi_drag_start: None,
            mask_roi_in_preview: true,
        }
    }

    fn rolling_online_report(&self) -> LatencyReport {
        let locked = self.state.lock().expect("state lock poisoned").clone();
        build_report_for_run(&locked, 0)
    }

    fn update_mode_state(&mut self, ctx: &egui::Context) {
        {
            let mut guard = self.state.lock().expect("state lock poisoned");
            if !guard.run_started {
                guard.run_started = true;
                guard.capture_active = true;
                guard.epoch = Instant::now();
                self.run_started_at = Instant::now();
            }
        }

        if matches!(self.cli.mode, RunMode::Offline) {
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
            } else if self.run_started_at.elapsed().as_secs_f64() >= self.cli.duration_s {
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

        let mut guard = self.state.lock().expect("state lock poisoned");
        if guard.capture_active && !guard.run_finished && self.last_tick.elapsed() >= self.period {
            let id = guard.current_transition_id;
            let ms = guard.epoch.elapsed().as_secs_f64() * 1000.0;
            let run = guard.current_run;
            guard.display_events.push(DisplayEvent { run, id, ms });
            guard.current_transition_id += 1;
            self.last_tick = Instant::now();
        }
    }

    fn preview_image_and_roi(&self) -> (Option<PreviewFrame>, Option<RoiNorm>) {
        let guard = self.state.lock().expect("state lock poisoned");
        (guard.latest_preview.clone(), guard.detection_roi)
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.update_mode_state(ctx);

        let (current_run, current_id, capture_active) = {
            let guard = self.state.lock().expect("state lock poisoned");
            (
                guard.current_run,
                guard.current_transition_id,
                guard.capture_active,
            )
        };
        let stimulus = state_for(self.cli.method, current_id);
        let (preview_opt, roi_opt) = self.preview_image_and_roi();

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.checkbox(&mut self.mask_roi_in_preview, "Mask ROI in preview");
                if ui.button("Clear ROI").clicked() {
                    let mut guard = self.state.lock().expect("state lock poisoned");
                    guard.detection_roi = None;
                }
            });

            ui.separator();
            ui.horizontal_top(|ui| {
                ui.vertical(|ui| {
                    ui.heading("Stimulus Pattern");
                    let side = ui.available_width().min(ui.available_height()).min(420.0);
                    let (rect, _) =
                        ui.allocate_exact_size(egui::vec2(side, side), egui::Sense::hover());
                    let painter = ui.painter_at(rect);

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
                        egui::Rect::from_min_size(
                            egui::pos2(x + half_w, y),
                            egui::vec2(half_w, half_h),
                        ),
                        0.0,
                        egui::Color32::from_rgb(c1[0], c1[1], c1[2]),
                    );
                    painter.rect_filled(
                        egui::Rect::from_min_size(
                            egui::pos2(x, y + half_h),
                            egui::vec2(half_w, half_h),
                        ),
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
                });

                ui.separator();

                ui.vertical(|ui| {
                    ui.heading("Status");
                    ui.label(format!(
                        "Mode: {:?} | Method: {:?} | Run: {}",
                        self.cli.mode,
                        self.cli.method,
                        current_run + 1
                    ));
                    ui.label(format!("Transition #{}", stimulus.transition_id));
                    if roi_opt.is_none() {
                        ui.colored_label(
                            egui::Color32::YELLOW,
                            "Select ROI in camera view to start robust detection.",
                        );
                    }
                    if matches!(self.cli.mode, RunMode::Online) {
                        let report = self.rolling_online_report();
                        if let Some(stats) = report.stats {
                            ui.label(format!(
                                "Realtime latency: mean {:.2} ms, p95 {:.2} ms, n={}.",
                                stats.mean_ms, stats.p95_ms, stats.count
                            ));
                        }
                    } else if !capture_active {
                        ui.label("Settling between runs...");
                    }

                    ui.separator();
                    ui.heading("Camera View (for ROI location)");

                    if let Some(preview) = preview_opt {
                        let mut rgb = preview.rgb.clone();
                        if self.mask_roi_in_preview {
                            if let Some(roi) = roi_opt {
                                apply_mask_to_roi(&mut rgb, preview.width, preview.height, roi);
                            }
                        }

                        let color_image = rgb_to_color_image(preview.width, preview.height, &rgb);
                        let texture = self.preview_texture.get_or_insert_with(|| {
                            ui.ctx().load_texture(
                                "camera-preview",
                                color_image.clone(),
                                egui::TextureOptions::LINEAR,
                            )
                        });
                        texture.set(color_image, egui::TextureOptions::LINEAR);

                        let size = egui::vec2(
                            480.0,
                            480.0 * (preview.height as f32 / preview.width as f32),
                        );
                        let (rect, response) =
                            ui.allocate_exact_size(size, egui::Sense::click_and_drag());
                        ui.painter().image(
                            texture.id(),
                            rect,
                            egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
                            egui::Color32::WHITE,
                        );

                        if response.drag_started() {
                            self.roi_drag_start = response.interact_pointer_pos();
                        }
                        if response.drag_stopped() {
                            if let (Some(start), Some(end)) =
                                (self.roi_drag_start.take(), response.interact_pointer_pos())
                            {
                                let roi = rect_to_normalized_roi(start, end, rect);
                                let mut guard = self.state.lock().expect("state lock poisoned");
                                guard.detection_roi = Some(roi.clamp());
                            }
                        }

                        if let Some(roi) = roi_opt {
                            let roi_rect = normalized_roi_to_rect(roi, rect);
                            ui.painter().rect_stroke(
                                roi_rect,
                                0.0,
                                egui::Stroke::new(2.0, egui::Color32::GREEN),
                            );
                        }
                    } else {
                        ui.label("Waiting for camera frames...");
                    }
                });
            });
        });

        ctx.request_repaint();
    }
}

fn rgb_to_color_image(width: usize, height: usize, rgb: &[u8]) -> egui::ColorImage {
    let mut pixels = Vec::with_capacity(width * height);
    for chunk in rgb.chunks_exact(3) {
        pixels.push(egui::Color32::from_rgb(chunk[0], chunk[1], chunk[2]));
    }
    egui::ColorImage {
        size: [width, height],
        pixels,
    }
}

fn rect_to_normalized_roi(start: egui::Pos2, end: egui::Pos2, bounds: egui::Rect) -> RoiNorm {
    let min_x = start.x.min(end.x).clamp(bounds.left(), bounds.right());
    let max_x = start.x.max(end.x).clamp(bounds.left(), bounds.right());
    let min_y = start.y.min(end.y).clamp(bounds.top(), bounds.bottom());
    let max_y = start.y.max(end.y).clamp(bounds.top(), bounds.bottom());

    RoiNorm {
        x: (min_x - bounds.left()) / bounds.width(),
        y: (min_y - bounds.top()) / bounds.height(),
        w: ((max_x - min_x) / bounds.width()).max(0.01),
        h: ((max_y - min_y) / bounds.height()).max(0.01),
    }
}

fn normalized_roi_to_rect(roi: RoiNorm, bounds: egui::Rect) -> egui::Rect {
    egui::Rect::from_min_max(
        egui::pos2(
            bounds.left() + roi.x * bounds.width(),
            bounds.top() + roi.y * bounds.height(),
        ),
        egui::pos2(
            bounds.left() + (roi.x + roi.w) * bounds.width(),
            bounds.top() + (roi.y + roi.h) * bounds.height(),
        ),
    )
}

fn apply_mask_to_roi(rgb: &mut [u8], width: usize, height: usize, roi: RoiNorm) {
    let roi = roi.clamp();
    let x0 = (roi.x * width as f32).floor() as usize;
    let y0 = (roi.y * height as f32).floor() as usize;
    let x1 = ((roi.x + roi.w) * width as f32).ceil() as usize;
    let y1 = ((roi.y + roi.h) * height as f32).ceil() as usize;

    for y in y0.min(height)..y1.min(height) {
        for x in x0.min(width)..x1.min(width) {
            let idx = (y * width + x) * 3;
            if idx + 2 < rgb.len() {
                rgb[idx] = (rgb[idx] as f32 * 0.15) as u8;
                rgb[idx + 1] = (rgb[idx + 1] as f32 * 0.15) as u8;
                rgb[idx + 2] = (rgb[idx + 2] as f32 * 0.15) as u8;
            }
        }
    }
}
