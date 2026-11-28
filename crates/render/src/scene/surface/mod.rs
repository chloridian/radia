mod region_water;
mod terrain;

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, Ordering},
    },
};

use assets::mesh::Mesh;
use math::DVec3;
use world::{Region, TerrainPatchId};

use self::terrain::Terrain;
pub(in crate::scene) use self::terrain::height_at;
use crate::scene::{Budget, MeshBuildError, PreparedMesh, SharedMesh, budget::MeshWork, mesh::retained_bytes, model::Mirror};

type Key = (SurfaceKind, Region);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, PartialOrd, Ord)]
pub enum SurfaceKind {
    RegionWater,
    Terrain(TerrainPatchId),
}

#[derive(Clone)]
pub struct Surface {
    pub region: Region,
    pub kind: SurfaceKind,
    pub position: DVec3,
    pub mesh: PreparedMesh,
    pub terrain_composition: Option<Arc<[f32]>>,
}

enum Input {
    Terrain(Terrain),
    RegionWater(f32),
}

pub(in crate::scene) struct SurfaceJob {
    key: Key,
    position: DVec3,
    input: Input,
    work: MeshWork,
    cancelled: Arc<AtomicBool>,
}

impl SurfaceJob {
    pub(in crate::scene) fn run(self) -> SurfaceCompletion {
        let result = utils::isolate(|| match &self.input {
            Input::Terrain(tiles) => {
                tiles.mesh(&self.cancelled).map(|(mesh, composition)| BuiltSurface { mesh, composition: Some(composition) })
            }
            Input::RegionWater(height) => region_water::mesh(*height).map(|mesh| BuiltSurface { mesh, composition: None }),
        })
        .unwrap_or(Err(MeshBuildError::Panicked));
        SurfaceCompletion { key: self.key, position: self.position, result, work: self.work, cancelled: self.cancelled }
    }
}

pub(in crate::scene) struct SurfaceCompletion {
    key: Key,
    position: DVec3,
    result: Result<BuiltSurface, MeshBuildError>,
    work: MeshWork,
    cancelled: Arc<AtomicBool>,
}

struct BuiltSurface {
    mesh: Mesh,
    composition: Option<Vec<f32>>,
}

pub(in crate::scene) enum SurfaceStep {
    Build(SurfaceJob),
    Remove(Region, SurfaceKind),
    Continue,
    Waiting,
    Idle,
}

pub(in crate::scene) struct SurfaceJobs {
    budget: Budget,
    dirty: BTreeSet<Key>,
    deferred: BTreeSet<Key>,
    running: BTreeMap<Key, Weak<AtomicBool>>,
    hydrating: BTreeSet<Region>,
}

impl SurfaceJobs {
    pub(in crate::scene) fn new(budget: Budget) -> Self {
        Self { budget, dirty: BTreeSet::new(), deferred: BTreeSet::new(), running: BTreeMap::new(), hydrating: BTreeSet::new() }
    }

    pub(in crate::scene) fn hydrating(&mut self, region: Region, pending: bool) {
        if pending {
            self.hydrating.insert(region);
        } else {
            self.hydrating.remove(&region);
            self.deferred.retain(|key| {
                if key.1 != region {
                    return true;
                }
                self.dirty.insert(*key);
                false
            });
        }
    }

    fn invalidate(&mut self, key: Key) {
        self.deferred.remove(&key);
        if let Some(cancelled) = self.running.remove(&key).and_then(|cancelled| cancelled.upgrade()) {
            cancelled.store(true, Ordering::Release);
        }
        self.dirty.insert(key);
    }

    pub(in crate::scene) fn patch(&mut self, mirror: &Mirror, region: Region, patch: TerrainPatchId) {
        for (region, patch) in terrain::affected_patches(mirror, region, patch) {
            self.invalidate((SurfaceKind::Terrain(patch), region));
        }
    }
    pub(in crate::scene) fn region_water(&mut self, region: Region) {
        self.invalidate((SurfaceKind::RegionWater, region));
    }

    pub(in crate::scene) fn terrain_settings(&mut self, mirror: &Mirror, region: Region) {
        for &(patch_region, patch) in mirror.patches.keys() {
            if patch_region == region {
                self.invalidate((SurfaceKind::Terrain(patch), region));
            }
        }
    }

    pub(in crate::scene) fn region_changed(
        &mut self,
        mirror: &Mirror,
        region: Region,
        old_origin: Option<DVec3>,
        new_origin: Option<DVec3>,
    ) {
        let moved = old_origin.is_some() && new_origin.is_some() && old_origin != new_origin;
        if moved {
            self.terrain_settings(mirror, region);
            if mirror.water.get(&region).copied().flatten().is_some() {
                self.region_water(region);
            }
        }
        for origin in [old_origin, new_origin].into_iter().flatten() {
            self.invalidate_adjacent_borders(mirror, region, origin);
        }
    }

    fn invalidate_adjacent_borders(&mut self, mirror: &Mirror, changed_region: Region, changed_origin: DVec3) {
        if !changed_origin.is_finite() {
            return;
        }
        let region_size = world::TERRAIN_REGION_SIZE as f64;
        for (&other_region, other_data) in &mirror.regions {
            if other_region == changed_region {
                continue;
            }
            let Some(other_origin) = other_data.summary().info.resolved_origin() else {
                continue;
            };
            let dx = (changed_origin.x - other_origin.x) / region_size;
            let dy = (changed_origin.y - other_origin.y) / region_size;
            if (dx != -1.0 && dx != 0.0 && dx != 1.0) || (dy != -1.0 && dy != 0.0 && dy != 1.0) || (dx == 0.0 && dy == 0.0) {
                continue;
            }
            let dx = dx as i32;
            let dy = dy as i32;
            let xs: &[u8] = match dx {
                -1 => &[0],
                1 => &[15],
                _ => &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
            };
            let ys: &[u8] = match dy {
                -1 => &[0],
                1 => &[15],
                _ => &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
            };
            for &x in xs {
                for &y in ys {
                    if let Some(patch) = TerrainPatchId::new(x, y)
                        && mirror.patches.contains_key(&(other_region, patch))
                    {
                        self.invalidate((SurfaceKind::Terrain(patch), other_region));
                    }
                }
            }
        }
    }

    pub(in crate::scene) fn retire(&mut self, region: Region) {
        self.hydrating.remove(&region);
        self.dirty.retain(|key| key.1 != region);
        self.deferred.retain(|key| key.1 != region);
        self.running.retain(|key, cancelled| {
            if key.1 != region {
                return true;
            }
            if let Some(cancelled) = cancelled.upgrade() {
                cancelled.store(true, Ordering::Release);
            }
            false
        });
    }

    pub(in crate::scene) fn start(&mut self, mirror: &Mirror) -> SurfaceStep {
        let (dirty, deferred, running) = (&mut self.dirty, &mut self.deferred, &mut self.running);
        running.retain(|key, cancelled| {
            if cancelled.upgrade().is_some() {
                return true;
            }
            deferred.remove(key);
            dirty.insert(*key);
            false
        });
        let Some(&key) = self.dirty.first() else {
            return if self.running.is_empty() && self.deferred.is_empty() { SurfaceStep::Idle } else { SurfaceStep::Waiting };
        };
        let origin = mirror.regions.get(&key.1).and_then(|region| region.summary().info.resolved_origin());
        let Some(origin) = origin.filter(|origin| origin.is_finite()) else {
            self.dirty.remove(&key);
            return SurfaceStep::Remove(key.1, key.0);
        };
        let (position, input) = match key.0 {
            SurfaceKind::Terrain(patch) => {
                let (x, y) = patch.coordinates();
                let tiles = Terrain::capture(mirror, key.1, patch, origin);
                if self.hydrating.contains(&key.1) && !tiles.complete() {
                    self.dirty.remove(&key);
                    self.deferred.insert(key);
                    return SurfaceStep::Continue;
                }
                (origin + DVec3::new(f64::from(x) * 16.0, f64::from(y) * 16.0, 0.0), Input::Terrain(tiles))
            }
            SurfaceKind::RegionWater => {
                let Some(height) = mirror.water.get(&key.1).copied().flatten() else {
                    self.dirty.remove(&key);
                    return SurfaceStep::Remove(key.1, key.0);
                };
                (origin, Input::RegionWater(height))
            }
        };
        let Some(work) = self.budget.mesh_work() else { return SurfaceStep::Waiting };
        let cancelled = Arc::new(AtomicBool::new(false));
        self.dirty.remove(&key);
        self.running.insert(key, Arc::downgrade(&cancelled));
        SurfaceStep::Build(SurfaceJob { key, position, input, work, cancelled })
    }

    pub(in crate::scene) fn complete(&mut self, mut completion: SurfaceCompletion) -> Option<Surface> {
        let cancelled = self.running.get(&completion.key)?;
        if !Weak::ptr_eq(cancelled, &Arc::downgrade(&completion.cancelled)) {
            return None;
        }
        self.running.remove(&completion.key);
        let (mesh, terrain_composition) = match completion.result {
            Ok(BuiltSurface { mesh, composition }) => {
                let composition_bytes = composition.as_ref().map_or(0, |values| size_of_val(values.as_slice()));
                let retained = retained_bytes(&mesh).checked_add(composition_bytes).expect("terrain allocation size is representable");
                let lease = completion.work.lease.split(retained).expect("terrain fits admitted work");
                (PreparedMesh::Ready(SharedMesh::new(mesh, lease)), composition.map(Arc::from))
            }
            Err(MeshBuildError::Cancelled) => {
                self.dirty.insert(completion.key);
                return None;
            }
            Err(error) => (PreparedMesh::Failed(error), None),
        };
        Some(Surface { region: completion.key.1, kind: completion.key.0, position: completion.position, mesh, terrain_composition })
    }
}

impl Drop for SurfaceJobs {
    fn drop(&mut self) {
        for cancelled in self.running.values() {
            if let Some(cancelled) = cancelled.upgrade() {
                cancelled.store(true, Ordering::Release);
            }
        }
    }
}
