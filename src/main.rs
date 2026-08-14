//! Command line meter: pick a source, watch what it plays.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Result, bail};
use clap::Parser;
use cpal::traits::{DeviceTrait, HostTrait};
use output_decibel_meter::capture::{self, CaptureMode, Source};
use output_decibel_meter::meter::Meter;
use output_decibel_meter::selftest;

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
    /// Play a tone of a known level, meter it back, and report whether the
    /// figures match. Meters this process, so nothing else has to stop.
    #[arg(long)]
    self_test: bool,
    /// Print everything this machine exposes of its audio stack, and what this
    /// tool makes of it. For working out where a problem sits.
    #[arg(long)]
    diagnose: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    if cli.diagnose {
        return diagnose();
    }

    if cli.self_test {
        return self_test();
    }

    if cli.list {
        for source in capture::sources()? {
            println!("  {} {:<8} {}", tag(&source), state(&source), source.name);
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
    // Wall clock, not measured seconds: a source that hands over nothing would
    // otherwise keep --seconds waiting for ever, which is what it did.
    let started = std::time::Instant::now();
    let limit = cli.seconds.map(Duration::from_secs_f64);
    while !stop.load(Ordering::Relaxed) {
        if limit.is_some_and(|limit| started.elapsed() >= limit) {
            break;
        }
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

    if last.seconds == 0.0 {
        println!();
        bail!(
            "{} handed over no audio at all in {:.1} s",
            source.name,
            started.elapsed().as_secs_f64()
        );
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

/// Play a tone, meter it back, and say whether the chain holds up.
///
/// Three outcomes, and they mean different things, so they exit differently:
/// the figures matched (0), the figures were wrong (1), or this machine has
/// nothing to play into (2). Only the middle one says anything about the code.
fn self_test() -> Result<()> {
    describe_machine();

    let outcome = selftest::run();
    if outcome.is_err() {
        report_delivery();
    }
    let report = match outcome? {
        selftest::Outcome::Ran(report) => report,
        selftest::Outcome::NothingToCaptureWith(why) => {
            println!("{why}");
            println!();
            println!("nothing was measured, which says nothing about the meter itself.");
            println!("run --diagnose for what the audio stack does expose here.");
            std::process::exit(2);
        }
        selftest::Outcome::NothingToPlayInto(why) => {
            println!("no audio output on this machine: {why}");
            println!();
            println!("nothing was measured, which says nothing about the meter itself.");
            println!("run --diagnose for what the audio stack does expose here.");
            std::process::exit(2);
        }
    };

    println!("metering {}", report.source);
    println!(
        "  {}",
        match report.mode {
            CaptureMode::Application =>
                "this process's own stream — anything else may keep playing",
            CaptureMode::Device => "an output in loopback — nothing else must be playing",
        }
    );
    println!("  {} channels", report.channels);
    println!();

    for check in &report.checks {
        println!(
            "  {:<34} {:>7.1} {:<5} expected {:>6.1} ± {:.1}   {}",
            check.what,
            check.measured,
            check.unit,
            check.expected,
            check.tolerance,
            if check.passed() { "ok" } else { "FAILED" }
        );
    }
    println!();

    if report.passed() {
        println!("self-test passed");
        return Ok(());
    }
    report_delivery();
    if report.needs_a_quiet_machine {
        println!("this ran on an output rather than on this process's own stream,");
        println!("so anything else the machine was playing landed in the same figures.");
    }
    bail!("self-test failed: the tone did not come back at the level it was played")
}

/// On macOS, what the tap callback actually saw. Silence has two causes there
/// and they are told apart by whether anything was handed over at all.
fn report_delivery() {
    #[cfg(target_os = "macos")]
    {
        let (calls, buffers, samples) = output_decibel_meter::coreaudio::delivery();
        println!("the tap callback ran {calls} times, over {buffers} buffers, {samples} samples");
    }
}

/// Everything about this machine's audio, printed rather than interpreted.
///
/// Meant for a machine nobody can log into — a CI runner, someone else's
/// desktop — where the first question is always the same: is the audio stack
/// there at all? Every step says what it found and what it could not, so a
/// failure can be pinned on the system or on this program without guessing.
fn diagnose() -> Result<()> {
    describe_machine();

    println!("cpal");
    let host = cpal::default_host();
    println!("  host: {:?}", host.id());
    match host.default_output_device() {
        Some(device) => println!("  default output: {}", describe_device(&device)),
        None => println!("  default output: none"),
    }
    match host.default_input_device() {
        Some(device) => println!("  default input:  {}", describe_device(&device)),
        None => println!("  default input:  none"),
    }
    report_devices("output devices", host.output_devices().map(|d| d.collect()));
    report_devices("input devices", host.input_devices().map(|d| d.collect()));
    println!();

    #[cfg(all(target_os = "linux", feature = "pipewire"))]
    {
        println!("PipeWire graph");
        match output_decibel_meter::graph::nodes() {
            Ok(nodes) => {
                println!("  {} nodes", nodes.len());
                for node in nodes {
                    println!(
                        "    {:<8} {:<7} pid {:<8} serial {:<7} {}",
                        format!("{:?}", node.kind),
                        if node.is_active { "active" } else { "idle" },
                        node.pid.map(|p| p.to_string()).unwrap_or("—".to_string()),
                        node.serial,
                        node.name
                    );
                }
            }
            Err(e) => println!("  unavailable: {e:#}"),
        }
        println!();
    }

    println!("what this tool would meter");
    match capture::sources() {
        Ok(sources) if sources.is_empty() => println!("  nothing"),
        Ok(sources) => {
            for source in &sources {
                println!("  {} {:<8} {}", tag(source), state(source), source.name);
            }
        }
        Err(e) => println!("  listing failed: {e:#}"),
    }
    match capture::default_output() {
        Ok(source) => println!("  default: {}", source.name),
        Err(e) => println!("  no default output: {e:#}"),
    }
    Ok(())
}

/// The lines that head both reports: what this is running on.
fn describe_machine() {
    println!(
        "output-decibel-meter {} on {} {}",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    let backend = if cfg!(all(target_os = "linux", feature = "pipewire")) {
        "PipeWire graph, with cpal as the fallback"
    } else if cfg!(target_os = "windows") {
        "WASAPI, with cpal as the fallback"
    } else {
        "cpal only"
    };
    println!("  backend: {backend}");
    for name in [
        "XDG_RUNTIME_DIR",
        "PIPEWIRE_REMOTE",
        "PULSE_SERVER",
        "DISPLAY",
    ] {
        if let Some(value) = std::env::var_os(name) {
            println!("  {name}={}", value.to_string_lossy());
        }
    }
    println!();
}

/// One device, with the configuration it would be opened at.
fn describe_device(device: &cpal::Device) -> String {
    let name = device
        .description()
        .map(|d| d.to_string())
        .unwrap_or_else(|e| format!("<unnamed: {e}>"));
    let config = match device.default_output_config() {
        Ok(config) => format!(
            "{} ch, {} Hz, {}",
            config.channels(),
            config.sample_rate(),
            config.sample_format()
        ),
        Err(e) => format!("no default output config: {e}"),
    };
    format!("{name} [{config}]")
}

/// A whole list of devices, or why there is none.
fn report_devices(what: &str, devices: std::result::Result<Vec<cpal::Device>, cpal::Error>) {
    match devices {
        Ok(devices) if devices.is_empty() => println!("  {what}: none"),
        Ok(devices) => {
            println!("  {what}: {}", devices.len());
            for device in &devices {
                println!("    {}", describe_device(device));
            }
        }
        Err(e) => println!("  {what}: cannot be listed: {e}"),
    }
}

/// Whether audio flows through it, this meter's own tap excluded, or a dash
/// where the backend cannot tell.
fn state(source: &Source) -> &'static str {
    match source.is_active {
        Some(true) => "active",
        Some(false) => "idle",
        None => "—",
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
