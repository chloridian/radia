use std::{env::consts::OS, io, sync::Arc};

use assets::cache::AssetCache;
use gpui_kit_assets::Assets;
use gpui_wgpu::WgpuDeviceRequirements;
use tokio::runtime::Builder;
use viewer::{gui, logging};

fn main() -> io::Result<()> {
    let (_logging, log_control) = logging::init();
    tracing::info!(version = env!("CARGO_PKG_VERSION"), os = OS, "viewer started");
    let runtime = Builder::new_multi_thread().enable_all().thread_name("radia-service").build()?;
    let services = runtime.handle().clone();
    let asset_cache = match AssetCache::open_default() {
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
    gpui_platform::application().with_assets(Assets).run(move |cx| {
        cx.set_gpu_requirements(Box::new(WgpuDeviceRequirements {
            features: render::required_features(),
            limits: Some(render::required_limits()),
            backends: None,
        }));
        gui::init(cx, services, gui_cache, log_control);
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
    runtime.shutdown_background();
    cache_shutdown
}
