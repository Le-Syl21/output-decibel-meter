//! Measure the loudness of what a program is actually playing.
//!
//! Plenty of Rust crates measure a buffer you hand them, or capture a
//! microphone. This one answers a different question: how loud is what leaves
//! this application, right now — and did my change to it make any difference?
//!
//! Two capture modes, because they are not equivalent:
//!
//! - **device**, a loopback of an output: everything the machine plays, mixed.
//!   Available everywhere, but the point of measurement differs per platform,
//!   and on Linux it depends on how that machine applies its volume.
//! - **application**, tapping one program's stream. Upstream of any device
//!   volume, which makes readings comparable between machines.
//!
//! On Linux both go through PipeWire, where what plays is a node in a graph and
//! a meter is a node linked to it. On Windows and macOS, `cpal` provides device
//! loopback and no program can be tapped yet.
//!
//! Everything is reported on two scales, both referenced to digital full scale
//! and therefore negative: **LUFS** for perceived loudness, gated as EBU R128
//! prescribes, and **dBTP** for true peak, which is what tells you whether it
//! clips.

pub mod capture;
#[cfg(all(target_os = "linux", feature = "pipewire"))]
pub mod graph;
pub mod meter;

pub use capture::{CaptureMode, Source, sources};
pub use meter::{Meter, Reading};
