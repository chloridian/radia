use std::{
    backtrace::Backtrace,
    collections::{BTreeMap, VecDeque},
    env,
    error::Error,
    fmt as std_fmt,
    fs::{self, OpenOptions},
    io::{self, IsTerminal},
    panic,
    path::Path,
    sync::{Arc, Mutex, PoisonError},
    time::{SystemTime, UNIX_EPOCH},
};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime, UtcOffset, format_description};
use tracing::{
    Event, Level, Subscriber,
    field::{self, Field, Visit},
};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{
    EnvFilter, Layer,
    fmt::{self, time::OffsetTime},
    layer::{Context, SubscriberExt},
    reload,
    util::SubscriberInitExt,
};

const LOG_DIR: &str = "target/logs";
const MAX_LOGS: usize = 20;
const RECENT_LOG_CAPACITY: usize = 256;
const MAX_LOG_TEXT_CHARS: usize = 512;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
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

    pub fn from_directive(directive: &str) -> Option<Self> {
        match directive {
            "error" => Some(Self::Error),
            "warn" => Some(Self::Warn),
            "info" => Some(Self::Info),
            "debug" => Some(Self::Debug),
            "trace" => Some(Self::Trace),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LogFilterError(String);

impl std_fmt::Display for LogFilterError {
    fn fmt(&self, formatter: &mut std_fmt::Formatter<'_>) -> std_fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for LogFilterError {}

#[derive(Clone)]
pub struct LogControl {
    state: Arc<Mutex<LogState>>,
    reload_console: Arc<dyn Fn(EnvFilter) + Send + Sync>,
    reload_file: Option<Arc<dyn Fn(EnvFilter) + Send + Sync>>,
}

struct LogState {
    level: LogLevel,
    noisy_network: bool,
    target_overrides: BTreeMap<String, LogLevel>,
}

impl LogControl {
    pub fn noop() -> Self {
        Self {
            state: Arc::new(Mutex::new(LogState { level: LogLevel::Info, noisy_network: false, target_overrides: BTreeMap::new() })),
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

    pub fn filter(&self) -> String {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        filter_spec(state.level, state.noisy_network, &state.target_overrides)
    }

    pub fn set_level(&self, level: LogLevel) {
        let (noisy_network, overrides) = {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            if state.level == level {
                return;
            }
            state.level = level;
            (state.noisy_network, state.target_overrides.clone())
        };
        self.apply(level, noisy_network, &overrides);
    }

    pub fn set_noisy_network(&self, noisy_network: bool) {
        let (level, overrides) = {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            if state.noisy_network == noisy_network {
                return;
            }
            state.noisy_network = noisy_network;
            (state.level, state.target_overrides.clone())
        };
        self.apply(level, noisy_network, &overrides);
    }

    pub fn set_filter(&self, level: Option<LogLevel>, target: Option<(&str, Option<LogLevel>)>) -> Result<String, LogFilterError> {
        if let Some((target_name, _)) = target
            && !is_valid_log_target(target_name)
        {
            return Err(LogFilterError(format!("invalid log target `{target_name}`")));
        }
        let (spec, console_filter, file_filter) = {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            let next_level = level.unwrap_or(state.level);
            let mut next_overrides = state.target_overrides.clone();
            if let Some((target_name, target_level)) = target {
                match target_level {
                    Some(target_level) => {
                        next_overrides.insert(target_name.to_owned(), target_level);
                    }
                    None => {
                        next_overrides.remove(target_name);
                    }
                }
            }
            let spec = filter_spec(next_level, state.noisy_network, &next_overrides);
            let console_filter = try_build_filter(&spec)?;
            let file_filter = self.reload_file.is_some().then(|| try_build_filter(&spec)).transpose()?;
            state.level = next_level;
            state.target_overrides = next_overrides;
            (spec, console_filter, file_filter)
        };
        (self.reload_console)(console_filter);
        if let (Some(reload_file), Some(file_filter)) = (&self.reload_file, file_filter) {
            reload_file(file_filter);
        }
        Ok(spec)
    }

    pub fn apply_startup_settings(&self, level: LogLevel, noisy_network: bool) {
        let overrides = {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            state.level = level;
            state.noisy_network = noisy_network;
            state.target_overrides.clone()
        };
        if env::var_os("LOG").is_none() {
            (self.reload_console)(build_filter(level, noisy_network, &overrides));
        }
        if env::var_os("FILE_LOG").is_none()
            && let Some(reload_file) = &self.reload_file
        {
            reload_file(build_filter(level, noisy_network, &overrides));
        }
    }

    fn apply(&self, level: LogLevel, noisy_network: bool, overrides: &BTreeMap<String, LogLevel>) {
        (self.reload_console)(build_filter(level, noisy_network, overrides));
        if let Some(reload_file) = &self.reload_file {
            reload_file(build_filter(level, noisy_network, overrides));
        }
    }
}

fn is_valid_log_target(target: &str) -> bool {
    !target.is_empty()
        && target.len() <= 128
        && !target.starts_with(':')
        && !target.ends_with(':')
        && target.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b':' | b'-'))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecentLogEntry {
    pub timestamp_ms: u64,
    pub level: LogLevel,
    pub target: String,
    pub message: String,
}

#[derive(Clone)]
pub struct RecentLogs {
    entries: Arc<Mutex<VecDeque<RecentLogEntry>>>,
    capacity: usize,
}

impl Default for RecentLogs {
    fn default() -> Self {
        Self::with_capacity(RECENT_LOG_CAPACITY)
    }
}

impl RecentLogs {
    pub fn with_capacity(capacity: usize) -> Self {
        Self { entries: Arc::new(Mutex::new(VecDeque::with_capacity(capacity))), capacity }
    }

    pub fn push(&self, entry: RecentLogEntry) {
        if self.capacity == 0 {
            return;
        }
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        if entries.len() >= self.capacity {
            entries.pop_front();
        }
        entries.push_back(entry);
    }

    pub fn snapshot(&self, limit: usize, min_level: Option<LogLevel>) -> Vec<RecentLogEntry> {
        let entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        let filtered = entries.iter().filter(|entry| match min_level {
            Some(LogLevel::Error) => entry.level == LogLevel::Error,
            Some(LogLevel::Warn) | None => matches!(entry.level, LogLevel::Warn | LogLevel::Error),
            Some(_) => true,
        });
        let mut all: Vec<RecentLogEntry> = filtered.cloned().collect();
        let excess = all.len().saturating_sub(limit);
        all.drain(..excess);
        all
    }
}

struct RecentLogLayer {
    logs: RecentLogs,
}

impl<S: Subscriber> Layer<S> for RecentLogLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let metadata = event.metadata();
        let level = match *metadata.level() {
            Level::ERROR => LogLevel::Error,
            Level::WARN => LogLevel::Warn,
            _ => return,
        };
        let mut visitor = RecentLogVisitor::default();
        event.record(&mut visitor);
        let timestamp_ms = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
        self.logs.push(RecentLogEntry {
            timestamp_ms,
            level,
            target: truncate_chars(metadata.target(), 128),
            message: truncate_chars(&visitor.finish(), MAX_LOG_TEXT_CHARS),
        });
    }
}

#[derive(Default)]
struct RecentLogVisitor {
    message: String,
    fields: String,
}

impl RecentLogVisitor {
    fn finish(self) -> String {
        if self.fields.is_empty() {
            self.message
        } else if self.message.is_empty() {
            self.fields
        } else {
            format!("{} ({})", self.message, self.fields)
        }
    }

    fn append_field(&mut self, name: &str, value: &str) {
        if is_sensitive_field(name) {
            return;
        }
        if name == "message" {
            self.message = value.to_owned();
            return;
        }
        if !self.fields.is_empty() {
            self.fields.push_str(", ");
        }
        self.fields.push_str(name);
        self.fields.push('=');
        self.fields.push_str(value);
    }
}

impl Visit for RecentLogVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std_fmt::Debug) {
        self.append_field(field.name(), &format!("{value:?}"));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.append_field(field.name(), value);
    }
}

fn is_sensitive_field(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.contains("secret")
        || lower.contains("token")
        || lower.contains("password")
        || lower.contains("cookie")
        || lower.contains("authorization")
        || lower.contains("credential")
        || lower.contains("capability")
        || lower.contains("session_id")
        || lower.contains("circuit_code")
        || lower == "url"
        || lower.ends_with("_url")
}

fn truncate_chars(input: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (index, ch) in input.chars().enumerate() {
        if index >= max_chars {
            out.push_str("...");
            break;
        }
        out.push(ch);
    }
    out
}

pub fn init() -> (Option<WorkerGuard>, LogControl, RecentLogs) {
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
                });
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

    let console_filter = EnvFilter::try_from_env("LOG").unwrap_or_else(|_| EnvFilter::new("warn,viewer=info,network=info"));
    let (console_filter, console_handle) = reload::Layer::new(console_filter);

    let console = fmt::layer()
        .compact()
        .with_timer(console_timer)
        .with_ansi(io::stderr().is_terminal())
        .with_writer(io::stderr)
        .with_filter(console_filter);

    let recent_logs = RecentLogs::default();
    let recent_layer = RecentLogLayer { logs: recent_logs.clone() };
    let subscriber = tracing_subscriber::registry().with(console).with(json).with(recent_layer);

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
        state: Arc::new(Mutex::new(LogState { level: LogLevel::Info, noisy_network: false, target_overrides: BTreeMap::new() })),
        reload_console: Arc::new(move |filter| {
            if let Err(error) = console_handle.reload(filter) {
                tracing::warn!(%error, "could not reload console log filter");
            }
        }),
        reload_file: file_reload,
    };

    (guard, control, recent_logs)
}

fn filter_spec(level: LogLevel, noisy_network: bool, overrides: &BTreeMap<String, LogLevel>) -> String {
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
    let mut spec = format!(
        "{base},viewer={app},assets={app},storage={app},inventory={app},render={app},world={app},llsd={app},network={net},protocol={net}"
    );
    for (target, target_level) in overrides {
        spec.push(',');
        spec.push_str(target);
        spec.push('=');
        spec.push_str(target_level.directive());
    }
    spec
}

fn try_build_filter(spec: &str) -> Result<EnvFilter, LogFilterError> {
    EnvFilter::try_new(spec).map_err(|error| LogFilterError(format!("invalid log filter: {error}")))
}

fn build_filter(level: LogLevel, noisy_network: bool, overrides: &BTreeMap<String, LogLevel>) -> EnvFilter {
    EnvFilter::new(filter_spec(level, noisy_network, overrides))
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

        let Ok(bytes) = <[u8; 15]>::try_from(stamp.as_bytes()) else {
            return false;
        };
        let (date, rest) = bytes.split_at(8);
        matches!(rest, [b'-', time @ ..] if date.iter().all(u8::is_ascii_digit) && time.iter().all(u8::is_ascii_digit))
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
    use std::collections::BTreeMap;

    use tracing::subscriber::with_default;
    use tracing_subscriber::layer::SubscriberExt;

    use super::{LogLevel, RecentLogLayer, RecentLogs, build_filter, filter_spec};

    #[test]
    fn recent_logs_captures_warn_and_error_with_bound() {
        let logs = RecentLogs::with_capacity(2);
        let subscriber = tracing_subscriber::registry().with(RecentLogLayer { logs: logs.clone() });
        with_default(subscriber, || {
            tracing::debug!("ignored debug");
            tracing::info!("ignored info");
            tracing::warn!(url = "https://cap.example/secret", "first warning");
            tracing::error!("first error");
            tracing::warn!("second warning");
        });
        let entries = logs.snapshot(10, None);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].level, LogLevel::Error);
        assert!(entries[0].message.contains("first error"));
        assert_eq!(entries[1].level, LogLevel::Warn);
        assert!(entries[1].message.contains("second warning"));
        let errors_only = logs.snapshot(10, Some(LogLevel::Error));
        assert_eq!(errors_only.len(), 1);
        assert_eq!(errors_only[0].level, LogLevel::Error);
    }

    #[test]
    fn recent_logs_omits_sensitive_fields() {
        let logs = RecentLogs::with_capacity(4);
        let subscriber = tracing_subscriber::registry().with(RecentLogLayer { logs: logs.clone() });
        with_default(subscriber, || {
            tracing::warn!(
                capability_url = "https://cap.example/abc",
                session_id = "secret-session",
                detail = "visible-detail",
                "sanitized warning"
            );
        });
        let entries = logs.snapshot(4, None);
        assert_eq!(entries.len(), 1);
        assert!(entries[0].message.contains("sanitized warning"));
        assert!(entries[0].message.contains("visible-detail"));
        assert!(!entries[0].message.contains("cap.example"));
        assert!(!entries[0].message.contains("secret-session"));
    }

    #[test]
    fn directives_match_levels() {
        let overrides = BTreeMap::new();
        for level in LogLevel::ALL {
            for noisy in [false, true] {
                let spec = filter_spec(level, noisy, &overrides);
                let parsed = build_filter(level, noisy, &overrides).to_string();
                assert!(parsed.contains(&format!("viewer={}", level.directive())), "{spec} -> {parsed}");
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
