use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use network::{CircuitMotionStatus, CircuitStats, SessionHandle, SimulatorId, capabilities::Client as CapabilityClient};
use protocol::messages::SimStats;
use render::{CaptureReadback, ObjectExplanation, PickDetail, PickError, Stats};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::{
    mpsc::{self, error::TrySendError},
    oneshot, watch,
};
use uuid::Uuid;
use wgpu::{AdapterInfo, Limits};
use world::{Entity, Region};

use crate::{
    asset_coordinator::AssetHandle,
    inventory::InventoryFetchHandle,
    logging::{LogControl, RecentLogs},
    session::AgentMotionSample,
    settings::SettingsStore,
    world_reader::WorldReader,
};

pub const VIEWPORT_QUERY_CAPACITY: usize = 8;
pub const FRAME_SPIKE_CAPACITY: usize = 32;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct FrameSpike {
    pub work_ms: f64,
    pub frame_ms: f64,
    pub game_ms: f64,
    pub render_ms: f64,
    pub gpu_timings_ms: Option<(f64, f64)>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct FrameSpikeRing {
    entries: [Option<FrameSpike>; FRAME_SPIKE_CAPACITY],
    head: usize,
    len: usize,
}

impl FrameSpikeRing {
    pub fn push(&mut self, spike: FrameSpike) {
        self.entries[self.head] = Some(spike);
        self.head = (self.head + 1) % FRAME_SPIKE_CAPACITY;
        if self.len < FRAME_SPIKE_CAPACITY {
            self.len += 1;
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn iter(&self) -> impl Iterator<Item = FrameSpike> + '_ {
        let start = if self.len == FRAME_SPIKE_CAPACITY { self.head } else { 0 };
        (0..self.len).filter_map(move |offset| self.entries[(start + offset) % FRAME_SPIKE_CAPACITY])
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BoundedQueueStats {
    pub depth: usize,
    pub capacity: usize,
    pub full_count: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SessionQueuesSnapshot {
    pub client_events_depth: usize,
    pub client_events_capacity: usize,
    pub experience_environment: BoundedQueueStats,
    pub object_media: Option<BoundedQueueStats>,
}

#[derive(Clone, Copy, Default)]
pub struct RenderStatsSnapshot {
    pub frame_ms: f64,
    pub game_ms: f64,
    pub render_ms: f64,
    pub fps: f64,
    pub render_stats: Stats,
    pub gpu_timings: Option<(f64, f64)>,
    pub viewport_size: [u32; 2],
    pub frame_spikes: FrameSpikeRing,
    pub texture_completions_capacity: usize,
    pub texture_completions_dropped: u64,
}

#[derive(Clone, Debug, Default)]
pub struct SceneQuerySummary {
    pub viewport_size: [u32; 2],
    pub scene_sequence: u64,
    pub ready: bool,
    pub has_camera: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct EnvironmentOverrideSummary {
    pub local_override: bool,
    pub local_sky: bool,
    pub local_water: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CameraStateSnapshot {
    pub mode: CameraModeRequest,
    pub position: Option<[f64; 3]>,
    pub direction: Option<[f32; 3]>,
    pub vertical_fov_deg: Option<f64>,
    pub draw_distance: Option<f64>,
    pub lod_factor: Option<f64>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CameraModeRequest {
    #[default]
    Follow,
    Free,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SetCameraRequest {
    pub mode: Option<CameraModeRequest>,
    pub position: Option<[f64; 3]>,
    pub look_at: Option<[f64; 3]>,
}

pub enum ViewportQuery {
    SceneQuery { reply: oneshot::Sender<SceneQuerySummary> },
    ExplainObject { entity: Entity, reply: oneshot::Sender<Option<ObjectExplanation>> },
    PixelPick { x: f32, y: f32, reply: oneshot::Sender<Result<Option<PickDetail>, PickError>> },
    BeginScreenshot { reply: oneshot::Sender<Option<CaptureReadback>> },
    EnvironmentOverride { reply: oneshot::Sender<EnvironmentOverrideSummary> },
    CameraState { reply: oneshot::Sender<CameraStateSnapshot> },
    SetCamera { request: SetCameraRequest, reply: oneshot::Sender<Result<CameraStateSnapshot, String>> },
}

impl ViewportQuery {
    pub fn is_closed(&self) -> bool {
        match self {
            Self::SceneQuery { reply } => reply.is_closed(),
            Self::ExplainObject { reply, .. } => reply.is_closed(),
            Self::PixelPick { reply, .. } => reply.is_closed(),
            Self::BeginScreenshot { reply } => reply.is_closed(),
            Self::EnvironmentOverride { reply } => reply.is_closed(),
            Self::CameraState { reply } => reply.is_closed(),
            Self::SetCamera { reply, .. } => reply.is_closed(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ObjectCacheStats {
    pub started: bool,
    pub ready: bool,
    pub disabled: bool,
    pub degraded: bool,
    pub write_queue_depth: usize,
    pub write_queue_capacity: usize,
    pub write_queue_bytes: usize,
    pub write_queue_byte_capacity: usize,
    pub outstanding_writes: usize,
    pub pending_submissions: usize,
    pub pending_submissions_capacity: usize,
    pub busy_drops: u64,
    pub full_drops: u64,
    pub dropped_writes: u64,
    pub identified_regions: usize,
    pub loaded_regions: usize,
    pub in_flight_region_loads: usize,
    pub loaded_entries: usize,
    pub loaded_bytes: usize,
}

#[derive(Clone, Copy, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InventoryRetryState {
    Exhausted,
    Cancelled,
    NonRetryable,
}

#[derive(Clone, Copy, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InventoryErrorKind {
    CapabilityUnavailable,
    FolderTooLarge,
    InvalidResponse,
    RetryExhausted,
    HttpStatus,
    Cancelled,
    RequestFailed,
    Model,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FailedInventoryFolder {
    pub folder_id: Uuid,
    pub owner_id: Uuid,
    pub retry_state: InventoryRetryState,
    pub error_kind: InventoryErrorKind,
    pub error: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct InventoryFetchStats {
    pub folders_known: usize,
    pub folders_pending: usize,
    pub folders_in_flight: usize,
    pub folders_done: usize,
    pub folders_failed: usize,
    pub background_done: bool,
    pub failed_folders: Vec<FailedInventoryFolder>,
}

#[derive(Clone)]
pub struct RegionNetworkHandle {
    pub region: Region,
    pub simulator_id: SimulatorId,
    pub region_name: Option<String>,
    pub is_primary: bool,
    pub circuit: watch::Receiver<CircuitMotionStatus>,
    pub stats: watch::Receiver<CircuitStats>,
    pub capabilities: Option<CapabilityClient>,
}

const MAX_UNKNOWN_SIM_STATS: usize = 64;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct RegionSimStats {
    pub region_x: u32,
    pub region_y: u32,
    pub region_flags: u32,
    pub region_flags_extended: Option<u64>,
    pub object_capacity: u32,
    pub pid: Option<i32>,
    pub time_dilation: Option<f32>,
    pub sim_fps: Option<f32>,
    pub physics_fps: Option<f32>,
    pub agent_updates_per_sec: Option<f32>,
    pub frame_ms: Option<f32>,
    pub net_ms: Option<f32>,
    pub other_ms: Option<f32>,
    pub physics_ms: Option<f32>,
    pub agent_ms: Option<f32>,
    pub image_ms: Option<f32>,
    pub script_ms: Option<f32>,
    pub total_objects: Option<f32>,
    pub active_objects: Option<f32>,
    pub main_agents: Option<f32>,
    pub child_agents: Option<f32>,
    pub active_scripts: Option<f32>,
    pub script_lsl_ips: Option<f32>,
    pub in_packets_per_sec: Option<f32>,
    pub out_packets_per_sec: Option<f32>,
    pub pending_downloads: Option<f32>,
    pub pending_uploads: Option<f32>,
    pub virtual_size_kb: Option<f32>,
    pub resident_size_kb: Option<f32>,
    pub pending_local_uploads: Option<f32>,
    pub unacked_bytes: Option<f32>,
    pub physics_pinned_tasks: Option<f32>,
    pub physics_lod_tasks: Option<f32>,
    pub physics_step_ms: Option<f32>,
    pub physics_shape_ms: Option<f32>,
    pub physics_other_ms: Option<f32>,
    pub physics_memory_mb: Option<f32>,
    pub script_eps: Option<f32>,
    pub spare_ms: Option<f32>,
    pub sleep_ms: Option<f32>,
    pub io_pump_ms: Option<f32>,
    pub scripts_run_pct: Option<f32>,
    pub ai_step_ms: Option<f32>,
    pub skipped_silhouette_steps: Option<f32>,
    pub characters_updated_pct: Option<f32>,
    pub other_stats: Vec<(u32, f32)>,
}

impl RegionSimStats {
    pub fn from_message(message: &SimStats) -> Self {
        let region_flags_extended =
            message.region_info.iter().filter_map(|info| info.region_flags_extended).reduce(|left, right| left | right);
        let mut stats = Self {
            region_x: message.region.region_x,
            region_y: message.region.region_y,
            region_flags: message.region.region_flags,
            region_flags_extended,
            object_capacity: message.region.object_capacity,
            pid: message.pid,
            ..Self::default()
        };
        for entry in &message.stats {
            if !entry.stat_value.is_finite() {
                continue;
            }
            let value = Some(entry.stat_value);
            match entry.stat_id {
                0 => stats.time_dilation = value,
                1 => stats.sim_fps = value,
                2 => stats.physics_fps = value,
                3 => stats.agent_updates_per_sec = value,
                4 => stats.frame_ms = value,
                5 => stats.net_ms = value,
                6 => stats.other_ms = value,
                7 => stats.physics_ms = value,
                8 => stats.agent_ms = value,
                9 => stats.image_ms = value,
                10 => stats.script_ms = value,
                11 => stats.total_objects = value,
                12 => stats.active_objects = value,
                13 => stats.main_agents = value,
                14 => stats.child_agents = value,
                15 => stats.active_scripts = value,
                16 => stats.script_lsl_ips = value,
                17 => stats.in_packets_per_sec = value,
                18 => stats.out_packets_per_sec = value,
                19 => stats.pending_downloads = value,
                20 => stats.pending_uploads = value,
                21 => stats.virtual_size_kb = value,
                22 => stats.resident_size_kb = value,
                23 => stats.pending_local_uploads = value,
                24 => stats.unacked_bytes = value,
                25 => stats.physics_pinned_tasks = value,
                26 => stats.physics_lod_tasks = value,
                27 => stats.physics_step_ms = value,
                28 => stats.physics_shape_ms = value,
                29 => stats.physics_other_ms = value,
                30 => stats.physics_memory_mb = value,
                31 => stats.script_eps = value,
                32 => stats.spare_ms = value,
                33 => stats.sleep_ms = value,
                34 => stats.io_pump_ms = value,
                35 => stats.scripts_run_pct = value,
                37 => stats.ai_step_ms = value,
                38 => stats.skipped_silhouette_steps = value,
                39 => stats.characters_updated_pct = value,
                id => {
                    if stats.other_stats.len() < MAX_UNKNOWN_SIM_STATS {
                        stats.other_stats.push((id, entry.stat_value));
                    }
                }
            }
        }
        stats
    }
}

#[derive(Clone)]
pub struct SessionDiagnostics {
    pub object_cache: watch::Receiver<ObjectCacheStats>,
    pub inventory: watch::Receiver<InventoryFetchStats>,
    pub network: watch::Receiver<Vec<RegionNetworkHandle>>,
    pub sim_stats: watch::Receiver<HashMap<Region, RegionSimStats>>,
    pub session_queues: watch::Receiver<SessionQueuesSnapshot>,
    pub network_session: SessionHandle,
    pub inventory_fetch: InventoryFetchHandle,
}

#[derive(Clone)]
pub struct SessionInspection {
    pub world: WorldReader,
    pub assets: AssetHandle,
    pub render_stats: watch::Receiver<RenderStatsSnapshot>,
    pub viewport_queries: mpsc::Sender<ViewportQuery>,
    pub viewport_query_full: Arc<AtomicU64>,
    pub(crate) agent_motion: watch::Receiver<Option<AgentMotionSample>>,
    pub diagnostics: SessionDiagnostics,
}

impl SessionInspection {
    pub fn try_send_viewport_query(&self, query: ViewportQuery) -> Result<(), &'static str> {
        self.viewport_queries.try_send(query).map_err(|error| match error {
            TrySendError::Full(_) => {
                self.viewport_query_full.fetch_add(1, Ordering::Relaxed);
                "viewport query queue busy or closed"
            }
            TrySendError::Closed(_) => "viewport query queue busy or closed",
        })
    }

    pub fn viewport_query_queue_stats(&self) -> BoundedQueueStats {
        let capacity = self.viewport_queries.max_capacity();
        BoundedQueueStats {
            depth: capacity.saturating_sub(self.viewport_queries.capacity()),
            capacity,
            full_count: self.viewport_query_full.load(Ordering::Relaxed),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GpuInfo {
    pub name: String,
    pub backend: String,
    pub driver: String,
    pub driver_info: String,
    pub device_type: String,
    pub max_texture_dimension_2d: u32,
    pub max_buffer_size: u64,
    pub max_bind_groups: u32,
}

impl GpuInfo {
    pub fn from_wgpu(info: &AdapterInfo, limits: &Limits) -> Self {
        Self {
            name: info.name.clone(),
            backend: format!("{:?}", info.backend),
            driver: info.driver.clone(),
            driver_info: info.driver_info.clone(),
            device_type: format!("{:?}", info.device_type),
            max_texture_dimension_2d: limits.max_texture_dimension_2d,
            max_buffer_size: limits.max_buffer_size,
            max_bind_groups: limits.max_bind_groups,
        }
    }
}

#[derive(Clone)]
pub struct ViewerInspectionHandle {
    recent_logs: RecentLogs,
    session: watch::Receiver<Option<SessionInspection>>,
    settings: watch::Receiver<Option<Arc<SettingsStore>>>,
    log_control: watch::Receiver<Option<LogControl>>,
    gpu_info: watch::Receiver<Option<GpuInfo>>,
}

impl ViewerInspectionHandle {
    pub fn recent_logs(&self) -> &RecentLogs {
        &self.recent_logs
    }

    pub fn session(&self) -> Option<SessionInspection> {
        self.session.borrow().clone()
    }

    pub fn session_watch(&self) -> watch::Receiver<Option<SessionInspection>> {
        self.session.clone()
    }

    pub fn settings(&self) -> Option<Arc<SettingsStore>> {
        self.settings.borrow().clone()
    }

    pub fn log_control(&self) -> Option<LogControl> {
        self.log_control.borrow().clone()
    }

    pub fn gpu_info(&self) -> Option<GpuInfo> {
        self.gpu_info.borrow().clone()
    }
}

#[derive(Clone)]
pub struct ViewerInspectionPublisher {
    session: watch::Sender<Option<SessionInspection>>,
    settings: watch::Sender<Option<Arc<SettingsStore>>>,
    log_control: watch::Sender<Option<LogControl>>,
    gpu_info: watch::Sender<Option<GpuInfo>>,
}

impl ViewerInspectionPublisher {
    pub fn set_session(&self, session: Option<SessionInspection>) {
        self.session.send_replace(session);
    }

    pub fn set_settings(&self, settings: Arc<SettingsStore>) {
        self.settings.send_replace(Some(settings));
    }

    pub fn set_log_control(&self, control: LogControl) {
        self.log_control.send_replace(Some(control));
    }

    pub fn set_gpu_info(&self, info: GpuInfo) {
        self.gpu_info.send_replace(Some(info));
    }
}

pub fn inspection_channel(recent_logs: RecentLogs) -> (ViewerInspectionPublisher, ViewerInspectionHandle) {
    let (session_tx, session_rx) = watch::channel(None);
    let (settings_tx, settings_rx) = watch::channel(None);
    let (log_control_tx, log_control_rx) = watch::channel(None);
    let (gpu_info_tx, gpu_info_rx) = watch::channel(None);
    (
        ViewerInspectionPublisher { session: session_tx, settings: settings_tx, log_control: log_control_tx, gpu_info: gpu_info_tx },
        ViewerInspectionHandle {
            recent_logs,
            session: session_rx,
            settings: settings_rx,
            log_control: log_control_rx,
            gpu_info: gpu_info_rx,
        },
    )
}
