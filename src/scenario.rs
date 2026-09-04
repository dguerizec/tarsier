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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PresencePhase {
    Absent,
    Appearing { since_ms: u64 },
    Present,
    Disappearing { since_ms: u64 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PresenceChange {
    Started,
    Ended,
}

pub struct FacePresenceStabilizer {
    dwell_ms: u64,
    release_ms: u64,
    phase: PresencePhase,
}

impl FacePresenceStabilizer {
    pub fn new(config: &PerceptionConfig) -> Self {
        Self {
            dwell_ms: config.face_dwell_ms,
            release_ms: config.face_release_ms,
            phase: PresencePhase::Absent,
        }
    }

    pub fn observe(&mut self, detected: bool, at_ms: u64) -> Option<PresenceChange> {
        match (self.phase, detected) {
            (PresencePhase::Absent, true) => {
                self.phase = PresencePhase::Appearing { since_ms: at_ms };
                None
            }
            (PresencePhase::Appearing { since_ms }, true)
                if at_ms.saturating_sub(since_ms) >= self.dwell_ms =>
            {
                self.phase = PresencePhase::Present;
                Some(PresenceChange::Started)
            }
            (PresencePhase::Appearing { .. }, false) => {
                self.phase = PresencePhase::Absent;
                None
            }
            (PresencePhase::Present, false) => {
                self.phase = PresencePhase::Disappearing { since_ms: at_ms };
                None
            }
            (PresencePhase::Disappearing { since_ms }, false)
                if at_ms.saturating_sub(since_ms) >= self.release_ms =>
            {
                self.phase = PresencePhase::Absent;
                Some(PresenceChange::Ended)
            }
            (PresencePhase::Disappearing { .. }, true) => {
                self.phase = PresencePhase::Present;
                None
            }
            _ => None,
        }
    }
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

    #[test]
    fn face_presence_requires_stable_appearance_and_disappearance() {
        let config = PerceptionConfig {
            face_dwell_ms: 300,
            face_release_ms: 500,
            ..PerceptionConfig::default()
        };
        let mut presence = FacePresenceStabilizer::new(&config);

        assert_eq!(presence.observe(true, 1000), None);
        assert_eq!(presence.observe(false, 1100), None);
        assert_eq!(presence.observe(true, 1200), None);
        assert_eq!(presence.observe(true, 1500), Some(PresenceChange::Started));
        assert_eq!(presence.observe(false, 1600), None);
        assert_eq!(presence.observe(true, 1800), None);
        assert_eq!(presence.observe(false, 2000), None);
        assert_eq!(presence.observe(false, 2500), Some(PresenceChange::Ended));
    }
}
