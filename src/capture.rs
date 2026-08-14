//! Getting audio out of a running system.
//!
//! Two modes, and they are not interchangeable — see [`CaptureMode`]. Two
//! backends too: the PipeWire graph where there is one, `cpal` everywhere else.
//! Whichever is used, the result is the same thing: blocks of interleaved `f32`
//! frames, handed over on a channel so the audio callback stays short.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
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
    /// Whether audio flows through it besides this meter, where the backend
    /// knows. `cpal` cannot tell, and answers `None`.
    pub is_active: Option<bool>,
    /// The process playing it, for a program the graph could name one for.
    pub pid: Option<u32>,
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
    /// A WASAPI endpoint, by its endpoint id. Outputs are opened in loopback.
    #[cfg(target_os = "windows")]
    Endpoint { id: String, loopback: bool },
    /// A Core Audio process tap: everything, or one program.
    #[cfg(target_os = "macos")]
    Tap(crate::coreaudio::What),
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
    #[cfg(target_os = "windows")]
    Endpoint {
        _tap: crate::wasapi::Tap,
    },
    #[cfg(target_os = "macos")]
    Tap {
        _tap: crate::coreaudio::Tap,
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
    if let Some(endpoints) = wasapi_sources() {
        return Ok(endpoints);
    }
    if let Some(taps) = tap_sources() {
        return Ok(taps);
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

    // The default output leads, and is never filtered out: whatever it is, it
    // is what `default_output` hands back, and a source that cannot be found
    // again in the listing cannot be reopened — a window would start on a
    // source it could not name.
    if let Some(device) = host.default_output_device() {
        found.push(Source {
            name: describe(&device),
            mode: CaptureMode::Device,
            is_output: true,
            is_active: None,
            pid: None,
            handle: Handle::Device(device),
        });
    }

    for device in host.output_devices().context("listing output devices")? {
        let name = describe(&device);
        if is_alsa_plugin(&name) || already_listed(&found, &name, true) {
            continue;
        }
        found.push(Source {
            name,
            mode: CaptureMode::Device,
            is_output: true,
            is_active: None,
            pid: None,
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
            is_active: None,
            pid: None,
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
        is_active: Some(node.is_active),
        pid: node.pid,
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
    if let Some(default) = wasapi_default_output() {
        return Ok(default);
    }
    let device = cpal::default_host()
        .default_output_device()
        .context("this machine reports no default output device")?;
    Ok(Source {
        name: describe(&device),
        mode: CaptureMode::Device,
        is_output: true,
        is_active: None,
        pid: None,
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

/// A source as it can be listed, sorted and shown, without holding it open.
///
/// A [`Source`] carries a device or a graph target, which cannot cross threads
/// on every platform; this is the part that can, and it reopens through
/// [`by_key`].
#[derive(Debug, Clone, PartialEq)]
pub struct SourceInfo {
    /// Name as the system reports it.
    pub name: String,
    /// How it would be tapped.
    pub mode: CaptureMode,
    /// True for an output, captured through loopback.
    pub is_output: bool,
    /// Whether audio flows through it besides this meter, where that is known.
    pub is_active: Option<bool>,
    /// The process playing it, for a program the graph could name one for.
    pub pid: Option<u32>,
    /// What [`by_key`] reopens it with.
    pub key: String,
}

impl Source {
    /// This source as it can be listed and kept.
    pub fn info(&self) -> SourceInfo {
        SourceInfo {
            name: self.name.clone(),
            mode: self.mode,
            is_output: self.is_output,
            is_active: self.is_active,
            pid: self.pid,
            key: self.key(),
        }
    }
}

/// A list of sources that keeps itself up to date.
///
/// Where the graph can be watched, it is: PipeWire announces a node the moment
/// it appears, so a program that starts playing is in the list as it starts,
/// with no polling at all. Elsewhere the list is taken again on a thread of its
/// own, which is the same promise at a coarser grain.
pub struct Listing {
    inner: Inner,
}

enum Inner {
    #[cfg(all(target_os = "linux", feature = "pipewire"))]
    Watched(crate::graph::Watch),
    Polled(Polled),
}

impl Listing {
    /// The sources as they stand now.
    pub fn sources(&self) -> Vec<SourceInfo> {
        match &self.inner {
            #[cfg(all(target_os = "linux", feature = "pipewire"))]
            Inner::Watched(watch) => watch
                .nodes()
                .into_iter()
                .map(|n| from_graph(n).info())
                .collect(),
            Inner::Polled(polled) => polled
                .seen
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
        }
    }
}

/// Start listing, live where that is possible.
pub fn listing() -> Listing {
    #[cfg(all(target_os = "linux", feature = "pipewire"))]
    if let Ok(watch) = crate::graph::watch() {
        return Listing {
            inner: Inner::Watched(watch),
        };
    }
    Listing {
        inner: Inner::Polled(Polled::start()),
    }
}

/// How often a polled listing is taken again.
const POLL_EVERY: Duration = std::time::Duration::from_secs(2);

/// The fallback: a thread taking the list again, forever.
struct Polled {
    seen: Arc<Mutex<Vec<SourceInfo>>>,
    stop: Arc<AtomicBool>,
}

impl Polled {
    fn start() -> Self {
        let seen = Arc::new(Mutex::new(listed()));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_seen = Arc::clone(&seen);
        let thread_stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            while !thread_stop.load(Ordering::Relaxed) {
                std::thread::sleep(POLL_EVERY);
                if thread_stop.load(Ordering::Relaxed) {
                    return;
                }
                let taken = listed();
                *thread_seen.lock().unwrap_or_else(|e| e.into_inner()) = taken;
            }
        });
        Self { seen, stop }
    }
}

impl Drop for Polled {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// One listing, as descriptions.
fn listed() -> Vec<SourceInfo> {
    sources()
        .map(|list| list.iter().map(Source::info).collect())
        .unwrap_or_default()
}

/// What WASAPI offers, on the platform where it is the only way in.
///
/// Preferred over `cpal` there for the reason the whole module exists: an
/// output has to be opened with the loopback flag, which no portable API sets.
#[cfg(target_os = "windows")]
fn wasapi_sources() -> Option<Vec<Source>> {
    let endpoints = match crate::wasapi::endpoints() {
        Ok(endpoints) if !endpoints.is_empty() => endpoints,
        Ok(_) => return None,
        Err(e) => {
            eprintln!("falling back to device listing: {e:#}");
            return None;
        }
    };
    Some(endpoints.into_iter().map(from_endpoint).collect())
}

/// An endpoint as a source.
#[cfg(target_os = "windows")]
fn from_endpoint(endpoint: crate::wasapi::Endpoint) -> Source {
    Source {
        name: endpoint.name,
        mode: CaptureMode::Device,
        is_output: endpoint.is_output,
        // WASAPI can be asked what an endpoint is doing, but only as a peak
        // sampled right now, which would flicker between listings.
        is_active: None,
        pid: None,
        handle: Handle::Endpoint {
            loopback: endpoint.is_output,
            id: endpoint.id,
        },
    }
}

/// The endpoint the system plays through, as WASAPI reports it.
#[cfg(target_os = "windows")]
fn wasapi_default_output() -> Option<Source> {
    let endpoints = crate::wasapi::endpoints().ok()?;
    let outputs = || endpoints.iter().filter(|e| e.is_output);
    // No default declared is no reason to give up: any output beats none.
    let chosen = outputs()
        .find(|e| e.is_default)
        .or_else(|| outputs().next())?;
    Some(from_endpoint(chosen.clone()))
}

#[cfg(not(target_os = "windows"))]
fn wasapi_default_output() -> Option<Source> {
    None
}

#[cfg(not(target_os = "windows"))]
fn wasapi_sources() -> Option<Vec<Source>> {
    None
}

/// What a Mac offers: the mix, tapped, plus its inputs through `cpal`.
///
/// There is one output entry rather than one per device, because a tap listens
/// to what is *played*, not to a card — which is the same thing wherever the
/// sound was going.
#[cfg(target_os = "macos")]
fn tap_sources() -> Option<Vec<Source>> {
    let mut found = vec![Source {
        name: "Everything this machine plays".to_string(),
        mode: CaptureMode::Device,
        is_output: true,
        is_active: None,
        pid: None,
        handle: Handle::Tap(crate::coreaudio::What::Everything),
    }];
    // Inputs are ordinary devices here, so cpal handles them as it always did.
    if let Ok(inputs) = cpal::default_host().input_devices() {
        for device in inputs {
            found.push(Source {
                name: describe(&device),
                mode: CaptureMode::Device,
                is_output: false,
                is_active: None,
                pid: None,
                handle: Handle::Device(device),
            });
        }
    }
    Some(found)
}

#[cfg(not(target_os = "macos"))]
fn tap_sources() -> Option<Vec<Source>> {
    None
}

/// Whether an output can be captured on this platform at all.
///
/// Capturing an output means recording what it plays, and no platform does that
/// through the same call. Linux gets away with opening one because a sink's
/// monitor is an ordinary input; Windows wants WASAPI's loopback flag and macOS
/// a Core Audio process tap, and `cpal` exposes neither. Asked anyway, macOS
/// answers `Unknown property` from three layers down, which explains nothing.
pub fn outputs_can_be_captured() -> bool {
    cfg!(any(
        target_os = "linux",
        target_os = "windows",
        target_os = "macos"
    ))
}

/// What to say when they cannot.
pub const WHY_NOT_OUTPUTS: &str = "capturing an output is not implemented on this platform yet — \
     macOS needs a Core Audio process tap, which cpal does not expose";

/// Find a source whose name contains `fragment`, case-insensitively.
pub fn find(fragment: &str) -> Result<Source> {
    let wanted = fragment.to_lowercase();
    sources()?
        .into_iter()
        .find(|s| s.name.to_lowercase().contains(&wanted))
        .with_context(|| format!("no audio source matching {fragment:?}"))
}

/// Every stream a process is playing, newest listing first.
///
/// What lets a program meter *itself*: pass [`std::process::id`] and what comes
/// back is its own output, tapped before the mix, whatever else the machine is
/// playing at the same time. Empty where programs cannot be tapped.
pub fn from_process(pid: u32) -> Result<Vec<Source>> {
    #[cfg(target_os = "macos")]
    {
        // Core Audio taps a process by id without any listing to go through,
        // which is all this needs — and is how a program meters itself here.
        return Ok(vec![Source {
            name: format!("process {pid}"),
            mode: CaptureMode::Application,
            is_output: true,
            is_active: None,
            pid: Some(pid),
            handle: Handle::Tap(crate::coreaudio::What::Process(pid)),
        }]);
    }
    #[cfg(not(target_os = "macos"))]
    Ok(sources()?
        .into_iter()
        .filter(|s| s.pid == Some(pid))
        .collect())
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
            #[cfg(target_os = "windows")]
            Handle::Endpoint { id, .. } => format!("endpoint:{id}"),
            #[cfg(target_os = "macos")]
            Handle::Tap(what) => match what {
                crate::coreaudio::What::Everything => "tap:everything".to_string(),
                crate::coreaudio::What::Process(pid) => format!("tap:{pid}"),
            },
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
            #[cfg(target_os = "macos")]
            Handle::Tap(what) => {
                let tapped = crate::coreaudio::open(*what)
                    .with_context(|| format!("tapping {}", self.name))?;
                return Ok(Capture {
                    channels: tapped.channels,
                    sample_rate: tapped.sample_rate,
                    blocks: tapped.blocks,
                    _running: Running::Tap { _tap: tapped.tap },
                });
            }
            #[cfg(target_os = "windows")]
            Handle::Endpoint { id, loopback } => {
                let tapped = crate::wasapi::open(id, *loopback)
                    .with_context(|| format!("tapping {}", self.name))?;
                return Ok(Capture {
                    channels: tapped.channels,
                    sample_rate: tapped.sample_rate,
                    blocks: tapped.blocks,
                    _running: Running::Endpoint { _tap: tapped.tap },
                });
            }
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

        if self.is_output && !outputs_can_be_captured() {
            bail!("{}: {}", self.name, WHY_NOT_OUTPUTS);
        }

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
