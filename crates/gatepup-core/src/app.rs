use std::path::Path;
use std::sync::Arc;

use gatepup_admin::AdminState;
use gatepup_config::{load_from_file, validate, GatePupConfig};
use gatepup_observability::Metrics;
use tokio::sync::watch;

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

/// Build the runtime, then serve the proxy and (if enabled) the admin server
/// until Ctrl-C. This is the composition root: it owns the shutdown signal and
/// the shared metrics, and runs both servers concurrently so a bind failure on
/// either surfaces immediately.
pub async fn serve(config: GatePupConfig) -> Result<(), CoreError> {
    let snapshot = Arc::new(gatepup_proxy::build_snapshot(&config)?);
    let metrics = Arc::new(Metrics::new()?);
    let effective_config = Arc::new(serde_json::to_string_pretty(&config)?);

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            let _ = shutdown_tx.send(true);
        }
    });

    let admin_state = match &config.admin {
        Some(admin) if admin.enabled => {
            let bind = admin
                .bind
                .parse()
                .map_err(|_| CoreError::AdminBind(admin.bind.clone()))?;
            let metrics_path = config
                .metrics
                .as_ref()
                .filter(|m| m.enabled)
                .map(|m| m.path.clone());
            Some(Arc::new(AdminState {
                bind,
                snapshot: snapshot.clone(),
                metrics: metrics.clone(),
                metrics_path,
                effective_config: effective_config.clone(),
                version: env!("CARGO_PKG_VERSION"),
            }))
        }
        _ => None,
    };

    let proxy_fut = async {
        gatepup_proxy::serve(snapshot.clone(), metrics.clone(), shutdown_rx.clone())
            .await
            .map_err(CoreError::from)
    };
    let admin_fut = async {
        match admin_state {
            Some(state) => gatepup_admin::serve(state, shutdown_rx.clone())
                .await
                .map_err(CoreError::from),
            None => Ok(()),
        }
    };

    tokio::try_join!(proxy_fut, admin_fut)?;
    Ok(())
}
