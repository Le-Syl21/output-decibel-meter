//! Getting audio out of a running system.
//!
//! Two modes, and they are not interchangeable — see [`CaptureMode`]. Whichever
//! is used, the result is the same thing: blocks of interleaved `f32` frames,
//! handed over on a channel so the audio callback stays short.

use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, SampleFormat, Stream, StreamConfig};

/// How a source is tapped, which decides what the numbers mean.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureMode {
    /// A loopback of an output device: everything the machine plays, mixed.
    ///
    /// Available everywhere, but the tap sits at a different place on each
    /// platform. Windows and macOS capture the render mix before the device
    /// volume; Linux exposes a sink's monitor, which is after it. Fine for
    /// comparing before and after on one machine, misleading between machines.
    Device,
    /// One program's stream, tapped at its output.
    ///
    /// The same point on all three platforms, upstream of any device volume, so
    /// readings can be compared across machines.
    Application,
}

impl CaptureMode {
    /// Whether the system volume is part of what gets measured.
    ///
    /// `None` when it depends on how the machine is configured, which is the
    /// case for a Linux sink monitor: the volume shows up in the capture when
    /// it is applied in software, and does not when the card applies it.
    pub fn includes_system_volume(self) -> Option<bool> {
        match self {
            CaptureMode::Application => Some(false),
            CaptureMode::Device if cfg!(target_os = "linux") => None,
            CaptureMode::Device => Some(false),
        }
    }
}

/// Something that can be metered.
pub struct Source {
    /// Name as the system reports it.
    pub name: String,
    /// How it will be tapped.
    pub mode: CaptureMode,
    /// True for an output device, captured through loopback.
    pub is_output: bool,
    device: Device,
}

impl std::fmt::Debug for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Source")
            .field("name", &self.name)
            .field("mode", &self.mode)
            .field("is_output", &self.is_output)
            .finish()
    }
}

/// A running capture. Dropping it stops the stream.
pub struct Capture {
    /// Interleaved channel count of the blocks.
    pub channels: u32,
    /// Frames per second of the blocks.
    pub sample_rate: u32,
    blocks: Receiver<Vec<f32>>,
    // Held only to keep the stream alive; cpal stops it on drop.
    _stream: Stream,
}

impl Capture {
    /// Next block of interleaved samples, or `None` if none arrived in time.
    ///
    /// A timeout rather than a block, so a caller driving a window can keep
    /// repainting while the source is silent.
    pub fn next_block(&self, timeout: Duration) -> Option<Vec<f32>> {
        self.blocks.recv_timeout(timeout).ok()
    }
}

/// ALSA advertises its internal plugins as devices: rate converters, mixers,
/// a null sink. They are not things anyone means to meter, and they bury the
/// real hardware under a dozen entries.
#[cfg(target_os = "linux")]
fn is_alsa_plugin(name: &str) -> bool {
    const PLUGINS: [&str; 12] = [
        "discard all samples",
        "rate converter",
        "sample rate conversion",
        "direct sample mixing",
        "direct sample snooping",
        "direct hardware device",
        "hardware device with all software conversions",
        "plugin for jack",
        "open sound system",
        "plugin using speex",
        "plugin for channel upmix",
        "plugin for channel downmix",
    ];
    let lower = name.to_lowercase();
    PLUGINS.iter().any(|p| lower.contains(p))
}

#[cfg(not(target_os = "linux"))]
fn is_alsa_plugin(_name: &str) -> bool {
    false
}

/// Every source that can be metered, outputs first.
///
/// Outputs come first because they are what one usually wants: an output
/// captured in loopback is what the speakers receive.
pub fn sources() -> Result<Vec<Source>> {
    let host = cpal::default_host();
    let mut found: Vec<Source> = Vec::new();

    // ALSA lists every card once per access path — hw, plughw, dmix and the
    // rest — so the same speakers show up a dozen times under one name. Only
    // the first of each is kept; they all reach the same hardware.
    let already_listed = |found: &[Source], name: &str, is_output: bool| {
        found
            .iter()
            .any(|s| s.name == name && s.is_output == is_output)
    };

    for device in host.output_devices().context("listing output devices")? {
        let name = describe(&device);
        if is_alsa_plugin(&name) || already_listed(&found, &name, true) {
            continue;
        }
        found.push(Source {
            name,
            mode: CaptureMode::Device,
            is_output: true,
            device,
        });
    }
    for device in host.input_devices().context("listing input devices")? {
        let name = describe(&device);
        if is_alsa_plugin(&name) || already_listed(&found, &name, false) {
            continue;
        }
        found.push(Source {
            name,
            mode: CaptureMode::Device,
            is_output: false,
            device,
        });
    }

    Ok(found)
}

/// The system's default output, captured in loopback.
pub fn default_output() -> Result<Source> {
    let device = cpal::default_host()
        .default_output_device()
        .context("this machine reports no default output device")?;
    Ok(Source {
        name: describe(&device),
        mode: CaptureMode::Device,
        is_output: true,
        device,
    })
}

/// Find a source whose name contains `fragment`, case-insensitively.
pub fn find(fragment: &str) -> Result<Source> {
    let wanted = fragment.to_lowercase();
    sources()?
        .into_iter()
        .find(|s| s.name.to_lowercase().contains(&wanted))
        .with_context(|| format!("no audio source matching {fragment:?}"))
}

impl Source {
    /// Start capturing.
    pub fn open(&self) -> Result<Capture> {
        let supported = if self.is_output {
            self.device.default_output_config()
        } else {
            self.device.default_input_config()
        }
        .with_context(|| format!("{} reports no usable configuration", self.name))?;

        let config: StreamConfig = supported.config();
        let channels = config.channels as u32;
        let sample_rate = config.sample_rate;

        let (tx, blocks): (Sender<Vec<f32>>, Receiver<Vec<f32>>) = channel();
        let on_error = |e| eprintln!("capture error: {e}");

        // The callback does the least it can: convert and hand over. Measuring
        // in here would risk holding up the audio thread and dropping frames.
        let stream = match supported.sample_format() {
            SampleFormat::F32 => self.device.build_input_stream(
                config,
                move |data: &[f32], _: &_| {
                    let _ = tx.send(data.to_vec());
                },
                on_error,
                None,
            ),
            SampleFormat::I16 => self.device.build_input_stream(
                config,
                move |data: &[i16], _: &_| {
                    let _ = tx.send(data.iter().map(|s| *s as f32 / 32768.0).collect());
                },
                on_error,
                None,
            ),
            SampleFormat::I32 => self.device.build_input_stream(
                config,
                move |data: &[i32], _: &_| {
                    let _ = tx.send(data.iter().map(|s| *s as f32 / 2_147_483_648.0).collect());
                },
                on_error,
                None,
            ),
            other => bail!("{} uses an unsupported sample format: {other}", self.name),
        }
        .with_context(|| format!("opening {}", self.name))?;

        stream
            .play()
            .with_context(|| format!("starting {}", self.name))?;

        Ok(Capture {
            channels,
            sample_rate,
            blocks,
            _stream: stream,
        })
    }
}

/// A readable name for a device, falling back when the backend has none.
fn describe(device: &Device) -> String {
    device
        .description()
        .map(|d| d.to_string())
        .unwrap_or_else(|_| "unnamed device".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn application_capture_never_includes_the_system_volume() {
        assert_eq!(
            CaptureMode::Application.includes_system_volume(),
            Some(false)
        );
    }

    #[test]
    fn device_capture_is_only_certain_off_linux() {
        let answer = CaptureMode::Device.includes_system_volume();
        if cfg!(target_os = "linux") {
            assert_eq!(answer, None, "a sink monitor depends on the setup");
        } else {
            assert_eq!(answer, Some(false));
        }
    }
}
