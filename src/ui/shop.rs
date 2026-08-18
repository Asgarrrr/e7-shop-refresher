//! The Shop tab: the slot table (hand-laid, hover-highlighted), or the
//! welcome screen while nothing is captured yet.

use eframe::egui;

use super::theme;
use super::view::{SlotRow, ViewState};

/// The Shop tab: the slot table, or the welcome screen while nothing is
/// captured yet.
///
/// `detail` yields one row's full console line for the hover tooltip. It is a
/// callback rather than a `SlotRow` field because egui only asks for the row
/// under the pointer — see `view::slot_detail`.
pub(super) fn render_shop_tab(
    ui: &mut egui::Ui,
    view: &ViewState,
    detail: &dyn Fn(usize) -> String,
) {
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
    shop_table(ui, &view.rows, detail);
}

/// The slot table, Linear-style: quiet uppercase header over a hairline rule,
/// no zebra fill — rows separate by breathing room and light up on hover, with
/// the full item detail as the row tooltip.
fn shop_table(ui: &mut egui::Ui, rows: &[SlotRow], detail: &dyn Fn(usize) -> String) {
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
    for (index, row) in rows.iter().enumerate() {
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
        let [c_slot, c_kind, c_name, c_price] = column_rects(rect.shrink2(edge));
        cell(ui, c_slot, false, styled(row, row.slot.to_string()));
        cell(ui, c_kind, false, styled(row, row.kind));
        cell(
            ui,
            c_name,
            false,
            styled(row, row.name.as_deref().unwrap_or("—")),
        );
        let price = crate::render::grouped_or_dash(row.price);
        cell(ui, c_price, true, styled(row, price));
        // `on_hover_ui`, not `on_hover_text`: egui runs the closure only for the
        // widget the pointer is over, so the item line is formatted for the one
        // hovered row instead of all six on every frame.
        response.on_hover_ui(|ui| {
            ui.label(detail(index));
        });
    }
}

/// One cell's text in its row's ink: matched-and-unbought rows read as body text
/// in the wanted green, sold-out rows mute and strike through — both signals
/// survive the hover fill because the text paints on top of it.
///
/// Takes `impl Into<String>` to match `RichText::new`, so a caller may pass an
/// owned value or a borrow without pre-allocating either. A free function rather
/// than a closure only because a closure cannot be generic over its argument.
/// Not measured, and not worth measuring at four columns of six rows.
fn styled(row: &SlotRow, text: impl Into<String>) -> egui::RichText {
    let mut text = egui::RichText::new(text);
    if row.wanted {
        text = text.strong().color(theme::WANTED);
    }
    if row.sold_out {
        text = text.weak().strikethrough();
    }
    text
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

    use super::super::view::{ViewState, slot_detail, view_state};
    use super::*;

    fn idle_view() -> ViewState {
        view_state(&Controller::new(Filter::default(), Limits::default()))
    }

    fn captured(slots: Vec<ShopItem>) -> (Controller, ViewState) {
        let mut ctrl = Controller::new(Filter::default(), Limits::default());
        ctrl.handle(Event::Snapshot {
            snapshot: ShopSnapshot {
                merchant: None,
                slots,
                refresh: None,
            },
            now_ms: 0,
        });
        let view = view_state(&ctrl);
        (ctrl, view)
    }

    /// The tooltip source the live shell builds from the controller lock; no
    /// test here hovers a row, so the rows only need it to exist.
    fn details(ctrl: &Controller) -> impl Fn(usize) -> String + '_ {
        |index| slot_detail(ctrl, index)
    }

    #[test]
    fn quick_start_shows_before_any_capture() {
        let view = idle_view();
        let harness = Harness::new_ui(|ui| render_shop_tab(ui, &view, &|_| String::new()));
        harness.get_by_label("QUICK START");
    }

    #[test]
    fn table_replaces_quick_start_once_a_shop_is_captured() {
        let (ctrl, view) = captured(vec![ShopItem {
            slot: 3,
            ..ShopItem::default()
        }]);
        let detail = details(&ctrl);
        let harness = Harness::new_ui(|ui| render_shop_tab(ui, &view, &detail));
        assert!(harness.query_by_label("QUICK START").is_none());
        harness.get_by_label("SLOT");
    }

    #[test]
    fn slotless_snapshot_shows_the_reopen_hint_not_quick_start() {
        let (ctrl, view) = captured(vec![]);
        let detail = details(&ctrl);
        let harness = Harness::new_ui(|ui| render_shop_tab(ui, &view, &detail));
        assert!(harness.query_by_label("QUICK START").is_none());
        harness.get_by_label("the last shop message carried no slots — re-open the shop in game");
    }
}
