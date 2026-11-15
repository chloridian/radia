#[cfg(any(target_os = "windows", target_os = "linux"))]
pub(crate) mod input;

use math::Quat;

#[cfg(any(target_os = "windows", target_os = "linux"))]
pub(crate) use input::{BodyRotation, HeldControls, KeyBinding, STOP};

pub(crate) const BODY_ROTATION_DOT_THRESHOLD: f32 = 0.9997;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct MovementObservation {
    pub(crate) control_flags: u32,
    pub(crate) body_rotation: Quat,
    pub(crate) always_run: bool,
    pub(crate) stop_sequence: u64,
}

impl Default for MovementObservation {
    fn default() -> Self {
        Self { control_flags: 0, body_rotation: Quat::IDENTITY, always_run: false, stop_sequence: 0 }
    }
}
