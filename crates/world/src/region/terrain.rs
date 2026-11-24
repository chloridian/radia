use std::{
    array,
    collections::{
        BTreeMap,
        Bound::{Excluded, Unbounded},
    },
};

use uuid::Uuid;

use crate::{
    ApplyError, Region, RegionApplied, UpdateSource, World, WorldChange, WorldId, WorldReadError, WorldRevision, newer_field,
    update::FieldStamp,
};

pub const TERRAIN_PATCH_SIZE: usize = 16;
pub const TERRAIN_PATCH_SAMPLES: usize = TERRAIN_PATCH_SIZE * TERRAIN_PATCH_SIZE;
pub const TERRAIN_REGION_SIZE: usize = 256;
pub const MAX_TERRAIN_PATCHES: usize = 65_536;
pub const MAX_TERRAIN_PAGE_PATCHES: usize = 8;

const DEFAULT_DETAIL_TEXTURES: [Uuid; 4] = [
    Uuid::from_u128(0x0bc5822874a07e8389bc5c23464bcec5),
    Uuid::from_u128(0x63338ede0037c4fd855b015d77112fc8),
    Uuid::from_u128(0x303cd3818560757923f1f0a880799740),
    Uuid::from_u128(0x53a2f40648951d13d541d2e3b86bc19c),
];

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TerrainSettings {
    detail_textures: [Uuid; 4],
    start_heights: [f32; 4],
    height_ranges: [f32; 4],
}

impl TerrainSettings {
    pub fn new(detail_textures: [Uuid; 4], start_heights: [f32; 4], height_ranges: [f32; 4]) -> Result<Self, TerrainSettingsError> {
        if !start_heights.iter().all(|height| height.is_finite()) {
            return Err(TerrainSettingsError::NonFiniteStartHeight);
        }
        if !height_ranges.iter().all(|range| range.is_finite() && *range > 0.0) {
            return Err(TerrainSettingsError::InvalidHeightRange);
        }
        Ok(Self {
            detail_textures: array::from_fn(|index| {
                if detail_textures[index].is_nil() { DEFAULT_DETAIL_TEXTURES[index] } else { detail_textures[index] }
            }),
            start_heights,
            height_ranges,
        })
    }

    pub fn detail_textures(&self) -> &[Uuid; 4] {
        &self.detail_textures
    }

    pub fn start_heights(&self) -> &[f32; 4] {
        &self.start_heights
    }

    pub fn height_ranges(&self) -> &[f32; 4] {
        &self.height_ranges
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerrainSettingsError {
    NonFiniteStartHeight,
    InvalidHeightRange,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TerrainPatchId(u8);

impl TerrainPatchId {
    pub fn new(x: u8, y: u8) -> Option<Self> {
        if x < 16 && y < 16 { Some(Self(y * 16 + x)) } else { None }
    }
    pub fn coordinates(self) -> (u8, u8) {
        (self.0 % 16, self.0 / 16)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerrainPatchError {
    UnsupportedCoordinates,
    NonFinite,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TerrainPatch {
    id: TerrainPatchId,
    heights: Box<[f32; TERRAIN_PATCH_SAMPLES]>,
}

impl TerrainPatch {
    pub fn new(x: u8, y: u8, heights: Box<[f32; TERRAIN_PATCH_SAMPLES]>) -> Result<Self, TerrainPatchError> {
        let id = TerrainPatchId::new(x, y).ok_or(TerrainPatchError::UnsupportedCoordinates)?;
        if !heights.iter().all(|height| height.is_finite()) {
            return Err(TerrainPatchError::NonFinite);
        }
        Ok(Self { id, heights })
    }
    pub fn id(&self) -> TerrainPatchId {
        self.id
    }
    pub fn heights(&self) -> &[f32; TERRAIN_PATCH_SAMPLES] {
        &self.heights
    }
}

struct ObservedPatch {
    patch: TerrainPatch,
    fence: FieldStamp,
}

#[derive(Default)]
pub(crate) struct Terrain {
    patches: BTreeMap<TerrainPatchId, ObservedPatch>,
    water: Option<(f32, FieldStamp)>,
    settings: Option<(TerrainSettings, FieldStamp)>,
}

impl Terrain {
    pub(crate) fn patch_count(&self) -> usize {
        self.patches.len()
    }
    pub(crate) fn settings(&self) -> Option<TerrainSettings> {
        self.settings.map(|(settings, _)| settings)
    }

    pub(crate) fn water_height(&self) -> Option<f32> {
        self.water.map(|(height, _)| height)
    }

    pub(crate) fn reclaim_one(&mut self) -> bool {
        if self.patches.pop_first().is_some() || self.water.take().is_some() { true } else { self.settings.take().is_some() }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TerrainPageCursor {
    world: WorldId,
    region: Region,
    after: TerrainPatchId,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TerrainPage {
    pub region: Region,
    pub hydration_revision: WorldRevision,
    pub dimensions: [u16; 2],
    pub known_patches: usize,
    pub water_height: Option<f32>,
    pub patches: Vec<TerrainPatch>,
    pub next: Option<TerrainPageCursor>,
}

impl World {
    pub fn observe_terrain_patch(&mut self, source: UpdateSource, patch: TerrainPatch) -> Result<RegionApplied, ApplyError> {
        let region = source.region();
        self.require_active_region(region)?;
        let Some(stamp) = source.stamp() else { return Ok(RegionApplied::unchanged()) };
        let fence = FieldStamp { authority: source.authority(), stamp };
        let current = self.regions[&region].terrain.patches.get(&patch.id);
        if !newer_field(fence, current.map(|current| current.fence)) {
            return Ok(RegionApplied::unchanged());
        }
        if current.is_some_and(|current| current.patch == patch) {
            self.regions.get_mut(&region).expect("active region").terrain.patches.get_mut(&patch.id).expect("known patch").fence = fence;
            return Ok(RegionApplied::unchanged());
        }
        let new_patch = current.is_none();
        if new_patch && self.terrain_patches >= MAX_TERRAIN_PATCHES {
            return Err(ApplyError::TerrainBudgetExhausted);
        }
        self.ensure_revision_available()?;
        let id = patch.id;
        self.regions.get_mut(&region).expect("active region").terrain.patches.insert(id, ObservedPatch { patch, fence });
        self.terrain_patches += usize::from(new_patch);
        Ok(self.publish_region_patch(WorldChange::TerrainPatchUpdated { region, patch: id }))
    }
    pub fn observe_water_height(&mut self, source: UpdateSource, height: f32) -> Result<RegionApplied, ApplyError> {
        let region = source.region();
        self.require_active_region(region)?;
        if !height.is_finite() {
            return Err(ApplyError::InvalidValue { field: "water_height" });
        }
        let Some(stamp) = source.stamp() else { return Ok(RegionApplied::unchanged()) };
        let fence = FieldStamp { authority: source.authority(), stamp };
        let current = self.regions[&region].terrain.water;
        if !newer_field(fence, current.map(|(_, fence)| fence)) {
            return Ok(RegionApplied::unchanged());
        }
        if current.is_some_and(|(current, _)| current == height) {
            self.regions.get_mut(&region).expect("active region").terrain.water = Some((height, fence));
            return Ok(RegionApplied::unchanged());
        }
        self.ensure_revision_available()?;
        self.regions.get_mut(&region).expect("active region").terrain.water = Some((height, fence));
        Ok(self.publish_region_patch(WorldChange::WaterHeightUpdated(region)))
    }
    pub fn observe_terrain_settings(&mut self, source: UpdateSource, settings: TerrainSettings) -> Result<RegionApplied, ApplyError> {
        let region = source.region();
        self.require_active_region(region)?;
        let Some(stamp) = source.stamp() else { return Ok(RegionApplied::unchanged()) };
        let fence = FieldStamp { authority: source.authority(), stamp };
        let current = self.regions[&region].terrain.settings;
        if !newer_field(fence, current.map(|(_, fence)| fence)) {
            return Ok(RegionApplied::unchanged());
        }
        if current.is_some_and(|(current, _)| current == settings) {
            self.regions.get_mut(&region).expect("active region").terrain.settings = Some((settings, fence));
            return Ok(RegionApplied::unchanged());
        }
        self.ensure_revision_available()?;
        self.regions.get_mut(&region).expect("active region").terrain.settings = Some((settings, fence));
        Ok(self.publish_region_patch(WorldChange::RegionUpdated(region)))
    }
    pub fn terrain_page(
        &self,
        region: Region,
        cursor: Option<TerrainPageCursor>,
        limit: usize,
    ) -> Result<Option<TerrainPage>, WorldReadError> {
        if limit == 0 || limit > MAX_TERRAIN_PAGE_PATCHES {
            return Err(WorldReadError::InvalidLimit);
        }
        if let Some(cursor) = cursor {
            if cursor.world != self.id {
                return Err(WorldReadError::StaleSession);
            }
            if cursor.region != region {
                return Err(WorldReadError::InvalidCursor);
            }
        }
        let Some(state) = self.regions.get(&region) else { return Ok(None) };
        let start = cursor.map_or(Unbounded, |cursor| Excluded(cursor.after));
        let mut iter = state.terrain.patches.range((start, Unbounded));
        let mut patches = Vec::with_capacity(limit);
        for (_, observed) in iter.by_ref().take(limit) {
            patches.push(observed.patch.clone());
        }
        let next = if iter.next().is_some() {
            patches.last().map(|patch| TerrainPageCursor { world: self.id, region, after: patch.id })
        } else {
            None
        };
        Ok(Some(TerrainPage {
            region,
            hydration_revision: self.revision(),
            dimensions: [TERRAIN_REGION_SIZE as u16; 2],
            known_patches: state.terrain.patches.len(),
            water_height: state.terrain.water.map(|(height, _)| height),
            patches,
            next,
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::assert_matches;

    use proptest::collection;

    use super::*;
    use crate::{OrderEvidence, RegionInfo, UpdateAuthority, WorldLimits};
    fn world() -> (World, Region) {
        let mut world = World::new(WorldLimits {
            max_regions: 3,
            max_entities: 8,
            max_entities_per_region: 8,
            max_tombstones_per_region: 8,
            max_tombstones: 8,
            max_tombstone_bytes_per_region: WorldLimits::tombstone_bytes_for_count(8),
            max_tombstone_bytes: WorldLimits::tombstone_bytes_for_count(8),
            max_journal_entries: 64,
            max_journal_changes: 128,
            max_region_name_bytes: 32,
            max_recovery_targets: 4,
        });
        let region = world.register_region().unwrap();
        (world, region)
    }
    fn source(region: Region, order: u32) -> UpdateSource {
        UpdateSource::new(region, order, OrderEvidence::Ordered(u64::from(order)), 0)
    }
    fn patch(x: u8, y: u8, height: f32) -> TerrainPatch {
        TerrainPatch::new(x, y, Box::new([height; TERRAIN_PATCH_SAMPLES])).unwrap()
    }
    fn page(world: &World, region: Region) -> TerrainPage {
        world.terrain_page(region, None, MAX_TERRAIN_PAGE_PATCHES).unwrap().unwrap()
    }
    #[test]
    fn unknown_differs_from_zero() {
        let (mut world, region) = world();
        let before = page(&world, region);
        assert_eq!(before.dimensions, [256, 256]);
        assert_eq!(before.known_patches, 0);
        assert_eq!(before.water_height, None);
        assert!(before.patches.is_empty());
        let barrier = world.read_barrier();
        let _ = world.observe_terrain_patch(source(region, 1), patch(3, 5, 0.0)).unwrap();
        let _ = world.observe_water_height(source(region, 1), 0.0).unwrap();
        let after = page(&world, region);
        assert_eq!(after.known_patches, 1);
        assert_eq!(after.water_height, Some(0.0));
        assert_eq!(after.patches[0].id().coordinates(), (3, 5));
        assert_eq!(after.patches[0].heights(), &[0.0; TERRAIN_PATCH_SAMPLES]);
        let changes = world.change_page(barrier.changes, 8).unwrap();
        assert_eq!(
            changes.records.iter().map(|record| record.change).collect::<Vec<_>>(),
            [
                WorldChange::TerrainPatchUpdated { region, patch: TerrainPatchId::new(3, 5).unwrap() },
                WorldChange::WaterHeightUpdated(region),
            ]
        );
        let _ = world.patch_region(region, RegionInfo { name: Some("region".into()), ..RegionInfo::default() }).unwrap();
        assert_eq!(page(&world, region).patches, after.patches);
        assert_eq!(page(&world, region).water_height, after.water_height);
    }
    #[test]
    fn source_fences_handle_ordering() {
        let (mut world, region) = world();
        let _ = world.observe_terrain_patch(source(region, 10), patch(0, 0, 1.0)).unwrap();
        let _ = world.observe_water_height(source(region, 10), 1.0).unwrap();
        let revision = world.revision();
        let _ = world.observe_terrain_patch(source(region, 20), patch(0, 0, 1.0)).unwrap();
        let _ = world.observe_water_height(source(region, 20), 1.0).unwrap();
        assert_eq!(world.revision(), revision);
        for source in [
            source(region, 15),
            source(region, 30).with_authority(UpdateAuthority::Untracked),
            UpdateSource::new(region, 30, OrderEvidence::Ambiguous, 0),
            UpdateSource::new(region, 30, OrderEvidence::Degraded, 0),
        ] {
            let _ = world.observe_terrain_patch(source, patch(0, 0, 2.0)).unwrap();
            let _ = world.observe_water_height(source, 2.0).unwrap();
        }
        assert_eq!(world.revision(), revision);
        let later_block = UpdateSource::new(region, 20, OrderEvidence::Ordered(20), 1);
        let _ = world.observe_terrain_patch(later_block, patch(0, 0, 3.0)).unwrap();
        assert_eq!(page(&world, region).patches[0].heights()[0], 3.0);
        assert_eq!(world.terrain_patches, 1);
    }
    #[test]
    fn page_cursors_respect_lifetime() {
        let (mut world, region) = world();
        for x in 0..10 {
            let _ = world.observe_terrain_patch(source(region, u32::from(x) + 1), patch(x, 0, f32::from(x))).unwrap();
        }
        let first = page(&world, region);
        assert_eq!(first.patches.len(), 8);
        let next = world.terrain_page(region, first.next, 8).unwrap().unwrap();
        assert_eq!(next.patches.len(), 2);
        assert_eq!(next.patches[0].id().coordinates(), (8, 0));
        assert!(next.next.is_none());
        let other_region = world.register_region().unwrap();
        let (stale_world, stale_region) = self::world();
        assert_eq!(world.terrain_page(other_region, first.next, 8), Err(WorldReadError::InvalidCursor));
        assert_eq!(stale_world.terrain_page(stale_region, first.next, 8), Err(WorldReadError::StaleSession));
        assert_eq!(world.terrain_page(region, None, 9), Err(WorldReadError::InvalidLimit));
        let _ = world.retire_region(region).unwrap();
        assert_eq!(world.terrain_patches, 10);
        while world.has_pending_removals() {
            let progress = world.reclaim_removals(crate::MAX_REMOVAL_WORK_PER_TURN);
            assert!(progress.work_units <= crate::MAX_REMOVAL_WORK_PER_TURN);
        }
        assert_eq!(world.terrain_patches, 0);
        assert_eq!(world.terrain_page(region, first.next, 8), Ok(None));
        assert_eq!(world.observe_terrain_patch(source(region, 100), patch(0, 0, 9.0)), Err(ApplyError::RegionRetired));
        assert_eq!(world.observe_water_height(source(region, 100), 9.0), Err(ApplyError::RegionRetired));
        assert_eq!(world.terrain_page(other_region, None, 8).unwrap().unwrap().water_height, None);
        assert_eq!(first.patches[0].heights()[0], 0.0);
    }
    #[test]
    fn nonfinite_rejected_exhaustion_rolls_back() {
        assert_eq!(TerrainPatch::new(16, 0, Box::new([0.0; TERRAIN_PATCH_SAMPLES])), Err(TerrainPatchError::UnsupportedCoordinates));
        assert_eq!(TerrainPatch::new(0, 0, Box::new([f32::NAN; TERRAIN_PATCH_SAMPLES])), Err(TerrainPatchError::NonFinite));
        let (mut world, region) = world();
        let _ = world.observe_terrain_patch(source(region, 1), patch(0, 0, 1.0)).unwrap();
        assert!(world.observe_water_height(source(region, 1), f32::INFINITY).is_err());
        world.journal.revision = WorldRevision(u64::MAX);
        assert_eq!(world.observe_terrain_patch(source(region, 2), patch(0, 0, 2.0)), Err(ApplyError::RevisionExhausted));
        assert_eq!(world.observe_water_height(source(region, 2), 2.0), Err(ApplyError::RevisionExhausted));
        assert_eq!(page(&world, region).patches[0].heights()[0], 1.0);
        assert_eq!(page(&world, region).water_height, None);
        assert_eq!(world.terrain_patches, 1);
    }
    #[test]
    fn null_ids_use_default_textures() {
        let settings = TerrainSettings::new([Uuid::nil(); 4], [0.0; 4], [1.0; 4]).unwrap();
        assert_eq!(settings.detail_textures, DEFAULT_DETAIL_TEXTURES);
        assert_eq!(TerrainSettings::new([Uuid::nil(); 4], [f32::NAN; 4], [1.0; 4]), Err(TerrainSettingsError::NonFiniteStartHeight));
        assert_eq!(TerrainSettings::new([Uuid::nil(); 4], [0.0; 4], [0.0; 4]), Err(TerrainSettingsError::InvalidHeightRange));
        assert_eq!(TerrainSettings::new([Uuid::nil(); 4], [0.0; 4], [f32::INFINITY; 4]), Err(TerrainSettingsError::InvalidHeightRange));
    }
    #[test]
    fn settings_follow_region_lifetime() {
        let (mut world, region) = world();
        let first = TerrainSettings::new([Uuid::nil(); 4], [0.0; 4], [10.0; 4]).unwrap();
        let second = TerrainSettings::new([Uuid::nil(); 4], [1.0; 4], [20.0; 4]).unwrap();
        let barrier = world.read_barrier();
        let _ = world.observe_terrain_settings(source(region, 2), first).unwrap();
        assert_eq!(world.region_page(None, 8).unwrap().regions[0].terrain_settings, Some(first));
        let revision = world.revision();
        let _ = world.observe_terrain_settings(source(region, 1), second).unwrap();
        assert_eq!(world.revision(), revision);
        let _ = world.observe_terrain_settings(source(region, 3), second).unwrap();
        assert_eq!(world.region_page(None, 8).unwrap().regions[0].terrain_settings, Some(second));
        assert_matches!(
            world.change_page(barrier.changes, 8).unwrap().records.last().unwrap().change,
            WorldChange::RegionUpdated(updated) if updated == region
        );
        let _ = world.retire_region(region).unwrap();
        assert_eq!(world.observe_terrain_settings(source(region, 4), first), Err(ApplyError::RegionRetired));
        assert!(world.region_page(None, 8).unwrap().regions.is_empty());
    }
    #[test]
    fn global_capacity_releases_on_retirement() {
        let (mut world, first_region) = world();
        world.limits.max_regions = 257;
        let mut regions = vec![first_region];
        for _ in 1..256 {
            regions.push(world.register_region().unwrap());
        }
        for region in regions {
            let source = UpdateSource::new(region, 1, OrderEvidence::Ordered(1), 0);
            for y in 0..16 {
                for x in 0..16 {
                    let _ = world.observe_terrain_patch(source, patch(x, y, 0.0)).unwrap();
                }
            }
        }
        assert_eq!(world.terrain_patches, MAX_TERRAIN_PATCHES);
        let region = world.register_region().unwrap();
        let target_source = UpdateSource::new(region, 1, OrderEvidence::Ordered(1), 0);
        assert_eq!(world.observe_terrain_patch(target_source, patch(0, 0, 1.0)), Err(ApplyError::TerrainBudgetExhausted));
        let _ = world.observe_terrain_patch(source(first_region, 2), patch(0, 0, 2.0)).unwrap();
        assert_eq!(page(&world, first_region).patches[0].heights()[0], 2.0);
        let _ = world.retire_region(first_region).unwrap();
        assert_eq!(world.observe_terrain_patch(target_source, patch(0, 0, 1.0)), Err(ApplyError::TerrainBudgetExhausted));
        while world.has_pending_removals() {
            let progress = world.reclaim_removals(crate::MAX_REMOVAL_WORK_PER_TURN);
            assert!(progress.work_units <= crate::MAX_REMOVAL_WORK_PER_TURN);
        }
        let _ = world.observe_terrain_patch(target_source, patch(0, 0, 1.0)).unwrap();
        assert_eq!(world.terrain_patches, MAX_TERRAIN_PATCHES - 255);
    }
    proptest::proptest! {
        #[test]
        fn reordered_patches_keep_newest(
            updates in collection::vec((0u32..200, 0u8..16, -100i16..100), 0..100)
        ) {
            let (mut world, region) = world();
            let mut expected = BTreeMap::<u8, (u32, f32)>::new();
            for (order, x, height) in updates {
                let height = f32::from(height);
                let _ = world.observe_terrain_patch(source(region, order), patch(x, 0, height)).unwrap();
                if expected.get(&x).is_none_or(|(previous, _)| order > *previous) {
                    expected.insert(x, (order, height));
                }
                let mut cursor = None;
                let mut observed = BTreeMap::new();
                loop {
                    let page = world.terrain_page(region, cursor, 8).unwrap().unwrap();
                    for patch in page.patches {
                        observed.insert(patch.id().coordinates().0, patch.heights()[0]);
                    }
                    cursor = page.next;
                    if cursor.is_none() { break; }
                }
                proptest::prop_assert_eq!(observed, expected.iter().map(|(x, (_, height))| (*x, *height)).collect());
                proptest::prop_assert_eq!(world.terrain_patches, expected.len());
            }
        }
    }
}
