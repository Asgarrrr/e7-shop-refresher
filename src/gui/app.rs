use std::sync::mpsc;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use std::path::PathBuf;

use eframe::App;
use egui::{Color32, Context, TextureHandle};
use image::RgbaImage;
use tracing::{debug, error, info, warn};

use crate::capture::WindowCapture;
use crate::config::{Config, ShopConfig};
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

/// Coalesces a slider drag (60 writes/s) to one write per drag — NTFS
/// journaling + AV on-access scans hitch the UI otherwise.
const AUTO_SAVE_DEBOUNCE_MS: u64 = 250;

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
    /// Set only when NCC matched this alias but the colour check rejected
    /// the hit — lets the Setup list explain the near-miss instead of
    /// showing the same silence as "not on screen".
    pub colour_reject: Option<crate::color_check::ColourReport>,
}

/// Output of one background capture + NCC pass driven by the Setup-tab
/// auto-refresh worker. `Err` lands in `debug_error`; `Ok` updates the
/// texture and `debug_matches`.
pub(super) struct SetupPreviewResult {
    pub rgba: Arc<RgbaImage>,
    pub matches: Vec<DebugMatch>,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct RunHistoryPoint {
    pub round: u32,
    pub mystic: u32,
    pub covenant: u32,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct DragRect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// Drives the "show me what I'm editing" overlays on the central
/// snapshot. Stored in egui temp memory so panels can write it without
/// plumbing through ShopGui.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EditFocus {
    Jitter,
    /// User is hovering / dragging one of the Buy-click DragValues. Drives
    /// the snapshot overlay to thicken the click band + draw the exact
    /// click line so alignment with the in-game buy button is visible.
    BuyClick,
    /// User is hovering / dragging an x/y/w/h DragValue for one of the
    /// layout rects. Snapshot thickens the matching overlay so the user
    /// sees which rect they're about to nudge. Carries the same name
    /// `crate::layout::overlay_rects()` emits.
    Rect(&'static str),
}

/// Which Buy-click handle the user is currently dragging on the snapshot.
/// Set on drag_started after proximity-test, cleared on drag_stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BuyDragHandle {
    Line,
    Box,
}

pub(super) fn edit_focus_id() -> egui::Id {
    egui::Id::new("active_edit_focus")
}

pub(super) fn current_edit_focus(ctx: &egui::Context) -> Option<EditFocus> {
    ctx.data(|d| d.get_temp::<EditFocus>(edit_focus_id()))
}

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

    pub(super) bot: Option<BotHandle>,
    /// Worker re-reads at every round boundary so UI edits apply
    /// without restarting.
    pub(super) live_shop: Arc<RwLock<ShopConfig>>,

    pub(super) snapshot_texture: Option<TextureHandle>,
    pub(super) snapshot_size: Option<[u32; 2]>,
    pub(super) snapshot_rgba: Option<Arc<RgbaImage>>,
    pub(super) snapshot_error: Option<String>,

    pub(super) show_layout_overlay: bool,

    /// Template alias the next snapshot drag will crop into. Region /
    /// zone overrides are typed by hand in the Layout card and don't go
    /// through the drag flow.
    pub(super) override_drag: Option<&'static str>,
    pub(super) override_drag_rect: Option<DragRect>,
    /// Preserved across frames so a wobble back past the anchor bounds
    /// against the original click position, not the running cursor min.
    pub(super) override_drag_anchor: Option<(u32, u32)>,

    pub(super) debug_matches: Vec<DebugMatch>,
    pub(super) debug_error: Option<String>,

    pub(super) saved_snapshot: AutoSavedFields,
    dirty_since: Option<Instant>,
    pub(super) auto_save_error: Option<String>,

    pub(super) active_tab: Tab,

    /// Which Buy-click handle the user is currently dragging on the
    /// snapshot, if any. Persists across frames because egui's drag
    /// events fire over many frames.
    pub(super) buy_drag_handle: Option<BuyDragHandle>,

    /// Wall clock of the last Setup-tab auto-refresh + detection.
    /// Drives the 2 Hz live preview without re-capturing every frame.
    pub(super) last_setup_refresh: Option<Instant>,
    /// Set while a Setup-tab preview worker is still capturing — gates
    /// the next spawn so we never queue more than one in flight.
    pub(super) setup_preview_in_flight: bool,
    /// `String` errors (vs `crate::error::Error`) so the channel can also
    /// carry "worker panicked" — a case `crate::error::Error` doesn't have
    /// a variant for and shouldn't grow one for an internal concern.
    pub(super) setup_preview_tx: mpsc::Sender<std::result::Result<SetupPreviewResult, String>>,
    pub(super) setup_preview_rx: mpsc::Receiver<std::result::Result<SetupPreviewResult, String>>,

    last_window_poll: Option<Instant>,

    stop_hotkey_pressed: Arc<std::sync::atomic::AtomicBool>,

    pub(super) bot_started_at: Option<Instant>,

    /// Per-round samples for the Run-tab progress sparkline. Append-only
    /// during a run, cleared on `start_bot`.
    pub(super) run_history: Vec<RunHistoryPoint>,
    last_recorded_round: u32,

    /// `Some` only when strictly newer than `CARGO_PKG_VERSION`.
    pub(super) update_status: crate::update_check::UpdateStatus,

    pub(super) webhook_test_status: crate::notifications::WebhookTestStatus,

    pub(super) update_progress: Option<UpdateProgress>,
    /// Auto-update rollback (.bak) is dropped only after the first
    /// successful repaint, so a crashing new binary still has the bak
    /// on disk for a manual recovery.
    pub(super) bak_cleaned: bool,
}

pub(super) struct UpdateProgress {
    pub state: UpdateState,
    /// `None` when no install thread is running (e.g. the install
    /// path failed before the worker was spawned). Skips the per-frame
    /// try_recv so a banner-only Failed state costs nothing.
    pub rx: Option<std::sync::mpsc::Receiver<crate::auto_update::UpdateEvent>>,
}

#[derive(Debug, Clone)]
pub(super) enum UpdateState {
    Downloading { bytes: u64, total: Option<u64> },
    Verifying,
    Installing,
    Failed(String),
}

fn install_icon_font(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
    ctx.set_fonts(fonts);
}

/// Pinned so a light-mode host OS doesn't render our dark-tuned palette
/// on white and turn the UI illegible.
fn force_dark_theme(ctx: &egui::Context) {
    ctx.set_theme(egui::Theme::Dark);
}

impl ShopGui {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        config: Config,
        config_path: PathBuf,
        logs: LogBuffer,
    ) -> Self {
        install_icon_font(&cc.egui_ctx);
        force_dark_theme(&cc.egui_ctx);

        let saved_snapshot = AutoSavedFields::from_config(&config);
        let live_shop = Arc::new(RwLock::new(config.shop.clone()));
        let stop_hotkey_pressed = super::hotkey::spawn_stop_hotkey(cc.egui_ctx.clone());
        let update_status = crate::update_check::UpdateStatus::new();
        // Next to config.toml so portable installs carry it along.
        let cache_path = config_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join("update_cache.json");
        crate::update_check::spawn_check(update_status.clone(), cache_path);
        let (setup_preview_tx, setup_preview_rx) = mpsc::channel();
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
            bot: None,
            live_shop,
            snapshot_texture: None,
            snapshot_size: None,
            snapshot_rgba: None,
            snapshot_error: None,
            show_layout_overlay: true,
            override_drag: None,
            override_drag_rect: None,
            override_drag_anchor: None,
            debug_matches: Vec::new(),
            debug_error: None,
            saved_snapshot,
            dirty_since: None,
            auto_save_error: None,
            active_tab: Tab::Run,
            buy_drag_handle: None,
            last_setup_refresh: None,
            setup_preview_in_flight: false,
            setup_preview_tx,
            setup_preview_rx,
            last_window_poll: None,
            stop_hotkey_pressed,
            bot_started_at: None,
            run_history: Vec::new(),
            last_recorded_round: 0,
            update_status,
            webhook_test_status: crate::notifications::WebhookTestStatus::new(),
            update_progress: None,
            bak_cleaned: false,
        };
        gui.try_acquire_window();
        gui
    }

    /// No-op while an update is in flight.
    pub(super) fn start_auto_update(&mut self) {
        if self.update_progress.is_some() {
            return;
        }
        let Some(tag) = self.update_status.snapshot() else {
            return;
        };
        let target = match crate::auto_update::ReleaseTarget::for_running_binary(tag) {
            Ok(t) => t,
            Err(e) => {
                self.update_progress = Some(UpdateProgress {
                    state: UpdateState::Failed(e.to_string()),
                    rx: None,
                });
                return;
            }
        };
        let (tx, rx) = std::sync::mpsc::channel();
        crate::auto_update::spawn_install(target, tx);
        // Pre-seed so the banner shows Downloading before the first
        // byte lands.
        self.update_progress = Some(UpdateProgress {
            state: UpdateState::Downloading {
                bytes: 0,
                total: None,
            },
            rx: Some(rx),
        });
    }

    pub(super) fn poll_update_progress(&mut self) {
        let Some(p) = self.update_progress.as_mut() else {
            return;
        };
        let Some(rx) = p.rx.as_ref() else {
            return;
        };
        use crate::auto_update::UpdateEvent;
        while let Ok(event) = rx.try_recv() {
            p.state = match event {
                UpdateEvent::Downloading { bytes, total } => {
                    UpdateState::Downloading { bytes, total }
                }
                UpdateEvent::Verifying => UpdateState::Verifying,
                UpdateEvent::InstallingAndRestarting => UpdateState::Installing,
                UpdateEvent::Failed(msg) => UpdateState::Failed(msg),
            };
        }
    }

    pub(super) fn clear_override_drag(&mut self) {
        self.override_drag = None;
        self.override_drag_rect = None;
        self.override_drag_anchor = None;
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
        // Resize BEFORE WindowCapture spins up its WGC session — after
        // invalidates the frame pool and `capture_image` blocks forever.
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
        let Some(size) = self.window_size else {
            self.detector = None;
            return;
        };
        // Swap on success — a failed rebuild keeps the previous Detector
        // live instead of blanking the bot.
        match Detector::new(&self.config, size) {
            Ok(d) => self.detector = Some(Arc::new(d)),
            Err(e) => {
                error!(error = %e, "failed to build detector — keeping previous if any");
                self.window_error = Some(e.to_string());
            }
        }
    }

    pub(super) fn template_path_for(&self, alias: &str) -> Option<PathBuf> {
        let file = match alias {
            "mystic_medal" => &self.config.templates.mystic_medal,
            "covenant" => &self.config.templates.covenant,
            _ => return None,
        };
        Some(self.config.template_dir().join(file))
    }

    pub(super) fn save_template_from_patch(&mut self, alias: &'static str, patch: RgbaImage) {
        let Some(path) = self.template_path_for(alias) else {
            warn!(alias, "unknown template alias — drop");
            return;
        };
        if let Some(parent) = path.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            self.snapshot_error = Some(format!("cannot create {}: {e}", parent.display()));
            return;
        }
        if let Err(e) = patch.save(&path) {
            self.snapshot_error = Some(format!("template save failed: {e}"));
            return;
        }
        info!(alias, path = %path.display(), "template override saved");
        // Pin base_resolution to the SNAPSHOT's size (what the patch
        // was cropped against), not the live window — the user may
        // have resized between Refresh and committing the drag.
        if let Some([w, h]) = self.snapshot_size {
            self.config.window.base_resolution = [w, h];
        }
        self.try_build_detector();
    }

    pub(super) fn region_mut(&mut self, name: &str) -> Option<&mut Option<[f32; 4]>> {
        match name {
            "shop_grid" => Some(&mut self.config.regions.shop_grid),
            _ => None,
        }
    }

    /// Footer reflects the game closing mid-session — worker's failure
    /// path doesn't touch `window_error` and the initial acquire only
    /// runs at startup.
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

    /// Quiet sibling of `try_acquire_window` for the idle poll — no
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

    /// The webhook dispatch is in the worker (so it can complete before
    /// any `suspend_to_sleep`) — GUI just clears bookkeeping here.
    pub(super) fn consume_finish_reason(&mut self) {
        let snap = self.stats.snapshot();
        if snap.finish_reason.is_none() {
            return;
        }
        self.bot_started_at = None;
        self.stats.update(|s| s.finish_reason = None);
    }

    /// One-shot — uncheck the box after the worker fires
    /// `suspend_to_sleep` so the next run doesn't silently sleep again.
    pub(super) fn consume_sleep_flag(&mut self) {
        let snap = self.stats.snapshot();
        if !snap.sleep_consumed {
            return;
        }
        self.config.shop.sleep_when_done = false;
        self.stats.update(|s| s.sleep_consumed = false);
    }

    pub(super) fn record_run_progress(&mut self) {
        let snap = self.stats.snapshot();
        if snap.round == self.last_recorded_round {
            return;
        }
        self.run_history.push(RunHistoryPoint {
            round: snap.round,
            mystic: snap.mystic_bought,
            covenant: snap.covenant_bought,
        });
        self.last_recorded_round = snap.round;
    }

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
                Ok(Err(e)) => {
                    error!(error = %e, "bot returned error");
                    // The runner may have returned Err without going through
                    // ProgressSink::failed — set Failed here so the UI exits
                    // the Running/Stopping state regardless.
                    let msg = e.to_string();
                    self.stats.update(|s| {
                        s.status = crate::gui::state::BotStatus::Failed;
                        s.last_error = Some(msg);
                        s.sub_status = None;
                        s.finish_reason = None;
                    });
                }
                Err(_) => {
                    error!("bot thread panicked");
                    self.stats.update(|s| {
                        s.status = crate::gui::state::BotStatus::Failed;
                        s.last_error = Some("worker thread panicked".into());
                        s.sub_status = None;
                        s.finish_reason = None;
                    });
                }
            }
            self.bot = None;
        }
    }

    pub(super) fn start_bot(&mut self) -> Result<()> {
        if self.detector.is_none() {
            self.try_build_detector();
        }
        let Some(capture) = self.capture.clone() else {
            warn!("cannot start: no window");
            return Ok(());
        };
        let Some(detector) = self.detector.clone() else {
            warn!("cannot start: detector unavailable (bundled fallback failed?)");
            return Ok(());
        };

        // Force back to the resolution the templates were cropped at
        // — avoids mis-scale when the user resized between calibration
        // and run.
        let target = self.config.window.base_resolution;
        match capture.rect() {
            Ok(r) if [r.width, r.height] != target => {
                match capture.resize_to(target[0], target[1]) {
                    Ok(true) => {
                        if let Ok(now) = capture.rect() {
                            info!(
                                from = format!("{}x{}", r.width, r.height),
                                to = format!("{}x{}", now.width, now.height),
                                target = format!("{}x{}", target[0], target[1]),
                                "window resized to crop-time resolution"
                            );
                            self.window_size = Some((now.width, now.height));
                        }
                    }
                    Ok(false) => warn!("cannot resize: HWND not resolved"),
                    Err(e) => warn!(error = %e, "resize to base_resolution failed — continuing"),
                }
            }
            Ok(_) => {}
            Err(e) => warn!(error = %e, "cannot read window rect — skipping pre-flight resize"),
        }
        self.stats.update(|s| {
            s.status = BotStatus::Running;
            s.round = 0;
            s.items_bought = 0;
            s.mystic_bought = 0;
            s.covenant_bought = 0;
            s.last_error = None;
            s.sub_status = None;
            s.finish_reason = None;
        });
        self.bot_started_at = Some(Instant::now());
        self.run_history.clear();
        self.last_recorded_round = 0;
        // Publish before spawn so the worker's first read is current.
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
            // egui repaints on input only — force a pass after the
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
                // Leave dirty_since/saved_snapshot so the next frame
                // retries without re-running the debounce.
                self.auto_save_error = Some(e.to_string());
                error!(error = %e, "auto-save failed");
            }
        }
    }
}

impl App for ShopGui {
    fn update(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        self.poll_bot();
        self.record_run_progress();
        self.consume_finish_reason();
        self.consume_sleep_flag();
        self.consume_stop_hotkey();
        self.auto_refresh_window_status(ctx);
        self.poll_update_progress();
        // Repaint at ~10 Hz during an update so the byte counter
        // advances visibly.
        if self.update_progress.is_some() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        // Cleared each frame so panels can re-register on hover. The
        // central snapshot renders after the side panels, reading the
        // latest value at the bottom of the frame.
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
            .min_width(320.0)
            .default_width(360.0)
            .show(ctx, |ui| {
                // Only render on a window-detection issue — the healthy
                // state shows up via the Start button enabling.
                let show_footer = self.window_error.is_some() || self.window_size.is_none();
                if show_footer {
                    egui::TopBottomPanel::bottom("window_status_footer")
                        .resizable(false)
                        .show_separator_line(true)
                        .show_inside(ui, |ui| {
                            crate::gui::panels::draw_window_footer(ui, self);
                        });
                }
                if self.update_status.snapshot().is_some() {
                    egui::TopBottomPanel::bottom("update_banner")
                        .resizable(false)
                        .show_separator_line(true)
                        .show_inside(ui, |ui| {
                            crate::gui::panels::draw_update_banner(ui, self);
                        });
                }

                // Above the tabs so a failed Setup-tab edit stays
                // visible even after the user switches to Run.
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

        // Cheap to publish unconditionally — `ShopConfig` is ~10
        // primitive fields and the lock is uncontended (this thread
        // writes, the worker reads).
        if let Ok(mut shop) = self.live_shop.write() {
            *shop = self.config.shop.clone();
        }

        // After the panels so their mutations land in the same frame.
        self.auto_save_if_dirty(ctx);

        // Drop the auto-update rollback only after the new binary has
        // proven it can render — if eframe panicked mid-frame above
        // we wouldn't reach here and the .bak would survive for a
        // manual recovery.
        if !self.bak_cleaned {
            self.bak_cleaned = true;
            crate::auto_update::cleanup_previous_bak();
        }
    }
}
