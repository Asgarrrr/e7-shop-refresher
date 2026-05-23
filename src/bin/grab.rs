//! `grab` — snapshot the game window and dump it to disk with an ROI
//! overlay. Useful for preparing templates and verifying `[regions]`.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::Parser;
use image::{Rgba, RgbaImage};
use imageproc::drawing::draw_hollow_rect_mut;
use imageproc::rect::Rect;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use e7_shop_refresher::capture::WindowCapture;
use e7_shop_refresher::config::Config;

#[derive(Parser, Debug)]
#[command(version, about = "Snapshot the game window and render configured ROIs")]
struct Cli {
    /// Path to the TOML config file
    #[arg(short, long, default_value = "config.toml")]
    config: PathBuf,

    /// Output directory for snapshots
    #[arg(short, long, default_value = "captures")]
    out: PathBuf,

    /// Skip the ROI overlay PNG
    #[arg(long)]
    no_overlay: bool,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    e7_shop_refresher::init();

    let cli = Cli::parse();
    let (cfg, created) = Config::load_or_init(&cli.config)?;
    if created {
        info!(path = %cli.config.display(), "config not found — wrote defaults");
    }
    std::fs::create_dir_all(&cli.out)?;

    let capture = WindowCapture::find(
        &cfg.window.title_contains,
        cfg.window.process_name.as_deref(),
    )?;
    let rect = capture.rect()?;
    info!(
        title = ?capture.title().unwrap_or_default(),
        size = format!("{}x{}", rect.width, rect.height),
        "game window located"
    );

    let snapshot: RgbaImage = capture.snapshot()?;
    let stamp = timestamp();

    let raw_path = cli.out.join(format!("snapshot_{stamp}.png"));
    snapshot.save(&raw_path)?;
    info!(path = %raw_path.display(), "raw snapshot saved");

    if !cli.no_overlay {
        let overlay_path = cli.out.join(format!("snapshot_{stamp}_overlay.png"));
        let mut overlay = snapshot.clone();
        draw_rois(&mut overlay, &cfg);
        overlay.save(&overlay_path)?;
        info!(path = %overlay_path.display(), "overlay saved");
    }

    print_roi_table(&cfg, snapshot.width(), snapshot.height());
    Ok(())
}

#[derive(Clone, Copy)]
struct RoiViz {
    name: &'static str,
    color: Rgba<u8>,
    rect: Option<[f32; 4]>,
}

fn rois(cfg: &Config) -> [RoiViz; 2] {
    [
        RoiViz {
            name: "shop_grid",
            color: Rgba([220, 70, 70, 255]),
            rect: cfg.regions.shop_grid,
        },
        RoiViz {
            name: "anchor_shop",
            color: Rgba([80, 140, 255, 255]),
            rect: cfg.regions.anchor_shop,
        },
    ]
}

fn draw_rois(img: &mut RgbaImage, cfg: &Config) {
    let (w, h) = (img.width(), img.height());
    for viz in rois(cfg) {
        let Some(r) = viz.rect else { continue };
        let Some(px) = roi_to_pixels(r, w, h) else {
            warn!(name = viz.name, "ROI out of bounds, skipped");
            continue;
        };
        // 3 nested rects = thick border that survives downscaling.
        for inset in 0..3 {
            let rect = Rect::at(px.0 + inset, px.1 + inset).of_size(
                (px.2 as i32 - 2 * inset).max(1) as u32,
                (px.3 as i32 - 2 * inset).max(1) as u32,
            );
            draw_hollow_rect_mut(img, rect, viz.color);
        }
    }
}

fn print_roi_table(cfg: &Config, w: u32, h: u32) {
    println!();
    println!("Configured ROIs at {w}×{h}:");
    println!(
        "  {:<14}  {:>6}  {:>6}  {:>6}  {:>6}",
        "name", "x", "y", "w", "h"
    );
    println!("  {}", "─".repeat(48));
    for viz in rois(cfg) {
        match viz.rect.and_then(|r| roi_to_pixels(r, w, h)) {
            Some((x, y, rw, rh)) => println!(
                "  {:<14}  {:>6}  {:>6}  {:>6}  {:>6}",
                viz.name, x, y, rw, rh
            ),
            None => println!("  {:<14}  (unset)", viz.name),
        }
    }
    println!();
}

fn roi_to_pixels(roi: [f32; 4], w: u32, h: u32) -> Option<(i32, i32, u32, u32)> {
    let [rx, ry, rw, rh] = roi;
    let x = (rx * w as f32).round() as i32;
    let y = (ry * h as f32).round() as i32;
    let rw_px = (rw * w as f32).round() as u32;
    let rh_px = (rh * h as f32).round() as u32;
    if rw_px == 0 || rh_px == 0 {
        return None;
    }
    Some((x, y, rw_px, rh_px))
}

fn timestamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    format!("{secs}")
}
