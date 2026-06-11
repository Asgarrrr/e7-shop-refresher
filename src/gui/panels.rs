use egui_phosphor::regular as icon;

use crate::gui::app::{ShopGui, Tab, palette};

mod banner;
mod logs_panel;
mod parsers;
mod run_tab;
mod setup_tab;
mod timing;

pub(super) use banner::draw_update_banner;
pub(super) use logs_panel::draw_logs;
pub(super) use run_tab::draw_run_tab;
pub(super) use setup_tab::draw_setup_tab;

/// Rendered only when window detection has an issue — the healthy state
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

/// 1 px rule in `SECTION_STROKE` — quieter than `ui.separator()`.
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
                // Leftover drag target would catch the next unrelated drag
                // on the snapshot and silently overwrite a rectangle.
                gui.clear_override_drag();
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
