use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use clap::Parser;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

use e7_shop_refresher::capture::WindowCapture;
use e7_shop_refresher::config::Config;
use e7_shop_refresher::detector::Detector;
use e7_shop_refresher::input::Clicker;
use e7_shop_refresher::shop::ShopRunner;

#[derive(Parser, Debug)]
#[command(version, about = "Epic Seven shop refresh automation (STOVE PC)")]
struct Cli {
    /// Path to the TOML config file. Defaults to a `config.toml` next to
    /// the .exe (portable mode) if one exists, otherwise
    /// `%APPDATA%\e7-shop-refresher\config.toml`.
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Locate window, load templates, but do not click
    #[arg(long)]
    dry_run: bool,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    e7_shop_refresher::init();

    let cli = Cli::parse();
    let config_path = cli
        .config
        .unwrap_or_else(e7_shop_refresher::config::default_config_path);
    let (cfg, created) = Config::load_or_init(&config_path)?;
    if created {
        info!(path = %config_path.display(), "config not found — wrote defaults");
    }
    info!(path = %config_path.display(), version = cfg.version, "config loaded");

    // Must run before WindowCapture spins up its WGC session —
    // resizing afterwards invalidates the frame pool.
    e7_shop_refresher::capture::preflight_resize_if_enabled(&cfg.window);

    let capture = WindowCapture::find(
        &cfg.window.title_contains,
        cfg.window.process_name.as_deref(),
    )?;
    let rect = capture.rect()?;
    info!(
        title = ?capture.title().unwrap_or_default(),
        x = rect.x,
        y = rect.y,
        size = format!("{}x{}", rect.width, rect.height),
        "game window located"
    );

    let detector = Detector::new(&cfg, (rect.width, rect.height))?;
    info!(
        templates = detector.template_count(),
        "templates loaded and scaled"
    );

    if cli.dry_run {
        info!("dry-run complete — exiting before any clicks");
        return Ok(());
    }

    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = Arc::clone(&stop);
        ctrlc::set_handler(move || {
            info!("Ctrl-C received, stopping after current step");
            stop.store(true, Ordering::Relaxed);
        })?;
    }

    let clicker = Clicker::new(cfg.timing.clone(), Arc::clone(&stop))?;

    // Headless: live_shop just mirrors the loaded config and never
    // changes. The runner still re-reads it each round (cheap) — same
    // code path the GUI uses.
    let live_shop = Arc::new(RwLock::new(cfg.shop.clone()));
    let mut runner = ShopRunner::new(
        Arc::new(capture),
        Arc::new(detector),
        Box::new(clicker),
        cfg,
        live_shop,
        stop,
    );
    if let Err(e) = runner.run() {
        error!(error = %e, "runner failed");
        return Err(e.into());
    }

    Ok(())
}
