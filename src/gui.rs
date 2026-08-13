//! The meter window: pick a source, watch the level, reset when comparing.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use egui::{Color32, CornerRadius, Pos2, Rect, Sense, Stroke, Vec2};
use output_decibel_meter::capture::{self, CaptureMode};
use output_decibel_meter::meter::{Meter, Reading};

/// Bottom of the scales, in LUFS and dBTP. Below this a signal is inaudible in
/// any practical setting, and drawing it would waste the whole meter on silence.
const FLOOR_DB: f32 = -60.0;

/// How fast the peak marker falls back, in dB per second. Slow enough to catch
/// a transient by eye, fast enough not to freeze the display on one accident.
const PEAK_FALL_DB_PER_S: f32 = 12.0;

/// Points kept in the graph — about a minute at one point per 100 ms.
const HISTORY: usize = 600;

/// What the capture thread publishes and the window reads.
#[derive(Default)]
struct Shared {
    reading: Reading,
    /// Short-term loudness and true peak, oldest first.
    history: VecDeque<(f32, f32)>,
    /// Set when the capture could not start or died.
    error: Option<String>,
    /// Stream shape, once known.
    format: Option<(u32, u32)>,
}

/// A running capture thread.
struct Worker {
    shared: Arc<Mutex<Shared>>,
    stop: Arc<AtomicBool>,
    reset: Arc<AtomicBool>,
}

impl Worker {
    /// Start metering a source by name.
    fn start(source_name: String) -> Self {
        let shared = Arc::new(Mutex::new(Shared::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let reset = Arc::new(AtomicBool::new(false));

        let thread_shared = Arc::clone(&shared);
        let thread_stop = Arc::clone(&stop);
        let thread_reset = Arc::clone(&reset);

        // cpal streams are not Send on every platform, so the capture is opened
        // and kept on this thread; only the readings cross over.
        std::thread::spawn(move || {
            let result = (|| -> anyhow::Result<()> {
                let source = capture::find(&source_name)?;
                let capture = source.open()?;
                let mut meter = Meter::new(capture.channels, capture.sample_rate)?;
                thread_shared.lock().unwrap().format =
                    Some((capture.channels, capture.sample_rate));

                let mut next_point = 0.0;
                while !thread_stop.load(Ordering::Relaxed) {
                    if thread_reset.swap(false, Ordering::Relaxed) {
                        meter.reset()?;
                        next_point = 0.0;
                        let mut shared = thread_shared.lock().unwrap();
                        shared.history.clear();
                        shared.reading = Reading::default();
                    }
                    let Some(block) = capture.next_block(Duration::from_millis(100)) else {
                        continue;
                    };
                    let reading = meter.add(&block)?;

                    let mut shared = thread_shared.lock().unwrap();
                    shared.reading = reading;
                    if reading.seconds >= next_point {
                        next_point = reading.seconds + 0.1;
                        if shared.history.len() == HISTORY {
                            shared.history.pop_front();
                        }
                        shared
                            .history
                            .push_back((reading.short_term as f32, reading.true_peak as f32));
                    }
                }
                Ok(())
            })();

            if let Err(e) = result {
                thread_shared.lock().unwrap().error = Some(format!("{e:#}"));
            }
        });

        Self {
            shared,
            stop,
            reset,
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

struct MeterApp {
    sources: Vec<(String, CaptureMode, bool)>,
    selected: usize,
    worker: Option<Worker>,
    /// Falling peak marker, in dB.
    peak_marker: f32,
}

impl MeterApp {
    fn new() -> Self {
        let sources = capture::sources()
            .map(|list| {
                list.into_iter()
                    .map(|s| (s.name, s.mode, s.is_output))
                    .collect()
            })
            .unwrap_or_default();
        let mut app = Self {
            sources,
            selected: 0,
            worker: None,
            peak_marker: FLOOR_DB,
        };
        app.restart();
        app
    }

    /// Drop the running capture and start on the selected source.
    fn restart(&mut self) {
        self.worker = None;
        self.peak_marker = FLOOR_DB;
        if let Some((name, _, _)) = self.sources.get(self.selected) {
            self.worker = Some(Worker::start(name.clone()));
        }
    }
}

/// Map a level in dB onto 0..1 of the meter, clamped at the floor.
fn scale(db: f32) -> f32 {
    if !db.is_finite() {
        return 0.0;
    }
    ((db - FLOOR_DB) / -FLOOR_DB).clamp(0.0, 1.0)
}

/// Green up to -18, amber to -6, red above: the usual reading of a meter.
fn level_color(db: f32) -> Color32 {
    if db >= -6.0 {
        Color32::from_rgb(206, 84, 74)
    } else if db >= -18.0 {
        Color32::from_rgb(207, 160, 66)
    } else {
        Color32::from_rgb(96, 158, 108)
    }
}

impl eframe::App for MeterApp {
    fn ui(&mut self, root: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = root.ctx().clone();
        // Audio moves whether or not the mouse does.
        ctx.request_repaint_after(Duration::from_millis(50));

        let (reading, history, error, format) = match &self.worker {
            Some(worker) => {
                let shared = worker.shared.lock().unwrap();
                (
                    shared.reading,
                    shared.history.iter().copied().collect::<Vec<_>>(),
                    shared.error.clone(),
                    shared.format,
                )
            }
            None => (Reading::default(), Vec::new(), None, None),
        };

        // The marker falls back on its own, so a peak stays readable a moment.
        let dt = ctx.input(|i| i.stable_dt).min(0.2);
        self.peak_marker = (self.peak_marker - PEAK_FALL_DB_PER_S * dt).max(FLOOR_DB);
        if reading.recent_peak.is_finite() && reading.recent_peak as f32 > self.peak_marker {
            self.peak_marker = reading.recent_peak as f32;
        }

        egui::Panel::top("source").show(root, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let label = self
                    .sources
                    .get(self.selected)
                    .map(|(n, _, _)| n.clone())
                    .unwrap_or_else(|| "no source".to_string());

                let mut changed = None;
                egui::ComboBox::from_id_salt("source")
                    .selected_text(label)
                    .width(360.0)
                    .show_ui(ui, |ui| {
                        for (index, (name, mode, is_output)) in self.sources.iter().enumerate() {
                            let tag = match (mode, is_output) {
                                (CaptureMode::Application, _) => "app",
                                (CaptureMode::Device, true) => "out",
                                (CaptureMode::Device, false) => "in ",
                            };
                            if ui
                                .selectable_label(index == self.selected, format!("[{tag}] {name}"))
                                .clicked()
                            {
                                changed = Some(index);
                            }
                        }
                    });
                if let Some(index) = changed {
                    self.selected = index;
                    self.restart();
                }

                if ui.button("Reset").clicked()
                    && let Some(worker) = &self.worker
                {
                    worker.reset.store(true, Ordering::Relaxed);
                    self.peak_marker = FLOOR_DB;
                }
            });

            // Where the tap sits changes what the numbers mean, so it is said
            // on screen rather than buried in a manual.
            if let Some((_, mode, _)) = self.sources.get(self.selected) {
                let note = match mode.includes_system_volume() {
                    Some(true) => "system volume included",
                    Some(false) => "measured before the system volume",
                    None => "system volume may be included on this machine",
                };
                let shape = format
                    .map(|(ch, rate)| format!("{ch} ch at {rate} Hz — "))
                    .unwrap_or_default();
                ui.label(egui::RichText::new(format!("{shape}{note}")).weak().small());
            }
            ui.add_space(4.0);
        });

        egui::CentralPanel::default().show(root, |ui| {
            if let Some(error) = error {
                ui.colored_label(Color32::from_rgb(206, 84, 74), error);
                return;
            }

            draw_bar(ui, reading.momentary as f32, self.peak_marker);
            ui.add_space(8.0);
            draw_history(ui, &history);
            ui.add_space(8.0);

            egui::Grid::new("figures")
                .num_columns(4)
                .spacing([18.0, 4.0])
                .show(ui, |ui| {
                    figure(ui, "momentary", reading.momentary, "LUFS");
                    figure(ui, "short term", reading.short_term, "LUFS");
                    ui.end_row();
                    figure(ui, "integrated", reading.integrated, "LUFS");
                    figure(ui, "true peak", reading.true_peak, "dBTP");
                    ui.end_row();
                });
            ui.label(
                egui::RichText::new(format!("{:.1} s since reset", reading.seconds))
                    .weak()
                    .small(),
            );
        });
    }
}

/// One labelled number, or a dash when there is nothing to show.
fn figure(ui: &mut egui::Ui, label: &str, value: f64, unit: &str) {
    ui.label(egui::RichText::new(label).weak());
    let text = if value.is_finite() {
        format!("{value:>7.1} {unit}")
    } else {
        format!("{:>7} {unit}", "—")
    };
    ui.monospace(text);
}

/// The level bar, with the falling peak marker over it.
fn draw_bar(ui: &mut egui::Ui, level_db: f32, peak_db: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 34.0), Sense::hover());
    let painter = ui.painter();
    let corners = CornerRadius::same(3);

    painter.rect_filled(rect, corners, ui.visuals().extreme_bg_color);

    let filled = scale(level_db);
    if filled > 0.0 {
        let bar = Rect::from_min_size(rect.min, Vec2::new(rect.width() * filled, rect.height()));
        painter.rect_filled(bar, corners, level_color(level_db));
    }

    if peak_db > FLOOR_DB {
        let x = rect.min.x + rect.width() * scale(peak_db);
        painter.line_segment(
            [Pos2::new(x, rect.min.y), Pos2::new(x, rect.max.y)],
            Stroke::new(2.0, level_color(peak_db)),
        );
    }

    // Graduations every 12 dB, so the eye has something to measure against.
    for db in [-48.0, -36.0, -24.0, -12.0] {
        let x = rect.min.x + rect.width() * scale(db);
        painter.line_segment(
            [Pos2::new(x, rect.max.y - 6.0), Pos2::new(x, rect.max.y)],
            Stroke::new(1.0, ui.visuals().weak_text_color()),
        );
    }
}

/// The scrolling graph: short-term loudness, with the peak line above it.
fn draw_history(ui: &mut egui::Ui, history: &[(f32, f32)]) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 120.0), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(3), ui.visuals().extreme_bg_color);

    for db in [-48.0, -36.0, -24.0, -12.0] {
        let y = rect.max.y - rect.height() * scale(db);
        painter.line_segment(
            [Pos2::new(rect.min.x, y), Pos2::new(rect.max.x, y)],
            Stroke::new(1.0, ui.visuals().faint_bg_color),
        );
    }

    if history.len() < 2 {
        return;
    }

    let step = rect.width() / (HISTORY - 1) as f32;
    let start = rect.min.x + rect.width() - step * (history.len() - 1) as f32;
    let point = |index: usize, db: f32| {
        Pos2::new(
            start + step * index as f32,
            rect.max.y - rect.height() * scale(db),
        )
    };

    let loudness: Vec<Pos2> = history
        .iter()
        .enumerate()
        .map(|(i, (s, _))| point(i, *s))
        .collect();
    let peaks: Vec<Pos2> = history
        .iter()
        .enumerate()
        .map(|(i, (_, p))| point(i, *p))
        .collect();

    painter.add(egui::Shape::line(
        peaks,
        Stroke::new(1.0, Color32::from_rgb(150, 120, 60)),
    ));
    painter.add(egui::Shape::line(
        loudness,
        Stroke::new(1.5, Color32::from_rgb(96, 158, 108)),
    ));
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([560.0, 380.0])
            .with_min_inner_size([420.0, 300.0])
            .with_title("Output decibel meter"),
        ..Default::default()
    };
    eframe::run_native(
        "output-decibel-meter",
        options,
        Box::new(|_cc| Ok(Box::new(MeterApp::new()))),
    )
}
