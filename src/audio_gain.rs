//! Speech-gated automatic gain for 20 ms, 48 kHz stereo PCM blocks.
use serde::{Deserialize, Serialize};
use webrtc_vad::{SampleRate, Vad, VadMode};

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct GainStatus {
    pub gain_db: f32,
    pub speech: bool,
}

struct SpeechDetector(Vad);
// libfvad owns all mutable state in its instance, with no thread affinity.
// This wrapper moves exclusive ownership between executor threads; it is not Sync.
unsafe impl Send for SpeechDetector {}

pub struct AutoGain {
    detector: SpeechDetector,
    learned_db: f32,
    applied_db: f32,
    voice_blocks: u32,
    quiet_blocks: u32,
    previous_gain: f32,
    speech_blocks: u32,
    rms_power: f32,
    peak_envelope: f32,
}

impl Default for AutoGain {
    fn default() -> Self {
        Self {
            detector: SpeechDetector(Vad::new_with_rate_and_mode(
                SampleRate::Rate48kHz,
                VadMode::Aggressive,
            )),
            learned_db: 0.0,
            applied_db: 0.0,
            voice_blocks: 0,
            quiet_blocks: 0,
            previous_gain: 1.0,
            speech_blocks: 0,
            rms_power: 0.0,
            peak_envelope: 0.0,
        }
    }
}

impl AutoGain {
    pub fn process(&mut self, pcm: &[u8]) -> (Vec<u8>, GainStatus) {
        if pcm.len() != 960 * 4 {
            return (vec![0; 960 * 4], GainStatus::default());
        }
        let mut channels = [[0_i16; 960]; 2];
        let mut energy = [0.0_f32; 2];
        let mut peak = 0.0_f32;
        for (i, frame) in pcm.chunks_exact(4).enumerate() {
            for channel in 0..2 {
                let sample = i16::from_le_bytes([frame[channel * 2], frame[channel * 2 + 1]]);
                channels[channel][i] = sample;
                let normalized = sample as f32 / 32768.0;
                energy[channel] += normalized * normalized;
                peak = peak.max(normalized.abs());
            }
        }
        // Use the stronger channel, avoiding cancellation with opposite-phase stereo.
        let channel = usize::from(energy[1] > energy[0]);
        let rms = (energy[channel] / 960.0).sqrt();
        let speech = self
            .detector
            .0
            .is_voice_segment(&channels[channel])
            .unwrap_or(false)
            && rms > 0.001;
        let status = self.advance(rms, peak, speech);
        let ceiling = if peak > 0.0 {
            0.891251 / peak
        } else {
            f32::MAX
        };
        let gain = 10.0_f32.powf(self.applied_db / 20.0).min(ceiling);
        let previous = self.previous_gain.min(ceiling);
        let mut output = Vec::with_capacity(pcm.len());
        for (i, frame) in pcm.chunks_exact(4).enumerate() {
            let interpolated = previous + (gain - previous) * (i + 1) as f32 / 960.0;
            for sample in frame.chunks_exact(2) {
                let sample = i16::from_le_bytes([sample[0], sample[1]]) as f32;
                output.extend_from_slice(
                    &((sample * interpolated).clamp(-29204.0, 29204.0).round() as i16)
                        .to_le_bytes(),
                );
            }
        }
        self.previous_gain = gain;
        (
            output,
            GainStatus {
                gain_db: 20.0 * gain.max(0.00001).log10(),
                ..status
            },
        )
    }

    fn advance(&mut self, rms: f32, peak: f32, speech: bool) -> GainStatus {
        if speech {
            self.voice_blocks = (self.voice_blocks + 1).min(10);
            self.quiet_blocks = 0;
            self.speech_blocks = self.speech_blocks.saturating_add(1);
            // Smooth speech energy instead of following individual syllables.
            if self.speech_blocks == 1 {
                self.rms_power = rms * rms;
            } else {
                self.rms_power += 0.15 * (rms * rms - self.rms_power);
            }
            self.peak_envelope = peak.max(self.peak_envelope * 0.98);
            let desired = (-18.0 - 10.0 * self.rms_power.max(1e-10).log10())
                .min(-3.0 - 20.0 * self.peak_envelope.max(0.00001).log10())
                .clamp(-24.0, 24.0);
            // Acquire the initial voice level quickly, then track changes smoothly.
            // Brief VAD gaps do not restart acquisition on every syllable.
            let rise = if self.speech_blocks <= 50 { 1.2 } else { 0.2 };
            if desired < self.learned_db || self.voice_blocks >= 5 {
                self.learned_db += (desired - self.learned_db).clamp(-0.8, rise);
            }
        } else {
            self.voice_blocks = self.voice_blocks.saturating_sub(1);
            self.quiet_blocks = self.quiet_blocks.saturating_add(1);
        }
        // Hold through ordinary pauses. Longer silences slowly release positive
        // gain, without learning from noise or raising it between words.
        if speech {
            self.applied_db += (self.learned_db - self.applied_db).clamp(-0.8, 1.2);
        } else if self.quiet_blocks > 50 {
            let target = self.learned_db.min(0.0);
            self.applied_db += (target - self.applied_db).clamp(-0.16, 0.0);
        }
        GainStatus {
            gain_db: self.applied_db,
            speech,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gain_learns_only_on_speech_and_returns_to_unity_in_silence() {
        let mut gain = AutoGain::default();
        for _ in 0..500 {
            gain.advance(0.01, 0.02, false);
        }
        assert_eq!(gain.learned_db, 0.0);
        for _ in 0..200 {
            gain.advance(0.01, 0.03, true);
        }
        assert!(gain.learned_db > 21.0 && gain.learned_db <= 22.0);
        let learned = gain.learned_db;
        for _ in 0..250 {
            gain.advance(0.001, 0.004, false);
        }
        assert_eq!(gain.learned_db, learned);
        assert_eq!(gain.applied_db, 0.0);
        for _ in 0..25 {
            gain.advance(0.01, 0.03, true);
        }
        assert!(gain.applied_db > 15.0);
    }

    #[test]
    fn short_phrases_acquire_gain_and_keep_it_across_word_pauses() {
        let mut gain = AutoGain::default();
        for i in 0..25 {
            gain.advance(0.01, 0.03, true);
            if i % 4 == 0 {
                gain.advance(0.001, 0.003, false);
            }
        }
        assert!(
            gain.applied_db > 18.0,
            "first short phrase must already be audible"
        );
        let before = gain.applied_db;
        for _ in 0..35 {
            gain.advance(0.001, 0.003, false);
        }
        assert_eq!(
            gain.applied_db, before,
            "do not pump gain during a 700 ms pause"
        );
        gain.advance(0.01, 0.03, true);
        assert!(gain.applied_db >= before);
    }

    #[test]
    fn loud_voice_reduces_gain_quickly_and_gain_is_bounded() {
        let mut gain = AutoGain::default();
        for _ in 0..1000 {
            gain.advance(0.0011, 0.003, true);
        }
        assert_eq!(gain.learned_db, 24.0);
        for _ in 0..50 {
            gain.advance(0.5, 0.9, true);
        }
        assert!(gain.applied_db < -10.0);
        assert!(gain.applied_db >= -24.0);
        assert_eq!(AutoGain::default().learned_db, 0.0);
    }

    #[test]
    fn silence_remains_silent_and_stereo_peaks_are_limited_without_cancellation() {
        let mut gain = AutoGain::default();
        for _ in 0..50 {
            let (pcm, status) = gain.process(&vec![0; 3840]);
            assert!(pcm.iter().all(|byte| *byte == 0));
            assert!(!status.speech);
            assert_eq!(status.gain_db, 0.0);
        }
        gain.applied_db = 24.0;
        gain.learned_db = 24.0;
        gain.previous_gain = 10.0;
        let pcm: Vec<u8> = [16000_i16, -16000]
            .into_iter()
            .flat_map(i16::to_le_bytes)
            .collect::<Vec<_>>()
            .repeat(960);
        let (output, status) = gain.process(&pcm);
        for frame in output.chunks_exact(4) {
            let left = i16::from_le_bytes([frame[0], frame[1]]);
            let right = i16::from_le_bytes([frame[2], frame[3]]);
            assert_eq!(left, -right);
            assert!(left.abs() <= 29204);
        }
        assert!(status.gain_db < 6.0);
    }

    #[test]
    #[ignore = "provide TARSIER_GAIN_TEST_PCM containing 48 kHz stereo s16le speech"]
    fn benchmark_real_speech() {
        let pcm = std::fs::read(std::env::var("TARSIER_GAIN_TEST_PCM").unwrap()).unwrap();
        if let Ok(path) = std::env::var("TARSIER_GAIN_TEST_OUTPUT") {
            let mut gain = AutoGain::default();
            let mut processed = Vec::new();
            for frame in pcm.chunks_exact(3840) {
                processed.extend_from_slice(&gain.process(frame).0);
            }
            std::fs::write(path, processed).unwrap();
        }
        let started = std::time::Instant::now();
        let mut blocks = 0;
        let mut voiced = 0;
        let mut maximum_gain = 0.0_f32;
        for _ in 0..20 {
            let mut gain = AutoGain::default();
            for frame in pcm.chunks_exact(3840) {
                let (_, status) = gain.process(std::hint::black_box(frame));
                voiced += usize::from(status.speech);
                maximum_gain = maximum_gain.max(status.gain_db);
                blocks += 1;
            }
        }
        let elapsed = started.elapsed().as_secs_f64();
        eprintln!(
            "{blocks} blocks, {voiced} voiced, max gain {maximum_gain:.1} dB, {:.1} us/block, {:.3}% of one CPU core",
            elapsed * 1e6 / blocks as f64,
            elapsed / (blocks as f64 * 0.02) * 100.0
        );
        assert!(voiced > 0);
        assert!(maximum_gain > 0.0);
    }
}
