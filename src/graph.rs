//! Reading and tapping the PipeWire graph.
//!
//! On Linux everything that plays or records is a node in one graph: a sink for
//! each output, a source for each input, and a stream node for each program
//! that opened audio. A meter is one more node, linked to whichever of them is
//! being watched — the monitor ports of a sink for "what the speakers get", or
//! the output ports of a program for "what that program plays".
//!
//! Going through PipeWire rather than through ALSA is not a preference. ALSA
//! shows the same card a dozen times, hides the monitors, and answers a request
//! to record the default output by handing over the default *input*: the meter
//! then reads the microphone while claiming to read the speakers. The graph has
//! no such ambiguity — a target is a node, and the link either exists or does
//! not.
//!
//! The work happens on its own thread: PipeWire wants a loop of its own and
//! none of its objects cross threads, so only the samples do, over a channel.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::mpsc::{Receiver, Sender, SyncSender, channel, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use pipewire as pw;
use pw::metadata::{Metadata, MetadataListener};
use pw::node::{Node, NodeChangeMask, NodeListener};
use pw::properties::properties;
use pw::spa;
use spa::param::audio::{AudioFormat, AudioInfoRaw};
use spa::param::format::{MediaSubtype, MediaType};
use spa::param::{ParamType, format_utils};
use spa::pod::{Object, Pod, Value, serialize::PodSerializer};
use spa::utils::dict::DictRef;
use spa::utils::{Direction, SpaTypes};

/// How long to wait for the server to hand back a negotiated format.
///
/// Reached only when the target vanished between listing and opening, or when
/// nothing can be linked; a live target answers in milliseconds.
const NEGOTIATION_TIMEOUT: Duration = Duration::from_secs(5);

/// What this meter calls its own capture node, so its taps can be told apart
/// from anyone else's when deciding whether a source is in use.
const METER_NODE_NAME: &str = "output-decibel-meter";

/// Longest name kept, in characters.
///
/// Media names carry whole video titles. Cut here rather than in the front
/// ends, so a name that is shown, matched or printed is always the same string.
const NAME_LIMIT: usize = 70;

/// What a node is, which decides how it is tapped and what the numbers mean.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// An output. Tapped at its monitor ports: everything the machine plays.
    Sink,
    /// An input, a microphone or a line in. Tapped at its output ports.
    Source,
    /// A program playing. Tapped at its own output ports, before the mix.
    Program,
}

/// A node of the graph that can be metered.
#[derive(Debug, Clone)]
pub struct GraphNode {
    /// The application and what it plays, or the device as it describes itself.
    pub name: String,
    /// `object.serial`, which the server never reuses, hence a target that
    /// cannot silently become another node between listing and opening.
    pub serial: String,
    /// What kind of node this is.
    pub kind: Kind,
    /// True for the sink or the source the system currently defaults to.
    pub is_default: bool,
    /// True while a program is playing into it, taking from it, or — for a
    /// program — playing into an output.
    ///
    /// Deliberately not PipeWire's own `running` state, which says the node
    /// turns in the graph: metering an output starts it turning, so the state
    /// would answer "running" to the very act of looking at it. What is asked
    /// here is whether audio flows through it *besides* this meter.
    pub is_active: bool,
}

/// What the capture thread needs to answer a `stop` and be waited on.
pub struct Tap {
    quit: Option<pw::channel::Sender<Terminate>>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Tap {
    fn drop(&mut self) {
        if let Some(quit) = self.quit.take() {
            let _ = quit.send(Terminate);
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// A capture in progress, as the shape of its stream plus where samples land.
pub struct Tapped {
    /// Interleaved channel count of the blocks.
    pub channels: u32,
    /// Frames per second of the blocks.
    pub sample_rate: u32,
    /// Blocks of interleaved `f32`, as the tapped node produced them.
    pub blocks: Receiver<Vec<f32>>,
    /// Keeps the capture thread alive; dropping it stops the tap.
    pub tap: Tap,
}

/// The one message the capture thread accepts: stop.
struct Terminate;

/// A live view of the graph, kept up to date by the server's own events.
///
/// Nothing is polled: PipeWire announces a node the moment it appears and again
/// when it changes, so a program that starts playing shows up as it starts.
/// Dropping the watch closes the connection.
pub struct Watch {
    seen: Arc<Mutex<Vec<GraphNode>>>,
    quit: Option<pw::channel::Sender<Terminate>>,
    thread: Option<JoinHandle<()>>,
}

impl Watch {
    /// The graph as it stands: outputs, then programs, then inputs.
    ///
    /// A program appears once per stream it opened, which is deliberate: a
    /// browser playing two tabs, or a table playing music apart from its
    /// effects, is two streams, and they are metered apart.
    pub fn nodes(&self) -> Vec<GraphNode> {
        self.seen.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        if let Some(quit) = self.quit.take() {
            let _ = quit.send(Terminate);
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Start watching the graph, returning once it has been read in full.
pub fn watch() -> Result<Watch> {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (ready_tx, ready_rx) = sync_channel::<Result<(), String>>(1);
    let (quit_tx, quit_rx) = pw::channel::channel::<Terminate>();

    let thread_seen = Arc::clone(&seen);
    let failure = ready_tx.clone();
    let thread = std::thread::Builder::new()
        .name("odm-graph".to_string())
        .spawn(move || {
            if let Err(e) = follow(thread_seen, ready_tx, quit_rx) {
                let _ = failure.try_send(Err(format!("{e:#}")));
            }
        })?;

    let watch = Watch {
        seen,
        quit: Some(quit_tx),
        thread: Some(thread),
    };

    match ready_rx.recv_timeout(NEGOTIATION_TIMEOUT) {
        Ok(Ok(())) => Ok(watch),
        Ok(Err(e)) => Err(anyhow!(e)),
        Err(_) => bail!("PipeWire did not answer within five seconds"),
    }
}

/// The graph, read once. A watch that is dropped as soon as it has answered.
pub fn nodes() -> Result<Vec<GraphNode>> {
    Ok(watch()?.nodes())
}

/// The watching thread: listen, publish, and keep listening.
fn follow(
    seen: Arc<Mutex<Vec<GraphNode>>>,
    ready: SyncSender<Result<(), String>>,
    quit_rx: pw::channel::Receiver<Terminate>,
) -> Result<()> {
    pw::init();

    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;
    let registry = core.get_registry_rc()?;

    let stopped = Rc::new(Cell::new(false));
    let _quit = quit_rx.attach(mainloop.loop_(), {
        let mainloop = mainloop.clone();
        let stopped = Rc::clone(&stopped);
        move |_| {
            stopped.set(true);
            mainloop.quit();
        }
    });

    // Indexed by the global id, because that is what a removal names. The
    // proxies and their listeners live here too: a bound node stops reporting
    // the moment its proxy is dropped.
    let found: Rc<RefCell<HashMap<u32, Found>>> = Rc::new(RefCell::new(HashMap::new()));
    let defaults: Rc<RefCell<Defaults>> = Rc::new(RefCell::new(Defaults::default()));
    let bound: Rc<RefCell<HashMap<u32, (Node, NodeListener)>>> =
        Rc::new(RefCell::new(HashMap::new()));
    // Links say what is actually connected to what, which is the only honest
    // answer to "is anything playing through this".
    let links: Rc<RefCell<HashMap<u32, (u32, u32)>>> = Rc::new(RefCell::new(HashMap::new()));
    let recorders: Rc<RefCell<HashMap<u32, Recorder>>> = Rc::new(RefCell::new(HashMap::new()));
    let watched: Rc<RefCell<Vec<(Metadata, MetadataListener)>>> = Rc::new(RefCell::new(Vec::new()));

    let publish = {
        let seen = Arc::clone(&seen);
        let found = Rc::clone(&found);
        let defaults = Rc::clone(&defaults);
        let links = Rc::clone(&links);
        let recorders = Rc::clone(&recorders);
        move || {
            let nodes = snapshot(
                &found.borrow(),
                &defaults.borrow(),
                &links.borrow(),
                &recorders.borrow(),
            );
            *seen.lock().unwrap_or_else(|e| e.into_inner()) = nodes;
        }
    };
    let publish = Rc::new(publish);

    let _registry_listener = registry
        .add_listener_local()
        .global({
            let registry = registry.clone();
            let found = Rc::clone(&found);
            let defaults = Rc::clone(&defaults);
            let bound = Rc::clone(&bound);
            let watched = Rc::clone(&watched);
            let links = Rc::clone(&links);
            let recorders = Rc::clone(&recorders);
            let publish = Rc::clone(&publish);
            move |global| {
                match global.type_ {
                    pw::types::ObjectType::Node => {}
                    pw::types::ObjectType::Link => {
                        let Some(props) = global.props else { return };
                        let ends = (
                            props.get("link.output.node").and_then(|n| n.parse().ok()),
                            props.get("link.input.node").and_then(|n| n.parse().ok()),
                        );
                        if let (Some(out), Some(into)) = ends {
                            links.borrow_mut().insert(global.id, (out, into));
                            publish();
                        }
                        return;
                    }
                    // Which sink and source the system defaults to is not a
                    // property of any node; it lives in this one metadata.
                    pw::types::ObjectType::Metadata => {
                        let Some(props) = global.props else { return };
                        if props.get("metadata.name") != Some("default") {
                            return;
                        }
                        let Ok(metadata) = registry.bind::<Metadata, _>(global) else {
                            return;
                        };
                        let listener = metadata
                            .add_listener_local()
                            .property({
                                let defaults = Rc::clone(&defaults);
                                let publish = Rc::clone(&publish);
                                move |_subject, key, _type, value| {
                                    let Some(value) = value.and_then(named) else {
                                        return 0;
                                    };
                                    match key {
                                        Some("default.audio.sink") => {
                                            defaults.borrow_mut().sink = Some(value)
                                        }
                                        Some("default.audio.source") => {
                                            defaults.borrow_mut().source = Some(value)
                                        }
                                        _ => return 0,
                                    }
                                    publish();
                                    0
                                }
                            })
                            .register();
                        watched.borrow_mut().push((metadata, listener));
                        return;
                    }
                    _ => return,
                }

                let Some(props) = global.props else { return };
                // Recorders are not metering targets, but they say whether an
                // input is in use — and one of them is this meter.
                if props.get("media.class") == Some("Stream/Input/Audio") {
                    recorders.borrow_mut().insert(
                        global.id,
                        Recorder {
                            is_ours: props.get("node.name") == Some(METER_NODE_NAME),
                        },
                    );
                    publish();
                    return;
                }
                let Some(kind) = kind_of(props) else { return };
                let Some(serial) = props.get("object.serial") else {
                    return;
                };
                let id = global.id;
                let entry = Found {
                    node_name: props.get("node.name").unwrap_or_default().to_string(),
                    // A placeholder only until the node describes itself, which
                    // is where the readable name comes from.
                    name: name_of(props, kind).unwrap_or_else(|| unnamed(kind).to_string()),
                    serial: serial.to_string(),
                    kind,
                    // Only the node itself says so; the registry does not.
                    is_running: false,
                };

                // What the registry already knows, in case the node itself
                // never answers: a listed node beats a missing one. A node
                // announced again keeps the name it has — the registry carries
                // fewer properties than the node itself, so overwriting could
                // only ever lose the readable one.
                {
                    let mut found = found.borrow_mut();
                    match found.get_mut(&id) {
                        Some(known) => {
                            if let Some(name) = name_of(props, kind) {
                                known.name = name;
                            }
                        }
                        None => {
                            found.insert(id, entry.clone());
                        }
                    }
                }
                publish();

                let Ok(node) = registry.bind::<Node, _>(global) else {
                    return;
                };
                let listener = node
                    .add_listener_local()
                    .info({
                        let found = Rc::clone(&found);
                        let publish = Rc::clone(&publish);
                        move |info| {
                            {
                                let mut found = found.borrow_mut();
                                let Some(known) = found.get_mut(&id) else {
                                    return;
                                };
                                known.is_running =
                                    matches!(info.state(), pw::node::NodeState::Running);
                                // An event that only reports a state change
                                // carries no properties. Reading them anyway
                                // would rename every node "unnamed" the first
                                // time it started or stopped playing.
                                if info.change_mask().contains(NodeChangeMask::PROPS)
                                    && let Some(props) = info.props()
                                {
                                    if let Some(name) = name_of(props, kind) {
                                        known.name = name;
                                    }
                                    if let Some(node_name) = props.get("node.name") {
                                        known.node_name = node_name.to_string();
                                    }
                                }
                            }
                            publish();
                        }
                    })
                    .register();
                bound.borrow_mut().insert(id, (node, listener));
            }
        })
        .global_remove({
            let found = Rc::clone(&found);
            let bound = Rc::clone(&bound);
            let links = Rc::clone(&links);
            let recorders = Rc::clone(&recorders);
            let publish = Rc::clone(&publish);
            move |id| {
                let known = found.borrow_mut().remove(&id).is_some()
                    | links.borrow_mut().remove(&id).is_some()
                    | recorders.borrow_mut().remove(&id).is_some();
                if known {
                    bound.borrow_mut().remove(&id);
                    publish();
                }
            }
        })
        .register();

    // Two round trips before answering, because they answer two questions. The
    // first brings every existing global — `done` means "you have seen them
    // all" — and the second waits for what was bound during the first to
    // describe itself. After that the events alone keep the list true.
    roundtrip(&core, &mainloop)?;
    roundtrip(&core, &mainloop)?;
    publish();
    let _ = ready.try_send(Ok(()));

    if !stopped.get() {
        mainloop.run();
    }
    Ok(())
}

/// The collected nodes as a sorted list, defaults and activity resolved.
fn snapshot(
    found: &HashMap<u32, Found>,
    defaults: &Defaults,
    links: &HashMap<u32, (u32, u32)>,
    recorders: &HashMap<u32, Recorder>,
) -> Vec<GraphNode> {
    let is_kind = |id: u32, kind: Kind| found.get(&id).is_some_and(|f| f.kind == kind);
    // A stream that exists is not a stream that plays: a paused tab keeps its
    // node linked to the output, corked, and would otherwise light the whole
    // chain up.
    let playing = |id: u32| {
        found
            .get(&id)
            .is_some_and(|f| f.kind == Kind::Program && f.is_running)
    };
    let active = |id: u32, node: &Found| match node.kind {
        // Something is playing into this output.
        Kind::Sink => links.values().any(|(from, to)| *to == id && playing(*from)),
        // Something other than this meter is recording from this input.
        Kind::Source => links
            .values()
            .any(|(from, to)| *from == id && recorders.get(to).is_some_and(|r| !r.is_ours)),
        // This program is turning, and it is playing into an output rather
        // than only into whoever is metering it.
        Kind::Program => {
            node.is_running
                && links
                    .values()
                    .any(|(from, to)| *from == id && is_kind(*to, Kind::Sink))
        }
    };

    let mut nodes: Vec<GraphNode> = found
        .iter()
        .map(|(id, f)| GraphNode {
            name: f.name.clone(),
            serial: f.serial.clone(),
            kind: f.kind,
            is_default: match f.kind {
                Kind::Sink => defaults.sink.as_deref() == Some(&f.node_name),
                Kind::Source => defaults.source.as_deref() == Some(&f.node_name),
                Kind::Program => false,
            },
            is_active: active(*id, f),
        })
        .collect();

    // Outputs first: they are always there and always mean the same thing.
    // Programs next, inputs last, since a meter is rarely opened for a
    // microphone. Names break ties, so a map's own order never shows through.
    nodes.sort_by(|a, b| {
        let rank = |k: Kind| match k {
            Kind::Sink => 0,
            Kind::Program => 1,
            Kind::Source => 2,
        };
        rank(a.kind)
            .cmp(&rank(b.kind))
            .then_with(|| a.name.cmp(&b.name))
    });
    nodes
}

/// What the graph said about the system's default devices.
#[derive(Debug, Clone, Default)]
struct Defaults {
    sink: Option<String>,
    source: Option<String>,
}

/// A node as it is collected, keeping the `node.name` the defaults refer to.
#[derive(Debug, Clone)]
struct Found {
    node_name: String,
    name: String,
    serial: String,
    kind: Kind,
    is_running: bool,
}

/// A recorder — anything capturing — as the graph announces it.
#[derive(Debug, Clone)]
struct Recorder {
    /// True when it is this meter, whose own tap must not count as activity.
    is_ours: bool,
}

/// The metadata stores `{"name":"alsa_output.…"}`; take the name out of it.
///
/// One key in one shape, read once per listing: a JSON parser would be a
/// dependency for a single pair.
fn named(value: &str) -> Option<String> {
    let rest = value.split_once("\"name\"")?.1;
    let rest = rest.split_once('"')?.1;
    let (name, _) = rest.split_once('"')?;
    Some(name.to_string())
}

/// What kind of node this is, or `None` for anything a meter cannot use.
fn kind_of(props: &DictRef) -> Option<Kind> {
    match props.get("media.class")? {
        "Audio/Sink" => Some(Kind::Sink),
        "Audio/Source" | "Audio/Source/Virtual" => Some(Kind::Source),
        "Stream/Output/Audio" => Some(Kind::Program),
        _ => None,
    }
}

/// Run the loop until the server has answered everything asked so far.
fn roundtrip(core: &pw::core::CoreRc, mainloop: &pw::main_loop::MainLoopRc) -> Result<()> {
    let pending = core.sync(0)?;
    let quit = mainloop.clone();
    let _listener = core
        .add_listener_local()
        .done(move |id, seq| {
            if id == pw::core::PW_ID_CORE && seq == pending {
                quit.quit();
            }
        })
        .register();
    mainloop.run();
    Ok(())
}

/// Start capturing the node with this `object.serial`.
pub fn open(serial: &str, kind: Kind) -> Result<Tapped> {
    let (blocks_tx, blocks) = channel();
    // Rendezvous with the capture thread: it reports the negotiated shape, or
    // why it never got one. Bounded at one, since it is sent exactly once.
    let (shape_tx, shape_rx) = sync_channel::<Result<(u32, u32), String>>(1);
    let (quit_tx, quit_rx) = pw::channel::channel::<Terminate>();

    let target = serial.to_string();
    let failure = shape_tx.clone();
    let thread = std::thread::Builder::new()
        .name("odm-pipewire".to_string())
        .spawn(move || {
            if let Err(e) = run(&target, kind, blocks_tx, shape_tx, quit_rx) {
                let _ = failure.try_send(Err(format!("{e:#}")));
            }
        })?;

    let tap = Tap {
        quit: Some(quit_tx),
        thread: Some(thread),
    };

    match shape_rx.recv_timeout(NEGOTIATION_TIMEOUT) {
        Ok(Ok((channels, sample_rate))) => Ok(Tapped {
            channels,
            sample_rate,
            blocks,
            tap,
        }),
        Ok(Err(e)) => Err(anyhow!(e)),
        // Dropping the tap here stops the thread that never got anywhere.
        Err(_) => bail!(
            "no audio arrived within five seconds — the target may have stopped or be silent to the graph"
        ),
    }
}

/// What the stream callbacks share: the negotiated format, and where to send.
struct Capturing {
    format: AudioInfoRaw,
    blocks: Sender<Vec<f32>>,
    /// Taken on the first usable format, so the shape is announced once.
    shape: Option<SyncSender<Result<(u32, u32), String>>>,
}

/// The capture thread: connect, then run the loop until told to stop.
fn run(
    target: &str,
    kind: Kind,
    blocks: Sender<Vec<f32>>,
    shape: SyncSender<Result<(u32, u32), String>>,
    quit_rx: pw::channel::Receiver<Terminate>,
) -> Result<()> {
    pw::init();

    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;

    let _quit = quit_rx.attach(mainloop.loop_(), {
        let mainloop = mainloop.clone();
        move |_| mainloop.quit()
    });

    let mut props = properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE => "Music",
        *pw::keys::NODE_NAME => METER_NODE_NAME,
        // What to tap. Without it the session manager would connect us to
        // whatever it thinks best, which is never what was asked for.
        *pw::keys::TARGET_OBJECT => target,
    };
    if kind == Kind::Sink {
        // An output has no output ports to record from; what it plays leaves
        // through its monitor ports, and this is what asks for those.
        props.insert(*pw::keys::STREAM_CAPTURE_SINK, "true");
    }

    let stream = pw::stream::StreamBox::new(&core, "output-decibel-meter", props)?;
    let state = Capturing {
        format: AudioInfoRaw::new(),
        blocks,
        shape: Some(shape),
    };

    let _listener = stream
        .add_local_listener_with_user_data(state)
        .param_changed(|_, state, id, param| {
            let Some(param) = param else { return };
            if id != ParamType::Format.as_raw() {
                return;
            }
            let Ok((media_type, media_subtype)) = format_utils::parse_format(param) else {
                return;
            };
            if media_type != MediaType::Audio || media_subtype != MediaSubtype::Raw {
                return;
            }
            if state.format.parse(param).is_err() {
                return;
            }
            let (channels, rate) = (state.format.channels(), state.format.rate());
            if channels == 0 || rate == 0 {
                return;
            }
            if let Some(shape) = state.shape.take() {
                let _ = shape.try_send(Ok((channels, rate)));
            }
        })
        .process(|stream, state| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let Some(data) = buffer.datas_mut().first_mut() else {
                return;
            };
            // Read the chunk's bounds before borrowing the memory it describes:
            // a buffer may be handed over partly filled, or filled from an
            // offset, and reading past that is reading stale audio.
            let (offset, size) = {
                let chunk = data.chunk();
                (chunk.offset() as usize, chunk.size() as usize)
            };
            let Some(bytes) = data.data() else { return };
            let start = offset.min(bytes.len());
            let end = start.saturating_add(size).min(bytes.len());
            let samples: Vec<f32> = bytes[start..end]
                .chunks_exact(std::mem::size_of::<f32>())
                .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                .collect();
            if !samples.is_empty() {
                let _ = state.blocks.send(samples);
            }
        })
        .register()?;

    // Ask for `f32` and let rate and channels stay open, so the graph's own
    // shape comes through: forcing a channel count would have the server upmix
    // or downmix, and a mono track duplicated into stereo reads 3 LU louder
    // than it is.
    let mut wanted = AudioInfoRaw::new();
    wanted.set_format(AudioFormat::F32LE);
    let pod = PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &Value::Object(Object {
            type_: SpaTypes::ObjectParamFormat.as_raw(),
            id: ParamType::EnumFormat.as_raw(),
            properties: wanted.into(),
        }),
    )
    .map_err(|e| anyhow!("building the format request: {e}"))?
    .0
    .into_inner();
    let mut params = [Pod::from_bytes(&pod).ok_or_else(|| anyhow!("malformed format request"))?];

    stream.connect(
        Direction::Input,
        None,
        pw::stream::StreamFlags::AUTOCONNECT
            | pw::stream::StreamFlags::MAP_BUFFERS
            | pw::stream::StreamFlags::RT_PROCESS,
        &mut params,
    )?;

    mainloop.run();
    Ok(())
}

/// A readable name for a node, or `None` when nothing in these properties names
/// it — which happens on events that report something else entirely.
///
/// A device says what it is in `node.description` — "Analog Stereo", the string
/// the system settings show. A program is worth naming by both halves, since
/// "Firefox" alone does not say which tab and a track title alone does not say
/// which program, but only when they differ.
fn name_of(props: &DictRef, kind: Kind) -> Option<String> {
    let name = match kind {
        Kind::Sink | Kind::Source => props
            .get("node.description")
            .or_else(|| props.get("node.nick"))
            .or_else(|| props.get("node.name"))?
            .to_string(),
        Kind::Program => {
            let application = props
                .get("application.name")
                .or_else(|| props.get("node.name"))?;
            match props.get("media.name") {
                Some(media) if !media.is_empty() && media != application => {
                    format!("{application} — {media}")
                }
                _ => application.to_string(),
            }
        }
    };
    (!name.is_empty()).then(|| shorten(&name))
}

/// What to show until a node has said what it is called.
fn unnamed(kind: Kind) -> &'static str {
    match kind {
        Kind::Sink | Kind::Source => "unnamed device",
        Kind::Program => "unnamed program",
    }
}

/// Cut a name to [`NAME_LIMIT`] characters, marking that it was cut.
fn shorten(name: &str) -> String {
    if name.chars().count() <= NAME_LIMIT {
        return name.to_string();
    }
    let kept: String = name.chars().take(NAME_LIMIT - 1).collect();
    format!("{}…", kept.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A node as the collector would hold it.
    fn node(kind: Kind, is_running: bool) -> Found {
        Found {
            node_name: "node".to_string(),
            name: "Something".to_string(),
            serial: "1".to_string(),
            kind,
            is_running,
        }
    }

    /// Run the activity rules over a graph given as (nodes, links, recorders).
    fn activity(
        nodes: &[(u32, Found)],
        links: &[(u32, u32)],
        recorders: &[(u32, bool)],
    ) -> Vec<bool> {
        let found: HashMap<u32, Found> = nodes.iter().cloned().collect();
        let links: HashMap<u32, (u32, u32)> = links
            .iter()
            .enumerate()
            .map(|(i, ends)| (1000 + i as u32, *ends))
            .collect();
        let recorders: HashMap<u32, Recorder> = recorders
            .iter()
            .map(|(id, is_ours)| (*id, Recorder { is_ours: *is_ours }))
            .collect();

        let snapshot = snapshot(&found, &Defaults::default(), &links, &recorders);
        nodes
            .iter()
            .map(|(_, f)| {
                snapshot
                    .iter()
                    .find(|n| n.serial == f.serial && n.kind == f.kind)
                    .is_some_and(|n| n.is_active)
            })
            .collect()
    }

    #[test]
    fn metering_an_output_does_not_make_it_active() {
        // The whole point: this meter links itself to the sink's monitor, which
        // starts the node turning. Nothing is playing, so nothing is active.
        let sink = node(Kind::Sink, true);
        assert_eq!(activity(&[(1, sink)], &[(1, 2)], &[(2, true)]), [false]);
    }

    #[test]
    fn an_output_a_program_plays_into_is_active() {
        let program = Found {
            serial: "2".to_string(),
            ..node(Kind::Program, true)
        };
        let sink = node(Kind::Sink, true);
        assert_eq!(
            activity(&[(1, sink), (2, program)], &[(2, 1)], &[]),
            [true, true]
        );
    }

    #[test]
    fn a_paused_program_lights_nothing() {
        // A tab on pause keeps its node linked to the output, corked.
        let program = Found {
            serial: "2".to_string(),
            ..node(Kind::Program, false)
        };
        let sink = node(Kind::Sink, true);
        assert_eq!(
            activity(&[(1, sink), (2, program)], &[(2, 1)], &[]),
            [false, false]
        );
    }

    #[test]
    fn an_input_is_active_only_for_someone_else() {
        let source = node(Kind::Source, true);
        assert_eq!(
            activity(&[(1, source.clone())], &[(1, 2)], &[(2, true)]),
            [false],
            "this meter recording it is not the input being in use"
        );
        assert_eq!(activity(&[(1, source)], &[(1, 2)], &[(2, false)]), [true]);
    }

    #[test]
    fn a_short_name_is_left_alone() {
        assert_eq!(shorten("Firefox"), "Firefox");
    }

    #[test]
    fn a_long_name_is_cut_to_the_limit() {
        let long = "a".repeat(NAME_LIMIT + 20);
        let cut = shorten(&long);
        assert_eq!(cut.chars().count(), NAME_LIMIT);
        assert!(cut.ends_with('…'));
    }

    #[test]
    fn the_default_sink_is_read_out_of_its_metadata() {
        assert_eq!(
            named(r#"{"name":"alsa_output.pci-0000_00_1f.3.analog-stereo"}"#).as_deref(),
            Some("alsa_output.pci-0000_00_1f.3.analog-stereo")
        );
        assert_eq!(named("null"), None);
    }
}
