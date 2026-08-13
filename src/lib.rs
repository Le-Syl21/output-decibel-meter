//! Measure the loudness of what a program is actually playing.
//!
//! Plenty of Rust crates measure a buffer you hand them, or capture a
//! microphone. This one answers a different question: how loud is what leaves
//! this application, right now — and did my change to it make any difference?
//!
//! Two capture modes, because they are not equivalent:
//!
//! - **device**, a loopback of an output. Simple, available everywhere through
//!   `cpal`, but the point of measurement differs per platform: on Linux it
//!   sits *after* the system volume, on Windows and macOS *before* it.
//! - **application**, tapping one program's stream. The same point of
//!   measurement on all three platforms, upstream of any device volume, which
//!   makes readings comparable between machines.
//!
//! Everything is reported on two scales, both referenced to digital full scale
//! and therefore negative: **LUFS** for perceived loudness, gated as EBU R128
//! prescribes, and **dBTP** for true peak, which is what tells you whether it
//! clips.

pub mod capture;
pub mod meter;

pub use capture::{CaptureMode, Source, sources};
pub use meter::{Meter, Reading};
