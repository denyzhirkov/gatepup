use tracing_subscriber::EnvFilter;

/// Install the global JSON logging subscriber. Call once, at proxy startup.
/// Level precedence: `GATEPUP_LOG` env var, then the config `logLevel`, then
/// `info`. Logs are written to stdout (access logs are meant to be collected).
pub fn init_logging(level: &str) {
    let filter = EnvFilter::try_from_env("GATEPUP_LOG")
        .or_else(|_| EnvFilter::try_new(level))
        .unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::fmt()
        .json()
        .flatten_event(true)
        .with_current_span(false)
        .with_env_filter(filter)
        .with_writer(std::io::stdout)
        .init();
}
