use render::{CaptureReadback, ObjectExplanation, PickDetail, PickError, Stats};
use tokio::sync::{mpsc, oneshot, watch};
use uuid::Uuid;
use world::Entity;

use crate::{asset_coordinator::AssetHandle, logging::RecentLogs, world_reader::WorldReader};

pub const VIEWPORT_QUERY_CAPACITY: usize = 8;

#[derive(Clone, Copy, Default)]
pub struct RenderStatsSnapshot {
    pub frame_ms: f64,
    pub game_ms: f64,
    pub render_ms: f64,
    pub fps: f64,
    pub render_stats: Stats,
    pub gpu_timings: Option<(f64, f64)>,
    pub viewport_size: [u32; 2],
}

#[derive(Clone, Debug, Default)]
pub struct SceneQuerySummary {
    pub viewport_size: [u32; 2],
    pub scene_sequence: u64,
    pub ready: bool,
    pub has_camera: bool,
}

pub enum ViewportQuery {
    SceneQuery { reply: oneshot::Sender<SceneQuerySummary> },
    ExplainObject { entity: Entity, reply: oneshot::Sender<Option<ObjectExplanation>> },
    PixelPick { x: f32, y: f32, reply: oneshot::Sender<Result<Option<PickDetail>, PickError>> },
    BeginScreenshot { reply: oneshot::Sender<Option<CaptureReadback>> },
}

impl ViewportQuery {
    pub fn is_closed(&self) -> bool {
        match self {
            Self::SceneQuery { reply } => reply.is_closed(),
            Self::ExplainObject { reply, .. } => reply.is_closed(),
            Self::PixelPick { reply, .. } => reply.is_closed(),
            Self::BeginScreenshot { reply } => reply.is_closed(),
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
pub struct SessionDiagnostics {
    pub object_cache: watch::Receiver<ObjectCacheStats>,
    pub inventory: watch::Receiver<InventoryFetchStats>,
}

#[derive(Clone)]
pub struct SessionInspection {
    pub world: WorldReader,
    pub assets: AssetHandle,
    pub render_stats: watch::Receiver<RenderStatsSnapshot>,
    pub viewport_queries: mpsc::Sender<ViewportQuery>,
    pub diagnostics: SessionDiagnostics,
}

#[derive(Clone)]
pub struct ViewerInspectionHandle {
    recent_logs: RecentLogs,
    session: watch::Receiver<Option<SessionInspection>>,
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
}

#[derive(Clone)]
pub struct ViewerInspectionPublisher {
    session: watch::Sender<Option<SessionInspection>>,
}

impl ViewerInspectionPublisher {
    pub fn set_session(&self, session: Option<SessionInspection>) {
        self.session.send_replace(session);
    }
}

pub fn inspection_channel(recent_logs: RecentLogs) -> (ViewerInspectionPublisher, ViewerInspectionHandle) {
    let (session_tx, session_rx) = watch::channel(None);
    (ViewerInspectionPublisher { session: session_tx }, ViewerInspectionHandle { recent_logs, session: session_rx })
}
