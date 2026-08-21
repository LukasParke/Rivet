//! JSON structured logging setup.

use tracing_subscriber::EnvFilter;

pub fn init(env: &str) {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(if env == "dev" { "debug" } else { "info" }));
    let _ = tracing_subscriber::fmt()
        .json()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}
