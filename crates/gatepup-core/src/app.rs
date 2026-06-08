use std::path::{Path, PathBuf};
use std::sync::Arc;

use arc_swap::ArcSwap;
use gatepup_admin::AdminState;
use gatepup_config::{load_from_file, resolve_config, validate, ConfigSource, GatePupConfig};
use gatepup_observability::Metrics;
use gatepup_proxy::SharedConfig;
use tokio::sync::watch;

use crate::error::CoreError;

/// Load a config file and validate it. The returned config is guaranteed to
/// have passed every validation check.
pub fn load_validated(path: impl AsRef<Path>) -> Result<GatePupConfig, CoreError> {
    let config = load_from_file(path)?;
    validate(&config).map_err(CoreError::Invalid)?;
    Ok(config)
}

/// Resolve the effective config from `--config` and/or the environment, then
/// validate it.
pub fn resolve_validated(
    cli_path: Option<&Path>,
) -> Result<(GatePupConfig, ConfigSource), CoreError> {
    let (config, source) = resolve_config(cli_path)?;
    validate(&config).map_err(CoreError::Invalid)?;
    Ok((config, source))
}

/// Resolve, validate, and render the effective config as pretty JSON.
pub fn print_config(cli_path: Option<&Path>) -> Result<String, CoreError> {
    let (config, _) = resolve_validated(cli_path)?;
    let rendered = serde_json::to_string_pretty(&config)?;
    Ok(rendered)
}

/// Build the runtime, then serve the proxy and (if enabled) the admin server
/// until a shutdown signal (SIGINT/SIGTERM). Composition root: it owns the
/// shutdown and reload signals plus the shared, swappable snapshot, and runs
/// both servers concurrently so a bind failure on either surfaces immediately.
/// SIGHUP re-reads `config_path` and hot-swaps the routing snapshot.
pub async fn serve(config: GatePupConfig, cli_path: Option<PathBuf>) -> Result<(), CoreError> {
    let shared: SharedConfig = Arc::new(ArcSwap::from_pointee(gatepup_proxy::build_snapshot(
        &config,
    )?));
    let effective: Arc<ArcSwap<String>> = Arc::new(ArcSwap::from_pointee(
        serde_json::to_string_pretty(&config)?,
    ));
    let metrics = Arc::new(Metrics::new()?);

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let (reload_tx, reload_rx) = watch::channel(0u64);

    tokio::spawn(signal_loop(
        cli_path,
        shared.clone(),
        effective.clone(),
        metrics.clone(),
        shutdown_tx,
        reload_tx,
    ));

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
                snapshot: shared.clone(),
                metrics: metrics.clone(),
                metrics_path,
                effective_config: effective.clone(),
                version: env!("CARGO_PKG_VERSION"),
            }))
        }
        _ => None,
    };

    let proxy_fut = async {
        gatepup_proxy::serve_shared(
            shared.clone(),
            metrics.clone(),
            shutdown_rx.clone(),
            reload_rx,
        )
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

/// Wait on termination + reload signals. SIGINT/SIGTERM -> shutdown; SIGHUP ->
/// hot reload from `config_path`. Falls back to Ctrl-C-only off Unix.
#[cfg(unix)]
async fn signal_loop(
    cli_path: Option<PathBuf>,
    shared: SharedConfig,
    effective: Arc<ArcSwap<String>>,
    metrics: Arc<Metrics>,
    shutdown_tx: watch::Sender<bool>,
    reload_tx: watch::Sender<u64>,
) {
    use tokio::signal::unix::{signal, Signal, SignalKind};

    async fn recv(sig: &mut Option<Signal>) {
        match sig {
            Some(s) => {
                s.recv().await;
            }
            None => std::future::pending::<()>().await,
        }
    }

    let mut term = signal(SignalKind::terminate()).ok();
    let mut int = signal(SignalKind::interrupt()).ok();
    let mut hup = signal(SignalKind::hangup()).ok();

    loop {
        tokio::select! {
            _ = recv(&mut term) => { let _ = shutdown_tx.send(true); break; }
            _ = recv(&mut int) => { let _ = shutdown_tx.send(true); break; }
            _ = recv(&mut hup) => {
                reload(cli_path.as_deref(), &shared, &effective, &metrics, &reload_tx);
            }
        }
    }
}

#[cfg(not(unix))]
async fn signal_loop(
    _cli_path: Option<PathBuf>,
    _shared: SharedConfig,
    _effective: Arc<ArcSwap<String>>,
    _metrics: Arc<Metrics>,
    shutdown_tx: watch::Sender<bool>,
    _reload_tx: watch::Sender<u64>,
) {
    let _ = tokio::signal::ctrl_c().await;
    let _ = shutdown_tx.send(true);
}

/// Re-read, validate, and hot-swap the routing snapshot. On ANY error the
/// current config is kept (a bad reload never breaks the running proxy).
fn reload(
    cli_path: Option<&Path>,
    shared: &SharedConfig,
    effective: &ArcSwap<String>,
    metrics: &Metrics,
    reload_tx: &watch::Sender<u64>,
) {
    match resolve_validated(cli_path).and_then(|(cfg, _)| {
        let snapshot = gatepup_proxy::build_reload_snapshot(&cfg)?;
        let rendered = serde_json::to_string_pretty(&cfg)?;
        Ok((snapshot, rendered))
    }) {
        Ok((snapshot, rendered)) => {
            shared.store(Arc::new(snapshot));
            effective.store(Arc::new(rendered));
            reload_tx.send_modify(|v| *v = v.wrapping_add(1));
            metrics.inc_config_reload(true);
            tracing::info!("config reloaded");
        }
        Err(err) => {
            metrics.inc_config_reload(false);
            tracing::error!(error = %err, "config reload failed, keeping current config");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn write_config(target_url: &str) -> PathBuf {
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let body = format!(
            r#"{{"app":{{"name":"t"}},"listeners":[{{"name":"l","bind":"127.0.0.1:0","routes":[{{"name":"r","match":{{"pathPrefix":"/"}},"upstream":"u"}}]}}],"upstreams":[{{"name":"u","targets":[{{"url":"{target_url}"}}]}}]}}"#
        );
        let path =
            std::env::temp_dir().join(format!("gatepup-core-{}-{seq}.json", std::process::id()));
        std::fs::write(&path, body).unwrap();
        path
    }

    fn shared_from(path: &Path) -> SharedConfig {
        let cfg = load_validated(path).unwrap();
        Arc::new(ArcSwap::from_pointee(
            gatepup_proxy::build_snapshot(&cfg).unwrap(),
        ))
    }

    #[test]
    fn reload_keeps_old_config_on_invalid() {
        let good = write_config("http://127.0.0.1:1");
        let shared = shared_from(&good);
        let before = shared.load_full();
        let effective = ArcSwap::from_pointee("old".to_string());
        let metrics = Metrics::new().unwrap();
        let (tx, _rx) = watch::channel(0u64);

        reload(
            Some(Path::new("/no/such/gatepup-config.json")),
            &shared,
            &effective,
            &metrics,
            &tx,
        );

        assert!(
            Arc::ptr_eq(&before, &shared.load_full()),
            "an invalid reload must keep the old snapshot"
        );
        assert_eq!(effective.load().as_str(), "old");
    }

    #[test]
    fn reload_swaps_snapshot_on_valid() {
        let a = write_config("http://127.0.0.1:1");
        let b = write_config("http://127.0.0.1:2");
        let shared = shared_from(&a);
        let before = shared.load_full();
        let effective = ArcSwap::from_pointee("old".to_string());
        let metrics = Metrics::new().unwrap();
        let (tx, _rx) = watch::channel(0u64);

        reload(Some(&b), &shared, &effective, &metrics, &tx);

        assert!(
            !Arc::ptr_eq(&before, &shared.load_full()),
            "a valid reload must swap the snapshot"
        );
        assert!(
            effective.load().contains("127.0.0.1:2"),
            "effective config should reflect the reloaded config"
        );
    }
}
