pub(crate) mod environment;
pub(crate) mod parcel;
pub(crate) mod terrain;

use crate::{RegionInfo, update::FieldStamp};

#[derive(Default)]
pub(crate) struct RegionState {
    pub(crate) info: RegionInfo,
    pub(crate) name_fence: Option<FieldStamp>,
    pub(crate) terrain: terrain::Terrain,
    pub(crate) environment: environment::Environments,
    pub(crate) parcels: parcel::ParcelState,
}

impl RegionState {
    pub(crate) fn reclaim_one(&mut self) -> bool {
        self.terrain.reclaim_one()
            || self.environment.reclaim_one()
            || self.parcels.reclaim_one()
            || self.info.name.take().is_some()
            || self.info.region_id.take().is_some()
            || self.info.handle.take().is_some()
            || self.info.origin.take().is_some()
            || self.name_fence.take().is_some()
    }
}
