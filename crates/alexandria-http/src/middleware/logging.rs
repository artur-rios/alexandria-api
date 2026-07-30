use tracing_subscriber::EnvFilter;

use alexandria_core::config::LogLevel;

pub fn init_tracing(level: &LogLevel) {
    let filter = EnvFilter::try_new(level.as_str()).unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}
