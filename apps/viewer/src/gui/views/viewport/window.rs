use std::{
    error::Error,
    fmt,
    future::Future,
    ops::{Deref, DerefMut},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc,
        mpsc::TryRecvError,
    },
    time::{Duration, Instant},
};

use egui::{Context, DragValue, Slider, ViewportId};
use render::scene;
use render::{CompressionQuality, FrameInput, Overlay, Renderer, RendererInit, Retirement, TextureCompletion};
use tokio::{runtime::Handle, task::JoinHandle};
use tracing::Instrument;
use utils::Summary;
#[cfg(target_os = "linux")]
use winit::platform::startup_notify::{EventLoopExtStartupNotify, WindowAttributesExtStartupNotify, reset_activation_token_env};
use winit::{
    dpi::{LogicalSize, PhysicalPosition},
    event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow},
    keyboard::{Key, ModifiersState, NamedKey},
    window::{CursorGrabMode, UserAttentionType, Window, WindowId},
};

use super::{
    FRAME_INTERVAL, InputCapture, SharedWindowState,
    environment::{EnvironmentControls, sky_frame_deadline, water_frame_deadline},
    timing::{FrameTimes, next_frame_deadline, redraw_interval, refresh_interval},
};
use crate::{
    agent_movement::{BodyRotation, HeldControls, MovementObservation, STOP},
    camera::{CameraMode, DEFAULT_DRAW_DISTANCE, FollowCamera, OrbitCamera, handle_keyboard_input},
    scene_feed::SceneFeed,
};

const RECOVERY_DELAY: Duration = Duration::from_millis(500);
const TEXTURE_DROP_REPORT_INTERVAL: Duration = Duration::from_secs(10);
pub(super) const MAX_RECOVERIES: u32 = 3;

fn display_refresh_interval(window: &Window) -> Duration {
    let millihertz = window.current_monitor().and_then(|monitor| monitor.refresh_rate_millihertz());
    refresh_interval(millihertz, FRAME_INTERVAL)
}

#[derive(Debug)]
pub(crate) enum SceneWindowError {
    WindowCreation(winit::error::OsError),
    Renderer(render::Error),
    RendererInitializationStopped,
    RecoveryExhausted,
}

impl fmt::Display for SceneWindowError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WindowCreation(error) => fmt::Display::fmt(error, f),
            Self::Renderer(error) => fmt::Display::fmt(error, f),
            Self::RendererInitializationStopped => f.write_str("Scene renderer initialization stopped unexpectedly."),
            Self::RecoveryExhausted => f.write_str("The scene GPU could not recover after three attempts."),
        }
    }
}

impl Error for SceneWindowError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::WindowCreation(error) => Some(error),
            Self::Renderer(error) => Some(error),
            Self::RendererInitializationStopped | Self::RecoveryExhausted => None,
        }
    }
}

#[derive(Default)]
pub(super) struct DrawActions {
    pub(super) first_frame: bool,
    pub(super) request_chat: bool,
}

struct DebugUiFrame {
    overlay: Overlay,
    request_chat: bool,
    recreate_renderer: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RendererToken {
    generation: u64,
    attempt: u64,
}

type InitializationMessage<T, E> = (RendererToken, Result<T, E>);

struct PendingInitialization<T, E> {
    receiver: mpsc::Receiver<InitializationMessage<T, E>>,
    task: JoinHandle<()>,
}

impl<T, E> Drop for PendingInitialization<T, E> {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct WakeOnDrop<R, F: FnOnce()> {
    resource: Option<R>,
    wake: Option<F>,
}

impl<R, F: FnOnce()> Drop for WakeOnDrop<R, F> {
    fn drop(&mut self) {
        drop(self.resource.take());
        if let Some(wake) = self.wake.take() {
            wake();
        }
    }
}

fn spawn_initialization<T, E, F, W>(runtime: &Handle, token: RendererToken, initialize: F, wake: W) -> PendingInitialization<T, E>
where
    T: Send + 'static,
    E: Send + 'static,
    F: Future<Output = Result<T, E>> + Send + 'static,
    W: FnOnce() + Send + 'static,
{
    let (sender, receiver) = mpsc::sync_channel(1);
    let wake = WakeOnDrop { resource: Some(sender), wake: Some(wake) };
    let task = runtime.spawn(async move {
        let result = initialize.await;
        if let Err(completion) =
            wake.resource.as_ref().expect("completion sender remains owned until task completion").send((token, result))
        {
            drop(completion.0);
        }
        drop(wake);
    });
    PendingInitialization { receiver, task }
}

fn poll_initialization<T, E>(pending: &PendingInitialization<T, E>) -> Result<Option<InitializationMessage<T, E>>, SceneWindowError> {
    match pending.receiver.try_recv() {
        Ok(completion) => Ok(Some(completion)),
        Err(TryRecvError::Empty) => Ok(None),
        Err(TryRecvError::Disconnected) => Err(SceneWindowError::RendererInitializationStopped),
    }
}

enum RendererState<P, R> {
    Initializing { token: RendererToken, pending: P },
    Ready(R),
    Recovering { pending: Option<(RendererToken, P)> },
    Failed,
}

#[derive(Debug)]
enum InitializationOutcome<R, E> {
    InitialReady,
    RecoveryReady,
    InitialFailed(E),
    RecoveryFailed(E),
    Stale(Result<R, E>),
}

struct RendererLifecycle<P, R> {
    generation: u64,
    next_attempt: u64,
    state: RendererState<P, R>,
}

impl<P, R> RendererLifecycle<P, R> {
    fn initializing(generation: u64, pending: P) -> Self {
        let token = RendererToken { generation, attempt: 0 };
        Self { generation, next_attempt: 1, state: RendererState::Initializing { token, pending } }
    }

    fn is_initializing(&self) -> bool {
        matches!(&self.state, RendererState::Initializing { .. })
    }

    fn is_ready(&self) -> bool {
        matches!(&self.state, RendererState::Ready(_))
    }

    fn is_failed(&self) -> bool {
        matches!(&self.state, RendererState::Failed)
    }

    fn pending_token(&self) -> Option<RendererToken> {
        match &self.state {
            RendererState::Initializing { token, .. } => Some(*token),
            RendererState::Recovering { pending: Some((token, _)) } => Some(*token),
            RendererState::Ready(_) | RendererState::Recovering { pending: None } | RendererState::Failed => None,
        }
    }

    fn pending_mut(&mut self) -> Option<&mut P> {
        match &mut self.state {
            RendererState::Initializing { pending, .. } => Some(pending),
            RendererState::Recovering { pending: Some((_, pending)) } => Some(pending),
            RendererState::Ready(_) | RendererState::Recovering { pending: None } | RendererState::Failed => None,
        }
    }

    fn ready(&self) -> Option<&R> {
        match &self.state {
            RendererState::Ready(renderer) => Some(renderer),
            RendererState::Initializing { .. } | RendererState::Recovering { .. } | RendererState::Failed => None,
        }
    }

    fn ready_mut(&mut self) -> Option<&mut R> {
        match &mut self.state {
            RendererState::Ready(renderer) => Some(renderer),
            RendererState::Initializing { .. } | RendererState::Recovering { .. } | RendererState::Failed => None,
        }
    }

    fn recovery_token(&self) -> Option<RendererToken> {
        if matches!(&self.state, RendererState::Recovering { pending: None }) && self.next_attempt.checked_add(1).is_some() {
            Some(RendererToken { generation: self.generation, attempt: self.next_attempt })
        } else {
            None
        }
    }

    fn start_recovery(&mut self, token: RendererToken, pending: P) -> bool {
        if self.recovery_token() != Some(token) {
            return false;
        }
        let Some(next_attempt) = self.next_attempt.checked_add(1) else {
            return false;
        };
        self.next_attempt = next_attempt;
        self.state = RendererState::Recovering { pending: Some((token, pending)) };
        true
    }

    fn retire_ready(&mut self) -> Option<R> {
        let state = std::mem::replace(&mut self.state, RendererState::Failed);
        match state {
            RendererState::Ready(renderer) => {
                self.state = RendererState::Recovering { pending: None };
                Some(renderer)
            }
            state => {
                self.state = state;
                None
            }
        }
    }

    fn complete<E>(&mut self, token: RendererToken, result: Result<R, E>) -> InitializationOutcome<R, E> {
        let state = std::mem::replace(&mut self.state, RendererState::Failed);
        match state {
            RendererState::Initializing { token: expected, pending } => {
                drop(pending);
                if expected != token || token.generation != self.generation {
                    return InitializationOutcome::Stale(result);
                }
                match result {
                    Ok(renderer) => {
                        self.state = RendererState::Ready(renderer);
                        InitializationOutcome::InitialReady
                    }
                    Err(error) => InitializationOutcome::InitialFailed(error),
                }
            }
            RendererState::Recovering { pending: Some((expected, pending)) } => {
                drop(pending);
                if expected != token || token.generation != self.generation {
                    self.state = RendererState::Recovering { pending: None };
                    return InitializationOutcome::Stale(result);
                }
                match result {
                    Ok(renderer) => {
                        self.state = RendererState::Ready(renderer);
                        InitializationOutcome::RecoveryReady
                    }
                    Err(error) => {
                        self.state = RendererState::Recovering { pending: None };
                        InitializationOutcome::RecoveryFailed(error)
                    }
                }
            }
            RendererState::Ready(renderer) => {
                self.state = RendererState::Ready(renderer);
                InitializationOutcome::Stale(result)
            }
            RendererState::Recovering { pending: None } => {
                self.state = RendererState::Recovering { pending: None };
                InitializationOutcome::Stale(result)
            }
            RendererState::Failed => InitializationOutcome::Stale(result),
        }
    }

    fn close(&mut self) -> (Option<R>, Option<P>) {
        let state = std::mem::replace(&mut self.state, RendererState::Failed);
        match state {
            RendererState::Ready(renderer) => (Some(renderer), None),
            RendererState::Initializing { pending, .. } => (None, Some(pending)),
            RendererState::Recovering { pending } => (None, pending.map(|(_, pending)| pending)),
            RendererState::Failed => (None, None),
        }
    }
}

type SceneRendererPending = PendingInitialization<UnpublishedRenderer, SceneWindowError>;
type SceneRendererLifecycle = RendererLifecycle<SceneRendererPending, UnpublishedRenderer>;

fn retire_unpublished_renderer(renderer: Renderer, runtime: Handle) {
    let retirement = renderer.retire();
    drop(runtime.spawn_blocking(move || {
        if let Err(error) = retirement.wait() {
            tracing::warn!(%error, "stale scene GPU initialization did not retire");
        }
    }));
}

struct UnpublishedRenderer {
    renderer: Option<Renderer>,
    runtime: Handle,
}

impl UnpublishedRenderer {
    fn retire(mut self) -> Retirement {
        match self.renderer.take() {
            Some(renderer) => renderer.retire(),
            None => unreachable!(),
        }
    }
}

impl Deref for UnpublishedRenderer {
    type Target = Renderer;

    fn deref(&self) -> &Self::Target {
        match &self.renderer {
            Some(renderer) => renderer,
            None => unreachable!(),
        }
    }
}

impl DerefMut for UnpublishedRenderer {
    fn deref_mut(&mut self) -> &mut Self::Target {
        match &mut self.renderer {
            Some(renderer) => renderer,
            None => unreachable!(),
        }
    }
}

impl Drop for UnpublishedRenderer {
    fn drop(&mut self) {
        if let Some(renderer) = self.renderer.take() {
            retire_unpublished_renderer(renderer, self.runtime.clone());
        }
    }
}

fn start_renderer_initialization(
    init: RendererInit,
    runtime: &Handle,
    token: RendererToken,
    wake: Arc<SharedWindowState>,
) -> SceneRendererPending {
    let cleanup_runtime = runtime.clone();
    spawn_initialization(
        runtime,
        token,
        async move {
            init.finish()
                .await
                .map(|renderer| UnpublishedRenderer { renderer: Some(renderer), runtime: cleanup_runtime })
                .map_err(SceneWindowError::Renderer)
        },
        move || {
            wake.wake();
        },
    )
}

pub(super) struct SceneWindow {
    wake: Arc<SharedWindowState>,
    dirty: bool,
    retired: Vec<Retirement>,
    texture_sender: mpsc::SyncSender<TextureCompletion>,
    texture_receiver: mpsc::Receiver<TextureCompletion>,
    dropped_textures: Arc<AtomicU64>,
    texture_drops: Summary<u64>,
    feed: Option<SceneFeed>,
    renderer: SceneRendererLifecycle,
    runtime: Handle,
    texture_soft_cap: u32,
    egui: Context,
    platform: egui_winit::State,
    next_ui: Option<Instant>,
    next_environment: Option<Instant>,
    next_sky: Option<Instant>,
    next_water: Option<Instant>,
    window: Arc<Window>,
    generation: u64,
    frame_times: FrameTimes,
    animation_redraw_at: Option<Instant>,
    frame_interval: Duration,
    next_frame: Instant,
    dpi: f64,
    occluded: bool,
    focused: bool,
    ready: bool,
    recoveries: u32,
    recovery: PresentationRecovery,
    camera: Option<OrbitCamera>,
    follow_camera: FollowCamera,
    camera_mode: CameraMode,
    controls: HeldControls,
    body_rotation: BodyRotation,
    stop_clear_pending: bool,
    last_camera: Option<scene::Camera>,
    cursor: PhysicalPosition<f64>,
    modifiers: ModifiersState,
    drag: Option<DragState>,
    environment_controls: EnvironmentControls,
}

impl SceneWindow {
    pub(super) fn new(
        event_loop: &ActiveEventLoop,
        generation: u64,
        activate: bool,
        wake: Arc<SharedWindowState>,
        runtime: Handle,
    ) -> Result<Self, SceneWindowError> {
        let attributes = Window::default_attributes()
            .with_title("Radia")
            .with_inner_size(LogicalSize::new(1280, 720))
            .with_min_inner_size(LogicalSize::new(460, 300));
        #[cfg(target_os = "linux")]
        let attributes = {
            let token = event_loop.read_token_from_env();
            reset_activation_token_env();
            if activate {
                match token {
                    Some(token) => attributes.with_activation_token(token),
                    None => attributes,
                }
            } else {
                attributes
            }
        };
        let window = Arc::new(event_loop.create_window(attributes).map_err(SceneWindowError::WindowCreation)?);
        if activate {
            window.focus_window();
        }
        let (egui, platform) = debug_ui(&window);
        let dpi = window.scale_factor();
        let size = window.inner_size();
        let init = RendererInit::new(Arc::clone(&window), size.width, size.height).map_err(SceneWindowError::Renderer)?;
        let token = RendererToken { generation, attempt: 0 };
        let pending = start_renderer_initialization(init, &runtime, token, Arc::clone(&wake));
        if !window.has_focus() {
            window.request_user_attention(Some(UserAttentionType::Informational));
        }
        let now = Instant::now();
        let frame_interval = display_refresh_interval(&window);
        let (texture_sender, texture_receiver) = mpsc::sync_channel(2);
        Ok(Self {
            wake,
            dirty: true,
            retired: Vec::new(),
            texture_sender,
            texture_receiver,
            dropped_textures: Arc::new(AtomicU64::new(0)),
            texture_drops: Summary::new(TEXTURE_DROP_REPORT_INTERVAL),
            feed: None,
            renderer: RendererLifecycle::initializing(generation, pending),
            runtime,
            texture_soft_cap: 0,
            egui,
            platform,
            next_ui: None,
            next_environment: None,
            next_sky: None,
            next_water: None,
            window,
            generation,
            frame_times: FrameTimes::default(),
            animation_redraw_at: None,
            frame_interval,
            next_frame: now,
            dpi,
            occluded: false,
            focused: true,
            ready: false,
            recoveries: 0,
            recovery: PresentationRecovery::default(),
            camera: None,
            follow_camera: FollowCamera::default(),
            camera_mode: CameraMode::Follow,
            controls: HeldControls::default(),
            body_rotation: BodyRotation::default(),
            stop_clear_pending: false,
            last_camera: None,
            cursor: PhysicalPosition::new(0.0, 0.0),
            modifiers: ModifiersState::empty(),
            drag: None,
            environment_controls: EnvironmentControls::default(),
        })
    }

    pub(super) fn generation(&self) -> u64 {
        self.generation
    }

    pub(super) fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    pub(super) fn matches_window(&self, id: WindowId) -> bool {
        self.window.id() == id
    }

    pub(super) fn on_window_event(&mut self, event: &WindowEvent) -> bool {
        self.platform.on_window_event(&self.window, event).consumed
    }

    pub(super) fn wants_pointer_input(&self) -> bool {
        self.egui.egui_wants_pointer_input()
    }

    pub(super) fn wants_keyboard_input(&self) -> bool {
        self.egui.egui_wants_keyboard_input()
    }

    pub(super) fn handle_scene_input(&mut self, event: &WindowEvent, capture: InputCapture) -> bool {
        match event {
            WindowEvent::ModifiersChanged(modifiers) => {
                let modifiers = modifiers.state();
                if self.modifiers != modifiers {
                    self.modifiers = modifiers;
                    self.dirty = true;
                }
            }
            WindowEvent::Focused(focused) => {
                self.focused = *focused;
                if !focused {
                    self.release_drag(false);
                    self.modifiers = ModifiersState::empty();
                    let now = Instant::now();
                    let (_, seed) = self.agent_pose_and_seed_at(now);
                    let rotation = self.body_rotation.update(seed, 0.0, now);
                    let movement = self.controls.release_and_observe(rotation);
                    self.stop_clear_pending = movement.control_flags & STOP != 0;
                    if let Some(feed) = &mut self.feed {
                        feed.set_movement(movement);
                    }
                }
                self.dirty = true;
            }
            WindowEvent::CursorMoved { position, .. } => self.cursor = *position,
            WindowEvent::MouseInput { button: MouseButton::Left, state, .. } => {
                self.handle_left_click(*state, capture);
            }
            WindowEvent::MouseWheel { delta, .. } if !capture.consumed && !capture.pointer => {
                let clicks = match delta {
                    MouseScrollDelta::LineDelta(_, y) => f64::from(*y),
                    MouseScrollDelta::PixelDelta(position) => position.y / (40.0 * self.dpi),
                };
                let zoom = 2.0f64.powf(-clicks / 4.0);
                match self.camera_mode {
                    CameraMode::Follow => {
                        let draw_distance = self.camera.as_ref().map_or(DEFAULT_DRAW_DISTANCE, |camera| camera.draw_distance);
                        self.follow_camera.zoom(zoom, draw_distance);
                    }
                    CameraMode::Free => {
                        if let Some(camera) = &mut self.camera {
                            camera.zoom(zoom);
                        }
                    }
                }
                self.dirty = true;
            }
            WindowEvent::KeyboardInput { event, .. }
                if event.state == ElementState::Pressed
                    && event.logical_key == Key::Named(NamedKey::Escape)
                    && !capture.consumed
                    && !capture.keyboard =>
            {
                self.release_drag(true);
                if self.camera_mode == CameraMode::Free {
                    self.transition_to_follow();
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                let was_free = self.camera_mode == CameraMode::Free;
                let Some(changed) = handle_keyboard_input(
                    &mut self.controls,
                    &mut self.camera_mode,
                    &event.logical_key,
                    event.state,
                    event.repeat,
                    capture.consumed || capture.keyboard,
                ) else {
                    return false;
                };
                if changed {
                    self.dirty = true;
                }
                if was_free && self.camera_mode == CameraMode::Follow {
                    self.transition_to_follow();
                }
                if self.camera_mode == CameraMode::Follow {
                    self.release_drag(true);
                }
            }
            _ => return false,
        }
        true
    }

    pub(super) fn resize(&mut self, width: u32, height: u32) {
        if let Some(renderer) = self.renderer.ready_mut() {
            renderer.resize(width, height);
        }
    }

    pub(super) fn set_occluded(&mut self, occluded: bool) {
        self.occluded = occluded;
    }

    pub(super) fn update_feed(&mut self, incoming: Option<(u64, SceneFeed)>) -> Result<(), scene::ApplyError> {
        if let Some((generation, feed)) = incoming
            && generation == self.generation
        {
            self.feed = Some(feed);
            self.next_environment = None;
            self.next_sky = None;
            self.next_water = None;
            self.dirty = true;
        }
        if let Some(feed) = &mut self.feed {
            let before = feed.view.version();
            feed.drain()?;
            self.dirty |= before != feed.view.version();
        }
        Ok(())
    }

    pub(super) fn take_retirements(&mut self) -> Vec<Retirement> {
        let (renderer, pending) = self.renderer.close();
        if let Some(renderer) = renderer {
            self.retired.push(renderer.retire());
        }
        drop(pending);
        std::mem::take(&mut self.retired)
    }

    pub(super) fn take_retired(&mut self) -> Vec<Retirement> {
        std::mem::take(&mut self.retired)
    }

    fn release_drag(&mut self, restore: bool) {
        if let Some(drag) = self.drag.take() {
            if let Err(error) = self.window.set_cursor_grab(CursorGrabMode::None) {
                tracing::warn!(%error, "could not release scene cursor capture");
            }
            self.window.set_cursor_visible(true);
            if restore && let Err(error) = self.window.set_cursor_position(drag.origin) {
                tracing::warn!(%error, "could not restore scene cursor position");
            }
        }
    }

    fn handle_left_click(&mut self, state: ElementState, capture: InputCapture) {
        if state == ElementState::Released {
            self.release_drag(true);
            return;
        }
        if capture.consumed || capture.pointer {
            return;
        }

        if !self.modifiers.alt_key() {
            self.start_drag();
            return;
        }

        let point = {
            let (Some(renderer), Some(observation)) = (self.renderer.ready(), self.last_camera) else {
                return;
            };
            let size = self.window.inner_size();
            match renderer.pick(observation, observation.ray([self.cursor.x, self.cursor.y], [size.width, size.height])) {
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
            self.start_drag();
        }
    }

    pub(super) fn drag_camera(&mut self, (dx, dy): (f64, f64)) {
        let Some(drag) = &mut self.drag else {
            return;
        };
        drag.travel[0] += dx.abs() / self.dpi;
        drag.travel[1] += dy.abs() / self.dpi;
        if drag.travel[0] >= 4.0 || drag.travel[1] >= 4.0 {
            self.dirty = true;
            let dy = if drag.travel[1] >= 4.0 { dy } else { 0.0 };
            let width = f64::from(self.window.inner_size().width);
            match self.camera_mode {
                CameraMode::Follow => {
                    let draw_distance = self.camera.as_ref().map_or(DEFAULT_DRAW_DISTANCE, |camera| camera.draw_distance);
                    self.follow_camera.drag([dx, dy], width, self.modifiers.control_key(), self.modifiers.shift_key(), draw_distance);
                }
                CameraMode::Free => {
                    if let Some(camera) = &mut self.camera {
                        camera.drag([dx, dy], width, self.modifiers.control_key(), self.modifiers.shift_key());
                    }
                }
            }
        }
    }

    fn start_drag(&mut self) {
        if self.drag.is_some() {
            return;
        }
        let origin = self.cursor;
        let size = self.window.inner_size();
        let center = PhysicalPosition::new(f64::from(size.width) * 0.5, f64::from(size.height) * 0.5);
        match self.window.set_cursor_grab(CursorGrabMode::Confined) {
            Ok(()) => {
                self.window.set_cursor_visible(false);
                self.drag = Some(DragState { origin, travel: [0.0; 2] });
                if let Err(error) = self.window.set_cursor_position(center) {
                    self.release_drag(false);
                    tracing::warn!(%error, "could not position scene cursor");
                }
            }
            Err(error) => tracing::warn!(%error, "could not capture scene cursor"),
        }
    }

    fn enter_free_camera(&mut self) {
        if self.camera_mode == CameraMode::Free {
            return;
        }
        let Some(mut pose) = self.agent_pose() else { return };
        pose.rotation = self.body_rotation.current();
        let (position, focus) = self.follow_camera.current_position_and_focus(pose, Instant::now());
        let (draw_distance, lod_factor) =
            self.camera.as_ref().map_or((DEFAULT_DRAW_DISTANCE, 1.0), |camera| (camera.draw_distance, camera.lod_factor));
        self.camera = Some(OrbitCamera::from_follow_view(position, focus, draw_distance, lod_factor));
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
            self.camera = Some(OrbitCamera::new(pose.position));
        }
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
        let viewport = self.window.inner_size().into();
        match self.camera_mode {
            CameraMode::Follow => pose.and_then(|mut pose| {
                pose.rotation = rotation;
                let (draw_distance, lod_factor) =
                    self.camera.as_ref().map_or((DEFAULT_DRAW_DISTANCE, 1.0), |camera| (camera.draw_distance, camera.lod_factor));
                self.follow_camera.follow(pose, viewport, draw_distance, lod_factor, now)
            }),
            CameraMode::Free => self.camera.as_ref().and_then(|camera| camera.scene_camera(viewport, now)),
        }
    }

    pub(super) fn draw(&mut self) -> Result<DrawActions, SceneWindowError> {
        let now = Instant::now();
        let (pose, seed) = self.agent_pose_and_seed_at(now);
        self.ensure_camera(pose);
        let rotation = self.body_rotation.update(seed, self.controls.yaw(self.modifiers.shift_key()), now);
        let movement: MovementObservation = self.controls.observation(self.modifiers.shift_key(), rotation);
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
        if now < self.next_frame {
            return Ok(DrawActions::default());
        }
        self.dirty = false;
        if !self.ensure_renderer(now)? {
            return Ok(DrawActions::default());
        }
        if let Some(feed) = &mut self.feed {
            feed.update_environment(now);
            self.next_environment = feed.next_environment();
        } else {
            self.next_environment = None;
        }
        self.complete_texture_jobs();
        let debug_ui = self.draw_debug_ui(now);
        Ok(self.submit_frame(observation, agent_pose, now, debug_ui))
    }

    fn ensure_renderer(&mut self, now: Instant) -> Result<bool, SceneWindowError> {
        if self.renderer.is_ready() {
            return Ok(true);
        }
        if self.renderer.is_initializing() || self.renderer.pending_token().is_some() {
            return Ok(false);
        }
        if self.renderer.is_failed() {
            return Err(SceneWindowError::RendererInitializationStopped);
        }
        match self.recovery.begin_attempt(now) {
            RecoveryAttempt::Start => {}
            RecoveryAttempt::WaitUntil(retry_at) => {
                self.next_frame = retry_at;
                self.dirty = true;
                return Ok(false);
            }
            RecoveryAttempt::Exhausted => return Err(SceneWindowError::RecoveryExhausted),
        }
        let Some(token) = self.renderer.recovery_token() else {
            return Err(SceneWindowError::RecoveryExhausted);
        };
        let size = self.window.inner_size();
        let init = match RendererInit::new(Arc::clone(&self.window), size.width, size.height) {
            Ok(init) => init,
            Err(error) => {
                tracing::warn!(%error, attempt = self.recovery.attempts, "scene GPU recovery failed");
                self.schedule_recovery(now);
                return Ok(false);
            }
        };
        let pending = start_renderer_initialization(init, &self.runtime, token, Arc::clone(&self.wake));
        if !self.renderer.start_recovery(token, pending) {
            return Err(SceneWindowError::RendererInitializationStopped);
        }
        Ok(false)
    }

    fn finish_renderer_initialization(&mut self, now: Instant) -> Result<(), SceneWindowError> {
        if self.renderer.pending_token().is_none() {
            return Ok(());
        }
        let completion = {
            let Some(pending) = self.renderer.pending_mut() else {
                return Ok(());
            };
            let Some(completion) = poll_initialization(pending)? else {
                return Ok(());
            };
            completion
        };
        let initial = self.renderer.is_initializing();
        match self.renderer.complete(completion.0, completion.1) {
            InitializationOutcome::InitialReady => self.renderer_initialized(false)?,
            InitializationOutcome::RecoveryReady => self.renderer_initialized(true)?,
            InitializationOutcome::InitialFailed(error) => return Err(error),
            InitializationOutcome::RecoveryFailed(error) => {
                tracing::warn!(%error, attempt = self.recovery.attempts, "scene GPU recovery failed");
                self.schedule_recovery(now);
            }
            InitializationOutcome::Stale(result) => {
                if let Ok(renderer) = result {
                    self.retired.push(renderer.retire());
                }
                tracing::debug!("discarded stale scene GPU initialization");
                if initial {
                    return Err(SceneWindowError::RendererInitializationStopped);
                }
                self.schedule_recovery(now);
            }
        }
        Ok(())
    }

    fn renderer_initialized(&mut self, recovered: bool) -> Result<(), SceneWindowError> {
        let size = self.window.inner_size();
        let Some(renderer) = self.renderer.ready_mut() else {
            return Err(SceneWindowError::RendererInitializationStopped);
        };
        renderer.resize(size.width, size.height);
        let info = renderer.adapter_info();
        self.texture_soft_cap = renderer.texture_soft_cap_mib();
        if recovered {
            tracing::info!(backend = ?info.backend, gpu = %info.name, driver = %info.driver, driver_info = %info.driver_info, "scene GPU recovered");
            (self.texture_sender, self.texture_receiver) = mpsc::sync_channel(2);
            (self.egui, self.platform) = debug_ui(&self.window);
            self.recoveries += 1;
        } else {
            tracing::info!(backend = ?info.backend, gpu = %info.name, driver = %info.driver, driver_info = %info.driver_info, "scene GPU initialized");
        }
        self.dirty = true;
        Ok(())
    }

    fn complete_texture_jobs(&mut self) {
        let dropped = self.dropped_textures.swap(0, Ordering::Relaxed);
        if dropped > 0
            && let Some(dropped) = self.texture_drops.record(Instant::now(), |total| *total = total.saturating_add(dropped))
        {
            tracing::warn!(dropped, "texture completions dropped; queue full");
        }
        if let Some(renderer) = self.renderer.ready_mut() {
            while let Ok(completion) = self.texture_receiver.try_recv() {
                renderer.complete_texture(completion);
            }
        }
    }

    fn draw_debug_ui(&mut self, now: Instant) -> DebugUiFrame {
        let renderer = self.renderer.ready().expect("renderer ensured before debug UI");
        let stats = renderer.stats();
        let gpu_timings = renderer.gpu_timings();
        let gpu_timing_supported = renderer.gpu_timing_supported();
        let frame_times = self.frame_times.summary();
        self.dpi = self.window.scale_factor();
        let input = self.platform.take_egui_input(&self.window);
        let mut request_chat = false;
        let mut recreate_renderer = false;
        let output = self.egui.run_ui(input, |root| {
            egui::Window::new("Rendering").default_width(400.0).show(root.ctx(), |ui| {
                if let Some(camera) = &mut self.camera {
                    ui.add(Slider::new(&mut camera.draw_distance, 32.0..=1024.0).text("Draw distance").suffix(" m"));
                    ui.add(Slider::new(&mut camera.lod_factor, 0.25..=4.0).text("LOD factor"))
                        .on_hover_text("Higher values keep more detailed prim geometry at a distance.");
                    if ui.button("Reset settings").clicked() {
                        camera.draw_distance = DEFAULT_DRAW_DISTANCE;
                        camera.lod_factor = 1.0;
                    }
                }
                ui.label(format!("Camera: {} (V)", self.camera_mode.label()));
                if let Some(times) = &frame_times {
                    ui.label(format!(
                        "Frametime: p50 {:.2} ms, p95 {:.2} ms",
                        times.p50.as_secs_f64() * 1000.0,
                        times.p95.as_secs_f64() * 1000.0
                    ));
                } else {
                    ui.label("Frametime: no samples");
                }
                self.environment_controls.draw(ui, self.feed.as_ref());
                ui.label(format!(
                    "Draw submissions: {}, indexed commands: {}, instances: {}",
                    stats.draw_calls, stats.draw_commands, stats.instances
                ));
                ui.collapsing("Diagnostics", |ui| {
                    ui.horizontal(|ui| {
                        ui.add(DragValue::new(&mut self.texture_soft_cap).speed(16).suffix(" MiB"))
                            .on_hover_text("Soft GPU backstop. Textures used by the current frame stay resident, even above this value.");
                        ui.label("Texture soft cap");
                    });
                    ui.label(format!("CPU prepare: {:.2} ms", stats.prepare_ms));
                    ui.label(format!(
                        "Prepare work: layouts {}, items updated {}, faces patched {}, blended sorts {}",
                        stats.layouts, stats.items_updated, stats.faces_patched, stats.blended_sorts
                    ));
                    ui.label(format!(
                        "Animated texture faces: {} visited, {} changed",
                        stats.animated_faces_visited, stats.animated_faces_changed
                    ));
                    if let Some((scene_ms, diagnostics_ms)) = gpu_timings {
                        ui.label(format!("GPU passes: scene {:.2} ms, diagnostics {:.2} ms", scene_ms, diagnostics_ms));
                    } else if gpu_timing_supported {
                        ui.label("GPU pass timings: collecting");
                    }
                    ui.label(format!("GPU meshes: {:.1} MiB, pending: {}", stats.mesh_bytes as f64 / 1048576.0, stats.pending_meshes));
                    ui.label(format!(
                        "GPU textures: {}, {:.1} MiB, pending: {}",
                        stats.texture_count,
                        stats.texture_bytes as f64 / 1048576.0,
                        stats.pending_textures
                    ));
                    ui.label(format!(
                        "Texture staging: {:.1} MiB, upload: {:.1} MiB",
                        stats.staging_bytes as f64 / 1048576.0,
                        stats.texture_upload_bytes as f64 / 1048576.0
                    ));
                    if stats.omitted_faces > 0 {
                        let reason = match stats.omission_reason {
                            Some(render::OmissionReason::InstanceBufferBudget) => {
                                "GPU instance buffer budget; nearest and largest objects kept first"
                            }
                            Some(render::OmissionReason::MeshMemoryBudget) => {
                                "GPU mesh memory budget; nearest and largest meshes kept first"
                            }
                            Some(render::OmissionReason::CombinedBudgets) => {
                                "GPU mesh and instance budgets; nearest and largest content kept first"
                            }
                            None => "unknown renderer limit",
                        };
                        ui.label(format!("Faces omitted: {} ({reason})", stats.omitted_faces));
                    }
                    if stats.mesh_budget_blocked > 0 {
                        ui.label(format!("Meshes blocked by GPU memory budget: {}", stats.mesh_budget_blocked));
                    }
                    ui.label(format!("GPU depth: {:.1} MiB", stats.depth_bytes as f64 / 1048576.0));
                    if let Some(feed) = &self.feed {
                        ui.label(format!(
                            "Scene records: {}, regions: {}, terrain patches: {}",
                            feed.view.tracked_object_count(),
                            feed.view.region_count(),
                            feed.view.patch_count()
                        ));
                        if let Some(revision) = feed.view.revision() {
                            ui.label(format!("World revision: {}", revision.get()));
                        }
                        ui.label(format!(
                            "Scene: {:.1} MiB, peak {:.1} MiB",
                            feed.budget.charged_bytes() as f64 / 1048576.0,
                            feed.budget.high_water_bytes() as f64 / 1048576.0
                        ));
                        if let Some(cache) = feed.assets.cache_stats() {
                            ui.label(format!(
                                "Asset cache: {:.1}/{:.1} MiB, protected {:.1} MiB",
                                cache.size_bytes as f64 / 1048576.0,
                                cache.size_limit_bytes as f64 / 1048576.0,
                                cache.protected_bytes as f64 / 1048576.0
                            ));
                            ui.label(format!(
                                "Cached: {} textures, {} PBR, {} legacy, {} environments, {} meshes",
                                cache.entries_by_kind.textures,
                                cache.entries_by_kind.pbr_materials,
                                cache.entries_by_kind.legacy_materials,
                                cache.entries_by_kind.environments,
                                cache.entries_by_kind.meshes
                            ));
                            let reasons = cache
                                .recent_evictions
                                .iter()
                                .filter_map(|reason| {
                                    reason.map(|reason| match reason {
                                        assets::cache::CacheEvictionReason::Infrequent => "infrequent",
                                        assets::cache::CacheEvictionReason::Stale => "stale",
                                        assets::cache::CacheEvictionReason::LowReconstructionCost => "low decode cost",
                                        assets::cache::CacheEvictionReason::LeastRecentlyUsed => "least recent",
                                        assets::cache::CacheEvictionReason::LowUtility => "low utility",
                                    })
                                })
                                .collect::<Vec<_>>()
                                .join(", ");
                            if !reasons.is_empty() {
                                ui.label(format!("Recent evictions: {reasons}"));
                            }
                        }
                    }
                    if self.recoveries > 0 {
                        ui.label(format!("GPU recreations: {}", self.recoveries));
                    }
                    recreate_renderer = ui.button("Recreate GPU resources").clicked();
                });
                request_chat = ui.button("Nearby chat (F6)").clicked();
            });
        });
        self.next_ui = output.viewport_output.get(&ViewportId::ROOT).and_then(|viewport| now.checked_add(viewport.repaint_delay));
        self.platform.handle_platform_output(&self.window, output.platform_output);
        if self.drag.is_some() {
            self.window.set_cursor_visible(false);
        }
        DebugUiFrame {
            overlay: Overlay {
                primitives: self.egui.tessellate(output.shapes, output.pixels_per_point),
                textures: output.textures_delta,
                pixels_per_point: output.pixels_per_point,
            },
            request_chat,
            recreate_renderer,
        }
    }

    fn submit_frame(
        &mut self,
        observation: Option<scene::Camera>,
        agent_pose: Option<world::WorldPose>,
        now: Instant,
        ui: DebugUiFrame,
    ) -> DrawActions {
        let scheduled_deadline = self.next_frame;
        if let Some(feed) = &mut self.feed {
            feed.refresh_motion_circuits(now);
        }
        let scene = self.feed.as_ref().zip(observation).map(|(feed, camera)| (&feed.view, camera));
        let avatar_pose = self.feed.as_ref().zip(agent_pose).map(|(feed, pose)| scene::AgentPose::new(feed.agent_id(), pose));
        let frame_input = self.feed.as_ref().map_or_else(FrameInput::default, |feed| FrameInput {
            water_height: feed.water_height,
            environment: feed.evaluated(),
            cloud_scroll: feed.cloud_scroll(),
            animation_time: feed.animation_time(),
            water_scroll: feed.water_scroll(),
        });
        let result = {
            let renderer = self.renderer.ready_mut().expect("renderer ensured before frame submission");
            renderer.set_texture_soft_cap(self.texture_soft_cap);
            renderer.draw(ui.overlay, scene, avatar_pose, frame_input, || self.window.pre_present_notify())
        };
        self.dispatch_texture_jobs();
        let completed = Instant::now();
        let frame_work = completed.saturating_duration_since(now);
        match result {
            Ok(presented) => {
                let active_motion = self.controls.active() || self.stop_clear_pending;
                self.dirty |= !presented
                    || active_motion
                    || self.feed.as_ref().is_some_and(|feed| feed.agent_motion_active(Instant::now()))
                    || (self.camera_mode == CameraMode::Follow
                        && agent_pose.is_some_and(|agent_pose| self.follow_camera.moving(agent_pose, Instant::now())))
                    || (self.camera_mode == CameraMode::Free && self.camera.as_ref().is_some_and(|camera| camera.moving(Instant::now())));
                if presented {
                    self.frame_times.record(frame_work);
                    self.last_camera = observation;
                    self.recovery.presented();
                    if self.dirty {
                        self.next_frame = next_frame_deadline(scheduled_deadline, completed, self.frame_interval);
                    } else {
                        self.next_frame = completed;
                    }
                    self.next_sky = sky_frame_deadline(
                        observation,
                        self.feed.as_ref().is_some_and(|feed| feed.clouds_animate() || feed.stars_animate()),
                        Instant::now(),
                    );
                    self.next_water = water_frame_deadline(
                        observation,
                        self.feed.as_ref().and_then(|feed| feed.water_height),
                        self.feed.as_ref().is_some_and(SceneFeed::water_animate),
                        Instant::now(),
                    );
                } else {
                    self.next_frame = next_frame_deadline(scheduled_deadline, completed, self.frame_interval);
                    self.next_sky = None;
                    self.next_water = None;
                }
                let first_frame = presented && !self.ready;
                self.ready |= presented;
                if ui.recreate_renderer {
                    self.retire_renderer();
                    self.schedule_recovery(now);
                }
                DrawActions { first_frame, request_chat: ui.request_chat }
            }
            Err(error) => {
                tracing::warn!(%error, "scene presentation failed; scheduling GPU recovery");
                self.retire_renderer();
                self.schedule_recovery(now);
                DrawActions { first_frame: false, request_chat: ui.request_chat }
            }
        }
    }

    fn dispatch_texture_jobs(&mut self) {
        if let Some(feed) = &self.feed {
            let Some(renderer) = self.renderer.ready_mut() else {
                return;
            };
            while let Some(job) = renderer.next_texture_job() {
                let sender = self.texture_sender.clone();
                let dropped = Arc::clone(&self.dropped_textures);
                let wake = Arc::clone(&self.wake);
                let span = tracing::Span::current();
                let work = feed.runtime.spawn_blocking(move || span.in_scope(|| job.run(CompressionQuality::Fast)));
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
                            wake.wake();
                        }
                        .in_current_span(),
                    ),
                );
            }
        }
    }

    pub(super) fn poll_renderer(&mut self, now: Instant) -> Result<bool, SceneWindowError> {
        self.finish_renderer_initialization(now)?;
        let Some(renderer) = self.renderer.ready_mut() else {
            self.animation_redraw_at = None;
            return Ok(false);
        };
        match renderer.poll() {
            Ok(progress) => {
                self.animation_redraw_at = progress.redraw_at;
                self.dirty |= progress.redraw;
                Ok(progress.pending)
            }
            Err(error) => {
                self.animation_redraw_at = None;
                tracing::warn!(%error, "scene GPU failed; scheduling recovery");
                self.retire_renderer();
                self.schedule_recovery(now);
                Ok(true)
            }
        }
    }

    fn schedule_recovery(&mut self, now: Instant) {
        self.next_frame = self.recovery.schedule_retry(now);
        self.dirty = true;
    }

    pub(super) fn schedule_redraw(&mut self, event_loop: &ActiveEventLoop, pending: bool, can_redraw: bool) {
        if self.occluded || self.window.inner_size().width == 0 || self.window.inner_size().height == 0 {
            event_loop.set_control_flow(if pending {
                ControlFlow::WaitUntil(Instant::now() + self.frame_interval)
            } else {
                ControlFlow::Wait
            });
            return;
        }
        let display_interval = display_refresh_interval(&self.window);
        self.frame_interval = redraw_interval(display_interval, self.focused, self.controls.active() || self.stop_clear_pending);
        let now = Instant::now();
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
        if self.next_ui.is_some_and(|at| now >= at) {
            self.next_ui = None;
            self.dirty = true;
        }
        if self.dirty && now >= self.next_frame && can_redraw {
            self.window.request_redraw();
            event_loop.set_control_flow(ControlFlow::Wait);
        } else {
            let redraw = (self.dirty && can_redraw).then_some(self.next_frame.max(now + Duration::from_millis(1)));
            let poll = pending.then_some(now + self.frame_interval);
            event_loop.set_control_flow(
                match redraw
                    .into_iter()
                    .chain(poll)
                    .chain(self.animation_redraw_at)
                    .chain(self.next_ui)
                    .chain(self.next_environment)
                    .chain(self.next_sky)
                    .chain(self.next_water)
                    .min()
                {
                    Some(at) => ControlFlow::WaitUntil(at),
                    None => ControlFlow::Wait,
                },
            );
        }
    }

    fn retire_renderer(&mut self) {
        if let Some(renderer) = self.renderer.retire_ready() {
            self.retired.push(renderer.retire());
        }
    }
}

fn deadline_passed(deadline: Option<Instant>, now: Instant) -> bool {
    deadline.is_some_and(|deadline| now >= deadline)
}

impl Drop for SceneWindow {
    fn drop(&mut self) {
        self.release_drag(false);
        let dropped = self.texture_drops.flush().unwrap_or(0).saturating_add(self.dropped_textures.swap(0, Ordering::Relaxed));
        if dropped > 0 {
            tracing::warn!(dropped, "texture completions dropped; queue full");
        }
    }
}

struct DragState {
    origin: PhysicalPosition<f64>,
    travel: [f64; 2],
}

#[derive(Debug, PartialEq, Eq)]
enum RecoveryAttempt {
    Start,
    WaitUntil(Instant),
    Exhausted,
}

#[derive(Default)]
struct PresentationRecovery {
    attempts: u32,
    retry_at: Option<Instant>,
}

impl PresentationRecovery {
    fn begin_attempt(&mut self, now: Instant) -> RecoveryAttempt {
        if let Some(retry_at) = self.retry_at
            && now < retry_at
        {
            return RecoveryAttempt::WaitUntil(retry_at);
        }
        if self.attempts == MAX_RECOVERIES {
            return RecoveryAttempt::Exhausted;
        }
        self.attempts += 1;
        self.retry_at = None;
        RecoveryAttempt::Start
    }

    fn schedule_retry(&mut self, now: Instant) -> Instant {
        let retry_at = now + RECOVERY_DELAY;
        self.retry_at = Some(retry_at);
        retry_at
    }

    fn presented(&mut self) {
        self.attempts = 0;
        self.retry_at = None;
    }
}

fn debug_ui(window: &Window) -> (Context, egui_winit::State) {
    let ctx = Context::default();
    ctx.global_style_mut(|style| style.animation_time = 0.0);
    let state = egui_winit::State::new(ctx.clone(), ViewportId::ROOT, window, Some(window.scale_factor() as f32), window.theme(), None);
    (ctx, state)
}

#[cfg(test)]
mod tests {
    use std::{
        future::Future,
        pin::Pin,
        sync::{
            Arc, Barrier,
            atomic::{AtomicUsize, Ordering},
            mpsc as sync_mpsc,
        },
        task::{Context, Poll},
        time::{Duration, Instant},
    };

    use tokio::{runtime::Handle, sync::oneshot};
    use winit::{
        event::ElementState,
        keyboard::{Key, NamedKey},
    };

    use crate::{
        agent_movement::HeldControls,
        camera::{CameraMode, handle_keyboard_input},
    };

    use super::deadline_passed;
    use super::{
        InitializationOutcome, PresentationRecovery, RECOVERY_DELAY, RecoveryAttempt, RendererLifecycle, RendererToken, SceneWindowError,
        poll_initialization, spawn_initialization,
    };

    struct TestPending;

    #[derive(Debug, PartialEq, Eq)]
    struct TestRenderer(u8);

    #[test]
    fn stale_generation_never_publishes() {
        let mut lifecycle = RendererLifecycle::initializing(9, TestPending);
        let token = lifecycle.pending_token().expect("initialization has a completion token");
        let stale = RendererToken { generation: 8, attempt: token.attempt };

        std::assert_matches!(lifecycle.complete(stale, Ok::<_, &str>(TestRenderer(3))), InitializationOutcome::Stale(Ok(TestRenderer(3))));
        assert!(lifecycle.is_failed());
        assert_eq!(lifecycle.ready(), None);
    }

    #[test]
    fn recovery_failure_allows_retry() {
        let mut lifecycle = RendererLifecycle::initializing(9, TestPending);
        let token = lifecycle.pending_token().expect("initialization has a completion token");
        std::assert_matches!(lifecycle.complete(token, Ok::<_, &str>(TestRenderer(3))), InitializationOutcome::InitialReady);
        assert_eq!(lifecycle.retire_ready().expect("ready renderer can be retired"), TestRenderer(3));
        let retry = lifecycle.recovery_token().expect("retirement starts recovery state");
        assert!(lifecycle.start_recovery(retry, TestPending));

        std::assert_matches!(
            lifecycle.complete(retry, Err("adapter unavailable")),
            InitializationOutcome::RecoveryFailed("adapter unavailable")
        );
        assert!(!lifecycle.is_initializing());
        assert!(!lifecycle.is_ready());
        assert!(!lifecycle.is_failed());
        assert_eq!(lifecycle.pending_token(), None);
        assert!(lifecycle.recovery_token().is_some());
    }

    struct DropSignal(Option<oneshot::Sender<()>>);

    impl Drop for DropSignal {
        fn drop(&mut self) {
            if let Some(sender) = self.0.take() {
                sender.send(()).expect("drop signal receiver remains alive");
            }
        }
    }

    #[tokio::test]
    async fn init_allows_input_and_close() {
        let (_gate_sender, gate_receiver) = oneshot::channel();
        let (started_sender, started_receiver) = oneshot::channel();
        let (dropped_sender, dropped_receiver) = oneshot::channel();
        let initialize = async move {
            let _signal = DropSignal(Some(dropped_sender));
            started_sender.send(()).map_err(|_| "start receiver closed")?;
            gate_receiver.await.map_err(|_| "gate closed")?;
            Ok::<_, &'static str>(())
        };
        let token = RendererToken { generation: 11, attempt: 0 };
        let pending = spawn_initialization(&Handle::current(), token, initialize, || {});
        let mut lifecycle = RendererLifecycle::<_, ()>::initializing(11, pending);
        started_receiver.await.expect("initialization reaches the completion gate");
        assert!(lifecycle.is_initializing());
        std::assert_matches!(poll_initialization(lifecycle.pending_mut().expect("initialization remains pending")), Ok(None));

        let mut controls = HeldControls::default();
        let mut camera_mode = CameraMode::Follow;
        assert_eq!(
            handle_keyboard_input(&mut controls, &mut camera_mode, &Key::Named(NamedKey::ArrowUp), ElementState::Pressed, false, false),
            Some(true)
        );
        assert!(lifecycle.is_initializing());

        let (_, pending) = lifecycle.close();
        drop(pending);
        dropped_receiver.await.expect("close cancels and drops the gated initialization");
        assert!(lifecycle.is_failed());
    }

    async fn panics() -> Result<(), &'static str> {
        panic!("renderer initialization panicked");
    }

    #[tokio::test]
    async fn panic_wakes_typed_initialization_failure() {
        let (wake_sender, wake_receiver) = oneshot::channel();
        let token = RendererToken { generation: 12, attempt: 0 };
        let pending = spawn_initialization(&Handle::current(), token, panics(), move || {
            wake_sender.send(()).expect("wake receiver remains alive");
        });
        let mut lifecycle = RendererLifecycle::<_, ()>::initializing(12, pending);

        wake_receiver.await.expect("task termination wakes the event loop");
        std::assert_matches!(
            poll_initialization(lifecycle.pending_mut().expect("renderer initialization is pending")),
            Err(SceneWindowError::RendererInitializationStopped)
        );
    }

    #[derive(Debug)]
    struct RetirementProbe(Arc<AtomicUsize>);

    impl Drop for RetirementProbe {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::AcqRel);
        }
    }

    #[tokio::test]
    async fn close_retires_queued_completion_once() {
        let (gate_sender, gate_receiver) = oneshot::channel();
        let (started_sender, started_receiver) = oneshot::channel();
        let (wake_sender, wake_receiver) = oneshot::channel();
        let retirements = Arc::new(AtomicUsize::new(0));
        let init_retirements = Arc::clone(&retirements);
        let initialize = async move {
            started_sender.send(()).map_err(|_| "start receiver closed")?;
            gate_receiver.await.map_err(|_| "gate closed")?;
            Ok::<_, &'static str>(RetirementProbe(init_retirements))
        };
        let token = RendererToken { generation: 13, attempt: 0 };
        let pending = spawn_initialization(&Handle::current(), token, initialize, move || {
            wake_sender.send(()).expect("wake receiver remains alive");
        });
        started_receiver.await.expect("initialization reaches the completion gate");

        gate_sender.send(()).expect("initialization gate remains open");
        wake_receiver.await.expect("completed initialization wakes the event loop");
        drop(pending);

        assert_eq!(retirements.load(Ordering::Acquire), 1);
    }

    struct CompleteAfterClose {
        started: Option<sync_mpsc::SyncSender<()>>,
        barrier: Arc<Barrier>,
        retirements: Arc<AtomicUsize>,
    }

    impl Future for CompleteAfterClose {
        type Output = Result<RetirementProbe, &'static str>;

        fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
            let this = self.get_mut();
            let Some(started) = this.started.take() else {
                panic!("initialization future polled after completion");
            };
            started.send(()).expect("start receiver remains alive");
            this.barrier.wait();
            Poll::Ready(Ok(RetirementProbe(Arc::clone(&this.retirements))))
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn close_retires_dropped_receiver_once() {
        let barrier = Arc::new(Barrier::new(2));
        let init_barrier = Arc::clone(&barrier);
        let retirements = Arc::new(AtomicUsize::new(0));
        let init_retirements = Arc::clone(&retirements);
        let (started_sender, started_receiver) = sync_mpsc::sync_channel(1);
        let (wake_sender, wake_receiver) = oneshot::channel();
        let initialize = CompleteAfterClose { started: Some(started_sender), barrier: init_barrier, retirements: init_retirements };
        let token = RendererToken { generation: 14, attempt: 0 };
        let pending = spawn_initialization(&Handle::current(), token, initialize, move || {
            wake_sender.send(()).expect("wake receiver remains alive");
        });

        started_receiver.recv().expect("initializer is synchronously completing");
        drop(pending);
        barrier.wait();
        wake_receiver.await.expect("terminal send wakes the event loop");

        assert_eq!(retirements.load(Ordering::Acquire), 1);
    }

    #[tokio::test]
    async fn late_completion_skips_replacement() {
        let (gate_sender, gate_receiver) = oneshot::channel();
        let (started_sender, started_receiver) = oneshot::channel();
        let (wake_sender, wake_receiver) = oneshot::channel();
        let retirements = Arc::new(AtomicUsize::new(0));
        let init_retirements = Arc::clone(&retirements);
        let initialize = async move {
            started_sender.send(()).map_err(|_| "start receiver closed")?;
            gate_receiver.await.map_err(|_| "gate closed")?;
            Ok::<_, &'static str>(RetirementProbe(init_retirements))
        };
        let token = RendererToken { generation: 15, attempt: 0 };
        let pending = spawn_initialization(&Handle::current(), token, initialize, move || {
            wake_sender.send(()).expect("wake receiver remains alive");
        });
        let mut previous = RendererLifecycle::<_, RetirementProbe>::initializing(15, pending);
        started_receiver.await.expect("initialization reaches the completion gate");

        gate_sender.send(()).expect("initialization gate remains open");
        wake_receiver.await.expect("completed initialization wakes the event loop");
        let completion = poll_initialization(previous.pending_mut().expect("old generation remains pending"))
            .expect("worker completed")
            .expect("completion is available");
        let mut current = RendererLifecycle::<TestPending, RetirementProbe>::initializing(16, TestPending);

        std::assert_matches!(current.complete(completion.0, completion.1), InitializationOutcome::Stale(Ok(_)));
        assert!(current.ready().is_none());
        assert_eq!(retirements.load(Ordering::Acquire), 1);
    }

    #[test]
    fn presentation_resets_recovery_limit() {
        let mut recovery = PresentationRecovery::default();
        let mut now = Instant::now();
        for _ in 0..6 {
            assert_eq!(recovery.begin_attempt(now), RecoveryAttempt::Start);
            recovery.presented();
        }
        for _ in 0..3 {
            assert_eq!(recovery.begin_attempt(now), RecoveryAttempt::Start);
            now = recovery.schedule_retry(now);
        }
        assert_eq!(recovery.begin_attempt(now), RecoveryAttempt::Exhausted, "three attempts without a presented frame exhaust recovery");
        recovery.presented();
        assert_eq!(recovery.begin_attempt(now), RecoveryAttempt::Start, "a presented frame makes recovery available again");
    }

    #[test]
    fn recovery_retries_after_delay() {
        let mut recovery = PresentationRecovery::default();
        let now = Instant::now();

        assert_eq!(recovery.begin_attempt(now), RecoveryAttempt::Start);
        let retry_at = recovery.schedule_retry(now);
        assert_eq!(retry_at, now + RECOVERY_DELAY);
        assert_eq!(recovery.begin_attempt(retry_at - Duration::from_nanos(1)), RecoveryAttempt::WaitUntil(retry_at));
        assert_eq!(recovery.begin_attempt(retry_at), RecoveryAttempt::Start);
    }

    #[test]
    fn deadline_invalidates_only_after_passing() {
        let now = Instant::now();
        let deadline = now + Duration::from_millis(16);
        assert!(!deadline_passed(Some(deadline), now));
        assert!(deadline_passed(Some(deadline), deadline));
        assert!(!deadline_passed(None, now));
    }
}
