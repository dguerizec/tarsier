//! Experimental, image-plane phone pose detection. No microphone side effects.
use serde::{Deserialize, Serialize};

use crate::model::{Landmark, PerceptionObservation};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct PhoneGestureConfig {
    pub enabled: bool,
    pub dwell_ms: u64,
    pub release_ms: u64,
    pub stale_ms: u64,
    /// Distance from pinky tip to mouth, in face widths.
    pub enter_radius: f32,
    pub exit_radius: f32,
}

impl Default for PhoneGestureConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            dwell_ms: 300,
            release_ms: 200,
            stale_ms: 500,
            enter_radius: 0.35,
            exit_radius: 0.45,
        }
    }
}

impl PhoneGestureConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.dwell_ms > 0 && self.release_ms > 0 && self.stale_ms > self.release_ms,
            "phone_near_mouth requires positive dwell/release and stale_ms > release_ms"
        );
        anyhow::ensure!(
            self.enter_radius.is_finite()
                && self.exit_radius.is_finite()
                && self.enter_radius > 0.0
                && self.exit_radius > self.enter_radius
                && self.exit_radius <= 2.0,
            "phone_near_mouth requires 0 < enter_radius < exit_radius <= 2"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct PhoneGestureState {
    pub active: bool,
    pub candidate: bool,
    pub hold_progress: f32,
    pub phone_shape: bool,
    pub near_mouth: bool,
    pub mouth_distance: Option<f32>,
    pub reason: String,
}

impl Default for PhoneGestureState {
    fn default() -> Self {
        Self {
            active: false,
            candidate: false,
            hold_progress: 0.0,
            phone_shape: false,
            near_mouth: false,
            mouth_distance: None,
            reason: "waiting_for_observation".into(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PhoneChange {
    Started,
    Ended,
}

pub struct PhoneGestureDetector {
    config: PhoneGestureConfig,
    pub state: PhoneGestureState,
    candidate_since: Option<u64>,
    last_valid: Option<u64>,
    last_capture: Option<u64>,
    wrist: Option<Point>,
}

impl PhoneGestureDetector {
    pub fn new(config: PhoneGestureConfig) -> Self {
        Self {
            config,
            state: PhoneGestureState::default(),
            candidate_since: None,
            last_valid: None,
            last_capture: None,
            wrist: None,
        }
    }

    pub fn reset(&mut self, reason: &str) -> Option<PhoneChange> {
        let change = self.state.active.then_some(PhoneChange::Ended);
        self.state = PhoneGestureState {
            reason: reason.into(),
            ..Default::default()
        };
        self.candidate_since = None;
        self.last_valid = None;
        self.wrist = None;
        change
    }

    /// Also runs without incoming observations, so a stopped worker releases a hold.
    pub fn expire(&mut self, now: u64) -> Option<PhoneChange> {
        if self
            .last_capture
            .is_some_and(|at| now.saturating_sub(at) >= self.config.stale_ms)
        {
            return self.reset("observations_stale");
        }
        if (self.state.active || self.state.candidate)
            && self
                .last_valid
                .is_some_and(|at| now.saturating_sub(at) >= self.config.release_ms)
        {
            return self.reset("gesture_released");
        }
        None
    }

    pub fn observe(
        &mut self,
        observation: &PerceptionObservation,
        now: u64,
    ) -> Option<PhoneChange> {
        if !self.config.enabled {
            return self.reset("disabled");
        }
        let at = observation.captured_at_ms;
        // Replayed, delayed or future frames cannot refresh an active hold.
        if at > now
            || now.saturating_sub(at) >= self.config.stale_ms
            || self.last_capture.is_some_and(|last| at <= last)
        {
            return self.expire(now);
        }
        let interrupted = self.expire(now);
        self.last_capture = Some(at);
        let radius = if self.state.active {
            self.config.exit_radius
        } else {
            self.config.enter_radius
        };
        let mut geometry = measure(observation, radius, self.wrist);
        let valid = geometry.phone_shape && geometry.near_mouth;
        let active = self.state.active;
        if valid {
            self.last_valid = Some(now);
            self.wrist = geometry.wrist;
            let since = *self.candidate_since.get_or_insert(now);
            let progress =
                (now.saturating_sub(since) as f32 / self.config.dwell_ms as f32).min(1.0);
            self.state = PhoneGestureState {
                active: active || progress >= 1.0,
                candidate: !active && progress < 1.0,
                hold_progress: if active { 1.0 } else { progress },
                phone_shape: true,
                near_mouth: true,
                mouth_distance: geometry.distance,
                reason: if active || progress >= 1.0 {
                    "active"
                } else {
                    "holding"
                }
                .into(),
            };
            if self.state.active && !active {
                return Some(PhoneChange::Started);
            }
        } else {
            self.candidate_since = None;
            if !active {
                self.wrist = None;
            }
            if active {
                geometry.reason = "release_pending";
            }
            self.state = PhoneGestureState {
                active,
                phone_shape: geometry.phone_shape,
                near_mouth: geometry.near_mouth,
                mouth_distance: geometry.distance,
                reason: geometry.reason.into(),
                ..Default::default()
            };
        }
        interrupted
    }
}

#[cfg(test)]
pub(crate) fn phone_fixture(at: u64) -> PerceptionObservation {
    let landmark = |x, y| Landmark {
        x,
        y,
        z: 0.0,
        visibility: None,
    };
    let mut face = vec![landmark(0.5, 0.4); 478];
    face[234] = landmark(0.3, 0.4);
    face[454] = landmark(0.7, 0.4);
    face[13] = landmark(0.5, 0.49);
    face[14] = landmark(0.5, 0.51);
    let mut hand = vec![landmark(0.65, 0.8); 21];
    for (index, x, y) in [
        (1, 0.72, 0.8),
        (2, 0.77, 0.75),
        (3, 0.8, 0.7),
        (4, 0.84, 0.65),
        (17, 0.55, 0.7),
        (18, 0.53, 0.63),
        (19, 0.515, 0.56),
        (20, 0.5, 0.5),
    ] {
        hand[index] = landmark(x, y);
    }
    for (base, x) in [(5, 0.71), (9, 0.65), (13, 0.6)] {
        for (offset, y) in [0.69, 0.61, 0.66, 0.73].into_iter().enumerate() {
            hand[base + offset] = landmark(x, y);
        }
    }
    PerceptionObservation {
        frame_id: at,
        captured_at_ms: at,
        image_width: 1000,
        image_height: 1000,
        face_detected: true,
        face_landmarks: face,
        hand_detected: true,
        hand_landmarks: hand,
        pose_detected: false,
        pose_landmarks: vec![],
        gesture: None,
        confidence: 0.0,
        latency_ms: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detector() -> PhoneGestureDetector {
        PhoneGestureDetector::new(PhoneGestureConfig::default())
    }

    fn activate(detector: &mut PhoneGestureDetector) {
        for at in [1000, 1100, 1200] {
            assert_eq!(detector.observe(&phone_fixture(at), at), None);
        }
        assert_eq!(
            detector.observe(&phone_fixture(1300), 1300),
            Some(PhoneChange::Started)
        );
    }

    #[test]
    fn phone_gesture_geometry_is_scale_rotation_mirror_and_aspect_invariant() {
        for aspect in [1.0_f32, 16.0 / 9.0, 9.0 / 16.0] {
            for scale in [0.5, 1.0] {
                for mirror in [-1.0, 1.0] {
                    for angle in [
                        0.0_f32,
                        0.8,
                        std::f32::consts::FRAC_PI_2,
                        std::f32::consts::PI,
                    ] {
                        let mut observation = phone_fixture(1000);
                        observation.image_width = (aspect * 10000.0) as u32;
                        observation.image_height = 10000;
                        for p in observation
                            .face_landmarks
                            .iter_mut()
                            .chain(&mut observation.hand_landmarks)
                        {
                            let x = (p.x - 0.5) * scale * mirror;
                            let y = (p.y - 0.5) * scale;
                            p.x = (0.5 + x * angle.cos() - y * angle.sin()) / aspect;
                            p.y = 0.5 + x * angle.sin() + y * angle.cos();
                        }
                        let geometry = measure(&observation, 0.35, None);
                        assert!(
                            geometry.phone_shape && geometry.near_mouth,
                            "{aspect} {scale} {mirror} {angle}: {}",
                            geometry.reason
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn phone_gesture_rejects_each_wrong_finger_and_distant_phone_pose() {
        for base in [5, 9, 13] {
            let mut observation = phone_fixture(1000);
            for (offset, y) in [0.69, 0.61, 0.53, 0.45].into_iter().enumerate() {
                observation.hand_landmarks[base + offset].y = y;
            }
            assert!(!measure(&observation, 0.35, None).phone_shape);
        }
        for tip in [4, 20] {
            let mut observation = phone_fixture(1000);
            observation.hand_landmarks[tip] = observation.hand_landmarks[0].clone();
            assert!(!measure(&observation, 0.35, None).phone_shape);
        }
        let mut observation = phone_fixture(1000);
        for p in &mut observation.hand_landmarks {
            p.x += 0.3;
        }
        let geometry = measure(&observation, 0.35, None);
        assert!(geometry.phone_shape);
        assert!(!geometry.near_mouth);
    }

    #[test]
    fn phone_gesture_accepts_a_partly_raised_thumb_but_rejects_a_folded_thumb() {
        let mut bent = phone_fixture(1000);
        // A raised thumb with a bend at its final joint, below the former 0.82 threshold.
        bent.hand_landmarks[4].x = 0.78;
        bent.hand_landmarks[4].y = 0.70;
        assert!(measure(&bent, 0.35, None).phone_shape);

        let mut closer = phone_fixture(1000);
        // Still extended, but separated from the index by only 0.45 palm lengths.
        for (index, x, y) in [
            (1, 0.65, 0.8),
            (2, 0.69, 0.76),
            (3, 0.73, 0.72),
            (4, 0.76, 0.69),
        ] {
            closer.hand_landmarks[index].x = x;
            closer.hand_landmarks[index].y = y;
        }
        assert!(measure(&closer, 0.35, None).phone_shape);

        closer.hand_landmarks[4] = closer.hand_landmarks[5].clone();
        assert!(!measure(&closer, 0.35, None).phone_shape);
        bent.hand_landmarks[4] = bent.hand_landmarks[0].clone();
        assert!(!measure(&bent, 0.35, None).phone_shape);
    }

    #[test]
    fn phone_gesture_uses_either_hand_without_merging_fingers() {
        let mut observation = phone_fixture(1000);
        let valid = observation.hand_landmarks.clone();
        observation.hand_landmarks[20] = observation.hand_landmarks[0].clone();
        observation.hand_landmarks.extend(valid);
        assert!(measure(&observation, 0.35, None).near_mouth);
        observation.hand_landmarks[25] = observation.hand_landmarks[21].clone();
        assert!(!measure(&observation, 0.35, None).phone_shape);
    }

    #[test]
    fn phone_gesture_dwell_release_rearm_and_no_duplicate_events() {
        let mut detector = detector();
        activate(&mut detector);
        assert_eq!(detector.observe(&phone_fixture(1400), 1400), None);
        assert!(detector.state.active);
        let mut absent = phone_fixture(1500);
        absent.hand_detected = false;
        assert_eq!(detector.observe(&absent, 1500), None);
        assert!(detector.state.active);
        assert_eq!(detector.expire(1600), Some(PhoneChange::Ended));
        assert_eq!(detector.expire(1700), None);
        for at in [1800, 1900, 2000] {
            assert_eq!(detector.observe(&phone_fixture(at), at), None);
        }
        assert_eq!(
            detector.observe(&phone_fixture(2100), 2100),
            Some(PhoneChange::Started)
        );
    }

    #[test]
    fn phone_gesture_hysteresis_and_short_occlusion_preserve_active_hold() {
        let mut detector = detector();
        activate(&mut detector);
        let mut shifted = phone_fixture(1400);
        for p in &mut shifted.hand_landmarks {
            p.x += 0.16;
        }
        assert!(!measure(&shifted, 0.35, None).near_mouth);
        assert_eq!(detector.observe(&shifted, 1400), None);
        assert!(detector.state.active && detector.state.near_mouth);
        let mut absent = phone_fixture(1450);
        absent.face_detected = false;
        detector.observe(&absent, 1450);
        assert_eq!(detector.observe(&phone_fixture(1500), 1500), None);
        assert!(detector.state.active);
    }

    #[test]
    fn phone_gesture_rejects_replays_and_expires_without_new_frames() {
        let mut detector = detector();
        activate(&mut detector);
        assert_eq!(detector.observe(&phone_fixture(1300), 1450), None);
        assert_eq!(
            detector.observe(&phone_fixture(1200), 1500),
            Some(PhoneChange::Ended)
        );
        assert!(!detector.state.active);
        assert_eq!(detector.observe(&phone_fixture(5000), 1600), None);
        assert!(!detector.state.candidate);
        assert_eq!(detector.observe(&phone_fixture(1000), 2000), None);
        assert!(!detector.state.candidate);
    }

    #[test]
    fn phone_gesture_missing_data_and_interrupted_dwell_fail_closed() {
        let mut detector = detector();
        detector.observe(&phone_fixture(1000), 1000);
        let mut invalid = phone_fixture(1100);
        invalid.image_width = 0;
        detector.observe(&invalid, 1100);
        assert!(!detector.state.candidate);
        detector.observe(&phone_fixture(1200), 1200);
        assert_eq!(detector.observe(&phone_fixture(1300), 1300), None);
        detector.expire(2000);
        assert!(!detector.state.candidate);
        invalid.image_width = 1000;
        invalid.hand_landmarks[0].x = f32::NAN;
        assert!(!measure(&invalid, 0.35, None).near_mouth);
        let mut disabled = PhoneGestureDetector::new(PhoneGestureConfig {
            enabled: false,
            ..Default::default()
        });
        activate_disabled(&mut disabled);
    }

    fn activate_disabled(detector: &mut PhoneGestureDetector) {
        for at in [1000, 1100, 1200, 1300] {
            assert_eq!(detector.observe(&phone_fixture(at), at), None);
        }
        assert!(!detector.state.active);
    }

    #[test]
    fn phone_gesture_does_not_accumulate_a_hold_across_different_hands_or_gaps() {
        let mut detector = detector();
        for at in [1000, 1100, 1200, 1300, 1400, 1500] {
            let mut observation = phone_fixture(at);
            if at % 200 != 0 {
                for p in &mut observation.hand_landmarks {
                    p.x = 1.0 - p.x;
                }
            }
            assert_eq!(detector.observe(&observation, at), None);
            assert!(!detector.state.active);
        }
        assert_eq!(detector.observe(&phone_fixture(2000), 2000), None);
        assert_eq!(detector.observe(&phone_fixture(2300), 2300), None);
        assert!(!detector.state.active);
    }

    #[test]
    fn phone_gesture_keeps_the_active_hand_when_another_hand_is_closer() {
        let mut detector = detector();
        activate(&mut detector);
        let mut observation = phone_fixture(1400);
        let mut tracked = observation.hand_landmarks.clone();
        for p in &mut tracked {
            p.x += 0.04;
        }
        for p in &mut observation.hand_landmarks {
            p.x = 1.0 - p.x;
        }
        observation.hand_landmarks.extend(tracked);
        assert_eq!(detector.observe(&observation, 1400), None);
        assert!(detector.state.active && detector.state.near_mouth);
        assert_eq!(detector.state.reason, "active");
    }

    #[test]
    fn phone_gesture_config_rejects_invalid_hysteresis_and_timing() {
        assert!(PhoneGestureConfig::default().validate().is_ok());
        assert!(
            PhoneGestureConfig {
                enter_radius: f32::NAN,
                ..Default::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            PhoneGestureConfig {
                exit_radius: 0.2,
                ..Default::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            PhoneGestureConfig {
                dwell_ms: 0,
                ..Default::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            PhoneGestureConfig {
                stale_ms: 100,
                ..Default::default()
            }
            .validate()
            .is_err()
        );
    }
}

#[derive(Clone, Copy)]
struct Point {
    x: f32,
    y: f32,
}
impl Point {
    fn distance(self, other: Self) -> f32 {
        (self.x - other.x).hypot(self.y - other.y)
    }
}

struct Geometry {
    phone_shape: bool,
    near_mouth: bool,
    distance: Option<f32>,
    wrist: Option<Point>,
    reason: &'static str,
}

fn measure(
    observation: &PerceptionObservation,
    radius: f32,
    previous_wrist: Option<Point>,
) -> Geometry {
    let mut result = Geometry {
        phone_shape: false,
        near_mouth: false,
        distance: None,
        wrist: None,
        reason: "face_missing",
    };
    if !observation.face_detected || observation.face_landmarks.len() != 478 {
        return result;
    }
    result.reason = "hand_missing";
    if !observation.hand_detected || !matches!(observation.hand_landmarks.len(), 21 | 42) {
        return result;
    }
    result.reason = "image_dimensions_missing";
    if observation.image_width == 0 || observation.image_height == 0 {
        return result;
    }
    let aspect = observation.image_width as f32 / observation.image_height as f32;
    let point = |landmark: &Landmark| Point {
        x: landmark.x * aspect,
        y: landmark.y,
    };
    result.reason = "invalid_landmarks";
    if observation
        .face_landmarks
        .iter()
        .chain(&observation.hand_landmarks)
        .any(|p| !p.x.is_finite() || !p.y.is_finite() || !p.z.is_finite())
    {
        return result;
    }
    let face = &observation.face_landmarks;
    let face_width = point(&face[234]).distance(point(&face[454]));
    if face_width < 0.01 {
        return result;
    }
    let upper = point(&face[13]);
    let lower = point(&face[14]);
    let mouth = Point {
        x: (upper.x + lower.x) / 2.0,
        y: (upper.y + lower.y) / 2.0,
    };
    result.reason = "phone_shape_missing";
    for hand in observation.hand_landmarks.chunks_exact(21) {
        let points: Vec<_> = hand.iter().map(point).collect();
        let palm = points[0].distance(points[9]);
        if palm < 0.005 {
            continue;
        }
        // End-to-end / articulated length is invariant under scale and in-plane rotation.
        let straightness = |indices: [usize; 4]| {
            let length: f32 = indices
                .windows(2)
                .map(|pair| points[pair[0]].distance(points[pair[1]]))
                .sum();
            if length < 0.002 {
                0.0
            } else {
                points[indices[0]].distance(points[indices[3]]) / length
            }
        };
        let extended = |base: usize| {
            straightness([base, base + 1, base + 2, base + 3]) >= 0.82
                && points[base + 3].distance(points[0])
                    > points[base + 1].distance(points[0]) + palm * 0.15
        };
        let curled = |base: usize| {
            straightness([base, base + 1, base + 2, base + 3]) < 0.75
                && points[base + 3].distance(points[0]) < points[base + 1].distance(points[0])
        };
        // A phone grip can keep the thumb raised with a bent joint or modest
        // separation from the index; require less extension than for the pinky.
        let shape = straightness([1, 2, 3, 4]) >= 0.72
            && points[4].distance(points[5]) > palm * 0.45
            && extended(17)
            && curled(5)
            && curled(9)
            && curled(13);
        if !shape {
            continue;
        }
        let distance = points[20].distance(mouth) / face_width;
        // Filter continuity before ranking distance: a second, closer hand must
        // not hide the hand that is already holding the gesture.
        if previous_wrist.is_some_and(|wrist| wrist.distance(points[0]) > palm * 1.5) {
            if !result.phone_shape {
                result.phone_shape = true;
                result.distance = Some(distance);
                result.reason = "hand_changed";
            }
            continue;
        }
        if result.wrist.is_some() && result.distance.is_some_and(|best| best <= distance) {
            continue;
        }
        result.phone_shape = true;
        result.distance = Some(distance);
        result.near_mouth = distance <= radius;
        result.reason = if result.near_mouth {
            "holding"
        } else {
            "pinky_too_far"
        };
        result.wrist = Some(points[0]);
        if result.near_mouth {
            break;
        }
    }
    result
}
