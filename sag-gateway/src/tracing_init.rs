use sag_config::LogFormat;
use std::io::IsTerminal;
use tracing_subscriber::EnvFilter;

/// Initialize the tracing subscriber with env-filter support.
///
/// `log_level` is an optional fallback log level from the config file.
/// The `RUST_LOG` environment variable takes precedence over this value.
/// `log_format` controls whether output is human-readable compact or JSON.
pub fn init(log_level: Option<&str>, log_format: LogFormat) {
    let default_level = log_level.unwrap_or("info");
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level));

    let fmt = tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_timer(tracing_subscriber::fmt::time::ChronoLocal::new(
            "%Y-%m-%dT%H:%M:%S%.6f".to_string(),
        ))
        .with_ansi(std::io::stdout().is_terminal())
        .with_target(true)
        .with_thread_ids(false)
        .with_file(false)
        .with_line_number(false);

    match log_format {
        LogFormat::Compact => fmt.compact().init(),
        LogFormat::Json => fmt.json().init(),
    }
}
