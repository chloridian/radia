use cvars::{
    CvarKey, CvarSnapshot, MEMORY_MAX_RAM_MIB, MEMORY_MAX_VRAM_MIB, MEMORY_VRAM_DIVISOR, NETWORK_MAX_BANDWIDTH_KBPS, RENDER_WORKER_THREADS,
    ValueDomain,
};
use network::{default_throttle_rates, throttle_rates_for_bandwidth_kbps};
use protocol::messages::ThrottleRates;
use render::{auto_worker_threads, scene, split_worker_threads};
use utils::hardware::{Hardware, Vram};

const MIB: usize = 1024 * 1024;
const MIN_DECODE_RESERVE_BYTES: usize = 64 * MIB;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StartupLimits {
    pub hardware: Hardware,
    pub assets: assets::Limits,
    pub scene: scene::Limits,
    pub texture_jobs: usize,
    pub texture_staging_bytes: usize,
    pub throttle_rates: ThrottleRates,
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
            throttle_rates: default_throttle_rates(),
        }
    }
}

impl StartupLimits {
    pub fn resolve(hardware: Hardware, snapshot: &CvarSnapshot) -> Self {
        let derived_render = render::Limits::derive(&hardware);
        let auto_workers = auto_worker_threads(hardware.cores);
        let total_workers = resolve_count(snapshot, RENDER_WORKER_THREADS, auto_workers);
        let (texture_jobs, mesh_jobs) = split_worker_threads(total_workers);
        let derived_assets = assets::Limits::derive(total_workers);
        let raw_ram_cap = snapshot.get(MEMORY_MAX_RAM_MIB);
        let (assets, memory_bytes) = if raw_ram_cap == 0 {
            (derived_assets, scene::UNBOUNDED_MEMORY_BYTES)
        } else {
            let cap_bytes = clamp_i64_to_domain(MEMORY_MAX_RAM_MIB, raw_ram_cap).saturating_mul(MIB);
            resolve_capped_ram(derived_assets, cap_bytes)
        };
        let texture_staging_bytes = derived_render.texture_staging_bytes;
        let throttle_rates = resolve_throttle_rates(snapshot);

        Self { hardware, assets, scene: scene::Limits { memory_bytes, mesh_jobs }, texture_jobs, texture_staging_bytes, throttle_rates }
    }

    pub fn resolve_render(&self, vram: Option<Vram>, snapshot: &CvarSnapshot) -> render::Limits {
        let hardware = Hardware { vram, ..self.hardware };
        let raw_budget = snapshot.get(MEMORY_MAX_VRAM_MIB);
        let max_vram_budget_bytes =
            (raw_budget != 0).then(|| (clamp_i64_to_domain(MEMORY_MAX_VRAM_MIB, raw_budget) as u64).saturating_mul(MIB as u64));
        let vram_divisor = u32::try_from(clamp_i64_to_domain(MEMORY_VRAM_DIVISOR, snapshot.get(MEMORY_VRAM_DIVISOR))).unwrap_or(1);
        let derived = render::Limits::derive_with_vram_policy(&hardware, max_vram_budget_bytes, vram_divisor);
        render::Limits {
            texture_soft_cap_bytes: derived.texture_soft_cap_bytes,
            mesh_bytes: derived.mesh_bytes,
            texture_staging_bytes: self.texture_staging_bytes,
            texture_jobs: self.texture_jobs,
        }
    }

    pub fn resolve_texture_soft_cap_mib(&self, vram: Option<Vram>, snapshot: &CvarSnapshot) -> u32 {
        let bytes = self.resolve_render(vram, snapshot).texture_soft_cap_bytes;
        u32::try_from(bytes / MIB).unwrap_or(u32::MAX)
    }
}

pub fn resolve_throttle_rates(snapshot: &CvarSnapshot) -> ThrottleRates {
    let raw = snapshot.get(NETWORK_MAX_BANDWIDTH_KBPS);
    if raw == 0 {
        return default_throttle_rates();
    }
    let kbps = u32::try_from(clamp_i64_to_domain(NETWORK_MAX_BANDWIDTH_KBPS, raw)).unwrap_or(u32::MAX);
    throttle_rates_for_bandwidth_kbps(kbps)
}

pub fn parse_max_texture_resolution(value: &str) -> u32 {
    match value {
        "512" => 512,
        "1024" => 1024,
        _ => 2048,
    }
}

fn resolve_capped_ram(derived_assets: assets::Limits, cap_bytes: usize) -> (assets::Limits, usize) {
    if cap_bytes >= derived_assets.work_bytes.saturating_add(scene::MIN_CAPPED_MEMORY_BYTES) {
        return (derived_assets, cap_bytes - derived_assets.work_bytes);
    }
    if cap_bytes >= scene::MIN_CAPPED_MEMORY_BYTES.saturating_add(MIN_DECODE_RESERVE_BYTES) {
        let scene_bytes = scene::MIN_CAPPED_MEMORY_BYTES;
        let assets = assets::Limits::from_work_bytes(cap_bytes - scene_bytes);
        return (assets, cap_bytes - assets.work_bytes);
    }
    let assets = assets::Limits::from_work_bytes(cap_bytes * 2 / 3);
    (assets, cap_bytes - assets.work_bytes)
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
        CvarRegistry, Layer, MEMORY_MAX_RAM_MIB, MEMORY_MAX_VRAM_MIB, MEMORY_VRAM_DIVISOR, NETWORK_MAX_BANDWIDTH_KBPS,
        RENDER_WORKER_THREADS,
    };
    use utils::hardware::{Hardware, Vram};

    use super::*;

    #[test]
    fn cap_zero_leaves_scene_unbounded() {
        let hardware = Hardware {
            cores: 16,
            ram_bytes: 32 * 1024 * 1024 * 1024,
            vram: Some(Vram { local_bytes: 12 * 1024 * 1024 * 1024, budget_bytes: None }),
        };
        let mut registry = CvarRegistry::new();
        registry.lock_startup_values();
        let startup = StartupLimits::resolve(hardware, &registry.snapshot());

        assert_eq!(startup.scene.memory_bytes, scene::UNBOUNDED_MEMORY_BYTES);
        assert_eq!(startup.texture_jobs + startup.scene.mesh_jobs, 14);
        assert_eq!(startup.assets.work_bytes, 14 * 128 * MIB);
        assert_eq!(startup.assets.texture_work_bytes, (14 * 128 * 3 / 8) * MIB);
        assert_eq!(startup.throttle_rates, default_throttle_rates());
    }

    #[test]
    fn override_wins() {
        let hardware = Hardware {
            cores: 32,
            ram_bytes: 64 * 1024 * 1024 * 1024,
            vram: Some(Vram { local_bytes: 24 * 1024 * 1024 * 1024, budget_bytes: None }),
        };
        let mut registry = CvarRegistry::new();
        registry.set(MEMORY_MAX_RAM_MIB, Layer::Global, 768).unwrap();
        registry.set(RENDER_WORKER_THREADS, Layer::Global, 12).unwrap();
        registry.set(MEMORY_MAX_VRAM_MIB, Layer::Global, 1792).unwrap();
        registry.set(MEMORY_VRAM_DIVISOR, Layer::Global, 4).unwrap();
        registry.set(NETWORK_MAX_BANDWIDTH_KBPS, Layer::Global, 16_000).unwrap();
        registry.lock_startup_values();
        let snapshot = registry.snapshot();

        let startup = StartupLimits::resolve(hardware, &snapshot);
        assert_eq!(startup.assets.work_bytes, 512 * MIB);
        assert_eq!(startup.assets.texture_work_bytes, 192 * MIB);
        assert_eq!(startup.scene.memory_bytes, 256 * MIB);
        assert_eq!(startup.assets.work_bytes + startup.scene.memory_bytes, 768 * MIB);
        assert_eq!(startup.texture_jobs, 6);
        assert_eq!(startup.scene.mesh_jobs, 6);
        assert_eq!(startup.texture_staging_bytes, render::Limits::derive(&hardware).texture_staging_bytes);
        assert_eq!(startup.throttle_rates, throttle_rates_for_bandwidth_kbps(16_000));

        let render = startup.resolve_render(hardware.vram, &snapshot);
        assert_eq!(render.mesh_bytes, 64 * MIB);
        assert_eq!(render.texture_soft_cap_bytes, 1024 * MIB);
        assert_eq!(render.texture_staging_bytes, startup.texture_staging_bytes);
        assert_eq!(render.texture_jobs, 6);

        let mut larger_cap = CvarRegistry::new();
        larger_cap.set(MEMORY_MAX_RAM_MIB, Layer::Global, 2048).unwrap();
        larger_cap.set(RENDER_WORKER_THREADS, Layer::Global, 12).unwrap();
        larger_cap.lock_startup_values();
        let larger = StartupLimits::resolve(hardware, &larger_cap.snapshot());
        assert_eq!(larger.assets.work_bytes, 1536 * MIB);
        assert_eq!(larger.assets.texture_work_bytes, 576 * MIB);
        assert_eq!(larger.scene.memory_bytes, 512 * MIB);
        assert_eq!(larger.assets.work_bytes + larger.scene.memory_bytes, 2048 * MIB);

        let mut min_cap = CvarRegistry::new();
        min_cap.set(MEMORY_MAX_RAM_MIB, Layer::Global, 256).unwrap();
        min_cap.lock_startup_values();
        let min = StartupLimits::resolve(hardware, &min_cap.snapshot());
        assert!(min.assets.work_bytes > 0);
        assert!(min.scene.memory_bytes > 0);
        assert_eq!(min.assets.work_bytes + min.scene.memory_bytes, 256 * MIB);

        assert_eq!(clamp_i64_to_domain(MEMORY_MAX_VRAM_MIB, 50), 1024);
        assert_eq!(clamp_i64_to_domain(MEMORY_MAX_VRAM_MIB, 999_999), 65536);
        assert_eq!(parse_max_texture_resolution("512"), 512);
        assert_eq!(parse_max_texture_resolution("1024"), 1024);
        assert_eq!(parse_max_texture_resolution("2048"), 2048);
    }
}
