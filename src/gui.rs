//! The meter window: pick a source, watch the level, reset when comparing.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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

/// How often the source list is taken again.
///
/// Programs come and go while the window stays open, and a list frozen at
/// startup would never show the one just started. Taken on a thread, because
/// enumerating devices can take long enough to be seen as a stutter.
const RELIST_EVERY: Duration = Duration::from_secs(2);

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

/// One line of the source list, as the window needs it.
///
/// A copy rather than a `Source`, because the list is taken on another thread
/// and only strings cross over.
#[derive(Clone, PartialEq)]
struct Row {
    name: String,
    mode: CaptureMode,
    is_output: bool,
    /// Whether audio is passing through it, where the backend knows.
    is_running: Option<bool>,
    /// What reopens this exact source later, names being ambiguous.
    key: String,
}

impl Row {
    /// The kind of source, as the table shows it.
    fn tag(&self) -> &'static str {
        match (self.mode, self.is_output) {
            (CaptureMode::Application, _) => "app",
            (CaptureMode::Device, true) => "out",
            (CaptureMode::Device, false) => "in",
        }
    }

    /// Where this kind sorts, so outputs head the table as they did.
    fn kind_order(&self) -> u8 {
        match (self.mode, self.is_output) {
            (CaptureMode::Device, true) => 0,
            (CaptureMode::Application, _) => 1,
            (CaptureMode::Device, false) => 2,
        }
    }

    /// Whether it is passing audio, or a dash where that cannot be known.
    fn state(&self) -> &'static str {
        match self.is_running {
            Some(true) => "running",
            Some(false) => "idle",
            None => "—",
        }
    }
}

/// Which column the table is sorted on.
#[derive(Clone, Copy, PartialEq)]
enum SortBy {
    Kind,
    Name,
    State,
}

fn rows() -> Vec<Row> {
    capture::sources()
        .map(|list| {
            list.into_iter()
                .map(|s| Row {
                    key: s.key(),
                    name: s.name,
                    mode: s.mode,
                    is_output: s.is_output,
                    is_running: s.is_running,
                })
                .collect()
        })
        .unwrap_or_default()
}

struct MeterApp {
    sources: Vec<Row>,
    /// The source being metered, kept whole: it may leave the list while it is
    /// still what the window is showing.
    selected: Option<Row>,
    worker: Option<Worker>,
    /// Falling peak marker, in dB.
    peak_marker: f32,
    /// A list being taken on another thread, if one is.
    relisting: Option<Receiver<Vec<Row>>>,
    last_relist: Instant,
    sort: SortBy,
    ascending: bool,
}

impl MeterApp {
    fn new() -> Self {
        let sources = rows();
        // Start on the system output rather than on whatever heads the list:
        // cpal opens with JACK and the sound servers, which are rarely what a
        // window opened to watch the speakers should be metering.
        let default = capture::default_output().ok().map(|s| s.key());
        let selected = default
            .and_then(|key| sources.iter().find(|r| r.key == key).cloned())
            .or_else(|| sources.first().cloned());
        let mut app = Self {
            selected,
            sources,
            worker: None,
            peak_marker: FLOOR_DB,
            relisting: None,
            last_relist: Instant::now(),
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

    /// Take the source list again, off the painting thread.
    fn relist(&mut self) {
        match &self.relisting {
            Some(pending) => match pending.try_recv() {
                Ok(sources) => {
                    self.sources = sources;
                    self.relisting = None;
                    self.last_relist = Instant::now();
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => self.relisting = None,
            },
            None if self.last_relist.elapsed() >= RELIST_EVERY => {
                let (tx, rx) = channel();
                std::thread::spawn(move || {
                    let _ = tx.send(rows());
                });
                self.relisting = Some(rx);
            }
            None => {}
        }
    }

    /// The rows in the order the table shows them.
    ///
    /// Sorted on display rather than on arrival, so a list taken again every
    /// two seconds does not have to remember how it was asked to be ordered.
    fn sorted(&self) -> Vec<Row> {
        let mut rows = self.sources.clone();
        rows.sort_by(|a, b| {
            let order = match self.sort {
                SortBy::Kind => a.kind_order().cmp(&b.kind_order()),
                SortBy::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                // Running first when ascending: what is playing is what one
                // came to meter.
                SortBy::State => b.is_running.cmp(&a.is_running),
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
                                    let mark = match (self.sort == column, self.ascending) {
                                        (true, true) => " ▲",
                                        (true, false) => " ▼",
                                        (false, _) => "",
                                    };
                                    let header = egui::RichText::new(format!("{label}{mark}"))
                                        .weak()
                                        .small();
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
                                        ui.selectable_label(selected, row.tag()).clicked();
                                    clicked |= ui.selectable_label(selected, row.state()).clicked();
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

        // The list is a snapshot of what is playing, so it is taken again while
        // the window is open.
        self.relist();
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
            egui::Grid::new("figures")
                .num_columns(3)
                .spacing([18.0, 4.0])
                .show(ui, |ui| {
                    figure(ui, "momentary", reading.momentary, "LUFS", "400 ms");
                    ui.end_row();
                    figure(ui, "short term", reading.short_term, "LUFS", "3 s");
                    ui.end_row();
                    figure(ui, "integrated", reading.integrated, "LUFS", &since_reset);
                    ui.end_row();
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

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([560.0, 520.0])
            .with_min_inner_size([420.0, 400.0])
            .with_title("Output decibel meter"),
        ..Default::default()
    };
    eframe::run_native(
        "output-decibel-meter",
        options,
        Box::new(|_cc| Ok(Box::new(MeterApp::new()))),
    )
}
