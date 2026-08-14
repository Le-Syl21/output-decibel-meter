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
| device | Linux | the monitor ports of a sink | **depends on the setup** |
| application | Linux | the program's own output ports | no |

"Depends on the setup" is meant literally: a sink's monitor carries the system
volume when the volume is applied in software, and does not when the card
applies it. This machine, PipeWire on an HDA codec, does not — the tone reads
−20.0 dBTP at 10 % volume as at 34 %. Another machine may differ, which is
exactly why the tools say `depends` rather than picking an answer.

Device capture is the simple one and it is enough to compare before and after
on a single machine. Application capture is the one whose readings can be
compared **between** machines, because the tap sits upstream of any device
volume — the same audio the program wrote, whatever the speakers do with it
afterwards.

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

# one program rather than the whole machine — any part of its name will do
output-decibel-meter --source "Firefox"
```

```
$ output-decibel-meter --list
  [output] active   Audio interne Stéréo analogique
  [output] idle     HyperX 7.1 Audio Stéréo numérique (IEC958)
  [app]    active   Firefox — Panneaux solaires à brancher sur une prise
  [app]    idle     speech-dispatcher-dummy — playback
  [input]  idle     Audio interne Stéréo analogique
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
makes an A/B comparison possible. Each figure carries the window it was taken
over, since three loudness numbers that differ only by how far back they look
cannot be read without it.

The sources are a table, sortable on any column, and it is live: PipeWire
announces a node as it appears, so a program shows up in the list the moment it
starts playing. The `state` column says whether audio flows through a source
*besides this meter* — metering an output would otherwise start it turning and
report "running" to the very act of looking at it.

```sh
cargo run --release --features gui --bin output-decibel-meter-gui
```

## Checking a machine

```sh
output-decibel-meter --self-test
```

It plays a 1 kHz tone at a known level, meters it back, and prints what it read
against what it played:

```
metering PipeWire ALSA [output-decibel-meter] — ALSA Playback
  this process's own stream — anything else may keep playing

  integrated loudness                  -20.0 LUFS  expected  -20.0 ± 0.5   ok
  true peak                            -20.0 dBTP  expected  -20.0 ± 0.5   ok
  drop when the source drops 6 dB        6.0 dB    expected    6.0 ± 0.5   ok
```

It meters *itself*: the tone is played by the process running the check and
tapped at that process's own output, found in the graph by its process id. So
nothing else has to stop — a video can keep playing and the figures do not move.
Where programs cannot be tapped it falls back to the output, and then the
machine does have to be quiet; it says which of the two it did.

The third line is a difference rather than a level, which is what makes it true
whatever the path does to the absolute gain: drop the source by 6 dB and the
reading has to drop by 6 dB.

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

The library pulls in `cpal` and `ebur128`, plus `pipewire` on Linux; `egui` only
arrives with the `gui` feature.

## Status

On Linux both modes work, and both go through PipeWire: what plays is a node in
one graph, and the meter is a node linked either to a sink's monitor ports or to
a program's own output ports. Against a −20 dBFS reference tone, both read
−20.0 LUFS and −20.0 dBTP; the program tap stays there with the system volume
pulled down to 5 %, which is the property that mode exists for.

The graph is also what gets listed, and that is not a detail. Asked through ALSA
for the default output opened for capture, the machine hands over the default
*input* — the meter reads the microphone and reports −65 LUFS while the speakers
play a −20 dBFS tone. Nothing in the numbers says so. Going through the graph
removes the ambiguity, and incidentally lists each device once instead of once
per access path.

Windows and macOS use device loopback through `cpal`, which is right there, and
list no programs yet. The mechanisms exist — WASAPI process loopback, Core Audio
process taps — and the shape of the code is ready for them: something lists,
something taps, everything above works on blocks of `f32`.

## Building

```sh
cargo build --release                 # library + CLI
cargo build --release --features gui  # and the meter window
```

Linux needs `libasound2-dev`, plus `libgtk-3-dev` for the GUI. The PipeWire
backend is on by default, so it also needs `libpipewire-0.3-dev` and `clang`;
`--no-default-features` drops both, and with them per-program capture and the
graph listing, leaving device capture through ALSA.
