use utils::hardware::Hardware;

use crate::geometry::{DEPTH_BYTES, INSTANCE_BUFFER_BYTES};

pub const RESERVED_THREADS: usize = 2;
const MIB: usize = 1024 * 1024;
const GIB: usize = 1024 * MIB;
const DEFAULT_TEXTURE_SOFT_CAP_BYTES: usize = GIB;
const DEFAULT_MESH_BYTES: usize = 64 * MIB;
const MAX_MESH_BYTES: usize = 1024 * MIB;
const DEFAULT_TEXTURE_STAGING_BYTES: usize = 48 * MIB;
const MAX_TEXTURE_STAGING_BYTES: usize = 512 * MIB;
const DEFAULT_TEXTURE_JOBS: usize = 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Limits {
    pub texture_soft_cap_bytes: usize,
    pub mesh_bytes: usize,
    pub texture_staging_bytes: usize,
    pub texture_jobs: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            texture_soft_cap_bytes: DEFAULT_TEXTURE_SOFT_CAP_BYTES,
            mesh_bytes: DEFAULT_MESH_BYTES,
            texture_staging_bytes: DEFAULT_TEXTURE_STAGING_BYTES,
            texture_jobs: DEFAULT_TEXTURE_JOBS,
        }
    }
}

impl Limits {
    pub fn derive(hardware: &Hardware) -> Self {
        let (texture_jobs, _) = worker_split(hardware.cores);
        let usable_ram = hardware.usable_ram_bytes();
        let texture_staging_bytes =
            usize::try_from(usable_ram / 128).unwrap_or(usize::MAX).clamp(DEFAULT_TEXTURE_STAGING_BYTES, MAX_TEXTURE_STAGING_BYTES) / MIB
                * MIB;
        let vram_bytes = hardware.vram.map(|vram| vram.budget_bytes.unwrap_or(vram.local_bytes));
        let mesh_bytes = match vram_bytes {
            Some(vram) => usize::try_from(vram / 32).unwrap_or(usize::MAX).clamp(DEFAULT_MESH_BYTES, MAX_MESH_BYTES) / MIB * MIB,
            None => DEFAULT_MESH_BYTES,
        };
        let texture_soft_cap_bytes = match vram_bytes {
            Some(vram) => {
                let budget = vram.max(GIB as u64);
                let target = budget.saturating_sub(512 * MIB as u64).min(budget * 4 / 5).max(GIB as u64);
                let other_gpu = (mesh_bytes + DEPTH_BYTES + INSTANCE_BUFFER_BYTES) as u64;
                usize::try_from(target.saturating_sub(other_gpu)).unwrap_or(usize::MAX).max(DEFAULT_TEXTURE_SOFT_CAP_BYTES) / MIB * MIB
            }
            None => DEFAULT_TEXTURE_SOFT_CAP_BYTES,
        };
        Self { texture_soft_cap_bytes, mesh_bytes, texture_staging_bytes, texture_jobs }
    }
}

pub(crate) fn worker_split(cores: usize) -> (usize, usize) {
    let total = cores.saturating_sub(RESERVED_THREADS).max(2);
    let texture_jobs = total.div_ceil(2).max(1);
    let mesh_jobs = total.saturating_sub(texture_jobs).max(1);
    (texture_jobs, mesh_jobs)
}

#[cfg(test)]
mod tests {
    use utils::hardware::{Hardware, Vram};

    use super::*;
    use crate::scene;

    #[test]
    fn derive_low_end() {
        let hardware = Hardware { cores: 4, ram_bytes: 8 * 1024 * 1024 * 1024, vram: None };
        let render = Limits::derive(&hardware);
        let scene_limits = scene::Limits::derive(&hardware);
        let asset_limits = assets::Limits::derive(&hardware);

        assert!(render.texture_soft_cap_bytes >= DEFAULT_TEXTURE_SOFT_CAP_BYTES);
        assert!(render.mesh_bytes >= DEFAULT_MESH_BYTES);
        assert!(render.texture_staging_bytes >= DEFAULT_TEXTURE_STAGING_BYTES);
        assert!(scene_limits.memory_bytes >= scene::Limits::default().memory_bytes);
        assert!(asset_limits.work_bytes >= assets::Limits::default().work_bytes);
        assert!(asset_limits.texture_work_bytes >= assets::Limits::default().texture_work_bytes);

        let workers = render.texture_jobs + scene_limits.mesh_jobs;
        assert!(workers <= hardware.cores - RESERVED_THREADS);
        assert!(workers >= 2);
    }

    #[test]
    fn derive_high_core() {
        let hardware = Hardware {
            cores: 32,
            ram_bytes: 64 * 1024 * 1024 * 1024,
            vram: Some(Vram { local_bytes: 24 * 1024 * 1024 * 1024, budget_bytes: None }),
        };
        let render = Limits::derive(&hardware);
        let scene_limits = scene::Limits::derive(&hardware);
        let asset_limits = assets::Limits::derive(&hardware);

        let workers = render.texture_jobs + scene_limits.mesh_jobs;
        assert!(workers <= hardware.cores - RESERVED_THREADS);
        assert!(workers >= 2);

        let ram_sum = (asset_limits.work_bytes + scene_limits.memory_bytes + render.texture_staging_bytes) as u64;
        assert!(ram_sum <= hardware.usable_ram_bytes());

        let vram = 24 * 1024 * 1024 * 1024_u64;
        let expected_target = vram.saturating_sub(512 * MIB as u64).min(vram * 4 / 5);
        let other_gpu = (render.mesh_bytes + DEPTH_BYTES + INSTANCE_BUFFER_BYTES) as u64;
        let expected_texture_cap = ((expected_target - other_gpu) as usize) / MIB * MIB;
        assert_eq!(render.texture_soft_cap_bytes, expected_texture_cap);
    }

    #[test]
    fn derive_vram_unknown() {
        let hardware = Hardware { cores: 16, ram_bytes: 32 * 1024 * 1024 * 1024, vram: None };
        let render = Limits::derive(&hardware);
        assert_eq!(render.texture_soft_cap_bytes, DEFAULT_TEXTURE_SOFT_CAP_BYTES);
    }
}
