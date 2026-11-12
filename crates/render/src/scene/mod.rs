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

use uuid::Uuid;
use world::WorldPose;

pub use asset_demand::{AssetChange, AssetRequest, AssetTicket};
pub use budget::{Budget, MEMORY_BYTES};
pub use camera::{Camera, CameraError};
pub use environment::{
    EnvironmentChoice, EnvironmentPreset, EnvironmentProperty, EnvironmentPush, EnvironmentPushError, ExperienceProperty,
    ExperienceSetting, LocalChoice, LocalEnvironment, ViewerEnvironment,
};
pub(crate) use environment::{PropertyChange, PropertyKind, PropertyValue};
pub use mesh::{Detail, DrawnMeshKey, MeshBuildError, MeshId, SharedMesh};
pub use mesh_cache::SculptMeshKey;
pub use mesh_cache::{MeshKey, PreparedMesh};
pub(crate) use model::VisibilityVersion;
pub use model::{
    ApplyError, Batch, Geometry, HiddenReason, Object, Patch, Placement, RecoveryReason, RegionData, SceneView, Status, ViewVersion,
};
pub use motion::MotionCircuit;
pub use planning::{MeshCompletion, MeshJob};
pub use reconcile::{
    MAX_BATCH_CHANGES, MAX_OBJECTS, MAX_REGIONS, MAX_RETAINED_BYTES, ReadFailure, ReadJob, ReadQuery, ReadReply, ReconcileStep, Reconciler,
};
pub use region_environment::RegionEnvironment;
pub use sculpt::tessellate_sculpt;
pub use surface::{Surface, SurfaceKind};
pub use utils::ExperienceId;

#[cfg(test)]
pub(crate) fn distinct_meshes_for_test(count: usize) -> Vec<SharedMesh> {
    let budget = Budget::default();
    (0..count)
        .map(|_| {
            let mesh = assets::mesh::Mesh::from_parts(
                vec![
                    assets::mesh::Vertex { position: math::Vec3::new(-1.0, -1.0, 0.0), normal: math::Vec3::Z, uv: math::Vec2::ZERO },
                    assets::mesh::Vertex { position: math::Vec3::new(1.0, -1.0, 0.0), normal: math::Vec3::Z, uv: math::Vec2::ZERO },
                    assets::mesh::Vertex { position: math::Vec3::new(0.0, 1.0, 0.0), normal: math::Vec3::Z, uv: math::Vec2::ZERO },
                ],
                vec![0, 1, 2],
                vec![assets::mesh::Face { slot: 0, indices: 0..3 }],
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
