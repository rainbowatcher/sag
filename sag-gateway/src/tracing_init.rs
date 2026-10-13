use sag_config::LogFormat;
use std::io::IsTerminal;
use std::path::Path;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::EnvFilter;

/// Initialize the tracing subscriber with env-filter support.
///
/// `log_dir` is the directory for the daily rotating log files (production
/// resolution: `$XDG_STATE_HOME/sag/logs`, see `sag_core::paths`).
/// `log_level` is an optional fallback log level from the config file.
/// The `RUST_LOG` environment variable takes precedence over this value.
/// `log_format` controls whether output is human-readable compact or JSON.
///
/// In dev mode (stdout is a TTY), logs are written to stdout only.
/// In production/test-env mode (stdout is not a TTY), logs are written to
/// rotating daily log files under `log_dir` (e.g.
/// `$XDG_STATE_HOME/sag/logs/sag.2026-06-26.log`) via `tracing-appender`.
/// The returned `WorkerGuard` must be held for the lifetime of the process —
/// dropping it flushes and shuts down the non-blocking writer thread.
pub fn init(log_dir: &Path, log_level: Option<&str>, log_format: LogFormat) -> Option<WorkerGuard> {
    let default_level = log_level.unwrap_or("info");
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level));

    let is_tty = std::io::stdout().is_terminal();

    let fmt = tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_timer(tracing_subscriber::fmt::time::ChronoLocal::new(
            "%Y-%m-%dT%H:%M:%S%.6f".to_string(),
        ))
        .with_ansi(is_tty)
        .with_target(true)
        .with_thread_ids(false)
        .with_file(false)
        .with_line_number(false);

    if is_tty {
        // Dev mode: write to stdout only (same as before).
        match log_format {
            LogFormat::Compact => fmt.compact().init(),
            LogFormat::Json => fmt.json().init(),
        }
        None
    } else {
        // Production / test-env: write to rotating daily log files under the
        // XDG state log dir. tracing-appender creates the directory
        // automatically.
        let file_appender = RollingFileAppender::new(Rotation::DAILY, log_dir, "sag.log");
        let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

        match log_format {
            LogFormat::Compact => fmt.compact().with_writer(non_blocking).init(),
            LogFormat::Json => fmt.json().with_writer(non_blocking).init(),
        }

        Some(guard)
    }
}
