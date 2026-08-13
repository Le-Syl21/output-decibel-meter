# output-decibel-meter

Live loudness metering of what a program actually outputs — per-application or
per-device capture on Linux, Windows and macOS, reported as EBU R128 loudness
and true peak. Pure Rust, with an optional egui meter.

## Why

Plenty of crates measure a buffer you hand them, or capture a microphone. This
one answers a different question: **how loud is what leaves this application,
right now — and did my change to it make any difference?**

That question comes up whenever a setting, a mix, a plugin or a config file is
supposed to have changed the level, and the only way to check is to listen and
guess. Two numbers settle it.

## The two scales

Both are referenced to digital full scale, so both are negative below it.

| | What it says | Read it for |
|---|---|---|
| **LUFS** | perceived loudness, gated as EBU R128 prescribes | is it *loud* |
| **dBTP** | true peak, measured with oversampling | does it *clip* |

Three loudness windows are reported: **momentary** (400 ms) follows the action,
**short term** (3 s) is what a listener calls "the level", and **integrated**
accumulates since the last reset — the one to compare two runs on, because its
gate ignores the silences between sounds.

## Where the tap sits

This matters more than it looks: a loudness figure means nothing without the
point it was taken at, and that point is not the same everywhere.

| Mode | Platform | Captured at | System volume included |
|---|---|---|---|
| device | Windows | WASAPI loopback of the render mix | no |
| device | macOS | Core Audio process tap | no |
| device | Linux | monitor of a sink | **depends on the setup** |
| application | all three | the program's own stream | no |

Device capture is the simple one and it is enough to compare before and after
on a single machine. Application capture is the one whose readings can be
compared **between** machines, because the tap sits at the same place on all
three platforms, upstream of any device volume.

The tools print which of these applies on every run rather than leaving it to
be guessed.

## Usage

```sh
# what can be metered here
output-decibel-meter --list

# the default output, in loopback, until Ctrl+C
output-decibel-meter

# a named source, for thirty seconds
output-decibel-meter --source "HyperX" --seconds 30
```

```
metering Audio interne Stéréo analogique
  output device, captured in loopback — the system volume may be included…
2 channels at 48000 Hz

     1.0s   M  -23.4   S  -24.1   I  -23.8 LUFS   peak  -11.2 dBTP
     2.0s   M  -19.8   S  -22.0   I  -22.6 LUFS   peak   -9.4 dBTP
```

The meter window adds a level bar with a falling peak marker, a scrolling graph
of short-term loudness with the peak line above it, and a reset button that
starts the figures again **without interrupting the capture** — which is what
makes an A/B comparison possible.

```sh
cargo run --release --features gui --bin output-decibel-meter-gui
```

## As a library

```rust
use output_decibel_meter::{capture, meter::Meter};
use std::time::Duration;

let source = capture::default_output()?;
let capture = source.open()?;
let mut meter = Meter::new(capture.channels, capture.sample_rate)?;

while let Some(block) = capture.next_block(Duration::from_millis(200)) {
    let reading = meter.add(&block)?;
    println!("{:.1} LUFS, peak {:.1} dBTP", reading.integrated, reading.true_peak);
}
# Ok::<(), anyhow::Error>(())
```

The library pulls in `cpal` and `ebur128` and nothing else; `egui` only arrives
with the `gui` feature.

## Status

Device capture works on the three platforms. Application capture is
**not implemented yet** — the mechanisms exist on all three (PipeWire port
links, WASAPI process loopback, Core Audio process taps) and Linux comes first.
Until then the mode is declared and refused rather than silently absent.

## Building

```sh
cargo build --release                 # library + CLI
cargo build --release --features gui  # and the meter window
```

Linux needs `libasound2-dev`, plus `libgtk-3-dev` for the GUI.
