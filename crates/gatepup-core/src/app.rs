use std::path::Path;
use std::sync::Arc;

use gatepup_config::{load_from_file, validate, GatePupConfig};

use crate::error::CoreError;

/// Load a config file and validate it. The returned config is guaranteed to
/// have passed every validation check.
pub fn load_validated(path: impl AsRef<Path>) -> Result<GatePupConfig, CoreError> {
    let config = load_from_file(path)?;
    validate(&config).map_err(CoreError::Invalid)?;
    Ok(config)
}

/// Load, validate, and render the effective config as pretty JSON.
pub fn print_config(path: impl AsRef<Path>) -> Result<String, CoreError> {
    let config = load_validated(path)?;
    let rendered = serde_json::to_string_pretty(&config)?;
    Ok(rendered)
}

/// Load, validate, build the runtime snapshot, and serve until shutdown.
pub async fn serve_from_file(path: impl AsRef<Path>) -> Result<(), CoreError> {
    let config = load_validated(path)?;
    let snapshot = Arc::new(gatepup_proxy::build_snapshot(&config)?);
    gatepup_proxy::run(snapshot).await?;
    Ok(())
}
