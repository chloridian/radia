use utils::hardware::Hardware;

use crate::geometry::{DEPTH_BYTES, INSTANCE_BUFFER_BYTES};

pub const RESERVED_THREADS: usize = 2;
const MIB: usize = 1024 * 1024;
const GIB: usize = 1024 * MIB;
const MIN_VRAM_BUDGET_BYTES: u64 = GIB as u64;
const MIN_TEXTURE_SOFT_CAP_BYTES: usize = 768 * MIB;
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
        Self::derive_with_vram_policy(hardware, None, 1)
    }

    pub fn derive_with_vram_policy(hardware: &Hardware, max_vram_budget_bytes: Option<u64>, vram_divisor: u32) -> Self {
        let (texture_jobs, _) = worker_split(hardware.cores);
        let usable_ram = hardware.usable_ram_bytes();
        let texture_staging_bytes =
            usize::try_from(usable_ram / 128).unwrap_or(usize::MAX).clamp(DEFAULT_TEXTURE_STAGING_BYTES, MAX_TEXTURE_STAGING_BYTES) / MIB
                * MIB;
        let divisor = u64::from(vram_divisor.clamp(1, 8));
        let budget = match max_vram_budget_bytes {
            Some(override_bytes) => override_bytes.max(MIN_VRAM_BUDGET_BYTES),
            None => match hardware.vram.map(|vram| vram.budget_bytes.unwrap_or(vram.local_bytes)) {
                Some(vram) => (vram / divisor).max(MIN_VRAM_BUDGET_BYTES),
                None => MIN_VRAM_BUDGET_BYTES,
            },
        };
        let mesh_bytes = usize::try_from(budget / 32).unwrap_or(usize::MAX).clamp(DEFAULT_MESH_BYTES, MAX_MESH_BYTES) / MIB * MIB;
        let target = budget.saturating_sub(512 * MIB as u64).min(budget * 4 / 5).max(MIN_TEXTURE_SOFT_CAP_BYTES as u64);
        let other_gpu = (mesh_bytes + DEPTH_BYTES + INSTANCE_BUFFER_BYTES) as u64;
        let texture_soft_cap_bytes =
            usize::try_from(target.saturating_sub(other_gpu)).unwrap_or(usize::MAX).max(MIN_TEXTURE_SOFT_CAP_BYTES) / MIB * MIB;
        Self { texture_soft_cap_bytes, mesh_bytes, texture_staging_bytes, texture_jobs }
    }
}

pub fn split_worker_threads(total: usize) -> (usize, usize) {
    let total = total.max(2);
    let texture_jobs = total.div_ceil(2).max(1);
    let mesh_jobs = total.saturating_sub(texture_jobs).max(1);
    (texture_jobs, mesh_jobs)
}

pub(crate) fn worker_split(cores: usize) -> (usize, usize) {
    split_worker_threads(cores.saturating_sub(RESERVED_THREADS).max(2))
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

        assert_eq!(render.texture_soft_cap_bytes, MIN_TEXTURE_SOFT_CAP_BYTES);
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
    fn derive_vram_and_divisor() {
        let hardware = Hardware {
            cores: 6,
            ram_bytes: 32 * 1024 * 1024 * 1024,
            vram: Some(Vram { local_bytes: 6 * 1024 * 1024 * 1024, budget_bytes: None }),
        };
        let div1 = Limits::derive_with_vram_policy(&hardware, None, 1);
        assert_eq!(div1.mesh_bytes, 192 * MIB);
        assert_eq!(div1.texture_soft_cap_bytes, (4915 - 192 - 128 - 64) * MIB);

        let div2 = Limits::derive_with_vram_policy(&hardware, None, 2);
        assert_eq!(div2.mesh_bytes, 96 * MIB);
        assert_eq!(div2.texture_soft_cap_bytes, (2457 - 96 - 128 - 64) * MIB);
    }

    #[test]
    fn derive_vram_override_wins_over_divisor() {
        let hardware = Hardware {
            cores: 6,
            ram_bytes: 32 * 1024 * 1024 * 1024,
            vram: Some(Vram { local_bytes: 6 * 1024 * 1024 * 1024, budget_bytes: None }),
        };
        let limits = Limits::derive_with_vram_policy(&hardware, Some(1792 * MIB as u64), 4);
        assert_eq!(limits.mesh_bytes, 64 * MIB);
        assert_eq!(limits.texture_soft_cap_bytes, 1024 * MIB);
    }

    #[test]
    fn derive_vram_unknown() {
        let hardware = Hardware { cores: 16, ram_bytes: 32 * 1024 * 1024 * 1024, vram: None };
        let render = Limits::derive(&hardware);
        assert_eq!(render.mesh_bytes, 64 * MIB);
        assert_eq!(render.texture_soft_cap_bytes, MIN_TEXTURE_SOFT_CAP_BYTES);
    }
}
