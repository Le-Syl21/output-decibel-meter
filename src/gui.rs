// A window, not a command: on Windows a console behind it is noise. Debug
// builds keep it, since that is where println is how one looks inside.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! The meter window: pick a source, watch the level, reset when comparing.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use egui::{Color32, CornerRadius, Pos2, Rect, Sense, Stroke, Vec2};
use output_decibel_meter::capture::{self, CaptureMode, SourceInfo};
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
    /// Start metering the source this key designates.
    fn start(source_key: String) -> Self {
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
                let source = capture::by_key(&source_key)?;
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

/// Which column the table is sorted on.
#[derive(Clone, Copy, PartialEq)]
enum SortBy {
    Kind,
    Name,
    State,
}

/// The kind of source, as the table shows it.
fn tag(source: &SourceInfo) -> &'static str {
    match (source.mode, source.is_output) {
        (CaptureMode::Application, _) => "app",
        (CaptureMode::Device, true) => "out",
        (CaptureMode::Device, false) => "in",
    }
}

/// Where a kind sorts, so outputs head the table.
fn kind_order(source: &SourceInfo) -> u8 {
    match (source.mode, source.is_output) {
        (CaptureMode::Device, true) => 0,
        (CaptureMode::Application, _) => 1,
        (CaptureMode::Device, false) => 2,
    }
}

/// Whether audio flows through it, or a dash where that cannot be known.
fn state(source: &SourceInfo) -> &'static str {
    match source.is_active {
        Some(true) => "active",
        Some(false) => "idle",
        None => "—",
    }
}

struct MeterApp {
    sources: Vec<SourceInfo>,
    /// The source being metered, kept whole: it may leave the list while it is
    /// still what the window is showing.
    selected: Option<SourceInfo>,
    worker: Option<Worker>,
    /// Falling peak marker, in dB.
    peak_marker: f32,
    /// The list, kept up to date by the graph itself rather than polled.
    listing: capture::Listing,
    sort: SortBy,
    ascending: bool,
}

impl MeterApp {
    fn new() -> Self {
        let listing = capture::listing();
        let sources = listing.sources();
        // Start on the system output rather than on whatever heads the list: a
        // fallback listing opens with JACK and the sound servers, which are
        // rarely what a window opened to watch the speakers should meter.
        let default = capture::default_output().ok().map(|s| s.key());
        let selected = default
            .and_then(|key| sources.iter().find(|r| r.key == key).cloned())
            .or_else(|| sources.first().cloned());
        let mut app = Self {
            selected,
            sources,
            worker: None,
            peak_marker: FLOOR_DB,
            listing,
            // Outputs, then programs, then inputs: the order the graph itself
            // reports, and the one a meter is usually opened for.
            sort: SortBy::Kind,
            ascending: true,
        };
        app.restart();
        app
    }

    /// Drop the running capture and start on the selected source.
    fn restart(&mut self) {
        self.worker = None;
        self.peak_marker = FLOOR_DB;
        if let Some(row) = &self.selected {
            self.worker = Some(Worker::start(row.key.clone()));
        }
    }

    /// The rows in the order the table shows them.
    ///
    /// Sorted on display rather than on arrival, so a list that changes under
    /// the window does not have to remember how it was asked to be ordered.
    fn sorted(&self) -> Vec<SourceInfo> {
        let mut rows = self.sources.clone();
        rows.sort_by(|a, b| {
            let order = match self.sort {
                SortBy::Kind => kind_order(a).cmp(&kind_order(b)),
                SortBy::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                // Active first when ascending: what is playing is what one
                // came to meter.
                SortBy::State => b.is_active.cmp(&a.is_active),
            };
            // Ties fall back on the name, so the table never reshuffles rows
            // that compare equal from one listing to the next.
            let order = order.then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
            if self.ascending {
                order
            } else {
                order.reverse()
            }
        });
        rows
    }

    /// The source table: headers that sort, one clickable line per source.
    ///
    /// It fills whatever height the meter left, so a machine with three sources
    /// shows three lines and one with twenty scrolls — rather than a box of a
    /// fixed size, empty in the first case and cramped in the second.
    fn draw_table(&mut self, ui: &mut egui::Ui) {
        let rows = self.sorted();
        let mut chosen_row = None;
        let mut sort_on = None;

        egui::Frame::new()
            .fill(ui.visuals().extreme_bg_color)
            .corner_radius(CornerRadius::same(3))
            .inner_margin(6.0)
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        egui::Grid::new("sources")
                            .num_columns(3)
                            .spacing([10.0, 3.0])
                            .striped(true)
                            .show(ui, |ui| {
                                for (label, column) in [
                                    ("kind", SortBy::Kind),
                                    ("state", SortBy::State),
                                    ("source", SortBy::Name),
                                ] {
                                    // ⏶ and ⏷ rather than ▲ and ▼: the
                                    // proportional font carries no geometric
                                    // shapes, and a missing glyph is drawn as
                                    // an empty box.
                                    let mark = match (self.sort == column, self.ascending) {
                                        (true, true) => " ⏶",
                                        (true, false) => " ⏷",
                                        (false, _) => "",
                                    };
                                    let header =
                                        egui::RichText::new(format!("{label}{mark}")).strong();
                                    if ui
                                        .add(egui::Button::new(header).frame(false))
                                        .on_hover_text("sort on this column")
                                        .clicked()
                                    {
                                        sort_on = Some(column);
                                    }
                                }
                                ui.end_row();

                                if rows.is_empty() {
                                    ui.label(egui::RichText::new("nothing to meter").weak());
                                    ui.end_row();
                                }
                                for row in &rows {
                                    let selected =
                                        self.selected.as_ref().is_some_and(|s| s.key == row.key);
                                    // Three cells rather than one line, so the
                                    // columns line up under their headers; any
                                    // of them selects the source.
                                    let mut clicked =
                                        ui.selectable_label(selected, tag(row)).clicked();
                                    clicked |= ui.selectable_label(selected, state(row)).clicked();
                                    clicked |= ui.selectable_label(selected, &row.name).clicked();
                                    if clicked {
                                        chosen_row = Some(row.clone());
                                    }
                                    ui.end_row();
                                }
                            });
                    });
            });

        if let Some(column) = sort_on {
            self.sort_on(column);
        }
        if let Some(row) = chosen_row {
            self.selected = Some(row);
            self.restart();
        }
    }

    /// Click a header: sort on it, or turn the order around if it already does.
    fn sort_on(&mut self, column: SortBy) {
        if self.sort == column {
            self.ascending = !self.ascending;
        } else {
            self.sort = column;
            self.ascending = true;
        }
    }

    /// True when the metered source has left the list — a program that stopped.
    fn selection_is_gone(&self) -> bool {
        match &self.selected {
            Some(row) => !self.sources.iter().any(|s| s.key == row.key),
            None => false,
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

        // The list keeps itself up to date; reading it is a clone under a lock.
        self.sources = self.listing.sources();
        let stopped = self.selection_is_gone();

        // Bottom first: egui gives the central panel whatever the side panels
        // leave, which is exactly the rule wanted here — the meter takes the
        // room it needs, the table takes the rest.
        egui::Panel::bottom("meter").show(root, |ui| {
            ui.add_space(6.0);

            // Where the tap sits changes what the numbers mean, so it is said
            // on screen rather than buried in a manual.
            if let Some(row) = &self.selected {
                let note = match row.mode.includes_system_volume() {
                    Some(true) => "system volume included",
                    Some(false) => "measured before the system volume",
                    None => "system volume may be included on this machine",
                };
                let shape = format
                    .map(|(ch, rate)| format!("{ch} ch at {rate} Hz — "))
                    .unwrap_or_default();
                let line = if stopped {
                    format!("{} — stopped; still showing what it played", row.name)
                } else {
                    format!("{} — {shape}{note}", row.name)
                };
                ui.label(egui::RichText::new(line).weak().small());
                ui.add_space(4.0);
            }

            if let Some(error) = &error {
                ui.colored_label(Color32::from_rgb(206, 84, 74), error);
                ui.add_space(6.0);
                return;
            }

            draw_bar(ui, reading.momentary as f32, self.peak_marker);
            ui.add_space(8.0);
            draw_history(ui, &history);
            ui.add_space(8.0);

            // Every figure with the window it was taken over: three loudness
            // numbers that differ only by how long they look back are three
            // numbers one cannot read without knowing that.
            let since_reset = format!("{:.1} s, since reset", reading.seconds);
            // Two by two rather than four deep: the figures are read as pairs
            // anyway — the two that follow the action, then the two that keep
            // score — and the two lines saved go to the source table.
            egui::Grid::new("figures")
                .num_columns(6)
                .spacing([18.0, 4.0])
                .show(ui, |ui| {
                    figure(ui, "momentary", reading.momentary, "LUFS", "400 ms");
                    figure(ui, "integrated", reading.integrated, "LUFS", &since_reset);
                    ui.end_row();
                    figure(ui, "short term", reading.short_term, "LUFS", "3 s");
                    figure(ui, "true peak", reading.true_peak, "dBTP", &since_reset);
                    ui.end_row();
                });
            ui.add_space(6.0);
        });

        egui::CentralPanel::default().show(root, |ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("Metering").weak().small());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Reset").clicked()
                        && let Some(worker) = &self.worker
                    {
                        worker.reset.store(true, Ordering::Relaxed);
                        self.peak_marker = FLOOR_DB;
                    }
                });
            });
            ui.add_space(2.0);
            self.draw_table(ui);
        });
    }
}

/// One labelled number and the window it covers, or a dash when there is
/// nothing to show yet.
fn figure(ui: &mut egui::Ui, label: &str, value: f64, unit: &str, window: &str) {
    ui.label(egui::RichText::new(label).weak());
    let text = if value.is_finite() {
        format!("{value:>7.1} {unit}")
    } else {
        format!("{:>7} {unit}", "—")
    };
    ui.monospace(text);
    ui.label(egui::RichText::new(window).weak().small());
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

/// The window icon: a VU meter, drawn rather than shipped.
///
/// Arithmetic instead of a PNG in the repository, and it comes out right at
/// whatever size the desktop asks for. The look is the classic one — cream
/// face, black scale, red over the loud third, needle a little past the middle
/// — because that is what reads as "a meter" at sixteen pixels.
fn icon() -> egui::IconData {
    const SIZE: usize = 64;
    const SUPER: usize = 3;
    const FACE: [f32; 3] = [0.94, 0.90, 0.80];
    const INK: [f32; 3] = [0.11, 0.11, 0.10];
    const RED: [f32; 3] = [0.76, 0.20, 0.16];

    // The scale is an arc swept either side of vertical, around a pivot sitting
    // low on the face — a needle hinged at the bottom, as on the real thing.
    let pivot = (0.5_f32, 0.80_f32);
    let radius = 0.54_f32;
    let sweep = 52.0_f32.to_radians();
    let needle_at = 17.0_f32.to_radians();
    let ticks: [f32; 7] = [-1.0, -0.66, -0.33, 0.0, 0.33, 0.66, 1.0];

    /// Signed distance to a rounded rectangle: negative inside.
    fn rounded(px: f32, py: f32, x0: f32, y0: f32, x1: f32, y1: f32, r: f32) -> f32 {
        let cx = px.clamp(x0 + r, x1 - r);
        let cy = py.clamp(y0 + r, y1 - r);
        ((px - cx).powi(2) + (py - cy).powi(2)).sqrt() - r
    }

    let mut rgba = Vec::with_capacity(SIZE * SIZE * 4);
    for y in 0..SIZE {
        for x in 0..SIZE {
            // Supersampled, so the arc and the needle come out smooth.
            let mut sum = [0.0_f32; 4];
            for sy in 0..SUPER {
                for sx in 0..SUPER {
                    let px = (x as f32 + (sx as f32 + 0.5) / SUPER as f32) / SIZE as f32;
                    let py = (y as f32 + (sy as f32 + 0.5) / SIZE as f32) / SIZE as f32;

                    let face = rounded(px, py, 0.06, 0.12, 0.94, 0.88, 0.10);
                    let (dx, dy) = (px - pivot.0, py - pivot.1);
                    let distance = (dx * dx + dy * dy).sqrt();
                    let angle = dx.atan2(-dy);
                    let on_scale = angle.abs() < sweep;
                    // Everything past two thirds of the sweep is the loud part.
                    let hot = angle > sweep * 0.34;

                    let on_arc = on_scale && (distance - radius).abs() < 0.030;
                    let on_tick = on_scale
                        && (radius - 0.10..radius - 0.03).contains(&distance)
                        && ticks
                            .iter()
                            .any(|t| (angle - t * sweep).abs() < 0.030 / distance.max(0.1));

                    // The needle, as a segment from the hub outwards.
                    let (nx, ny) = (needle_at.sin(), -needle_at.cos());
                    let along = (dx * nx + dy * ny).clamp(0.0, radius - 0.09);
                    let to_needle = ((dx - nx * along).powi(2) + (dy - ny * along).powi(2)).sqrt();
                    let on_needle = to_needle < 0.020 || distance < 0.045;

                    let colour = if face > 0.0 {
                        [0.0, 0.0, 0.0, 0.0]
                    } else if face > -0.022 {
                        // A rim, so the face has an edge on a light desktop too.
                        [INK[0], INK[1], INK[2], 1.0]
                    } else if on_needle {
                        [INK[0], INK[1], INK[2], 1.0]
                    } else if on_arc || on_tick {
                        let c = if hot { RED } else { INK };
                        [c[0], c[1], c[2], 1.0]
                    } else {
                        [FACE[0], FACE[1], FACE[2], 1.0]
                    };
                    for (slot, value) in sum.iter_mut().zip(colour) {
                        *slot += value;
                    }
                }
            }
            let samples = (SUPER * SUPER) as f32;
            for channel in sum {
                rgba.push(((channel / samples) * 255.0).round().clamp(0.0, 255.0) as u8);
            }
        }
    }

    egui::IconData {
        rgba,
        width: SIZE as u32,
        height: SIZE as u32,
    }
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([840.0, 780.0])
            .with_min_inner_size([620.0, 600.0])
            .with_icon(icon())
            .with_title("Output decibel meter"),
        ..Default::default()
    };
    eframe::run_native(
        "output-decibel-meter",
        options,
        Box::new(|cc| {
            // Everything half again as large: a meter is read from a step back,
            // next to whatever is being listened to.
            cc.egui_ctx.set_zoom_factor(1.5);
            Ok(Box::new(MeterApp::new()))
        }),
    )
}
