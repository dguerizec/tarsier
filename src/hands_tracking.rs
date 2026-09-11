use crate::model::Landmark;

const HAND_LANDMARK_COUNT: usize = 21;
const ARM_RECOVERY_MS: u64 = 800;
const ARM_MINIMUM_VISIBILITY: f32 = 0.6;
const ARM_HAND_MATCH_DISTANCE: f32 = 0.20;
const ARM_RECOVERY_SPEED: f64 = 0.08;
const ARM_RECOVERY_ZOOM_STEP: f32 = 0.12;
const TARGET_X: f32 = 0.5;
const TARGET_Y: f32 = 0.5;
const PAN_START_THRESHOLD: f32 = 0.10;
const PAN_STOP_THRESHOLD: f32 = 0.06;
const TILT_START_THRESHOLD: f32 = 0.11;
const TILT_STOP_THRESHOLD: f32 = 0.065;
const MINIMUM_SPEED_FRACTION: f64 = 0.01;
const BASE_MAXIMUM_SPEED_FRACTION: f64 = 0.15;
const MAXIMUM_SPEED_FRACTION: f64 = 0.60;
const MAXIMUM_ACCELERATION_STEP: f64 = 0.18;
const ACCELERATION_STEP: f64 = 0.027;
const DECELERATION_STEP: f64 = 0.02;
const MAXIMUM_IMAGE_ERROR: f32 = 0.5;
const RAPID_HAND_SPEED_PER_SECOND: f32 = 0.85;
const MAXIMUM_SPEED_SAMPLE_INTERVAL_MS: u64 = 500;
const MINIMUM_HAND_SPAN: f32 = 0.03;
const MINIMUM_TARGET_SPAN: f32 = 0.18;
const MAXIMUM_TARGET_SPAN: f32 = 0.62;
const SPAN_SMOOTHING_ALPHA: f32 = 0.40;
const ZOOM_START_THRESHOLD_FRACTION: f32 = 0.08;
const ZOOM_MAXIMUM_STEP: f32 = 0.08;
const EDGE_ZOOM_MAXIMUM_STEP: f32 = 0.20;
const EDGE_MARGIN_START: f32 = 0.15;
const EDGE_MARGIN_FULL: f32 = 0.03;
const ZOOM_MINIMUM_STEP: f32 = 0.01;
const ZOOM_RAMP_GAIN: f32 = 0.50;
const ZOOM_DESTINATION_TOLERANCE: f32 = 0.005;
const ZOOM_MINIMUM_INTERVAL_MS: u64 = 100;
const ZOOM_SETTLE_MS: u64 = 150;
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
    pub recovering_arms: bool,
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
    arm_last_hand_at_ms: [Option<u64>; 2],
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
        self.arm_last_hand_at_ms = [None; 2];
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
        self.arm_last_hand_at_ms = [None; 2];
        self.last_zoom_command_at_ms = None;
        self.zoom_destination = None;
        self.zoom_settle_until_ms = None;
    }

    #[cfg(test)]
    pub fn observe(
        &mut self,
        landmarks: &[Landmark],
        zoom_magnification: Option<f32>,
        captured_at_ms: u64,
    ) -> HandsTrackingDecision {
        self.observe_with_pose(landmarks, &[], zoom_magnification, captured_at_ms)
    }

    pub fn observe_with_pose(
        &mut self,
        landmarks: &[Landmark],
        pose_landmarks: &[Landmark],
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
        let recovery = self.arm_recovery_target(&hands, pose_landmarks, captured_at_ms);
        if let Some((x, y)) = recovery.filter(|_| !rapid_motion) {
            self.smoothed_span = None;
            self.zoom_destination = None;
            self.zoom_settle_until_ms = None;
            let mut motion = self.motion_for_target(x, y);
            motion.speed_fraction = motion.speed_fraction.min(ARM_RECOVERY_SPEED);
            let zoom = zoom_magnification.filter(|zoom| {
                zoom.is_finite()
                    && (MINIMUM_ZOOM_MAGNIFICATION..=MAXIMUM_ZOOM_MAGNIFICATION).contains(zoom)
            });
            let at_limit = zoom.is_some_and(|zoom| zoom <= MINIMUM_ZOOM_MAGNIFICATION);
            let due = self.last_zoom_command_at_ms.is_none_or(|previous| {
                captured_at_ms.saturating_sub(previous) >= ZOOM_MINIMUM_INTERVAL_MS
            });
            let requested_magnification = zoom.filter(|_| !at_limit && due).map(|zoom| {
                ((zoom - ARM_RECOVERY_ZOOM_STEP).max(MINIMUM_ZOOM_MAGNIFICATION) * 100.0).round()
                    / 100.0
            });
            if requested_magnification.is_some() {
                self.last_zoom_command_at_ms = Some(captured_at_ms);
            }
            return HandsTrackingDecision {
                hands_visible,
                recovering_arms: true,
                zoom_frozen: zoom.is_none() || at_limit,
                target_x: Some(x),
                target_y: Some(y),
                motion,
                calibrated: self.target_span.is_some(),
                target_span: self.target_span,
                requested_magnification,
                at_limit,
                ..HandsTrackingDecision::default()
            };
        }
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
        let edge_urgency = if hands_visible == 2 {
            let (min_x, max_x, min_y, max_y) = union_bounds(&hands);
            let margin = min_x.min(1.0 - max_x).min(min_y).min(1.0 - max_y);
            ((EDGE_MARGIN_START - margin) / (EDGE_MARGIN_START - EDGE_MARGIN_FULL)).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let zoom = self.zoom_decision(
            hand_span,
            zoom_magnification,
            captured_at_ms,
            zoom_frozen,
            edge_urgency,
        );

        HandsTrackingDecision {
            hands_visible,
            rapid_motion,
            recovering_arms: false,
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

    fn arm_recovery_target(
        &mut self,
        hands: &[HandGeometry],
        pose: &[Landmark],
        captured_at_ms: u64,
    ) -> Option<(f32, f32)> {
        if self
            .recalibrate_zoom_after_ms
            .is_some_and(|after| captured_at_ms < after)
        {
            return None;
        }
        let arms = [
            arm_endpoint(pose, 11, 13, 15),
            arm_endpoint(pose, 12, 14, 16),
        ];
        // Match each detected hand to at most one arm, independent of hand order.
        let mut pairs = Vec::new();
        for (side, arm) in arms.iter().enumerate() {
            if let Some((x, y)) = arm {
                for (index, hand) in hands.iter().enumerate() {
                    let distance = (x - hand.center_x).hypot(y - hand.center_y);
                    if distance <= ARM_HAND_MATCH_DISTANCE {
                        pairs.push((distance, side, index));
                    }
                }
            }
        }
        pairs.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut matched = [false; 2];
        let mut used_hands = [false; 2];
        for (_, side, index) in pairs {
            if !matched[side] && !used_hands[index] {
                matched[side] = true;
                used_hands[index] = true;
                self.arm_last_hand_at_ms[side] = Some(captured_at_ms);
            }
        }
        if hands.len() == 2 {
            return None;
        }
        let mut targets = Vec::new();
        for (side, arm) in arms.into_iter().enumerate() {
            let recent = self.arm_last_hand_at_ms[side].is_some_and(|last| {
                captured_at_ms >= last && captured_at_ms - last <= ARM_RECOVERY_MS
            });
            if !matched[side] && recent {
                if let Some((x, y)) = arm {
                    if x.min(1.0 - x).min(y).min(1.0 - y) < EDGE_MARGIN_START {
                        targets.push((x.clamp(0.0, 1.0), y.clamp(0.0, 1.0)));
                    }
                }
            }
        }
        if targets.is_empty() {
            return None;
        }
        targets.extend(hands.iter().map(|hand| (hand.center_x, hand.center_y)));
        let count = targets.len() as f32;
        Some((
            targets.iter().map(|p| p.0).sum::<f32>() / count,
            targets.iter().map(|p| p.1).sum::<f32>() / count,
        ))
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
            let strength = tracking_strength(
                x - TARGET_X,
                y - TARGET_Y,
                pan_direction,
                image_tilt_direction,
            );
            let previous = (self.motion.pan_direction, self.motion.tilt_direction);
            let continuing = (pan_direction != 0 && pan_direction == previous.0)
                || (tilt_direction != 0 && tilt_direction == previous.1);
            let reversing = pan_direction * previous.0 < 0 || tilt_direction * previous.1 < 0;
            let current = if continuing && !reversing {
                self.motion.speed_fraction
            } else {
                0.0
            };
            // Use gradual catch-up and an edge plateau with stronger hand gains.
            let offset = ((strength - 0.15) / 0.85).clamp(0.0, 1.0);
            let catch_up = offset * offset * (3.0 - 2.0 * offset);
            let requested = MINIMUM_SPEED_FRACTION
                + strength * (BASE_MAXIMUM_SPEED_FRACTION - MINIMUM_SPEED_FRACTION)
                + catch_up * (MAXIMUM_SPEED_FRACTION - BASE_MAXIMUM_SPEED_FRACTION);
            let acceleration = if reversing {
                ACCELERATION_STEP
            } else {
                ACCELERATION_STEP + catch_up * (MAXIMUM_ACCELERATION_STEP - ACCELERATION_STEP)
            };
            ramp_speed(current, requested, acceleration)
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
        edge_urgency: f32,
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
        // React to the current extent near a border instead of waiting for the
        // smoothed span or a previous zoom destination to catch up.
        let urgency = if hand_span > self.target_span.unwrap_or(hand_span) {
            edge_urgency
        } else {
            0.0
        };
        let alpha = SPAN_SMOOTHING_ALPHA + urgency * (1.0 - SPAN_SMOOTHING_ALPHA);
        let smoothed = self.smoothed_span.map_or(hand_span, |previous| {
            previous + alpha * (hand_span - previous)
        });
        if urgency > 0.0 {
            self.zoom_settle_until_ms = None;
            self.zoom_destination = None;
        }
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
        let outward_urgency = if remaining < 0.0 { urgency } else { 0.0 };
        let maximum_step =
            ZOOM_MAXIMUM_STEP + outward_urgency * (EDGE_ZOOM_MAXIMUM_STEP - ZOOM_MAXIMUM_STEP);
        let gain = ZOOM_RAMP_GAIN + outward_urgency * (0.85 - ZOOM_RAMP_GAIN);
        let step = (remaining.abs() * gain)
            .clamp(ZOOM_MINIMUM_STEP, maximum_step)
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

// Prefer the pose wrist; only extrapolate when the visible elbow itself is
// near a border. Central or low-confidence arms are not evidence of an exit.
fn arm_endpoint(
    pose: &[Landmark],
    shoulder: usize,
    elbow: usize,
    wrist: usize,
) -> Option<(f32, f32)> {
    if pose.len() != 33 {
        return None;
    }
    let reliable = |joint: &Landmark| {
        joint.x.is_finite()
            && joint.y.is_finite()
            && joint
                .visibility
                .is_some_and(|v| v.is_finite() && v >= ARM_MINIMUM_VISIBILITY)
    };
    let shoulder = &pose[shoulder];
    let elbow = &pose[elbow];
    if !reliable(shoulder)
        || !reliable(elbow)
        || !(0.0..=1.0).contains(&shoulder.x)
        || !(0.0..=1.0).contains(&shoulder.y)
        || !(0.0..=1.0).contains(&elbow.x)
        || !(0.0..=1.0).contains(&elbow.y)
    {
        return None;
    }
    let wrist = &pose[wrist];
    if reliable(wrist) && (-0.2..=1.2).contains(&wrist.x) && (-0.2..=1.2).contains(&wrist.y) {
        return Some((wrist.x, wrist.y));
    }
    if elbow.x.min(1.0 - elbow.x).min(elbow.y).min(1.0 - elbow.y) > 0.2 {
        return None;
    }
    Some((
        elbow.x + (elbow.x - shoulder.x),
        elbow.y + (elbow.y - shoulder.y),
    ))
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

fn tracking_strength(
    pan_error: f32,
    tilt_error: f32,
    pan_direction: i8,
    image_tilt_direction: i8,
) -> f64 {
    let pan_strength = axis_strength(pan_error, pan_direction, PAN_STOP_THRESHOLD);
    let tilt_strength = axis_strength(tilt_error, image_tilt_direction, TILT_STOP_THRESHOLD);
    f64::from(pan_strength.max(tilt_strength))
}

fn axis_strength(error: f32, direction: i8, stop_threshold: f32) -> f32 {
    if direction == 0 {
        return 0.0;
    }
    ((error.abs() - stop_threshold) / (MAXIMUM_IMAGE_ERROR - stop_threshold)).clamp(0.0, 1.0)
}

fn ramp_speed(current: f64, requested: f64, acceleration: f64) -> f64 {
    if requested > current {
        requested.min(current + acceleration)
    } else {
        requested.max(current - DECELERATION_STEP.max(current * 0.25))
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

    fn rightward_arm(wrist_x: f32) -> Vec<Landmark> {
        let mut pose = vec![
            Landmark {
                x: 0.5,
                y: 0.5,
                z: 0.0,
                visibility: Some(0.0)
            };
            33
        ];
        for (index, x) in [(11, 0.5), (13, 0.8), (15, wrist_x)] {
            pose[index].x = x;
            pose[index].visibility = Some(0.9);
        }
        pose
    }

    #[test]
    fn arm_recovery_zooms_out_then_returns_to_a_reappearing_hand() {
        let mut controller = HandsTrackingController::default();
        controller.set_enabled(true);
        controller.observe_with_pose(
            &hand_at(0.9, 0.5, 0.1),
            &rightward_arm(0.9),
            Some(2.0),
            1_000,
        );
        let missing = controller.observe_with_pose(&[], &rightward_arm(1.05), Some(2.0), 1_100);
        assert!(missing.recovering_arms);
        assert_eq!(missing.hands_visible, 0);
        assert_eq!(missing.motion.pan_direction, 1);
        assert!(missing.motion.speed_fraction <= ARM_RECOVERY_SPEED);
        assert_eq!(missing.requested_magnification, Some(1.88));
        let too_soon = controller.observe_with_pose(&[], &rightward_arm(1.05), Some(1.88), 1_150);
        assert_eq!(too_soon.requested_magnification, None);
        let returned = controller.observe_with_pose(
            &hand_at(0.9, 0.5, 0.1),
            &rightward_arm(0.9),
            Some(1.88),
            1_200,
        );
        assert!(!returned.recovering_arms);
        assert!(returned.zoom_frozen);
        assert_eq!(returned.requested_magnification, None);
    }

    #[test]
    fn arm_recovery_requires_recent_hands_and_valid_outward_arm_evidence() {
        let mut controller = HandsTrackingController::default();
        controller.set_enabled(true);
        assert!(
            !controller
                .observe_with_pose(&[], &rightward_arm(1.05), Some(2.0), 900)
                .recovering_arms
        );
        controller.observe_with_pose(
            &hand_at(0.9, 0.5, 0.1),
            &rightward_arm(0.9),
            Some(2.0),
            1_000,
        );
        assert!(
            !controller
                .observe_with_pose(&[], &rightward_arm(0.6), Some(2.0), 1_100)
                .recovering_arms
        );
        let mut unreliable = rightward_arm(1.05);
        unreliable[13].visibility = Some(0.1);
        assert!(
            !controller
                .observe_with_pose(&[], &unreliable, Some(2.0), 1_200)
                .recovering_arms
        );
        unreliable[13].visibility = Some(f32::NAN);
        assert!(
            !controller
                .observe_with_pose(&[], &unreliable, Some(2.0), 1_300)
                .recovering_arms
        );
        assert!(
            !controller
                .observe_with_pose(&[], &rightward_arm(1.05), Some(2.0), 1_801)
                .recovering_arms
        );
    }

    #[test]
    fn arm_recovery_can_extrapolate_a_missing_wrist_and_respects_zoom_limits() {
        let mut controller = HandsTrackingController::default();
        controller.set_enabled(true);
        controller.observe_with_pose(
            &hand_at(0.9, 0.5, 0.1),
            &rightward_arm(0.9),
            Some(1.05),
            1_000,
        );
        let mut pose = rightward_arm(1.05);
        pose[15].visibility = Some(0.0);
        pose[13].x = 0.85;
        let missing = controller.observe_with_pose(&[], &pose, Some(1.05), 1_100);
        assert!(missing.recovering_arms);
        assert_eq!(missing.requested_magnification, Some(1.0));
        let at_limit = controller.observe_with_pose(&[], &pose, Some(1.0), 1_200);
        assert!(at_limit.at_limit);
        assert_eq!(at_limit.requested_magnification, None);
        controller.set_enabled(false);
        controller.set_enabled(true);
        assert!(
            !controller
                .observe_with_pose(&[], &pose, Some(2.0), 1_300)
                .recovering_arms
        );
    }

    #[test]
    fn remaining_hand_does_not_extend_the_missing_arms_recovery_window() {
        let mut controller = HandsTrackingController::default();
        controller.set_enabled(true);
        let mut pose = rightward_arm(0.9);
        for (index, x) in [(12, 0.5), (14, 0.2), (16, 0.1)] {
            pose[index].x = x;
            pose[index].visibility = Some(0.9);
        }
        controller.observe_with_pose(&two_hands(0.1, 0.9), &pose, Some(2.0), 1_000);
        pose[15].x = 1.05;
        let left = hand_at(0.1, 0.5, 0.1);
        let missing = controller.observe_with_pose(&left, &pose, Some(2.0), 1_200);
        assert!(missing.recovering_arms);
        assert_eq!(missing.hands_visible, 1);
        assert!(missing.requested_magnification.unwrap() < 2.0);
        let expired = controller.observe_with_pose(&left, &pose, Some(1.88), 1_801);
        assert!(!expired.recovering_arms);
        assert!(expired.zoom_frozen);
    }

    #[test]
    fn two_hands_drive_ramped_pan_tilt_and_calibrate_zoom() {
        let mut controller = HandsTrackingController::default();
        controller.set_enabled(true);

        let decision = controller.observe(&two_hands(0.65, 0.85), Some(2.0), 1_000);

        assert_eq!(decision.hands_visible, 2);
        assert!(!decision.zoom_frozen);
        assert!(decision.calibrated);
        assert_eq!(decision.motion.pan_direction, 1);
        assert!(decision.motion.speed_fraction > ACCELERATION_STEP);
        assert!(decision.motion.speed_fraction <= MAXIMUM_ACCELERATION_STEP);
        assert_eq!(decision.requested_magnification, None);
    }

    #[test]
    fn catch_up_accelerates_earlier_and_flattens_near_the_edge() {
        for vertical in [false, true] {
            let controller = HandsTrackingController::default();
            let speeds: Vec<_> = [0.62, 0.7, 0.8, 0.9, 1.0]
                .into_iter()
                .map(|position| {
                    let (x, y) = if vertical {
                        (0.5, position)
                    } else {
                        (position, 0.5)
                    };
                    controller.motion_for_target(x, y).speed_fraction
                })
                .collect();
            assert!(speeds.windows(2).all(|pair| pair[1] > pair[0]));
            assert!(speeds[2] > 0.055);
            assert!(speeds[4] <= MAXIMUM_ACCELERATION_STEP);
            assert!(speeds[4] - speeds[3] < speeds[2] - speeds[1]);
        }
    }

    #[test]
    fn axis_transitions_preserve_speed_and_return_to_center_brakes() {
        let mut controller = HandsTrackingController::default();
        controller.record_motion(HandsTrackingMotion {
            pan_direction: 1,
            tilt_direction: 0,
            speed_fraction: 0.15,
        });
        let diagonal = controller.motion_for_target(0.9, 0.7);
        assert!(diagonal.speed_fraction > 0.15);
        controller.record_motion(diagonal);
        let horizontal = controller.motion_for_target(0.9, 0.5);
        assert!(horizontal.speed_fraction >= diagonal.speed_fraction);
        controller.record_motion(horizontal);
        let braking = controller.motion_for_target(0.58, 0.5);
        assert!(braking.speed_fraction <= horizontal.speed_fraction * 0.75);
        controller.record_motion(braking);
        assert_eq!(
            controller.motion_for_target(0.5, 0.5),
            HandsTrackingMotion::default()
        );
    }

    #[test]
    fn one_hand_reaches_a_stronger_bounded_catch_up_speed() {
        let mut controller = HandsTrackingController::default();
        controller.set_enabled(true);
        let landmarks = hand_at(0.8, 0.5, 0.1);
        let first = controller.observe(&landmarks, Some(2.0), 1_000);
        assert!(first.motion.speed_fraction > 0.08);
        controller.record_motion(first.motion);
        for frame in 1..=10 {
            let decision = controller.observe(&landmarks, Some(2.0), 1_000 + frame * 100);
            assert!(!decision.rapid_motion);
            assert!(decision.zoom_frozen);
            assert!(decision.motion.speed_fraction <= MAXIMUM_SPEED_FRACTION);
            controller.record_motion(decision.motion);
        }
        assert!(controller.motion().speed_fraction > 0.28);
        let lost = controller.observe(&[], Some(2.0), 2_100);
        assert_eq!(lost.motion, HandsTrackingMotion::default());
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

        let decision = controller.observe(&two_hands(0.25, 0.75), Some(2.0), 2_000);

        assert!(decision.requested_magnification.unwrap() < 2.0);
        assert!(
            2.0 - decision.requested_magnification.unwrap() <= ZOOM_MAXIMUM_STEP + f32::EPSILON
        );
    }

    #[test]
    fn two_hand_zoom_advances_on_each_100_ms_observation() {
        let mut controller = HandsTrackingController::default();
        controller.set_enabled(true);
        controller.zoom_decision(Some(0.4), Some(2.0), 1_000, false, 0.0);
        let mut zoom = 2.0;
        for at in [1_100, 1_200, 1_300] {
            let decision = controller.zoom_decision(Some(0.8), Some(zoom), at, false, 0.0);
            let requested = decision.requested_magnification.unwrap();
            assert!(requested < zoom);
            assert!(zoom - requested <= ZOOM_MAXIMUM_STEP + f32::EPSILON);
            zoom = requested;
            let too_soon = controller.zoom_decision(Some(0.8), Some(zoom), at + 50, false, 0.0);
            assert_eq!(too_soon.requested_magnification, None);
        }
        assert!(zoom < 1.8);
    }

    #[test]
    fn zoom_uses_fine_final_steps_then_resumes_after_a_short_settle() {
        let mut controller = HandsTrackingController::default();
        controller.set_enabled(true);
        controller.zoom_decision(Some(0.4), Some(2.0), 1_000, false, 0.0);
        controller.zoom_destination = Some(2.01);
        let fine = controller.zoom_decision(Some(0.4), Some(2.0), 1_100, false, 0.0);
        assert_eq!(fine.requested_magnification, Some(2.01));
        let arrived = controller.zoom_decision(Some(0.4), Some(2.01), 1_200, false, 0.0);
        assert_eq!(arrived.requested_magnification, None);
        assert_eq!(controller.zoom_settle_until_ms, Some(1_350));
        let settling = controller.zoom_decision(Some(0.2), Some(2.01), 1_300, false, 0.0);
        assert_eq!(settling.requested_magnification, None);
        let resumed = controller.zoom_decision(Some(0.2), Some(2.01), 1_400, false, 0.0);
        assert!(resumed.requested_magnification.unwrap() > 2.01);
    }

    #[test]
    fn approaching_edges_progressively_strengthens_zoom_out() {
        let mut previous_step = 0.0;
        for urgency in [0.0, 0.5, 1.0] {
            let mut controller = HandsTrackingController::default();
            controller.zoom_decision(Some(0.4), Some(2.0), 1_000, false, 0.0);
            let decision = controller.zoom_decision(Some(0.9), Some(2.0), 1_100, false, urgency);
            let step = 2.0 - decision.requested_magnification.unwrap();
            assert!(step > previous_step);
            assert!(step <= EDGE_ZOOM_MAXIMUM_STEP + f32::EPSILON);
            previous_step = step;
        }
    }

    #[test]
    fn edge_hands_bypass_settling_but_still_freeze_when_one_disappears() {
        let mut controller = HandsTrackingController::default();
        controller.set_enabled(true);
        controller.observe(&two_hands(0.35, 0.65), Some(2.0), 1_000);
        controller.zoom_settle_until_ms = Some(2_500);
        let edge = controller.observe(&two_hands(0.08, 0.92), Some(2.0), 2_000);
        assert!(!edge.rapid_motion);
        assert!((edge.requested_magnification.unwrap() - 1.8).abs() < 0.001);
        let lost = controller.observe(&hand_at(0.08, 0.5, 0.1), Some(1.8), 2_100);
        assert!(lost.zoom_frozen);
        assert_eq!(lost.requested_magnification, None);
        assert_eq!(controller.zoom_destination, None);
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
