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
const MAXIMUM_SPEED_FRACTION: f64 = 0.10;
const ACCELERATION_STEP: f64 = 0.018;
const DECELERATION_STEP: f64 = 0.02;
const MAXIMUM_IMAGE_ERROR: f32 = 0.5;

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
    pub motion: FaceTrackingMotion,
}

#[derive(Default)]
pub struct FaceTrackingController {
    enabled: bool,
    motion: FaceTrackingMotion,
    shoulder_face_y_offset: Option<f32>,
}

impl FaceTrackingController {
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        self.motion = FaceTrackingMotion::default();
        self.shoulder_face_y_offset = None;
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
        let (x, y) = face_center(face_landmarks)?;
        if let Some((_, shoulder_y, _)) = shoulder_geometry(pose_landmarks) {
            let observed_offset = y - shoulder_y;
            self.shoulder_face_y_offset = Some(
                self.shoulder_face_y_offset
                    .map_or(observed_offset, |previous| {
                        previous + SHOULDER_CALIBRATION_ALPHA * (observed_offset - previous)
                    }),
            );
        }
        Some(self.target_at(x, y))
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
        FaceTarget {
            x,
            y,
            motion: FaceTrackingMotion {
                pan_direction,
                tilt_direction,
                speed_fraction,
            },
        }
    }
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

fn face_center(landmarks: &[Landmark]) -> Option<(f32, f32)> {
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
    Some(((minimum_x + maximum_x) / 2.0, (minimum_y + maximum_y) / 2.0))
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
        assert!(second.motion.speed_fraction <= first.motion.speed_fraction + ACCELERATION_STEP);
        assert!(second.motion.speed_fraction <= MAXIMUM_SPEED_FRACTION);
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
}
