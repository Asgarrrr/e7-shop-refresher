use crate::gui::app::{ShopGui, palette};

use super::parsers::ms_drag;
use super::section_header;

const TIMING_LABEL_W: f32 = 140.0;

fn apply_timing_field_width(ui: &mut egui::Ui) {
    ui.spacing_mut().interact_size.x = 76.0;
}

pub(in crate::gui) fn draw_timing_section(ui: &mut egui::Ui, gui: &mut ShopGui) {
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

            ui.label("Yield to user (idle ms)");
            ui.add(ms_drag(
                &mut gui.config.timing.cooperative_idle_ms,
                5.0,
                0..=10_000,
                "",
            ))
            .on_hover_text(
                "Cooperative mode. When you touch mouse/keyboard, the bot \
                 pauses and resumes only after this many ms of idle. \
                 0 disables (bot fights you for the cursor).",
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
        });
}
