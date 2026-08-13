//! Turning captured samples into readings.
//!
//! Nothing here knows where the audio came from — it takes interleaved `f32`
//! frames and keeps an EBU R128 state up to date. Both scales it reports are
//! referenced to digital full scale, so both are negative below it.

use anyhow::Result;
use ebur128::{EbuR128, Mode};

/// What the meter says at one moment.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Reading {
    /// Loudness over the last 400 ms, in LUFS. Follows the action.
    pub momentary: f64,
    /// Loudness over the last 3 s, in LUFS. What a listener calls "the level".
    pub short_term: f64,
    /// Gated loudness since the last reset, in LUFS. The figure to compare two
    /// runs on, because it ignores the silences between sounds.
    pub integrated: f64,
    /// Loudest true peak since the last reset, in dBTP. `0` is full scale.
    pub true_peak: f64,
    /// True peak of the last block alone, in dBTP, for a falling indicator.
    pub recent_peak: f64,
    /// Seconds of audio measured since the last reset.
    pub seconds: f64,
}

impl Default for Reading {
    fn default() -> Self {
        Self {
            momentary: f64::NEG_INFINITY,
            short_term: f64::NEG_INFINITY,
            integrated: f64::NEG_INFINITY,
            true_peak: f64::NEG_INFINITY,
            recent_peak: f64::NEG_INFINITY,
            seconds: 0.0,
        }
    }
}

/// Accumulates loudness over a stream of blocks.
pub struct Meter {
    meter: EbuR128,
    channels: u32,
    sample_rate: u32,
    frames: u64,
    peak_hold: f64,
}

const MODE: Mode = Mode::M.union(Mode::S).union(Mode::I).union(Mode::TRUE_PEAK);

impl Meter {
    /// A meter for a stream of the given shape.
    pub fn new(channels: u32, sample_rate: u32) -> Result<Self> {
        Ok(Self {
            meter: EbuR128::new(channels, sample_rate, MODE)?,
            channels,
            sample_rate,
            frames: 0,
            peak_hold: f64::NEG_INFINITY,
        })
    }

    /// Forget everything measured so far, keeping the stream shape.
    ///
    /// This is what a reset button does: the capture carries on, only the
    /// figures start again — which is the whole point when comparing a change
    /// against what came before it.
    pub fn reset(&mut self) -> Result<()> {
        self.meter = EbuR128::new(self.channels, self.sample_rate, MODE)?;
        self.frames = 0;
        self.peak_hold = f64::NEG_INFINITY;
        Ok(())
    }

    /// Feed one block of interleaved samples and read the meter.
    pub fn add(&mut self, block: &[f32]) -> Result<Reading> {
        if block.is_empty() {
            return Ok(self.reading(f64::NEG_INFINITY));
        }
        self.meter.add_frames_f32(block)?;
        self.frames += block.len() as u64 / self.channels.max(1) as u64;

        // True peak is cumulative in ebur128, so the block's own peak is read
        // from the sample values; it only feeds the falling indicator.
        let block_peak = block.iter().fold(0.0f32, |worst, s| worst.max(s.abs()));
        let recent = if block_peak > 0.0 {
            20.0 * (block_peak as f64).log10()
        } else {
            f64::NEG_INFINITY
        };

        for channel in 0..self.channels {
            let peak = self.meter.true_peak(channel)?;
            let dbtp = if peak > 0.0 {
                20.0 * peak.log10()
            } else {
                f64::NEG_INFINITY
            };
            if dbtp > self.peak_hold {
                self.peak_hold = dbtp;
            }
        }

        Ok(self.reading(recent))
    }

    /// Seconds of audio measured since the last reset.
    pub fn seconds(&self) -> f64 {
        self.frames as f64 / self.sample_rate.max(1) as f64
    }

    fn reading(&self, recent_peak: f64) -> Reading {
        Reading {
            momentary: self.meter.loudness_momentary().unwrap_or(f64::NEG_INFINITY),
            short_term: self.meter.loudness_shortterm().unwrap_or(f64::NEG_INFINITY),
            integrated: self.meter.loudness_global().unwrap_or(f64::NEG_INFINITY),
            true_peak: self.peak_hold,
            recent_peak,
            seconds: self.seconds(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::TAU;

    /// One second of a 1 kHz sine at a known peak, interleaved stereo.
    fn tone(peak_dbfs: f64, rate: u32) -> Vec<f32> {
        let amp = 10.0_f64.powf(peak_dbfs / 20.0);
        (0..rate)
            .flat_map(|i| {
                let v = (amp * (TAU * 1000.0 * i as f64 / rate as f64).sin()) as f32;
                [v, v]
            })
            .collect()
    }

    #[test]
    fn a_known_tone_reads_back_at_its_own_level() {
        // A 1 kHz sine is where K-weighting is flat and the LUFS calibration
        // cancels the 3 dB between peak and rms, so -20 dBFS reads -20 LUFS.
        let mut meter = Meter::new(2, 48000).unwrap();
        for _ in 0..4 {
            meter.add(&tone(-20.0, 48000)).unwrap();
        }
        let r = meter.add(&tone(-20.0, 48000)).unwrap();
        assert!(
            (r.integrated - -20.0).abs() < 0.3,
            "integrated was {}",
            r.integrated
        );
        assert!(
            (r.true_peak - -20.0).abs() < 0.3,
            "true peak was {}",
            r.true_peak
        );
    }

    #[test]
    fn reset_forgets_the_loud_part() {
        let mut meter = Meter::new(2, 48000).unwrap();
        for _ in 0..4 {
            meter.add(&tone(-6.0, 48000)).unwrap();
        }
        meter.reset().unwrap();
        for _ in 0..4 {
            meter.add(&tone(-30.0, 48000)).unwrap();
        }
        let r = meter.add(&tone(-30.0, 48000)).unwrap();
        assert!(
            (r.integrated - -30.0).abs() < 0.3,
            "integrated was {}",
            r.integrated
        );
        assert!(r.true_peak < -25.0, "true peak was {}", r.true_peak);
        assert!((r.seconds - 5.0).abs() < 0.01);
    }
}
