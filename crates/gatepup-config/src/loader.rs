use std::path::Path;

use crate::error::ConfigError;
use crate::model::GatePupConfig;

/// Read and parse a config file. Does **not** validate — call
/// [`crate::validate`] on the result before building a runtime snapshot.
pub fn load_from_file(path: impl AsRef<Path>) -> Result<GatePupConfig, ConfigError> {
    let path = path.as_ref();
    let raw = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    serde_json::from_str(&raw).map_err(|source| ConfigError::Parse {
        path: path.to_path_buf(),
        source,
    })
}
