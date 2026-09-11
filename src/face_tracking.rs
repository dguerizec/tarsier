use crate::model::Landmark;

const FACE_LANDMARK_COUNT: usize = 478;
const POSE_LANDMARK_COUNT: usize = 33;
const LEFT_SHOULDER_INDEX: usize = 11;
const RIGHT_SHOULDER_INDEX: usize = 12;
const MINIMUM_SHOULDER_VISIBILITY: f32 = 0.5;
const ESTIMATED_FACE_OFFSET_IN_SHOULDER_WIDTHS: f32 = 0.5;
const SHOULDER_CALIBRATION_ALPHA: f32 = 0.2;
const TARGET_X: f32 = 0.5;
const TARGET_Y: f32 = 0.5;
const PAN_START_THRESHOLD: f32 = 0.08;
const PAN_STOP_THRESHOLD: f32 = 0.04;
const TILT_START_THRESHOLD: f32 = 0.09;
const TILT_STOP_THRESHOLD: f32 = 0.045;
const MINIMUM_SPEED_FRACTION: f64 = 0.01;
const BASE_MAXIMUM_SPEED_FRACTION: f64 = 0.10;
const MAXIMUM_SPEED_FRACTION: f64 = 0.40;
const MAXIMUM_ACCELERATION_STEP: f64 = 0.12;
const ACCELERATION_STEP: f64 = 0.018;
const DECELERATION_STEP: f64 = 0.02;
const MAXIMUM_IMAGE_ERROR: f32 = 0.5;
const MINIMUM_FACE_SIZE: f32 = 0.02;
const FACE_SIZE_SMOOTHING_ALPHA: f32 = 0.35;
const AUTO_ZOOM_START_THRESHOLD_FRACTION: f32 = 0.06;
const AUTO_ZOOM_STOP_THRESHOLD_FRACTION: f32 = 0.02;
const AUTO_ZOOM_MAXIMUM_STEP: f32 = 0.16;
const AUTO_ZOOM_MINIMUM_STEP: f32 = 0.03;
const AUTO_ZOOM_RAMP_GAIN: f32 = 0.55;
const AUTO_ZOOM_DESTINATION_TOLERANCE: f32 = 0.02;
const AUTO_ZOOM_MINIMUM_INTERVAL_MS: u64 = 100;
const AUTO_ZOOM_SETTLE_MS: u64 = 300;
pub const MANUAL_ZOOM_SETTLE_MS: u64 = 600;
const MINIMUM_ZOOM_MAGNIFICATION: f32 = 1.0;
const MAXIMUM_ZOOM_MAGNIFICATION: f32 = 4.0;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct FaceTrackingMotion {
    pub pan_direction: i8,
    pub tilt_direction: i8,
    pub speed_fraction: f64,
}

impl FaceTrackingMotion {
    pub fn active(self) -> bool {
        self.pan_direction != 0 || self.tilt_direction != 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FaceTarget {
    pub x: f32,
    pub y: f32,
    pub size: Option<f32>,
    pub motion: FaceTrackingMotion,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct AutoZoomDecision {
    pub enabled: bool,
    pub calibrated: bool,
    pub target_face_size: Option<f32>,
    pub face_size: Option<f32>,
    pub requested_magnification: Option<f32>,
    pub at_limit: bool,
}

#[derive(Default)]
pub struct FaceTrackingController {
    enabled: bool,
    motion: FaceTrackingMotion,
    tracking_zoom: f32,
    shoulder_face_y_offset: Option<f32>,
    auto_zoom_enabled: bool,
    auto_zoom_target_face_size: Option<f32>,
    smoothed_face_size: Option<f32>,
    recalibrate_auto_zoom_after_ms: Option<u64>,
    last_auto_zoom_command_at_ms: Option<u64>,
    last_auto_zoom_applied_at_ms: Option<u64>,
    auto_zoom_destination: Option<f32>,
    auto_zoom_settle_until_ms: Option<u64>,
}

impl FaceTrackingController {
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        self.motion = FaceTrackingMotion::default();
        self.shoulder_face_y_offset = None;
        if !enabled {
            self.set_auto_zoom_enabled(false, 0);
        }
    }

    pub fn auto_zoom_enabled(&self) -> bool {
        self.auto_zoom_enabled
    }

    pub fn set_auto_zoom_enabled(&mut self, enabled: bool, calibrate_after_ms: u64) {
        self.auto_zoom_enabled = enabled;
        self.auto_zoom_target_face_size = None;
        self.smoothed_face_size = None;
        self.recalibrate_auto_zoom_after_ms = enabled.then_some(calibrate_after_ms);
        self.last_auto_zoom_command_at_ms = None;
        self.last_auto_zoom_applied_at_ms = None;
        self.auto_zoom_destination = None;
        self.auto_zoom_settle_until_ms = None;
    }

    pub fn recalibrate_auto_zoom_after(&mut self, calibrate_after_ms: u64) {
        if !self.auto_zoom_enabled {
            return;
        }
        self.auto_zoom_target_face_size = None;
        self.smoothed_face_size = None;
        self.recalibrate_auto_zoom_after_ms = Some(calibrate_after_ms);
        self.last_auto_zoom_command_at_ms = None;
        self.last_auto_zoom_applied_at_ms = None;
        self.auto_zoom_destination = None;
        self.auto_zoom_settle_until_ms = None;
    }

    pub fn record_auto_zoom_applied(&mut self, applied_at_ms: u64) {
        self.last_auto_zoom_applied_at_ms = Some(applied_at_ms);
        // The next image has a different scale; do not blend it with samples
        // taken before the zoom command.
        self.smoothed_face_size = None;
    }

    pub fn set_tracking_zoom(&mut self, zoom: Option<f32>) {
        // Unknown zoom uses the most conservative gain until a value is available.
        self.tracking_zoom = zoom
            .filter(|zoom| zoom.is_finite() && (1.0..=4.0).contains(zoom))
            .unwrap_or(MAXIMUM_ZOOM_MAGNIFICATION);
    }

    pub fn motion(&self) -> FaceTrackingMotion {
        self.motion
    }

    pub fn record_motion(&mut self, motion: FaceTrackingMotion) {
        self.motion = motion;
    }

    pub fn face_target(
        &mut self,
        face_landmarks: &[Landmark],
        pose_landmarks: &[Landmark],
    ) -> Option<FaceTarget> {
        let (x, y, size) = face_geometry(face_landmarks)?;
        if let Some((_, shoulder_y, _)) = shoulder_geometry(pose_landmarks) {
            let observed_offset = y - shoulder_y;
            self.shoulder_face_y_offset = Some(
                self.shoulder_face_y_offset
                    .map_or(observed_offset, |previous| {
                        previous + SHOULDER_CALIBRATION_ALPHA * (observed_offset - previous)
                    }),
            );
        }
        let mut target = self.target_at(x, y);
        target.size = size;
        Some(target)
    }

    pub fn shoulder_target(&self, landmarks: &[Landmark]) -> Option<FaceTarget> {
        let (x, shoulder_y, shoulder_width) = shoulder_geometry(landmarks)?;
        let y = (shoulder_y
            + self
                .shoulder_face_y_offset
                .unwrap_or(-shoulder_width * ESTIMATED_FACE_OFFSET_IN_SHOULDER_WIDTHS))
        .clamp(0.0, 1.0);
        Some(self.target_at(x, y))
    }

    fn target_at(&self, x: f32, y: f32) -> FaceTarget {
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
            // Keep the ramp when an axis joins or leaves an ongoing movement.
            // Only a reversal or a fresh start requires accelerating from rest.
            let previous = (self.motion.pan_direction, self.motion.tilt_direction);
            let continuing = (pan_direction != 0 && pan_direction == previous.0)
                || (tilt_direction != 0 && tilt_direction == previous.1);
            let reversing = pan_direction * previous.0 < 0 || tilt_direction * previous.1 < 0;
            let current = if continuing && !reversing {
                self.motion.speed_fraction
            } else {
                0.0
            };
            let offset = ((strength - 0.15) / 0.85).clamp(0.0, 1.0);
            // Smoothstep brings catch-up forward into medium offsets and flattens
            // its response near the edge, without a slope jump at either end.
            let catch_up = offset * offset * (3.0 - 2.0 * offset);
            let requested = MINIMUM_SPEED_FRACTION
                + strength * (BASE_MAXIMUM_SPEED_FRACTION - MINIMUM_SPEED_FRACTION)
                + catch_up * (MAXIMUM_SPEED_FRACTION - BASE_MAXIMUM_SPEED_FRACTION);
            let acceleration = if reversing {
                ACCELERATION_STEP
            } else {
                ACCELERATION_STEP + catch_up * (MAXIMUM_ACCELERATION_STEP - ACCELERATION_STEP)
            };
            // Zoom amplifies image motion. Reduce both gain and acceleration,
            // with extra damping at high magnification to avoid hunting.
            let zoom = f64::from(self.tracking_zoom.clamp(1.0, 4.0));
            let gain = 1.0 / (zoom * zoom);
            let current = current.min(MAXIMUM_SPEED_FRACTION * gain);
            ramp_speed(current, requested * gain, acceleration * gain, zoom)
        };
        FaceTarget {
            x,
            y,
            size: None,
            motion: FaceTrackingMotion {
                pan_direction,
                tilt_direction,
                speed_fraction,
            },
        }
    }

    pub fn auto_zoom(
        &mut self,
        face_size: Option<f32>,
        zoom_magnification: Option<f32>,
        captured_at_ms: u64,
    ) -> AutoZoomDecision {
        let mut decision = AutoZoomDecision {
            enabled: self.auto_zoom_enabled,
            calibrated: self.auto_zoom_target_face_size.is_some(),
            target_face_size: self.auto_zoom_target_face_size,
            ..AutoZoomDecision::default()
        };
        if !self.auto_zoom_enabled {
            return decision;
        }
        let face_size = face_size.filter(|size| size.is_finite() && *size >= MINIMUM_FACE_SIZE);
        decision.face_size = face_size;
        let Some(face_size) = face_size else {
            self.smoothed_face_size = None;
            self.auto_zoom_destination = None;
            self.auto_zoom_settle_until_ms = None;
            self.last_auto_zoom_command_at_ms = None;
            return decision;
        };
        if self
            .recalibrate_auto_zoom_after_ms
            .is_some_and(|minimum| captured_at_ms < minimum)
        {
            return decision;
        }

        if self
            .last_auto_zoom_applied_at_ms
            .is_some_and(|applied| captured_at_ms <= applied)
        {
            return decision;
        }

        if self.auto_zoom_target_face_size.is_none() {
            self.auto_zoom_target_face_size = Some(face_size);
            self.smoothed_face_size = Some(face_size);
            self.recalibrate_auto_zoom_after_ms = None;
            decision.calibrated = true;
            decision.target_face_size = Some(face_size);
            return decision;
        }

        let smoothed = self.smoothed_face_size.map_or(face_size, |previous| {
            previous + FACE_SIZE_SMOOTHING_ALPHA * (face_size - previous)
        });
        self.smoothed_face_size = Some(smoothed);
        decision.face_size = Some(smoothed);
        let Some(current_zoom) = zoom_magnification.filter(|zoom| {
            zoom.is_finite()
                && (MINIMUM_ZOOM_MAGNIFICATION..=MAXIMUM_ZOOM_MAGNIFICATION).contains(zoom)
        }) else {
            return decision;
        };

        let target = self.auto_zoom_target_face_size.unwrap_or(face_size);
        let observed_error = face_size / target - 1.0;
        // A destination estimated from an older frame must not keep zooming
        // once the measured face has regained its reference size.
        if observed_error.abs() <= AUTO_ZOOM_STOP_THRESHOLD_FRACTION {
            if self.auto_zoom_destination.take().is_some() {
                self.auto_zoom_settle_until_ms =
                    Some(captured_at_ms.saturating_add(AUTO_ZOOM_SETTLE_MS));
            }
            self.smoothed_face_size = Some(face_size);
            decision.face_size = Some(face_size);
            return decision;
        }
        let mut current_size = smoothed;
        if self
            .auto_zoom_destination
            .is_some_and(|destination| observed_error * (destination - current_zoom) > 0.0)
        {
            // The subject crossed the target size. Discard lagging samples
            // before choosing the opposite correction direction.
            self.auto_zoom_destination = None;
            self.auto_zoom_settle_until_ms = None;
            self.smoothed_face_size = Some(face_size);
            current_size = face_size;
            decision.face_size = Some(face_size);
        }
        if let Some(settle_until) = self.auto_zoom_settle_until_ms {
            if captured_at_ms < settle_until {
                return decision;
            }
            self.auto_zoom_settle_until_ms = None;
            self.smoothed_face_size = Some(face_size);
            current_size = face_size;
            decision.face_size = Some(face_size);
        }

        // Compare both directions to the same reference size, not to the
        // measured size (which gave zoom-in and zoom-out different thresholds).
        let relative_error = 1.0 - current_size / target;
        if let Some(destination) = self.auto_zoom_destination {
            let destination_direction = (destination - current_zoom).signum();
            if relative_error.abs() >= AUTO_ZOOM_START_THRESHOLD_FRACTION
                && relative_error.signum() != destination_direction
            {
                self.auto_zoom_destination = None;
            }
        }

        if self.auto_zoom_destination.is_none() {
            if relative_error.abs() < AUTO_ZOOM_START_THRESHOLD_FRACTION {
                return decision;
            }
            let unconstrained = current_zoom * target / current_size;
            let bounded =
                unconstrained.clamp(MINIMUM_ZOOM_MAGNIFICATION, MAXIMUM_ZOOM_MAGNIFICATION);
            decision.at_limit = (bounded - unconstrained).abs() > f32::EPSILON;
            if (bounded - current_zoom).abs() < AUTO_ZOOM_MINIMUM_STEP {
                return decision;
            }
            self.auto_zoom_destination = Some(bounded);
        }

        let destination = self.auto_zoom_destination.unwrap_or(current_zoom);
        let remaining = destination - current_zoom;
        if remaining.abs() <= AUTO_ZOOM_DESTINATION_TOLERANCE {
            self.auto_zoom_destination = None;
            self.auto_zoom_settle_until_ms =
                Some(captured_at_ms.saturating_add(AUTO_ZOOM_SETTLE_MS));
            return decision;
        }
        if self.last_auto_zoom_command_at_ms.is_some_and(|previous| {
            captured_at_ms.saturating_sub(previous) < AUTO_ZOOM_MINIMUM_INTERVAL_MS
        }) {
            return decision;
        }
        let step = (remaining.abs() * AUTO_ZOOM_RAMP_GAIN)
            .clamp(AUTO_ZOOM_MINIMUM_STEP, AUTO_ZOOM_MAXIMUM_STEP)
            .min(remaining.abs());
        let delta = remaining.signum() * step;
        let requested = ((current_zoom + delta)
            .clamp(MINIMUM_ZOOM_MAGNIFICATION, MAXIMUM_ZOOM_MAGNIFICATION)
            * 100.0)
            .round()
            / 100.0;
        if (requested - current_zoom).abs() < AUTO_ZOOM_DESTINATION_TOLERANCE {
            // Rounding can make the last step smaller than the send threshold
            // while the unrounded destination remains outside its tolerance.
            // Finish this segment so fresh geometry can choose the next one.
            self.auto_zoom_destination = None;
            self.auto_zoom_settle_until_ms =
                Some(captured_at_ms.saturating_add(AUTO_ZOOM_SETTLE_MS));
            return decision;
        }
        self.last_auto_zoom_command_at_ms = Some(captured_at_ms);
        decision.requested_magnification = Some(requested);
        decision
    }
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

fn ramp_speed(current: f64, requested: f64, acceleration: f64, zoom: f64) -> f64 {
    if requested > current {
        requested.min(current + acceleration)
    } else {
        // Brake faster after a catch-up movement to avoid carrying its speed
        // into the center of the image.
        let braking_fraction = 1.0 - 0.75 / zoom;
        requested.max(current - DECELERATION_STEP.max(current * braking_fraction))
    }
}

fn shoulder_geometry(landmarks: &[Landmark]) -> Option<(f32, f32, f32)> {
    if landmarks.len() != POSE_LANDMARK_COUNT {
        return None;
    }
    let left = &landmarks[LEFT_SHOULDER_INDEX];
    let right = &landmarks[RIGHT_SHOULDER_INDEX];
    if !left.x.is_finite()
        || !left.y.is_finite()
        || !right.x.is_finite()
        || !right.y.is_finite()
        || left.visibility.unwrap_or(1.0) < MINIMUM_SHOULDER_VISIBILITY
        || right.visibility.unwrap_or(1.0) < MINIMUM_SHOULDER_VISIBILITY
    {
        return None;
    }
    let shoulder_width = (right.x - left.x).hypot(right.y - left.y);
    let x = (left.x + right.x) / 2.0;
    let y = (left.y + right.y) / 2.0;
    Some((x, y, shoulder_width))
}

fn face_geometry(landmarks: &[Landmark]) -> Option<(f32, f32, Option<f32>)> {
    if landmarks.len() != FACE_LANDMARK_COUNT {
        return None;
    }
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
    let size = maximum_x - minimum_x;
    Some((
        (minimum_x + maximum_x) / 2.0,
        (minimum_y + maximum_y) / 2.0,
        (size.is_finite() && size >= MINIMUM_FACE_SIZE).then_some(size),
    ))
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

    fn face_at(x: f32, y: f32) -> Vec<Landmark> {
        vec![
            Landmark {
                x,
                y,
                z: 0.0,
                visibility: None,
            };
            FACE_LANDMARK_COUNT
        ]
    }

    fn face_with_size(x: f32, y: f32, size: f32) -> Vec<Landmark> {
        let mut face = face_at(x, y);
        face[0].x = x - size / 2.0;
        face[1].x = x + size / 2.0;
        face
    }

    #[test]
    fn centers_a_face_without_requesting_motion() {
        let mut controller = FaceTrackingController::default();
        let target = controller.face_target(&face_at(0.5, 0.5), &[]).unwrap();
        assert_eq!(
            (target.motion.pan_direction, target.motion.tilt_direction),
            (0, 0)
        );
        assert_eq!(target.motion.speed_fraction, 0.0);
    }

    #[test]
    fn measures_face_size_from_the_horizontal_mesh_span() {
        let mut controller = FaceTrackingController::default();
        let target = controller
            .face_target(&face_with_size(0.5, 0.5, 0.2), &[])
            .unwrap();

        assert!((target.size.unwrap() - 0.2).abs() < 1e-6);
    }

    #[test]
    fn follows_horizontal_and_vertical_image_error() {
        let mut controller = FaceTrackingController::default();
        let target = controller.face_target(&face_at(0.75, 0.25), &[]).unwrap();
        assert_eq!(
            (target.motion.pan_direction, target.motion.tilt_direction),
            (1, 1)
        );
    }

    #[test]
    fn hysteresis_keeps_motion_until_the_face_reaches_the_inner_dead_zone() {
        let mut controller = FaceTrackingController::default();
        controller.record_motion(FaceTrackingMotion {
            pan_direction: 1,
            tilt_direction: 0,
            speed_fraction: 0.05,
        });
        assert_eq!(
            controller
                .face_target(&face_at(0.57, 0.5), &[])
                .unwrap()
                .motion
                .pan_direction,
            1
        );
        assert_eq!(
            controller
                .face_target(&face_at(0.53, 0.5), &[])
                .unwrap()
                .motion
                .pan_direction,
            0
        );
    }

    #[test]
    fn refuses_incomplete_or_non_finite_face_meshes() {
        let mut controller = FaceTrackingController::default();
        assert!(controller.face_target(&[], &[]).is_none());
        assert!(
            controller
                .face_target(&face_at(f32::NAN, 0.5), &[])
                .is_none()
        );
    }

    #[test]
    fn estimates_a_face_target_from_visible_shoulders() {
        let mut pose = vec![
            Landmark {
                x: 0.5,
                y: 0.5,
                z: 0.0,
                visibility: Some(0.9),
            };
            POSE_LANDMARK_COUNT
        ];
        pose[LEFT_SHOULDER_INDEX].x = 0.6;
        pose[LEFT_SHOULDER_INDEX].y = 0.4;
        pose[RIGHT_SHOULDER_INDEX].x = 0.8;
        pose[RIGHT_SHOULDER_INDEX].y = 0.4;

        let target = FaceTrackingController::default()
            .shoulder_target(&pose)
            .unwrap();
        assert!((target.x - 0.7).abs() < f32::EPSILON);
        assert!((target.y - 0.3).abs() < f32::EPSILON);
        assert_eq!(
            (target.motion.pan_direction, target.motion.tilt_direction),
            (1, 1)
        );

        pose[RIGHT_SHOULDER_INDEX].visibility = Some(0.49);
        assert!(
            FaceTrackingController::default()
                .shoulder_target(&pose)
                .is_none()
        );
    }

    #[test]
    fn calibrated_shoulders_preserve_head_height_when_the_face_disappears() {
        let mut pose = vec![
            Landmark {
                x: 0.5,
                y: 0.5,
                z: 0.0,
                visibility: Some(0.99),
            };
            POSE_LANDMARK_COUNT
        ];
        pose[LEFT_SHOULDER_INDEX].x = 0.4;
        pose[LEFT_SHOULDER_INDEX].y = 0.7;
        pose[RIGHT_SHOULDER_INDEX].x = 0.6;
        pose[RIGHT_SHOULDER_INDEX].y = 0.7;
        let mut controller = FaceTrackingController::default();

        controller.face_target(&face_at(0.5, 0.5), &pose).unwrap();
        let shoulder_target = controller.shoulder_target(&pose).unwrap();

        assert!((shoulder_target.y - 0.5).abs() < f32::EPSILON);
        assert_eq!(shoulder_target.motion.tilt_direction, 0);
    }

    #[test]
    fn crossing_the_dead_zone_starts_with_a_low_ramped_speed() {
        let mut controller = FaceTrackingController::default();
        let first = controller.face_target(&face_at(0.585, 0.5), &[]).unwrap();
        assert_eq!(first.motion.pan_direction, 1);
        assert!((first.motion.speed_fraction - ACCELERATION_STEP).abs() < f64::EPSILON);

        controller.record_motion(first.motion);
        let second = controller.face_target(&face_at(0.9, 0.5), &[]).unwrap();
        assert!(second.motion.speed_fraction > first.motion.speed_fraction);
        assert!(
            second.motion.speed_fraction <= first.motion.speed_fraction + MAXIMUM_ACCELERATION_STEP
        );
        assert!(second.motion.speed_fraction <= MAXIMUM_SPEED_FRACTION);
    }

    #[test]
    fn larger_offsets_accelerate_faster_and_reach_catch_up_speed() {
        let mut previous_start = 0.0;
        for x in [0.59, 0.7, 0.8, 0.9, 1.0] {
            let mut controller = FaceTrackingController::default();
            let first = controller.face_target(&face_at(x, 0.5), &[]).unwrap();
            assert!(first.motion.speed_fraction > previous_start);
            assert!(first.motion.speed_fraction <= MAXIMUM_ACCELERATION_STEP);
            previous_start = first.motion.speed_fraction;
            controller.record_motion(first.motion);
            for _ in 0..20 {
                let target = controller.face_target(&face_at(x, 0.5), &[]).unwrap();
                assert!(target.motion.speed_fraction <= MAXIMUM_SPEED_FRACTION);
                controller.record_motion(target.motion);
            }
            if x >= 0.8 {
                assert!(controller.motion().speed_fraction > BASE_MAXIMUM_SPEED_FRACTION);
            }
        }
    }

    #[test]
    fn catch_up_builds_at_medium_offsets_and_flattens_near_the_edge() {
        let starts: Vec<f64> = [0.7, 0.8, 0.9, 1.0]
            .into_iter()
            .map(|x| {
                FaceTrackingController::default()
                    .face_target(&face_at(x, 0.5), &[])
                    .unwrap()
                    .motion
                    .speed_fraction
            })
            .collect();
        assert!(starts[0] > 0.025);
        assert!(starts[1] > 0.06);
        let middle_increase = starts[1] - starts[0];
        let edge_increase = starts[3] - starts[2];
        assert!(edge_increase < middle_increase * 0.5);
    }

    #[test]
    fn zoom_reduces_face_and_shoulder_speed_and_acceleration() {
        let mut baseline = FaceTrackingController::default();
        baseline.set_tracking_zoom(Some(1.0));
        let initial = baseline.target_at(0.9, 0.5).motion.speed_fraction;
        for zoom in [2.0_f32, 3.0, 4.0] {
            let mut controller = FaceTrackingController::default();
            controller.set_tracking_zoom(Some(zoom));
            let mut pose = vec![
                Landmark {
                    x: 0.9,
                    y: 0.6,
                    z: 0.0,
                    visibility: Some(0.9)
                };
                33
            ];
            pose[LEFT_SHOULDER_INDEX].x = 0.8;
            pose[RIGHT_SHOULDER_INDEX].x = 1.0;
            let face = controller.face_target(&face_at(0.9, 0.5), &pose).unwrap();
            let shoulder = controller.shoulder_target(&pose).unwrap();
            let expected = initial / f64::from(zoom * zoom);
            assert!((face.motion.speed_fraction - expected).abs() < 1e-8);
            assert!((shoulder.motion.speed_fraction - expected).abs() < 1e-8);
            for _ in 0..20 {
                let motion = controller.target_at(0.9, 0.5).motion;
                assert!(motion.speed_fraction <= MAXIMUM_SPEED_FRACTION / f64::from(zoom * zoom));
                controller.record_motion(motion);
            }
        }
    }

    #[test]
    fn zoom_in_clamps_existing_speed_and_brakes_before_centering() {
        let mut controller = FaceTrackingController::default();
        controller.record_motion(FaceTrackingMotion {
            pan_direction: 1,
            tilt_direction: 0,
            speed_fraction: 0.35,
        });
        controller.set_tracking_zoom(Some(3.0));
        let fast = controller.target_at(0.95, 0.5).motion;
        assert!(fast.speed_fraction <= MAXIMUM_SPEED_FRACTION / 9.0);
        controller.record_motion(fast);
        let braking = controller.target_at(0.57, 0.5).motion;
        assert!(braking.speed_fraction <= fast.speed_fraction * 0.25 + 1e-8);
        controller.record_motion(braking);
        assert_eq!(
            controller.target_at(0.5, 0.5).motion,
            FaceTrackingMotion::default()
        );
    }

    #[test]
    fn unknown_or_invalid_zoom_uses_conservative_tracking_gains() {
        for zoom in [None, Some(f32::NAN), Some(0.0), Some(5.0)] {
            let mut controller = FaceTrackingController::default();
            controller.set_tracking_zoom(zoom);
            assert!(
                controller.target_at(1.0, 0.5).motion.speed_fraction
                    <= MAXIMUM_ACCELERATION_STEP / 16.0
            );
        }
    }

    #[test]
    fn catch_up_motion_brakes_and_stops_at_the_center() {
        let mut controller = FaceTrackingController::default();
        controller.record_motion(FaceTrackingMotion {
            pan_direction: 1,
            tilt_direction: 0,
            speed_fraction: MAXIMUM_SPEED_FRACTION,
        });
        for _ in 0..8 {
            let previous = controller.motion().speed_fraction;
            let target = controller.face_target(&face_at(0.57, 0.5), &[]).unwrap();
            assert!(target.motion.speed_fraction < previous);
            controller.record_motion(target.motion);
        }
        assert!(controller.motion().speed_fraction < 0.04);
        let centered = controller.face_target(&face_at(0.5, 0.5), &[]).unwrap();
        assert_eq!(centered.motion, FaceTrackingMotion::default());
    }

    #[test]
    fn secondary_axis_transitions_preserve_the_ongoing_speed_ramp() {
        for (x, y, previous, expected) in [
            (0.9, 0.3, (1, 0), (1, 1)),
            (0.9, 0.5, (1, 1), (1, 0)),
            (0.7, 0.1, (0, 1), (1, 1)),
            (0.5, 0.1, (1, 1), (0, 1)),
        ] {
            let mut controller = FaceTrackingController::default();
            controller.record_motion(FaceTrackingMotion {
                pan_direction: previous.0,
                tilt_direction: previous.1,
                speed_fraction: 0.06,
            });
            let target = controller.face_target(&face_at(x, y), &[]).unwrap();
            assert_eq!(
                (target.motion.pan_direction, target.motion.tilt_direction),
                expected
            );
            assert!(target.motion.speed_fraction > 0.06);
            assert!(target.motion.speed_fraction <= 0.06 + MAXIMUM_ACCELERATION_STEP);
        }
    }

    #[test]
    fn reversing_an_axis_still_restarts_the_speed_ramp() {
        let mut controller = FaceTrackingController::default();
        controller.record_motion(FaceTrackingMotion {
            pan_direction: 1,
            tilt_direction: 1,
            speed_fraction: 0.08,
        });
        let target = controller.face_target(&face_at(0.9, 0.9), &[]).unwrap();
        assert_eq!(target.motion.pan_direction, 1);
        assert_eq!(target.motion.tilt_direction, -1);
        assert!((target.motion.speed_fraction - ACCELERATION_STEP).abs() < f64::EPSILON);
    }

    #[test]
    fn narrowed_tilt_dead_zone_reacts_to_a_small_vertical_offset() {
        let mut controller = FaceTrackingController::default();
        let target = controller.face_target(&face_at(0.5, 0.595), &[]).unwrap();
        assert_eq!(target.motion.tilt_direction, -1);
        assert!(target.motion.speed_fraction > 0.0);
        assert!(target.motion.speed_fraction <= ACCELERATION_STEP);
    }

    #[test]
    fn approaching_the_center_decelerates_before_stopping() {
        let mut controller = FaceTrackingController::default();
        controller.record_motion(FaceTrackingMotion {
            pan_direction: 1,
            tilt_direction: 0,
            speed_fraction: 0.08,
        });
        let target = controller.face_target(&face_at(0.57, 0.5), &[]).unwrap();
        assert_eq!(target.motion.pan_direction, 1);
        assert!((target.motion.speed_fraction - 0.06).abs() < f64::EPSILON);
    }

    #[test]
    fn auto_zoom_calibrates_from_the_first_fresh_face() {
        let mut controller = FaceTrackingController::default();
        controller.set_enabled(true);
        controller.set_auto_zoom_enabled(true, 1_000);

        let stale = controller.auto_zoom(Some(0.2), Some(2.0), 999);
        assert!(!stale.calibrated);
        assert_eq!(stale.requested_magnification, None);

        let fresh = controller.auto_zoom(Some(0.2), Some(2.0), 1_000);
        assert!(fresh.calibrated);
        assert_eq!(fresh.target_face_size, Some(0.2));
        assert_eq!(fresh.requested_magnification, None);
    }

    #[test]
    fn auto_zoom_corrects_size_changes_gradually_and_rate_limits_commands() {
        let mut controller = FaceTrackingController::default();
        controller.set_enabled(true);
        controller.set_auto_zoom_enabled(true, 0);
        controller.auto_zoom(Some(0.2), Some(2.0), 1_000);

        let first = controller.auto_zoom(Some(0.1), Some(2.0), 1_100);
        assert_eq!(first.requested_magnification, Some(2.16));

        let rate_limited = controller.auto_zoom(Some(0.1), Some(2.16), 1_150);
        assert_eq!(rate_limited.requested_magnification, None);

        let next = controller.auto_zoom(Some(0.1), Some(2.16), 1_200);
        assert_eq!(next.requested_magnification, Some(2.31));
    }

    #[test]
    fn auto_zoom_ignores_small_face_size_changes() {
        let mut controller = FaceTrackingController::default();
        controller.set_enabled(true);
        controller.set_auto_zoom_enabled(true, 0);
        controller.auto_zoom(Some(0.2), Some(2.0), 1_000);

        let decision = controller.auto_zoom(Some(0.19), Some(2.0), 2_000);

        assert_eq!(decision.requested_magnification, None);
        assert!(!decision.at_limit);
    }

    #[test]
    fn auto_zoom_uses_symmetric_reference_size_thresholds() {
        for size in [0.1884, 0.2116] {
            let mut controller = FaceTrackingController::default();
            controller.set_auto_zoom_enabled(true, 0);
            controller.auto_zoom(Some(0.2), Some(2.0), 1_000);
            controller.smoothed_face_size = Some(size);
            assert_eq!(
                controller
                    .auto_zoom(Some(size), Some(2.0), 1_100)
                    .requested_magnification,
                None
            );
        }
        for size in [0.186, 0.214] {
            let mut controller = FaceTrackingController::default();
            controller.set_auto_zoom_enabled(true, 0);
            controller.auto_zoom(Some(0.2), Some(2.0), 1_000);
            controller.smoothed_face_size = Some(size);
            let requested = controller
                .auto_zoom(Some(size), Some(2.0), 1_100)
                .requested_magnification
                .unwrap();
            assert_eq!(requested > 2.0, size < 0.2);
        }
    }

    #[test]
    fn auto_zoom_cancels_old_destinations_when_reference_size_is_recovered() {
        for initial_size in [0.1, 0.35] {
            let mut controller = FaceTrackingController::default();
            controller.set_auto_zoom_enabled(true, 0);
            controller.auto_zoom(Some(0.2), Some(2.0), 1_000);
            let zoom = controller
                .auto_zoom(Some(initial_size), Some(2.0), 1_100)
                .requested_magnification
                .unwrap();
            assert!(controller.auto_zoom_destination.is_some());
            let recovered = controller.auto_zoom(Some(0.2), Some(zoom), 1_200);
            assert_eq!(recovered.requested_magnification, None);
            assert_eq!(controller.auto_zoom_destination, None);
            assert_eq!(controller.smoothed_face_size, Some(0.2));
        }
    }

    #[test]
    fn auto_zoom_reverses_on_measured_overshoot_without_waiting_for_smoothing() {
        let mut controller = FaceTrackingController::default();
        controller.set_auto_zoom_enabled(true, 0);
        controller.auto_zoom(Some(0.2), Some(2.0), 1_000);
        let zoom = controller
            .auto_zoom(Some(0.1), Some(2.0), 1_100)
            .requested_magnification
            .unwrap();
        let returned = controller.auto_zoom(Some(0.22), Some(zoom), 1_200);
        assert!(returned.requested_magnification.unwrap() < zoom);
        assert_eq!(returned.target_face_size, Some(0.2));
    }

    #[test]
    fn auto_zoom_ignores_pre_command_frames_and_reacts_to_a_fast_return() {
        let mut controller = FaceTrackingController::default();
        controller.set_auto_zoom_enabled(true, 0);
        controller.auto_zoom(Some(0.2), Some(2.0), 1_000);
        let zoom = controller
            .auto_zoom(Some(0.1), Some(2.0), 1_100)
            .requested_magnification
            .unwrap();
        controller.record_auto_zoom_applied(1_250);
        for captured in [1_200, 1_250] {
            let stale = controller.auto_zoom(Some(0.1), Some(zoom), captured);
            assert_eq!(stale.requested_magnification, None);
            assert_eq!(controller.smoothed_face_size, None);
        }
        let returned = controller.auto_zoom(Some(0.22), Some(zoom), 1_300);
        assert!(returned.requested_magnification.unwrap() < zoom);
        assert_eq!(returned.target_face_size, Some(0.2));
    }

    #[test]
    fn auto_zoom_reassesses_after_a_rounded_final_step_cannot_be_sent() {
        let mut controller = FaceTrackingController::default();
        controller.set_auto_zoom_enabled(true, 0);
        controller.auto_zoom(Some(0.2), Some(2.0), 1_000);
        controller.auto_zoom_destination = Some(2.580645);
        controller.smoothed_face_size = Some(0.1792);
        let last_step = controller.auto_zoom(Some(0.1792), Some(2.56), 1_100);
        assert_eq!(last_step.requested_magnification, None);
        assert_eq!(controller.auto_zoom_destination, None);
        let reassessed = controller.auto_zoom(Some(0.1792), Some(2.56), 1_400);
        assert!(reassessed.requested_magnification.unwrap() > 2.58);
    }

    #[test]
    fn auto_zoom_preserves_reference_size_across_simulated_distance_round_trips() {
        for transition_frames in [1, 5, 20] {
            let mut controller = FaceTrackingController::default();
            controller.set_auto_zoom_enabled(true, 0);
            let mut zoom = 2.0;
            let mut timestamp = 1_000;
            let mut previous_scale = 0.1;
            controller.auto_zoom(Some(0.2), Some(zoom), timestamp);
            // Ideal linear digital zoom with different subject movement speeds.
            // This checks the controller's ratio, not physical camera latency.
            for target_scale in [0.07, 0.1, 0.07, 0.1] {
                for frame in 1..=transition_frames + 80 {
                    let progress = (frame as f32 / transition_frames as f32).min(1.0);
                    let scale = previous_scale + (target_scale - previous_scale) * progress;
                    timestamp += 100;
                    let decision = controller.auto_zoom(Some(scale * zoom), Some(zoom), timestamp);
                    if let Some(requested) = decision.requested_magnification {
                        zoom = requested;
                        controller.record_auto_zoom_applied(timestamp + 50);
                    }
                    assert_eq!(decision.target_face_size, Some(0.2));
                }
                assert!(
                    (target_scale * zoom / 0.2 - 1.0).abs() <= 0.061,
                    "frames={transition_frames}, scale={target_scale}, zoom={zoom}, destination={:?}",
                    controller.auto_zoom_destination
                );
                previous_scale = target_scale;
            }
        }
    }

    #[test]
    fn auto_zoom_keeps_one_destination_while_the_camera_image_catches_up() {
        let mut controller = FaceTrackingController::default();
        controller.set_enabled(true);
        controller.set_auto_zoom_enabled(true, 0);
        controller.auto_zoom(Some(0.2), Some(2.0), 1_000);
        let first = controller.auto_zoom(Some(0.1), Some(2.0), 1_100);
        let destination = controller.auto_zoom_destination.unwrap();
        assert_eq!(first.requested_magnification, Some(2.16));

        let continuing = controller.auto_zoom(Some(0.1), Some(2.16), 1_200);
        assert_eq!(continuing.requested_magnification, Some(2.31));
        assert_eq!(controller.auto_zoom_destination, Some(destination));
    }

    #[test]
    fn auto_zoom_reverses_when_the_observed_size_crosses_the_target() {
        let mut controller = FaceTrackingController::default();
        controller.set_enabled(true);
        controller.set_auto_zoom_enabled(true, 0);
        controller.auto_zoom(Some(0.2), Some(2.0), 1_000);

        let zooming_in = controller.auto_zoom(Some(0.1), Some(2.0), 1_100);
        assert_eq!(zooming_in.requested_magnification, Some(2.16));
        assert!(controller.auto_zoom_destination.unwrap() > 2.16);

        let reversing = controller.auto_zoom(Some(0.4), Some(2.16), 1_200);
        assert!(reversing.requested_magnification.unwrap() < 2.16);
        assert!(controller.auto_zoom_destination.unwrap() < 2.16);
    }

    #[test]
    fn losing_the_face_mesh_cancels_zoom_until_a_valid_face_returns() {
        let mut controller = FaceTrackingController::default();
        controller.set_enabled(true);
        controller.set_auto_zoom_enabled(true, 0);
        controller.auto_zoom(Some(0.2), Some(2.0), 1_000);

        let zooming_in = controller.auto_zoom(Some(0.1), Some(2.0), 1_100);
        assert_eq!(zooming_in.requested_magnification, Some(2.16));
        assert!(controller.auto_zoom_destination.is_some());

        let face_lost = controller.auto_zoom(None, Some(2.16), 1_200);
        assert_eq!(face_lost.requested_magnification, None);
        assert_eq!(controller.auto_zoom_destination, None);
        assert_eq!(controller.smoothed_face_size, None);
        assert_eq!(controller.auto_zoom_settle_until_ms, None);
        assert_eq!(controller.last_auto_zoom_command_at_ms, None);

        let still_missing = controller.auto_zoom(None, Some(2.16), 1_300);
        assert_eq!(still_missing.requested_magnification, None);

        let face_returned = controller.auto_zoom(Some(0.2), Some(2.16), 1_400);
        assert_eq!(face_returned.requested_magnification, None);
        assert_eq!(controller.smoothed_face_size, Some(0.2));
    }

    #[test]
    fn auto_zoom_step_scales_with_the_observed_size_change() {
        let mut moderate = FaceTrackingController::default();
        moderate.set_enabled(true);
        moderate.set_auto_zoom_enabled(true, 0);
        moderate.auto_zoom(Some(0.2), Some(2.0), 1_000);
        moderate.smoothed_face_size = Some(0.2);
        let moderate_step = moderate
            .auto_zoom(Some(0.16), Some(2.0), 1_100)
            .requested_magnification
            .unwrap()
            - 2.0;

        let mut large = FaceTrackingController::default();
        large.set_enabled(true);
        large.set_auto_zoom_enabled(true, 0);
        large.auto_zoom(Some(0.2), Some(2.0), 1_000);
        large.smoothed_face_size = Some(0.2);
        let large_step = large
            .auto_zoom(Some(0.1), Some(2.0), 1_100)
            .requested_magnification
            .unwrap()
            - 2.0;

        assert!(moderate_step >= AUTO_ZOOM_MINIMUM_STEP);
        assert!(large_step > moderate_step);
        assert!(large_step <= AUTO_ZOOM_MAXIMUM_STEP + f32::EPSILON);
    }

    #[test]
    fn manual_zoom_change_recalibrates_after_the_lens_settles() {
        let mut controller = FaceTrackingController::default();
        controller.set_enabled(true);
        controller.set_auto_zoom_enabled(true, 0);
        controller.auto_zoom(Some(0.2), Some(2.0), 1_000);
        controller.recalibrate_auto_zoom_after(2_000);

        let settling = controller.auto_zoom(Some(0.3), Some(3.0), 1_999);
        assert!(!settling.calibrated);
        let recalibrated = controller.auto_zoom(Some(0.3), Some(3.0), 2_000);
        assert!(recalibrated.calibrated);
        assert_eq!(recalibrated.target_face_size, Some(0.3));
        assert_eq!(recalibrated.requested_magnification, None);
    }
}
