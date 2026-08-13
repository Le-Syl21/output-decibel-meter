//! Command line meter: pick a source, watch what it plays.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use output_decibel_meter::capture::{self, CaptureMode, Source};
use output_decibel_meter::meter::Meter;

#[derive(Parser)]
#[command(
    version,
    about = "Measure the loudness of what a program or a device is playing"
)]
struct Cli {
    /// Part of a source name. Defaults to the system output, in loopback.
    #[arg(long)]
    source: Option<String>,
    /// Stop after this many seconds. Runs until Ctrl+C otherwise.
    #[arg(long)]
    seconds: Option<f64>,
    /// List what can be metered and exit.
    #[arg(long)]
    list: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    if cli.list {
        for source in capture::sources()? {
            println!("  {} {}", tag(&source), source.name);
        }
        return Ok(());
    }

    let source = match cli.source.as_deref() {
        Some(fragment) => capture::find(fragment)?,
        None => capture::default_output()?,
    };
    announce(&source);

    let capture = source.open()?;
    let mut meter = Meter::new(capture.channels, capture.sample_rate)?;
    println!(
        "{} channels at {} Hz\n",
        capture.channels, capture.sample_rate
    );

    let stop = Arc::new(AtomicBool::new(false));
    let on_signal = Arc::clone(&stop);
    let _ = ctrlc::set_handler(move || on_signal.store(true, Ordering::Relaxed));

    let mut shown = 0.0;
    let mut last = Default::default();
    while !stop.load(Ordering::Relaxed) {
        let Some(block) = capture.next_block(Duration::from_millis(200)) else {
            continue;
        };
        last = meter.add(&block)?;

        // One line per second: enough to follow by eye, few enough to scroll.
        if last.seconds - shown >= 1.0 {
            shown = last.seconds;
            println!(
                "  {:>6.1}s   M {:>6.1}   S {:>6.1}   I {:>6.1} LUFS   peak {:>6.1} dBTP",
                last.seconds, last.momentary, last.short_term, last.integrated, last.true_peak
            );
        }

        if cli.seconds.is_some_and(|limit| last.seconds >= limit) {
            break;
        }
    }

    println!("\nover {:.1} s", last.seconds);
    println!("  integrated  {:>8.1} LUFS", last.integrated);
    println!("  true peak   {:>8.1} dBTP", last.true_peak);
    Ok(())
}

/// What kind of source this is, in a column of its own so the list lines up.
fn tag(source: &Source) -> &'static str {
    match (source.mode, source.is_output) {
        (CaptureMode::Application, _) => "[app]   ",
        (CaptureMode::Device, true) => "[output]",
        (CaptureMode::Device, false) => "[input] ",
    }
}

/// Say what is being measured and where the tap sits.
///
/// Printed every run on purpose: a loudness figure means nothing without the
/// point it was taken at, and the point differs per platform.
fn announce(source: &Source) {
    println!("metering {}", source.name);
    let how = match source.mode {
        CaptureMode::Device if source.is_output => "output device, captured in loopback",
        CaptureMode::Device => "input device",
        CaptureMode::Application => "application stream",
    };
    let volume = match source.mode.includes_system_volume() {
        Some(true) => "the system volume is included",
        Some(false) => "measured before the system volume",
        None => "the system volume may be included, depending on how this machine applies it",
    };
    println!("  {how} — {volume}");
}
