//! Internal LED modes and temporary MCP activity feedback.
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
    pulse: Option<(Instant, Instant, u8)>,
}

impl Led {
    pub fn new(now: Instant) -> Self {
        Self {
            mode: LedMode::Off,
            initialized: false,
            applied: None,
            retry_at: now,
            epoch: now,
            pulse: None,
        }
    }

    pub fn set_mode(&mut self, mode: LedMode, now: Instant) {
        if self.mode != mode {
            self.mode = mode;
            self.epoch = now;
        }
    }

    pub fn notify_mcp(&mut self, now: Instant) {
        self.advance(now);
        let deadline = now + Duration::from_secs(1);
        if let Some((_, expires, _)) = &mut self.pulse {
            // Retrigger the timeout without interrupting the current phase.
            *expires = deadline;
        } else {
            self.pulse = Some((
                now,
                deadline,
                self.applied.unwrap_or(self.base_brightness(now)),
            ));
        }
    }

    pub fn advance(&mut self, now: Instant) {
        if self.pulse.is_some_and(|(_, deadline, _)| now >= deadline) {
            self.pulse = None;
        }
    }

    pub fn brightness(&self, now: Instant) -> u8 {
        if let Some((start, deadline, baseline)) = self.pulse
            && now < deadline
        {
            let phase = now.duration_since(start).as_nanos() * 6 / 1_000_000_000;
            return if phase.is_multiple_of(2) {
                if baseline == 0 { 3 } else { 0 }
            } else {
                baseline
            };
        }
        self.base_brightness(now)
    }

    fn base_brightness(&self, now: Instant) -> u8 {
        match self.mode {
            LedMode::Off => 0,
            LedMode::Steady => 3,
            // Derive the phase from elapsed time so delayed I/O never queues a burst.
            LedMode::Blinking => {
                if (now.duration_since(self.epoch).as_nanos() * 6 / 1_000_000_000).is_multiple_of(2) {
                    3
                } else {
                    0
                }
            }
        }
    }

    pub fn invalidate(&mut self, now: Instant) {
        self.pulse = None;
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
    #[test]
    fn mcp_feedback_inverts_both_baselines_and_restores_after_three_cycles() {
        for (mode, baseline) in [(LedMode::Off, 0), (LedMode::Steady, 3)] {
            let now = Instant::now();
            let mut led = Led::new(now);
            led.set_mode(mode, now);
            led.applied = Some(baseline);
            led.notify_mcp(now);
            for (ms, inverted) in [
                (0, true),
                (167, false),
                (334, true),
                (500, false),
                (667, true),
                (834, false),
            ] {
                assert_eq!(
                    led.brightness(now + Duration::from_millis(ms)),
                    if inverted { 3 - baseline } else { baseline }
                );
            }
            led.advance(now + Duration::from_secs(1));
            assert_eq!(led.brightness(now + Duration::from_secs(1)), baseline);
            assert!(led.pulse.is_none());
        }
    }

    #[test]
    fn overlapping_calls_extend_timeout_without_resetting_phase() {
        let now = Instant::now();
        let mut led = Led::new(now);
        led.notify_mcp(now);
        led.applied = Some(0);
        led.notify_mcp(now + Duration::from_millis(200));
        assert_eq!(led.brightness(now + Duration::from_millis(200)), 0);
        assert_eq!(led.brightness(now + Duration::from_millis(334)), 3);
        led.advance(now + Duration::from_millis(1000));
        assert!(led.pulse.is_some());
        assert_eq!(led.brightness(now + Duration::from_millis(1000)), 3);
        led.advance(now + Duration::from_millis(1200));
        assert!(led.pulse.is_none());
        assert_eq!(led.brightness(now + Duration::from_millis(1200)), 0);
        led.notify_mcp(now + Duration::from_secs(2));
        led.invalidate(now + Duration::from_secs(2));
        assert!(led.pulse.is_none());
    }
}
