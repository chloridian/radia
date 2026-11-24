use std::{
    num::NonZeroU32,
    sync::atomic::{AtomicU64, Ordering},
};

use math::DVec3;
use uuid::Uuid;

static NEXT_WORLD_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct Entity(pub(super) hecs::Entity, pub(super) WorldId);

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(super) struct WorldId(u64);

impl WorldId {
    pub(super) fn fresh() -> Self {
        let id = NEXT_WORLD_ID
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |current| current.checked_add(1))
            .unwrap_or_else(|_| panic!("World identity space exhausted"));
        Self(id)
    }

    pub(crate) const fn raw(self) -> u64 {
        self.0
    }

    pub(crate) const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Region {
    world: WorldId,
    generation: u64,
}

impl Region {
    pub(crate) const fn new(world: WorldId, generation: u64) -> Self {
        Self { world, generation }
    }

    pub const fn get(self) -> u64 {
        self.generation
    }

    pub(crate) const fn minimum() -> Self {
        Self { world: WorldId(0), generation: 0 }
    }

    pub(crate) const fn belongs_to(self, world: WorldId) -> bool {
        self.world.0 == world.0
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct LocalAddress {
    pub(super) region: Region,
    pub(super) local_id: NonZeroU32,
}

impl LocalAddress {
    pub const fn new(region: Region, local_id: NonZeroU32) -> Self {
        Self { region, local_id }
    }

    pub const fn region(self) -> Region {
        self.region
    }

    pub const fn local_id(self) -> NonZeroU32 {
        self.local_id
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ObjectBindingTarget {
    pub(super) entity: Entity,
    pub(super) id: Uuid,
    pub(super) address: LocalAddress,
    pub(super) address_generation: u64,
}

impl ObjectBindingTarget {
    pub const fn entity(self) -> Entity {
        self.entity
    }

    pub const fn object_id(self) -> Uuid {
        self.id
    }

    pub const fn address(self) -> LocalAddress {
        self.address
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct RegionHandle(u64);

impl RegionHandle {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }

    pub const fn origin(self) -> DVec3 {
        DVec3::new((self.0 >> 32) as u32 as f64, self.0 as u32 as f64, 0.0)
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct RegionInfo {
    pub region_id: Option<Uuid>,
    pub handle: Option<RegionHandle>,
    pub name: Option<String>,
    pub origin: Option<DVec3>,
}

impl RegionInfo {
    pub fn resolved_origin(&self) -> Option<DVec3> {
        self.origin.or_else(|| self.handle.map(RegionHandle::origin))
    }
}
