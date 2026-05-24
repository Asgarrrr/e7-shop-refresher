use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use std::path::PathBuf;

use eframe::App;
use egui::{Color32, ColorImage, Context, Pos2, TextureHandle, TextureOptions};
use egui_phosphor::regular as icon;
use image::RgbaImage;
use tracing::{debug, error, info, warn};

use crate::capture::WindowCapture;
use crate::config::{Config, MissingTemplate, MissingZone, ShopConfig};
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

/// Tag of the parameter currently being interacted with — drives
/// "show me what I'm editing" overlays on the central snapshot. Stored
/// in egui temp memory so panels can write it and `draw_snapshot` can
/// read it without plumbing through `ShopGui`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EditFocus {
    BuyOffset,
    Jitter,
}

pub(super) fn edit_focus_id() -> egui::Id {
    egui::Id::new("active_edit_focus")
}

pub(super) fn current_edit_focus(ctx: &egui::Context) -> Option<EditFocus> {
    ctx.data(|d| d.get_temp::<EditFocus>(edit_focus_id()))
}

/// Records `focus` as the active edit if `resp` is being hovered,
/// dragged, or focused. Called from each visualisable widget; the
/// reset happens once per frame from `App::update`.
pub(super) fn register_edit_focus(resp: &egui::Response, focus: EditFocus) {
    if resp.hovered() || resp.dragged() || resp.has_focus() {
        resp.ctx.data_mut(|d| d.insert_temp(edit_focus_id(), focus));
    }
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
    /// Live mirror of `config.shop` published every frame; the worker
    /// re-reads it at every round boundary so UI edits to Run-tab fields
    /// take effect without restarting the bot.
    pub(super) live_shop: Arc<RwLock<ShopConfig>>,

    pub(super) snapshot_texture: Option<TextureHandle>,
    pub(super) snapshot_size: Option<[u32; 2]>,
    pub(super) snapshot_rgba: Option<Arc<RgbaImage>>,
    pub(super) snapshot_error: Option<String>,

    pub(super) show_rois: BTreeMap<&'static str, bool>,
    /// Mirror of `zone_drag_target` for the Regions editor — set to the
    /// region name while the user is drawing one on the snapshot.
    pub(super) region_drag_target: Option<&'static str>,

    pub(super) crop_drag_start: Option<Pos2>,
    pub(super) crop_selection: Option<CropRect>,
    pub(super) crop_target: String,
    pub(super) crop_save_error: Option<String>,
    pub(super) crop_save_notice: Option<String>,

    /// When set, the next snapshot drag fills the named zone instead of
    /// being treated as a crop selection.
    pub(super) zone_drag_target: Option<&'static str>,
    pub(super) show_zones: BTreeMap<&'static str, bool>,
    pub(super) zone_status: Vec<MissingZone>,

    pub(super) debug_matches: Vec<DebugMatch>,
    pub(super) debug_error: Option<String>,

    pub(super) saved_snapshot: AutoSavedFields,
    /// Timestamp the live config first diverged from `saved_snapshot`;
    /// drives the auto-save debounce.
    dirty_since: Option<Instant>,
    pub(super) auto_save_error: Option<String>,

    pub(super) active_tab: Tab,

    last_window_poll: Option<Instant>,

    /// Flipped by the Win32 hotkey thread on Ctrl+7; consumed by
    /// `update()` each frame so the same code path handles hotkey
    /// Stop and click Stop.
    stop_hotkey_pressed: Arc<std::sync::atomic::AtomicBool>,
}

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
        let live_shop = Arc::new(RwLock::new(config.shop.clone()));
        let stop_hotkey_pressed = super::hotkey::spawn_stop_hotkey(cc.egui_ctx.clone());
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
            live_shop,
            snapshot_texture: None,
            snapshot_size: None,
            snapshot_rgba: None,
            snapshot_error: None,
            show_rois,
            region_drag_target: None,
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
            last_window_poll: None,
            stop_hotkey_pressed,
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
        // Resize BEFORE WindowCapture spins up its WGC session — doing it
        // after invalidates the frame pool and `capture_image` blocks forever.
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
                // Reuse the GPU texture — `load_texture` would re-upload
                // ~8 MB on every Refresh click.
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
        self.config.template_path(alias)
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
                self.crop_save_notice =
                    Some(format!("saved {} {}", icon::ARROW_RIGHT, path.display()));
                // `base_resolution` records the window size at crop time
                // so scaling stays correct across sessions.
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

    /// Idle re-poll of the game window every ~2 s so the footer reflects
    /// the game closing mid-session — the worker's failure path doesn't
    /// touch `window_error` and the initial acquire only runs at startup.
    pub(super) fn auto_refresh_window_status(&mut self, ctx: &Context) {
        const INTERVAL: Duration = Duration::from_millis(2000);
        if self
            .bot
            .as_ref()
            .is_some_and(super::bot::BotHandle::is_running)
        {
            return;
        }
        let now = Instant::now();
        let due = self
            .last_window_poll
            .map(|t| now.duration_since(t) >= INTERVAL)
            .unwrap_or(true);
        if !due {
            ctx.request_repaint_after(INTERVAL);
            return;
        }
        self.last_window_poll = Some(now);
        self.passive_recheck_window();
        ctx.request_repaint_after(INTERVAL);
    }

    /// Quiet sibling of `try_acquire_window` for the idle poll: no
    /// pre-flight resize, no rebuild of an already-good capture, no
    /// log spam when the game isn't open.
    fn passive_recheck_window(&mut self) {
        match WindowCapture::find(
            &self.config.window.title_contains,
            self.config.window.process_name.as_deref(),
        ) {
            Ok(c) => {
                let rect = c.rect();
                let had_capture = self.capture.is_some();
                self.window_title = c.title().ok();
                self.window_error = None;
                if !had_capture {
                    self.capture = Some(Arc::new(c));
                }
                if let Ok(r) = rect {
                    let new_size = (r.width, r.height);
                    if self.window_size != Some(new_size) {
                        self.window_size = Some(new_size);
                        self.try_build_detector();
                    }
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

    /// One-shot semantics for `sleep_when_done`: turn the checkbox off
    /// the frame after the worker fires `suspend_to_sleep`, so the next
    /// run doesn't silently sleep the PC again.
    pub(super) fn consume_sleep_flag(&mut self) {
        let snap = self.stats.snapshot();
        if !snap.sleep_consumed {
            return;
        }
        self.config.shop.sleep_when_done = false;
        self.stats.update(|s| s.sleep_consumed = false);
    }

    /// Polled each frame: if the Win32 hotkey thread flipped the flag,
    /// route it through `stop_bot()` exactly like a Stop button click.
    /// No-op when no run is in flight — the hotkey is harmless idle.
    pub(super) fn consume_stop_hotkey(&mut self) {
        if !self
            .stop_hotkey_pressed
            .swap(false, std::sync::atomic::Ordering::Relaxed)
        {
            return;
        }
        if self.bot.is_some() {
            info!("stop hotkey pressed");
            self.stop_bot();
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
        // Templates / zones may have changed since startup (PNG dropped
        // in, hand-edited TOML, etc.) — re-check before spawning.
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
            s.mystic_bought = 0;
            s.covenant_bought = 0;
            s.last_error = None;
            s.sub_status = None;
        });
        // Publish before spawn so the worker sees the current UI state
        // on its first read.
        if let Ok(mut shop) = self.live_shop.write() {
            *shop = self.config.shop.clone();
        }
        let handle = BotHandle::spawn(
            self.config.clone(),
            Arc::clone(&self.live_shop),
            capture,
            detector,
            self.stats.clone(),
        )?;
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
            // egui only repaints on input — force a pass after the
            // debounce so the trailing edit gets flushed on idle.
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
                // Leave `dirty_since` / `saved_snapshot` alone so the
                // next frame retries without re-running the debounce.
                self.auto_save_error = Some(e.to_string());
                error!(error = %e, "auto-save failed");
            }
        }
    }

    /// Runs the live bot's NCC pipeline once for the debug overlay, so
    /// what the overlay shows is exactly what the bot would act on.
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
        // detection then runs on.
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
        self.consume_sleep_flag();
        self.consume_stop_hotkey();
        self.auto_refresh_window_status(ctx);
        // Cleared each frame so panels can re-register on hover; the
        // central snapshot renders AFTER the side panels, so it reads
        // the latest value at the bottom of the frame.
        ctx.data_mut(|d| d.remove::<EditFocus>(edit_focus_id()));

        // Repaint while a run is live so progress + logs update.
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
                // Only render when window detection has an issue — the
                // healthy state shows up via the Start button enabling.
                let show_footer = self.window_error.is_some() || self.window_size.is_none();
                if show_footer {
                    egui::TopBottomPanel::bottom("window_status_footer")
                        .resizable(false)
                        .show_separator_line(true)
                        .show_inside(ui, |ui| {
                            crate::gui::panels::draw_window_footer(ui, self);
                        });
                }

                // Above the tabs so a failed Setup-tab edit stays visible
                // even after the user switches to Run.
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

        // Cheap to publish unconditionally: `ShopConfig` is ~10 primitive
        // fields and the write lock is uncontended (only this thread
        // writes, only the worker reads).
        if let Ok(mut shop) = self.live_shop.write() {
            *shop = self.config.shop.clone();
        }

        // After the panels so their mutations land in the same frame.
        self.auto_save_if_dirty(ctx);
    }
}
