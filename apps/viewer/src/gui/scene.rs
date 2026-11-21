use prism::{
    AnyElement, AnyWindowHandle, App, AppContext, Entity, Render, Subscription, TitlebarOptions, Window, WindowBounds, WindowId,
    WindowOptions,
    component::{StyledExt, Theme, TitleBar},
    div,
    prelude::*,
    px, size,
};
use prism_tokio::Tokio;
use render::scene::Camera;
use tokio::sync::watch;
use uuid::Uuid;

use crate::{
    agent_movement::MovementObservation,
    asset_coordinator::AssetHandle,
    gui::{
        AppLifecycle, log_control, login_options, settings,
        views::{
            chat::ChatScreen,
            viewport::{SceneView, SceneViewConfig, ViewportError},
        },
        window_geometry,
    },
    scene_feed::SceneSources,
    session::{AgentMotionSample, EnvironmentInbox, SessionEnd},
    world_reader::WorldReader,
};

pub(super) struct State {
    main: MainWindow,
    scene_window: Option<AnyWindowHandle>,
    scene_view: Option<Entity<SceneView>>,
    chat_window: Option<AnyWindowHandle>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MainWindow {
    Login,
    Starting(u64),
    Scene(u64),
}

impl MainWindow {
    fn blocks_start(self, generation: u64) -> bool {
        matches!(self, Self::Starting(active) | Self::Scene(active) if active >= generation)
    }

    fn is_current(self, generation: u64) -> bool {
        matches!(self, Self::Starting(active) | Self::Scene(active) if active == generation)
    }
}

impl State {
    pub(super) fn new() -> Self {
        Self { main: MainWindow::Login, scene_window: None, scene_view: None, chat_window: None }
    }

    pub(super) fn window_closed(&mut self, closed_window_id: WindowId) -> bool {
        if self.chat_window.is_some_and(|window| window.window_id() == closed_window_id) {
            self.chat_window = None;
            return matches!(self.main, MainWindow::Starting(_) | MainWindow::Scene(_));
        }
        if self.scene_window.is_some_and(|window| window.window_id() == closed_window_id) {
            self.scene_window = None;
            self.scene_view = None;
            return !matches!(self.main, MainWindow::Starting(_) | MainWindow::Scene(_));
        }
        matches!(self.main, MainWindow::Starting(_) | MainWindow::Scene(_))
    }
}

pub(super) fn shutdown(cx: &mut App) {
    let view = cx.global_mut::<AppLifecycle>().scene.scene_view.take();
    if let Some(view) = view {
        view.update(cx, |scene, _| scene.retire());
    }
    cx.global_mut::<AppLifecycle>().scene.scene_window = None;
    cx.global_mut::<AppLifecycle>().scene.chat_window = None;
}

pub(crate) fn start_session(
    generation: u64,
    reader: &WorldReader,
    environments: EnvironmentInbox,
    inventory_updates: watch::Receiver<Option<inventory::InventorySnapshot>>,
    assets: &AssetHandle,
    agent_id: Uuid,
    camera: watch::Sender<Option<Camera>>,
    movement: watch::Sender<Option<MovementObservation>>,
    agent_motion: watch::Receiver<Option<AgentMotionSample>>,
    motion_circuits: watch::Receiver<Vec<(world::Region, watch::Receiver<network::CircuitMotionStatus>)>>,
    cx: &mut App,
) {
    if !cx.has_global::<AppLifecycle>()
        || cx.global::<AppLifecycle>().quitting
        || cx.global::<AppLifecycle>().scene.main.blocks_start(generation)
    {
        return;
    }
    let login_window = cx.global::<AppLifecycle>().login_window;
    let activate = login_window.is_some_and(|window| window.update(cx, |_, window, _| window.is_window_active()).unwrap_or(false));
    cx.global_mut::<AppLifecycle>().scene.main = MainWindow::Starting(generation);

    let sources = SceneSources { reader: reader.clone(), environments, assets: assets.clone(), motion_circuits, inventory_updates };
    let runtime = Tokio::handle(cx);
    let logs = log_control(cx);
    let settings = settings(cx);
    let config = SceneViewConfig { generation, sources, agent_id, camera, movement, agent_motion, runtime, log_control: logs, settings };

    let minimum = size(px(460.), px(300.));
    let options = WindowOptions {
        titlebar: Some(TitlebarOptions { title: Some("Radia".into()), ..TitleBar::title_bar_options() }),
        window_bounds: Some(WindowBounds::Windowed(window_geometry::restore_bounds("scene", None, size(px(1280.), px(720.)), minimum, cx))),
        window_min_size: Some(minimum),
        focus: activate,
        ..TitleBar::window_options()
    };

    match prism::open_window(options, cx, |window, cx| cx.new(|cx| SceneView::new(config, window, cx))) {
        Ok((window, view)) => {
            let lifecycle = cx.global_mut::<AppLifecycle>();
            lifecycle.scene.scene_window = Some(window);
            lifecycle.scene.scene_view = Some(view);
        }
        Err(error) => {
            cx.defer(move |cx| scene_failed(generation, ViewportError::WindowOpen(error.to_string()), cx));
        }
    }
}

pub(crate) fn end_session(generation: u64, cx: &mut App) {
    if !cx.has_global::<AppLifecycle>() {
        return;
    }
    let (scene_window, scene_view) = {
        let lifecycle = cx.global_mut::<AppLifecycle>();
        if !lifecycle.scene.main.is_current(generation) {
            return;
        }
        lifecycle.scene.main = MainWindow::Login;
        (lifecycle.scene.scene_window.take(), lifecycle.scene.scene_view.take())
    };
    if let Some(view) = scene_view {
        view.update(cx, |scene, _| scene.retire());
    }
    if let Some(window) = scene_window {
        window.update(cx, |_, window, _| window.remove_window()).ok();
    }
    cx.defer(show_login);
}

pub(crate) fn login_content(_: bool, login_body: AnyElement, _: AnyElement) -> AnyElement {
    login_body
}

pub(crate) fn scene_ready(generation: u64, cx: &mut App) {
    if !cx.has_global::<AppLifecycle>() || cx.global::<AppLifecycle>().quitting {
        return;
    }
    if cx.global::<AppLifecycle>().scene.main != MainWindow::Starting(generation) {
        return;
    }
    cx.global_mut::<AppLifecycle>().scene.main = MainWindow::Scene(generation);
    if let Some(window) = cx.global_mut::<AppLifecycle>().login_window.take() {
        window.update(cx, |_, window, _| window.remove_window()).ok();
    }
}

pub(crate) fn request_chat(generation: u64, cx: &mut App) {
    if !cx.has_global::<AppLifecycle>() || cx.global::<AppLifecycle>().quitting {
        return;
    }
    if cx.global::<AppLifecycle>().scene.main == MainWindow::Scene(generation) {
        show_chat(true, cx);
    }
}

pub(crate) fn scene_failed(generation: u64, error: ViewportError, cx: &mut App) {
    if !cx.has_global::<AppLifecycle>() {
        return;
    }
    let lifecycle = cx.global::<AppLifecycle>();
    if lifecycle.quitting || !lifecycle.scene.main.is_current(generation) {
        return;
    }
    let controller = lifecycle.controller.clone();
    if let Some(controller) = controller {
        controller.update(cx, |login, cx| {
            tracing::error!(%error, "scene renderer stopped");
            login.end_session(SessionEnd::Failed("Unable to display the world.".into()), cx);
        });
    }
}

fn show_login(cx: &mut App) {
    let lifecycle = cx.global::<AppLifecycle>();
    if lifecycle.quitting {
        return;
    }
    if let Some(handle) = lifecycle.login_window {
        handle.update(cx, |_, window, _| window.activate()).ok();
    } else {
        let Some(controller) = lifecycle.controller.clone() else {
            return;
        };
        match prism::open_window(login_options(cx), cx, move |window, cx| {
            controller.update(cx, |login, cx| login.attach_window(window, cx));
            controller
        }) {
            Ok((handle, _)) => cx.global_mut::<AppLifecycle>().login_window = Some(handle),
            Err(error) => {
                tracing::error!(%error, "could not reopen login window");
                return;
            }
        }
    }
    if let Some(chat) = cx.global_mut::<AppLifecycle>().scene.chat_window.take() {
        chat.update(cx, |_, window, _| window.remove_window()).ok();
    }
}

fn show_chat(focus: bool, cx: &mut App) {
    let lifecycle = cx.global::<AppLifecycle>();
    if lifecycle.quitting {
        return;
    }
    if let Some(window) = lifecycle.scene.chat_window {
        if focus {
            window.update(cx, |_, window, _| window.activate()).ok();
        }
        return;
    }
    let Some(controller) = &lifecycle.controller else {
        return;
    };
    let chat = controller.read(cx).chat_view();
    let minimum = size(px(460.), px(300.));
    let options = WindowOptions {
        titlebar: Some(TitlebarOptions { title: Some("Nearby chat".into()), ..TitleBar::title_bar_options() }),
        window_bounds: Some(WindowBounds::Windowed(window_geometry::restore_bounds("chat", None, size(px(640.), px(520.)), minimum, cx))),
        window_min_size: Some(minimum),
        focus,
        ..TitleBar::window_options()
    };
    match prism::open_window(options, cx, move |window, cx| cx.new(|cx| ChatWindow::new(chat, window, cx))) {
        Ok((window, _)) => cx.global_mut::<AppLifecycle>().scene.chat_window = Some(window),
        Err(error) => tracing::warn!(%error, "could not open nearby chat window"),
    }
}

struct ChatWindow {
    chat: Entity<ChatScreen>,
    _geometry: Subscription,
}

impl ChatWindow {
    fn new(chat: Entity<ChatScreen>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self { chat, _geometry: window_geometry::observe("chat", window, cx) }
    }
}

impl Render for ChatWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::global(cx).colors;
        let title = div().text_sm().font_semibold().text_color(theme.foreground).child("Nearby Chat");
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(TitleBar::new().child(title))
            .child(div().flex_1().min_h_0().child(self.chat.clone()))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use prism::App;
    use render::scene::ApplyError;

    use super::{MainWindow, end_session, request_chat, scene_failed, scene_ready};
    use crate::gui::{
        AppLifecycle, SessionLifecycle, install_lifecycle, shutdown as app_shutdown,
        views::{login::LoginScreen, viewport::ViewportError},
    };

    fn scene_closed(generation: u64, cx: &mut App) {
        if !cx.has_global::<AppLifecycle>() || cx.global::<AppLifecycle>().quitting {
            return;
        }
        if cx.global::<AppLifecycle>().scene.main.is_current(generation) {
            app_shutdown(cx);
        }
    }

    #[prism::test]
    fn handoff_preserves_window_lifetimes(cx: &mut prism::TestAppContext) {
        let session = Arc::new(SessionLifecycle::default());
        cx.update(|cx| {
            prism::init(cx);
            cx.set_quit_mode(prism::QuitMode::Explicit);
            install_lifecycle(Arc::clone(&session), cx);
        });
        let (controller, visual) = cx.add_window_view(|window, cx| LoginScreen::new(window, cx, session));
        let login_id = visual.update(|window, cx| {
            let lifecycle = cx.global_mut::<AppLifecycle>();
            lifecycle.controller = Some(controller.clone());
            lifecycle.login_window = Some(window.window_handle());
            lifecycle.scene.main = MainWindow::Starting(2);
            window.window_handle().window_id()
        });
        cx.update(|cx| {
            scene_ready(1, cx);
            assert_eq!(cx.global::<AppLifecycle>().scene.main, MainWindow::Starting(2));
            scene_ready(2, cx);
            let lifecycle = cx.global::<AppLifecycle>();
            assert_eq!(lifecycle.scene.main, MainWindow::Scene(2));
            assert!(lifecycle.login_window.is_none());
            assert!(lifecycle.scene.chat_window.is_none(), "chat must stay closed at login");
            scene_closed(1, cx);
            scene_failed(1, ViewportError::ScenePublication(ApplyError::StaleSession), cx);
            assert!(!cx.global::<AppLifecycle>().quitting);
            request_chat(2, cx);
            let chat = cx.global::<AppLifecycle>().scene.chat_window.expect("chat opens when requested");
            assert_ne!(chat.window_id(), login_id, "chat must have its own native window");
            chat.update(cx, |_, window, _| window.remove_window()).unwrap();
        });
        cx.run_until_parked();
        cx.update(|cx| {
            let lifecycle = cx.global::<AppLifecycle>();
            assert!(lifecycle.login_window.is_none());
            assert!(lifecycle.scene.chat_window.is_none());
            assert_eq!(lifecycle.scene.main, MainWindow::Scene(2));
            assert!(!lifecycle.quitting, "closing chat must leave the scene and session alive");
            request_chat(2, cx);
            assert!(cx.global::<AppLifecycle>().scene.chat_window.is_some(), "chat must reopen without replacing the session controller");
            assert_eq!(cx.global::<AppLifecycle>().controller.as_ref().unwrap().entity_id(), controller.entity_id());
            scene_closed(2, cx);
            assert!(cx.global::<AppLifecycle>().quitting, "closing the current scene owns shutdown");
        });
        cx.run_until_parked();
    }

    #[prism::test]
    fn early_end_rejects_ready(cx: &mut prism::TestAppContext) {
        cx.update(|cx| {
            cx.set_quit_mode(prism::QuitMode::Explicit);
            install_lifecycle(Arc::new(SessionLifecycle::default()), cx);
        });
        let login = cx.add_empty_window().update(|window, _| window.window_handle());
        cx.update(|cx| {
            let lifecycle = cx.global_mut::<AppLifecycle>();
            lifecycle.login_window = Some(login);
            lifecycle.scene.main = MainWindow::Starting(2);
            end_session(2, cx);
            scene_ready(2, cx);
            let lifecycle = cx.global::<AppLifecycle>();
            assert_eq!(lifecycle.scene.main, MainWindow::Login);
            assert!(lifecycle.scene.chat_window.is_none());
            assert!(!lifecycle.quitting);
        });
        cx.run_until_parked();
    }

    #[prism::test]
    fn close_discards_late_ready(cx: &mut prism::TestAppContext) {
        cx.update(|cx| {
            cx.set_quit_mode(prism::QuitMode::Explicit);
            install_lifecycle(Arc::new(SessionLifecycle::default()), cx);
        });
        let login = cx.add_empty_window().update(|window, _| window.window_handle());
        cx.update(|cx| {
            let lifecycle = cx.global_mut::<AppLifecycle>();
            lifecycle.login_window = Some(login);
            lifecycle.scene.main = MainWindow::Starting(2);
            scene_closed(2, cx);
            scene_ready(2, cx);
            let lifecycle = cx.global::<AppLifecycle>();
            assert!(lifecycle.quitting);
            assert_eq!(lifecycle.scene.main, MainWindow::Starting(2));
            assert!(lifecycle.scene.scene_window.is_none());
            assert!(lifecycle.scene.chat_window.is_none());
        });
        cx.run_until_parked();
    }
}
