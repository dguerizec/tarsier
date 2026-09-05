use crate::model::Landmark;

const FACE_LANDMARK_COUNT: usize = 478;
const POSE_LANDMARK_COUNT: usize = 33;
const LEFT_SHOULDER_INDEX: usize = 11;
const RIGHT_SHOULDER_INDEX: usize = 12;
const MINIMUM_SHOULDER_VISIBILITY: f32 = 0.5;
const ESTIMATED_FACE_OFFSET_IN_SHOULDER_WIDTHS: f32 = 0.5;
const TARGET_X: f32 = 0.5;
const TARGET_Y: f32 = 0.5;
const PAN_START_THRESHOLD: f32 = 0.10;
const PAN_STOP_THRESHOLD: f32 = 0.05;
const TILT_START_THRESHOLD: f32 = 0.12;
const TILT_STOP_THRESHOLD: f32 = 0.06;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FaceTarget {
    pub x: f32,
    pub y: f32,
    pub pan_direction: i8,
    pub tilt_direction: i8,
}

#[derive(Default)]
pub struct FaceTrackingController {
    enabled: bool,
    motion: (i8, i8),
}

impl FaceTrackingController {
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        self.motion = (0, 0);
    }

    pub fn motion(&self) -> (i8, i8) {
        self.motion
    }

    pub fn record_motion(&mut self, motion: (i8, i8)) {
        self.motion = motion;
    }

    pub fn face_target(&self, landmarks: &[Landmark]) -> Option<FaceTarget> {
        let (x, y) = face_center(landmarks)?;
        Some(self.target_at(x, y))
    }

    pub fn shoulder_target(&self, landmarks: &[Landmark]) -> Option<FaceTarget> {
        let (x, y) = shoulder_face_estimate(landmarks)?;
        Some(self.target_at(x, y))
    }

    fn target_at(&self, x: f32, y: f32) -> FaceTarget {
        let pan_direction = axis_direction(
            x - TARGET_X,
            self.motion.0,
            PAN_START_THRESHOLD,
            PAN_STOP_THRESHOLD,
        );
        let image_tilt_direction = axis_direction(
            y - TARGET_Y,
            -self.motion.1,
            TILT_START_THRESHOLD,
            TILT_STOP_THRESHOLD,
        );
        FaceTarget {
            x,
            y,
            pan_direction,
            tilt_direction: -image_tilt_direction,
        }
    }
}

fn shoulder_face_estimate(landmarks: &[Landmark]) -> Option<(f32, f32)> {
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
    let y = ((left.y + right.y) / 2.0 - shoulder_width * ESTIMATED_FACE_OFFSET_IN_SHOULDER_WIDTHS)
        .clamp(0.0, 1.0);
    Some((x, y))
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
        let controller = FaceTrackingController::default();
        let target = controller.face_target(&face_at(0.5, 0.5)).unwrap();
        assert_eq!((target.pan_direction, target.tilt_direction), (0, 0));
    }

    #[test]
    fn follows_horizontal_and_vertical_image_error() {
        let controller = FaceTrackingController::default();
        let target = controller.face_target(&face_at(0.75, 0.25)).unwrap();
        assert_eq!((target.pan_direction, target.tilt_direction), (1, 1));
    }

    #[test]
    fn hysteresis_keeps_motion_until_the_face_reaches_the_inner_dead_zone() {
        let mut controller = FaceTrackingController::default();
        controller.record_motion((1, 0));
        assert_eq!(
            controller
                .face_target(&face_at(0.57, 0.5))
                .unwrap()
                .pan_direction,
            1
        );
        assert_eq!(
            controller
                .face_target(&face_at(0.53, 0.5))
                .unwrap()
                .pan_direction,
            0
        );
    }

    #[test]
    fn refuses_incomplete_or_non_finite_face_meshes() {
        let controller = FaceTrackingController::default();
        assert!(controller.face_target(&[]).is_none());
        assert!(controller.face_target(&face_at(f32::NAN, 0.5)).is_none());
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
        assert_eq!((target.pan_direction, target.tilt_direction), (1, 1));

        pose[RIGHT_SHOULDER_INDEX].visibility = Some(0.49);
        assert!(
            FaceTrackingController::default()
                .shoulder_target(&pose)
                .is_none()
        );
    }
}
