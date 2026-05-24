use egui::{Context, Sense, Vec2};
use egui_phosphor::regular as icon;
use tracing::error;

use crate::gui::app::{ROI_LIST, SECTION_GAP, ShopGui, TEMPLATE_ALIASES, Tab, ZONE_LIST, palette};
use crate::gui::bot::effective_status;
use crate::gui::logs::LogBuffer;
use crate::gui::state::BotStatus;

/// Only rendered when window detection has an issue — the healthy state
/// is signalled by the Start button being enabled.
pub(super) fn draw_window_footer(ui: &mut egui::Ui, gui: &mut ShopGui) {
    let window_error = gui.window_error.clone();
    ui.add_space(6.0);
    if let Some(e) = window_error {
        ui.horizontal_top(|ui| {
            ui.colored_label(palette::ERROR, icon::WARNING);
            ui.vertical(|ui| {
                ui.add(
                    egui::Label::new(egui::RichText::new(&e).color(palette::ERROR))
                        .wrap_mode(egui::TextWrapMode::Wrap),
                );
                ui.add_space(2.0);
                if ui
                    .small_button(format!("{}  Retry", icon::ARROW_CLOCKWISE))
                    .clicked()
                {
                    gui.refresh_template_status();
                    gui.try_acquire_window();
                }
            });
        });
    } else {
        // Briefly reachable between construction and the first acquire.
        ui.horizontal(|ui| {
            ui.colored_label(palette::TEXT_MUTED, icon::DOT);
            ui.colored_label(palette::TEXT_MUTED, "No window detected yet");
        });
    }
    ui.add_space(6.0);
}

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

/// 1 px rule in `SECTION_STROKE`, quieter than `ui.separator()`.
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

fn primary_button(label: &str) -> egui::Button<'_> {
    egui::Button::new(
        egui::RichText::new(label)
            .strong()
            .color(palette::ACCENT_TEXT),
    )
    .fill(palette::ACCENT)
}

pub(super) fn tab_baseline_id() -> egui::Id {
    egui::Id::new("tab_bar_baseline_y")
}

pub(super) fn draw_tab_bar(ui: &mut egui::Ui, gui: &mut ShopGui) {
    ui.add_space(4.0);

    let panel_rect = ui.max_rect();
    let mut active_rect: Option<egui::Rect> = None;

    let row = ui.horizontal(|ui| {
        // Gap wide enough that each underline obviously belongs to one tab.
        ui.spacing_mut().item_spacing.x = 24.0;
        for (tab, label) in [(Tab::Run, "Run"), (Tab::Setup, "Setup")] {
            let selected = gui.active_tab == tab;
            let color = if selected {
                palette::SECTION_HEADER
            } else {
                palette::TEXT_DIM
            };
            let mut text = egui::RichText::new(label).size(15.0).color(color);
            if selected {
                text = text.strong();
            }
            let resp = ui.add(egui::Label::new(text).sense(egui::Sense::click()));
            if resp.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            if resp.clicked() {
                gui.active_tab = tab;
                // A leftover draw target would catch the next unrelated
                // drag on the snapshot and silently overwrite a rectangle.
                gui.zone_drag_target = None;
                gui.region_drag_target = None;
            }
            if selected {
                active_rect = Some(resp.rect);
            }
        }
    });

    // Baseline + active underline share the same y so the marker continues
    // the rule rather than floating above it.
    let baseline_y = row.response.rect.bottom() + 6.0;
    // Exposed so the central panel's stepper hairline can anchor onto the
    // same absolute y — CentralPanel and SidePanel have different default
    // insets, so matching add_space() values isn't enough.
    ui.ctx()
        .data_mut(|d| d.insert_temp(tab_baseline_id(), baseline_y));
    let painter = ui.painter();
    painter.line_segment(
        [
            egui::pos2(panel_rect.left(), baseline_y),
            egui::pos2(panel_rect.right(), baseline_y),
        ],
        egui::Stroke::new(1.0, palette::SECTION_STROKE),
    );
    if let Some(rect) = active_rect {
        painter.line_segment(
            [
                egui::pos2(rect.left(), baseline_y),
                egui::pos2(rect.right(), baseline_y),
            ],
            egui::Stroke::new(2.0, palette::ACCENT),
        );
    }

    ui.add_space(10.0);
}

pub(super) fn draw_run_tab(ui: &mut egui::Ui, gui: &mut ShopGui, _ctx: &Context) {
    let stats = gui.stats.snapshot();
    let effective = effective_status(gui.bot.as_ref(), stats.status.clone());
    let bot_active = effective.is_active();
    let can_start = gui.capture.is_some()
        && gui.detector.is_some()
        && gui.template_status.is_empty()
        && gui.zone_status.is_empty()
        && !bot_active;

    draw_action_row(ui, gui, &effective, can_start);

    if bot_active || stats.round > 0 {
        ui.add_space(6.0);
        draw_run_stats(
            ui,
            stats.round,
            stats.total_rounds,
            stats.mystic_bought,
            stats.covenant_bought,
        );
    }
    if let Some(err) = &stats.last_error {
        ui.add_space(4.0);
        ui.colored_label(palette::ERROR, format!("Error: {err}"));
    }

    section_separator(ui);

    // No `add_enabled_ui` wrapper: targets / stop conditions / sleep-on-done
    // are live-editable. The GUI publishes them every frame, the worker
    // re-reads at every round boundary.
    section_header(ui, "Targets");
    ui.checkbox(&mut gui.config.shop.buy_mystic_medals, "Buy mystic medals");
    ui.checkbox(&mut gui.config.shop.buy_covenant, "Buy covenant bookmarks");

    section_separator(ui);

    section_header(ui, "Stop when…");
    ui.label(
        "Any limit set to 0 is disabled. The run halts at whichever \
         is reached first. All zeros = no auto-stop (manual Stop only).",
    );
    let summary = stop_conditions_summary(&gui.config.shop);
    ui.add_space(2.0);
    if summary.is_empty() {
        ui.colored_label(palette::TEXT_MUTED, "No auto-stop — manual Stop only.");
    } else {
        ui.colored_label(palette::TEXT_MUTED, format!("Active limits: {summary}"));
    }
    ui.add_space(4.0);

    egui::Grid::new("stop_when_grid")
        .num_columns(2)
        .spacing([10.0, 4.0])
        .show(ui, |ui| {
            stop_condition_row(
                ui,
                "Refreshes done",
                &mut gui.config.shop.max_refreshes,
                1.0,
                0..=10_000,
                "Total refresh rounds before halting.",
            );
            stop_duration_row(
                ui,
                "Minutes elapsed",
                &mut gui.config.shop.stop_after_minutes,
                "Wall-clock duration limit. Checked at every round boundary.",
            );
            stop_condition_row(
                ui,
                "Mystic medals bought",
                &mut gui.config.shop.stop_when_mystic_medals,
                0.5,
                0..=10_000,
                "Halt after this many mystic medals have been bought this run.",
            );
            stop_condition_row(
                ui,
                "Covenants bought",
                &mut gui.config.shop.stop_when_covenants,
                0.5,
                0..=10_000,
                "Halt after this many covenant bookmarks have been bought this run.",
            );
        });

    section_separator(ui);

    section_header(ui, "On completion");
    ui.checkbox(
        &mut gui.config.shop.sleep_when_done,
        "Sleep PC when goal reached",
    )
    .on_hover_text(
        "Suspends the system to sleep once a stop condition fires. \
         Never triggers on manual Stop.",
    );
}

fn draw_action_row(ui: &mut egui::Ui, gui: &mut ShopGui, effective: &BotStatus, can_start: bool) {
    enum Action {
        Start,
        Stop,
        OpenSetup,
        None,
    }

    let no_window = gui.capture.is_none();
    let (glyph, status, status_color, btn, action, enabled) = match effective {
        BotStatus::Running => (
            icon::PLAY_CIRCLE,
            "Running",
            palette::OK,
            Some(("Stop", palette::ERROR)),
            Action::Stop,
            true,
        ),
        BotStatus::Stopping => (
            icon::HOURGLASS,
            "Stopping…",
            palette::WARN,
            None,
            Action::None,
            false,
        ),
        _ if can_start => (
            icon::CHECK_CIRCLE,
            "Ready to run",
            palette::OK,
            Some(("Start", palette::ACCENT)),
            Action::Start,
            true,
        ),
        _ if no_window => (
            icon::WARNING,
            "Waiting for Epic Seven",
            palette::WARN,
            None,
            Action::None,
            false,
        ),
        _ => {
            // Verbose copy clips mid-word in the 310 px sidebar — keep the
            // status to a short noun and let the button carry the verb.
            let blocker = if !gui.template_status.is_empty() {
                "Templates missing"
            } else if !gui.zone_status.is_empty() {
                "Zones not drawn"
            } else {
                "Setup incomplete"
            };
            (
                icon::WARNING,
                blocker,
                palette::WARN,
                Some(("Setup", palette::ACCENT)),
                Action::OpenSetup,
                true,
            )
        }
    };

    ui.horizontal(|ui| {
        // Bumped one step over body text so the eye lands on the indicator first.
        ui.colored_label(status_color, egui::RichText::new(glyph).size(14.0).strong());
        ui.colored_label(status_color, status);
        if let Some((label, fill)) = btn {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let button = egui::Button::new(
                    egui::RichText::new(label)
                        .strong()
                        .color(palette::ACCENT_TEXT),
                )
                .fill(fill);
                let resp = ui.add_enabled(enabled, button);
                if resp.clicked() {
                    match action {
                        Action::Start => {
                            if let Err(e) = gui.start_bot() {
                                error!(error = %e, "start failed");
                            }
                        }
                        Action::Stop => {
                            if gui.bot.is_some() {
                                gui.stop_bot();
                            } else {
                                // Demo / orphan-stats path: no worker to signal.
                                gui.stats.update(|s| *s = Default::default());
                            }
                        }
                        Action::OpenSetup => {
                            gui.active_tab = Tab::Setup;
                        }
                        Action::None => {}
                    }
                }
            });
        }
    });
    if matches!(effective, BotStatus::Running | BotStatus::Stopping) {
        let snap = gui.stats.snapshot();
        if let Some(sub) = snap.sub_status {
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                ui.add_space(24.0);
                ui.colored_label(palette::TEXT_MUTED, sub);
            });
        }
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
    // Suppress the active-limit highlight when the parent ui is disabled
    // (bot running) so the row fades with the rest of the panel chrome.
    let active = ui.is_enabled() && *value > 0;
    stop_condition_label(ui, label, active);
    scoped_value_color(ui, active, |ui| {
        ui.add(egui::DragValue::new(value).speed(speed).range(range))
            .on_hover_text(hover);
    });
    ui.end_row();
}

/// Like `stop_condition_row` but the value formats/parses as a duration
/// (`45m`, `1h30`, `2h`); `0` = disabled.
fn stop_duration_row(ui: &mut egui::Ui, label: &str, value: &mut u32, hover: &str) {
    let active = ui.is_enabled() && *value > 0;
    stop_condition_label(ui, label, active);
    scoped_value_color(ui, active, |ui| {
        ui.add(
            egui::DragValue::new(value)
                .speed(0.5)
                .range(0..=1440)
                .custom_formatter(|n, _| format_minutes(n as u32))
                .custom_parser(|s| parse_minutes(s).map(f64::from)),
        )
        .on_hover_text(hover);
    });
    ui.end_row();
}

fn stop_condition_label(ui: &mut egui::Ui, label: &str, active: bool) {
    let color = if active {
        palette::SECTION_HEADER
    } else {
        palette::TEXT_MUTED
    };
    ui.label(egui::RichText::new(label).color(color));
}

/// Scoped to a nested ui so the colour doesn't leak to widgets after this row.
fn scoped_value_color(ui: &mut egui::Ui, active: bool, add_contents: impl FnOnce(&mut egui::Ui)) {
    let color = if active {
        palette::ACCENT
    } else {
        palette::TEXT_MUTED
    };
    ui.scope(|ui| {
        ui.style_mut().visuals.override_text_color = Some(color);
        add_contents(ui);
    });
}

fn stop_conditions_summary(shop: &crate::config::ShopConfig) -> String {
    let mut parts: Vec<String> = Vec::new();
    if shop.max_refreshes > 0 {
        parts.push(format!("{} refreshes", shop.max_refreshes));
    }
    if shop.stop_after_minutes > 0 {
        parts.push(format_minutes(shop.stop_after_minutes));
    }
    if shop.stop_when_mystic_medals > 0 {
        parts.push(format!("{} mystic", shop.stop_when_mystic_medals));
    }
    if shop.stop_when_covenants > 0 {
        parts.push(format!("{} covenant", shop.stop_when_covenants));
    }
    parts.join(", ")
}

fn format_minutes(total: u32) -> String {
    // Match the bare-integer "disabled" sentinel used by the other rows
    // so the grid stays consistent with the "set to 0 to disable" copy.
    if total == 0 {
        return "0".to_string();
    }
    let hours = total / 60;
    let minutes = total % 60;
    match (hours, minutes) {
        (0, m) => format!("{m}m"),
        (h, 0) => format!("{h}h"),
        (h, m) => format!("{h}h{m:02}"),
    }
}

/// Accepts: empty, bare integer (minutes), `30m`, `30 min`, `2h`, `1h30`,
/// `1h 30`, `1h30m`. Returns `None` on anything else so the DragValue
/// keeps the previous value rather than silently zeroing on a typo.
fn parse_minutes(input: &str) -> Option<u32> {
    let s = input.trim().to_ascii_lowercase();
    if s.is_empty() {
        return Some(0);
    }
    if let Some(h_pos) = s.find('h') {
        let hours: u32 = s[..h_pos].trim().parse().ok()?;
        let rest = strip_minute_suffix(s[h_pos + 1..].trim());
        let minutes: u32 = if rest.is_empty() {
            0
        } else {
            rest.parse().ok()?
        };
        return Some(hours * 60 + minutes);
    }
    strip_minute_suffix(&s).parse().ok()
}

fn strip_minute_suffix(s: &str) -> &str {
    let s = s
        .strip_suffix("min")
        .or_else(|| s.strip_suffix('m'))
        .unwrap_or(s);
    s.trim()
}

/// Renders an ms value as the shortest readable form: bare ms under 1 s,
/// `1.5s` / `12s` above. Plays the same role for `[timing]` knobs as
/// `format_minutes` does for the Stop-when grid.
fn format_ms(ms: u64) -> String {
    if ms < 1000 {
        return format!("{ms}ms");
    }
    let seconds = ms as f64 / 1000.0;
    // One decimal up to 10 s, integer beyond — keeps the field narrow.
    if seconds < 10.0 {
        format!("{seconds:.1}s")
    } else {
        format!("{}s", seconds.round() as u64)
    }
}

/// Accepts: bare integer (ms), `500ms`, `1.5s`, `12s`, `2 s`. Returns
/// `None` on garbage so the DragValue keeps the previous value.
fn parse_ms(input: &str) -> Option<u64> {
    let s = input.trim().to_ascii_lowercase();
    if s.is_empty() {
        return Some(0);
    }
    if let Some(rest) = s.strip_suffix("ms") {
        return rest.trim().parse().ok();
    }
    if let Some(rest) = s.strip_suffix('s') {
        let v: f64 = rest.trim().parse().ok()?;
        return Some((v * 1000.0).round() as u64);
    }
    s.parse().ok()
}

/// `DragValue` wired with `format_ms` / `parse_ms`, optionally prefixed
/// with "min "/"max " (so it can be used inside min/max pairs without
/// extra plumbing per call site).
fn ms_drag<'a, Num: egui::emath::Numeric>(
    value: &'a mut Num,
    speed: f32,
    range: std::ops::RangeInclusive<Num>,
    prefix: &'static str,
) -> egui::DragValue<'a> {
    egui::DragValue::new(value)
        .speed(speed)
        .range(range)
        .custom_formatter(move |n, _| {
            let s = format_ms(n.max(0.0).round() as u64);
            if prefix.is_empty() {
                s
            } else {
                format!("{prefix}{s}")
            }
        })
        .custom_parser(move |s| {
            let s = if prefix.is_empty() {
                s
            } else {
                s.strip_prefix(prefix).unwrap_or(s)
            };
            parse_ms(s).map(|v| v as f64)
        })
}

fn draw_run_stats(
    ui: &mut egui::Ui,
    round: u32,
    total_rounds: u32,
    mystic_bought: u32,
    covenant_bought: u32,
) {
    let round_value = if total_rounds > 0 {
        format!("{round} / {total_rounds}")
    } else {
        format!("{round}")
    };
    egui::Grid::new("run_stats_block")
        .num_columns(3)
        .spacing([24.0, 2.0])
        .show(ui, |ui| {
            stat_value(ui, &round_value);
            stat_value(ui, &format!("{mystic_bought}"));
            stat_value(ui, &format!("{covenant_bought}"));
            ui.end_row();
            stat_caption(ui, "rounds");
            stat_caption(ui, "mystic");
            stat_caption(ui, "covenant");
            ui.end_row();
        });
}

fn stat_value(ui: &mut egui::Ui, text: &str) {
    ui.label(
        egui::RichText::new(text)
            .size(20.0)
            .strong()
            .color(palette::SECTION_HEADER),
    );
}

fn stat_caption(ui: &mut egui::Ui, text: &str) {
    ui.label(
        egui::RichText::new(text)
            .size(11.0)
            .color(palette::TEXT_DIM),
    );
}

pub(super) fn draw_setup_tab(ui: &mut egui::Ui, gui: &mut ShopGui, ctx: &Context) {
    let bot_active = effective_status(gui.bot.as_ref(), gui.stats.snapshot().status).is_active();

    section_card(ui, "Snapshot", |ui| {
        draw_snapshot_section(ui, gui, ctx, bot_active)
    });

    ui.add_enabled_ui(!bot_active, |ui| {
        section_card(ui, &templates_card_title(gui), |ui| {
            draw_crop_workflow(ui, gui)
        });
    });

    ui.add_enabled_ui(!bot_active, |ui| {
        section_card(ui, &regions_card_title(gui), |ui| {
            draw_regions_editor(ui, gui)
        });
    });

    ui.add_enabled_ui(!bot_active, |ui| {
        section_card(ui, &zones_card_title(gui), |ui| draw_zones_editor(ui, gui));
    });

    ui.add_enabled_ui(!bot_active, |ui| {
        section_card(ui, "Timing", |ui| draw_timing_section(ui, gui));
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

fn regions_card_title(gui: &ShopGui) -> String {
    let r = &gui.config.regions;
    let unset = [r.shop_grid, r.anchor_shop]
        .iter()
        .filter(|v| v.is_none())
        .count();
    if unset == 0 {
        "Search regions  ·  ready".to_string()
    } else {
        format!("Search regions  ·  {unset} unset")
    }
}

fn zones_card_title(gui: &ShopGui) -> String {
    let z = &gui.config.zones;
    let unset = [z.refresh, z.refresh_confirm, z.buy_confirm, z.buy_column]
        .iter()
        .filter(|v| v.is_none())
        .count();
    if unset == 0 {
        "Click targets  ·  ready".to_string()
    } else {
        format!("Click targets  ·  {unset} to draw")
    }
}

fn draw_snapshot_section(ui: &mut egui::Ui, gui: &mut ShopGui, ctx: &Context, bot_active: bool) {
    let refresh_enabled = !bot_active;
    ui.horizontal(|ui| {
        if ui
            .add_enabled(
                refresh_enabled,
                primary_button(&format!("{}  Refresh", icon::ARROWS_CLOCKWISE)),
            )
            .on_hover_text("Capture the current game window into the central preview.")
            .clicked()
        {
            gui.refresh_snapshot(ctx);
        }
        if ui
            .add_enabled(
                refresh_enabled,
                egui::Button::new(format!("{}  Run detection", icon::CROSSHAIR)),
            )
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

    ui.add_space(8.0);
    ui.add_enabled_ui(!bot_active, |ui| draw_detection_settings(ui, gui));
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
                    && ui.small_button("Clear").clicked()
                    && let Some(slot) = gui.zone_mut(name)
                {
                    *slot = None;
                    gui.refresh_zone_status();
                }
                let label = if is_drawing { "Cancel" } else { "Draw" };
                if ui.small_button(label).clicked() {
                    if is_drawing {
                        gui.zone_drag_target = None;
                    } else {
                        gui.zone_drag_target = Some(*name);
                        gui.region_drag_target = None;
                    }
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
        "Click Draw then drag a rectangle on the snapshot. \
         Regions are optional — unset falls back to the full image.",
    );

    let r = &gui.config.regions;
    let mut unset: Vec<&str> = Vec::new();
    if r.shop_grid.is_none() {
        unset.push("shop_grid");
    }
    if r.anchor_shop.is_none() {
        unset.push("anchor_shop");
    }
    if !unset.is_empty() {
        ui.add_space(2.0);
        ui.colored_label(
            palette::WARN,
            format!("{} unset: {}", unset.len(), unset.join(", ")),
        );
    }

    if let Some(name) = gui.region_drag_target {
        ui.add_space(2.0);
        ui.colored_label(
            palette::DEBUG_LABEL,
            format!("Drag on snapshot to set `{name}` (Cancel to abort)."),
        );
    }

    ui.add_space(4.0);

    for (name, color) in ROI_LIST {
        let is_drawing = gui.region_drag_target == Some(*name);
        ui.horizontal(|ui| {
            let visible = gui.show_rois.get_mut(*name).unwrap();
            ui.checkbox(visible, "");
            let (color_rect, _) = ui.allocate_exact_size(Vec2::new(10.0, 10.0), Sense::hover());
            ui.painter().rect_filled(color_rect, 2.0, *color);
            let mut name_text = egui::RichText::new(*name);
            if is_drawing {
                name_text = name_text.strong().color(palette::DEBUG_LABEL);
            }
            ui.label(name_text);

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let has_value = gui.region_mut(name).is_some_and(|s| s.is_some());
                if has_value
                    && ui.small_button("Clear").clicked()
                    && let Some(slot) = gui.region_mut(name)
                {
                    *slot = None;
                }
                let label = if is_drawing { "Cancel" } else { "Draw" };
                if ui.small_button(label).clicked() {
                    if is_drawing {
                        gui.region_drag_target = None;
                    } else {
                        gui.region_drag_target = Some(*name);
                        gui.zone_drag_target = None;
                    }
                }
            });
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
                });
            }
        }
        ui.add_space(2.0);
    }
}

fn draw_crop_workflow(ui: &mut egui::Ui, gui: &mut ShopGui) {
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
            ui.colored_label(palette::TEXT_MUTED, "No selection");
        }
    }

    // Status shown inline on each dropdown row instead of as a separate
    // checklist above the combo.
    let missing_aliases: std::collections::HashSet<&str> = gui
        .template_status
        .iter()
        .map(|m| m.name.as_str())
        .collect();

    ui.horizontal(|ui| {
        ui.label("Save as:");
        let selected_status = if missing_aliases.contains(gui.crop_target.as_str()) {
            " · missing"
        } else {
            " · saved"
        };
        egui::ComboBox::from_id_salt("crop_target")
            .selected_text(format!("{}{selected_status}", gui.crop_target))
            .show_ui(ui, |ui| {
                for alias in TEMPLATE_ALIASES {
                    let is_missing = missing_aliases.contains(*alias);
                    let suffix = if is_missing {
                        " · missing"
                    } else {
                        " · saved"
                    };
                    let color = if is_missing {
                        palette::WARN
                    } else {
                        palette::OK
                    };
                    let label = egui::RichText::new(format!("{alias}{suffix}")).color(color);
                    ui.selectable_value(&mut gui.crop_target, (*alias).to_string(), label);
                }
            });
    });

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
            let resp = ui
                .add(
                    egui::DragValue::new(&mut gui.config.shop.buy_button_y_offset_ratio)
                        .speed(0.002)
                        .range(0.0..=0.15)
                        .max_decimals(3),
                )
                .on_hover_text(
                    "Fraction of window height between an item icon's center and \
                     its row's buy button. Hover this row to preview the click \
                     band over the last detected items.",
                );
            crate::gui::app::register_edit_focus(&resp, crate::gui::app::EditFocus::BuyOffset);
            ui.end_row();
        });
}

const TIMING_LABEL_W: f32 = 140.0;

fn apply_timing_field_width(ui: &mut egui::Ui) {
    ui.spacing_mut().interact_size.x = 76.0;
}

fn draw_timing_section(ui: &mut egui::Ui, gui: &mut ShopGui) {
    ui.colored_label(
        palette::TEXT_DIM,
        "Anti-detection tuning. Defaults work for most users.",
    );
    ui.add_space(6.0);

    draw_timing_click(ui, gui);
    ui.add_space(10.0);
    draw_timing_mouse(ui, gui);
    ui.add_space(10.0);
    draw_timing_round_pacing(ui, gui);
    ui.add_space(10.0);
    draw_timing_misc(ui, gui);
}

fn draw_timing_click(ui: &mut egui::Ui, gui: &mut ShopGui) {
    section_header(ui, "Click");
    egui::Grid::new("timing_click")
        .num_columns(2)
        .min_col_width(TIMING_LABEL_W)
        .spacing([12.0, 6.0])
        .show(ui, |ui| {
            apply_timing_field_width(ui);

            ui.label("Mean delay");
            ui.add(ms_drag(
                &mut gui.config.timing.click_delay_mean_ms,
                1.0,
                10.0..=2000.0,
                "",
            ))
            .on_hover_text("Log-normal mean inter-click delay.");
            ui.end_row();

            ui.label("Delay σ");
            ui.add(
                egui::DragValue::new(&mut gui.config.timing.click_delay_sigma)
                    .speed(0.01)
                    .range(0.0..=1.5)
                    .max_decimals(2),
            )
            .on_hover_text(
                "Log-space dispersion — higher = more variance, fatter tail. \
                 0.3 is the shipped default.",
            );
            ui.end_row();

            ui.label("Delay clamp");
            ui.horizontal(|ui| {
                apply_timing_field_width(ui);
                ui.add(ms_drag(
                    &mut gui.config.timing.click_delay_min_ms,
                    1.0,
                    0..=10_000,
                    "min ",
                ));
                ui.add(ms_drag(
                    &mut gui.config.timing.click_delay_max_ms,
                    1.0,
                    0..=10_000,
                    "max ",
                ));
            });
            ui.end_row();

            ui.label("Jitter radius");
            let resp = ui
                .add(
                    egui::DragValue::new(&mut gui.config.timing.jitter_radius_px)
                        .speed(0.1)
                        .range(0.0..=20.0)
                        .max_decimals(1)
                        .suffix(" px"),
                )
                .on_hover_text(
                    "Rayleigh-distributed offset added to clicks on matched \
                     items (mystic medals, covenants). Zone clicks (refresh, \
                     confirm modals, buy column) already pick a uniform point \
                     inside the zone and ignore this. Hover + Run detection \
                     to preview the scatter.",
                );
            crate::gui::app::register_edit_focus(&resp, crate::gui::app::EditFocus::Jitter);
            ui.end_row();
        });
}

fn draw_timing_mouse(ui: &mut egui::Ui, gui: &mut ShopGui) {
    section_header(ui, "Mouse motion");
    egui::Grid::new("timing_mouse")
        .num_columns(2)
        .min_col_width(TIMING_LABEL_W)
        .spacing([12.0, 6.0])
        .show(ui, |ui| {
            apply_timing_field_width(ui);

            ui.label("Path steps");
            ui.horizontal(|ui| {
                apply_timing_field_width(ui);
                ui.add(
                    egui::DragValue::new(&mut gui.config.timing.move_steps_min)
                        .speed(0.1)
                        .range(1..=50)
                        .prefix("min "),
                );
                ui.add(
                    egui::DragValue::new(&mut gui.config.timing.move_steps_max)
                        .speed(0.1)
                        .range(1..=50)
                        .prefix("max "),
                );
            });
            ui.end_row();

            ui.label("Step duration");
            ui.horizontal(|ui| {
                apply_timing_field_width(ui);
                ui.add(ms_drag(
                    &mut gui.config.timing.move_step_min_ms,
                    0.2,
                    0..=200,
                    "min ",
                ));
                ui.add(ms_drag(
                    &mut gui.config.timing.move_step_max_ms,
                    0.2,
                    0..=200,
                    "max ",
                ));
            });
            ui.end_row();

            ui.label("Pre-click pause");
            ui.horizontal(|ui| {
                apply_timing_field_width(ui);
                ui.add(ms_drag(
                    &mut gui.config.timing.move_to_click_min_ms,
                    0.5,
                    0..=500,
                    "min ",
                ));
                ui.add(ms_drag(
                    &mut gui.config.timing.move_to_click_max_ms,
                    0.5,
                    0..=500,
                    "max ",
                ));
            });
            ui.end_row();

            ui.label("Curve amplitude");
            ui.add(
                egui::DragValue::new(&mut gui.config.timing.move_curve_amplitude_px)
                    .speed(0.1)
                    .range(0.0..=30.0)
                    .max_decimals(1)
                    .suffix(" px"),
            )
            .on_hover_text("Perpendicular arc strength of the mouse path. 0 = straight line.");
            ui.end_row();
        });
}

fn draw_timing_round_pacing(ui: &mut egui::Ui, gui: &mut ShopGui) {
    section_header(ui, "Round pacing");
    egui::Grid::new("timing_pacing")
        .num_columns(2)
        .min_col_width(TIMING_LABEL_W)
        .spacing([12.0, 6.0])
        .show(ui, |ui| {
            apply_timing_field_width(ui);

            ui.label("Inter-round");
            ui.horizontal(|ui| {
                apply_timing_field_width(ui);
                ui.add(ms_drag(
                    &mut gui.config.timing.inter_round_min_ms,
                    5.0,
                    0..=60_000,
                    "min ",
                ));
                ui.add(ms_drag(
                    &mut gui.config.timing.inter_round_max_ms,
                    5.0,
                    0..=60_000,
                    "max ",
                ));
            });
            ui.end_row();

            ui.label("Long pause every");
            ui.add(
                egui::DragValue::new(&mut gui.config.timing.long_pause_every_n)
                    .speed(0.1)
                    .range(0..=100)
                    .suffix(" rounds"),
            )
            .on_hover_text("0 disables the long-pause cadence.");
            ui.end_row();

            ui.label("Long pause");
            ui.horizontal(|ui| {
                apply_timing_field_width(ui);
                ui.add(ms_drag(
                    &mut gui.config.timing.long_pause_min_ms,
                    10.0,
                    0..=120_000,
                    "min ",
                ));
                ui.add(ms_drag(
                    &mut gui.config.timing.long_pause_max_ms,
                    10.0,
                    0..=120_000,
                    "max ",
                ));
            });
            ui.end_row();

            ui.label("Anchor timeout");
            ui.add(ms_drag(
                &mut gui.config.timing.anchor_timeout_ms,
                10.0,
                500..=30_000,
                "",
            ))
            .on_hover_text(
                "How long to wait for the shop header before giving up the round. \
                 Bump if the game takes long to load.",
            );
            ui.end_row();

            ui.label("Poll interval");
            ui.add(ms_drag(
                &mut gui.config.timing.poll_interval_ms,
                1.0,
                10..=2000,
                "",
            ))
            .on_hover_text(
                "How often the anchor wait re-snapshots. Lower = snappier \
                 but more CPU.",
            );
            ui.end_row();
        });
}

fn draw_timing_misc(ui: &mut egui::Ui, gui: &mut ShopGui) {
    section_header(ui, "Modal & scroll");
    egui::Grid::new("timing_misc")
        .num_columns(2)
        .min_col_width(TIMING_LABEL_W)
        .spacing([12.0, 6.0])
        .show(ui, |ui| {
            apply_timing_field_width(ui);

            ui.label("Modal open pause");
            ui.add(ms_drag(
                &mut gui.config.timing.modal_open_pause_ms,
                2.0,
                0..=2000,
                "",
            ))
            .on_hover_text(
                "Wait after click for the confirm modal slide-in animation. \
                 Lower this and the bot may click before the modal is ready.",
            );
            ui.end_row();

            ui.label("Scroll amount");
            ui.add(
                egui::DragValue::new(&mut gui.config.timing.scroll_amount)
                    .speed(0.1)
                    .range(-30..=30),
            )
            .on_hover_text(
                "Wheel notches per scroll. Positive scrolls down. Negative \
                 inverts everything (rarely useful).",
            );
            ui.end_row();

            ui.label("Scroll pause");
            ui.add(ms_drag(
                &mut gui.config.timing.scroll_pause_ms,
                2.0,
                0..=2000,
                "",
            ));
            ui.end_row();
        });
}

pub(super) fn draw_logs(ui: &mut egui::Ui, logs: &LogBuffer) {
    // Stored in egui memory so the filter setting doesn't need to be
    // plumbed through ShopGui.
    let filter_id = egui::Id::new("log_min_level");
    let mut min_level = ui
        .ctx()
        .data(|d| d.get_temp::<LogLevel>(filter_id))
        .unwrap_or(LogLevel::Info);

    ui.horizontal(|ui| {
        section_header(ui, "Logs");
        if ui.button("Clear").clicked() {
            logs.clear();
        }
        ui.add_space(8.0);
        ui.label("Min level:");
        let prev = min_level;
        egui::ComboBox::from_id_salt("log_min_level_combo")
            .selected_text(min_level.label())
            .show_ui(ui, |ui| {
                for option in LogLevel::ALL {
                    ui.selectable_value(&mut min_level, *option, option.label());
                }
            });
        if min_level != prev {
            ui.ctx().data_mut(|d| d.insert_temp(filter_id, min_level));
        }
    });
    let lines = logs.snapshot();
    let threshold = min_level.to_tracing();
    egui::ScrollArea::vertical()
        .stick_to_bottom(true)
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for line in lines {
                if line.level > threshold {
                    continue;
                }
                let color = level_color(line.level);
                ui.colored_label(color, format!("{}  {}", level_glyph(line.level), line.text));
            }
        });
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl LogLevel {
    const ALL: &'static [Self] = &[
        Self::Trace,
        Self::Debug,
        Self::Info,
        Self::Warn,
        Self::Error,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Trace => "TRACE",
            Self::Debug => "DEBUG",
            Self::Info => "INFO",
            Self::Warn => "WARN",
            Self::Error => "ERROR",
        }
    }

    fn to_tracing(self) -> tracing::Level {
        match self {
            Self::Trace => tracing::Level::TRACE,
            Self::Debug => tracing::Level::DEBUG,
            Self::Info => tracing::Level::INFO,
            Self::Warn => tracing::Level::WARN,
            Self::Error => tracing::Level::ERROR,
        }
    }
}

fn level_glyph(level: tracing::Level) -> &'static str {
    // `tracing::Level` constants are associated consts on a struct, not
    // enum variants — they can't be matched against.
    if level == tracing::Level::ERROR {
        "ERROR"
    } else if level == tracing::Level::WARN {
        "WARN "
    } else if level == tracing::Level::INFO {
        "INFO "
    } else if level == tracing::Level::DEBUG {
        "DEBUG"
    } else {
        "TRACE"
    }
}

fn level_color(level: tracing::Level) -> egui::Color32 {
    if level == tracing::Level::ERROR {
        palette::ERROR
    } else if level == tracing::Level::WARN {
        palette::WARN
    } else if level == tracing::Level::INFO {
        palette::SECTION_HEADER
    } else {
        palette::TEXT_DIM
    }
}

#[cfg(test)]
mod tests {
    use super::{format_minutes, format_ms, parse_minutes, parse_ms};

    #[test]
    fn format_minutes_handles_canonical_cases() {
        assert_eq!(format_minutes(0), "0");
        assert_eq!(format_minutes(1), "1m");
        assert_eq!(format_minutes(45), "45m");
        assert_eq!(format_minutes(60), "1h");
        assert_eq!(format_minutes(90), "1h30");
        assert_eq!(format_minutes(125), "2h05"); // zero-padded minutes
        assert_eq!(format_minutes(1440), "24h");
    }

    #[test]
    fn parse_minutes_accepts_canonical_forms() {
        assert_eq!(parse_minutes(""), Some(0));
        assert_eq!(parse_minutes("0"), Some(0));
        assert_eq!(parse_minutes("45"), Some(45));
        assert_eq!(parse_minutes("45m"), Some(45));
        assert_eq!(parse_minutes("45 min"), Some(45));
        assert_eq!(parse_minutes("2h"), Some(120));
        assert_eq!(parse_minutes("1h30"), Some(90));
        assert_eq!(parse_minutes("1h 30"), Some(90));
        assert_eq!(parse_minutes("1h30m"), Some(90));
        assert_eq!(parse_minutes("1h05"), Some(65));
    }

    #[test]
    fn parse_minutes_rejects_garbage() {
        assert_eq!(parse_minutes("abc"), None);
        assert_eq!(parse_minutes("h30"), None); // hours empty
        assert_eq!(parse_minutes("1h xyz"), None);
    }

    #[test]
    fn format_and_parse_round_trip_for_typical_values() {
        for &n in &[0u32, 5, 30, 60, 75, 120, 240, 1439] {
            let formatted = format_minutes(n);
            let parsed = parse_minutes(&formatted).expect("formatted output must parse back");
            assert_eq!(parsed, n, "round-trip failed for {n} via {formatted:?}");
        }
    }

    #[test]
    fn format_ms_picks_ms_or_seconds_by_magnitude() {
        assert_eq!(format_ms(0), "0ms");
        assert_eq!(format_ms(120), "120ms");
        assert_eq!(format_ms(999), "999ms");
        assert_eq!(format_ms(1000), "1.0s");
        assert_eq!(format_ms(1500), "1.5s");
        assert_eq!(format_ms(9999), "10.0s");
        // ≥ 10 s drops the decimal so the field stays narrow.
        assert_eq!(format_ms(10_000), "10s");
        assert_eq!(format_ms(12_500), "13s"); // rounded
        assert_eq!(format_ms(60_000), "60s");
    }

    #[test]
    fn parse_ms_accepts_canonical_forms() {
        assert_eq!(parse_ms(""), Some(0));
        assert_eq!(parse_ms("0"), Some(0));
        assert_eq!(parse_ms("500"), Some(500));
        assert_eq!(parse_ms("500ms"), Some(500));
        assert_eq!(parse_ms("1.5s"), Some(1500));
        assert_eq!(parse_ms("2 s"), Some(2000));
        assert_eq!(parse_ms("12s"), Some(12_000));
    }

    #[test]
    fn parse_ms_rejects_garbage() {
        assert_eq!(parse_ms("abc"), None);
        assert_eq!(parse_ms("ms"), None); // value-less
        assert_eq!(parse_ms("xyz s"), None);
    }

    #[test]
    fn format_and_parse_ms_round_trip() {
        for &n in &[0u64, 1, 120, 250, 999, 1000, 1500, 5000, 12_000, 60_000] {
            let formatted = format_ms(n);
            let parsed = parse_ms(&formatted).expect("formatted output must parse back");
            // ≥ 10 s drops decimal so 12_500 rounds to 13_000; only test
            // values that don't hit that.
            assert_eq!(parsed, n, "round-trip failed for {n} via {formatted:?}");
        }
    }
}
