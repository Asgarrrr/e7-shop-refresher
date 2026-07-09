//! Point d'entrée du relais Secret Shop Watcher.

use std::process::ExitCode;

use tracing_subscriber::EnvFilter;

use arkyve_refresh_shop::{app, Config};

const CONFIG_PATH: &str = "config.toml";

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("arkyve_refresh_shop=info,warn")),
        )
        .with_target(false)
        .init();

    let config = match Config::load(CONFIG_PATH) {
        Ok(config) => config,
        Err(err) => {
            eprintln!("Configuration invalide : {err}");
            return ExitCode::FAILURE;
        }
    };

    match app::run(config).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("Erreur fatale : {err}");
            ExitCode::FAILURE
        }
    }
}
