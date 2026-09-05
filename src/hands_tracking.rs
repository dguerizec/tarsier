use crate::model::Landmark;

const HAND_LANDMARK_COUNT: usize = 21;
const TARGET_X: f32 = 0.5;
const TARGET_Y: f32 = 0.5;
const PAN_START_THRESHOLD: f32 = 0.10;
const PAN_STOP_THRESHOLD: f32 = 0.06;
const TILT_START_THRESHOLD: f32 = 0.11;
const TILT_STOP_THRESHOLD: f32 = 0.065;
const MINIMUM_SPEED_FRACTION: f64 = 0.004;
const MAXIMUM_SPEED_FRACTION: f64 = 0.035;
const ACCELERATION_STEP: f64 = 0.006;
const DECELERATION_STEP: f64 = 0.008;
const MAXIMUM_IMAGE_ERROR: f32 = 0.5;
const RAPID_HAND_SPEED_PER_SECOND: f32 = 0.85;
const MAXIMUM_SPEED_SAMPLE_INTERVAL_MS: u64 = 500;
const MINIMUM_HAND_SPAN: f32 = 0.03;
const MINIMUM_TARGET_SPAN: f32 = 0.18;
const MAXIMUM_TARGET_SPAN: f32 = 0.62;
const SPAN_SMOOTHING_ALPHA: f32 = 0.25;
const ZOOM_START_THRESHOLD_FRACTION: f32 = 0.08;
const ZOOM_MAXIMUM_STEP: f32 = 0.08;
const ZOOM_MINIMUM_STEP: f32 = 0.02;
const ZOOM_RAMP_GAIN: f32 = 0.35;
const ZOOM_DESTINATION_TOLERANCE: f32 = 0.02;
const ZOOM_MINIMUM_INTERVAL_MS: u64 = 200;
const ZOOM_SETTLE_MS: u64 = 400;
const MINIMUM_ZOOM_MAGNIFICATION: f32 = 1.0;
const MAXIMUM_ZOOM_MAGNIFICATION: f32 = 4.0;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct HandsTrackingMotion {
    pub pan_direction: i8,
    pub tilt_direction: i8,
    pub speed_fraction: f64,
}

impl HandsTrackingMotion {
    pub fn active(self) -> bool {
        self.pan_direction != 0 || self.tilt_direction != 0
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct HandsTrackingDecision {
    pub hands_visible: u8,
    pub rapid_motion: bool,
    pub zoom_frozen: bool,
    pub target_x: Option<f32>,
    pub target_y: Option<f32>,
    pub motion: HandsTrackingMotion,
    pub calibrated: bool,
    pub target_span: Option<f32>,
    pub hand_span: Option<f32>,
    pub requested_magnification: Option<f32>,
    pub at_limit: bool,
}

#[derive(Clone, Copy, Debug)]
struct HandGeometry {
    center_x: f32,
    center_y: f32,
    minimum_x: f32,
    maximum_x: f32,
    minimum_y: f32,
    maximum_y: f32,
}

#[derive(Default)]
pub struct HandsTrackingController {
    enabled: bool,
    motion: HandsTrackingMotion,
    previous_centers: Vec<(f32, f32)>,
    previous_captured_at_ms: Option<u64>,
    target_span: Option<f32>,
    smoothed_span: Option<f32>,
    recalibrate_zoom_after_ms: Option<u64>,
    last_zoom_command_at_ms: Option<u64>,
    zoom_destination: Option<f32>,
    zoom_settle_until_ms: Option<u64>,
}

impl HandsTrackingController {
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        self.motion = HandsTrackingMotion::default();
        self.previous_centers.clear();
        self.previous_captured_at_ms = None;
        self.target_span = None;
        self.smoothed_span = None;
        self.recalibrate_zoom_after_ms = None;
        self.last_zoom_command_at_ms = None;
        self.zoom_destination = None;
        self.zoom_settle_until_ms = None;
    }

    pub fn motion(&self) -> HandsTrackingMotion {
        self.motion
    }

    pub fn record_motion(&mut self, motion: HandsTrackingMotion) {
        self.motion = motion;
    }

    pub fn recalibrate_zoom_after(&mut self, captured_at_ms: u64) {
        if !self.enabled {
            return;
        }
        self.target_span = None;
        self.smoothed_span = None;
        self.recalibrate_zoom_after_ms = Some(captured_at_ms);
        self.last_zoom_command_at_ms = None;
        self.zoom_destination = None;
        self.zoom_settle_until_ms = None;
    }

    pub fn observe(
        &mut self,
        landmarks: &[Landmark],
        zoom_magnification: Option<f32>,
        captured_at_ms: u64,
    ) -> HandsTrackingDecision {
        let hands = hand_geometries(landmarks);
        let rapid_motion = self.rapid_motion(&hands, captured_at_ms);
        self.previous_centers = hands
            .iter()
            .map(|hand| (hand.center_x, hand.center_y))
            .collect();
        self.previous_captured_at_ms = Some(captured_at_ms);

        let hands_visible = hands.len() as u8;
        let zoom_frozen = hands_visible != 2 || rapid_motion;
        let target = if hands.is_empty() || rapid_motion {
            None
        } else {
            let (minimum_x, maximum_x, minimum_y, maximum_y) = union_bounds(&hands);
            Some(((minimum_x + maximum_x) / 2.0, (minimum_y + maximum_y) / 2.0))
        };
        let motion = target
            .map(|(x, y)| self.motion_for_target(x, y))
            .unwrap_or_default();

        let hand_span = (hands_visible == 2).then(|| {
            let (minimum_x, maximum_x, minimum_y, maximum_y) = union_bounds(&hands);
            (maximum_x - minimum_x).max(maximum_y - minimum_y)
        });
        let zoom = self.zoom_decision(hand_span, zoom_magnification, captured_at_ms, zoom_frozen);

        HandsTrackingDecision {
            hands_visible,
            rapid_motion,
            zoom_frozen,
            target_x: target.map(|(x, _)| x),
            target_y: target.map(|(_, y)| y),
            motion,
            calibrated: self.target_span.is_some(),
            target_span: self.target_span,
            hand_span: zoom.hand_span,
            requested_magnification: zoom.requested_magnification,
            at_limit: zoom.at_limit,
        }
    }

    fn rapid_motion(&self, hands: &[HandGeometry], captured_at_ms: u64) -> bool {
        let Some(previous_at_ms) = self.previous_captured_at_ms else {
            return false;
        };
        let elapsed_ms = captured_at_ms.saturating_sub(previous_at_ms);
        if elapsed_ms == 0
            || elapsed_ms > MAXIMUM_SPEED_SAMPLE_INTERVAL_MS
            || self.previous_centers.is_empty()
        {
            return false;
        }
        let elapsed_seconds = elapsed_ms as f32 / 1_000.0;
        matched_center_distances(&self.previous_centers, hands)
            .into_iter()
            .any(|distance| distance / elapsed_seconds >= RAPID_HAND_SPEED_PER_SECOND)
    }

    fn motion_for_target(&self, x: f32, y: f32) -> HandsTrackingMotion {
        let pan_direction = axis_direction(
            x - TARGET_X,
            self.motion.pan_direction,
            PAN_START_THRESHOLD,
            PAN_STOP_THRESHOLD,
        );
        let image_tilt_direction = axis_direction(
            y - TARGET_Y,
            -self.motion.tilt_direction,
            TILT_START_THRESHOLD,
            TILT_STOP_THRESHOLD,
        );
        let tilt_direction = -image_tilt_direction;
        let directions = (pan_direction, tilt_direction);
        let speed_fraction = if directions == (0, 0) {
            0.0
        } else {
            let requested = proportional_speed(
                x - TARGET_X,
                y - TARGET_Y,
                pan_direction,
                image_tilt_direction,
            );
            let current = if directions == (self.motion.pan_direction, self.motion.tilt_direction) {
                self.motion.speed_fraction
            } else {
                0.0
            };
            ramp_speed(current, requested)
        };
        HandsTrackingMotion {
            pan_direction,
            tilt_direction,
            speed_fraction,
        }
    }

    fn zoom_decision(
        &mut self,
        hand_span: Option<f32>,
        zoom_magnification: Option<f32>,
        captured_at_ms: u64,
        frozen: bool,
    ) -> ZoomDecision {
        if frozen {
            self.smoothed_span = None;
            self.last_zoom_command_at_ms = None;
            self.zoom_destination = None;
            self.zoom_settle_until_ms = None;
            return ZoomDecision::default();
        }
        let hand_span = hand_span.filter(|span| span.is_finite() && *span >= MINIMUM_HAND_SPAN);
        let Some(hand_span) = hand_span else {
            return ZoomDecision::default();
        };
        let mut decision = ZoomDecision {
            hand_span: Some(hand_span),
            ..ZoomDecision::default()
        };
        if self
            .recalibrate_zoom_after_ms
            .is_some_and(|minimum| captured_at_ms < minimum)
        {
            return decision;
        }
        if self.target_span.is_none() {
            self.target_span = Some(hand_span.clamp(MINIMUM_TARGET_SPAN, MAXIMUM_TARGET_SPAN));
            self.smoothed_span = Some(hand_span);
            self.recalibrate_zoom_after_ms = None;
            return decision;
        }
        let smoothed = self.smoothed_span.map_or(hand_span, |previous| {
            previous + SPAN_SMOOTHING_ALPHA * (hand_span - previous)
        });
        self.smoothed_span = Some(smoothed);
        decision.hand_span = Some(smoothed);
        let Some(current_zoom) = zoom_magnification.filter(|zoom| {
            zoom.is_finite()
                && (MINIMUM_ZOOM_MAGNIFICATION..=MAXIMUM_ZOOM_MAGNIFICATION).contains(zoom)
        }) else {
            return decision;
        };

        let mut current_span = smoothed;
        if let Some(settle_until) = self.zoom_settle_until_ms {
            if captured_at_ms < settle_until {
                return decision;
            }
            self.zoom_settle_until_ms = None;
            self.smoothed_span = Some(hand_span);
            current_span = hand_span;
            decision.hand_span = Some(hand_span);
        }
        let target = self.target_span.unwrap_or(current_span);
        let relative_error = target / current_span - 1.0;
        if let Some(destination) = self.zoom_destination {
            let destination_direction = (destination - current_zoom).signum();
            if relative_error.abs() >= ZOOM_START_THRESHOLD_FRACTION
                && relative_error.signum() != destination_direction
            {
                self.zoom_destination = None;
            }
        }
        if self.zoom_destination.is_none() {
            if relative_error.abs() < ZOOM_START_THRESHOLD_FRACTION {
                return decision;
            }
            let unconstrained = current_zoom * target / current_span;
            let bounded =
                unconstrained.clamp(MINIMUM_ZOOM_MAGNIFICATION, MAXIMUM_ZOOM_MAGNIFICATION);
            decision.at_limit = (bounded - unconstrained).abs() > f32::EPSILON;
            if (bounded - current_zoom).abs() < ZOOM_MINIMUM_STEP {
                return decision;
            }
            self.zoom_destination = Some(bounded);
        }
        let destination = self.zoom_destination.unwrap_or(current_zoom);
        let remaining = destination - current_zoom;
        if remaining.abs() <= ZOOM_DESTINATION_TOLERANCE {
            self.zoom_destination = None;
            self.zoom_settle_until_ms = Some(captured_at_ms.saturating_add(ZOOM_SETTLE_MS));
            return decision;
        }
        if self.last_zoom_command_at_ms.is_some_and(|previous| {
            captured_at_ms.saturating_sub(previous) < ZOOM_MINIMUM_INTERVAL_MS
        }) {
            return decision;
        }
        let step = (remaining.abs() * ZOOM_RAMP_GAIN)
            .clamp(ZOOM_MINIMUM_STEP, ZOOM_MAXIMUM_STEP)
            .min(remaining.abs());
        let requested = ((current_zoom + remaining.signum() * step)
            .clamp(MINIMUM_ZOOM_MAGNIFICATION, MAXIMUM_ZOOM_MAGNIFICATION)
            * 100.0)
            .round()
            / 100.0;
        if (requested - current_zoom).abs() < ZOOM_DESTINATION_TOLERANCE {
            return decision;
        }
        self.last_zoom_command_at_ms = Some(captured_at_ms);
        decision.requested_magnification = Some(requested);
        decision
    }
}

fn matched_center_distances(previous: &[(f32, f32)], hands: &[HandGeometry]) -> Vec<f32> {
    let distance = |previous: (f32, f32), hand: &HandGeometry| {
        (hand.center_x - previous.0).hypot(hand.center_y - previous.1)
    };
    match (previous, hands) {
        ([], _) | (_, []) => Vec::new(),
        ([previous], [hand]) => vec![distance(*previous, hand)],
        ([previous], [first, second]) => {
            vec![distance(*previous, first).min(distance(*previous, second))]
        }
        ([first, second], [hand]) => {
            vec![distance(*first, hand).min(distance(*second, hand))]
        }
        ([first_previous, second_previous], [first_hand, second_hand]) => {
            let direct = [
                distance(*first_previous, first_hand),
                distance(*second_previous, second_hand),
            ];
            let crossed = [
                distance(*first_previous, second_hand),
                distance(*second_previous, first_hand),
            ];
            if direct.iter().sum::<f32>() <= crossed.iter().sum::<f32>() {
                direct.to_vec()
            } else {
                crossed.to_vec()
            }
        }
        _ => Vec::new(),
    }
}

#[derive(Default)]
struct ZoomDecision {
    hand_span: Option<f32>,
    requested_magnification: Option<f32>,
    at_limit: bool,
}

fn hand_geometries(landmarks: &[Landmark]) -> Vec<HandGeometry> {
    if !matches!(landmarks.len(), HAND_LANDMARK_COUNT | 42) {
        return Vec::new();
    }
    landmarks
        .chunks_exact(HAND_LANDMARK_COUNT)
        .filter_map(hand_geometry)
        .collect()
}

fn hand_geometry(landmarks: &[Landmark]) -> Option<HandGeometry> {
    let mut minimum_x = f32::INFINITY;
    let mut maximum_x = f32::NEG_INFINITY;
    let mut minimum_y = f32::INFINITY;
    let mut maximum_y = f32::NEG_INFINITY;
    for landmark in landmarks {
        if !landmark.x.is_finite() || !landmark.y.is_finite() {
            return None;
        }
        minimum_x = minimum_x.min(landmark.x);
        maximum_x = maximum_x.max(landmark.x);
        minimum_y = minimum_y.min(landmark.y);
        maximum_y = maximum_y.max(landmark.y);
    }
    Some(HandGeometry {
        center_x: (minimum_x + maximum_x) / 2.0,
        center_y: (minimum_y + maximum_y) / 2.0,
        minimum_x,
        maximum_x,
        minimum_y,
        maximum_y,
    })
}

fn union_bounds(hands: &[HandGeometry]) -> (f32, f32, f32, f32) {
    hands.iter().fold(
        (
            f32::INFINITY,
            f32::NEG_INFINITY,
            f32::INFINITY,
            f32::NEG_INFINITY,
        ),
        |(minimum_x, maximum_x, minimum_y, maximum_y), hand| {
            (
                minimum_x.min(hand.minimum_x),
                maximum_x.max(hand.maximum_x),
                minimum_y.min(hand.minimum_y),
                maximum_y.max(hand.maximum_y),
            )
        },
    )
}

fn proportional_speed(
    pan_error: f32,
    tilt_error: f32,
    pan_direction: i8,
    image_tilt_direction: i8,
) -> f64 {
    let pan_strength = axis_strength(pan_error, pan_direction, PAN_STOP_THRESHOLD);
    let tilt_strength = axis_strength(tilt_error, image_tilt_direction, TILT_STOP_THRESHOLD);
    let strength = f64::from(pan_strength.max(tilt_strength));
    MINIMUM_SPEED_FRACTION + strength * (MAXIMUM_SPEED_FRACTION - MINIMUM_SPEED_FRACTION)
}

fn axis_strength(error: f32, direction: i8, stop_threshold: f32) -> f32 {
    if direction == 0 {
        return 0.0;
    }
    ((error.abs() - stop_threshold) / (MAXIMUM_IMAGE_ERROR - stop_threshold)).clamp(0.0, 1.0)
}

fn ramp_speed(current: f64, requested: f64) -> f64 {
    if requested > current {
        requested.min(current + ACCELERATION_STEP)
    } else {
        requested.max(current - DECELERATION_STEP)
    }
}

fn axis_direction(error: f32, previous: i8, start: f32, stop: f32) -> i8 {
    let direction = if error > 0.0 {
        1
    } else if error < 0.0 {
        -1
    } else {
        0
    };
    if error.abs() >= start {
        direction
    } else if direction == previous && error.abs() > stop {
        previous
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hand_at(x: f32, y: f32, size: f32) -> Vec<Landmark> {
        let mut landmarks = vec![
            Landmark {
                x,
                y,
                z: 0.0,
                visibility: None,
            };
            HAND_LANDMARK_COUNT
        ];
        landmarks[0].x = x - size / 2.0;
        landmarks[1].x = x + size / 2.0;
        landmarks[2].y = y - size / 2.0;
        landmarks[3].y = y + size / 2.0;
        landmarks
    }

    fn two_hands(left_x: f32, right_x: f32) -> Vec<Landmark> {
        let mut landmarks = hand_at(left_x, 0.5, 0.1);
        landmarks.extend(hand_at(right_x, 0.5, 0.1));
        landmarks
    }

    #[test]
    fn two_hands_drive_slow_pan_tilt_and_calibrate_zoom() {
        let mut controller = HandsTrackingController::default();
        controller.set_enabled(true);

        let decision = controller.observe(&two_hands(0.65, 0.85), Some(2.0), 1_000);

        assert_eq!(decision.hands_visible, 2);
        assert!(!decision.zoom_frozen);
        assert!(decision.calibrated);
        assert_eq!(decision.motion.pan_direction, 1);
        assert!(decision.motion.speed_fraction <= ACCELERATION_STEP);
        assert_eq!(decision.requested_magnification, None);
    }

    #[test]
    fn one_remaining_hand_is_followed_immediately_without_zoom() {
        let mut controller = HandsTrackingController::default();
        controller.set_enabled(true);
        controller.observe(&two_hands(0.3, 0.7), Some(2.0), 1_000);

        let decision = controller.observe(&hand_at(0.75, 0.5, 0.1), Some(2.0), 1_200);

        assert_eq!(decision.hands_visible, 1);
        assert!(decision.zoom_frozen);
        assert_eq!(decision.motion.pan_direction, 1);
        assert_eq!(decision.requested_magnification, None);
    }

    #[test]
    fn rapid_remaining_hand_stops_the_camera_instead_of_being_chased() {
        let mut controller = HandsTrackingController::default();
        controller.set_enabled(true);
        let initial = controller.observe(&hand_at(0.5, 0.5, 0.1), Some(2.0), 1_000);
        controller.record_motion(initial.motion);

        let decision = controller.observe(&hand_at(0.8, 0.5, 0.1), Some(2.0), 1_100);

        assert!(decision.rapid_motion);
        assert_eq!(decision.motion, HandsTrackingMotion::default());
        assert!(decision.zoom_frozen);
    }

    #[test]
    fn a_reappearing_second_hand_is_not_mistaken_for_rapid_motion() {
        let mut controller = HandsTrackingController::default();
        controller.set_enabled(true);
        controller.observe(&hand_at(0.3, 0.5, 0.1), Some(2.0), 1_000);

        let decision = controller.observe(&two_hands(0.3, 0.8), Some(2.0), 1_100);

        assert_eq!(decision.hands_visible, 2);
        assert!(!decision.rapid_motion);
        assert!(!decision.zoom_frozen);
    }

    #[test]
    fn no_visible_hands_stops_motion_and_freezes_zoom() {
        let mut controller = HandsTrackingController::default();
        controller.set_enabled(true);
        controller.record_motion(HandsTrackingMotion {
            pan_direction: 1,
            tilt_direction: 0,
            speed_fraction: 0.02,
        });

        let decision = controller.observe(&[], Some(2.0), 1_000);

        assert_eq!(decision.hands_visible, 0);
        assert_eq!(decision.motion, HandsTrackingMotion::default());
        assert!(decision.zoom_frozen);
    }

    #[test]
    fn two_hand_span_changes_adjust_zoom_slowly() {
        let mut controller = HandsTrackingController::default();
        controller.set_enabled(true);
        controller.observe(&two_hands(0.35, 0.65), Some(2.0), 1_000);

        let decision = controller.observe(&two_hands(0.15, 0.85), Some(2.0), 2_000);

        assert!(decision.requested_magnification.unwrap() < 2.0);
        assert!(
            2.0 - decision.requested_magnification.unwrap() <= ZOOM_MAXIMUM_STEP + f32::EPSILON
        );
    }

    #[test]
    fn losing_one_hand_cancels_a_pending_zoom_destination() {
        let mut controller = HandsTrackingController::default();
        controller.set_enabled(true);
        controller.observe(&two_hands(0.35, 0.65), Some(2.0), 1_000);
        controller.observe(&two_hands(0.15, 0.85), Some(2.0), 2_000);
        assert!(controller.zoom_destination.is_some());

        let one_hand = controller.observe(&hand_at(0.5, 0.5, 0.1), Some(1.92), 2_100);

        assert!(one_hand.zoom_frozen);
        assert_eq!(one_hand.requested_magnification, None);
        assert_eq!(controller.zoom_destination, None);
    }

    #[test]
    fn invalid_landmark_groups_are_treated_as_no_hands() {
        let mut controller = HandsTrackingController::default();
        controller.set_enabled(true);

        let decision = controller.observe(&hand_at(f32::NAN, 0.5, 0.1), Some(2.0), 1_000);

        assert_eq!(decision.hands_visible, 0);
        assert!(decision.zoom_frozen);
    }
}
