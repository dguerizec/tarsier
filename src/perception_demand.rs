//! Detection demand is the union of internal consumers and live socket leases.
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Models {
    pub face: bool,
    pub hands: bool,
    pub pose: bool,
}
impl Models {
    pub fn all() -> Self {
        Self {
            face: true,
            hands: true,
            pose: true,
        }
    }
    pub fn union(self, other: Self) -> Self {
        Self {
            face: self.face || other.face,
            hands: self.hands || other.hands,
            pose: self.pose || other.pose,
        }
    }
    pub fn event(event: &str) -> Option<Self> {
        match event {
            "gesture.phone_near_mouth.started" | "gesture.phone_near_mouth.ended" => Some(Self {
                face: true,
                hands: true,
                pose: false,
            }),
            "gesture.open_palm.held" => Some(Self {
                hands: true,
                ..Self::default()
            }),
            "face.present.started" | "face.present.ended" => Some(Self {
                face: true,
                ..Self::default()
            }),
            _ => None,
        }
    }
}
#[derive(Clone, Default)]
pub struct Registry(Arc<Mutex<HashMap<u128, Models>>>);
impl Registry {
    pub fn lease(&self) -> Lease {
        Lease {
            registry: self.clone(),
            id: rand::random(),
        }
    }
    pub fn models(&self) -> Models {
        self.0
            .lock()
            .unwrap()
            .values()
            .copied()
            .fold(Models::default(), Models::union)
    }
}
pub struct Lease {
    registry: Registry,
    id: u128,
}
impl Lease {
    pub fn replace(&self, models: Models) {
        self.registry.0.lock().unwrap().insert(self.id, models);
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        self.registry.0.lock().unwrap().remove(&self.id);
    }
}

pub fn internal(config: &crate::config::Config, state: &crate::model::RuntimeState) -> Models {
    let mut models = Models {
        face: state.camera.face_tracking.enabled
            || state.camera.face_tracking.auto_zoom.enabled
            || config.perception.phone_near_mouth.enabled,
        hands: state.camera.hands_tracking.enabled || config.perception.phone_near_mouth.enabled,
        // Local tracking needs shoulders for face fallback and arms for hand recovery.
        pose: state.camera.face_tracking.enabled
            || state.camera.hands_tracking.enabled
            || (state.video_effects.output_mode == crate::model::VideoOutputMode::Camera
                && state.video_effects.background_enabled),
    };
    for scenario in config.scenarios.iter().filter(|s| s.enabled) {
        if let Some(required) = Models::event(&scenario.event) {
            models = models.union(required);
        }
    }
    models
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn independent_consumers_replace_and_release_their_own_demands() {
        let registry = Registry::default();
        let a = registry.lease();
        let b = registry.lease();
        a.replace(Models {
            face: true,
            ..Models::default()
        });
        b.replace(Models {
            hands: true,
            ..Models::default()
        });
        assert_eq!(
            registry.models(),
            Models {
                face: true,
                hands: true,
                pose: false
            }
        );
        a.replace(Models {
            pose: true,
            ..Models::default()
        });
        drop(b);
        assert_eq!(
            registry.models(),
            Models {
                pose: true,
                ..Models::default()
            }
        );
        drop(a);
        assert_eq!(registry.models(), Models::default());
    }
    #[test]
    fn face_tracking_keeps_its_shoulder_fallback_available_without_background_effects() {
        let mut config = crate::config::Config::default();
        config.perception.phone_near_mouth.enabled = false;
        config.scenarios.clear();
        let mut state = crate::model::RuntimeState::default();
        state.camera.face_tracking.enabled = true;
        assert_eq!(
            internal(&config, &state),
            Models {
                face: true,
                hands: false,
                pose: true
            }
        );
        state.camera.face_tracking.enabled = false;
        assert_eq!(internal(&config, &state), Models::default());
    }

    #[test]
    fn hands_tracking_requests_pose_for_arm_recovery() {
        let mut config = crate::config::Config::default();
        config.perception.phone_near_mouth.enabled = false;
        config.scenarios.clear();
        let mut state = crate::model::RuntimeState::default();
        state.camera.hands_tracking.enabled = true;
        assert_eq!(
            internal(&config, &state),
            Models {
                face: false,
                hands: true,
                pose: true
            }
        );
        state.camera.hands_tracking.enabled = false;
        assert_eq!(internal(&config, &state), Models::default());
    }

    #[test]
    fn internal_dependencies_preserve_gestures_and_background_pose() {
        let mut config = crate::config::Config::default();
        let mut state = crate::model::RuntimeState::default();
        assert!(internal(&config, &state).face && internal(&config, &state).hands);
        config.perception.phone_near_mouth.enabled = false;
        config.scenarios.clear();
        assert_eq!(internal(&config, &state), Models::default());
        state.video_effects.background_enabled = true;
        assert_eq!(
            internal(&config, &state),
            Models {
                pose: true,
                ..Models::default()
            }
        );
        assert!(Models::event("unknown").is_none());
    }
}
