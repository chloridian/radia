use std::{
    cell::Cell,
    rc::Rc,
    sync::{
        Arc, Weak,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

use cvars::{
    CAMERA_TURN_RATE_DEG_PER_SEC, LOG_LEVEL, LOG_NOISY_NETWORK, RENDER_DRAW_DISTANCE, RENDER_MAX_FPS, RENDER_MAX_TEXTURE_RESOLUTION,
    RENDER_SHOW_STATS, SCENE_LOD_FACTOR, ValueDomain,
};
use futures::{StreamExt, channel::mpsc as async_mpsc};
use prism::{
    Anchor, AnyElement, Bounds, Context, DevicePixels, DispatchPhase, Entity, FocusHandle, Focusable, Hsla, IntoElement, KeyDownEvent,
    KeyUpEvent, Modifiers, ModifiersChangedEvent, MouseButton, MouseDownEvent, MouseUpEvent, ObjectFit, Pixels, RawMouseMotionEvent,
    Render, ScrollDelta, ScrollWheelEvent, SharedString, Subscription, SurfaceSource, Window,
    assets::IconName as AssetIconName,
    canvas,
    component::{
        Disableable, Icon, IconName, IndexPath, Sizable, Theme, TitleBar,
        button::{Button, ButtonVariants as _},
        form::{field, v_form},
        popover::Popover,
        select::{Select, SelectEvent, SelectItem, SelectState},
        slider::{Slider, SliderEvent, SliderState},
        switch::Switch,
    },
    div,
    prelude::*,
    px, rgb, rgb_to_hsla, size, surface, white,
};
use prism_wgpu::WgpuContextHandle;
use render::{CompressionQuality, FrameInput, Renderer, TextureCompletion, WorkerBudget, WorkerKind, scene, scene::EnvironmentChoice};
use tokio::{
    runtime::Handle,
    sync::{mpsc as tokio_mpsc, watch},
};
use tracing::Instrument;
use utils::{
    Summary,
    hardware::{Pressure, Vram},
};
use uuid::Uuid;
use wgpu::{Device, Queue};

use super::{
    FRAME_INTERVAL, ViewportError,
    environment::{EnvOption, menu_from_items, sky_frame_deadline, sky_options, step_index, water_frame_deadline, water_options},
    timing::{
        FrameTimes, MetricStatus, format_bytes, format_count, fps_status, frame_time_status, next_frame_deadline, ping_status, vram_status,
    },
};
use crate::{
    agent_movement::{BodyRotation, HeldControls, MovementObservation, STOP},
    camera::{CameraConfig, CameraMode, FollowCamera, OrbitCamera, handle_keyboard_input},
    gui::{inspection, scene as gui_scene, window_geometry},
    inspection::{
        CameraModeRequest, CameraStateSnapshot, EnvironmentOverrideSummary, GpuInfo, RenderStatsSnapshot, SceneQuerySummary,
        SetCameraRequest, ViewportQuery,
    },
    inventory::environments::EnvironmentMenu,
    limits::{StartupLimits, parse_max_texture_resolution},
    logging::{LogControl, LogLevel},
    scene_feed::{SceneFeed, SceneSources},
    session::AgentMotionSample,
    settings::SettingsStore,
    worker_pool::WorkerPool,
};

const TEXTURE_DROP_REPORT_INTERVAL: Duration = Duration::from_secs(10);
const TITLE_BAR_HEIGHT: f32 = 40.0;
const VIEWPORT_SIDE_PADDING: f32 = 3.0;
const VIEWPORT_BOTTOM_PADDING: f32 = 3.0;
const VIEWPORT_CORNER_RADIUS: f32 = 4.0;
const DRAG_THRESHOLD_PX: f64 = 4.0;

pub(crate) struct SceneViewConfig {
    pub(crate) generation: u64,
    pub(crate) sources: SceneSources,
    pub(crate) agent_id: Uuid,
    pub(crate) camera: watch::Sender<Option<scene::Camera>>,
    pub(crate) movement: watch::Sender<Option<MovementObservation>>,
    pub(crate) agent_motion: watch::Receiver<Option<AgentMotionSample>>,
    pub(crate) runtime: Handle,
    pub(crate) log_control: LogControl,
    pub(crate) settings: Arc<SettingsStore>,
    pub(crate) startup_limits: StartupLimits,
    pub(crate) scene_budget: scene::Budget,
    pub(crate) pressure: watch::Receiver<Pressure>,
    pub(crate) render_stats: watch::Sender<RenderStatsSnapshot>,
    pub(crate) viewport_queries: tokio_mpsc::Receiver<ViewportQuery>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct LogLevelOption(LogLevel);

impl SelectItem for LogLevelOption {
    type Value = LogLevel;

    fn title(&self) -> SharedString {
        self.0.label().into()
    }

    fn value(&self) -> &Self::Value {
        &self.0
    }
}

struct DragState {
    travel: [f64; 2],
}

pub(crate) struct SceneView {
    generation: u64,
    focus_handle: FocusHandle,
    renderer: Option<Renderer>,
    feed: Option<SceneFeed>,
    wake_tx: async_mpsc::Sender<()>,
    texture_sender: mpsc::SyncSender<TextureCompletion>,
    texture_receiver: mpsc::Receiver<TextureCompletion>,
    dropped_textures: Arc<AtomicU64>,
    texture_completions_dropped_total: u64,
    texture_drops: Summary<u64>,
    viewport_bounds: Rc<Cell<Bounds<Pixels>>>,
    viewport_size: [u32; 2],
    scale_factor: f32,
    frame_times: FrameTimes,
    render_stats: watch::Sender<RenderStatsSnapshot>,
    next_frame: Instant,
    next_environment: Option<Instant>,
    next_sky: Option<Instant>,
    next_water: Option<Instant>,
    animation_redraw_at: Option<Instant>,
    timer_deadline: Option<Instant>,
    dirty: bool,
    ready: bool,
    failed: bool,
    settings: Arc<SettingsStore>,
    startup_limits: StartupLimits,
    worker_threads: usize,
    workers: WorkerBudget,
    worker_pool: WorkerPool,
    pressure: watch::Receiver<Pressure>,
    vram: Option<Vram>,
    lod_factor: f32,
    draw_distance: f32,
    max_texture_resolution: u32,
    camera: Option<OrbitCamera>,
    follow_camera: FollowCamera,
    camera_mode: CameraMode,
    controls: HeldControls,
    body_rotation: BodyRotation,
    stop_clear_pending: bool,
    last_camera: Option<scene::Camera>,
    modifiers: Modifiers,
    drag: Option<DragState>,
    lod_slider: Entity<SliderState>,
    draw_distance_slider: Entity<SliderState>,
    sky_select: Entity<SelectState<Vec<EnvOption>>>,
    water_select: Entity<SelectState<Vec<EnvOption>>>,
    sky_options_cache: Vec<EnvOption>,
    water_options_cache: Vec<EnvOption>,
    environment_menu: Option<Weak<EnvironmentMenu>>,
    show_stats: bool,
    log_level_select: Entity<SelectState<Vec<LogLevelOption>>>,
    log_control: LogControl,
    _subscriptions: Vec<Subscription>,
}

impl SceneView {
    pub(crate) fn new(config: SceneViewConfig, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        let (wake_tx, mut wake_rx) = async_mpsc::channel::<()>(1);
        let feed_wake = wake_tx.clone();
        let worker_threads = config.startup_limits.worker_threads;
        let workers = config.scene_budget.workers();
        let worker_pool = WorkerPool::new(worker_threads);
        let feed = SceneFeed::start(
            config.sources,
            config.agent_id,
            config.camera,
            config.movement,
            config.agent_motion,
            config.scene_budget,
            worker_pool.clone(),
            config.runtime,
            move || {
                let mut wake = feed_wake.clone();
                let _ = wake.try_send(());
            },
        );

        let (texture_sender, texture_receiver) = mpsc::sync_channel(worker_threads);
        let settings = config.settings;
        let snapshot = settings.snapshot();
        let lod_factor = snapshot.get(SCENE_LOD_FACTOR);
        let draw_distance = snapshot.get(RENDER_DRAW_DISTANCE);
        let max_texture_resolution = parse_max_texture_resolution(snapshot.get(RENDER_MAX_TEXTURE_RESOLUTION));
        let show_stats = snapshot.get(RENDER_SHOW_STATS);
        let camera_config = CameraConfig::from_snapshot(&snapshot);
        let (lod_min, lod_max, lod_step) = f32_slider_bounds(SCENE_LOD_FACTOR.descriptor().domain);
        let (draw_min, draw_max, draw_step) = f32_slider_bounds(RENDER_DRAW_DISTANCE.descriptor().domain);

        let lod_slider = cx.new(|_| SliderState::new().min(lod_min).max(lod_max).step(lod_step).default_value(lod_factor));
        let draw_distance_slider = cx.new(|_| SliderState::new().min(draw_min).max(draw_max).step(draw_step).default_value(draw_distance));

        let initial_sky = sky_options(None);
        let initial_water = water_options(None);
        let sky_select = cx.new(|cx| SelectState::new(initial_sky.clone(), Some(IndexPath::default().row(0)), window, cx));
        let water_select = cx.new(|cx| SelectState::new(initial_water.clone(), Some(IndexPath::default().row(0)), window, cx));

        let log_levels = LogLevel::ALL.into_iter().map(LogLevelOption).collect::<Vec<_>>();
        let current_level = config.log_control.level();
        let current_level_row = LogLevel::ALL.iter().position(|level| *level == current_level).unwrap_or(2);
        let log_level_select = cx.new(|cx| SelectState::new(log_levels, Some(IndexPath::default().row(current_level_row)), window, cx));

        let mut subscriptions = Vec::new();
        subscriptions.push(window_geometry::observe("scene", window, cx));
        subscriptions.push(cx.observe_window_activation(window, |this, window, cx| {
            if !window.is_window_active() {
                this.handle_focus_lost(window, cx);
            }
        }));
        subscriptions.push(cx.subscribe(&lod_slider, move |this, _, event: &SliderEvent, cx| {
            let SliderEvent::Change(value) = event else { return };
            let next = value.end().clamp(lod_min, lod_max);
            if (this.lod_factor - next).abs() > f32::EPSILON {
                if let Err(error) = this.settings.set_global(SCENE_LOD_FACTOR, next) {
                    tracing::warn!(%error, "failed to update scene.lod_factor CVar");
                    return;
                }
                this.lod_factor = next;
                if let Some(camera) = &mut this.camera {
                    camera.lod_factor = next;
                }
                this.dirty = true;
                cx.notify();
            }
        }));
        subscriptions.push(cx.subscribe(&draw_distance_slider, move |this, _, event: &SliderEvent, cx| {
            let SliderEvent::Change(value) = event else { return };
            let next = value.end().clamp(draw_min, draw_max);
            if (this.draw_distance - next).abs() > f32::EPSILON {
                if let Err(error) = this.settings.set_global(RENDER_DRAW_DISTANCE, next) {
                    tracing::warn!(%error, "failed to update render.draw_distance CVar");
                    return;
                }
                this.draw_distance = next;
                if let Some(camera) = &mut this.camera {
                    camera.draw_distance = next;
                }
                this.dirty = true;
                cx.notify();
            }
        }));
        subscriptions.push(cx.subscribe(&sky_select, |this, _, event: &SelectEvent<Vec<EnvOption>>, cx| {
            if let SelectEvent::Confirm(Some(choice)) = event
                && let Some(feed) = &this.feed
            {
                feed.set_sky_choice(choice.clone());
                this.dirty = true;
                cx.notify();
            }
        }));
        subscriptions.push(cx.subscribe(&water_select, |this, _, event: &SelectEvent<Vec<EnvOption>>, cx| {
            if let SelectEvent::Confirm(Some(choice)) = event
                && let Some(feed) = &this.feed
            {
                feed.set_water_choice(choice.clone());
                this.dirty = true;
                cx.notify();
            }
        }));
        subscriptions.push(cx.subscribe(&log_level_select, |this, _, event: &SelectEvent<Vec<LogLevelOption>>, cx| {
            if let SelectEvent::Confirm(Some(level)) = event {
                if let Err(error) = this.settings.set_global(LOG_LEVEL, level.directive()) {
                    tracing::warn!(%error, "failed to update log.level CVar");
                    return;
                }
                this.log_control.set_level(*level);
                cx.notify();
            }
        }));

        let mut viewport_queries = config.viewport_queries;
        cx.spawn_in(window, async move |this, cx| {
            while let Some(query) = viewport_queries.recv().await {
                if query.is_closed() {
                    continue;
                }
                if this.update(cx, |view, cx| view.handle_viewport_query(query, cx)).is_err() {
                    break;
                }
            }
        })
        .detach();

        cx.spawn_in(window, async move |this, cx| {
            while wake_rx.next().await.is_some() {
                if this
                    .update(cx, |view, cx| {
                        view.dirty = true;
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();

        let scale_factor = window.scale_factor();
        let initial_size = estimate_viewport_size(window);
        let mut follow_camera = FollowCamera::default();
        follow_camera.config = camera_config;

        let mut view = Self {
            generation: config.generation,
            focus_handle,
            renderer: None,
            feed: Some(feed),
            wake_tx,
            texture_sender,
            texture_receiver,
            dropped_textures: Arc::new(AtomicU64::new(0)),
            texture_completions_dropped_total: 0,
            texture_drops: Summary::new(TEXTURE_DROP_REPORT_INTERVAL),
            viewport_bounds: Rc::new(Cell::new(Bounds::default())),
            viewport_size: initial_size,
            scale_factor,
            frame_times: FrameTimes::default(),
            render_stats: config.render_stats,
            next_frame: Instant::now(),
            next_environment: None,
            next_sky: None,
            next_water: None,
            animation_redraw_at: None,
            timer_deadline: None,
            dirty: true,
            ready: false,
            failed: false,
            settings,
            startup_limits: config.startup_limits,
            worker_threads,
            workers,
            worker_pool,
            pressure: config.pressure,
            vram: None,
            lod_factor,
            draw_distance,
            max_texture_resolution,
            camera: None,
            follow_camera,
            camera_mode: CameraMode::Follow,
            controls: HeldControls::default(),
            body_rotation: BodyRotation::default(),
            stop_clear_pending: false,
            last_camera: None,
            modifiers: Modifiers::default(),
            drag: None,
            lod_slider,
            draw_distance_slider,
            sky_select,
            water_select,
            sky_options_cache: initial_sky,
            water_options_cache: initial_water,
            environment_menu: None,
            show_stats,
            log_level_select,
            log_control: config.log_control,
            _subscriptions: subscriptions,
        };
        view.init_renderer(window, cx);
        window.focus(&view.focus_handle, cx);
        view
    }

    pub(crate) fn retire(&mut self) {
        self.feed = None;
        if let Some(renderer) = self.renderer.take() {
            let retirement = renderer.retire();
            if let Err(error) = retirement.wait() {
                tracing::warn!(%error, "scene GPU retirement did not finish");
            }
        }
    }

    fn init_renderer(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.renderer.is_some() || self.failed {
            return;
        }
        let Some(gpu_box) = window.gpu_context() else {
            self.fail(ViewportError::GpuContextUnavailable, cx);
            return;
        };
        let Ok(context) = gpu_box.downcast::<(Arc<Device>, Arc<Queue>)>() else {
            self.fail(ViewportError::GpuContextUnavailable, cx);
            return;
        };
        let (device, queue) = *context;
        let Some(handle) = WgpuContextHandle::from_window(window) else {
            self.fail(ViewportError::GpuContextUnavailable, cx);
            return;
        };
        let format = handle.texture_format();
        let info = handle.adapter_info().clone();
        let vram = handle.adapter_memory().map(|memory| Vram { local_bytes: memory.local_bytes, budget_bytes: memory.budget_bytes });
        self.vram = vram;
        let render_limits = self.startup_limits.resolve_render(vram, &self.settings.snapshot());
        let limits = device.limits();
        let [width, height] = self.viewport_size;
        match Renderer::new(device, queue, info, format, width, height, render_limits, self.workers.clone()) {
            Ok(mut renderer) => {
                renderer.set_pressure(*self.pressure.borrow_and_update());
                let info = renderer.adapter_info();
                if let Some(publisher) = inspection(cx) {
                    publisher.set_gpu_info(GpuInfo::from_wgpu(info, &limits));
                }
                tracing::info!(
                    backend = ?info.backend,
                    gpu = %info.name,
                    driver = %info.driver,
                    driver_info = %info.driver_info,
                    vram_local_mib = vram.map(|v| v.local_bytes / (1024 * 1024)),
                    vram_budget_mib = vram.and_then(|v| v.budget_bytes).map(|b| b / (1024 * 1024)),
                    texture_soft_cap_mib = render_limits.texture_soft_cap_bytes / (1024 * 1024),
                    mesh_memory_mib = render_limits.mesh_bytes / (1024 * 1024),
                    texture_staging_mib = render_limits.texture_staging_bytes / (1024 * 1024),
                    worker_threads = render_limits.worker_threads,
                    "scene GPU initialized"
                );
                self.renderer = Some(renderer);
                self.dirty = true;
            }
            Err(error) => self.fail(ViewportError::Renderer(error), cx),
        }
    }

    fn fail(&mut self, error: ViewportError, cx: &mut Context<Self>) {
        if self.failed {
            return;
        }
        self.failed = true;
        let generation = self.generation;
        cx.defer(move |cx| gui_scene::scene_failed(generation, error, cx));
    }

    fn handle_focus_lost(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.release_drag(window);
        self.modifiers = Modifiers::default();
        let now = Instant::now();
        let (_, seed) = self.agent_pose_and_seed_at(now);
        let rotation = self.body_rotation.update(seed, 0.0, now);
        let movement = self.controls.release_and_observe(rotation);
        self.stop_clear_pending = movement.control_flags & STOP != 0;
        if let Some(feed) = &mut self.feed {
            feed.set_movement(movement);
        }
        self.dirty = true;
        cx.notify();
    }

    fn start_drag(&mut self, window: &mut Window) {
        if self.drag.is_some() {
            return;
        }
        window.capture_cursor();
        self.drag = Some(DragState { travel: [0.0; 2] });
    }

    fn release_drag(&mut self, window: &mut Window) {
        if self.drag.take().is_some() {
            window.release_cursor();
        }
    }

    fn drag_camera(&mut self, delta: [f64; 2], cx: &mut Context<Self>) {
        let Some(drag) = &mut self.drag else {
            return;
        };
        let scale = f64::from(self.scale_factor.max(1.0));
        drag.travel[0] += delta[0].abs() / scale;
        drag.travel[1] += delta[1].abs() / scale;
        if drag.travel[0] >= DRAG_THRESHOLD_PX || drag.travel[1] >= DRAG_THRESHOLD_PX {
            self.dirty = true;
            let dy = if drag.travel[1] >= DRAG_THRESHOLD_PX { delta[1] } else { 0.0 };
            let width = f64::from(self.viewport_size[0]);
            match self.camera_mode {
                CameraMode::Follow => {
                    self.follow_camera.drag([delta[0], dy], width, self.modifiers.control, self.modifiers.shift, self.draw_distance);
                }
                CameraMode::Free => {
                    if let Some(camera) = &mut self.camera {
                        camera.drag([delta[0], dy], width, self.modifiers.control, self.modifiers.shift);
                    }
                }
            }
            cx.notify();
        }
    }

    fn handle_mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if event.button != MouseButton::Left {
            return;
        }
        window.focus(&self.focus_handle, cx);
        self.modifiers = event.modifiers;
        if !event.modifiers.alt {
            self.start_drag(window);
            return;
        }
        let bounds = self.viewport_bounds.get();
        let local_x = f64::from(f32::from(event.position.x - bounds.origin.x)) * f64::from(self.scale_factor);
        let local_y = f64::from(f32::from(event.position.y - bounds.origin.y)) * f64::from(self.scale_factor);
        let point = {
            let (Some(renderer), Some(observation)) = (&self.renderer, self.last_camera) else {
                return;
            };
            match renderer.pick(observation, observation.ray([local_x, local_y], self.viewport_size)) {
                Ok(point) => point,
                Err(error) => {
                    tracing::debug!(%error, "scene focus pick exceeded its work limit");
                    return;
                }
            }
        };
        let Some(point) = point else { return };
        self.enter_free_camera();
        if let Some(camera) = &mut self.camera {
            camera.focus(point, Instant::now());
            self.start_drag(window);
            self.dirty = true;
            cx.notify();
        }
    }

    fn handle_scroll_wheel(&mut self, event: &ScrollWheelEvent, cx: &mut Context<Self>) {
        let clicks = match event.delta {
            ScrollDelta::Lines(delta) => f64::from(delta.y),
            ScrollDelta::Pixels(delta) => f64::from(f32::from(delta.y)) / 40.0,
        };
        let zoom = 2.0f64.powf(-clicks / 4.0);
        match self.camera_mode {
            CameraMode::Follow => self.follow_camera.zoom(zoom, self.draw_distance),
            CameraMode::Free => {
                if let Some(camera) = &mut self.camera {
                    camera.zoom(zoom);
                }
            }
        }
        self.dirty = true;
        cx.notify();
    }

    fn handle_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.modifiers = event.keystroke.modifiers;
        let key = event.keystroke.key.as_str();
        if !event.is_held && key.eq_ignore_ascii_case("f6") {
            let generation = self.generation;
            cx.defer(move |cx| gui_scene::request_chat(generation, cx));
            return;
        }
        if key.eq_ignore_ascii_case("escape") {
            self.release_drag(window);
            if self.camera_mode == CameraMode::Free {
                self.transition_to_follow();
                cx.notify();
            }
            return;
        }
        if !event.is_held && key.eq_ignore_ascii_case("v") {
            match self.camera_mode {
                CameraMode::Follow => self.enter_free_camera(),
                CameraMode::Free => self.transition_to_follow(),
            }
            self.dirty = true;
            cx.notify();
            return;
        }
        let was_free = self.camera_mode == CameraMode::Free;
        let Some(changed) = handle_keyboard_input(&mut self.controls, &mut self.camera_mode, key, true, event.is_held, false) else {
            return;
        };
        if changed {
            self.dirty = true;
        }
        if was_free && self.camera_mode == CameraMode::Follow {
            self.transition_to_follow();
        }
        if self.camera_mode == CameraMode::Follow {
            self.release_drag(window);
        }
        if changed {
            cx.notify();
        }
    }

    fn handle_key_up(&mut self, event: &KeyUpEvent, cx: &mut Context<Self>) {
        self.modifiers = event.keystroke.modifiers;
        let key = event.keystroke.key.as_str();
        if let Some(true) = handle_keyboard_input(&mut self.controls, &mut self.camera_mode, key, false, false, false) {
            self.dirty = true;
            cx.notify();
        }
    }

    fn enter_free_camera(&mut self) {
        if self.camera_mode == CameraMode::Free {
            return;
        }
        let Some(mut pose) = self.agent_pose() else { return };
        pose.rotation = self.body_rotation.current();
        let (position, focus) = self.follow_camera.current_position_and_focus(pose, Instant::now());
        self.camera = Some(OrbitCamera::from_follow_view(position, focus, self.draw_distance, self.lod_factor));
        self.camera_mode.detach_for_focus();
    }

    fn transition_to_follow(&mut self) {
        if let Some(camera) = &self.camera {
            let now = Instant::now();
            let (position, focus) = camera.current_position_and_focus(now);
            self.follow_camera.transition_from(position, focus, now);
        }
        self.camera_mode.return_to_follow();
        self.dirty = true;
    }

    fn ensure_camera(&mut self, pose: Option<world::WorldPose>) {
        if self.camera.is_none()
            && let Some(pose) = pose
        {
            let mut camera = OrbitCamera::new(pose.position);
            camera.draw_distance = self.draw_distance;
            camera.lod_factor = self.lod_factor;
            self.camera = Some(camera);
        }
    }

    fn publish_render_stats(&self) {
        let Some(renderer) = &self.renderer else { return };
        let metrics = self.frame_times.metrics();
        let snapshot = RenderStatsSnapshot {
            frame_ms: metrics.map_or(0.0, |m| m.frametime_ms),
            frame_p50_ms: metrics.map_or(0.0, |m| m.frame_p50_ms),
            frame_p99_ms: metrics.map_or(0.0, |m| m.frame_p99_ms),
            game_ms: metrics.map_or(0.0, |m| m.game_ms),
            render_ms: metrics.map_or(0.0, |m| m.render_ms),
            fps: metrics.map_or(0.0, |m| m.fps),
            render_stats: renderer.stats(),
            gpu_timings: renderer.gpu_timings(),
            viewport_size: self.viewport_size,
            frame_spikes: self.frame_times.spikes(),
            texture_completions_capacity: self.worker_threads,
            texture_completions_dropped: self.texture_completions_dropped_total,
            detail_batch_waits: self.feed.as_ref().map_or(0, |feed| feed.view.detail_batch_waits()),
            workers: self.workers.stats(),
        };
        self.render_stats.send_replace(snapshot);
    }

    fn handle_viewport_query(&mut self, query: ViewportQuery, cx: &mut Context<Self>) {
        if query.is_closed() {
            return;
        }
        match query {
            ViewportQuery::SceneQuery { reply } => {
                let summary = SceneQuerySummary {
                    viewport_size: self.viewport_size,
                    scene_sequence: self.feed.as_ref().map_or(0, |feed| feed.view.sequence()),
                    ready: self.ready,
                    has_camera: self.last_camera.is_some(),
                };
                drop(reply.send(summary));
            }
            ViewportQuery::ExplainObject { entity, reply } => {
                let result =
                    self.renderer.as_ref().zip(self.feed.as_ref()).map(|(renderer, feed)| renderer.explain_object(&feed.view, entity));
                drop(reply.send(result));
            }
            ViewportQuery::PixelPick { x, y, reply } => {
                let result = match (self.renderer.as_ref(), self.last_camera) {
                    (Some(renderer), Some(camera)) => {
                        let [width, height] = self.viewport_size;
                        if width == 0 || height == 0 {
                            Ok(None)
                        } else {
                            let ray = camera.ray([f64::from(x), f64::from(y)], self.viewport_size);
                            renderer.pick_detailed(camera, ray)
                        }
                    }
                    _ => Ok(None),
                };
                drop(reply.send(result));
            }
            ViewportQuery::BeginScreenshot { reply } => {
                let readback = self.renderer.as_ref().and_then(Renderer::begin_capture_rgba);
                drop(reply.send(readback));
            }
            ViewportQuery::EnvironmentOverride { reply } => {
                let summary = match self.feed.as_ref().map(|feed| feed.environment_choice()) {
                    Some(EnvironmentChoice::Local(_)) => {
                        EnvironmentOverrideSummary { local_override: true, local_sky: true, local_water: true }
                    }
                    Some(EnvironmentChoice::Split { sky, water }) => EnvironmentOverrideSummary {
                        local_override: sky.is_some() || water.is_some(),
                        local_sky: sky.is_some(),
                        local_water: water.is_some(),
                    },
                    Some(EnvironmentChoice::Shared) | None => EnvironmentOverrideSummary::default(),
                };
                reply.send(summary).ok();
            }
            ViewportQuery::CameraState { reply } => {
                let snapshot = self.camera_state_snapshot(self.last_camera);
                reply.send(snapshot).ok();
            }
            ViewportQuery::SetCamera { request, reply } => {
                let result = self.apply_set_camera(request, cx);
                reply.send(result).ok();
            }
        }
    }

    fn camera_state_snapshot(&self, camera: Option<scene::Camera>) -> CameraStateSnapshot {
        let mode = match self.camera_mode {
            CameraMode::Follow => CameraModeRequest::Follow,
            CameraMode::Free => CameraModeRequest::Free,
        };
        match camera {
            Some(camera) => {
                let position = camera.position();
                let direction = camera.direction();
                CameraStateSnapshot {
                    mode,
                    position: Some([position.x, position.y, position.z]),
                    direction: Some([direction.x, direction.y, direction.z]),
                    vertical_fov_deg: Some(camera.vertical_fov().to_degrees()),
                    draw_distance: Some(camera.draw_distance()),
                    lod_factor: Some(f64::from(self.lod_factor)),
                }
            }
            None => CameraStateSnapshot { mode, ..CameraStateSnapshot::default() },
        }
    }

    fn apply_set_camera(&mut self, request: SetCameraRequest, cx: &mut Context<Self>) -> Result<CameraStateSnapshot, String> {
        if request.mode.is_none() && request.position.is_none() && request.look_at.is_none() {
            return Err("provide at least one of mode, position, or look_at".to_owned());
        }
        if request.mode == Some(CameraModeRequest::Follow) && (request.position.is_some() || request.look_at.is_some()) {
            return Err("position and look_at cannot be set when mode is follow".to_owned());
        }
        if let Some(pos) = request.position
            && !pos.iter().all(|v| v.is_finite())
        {
            return Err("position coordinates must be finite".to_owned());
        }
        if let Some(look_at) = request.look_at
            && !look_at.iter().all(|v| v.is_finite())
        {
            return Err("look_at coordinates must be finite".to_owned());
        }

        let now = Instant::now();
        if request.mode == Some(CameraModeRequest::Follow) {
            self.transition_to_follow();
            let pose = self.agent_pose();
            let rotation = self.body_rotation.current();
            let camera = self.camera_observation(pose, rotation, now).or(self.last_camera);
            if let Some(camera) = camera {
                self.last_camera = Some(camera);
            }
            cx.notify();
            return Ok(self.camera_state_snapshot(camera));
        }

        let current = if self.camera_mode == CameraMode::Free
            && let Some(camera) = &self.camera
        {
            Some(camera.current_position_and_focus(now))
        } else if let Some(mut pose) = self.agent_pose() {
            pose.rotation = self.body_rotation.current();
            Some(self.follow_camera.current_position_and_focus(pose, now))
        } else {
            self.last_camera.map(|last| {
                let pos = last.position();
                (pos, pos + last.direction().as_dvec3() * 4.0)
            })
        };

        let position = request
            .position
            .map(math::DVec3::from_array)
            .or_else(|| current.map(|(pos, _)| pos))
            .ok_or_else(|| "camera position is unavailable; provide both position and look_at".to_owned())?;
        let focus = request
            .look_at
            .map(math::DVec3::from_array)
            .or_else(|| current.map(|(_, focus)| focus))
            .ok_or_else(|| "camera look_at is unavailable; provide both position and look_at".to_owned())?;
        if (focus - position).length() < 0.02 {
            return Err("position and look_at must be at least 0.02m apart".to_owned());
        }

        let mut orbit = OrbitCamera::from_follow_view(position, focus, self.draw_distance, self.lod_factor);
        orbit.config = self.follow_camera.config;
        let viewport = [self.viewport_size[0].max(1), self.viewport_size[1].max(1)];
        let scene_camera = orbit
            .scene_camera(viewport, now)
            .ok_or_else(|| "invalid camera pose".to_owned())?
            .with_max_texture_resolution(self.max_texture_resolution);

        self.camera = Some(orbit);
        self.camera_mode.detach_for_focus();
        self.last_camera = Some(scene_camera);
        self.dirty = true;
        cx.notify();
        Ok(self.camera_state_snapshot(Some(scene_camera)))
    }

    fn agent_pose(&mut self) -> Option<world::WorldPose> {
        self.feed.as_mut()?.camera_pose()
    }

    fn agent_pose_and_seed_at(&mut self, now: Instant) -> (Option<world::WorldPose>, Option<(u64, math::Quat)>) {
        let Some((pose, seed)) = self.feed.as_mut().and_then(|feed| feed.camera_pose_and_seed_at(now)) else {
            return (None, None);
        };
        (Some(pose), Some(seed))
    }

    fn camera_observation(&mut self, pose: Option<world::WorldPose>, rotation: math::Quat, now: Instant) -> Option<scene::Camera> {
        let viewport = self.viewport_size;
        let camera = match self.camera_mode {
            CameraMode::Follow => pose.and_then(|mut pose| {
                pose.rotation = rotation;
                self.follow_camera.follow(pose, viewport, self.draw_distance, self.lod_factor, now)
            }),
            CameraMode::Free => self.camera.as_ref().and_then(|camera| camera.scene_camera(viewport, now)),
        };
        camera.map(|camera| camera.with_max_texture_resolution(self.max_texture_resolution))
    }

    fn sync_environment_selects(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(feed) = &self.feed else { return };
        let items = feed.environment_items();
        let menu = menu_from_items(&items);
        let menu_changed = match (&self.environment_menu, menu) {
            (None, None) => false,
            (Some(previous), Some(current)) => !previous.upgrade().is_some_and(|prev| Arc::ptr_eq(&prev, current)),
            _ => true,
        };
        if menu_changed {
            self.environment_menu = menu.map(Arc::downgrade);
            let sky = sky_options(menu.map(Arc::as_ref));
            let water = water_options(menu.map(Arc::as_ref));
            let choice = feed.environment_choice();
            let selected_sky = choice.sky_choice().cloned();
            let selected_water = choice.water_choice().cloned();
            self.sky_options_cache = sky.clone();
            self.water_options_cache = water.clone();
            self.sky_select.update(cx, |state, cx| {
                state.set_items(sky, window, cx);
                state.set_selected_value(&selected_sky, window, cx);
            });
            self.water_select.update(cx, |state, cx| {
                state.set_items(water, window, cx);
                state.set_selected_value(&selected_water, window, cx);
            });
        }
    }

    fn step_sky(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        let current = self.feed.as_ref().and_then(|feed| feed.environment_choice().sky_choice().cloned());
        let index = self.sky_options_cache.iter().position(|opt| opt.choice == current).unwrap_or(0);
        let Some(next_index) = step_index(index, self.sky_options_cache.len(), delta) else { return };
        let next_choice = self.sky_options_cache[next_index].choice.clone();
        self.sky_select.update(cx, |state, cx| state.set_selected_value(&next_choice, window, cx));
        if let Some(feed) = &self.feed {
            feed.set_sky_choice(next_choice);
            self.dirty = true;
            cx.notify();
        }
    }

    fn step_water(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        let current = self.feed.as_ref().and_then(|feed| feed.environment_choice().water_choice().cloned());
        let index = self.water_options_cache.iter().position(|opt| opt.choice == current).unwrap_or(0);
        let Some(next_index) = step_index(index, self.water_options_cache.len(), delta) else { return };
        let next_choice = self.water_options_cache[next_index].choice.clone();
        self.water_select.update(cx, |state, cx| state.set_selected_value(&next_choice, window, cx));
        if let Some(feed) = &self.feed {
            feed.set_water_choice(next_choice);
            self.dirty = true;
            cx.notify();
        }
    }

    fn complete_texture_jobs(&mut self) {
        let dropped = self.dropped_textures.swap(0, Ordering::Relaxed);
        if dropped > 0 {
            self.texture_completions_dropped_total = self.texture_completions_dropped_total.saturating_add(dropped);
            if let Some(dropped) = self.texture_drops.record(Instant::now(), |total| *total = total.saturating_add(dropped)) {
                tracing::warn!(dropped, "texture completions dropped; queue full");
            }
        }
        if let Some(renderer) = &mut self.renderer {
            while let Ok(completion) = self.texture_receiver.try_recv() {
                renderer.complete_texture(completion);
                self.dirty = true;
            }
        }
    }

    fn dispatch_texture_jobs(&mut self) {
        let (Some(feed), Some(renderer)) = (&self.feed, &mut self.renderer) else {
            return;
        };
        while let Some(job) = renderer.next_texture_job() {
            let sender = self.texture_sender.clone();
            let dropped = Arc::clone(&self.dropped_textures);
            let mut wake = self.wake_tx.clone();
            let span = tracing::Span::current();
            let visible = job.is_visible();
            let workers = self.workers.clone();
            let work = self.worker_pool.spawn(visible, move || {
                let started = Instant::now();
                let completion = span.in_scope(|| job.run(CompressionQuality::Fast));
                workers.record_busy(WorkerKind::Texture, started.elapsed());
                completion
            });
            drop(
                feed.runtime.spawn(
                    async move {
                        match work.await {
                            Ok(completion) => match sender.try_send(completion) {
                                Ok(()) | Err(mpsc::TrySendError::Disconnected(_)) => {}
                                Err(mpsc::TrySendError::Full(_)) => {
                                    dropped.fetch_add(1, Ordering::Relaxed);
                                }
                            },
                            Err(error) => tracing::error!(%error, "texture worker stopped unexpectedly"),
                        }
                        let _ = wake.try_send(());
                    }
                    .in_current_span(),
                ),
            );
        }
    }

    fn step_frame(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.failed {
            return;
        }
        if window.gpu_device_lost() == Some(true) {
            self.fail(ViewportError::Renderer(render::Error::DeviceLost), cx);
            return;
        }
        self.scale_factor = window.scale_factor();
        if let Some(feed) = &mut self.feed {
            let before = feed.view.version();
            if let Err(error) = feed.drain() {
                self.fail(ViewportError::ScenePublication(error), cx);
                return;
            }
            self.dirty |= before != feed.view.version();
        }
        self.complete_texture_jobs();
        if self.pressure.has_changed().unwrap_or(false) {
            let level = *self.pressure.borrow_and_update();
            if let Some(renderer) = &mut self.renderer {
                renderer.set_pressure(level);
            }
        }

        let now = Instant::now();
        let snapshot = self.settings.snapshot();
        let camera_config = CameraConfig::from_snapshot(&snapshot);
        self.follow_camera.config = camera_config;
        if let Some(camera) = &mut self.camera {
            camera.config = camera_config;
        }
        let turn_rate = snapshot.get(CAMERA_TURN_RATE_DEG_PER_SEC).to_radians();
        let frame_interval = frame_interval_for_fps(snapshot.get(RENDER_MAX_FPS));
        let max_texture_resolution = parse_max_texture_resolution(snapshot.get(RENDER_MAX_TEXTURE_RESOLUTION));
        if self.max_texture_resolution != max_texture_resolution {
            self.max_texture_resolution = max_texture_resolution;
            self.dirty = true;
        }
        let texture_soft_cap_mib = self.startup_limits.resolve_texture_soft_cap_mib(self.vram, &snapshot);
        if let Some(renderer) = &mut self.renderer
            && renderer.texture_soft_cap_mib() != texture_soft_cap_mib
        {
            renderer.set_texture_soft_cap(texture_soft_cap_mib);
        }
        let (pose, seed) = self.agent_pose_and_seed_at(now);
        self.ensure_camera(pose);
        let rotation = self.body_rotation.update_with_turn_rate(seed, self.controls.yaw(self.modifiers.shift), turn_rate, now);
        let movement: MovementObservation = self.controls.observation(self.modifiers.shift, rotation);
        self.stop_clear_pending = movement.control_flags & STOP != 0;
        let observation = self.camera_observation(pose, rotation, now);
        let agent_pose = pose.map(|mut pose| {
            pose.rotation = rotation;
            pose
        });
        if let Some(feed) = &mut self.feed {
            feed.set_camera(observation);
            feed.set_movement(movement);
        }

        let mut gpu_pending = false;
        if let Some(renderer) = &mut self.renderer {
            match renderer.poll() {
                Ok(progress) => {
                    self.animation_redraw_at = progress.redraw_at;
                    self.dirty |= progress.redraw;
                    gpu_pending = progress.pending;
                }
                Err(error) => {
                    self.fail(ViewportError::Renderer(error), cx);
                    return;
                }
            }
            renderer.resize(self.viewport_size[0], self.viewport_size[1]);
        }

        if deadline_passed(self.next_environment, now) {
            self.next_environment = None;
            self.dirty = true;
        }
        if deadline_passed(self.next_sky, now) {
            self.next_sky = None;
            self.dirty = true;
        }
        if deadline_passed(self.next_water, now) {
            self.next_water = None;
            self.dirty = true;
        }

        if self.dirty && !self.viewport_size.contains(&0) {
            self.dirty = false;
            if let Some(feed) = &mut self.feed {
                feed.update_environment(now);
                self.next_environment = feed.next_environment();
                feed.refresh_motion_circuits(now);
            } else {
                self.next_environment = None;
            }
            let scheduled_deadline = self.next_frame;
            let scene = self.feed.as_ref().zip(observation).map(|(feed, camera)| (&feed.view, camera));
            let avatar_pose = self.feed.as_ref().zip(agent_pose).map(|(feed, pose)| scene::AgentPose::new(feed.agent_id(), pose));
            let frame_input = self.feed.as_ref().map_or_else(FrameInput::default, |feed| FrameInput {
                water_height: feed.water_height,
                environment: feed.evaluated(),
                cloud_scroll: feed.cloud_scroll(),
                animation_time: feed.animation_time(),
                water_scroll: feed.water_scroll(),
            });
            let draw_started = Instant::now();
            let game_work = draw_started.saturating_duration_since(now);
            let result = self.renderer.as_mut().map(|renderer| renderer.draw(scene, avatar_pose, frame_input));
            self.dispatch_texture_jobs();
            let completed = Instant::now();
            let render_work = completed.saturating_duration_since(draw_started);
            match result {
                Some(Ok(presented)) => {
                    let active_motion = self.controls.active() || self.stop_clear_pending;
                    self.dirty |= !presented
                        || active_motion
                        || self.feed.as_ref().is_some_and(|feed| feed.agent_motion_active(completed))
                        || (self.camera_mode == CameraMode::Follow
                            && agent_pose.is_some_and(|agent_pose| self.follow_camera.moving(agent_pose, completed)))
                        || (self.camera_mode == CameraMode::Free && self.camera.as_ref().is_some_and(|camera| camera.moving(completed)));
                    if presented {
                        let gpu_timings = self.renderer.as_ref().and_then(Renderer::gpu_timings);
                        self.frame_times.record(completed, game_work, render_work, gpu_timings);
                        self.publish_render_stats();
                        self.last_camera = observation;
                        self.next_frame =
                            if self.dirty { next_frame_deadline(scheduled_deadline, completed, frame_interval) } else { completed };
                        self.next_sky = sky_frame_deadline(
                            observation,
                            self.feed.as_ref().is_some_and(|feed| feed.clouds_animate() || feed.stars_animate()),
                            completed,
                        );
                        self.next_water = water_frame_deadline(
                            observation,
                            self.feed.as_ref().and_then(|feed| feed.water_height),
                            self.feed.as_ref().is_some_and(SceneFeed::water_animate),
                            completed,
                        );
                        if !self.ready {
                            self.ready = true;
                            let generation = self.generation;
                            cx.defer(move |cx| gui_scene::scene_ready(generation, cx));
                        }
                    } else {
                        self.next_frame = next_frame_deadline(scheduled_deadline, completed, frame_interval);
                        self.next_sky = None;
                        self.next_water = None;
                    }
                }
                Some(Err(error)) => {
                    self.fail(ViewportError::Renderer(error), cx);
                    return;
                }
                None => {}
            }
        }

        self.schedule_next_wake(gpu_pending, window, cx);
    }

    fn schedule_next_wake(&mut self, gpu_pending: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.dirty {
            window.request_animation_frame();
            return;
        }
        let now = Instant::now();
        let poll = gpu_pending.then_some(now + FRAME_INTERVAL);
        let next = [poll, self.animation_redraw_at, self.next_environment, self.next_sky, self.next_water].into_iter().flatten().min();
        let Some(deadline) = next else {
            self.timer_deadline = None;
            return;
        };
        if self.timer_deadline.is_some_and(|existing| existing <= deadline && existing > now) {
            return;
        }
        self.timer_deadline = Some(deadline);
        let delay = deadline.saturating_duration_since(now).max(Duration::from_millis(1));
        cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(delay).await;
            let _ = this.update(cx, |view, cx| {
                view.timer_deadline = None;
                view.dirty = true;
                cx.notify();
            });
        })
        .detach();
    }

    fn render_title_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let generation = self.generation;
        let view_entity = cx.entity();
        let lod_slider = self.lod_slider.clone();
        let draw_distance_slider = self.draw_distance_slider.clone();
        let sky_select = self.sky_select.clone();
        let water_select = self.water_select.clone();
        let log_level_select = self.log_level_select.clone();
        let lod_factor = self.lod_factor;
        let draw_distance = self.draw_distance;
        let show_stats = self.show_stats;
        let noisy_network = self.log_control.noisy_network();
        let can_select_env = self.feed.as_ref().is_some_and(|feed| feed.agent().is_some());

        let left = div().flex().items_center().gap_1().child(
            Button::new("open-nearby-chat")
                .ghost()
                .small()
                .icon(Icon::new(AssetIconName::MessageCircle))
                .tooltip("Nearby chat")
                .on_click(move |_, _, cx| cx.defer(move |cx| gui_scene::request_chat(generation, cx))),
        );

        let right = div().flex().items_center().gap_1().pr_2().child(
            Popover::new("scene-settings-popover")
                .anchor(Anchor::TopRight)
                .trigger(Button::new("scene-settings-button").ghost().small().icon(IconName::Settings).tooltip("Settings"))
                .content(move |_, _window, cx| {
                    let theme = Theme::global(cx).colors;
                    let view_for_sky_prev = view_entity.clone();
                    let view_for_sky_next = view_entity.clone();
                    let view_for_water_prev = view_entity.clone();
                    let view_for_water_next = view_entity.clone();
                    let view_for_stats = view_entity.clone();
                    let view_for_noisy = view_entity.clone();

                    v_form()
                        .small()
                        .w(px(300.))
                        .p_3()
                        .text_sm()
                        .text_color(theme.foreground)
                        .child(
                            field().label_indent(false).child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap_1()
                                    .child(
                                        div()
                                            .flex()
                                            .items_center()
                                            .justify_between()
                                            .child("LOD factor")
                                            .child(div().text_color(theme.muted_foreground).child(format!("{lod_factor:.2}"))),
                                    )
                                    .child(Slider::new(&lod_slider)),
                            ),
                        )
                        .child(
                            field().label_indent(false).child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap_1()
                                    .child(
                                        div()
                                            .flex()
                                            .items_center()
                                            .justify_between()
                                            .child("Draw distance")
                                            .child(div().text_color(theme.muted_foreground).child(format!("{draw_distance:.0} m"))),
                                    )
                                    .child(Slider::new(&draw_distance_slider)),
                            ),
                        )
                        .child(
                            field().label("Sky").child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_1()
                                    .child(div().flex_1().min_w_0().child(Select::new(&sky_select).small().disabled(!can_select_env)))
                                    .child(
                                        Disableable::disabled(
                                            Button::new("sky-prev").ghost().small().icon(IconName::ChevronLeft),
                                            !can_select_env,
                                        )
                                        .on_click(move |_, window, cx| {
                                            view_for_sky_prev.update(cx, |view, cx| view.step_sky(-1, window, cx));
                                        }),
                                    )
                                    .child(
                                        Disableable::disabled(
                                            Button::new("sky-next").ghost().small().icon(IconName::ChevronRight),
                                            !can_select_env,
                                        )
                                        .on_click(move |_, window, cx| {
                                            view_for_sky_next.update(cx, |view, cx| view.step_sky(1, window, cx));
                                        }),
                                    ),
                            ),
                        )
                        .child(
                            field().label("Water").child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_1()
                                    .child(div().flex_1().min_w_0().child(Select::new(&water_select).small().disabled(!can_select_env)))
                                    .child(
                                        Disableable::disabled(
                                            Button::new("water-prev").ghost().small().icon(IconName::ChevronLeft),
                                            !can_select_env,
                                        )
                                        .on_click(move |_, window, cx| {
                                            view_for_water_prev.update(cx, |view, cx| view.step_water(-1, window, cx));
                                        }),
                                    )
                                    .child(
                                        Disableable::disabled(
                                            Button::new("water-next").ghost().small().icon(IconName::ChevronRight),
                                            !can_select_env,
                                        )
                                        .on_click(move |_, window, cx| {
                                            view_for_water_next.update(cx, |view, cx| view.step_water(1, window, cx));
                                        }),
                                    ),
                            ),
                        )
                        .child(field().label_indent(false).child(
                            div().w_full().flex().items_center().justify_between().child("Show stats").child(
                                Switch::new("show-stats-toggle").checked(show_stats).on_click(move |checked, _, cx| {
                                    let checked = *checked;
                                    view_for_stats.update(cx, |view, cx| {
                                        if let Err(error) = view.settings.set_session(RENDER_SHOW_STATS, checked) {
                                            tracing::warn!(%error, "failed to update render.show_stats CVar");
                                            return;
                                        }
                                        view.show_stats = checked;
                                        cx.notify();
                                    });
                                }),
                            ),
                        ))
                        .child(field().label("Log level").child(Select::new(&log_level_select).small()))
                        .child(field().label_indent(false).child(
                            div().w_full().flex().items_center().justify_between().child("Noisy logs").child(
                                Switch::new("noisy-logs-toggle").checked(noisy_network).on_click(move |checked, _, cx| {
                                    let checked = *checked;
                                    view_for_noisy.update(cx, |view, cx| {
                                        if let Err(error) = view.settings.set_global(LOG_NOISY_NETWORK, checked) {
                                            tracing::warn!(%error, "failed to update log.noisy_network CVar");
                                            return;
                                        }
                                        view.log_control.set_noisy_network(checked);
                                        cx.notify();
                                    });
                                }),
                            ),
                        ))
                }),
        );

        TitleBar::new().h(px(TITLE_BAR_HEIGHT)).child(left).child(right)
    }

    fn render_stats_overlay(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.show_stats {
            return None;
        }
        let renderer = self.renderer.as_ref()?;
        let stats = renderer.stats();
        let gpu_ms = renderer.gpu_timings().map(|(scene_ms, post_ms)| scene_ms + post_ms);
        let metrics = self.frame_times.metrics();
        let vram_bytes = stats.vram_bytes();
        let vram_budget = renderer.vram_budget_bytes();
        let ram_bytes = self.feed.as_ref().map(|feed| {
            let cache_bytes = feed.assets.cache_stats().map_or(0, |cache| usize::try_from(cache.size_bytes).unwrap_or(usize::MAX));
            feed.budget.charged_bytes().saturating_add(cache_bytes)
        });
        let ping_ms = self.feed.as_ref().and_then(SceneFeed::ping_delay).map(|delay| delay.as_secs_f64() * 1000.0);

        let unavailable = rgb_to_hsla(rgb(0x9CA3AF));
        let info = white();

        let (fps_text, fps_color) = match metrics {
            Some(metrics) => (format!("{:.2} FPS", metrics.fps), status_color(fps_status(metrics.fps))),
            None => ("N/A".into(), unavailable),
        };
        let (frametime_text, frametime_color) = match metrics {
            Some(metrics) => (format!("{:.2} ms", metrics.frametime_ms), status_color(frame_time_status(metrics.frametime_ms))),
            None => ("N/A".into(), unavailable),
        };
        let (game_text, game_color) = match metrics {
            Some(metrics) => (format!("{:.2} ms", metrics.game_ms), status_color(frame_time_status(metrics.game_ms))),
            None => ("N/A".into(), unavailable),
        };
        let (render_text, render_color) = match metrics {
            Some(metrics) => (format!("{:.2} ms", metrics.render_ms), status_color(frame_time_status(metrics.render_ms))),
            None => ("N/A".into(), unavailable),
        };
        let (gpu_text, gpu_color) = match gpu_ms {
            Some(ms) => (format!("{ms:.2} ms"), status_color(frame_time_status(ms))),
            None => ("N/A".into(), unavailable),
        };
        let vram_text = format_bytes(vram_bytes);
        let vram_color = status_color(vram_status(vram_bytes, vram_budget));
        let (ram_text, ram_color) = match ram_bytes {
            Some(bytes) => (format_bytes(bytes), info),
            None => ("N/A".into(), unavailable),
        };
        let draws_text = stats.draw_calls.to_string();
        let tris_text = format_count(stats.triangles);
        let (ping_text, ping_color) = match ping_ms {
            Some(ms) => (format!("{ms:.0} ms"), status_color(ping_status(ms))),
            None => ("N/A".into(), unavailable),
        };

        let rows = [
            ("", fps_text, fps_color),
            ("", frametime_text, frametime_color),
            ("Game:", game_text, game_color),
            ("Render:", render_text, render_color),
            ("GPU:", gpu_text, gpu_color),
            ("VRAM:", vram_text, vram_color),
            ("RAM:", ram_text, ram_color),
            ("Draws:", draws_text, info),
            ("Tris:", tris_text, info),
            ("Ping:", ping_text, ping_color),
            ("Packet Loss:", "N/A".into(), unavailable),
        ];

        let mono = Theme::global(cx).mono_font_family.clone();
        Some(
            div()
                .absolute()
                .top(px(8.))
                .right(px(12.))
                .font_family(mono)
                .text_xs()
                .flex()
                .flex_col()
                .gap_0p5()
                .children(rows.into_iter().map(|(label, value, color)| {
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(div().w(px(84.)).text_right().text_color(info).child(label))
                        .child(div().min_w(px(72.)).text_color(color).child(value))
                }))
                .into_any_element(),
        )
    }
}

impl Focusable for SceneView {
    fn focus_handle(&self, _cx: &prism::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for SceneView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_environment_selects(window, cx);
        self.step_frame(window, cx);

        let theme = Theme::global(cx).colors;
        let surface_element = self.renderer.as_ref().and_then(Renderer::output_texture).map(|(texture, [width, height])| {
            surface(SurfaceSource::Texture { texture, size: size(DevicePixels(width as i32), DevicePixels(height as i32)) })
                .object_fit(ObjectFit::Fill)
                .rounded(px(VIEWPORT_CORNER_RADIUS))
                .size_full()
                .into_any_element()
        });

        let bounds_cell = Rc::clone(&self.viewport_bounds);
        let view_for_bounds = cx.entity().downgrade();
        let view_for_raw = cx.entity().downgrade();
        let view_for_up = cx.entity().downgrade();
        let tracker = canvas(
            move |bounds, window, cx| {
                bounds_cell.set(bounds);
                let scale = window.scale_factor();
                let width = (f32::from(bounds.size.width) * scale).round().max(0.0) as u32;
                let height = (f32::from(bounds.size.height) * scale).round().max(0.0) as u32;
                let _ = view_for_bounds.update(cx, |view, cx| {
                    if view.viewport_size != [width, height] {
                        view.viewport_size = [width, height];
                        view.dirty = true;
                        cx.notify();
                    }
                });
            },
            move |_, _, window, _| {
                window.on_mouse_event(move |event: &RawMouseMotionEvent, phase, _, cx| {
                    if phase == DispatchPhase::Bubble {
                        let _ = view_for_raw.update(cx, |view, cx| view.drag_camera([event.delta.x, event.delta.y], cx));
                    }
                });
                window.on_mouse_event(move |event: &MouseUpEvent, phase, window, cx| {
                    if phase == DispatchPhase::Bubble && event.button == MouseButton::Left {
                        let _ = view_for_up.update(cx, |view, _| view.release_drag(window));
                    }
                });
            },
        )
        .absolute()
        .inset_0()
        .size_full();

        let stats_overlay = self.render_stats_overlay(cx);

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.background)
            .text_color(theme.foreground)
            .on_modifiers_changed(cx.listener(|this, event: &ModifiersChangedEvent, _, _| {
                this.modifiers = event.modifiers;
            }))
            .child(self.render_title_bar(cx))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .pl(px(VIEWPORT_SIDE_PADDING))
                    .pr(px(VIEWPORT_SIDE_PADDING))
                    .pb(px(VIEWPORT_BOTTOM_PADDING))
                    .pt_0()
                    .child(
                        div()
                            .id("scene-viewport")
                            .track_focus(&self.focus_handle)
                            .relative()
                            .size_full()
                            .rounded(px(VIEWPORT_CORNER_RADIUS))
                            .overflow_hidden()
                            .bg(theme.background)
                            .on_mouse_down(MouseButton::Left, cx.listener(Self::handle_mouse_down))
                            .on_scroll_wheel(cx.listener(|this, event, _, cx| this.handle_scroll_wheel(event, cx)))
                            .on_key_down(cx.listener(Self::handle_key_down))
                            .on_key_up(cx.listener(|this, event, _, cx| this.handle_key_up(event, cx)))
                            .children(surface_element)
                            .child(tracker)
                            .children(stats_overlay),
                    ),
            )
    }
}

fn status_color(status: MetricStatus) -> Hsla {
    match status {
        MetricStatus::Green => rgb_to_hsla(rgb(0x4ADE80)),
        MetricStatus::Yellow => rgb_to_hsla(rgb(0xFACC15)),
        MetricStatus::Red => rgb_to_hsla(rgb(0xF87171)),
    }
}

fn estimate_viewport_size(window: &Window) -> [u32; 2] {
    let viewport = window.viewport_size();
    let scale = window.scale_factor();
    let width = ((f32::from(viewport.width) - 2.0 * VIEWPORT_SIDE_PADDING).max(0.0) * scale).round() as u32;
    let height = ((f32::from(viewport.height) - TITLE_BAR_HEIGHT - VIEWPORT_BOTTOM_PADDING).max(0.0) * scale).round() as u32;
    [width, height]
}

fn deadline_passed(deadline: Option<Instant>, now: Instant) -> bool {
    deadline.is_some_and(|deadline| now >= deadline)
}

fn f32_slider_bounds(domain: ValueDomain) -> (f32, f32, f32) {
    match domain {
        ValueDomain::F32 { min, max, step } => (min, max, step.unwrap_or(1.0)),
        _ => unreachable!("slider CVar must use f32 domain"),
    }
}

fn frame_interval_for_fps(max_fps: i64) -> Duration {
    u64::try_from(max_fps).ok().filter(|fps| *fps > 0).map_or(FRAME_INTERVAL, |fps| Duration::from_secs_f64(1.0 / fps as f64))
}
