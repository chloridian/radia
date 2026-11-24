use std::sync::Arc;

use network::{CircuitMotionStatus, SimulatorId, capabilities::Client as CapabilityClient};
use render::{CaptureReadback, ObjectExplanation, PickDetail, PickError, Stats};
use tokio::sync::{mpsc, oneshot, watch};
use uuid::Uuid;
use world::{Entity, Region};

use crate::{
    asset_coordinator::AssetHandle,
    logging::{LogControl, RecentLogs},
    session::AgentMotionSample,
    settings::SettingsStore,
    world_reader::WorldReader,
};

pub const VIEWPORT_QUERY_CAPACITY: usize = 8;
pub const FRAME_SPIKE_CAPACITY: usize = 32;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct FrameSpike {
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
    pub mode: &'static str,
    pub position: Option<[f64; 3]>,
    pub direction: Option<[f32; 3]>,
    pub vertical_fov_deg: Option<f64>,
    pub draw_distance: Option<f64>,
    pub lod_factor: Option<f64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CameraModeRequest {
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
    pub worker_queue_capacity: usize,
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FailedInventoryFolder {
    pub folder_id: Uuid,
    pub owner_id: Uuid,
    pub retry_state: &'static str,
    pub error_kind: &'static str,
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
    pub capabilities: Option<CapabilityClient>,
}

#[derive(Clone)]
pub struct SessionDiagnostics {
    pub object_cache: watch::Receiver<ObjectCacheStats>,
    pub inventory: watch::Receiver<InventoryFetchStats>,
    pub network: watch::Receiver<Vec<RegionNetworkHandle>>,
}

#[derive(Clone)]
pub struct SessionInspection {
    pub world: WorldReader,
    pub assets: AssetHandle,
    pub render_stats: watch::Receiver<RenderStatsSnapshot>,
    pub viewport_queries: mpsc::Sender<ViewportQuery>,
    pub(crate) agent_motion: watch::Receiver<Option<AgentMotionSample>>,
    pub diagnostics: SessionDiagnostics,
}

#[derive(Clone)]
pub struct ViewerInspectionHandle {
    recent_logs: RecentLogs,
    session: watch::Receiver<Option<SessionInspection>>,
    settings: watch::Receiver<Option<Arc<SettingsStore>>>,
    log_control: watch::Receiver<Option<LogControl>>,
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
}

#[derive(Clone)]
pub struct ViewerInspectionPublisher {
    session: watch::Sender<Option<SessionInspection>>,
    settings: watch::Sender<Option<Arc<SettingsStore>>>,
    log_control: watch::Sender<Option<LogControl>>,
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
}

pub fn inspection_channel(recent_logs: RecentLogs) -> (ViewerInspectionPublisher, ViewerInspectionHandle) {
    let (session_tx, session_rx) = watch::channel(None);
    let (settings_tx, settings_rx) = watch::channel(None);
    let (log_control_tx, log_control_rx) = watch::channel(None);
    (
        ViewerInspectionPublisher { session: session_tx, settings: settings_tx, log_control: log_control_tx },
        ViewerInspectionHandle { recent_logs, session: session_rx, settings: settings_rx, log_control: log_control_rx },
    )
}
