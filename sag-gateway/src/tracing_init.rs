use std::io::IsTerminal;
use tracing_subscriber::EnvFilter;

/// Initialize the tracing subscriber with env-filter support and JSON output.
pub fn init() {
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_timer(tracing_subscriber::fmt::time::ChronoLocal::new(
            "%Y-%m-%dT%H:%M:%S%.6f".to_string(),
        ))
        .with_ansi(std::io::stdout().is_terminal())
        .with_target(true)
        .with_thread_ids(false)
        .with_file(false)
        .with_line_number(false)
        .compact()
        .init();
}
