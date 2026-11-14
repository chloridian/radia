use std::time::{Duration, Instant};

use math::{DVec3, Quat, Vec3};
use protocol::messages::AgentMovementComplete;
use tokio::sync::watch;
use uuid::Uuid;
use world::{ObjectDetail, Parent, Region, WorldTransformState};

const MAX_ARRIVAL_XY: f32 = 256.0;
const MAX_ARRIVAL_Z: f32 = 4096.0;
const MAX_MOTION_COMPONENT: f32 = 256.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct AgentMotionSample {
    pub(crate) region: Region,
    pub(crate) position: DVec3,
    pub(crate) scale: Vec3,
    pub(crate) velocity: Vec3,
    pub(crate) acceleration: Vec3,
    pub(crate) time_dilation: f32,
    pub(crate) ping_delay: Option<Duration>,
    pub(crate) parented: bool,
    pub(crate) simulator_rotation: Quat,
    pub(crate) received_at: Instant,
    pub(crate) seed: u64,
}

impl AgentMotionSample {
    pub(crate) fn is_finite(self) -> bool {
        self.position.is_finite()
            && self.velocity.is_finite()
            && self.acceleration.is_finite()
            && self.scale.is_finite()
            && self.scale.min_element() > 0.0
            && self.time_dilation.is_finite()
            && (0.0..=1.0).contains(&self.time_dilation)
            && self.simulator_rotation.is_finite()
            && self.simulator_rotation.length_squared().is_finite()
            && self.simulator_rotation.length_squared() > f32::EPSILON
    }
}

pub(crate) struct AgentMotionPublisher {
    sender: watch::Sender<Option<AgentMotionSample>>,
    seed: u64,
}

impl AgentMotionPublisher {
    pub(crate) fn new() -> Self {
        let (sender, _) = watch::channel(None);
        Self { sender, seed: 0 }
    }

    #[cfg(any(test, target_os = "windows", target_os = "linux"))]
    pub(crate) fn receiver(&self) -> watch::Receiver<Option<AgentMotionSample>> {
        self.sender.subscribe()
    }

    pub(crate) fn publish_arrival(
        &mut self,
        agent_id: Uuid,
        session_id: Uuid,
        movement: &AgentMovementComplete,
        region: Region,
        region_origin: DVec3,
        received_at: Instant,
        ping_delay: Option<Duration>,
    ) -> bool {
        if self.sender.borrow().is_some()
            || movement.agent_data.agent_id != agent_id
            || movement.agent_data.session_id != session_id
            || !region_origin.is_finite()
        {
            return false;
        }
        let local_position = movement.data.position;
        if !(0.0..=MAX_ARRIVAL_XY).contains(&local_position.x)
            || !(0.0..=MAX_ARRIVAL_XY).contains(&local_position.y)
            || !(0.0..=MAX_ARRIVAL_Z).contains(&local_position.z)
        {
            return false;
        }
        let Some(look_at) = Vec3::new(movement.data.look_at.x, movement.data.look_at.y, 0.0).try_normalize() else {
            return false;
        };
        let position = region_origin + local_position.as_dvec3();
        let simulator_rotation = Quat::from_rotation_z(look_at.y.atan2(look_at.x));
        let sample = AgentMotionSample {
            region,
            position,
            scale: Vec3::new(0.6, 0.45, 1.9),
            velocity: Vec3::ZERO,
            acceleration: Vec3::ZERO,
            time_dilation: 1.0,
            ping_delay,
            parented: false,
            simulator_rotation,
            received_at,
            seed: self.seed.wrapping_add(1),
        };
        if !sample.is_finite() {
            return false;
        }
        self.seed = sample.seed;
        self.sender.send_replace(Some(sample));
        true
    }

    pub(crate) fn publish_world_detail(&self, detail: &ObjectDetail, received_at: Instant) -> bool {
        let Ok(transform) = &detail.world_transform else { return false };
        let pose = match transform {
            WorldTransformState::Resolved(transform) => transform.pose,
            WorldTransformState::MissingScale(pose) => *pose,
            WorldTransformState::Provisional { .. } | WorldTransformState::Unresolved(_) => return false,
        };
        let motion = detail.linear_motion;
        let source = detail.motion_source;
        let received_at = source.and_then(world::UpdateSource::motion_received_at).unwrap_or(received_at);
        let parented = matches!(detail.summary.parent, Parent::Resolved(_) | Parent::Pending { .. });
        let (velocity, acceleration) = if parented {
            (Vec3::ZERO, Vec3::ZERO)
        } else {
            (motion.velocity.unwrap_or_default(), motion.acceleration.unwrap_or_default())
        };
        let (velocity, acceleration) =
            if velocity.abs().max_element() <= MAX_MOTION_COMPONENT && acceleration.abs().max_element() <= MAX_MOTION_COMPONENT {
                (velocity, acceleration)
            } else {
                (Vec3::ZERO, Vec3::ZERO)
            };
        self.publish(AgentMotionSample {
            region: detail.summary.address.region(),
            position: pose.position,
            scale: detail.summary.local_transform.scale().unwrap_or(Vec3::new(0.6, 0.45, 1.9)),
            velocity,
            acceleration,
            time_dilation: detail.time_dilation,
            ping_delay: source.and_then(world::UpdateSource::motion_ping_delay),
            parented,
            simulator_rotation: pose.rotation,
            received_at,
            seed: self.seed,
        })
    }

    fn publish(&self, sample: AgentMotionSample) -> bool {
        if !sample.is_finite() {
            return false;
        }
        self.sender.send_replace(Some(sample));
        true
    }
}

#[cfg(test)]
mod tests {
    use std::{
        num::NonZeroU32,
        time::{Duration, Instant},
    };

    use math::{DVec3, Quat, Vec3};
    use protocol::{lludp::Message, messages::AgentMovementComplete};
    use uuid::Uuid;
    use world::{Kind, LinearMotion, ObjectObservation, OrderEvidence::Ordered, ParentUpdate, Pose, RegionInfo, UpdateSource, World};

    use crate::session::world::viewer_world_limits;

    use super::{AgentMotionPublisher, AgentMotionSample};

    fn publish_agent(publisher: &AgentMotionPublisher, world: &World, agent_id: Uuid, received_at: Instant) -> bool {
        let entity = world.entity(agent_id).expect("agent entity");
        let detail = world.object_detail(entity).expect("agent detail");
        publisher.publish_world_detail(&detail, received_at)
    }

    fn arrival() -> AgentMovementComplete {
        let bytes = test_support::parse_hex_fixture(test_support::lludp::vectors::AGENT_MOVEMENT_COMPLETE).expect("movement fixture");
        let packet = protocol::lludp::Packet::decode(&bytes).expect("valid movement packet");
        let Message::AgentMovementComplete(movement) = packet.message() else { panic!("movement-complete fixture") };
        movement.clone()
    }

    #[test]
    fn arrival_flattens_look_bounds_position() {
        let (_region_world, region) = crate::test_helpers::registered_region();
        let mut publisher = AgentMotionPublisher::new();
        let mut movement = arrival();
        movement.data.position = Vec3::new(10.0, 20.0, 30.0);
        movement.data.look_at = Vec3::new(0.0, 1.0, 10.0);
        let now = std::time::Instant::now();
        assert!(publisher.publish_arrival(
            movement.agent_data.agent_id,
            movement.agent_data.session_id,
            &movement,
            region,
            DVec3::new(256.0, 512.0, 0.0),
            now,
            None,
        ));
        let sample = publisher.receiver().borrow().expect("arrival sample");
        assert_eq!(sample.position, DVec3::new(266.0, 532.0, 30.0));
        assert_eq!(sample.received_at, now);
        assert_eq!(sample.velocity, Vec3::ZERO);
        assert_eq!(sample.acceleration, Vec3::ZERO);
        assert!((sample.simulator_rotation * Vec3::X - Vec3::Y).length() < 1e-6);

        movement.data.position.x = 1.0e20;
        assert!(!publisher.publish_arrival(
            movement.agent_data.agent_id,
            movement.agent_data.session_id,
            &movement,
            region,
            DVec3::ZERO,
            now + Duration::from_secs(1),
            None,
        ));
        assert_eq!(publisher.receiver().borrow().expect("last valid sample").position, DVec3::new(266.0, 532.0, 30.0));
    }

    #[test]
    fn arrival_keeps_existing_motion() {
        let (_region_world, region) = crate::test_helpers::registered_region();
        let mut publisher = AgentMotionPublisher::new();
        let movement = arrival();
        let (agent_id, session_id) = (movement.agent_data.agent_id, movement.agent_data.session_id);
        let now = std::time::Instant::now();
        assert!(publisher.publish_arrival(agent_id, session_id, &movement, region, DVec3::ZERO, now, None));
        let first = publisher.receiver().borrow().expect("arrival sample");
        assert!(!publisher.publish_arrival(
            agent_id,
            session_id,
            &movement,
            region,
            DVec3::new(256.0, 0.0, 0.0),
            now + Duration::from_secs(1),
            None
        ));
        assert_eq!(publisher.receiver().borrow().expect("first arrival kept"), first);
    }

    #[test]
    fn seated_agent_publishes_zero_motion() {
        let mut world = World::new(viewer_world_limits());
        let region = world.register_region().expect("region registers");
        let _ = world
            .patch_region(region, RegionInfo { origin: Some(DVec3::new(256.0, 0.0, 0.0)), ..RegionInfo::default() })
            .expect("region origin");
        let agent_id = Uuid::from_u128(1);
        world.set_self_agent(agent_id).expect("self agent");
        let source = UpdateSource::new(region, 1, Ordered(1), 0);
        let seat_id = Uuid::from_u128(2);
        let mut seat =
            ObjectObservation::new(source, seat_id, NonZeroU32::new(1).expect("seat local ID"), Kind::Object, ParentUpdate::Root);
        seat.pose = Some(Pose { position: Some(Vec3::new(10.0, 20.0, 0.0)), rotation: Some(Quat::IDENTITY) });
        let _ = world.observe_object(seat).expect("seat observation");
        let mut agent = ObjectObservation::new(
            source,
            agent_id,
            NonZeroU32::new(2).expect("agent local ID"),
            Kind::Avatar,
            ParentUpdate::Local(NonZeroU32::new(1).expect("seat local ID")),
        );
        agent.pose = Some(Pose { position: Some(Vec3::new(0.0, 0.0, 1.0)), rotation: Some(Quat::IDENTITY) });
        agent.linear_motion = Some(LinearMotion { velocity: Some(Vec3::X), acceleration: Some(Vec3::Y) });
        let _ = world.observe_object(agent).expect("agent observation");

        let publisher = AgentMotionPublisher::new();
        let now = std::time::Instant::now();
        assert!(publish_agent(&publisher, &world, agent_id, now));
        let sample = publisher.receiver().borrow().expect("world sample");
        assert_eq!(sample.position, DVec3::new(266.0, 20.0, 1.0));
        assert_eq!(sample.velocity, Vec3::ZERO);
        assert_eq!(sample.acceleration, Vec3::ZERO);
        assert_eq!(sample.received_at, now);
    }

    #[test]
    fn root_agent_publishes_world_motion() {
        let mut world = World::new(viewer_world_limits());
        let region = world.register_region().expect("region registers");
        let _ = world
            .patch_region(region, RegionInfo { origin: Some(DVec3::new(256.0, 0.0, 0.0)), ..RegionInfo::default() })
            .expect("region origin");
        let agent_id = Uuid::from_u128(1);
        world.set_self_agent(agent_id).expect("self agent");
        let rotation = Quat::from_rotation_z(0.4);
        let received_at = std::time::Instant::now();
        let ping_delay = Some(Duration::from_millis(120));
        let mut agent = ObjectObservation::new(
            UpdateSource::new(region, 1, Ordered(1), 0).with_motion_timing(received_at, ping_delay),
            agent_id,
            NonZeroU32::new(1).expect("agent local ID"),
            Kind::Avatar,
            ParentUpdate::Root,
        );
        agent.pose = Some(Pose { position: Some(Vec3::new(10.0, 20.0, 3.0)), rotation: Some(rotation) });
        agent.linear_motion = Some(LinearMotion { velocity: Some(Vec3::X), acceleration: Some(Vec3::Y) });
        let _ = world.observe_object(agent).expect("agent observation");

        let publisher = AgentMotionPublisher::new();
        let now = received_at + Duration::from_millis(100);
        assert!(publish_agent(&publisher, &world, agent_id, now));
        let sample = publisher.receiver().borrow().expect("world sample");
        assert_eq!(sample.position, DVec3::new(266.0, 20.0, 3.0));
        assert_eq!(sample.velocity, Vec3::X);
        assert_eq!(sample.acceleration, Vec3::Y);
        assert_eq!(sample.simulator_rotation, rotation);
        assert_eq!(sample.received_at, received_at);
        assert_eq!(sample.ping_delay, ping_delay);
    }

    #[test]
    fn huge_velocity_is_discarded() {
        let mut world = World::new(viewer_world_limits());
        let region = world.register_region().expect("region registers");
        let _ = world.patch_region(region, RegionInfo { origin: Some(DVec3::ZERO), ..RegionInfo::default() }).expect("region origin");
        let agent_id = Uuid::from_u128(1);
        world.set_self_agent(agent_id).expect("self agent");
        let mut agent = ObjectObservation::new(
            UpdateSource::new(region, 1, Ordered(1), 0),
            agent_id,
            NonZeroU32::new(1).expect("agent local ID"),
            Kind::Avatar,
            ParentUpdate::Root,
        );
        agent.pose = Some(Pose { position: Some(Vec3::new(10.0, 20.0, 3.0)), rotation: Some(Quat::IDENTITY) });
        agent.linear_motion = Some(LinearMotion { velocity: Some(Vec3::X * 1.0e30), acceleration: Some(Vec3::Y) });
        let _ = world.observe_object(agent).expect("agent observation");

        let publisher = AgentMotionPublisher::new();
        assert!(publish_agent(&publisher, &world, agent_id, std::time::Instant::now()));
        let sample = publisher.receiver().borrow().expect("world sample");
        assert_eq!(sample.position, DVec3::new(10.0, 20.0, 3.0));
        assert_eq!(sample.velocity, Vec3::ZERO);
        assert_eq!(sample.acceleration, Vec3::ZERO);
    }

    #[test]
    fn non_finite_sample_is_rejected() {
        let (_region_world, region) = crate::test_helpers::registered_region();
        let sample = AgentMotionSample {
            region,
            position: DVec3::ZERO,
            scale: Vec3::ONE,
            velocity: Vec3::new(f32::INFINITY, 0.0, 0.0),
            acceleration: Vec3::ZERO,
            time_dilation: 1.0,
            ping_delay: None,
            parented: false,
            simulator_rotation: Quat::IDENTITY,
            received_at: std::time::Instant::now(),
            seed: 0,
        };
        assert!(!sample.is_finite());
    }
}
