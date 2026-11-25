use cvars::{
    ASSET_TEXTURE_WORK_BUDGET_MIB, ASSET_WORK_BUDGET_MIB, CvarKey, CvarSnapshot, RENDER_MESH_JOBS, RENDER_MESH_MEMORY_MIB,
    RENDER_TEXTURE_JOBS, RENDER_TEXTURE_SOFT_CAP_MIB, RENDER_TEXTURE_STAGING_MIB, SCENE_MEMORY_BUDGET_MIB, ValueDomain,
};
use render::scene;
use utils::hardware::{Hardware, Vram};

const MIB: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StartupLimits {
    pub hardware: Hardware,
    pub assets: assets::Limits,
    pub scene: scene::Limits,
    pub texture_jobs: usize,
    pub texture_staging_bytes: usize,
}

impl Default for StartupLimits {
    fn default() -> Self {
        let render = render::Limits::default();
        Self {
            hardware: Hardware { cores: 4, ram_bytes: 0, vram: None },
            assets: assets::Limits::default(),
            scene: scene::Limits::default(),
            texture_jobs: render.texture_jobs,
            texture_staging_bytes: render.texture_staging_bytes,
        }
    }
}

impl StartupLimits {
    pub fn resolve(hardware: Hardware, snapshot: &CvarSnapshot) -> Self {
        let derived_assets = assets::Limits::derive(&hardware);
        let derived_scene = scene::Limits::derive(&hardware);
        let derived_render = render::Limits::derive(&hardware);

        let work_bytes = resolve_mib(snapshot, ASSET_WORK_BUDGET_MIB, derived_assets.work_bytes);
        let texture_work_bytes = resolve_mib(snapshot, ASSET_TEXTURE_WORK_BUDGET_MIB, derived_assets.texture_work_bytes).min(work_bytes);
        let memory_bytes = resolve_mib(snapshot, SCENE_MEMORY_BUDGET_MIB, derived_scene.memory_bytes);
        let mesh_jobs = resolve_count(snapshot, RENDER_MESH_JOBS, derived_scene.mesh_jobs);
        let texture_jobs = resolve_count(snapshot, RENDER_TEXTURE_JOBS, derived_render.texture_jobs);
        let texture_staging_bytes = resolve_mib(snapshot, RENDER_TEXTURE_STAGING_MIB, derived_render.texture_staging_bytes);

        Self {
            hardware,
            assets: assets::Limits { work_bytes, texture_work_bytes },
            scene: scene::Limits { memory_bytes, mesh_jobs },
            texture_jobs,
            texture_staging_bytes,
        }
    }

    pub fn resolve_render(&self, vram: Option<Vram>, snapshot: &CvarSnapshot) -> render::Limits {
        let hardware = Hardware { vram, ..self.hardware };
        let derived = render::Limits::derive(&hardware);
        let texture_soft_cap_bytes = resolve_mib(snapshot, RENDER_TEXTURE_SOFT_CAP_MIB, derived.texture_soft_cap_bytes);
        let mesh_bytes = resolve_mib(snapshot, RENDER_MESH_MEMORY_MIB, derived.mesh_bytes);
        render::Limits {
            texture_soft_cap_bytes,
            mesh_bytes,
            texture_staging_bytes: self.texture_staging_bytes,
            texture_jobs: self.texture_jobs,
        }
    }

    pub fn resolve_texture_soft_cap_mib(&self, vram: Option<Vram>, snapshot: &CvarSnapshot) -> u32 {
        let bytes = self.resolve_render(vram, snapshot).texture_soft_cap_bytes;
        u32::try_from(bytes / MIB).unwrap_or(u32::MAX)
    }
}

fn resolve_mib(snapshot: &CvarSnapshot, key: CvarKey<i64>, derived_bytes: usize) -> usize {
    let raw = snapshot.get(key);
    if raw == 0 {
        return derived_bytes;
    }
    clamp_i64_to_domain(key, raw).saturating_mul(MIB)
}

fn resolve_count(snapshot: &CvarSnapshot, key: CvarKey<i64>, derived: usize) -> usize {
    let raw = snapshot.get(key);
    if raw == 0 {
        return derived;
    }
    clamp_i64_to_domain(key, raw)
}

fn clamp_i64_to_domain(key: CvarKey<i64>, value: i64) -> usize {
    let ValueDomain::I64 { min, max } = key.descriptor().domain else {
        unreachable!("i64 CVarKey has I64 domain");
    };
    usize::try_from(value.clamp(min, max)).unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use cvars::{
        ASSET_TEXTURE_WORK_BUDGET_MIB, ASSET_WORK_BUDGET_MIB, CvarRegistry, Layer, RENDER_MESH_JOBS, RENDER_MESH_MEMORY_MIB,
        RENDER_TEXTURE_JOBS, RENDER_TEXTURE_SOFT_CAP_MIB, RENDER_TEXTURE_STAGING_MIB, SCENE_MEMORY_BUDGET_MIB,
    };
    use utils::hardware::{Hardware, Vram};

    use super::*;

    #[test]
    fn override_wins() {
        let hardware = Hardware {
            cores: 32,
            ram_bytes: 64 * 1024 * 1024 * 1024,
            vram: Some(Vram { local_bytes: 24 * 1024 * 1024 * 1024, budget_bytes: None }),
        };
        let mut registry = CvarRegistry::new();
        registry.set(ASSET_WORK_BUDGET_MIB, Layer::Global, 768).unwrap();
        registry.set(ASSET_TEXTURE_WORK_BUDGET_MIB, Layer::Global, 256).unwrap();
        registry.set(SCENE_MEMORY_BUDGET_MIB, Layer::Global, 512).unwrap();
        registry.set(RENDER_MESH_JOBS, Layer::Global, 5).unwrap();
        registry.set(RENDER_TEXTURE_JOBS, Layer::Global, 7).unwrap();
        registry.set(RENDER_TEXTURE_STAGING_MIB, Layer::Global, 96).unwrap();
        registry.set(RENDER_MESH_MEMORY_MIB, Layer::Global, 128).unwrap();
        registry.set(RENDER_TEXTURE_SOFT_CAP_MIB, Layer::Global, 2048).unwrap();
        registry.lock_startup_values();
        let snapshot = registry.snapshot();

        let startup = StartupLimits::resolve(hardware, &snapshot);
        assert_eq!(startup.assets.work_bytes, 768 * MIB);
        assert_eq!(startup.assets.texture_work_bytes, 256 * MIB);
        assert_eq!(startup.scene.memory_bytes, 512 * MIB);
        assert_eq!(startup.scene.mesh_jobs, 5);
        assert_eq!(startup.texture_jobs, 7);
        assert_eq!(startup.texture_staging_bytes, 96 * MIB);

        let render = startup.resolve_render(hardware.vram, &snapshot);
        assert_eq!(render.mesh_bytes, 128 * MIB);
        assert_eq!(render.texture_soft_cap_bytes, 2048 * MIB);
        assert_eq!(render.texture_staging_bytes, 96 * MIB);
        assert_eq!(render.texture_jobs, 7);

        assert_eq!(clamp_i64_to_domain(RENDER_TEXTURE_SOFT_CAP_MIB, 50), 128);
        assert_eq!(clamp_i64_to_domain(RENDER_TEXTURE_SOFT_CAP_MIB, 999_999), 65536);
    }
}
