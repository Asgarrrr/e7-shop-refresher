use egui::{Color32, Pos2, Rect, Sense, Stroke, StrokeKind, Vec2};
use tracing::info;

use crate::config::Config;
use crate::gui::app::{CropRect, ROI_LIST, ShopGui, TEMPLATE_ALIASES, ZONE_LIST, palette};

pub(super) fn draw_snapshot(ui: &mut egui::Ui, gui: &mut ShopGui) {
    let Some(texture) = gui.snapshot_texture.as_ref() else {
        draw_onboarding(ui, gui);
        return;
    };

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

    // Drag is either a crop selection (default) or a zone capture when
    // `zone_drag_target` is set — same drag handler for both.
    if response.drag_started()
        && let Some(pos) = response.interact_pointer_pos()
    {
        gui.crop_drag_start = Some(pos);
        gui.crop_selection = None;
        gui.crop_save_notice = None;
        gui.crop_save_error = None;
    }
    if response.dragged()
        && let (Some(start), Some(now)) = (gui.crop_drag_start, response.interact_pointer_pos())
    {
        gui.crop_selection = Some(rect_from_screen_drag(start, now, image_rect, snap_size));
    }
    if response.drag_stopped() {
        if let (Some(start), Some(now)) = (gui.crop_drag_start, response.interact_pointer_pos()) {
            gui.crop_selection = Some(rect_from_screen_drag(start, now, image_rect, snap_size));
        }
        gui.crop_drag_start = None;

        // In zone-draw mode, commit the rect to the zone and exit. Clear
        // the crop selection so the zone renders as a normal overlay,
        // not as a pending-crop highlight.
        if let Some(zone_name) = gui.zone_drag_target.take()
            && let Some(sel) = gui.crop_selection.take()
            && sel.w > 0
            && sel.h > 0
        {
            let rect = pixel_to_ratio_rect(sel, snap_size);
            if let Some(slot) = gui.zone_mut(zone_name) {
                *slot = Some(rect);
                gui.refresh_zone_status();
                info!(zone = zone_name, "zone updated from drag");
            }
        }
    }

    let painter = ui.painter_at(image_rect);

    // ROI overlays
    for (name, color) in ROI_LIST {
        if !gui.show_rois.get(*name).copied().unwrap_or(true) {
            continue;
        }
        let Some(roi) = roi_for(&gui.config, name) else {
            continue;
        };
        let rect = ratio_rect(image_rect, roi);
        painter.rect_stroke(rect, 0.0, Stroke::new(2.0, *color), StrokeKind::Inside);
        painter.text(
            rect.left_top() + Vec2::new(4.0, 2.0),
            egui::Align2::LEFT_TOP,
            *name,
            egui::FontId::proportional(12.0),
            *color,
        );
    }

    // Zones are filled + outlined to distinguish them from the
    // stroke-only ROI rectangles above.
    for (name, color) in ZONE_LIST {
        if !gui.show_zones.get(*name).copied().unwrap_or(true) {
            continue;
        }
        let Some(zone) = zone_for(&gui.config, name) else {
            continue;
        };
        let rect = ratio_rect(image_rect, zone);
        let fill = Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 40);
        painter.rect_filled(rect, 0.0, fill);
        painter.rect_stroke(rect, 0.0, Stroke::new(2.0, *color), StrokeKind::Inside);
        painter.text(
            rect.left_bottom() + Vec2::new(4.0, -14.0),
            egui::Align2::LEFT_TOP,
            *name,
            egui::FontId::proportional(12.0),
            *color,
        );
    }

    // Debug overlays drawn last so the ROI/Zone overlays don't obscure them.
    let snap_w = snap_size[0] as f32;
    let snap_h = snap_size[1] as f32;
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

        // Same helper as the runner so the overlay matches the actual click.
        if let Some(column) = gui.config.zones.buy_column {
            let [bx_px, by_px, bw_px, bh_px] = crate::shop::buy_column_row_rect_for(
                column,
                hit.y,
                snap_size[1],
                snap_size[0],
                snap_size[1],
                gui.config.shop.buy_button_y_offset_ratio,
            );
            let rect = ratio_rect(
                image_rect,
                [
                    bx_px as f32 / snap_w,
                    by_px as f32 / snap_h,
                    bw_px as f32 / snap_w,
                    bh_px as f32 / snap_h,
                ],
            );
            let fill = Color32::from_rgba_unmultiplied(255, 100, 100, 70);
            painter.rect_filled(rect, 0.0, fill);
            painter.rect_stroke(
                rect,
                0.0,
                Stroke::new(1.5, palette::DEBUG_BAND_STROKE),
                StrokeKind::Inside,
            );
        }
    }

    // Current drag selection, tinted with the target zone's color in
    // zone-draw mode.
    if let Some(sel) = gui.crop_selection {
        let rect = crop_to_screen_rect(sel, image_rect, snap_size);
        let color = gui
            .zone_drag_target
            .and_then(|name| ZONE_LIST.iter().find(|(n, _)| *n == name).map(|(_, c)| *c))
            .unwrap_or(Color32::WHITE);
        painter.rect_filled(
            rect,
            0.0,
            Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 30),
        );
        painter.rect_stroke(rect, 0.0, Stroke::new(2.0, color), StrokeKind::Inside);
    }
}

fn rect_from_screen_drag(
    start: Pos2,
    end: Pos2,
    image_rect: Rect,
    snap_size: [u32; 2],
) -> CropRect {
    let (sx, sy) = screen_to_pixel(start, image_rect, snap_size);
    let (ex, ey) = screen_to_pixel(end, image_rect, snap_size);
    let x = sx.min(ex);
    let y = sy.min(ey);
    let w = sx.max(ex).saturating_sub(x);
    let h = sy.max(ey).saturating_sub(y);
    CropRect { x, y, w, h }
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

fn crop_to_screen_rect(sel: CropRect, image_rect: Rect, snap_size: [u32; 2]) -> Rect {
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

fn roi_for(cfg: &Config, name: &str) -> Option<[f32; 4]> {
    match name {
        "shop_grid" => cfg.regions.shop_grid,
        "anchor_shop" => cfg.regions.anchor_shop,
        _ => None,
    }
}

fn zone_for(cfg: &Config, name: &str) -> Option<[f32; 4]> {
    match name {
        "refresh" => cfg.zones.refresh,
        "refresh_confirm" => cfg.zones.refresh_confirm,
        "buy_confirm" => cfg.zones.buy_confirm,
        "buy_column" => cfg.zones.buy_column,
        _ => None,
    }
}

/// Empty-state shown in the central panel before any snapshot has been
/// captured. Doubles as the first-run onboarding: with shipped defaults,
/// the only thing left for a new user to do is crop the 3 icons.
fn draw_onboarding(ui: &mut egui::Ui, gui: &ShopGui) {
    // Vertical centering: pad the top with half the leftover space so
    // the block sits roughly mid-screen on tall windows.
    let estimated_h = 280.0;
    let pad_top = ((ui.available_height() - estimated_h) * 0.5).max(20.0);
    ui.add_space(pad_top);

    ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui| {
        ui.label(egui::RichText::new("E7 Shop Refresher").size(22.0).strong());
        ui.add_space(2.0);
        ui.colored_label(palette::TEXT_DIM, "Automated secret-shop refresh + buy.");

        ui.add_space(22.0);

        // Constrain the step column to ~440 px so each row stays on a
        // comfortable reading width regardless of how wide the central
        // panel gets.
        ui.scope(|ui| {
            ui.set_max_width(440.0);
            ui.vertical(|ui| draw_onboarding_steps(ui, gui));
        });
    });
}

fn draw_onboarding_steps(ui: &mut egui::Ui, gui: &ShopGui) {
    let window_ok = gui.window_size.is_some();
    let window_label = match gui.window_size {
        Some((w, h)) => format!("Game window detected ({w}×{h})"),
        None => "Open Epic Seven (window not detected yet)".to_string(),
    };
    onboarding_step(ui, window_ok, &window_label);

    onboarding_step(ui, false, "Click Refresh in the Snapshot panel");

    let total = TEMPLATE_ALIASES.len();
    let missing_n = gui.template_status.len();
    let templates_done = missing_n == 0;
    let tpl_label = if templates_done {
        format!("All {total} templates cropped")
    } else {
        format!("Crop {missing_n} of {total} icons from your snapshot:")
    };
    onboarding_step(ui, templates_done, &tpl_label);

    if !templates_done {
        for tpl in &gui.template_status {
            ui.horizontal(|ui| {
                ui.add_space(28.0);
                ui.colored_label(palette::TEXT_DIM, format!("• {}", tpl.name));
            });
        }
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.add_space(28.0);
            ui.colored_label(
                palette::TEXT_MUTED,
                "Drag a rect on the snapshot, pick the target in Setup → Crop & Save.",
            );
        });
    }

    ui.add_space(2.0);
    onboarding_step(ui, false, "Switch to Run, click Start");
}

fn onboarding_step(ui: &mut egui::Ui, done: bool, text: &str) {
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        let (icon, icon_color) = if done {
            ("✓", palette::OK)
        } else {
            ("○", palette::TEXT_MUTED)
        };
        ui.colored_label(icon_color, icon);
        ui.add_space(4.0);
        if done {
            ui.colored_label(palette::TEXT_DIM, text);
        } else {
            ui.label(text);
        }
    });
}

fn pixel_to_ratio_rect(sel: CropRect, snap_size: [u32; 2]) -> [f32; 4] {
    let sw = snap_size[0].max(1) as f32;
    let sh = snap_size[1].max(1) as f32;
    let x = sel.x as f32 / sw;
    let y = sel.y as f32 / sh;
    let w = sel.w as f32 / sw;
    let h = sel.h as f32 / sh;
    [
        x.clamp(0.0, 1.0),
        y.clamp(0.0, 1.0),
        w.clamp(0.001, 1.0 - x),
        h.clamp(0.001, 1.0 - y),
    ]
}
