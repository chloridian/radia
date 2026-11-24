use std::{borrow::Cow, env, env::consts::OS, io, sync::Arc};

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
    inspection::inspection_channel,
    logging::{self, LogLevel},
    mcp::{McpServer, McpServerConfig},
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

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct CliOptions {
    mcp: bool,
    mcp_port: u16,
}

fn parse_cli_args(args: impl IntoIterator<Item = String>) -> Result<CliOptions, String> {
    let mut options = CliOptions::default();
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--mcp" => options.mcp = true,
            "--mcp-port" => {
                let value = iter.next().ok_or_else(|| "--mcp-port requires a port number".to_owned())?;
                options.mcp_port = value.parse::<u16>().map_err(|e| format!("invalid --mcp-port '{value}': {e}"))?;
                options.mcp = true;
            }
            _ => {}
        }
    }
    Ok(options)
}

fn main() -> io::Result<()> {
    let cli = parse_cli_args(env::args().skip(1)).map_err(|message| io::Error::new(io::ErrorKind::InvalidInput, message))?;
    let (_logging, log_control, recent_logs) = logging::init();
    tracing::info!(version = env!("CARGO_PKG_VERSION"), os = OS, "viewer started");
    let runtime = Builder::new_multi_thread().enable_all().thread_name("radia-service").build()?;
    let services = runtime.handle().clone();
    let (inspection_publisher, mcp_server) = if cli.mcp {
        let (publisher, handle) = inspection_channel(recent_logs);
        let server = McpServer::start(handle, McpServerConfig { port: cli.mcp_port, ..McpServerConfig::default() }, &services)?;
        println!("MCP server listening on http://{}", server.addr());
        (Some(publisher), Some(server))
    } else {
        (None, None)
    };
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
        gui::init(cx, services, gui_cache, log_control, gui_settings, inspection_publisher);
    });
    if let Some(server) = mcp_server {
        server.shutdown();
    }
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

#[cfg(test)]
mod tests {
    use super::{CliOptions, parse_cli_args};

    #[test]
    fn mcp_cli_off_by_default() {
        let parsed = parse_cli_args(Vec::<String>::new()).expect("empty args");
        assert_eq!(parsed, CliOptions { mcp: false, mcp_port: 0 });
    }

    #[test]
    fn mcp_cli_parses_flag_and_port() {
        let parsed = parse_cli_args(["--mcp-port".to_owned(), "4040".to_owned()]).expect("port args");
        assert_eq!(parsed, CliOptions { mcp: true, mcp_port: 4040 });
    }
}
