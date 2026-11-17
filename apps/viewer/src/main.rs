mod logging;

use std::io;
use std::sync::Arc;

use gpui_component_assets::Assets;
use tokio::runtime::Builder;

fn main() -> io::Result<()> {
    let _logging = logging::init();
    tracing::info!(version = env!("CARGO_PKG_VERSION"), os = std::env::consts::OS, "viewer started");
    let runtime = Builder::new_multi_thread().enable_all().thread_name("radia-service").build()?;
    let services = runtime.handle().clone();
    let asset_cache = match assets::cache::AssetCache::open_default() {
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
    gpui_platform::application().with_assets(Assets).run(move |cx| viewer::gui::init(cx, services, gui_cache));
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
