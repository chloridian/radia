use std::{
    backtrace::Backtrace,
    fs::{self, OpenOptions},
    io::{self, IsTerminal},
    panic,
    path::Path,
};

use time::{Duration, OffsetDateTime, UtcOffset, format_description};
use tracing::field;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{
    EnvFilter, Layer,
    fmt::{self, time::OffsetTime},
    layer::SubscriberExt,
    util::SubscriberInitExt,
};

const LOG_DIR: &str = "target/logs";
const MAX_LOGS: usize = 20;

pub(crate) fn init() -> Option<WorkerGuard> {
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

    let (json, guard, file_error, maintenance_warning) = match create_session_log(offset) {
        Ok((file, warning)) => {
            let (writer, guard) = tracing_appender::non_blocking(file);
            let file_filter = EnvFilter::try_from_env("FILE_LOG")
                .unwrap_or_else(|_| {
                    EnvFilter::new(
                        "info,viewer=debug,network=debug,assets=debug,storage=debug,inventory=debug,render=debug,world=info,protocol=info,llsd=info",
                    )
                })
                .add_directive("jpeg2k=warn".parse().expect("valid jpeg2k tracing directive"));
            let json = fmt::layer()
                .json()
                .with_timer(json_timer)
                .with_file(true)
                .with_line_number(true)
                .with_writer(writer)
                .with_filter(file_filter);
            (Some(json), Some(guard), None, warning)
        }
        Err(error) => (None, None, Some(error), None),
    };

    let console_filter = EnvFilter::try_from_env("LOG")
        .unwrap_or_else(|_| EnvFilter::new("warn,viewer=info,network=info"))
        .add_directive("jpeg2k=warn".parse().expect("valid jpeg2k tracing directive"));

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

    guard
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
