use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use std::path::PathBuf;

use eframe::App;
use egui::{Color32, ColorImage, Context, Pos2, TextureHandle, TextureOptions};
use image::RgbaImage;
use tracing::{debug, error, info, warn};

use crate::capture::WindowCapture;
use crate::config::{Config, MissingTemplate, MissingZone};
use crate::detector::{Detector, Hit};
use crate::error::Result;
use crate::gui::bot::BotHandle;
use crate::gui::logs::LogBuffer;
use crate::gui::persist::{AutoSavedFields, write_all_back};
use crate::gui::state::{BotStatus, SharedStats};

pub(super) mod palette {
    use egui::Color32;
    pub const TEXT_DIM: Color32 = Color32::from_rgb(160, 160, 160);
    pub const TEXT_MUTED: Color32 = Color32::from_rgb(180, 180, 180);
    pub const SECTION_HEADER: Color32 = Color32::from_rgb(230, 230, 235);
    pub const OK: Color32 = Color32::from_rgb(110, 200, 110);
    pub const WARN: Color32 = Color32::from_rgb(230, 180, 60);
    pub const ERROR: Color32 = Color32::from_rgb(220, 90, 90);
    pub const DEBUG_LABEL: Color32 = Color32::from_rgb(255, 240, 100);
    pub const DEBUG_BAND_STROKE: Color32 = Color32::from_rgb(255, 80, 80);

    // Setup-tab chrome: hairline rule under each section header +
    // accent fill for primary buttons. No card backgrounds — sections
    // are separated by whitespace + typography.
    pub const SECTION_STROKE: Color32 = Color32::from_rgb(58, 60, 66);
    pub const ACCENT: Color32 = Color32::from_rgb(80, 140, 255);
    pub const ACCENT_TEXT: Color32 = Color32::from_rgb(240, 245, 255);
}

pub(super) const SECTION_GAP: f32 = 10.0;

/// Coalesces a slider drag (60 writes/s) to one write per drag plus one
/// trailing write. NTFS journaling + AV on-access scans can hitch the UI
/// otherwise.
const AUTO_SAVE_DEBOUNCE_MS: u64 = 250;

pub(super) const ROI_LIST: &[(&str, Color32)] = &[
    ("shop_grid", Color32::from_rgb(220, 70, 70)),
    ("anchor_shop", Color32::from_rgb(80, 140, 255)),
];

pub(super) const ZONE_LIST: &[(&str, Color32)] = &[
    ("refresh", Color32::from_rgb(250, 200, 60)),
    ("refresh_confirm", Color32::from_rgb(60, 220, 200)),
    ("buy_confirm", Color32::from_rgb(255, 140, 60)),
    ("buy_column", Color32::from_rgb(200, 200, 200)),
];

pub(super) const TEMPLATE_ALIASES: &[&str] = &["anchor_shop", "mystic_medal", "covenant"];

#[derive(Debug, Clone, Copy)]
pub(super) struct CropRect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Tab {
    Run,
    Setup,
}

#[derive(Debug, Clone)]
pub(super) struct DebugMatch {
    pub alias: &'static str,
    pub hit: Option<Hit>,
    pub tpl_size: Option<(u32, u32)>,
}

pub struct ShopGui {
    pub(super) config: Config,
    pub(super) config_path: PathBuf,
    pub(super) logs: LogBuffer,
    pub(super) stats: SharedStats,

    pub(super) capture: Option<Arc<WindowCapture>>,
    pub(super) detector: Option<Arc<Detector>>,
    pub(super) window_size: Option<(u32, u32)>,
    pub(super) window_title: Option<String>,
    pub(super) window_error: Option<String>,
    pub(super) template_status: Vec<MissingTemplate>,

    pub(super) bot: Option<BotHandle>,

    pub(super) snapshot_texture: Option<TextureHandle>,
    pub(super) snapshot_size: Option<[u32; 2]>,
    pub(super) snapshot_rgba: Option<Arc<RgbaImage>>,
    pub(super) snapshot_error: Option<String>,

    pub(super) show_rois: BTreeMap<&'static str, bool>,

    pub(super) crop_drag_start: Option<Pos2>,
    pub(super) crop_selection: Option<CropRect>,
    pub(super) crop_target: String,
    pub(super) crop_save_error: Option<String>,
    pub(super) crop_save_notice: Option<String>,

    /// `Some(name)` while a click-drag on the snapshot fills the named
    /// zone instead of acting as a crop selection.
    pub(super) zone_drag_target: Option<&'static str>,
    pub(super) show_zones: BTreeMap<&'static str, bool>,
    pub(super) zone_status: Vec<MissingZone>,

    pub(super) debug_matches: Vec<DebugMatch>,
    pub(super) debug_error: Option<String>,

    pub(super) saved_snapshot: AutoSavedFields,
    /// `Some(t)` when the live config diverged from `saved_snapshot` at
    /// time `t` without yet being written back. Drives the auto-save
    /// debounce.
    dirty_since: Option<Instant>,
    pub(super) auto_save_error: Option<String>,

    pub(super) active_tab: Tab,
}

/// Merges Phosphor regular into the default font set so icon constants
/// from `egui_phosphor::regular` render as glyphs anywhere in the UI.
fn install_icon_font(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
    ctx.set_fonts(fonts);
}

impl ShopGui {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        config: Config,
        config_path: PathBuf,
        logs: LogBuffer,
    ) -> Self {
        install_icon_font(&cc.egui_ctx);

        let mut show_rois = BTreeMap::new();
        for (name, _) in ROI_LIST {
            show_rois.insert(*name, true);
        }
        let mut show_zones = BTreeMap::new();
        for (name, _) in ZONE_LIST {
            show_zones.insert(*name, true);
        }
        let saved_snapshot = AutoSavedFields::from_config(&config);
        let mut gui = Self {
            config,
            config_path,
            logs,
            stats: SharedStats::new(),
            capture: None,
            detector: None,
            window_size: None,
            window_title: None,
            window_error: None,
            template_status: Vec::new(),
            bot: None,
            snapshot_texture: None,
            snapshot_size: None,
            snapshot_rgba: None,
            snapshot_error: None,
            show_rois,
            crop_drag_start: None,
            crop_selection: None,
            crop_target: TEMPLATE_ALIASES[0].to_string(),
            crop_save_error: None,
            crop_save_notice: None,
            zone_drag_target: None,
            show_zones,
            zone_status: Vec::new(),
            debug_matches: Vec::new(),
            debug_error: None,
            saved_snapshot,
            dirty_since: None,
            auto_save_error: None,
            active_tab: Tab::Run,
        };
        gui.refresh_template_status();
        gui.refresh_zone_status();
        gui.try_acquire_window();
        gui
    }

    pub(super) fn refresh_template_status(&mut self) {
        self.template_status = self.config.missing_templates();
    }

    pub(super) fn refresh_zone_status(&mut self) {
        self.zone_status = self.config.missing_zones();
    }

    pub(super) fn zone_mut(&mut self, name: &str) -> Option<&mut Option<[f32; 4]>> {
        match name {
            "refresh" => Some(&mut self.config.zones.refresh),
            "refresh_confirm" => Some(&mut self.config.zones.refresh_confirm),
            "buy_confirm" => Some(&mut self.config.zones.buy_confirm),
            "buy_column" => Some(&mut self.config.zones.buy_column),
            _ => None,
        }
    }

    pub(super) fn try_acquire_window(&mut self) {
        // Pre-flight resize must run BEFORE WindowCapture creates its WGC
        // session — resizing afterwards invalidates the frame pool and
        // makes capture_image() block forever.
        crate::capture::preflight_resize_if_enabled(&self.config.window);

        match WindowCapture::find(
            &self.config.window.title_contains,
            self.config.window.process_name.as_deref(),
        ) {
            Ok(c) => {
                self.window_title = c.title().ok();
                self.window_error = None;
                let rect = c.rect();
                self.capture = Some(Arc::new(c));
                if let Ok(r) = rect {
                    self.window_size = Some((r.width, r.height));
                    self.try_build_detector();
                }
            }
            Err(e) => {
                self.window_error = Some(e.to_string());
                self.capture = None;
                self.detector = None;
                self.window_size = None;
            }
        }
    }

    pub(super) fn try_build_detector(&mut self) {
        self.detector = None;
        if !self.template_status.is_empty() {
            return;
        }
        let Some(size) = self.window_size else { return };
        match Detector::new(&self.config, size) {
            Ok(d) => self.detector = Some(Arc::new(d)),
            Err(e) => {
                error!(error = %e, "failed to build detector");
                self.window_error = Some(e.to_string());
            }
        }
    }

    pub(super) fn refresh_snapshot(&mut self, ctx: &Context) {
        let Some(capture) = self.capture.clone() else {
            self.snapshot_error = Some("no window".into());
            return;
        };
        match capture.snapshot() {
            Ok(rgba) => {
                let size = [rgba.width() as usize, rgba.height() as usize];
                let color_image = ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
                // load_texture allocates a fresh handle each call, which
                // re-uploads ~8 MB per Refresh click. Reuse the existing
                // GPU texture instead.
                match self.snapshot_texture.as_mut() {
                    Some(handle) => handle.set(color_image, TextureOptions::LINEAR),
                    None => {
                        self.snapshot_texture =
                            Some(ctx.load_texture("snapshot", color_image, TextureOptions::LINEAR));
                    }
                }
                self.snapshot_size = Some([rgba.width(), rgba.height()]);
                self.snapshot_rgba = Some(Arc::new(rgba));
                self.snapshot_error = None;
            }
            Err(e) => {
                self.snapshot_error = Some(e.to_string());
                warn!(error = %e, "snapshot failed");
            }
        }
    }

    pub(super) fn region_mut(&mut self, name: &str) -> Option<&mut Option<[f32; 4]>> {
        match name {
            "shop_grid" => Some(&mut self.config.regions.shop_grid),
            "anchor_shop" => Some(&mut self.config.regions.anchor_shop),
            _ => None,
        }
    }

    pub(super) fn template_path_for(&self, alias: &str) -> Option<PathBuf> {
        let t = &self.config.templates;
        let file = match alias {
            "anchor_shop" => &t.anchor_shop,
            "mystic_medal" => &t.mystic_medal,
            "covenant" => &t.covenant,
            _ => return None,
        };
        Some(t.dir.join(file))
    }

    pub(super) fn save_crop(&mut self) {
        self.crop_save_error = None;
        self.crop_save_notice = None;

        let Some(sel) = self.crop_selection else {
            self.crop_save_error = Some("no selection — drag a rectangle on the snapshot".into());
            return;
        };
        if sel.w == 0 || sel.h == 0 {
            self.crop_save_error = Some("selection has zero size".into());
            return;
        }
        let Some(rgba) = self.snapshot_rgba.clone() else {
            self.crop_save_error = Some("no snapshot — click Refresh first".into());
            return;
        };
        let Some(path) = self.template_path_for(&self.crop_target) else {
            self.crop_save_error = Some(format!("unknown alias `{}`", self.crop_target));
            return;
        };

        if let Some(parent) = path.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            self.crop_save_error = Some(format!("cannot create {}: {e}", parent.display()));
            return;
        }

        let cropped = image::imageops::crop_imm(&*rgba, sel.x, sel.y, sel.w, sel.h).to_image();
        match cropped.save(&path) {
            Ok(()) => {
                info!(
                    alias = self.crop_target,
                    path = %path.display(),
                    "template saved"
                );
                self.crop_save_notice = Some(format!("saved → {}", path.display()));
                // base_resolution = window size every template was cropped
                // at. The auto-save tick picks this mutation up.
                if let Some((w, h)) = self.window_size
                    && self.config.window.base_resolution != [w, h]
                {
                    self.config.window.base_resolution = [w, h];
                }
                self.refresh_template_status();
                self.try_build_detector();
            }
            Err(e) => {
                error!(error = %e, path = %path.display(), "template save failed");
                self.crop_save_error = Some(e.to_string());
            }
        }
    }

    pub(super) fn poll_bot(&mut self) {
        let Some(bot) = self.bot.as_mut() else { return };
        if let Some(join_result) = bot.poll() {
            match join_result {
                Ok(Ok(())) => info!("bot finished cleanly"),
                Ok(Err(e)) => error!(error = %e, "bot returned error"),
                Err(_) => error!("bot thread panicked"),
            }
            self.bot = None;
        }
    }

    pub(super) fn start_bot(&mut self) -> Result<()> {
        // Re-check in case the user edited templates/zones since startup
        // (dropped PNGs, drew zones, hand-edited the TOML).
        self.refresh_template_status();
        self.refresh_zone_status();
        if self.detector.is_none() {
            self.try_build_detector();
        }
        let Some(capture) = self.capture.clone() else {
            warn!("cannot start: no window");
            return Ok(());
        };
        let Some(detector) = self.detector.clone() else {
            warn!(
                missing = self.template_status.len(),
                "cannot start: templates missing"
            );
            return Ok(());
        };
        self.stats.update(|s| {
            s.status = BotStatus::Running;
            s.round = 0;
            s.items_bought = 0;
            s.last_error = None;
        });
        let handle = BotHandle::spawn(self.config.clone(), capture, detector, self.stats.clone())?;
        self.bot = Some(handle);
        Ok(())
    }

    pub(super) fn stop_bot(&mut self) {
        if let Some(bot) = self.bot.as_ref() {
            bot.request_stop();
            self.stats.update(|s| s.status = BotStatus::Stopping);
        }
    }

    fn auto_save_if_dirty(&mut self, ctx: &Context) {
        let live = AutoSavedFields::from_config(&self.config);
        if live == self.saved_snapshot {
            self.dirty_since = None;
            return;
        }
        let now = Instant::now();
        let started = *self.dirty_since.get_or_insert(now);
        let debounce = Duration::from_millis(AUTO_SAVE_DEBOUNCE_MS);
        if now.duration_since(started) < debounce {
            // egui only repaints on input. Force one more pass after the
            // debounce so the trailing edit gets persisted when the user
            // idles.
            ctx.request_repaint_after(debounce);
            return;
        }
        match write_all_back(&self.config_path, &self.config) {
            Ok(()) => {
                self.saved_snapshot = live;
                self.dirty_since = None;
                self.auto_save_error = None;
                debug!(path = %self.config_path.display(), "config auto-saved");
            }
            Err(e) => {
                // Keep `dirty_since` so we don't loop the debounce timer;
                // don't update `saved_snapshot` so the next frame retries.
                self.auto_save_error = Some(e.to_string());
                error!(error = %e, "auto-save failed");
            }
        }
    }

    /// Same NCC pipeline as the live bot, so what the overlay shows is
    /// exactly what the bot would act on.
    pub(super) fn run_debug_detection(&mut self, ctx: &Context) {
        self.debug_matches.clear();
        self.debug_error = None;

        let Some(capture) = self.capture.clone() else {
            self.debug_error = Some("no window".into());
            return;
        };
        let Some(detector) = self.detector.clone() else {
            self.debug_error = Some("detector not ready — templates missing?".into());
            return;
        };

        // Snapshot first so the overlay is drawn over the exact pixels
        // detection ran on.
        self.refresh_snapshot(ctx);
        let gray = match capture.snapshot_gray() {
            Ok(g) => g,
            Err(e) => {
                self.debug_error = Some(format!("snapshot failed: {e}"));
                return;
            }
        };

        let targets = self.config.enabled_targets();
        for alias in &targets {
            let hit = detector
                .find(&gray, alias, self.config.regions.shop_grid)
                .ok()
                .flatten();
            let tpl_size = detector.template_dimensions(alias);
            self.debug_matches.push(DebugMatch {
                alias,
                hit,
                tpl_size,
            });
        }
    }
}

impl App for ShopGui {
    fn update(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        self.poll_bot();

        // Repaint during active runs so progress + logs update.
        if self
            .bot
            .as_ref()
            .is_some_and(super::bot::BotHandle::is_running)
        {
            ctx.request_repaint_after(Duration::from_millis(250));
        }

        egui::TopBottomPanel::bottom("logs")
            .resizable(true)
            .min_height(120.0)
            .default_height(180.0)
            .show(ctx, |ui| crate::gui::panels::draw_logs(ui, &self.logs));
        egui::SidePanel::right("controls")
            .min_width(280.0)
            .default_width(310.0)
            .show(ctx, |ui| {
                // Above the tabs so an edit on Setup that failed to
                // persist isn't hidden when the user switches to Run.
                if let Some(err) = &self.auto_save_error {
                    ui.colored_label(
                        Color32::from_rgb(220, 90, 90),
                        format!("auto-save failed: {err}"),
                    );
                    ui.add_space(4.0);
                }
                crate::gui::panels::draw_tab_bar(ui, self);
                ui.add_space(4.0);
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| match self.active_tab {
                        Tab::Run => crate::gui::panels::draw_run_tab(ui, self, ctx),
                        Tab::Setup => crate::gui::panels::draw_setup_tab(ui, self, ctx),
                    });
            });
        egui::CentralPanel::default().show(ctx, |ui| crate::gui::snapshot::draw_snapshot(ui, self));

        // Must run AFTER the panels so their mutations are observed in
        // the same frame.
        self.auto_save_if_dirty(ctx);
    }
}
