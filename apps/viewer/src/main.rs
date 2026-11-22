use std::{borrow::Cow, env::consts::OS, io, sync::Arc};

use assets::cache::AssetCache;
use cvars::{ASSET_DISK_CACHE_CAPACITY_MIB, LOG_LEVEL, LOG_NOISY_NETWORK};
use prism::{
    AssetSource, SharedString,
    assets::{Assets, icon_assets},
};
use prism_wgpu::WgpuDeviceRequirements;
use tokio::runtime::Builder;
use viewer::{
    gui,
    logging::{self, LogLevel},
    settings::{SettingsStore, default_settings_path},
};

icon_assets!(ExtraIcons, [MessageCircle]);

struct ViewerAssets;

impl AssetSource for ViewerAssets {
    fn load(&self, path: &str) -> prism::Result<Option<Cow<'static, [u8]>>> {
        match ExtraIcons.load(path)? {
            Some(data) => Ok(Some(data)),
            None => Assets.load(path),
        }
    }

    fn list(&self, path: &str) -> prism::Result<Vec<SharedString>> {
        let mut items = Assets.list(path)?;
        items.extend(ExtraIcons.list(path)?);
        Ok(items)
    }
}

fn main() -> io::Result<()> {
    let (_logging, log_control) = logging::init();
    tracing::info!(version = env!("CARGO_PKG_VERSION"), os = OS, "viewer started");
    let runtime = Builder::new_multi_thread().enable_all().thread_name("radia-service").build()?;
    let services = runtime.handle().clone();
    let settings = match default_settings_path() {
        Ok(path) => {
            let (store, report) = SettingsStore::load_or_default(path.clone(), Some(services.clone()));
            match report {
                Ok(report) => {
                    if !report.unknown_keys.is_empty() {
                        tracing::warn!(unknown_keys = ?report.unknown_keys, path = %path.display(), "unknown keys in settings.toml");
                    }
                }
                Err(error) => {
                    tracing::warn!(%error, path = %path.display(), "failed to load settings.toml; using defaults");
                }
            }
            Arc::new(store)
        }
        Err(error) => {
            tracing::warn!(%error, "settings path unavailable; using in-memory defaults");
            Arc::new(SettingsStore::in_memory())
        }
    };
    if let Some(level) = LogLevel::from_directive(settings.get(LOG_LEVEL)) {
        log_control.apply_startup_settings(level, settings.get(LOG_NOISY_NETWORK));
    }
    settings.lock_startup_values();

    let cache_limit_bytes = u64::try_from(settings.get(ASSET_DISK_CACHE_CAPACITY_MIB)).unwrap_or(1024).saturating_mul(1024 * 1024);
    let asset_cache = match AssetCache::open_default_with_limit(cache_limit_bytes) {
        Ok(cache) => {
            let stats = cache.stats();
            tracing::info!(entries = ?stats.entries_by_kind, bytes = stats.size_bytes, "asset cache opened");
            Some(Arc::new(cache))
        }
        Err(error) => {
            tracing::warn!(%error, "asset cache unavailable; continuing without persistence");
            None
        }
    };
    let gui_cache = asset_cache.clone();
    let gui_settings = Arc::clone(&settings);
    prism::application().with_assets(ViewerAssets).run(move |cx| {
        cx.set_gpu_requirements(Box::new(WgpuDeviceRequirements {
            features: render::required_features(),
            limits: Some(render::required_limits()),
            backends: None,
        }));
        gui::init(cx, services, gui_cache, log_control, gui_settings);
    });
    let cache_shutdown = if let Some(cache) = asset_cache {
        Arc::try_unwrap(cache)
            .map_err(|cache| io::Error::other(format!("asset cache still has {} owners after viewer shutdown", Arc::strong_count(&cache))))
            .map(|cache| {
                if let Err(error) = cache.shutdown() {
                    tracing::warn!(%error, "asset cache shutdown failed");
                }
            })
    } else {
        Ok(())
    };
    settings.wait_for_pending_save();
    runtime.shutdown_background();
    cache_shutdown
}
