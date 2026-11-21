use std::{
    f32::consts::FRAC_PI_2,
    time::{Duration, Instant},
};

use math::{Quat, Vec3};

use super::MovementObservation;

const AT_POS: u32 = 0x0000_0001;
const AT_NEG: u32 = 0x0000_0002;
const LEFT_POS: u32 = 0x0000_0004;
const LEFT_NEG: u32 = 0x0000_0008;
const UP_POS: u32 = 0x0000_0010;
const UP_NEG: u32 = 0x0000_0020;
const YAW_POS: u32 = 0x0000_0100;
const YAW_NEG: u32 = 0x0000_0200;
const FAST_AT: u32 = 0x0000_0400;
const FAST_LEFT: u32 = 0x0000_0800;
const FAST_UP: u32 = 0x0000_1000;
const FLY: u32 = 0x0000_2000;
pub(crate) const STOP: u32 = 0x0000_4000;
const TURN_RATE: f32 = FRAC_PI_2;
const MAX_TURN_STEP: Duration = Duration::from_millis(100);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MovementKey {
    Forward,
    ForwardArrow,
    Backward,
    BackwardArrow,
    TurnLeft,
    TurnLeftArrow,
    TurnRight,
    TurnRightArrow,
    Jump,
    JumpPage,
    Down,
    DownPage,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum KeyBinding {
    Movement(MovementKey),
    ToggleFlying,
    ToggleRun,
    Stop,
}

#[derive(Default)]
pub(crate) struct HeldControls {
    pressed: u16,
    flying: bool,
    always_run: bool,
    stop_pending: bool,
    stop_sequence: u64,
}

pub(crate) struct BodyRotation {
    seed: Option<u64>,
    current: Quat,
    updated_at: Option<Instant>,
}

impl Default for BodyRotation {
    fn default() -> Self {
        Self { seed: None, current: Quat::IDENTITY, updated_at: None }
    }
}

impl KeyBinding {
    pub(crate) fn from_key(key: &str) -> Option<Self> {
        if key.eq_ignore_ascii_case("w") {
            Some(Self::Movement(MovementKey::Forward))
        } else if key.eq_ignore_ascii_case("s") {
            Some(Self::Movement(MovementKey::Backward))
        } else if key.eq_ignore_ascii_case("a") {
            Some(Self::Movement(MovementKey::TurnLeft))
        } else if key.eq_ignore_ascii_case("d") {
            Some(Self::Movement(MovementKey::TurnRight))
        } else if key.eq_ignore_ascii_case("e") {
            Some(Self::Movement(MovementKey::Jump))
        } else if key.eq_ignore_ascii_case("c") {
            Some(Self::Movement(MovementKey::Down))
        } else if key.eq_ignore_ascii_case("f") {
            Some(Self::ToggleFlying)
        } else if key.eq_ignore_ascii_case("r") {
            Some(Self::ToggleRun)
        } else if key.eq_ignore_ascii_case("up") {
            Some(Self::Movement(MovementKey::ForwardArrow))
        } else if key.eq_ignore_ascii_case("down") {
            Some(Self::Movement(MovementKey::BackwardArrow))
        } else if key.eq_ignore_ascii_case("left") {
            Some(Self::Movement(MovementKey::TurnLeftArrow))
        } else if key.eq_ignore_ascii_case("right") {
            Some(Self::Movement(MovementKey::TurnRightArrow))
        } else if key.eq_ignore_ascii_case("pageup") {
            Some(Self::Movement(MovementKey::JumpPage))
        } else if key.eq_ignore_ascii_case("pagedown") {
            Some(Self::Movement(MovementKey::DownPage))
        } else if key.eq_ignore_ascii_case("home") {
            Some(Self::ToggleFlying)
        } else if key.eq_ignore_ascii_case("space") {
            Some(Self::Stop)
        } else {
            None
        }
    }
}

impl MovementKey {
    fn mask(self) -> u16 {
        1 << (self as u8)
    }
}

impl HeldControls {
    pub(crate) fn set_pressed(&mut self, key: MovementKey, pressed: bool) -> bool {
        let mask = key.mask();
        let previous = self.pressed;
        if pressed {
            self.pressed |= mask;
        } else {
            self.pressed &= !mask;
        }
        self.pressed != previous
    }

    pub(crate) fn keyboard_event(&mut self, key: &str, pressed: bool, repeat: bool, captured: bool) -> Option<(KeyBinding, bool)> {
        let binding = KeyBinding::from_key(key)?;
        if !pressed {
            let changed = match binding {
                KeyBinding::Movement(key) => self.set_pressed(key, false),
                _ => false,
            };
            return Some((binding, changed));
        }
        if repeat {
            return Some((binding, false));
        }
        if captured {
            return None;
        }
        let changed = match binding {
            KeyBinding::Movement(key) => self.set_pressed(key, true),
            KeyBinding::ToggleFlying => {
                self.toggle_flying();
                true
            }
            KeyBinding::ToggleRun => {
                self.toggle_run();
                true
            }
            KeyBinding::Stop => {
                self.stop();
                true
            }
        };
        Some((binding, changed))
    }

    pub(crate) fn toggle_flying(&mut self) {
        self.flying = !self.flying;
    }

    pub(crate) fn toggle_run(&mut self) {
        self.always_run = !self.always_run;
    }

    pub(crate) fn stop(&mut self) {
        self.pressed = 0;
        self.request_stop();
    }

    pub(crate) fn release_all(&mut self) {
        self.pressed = 0;
        self.request_stop();
    }

    pub(crate) fn release_and_observe(&mut self, body_rotation: Quat) -> MovementObservation {
        self.release_all();
        self.observation(false, body_rotation)
    }

    pub(crate) fn active(&self) -> bool {
        self.pressed != 0
    }

    pub(crate) fn yaw(&self, shift: bool) -> f32 {
        if shift {
            return 0.0;
        }
        let left = self.is_pressed(MovementKey::TurnLeft) || self.is_pressed(MovementKey::TurnLeftArrow);
        let right = self.is_pressed(MovementKey::TurnRight) || self.is_pressed(MovementKey::TurnRightArrow);
        f32::from(left as u8) - f32::from(right as u8)
    }

    pub(crate) fn observation(&mut self, shift: bool, body_rotation: Quat) -> MovementObservation {
        let mut control_flags = 0;
        let forward = self.is_pressed(MovementKey::Forward) || self.is_pressed(MovementKey::ForwardArrow);
        let backward = self.is_pressed(MovementKey::Backward) || self.is_pressed(MovementKey::BackwardArrow);
        let left = self.is_pressed(MovementKey::TurnLeft) || self.is_pressed(MovementKey::TurnLeftArrow);
        let right = self.is_pressed(MovementKey::TurnRight) || self.is_pressed(MovementKey::TurnRightArrow);
        if forward {
            control_flags |= AT_POS | FAST_AT;
        }
        if backward {
            control_flags |= AT_NEG | FAST_AT;
        }
        if shift {
            if left {
                control_flags |= LEFT_POS | FAST_LEFT;
            }
            if right {
                control_flags |= LEFT_NEG | FAST_LEFT;
            }
        } else {
            if left {
                control_flags |= YAW_POS;
            }
            if right {
                control_flags |= YAW_NEG;
            }
        }
        if self.is_pressed(MovementKey::Jump) || self.is_pressed(MovementKey::JumpPage) {
            control_flags |= UP_POS | FAST_UP;
        }
        if self.is_pressed(MovementKey::Down) || self.is_pressed(MovementKey::DownPage) {
            control_flags |= UP_NEG | FAST_UP;
        }
        if self.flying {
            control_flags |= FLY;
        }
        if self.stop_pending {
            control_flags |= STOP;
            self.stop_pending = false;
        }
        MovementObservation { control_flags, body_rotation, always_run: self.always_run, stop_sequence: self.stop_sequence }
    }

    fn is_pressed(&self, key: MovementKey) -> bool {
        self.pressed & key.mask() != 0
    }

    fn request_stop(&mut self) {
        self.stop_pending = true;
        self.stop_sequence = self.stop_sequence.wrapping_add(1);
    }
}

impl BodyRotation {
    pub(crate) fn current(&self) -> Quat {
        self.current
    }

    pub(crate) fn update(&mut self, seed: Option<(u64, Quat)>, yaw: f32, now: Instant) -> Quat {
        if let Some((generation, rotation)) = seed
            && self.seed != Some(generation)
            && let Some(rotation) = yaw_rotation(rotation)
        {
            self.seed = Some(generation);
            self.current = rotation;
        }
        let dt = self.updated_at.map_or(Duration::ZERO, |previous| now.saturating_duration_since(previous).min(MAX_TURN_STEP));
        self.updated_at = Some(now);
        if yaw != 0.0 {
            self.current = (Quat::from_rotation_z(yaw * TURN_RATE * dt.as_secs_f32()) * self.current).normalize();
        }
        self.current
    }
}

fn yaw_rotation(rotation: Quat) -> Option<Quat> {
    let forward = rotation * Vec3::X;
    let forward = Vec3::new(forward.x, forward.y, 0.0).try_normalize()?;
    Some(Quat::from_rotation_z(forward.y.atan2(forward.x)))
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use math::{Quat, Vec3};

    use super::{
        AT_NEG, AT_POS, BodyRotation, FAST_AT, FAST_LEFT, FAST_UP, FLY, HeldControls, KeyBinding, LEFT_NEG, LEFT_POS, MovementKey, STOP,
        UP_NEG, UP_POS, YAW_NEG, YAW_POS,
    };

    #[test]
    fn bindings_derive_control_flags() {
        let mut controls = HeldControls::default();
        for (key, expected) in [
            ("w", AT_POS | FAST_AT),
            ("s", AT_NEG | FAST_AT),
            ("a", YAW_POS),
            ("d", YAW_NEG),
            ("e", UP_POS | FAST_UP),
            ("c", UP_NEG | FAST_UP),
        ] {
            let binding = KeyBinding::from_key(key).expect("movement key");
            let KeyBinding::Movement(key) = binding else { panic!("movement binding") };
            controls.set_pressed(key, true);
            assert_eq!(controls.observation(false, Quat::IDENTITY).control_flags, expected);
            controls.set_pressed(key, false);
        }
        controls.set_pressed(MovementKey::TurnLeft, true);
        assert_eq!(controls.observation(true, Quat::IDENTITY).control_flags, LEFT_POS | FAST_LEFT);
        controls.set_pressed(MovementKey::TurnLeft, false);
        controls.set_pressed(MovementKey::TurnRightArrow, true);
        assert_eq!(controls.observation(true, Quat::IDENTITY).control_flags, LEFT_NEG | FAST_LEFT);
        controls.set_pressed(MovementKey::TurnRightArrow, false);
        controls.set_pressed(MovementKey::Forward, true);
        controls.set_pressed(MovementKey::BackwardArrow, true);
        assert_eq!(controls.observation(false, Quat::IDENTITY).control_flags, AT_POS | AT_NEG | FAST_AT);
    }

    #[test]
    fn captured_press_does_not_move() {
        let mut controls = HeldControls::default();
        assert_eq!(controls.keyboard_event("w", true, false, true), None);
        assert_eq!(controls.observation(false, Quat::IDENTITY).control_flags, 0);

        controls.keyboard_event("w", true, false, false).expect("forward press");
        controls.keyboard_event("w", false, false, true).expect("forward release");
        assert_eq!(controls.observation(false, Quat::IDENTITY).control_flags, 0);
    }

    #[test]
    fn toggles_and_release_publish_state() {
        let mut controls = HeldControls::default();
        controls.set_pressed(MovementKey::Forward, true);
        controls.toggle_flying();
        controls.toggle_run();
        let movement = controls.observation(false, Quat::IDENTITY);
        assert_eq!(movement.control_flags, AT_POS | FAST_AT | FLY);
        assert!(movement.always_run);

        let stopped = controls.release_and_observe(Quat::IDENTITY);
        assert_eq!(stopped.control_flags, STOP | FLY);
        assert!(stopped.stop_sequence > movement.stop_sequence);
        assert!(stopped.always_run);
        assert_eq!(controls.observation(false, Quat::IDENTITY).control_flags, FLY);
    }

    #[test]
    fn yaw_rate_is_ninety_dps() {
        let start = Instant::now();
        let mut rotation = BodyRotation::default();
        assert_eq!(rotation.update(Some((1, Quat::IDENTITY)), 1.0, start), Quat::IDENTITY);
        let mut current = Quat::IDENTITY;
        for step in 1..=10 {
            current = rotation.update(Some((1, Quat::IDENTITY)), 1.0, start + Duration::from_millis(step * 100));
        }
        assert!((current * Vec3::X).distance(Vec3::Y) < 1e-5);
    }

    #[test]
    fn seeded_rotation_ignores_simulator() {
        let start = Instant::now();
        let mut rotation = BodyRotation::default();
        rotation.update(Some((3, Quat::IDENTITY)), 1.0, start);
        let turned = rotation.update(Some((3, Quat::IDENTITY)), 1.0, start + Duration::from_millis(100));
        let current = rotation.update(Some((3, Quat::from_rotation_z(0.2))), 0.0, start + Duration::from_millis(200));
        assert!((current * Vec3::X).distance(turned * Vec3::X) < 1e-5);
    }

    #[test]
    fn arrival_marker_reseeds_rotation() {
        let start = Instant::now();
        let mut rotation = BodyRotation::default();
        rotation.update(Some((1, Quat::IDENTITY)), 1.0, start);
        rotation.update(Some((1, Quat::IDENTITY)), 1.0, start + Duration::from_millis(100));

        let arrival = Quat::from_rotation_z(0.7);
        let current = rotation.update(Some((2, arrival)), 0.0, start + Duration::from_millis(200));
        assert!((current * Vec3::X).distance(arrival * Vec3::X) < 1e-5);
    }

    #[test]
    fn seed_rotation_is_flattened() {
        let start = Instant::now();
        let mut rotation = BodyRotation::default();
        let seed = Quat::from_rotation_y(0.8) * Quat::from_rotation_z(0.5);
        let current = rotation.update(Some((1, seed)), 0.0, start);
        let forward = seed * Vec3::X;
        let expected = Vec3::new(forward.x, forward.y, 0.0).normalize();
        assert!((current * Vec3::X - expected).length() < 1e-5);
    }
}
