use std::sync::Arc;
use std::time::{Duration, Instant};

use egui::{Color32, Pos2, Rect, Sense, Stroke, StrokeKind, Vec2};
use egui_phosphor::regular as icon;
use image::RgbaImage;
use tracing::{info, warn};

use crate::color_check::ColourReport;
use crate::detector::Hit;
use crate::gui::app::{
    BuyDragHandle, DebugMatch, DragRect, SetupPreviewResult, ShopGui, Tab, palette,
};
use crate::gui::bot::effective_status;

pub(super) fn draw_snapshot(ui: &mut egui::Ui, gui: &mut ShopGui) {
    if gui.active_tab == Tab::Run {
        draw_onboarding(ui, gui);
        return;
    }

    maybe_auto_refresh_setup(gui, ui.ctx());

    let Some(texture) = gui.snapshot_texture.as_ref() else {
        draw_onboarding(ui, gui);
        return;
    };

    if setup_incomplete(gui) {
        draw_compact_stepper(ui, gui);
        ui.add_space(6.0);
    }

    let available = ui.available_size();
    let response = ui.add(
        egui::Image::from_texture(texture)
            .max_size(available)
            .maintain_aspect_ratio(true)
            .fit_to_exact_size(available)
            .sense(Sense::click_and_drag()),
    );
    let image_rect = response.rect;
    let snap_size = gui.snapshot_size.unwrap_or([1, 1]);

    // Drag handler: only fires when a template Edit is armed from the
    // Layout card. Otherwise the snapshot panel is pure display.
    if gui.override_drag.is_some() {
        handle_override_drag(gui, &response, image_rect, snap_size);
    }

    let painter = ui.painter_at(image_rect);

    // Debug overlays drawn last so the layout overlay doesn't obscure them.
    let snap_w = snap_size[0] as f32;
    let snap_h = snap_size[1] as f32;
    let edit_focus = crate::gui::app::current_edit_focus(ui.ctx());

    // Live layout overlay: where the bot WILL look (Search) and click
    // (Click) on the next round. Reads the EFFECTIVE rect per element
    // (override-or-bundled-default) so users who've drawn a custom one
    // see their own value.
    if gui.show_layout_overlay {
        const SEARCH_COLOR: Color32 = Color32::from_rgb(0x4a, 0xd9, 0x90); // green
        const CLICK_COLOR: Color32 = Color32::from_rgb(0xff, 0x9d, 0x4a); // orange
        for (name, default_ratio, kind) in crate::layout::overlay_rects() {
            let color = match kind {
                crate::layout::OverlayKind::Search => SEARCH_COLOR,
                crate::layout::OverlayKind::Click => CLICK_COLOR,
            };
            let effective = effective_rect(gui, &name, default_ratio);
            let rect = ratio_rect(image_rect, effective);
            // Mirror BuyClick: hover/drag of the row's x/y/w/h inputs
            // thickens the stroke + deepens the fill so the user sees
            // which rect they're about to nudge.
            let focused =
                matches!(edit_focus, Some(crate::gui::app::EditFocus::Rect(n)) if n == name);
            let (fill_alpha, stroke_w) = if focused { (70, 2.5) } else { (30, 1.5) };
            // Click zones get a faint fill so they read as "target",
            // search regions stay stroke-only so they read as "look here".
            if kind == crate::layout::OverlayKind::Click {
                let fill =
                    Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), fill_alpha);
                painter.rect_filled(rect, 0.0, fill);
            }
            painter.rect_stroke(rect, 0.0, Stroke::new(stroke_w, color), StrokeKind::Inside);
            painter.text(
                rect.left_top() + Vec2::new(3.0, 1.0),
                egui::Align2::LEFT_TOP,
                &name,
                egui::FontId::proportional(10.0),
                color,
            );
        }
    }

    // Buy-click calibration overlay: a full-width LINE the user drags to
    // an item centre (their reference) and a BOX at line + offset that
    // they drag onto that row's Buy button. Box width follows
    // `zones.buy_column` so it reads as the column-bound click target.
    // Always drawn (even with no detection) — calibration is the whole
    // point of the Setup tab, so the affordances must be visible.
    if gui.show_layout_overlay {
        let line_ratio = gui.config.shop.buy_calibration_line_y_ratio;
        let offset = gui.config.shop.buy_button_y_offset_ratio;
        let band_h_ratio = gui.config.shop.buy_button_band_h_ratio;
        let column = gui.config.zones.buy_column.unwrap_or([
            crate::layout::BUY_COLUMN_X,
            0.0,
            crate::layout::BUY_COLUMN_W,
            0.0,
        ]);

        let line_y_screen = image_rect.min.y + line_ratio * image_rect.height();
        let click_y_ratio = line_ratio + offset;
        let box_x_ratio = column[0];
        let box_w_ratio = column[2];
        let box_y_ratio = (click_y_ratio - band_h_ratio * 0.5).clamp(0.0, 1.0);
        let box_rect = ratio_rect(
            image_rect,
            [box_x_ratio, box_y_ratio, box_w_ratio, band_h_ratio],
        );

        handle_buy_click_drag(gui, &response, image_rect, line_y_screen, box_rect);

        let editing = edit_focus == Some(crate::gui::app::EditFocus::BuyClick)
            || gui.buy_drag_handle.is_some();
        let line_stroke = if editing { 2.0 } else { 1.0 };
        let (fill_alpha, box_stroke) = if editing { (120, 2.5) } else { (70, 1.5) };

        // Reference line — full width, drawn first so the box sits on top.
        painter.line_segment(
            [
                egui::pos2(image_rect.min.x, line_y_screen),
                egui::pos2(image_rect.max.x, line_y_screen),
            ],
            Stroke::new(line_stroke, palette::DEBUG_BAND_STROKE),
        );
        painter.rect_filled(
            box_rect,
            0.0,
            Color32::from_rgba_unmultiplied(255, 100, 100, fill_alpha),
        );
        painter.rect_stroke(
            box_rect,
            0.0,
            Stroke::new(box_stroke, palette::DEBUG_BAND_STROKE),
            StrokeKind::Inside,
        );
    }

    for m in &gui.debug_matches {
        let Some(hit) = m.hit.as_ref() else { continue };
        let label_color = palette::DEBUG_LABEL;

        // hit.x/y is the match CENTER; offset by half template size
        // to get the top-left for the bbox.
        if let Some((tw, th)) = m.tpl_size {
            let half_w = (tw as f32 * hit.scale * 0.5) as i32;
            let half_h = (th as f32 * hit.scale * 0.5) as i32;
            let x0 = (hit.x - half_w).max(0) as f32;
            let y0 = (hit.y - half_h).max(0) as f32;
            let bx_ratio = x0 / snap_w;
            let by_ratio = y0 / snap_h;
            let bw_ratio = (tw as f32 * hit.scale) / snap_w;
            let bh_ratio = (th as f32 * hit.scale) / snap_h;
            let rect = ratio_rect(image_rect, [bx_ratio, by_ratio, bw_ratio, bh_ratio]);
            painter.rect_stroke(rect, 0.0, Stroke::new(2.5, label_color), StrokeKind::Inside);
            painter.text(
                rect.left_top() + Vec2::new(2.0, -14.0),
                egui::Align2::LEFT_TOP,
                format!("{} {:.3}", m.alias, hit.score),
                egui::FontId::proportional(13.0),
                label_color,
            );
        }
    }

    // Jitter preview: a dotted circle around each detected match. Zone
    // clicks (refresh, the confirm modals, buy_column) pick a uniform
    // random point inside the zone and DON'T run Rayleigh jitter on top
    // — only NCC-matched item clicks do — so drawing the radius around
    // those is the only honest place for the preview.
    if edit_focus == Some(crate::gui::app::EditFocus::Jitter) && snap_w > 0.0 && snap_h > 0.0 {
        let radius_px = gui.config.timing.jitter_radius_px.max(0.0);
        let radius_screen = radius_px * (image_rect.width() / snap_w);
        let any_match = gui.debug_matches.iter().any(|m| m.hit.is_some());
        if any_match && radius_px > 0.0 {
            for m in &gui.debug_matches {
                let Some(hit) = m.hit.as_ref() else { continue };
                let center = Pos2::new(
                    image_rect.min.x + (hit.x as f32 / snap_w) * image_rect.width(),
                    image_rect.min.y + (hit.y as f32 / snap_h) * image_rect.height(),
                );
                draw_dotted_circle(&painter, center, radius_screen, palette::DEBUG_LABEL);
            }
        } else if !any_match {
            painter.text(
                image_rect.center(),
                egui::Align2::CENTER_CENTER,
                "Click \"Run detection\" first to preview the jitter scatter on each matched item.",
                egui::FontId::proportional(13.0),
                palette::DEBUG_LABEL,
            );
        }
    }

    // In-flight override drag preview — a faint rect under the cursor
    // so the user sees what they're about to commit.
    if let Some(sel) = gui.override_drag_rect {
        let rect = drag_to_screen_rect(sel, image_rect, snap_size);
        painter.rect_filled(
            rect,
            0.0,
            Color32::from_rgba_unmultiplied(255, 255, 255, 30),
        );
        painter.rect_stroke(
            rect,
            0.0,
            Stroke::new(2.0, Color32::WHITE),
            StrokeKind::Inside,
        );
    }
}

// "Config override if set, else bundled default" — keeps the overlay
// showing what the runner will actually use.
fn effective_rect(gui: &ShopGui, name: &str, default_ratio: [f32; 4]) -> [f32; 4] {
    match name {
        "shop_grid" => gui.config.regions.shop_grid.unwrap_or(default_ratio),
        "refresh" => gui.config.zones.refresh.unwrap_or(default_ratio),
        "refresh_confirm" => gui.config.zones.refresh_confirm.unwrap_or(default_ratio),
        "buy_confirm" => gui.config.zones.buy_confirm.unwrap_or(default_ratio),
        "buy_column" => gui.config.zones.buy_column.unwrap_or(default_ratio),
        _ => default_ratio,
    }
}

fn handle_override_drag(
    gui: &mut ShopGui,
    response: &egui::Response,
    image_rect: Rect,
    snap_size: [u32; 2],
) {
    if response.drag_started()
        && let Some(pos) = response.interact_pointer_pos()
    {
        let (px, py) = screen_to_pixel(pos, image_rect, snap_size);
        gui.override_drag_anchor = Some((px, py));
        gui.override_drag_rect = Some(DragRect {
            x: px,
            y: py,
            w: 0,
            h: 0,
        });
    }
    if response.dragged()
        && let Some(now) = response.interact_pointer_pos()
        && let Some(anchor) = gui.override_drag_anchor
    {
        gui.override_drag_rect = Some(rect_from_pixel_corners(anchor, now, image_rect, snap_size));
    }
    if response.drag_stopped() {
        gui.override_drag_anchor = None;
        let Some(alias) = gui.override_drag.take() else {
            gui.override_drag_rect = None;
            return;
        };
        let Some(rect) = gui.override_drag_rect.take() else {
            return;
        };
        if rect.w == 0 || rect.h == 0 {
            return;
        }
        commit_template_crop(gui, alias, rect);
    }
}

/// Direct-manipulation of the Buy-click calibration overlay: drag the
/// line to set the icon-row reference, drag the box to set the click
/// offset. No-op while a template Edit is armed (override_drag wins).
fn handle_buy_click_drag(
    gui: &mut ShopGui,
    response: &egui::Response,
    image_rect: Rect,
    line_y_screen: f32,
    box_rect: Rect,
) {
    // Override-drag (template crop) consumes the same response; let it
    // take priority so an armed Edit still works on top of the overlay.
    if gui.override_drag.is_some() {
        return;
    }

    // 8 px slack so a line 1 px thick is still grabbable. Box wins ties
    // because its hit area is larger and intentional.
    const LINE_PROXIMITY_PX: f32 = 8.0;

    if response.drag_started()
        && gui.buy_drag_handle.is_none()
        && let Some(pos) = response.interact_pointer_pos()
    {
        if box_rect.contains(pos) {
            gui.buy_drag_handle = Some(BuyDragHandle::Box);
        } else if (pos.y - line_y_screen).abs() <= LINE_PROXIMITY_PX {
            gui.buy_drag_handle = Some(BuyDragHandle::Line);
        }
    }

    if let Some(handle) = gui.buy_drag_handle
        && response.dragged()
    {
        // Image height in screen px maps 1:1 to window-height fraction
        // (the image fills its rect, so 1 ratio unit = image_rect.height()).
        let delta_ratio = response.drag_delta().y / image_rect.height().max(1.0);
        match handle {
            BuyDragHandle::Line => {
                let r = &mut gui.config.shop.buy_calibration_line_y_ratio;
                *r = (*r + delta_ratio).clamp(0.0, 1.0);
            }
            BuyDragHandle::Box => {
                // Offset's upper bound matches the DragValue range so a
                // wild drag can't push the value out of the configurable
                // space.
                let r = &mut gui.config.shop.buy_button_y_offset_ratio;
                *r = (*r + delta_ratio).clamp(0.0, 0.15);
            }
        }
    }

    if response.drag_stopped() {
        gui.buy_drag_handle = None;
    }
}

fn commit_template_crop(gui: &mut ShopGui, alias: &'static str, rect: DragRect) {
    let Some(rgba) = gui.snapshot_rgba.clone() else {
        gui.snapshot_error =
            Some("no snapshot to crop from — wait for the live preview to load".into());
        return;
    };
    let (img_w, img_h) = (rgba.width(), rgba.height());
    let x = rect.x.min(img_w.saturating_sub(1));
    let y = rect.y.min(img_h.saturating_sub(1));
    let w = rect.w.min(img_w - x);
    let h = rect.h.min(img_h - y);
    if w == 0 || h == 0 {
        return;
    }
    let patch = image::imageops::crop_imm(&*rgba, x, y, w, h).to_image();
    gui.save_template_from_patch(alias, patch);
    info!(alias, "template override updated from drag");
}

fn rect_from_pixel_corners(
    start: (u32, u32),
    end_screen: Pos2,
    image_rect: Rect,
    snap_size: [u32; 2],
) -> DragRect {
    let (ex, ey) = screen_to_pixel(end_screen, image_rect, snap_size);
    let x = start.0.min(ex);
    let y = start.1.min(ey);
    let w = start.0.max(ex).saturating_sub(x);
    let h = start.1.max(ey).saturating_sub(y);
    DragRect { x, y, w, h }
}

fn screen_to_pixel(p: Pos2, image_rect: Rect, snap_size: [u32; 2]) -> (u32, u32) {
    let nx = ((p.x - image_rect.min.x) / image_rect.width().max(1.0)).clamp(0.0, 1.0);
    let ny = ((p.y - image_rect.min.y) / image_rect.height().max(1.0)).clamp(0.0, 1.0);
    let x = (nx * snap_size[0] as f32).round() as u32;
    let y = (ny * snap_size[1] as f32).round() as u32;
    (
        x.min(snap_size[0].saturating_sub(1)),
        y.min(snap_size[1].saturating_sub(1)),
    )
}

fn drag_to_screen_rect(sel: DragRect, image_rect: Rect, snap_size: [u32; 2]) -> Rect {
    let nx = sel.x as f32 / snap_size[0] as f32;
    let ny = sel.y as f32 / snap_size[1] as f32;
    let nw = sel.w as f32 / snap_size[0] as f32;
    let nh = sel.h as f32 / snap_size[1] as f32;
    Rect::from_min_size(
        Pos2::new(
            image_rect.min.x + nx * image_rect.width(),
            image_rect.min.y + ny * image_rect.height(),
        ),
        Vec2::new(nw * image_rect.width(), nh * image_rect.height()),
    )
}

pub(super) fn ratio_rect(container: Rect, [rx, ry, rw, rh]: [f32; 4]) -> Rect {
    let min = container.left_top() + Vec2::new(rx * container.width(), ry * container.height());
    let size = Vec2::new(rw * container.width(), rh * container.height());
    Rect::from_min_size(Pos2::new(min.x, min.y), size)
}

/// Approximates a dotted ring by sampling N short arcs around the circle.
/// egui has no native dashed-stroke API for circles.
fn draw_dotted_circle(painter: &egui::Painter, center: Pos2, radius: f32, color: Color32) {
    const SEGMENTS: usize = 24;
    let stroke = Stroke::new(1.5, color);
    let step = std::f32::consts::TAU / SEGMENTS as f32;
    for i in 0..SEGMENTS {
        // Every other segment skipped — that's the "dotted" look.
        if i.is_multiple_of(2) {
            continue;
        }
        let a0 = i as f32 * step;
        let a1 = (i + 1) as f32 * step;
        let p0 = center + Vec2::new(a0.cos(), a0.sin()) * radius;
        let p1 = center + Vec2::new(a1.cos(), a1.sin()) * radius;
        painter.line_segment([p0, p1], stroke);
    }
}

/// First-run empty state — central panel before any snapshot exists.
fn draw_onboarding(ui: &mut egui::Ui, gui: &ShopGui) {
    let has_window = gui.window_size.is_some();
    let estimated_h = if has_window { 440.0 } else { 160.0 };
    let pad_top = ((ui.available_height() - estimated_h) * 0.5).max(20.0);
    ui.add_space(pad_top);

    ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui| {
        ui.label(egui::RichText::new("E7 Shop Refresher").size(22.0).strong());
        ui.add_space(2.0);
        ui.colored_label(palette::TEXT_DIM, "Automated secret-shop refresh + buy.");

        if !has_window {
            ui.add_space(18.0);
            ui.colored_label(
                palette::TEXT_MUTED,
                "Open Epic Seven — its window is detected automatically.",
            );
            return;
        }

        ui.add_space(24.0);
        let block_w = 440.0_f32.min(ui.available_width() - 32.0);
        ui.allocate_ui_with_layout(
            egui::vec2(block_w, 0.0),
            egui::Layout::top_down(egui::Align::LEFT),
            |ui| {
                ui.label(
                    egui::RichText::new("QUICK START")
                        .size(11.0)
                        .color(palette::TEXT_DIM)
                        .strong(),
                );
                ui.add_space(10.0);
                let steps: &[&str] = &[
                    "Open the secret shop in Epic Seven.",
                    "Open the Setup tab — it captures the shop live and runs detection on its own.",
                    "Tick \"Show layout overlay\" and check the green (scan) / orange (click) zones line up with the shop UI.",
                    "Switch to the Run tab and hit Start — watch the first few rounds; the bot refreshes and buys on its own.",
                ];
                egui::Grid::new("onboarding_checklist")
                    .num_columns(2)
                    .min_col_width(0.0)
                    .spacing([10.0, 10.0])
                    .show(ui, |ui| {
                        for (i, text) in steps.iter().enumerate() {
                            ui.label(
                                egui::RichText::new(format!("{}.", i + 1))
                                    .size(13.0)
                                    .color(palette::ACCENT)
                                    .strong(),
                            );
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(*text)
                                        .size(13.0)
                                        .color(palette::TEXT_MUTED),
                                )
                                .wrap(),
                            );
                            ui.end_row();
                        }
                    });

                ui.add_space(18.0);
                ui.label(
                    egui::RichText::new("GOOD TO KNOW")
                        .size(11.0)
                        .color(palette::TEXT_DIM)
                        .strong(),
                );
                ui.add_space(10.0);
                let tips: &[&str] = &[
                    "Hover any input, checkbox, or button for a tooltip with details.",
                    "Ctrl+7 stops the bot from anywhere — even when Epic Seven has focus. If the first rounds look right, let it run.",
                ];
                egui::Grid::new("onboarding_tips")
                    .num_columns(2)
                    .min_col_width(0.0)
                    .spacing([10.0, 8.0])
                    .show(ui, |ui| {
                        for text in tips {
                            ui.label(
                                egui::RichText::new("•")
                                    .size(13.0)
                                    .color(palette::ACCENT)
                                    .strong(),
                            );
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(*text)
                                        .size(13.0)
                                        .color(palette::TEXT_MUTED),
                                )
                                .wrap(),
                            );
                            ui.end_row();
                        }
                    });
            },
        );
    });
}

/// "Are we missing something before the bot is runnable?" Only window
/// acquisition + first snapshot are still required — templates and
/// zones have bundled fallbacks now and never block.
pub(super) fn setup_incomplete(gui: &ShopGui) -> bool {
    gui.window_size.is_none() || gui.snapshot_size.is_none()
}

/// Drives the Setup-tab live preview. Capture + NCC run on a background
/// thread so the UI stays responsive even while WGC is grabbing a frame.
/// No-op while the bot runs (it owns the capture loop) or before the
/// game window has been acquired. Cadence is read live from
/// `matching.preview_refresh_ms` so the inline DragValue takes effect
/// without a restart.
fn maybe_auto_refresh_setup(gui: &mut ShopGui, ctx: &egui::Context) {
    drain_setup_preview(gui, ctx);

    let status = gui.stats.snapshot().status;
    if effective_status(gui.bot.as_ref(), status).is_active() {
        return;
    }
    if gui.capture.is_none() || gui.detector.is_none() {
        return;
    }
    let interval = Duration::from_millis(u64::from(gui.config.matching.preview_refresh_ms));
    if gui.setup_preview_in_flight {
        ctx.request_repaint_after(interval);
        return;
    }

    let now = Instant::now();
    let due = gui
        .last_setup_refresh
        .is_none_or(|t| now.duration_since(t) >= interval);
    if due {
        spawn_setup_preview(gui, ctx);
        gui.last_setup_refresh = Some(now);
    }
    ctx.request_repaint_after(interval);
}

fn drain_setup_preview(gui: &mut ShopGui, ctx: &egui::Context) {
    // Loop so a stale + a fresh result (e.g. two workers somehow queued)
    // collapse to "show the newest only" without growing context.
    while let Ok(result) = gui.setup_preview_rx.try_recv() {
        gui.setup_preview_in_flight = false;
        apply_setup_preview(gui, ctx, result);
    }
}

fn apply_setup_preview(
    gui: &mut ShopGui,
    ctx: &egui::Context,
    result: std::result::Result<SetupPreviewResult, String>,
) {
    match result {
        Err(e) => {
            gui.debug_error = Some(e);
        }
        Ok(frame) => {
            gui.debug_error = None;
            gui.snapshot_error = None;
            gui.debug_matches = frame.matches;
            upload_snapshot_texture(gui, ctx, &frame.rgba);
            gui.snapshot_size = Some([frame.rgba.width(), frame.rgba.height()]);
            gui.snapshot_rgba = Some(frame.rgba);
        }
    }
}

fn upload_snapshot_texture(gui: &mut ShopGui, ctx: &egui::Context, rgba: &RgbaImage) {
    let size = [rgba.width() as usize, rgba.height() as usize];
    let color_image = egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
    match gui.snapshot_texture.as_mut() {
        Some(handle) => handle.set(color_image, egui::TextureOptions::LINEAR),
        None => {
            gui.snapshot_texture =
                Some(ctx.load_texture("snapshot", color_image, egui::TextureOptions::LINEAR));
        }
    }
}

fn spawn_setup_preview(gui: &mut ShopGui, ctx: &egui::Context) {
    let Some(capture) = gui.capture.clone() else {
        return;
    };
    let Some(detector) = gui.detector.clone() else {
        return;
    };
    let tx = gui.setup_preview_tx.clone();
    let ctx = ctx.clone();
    let targets = gui.config.enabled_targets();
    let shop_grid = gui
        .config
        .regions
        .shop_grid
        .unwrap_or(crate::layout::SHOP_GRID);

    gui.setup_preview_in_flight = true;
    // Named so the thread shows up under a useful label in panic logs /
    // debuggers instead of `<unnamed>`. Spawn-on-tick at 2 Hz; the ~µs
    // spawn cost is dwarfed by the WGC capture inside.
    let builder = std::thread::Builder::new().name("setup-preview".into());
    let spawn_result = builder.spawn(move || {
        // catch_unwind so a panic inside the capture / NCC stack still
        // sends *something* down the channel — otherwise `in_flight` would
        // never clear and auto-refresh would silently die.
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_setup_preview(capture, detector, targets, shop_grid).map_err(|e| e.to_string())
        }))
        .unwrap_or_else(|payload| Err(panic_payload_message(payload)));
        if let Err(e) = &outcome {
            warn!(error = %e, "setup-preview worker failed");
        }
        let _ = tx.send(outcome);
        ctx.request_repaint();
    });
    if let Err(e) = spawn_result {
        // Spawn itself failed (rare — OOM, ulimit). Surface it and clear
        // the flag so the next tick can retry.
        warn!(error = %e, "failed to spawn setup-preview worker");
        gui.setup_preview_in_flight = false;
        gui.debug_error = Some(format!("worker spawn failed: {e}"));
    }
}

fn panic_payload_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        format!("worker panicked: {s}")
    } else if let Some(s) = payload.downcast_ref::<String>() {
        format!("worker panicked: {s}")
    } else {
        "worker panicked (unknown payload)".into()
    }
}

fn run_setup_preview(
    capture: Arc<crate::capture::WindowCapture>,
    detector: Arc<crate::detector::Detector>,
    targets: Vec<&'static str>,
    shop_grid: [f32; 4],
) -> crate::error::Result<SetupPreviewResult> {
    let scan = crate::shop::scan_shop_raw(&*capture, &detector, &targets, shop_grid)?;
    let colour_check = crate::color_check::ColorVerifier::new();

    let matches = scan
        .hits
        .into_iter()
        .map(|(alias, hit)| {
            let report = hit.as_ref().and_then(|h| {
                colour_check.evaluate(alias, &crate::shop::crop_icon_patch(&scan.rgba, h))
            });
            let (hit, colour_reject) = resolve_colour_verdict(hit, report);
            DebugMatch {
                alias,
                hit,
                tpl_size: detector.template_dimensions(alias),
                colour_reject,
            }
        })
        .collect();

    Ok(SetupPreviewResult {
        rgba: Arc::new(scan.rgba),
        matches,
    })
}

/// Splits an NCC hit and its colour-check report into the hit the bot
/// would act on and the rejection to surface. A colour-rejected hit is
/// dropped from `hit` so the overlay keeps drawing only acted-on matches,
/// and its report is returned separately so the Setup list can explain
/// the near-miss rather than fall silent.
fn resolve_colour_verdict(
    hit: Option<Hit>,
    report: Option<ColourReport>,
) -> (Option<Hit>, Option<ColourReport>) {
    match (hit, report) {
        (Some(_), Some(r)) if !r.passed => (None, Some(r)),
        (hit, _) => (hit, None),
    }
}

pub(super) fn draw_compact_stepper(ui: &mut egui::Ui, gui: &ShopGui) {
    let window_ok = gui.window_size.is_some();
    let snapshot_ok = gui.snapshot_size.is_some();

    ui.horizontal_wrapped(|ui| {
        stepper_chip(ui, window_ok, "Window");
        stepper_separator(ui);
        stepper_chip(ui, snapshot_ok, "Snapshot");
    });

    // Anchor the hairline to the sidebar tab-bar baseline so the two
    // lines read as one rule across panels. Falls back to a local
    // position the very first frame, before the tab bar has run.
    let shared_y = ui
        .ctx()
        .data(|d| d.get_temp::<f32>(crate::gui::panels::tab_baseline_id()));
    let clip = ui.clip_rect();
    let local_cursor_top = {
        ui.add_space(6.0);
        ui.cursor().top()
    };
    let y = shared_y.unwrap_or(local_cursor_top);
    ui.painter().hline(
        clip.left()..=clip.right(),
        y,
        egui::Stroke::new(1.0, palette::SECTION_STROKE),
    );
    // Push the cursor below the painted line so following content
    // (the snapshot image) doesn't overlap it.
    let cur = ui.cursor().top();
    if cur < y + 1.0 {
        ui.add_space(y + 1.0 - cur);
    }
}

fn stepper_chip(ui: &mut egui::Ui, done: bool, text: &str) {
    let (glyph, color) = if done {
        (icon::CHECK_CIRCLE, palette::OK)
    } else {
        (icon::CIRCLE, palette::TEXT_MUTED)
    };
    ui.colored_label(color, glyph);
    ui.colored_label(
        if done {
            palette::TEXT_DIM
        } else {
            palette::TEXT_MUTED
        },
        text,
    );
}

fn stepper_separator(ui: &mut egui::Ui) {
    ui.add_space(4.0);
    ui.colored_label(palette::TEXT_MUTED, "·");
    ui.add_space(4.0);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit() -> Hit {
        Hit {
            x: 10,
            y: 20,
            score: 0.9,
            scale: 1.0,
            margin: 0.2,
        }
    }

    fn report(passed: bool) -> ColourReport {
        ColourReport {
            passed,
            distance: 0.42,
            coloured_fraction: 0.18,
        }
    }

    #[test]
    fn passing_colour_keeps_the_hit_and_reports_no_rejection() {
        let (kept, reject) = resolve_colour_verdict(Some(hit()), Some(report(true)));
        assert!(kept.is_some());
        assert!(reject.is_none());
    }

    #[test]
    fn failing_colour_drops_the_hit_and_surfaces_the_report() {
        let (kept, reject) = resolve_colour_verdict(Some(hit()), Some(report(false)));
        assert!(
            kept.is_none(),
            "overlay must not draw a colour-rejected hit"
        );
        let reject = reject.expect("the rejection report must be surfaced");
        assert!(!reject.passed);
        assert_eq!(reject.distance, 0.42);
    }

    #[test]
    fn no_ncc_hit_yields_no_hit_and_no_rejection() {
        let (kept, reject) = resolve_colour_verdict(None, None);
        assert!(kept.is_none());
        assert!(reject.is_none());
    }

    #[test]
    fn hit_without_a_report_passes_through_unrejected() {
        // Unknown alias → ColorVerifier::evaluate returns None; the hit
        // must survive (the colour filter is a no-op there).
        let (kept, reject) = resolve_colour_verdict(Some(hit()), None);
        assert!(kept.is_some());
        assert!(reject.is_none());
    }
}
