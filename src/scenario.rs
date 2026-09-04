use crate::config::PerceptionConfig;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Idle,
    Candidate { since_ms: u64 },
    Held,
}

pub struct OpenPalmStabilizer {
    minimum_confidence: f32,
    release_confidence: f32,
    dwell_ms: u64,
    cooldown_ms: u64,
    phase: Phase,
    last_trigger_ms: Option<u64>,
}

impl OpenPalmStabilizer {
    pub fn new(config: &PerceptionConfig) -> Self {
        Self {
            minimum_confidence: config.minimum_confidence,
            release_confidence: config.release_confidence,
            dwell_ms: config.dwell_ms,
            cooldown_ms: config.cooldown_ms,
            phase: Phase::Idle,
            last_trigger_ms: None,
        }
    }

    pub fn observe(&mut self, open_palm: bool, confidence: f32, at_ms: u64) -> bool {
        if !open_palm || confidence < self.release_confidence {
            self.phase = Phase::Idle;
            return false;
        }
        if confidence < self.minimum_confidence {
            return false;
        }

        match self.phase {
            Phase::Idle => {
                self.phase = Phase::Candidate { since_ms: at_ms };
                false
            }
            Phase::Candidate { since_ms } if at_ms.saturating_sub(since_ms) >= self.dwell_ms => {
                self.phase = Phase::Held;
                let cooled_down = self
                    .last_trigger_ms
                    .is_none_or(|last| at_ms.saturating_sub(last) >= self.cooldown_ms);
                if cooled_down {
                    self.last_trigger_ms = Some(at_ms);
                }
                cooled_down
            }
            Phase::Candidate { .. } | Phase::Held => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stabilizer() -> OpenPalmStabilizer {
        OpenPalmStabilizer::new(&PerceptionConfig {
            dwell_ms: 800,
            cooldown_ms: 3000,
            ..PerceptionConfig::default()
        })
    }

    #[test]
    fn emits_once_after_dwell() {
        let mut s = stabilizer();
        assert!(!s.observe(true, 0.9, 1000));
        assert!(!s.observe(true, 0.9, 1700));
        assert!(s.observe(true, 0.9, 1800));
        assert!(!s.observe(true, 0.9, 1900));
    }

    #[test]
    fn hysteresis_requires_a_real_release() {
        let mut s = stabilizer();
        assert!(!s.observe(true, 0.9, 1000));
        assert!(s.observe(true, 0.9, 1800));
        assert!(!s.observe(true, 0.7, 1900));
        assert!(!s.observe(false, 0.2, 2000));
        assert!(!s.observe(true, 0.9, 5000));
        assert!(s.observe(true, 0.9, 5800));
    }
}
