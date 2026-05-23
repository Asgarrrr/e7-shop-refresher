pub mod app;
pub mod bot;
pub mod logs;
mod panels;
mod persist;
mod snapshot;
pub mod state;

use std::path::Path;

use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use crate::config::Config;
use crate::gui::app::ShopGui;
use crate::gui::logs::LogBuffer;

pub fn run(config_path: &Path) -> anyhow::Result<()> {
    let logs = LogBuffer::new();
    install_tracing(&logs);

    crate::init();

    let (config, created) = Config::load_or_init(config_path)?;
    if created {
        tracing::info!(path = %config_path.display(), "config not found — wrote defaults");
    }
    tracing::info!(?config_path, "config loaded");

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1180.0, 760.0])
            .with_min_inner_size([900.0, 600.0])
            .with_title("E7 Shop Refresher")
            .with_icon(load_window_icon()),
        ..Default::default()
    };

    let config_path = config_path.to_path_buf();
    eframe::run_native(
        "e7-shop-refresher",
        native_options,
        Box::new(move |cc| Ok(Box::new(ShopGui::new(cc, config, config_path, logs)))),
    )
    .map_err(|e| anyhow::anyhow!("eframe failed: {e}"))
}

/// Decoded once at startup. The PNG is embedded at compile time so the
/// binary stays self-contained.
fn load_window_icon() -> egui::IconData {
    let bytes = include_bytes!("../../assets/icon.png");
    let img = image::load_from_memory(bytes)
        .expect("embedded icon.png is valid")
        .to_rgba8();
    let (width, height) = img.dimensions();
    egui::IconData {
        rgba: img.into_raw(),
        width,
        height,
    }
}

fn install_tracing(logs: &LogBuffer) {
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let ui_layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_target(false)
        .with_writer(logs.clone());
    let stderr_layer = tracing_subscriber::fmt::layer().with_writer(std::io::stderr);
    tracing_subscriber::registry()
        .with(env_filter)
        .with(ui_layer)
        .with(stderr_layer)
        .init();
}
