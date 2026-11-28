mod scene;
mod views;
mod window_geometry;

use std::{
    mem,
    sync::{Arc, Mutex, PoisonError},
};

use assets::{Budget as AssetBudget, cache::AssetCache};
use futures::FutureExt;
use prism::{
    AnyWindowHandle, App, AppContext, ColorExt, Entity, Global, QuitMode, Subscription, TitlebarOptions, Window, WindowBounds,
    WindowOptions,
    component::{Theme, ThemeMode, TitleBar},
    px, rgb, rgb_to_hsla, size,
};
use prism_wgpu::WgpuContextHandle;
use render::scene::Budget as SceneBudget;
use tokio::{runtime::Handle, sync::watch, task::JoinHandle};
use utils::hardware::Pressure;
use wgpu::{Device, Queue};

use crate::{
    gui::{views::login::LoginScreen, window_geometry::restore_bounds},
    inspection::{GpuInfo, ViewerInspectionPublisher},
    limits::StartupLimits,
    logging::LogControl,
    settings::SettingsStore,
};

struct AppLifecycle {
    _window_closed: Subscription,
    controller: Option<Entity<LoginScreen>>,
    login_window: Option<AnyWindowHandle>,
    scene: scene::State,
    quitting: bool,
    session_lifecycle: Arc<SessionLifecycle>,
    asset_cache: Option<Arc<AssetCache>>,
    log_control: LogControl,
    settings: Arc<SettingsStore>,
    inspection: Option<ViewerInspectionPublisher>,
    startup_limits: StartupLimits,
    asset_budget: AssetBudget,
    scene_budget: SceneBudget,
    pressure: watch::Receiver<Pressure>,
}

impl Global for AppLifecycle {}

#[derive(Default)]
struct SessionLifecycle {
    state: Mutex<SessionState>,
}

#[derive(Default)]
struct SessionState {
    active: Option<network::SessionHandle>,
    drains: Vec<JoinHandle<()>>,
}

impl SessionLifecycle {
    fn set_active(&self, active: Option<network::SessionHandle>) {
        self.state.lock().unwrap_or_else(PoisonError::into_inner).active = active;
    }

    fn active(&self) -> Option<network::SessionHandle> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner).active.clone()
    }

    fn track_drain(&self, drain: JoinHandle<()>) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.drains.retain_mut(|drain| {
            if !drain.is_finished() {
                return true;
            }
            match drain.now_or_never() {
                Some(Ok(())) => false,
                Some(Err(error)) => {
                    tracing::warn!(%error, "session drain task failed");
                    false
                }
                None => true,
            }
        });
        state.drains.push(drain);
    }

    async fn wait_for_drains(&self) {
        let drains = {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            mem::take(&mut state.drains)
        };
        for drain in drains {
            if let Err(error) = drain.await {
                tracing::warn!(%error, "session drain task failed");
            }
        }
    }
}

pub(crate) fn asset_cache(cx: &App) -> Option<Arc<AssetCache>> {
    cx.global::<AppLifecycle>().asset_cache.clone()
}

pub(crate) fn log_control(cx: &App) -> LogControl {
    cx.try_global::<AppLifecycle>().map_or_else(LogControl::noop, |lifecycle| lifecycle.log_control.clone())
}

pub(crate) fn settings(cx: &App) -> Arc<SettingsStore> {
    cx.try_global::<AppLifecycle>().map_or_else(|| Arc::new(SettingsStore::in_memory()), |lifecycle| Arc::clone(&lifecycle.settings))
}

pub(crate) fn inspection(cx: &App) -> Option<ViewerInspectionPublisher> {
    cx.try_global::<AppLifecycle>().and_then(|lifecycle| lifecycle.inspection.clone())
}

pub(crate) fn startup_limits(cx: &App) -> StartupLimits {
    cx.try_global::<AppLifecycle>().map_or_else(StartupLimits::default, |lifecycle| lifecycle.startup_limits)
}

pub(crate) fn asset_budget(cx: &App) -> AssetBudget {
    cx.try_global::<AppLifecycle>().map_or_else(AssetBudget::default, |lifecycle| lifecycle.asset_budget.clone())
}

pub(crate) fn scene_budget(cx: &App) -> SceneBudget {
    cx.try_global::<AppLifecycle>().map_or_else(SceneBudget::default, |lifecycle| lifecycle.scene_budget.clone())
}

pub(crate) fn pressure(cx: &App) -> watch::Receiver<Pressure> {
    cx.try_global::<AppLifecycle>().map(|lifecycle| lifecycle.pressure.clone()).unwrap_or_else(|| watch::channel(Pressure::Normal).1)
}

pub(crate) fn publish_window_gpu_info(window: &Window, cx: &App) {
    let Some(publisher) = inspection(cx) else { return };
    let Some(handle) = WgpuContextHandle::from_window(window) else { return };
    let Some(gpu_box) = window.gpu_context() else { return };
    let Ok(context) = gpu_box.downcast::<(Arc<Device>, Arc<Queue>)>() else { return };
    let (device, _) = *context;
    publisher.set_gpu_info(GpuInfo::from_wgpu(handle.adapter_info(), &device.limits()));
}

pub fn init(
    cx: &mut App,
    services: Handle,
    asset_cache: Option<Arc<AssetCache>>,
    log_control: LogControl,
    settings: Arc<SettingsStore>,
    inspection: Option<ViewerInspectionPublisher>,
    startup_limits: StartupLimits,
    pressure: watch::Receiver<Pressure>,
) {
    let session_lifecycle = Arc::new(SessionLifecycle::default());
    cx.set_app_identity("org.radia.viewer", "Radia");
    cx.set_keyring_label("Radia");
    prism_tokio::init_from_handle(cx, services.clone());
    cx.set_quit_mode(QuitMode::Explicit);

    prism::init(cx);
    set_dark_theme(cx);

    install_lifecycle(Arc::clone(&session_lifecycle), cx);
    let asset_budget = AssetBudget::new(startup_limits.assets);
    let scene_budget = SceneBudget::new(startup_limits.scene);
    {
        let mut rx = pressure.clone();
        let asset_budget = asset_budget.clone();
        let scene_budget = scene_budget.clone();
        drop(services.spawn(async move {
            let initial = *rx.borrow_and_update();
            asset_budget.set_pressure(initial);
            scene_budget.set_pressure(initial);
            while rx.changed().await.is_ok() {
                let level = *rx.borrow_and_update();
                asset_budget.set_pressure(level);
                scene_budget.set_pressure(level);
            }
        }));
    }
    {
        let lifecycle = cx.global_mut::<AppLifecycle>();
        lifecycle.asset_cache = asset_cache;
        lifecycle.log_control = log_control;
        lifecycle.settings = settings;
        lifecycle.inspection = inspection;
        lifecycle.startup_limits = startup_limits;
        lifecycle.asset_budget = asset_budget;
        lifecycle.scene_budget = scene_budget;
        lifecycle.pressure = pressure;
    }

    let (main_window, login) = prism::open_window(login_options(cx), cx, move |window, cx| {
        publish_window_gpu_info(window, cx);
        cx.new(|cx| LoginScreen::new(window, cx, session_lifecycle))
    })
    .expect("failed to open the main window");
    let lifecycle = cx.global_mut::<AppLifecycle>();
    lifecycle.controller = Some(login);
    lifecycle.login_window = Some(main_window);
    cx.activate(true);
}

fn install_lifecycle(session_lifecycle: Arc<SessionLifecycle>, cx: &mut App) {
    let window_closed = cx.on_window_closed(|cx, closed_window_id| {
        let lifecycle = cx.global_mut::<AppLifecycle>();
        let closing_main = lifecycle.login_window.is_some_and(|window| window.window_id() == closed_window_id);
        if closing_main {
            lifecycle.login_window = None;
        }
        match lifecycle.scene.window_closed(closed_window_id) {
            scene::WindowClose::Auxiliary => return,
            scene::WindowClose::SceneClosed => {
                shutdown(cx);
                return;
            }
            scene::WindowClose::Ignored => {}
        }
        if closing_main || cx.windows().is_empty() {
            shutdown(cx);
        }
    });
    cx.set_global(AppLifecycle {
        _window_closed: window_closed,
        controller: None,
        login_window: None,
        scene: scene::State::new(),
        quitting: false,
        session_lifecycle,
        asset_cache: None,
        log_control: LogControl::noop(),
        settings: Arc::new(SettingsStore::in_memory()),
        inspection: None,
        startup_limits: StartupLimits::default(),
        asset_budget: AssetBudget::default(),
        scene_budget: SceneBudget::default(),
        pressure: watch::channel(Pressure::Normal).1,
    });
}

fn login_options(cx: &App) -> WindowOptions {
    let minimum = size(px(460.), px(420.));
    WindowOptions {
        titlebar: Some(TitlebarOptions { title: Some("Radia".into()), ..TitleBar::title_bar_options() }),
        window_bounds: Some(WindowBounds::Windowed(restore_bounds("login", None, size(px(720.), px(480.)), minimum, cx))),
        window_min_size: Some(minimum),
        ..TitleBar::window_options()
    }
}

fn shutdown(cx: &mut App) -> bool {
    let session_lifecycle = {
        let lifecycle = cx.global_mut::<AppLifecycle>();
        if lifecycle.quitting {
            return false;
        }
        lifecycle.quitting = true;
        drop(lifecycle.asset_cache.take());
        Arc::clone(&lifecycle.session_lifecycle)
    };
    scene::shutdown(cx);
    for window in cx.windows() {
        window.update(cx, |_, window, _| window.remove_window()).ok();
    }
    cx.spawn(async move |cx| {
        disconnect_session(&session_lifecycle).await;
        session_lifecycle.wait_for_drains().await;
        cx.update(|app| app.quit());
    })
    .detach();
    true
}

async fn disconnect_session(session_lifecycle: &SessionLifecycle) {
    let client = session_lifecycle.active();
    let Some(client) = client else {
        return;
    };

    client.disconnect().await;
    session_lifecycle.set_active(None);
}

fn set_dark_theme(cx: &mut App) {
    Theme::change(ThemeMode::Dark, None, cx);

    let theme = Theme::global_mut(cx);
    theme.background = rgb_to_hsla(rgb(0x171717));
    theme.foreground = rgb_to_hsla(rgb(0xe6e6e6));
    theme.muted = rgb_to_hsla(rgb(0x202020));
    theme.muted_foreground = rgb_to_hsla(rgb(0x9b9b9b));
    theme.border = rgb_to_hsla(rgb(0x363636));
    theme.group_box = rgb_to_hsla(rgb(0x1b1b1b));
    theme.group_box_foreground = theme.foreground;
    theme.sidebar = rgb_to_hsla(rgb(0x191919));
    theme.sidebar_foreground = theme.foreground;
    theme.sidebar_border = theme.border;
    theme.input = rgb_to_hsla(rgb(0x222222));
    theme.button = rgb_to_hsla(rgb(0x252525));
    theme.button_foreground = theme.foreground;
    theme.button_hover = rgb_to_hsla(rgb(0x303030));
    theme.button_active = rgb_to_hsla(rgb(0x202020));
    theme.ring = rgb_to_hsla(rgb(0x898989));
    theme.caret = rgb_to_hsla(rgb(0xf2f2f2));
    theme.selection = rgb_to_hsla(rgb(0x3a3a3a));
    theme.accent = rgb_to_hsla(rgb(0x292929));
    theme.accent_foreground = rgb_to_hsla(rgb(0xe6e6e6));

    theme.primary = rgb_to_hsla(rgb(0x3a3a3a));
    theme.primary_foreground = rgb_to_hsla(rgb(0xf0f0f0));
    theme.primary_hover = rgb_to_hsla(rgb(0x484848));
    theme.primary_active = rgb_to_hsla(rgb(0x303030));
    theme.secondary = rgb_to_hsla(rgb(0x252525));
    theme.secondary_foreground = theme.foreground;
    theme.secondary_hover = rgb_to_hsla(rgb(0x303030));
    theme.secondary_active = rgb_to_hsla(rgb(0x202020));

    theme.link = rgb_to_hsla(rgb(0xc6c6c6));
    theme.link_hover = rgb_to_hsla(rgb(0xe0e0e0));
    theme.link_active = rgb_to_hsla(rgb(0xababab));
    theme.title_bar = theme.background;
    theme.title_bar_border = theme.background;

    theme.danger = rgb_to_hsla(rgb(0xe81120));
    theme.danger_hover = rgb_to_hsla(rgb(0xe81120));
    theme.danger_active = rgb_to_hsla(rgb(0xc50f1f));
    theme.danger_foreground = rgb_to_hsla(rgb(0xffffff));
    theme.tokens = theme.colors.into();
    theme.tokens.table = theme.group_box.into();
    theme.tokens.table_head = theme.group_box.into();
    theme.table_active = rgb_to_hsla(rgb(0x666666)).opacity(0.35);
    theme.table_active_border = theme.table_active.opacity(0.0);
    theme.tokens.table_active = theme.table_active.into();
    theme.icon_stroke_width = Some(1.33);
    Theme::sync_base(cx);
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio::sync::oneshot;

    use super::{AppLifecycle, SessionLifecycle, install_lifecycle, shutdown};

    #[prism::test]
    fn main_window_owns_shutdown(cx: &mut prism::TestAppContext) {
        cx.update(|cx| {
            cx.set_quit_mode(prism::QuitMode::Explicit);
            install_lifecycle(Arc::new(SessionLifecycle::default()), cx);
        });
        let main = cx.add_empty_window().update(|window, _| window.window_handle());
        let tool = cx.add_empty_window().update(|window, _| window.window_handle());
        cx.update(|cx| cx.global_mut::<AppLifecycle>().login_window = Some(main));
        tool.update(cx, |_, window, _| window.remove_window()).expect("close tool window");
        cx.run_until_parked();
        cx.read_global::<AppLifecycle, _>(|lifecycle, _| assert!(!lifecycle.quitting));
        cx.add_empty_window();
        main.update(cx, |_, window, _| window.remove_window()).expect("close main window");
        cx.run_until_parked();
        cx.read_global::<AppLifecycle, _>(|lifecycle, _| assert!(lifecycle.quitting));
    }

    #[prism::test]
    fn repeated_shutdown_starts_once(cx: &mut prism::TestAppContext) {
        cx.update(|cx| {
            cx.set_quit_mode(prism::QuitMode::Explicit);
            install_lifecycle(Arc::new(SessionLifecycle::default()), cx);
            assert!(shutdown(cx));
            assert!(!shutdown(cx));
        });
        cx.run_until_parked();
        cx.read_global::<AppLifecycle, _>(|lifecycle, _| assert!(lifecycle.quitting));
    }

    #[tokio::test]
    async fn session_drains_survive_relogin() {
        let lifecycle = SessionLifecycle::default();

        let (old_started_tx, old_started_rx) = oneshot::channel();
        let (old_release_tx, old_release_rx) = oneshot::channel();
        let old_drain = tokio::spawn(async move {
            old_started_tx.send(()).expect("old drain start is observed");
            old_release_rx.await.expect("old drain is released");
        });
        lifecycle.track_drain(old_drain);
        old_started_rx.await.expect("old drain started");

        let (new_started_tx, new_started_rx) = oneshot::channel();
        let (new_release_tx, new_release_rx) = oneshot::channel();
        let (new_finished_tx, new_finished_rx) = oneshot::channel();
        let new_drain = tokio::spawn(async move {
            new_started_tx.send(()).expect("new drain start is observed");
            new_release_rx.await.expect("new drain is released");
            new_finished_tx.send(()).expect("new drain completion is observed");
        });
        lifecycle.track_drain(new_drain);
        new_started_rx.await.expect("new drain started");

        let mut wait = Box::pin(lifecycle.wait_for_drains());

        new_release_tx.send(()).expect("new drain is waiting");
        tokio::select! {
            biased;
            _ = &mut wait => panic!("shutdown finished before the old drain"),
            result = new_finished_rx => result.expect("new drain completed"),
        }

        old_release_tx.send(()).expect("old drain is waiting");
        wait.await;
    }
}
