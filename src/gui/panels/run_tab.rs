use egui::Context;
use egui_phosphor::regular as icon;
use tracing::error;

use crate::gui::app::{RunHistoryPoint, ShopGui, Tab, palette};
use crate::gui::bot::effective_status;
use crate::gui::state::BotStatus;
use crate::shop::{COVENANT_DROP_PER_SLOT, MYSTIC_DROP_PER_SLOT, SHOP_SLOTS_PER_REFRESH};

use super::parsers::{format_gold, format_minutes, parse_gold, parse_minutes};
use super::{section_card, section_header};

fn expected_mystic_per_round() -> f64 {
    MYSTIC_DROP_PER_SLOT * f64::from(SHOP_SLOTS_PER_REFRESH)
}
fn expected_covenant_per_round() -> f64 {
    COVENANT_DROP_PER_SLOT * f64::from(SHOP_SLOTS_PER_REFRESH)
}

const MYSTIC_COLOR: egui::Color32 = egui::Color32::from_rgb(120, 190, 255);
const COVENANT_COLOR: egui::Color32 = egui::Color32::from_rgb(255, 200, 90);

pub(in crate::gui) fn draw_run_tab(ui: &mut egui::Ui, gui: &mut ShopGui, _ctx: &Context) {
    let stats = gui.stats.snapshot();
    let effective = effective_status(gui.bot.as_ref(), stats.status.clone());
    let bot_active = effective.is_active();
    let can_start = gui.capture.is_some() && gui.detector.is_some() && !bot_active;

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
        if stats.round > 0 {
            ui.add_space(8.0);
            draw_progress_graph(ui, &gui.run_history, stats.round);
            draw_chart_legend(ui);
        }
    }
    if let Some(err) = &stats.last_error {
        ui.add_space(4.0);
        ui.colored_label(palette::ERROR, format!("Error: {err}"));
    }

    ui.add_space(10.0);

    // No `add_enabled_ui` wrapper: targets / stop conditions / sleep-on-done
    // are live-editable. The GUI publishes them every frame, the worker
    // re-reads at every round boundary.
    section_card(ui, "Targets", |ui| {
        ui.checkbox(&mut gui.config.shop.buy_mystic_medals, "Buy mystic medals");
        ui.checkbox(&mut gui.config.shop.buy_covenant, "Buy covenant bookmarks");
    });

    section_card(ui, "Stop when…", |ui| {
        ui.colored_label(
            palette::TEXT_DIM,
            "Any limit set to 0 is disabled. The run halts at whichever is reached first.",
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
                    "Time elapsed",
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
                    "Covenant bookmarks bought",
                    &mut gui.config.shop.stop_when_covenants,
                    0.5,
                    0..=10_000,
                    "Halt after this many covenant bookmarks have been bought this run.",
                );
                stop_gold_row(
                    ui,
                    "Gold spent",
                    &mut gui.config.shop.stop_when_gold_spent,
                    "Halt after this much gold has been spent on shop items. \
                     Mystic medals = 280Kg, covenant bookmarks = 185Kg.",
                );
            });
    });

    section_card(ui, "On completion", |ui| {
        ui.checkbox(
            &mut gui.config.shop.sleep_when_done,
            "Sleep PC when goal reached",
        )
        .on_hover_text(
            "Suspends the system to sleep once a stop condition fires. \
             Never triggers on manual Stop.",
        );
        ui.add_space(8.0);
        draw_completion_webhook(ui, gui);
    });
}

fn draw_completion_webhook(ui: &mut egui::Ui, gui: &mut ShopGui) {
    section_header(ui, "Notify Discord");
    ui.colored_label(
        palette::TEXT_DIM,
        "Paste a Discord webhook to receive a summary when a stop condition fires. \
         Leave empty to disable.",
    );
    ui.add_space(4.0);

    ui.add(
        egui::TextEdit::singleline(&mut gui.config.notifications.discord_webhook_url)
            .hint_text("https://discord.com/api/webhooks/…")
            .desired_width(f32::INFINITY)
            .password(true),
    )
    .on_hover_text(
        "Server Settings → Integrations → Webhooks → New Webhook → Copy Webhook URL. \
         Stored locally in config.toml — never sent anywhere except Discord.",
    );

    ui.add_space(4.0);
    ui.horizontal(|ui| {
        let url = gui.config.notifications.webhook_url().to_string();
        let can_test = !url.is_empty();
        if ui
            .add_enabled(
                can_test,
                egui::Button::new(format!("{}  Send test", icon::PAPER_PLANE_TILT)),
            )
            .on_hover_text(
                "Fires a one-line test message to the webhook so you can confirm it works.",
            )
            .clicked()
        {
            crate::notifications::send_discord_test(
                url,
                "**e7-shop-refresher** — webhook test :white_check_mark:".into(),
                gui.webhook_test_status.clone(),
            );
        }
        if !can_test {
            ui.colored_label(palette::TEXT_MUTED, "(disabled — paste a URL first)");
        }
    });

    use crate::notifications::TestEventKind;
    if let Some(kind) = gui.webhook_test_status.visible() {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.add_space(4.0);
            match kind {
                TestEventKind::Pending => {
                    ui.spinner();
                    ui.colored_label(palette::TEXT_MUTED, "Sending…");
                }
                TestEventKind::Ok => {
                    ui.colored_label(palette::OK, format!("{}  Sent", icon::CHECK_CIRCLE));
                }
                TestEventKind::Err(msg) => {
                    ui.colored_label(palette::ERROR, format!("{}  Failed: {msg}", icon::WARNING));
                }
            }
        });
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(250));
    }
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
            // Only window/detector acquisition can block readiness now
            // — templates and layout have bundled fallbacks.
            let blocker = if gui.capture.is_none() {
                "Waiting for window"
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

    // Hide the status line when the CTA button alone already conveys it
    // ("Ready to run" + Start button is redundant). All other states
    // carry information the button can't show on its own.
    if !matches!(action, Action::Start) {
        ui.horizontal(|ui| {
            ui.colored_label(status_color, egui::RichText::new(glyph).size(14.0).strong());
            ui.colored_label(status_color, status);
        });
    }
    if let Some((label, fill)) = btn {
        if !matches!(action, Action::Start) {
            ui.add_space(6.0);
        }
        let button = egui::Button::new(
            egui::RichText::new(label)
                .strong()
                .size(15.0)
                .color(palette::ACCENT_TEXT),
        )
        .fill(fill)
        .min_size(egui::vec2(ui.available_width(), 32.0));
        let mut resp = ui.add_enabled(enabled, button);
        if matches!(action, Action::Stop) {
            resp = resp.on_hover_text(
                "Stop the bot. Ctrl+7 works from anywhere — even when \
                 Epic Seven has focus.",
            );
        }
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
                        gui.stats.update(|s| *s = Default::default());
                    }
                }
                Action::OpenSetup => {
                    gui.active_tab = Tab::Setup;
                }
                Action::None => {}
            }
        }
    }
    // Also rendered for Finished so the completion-webhook dispatch
    // ("Sending Discord notification…") is visible while the worker
    // blocks on the POST between finished() and suspend_to_sleep.
    if matches!(
        effective,
        BotStatus::Running | BotStatus::Stopping | BotStatus::Finished
    ) {
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

fn stop_row<'a>(
    ui: &mut egui::Ui,
    label: &str,
    value: &'a mut u32,
    hover: &str,
    drag: impl FnOnce(&'a mut u32) -> egui::DragValue<'a>,
) {
    // Suppress the active-limit highlight when the parent ui is disabled
    // (bot running) so the row fades with the rest of the panel chrome.
    let active = ui.is_enabled() && *value > 0;
    scoped_value_color(ui, active, |ui| {
        ui.add_sized([72.0, ui.spacing().interact_size.y], drag(value))
            .on_hover_text(hover);
    });
    stop_condition_label(ui, label, active);
    ui.end_row();
}

fn stop_condition_row(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut u32,
    speed: f32,
    range: std::ops::RangeInclusive<u32>,
    hover: &str,
) {
    stop_row(ui, label, value, hover, |v| {
        egui::DragValue::new(v).speed(speed).range(range)
    });
}

fn stop_gold_row(ui: &mut egui::Ui, label: &str, value: &mut u32, hover: &str) {
    stop_row(ui, label, value, hover, |v| {
        egui::DragValue::new(v)
            .speed(1000.0)
            .range(0..=100_000_000u32)
            .custom_formatter(|n, _| format_gold(n as u32))
            .custom_parser(|s| parse_gold(s).map(f64::from))
    });
}

fn stop_duration_row(ui: &mut egui::Ui, label: &str, value: &mut u32, hover: &str) {
    stop_row(ui, label, value, hover, |v| {
        egui::DragValue::new(v)
            .speed(0.5)
            .range(0..=1440)
            .custom_formatter(|n, _| format_minutes(n as u32))
            .custom_parser(|s| parse_minutes(s).map(f64::from))
    });
}

fn stop_condition_label(ui: &mut egui::Ui, label: &str, active: bool) {
    let color = if active {
        palette::SECTION_HEADER
    } else {
        palette::TEXT_MUTED
    };
    ui.label(egui::RichText::new(label).color(color));
}

// Scoped so the colour doesn't leak to widgets after this row.
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
    if shop.stop_when_gold_spent > 0 {
        parts.push(format_gold(shop.stop_when_gold_spent));
    }
    parts.join(", ")
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
    let exp_m = f64::from(round) * expected_mystic_per_round();
    let exp_c = f64::from(round) * expected_covenant_per_round();
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
            ui.label("");
            luck_caption(ui, mystic_bought, exp_m);
            luck_caption(ui, covenant_bought, exp_c);
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

// `—` until expected ≥ 0.5 to avoid flashing 0.00 at the start of a run.
fn luck_caption(ui: &mut egui::Ui, actual: u32, expected: f64) {
    let (text, color, hover) = if expected < 0.5 {
        (
            "—".to_string(),
            palette::TEXT_MUTED,
            format!(
                "Luck ratio (actual ÷ expected drops). Hidden until expected ≥ 0.5 \
                 — currently ≈{expected:.2}."
            ),
        )
    } else {
        let ratio = f64::from(actual) / expected;
        let c = if ratio >= 1.0 {
            palette::OK
        } else if ratio < 0.5 {
            palette::ERROR
        } else {
            palette::TEXT_DIM
        };
        (
            format!("×{ratio:.2}"),
            c,
            format!(
                "Luck: {actual} bought ÷ {expected:.2} expected from shop drop rates. \
                 ×1.00 is average, higher = luckier."
            ),
        )
    };
    ui.label(egui::RichText::new(text).size(11.0).color(color))
        .on_hover_text(hover);
}

fn draw_progress_graph(ui: &mut egui::Ui, history: &[RunHistoryPoint], current_round: u32) {
    if current_round == 0 {
        return;
    }
    let avail_w = ui.available_width();
    let height = 80.0;
    let (rect, _resp) =
        ui.allocate_exact_size(egui::vec2(avail_w, height), egui::Sense::hover());
    let painter = ui.painter_at(rect);

    painter.rect_filled(
        rect,
        4.0,
        egui::Color32::from_rgba_unmultiplied(255, 255, 255, 8),
    );

    let max_round = current_round.max(1) as f32;
    let exp_m_final = max_round * expected_mystic_per_round() as f32;
    let exp_c_final = max_round * expected_covenant_per_round() as f32;
    let actual_m = history.last().map(|p| p.mystic).unwrap_or(0) as f32;
    let actual_c = history.last().map(|p| p.covenant).unwrap_or(0) as f32;
    let max_y = exp_m_final
        .max(exp_c_final)
        .max(actual_m)
        .max(actual_c)
        .max(1.0);

    let to_screen = |round: f32, count: f32| -> egui::Pos2 {
        let nx = round / max_round;
        let ny = count / max_y;
        egui::pos2(
            rect.min.x + nx * rect.width(),
            rect.max.y - ny * rect.height(),
        )
    };

    let mystic = MYSTIC_COLOR;
    let covenant = COVENANT_COLOR;
    let exp_alpha = 70;
    let exp_mystic = egui::Color32::from_rgba_unmultiplied(mystic.r(), mystic.g(), mystic.b(), exp_alpha);
    let exp_covenant =
        egui::Color32::from_rgba_unmultiplied(covenant.r(), covenant.g(), covenant.b(), exp_alpha);

    // Theoretical expected curves are linear in `round` — a single
    // segment from origin to (current_round, expected_total) is exact.
    painter.line_segment(
        [to_screen(0.0, 0.0), to_screen(max_round, exp_m_final)],
        egui::Stroke::new(1.5, exp_mystic),
    );
    painter.line_segment(
        [to_screen(0.0, 0.0), to_screen(max_round, exp_c_final)],
        egui::Stroke::new(1.5, exp_covenant),
    );

    let actual_line = |painter: &egui::Painter, color: egui::Color32, pick: fn(&RunHistoryPoint) -> u32| {
        let mut prev = to_screen(0.0, 0.0);
        for pt in history {
            let next = to_screen(pt.round as f32, pick(pt) as f32);
            painter.line_segment([prev, next], egui::Stroke::new(2.0, color));
            prev = next;
        }
    };
    actual_line(&painter, mystic, |p| p.mystic);
    actual_line(&painter, covenant, |p| p.covenant);
}

fn draw_chart_legend(ui: &mut egui::Ui) {
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.add_space(4.0);
        let swatch = |ui: &mut egui::Ui, color: egui::Color32, label: &str| {
            ui.label(egui::RichText::new("■").color(color).size(11.0));
            ui.label(
                egui::RichText::new(label)
                    .size(10.0)
                    .color(palette::TEXT_DIM),
            );
        };
        swatch(ui, MYSTIC_COLOR, "mystic");
        ui.add_space(8.0);
        swatch(ui, COVENANT_COLOR, "covenant");
        ui.add_space(10.0);
        ui.label(
            egui::RichText::new("— bold = actual, faint = expected")
                .size(10.0)
                .color(palette::TEXT_MUTED),
        );
    })
    .response
    .on_hover_text(
        "Expected curves are linear projections from the shop's mystic / covenant \
         drop rates × slots per refresh.",
    );
}
