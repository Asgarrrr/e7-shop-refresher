use egui::{Context, Sense, Vec2};
use egui_phosphor::regular as icon;
use tracing::error;

use crate::config::Config;
use crate::gui::app::{ROI_LIST, SECTION_GAP, ShopGui, TEMPLATE_ALIASES, Tab, ZONE_LIST, palette};
use crate::gui::bot::effective_status;
use crate::gui::logs::LogBuffer;
use crate::gui::persist::AutoSavedFields;
use crate::gui::state::BotStatus;

/// Footer surfacing window-detection problems. Only invoked when
/// something is off (no detection yet at boot, or detection failed) —
/// the healthy state is signalled by the Start button being enabled,
/// so a permanent green chip would be redundant.
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
        // Reachable transiently between ShopGui construction and the
        // first `try_acquire_window` call.
        ui.horizontal(|ui| {
            ui.colored_label(palette::TEXT_MUTED, icon::DOT);
            ui.colored_label(palette::TEXT_MUTED, "No window detected yet");
        });
    }
    ui.add_space(6.0);
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

/// Renders a filesystem path as a compact, clickable label:
///
/// - Display is truncated to the trailing two components prefixed
///   with `…` (e.g. `…\templates\shop_header.png`) so long AppData
///   paths don't blow out the 310 px sidebar.
/// - Hover shows the full path in a tooltip with a "click to copy"
///   hint, and switches the cursor to a pointing hand.
/// - Click writes the full path to the system clipboard so the user
///   can paste it straight into Explorer's address bar.
fn path_label(ui: &mut egui::Ui, path: &std::path::Path) {
    let full = path.display().to_string();
    let short = format_short_path(path);
    let resp = ui.add(
        egui::Label::new(egui::RichText::new(&short).color(palette::TEXT_DIM))
            .sense(egui::Sense::click()),
    );
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    if resp.clicked() {
        ui.ctx().copy_text(full.clone());
    }
    resp.on_hover_text(format!("{full}\n\nClick to copy"));
}

/// `…<sep><parent><sep><filename>` when the path has 3+ components,
/// otherwise the original display form. Uses the OS-native separator
/// so Windows paths render with backslashes.
fn format_short_path(path: &std::path::Path) -> String {
    let sep = std::path::MAIN_SEPARATOR_STR;
    let components: Vec<_> = path
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect();
    if components.len() <= 2 {
        return path.display().to_string();
    }
    let n = components.len();
    format!("…{sep}{}{sep}{}", components[n - 2], components[n - 1])
}

/// Linear/Vercel-style underline tabs: no background chrome on the
/// labels, a hairline baseline rule spanning the full panel width, and
/// the active tab marked by a thicker accent-coloured underline drawn
/// on top of that baseline. Labels in muted grey when inactive, near-
/// white and bold when active.
pub(super) fn draw_tab_bar(ui: &mut egui::Ui, gui: &mut ShopGui) {
    ui.add_space(4.0);

    let panel_rect = ui.max_rect();
    let mut active_rect: Option<egui::Rect> = None;

    let row = ui.horizontal(|ui| {
        // Generous gap between labels — the underline reads as belonging
        // to one tab only if neighbouring labels stay clearly separated.
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
            }
            if selected {
                active_rect = Some(resp.rect);
            }
        }
    });

    // Baseline rule + active underline both sit on the same y so the
    // active marker visually continues the rule rather than floating
    // above it.
    let baseline_y = row.response.rect.bottom() + 6.0;
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

/// Renders the Run tab. Run-tab parameters (targets, stop conditions,
/// sleep-on-done) are live-editable mid-run — UI edits ride through
/// the shared `live_shop` handle and are picked up by the worker at
/// every round boundary.
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

    // Run-tab fields are live-editable mid-run: the GUI publishes them
    // to `live_shop` every frame, the worker re-reads at every round
    // boundary. No `add_enabled_ui` wrapper here — edits to targets /
    // stop conditions / sleep-on-done take effect within ~1 round.
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

    #[cfg(debug_assertions)]
    draw_demo_controls(ui, gui);
}

/// Debug-only helper: writes plausible "mid-run" values into the shared
/// stats sink so developers without a live Epic Seven window can see
/// the running-state UI (toggle button switching to Stop, stat block
/// appearing, sections disabling). Clicking the main toggle button
/// while in this state clears the stats back to Idle.
#[cfg(debug_assertions)]
fn draw_demo_controls(ui: &mut egui::Ui, gui: &mut ShopGui) {
    ui.add_space(12.0);
    ui.separator();
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.colored_label(palette::TEXT_MUTED, "Debug:");
        if ui.small_button("Inject demo stats").clicked() {
            gui.stats.update(|s| {
                s.status = BotStatus::Running;
                s.round = 7;
                s.total_rounds = 30;
                s.mystic_bought = 2;
                s.covenant_bought = 1;
                s.items_bought = 3;
                s.last_error = None;
            });
        }
    });
}

/// Compact action header — replaces the previous big morphing button.
/// Status text sits on the left (colored dot + label describing the
/// current state), action button on the right (small, primary_button
/// style — same visual weight as `Save crop` and `Refresh` elsewhere).
/// Fits the rest of the panel's typography-driven aesthetic instead
/// of dominating the top with a chunky filled rectangle.
fn draw_action_row(ui: &mut egui::Ui, gui: &mut ShopGui, effective: &BotStatus, can_start: bool) {
    enum Action {
        Start,
        Stop,
        OpenSetup,
        None,
    }

    let no_window = gui.capture.is_none();
    // (status_glyph, status_label, status_color,
    //  optional (button_label, button_fill), action, enabled)
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
            Some(("Stopping…", palette::WARN)),
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
            // Short status + clear action verb on the button reads as
            // "Templates missing → Setup": the user doesn't need a
            // full sentence to know what to do. Earlier verbose copy
            // ("Setup needed — crop your icon templates") clipped
            // mid-word in the 310 px sidebar.
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
        // Larger glyph so the status indicator actually registers —
        // matches the body text size but bumped by a step so the eye
        // lands on it first.
        ui.colored_label(status_color, egui::RichText::new(glyph).size(14.0).strong());
        ui.colored_label(status_color, status);
        // Push the button to the right.
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
                                // Demo / orphan-stats path: no worker
                                // to signal — just clear the sink.
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
}

fn stop_condition_row(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut u32,
    speed: f32,
    range: std::ops::RangeInclusive<u32>,
    hover: &str,
) {
    // When the surrounding ui is disabled (bot running), suppress the
    // active-limit highlight so the row fades in step with the rest of
    // the panel chrome — otherwise the blue values stay eye-poppingly
    // bright while every neighbouring widget greys out.
    let active = ui.is_enabled() && *value > 0;
    stop_condition_label(ui, label, active);
    scoped_value_color(ui, active, |ui| {
        ui.add(egui::DragValue::new(value).speed(speed).range(range))
            .on_hover_text(hover);
    });
    ui.end_row();
}

/// Same row layout as `stop_condition_row` but the value displays /
/// parses as a duration: `0` for disabled, `45m` for under an hour,
/// `1h30` for hours-plus-minutes, `2h` for round hours. Parsing
/// accepts the same forms plus a bare integer (interpreted as
/// minutes).
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

/// `override_text_color` is scoped to a nested ui so the inner
/// DragValue picks it up in both display and edit modes without
/// leaking to widgets rendered after this row.
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

fn format_minutes(total: u32) -> String {
    // `0` mirrors the bare-integer "disabled" sentinel used by every
    // other stop condition row — keeps the grid visually consistent
    // and matches the section's intro text ("set to 0 to disable").
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

/// Accepts an empty string, a bare integer (interpreted as minutes),
/// `30m`, `30 min`, `2h`, `1h30`, `1h 30`, `1h30m`. Returns `None` for
/// anything else so the DragValue keeps the previous value instead of
/// silently zeroing out on a typo.
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

/// Three-column stat block shown when the bot is running (or has run
/// during this session). Big numbers + small captions read faster than
/// a single text line, especially at a glance from across the room.
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

    // Workflow order: capture → verify templates → crop → calibrate
    // overlays → advanced tuning. Snapshot sits first because it's the
    // most-used button during tuning sessions.
    section_card(ui, "Snapshot", |ui| {
        draw_snapshot_section(ui, gui, ctx, bot_active)
    });

    // Templates merges what used to be two sections (Templates status
    // + Crop & Save tool). They describe the same concern — the bot
    // needs three icon crops to work — so splitting them only adds
    // visual chrome. Status + Recheck stay enabled even while the
    // bot runs; the crop workflow gates on !bot_active inside.
    section_card(ui, &templates_card_title(gui), |ui| {
        draw_templates_section(ui, gui, bot_active);
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

/// Internal crop workflow: instructional line + selection state +
/// alias dropdown + target-path preview + Save / Clear buttons. Lives
/// inside the Templates section now — the previous standalone "Crop &
/// Save" card was the same concern under a different header.
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

    // Build a quick lookup of which aliases are still missing so each
    // dropdown row can show its status inline (avoids a redundant
    // checklist above the dropdown).
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

    if let Some(path) = gui.template_path_for(&gui.crop_target.clone()) {
        path_label(ui, &path);
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

/// Merged Templates section: Recheck pinned top-right, then the
/// crop workflow. The workflow's own intro line carries the
/// instructions, so this wrapper stays nearly empty — no redundant
/// "Crop each missing icon" header to clip against the button.
/// Per-alias status lives inside the workflow's dropdown.
fn draw_templates_section(ui: &mut egui::Ui, gui: &mut ShopGui, bot_active: bool) {
    // `with_layout(right_to_left)` at the top level would claim the
    // full remaining vertical space and float the button mid-panel.
    // Wrapping it in `ui.horizontal` constrains it to a single row.
    ui.horizontal(|ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .small_button(format!("{}  Recheck", icon::ARROW_CLOCKWISE))
                .on_hover_text(
                    "Re-scan the templates folder. Useful if you've \
                     added a PNG by hand or while the bot was running.",
                )
                .clicked()
            {
                gui.refresh_template_status();
                gui.try_build_detector();
            }
        });
    });
    ui.add_space(4.0);

    ui.add_enabled_ui(!bot_active, |ui| {
        draw_crop_workflow(ui, gui);
    });
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

#[cfg(test)]
mod tests {
    use super::{format_minutes, parse_minutes};

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
}
