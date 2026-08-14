//! Checking the whole chain against a tone of a known level.
//!
//! The unit tests feed samples straight into the meter, which proves the
//! arithmetic and nothing else. This plays a tone through the audio server and
//! meters it back, which is the only way to know that the tap is on the audio
//! that is playing, at the level it was played.
//!
//! It meters *this process*: the tone is played by the program running the
//! check, found in the graph by its own process id, and tapped at its own
//! output. Whatever else the machine happens to be playing lands in the output
//! mix and not in the measurement, so the check needs no quiet room, no
//! dedicated sink and no cooperation from the desktop.
//!
//! Where programs cannot be tapped — every platform but Linux, so far — it
//! falls back to metering the output, and then it *does* need the machine to be
//! otherwise quiet. It says so in what it reports rather than quietly changing
//! what it proves.

use std::f64::consts::TAU;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::capture::{self, CaptureMode, Source};
use crate::meter::Meter;

/// Level of the reference tone, in dBFS.
const REFERENCE_DBFS: f64 = -20.0;

/// How far the second tone sits below the first, in dB.
const STEP_DB: f64 = 6.0;

/// How far a reading may sit from what was played.
///
/// A 1 kHz sine is where K-weighting is flat and the LUFS calibration cancels
/// the 3 dB between peak and rms, so a −20 dBFS tone reads −20 LUFS; what is
/// left is resampling and the meter's own rounding.
const TOLERANCE: f64 = 0.5;

/// How long each measurement runs.
const MEASURE: Duration = Duration::from_secs(3);

/// One thing that was checked.
#[derive(Debug, Clone)]
pub struct Check {
    /// What was measured, in a few words.
    pub what: String,
    /// What came out of the meter.
    pub measured: f64,
    /// What it should have been.
    pub expected: f64,
    /// How far it was allowed to be.
    pub tolerance: f64,
    /// The unit both figures are in.
    pub unit: &'static str,
}

impl Check {
    /// Whether the reading landed inside its tolerance.
    pub fn passed(&self) -> bool {
        (self.measured - self.expected).abs() < self.tolerance
    }
}

/// What the check ran against, which decides what it proves.
#[derive(Debug, Clone)]
pub struct Report {
    /// The source that was metered.
    pub source: String,
    /// How it was tapped.
    pub mode: CaptureMode,
    /// Set when the machine had to be quiet for the figures to mean anything.
    pub needs_a_quiet_machine: bool,
    /// Everything that was checked.
    pub checks: Vec<Check>,
}

impl Report {
    /// Whether every check landed.
    pub fn passed(&self) -> bool {
        self.checks.iter().all(Check::passed)
    }
}

/// Play a tone, meter it back, and report what came out.
pub fn run() -> Result<Report> {
    // The loud tone first, and the source is found while it plays: a program
    // only exists in the graph for as long as it has a stream open.
    let tone = Tone::start(REFERENCE_DBFS)?;
    let source = ours_or_the_output()?;
    let mode = source.mode;
    let name = source.name.clone();
    let (loud, channels) = measure(&source)?;
    drop(tone);

    // A tone written to one channel reads 3 dB under the same tone written to
    // two, since loudness sums the channels' energy. What was asked for is a
    // stereo reference, so a mono stream is expected to read lower.
    let expected = REFERENCE_DBFS + 10.0 * (channels.min(2) as f64 / 2.0).log10();

    let mut checks = vec![
        Check {
            what: "integrated loudness".to_string(),
            measured: loud.0,
            expected,
            tolerance: TOLERANCE,
            unit: "LUFS",
        },
        Check {
            what: "true peak".to_string(),
            measured: loud.1,
            expected: REFERENCE_DBFS,
            tolerance: TOLERANCE,
            unit: "dBTP",
        },
    ];

    // Then the same tone six decibels down. This one is a difference, so it
    // holds whatever the path does to the absolute level — a system volume in
    // the way, a monitor that carries it, a resampler.
    let tone = Tone::start(REFERENCE_DBFS - STEP_DB)?;
    let source = ours_or_the_output()?;
    let (quiet, _) = measure(&source)?;
    drop(tone);

    checks.push(Check {
        what: format!("drop when the source drops {STEP_DB:.0} dB"),
        measured: loud.0 - quiet.0,
        expected: STEP_DB,
        tolerance: TOLERANCE,
        unit: "dB",
    });

    Ok(Report {
        source: name,
        needs_a_quiet_machine: mode != CaptureMode::Application,
        mode,
        checks,
    })
}

/// This process's own stream, or the output it is playing into.
fn ours_or_the_output() -> Result<Source> {
    let mine = capture::from_process(std::process::id())?;
    if let Some(source) = mine.into_iter().next() {
        return Ok(source);
    }
    capture::default_output().context("nothing of ours is listed, and there is no output either")
}

/// Meter a source for [`MEASURE`], returning (integrated, true peak) and the
/// channel count the stream came in at.
fn measure(source: &Source) -> Result<((f64, f64), u32)> {
    let capture = source
        .open()
        .with_context(|| format!("opening {}", source.name))?;
    let mut meter = Meter::new(capture.channels, capture.sample_rate)?;

    let mut last = Default::default();
    let mut reading = crate::meter::Reading::default();
    while reading.seconds < MEASURE.as_secs_f64() {
        let Some(block) = capture.next_block(Duration::from_millis(500)) else {
            bail!("no audio arrived from {} in half a second", source.name);
        };
        reading = meter.add(&block)?;
        last = (reading.integrated, reading.true_peak);
    }
    Ok((last, capture.channels))
}

/// A 1 kHz sine playing on the default output for as long as it is held.
struct Tone {
    // cpal stops the stream when this drops.
    _stream: cpal::Stream,
}

impl Tone {
    /// Start playing at `peak_dbfs`, and wait for the server to route it.
    fn start(peak_dbfs: f64) -> Result<Self> {
        let device = cpal::default_host()
            .default_output_device()
            .context("this machine reports no default output to play into")?;
        let supported = device
            .default_output_config()
            .context("the default output reports no usable configuration")?;
        let config: cpal::StreamConfig = supported.config();
        let channels = config.channels as usize;
        let rate = config.sample_rate as f64;
        let amplitude = 10.0_f64.powf(peak_dbfs / 20.0);

        let mut frame_index = 0.0_f64;
        let stream = device
            .build_output_stream(
                config,
                move |out: &mut [f32], _: &_| {
                    for frame in out.chunks_mut(channels.max(1)) {
                        let sample = (amplitude * (TAU * 1000.0 * frame_index / rate).sin()) as f32;
                        frame_index += 1.0;
                        // The first two channels only: on a surround output,
                        // filling every channel would add energy the reference
                        // is not supposed to have.
                        for slot in frame.iter_mut().take(2) {
                            *slot = sample;
                        }
                    }
                },
                |e| eprintln!("the tone stopped: {e}"),
                None,
            )
            .context("opening the default output to play the tone")?;
        stream.play().context("starting the tone")?;
        // The stream has to reach the graph before anything can look for it.
        std::thread::sleep(Duration::from_millis(600));
        Ok(Self { _stream: stream })
    }
}
