//! The Shop tab: the slot table (hand-laid, hover-highlighted), or the
//! welcome screen while nothing is captured yet.

use eframe::egui;

use super::theme;
use super::view::{SlotRow, ViewState};

/// The Shop tab: the slot table, or the welcome screen while nothing is
/// captured yet.
pub(super) fn render_shop_tab(ui: &mut egui::Ui, view: &ViewState) {
    // Keyed on "no capture yet", not empty rows: a tolerated slotless
    // snapshot mid-session must not resurrect first-run onboarding.
    if !view.has_snapshot {
        super::content_inset(ui, render_quick_start);
        return;
    }
    if view.rows.is_empty() {
        super::content_inset(ui, |ui| {
            ui.weak("the last shop message carried no slots — re-open the shop in game");
        });
        return;
    }
    shop_table(ui, &view.rows);
}

/// The slot table, Linear-style: quiet uppercase header over a hairline rule,
/// no zebra fill — rows separate by breathing room and light up on hover, with
/// the full item detail as the row tooltip.
fn shop_table(ui: &mut egui::Ui, rows: &[SlotRow]) {
    let width = ui.available_width();
    // Text columns sit inside this inset; the row band the columns are laid in
    // spans the full width, so the hover fill and header rule reach the edges.
    let edge = egui::vec2(f32::from(theme::EDGE), 0.0);

    let (header, _) = ui.allocate_exact_size(egui::vec2(width, 20.0), egui::Sense::hover());
    let [h_slot, h_kind, h_name, h_price] = column_rects(header.shrink2(edge));
    cell(ui, h_slot, false, theme::section("Slot"));
    cell(ui, h_kind, false, theme::section("Kind"));
    cell(ui, h_name, false, theme::section("Name"));
    // The unit rides in the header so the value cells stay bare numbers, which
    // read cleaner right-aligned than a "184,000 gold" repeated down the column.
    cell(ui, h_price, true, theme::section("Price (gold)"));
    ui.painter().hline(
        header.x_range(),
        header.bottom(),
        egui::Stroke::new(1.0, theme::HAIRLINE),
    );
    ui.add_space(theme::SP_XS);

    // Grow the hover fill into half the inter-row gap on each side, so it covers
    // the full row band (text centred) instead of a tight strip.
    let pad = ui.spacing().item_spacing.y / 2.0;
    for row in rows {
        let (rect, response) =
            ui.allocate_exact_size(egui::vec2(width, 26.0), egui::Sense::hover());
        // `contains_pointer`, not `hovered`: the cell labels sit on top of the
        // row and a truncating label senses hover for its own tooltip, which
        // would otherwise steal the row's `hovered` when the pointer is over
        // the text. Square corners so the fill reads full-width, no side inset.
        if response.contains_pointer() {
            ui.painter().rect_filled(
                rect.expand2(egui::vec2(0.0, pad)),
                egui::CornerRadius::ZERO,
                theme::STRIPE,
            );
        }
        // Matched-and-unbought rows read as body text in the wanted green;
        // sold-out rows mute and strike through — both signals survive the
        // hover fill because the text paints on top of it.
        let style = |text: String| {
            let mut text = egui::RichText::new(text);
            if row.wanted {
                text = text.strong().color(theme::WANTED);
            }
            if row.sold_out {
                text = text.weak().strikethrough();
            }
            text
        };
        let [c_slot, c_kind, c_name, c_price] = column_rects(rect.shrink2(edge));
        cell(ui, c_slot, false, style(row.slot.to_string()));
        cell(ui, c_kind, false, style(row.kind.to_owned()));
        cell(
            ui,
            c_name,
            false,
            style(row.name.clone().unwrap_or_else(|| "—".to_owned())),
        );
        cell(
            ui,
            c_price,
            true,
            style(crate::render::grouped_or_dash(row.price)),
        );
        response.on_hover_text(&row.detail);
    }
}

/// Column geometry shared by the header and every data row so the two always
/// line up: fixed Slot/Kind/Price, Name takes whatever is left.
fn column_rects(row: egui::Rect) -> [egui::Rect; 4] {
    const SLOT: f32 = 40.0;
    const KIND: f32 = 76.0;
    const PRICE: f32 = 104.0;
    const GAP: f32 = 10.0;
    let (top, height) = (row.top(), row.height());
    let kind_x = row.left() + SLOT + GAP;
    let name_x = kind_x + KIND + GAP;
    let price_x = row.right() - PRICE;
    let name_w = (price_x - GAP - name_x).max(60.0);
    let mk = |x: f32, w: f32| egui::Rect::from_min_size(egui::pos2(x, top), egui::vec2(w, height));
    [
        mk(row.left(), SLOT),
        mk(kind_x, KIND),
        mk(name_x, name_w),
        mk(price_x, PRICE),
    ]
}

/// One table cell: a truncating label placed in its column rect, left- or
/// right-aligned (Price sits right).
fn cell(ui: &mut egui::Ui, rect: egui::Rect, align_right: bool, text: egui::RichText) {
    let layout = if align_right {
        egui::Layout::right_to_left(egui::Align::Center)
    } else {
        egui::Layout::left_to_right(egui::Align::Center)
    };
    ui.scope_builder(egui::UiBuilder::new().max_rect(rect).layout(layout), |ui| {
        ui.add(egui::Label::new(text).truncate());
    });
}

/// Welcome screen until the first snapshot lands: what the tool is, and the
/// three steps that make it go.
fn render_quick_start(ui: &mut egui::Ui) {
    ui.add_space(theme::SP_XL);
    ui.vertical_centered(|ui| {
        ui.label(
            egui::RichText::new(crate::APP_NAME)
                .size(22.0)
                .color(theme::INK),
        );
        ui.add_space(theme::SP_XS);
        ui.weak("secret-shop relay — refresh + buy");
    });
    ui.add_space(theme::SP_XL);
    ui.label(theme::section("Quick start"));
    ui.add_space(theme::SP_SM);
    for (number, step) in [
        (
            "1.",
            "Open the Secret Shop in game — the relay captures it live.",
        ),
        ("2.", "Setup tab — define what to hunt and when to stop."),
        ("3.", "Start — the loop refreshes and buys on its own."),
    ] {
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(number).color(theme::ACCENT));
            ui.label(step);
        });
        ui.add_space(theme::SP_XS);
    }
}

#[cfg(test)]
mod tests {
    use egui_kittest::{Harness, kittest::Queryable};

    use crate::domain::control::{Controller, Event, Limits};
    use crate::domain::filter::Filter;
    use crate::domain::shop::{ShopItem, ShopSnapshot};

    use super::super::view::{ViewState, view_state};
    use super::*;

    fn idle_view() -> ViewState {
        view_state(&Controller::new(Filter::default(), Limits::default()))
    }

    fn captured_view(slots: Vec<ShopItem>) -> ViewState {
        let mut ctrl = Controller::new(Filter::default(), Limits::default());
        ctrl.handle(Event::Snapshot {
            snapshot: ShopSnapshot {
                merchant: None,
                slots,
                refresh: None,
            },
            now_ms: 0,
        });
        view_state(&ctrl)
    }

    #[test]
    fn quick_start_shows_before_any_capture() {
        let view = idle_view();
        let harness = Harness::new_ui(|ui| render_shop_tab(ui, &view));
        harness.get_by_label("QUICK START");
    }

    #[test]
    fn table_replaces_quick_start_once_a_shop_is_captured() {
        let view = captured_view(vec![ShopItem {
            slot: 3,
            ..ShopItem::default()
        }]);
        let harness = Harness::new_ui(|ui| render_shop_tab(ui, &view));
        assert!(harness.query_by_label("QUICK START").is_none());
        harness.get_by_label("SLOT");
    }

    #[test]
    fn slotless_snapshot_shows_the_reopen_hint_not_quick_start() {
        let view = captured_view(vec![]);
        let harness = Harness::new_ui(|ui| render_shop_tab(ui, &view));
        assert!(harness.query_by_label("QUICK START").is_none());
        harness.get_by_label("the last shop message carried no slots — re-open the shop in game");
    }
}
