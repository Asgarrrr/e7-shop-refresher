use egui::{Color32, Context, Sense, Vec2};
use tracing::error;

use crate::config::Config;
use crate::gui::app::{ROI_LIST, SECTION_GAP, ShopGui, TEMPLATE_ALIASES, Tab, ZONE_LIST, palette};
use crate::gui::bot::effective_status;
use crate::gui::logs::LogBuffer;
use crate::gui::persist::AutoSavedFields;
use crate::gui::state::BotStatus;

fn draw_window_status(ui: &mut egui::Ui, gui: &mut ShopGui) {
    // Snapshot the fields the closure reads before it takes &mut gui to
    // call methods.
    let window_error = gui.window_error.clone();
    let window_size = gui.window_size;
    let window_title = gui.window_title.clone();
    if let Some(e) = window_error {
        ui.horizontal(|ui| {
            ui.colored_label(palette::ERROR, format!("✗ {e}"));
            if ui.button("Retry").clicked() {
                gui.refresh_template_status();
                gui.try_acquire_window();
            }
        });
    } else if let Some((w, h)) = window_size {
        ui.colored_label(
            Color32::from_rgb(110, 200, 110),
            format!(
                "{} ({}×{})",
                window_title.as_deref().unwrap_or("game"),
                w,
                h
            ),
        );
    }
}

/// Quieter alternative to `ui.heading()` (which renders at ~20 px and
/// dominates the sidebar).
pub(super) fn section_header(ui: &mut egui::Ui, text: &str) {
    ui.label(
        egui::RichText::new(text)
            .strong()
            .size(14.0)
            .color(palette::SECTION_HEADER),
    );
    ui.add_space(2.0);
}

pub(super) fn section_separator(ui: &mut egui::Ui) {
    ui.add_space(SECTION_GAP);
    ui.separator();
    ui.add_space(4.0);
}

/// Setup-tab section: a larger, accent-tinted header followed by a
/// thin hairline rule, then content. No frame chrome — hierarchy comes
/// from typography and whitespace so the panel stays airy on a 310 px
/// sidebar.
pub(super) fn section_card(
    ui: &mut egui::Ui,
    title: &str,
    add_contents: impl FnOnce(&mut egui::Ui),
) {
    ui.add_space(2.0);
    ui.label(
        egui::RichText::new(title)
            .strong()
            .size(15.0)
            .color(palette::SECTION_HEADER),
    );
    ui.add_space(3.0);
    section_hairline(ui);
    ui.add_space(6.0);
    add_contents(ui);
    ui.add_space(14.0);
}

/// 1 px rule rendered at panel width — a quieter alternative to
/// `ui.separator()` (which draws a thicker, lighter line). Used to
/// anchor section headers without adding a full card frame.
fn section_hairline(ui: &mut egui::Ui) {
    let rect = ui.available_rect_before_wrap();
    let y = rect.top();
    ui.painter().hline(
        rect.left()..=rect.right(),
        y,
        egui::Stroke::new(1.0, palette::SECTION_STROKE),
    );
    ui.add_space(1.0);
}

/// Primary action — `Save crop`, `Refresh snapshot`. Accent fill +
/// bolder text so the main verb stands out from the secondary buttons.
fn primary_button(label: &str) -> egui::Button<'_> {
    egui::Button::new(
        egui::RichText::new(label)
            .strong()
            .color(palette::ACCENT_TEXT),
    )
    .fill(palette::ACCENT)
}

pub(super) fn draw_tab_bar(ui: &mut egui::Ui, gui: &mut ShopGui) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        for (tab, label) in [(Tab::Run, "Run"), (Tab::Setup, "Setup")] {
            let selected = gui.active_tab == tab;
            let mut text = egui::RichText::new(label).size(15.0);
            if selected {
                text = text.strong();
            } else {
                text = text.color(palette::TEXT_DIM);
            }
            if ui.selectable_label(selected, text).clicked() {
                gui.active_tab = tab;
            }
        }
    });
    ui.separator();
}

/// Config widgets are disabled while the bot runs because the worker
/// thread captured `Config` by value at spawn — mid-run edits wouldn't
/// take effect anyway.
pub(super) fn draw_run_tab(ui: &mut egui::Ui, gui: &mut ShopGui, _ctx: &Context) {
    let stats = gui.stats.snapshot();
    let effective = effective_status(gui.bot.as_ref(), stats.status.clone());
    let bot_active = effective.is_active();
    let can_start = gui.capture.is_some()
        && gui.detector.is_some()
        && gui.template_status.is_empty()
        && gui.zone_status.is_empty()
        && !bot_active;

    draw_window_status(ui, gui);
    ui.horizontal(|ui| {
        // Start is the primary action — larger + bolder than Stop.
        let start_btn = egui::Button::new(egui::RichText::new("Start").size(14.0).strong());
        if ui.add_enabled(can_start, start_btn).clicked()
            && let Err(e) = gui.start_bot()
        {
            error!(error = %e, "start failed");
        }
        if ui
            .add_enabled(bot_active, egui::Button::new("Stop"))
            .clicked()
        {
            gui.stop_bot();
        }
        ui.colored_label(
            match effective {
                BotStatus::Running => palette::OK,
                BotStatus::Stopping => palette::WARN,
                BotStatus::Failed => palette::ERROR,
                _ => palette::TEXT_MUTED,
            },
            effective.label(),
        );
    });

    if !can_start && !bot_active {
        let reason = if gui.capture.is_none() {
            "game window not found"
        } else if !gui.template_status.is_empty() {
            "templates missing — see Setup tab"
        } else if !gui.zone_status.is_empty() {
            "zones not drawn — see Setup tab"
        } else if gui.detector.is_none() {
            "detector not ready"
        } else {
            ""
        };
        if !reason.is_empty() {
            ui.colored_label(palette::TEXT_MUTED, reason);
        }
    }

    if bot_active || stats.round > 0 {
        let round_line = if stats.total_rounds > 0 {
            format!(
                "Round {} / {}   ·   {} mystic   ·   {} covenant",
                stats.round, stats.total_rounds, stats.mystic_bought, stats.covenant_bought
            )
        } else {
            format!(
                "Round {}   ·   {} mystic   ·   {} covenant",
                stats.round, stats.mystic_bought, stats.covenant_bought
            )
        };
        ui.label(round_line);
    }
    if let Some(err) = &stats.last_error {
        ui.colored_label(palette::ERROR, format!("Error: {err}"));
    }

    section_separator(ui);

    ui.add_enabled_ui(!bot_active, |ui| {
        section_header(ui, "Targets");
        ui.checkbox(&mut gui.config.shop.buy_mystic_medals, "Buy mystic medals");
        ui.checkbox(&mut gui.config.shop.buy_covenant, "Buy covenant bookmarks");

        section_separator(ui);

        section_header(ui, "Stop when…");
        ui.label(
            "Any limit set to 0 is disabled. The run halts at whichever \
             is reached first. All zeros = no auto-stop (manual Stop only).",
        );
        ui.add_space(4.0);

        // Grid aligns every DragValue at the same X regardless of label width.
        egui::Grid::new("stop_when_grid")
            .num_columns(2)
            .spacing([10.0, 4.0])
            .show(ui, |ui| {
                stop_condition_row(
                    ui,
                    "refreshes done",
                    &mut gui.config.shop.max_refreshes,
                    1.0,
                    0..=10_000,
                    "Total refresh rounds before halting.",
                );
                stop_condition_row(
                    ui,
                    "minutes elapsed",
                    &mut gui.config.shop.stop_after_minutes,
                    0.5,
                    0..=1440,
                    "Wall-clock duration limit. Checked at every round boundary.",
                );
                stop_condition_row(
                    ui,
                    "mystic medals bought",
                    &mut gui.config.shop.stop_when_mystic_medals,
                    0.5,
                    0..=10_000,
                    "Halt after this many mystic medals have been bought this run.",
                );
                stop_condition_row(
                    ui,
                    "covenants bought",
                    &mut gui.config.shop.stop_when_covenants,
                    0.5,
                    0..=10_000,
                    "Halt after this many covenant bookmarks have been bought this run.",
                );
            });

        ui.add_space(4.0);
        ui.checkbox(
            &mut gui.config.shop.sleep_when_done,
            "Sleep PC when goal reached",
        )
        .on_hover_text(
            "Suspends the system to sleep once a stop condition fires. \
             Never triggers on manual Stop.",
        );
    });

    if bot_active {
        ui.add_space(4.0);
        ui.colored_label(
            palette::TEXT_MUTED,
            "Bot is running — stop it to edit these.",
        );
    }
}

fn stop_condition_row(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut u32,
    speed: f32,
    range: std::ops::RangeInclusive<u32>,
    hover: &str,
) {
    ui.label(label);
    ui.add(egui::DragValue::new(value).speed(speed).range(range))
        .on_hover_text(hover);
    ui.end_row();
}

pub(super) fn draw_setup_tab(ui: &mut egui::Ui, gui: &mut ShopGui, ctx: &Context) {
    let bot_active = effective_status(gui.bot.as_ref(), gui.stats.snapshot().status).is_active();

    // Workflow order: capture → verify templates → crop → calibrate
    // overlays → advanced tuning. Snapshot sits first because it's the
    // most-used button during tuning sessions.
    section_card(ui, "Snapshot", |ui| {
        draw_snapshot_section(ui, gui, ctx, bot_active)
    });

    section_card(ui, &templates_card_title(gui), |ui| draw_templates(ui, gui));

    ui.add_enabled_ui(!bot_active, |ui| {
        section_card(ui, "Crop & Save", |ui| draw_crop_panel(ui, gui));
    });

    ui.add_enabled_ui(!bot_active, |ui| {
        section_card(ui, "Regions", |ui| draw_regions_editor(ui, gui));
    });

    ui.add_enabled_ui(!bot_active, |ui| {
        section_card(ui, "Zones", |ui| draw_zones_editor(ui, gui));
    });

    ui.add_enabled_ui(!bot_active, |ui| {
        section_card(ui, "Detection", |ui| draw_detection_settings(ui, gui));
    });

    if bot_active {
        ui.add_space(2.0);
        ui.colored_label(
            palette::TEXT_MUTED,
            "Bot is running — stop it to edit calibration.",
        );
    }
}

fn templates_card_title(gui: &ShopGui) -> String {
    if gui.template_status.is_empty() {
        "Templates  ·  ready".to_string()
    } else {
        format!("Templates  ·  {} missing", gui.template_status.len())
    }
}

fn draw_snapshot_section(ui: &mut egui::Ui, gui: &mut ShopGui, ctx: &Context, bot_active: bool) {
    let refresh_enabled = !bot_active;
    ui.horizontal(|ui| {
        if ui
            .add_enabled(refresh_enabled, primary_button("Refresh"))
            .on_hover_text("Capture the current game window into the central preview.")
            .clicked()
        {
            gui.refresh_snapshot(ctx);
        }
        if ui
            .add_enabled(refresh_enabled, egui::Button::new("Run detection"))
            .on_hover_text(
                "Snapshot + run NCC for each item template. Draws the match \
                 bounding box + the buy_column click band so you can see \
                 exactly what the bot would do.",
            )
            .clicked()
        {
            gui.run_debug_detection(ctx);
        }
        if let Some([w, h]) = gui.snapshot_size {
            ui.colored_label(palette::TEXT_DIM, format!("{w}×{h}"));
        }
        if bot_active {
            ui.colored_label(palette::TEXT_MUTED, "(disabled while bot runs)");
        }
    });
    if let Some(err) = &gui.snapshot_error {
        ui.colored_label(palette::ERROR, err);
    }
    if let Some(err) = &gui.debug_error {
        ui.colored_label(palette::ERROR, format!("detection: {err}"));
    }

    if !gui.debug_matches.is_empty() {
        ui.add_space(4.0);
        ui.label(egui::RichText::new("Last detection").color(palette::TEXT_DIM));
        for m in &gui.debug_matches {
            match &m.hit {
                Some(hit) => ui.colored_label(
                    palette::OK,
                    format!(
                        "  {}: score={:.3} margin={:.3} @ ({}, {})",
                        m.alias, hit.score, hit.margin, hit.x, hit.y
                    ),
                ),
                None => ui.colored_label(palette::TEXT_MUTED, format!("  {}: no match", m.alias)),
            };
        }
    }
}

fn draw_zones_editor(ui: &mut egui::Ui, gui: &mut ShopGui) {
    ui.colored_label(
        palette::TEXT_DIM,
        "Click Draw then drag a rectangle on the snapshot.",
    );

    if !gui.zone_status.is_empty() {
        ui.add_space(2.0);
        ui.colored_label(
            palette::WARN,
            format!(
                "{} still to draw: {}",
                gui.zone_status.len(),
                gui.zone_status
                    .iter()
                    .map(|z| z.name)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        );
    }

    if let Some(name) = gui.zone_drag_target {
        ui.add_space(2.0);
        ui.colored_label(
            palette::DEBUG_LABEL,
            format!("Drag on snapshot to set `{name}` (Cancel to abort)."),
        );
    }

    ui.add_space(4.0);

    for (name, color) in ZONE_LIST {
        let is_drawing = gui.zone_drag_target == Some(*name);
        ui.horizontal(|ui| {
            let visible = gui.show_zones.get_mut(*name).unwrap();
            ui.checkbox(visible, "");
            let (color_rect, _) = ui.allocate_exact_size(Vec2::new(10.0, 10.0), Sense::hover());
            ui.painter().rect_filled(color_rect, 2.0, *color);
            let mut name_text = egui::RichText::new(*name);
            if is_drawing {
                name_text = name_text.strong().color(palette::DEBUG_LABEL);
            }
            ui.label(name_text);

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let has_value = gui.zone_mut(name).is_some_and(|s| s.is_some());
                if has_value
                    && ui.small_button("clear").clicked()
                    && let Some(slot) = gui.zone_mut(name)
                {
                    *slot = None;
                    gui.refresh_zone_status();
                }
                let label = if is_drawing { "Cancel" } else { "Draw" };
                if ui.small_button(label).clicked() {
                    gui.zone_drag_target = if is_drawing { None } else { Some(*name) };
                }
            });
        });

        let Some(slot) = gui.zone_mut(name) else {
            continue;
        };
        match slot {
            Some(values) => draw_rect_coords_row(ui, values),
            None => {
                ui.horizontal(|ui| {
                    ui.add_space(20.0);
                    ui.colored_label(palette::TEXT_MUTED, "(unset)");
                });
            }
        }
        ui.add_space(2.0);
    }
}

/// Shared x/y/w/h DragValue row used by both Regions and Zones. Indents
/// under its label row so the four values visually attach to the named
/// rect above.
fn draw_rect_coords_row(ui: &mut egui::Ui, values: &mut [f32; 4]) {
    ui.horizontal(|ui| {
        ui.add_space(20.0);
        let cur_x = values[0];
        let cur_y = values[1];
        ui.spacing_mut().interact_size.x = 0.0;
        ui.add(
            egui::DragValue::new(&mut values[0])
                .speed(0.002)
                .range(0.0..=1.0)
                .prefix("x:")
                .max_decimals(3),
        );
        ui.add(
            egui::DragValue::new(&mut values[1])
                .speed(0.002)
                .range(0.0..=1.0)
                .prefix("y:")
                .max_decimals(3),
        );
        ui.add(
            egui::DragValue::new(&mut values[2])
                .speed(0.002)
                .range(0.001..=(1.0 - cur_x).max(0.001))
                .prefix("w:")
                .max_decimals(3),
        );
        ui.add(
            egui::DragValue::new(&mut values[3])
                .speed(0.002)
                .range(0.001..=(1.0 - cur_y).max(0.001))
                .prefix("h:")
                .max_decimals(3),
        );
        // Belt-and-braces: x/y could have been bumped after w/h were
        // already at their boundary.
        values[2] = values[2].min(1.0 - values[0]);
        values[3] = values[3].min(1.0 - values[1]);
    });
}

fn draw_regions_editor(ui: &mut egui::Ui, gui: &mut ShopGui) {
    ui.colored_label(
        palette::TEXT_DIM,
        "Drag the values to position each ROI on the snapshot.",
    );
    ui.add_space(4.0);

    for (name, color) in ROI_LIST {
        ui.horizontal(|ui| {
            let visible = gui.show_rois.get_mut(*name).unwrap();
            ui.checkbox(visible, "");
            let (color_rect, _) = ui.allocate_exact_size(Vec2::new(10.0, 10.0), Sense::hover());
            ui.painter().rect_filled(color_rect, 2.0, *color);
            ui.label(*name);
        });

        let Some(slot) = gui.region_mut(name) else {
            continue;
        };
        match slot {
            Some(values) => draw_rect_coords_row(ui, values),
            None => {
                ui.horizontal(|ui| {
                    ui.add_space(20.0);
                    ui.colored_label(palette::TEXT_MUTED, "(unset)");
                    if ui.small_button("+ add").clicked() {
                        *slot = Some([0.10, 0.10, 0.20, 0.20]);
                    }
                });
            }
        }
        ui.add_space(2.0);
    }

    ui.add_space(4.0);
    if ui
        .button("Reload from disk")
        .on_hover_text("Discard unsaved edits and re-read config.toml.")
        .clicked()
    {
        match Config::load(&gui.config_path) {
            Ok(c) => {
                gui.config = c;
                gui.saved_snapshot = AutoSavedFields::from_config(&gui.config);
                gui.auto_save_error = None;
                gui.refresh_template_status();
                gui.refresh_zone_status();
            }
            Err(e) => gui.auto_save_error = Some(e.to_string()),
        }
    }
}

fn draw_crop_panel(ui: &mut egui::Ui, gui: &mut ShopGui) {
    ui.colored_label(
        palette::TEXT_DIM,
        "Drag a rectangle on the snapshot, pick a target, click Save.",
    );
    ui.add_space(4.0);

    match gui.crop_selection {
        Some(sel) if sel.w > 0 && sel.h > 0 => {
            ui.label(format!(
                "Selection:  {}×{} at ({}, {})",
                sel.w, sel.h, sel.x, sel.y
            ));
        }
        _ => {
            ui.colored_label(palette::TEXT_MUTED, "no selection");
        }
    }

    ui.horizontal(|ui| {
        ui.label("Save as:");
        egui::ComboBox::from_id_salt("crop_target")
            .selected_text(gui.crop_target.as_str())
            .show_ui(ui, |ui| {
                for alias in TEMPLATE_ALIASES {
                    ui.selectable_value(&mut gui.crop_target, (*alias).to_string(), *alias);
                }
            });
    });

    if let Some(path) = gui.template_path_for(&gui.crop_target.clone()) {
        ui.colored_label(palette::TEXT_DIM, format!("→ {}", path.display()));
    }

    ui.add_space(2.0);
    ui.horizontal(|ui| {
        let can_save = gui.crop_selection.is_some_and(|s| s.w > 0 && s.h > 0);
        if ui
            .add_enabled(can_save, primary_button("Save crop"))
            .clicked()
        {
            gui.save_crop();
        }
        if ui.button("Clear").clicked() {
            gui.crop_selection = None;
            gui.crop_drag_start = None;
            gui.crop_save_error = None;
            gui.crop_save_notice = None;
        }
    });

    if let Some(notice) = &gui.crop_save_notice {
        ui.colored_label(palette::OK, notice);
    }
    if let Some(err) = &gui.crop_save_error {
        ui.colored_label(palette::ERROR, err);
    }
}

fn draw_detection_settings(ui: &mut egui::Ui, gui: &mut ShopGui) {
    ui.colored_label(
        palette::TEXT_DIM,
        "NCC + click geometry. Advanced timing knobs live in config.toml.",
    );
    ui.add_space(6.0);

    // Grid keeps both DragValues aligned at the same X regardless of
    // label width — same pattern as the Run tab's `Stop when…` grid.
    egui::Grid::new("detection_grid")
        .num_columns(2)
        .spacing([12.0, 6.0])
        .show(ui, |ui| {
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
            ui.end_row();

            ui.label("Button Y offset");
            ui.add(
                egui::DragValue::new(&mut gui.config.shop.buy_button_y_offset_ratio)
                    .speed(0.002)
                    .range(0.0..=0.15)
                    .max_decimals(3),
            )
            .on_hover_text(
                "Fraction of window height between an item icon's center and \
                 its row's buy button. E7 puts the button below the icon — \
                 tune via Run detection until the red band lands on it.",
            );
            ui.end_row();
        });
}

fn draw_templates(ui: &mut egui::Ui, gui: &mut ShopGui) {
    if gui.template_status.is_empty() {
        ui.horizontal(|ui| {
            ui.colored_label(
                palette::OK,
                format!("✓ all {} templates present", TEMPLATE_ALIASES.len()),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("Recheck").clicked() {
                    gui.refresh_template_status();
                    gui.try_build_detector();
                }
            });
        });
    } else {
        ui.horizontal(|ui| {
            ui.colored_label(
                palette::WARN,
                format!("{} missing — drop the PNGs at:", gui.template_status.len()),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("Recheck").clicked() {
                    gui.refresh_template_status();
                    gui.try_build_detector();
                }
            });
        });
        egui::ScrollArea::vertical()
            .max_height(110.0)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                for missing in &gui.template_status {
                    ui.label(format!("• {} → {}", missing.name, missing.path.display()));
                }
            });
    }
}

pub(super) fn draw_logs(ui: &mut egui::Ui, logs: &LogBuffer) {
    ui.horizontal(|ui| {
        section_header(ui, "Logs");
        if ui.button("Clear").clicked() {
            logs.clear();
        }
    });
    let lines = logs.snapshot();
    egui::ScrollArea::vertical()
        .stick_to_bottom(true)
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for line in lines {
                ui.label(line);
            }
        });
}
