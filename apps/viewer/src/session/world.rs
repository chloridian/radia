use std::{
    collections::{HashMap, VecDeque},
    time::{Duration, Instant},
};

use network::SimulatorId;
use protocol::messages::CacheMissType;
use utils::Summary;
use uuid::Uuid;
use world::{
    Appearance, Applied, ApplyError,
    ApplyOutcome::{AgentHandedOff, Deferred, Introduced, TombstoneRecorded},
    Entity, EnvironmentAssignment, EnvironmentTarget, FieldRejectionReason, Kind, LocalAddress, MAX_OBJECT_LOOKUP_WORK_ITEMS,
    ObjectLookupCursor, ObjectLookupProgress, ObjectMediaUpdate, ObjectObservation, ObjectPatch, OrderEvidence, ParcelLocalId,
    ParcelOverlayObservation, ParcelPropertiesObservation, ParentResolutionCursor, ParentResolutionPlan, ParentResolutionProgress,
    ParentUpdate, Region, RegionApplied, RegionHandle, RegionInfo, RegionOutcome, TerrainSettings, UpdateSource, World, WorldLimits,
    WorldRevision,
};

use super::{
    inbound::{AvatarAppearanceMessage, ObjectBatch, ObjectMediaRequest, ObjectOperation, SitTargetUpdate},
    object_cache::CachedOverride,
    object_media::ObjectMediaTarget,
    terrain::DecodedTerrain,
};

pub(super) const MAX_WORLD_ENTITIES: usize = 65_536;
const MAX_WORLD_TOMBSTONES_PER_REGION: usize = 4_096;
const MAX_WORLD_TOMBSTONES: usize = 16_384;
const MAX_WORLD_JOURNAL_ENTRIES: usize = 4_096;
const MAX_REGION_NAME_BYTES: usize = 256;
const MAX_INGEST_DIAGNOSTIC_ADDRESSES: usize = 8;
pub(super) const MAX_OBJECTS_PER_INTAKE_QUANTUM: usize = 32;
const MAX_RELATIONSHIPS_PER_TURN: usize = 64;
const PARCEL_REJECTION_LOG_INTERVAL: u64 = 64;
const DEGRADED_UPDATE_REPORT_INTERVAL: Duration = Duration::from_secs(10);
const REJECTED_VALUE_REPORT_INTERVAL: Duration = Duration::from_secs(30);
const OVERRIDE_FAILURE_REPORT_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum IngestStatus {
    Applied,
    RegionHandleMismatch,
    RegionContextMismatch,
    RegionMetadataRejected(ApplyError),
}

#[derive(Debug)]
pub(super) enum RegistrationError {
    HandleChanged,
    Apply(ApplyError),
}

pub(super) enum ObjectMediaPreparationProgress {
    Pending(ObjectLookupCursor),
    Complete { target: Option<ObjectMediaTarget>, work_units: usize },
}

#[derive(Default)]
pub(super) struct ObjectMediaTargetCheck {
    revision: Option<WorldRevision>,
    entity_cursor: Option<ObjectLookupCursor>,
    address_cursor: Option<ObjectLookupCursor>,
    stage: ObjectMediaTargetCheckStage,
}

#[derive(Clone, Copy, Default)]
enum ObjectMediaTargetCheckStage {
    #[default]
    Entity,
    Address,
    Complete,
}

pub(super) enum ObjectMediaTargetCheckProgress {
    Pending,
    Complete { current: bool, work_units: usize },
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum BlockOutcome {
    Invalid,
    RecoveryRequired { address: LocalAddress, cache_miss_type: CacheMissType },
    Applied { address: Option<LocalAddress>, applied: Applied, recovered: Option<LocalAddress> },
    Rejected { address: Option<LocalAddress>, error: ApplyError },
}

impl BlockOutcome {
    pub(super) fn applied(&self) -> Option<&Applied> {
        match self {
            Self::Applied { applied, .. } => Some(applied),
            _ => None,
        }
    }

    fn address(&self) -> Option<LocalAddress> {
        match self {
            Self::RecoveryRequired { address, .. } => Some(*address),
            Self::Applied { address, .. } | Self::Rejected { address, .. } => *address,
            Self::Invalid => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct IngestSummary {
    pub(super) status: IngestStatus,
    pub(super) accepted_region_handle: Option<RegionHandle>,
    pub(super) blocks: Vec<BlockOutcome>,
    pub(super) invalid_blocks: usize,
    pub(super) invalid_fields: usize,
}

impl IngestSummary {
    pub(super) fn new(status: IngestStatus) -> Self {
        Self { status, accepted_region_handle: None, blocks: Vec::new(), invalid_blocks: 0, invalid_fields: 0 }
    }

    pub(super) fn push_applied(&mut self, applied: Applied, address: Option<LocalAddress>) {
        self.blocks.push(BlockOutcome::Applied { address, applied, recovered: None });
    }

    pub(super) fn push_error(&mut self, error: ApplyError, address: Option<LocalAddress>) {
        self.blocks.push(BlockOutcome::Rejected { address, error });
    }

    pub(super) fn touches_entity(&self, entity: world::Entity) -> bool {
        self.blocks.iter().filter_map(BlockOutcome::applied).any(|applied| {
            matches!(
                &applied.outcome,
                world::ApplyOutcome::Introduced(updated)
                    | world::ApplyOutcome::Updated(updated)
                    | world::ApplyOutcome::AgentHandedOff(updated)
                    if *updated == entity
            )
        })
    }

    pub(super) fn append(&mut self, other: Self) {
        self.blocks.extend(other.blocks);
        self.invalid_blocks = self.invalid_blocks.saturating_add(other.invalid_blocks);
        self.invalid_fields = self.invalid_fields.saturating_add(other.invalid_fields);
        self.accepted_region_handle = self.accepted_region_handle.or(other.accepted_region_handle);
        if other.status != IngestStatus::Applied {
            self.status = other.status;
        }
    }

    #[cfg(test)]
    pub(super) fn recovery_targets(&self) -> impl Iterator<Item = LocalAddress> + '_ {
        self.recovery_requests().map(|(address, _)| address)
    }

    pub(super) fn recovery_requests(&self) -> impl Iterator<Item = (LocalAddress, CacheMissType)> + '_ {
        self.blocks.iter().flat_map(|block| {
            let required = match block {
                BlockOutcome::RecoveryRequired { address, cache_miss_type } => Some((*address, *cache_miss_type)),
                _ => None,
            };
            required.into_iter().chain(
                block
                    .applied()
                    .into_iter()
                    .flat_map(|applied| applied.recovery.requests.iter().copied().map(|address| (address, CacheMissType::Full))),
            )
        })
    }

    #[cfg(test)]
    pub(super) fn recovery_cancellation_addresses(&self) -> impl Iterator<Item = LocalAddress> + '_ {
        self.recovery_cancellations().map(|(_, address)| address)
    }

    pub(super) fn recovery_cancellations(&self) -> impl Iterator<Item = (Option<Entity>, LocalAddress)> + '_ {
        self.blocks
            .iter()
            .filter_map(|block| {
                let BlockOutcome::Applied { applied, recovered, .. } = block else { return None };
                let (entity, address) = match &applied.outcome {
                    TombstoneRecorded(address) => (None, Some(*address)),
                    world::ApplyOutcome::Introduced(entity) | world::ApplyOutcome::Updated(entity) | AgentHandedOff(entity) => {
                        (Some(*entity), *recovered)
                    }
                    _ => (None, *recovered),
                };
                Some((applied, entity, address))
            })
            .flat_map(|(result, recovered_entity, address)| {
                result
                    .recovery_cancellations
                    .iter()
                    .map(|cancellation| (Some(cancellation.entity()), cancellation.address()))
                    .chain(address.map(|address| (recovered_entity, address)))
            })
    }

    fn rejections(&self) -> IngestRejections {
        use ApplyError::{
            AddressGenerationExhausted, EntityBudgetExhausted, EnvironmentBudgetExhausted, FrameGenerationExhausted, InvalidValue,
            KindMismatch, RegionBudgetExhausted, RegionGenerationExhausted, RegionRetired, RemovalCapacityExhausted, RevisionExhausted,
            StaleRegion, TerrainBudgetExhausted,
        };
        use FieldRejectionReason::{
            InvalidRotation, InvalidValue as RejectedValue, ParentCycle, ParentDependsOnRejectedParent, ResourceLimit, StaleFrame,
            StaleVersion, UntrustedSource,
        };

        if let IngestStatus::RegionMetadataRejected(error) = self.status {
            return IngestRejections { region_metadata: Some(error), ..IngestRejections::default() };
        }
        let region_mismatch = matches!(self.status, IngestStatus::RegionHandleMismatch | IngestStatus::RegionContextMismatch);
        let mut malformed = (self.invalid_blocks as u64).saturating_add(self.invalid_fields as u64);
        let mut resource_limits = 0_u64;
        let mut rejected_addresses = [None; MAX_INGEST_DIAGNOSTIC_ADDRESSES];
        let mut rejected_address_count = 0;
        for block in &self.blocks {
            let mut rejected = false;
            match block {
                BlockOutcome::Rejected { error: InvalidValue { .. } | KindMismatch { .. }, .. } => {
                    malformed = malformed.saturating_add(1);
                    rejected = true;
                }
                BlockOutcome::Rejected {
                    error:
                        EntityBudgetExhausted
                        | TerrainBudgetExhausted
                        | EnvironmentBudgetExhausted
                        | world::ApplyError::ParcelBudgetExhausted
                        | RegionBudgetExhausted
                        | RegionGenerationExhausted
                        | RemovalCapacityExhausted
                        | RevisionExhausted
                        | FrameGenerationExhausted
                        | AddressGenerationExhausted,
                    ..
                } => {
                    resource_limits = resource_limits.saturating_add(1);
                    rejected = true;
                }
                BlockOutcome::Rejected { error: StaleRegion | RegionRetired, .. } => {}
                BlockOutcome::Applied { applied, .. } => {
                    if matches!(&applied.outcome, Deferred(world::DeferredReason::RelationshipCapacity)) {
                        resource_limits = resource_limits.saturating_add(1);
                        rejected = true;
                    }
                    for rejection in applied.rejections.iter() {
                        match rejection.reason {
                            RejectedValue { .. } | InvalidRotation | ParentCycle | ParentDependsOnRejectedParent => {
                                malformed = malformed.saturating_add(1);
                                rejected = true;
                            }
                            ResourceLimit { .. } => {
                                resource_limits = resource_limits.saturating_add(1);
                                rejected = true;
                            }
                            StaleFrame | StaleVersion | UntrustedSource => {}
                        }
                    }
                }
                BlockOutcome::Invalid | BlockOutcome::RecoveryRequired { .. } => {}
            }
            if rejected
                && rejected_address_count < MAX_INGEST_DIAGNOSTIC_ADDRESSES
                && let Some(address) = block.address()
            {
                rejected_addresses[rejected_address_count] = Some(address);
                rejected_address_count += 1;
            }
        }
        IngestRejections { region_metadata: None, region_mismatch, malformed, resource_limits, addresses: rejected_addresses }
    }
}

#[derive(Default)]
struct IngestRejections {
    region_metadata: Option<ApplyError>,
    region_mismatch: bool,
    malformed: u64,
    resource_limits: u64,
    addresses: [Option<LocalAddress>; MAX_INGEST_DIAGNOSTIC_ADDRESSES],
}

#[derive(Clone, Copy)]
pub(super) enum ValueRejection {
    Sit,
    SitError,
    Appearance,
    AppearanceError,
}

#[derive(Clone, Copy)]
struct RejectionSample {
    simulator: SimulatorId,
    region: Option<Region>,
    addresses: [Option<LocalAddress>; MAX_INGEST_DIAGNOSTIC_ADDRESSES],
}

impl RejectionSample {
    fn local_ids(&self) -> Vec<u32> {
        self.addresses.iter().flatten().map(|address| address.local_id().get()).collect()
    }
}

#[derive(Default)]
struct DegradedUpdates {
    region_mismatch: u64,
    resource_limits: u64,
    region_metadata: u64,
    malformed: u64,
    sample: Option<RejectionSample>,
    last_error: Option<ApplyError>,
}

impl DegradedUpdates {
    fn report(self) {
        let Some(sample) = self.sample else { return };
        tracing::warn!(
            region_mismatch = self.region_mismatch,
            resource_limits = self.resource_limits,
            region_metadata = self.region_metadata,
            malformed = self.malformed,
            sample_simulator = sample.simulator.get(),
            sample_region = sample.region.map(|region| region.get()),
            sample_local_ids = ?sample.local_ids(),
            last_error = self.last_error.map(tracing::field::display),
            "world updates were partially rejected; world data may be incomplete"
        );
    }
}

#[derive(Default)]
struct RejectedValues {
    malformed: u64,
    sit: u64,
    sit_error: u64,
    appearance: u64,
    appearance_error: u64,
    terrain: u64,
    sample: Option<RejectionSample>,
    last_error: Option<ApplyError>,
}

impl RejectedValues {
    fn report(self) {
        let Some(sample) = self.sample else { return };
        tracing::debug!(
            malformed = self.malformed,
            sit = self.sit,
            sit_error = self.sit_error,
            appearance = self.appearance,
            appearance_error = self.appearance_error,
            terrain = self.terrain,
            sample_simulator = sample.simulator.get(),
            sample_region = sample.region.map(|region| region.get()),
            sample_local_ids = ?sample.local_ids(),
            last_error = self.last_error.map(tracing::field::display),
            "world updates contained rejected values"
        );
    }
}

#[derive(Default)]
struct OverrideFailures {
    failed: u64,
    last_error: Option<ApplyError>,
}

impl OverrideFailures {
    fn report(self) {
        tracing::warn!(
            failed = self.failed,
            last_error = self.last_error.map(tracing::field::display),
            "cached PBR overrides not restored"
        );
    }
}

pub(super) struct SessionWorld {
    world: World,
    regions: HashMap<SimulatorId, Region>,
    parcel_rejections: u64,
    degraded_updates: Summary<DegradedUpdates>,
    rejected_values: Summary<RejectedValues>,
    override_failures: Summary<OverrideFailures>,
}

#[derive(Clone, Copy)]
enum BatchStage {
    Operations,
    Cancellations,
}

#[derive(Clone, Copy)]
enum ObjectLookupTarget {
    Id(Uuid),
    Address(LocalAddress),
}

#[derive(Debug)]
pub(super) enum ObjectBatchProgress {
    Pending,
    Complete(IngestSummary),
}

pub(super) struct ObjectBatchContinuation {
    summary: IngestSummary,
    operations: std::vec::IntoIter<Option<ObjectOperation>>,
    part: IngestSummary,
    current: Option<Option<ObjectOperation>>,
    lookup_index: u8,
    lookup_cursor: Option<ObjectLookupCursor>,
    lookup_entity: Option<Entity>,
    parent_cursor: Option<ParentResolutionCursor>,
    parent_plan: Option<ParentResolutionPlan>,
    cancellations: VecDeque<(Option<Entity>, LocalAddress)>,
    current_cancellation: Option<(Option<Entity>, LocalAddress)>,
    cancellation_cursor: Option<ObjectLookupCursor>,
    updated: Vec<LocalAddress>,
    operation_count: usize,
    stage: BatchStage,
    callback_summary: bool,
}

impl ObjectBatchContinuation {
    fn begin_cancellations(&mut self) {
        self.cancellations.extend(self.part.recovery_cancellations());
        self.stage = BatchStage::Cancellations;
    }
}

fn operation_lookup(operation: &Option<ObjectOperation>, index: u8) -> Option<ObjectLookupTarget> {
    match (operation.as_ref(), index) {
        (Some(ObjectOperation::Observe(observation)), 0) => Some(ObjectLookupTarget::Id(observation.id)),
        (Some(ObjectOperation::Observe(observation)), 1) => {
            Some(ObjectLookupTarget::Address(LocalAddress::new(observation.source.region(), observation.local_id)))
        }
        (Some(ObjectOperation::Patch(patch)), 0) => {
            Some(ObjectLookupTarget::Address(LocalAddress::new(patch.source.region(), patch.local_id)))
        }
        (Some(ObjectOperation::Remove { source, local_id }), 0) => {
            Some(ObjectLookupTarget::Address(LocalAddress::new(source.region(), *local_id)))
        }
        _ => None,
    }
}

fn operation_parent(operation: &Option<ObjectOperation>) -> Option<(UpdateSource, std::num::NonZeroU32, ParentUpdate)> {
    match operation.as_ref()? {
        ObjectOperation::Observe(observation) => Some((observation.source, observation.local_id, observation.parent)),
        ObjectOperation::Patch(patch) => patch.parent.map(|parent| (patch.source, patch.local_id, parent)),
        ObjectOperation::Remove { .. } | ObjectOperation::Cached { .. } => None,
    }
}

impl SessionWorld {
    pub(super) fn new(limits: WorldLimits) -> Self {
        let world = World::new(limits);
        Self {
            world,
            regions: HashMap::new(),
            parcel_rejections: 0,
            degraded_updates: Summary::new(DEGRADED_UPDATE_REPORT_INTERVAL),
            rejected_values: Summary::new(REJECTED_VALUE_REPORT_INTERVAL),
            override_failures: Summary::new(OVERRIDE_FAILURE_REPORT_INTERVAL),
        }
    }

    pub(super) fn record_ingest(&mut self, summary: &IngestSummary, simulator_id: SimulatorId) {
        let rejections = summary.rejections();
        let sample = RejectionSample { simulator: simulator_id, region: self.region_for(simulator_id), addresses: rejections.addresses };
        if rejections.region_metadata.is_some() || rejections.region_mismatch || rejections.resource_limits > 0 {
            let due = self.degraded_updates.record(Instant::now(), |updates| {
                updates.region_mismatch = updates.region_mismatch.saturating_add(u64::from(rejections.region_mismatch));
                updates.resource_limits = updates.resource_limits.saturating_add(rejections.resource_limits);
                updates.malformed = updates.malformed.saturating_add(rejections.malformed);
                if let Some(error) = rejections.region_metadata {
                    updates.region_metadata = updates.region_metadata.saturating_add(1);
                    updates.last_error = Some(error);
                }
                updates.sample = Some(sample);
            });
            if let Some(due) = due {
                due.report();
            }
        } else if rejections.malformed > 0 {
            self.record_rejected_values(sample, |values| values.malformed = values.malformed.saturating_add(rejections.malformed));
        }
    }

    pub(super) fn record_value_rejection(
        &mut self,
        simulator_id: SimulatorId,
        address: Option<LocalAddress>,
        rejection: ValueRejection,
        error: Option<ApplyError>,
    ) {
        let mut addresses = [None; MAX_INGEST_DIAGNOSTIC_ADDRESSES];
        addresses[0] = address;
        let sample = RejectionSample { simulator: simulator_id, region: self.region_for(simulator_id), addresses };
        self.record_rejected_values(sample, |values| {
            let count = match rejection {
                ValueRejection::Sit => &mut values.sit,
                ValueRejection::SitError => &mut values.sit_error,
                ValueRejection::Appearance => &mut values.appearance,
                ValueRejection::AppearanceError => &mut values.appearance_error,
            };
            *count = count.saturating_add(1);
            if error.is_some() {
                values.last_error = error;
            }
        });
    }

    fn record_rejected_values(&mut self, sample: RejectionSample, update: impl FnOnce(&mut RejectedValues)) {
        let due = self.rejected_values.record(Instant::now(), |values| {
            update(values);
            values.sample = Some(sample);
        });
        if let Some(due) = due {
            due.report();
        }
    }

    pub(super) fn flush_summaries(&mut self) {
        if let Some(due) = self.degraded_updates.flush() {
            due.report();
        }
        if let Some(due) = self.rejected_values.flush() {
            due.report();
        }
        if let Some(due) = self.override_failures.flush() {
            due.report();
        }
    }

    pub(super) fn world(&self) -> &World {
        &self.world
    }

    pub(super) fn world_mut(&mut self) -> &mut World {
        &mut self.world
    }

    pub(super) fn lookup_entity_step(&mut self, id: Uuid, cursor: Option<ObjectLookupCursor>, work_limit: usize) -> ObjectLookupProgress {
        self.world.lookup_entity_with_budget(id, cursor, work_limit)
    }

    pub(super) fn lookup_entity_at_step(
        &mut self,
        address: LocalAddress,
        cursor: Option<ObjectLookupCursor>,
        work_limit: usize,
    ) -> ObjectLookupProgress {
        self.world.lookup_entity_at_with_budget(address, cursor, work_limit)
    }

    #[cfg(test)]
    pub(super) async fn lookup_entity(&mut self, id: Uuid) -> Option<Entity> {
        let mut cursor: Option<ObjectLookupCursor> = None;
        loop {
            match self.world.lookup_entity(id, cursor) {
                ObjectLookupProgress::Visible { entity, .. } => return Some(entity),
                ObjectLookupProgress::Hidden { .. } => return None,
                ObjectLookupProgress::Pending { cursor: next, .. } => {
                    cursor = Some(next);
                    tokio::task::yield_now().await;
                }
            }
        }
    }

    #[cfg(test)]
    pub(super) async fn lookup_entity_at(&mut self, address: LocalAddress) -> Option<Entity> {
        let mut cursor: Option<ObjectLookupCursor> = None;
        loop {
            match self.world.lookup_entity_at(address, cursor) {
                ObjectLookupProgress::Visible { entity, .. } => return Some(entity),
                ObjectLookupProgress::Hidden { .. } => return None,
                ObjectLookupProgress::Pending { cursor: next, .. } => {
                    cursor = Some(next);
                    tokio::task::yield_now().await;
                }
            }
        }
    }

    pub(super) fn resolve_relationships(&mut self) -> Result<world::RelationshipResolution, ApplyError> {
        self.world.resolve_relationships(MAX_RELATIONSHIPS_PER_TURN)
    }

    pub(super) fn reclaim_removals(&mut self) -> world::RemovalProgress {
        self.world.reclaim_removals(world::MAX_REMOVAL_WORK_PER_TURN)
    }

    pub(super) fn observe_agent_parcel(
        &mut self,
        source: UpdateSource,
        parcel: Option<ParcelLocalId>,
    ) -> Result<RegionApplied, ApplyError> {
        self.world.observe_agent_parcel(source, parcel)
    }

    pub(super) fn clear_agent_parcel(&mut self, region: Region) -> Result<RegionApplied, ApplyError> {
        self.world.clear_agent_parcel(region)
    }

    pub(super) fn observe_parcel_overlay(
        &mut self,
        source: UpdateSource,
        observation: ParcelOverlayObservation,
    ) -> Result<RegionApplied, ApplyError> {
        self.world.observe_parcel_overlay(source, observation)
    }

    pub(super) fn observe_parcel_properties(
        &mut self,
        source: UpdateSource,
        observation: ParcelPropertiesObservation,
    ) -> Result<RegionApplied, ApplyError> {
        self.world.observe_parcel_properties(source, observation)
    }

    pub(super) fn observe_region_flags(
        &mut self,
        source: UpdateSource,
        low: Option<u32>,
        extended: Option<u64>,
    ) -> Result<RegionApplied, ApplyError> {
        self.world.observe_region_flags(source, low, extended)
    }

    pub(super) fn record_parcel_rejection(&mut self) -> Option<u64> {
        self.parcel_rejections = self.parcel_rejections.saturating_add(1);
        (self.parcel_rejections != u64::MAX && self.parcel_rejections.is_multiple_of(PARCEL_REJECTION_LOG_INTERVAL))
            .then_some(self.parcel_rejections)
    }

    pub(super) fn observe_environment(
        &mut self,
        target: EnvironmentTarget,
        source: UpdateSource,
        assignment: EnvironmentAssignment,
    ) -> Result<RegionApplied, ApplyError> {
        self.world.observe_environment(target, source, assignment)
    }

    pub(super) fn set_self_agent(&mut self, simulator_id: SimulatorId, agent_id: Uuid) {
        let region = self.region_for(simulator_id).map(|region| region.get());
        if let Err(error) = self.world.set_self_agent(agent_id) {
            tracing::warn!(simulator = simulator_id.get(), region, %error, "could not set authenticated world agent");
        }
    }

    pub(super) fn register_simulator(
        &mut self,
        simulator_id: SimulatorId,
        handle: Option<RegionHandle>,
    ) -> Result<Region, RegistrationError> {
        if let Some(region) = self.region_for(simulator_id) {
            let current = self.world.region(region).expect("registered simulator has a live World region").handle;
            resolve_region_handle(handle, current).map_err(|_| RegistrationError::HandleChanged)?;
            self.update_region_handle(region, handle).map_err(RegistrationError::Apply)?;
            return Ok(region);
        }

        let region = self.world.register_region().map_err(RegistrationError::Apply)?;
        self.regions.insert(simulator_id, region);
        self.update_region_handle(region, handle).map_err(RegistrationError::Apply)?;
        Ok(region)
    }

    fn update_region_handle(&mut self, region: Region, handle: Option<RegionHandle>) -> Result<(), ApplyError> {
        if let Some(handle) = handle {
            self.update_region_metadata(region, RegionInfo { handle: Some(handle), ..RegionInfo::default() })?;
        }
        Ok(())
    }

    pub(super) fn region_for(&self, simulator_id: SimulatorId) -> Option<Region> {
        self.regions.get(&simulator_id).copied()
    }

    pub(super) fn region_name(&self, simulator_id: SimulatorId) -> Option<&str> {
        self.world.region(self.region_for(simulator_id)?).and_then(|info| info.name.as_deref())
    }

    pub(super) fn simulator_for_region(&self, region: Region) -> Option<SimulatorId> {
        self.regions.iter().find_map(|(simulator_id, known)| (*known == region).then_some(*simulator_id))
    }

    pub(super) fn update_region_metadata(&mut self, region: Region, info: RegionInfo) -> Result<(), ApplyError> {
        self.world.patch_region(region, info).map(|_| ())
    }

    pub(super) fn update_water_height(&mut self, source: UpdateSource, height: f32) -> Result<(), ApplyError> {
        self.world.observe_water_height(source, height).map(|_| ())
    }

    pub(super) fn update_terrain_settings(
        &mut self,
        source: UpdateSource,
        detail_textures: [Uuid; 4],
        start_heights: [f32; 4],
        height_ranges: [f32; 4],
    ) -> Result<(), ApplyError> {
        let settings = TerrainSettings::new(detail_textures, start_heights, height_ranges)
            .map_err(|_| ApplyError::InvalidValue { field: "terrain_settings" })?;
        self.world.observe_terrain_settings(source, settings).map(|_| ())
    }

    pub(super) fn apply_terrain(&mut self, decoded: DecodedTerrain) -> usize {
        let Some(simulator) = self.simulator_for_region(decoded.source.region()) else { return 0 };
        let mut rejected = decoded.rejected;
        let mut applied = 0;
        for (ordinal, patch) in decoded.patches {
            let source = UpdateSource::new(decoded.source.region(), decoded.source.sequence(), decoded.source.order(), ordinal)
                .with_authority(decoded.source.authority());
            match self.world.observe_terrain_patch(source, patch) {
                Ok(result) if matches!(result.outcome, RegionOutcome::Patched { .. }) => applied += 1,
                Ok(_) => {}
                Err(_) => rejected += 1,
            }
        }
        if rejected > 0 {
            let sample =
                RejectionSample { simulator, region: Some(decoded.source.region()), addresses: [None; MAX_INGEST_DIAGNOSTIC_ADDRESSES] };
            self.record_rejected_values(sample, |values| values.terrain = values.terrain.saturating_add(rejected as u64));
        }
        applied
    }

    pub(super) fn update_handshake_metadata(
        &mut self,
        source: UpdateSource,
        region_id: Option<Uuid>,
        name: String,
    ) -> Result<(), ApplyError> {
        self.world.observe_region_metadata(source, RegionInfo { region_id, name: Some(name), ..RegionInfo::default() }).map(|_| ())
    }

    pub(super) fn apply_object_batch(&mut self, simulator_id: SimulatorId, batch: ObjectBatch) -> Option<IngestSummary> {
        let region = self.region_for(simulator_id)?;
        if region != batch.region {
            return Some(IngestSummary::new(IngestStatus::RegionContextMismatch));
        }
        let current_handle = self.world.region(region).expect("registered simulator has a live World region").handle;
        let accepted_handle = match resolve_region_handle(batch.region_handle, current_handle) {
            Ok(handle) => handle,
            Err(status) => return Some(IngestSummary::new(status)),
        };

        let mut summary = IngestSummary::new(IngestStatus::Applied);
        if batch.blocks.is_empty() || batch.blocks.iter().any(Option::is_some) {
            if let Err(error) = self.update_region_handle(region, accepted_handle) {
                summary.status = IngestStatus::RegionMetadataRejected(error);
                return Some(summary);
            }
            summary.accepted_region_handle = accepted_handle;
        }
        summary.invalid_blocks = batch.invalid_blocks;
        summary.invalid_fields = batch.invalid_fields;
        for operation in batch.blocks {
            summary.blocks.push(self.apply_object_operation(operation, None));
        }
        Some(summary)
    }

    fn apply_object_operation(
        &mut self,
        operation: Option<ObjectOperation>,
        parent_resolution: Option<ParentResolutionPlan>,
    ) -> BlockOutcome {
        match operation {
            Some(ObjectOperation::Cached { source, local_id, cache_miss_type }) => {
                let address = LocalAddress::new(source.region(), local_id);
                BlockOutcome::RecoveryRequired { address, cache_miss_type }
            }
            Some(operation) => {
                let address = operation.address();
                match apply_operation(&mut self.world, operation, parent_resolution) {
                    Ok(applied) => {
                        let recovered = applied.recovery.satisfied.then_some(address);
                        BlockOutcome::Applied { address: Some(address), applied, recovered }
                    }
                    Err(error) => BlockOutcome::Rejected { address: Some(address), error },
                }
            }
            None => BlockOutcome::Invalid,
        }
    }

    pub(super) fn begin_object_batch(&mut self, simulator_id: SimulatorId, batch: ObjectBatch) -> Option<ObjectBatchContinuation> {
        let ObjectBatch { region, region_handle, blocks, invalid_blocks, invalid_fields } = batch;
        let has_object_operations = blocks.iter().any(Option::is_some);
        let summary = if blocks.is_empty() || has_object_operations {
            self.apply_object_batch(
                simulator_id,
                ObjectBatch { region, region_handle, blocks: Vec::new(), invalid_blocks, invalid_fields },
            )?
        } else {
            let registered_region = self.region_for(simulator_id)?;
            let status = if registered_region == region { IngestStatus::Applied } else { IngestStatus::RegionContextMismatch };
            let mut summary = IngestSummary::new(status);
            summary.invalid_blocks = invalid_blocks;
            summary.invalid_fields = invalid_fields;
            summary
        };
        let callback_summary = blocks.is_empty() || summary.status != IngestStatus::Applied;
        Some(ObjectBatchContinuation {
            summary,
            operations: blocks.into_iter(),
            part: IngestSummary::new(IngestStatus::Applied),
            current: None,
            lookup_index: 0,
            lookup_cursor: None,
            lookup_entity: None,
            parent_cursor: None,
            parent_plan: None,
            cancellations: VecDeque::new(),
            current_cancellation: None,
            cancellation_cursor: None,
            updated: Vec::new(),
            operation_count: 0,
            stage: BatchStage::Operations,
            callback_summary,
        })
    }

    pub(super) fn advance_object_batch(
        &mut self,
        continuation: &mut ObjectBatchContinuation,
        on_chunk: &mut impl FnMut(Vec<(Option<SimulatorId>, LocalAddress, CacheMissType)>, Vec<LocalAddress>),
    ) -> ObjectBatchProgress {
        if continuation.callback_summary {
            let targets = continuation
                .summary
                .recovery_requests()
                .map(|(address, cache_miss_type)| (self.simulator_for_region(address.region()), address, cache_miss_type))
                .collect();
            on_chunk(targets, Vec::new());
            continuation.callback_summary = false;
            return ObjectBatchProgress::Complete(std::mem::replace(&mut continuation.summary, IngestSummary::new(IngestStatus::Applied)));
        }

        let mut lookup_work = 0;
        loop {
            if lookup_work >= MAX_OBJECT_LOOKUP_WORK_ITEMS {
                return ObjectBatchProgress::Pending;
            }
            match continuation.stage {
                BatchStage::Operations => {
                    if continuation.operation_count == MAX_OBJECTS_PER_INTAKE_QUANTUM {
                        continuation.begin_cancellations();
                        continue;
                    }
                    if continuation.current.is_none() {
                        let Some(operation) = continuation.operations.next() else {
                            if continuation.part.blocks.is_empty() && continuation.operation_count == 0 {
                                let summary = std::mem::replace(&mut continuation.summary, IngestSummary::new(IngestStatus::Applied));
                                on_chunk(Vec::new(), Vec::new());
                                return ObjectBatchProgress::Complete(summary);
                            }
                            continuation.begin_cancellations();
                            continue;
                        };
                        continuation.current = Some(operation);
                        continuation.lookup_index = 0;
                        continuation.lookup_entity = None;
                        continuation.parent_cursor = None;
                        continuation.parent_plan = None;
                    }
                    let lookup = continuation.current.as_ref().and_then(|operation| operation_lookup(operation, continuation.lookup_index));
                    if let Some(target) = lookup {
                        let work_limit = MAX_OBJECT_LOOKUP_WORK_ITEMS.saturating_sub(lookup_work);
                        let progress = match target {
                            ObjectLookupTarget::Id(id) => {
                                self.world.lookup_entity_with_budget(id, continuation.lookup_cursor.take(), work_limit)
                            }
                            ObjectLookupTarget::Address(address) => {
                                self.world.lookup_entity_at_with_budget(address, continuation.lookup_cursor.take(), work_limit)
                            }
                        };
                        match progress {
                            ObjectLookupProgress::Visible { entity, work_units } => {
                                lookup_work = lookup_work.saturating_add(work_units);
                                if continuation.lookup_index == 0 {
                                    continuation.lookup_entity = Some(entity);
                                }
                                continuation.lookup_index = continuation.lookup_index.saturating_add(1);
                            }
                            ObjectLookupProgress::Hidden { work_units } => {
                                lookup_work = lookup_work.saturating_add(work_units);
                                continuation.lookup_index = continuation.lookup_index.saturating_add(1);
                            }
                            ObjectLookupProgress::Pending { cursor, .. } => {
                                continuation.lookup_cursor = Some(cursor);
                                return ObjectBatchProgress::Pending;
                            }
                        }
                        continue;
                    }
                    let parent_update = continuation.current.as_ref().and_then(operation_parent);
                    let patch_needs_target = matches!(continuation.current, Some(Some(ObjectOperation::Patch(_))));
                    if continuation.parent_plan.is_none()
                        && (!patch_needs_target || continuation.lookup_entity.is_some())
                        && let Some((source, local_id, update)) = parent_update
                    {
                        match self.world.prepare_parent_resolution_with_budget(
                            source,
                            local_id,
                            update,
                            continuation.lookup_entity,
                            continuation.parent_cursor.take(),
                            MAX_OBJECT_LOOKUP_WORK_ITEMS.saturating_sub(lookup_work),
                        ) {
                            ParentResolutionProgress::Ready { plan, work_units } => {
                                lookup_work = lookup_work.saturating_add(work_units);
                                continuation.parent_plan = Some(plan);
                            }
                            ParentResolutionProgress::Pending { cursor, .. } => {
                                continuation.parent_cursor = Some(cursor);
                                return ObjectBatchProgress::Pending;
                            }
                        }
                    }
                    let operation = continuation.current.take().flatten();
                    let parent_resolution = continuation.parent_plan.take();
                    continuation.part.blocks.push(self.apply_object_operation(operation, parent_resolution));
                    continuation.operation_count += 1;
                    continuation.lookup_index = 0;
                    continuation.lookup_cursor = None;
                    continuation.lookup_entity = None;
                    continuation.parent_cursor = None;
                    continuation.parent_plan = None;
                }
                BatchStage::Cancellations => {
                    let (expected, address) = match continuation.current_cancellation {
                        Some(cancellation) => cancellation,
                        None => {
                            let Some(cancellation) = continuation.cancellations.pop_front() else {
                                let targets = continuation
                                    .part
                                    .recovery_requests()
                                    .map(|(address, cache_miss_type)| {
                                        (self.simulator_for_region(address.region()), address, cache_miss_type)
                                    })
                                    .collect();
                                let updated = std::mem::take(&mut continuation.updated);
                                on_chunk(targets, updated);
                                continuation
                                    .summary
                                    .append(std::mem::replace(&mut continuation.part, IngestSummary::new(IngestStatus::Applied)));
                                continuation.operation_count = 0;
                                continuation.current_cancellation = None;
                                continuation.cancellation_cursor = None;
                                if continuation.summary.status != IngestStatus::Applied || continuation.operations.len() == 0 {
                                    return ObjectBatchProgress::Complete(std::mem::replace(
                                        &mut continuation.summary,
                                        IngestSummary::new(IngestStatus::Applied),
                                    ));
                                }
                                continuation.stage = BatchStage::Operations;
                                return ObjectBatchProgress::Pending;
                            };
                            continuation.current_cancellation = Some(cancellation);
                            cancellation
                        }
                    };
                    let work_limit = MAX_OBJECT_LOOKUP_WORK_ITEMS.saturating_sub(lookup_work);
                    match self.world.lookup_entity_at_with_budget(address, continuation.cancellation_cursor.take(), work_limit) {
                        ObjectLookupProgress::Visible { entity: current, work_units } => {
                            lookup_work = lookup_work.saturating_add(work_units);
                            if expected.is_none_or(|expected| expected == current) {
                                continuation.updated.push(address);
                            }
                            continuation.current_cancellation = None;
                        }
                        ObjectLookupProgress::Hidden { work_units } => {
                            lookup_work = lookup_work.saturating_add(work_units);
                            continuation.updated.push(address);
                            continuation.current_cancellation = None;
                        }
                        ObjectLookupProgress::Pending { cursor, .. } => {
                            continuation.cancellation_cursor = Some(cursor);
                            return ObjectBatchProgress::Pending;
                        }
                    }
                }
            }
        }
    }

    #[cfg(test)]
    pub(super) async fn apply_object_batch_cooperatively(
        &mut self,
        simulator_id: SimulatorId,
        batch: ObjectBatch,
        mut on_chunk: impl FnMut(Vec<(Option<SimulatorId>, LocalAddress, CacheMissType)>, Vec<LocalAddress>),
    ) -> Option<IngestSummary> {
        let mut continuation = self.begin_object_batch(simulator_id, batch)?;
        loop {
            match self.advance_object_batch(&mut continuation, &mut on_chunk) {
                ObjectBatchProgress::Pending => tokio::task::yield_now().await,
                ObjectBatchProgress::Complete(summary) => return Some(summary),
            }
        }
    }

    #[cfg(test)]
    pub(super) async fn prepare_object_media(
        &mut self,
        simulator_id: SimulatorId,
        request: ObjectMediaRequest,
    ) -> Option<ObjectMediaTarget> {
        let mut cursor = None;
        loop {
            match self.prepare_object_media_step(simulator_id, request, cursor.take(), MAX_OBJECT_LOOKUP_WORK_ITEMS) {
                ObjectMediaPreparationProgress::Pending(next) => {
                    cursor = Some(next);
                    tokio::task::yield_now().await;
                }
                ObjectMediaPreparationProgress::Complete { target, .. } => return target,
            }
        }
    }

    pub(super) fn prepare_object_media_step(
        &mut self,
        simulator_id: SimulatorId,
        request: ObjectMediaRequest,
        cursor: Option<ObjectLookupCursor>,
        work_limit: usize,
    ) -> ObjectMediaPreparationProgress {
        let Some(region) = self.region_for(simulator_id) else {
            return ObjectMediaPreparationProgress::Complete { target: None, work_units: 0 };
        };
        if region != request.source.region() || !matches!(request.source.order(), OrderEvidence::Ordered(_)) {
            return ObjectMediaPreparationProgress::Complete { target: None, work_units: 0 };
        }
        let address = LocalAddress::new(region, request.local_id);
        match self.world.lookup_entity_at_with_budget(address, cursor, work_limit) {
            ObjectLookupProgress::Visible { entity, work_units } => {
                let target = if self.world.object(entity).is_some_and(|object| {
                    object.id() == request.object_id
                        && object.kind() == Kind::Object
                        && object.appearance() == Some(Appearance::Surface)
                        && !object.object_media().is_some_and(|media| media.version() >= request.version)
                }) {
                    self.world.object_binding_target(entity).map(|binding| ObjectMediaTarget {
                        simulator_id,
                        binding,
                        source: request.source,
                        version: request.version,
                    })
                } else {
                    None
                };
                ObjectMediaPreparationProgress::Complete { target, work_units }
            }
            ObjectLookupProgress::Hidden { work_units } => ObjectMediaPreparationProgress::Complete { target: None, work_units },
            ObjectLookupProgress::Pending { cursor, .. } => ObjectMediaPreparationProgress::Pending(cursor),
        }
    }

    pub(super) fn object_media_target_is_current_step(
        &mut self,
        target: ObjectMediaTarget,
        check: &mut ObjectMediaTargetCheck,
        work_limit: usize,
    ) -> ObjectMediaTargetCheckProgress {
        let Some(region) = self.region_for(target.simulator_id) else {
            return ObjectMediaTargetCheckProgress::Complete { current: false, work_units: 0 };
        };
        if region != target.source.region() || !matches!(target.source.order(), OrderEvidence::Ordered(_)) {
            return ObjectMediaTargetCheckProgress::Complete { current: false, work_units: 0 };
        }
        if target.binding.address().region() != region {
            return ObjectMediaTargetCheckProgress::Complete { current: false, work_units: 0 };
        }
        let revision = self.world.revision();
        if check.revision != Some(revision) {
            check.revision = Some(revision);
            check.entity_cursor = None;
            check.address_cursor = None;
            check.stage = ObjectMediaTargetCheckStage::Entity;
        }
        let work_limit = work_limit.min(MAX_OBJECT_LOOKUP_WORK_ITEMS);
        if work_limit == 0 {
            return ObjectMediaTargetCheckProgress::Pending;
        }
        let mut work = 0usize;
        loop {
            if work >= work_limit {
                return ObjectMediaTargetCheckProgress::Pending;
            }
            match check.stage {
                ObjectMediaTargetCheckStage::Entity => {
                    match self.world.lookup_entity_with_budget(
                        target.binding.object_id(),
                        check.entity_cursor.take(),
                        work_limit.saturating_sub(work),
                    ) {
                        ObjectLookupProgress::Visible { entity, work_units } => {
                            work = work.saturating_add(work_units);
                            if entity != target.binding.entity() {
                                return ObjectMediaTargetCheckProgress::Complete { current: false, work_units: work };
                            }
                            check.stage = ObjectMediaTargetCheckStage::Address;
                        }
                        ObjectLookupProgress::Hidden { work_units } => {
                            work = work.saturating_add(work_units);
                            return ObjectMediaTargetCheckProgress::Complete { current: false, work_units: work };
                        }
                        ObjectLookupProgress::Pending { cursor, .. } => {
                            check.entity_cursor = Some(cursor);
                            return ObjectMediaTargetCheckProgress::Pending;
                        }
                    }
                }
                ObjectMediaTargetCheckStage::Address => {
                    match self.world.lookup_entity_at_with_budget(
                        target.binding.address(),
                        check.address_cursor.take(),
                        work_limit.saturating_sub(work),
                    ) {
                        ObjectLookupProgress::Visible { entity, work_units } => {
                            work = work.saturating_add(work_units);
                            if entity != target.binding.entity() {
                                return ObjectMediaTargetCheckProgress::Complete { current: false, work_units: work };
                            }
                            check.stage = ObjectMediaTargetCheckStage::Complete;
                        }
                        ObjectLookupProgress::Hidden { work_units } => {
                            work = work.saturating_add(work_units);
                            return ObjectMediaTargetCheckProgress::Complete { current: false, work_units: work };
                        }
                        ObjectLookupProgress::Pending { cursor, .. } => {
                            check.address_cursor = Some(cursor);
                            return ObjectMediaTargetCheckProgress::Pending;
                        }
                    }
                }
                ObjectMediaTargetCheckStage::Complete => {
                    if !self.world.object_binding_target_is_current(target.binding) {
                        return ObjectMediaTargetCheckProgress::Complete { current: false, work_units: work };
                    }
                    return ObjectMediaTargetCheckProgress::Complete {
                        current: self.world.object(target.binding.entity()).is_some_and(|object| {
                            object.kind() == Kind::Object
                                && object.appearance() == Some(Appearance::Surface)
                                && object.object_media().is_none_or(|media| media.version() < target.version)
                        }),
                        work_units: work,
                    };
                }
            }
        }
    }

    pub(super) fn apply_object_media_after_check(
        &mut self,
        target: ObjectMediaTarget,
        update: ObjectMediaUpdate,
    ) -> Option<Result<Applied, ApplyError>> {
        if update.version() < target.version
            || !self.world.object_binding_target_is_current(target.binding)
            || !self.world.object(target.binding.entity()).is_some_and(|object| {
                object.kind() == Kind::Object
                    && object.appearance() == Some(Appearance::Surface)
                    && object.object_media().is_none_or(|media| media.version() < target.version)
            })
        {
            return None;
        }
        let mut patch = ObjectPatch::new(target.source, target.binding.address().local_id());
        patch.object_media = Some(update);
        let result = self.world.patch_object(patch);
        Some(result)
    }

    pub(super) fn restore_cached_overrides(&mut self, overrides: Vec<CachedOverride>) {
        if let Err(error) = self.retain_cached_overrides(overrides) {
            let due = self.override_failures.record(Instant::now(), |failures| {
                failures.failed = failures.failed.saturating_add(1);
                failures.last_error = Some(error);
            });
            if let Some(due) = due {
                due.report();
            }
        }
    }

    pub(super) fn retain_cached_overrides(&mut self, overrides: Vec<CachedOverride>) -> Result<(), ApplyError> {
        for cached in overrides {
            drop(self.world.retain_pbr_material_overrides(cached.source, cached.local_id, cached.expected_id, cached.overrides)?);
        }
        Ok(())
    }

    #[cfg(test)]
    pub(super) async fn apply_object_media(
        &mut self,
        target: ObjectMediaTarget,
        update: ObjectMediaUpdate,
    ) -> Option<Result<Applied, ApplyError>> {
        let mut check = ObjectMediaTargetCheck::default();
        loop {
            match self.object_media_target_is_current_step(target, &mut check, MAX_OBJECT_LOOKUP_WORK_ITEMS) {
                ObjectMediaTargetCheckProgress::Pending => tokio::task::yield_now().await,
                ObjectMediaTargetCheckProgress::Complete { current: true, .. } => {
                    return self.apply_object_media_after_check(target, update);
                }
                ObjectMediaTargetCheckProgress::Complete { current: false, .. } => return None,
            }
        }
    }

    pub(super) fn apply_sit_target_update(
        &mut self,
        simulator_id: SimulatorId,
        source: UpdateSource,
        update: SitTargetUpdate,
    ) -> Option<Result<Applied, ApplyError>> {
        let region = self.region_for(simulator_id)?;
        if region != source.region() {
            return None;
        }
        let result = self.world.patch_sit_target(source, update.object_id, Some(update.target));
        Some(result)
    }

    pub(super) fn apply_avatar_appearance(
        &mut self,
        simulator_id: SimulatorId,
        source: UpdateSource,
        entity: Option<Entity>,
        update: AvatarAppearanceMessage,
    ) -> Option<Result<Applied, ApplyError>> {
        let region = self.region_for(simulator_id)?;
        if region != source.region() {
            return None;
        }
        let entity = entity?;
        let object = self.world.object(entity)?;
        if object.id() != update.object_id || object.kind() != Kind::Avatar || object.address().region() != region {
            return None;
        }
        let local_id = object.address().local_id();
        let mut patch = ObjectPatch::new(source, local_id);
        patch.avatar_appearance = Some(update.update);
        let result = self.world.patch_object(patch);
        Some(result)
    }

    pub(super) fn complete_agent_handoff(
        &mut self,
        from_simulator: SimulatorId,
        to_simulator: SimulatorId,
        current_entity: Option<Entity>,
        observation: ObjectObservation,
    ) -> (bool, Option<Applied>) {
        let Some(to) = self.region_for(to_simulator) else { return (false, None) };
        if observation.source.region() != to || observation.kind != Kind::Avatar {
            return (false, None);
        }
        let Some(entity) = current_entity else {
            let applied = match self.world.observe_object(observation) {
                Ok(applied) => applied,
                Err(_) => return (false, None),
            };
            let introduced = matches!(applied.outcome, Introduced(_));
            return (introduced, Some(applied));
        };
        let Some(current_region) = self.world.object(entity).map(|object| object.address().region()) else { return (false, None) };
        if current_region == to {
            return (true, None);
        }
        let Some(from) = self.region_for(from_simulator).filter(|region| *region == current_region) else { return (false, None) };
        let applied = match self.world.handoff_agent(from, observation) {
            Ok(applied) => applied,
            Err(error) => {
                tracing::debug!(simulator = to_simulator.get(), region = to.get(), %error, "agent region handoff is waiting for valid source state");
                return (false, None);
            }
        };
        let completed = matches!(applied.outcome, AgentHandedOff(_));
        (completed, Some(applied))
    }

    pub(super) fn retire_simulator(&mut self, simulator_id: SimulatorId) -> Result<(), ApplyError> {
        let Some(region) = self.region_for(simulator_id) else { return Ok(()) };
        let _ = self.world.retire_region(region)?;
        self.regions.remove(&simulator_id);
        Ok(())
    }

    pub(super) fn retire_all(&mut self) -> Vec<(SimulatorId, ApplyError)> {
        let simulator_ids = self.regions.keys().copied().collect::<Vec<_>>();
        let mut failures = Vec::new();
        for simulator_id in simulator_ids {
            if let Err(error) = self.retire_simulator(simulator_id) {
                failures.push((simulator_id, error));
            }
        }
        failures
    }
}

fn resolve_region_handle(
    message_handle: Option<RegionHandle>,
    current_handle: Option<RegionHandle>,
) -> Result<Option<RegionHandle>, IngestStatus> {
    if let Some(handle) = message_handle {
        if current_handle.is_some_and(|current| current != handle) {
            return Err(IngestStatus::RegionHandleMismatch);
        }
        return Ok(Some(handle));
    }
    Ok(current_handle)
}

fn apply_operation(
    world: &mut World,
    operation: ObjectOperation,
    parent_resolution: Option<ParentResolutionPlan>,
) -> Result<Applied, ApplyError> {
    match operation {
        ObjectOperation::Observe(observation) => match parent_resolution {
            Some(plan) => world.observe_object_with_parent_resolution(observation, plan),
            None => world.observe_object(observation),
        },
        ObjectOperation::Patch(patch) => match parent_resolution {
            Some(plan) => world.patch_object_with_parent_resolution(patch, plan),
            None => world.patch_object(patch),
        },
        ObjectOperation::Remove { source, local_id } => world.remove_object(source, local_id),
        ObjectOperation::Cached { .. } => unreachable!("cached operations are recovery intents"),
    }
}

pub(super) fn viewer_world_limits() -> WorldLimits {
    WorldLimits {
        max_regions: MAX_WORLD_ENTITIES,
        max_entities: MAX_WORLD_ENTITIES,
        max_entities_per_region: MAX_WORLD_ENTITIES,
        max_tombstones_per_region: MAX_WORLD_TOMBSTONES_PER_REGION,
        max_tombstones: MAX_WORLD_TOMBSTONES,
        max_tombstone_bytes_per_region: WorldLimits::tombstone_bytes_for_count(MAX_WORLD_TOMBSTONES_PER_REGION),
        max_tombstone_bytes: WorldLimits::tombstone_bytes_for_count(MAX_WORLD_TOMBSTONES),
        max_journal_entries: MAX_WORLD_JOURNAL_ENTRIES,
        max_journal_changes: MAX_WORLD_ENTITIES,
        max_region_name_bytes: MAX_REGION_NAME_BYTES,
        max_recovery_targets: MAX_WORLD_ENTITIES,
    }
}

#[cfg(test)]
#[path = "world_tests.rs"]
mod tests;
