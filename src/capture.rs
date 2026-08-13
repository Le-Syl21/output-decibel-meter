//! Getting audio out of a running system.
//!
//! Two modes, and they are not interchangeable — see [`CaptureMode`]. Two
//! backends too: the PipeWire graph where there is one, `cpal` everywhere else.
//! Whichever is used, the result is the same thing: blocks of interleaved `f32`
//! frames, handed over on a channel so the audio callback stays short.

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
    /// volume; Linux taps a sink's monitor ports, which carry the volume or not
    /// depending on where that machine applies it. Fine for comparing before
    /// and after on one machine, misleading between machines.
    Device,
    /// One program's stream, tapped at its output.
    ///
    /// Upstream of any device volume, so readings can be compared across
    /// machines. Linux taps it through PipeWire; the other platforms have the
    /// mechanism but not the implementation, and list no programs.
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
    handle: Handle,
}

/// What a source needs to be opened, which differs per backend.
enum Handle {
    /// A `cpal` device, output or input.
    Device(Device),
    /// A node of the PipeWire graph, by the `object.serial` it was given.
    #[cfg(all(target_os = "linux", feature = "pipewire"))]
    Graph {
        serial: String,
        kind: crate::graph::Kind,
    },
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
    // Held only to keep the capture alive; both backends stop on drop.
    _running: Running,
}

/// Whatever has to stay alive for samples to keep arriving.
enum Running {
    Device {
        _stream: Stream,
    },
    #[cfg(all(target_os = "linux", feature = "pipewire"))]
    Graph {
        _tap: crate::graph::Tap,
    },
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

/// Every source that can be metered: outputs, then programs, then inputs.
///
/// Outputs come first because they are always there and always mean the same
/// thing: what the speakers receive. Programs come next, and only those playing
/// right now — the list is a snapshot, and worth taking again when it matters.
pub fn sources() -> Result<Vec<Source>> {
    if let Some(graph) = graph_sources() {
        return Ok(graph);
    }
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
            handle: Handle::Device(device),
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
            handle: Handle::Device(device),
        });
    }

    Ok(found)
}

/// The graph's own listing, or `None` where there is no graph to ask.
///
/// Preferred over `cpal` wherever it answers, because it is the only listing
/// that is *true*: the outputs it returns really are tapped at their monitors,
/// each device appears once, and programs appear at all.
#[cfg(all(target_os = "linux", feature = "pipewire"))]
fn graph_sources() -> Option<Vec<Source>> {
    let nodes = match crate::graph::nodes() {
        Ok(nodes) if !nodes.is_empty() => nodes,
        // No PipeWire, or nothing in it: fall back to plain devices rather than
        // refuse the listing. A machine on bare ALSA still has speakers.
        Ok(_) => return None,
        Err(e) => {
            eprintln!("falling back to device listing: {e:#}");
            return None;
        }
    };
    Some(nodes.into_iter().map(from_graph).collect())
}

/// A graph node as a source, which is mostly a matter of naming the mode.
#[cfg(all(target_os = "linux", feature = "pipewire"))]
fn from_graph(node: crate::graph::GraphNode) -> Source {
    use crate::graph::Kind;

    Source {
        name: node.name,
        mode: match node.kind {
            Kind::Program => CaptureMode::Application,
            Kind::Sink | Kind::Source => CaptureMode::Device,
        },
        is_output: node.kind != Kind::Source,
        handle: Handle::Graph {
            serial: node.serial,
            kind: node.kind,
        },
    }
}

#[cfg(not(all(target_os = "linux", feature = "pipewire")))]
fn graph_sources() -> Option<Vec<Source>> {
    None
}

/// The system's default output, captured in loopback.
pub fn default_output() -> Result<Source> {
    if let Some(default) = graph_default_output() {
        return Ok(default);
    }
    let device = cpal::default_host()
        .default_output_device()
        .context("this machine reports no default output device")?;
    Ok(Source {
        name: describe(&device),
        mode: CaptureMode::Device,
        is_output: true,
        handle: Handle::Device(device),
    })
}

/// The sink the system plays through, as the graph reports it.
#[cfg(all(target_os = "linux", feature = "pipewire"))]
fn graph_default_output() -> Option<Source> {
    use crate::graph::Kind;

    let nodes = crate::graph::nodes().ok()?;
    let sinks = || nodes.iter().filter(|n| n.kind == Kind::Sink);
    // No default declared is not a reason to give up: any output is a better
    // answer than the microphone ALSA would hand over.
    let chosen = sinks().find(|n| n.is_default).or_else(|| sinks().next())?;
    Some(from_graph(chosen.clone()))
}

#[cfg(not(all(target_os = "linux", feature = "pipewire")))]
fn graph_default_output() -> Option<Source> {
    None
}

/// Find a source whose name contains `fragment`, case-insensitively.
pub fn find(fragment: &str) -> Result<Source> {
    let wanted = fragment.to_lowercase();
    sources()?
        .into_iter()
        .find(|s| s.name.to_lowercase().contains(&wanted))
        .with_context(|| format!("no audio source matching {fragment:?}"))
}

/// Find the source a [`Source::key`] came from.
pub fn by_key(key: &str) -> Result<Source> {
    sources()?
        .into_iter()
        .find(|s| s.key() == key)
        .with_context(|| format!("{key} is no longer there"))
}

impl Source {
    /// A handle that still designates this source once the list is taken again.
    ///
    /// Names do not do that job: a browser can play two tabs under one name,
    /// and a program that stops and starts again is a different stream.
    pub fn key(&self) -> String {
        match &self.handle {
            Handle::Device(_) => format!("device:{}:{}", self.is_output, self.name),
            #[cfg(all(target_os = "linux", feature = "pipewire"))]
            Handle::Graph { serial, .. } => format!("graph:{serial}"),
        }
    }

    /// Start capturing.
    pub fn open(&self) -> Result<Capture> {
        // Where programs cannot be tapped there is one arm left, and a match on
        // one arm is a `let` — but writing it as one would not compile where
        // there are two.
        #[cfg_attr(
            not(all(target_os = "linux", feature = "pipewire")),
            allow(clippy::infallible_destructuring_match)
        )]
        let device = match &self.handle {
            Handle::Device(device) => device,
            #[cfg(all(target_os = "linux", feature = "pipewire"))]
            Handle::Graph { serial, kind } => {
                let tapped = crate::graph::open(serial, *kind)
                    .with_context(|| format!("tapping {}", self.name))?;
                return Ok(Capture {
                    channels: tapped.channels,
                    sample_rate: tapped.sample_rate,
                    blocks: tapped.blocks,
                    _running: Running::Graph { _tap: tapped.tap },
                });
            }
        };

        let supported = if self.is_output {
            device.default_output_config()
        } else {
            device.default_input_config()
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
            SampleFormat::F32 => device.build_input_stream(
                config,
                move |data: &[f32], _: &_| {
                    let _ = tx.send(data.to_vec());
                },
                on_error,
                None,
            ),
            SampleFormat::I16 => device.build_input_stream(
                config,
                move |data: &[i16], _: &_| {
                    let _ = tx.send(data.iter().map(|s| *s as f32 / 32768.0).collect());
                },
                on_error,
                None,
            ),
            SampleFormat::I32 => device.build_input_stream(
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
            _running: Running::Device { _stream: stream },
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
