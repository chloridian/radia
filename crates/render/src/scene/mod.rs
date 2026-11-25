mod asset_demand;
mod budget;
mod camera;
mod dependencies;
mod environment;
mod environment_textures;
mod mesh;
mod mesh_cache;
mod model;
pub(crate) mod motion;
mod planning;
mod prim;
mod reconcile;
mod region_environment;
mod sculpt;
mod surface;
pub(crate) mod texture_animation;

#[cfg(test)]
use assets::mesh::{Face, Mesh, Vertex};
pub use utils::ExperienceId;
use uuid::Uuid;
use world::WorldPose;

pub use self::{
    asset_demand::{AssetChange, AssetRequest, AssetTicket},
    budget::{Budget, Limits},
    camera::{Camera, CameraError},
    environment::{
        EnvironmentChoice, EnvironmentPreset, EnvironmentProperty, EnvironmentPush, EnvironmentPushError, ExperienceProperty,
        ExperienceSetting, LocalChoice, LocalEnvironment, ViewerEnvironment,
    },
    mesh::{Detail, DrawnMeshKey, MeshBuildError, MeshId, SharedMesh},
    mesh_cache::{MeshKey, PreparedMesh, SculptMeshKey},
    model::{
        ApplyError, Batch, Geometry, HiddenReason, Object, Patch, Placement, RecoveryReason, RegionData, SceneView, Status, ViewVersion,
    },
    motion::MotionCircuit,
    planning::{MeshCompletion, MeshJob},
    reconcile::{
        MAX_BATCH_CHANGES, MAX_OBJECTS, MAX_REGIONS, MAX_RETAINED_BYTES, ReadFailure, ReadJob, ReadQuery, ReadReply, ReconcileStep,
        Reconciler,
    },
    region_environment::RegionEnvironment,
    sculpt::tessellate_sculpt,
    surface::{Surface, SurfaceKind},
};
pub(crate) use self::{
    camera::RANGE_MARGIN,
    environment::{PropertyChange, PropertyKind, PropertyValue},
    model::VisibilityVersion,
};

#[cfg(test)]
pub(crate) fn distinct_meshes_for_test(count: usize) -> Vec<SharedMesh> {
    let budget = Budget::default();
    (0..count)
        .map(|_| {
            let mesh = Mesh::from_parts(
                vec![
                    Vertex { position: math::Vec3::new(-1.0, -1.0, 0.0), normal: math::Vec3::Z, uv: math::Vec2::ZERO },
                    Vertex { position: math::Vec3::new(1.0, -1.0, 0.0), normal: math::Vec3::Z, uv: math::Vec2::ZERO },
                    Vertex { position: math::Vec3::new(0.0, 1.0, 0.0), normal: math::Vec3::Z, uv: math::Vec2::ZERO },
                ],
                vec![0, 1, 2],
                vec![Face { slot: 0, indices: 0..3 }],
            )
            .expect("test triangle is valid");
            let lease = budget.reserve(mesh.owned_bytes() + 64).expect("test meshes fit the scene budget");
            SharedMesh::new(mesh, lease)
        })
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AgentPose {
    agent_id: Uuid,
    pose: WorldPose,
}

impl AgentPose {
    pub fn new(agent_id: Uuid, pose: WorldPose) -> Self {
        Self { agent_id, pose }
    }

    pub(crate) fn agent_id(self) -> Uuid {
        self.agent_id
    }

    pub(crate) fn pose(self) -> WorldPose {
        self.pose
    }
}
