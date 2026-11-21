use std::{
    backtrace::Backtrace,
    fs::{self, OpenOptions},
    io::{self, IsTerminal},
    panic,
    path::Path,
    sync::{Arc, Mutex, PoisonError},
};

use time::{Duration, OffsetDateTime, UtcOffset, format_description};
use tracing::field;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{
    EnvFilter, Layer,
    fmt::{self, time::OffsetTime},
    layer::SubscriberExt,
    reload,
    util::SubscriberInitExt,
};

const LOG_DIR: &str = "target/logs";
const MAX_LOGS: usize = 20;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum LogLevel {
    Error,
    Warn,
    #[default]
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    pub const ALL: [Self; 5] = [Self::Error, Self::Warn, Self::Info, Self::Debug, Self::Trace];

    pub fn label(self) -> &'static str {
        match self {
            Self::Error => "Error",
            Self::Warn => "Warn",
            Self::Info => "Info",
            Self::Debug => "Debug",
            Self::Trace => "Trace",
        }
    }

    pub(crate) fn directive(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
            Self::Trace => "trace",
        }
    }
}

#[derive(Clone)]
pub struct LogControl {
    state: Arc<Mutex<LogState>>,
    reload_console: Arc<dyn Fn(EnvFilter) + Send + Sync>,
    reload_file: Option<Arc<dyn Fn(EnvFilter) + Send + Sync>>,
}

struct LogState {
    level: LogLevel,
    noisy_network: bool,
}

impl LogControl {
    pub fn noop() -> Self {
        Self {
            state: Arc::new(Mutex::new(LogState { level: LogLevel::Info, noisy_network: false })),
            reload_console: Arc::new(|_| {}),
            reload_file: None,
        }
    }

    pub fn level(&self) -> LogLevel {
        self.state.lock().unwrap_or_else(PoisonError::into_inner).level
    }

    pub fn noisy_network(&self) -> bool {
        self.state.lock().unwrap_or_else(PoisonError::into_inner).noisy_network
    }

    pub fn set_level(&self, level: LogLevel) {
        let noisy_network = {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            if state.level == level {
                return;
            }
            state.level = level;
            state.noisy_network
        };
        self.apply(level, noisy_network);
    }

    pub fn set_noisy_network(&self, noisy_network: bool) {
        let level = {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            if state.noisy_network == noisy_network {
                return;
            }
            state.noisy_network = noisy_network;
            state.level
        };
        self.apply(level, noisy_network);
    }

    fn apply(&self, level: LogLevel, noisy_network: bool) {
        (self.reload_console)(build_filter(level, noisy_network));
        if let Some(reload_file) = &self.reload_file {
            reload_file(build_filter(level, noisy_network));
        }
    }
}

pub fn init() -> (Option<WorkerGuard>, LogControl) {
    let offset = UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC);
    let json_timer = OffsetTime::new(
        offset,
        format_description::parse_owned::<2>(
            "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3][offset_hour sign:mandatory]:[offset_minute]",
        )
        .expect("valid json log timestamp format"),
    );
    let console_timer = OffsetTime::new(
        offset,
        format_description::parse_owned::<2>("[hour]:[minute]:[second].[subsecond digits:3]").expect("valid console log timestamp format"),
    );

    let (json, guard, file_reload, file_error, maintenance_warning) = match create_session_log(offset) {
        Ok((file, warning)) => {
            let (writer, guard) = tracing_appender::non_blocking(file);
            let file_filter = EnvFilter::try_from_env("FILE_LOG")
                .unwrap_or_else(|_| {
                    EnvFilter::new(
                        "info,viewer=debug,network=debug,assets=debug,storage=debug,inventory=debug,render=debug,world=info,protocol=info,llsd=info",
                    )
                })
                .add_directive("jpeg2k=warn".parse().expect("valid jpeg2k tracing directive"));
            let (file_filter, file_handle) = reload::Layer::new(file_filter);
            let json = fmt::layer()
                .json()
                .with_timer(json_timer)
                .with_file(true)
                .with_line_number(true)
                .with_writer(writer)
                .with_filter(file_filter);
            let reload_file: Arc<dyn Fn(EnvFilter) + Send + Sync> = Arc::new(move |filter| {
                if let Err(error) = file_handle.reload(filter) {
                    tracing::warn!(%error, "could not reload file log filter");
                }
            });
            (Some(json), Some(guard), Some(reload_file), None, warning)
        }
        Err(error) => (None, None, None, Some(error), None),
    };

    let console_filter = EnvFilter::try_from_env("LOG")
        .unwrap_or_else(|_| EnvFilter::new("warn,viewer=info,network=info"))
        .add_directive("jpeg2k=warn".parse().expect("valid jpeg2k tracing directive"));
    let (console_filter, console_handle) = reload::Layer::new(console_filter);

    let console = fmt::layer()
        .compact()
        .with_timer(console_timer)
        .with_ansi(io::stderr().is_terminal())
        .with_writer(io::stderr)
        .with_filter(console_filter);

    let subscriber = tracing_subscriber::registry().with(console).with(json);

    #[cfg(feature = "tracy")]
    let subscriber = subscriber.with(tracing_tracy::TracyLayer::default());

    if let Err(error) = subscriber.try_init() {
        eprintln!("Could not initialize tracing: {error}");
    }

    if let Some(error) = file_error {
        tracing::warn!(%error, "Could not create session log; continuing with console logging");
    }
    if let Some(warning) = maintenance_warning {
        tracing::warn!(%warning, "Could not maintain session log links or prune old logs");
    }

    let default_hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        tracing::error!(
            target: "viewer::panic",
            panic = info.payload_as_str().unwrap_or("non-string panic payload"),
            location = info.location().map(field::display),
            backtrace = %Backtrace::force_capture(),
            "panic"
        );
        default_hook(info);
    }));

    let control = LogControl {
        state: Arc::new(Mutex::new(LogState { level: LogLevel::Info, noisy_network: false })),
        reload_console: Arc::new(move |filter| {
            if let Err(error) = console_handle.reload(filter) {
                tracing::warn!(%error, "could not reload console log filter");
            }
        }),
        reload_file: file_reload,
    };

    (guard, control)
}

fn filter_spec(level: LogLevel, noisy_network: bool) -> String {
    let base = match level {
        LogLevel::Error => "error",
        LogLevel::Warn | LogLevel::Info | LogLevel::Debug | LogLevel::Trace => "warn",
    };
    let app = level.directive();
    let net = if noisy_network {
        "trace"
    } else {
        match level {
            LogLevel::Error => "error",
            LogLevel::Warn => "warn",
            LogLevel::Info | LogLevel::Debug | LogLevel::Trace => "info",
        }
    };
    format!(
        "{base},viewer={app},assets={app},storage={app},inventory={app},render={app},world={app},llsd={app},network={net},protocol={net},jpeg2k=warn"
    )
}

fn build_filter(level: LogLevel, noisy_network: bool) -> EnvFilter {
    EnvFilter::new(filter_spec(level, noisy_network))
}

fn create_session_log(offset: UtcOffset) -> io::Result<(fs::File, Option<String>)> {
    let dir = Path::new(LOG_DIR);
    fs::create_dir_all(dir)?;

    let stamp_format = format_description::parse_owned::<2>("[year][month][day]-[hour][minute][second]")
        .expect("valid session log filename timestamp format");
    let mut now = OffsetDateTime::now_utc().to_offset(offset);

    let (file, path) = loop {
        let stamp = now.format(&stamp_format).expect("valid session log timestamp");
        let path = dir.join(format!("{stamp}.jsonl"));

        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => break (file, path),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                now += Duration::seconds(1);
            }
            Err(error) => return Err(error),
        }
    };

    let latest = dir.join("latest.jsonl");

    let mut warnings = Vec::new();
    match fs::remove_file(&latest) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => warnings.push(format!("Could not remove the latest session log link: {error}")),
    }

    if let Err(error) = fs::hard_link(&path, &latest) {
        warnings.push(format!("Could not create the latest session log link: {error}"));
    }
    if let Err(error) = prune_logs(dir) {
        warnings.push(format!("Could not prune old session logs: {error}"));
    }

    Ok((file, (!warnings.is_empty()).then(|| warnings.join("; "))))
}

fn prune_logs(dir: &Path) -> io::Result<()> {
    let mut logs = fs::read_dir(dir)?.map(|entry| entry.map(|entry| entry.path())).collect::<io::Result<Vec<_>>>()?;

    logs.retain(|path| {
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            return false;
        };
        let Some(stamp) = name.strip_suffix(".jsonl") else {
            return false;
        };

        let bytes = stamp.as_bytes();
        bytes.len() == 15 && bytes[8] == b'-' && bytes[..8].iter().all(u8::is_ascii_digit) && bytes[9..].iter().all(u8::is_ascii_digit)
    });

    logs.sort();

    let excess = logs.len().saturating_sub(MAX_LOGS);
    for path in logs.into_iter().take(excess) {
        fs::remove_file(path)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{LogLevel, build_filter, filter_spec};

    #[test]
    fn filter_directives_match_levels_and_noisy_network() {
        for level in LogLevel::ALL {
            for noisy in [false, true] {
                let spec = filter_spec(level, noisy);
                let parsed = build_filter(level, noisy).to_string();
                assert!(parsed.contains(&format!("viewer={}", level.directive())), "{spec} -> {parsed}");
                assert!(parsed.contains("jpeg2k=warn"), "{spec} -> {parsed}");
                let expected_net = if noisy {
                    "trace"
                } else {
                    match level {
                        LogLevel::Error => "error",
                        LogLevel::Warn => "warn",
                        LogLevel::Info | LogLevel::Debug | LogLevel::Trace => "info",
                    }
                };
                assert!(parsed.contains(&format!("network={expected_net}")), "{spec} -> {parsed}");
                assert!(parsed.contains(&format!("protocol={expected_net}")), "{spec} -> {parsed}");
            }
        }
    }
}
