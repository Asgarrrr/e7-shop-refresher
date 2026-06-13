use egui::Context;
use egui_phosphor::regular as icon;

use crate::gui::app::{ShopGui, palette};
use crate::gui::bot::effective_status;

use super::timing::draw_timing_section;
use super::{section_card, section_header};

pub(in crate::gui) fn draw_setup_tab(ui: &mut egui::Ui, gui: &mut ShopGui, ctx: &Context) {
    let bot_active = effective_status(gui.bot.as_ref(), gui.stats.snapshot().status).is_active();

    section_card(ui, "Detection", |ui| {
        draw_detection_section(ui, gui, ctx, bot_active)
    });

    ui.add_enabled_ui(!bot_active, |ui| {
        section_card(ui, "Layout", |ui| draw_layout_card(ui, gui));
    });

    ui.add_enabled_ui(!bot_active, |ui| {
        section_card(ui, "Timing", |ui| draw_timing_section(ui, gui));
    });

    if bot_active {
        ui.add_space(2.0);
        ui.colored_label(
            palette::TEXT_MUTED,
            "Bot is running — stop it to edit timing or layout overrides.",
        );
    }
}

const LAYOUT_REGIONS: &[(&str, &str)] = &[("shop_grid", "Item grid")];

const LAYOUT_ZONES: &[(&str, &str)] = &[
    ("refresh", "Refresh button"),
    ("refresh_confirm", "Refresh confirm modal"),
    ("buy_confirm", "Buy confirm modal"),
    ("buy_column", "Buy column (X strip)"),
];

const LAYOUT_TEMPLATES: &[(&str, &str)] = &[
    ("mystic_medal", "Mystic Medal icon"),
    ("covenant", "Covenant Bookmark icon"),
];

// Matches `TIMING_LABEL_W` so labels line up across cards.
const LAYOUT_LABEL_W: f32 = 140.0;

// Pinned width so the prefix text on DragValues doesn't make widgets
// grow unpredictably and break `add_space` alignment in `buy_click_row`.
const LAYOUT_RECT_INPUT_W: f32 = 58.0;

// Hover for click-zone rows. A zone smaller than its button just tightens
// the random-click spread; one that overflows can land off-target.
const ZONE_FIT_HINT: &str = "Keep the box inside the real button — smaller is fine, \
     but if it spills past the edge a click can miss and the round fails.";

fn draw_layout_card(ui: &mut egui::Ui, gui: &mut ShopGui) {
    ui.checkbox(
        &mut gui.show_layout_overlay,
        "Show layout overlay on snapshot",
    )
    .on_hover_text(
        "Draws every region the bot will look in (green) and every \
         pixel it will click (orange) over the central snapshot. \
         Useful for spotting layout drift on unusual resolutions.",
    );
    ui.add_space(6.0);

    section_header(ui, "Search regions");
    egui::Grid::new("layout_regions")
        .num_columns(2)
        .min_col_width(LAYOUT_LABEL_W)
        .spacing([12.0, 6.0])
        .show(ui, |ui| {
            for (name, label) in LAYOUT_REGIONS {
                let bundled = bundled_rect(name);
                if let Some(slot) = gui.region_mut(name) {
                    rect_row(ui, label, name, bundled, slot, "");
                }
                ui.end_row();
            }
        });
    ui.add_space(10.0);

    section_header(ui, "Click zones");
    egui::Grid::new("layout_zones")
        .num_columns(2)
        .min_col_width(LAYOUT_LABEL_W)
        .spacing([12.0, 6.0])
        .show(ui, |ui| {
            for (name, label) in LAYOUT_ZONES {
                let bundled = bundled_rect(name);
                if let Some(slot) = gui.zone_mut(name) {
                    rect_row(ui, label, name, bundled, slot, ZONE_FIT_HINT);
                }
                ui.end_row();
            }
            buy_reference_row(ui, gui);
            ui.end_row();
            buy_click_row(ui, gui);
            ui.end_row();
        });
    ui.add_space(10.0);

    section_header(ui, "Reference templates");
    egui::Grid::new("layout_templates")
        .num_columns(2)
        .min_col_width(LAYOUT_LABEL_W)
        .spacing([12.0, 6.0])
        .show(ui, |ui| {
            for (name, label) in LAYOUT_TEMPLATES {
                template_row(ui, gui, label, name);
                ui.end_row();
            }
        });

    if let Some(alias) = gui.override_drag {
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.colored_label(palette::DEBUG_LABEL, icon::HAND_POINTING);
            ui.colored_label(
                palette::DEBUG_LABEL,
                format!("Drag a rectangle on the snapshot to set `{alias}`."),
            );
        });
    }
}

// Must match the runner's fallback when the override is `None` — the
// inputs read what the bot will actually use.
fn bundled_rect(name: &str) -> [f32; 4] {
    use crate::layout::{
        BUY_CONFIRM, REFRESH, REFRESH_CONFIRM, SHOP_GRID, buy_column_overlay_rect,
    };
    match name {
        "shop_grid" => SHOP_GRID,
        "refresh" => REFRESH,
        "refresh_confirm" => REFRESH_CONFIRM,
        "buy_confirm" => BUY_CONFIRM,
        "buy_column" => buy_column_overlay_rect(),
        _ => [0.0; 4],
    }
}

fn rect_row(
    ui: &mut egui::Ui,
    label: &str,
    name: &'static str,
    bundled: [f32; 4],
    slot: &mut Option<[f32; 4]>,
    hover: &str,
) {
    let lbl = ui.label(label);
    if !hover.is_empty() {
        lbl.on_hover_text(hover);
    }

    // Promote to `Some(...)` only on actual edit — keeps the bundled
    // default in play until the user touches a field.
    let mut current = slot.unwrap_or(bundled);
    let mut changed = false;
    ui.horizontal(|ui| {
        let h = ui.spacing().interact_size.y;
        for (axis, v) in ["x ", "y ", "w ", "h "].iter().zip(current.iter_mut()) {
            let r = ui.add_sized(
                [LAYOUT_RECT_INPUT_W, h],
                egui::DragValue::new(v)
                    .speed(0.001)
                    .range(0.0..=1.0)
                    .max_decimals(3)
                    .min_decimals(3)
                    .prefix(*axis),
            );
            crate::gui::app::register_edit_focus(&r, crate::gui::app::EditFocus::Rect(name));
            if r.changed() {
                changed = true;
            }
        }
    });
    if changed {
        *slot = Some(current);
    }
}

// Mirror of `buy_click_row` for the reference line itself — sharing the
// y column makes "line y, then offset y" read top-to-bottom on the same
// axis. Mouse drag on the snapshot stays as the primary affordance; this
// is the typed-input escape hatch.
fn buy_reference_row(ui: &mut egui::Ui, gui: &mut ShopGui) {
    ui.label("Buy reference");
    ui.horizontal(|ui| {
        let h = ui.spacing().interact_size.y;
        let skip = LAYOUT_RECT_INPUT_W + ui.spacing().item_spacing.x;

        ui.add_space(skip); // x slot
        let y_resp = ui
            .add_sized(
                [LAYOUT_RECT_INPUT_W, h],
                egui::DragValue::new(&mut gui.config.shop.buy_calibration_line_y_ratio)
                    .speed(0.001)
                    .range(0.0..=1.0)
                    .max_decimals(3)
                    .min_decimals(3)
                    .prefix("y "),
            )
            .on_hover_text(
                "Align the red line with the centre of any shop item. \
                 Rough alignment is enough — the bot detects items live; \
                 the line is just your visual anchor for the box below.",
            );
        crate::gui::app::register_edit_focus(&y_resp, crate::gui::app::EditFocus::BuyClick);
    });
}

// Inputs land under the `y` / `h` columns of the rect rows above so the
// row reads as "extra y/h knobs for buy_column". Each skip folds in
// `item_spacing.x` because `add_space` doesn't emit it.
fn buy_click_row(ui: &mut egui::Ui, gui: &mut ShopGui) {
    ui.label("Buy click");
    ui.horizontal(|ui| {
        let h = ui.spacing().interact_size.y;
        let skip = LAYOUT_RECT_INPUT_W + ui.spacing().item_spacing.x;

        ui.add_space(skip); // x slot
        let y_resp = ui
            .add_sized(
                [LAYOUT_RECT_INPUT_W, h],
                egui::DragValue::new(&mut gui.config.shop.buy_button_y_offset_ratio)
                    .speed(0.001)
                    .range(0.0..=0.15)
                    .max_decimals(3)
                    .min_decimals(3)
                    .prefix("y "),
            )
            .on_hover_text(
                "Sit the red box on that row's Buy button. Distance \
                 from the reference line down to the click.",
            );
        crate::gui::app::register_edit_focus(&y_resp, crate::gui::app::EditFocus::BuyClick);

        ui.add_space(skip); // w slot
        let h_resp = ui
            .add_sized(
                [LAYOUT_RECT_INPUT_W, h],
                egui::DragValue::new(&mut gui.config.shop.buy_button_band_h_ratio)
                    .speed(0.001)
                    .range(0.005..=0.20)
                    .max_decimals(3)
                    .min_decimals(3)
                    .prefix("h "),
            )
            .on_hover_text(
                "Box height — keep it within the Buy button face. Each \
                 click picks a random Y inside the box.",
            );
        crate::gui::app::register_edit_focus(&h_resp, crate::gui::app::EditFocus::BuyClick);
    });
}

fn template_row(ui: &mut egui::Ui, gui: &mut ShopGui, label: &str, name: &'static str) {
    let is_arming = gui.override_drag == Some(name);

    ui.label(label);

    let (edit_label, edit_hover) = if is_arming {
        ("Cancel", "Cancel the armed override.")
    } else {
        (
            "Edit",
            "Arm an override. Drag a rectangle on the snapshot to \
             commit the new value.",
        )
    };
    if ui
        .add(egui::Button::new(edit_label).small())
        .on_hover_text(edit_hover)
        .clicked()
    {
        gui.clear_override_drag();
        if !is_arming {
            gui.override_drag = Some(name);
        }
    }
}

fn draw_detection_section(ui: &mut egui::Ui, gui: &mut ShopGui, _ctx: &Context, bot_active: bool) {
    draw_detection_status(ui, gui, bot_active);

    if let Some(err) = &gui.snapshot_error {
        ui.colored_label(palette::ERROR, err);
    }
    if let Some(err) = &gui.debug_error {
        ui.colored_label(palette::ERROR, format!("detection: {err}"));
    }

    ui.add_space(6.0);
    ui.add_enabled_ui(!bot_active, |ui| {
        ui.horizontal(|ui| {
            ui.label("NCC threshold");
            ui.add(
                egui::DragValue::new(&mut gui.config.matching.threshold)
                    .speed(0.005)
                    .range(0.50..=0.999)
                    .max_decimals(3),
            )
            .on_hover_text(
                "Raise to drop false matches (icons that look vaguely similar). \
                 Lower if real items are missed.",
            );
        });
    });

    ui.add_space(6.0);
    draw_detection_matches(ui, gui);
}

fn draw_detection_status(ui: &mut egui::Ui, gui: &mut ShopGui, bot_active: bool) {
    let dot = |ui: &mut egui::Ui| {
        ui.colored_label(palette::TEXT_MUTED, "·");
    };
    ui.horizontal(|ui| {
        match gui.snapshot_size {
            Some([w, h]) => {
                ui.colored_label(palette::TEXT_DIM, format!("{w}×{h}"));
            }
            None => {
                ui.colored_label(palette::TEXT_MUTED, "waiting for first frame…");
                return;
            }
        }
        dot(ui);
        if bot_active {
            ui.colored_label(palette::TEXT_MUTED, "paused while bot runs");
        } else if gui.capture.is_none() {
            ui.colored_label(palette::TEXT_MUTED, "waiting for Epic Seven");
        } else {
            ui.colored_label(palette::TEXT_MUTED, "live · every");
            ui.add(
                egui::DragValue::new(&mut gui.config.matching.preview_refresh_ms)
                    .range(100..=5000)
                    .speed(10.0)
                    .suffix(" ms"),
            )
            .on_hover_text(
                "How often the Setup-tab preview captures + re-runs detection. \
                 Lower = more responsive zone drag, higher CPU. Bot loop is \
                 unaffected.",
            );
        }
    });
}

fn draw_detection_matches(ui: &mut egui::Ui, gui: &ShopGui) {
    // Pure "not on screen" rows stay silent — the overlay already shows
    // nothing. But a colour-rejected near-miss IS surfaced: it's the one
    // case where calibration looks like "nothing found" yet NCC actually
    // matched, so the user needs the distance/colour numbers to tell a
    // bad template apart from a colour-margin problem.
    let mut printed = 0;
    for m in &gui.debug_matches {
        if let Some(hit) = m.hit.as_ref() {
            ui.horizontal(|ui| {
                ui.colored_label(palette::OK, m.alias);
                ui.colored_label(
                    palette::TEXT_MUTED,
                    format!("score {:.3} · margin {:.3}", hit.score, hit.margin),
                );
            });
            printed += 1;
        } else if let Some(rej) = m.colour_reject.as_ref() {
            ui.horizontal(|ui| {
                ui.colored_label(palette::WARN, m.alias);
                ui.colored_label(
                    palette::TEXT_MUTED,
                    format!(
                        "colour reject · dist {:.3} · colour {:.0}%",
                        rej.distance,
                        rej.coloured_fraction * 100.0
                    ),
                );
            });
            printed += 1;
        }
    }
    if printed == 0 && !gui.debug_matches.is_empty() {
        ui.colored_label(
            palette::TEXT_MUTED,
            egui::RichText::new("no items detected in current frame").italics(),
        );
    }
}
