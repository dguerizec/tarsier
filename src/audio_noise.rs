//! In-memory stationary-noise profiling and stereo overlap-add suppression.
//! A 40 ms sine window with a 20 ms hop reconstructs unity gain exactly.
use rustfft::{Fft, FftPlanner, num_complex::Complex};
use serde::{Deserialize, Serialize};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

const HOP: usize = 960;
const SIZE: usize = HOP * 2;
const BINS: usize = SIZE / 2 + 1;
const PROFILE_BLOCKS: usize = 150;

#[derive(Clone, Serialize)]
pub struct Status {
    pub source: Option<String>,
    pub enabled: bool,
    pub ready: bool,
    pub analyzing: bool,
    pub progress: f32,
    pub strength: f32,
    pub error: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub source: String,
    pub action: Action,
    pub strength: Option<f32>,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Analyze,
    Cancel,
    Enable,
    Disable,
    Strength,
}

pub struct NoiseReducer {
    status: Status,
    fft: Arc<dyn Fft<f32>>,
    inverse: Arc<dyn Fft<f32>>,
    window: Vec<f32>,
    previous: [Vec<f32>; 2],
    overlap: [Vec<f32>; 2],
    gains: Vec<f32>,
    profile: Vec<f32>,
    sum: Vec<f32>,
    blocks: usize,
    last_audio: Option<Instant>,
}

impl Default for NoiseReducer {
    fn default() -> Self {
        let mut planner = FftPlanner::new();
        Self {
            status: Status {
                source: None,
                enabled: false,
                ready: false,
                analyzing: false,
                progress: 0.0,
                strength: 0.65,
                error: None,
            },
            fft: planner.plan_fft_forward(SIZE),
            inverse: planner.plan_fft_inverse(SIZE),
            window: (0..SIZE)
                .map(|i| (std::f32::consts::PI * (i as f32 + 0.5) / SIZE as f32).sin())
                .collect(),
            previous: std::array::from_fn(|_| vec![0.0; HOP]),
            overlap: std::array::from_fn(|_| vec![0.0; HOP]),
            gains: vec![1.0; BINS],
            profile: vec![0.0; BINS],
            sum: vec![0.0; BINS],
            blocks: 0,
            last_audio: None,
        }
    }
}

impl NoiseReducer {
    pub fn select(&mut self, source: Option<&str>) {
        if self.status.source.as_deref() != source {
            self.status.source = source.map(str::to_owned);
            self.status.enabled = false;
            self.status.ready = false;
            self.status.analyzing = false;
            self.status.progress = 0.0;
            self.status.error = None;
            self.profile.fill(0.0);
            self.reset_stream();
        }
    }

    fn reset_stream(&mut self) {
        for channel in &mut self.previous {
            channel.fill(0.0);
        }
        for channel in &mut self.overlap {
            channel.fill(0.0);
        }
        self.gains.fill(1.0);
        self.last_audio = None;
    }

    pub fn interrupt(&mut self) {
        if self.status.analyzing {
            self.status.analyzing = false;
            self.status.error =
                Some("Analysis interrupted. Check the microphone and try again.".into());
        }
        self.reset_stream();
    }

    /// A single missed capture tick is scheduling jitter, not a failed analysis.
    /// Discard overlap so delayed samples cannot leak into the next output block.
    pub fn gap(&mut self) {
        let last_audio = self.last_audio;
        if last_audio.is_none_or(|time| time.elapsed() > Duration::from_millis(120)) {
            self.interrupt();
        } else {
            self.reset_stream();
            self.last_audio = last_audio;
        }
    }

    pub fn status(&mut self) -> Status {
        if self.status.analyzing
            && self
                .last_audio
                .is_none_or(|t| t.elapsed() > Duration::from_millis(500))
        {
            self.interrupt();
        }
        self.status.clone()
    }

    pub fn command(
        &mut self,
        action: Action,
        strength: Option<f32>,
    ) -> Result<Status, &'static str> {
        if let Some(value) = strength
            && (!value.is_finite() || !(0.0..=1.0).contains(&value))
        {
            return Err("Strength must be between 0 and 1");
        }
        match action {
            Action::Analyze => {
                if self.status.analyzing {
                    return Err("Analysis is already running");
                }
                self.status.analyzing = true;
                self.status.progress = 0.0;
                self.status.error = None;
                self.sum.fill(0.0);
                self.blocks = 0;
                self.last_audio = Some(Instant::now());
            }
            Action::Cancel => {
                self.status.analyzing = false;
                self.status.progress = 0.0;
            }
            Action::Enable => {
                if !self.status.ready {
                    return Err("Analyze the ambient noise first");
                }
                self.status.enabled = true;
            }
            Action::Disable => self.status.enabled = false,
            Action::Strength => {
                if strength.is_none() {
                    return Err("Strength is required");
                }
            }
        }
        if let Some(value) = strength {
            self.status.strength = value;
        }
        Ok(self.status.clone())
    }

    /// Called only with fresh, authorized input; mute is applied downstream.
    pub fn process(&mut self, pcm: &[u8]) -> Vec<u8> {
        if pcm.len() != HOP * 4 {
            self.interrupt();
            return vec![0; HOP * 4];
        }
        self.last_audio = Some(Instant::now());
        let mut spectra: [Vec<Complex<f32>>; 2] =
            std::array::from_fn(|_| vec![Complex::default(); SIZE]);
        for (channel, spectrum) in spectra.iter_mut().enumerate() {
            for i in 0..HOP {
                let sample =
                    i16::from_le_bytes([pcm[i * 4 + channel * 2], pcm[i * 4 + channel * 2 + 1]])
                        as f32
                        / 32768.0;
                spectrum[i].re = self.previous[channel][i] * self.window[i];
                spectrum[i + HOP].re = sample * self.window[i + HOP];
                self.previous[channel][i] = sample;
            }
            self.fft.process(spectrum);
        }
        let power: Vec<f32> = (0..BINS)
            .map(|i| spectra[0][i].norm_sqr().max(spectra[1][i].norm_sqr()))
            .collect();
        if self.status.analyzing {
            // Discard the first half-window so the profile uses full real frames.
            if self.blocks > 0 {
                for (sum, power) in self.sum.iter_mut().zip(&power) {
                    *sum += power;
                }
            }
            self.blocks += 1;
            self.status.progress = ((self.blocks - 1) as f32 / PROFILE_BLOCKS as f32).min(1.0);
            if self.blocks > PROFILE_BLOCKS {
                self.status.analyzing = false;
                if self.sum.iter().sum::<f32>() / (PROFILE_BLOCKS as f32) < 0.0001 {
                    self.status.error = Some(
                        "No audible noise detected. Check that the input is not muted.".into(),
                    );
                } else {
                    for (profile, sum) in self.profile.iter_mut().zip(&self.sum) {
                        *profile = sum / PROFILE_BLOCKS as f32;
                    }
                    self.status.ready = true;
                }
            }
        }
        let active = self.status.enabled && self.status.ready;
        let floor = 10.0_f32.powf(-24.0 * self.status.strength / 20.0);
        for (i, power) in power.iter().enumerate() {
            let target = if active {
                (1.0 - (1.0 + self.status.strength * 2.0) * self.profile[i] / power.max(1e-12))
                    .max(0.0)
                    .sqrt()
                    .max(floor)
            } else {
                1.0
            };
            // Shared stereo mask preserves phase and avoids channel cancellation.
            self.gains[i] = 0.65 * self.gains[i] + 0.35 * target;
        }
        let mut output = vec![0; HOP * 4];
        for (channel, spectrum) in spectra.iter_mut().enumerate() {
            for (i, bin) in spectrum.iter_mut().enumerate() {
                *bin *= self.gains[i.min(SIZE - i)];
            }
            self.inverse.process(spectrum);
            for i in 0..HOP {
                let sample =
                    spectrum[i].re / SIZE as f32 * self.window[i] + self.overlap[channel][i];
                self.overlap[channel][i] =
                    spectrum[i + HOP].re / SIZE as f32 * self.window[i + HOP];
                let bytes = (sample * 32768.0).round().clamp(-32768.0, 32767.0) as i16;
                output[i * 4 + channel * 2..i * 4 + channel * 2 + 2]
                    .copy_from_slice(&bytes.to_le_bytes());
            }
        }
        output
    }
}

/// Log-frequency display: 80 bands from 50 Hz to 20 kHz, fixed -90..0 dBFS.
/// Power is measured independently per channel to preserve opposite-phase stereo.
pub struct Spectrum {
    fft: Arc<dyn Fft<f32>>,
}
impl Default for Spectrum {
    fn default() -> Self {
        Self {
            fft: FftPlanner::new().plan_fft_forward(HOP),
        }
    }
}
impl Spectrum {
    pub fn measure(&self, pcm: &[u8]) -> Vec<f32> {
        let mut powers = vec![0.0_f32; HOP / 2 + 1];
        for channel in 0..2 {
            let mut data = vec![Complex::default(); HOP];
            for (i, frame) in pcm.chunks_exact(4).take(HOP).enumerate() {
                data[i].re = i16::from_le_bytes([frame[channel * 2], frame[channel * 2 + 1]])
                    as f32
                    / 32768.0
                    * (0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / HOP as f32).cos());
            }
            self.fft.process(&mut data);
            for (power, bin) in powers.iter_mut().zip(data) {
                *power = power.max(bin.norm_sqr());
            }
        }
        (0..80)
            .map(|band| {
                let low = (50.0 * 400.0_f32.powf(band as f32 / 80.0) / 50.0).round() as usize;
                let high =
                    (50.0 * 400.0_f32.powf((band + 1) as f32 / 80.0) / 50.0).round() as usize;
                let power = powers[low.min(HOP / 2)..high.max(low + 1).min(HOP / 2 + 1)]
                    .iter()
                    .copied()
                    .fold(0.0_f32, f32::max);
                (10.0 * (power * (4.0 / HOP as f32).powi(2)).max(1e-9).log10()).clamp(-90.0, 0.0)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn tone(block: usize, noise: f32, voice: f32) -> Vec<u8> {
        (0..HOP)
            .flat_map(|i| {
                let time = (block * HOP + i) as f32 / 48000.0;
                let sample = ((noise * (std::f32::consts::TAU * 1000.0 * time).sin()
                    + voice * (std::f32::consts::TAU * 3000.0 * time).sin())
                    * 32767.0) as i16;
                // Opposite phase must not disappear from profiling or display.
                [sample.to_le_bytes(), (-sample).to_le_bytes()].concat()
            })
            .collect()
    }
    fn amplitude(pcm: &[u8], frequency: f32) -> f32 {
        let mut sum = Complex::<f32>::default();
        for (i, frame) in pcm.chunks_exact(4).enumerate() {
            let sample = i16::from_le_bytes([frame[0], frame[1]]) as f32 / 32768.0;
            sum += Complex::from_polar(
                sample,
                std::f32::consts::TAU * frequency * i as f32 / 48000.0,
            );
        }
        sum.norm() * 2.0 / HOP as f32
    }
    #[test]
    fn bypass_reconstructs_stereo_with_one_hop_delay() {
        let mut reducer = NoiseReducer::default();
        let mut previous = vec![0; HOP * 4];
        for block in 0..20 {
            let pcm = tone(block, 0.05, 0.1);
            let output = reducer.process(&pcm);
            for (actual, expected) in output.chunks_exact(2).zip(previous.chunks_exact(2)) {
                let actual = i16::from_le_bytes([actual[0], actual[1]]) as i32;
                let expected = i16::from_le_bytes([expected[0], expected[1]]) as i32;
                assert!((actual - expected).abs() <= 1);
            }
            previous = pcm;
        }
    }
    #[test]
    fn learned_noise_is_reduced_while_unprofiled_signal_survives() {
        let mut reducer = NoiseReducer::default();
        reducer.select(Some("mic"));
        reducer.command(Action::Analyze, None).unwrap();
        for block in 0..=PROFILE_BLOCKS {
            reducer.process(&tone(block, 0.05, 0.0));
        }
        assert!(reducer.status().ready);
        assert!(!reducer.status().enabled);
        reducer.command(Action::Enable, Some(0.8)).unwrap();
        let mut output = Vec::new();
        for block in 151..181 {
            output = reducer.process(&tone(block, 0.05, 0.1));
        }
        assert!(amplitude(&output, 1000.0) < 0.009, "noise attenuation");
        assert!(
            (amplitude(&output, 3000.0) - 0.1).abs() < 0.005,
            "unprofiled signal preserved"
        );
        reducer.command(Action::Disable, None).unwrap();
        for block in 181..211 {
            output = reducer.process(&tone(block, 0.05, 0.1));
        }
        assert!((amplitude(&output, 1000.0) - 0.05).abs() < 0.001);
        reducer.select(Some("other"));
        assert!(!reducer.status().ready);
        assert!(!reducer.status().enabled);
    }
    #[test]
    fn silence_interrupts_and_invalid_commands_never_create_a_profile() {
        let mut reducer = NoiseReducer::default();
        assert!(reducer.command(Action::Enable, None).is_err());
        assert!(reducer.command(Action::Strength, Some(f32::NAN)).is_err());
        assert!(reducer.command(Action::Strength, Some(2.0)).is_err());
        reducer.command(Action::Analyze, None).unwrap();
        for _ in 0..=PROFILE_BLOCKS {
            reducer.process(&vec![0; HOP * 4]);
        }
        assert!(!reducer.status().ready);
        assert!(reducer.status().error.is_some());
        reducer.command(Action::Analyze, None).unwrap();
        reducer.gap();
        assert!(reducer.status().analyzing);
        reducer.last_audio = Some(Instant::now() - Duration::from_secs(1));
        reducer.gap();
        assert!(!reducer.status().analyzing);
        assert!(!reducer.status().ready);
        reducer.command(Action::Analyze, None).unwrap();
        reducer.command(Action::Cancel, None).unwrap();
        assert!(!reducer.status().analyzing);
    }
    #[test]
    fn stationary_broadband_noise_loses_at_least_six_decibels() {
        let mut seed = 12345_u32;
        let mut block = || -> Vec<u8> {
            (0..HOP)
                .flat_map(|_| {
                    seed ^= seed << 13;
                    seed ^= seed >> 17;
                    seed ^= seed << 5;
                    let sample = ((seed as i32 as f64 / i32::MAX as f64) * 1600.0) as i16;
                    [sample.to_le_bytes(), sample.to_le_bytes()].concat()
                })
                .collect()
        };
        let mut reducer = NoiseReducer::default();
        reducer.command(Action::Analyze, None).unwrap();
        for _ in 0..=PROFILE_BLOCKS {
            reducer.process(&block());
        }
        reducer.command(Action::Enable, None).unwrap();
        let energy = |pcm: &[u8]| {
            pcm.chunks_exact(2)
                .map(|s| (i16::from_le_bytes([s[0], s[1]]) as f64).powi(2))
                .sum::<f64>()
        };
        let mut input_energy = 0.0;
        let mut output_energy = 0.0;
        for i in 0..60 {
            let input = block();
            let output = reducer.process(&input);
            if i > 10 {
                input_energy += energy(&input);
                output_energy += energy(&output);
            }
        }
        assert!(output_energy < input_energy * 0.25);
    }

    #[test]
    fn spectrogram_resolves_tone_and_opposite_phase_stereo() {
        let spectrum = Spectrum::default();
        let bins = spectrum.measure(&tone(0, 0.1, 0.0));
        assert_eq!(bins.len(), 80);
        let (band, db) = bins
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .unwrap();
        assert!((38..42).contains(&band));
        assert!((*db + 20.0).abs() < 0.2);
        assert!(
            spectrum
                .measure(&vec![0; HOP * 4])
                .iter()
                .all(|db| *db == -90.0)
        );
    }
}
