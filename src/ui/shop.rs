//! The Shop tab: the slot table (hand-laid, hover-highlighted), or the
//! welcome screen while nothing is captured yet.

use eframe::egui;

use super::theme;
use super::view::{SlotRow, ViewState};

/// `rows` is borrowed from the shell's cache, re-derived only when the shop
/// moves — see `view::SlotRows`. `detail` is a callback rather than a
/// `SlotRow` field, since egui only asks for the hovered row — see
/// `view::slot_detail`.
pub(super) fn render_shop_tab(
    ui: &mut egui::Ui,
    view: &ViewState,
    rows: &[SlotRow],
    detail: &dyn Fn(usize) -> String,
) {
    // Keyed on "no capture yet", not empty rows: a tolerated slotless
    // snapshot mid-session must not resurrect first-run onboarding.
    if !view.has_snapshot {
        super::content_inset(ui, render_quick_start);
        return;
    }
    if rows.is_empty() {
        super::content_inset(ui, |ui| {
            // Names no cause, because this line no longer has one. It used to be
            // reachable only from a genuinely slotless shop, so "re-open it in
            // game" was sound advice; since `domain::shop::lenient_slots` began
            // degrading an unusable `slots` to empty rather than failing the
            // whole message, the same line is also what a *server* fault looks
            // like — and sending the player to act on the game for a payload
            // problem is the same misattribution the tolerance was filed
            // against. The log has the `warn!` that tells the two apart, which
            // is why the advice is to send it.
            ui.weak(
                "the last shop message carried no usable slots — if this repeats, send the log",
            );
        });
        return;
    }
    shop_table(ui, rows, detail);
}

/// The slot table: uppercase header over a hairline rule, no zebra fill —
/// rows light up on hover, with the full item detail as the tooltip.
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
    // The unit rides in the header so the value cells stay bare, right-aligned numbers.
    cell(ui, h_price, true, theme::section("Price (gold)"));
    ui.painter().hline(
        header.x_range(),
        header.bottom(),
        egui::Stroke::new(1.0, theme::HAIRLINE),
    );
    ui.add_space(theme::SP_XS);

    // Grow the hover fill into half the inter-row gap on each side to cover the full row band.
    let pad = ui.spacing().item_spacing.y / 2.0;
    for (index, row) in rows.iter().enumerate() {
        let (rect, response) =
            ui.allocate_exact_size(egui::vec2(width, 26.0), egui::Sense::hover());
        // `contains_pointer`, not `hovered`: a truncating cell label senses
        // hover for its own tooltip, which would otherwise steal the row's
        // `hovered` while the pointer is over the text.
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
        let price = crate::render::amount_or_dash(row.price);
        cell(ui, c_price, true, styled(row, price));
        // `on_hover_ui`, not `on_hover_text`: egui runs the closure only for
        // the hovered widget, so the item line is formatted for one row, not all six, per frame.
        response.on_hover_ui(|ui| {
            // A tooltip `Area` is sized on its first pass and caches that
            // size (egui's own note on `Area::default_width`, filed as
            // emilk/egui#5167). Each row's tooltip has its own id, so a slot
            // first hovered with a short line ("slot 3 · Equipment") would
            // keep that width on a later roll with a longer one, wrapping or
            // squeezing it. Setting the max width per hover restores egui's
            // own bound.
            ui.set_max_width(ui.spacing().tooltip_width);
            ui.label(detail(index));
        });
    }
}

/// One cell's text in its row's ink: matched-and-unbought rows read in the
/// wanted green, sold-out rows mute and strike through — both survive the
/// hover fill since the text paints on top of it.
///
/// Takes `impl Into<String>` to match `RichText::new`, so a caller may pass
/// an owned value or a borrow without pre-allocating either. A free function
/// rather than a closure because a closure cannot be generic over its argument.
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

    use super::super::view::{SlotRows, ViewState, slot_detail, view_state};
    use super::*;

    fn idle_view() -> ViewState {
        view_state(&Controller::new(Filter::default(), Limits::default()))
    }

    fn captured(slots: Vec<ShopItem>) -> (Controller, ViewState, SlotRows) {
        let mut ctrl = Controller::new(Filter::default(), Limits::default());
        let _ = ctrl.handle(Event::Snapshot {
            snapshot: ShopSnapshot {
                merchant: None,
                slots,
                refresh: None,
            },
            now_ms: 0,
        });
        let view = view_state(&ctrl);
        let mut rows = SlotRows::default();
        rows.sync(&ctrl);
        (ctrl, view, rows)
    }

    /// The tooltip source the live shell builds; no test here hovers a row, so it only needs to exist.
    fn details(ctrl: &Controller) -> impl Fn(usize) -> String + '_ {
        |index| slot_detail(ctrl, index)
    }

    #[test]
    fn quick_start_shows_before_any_capture() {
        let view = idle_view();
        let harness = Harness::new_ui(|ui| render_shop_tab(ui, &view, &[], &|_| String::new()));
        harness.get_by_label("QUICK START");
    }

    #[test]
    fn table_replaces_quick_start_once_a_shop_is_captured() {
        let (ctrl, view, rows) = captured(vec![ShopItem {
            slot: 3,
            ..ShopItem::default()
        }]);
        let detail = details(&ctrl);
        let harness = Harness::new_ui(|ui| render_shop_tab(ui, &view, rows.rows(), &detail));
        assert!(harness.query_by_label("QUICK START").is_none());
        harness.get_by_label("SLOT");
    }

    /// Two claims, and the second is the one that moved: the placeholder must
    /// not resurrect onboarding, and it must not blame the player for a payload
    /// only the server could have sent. An empty `rows` reaches here from a
    /// slotless shop *and* from a `slots` the decoder degraded, and the text
    /// cannot tell which — so it must not pretend to.
    #[test]
    fn slotless_snapshot_shows_a_hint_that_blames_nobody_not_quick_start() {
        let (ctrl, view, rows) = captured(vec![]);
        let detail = details(&ctrl);
        let harness = Harness::new_ui(|ui| render_shop_tab(ui, &view, rows.rows(), &detail));
        assert!(harness.query_by_label("QUICK START").is_none());
        harness.get_by_label(
            "the last shop message carried no usable slots — if this repeats, send the log",
        );
        assert!(
            harness
                .query_by_label("the last shop message carried no slots — re-open the shop in game")
                .is_none(),
            "the old wording sent the player to act on the game for a server fault"
        );
    }
}
