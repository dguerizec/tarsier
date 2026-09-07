//! Internal LED policy primitives. Usage mapping is intentionally not assigned yet.
use std::time::{Duration, Instant};

#[allow(dead_code)] // Reserved for the future backend usage policy, never user settings.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum LedMode {
    #[default]
    Off,
    Steady,
    Blinking,
}

pub(super) struct Led {
    pub mode: LedMode,
    pub initialized: bool,
    pub applied: Option<u8>,
    pub retry_at: Instant,
    epoch: Instant,
}

impl Led {
    pub fn new(now: Instant) -> Self {
        Self {
            mode: LedMode::Off,
            initialized: false,
            applied: None,
            retry_at: now,
            epoch: now,
        }
    }

    pub fn set_mode(&mut self, mode: LedMode, now: Instant) {
        if self.mode != mode {
            self.mode = mode;
            self.epoch = now;
        }
    }

    pub fn brightness(&self, now: Instant) -> u8 {
        match self.mode {
            LedMode::Off => 0,
            LedMode::Steady => 3,
            // Derive the phase from elapsed time so delayed I/O never queues a burst.
            LedMode::Blinking => {
                if (now.duration_since(self.epoch).as_nanos() * 6 / 1_000_000_000) % 2 == 0 {
                    3
                } else {
                    0
                }
            }
        }
    }

    pub fn invalidate(&mut self, now: Instant) {
        self.initialized = false;
        self.applied = None;
        self.retry_at = now;
    }

    pub fn failed(&mut self, now: Instant) {
        self.applied = None;
        self.retry_at = now + Duration::from_secs(5);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transitions_and_delayed_ticks_preserve_three_hertz_phase() {
        let now = Instant::now();
        let mut led = Led::new(now);
        assert_eq!(led.brightness(now), 0);
        led.set_mode(LedMode::Blinking, now);
        assert_eq!(led.brightness(now), 3);
        assert_eq!(led.brightness(now + Duration::from_millis(167)), 0);
        assert_eq!(led.brightness(now + Duration::from_millis(334)), 3);
        assert_eq!(led.brightness(now + Duration::from_millis(850)), 0);
        led.set_mode(LedMode::Blinking, now + Duration::from_millis(850));
        assert_eq!(led.brightness(now + Duration::from_millis(850)), 0);
        led.set_mode(LedMode::Steady, now);
        assert_eq!(led.brightness(now), 3);
        led.set_mode(LedMode::Off, now);
        assert_eq!(led.brightness(now), 0);
    }
}
