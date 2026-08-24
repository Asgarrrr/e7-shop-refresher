//! Crystal-blue dark theme: the palette and the visual style. Widgets take
//! colors from here, never hand-picked hex.
//!
//! Hierarchy comes from size, ink level and fill — not from hue. Nothing here
//! maps a session state to a colour any more: the run says what it is doing
//! with a figure, a [`gauge`] and the verb on its command, so the green/amber/
//! red dot that used to sit in the status bar is gone along with the mapping
//! behind it. The accent survives in three places that are not states: the
//! active tab's underline, the selection fill, and [`primary_button`] — which
//! now marks a commit (Setup's Apply) rather than permanent chrome.

use eframe::egui::{self, Color32, CornerRadius, FontFamily, FontId, Stroke, TextStyle};

const PAGE: Color32 = Color32::from_rgb(0x0d, 0x0d, 0x0d);
const PANEL: Color32 = Color32::from_rgb(0x1a, 0x1a, 0x19);
/// Row hover, one step above the panel. (Also egui's `faint_bg_color`.)
pub(super) const STRIPE: Color32 = Color32::from_rgb(0x22, 0x22, 0x21);
/// Accent: selection, links, the active tab's underline, the primary button.
pub(super) const ACCENT: Color32 = Color32::from_rgb(0x39, 0x87, 0xe5);
const ACCENT_HOVER: Color32 = Color32::from_rgb(0x4f, 0x95, 0xea);
const ACCENT_PRESSED: Color32 = Color32::from_rgb(0x2f, 0x74, 0xc9);
/// Primary ink.
pub(super) const INK: Color32 = Color32::from_rgb(0xff, 0xff, 0xff);
/// Secondary ink (static labels, counters, inactive tabs).
pub(super) const INK_MUTED: Color32 = Color32::from_rgb(0xc3, 0xc2, 0xb7);
/// Tertiary ink: hints, the journal caret.
pub(super) const INK_FAINT: Color32 = Color32::from_rgb(0x89, 0x87, 0x81);
/// Hairline strokes and separators (panel dividers, the table's header rule).
pub(super) const HAIRLINE: Color32 = Color32::from_rgb(0x2c, 0x2c, 0x2a);
/// The slab command's edge and its hover fill: one step either side of
/// [`STRIPE`], so a full-width control reads as a surface that can be pressed
/// rather than as a saturated call to action.
const SLAB_EDGE: Color32 = Color32::from_rgb(0x35, 0x35, 0x2f);
const SLAB_HOVER: Color32 = Color32::from_rgb(0x2b, 0x2b, 0x29);
/// The rule under a bare verb, and the same rule lifted while it is hovered.
const VERB_RULE: Color32 = Color32::from_rgb(0x46, 0x45, 0x3f);
/// Paused, and stops the player planned (limits).
pub(super) const AMBER: Color32 = Color32::from_rgb(0xfa, 0xb2, 0x19);
/// Stops the player did not plan (machine faults).
const RED: Color32 = Color32::from_rgb(0xe5, 0x48, 0x4d);
/// Matched rows in the shop table: brighter than the status green, to stay
/// legible as body text on the panel.
pub(super) const WANTED: Color32 = Color32::from_rgb(0x90, 0xee, 0x90);
/// A timing meter's tuned baseline, muted so the `ACCENT` slack past it reads
/// as the player-controlled part.
pub(super) const METER_BASE: Color32 = Color32::from_rgb(0x2c, 0x42, 0x60);

/// Spacing scale (4px grid). Every gap the layout inserts comes from here.
pub(super) const SP_XS: f32 = 4.0;
pub(super) const SP_SM: f32 = 8.0;
/// Horizontal button padding, wider than the vertical rhythm on purpose.
pub(super) const SP_MD: f32 = 12.0;
pub(super) const SP_XL: f32 = 24.0;

/// Horizontal inset for tab content, while rules and hover fills bleed to the
/// window edges. Matches the chrome's side margin, so the table text lines up
/// under the status bar.
pub(super) const EDGE: i8 = 16;

/// Installs the theme on the context. Called once per window, before the first
/// frame.
pub(super) fn apply(ctx: &egui::Context) {
    // Pinned, so an OS in light mode does not swap egui's light visuals under
    // a palette that is dark by design.
    ctx.set_theme(egui::Theme::Dark);
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = PANEL;
    visuals.window_fill = PANEL;
    visuals.extreme_bg_color = PAGE;
    visuals.faint_bg_color = STRIPE;
    visuals.selection.bg_fill = ACCENT;
    visuals.selection.stroke = Stroke::new(1.0, INK);
    visuals.hyperlink_color = ACCENT;
    visuals.error_fg_color = RED;
    visuals.warn_fg_color = AMBER;
    // `ui.weak` and hand-colored hints share this grey.
    visuals.weak_text_color = Some(INK_FAINT);
    visuals.window_corner_radius = CornerRadius::same(6);
    visuals.menu_corner_radius = CornerRadius::same(6);
    for widget in [
        &mut visuals.widgets.noninteractive,
        &mut visuals.widgets.inactive,
        &mut visuals.widgets.hovered,
        &mut visuals.widgets.active,
        &mut visuals.widgets.open,
    ] {
        widget.corner_radius = CornerRadius::same(6);
    }
    visuals.widgets.noninteractive.bg_stroke.color = HAIRLINE;
    visuals.widgets.noninteractive.fg_stroke.color = INK_MUTED;
    visuals.widgets.inactive.fg_stroke.color = INK;
    visuals.widgets.hovered.fg_stroke.color = INK;
    visuals.widgets.active.fg_stroke.color = INK;
    visuals.widgets.open.fg_stroke.color = INK;
    ctx.set_visuals(visuals);

    ctx.all_styles_mut(|style| {
        style.text_styles = [
            (TextStyle::Body, FontId::new(14.0, FontFamily::Proportional)),
            (
                TextStyle::Button,
                FontId::new(14.0, FontFamily::Proportional),
            ),
            (
                TextStyle::Heading,
                FontId::new(16.0, FontFamily::Proportional),
            ),
            (
                TextStyle::Small,
                FontId::new(11.0, FontFamily::Proportional),
            ),
            (
                TextStyle::Monospace,
                FontId::new(12.5, FontFamily::Monospace),
            ),
        ]
        .into();
        style.spacing.item_spacing = egui::vec2(SP_SM, SP_SM);
        style.spacing.button_padding = egui::vec2(SP_MD, SP_SM);
    });
}

/// Section header: small grey capitals. `.small()`/`.weak()` rather than fixed
/// values, so a retune in [`apply`] carries over.
pub(super) fn section(text: &str) -> egui::RichText {
    egui::RichText::new(text.to_uppercase()).small().weak()
}

/// A full-bleed hairline across the panel, painted past the side margin so it
/// reaches the window edges like the tab strip's rule and the tables' own.
///
/// Takes its colour because the two callers divide different things: a rule
/// between rows of one block is dimmed, a rule between blocks is not.
pub(super) fn rule(ui: &mut egui::Ui, color: Color32) {
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 1.0), egui::Sense::hover());
    ui.painter().hline(
        ui.clip_rect().x_range(),
        rect.center().y,
        Stroke::new(1.0, color),
    );
}

/// One table cell: a truncating label placed in its column rect.
///
/// Shared by both tables in this window — the shop's slot list and the capture
/// counters — so a header and its values are placed the same way in each. It
/// lived in [`super::shop`] while there was only one; a second copy beside the
/// second table is how two tables drift into two idioms.
///
/// Truncating rather than wrapping, because a cell that grows a second line
/// pushes its row out of step with the column rects the caller computed.
pub(super) fn cell(ui: &mut egui::Ui, rect: egui::Rect, align_right: bool, text: egui::RichText) {
    let layout = if align_right {
        egui::Layout::right_to_left(egui::Align::Center)
    } else {
        egui::Layout::left_to_right(egui::Align::Center)
    };
    ui.scope_builder(egui::UiBuilder::new().max_rect(rect).layout(layout), |ui| {
        ui.add(egui::Label::new(text).truncate());
    });
}

/// The one emphasis size, shared by the status label and the primary button.
pub(super) fn emphasis(text: impl Into<String>) -> egui::RichText {
    egui::RichText::new(text.into()).size(15.0)
}

/// The one saturated element on screen. Text color comes from the themed
/// `fg_stroke` so a disabled button still mutes, and `active.bg_stroke` is left
/// alone because keyboard focus renders Active and that stroke is its ring.
pub(super) fn primary_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    ui.scope(|ui| {
        let widgets = &mut ui.style_mut().visuals.widgets;
        widgets.inactive.weak_bg_fill = ACCENT;
        widgets.hovered.weak_bg_fill = ACCENT_HOVER;
        widgets.hovered.bg_stroke = Stroke::NONE;
        widgets.active.weak_bg_fill = ACCENT_PRESSED;
        ui.add(egui::Button::new(emphasis(text)).min_size(egui::vec2(88.0, 32.0)))
    })
    .inner
}

/// A bare (label-less) checkbox filling the accent when checked. Squared to a
/// 2px radius because the global 6px radius rounds the ~16px checkbox icon into
/// a circle that reads as a radio button. The caller owns the row's label.
pub(super) fn accent_checkbox(ui: &mut egui::Ui, on: &mut bool) -> egui::Response {
    ui.scope(|ui| {
        let visuals = &mut ui.style_mut().visuals;
        for state in [
            &mut visuals.widgets.inactive,
            &mut visuals.widgets.hovered,
            &mut visuals.widgets.active,
        ] {
            state.corner_radius = CornerRadius::same(2);
        }
        if *on {
            visuals.widgets.inactive.bg_fill = ACCENT;
            visuals.widgets.hovered.bg_fill = ACCENT_HOVER;
            visuals.widgets.active.bg_fill = ACCENT_PRESSED;
        }
        ui.checkbox(on, "")
    })
    .inner
}

/// A chip's corner, rounder than the global 6px so a value in a wrapped row
/// reads as a pill rather than as a small button.
const CHIP_RADIUS: u8 = 8;

/// What a toggling *surface* looks like: the fill and the edge one takes for a
/// given state.
///
/// One spelling, because two controls wear it and they sit on the same screen:
/// [`chip`], and the token card that carries a price and so is too tall to be
/// one. Written as a function rather than as two constants because the answer
/// is a pair and the three states do not vary independently — an unchosen chip
/// is edged and unfilled, a chosen one is both, and hover only moves the
/// unchosen case.
pub(super) fn toggle_skin(on: bool, hovered: bool) -> (Color32, Stroke) {
    let fill = match (on, hovered) {
        // `ACCENT` and not a tint: this is the theme's own selection fill, the
        // same one `accent_checkbox` takes when it is ticked.
        (true, _) => ACCENT,
        (false, true) => STRIPE,
        (false, false) => Color32::TRANSPARENT,
    };
    let edge = match (on, hovered) {
        (true, _) => ACCENT,
        (false, true) => SLAB_EDGE,
        (false, false) => HAIRLINE,
    };
    (fill, Stroke::new(1.0, edge))
}

/// One toggling chip: a rounded outlined box, accent-filled and accent-edged
/// once chosen.
///
/// Takes the [`egui::Button`] already built rather than a label, because its
/// callers differ in what a chip HOLDS — a gear set the server sent a picture
/// for draws the picture, every other value draws its name — while the skin is
/// the one thing they must share.
///
/// **The caller owns the accessible name.** `Button::selected` states a
/// `WidgetType::Button`, and a chip that toggles is a checkbox; a picture-only
/// button states no name at all. Both callers restate the info, and
/// `hunt::text_chip` / `hunt::icon_chip` carry the reasoning.
///
/// **It styles the caller's `Ui` in place and restores it, rather than adding
/// the chip inside a `ui.scope`.** A scope builds a CHILD `Ui`, and a child
/// never reaches `Layout::next_frame` — where `main_wrap` lives — so a chip
/// drawn in one is invisible to `horizontal_wrapped` and simply takes whatever
/// the cursor has left. That is the same defect the id salt on these rows was
/// moved to fix, and styling through a scope reintroduced it exactly: measured
/// on the live 24-set catalog at the window's fixed 440px, the row ran on one
/// line out to x=1022 with every chip past the fifth squeezed to ~24px of
/// unreadable text. Restoring the saved `Arc<Style>` is what keeps the chip in
/// the caller's own `Ui`; the clone is a refcount bump, not a copy of the style.
pub(super) fn chip(ui: &mut egui::Ui, button: egui::Button<'_>, on: bool) -> egui::Response {
    let saved = ui.style().clone();
    // Tighter than the theme's `SP_MD`/`SP_SM` button box, which is sized for a
    // standalone command. The window is pinned at [`super::WINDOW_WIDTH`] and
    // the sets row is twenty-four chips wide, so the padding a chip can afford
    // is decided by that row and not by taste.
    ui.spacing_mut().button_padding = egui::vec2(SP_SM, SP_XS);
    let widgets = &mut ui.style_mut().visuals.widgets;
    for (state, hovered) in [
        (&mut widgets.inactive, false),
        (&mut widgets.hovered, true),
        (&mut widgets.active, true),
    ] {
        let (fill, edge) = toggle_skin(on, hovered);
        state.corner_radius = CornerRadius::same(CHIP_RADIUS);
        state.weak_bg_fill = fill;
        state.bg_stroke = edge;
        // A hovered egui widget grows by its `expansion`, and these wrap: two
        // pixels under the pointer re-flow every chip after it, so the row
        // twitches as the mouse crosses it.
        state.expansion = 0.0;
    }
    // Unchosen chips sit one ink level down, the way the segmented strip dims
    // its inactive cells — with twenty-four of them, full ink on every unchosen
    // value is what stops the chosen ones reading as chosen. A chosen chip takes
    // its text colour from `selection.stroke` and is unaffected.
    widgets.inactive.fg_stroke.color = INK_MUTED;
    // `selected` is what fills it: egui swaps `selection.bg_fill` and the
    // matching text colour in, which is this theme's accent pair already.
    let response = ui.add(button.selected(on));
    ui.set_style(saved);
    response
}

/// Several exclusive cells sharing one border: the active one filled, the strip
/// reading as a single control rather than as loose buttons.
///
/// Snug by construction — the cells carry no spacing between them, which is
/// what makes the shared ground visible between the rounded ends.
///
/// Two sections use it, which is why it lives here: the Click-timing mode row
/// and Hunt's substat floor. A second copy of the recipe beside the second
/// caller is how one control becomes two that drift.
pub(super) fn segmented_strip<R>(
    ui: &mut egui::Ui,
    add_contents: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    egui::Frame::new()
        .fill(STRIPE)
        .corner_radius(CornerRadius::same(8))
        .inner_margin(3)
        .show(ui, |ui| {
            ui.style_mut().visuals.widgets.inactive.fg_stroke.color = INK_MUTED;
            ui.spacing_mut().item_spacing.x = 0.0;
            ui.horizontal(add_contents).inner
        })
        .inner
}

/// A full-width collapsible section header. Painted, not nested widgets, so
/// nothing steals hover over the bar. Returns true on click.
///
/// While collapsed, `summary` trails right-aligned and rides in the accessible
/// name too (`title · summary`), so a folded section still shows what it holds.
pub(super) fn collapsing_section(
    ui: &mut egui::Ui,
    title: &str,
    summary: Option<&str>,
    open: bool,
) -> bool {
    let (bar, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 28.0), egui::Sense::hover());
    // Grown by half the item spacing so consecutive bars tile at the seam:
    // egui hit-tests wider than the 28px fill, so hovering the gap would light
    // the wrong bar. Symmetric, so the label stays centred.
    let pad = ui.spacing().item_spacing.y * 0.5;
    let full = egui::Rect::from_x_y_ranges(
        ui.clip_rect().x_range(),
        egui::Rangef::new(bar.top() - pad, bar.bottom() + pad),
    );
    let response = ui.interact(full, ui.id().with(("section", title)), egui::Sense::click());
    let enabled = ui.is_enabled();
    let peek = (!open).then_some(()).and(summary);
    // Built inside the closure: `widget_info` only calls it when AccessKit is
    // live, a harness is reading, or the widget was just clicked. Outside, the
    // `format!` would be paid every frame for nothing.
    response.widget_info(|| {
        let name = match peek {
            Some(summary) => format!("{title} · {summary}"),
            None => title.to_owned(),
        };
        egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, &name)
    });

    // The caret trails one step down so the word leads; hover lifts both to
    // full ink over the fill.
    let hovered = response.hovered();
    let (title_color, caret_color) = if hovered {
        (INK, INK)
    } else {
        (INK_MUTED, INK_FAINT)
    };
    if hovered {
        ui.painter().rect_filled(full, CornerRadius::ZERO, STRIPE);
    }
    let painter = ui.painter().with_clip_rect(full);
    caret(&painter, bar, open, caret_color);
    let title_rect = painter.text(
        egui::pos2(bar.left() + 22.0, bar.center().y),
        egui::Align2::LEFT_CENTER,
        title.to_uppercase(),
        FontId::new(13.0, FontFamily::Proportional),
        title_color,
    );
    // Clipped left of the title, so a long summary never crosses it.
    if let Some(summary) = peek {
        let region = egui::Rect::from_min_max(
            egui::pos2(title_rect.right() + SP_SM, bar.top()),
            egui::pos2(bar.right() - SP_XS, bar.bottom()),
        );
        ui.painter().with_clip_rect(region).text(
            egui::pos2(region.right(), bar.center().y),
            egui::Align2::RIGHT_CENTER,
            summary,
            FontId::new(12.0, FontFamily::Proportional),
            INK_FAINT,
        );
    }
    // At the tiled seam, not the inner bar, so the hover fill stops exactly at
    // the divider instead of spilling into the next row. Drawn with the
    // unclipped painter so the seam-edge line is not cut.
    ui.painter()
        .hline(full.x_range(), full.bottom(), Stroke::new(1.0, HAIRLINE));
    response.clicked()
}

/// A small painted disclosure caret at the left of `row` (right = closed,
/// down = open), not a `▸` glyph, so it never depends on the stock font
/// carrying the symbol.
pub(super) fn caret(painter: &egui::Painter, row: egui::Rect, open: bool, color: Color32) {
    let c = egui::pos2(row.left() + 6.0, row.center().y);
    let r = 3.5;
    let points = if open {
        vec![
            egui::pos2(c.x - r, c.y - r * 0.5),
            egui::pos2(c.x + r, c.y - r * 0.5),
            egui::pos2(c.x, c.y + r),
        ]
    } else {
        vec![
            egui::pos2(c.x - r * 0.5, c.y - r),
            egui::pos2(c.x - r * 0.5, c.y + r),
            egui::pos2(c.x + r, c.y),
        ]
    };
    painter.add(egui::Shape::convex_polygon(points, color, Stroke::NONE));
}

/// The run's one command while nothing has run yet: full width, one step above
/// the panel, edged rather than filled.
///
/// Not [`primary_button`], and the difference is the point. A saturated pill is
/// sized for the instant someone decides to start, but the status bar showed it
/// for the whole run — an hour of the loudest object on screen bought two
/// clicks. This one only exists in the band that has nothing else to say;
/// once a run is live the command is a [`bare_verb`], and the accent survives
/// exactly where a commit is being made, on Setup's Apply.
pub(super) fn slab_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    ui.scope(|ui| {
        let widgets = &mut ui.style_mut().visuals.widgets;
        for state in [
            &mut widgets.inactive,
            &mut widgets.hovered,
            &mut widgets.active,
        ] {
            state.bg_stroke = Stroke::new(1.0, SLAB_EDGE);
        }
        widgets.inactive.weak_bg_fill = STRIPE;
        widgets.hovered.weak_bg_fill = SLAB_HOVER;
        widgets.active.weak_bg_fill = STRIPE;
        let width = ui.available_width();
        ui.add_sized([width, 36.0], egui::Button::new(emphasis(text)))
    })
    .inner
}

/// A command reduced to its verb over a hairline: no fill, no border, the same
/// typographic register as the tab strip.
///
/// What `Stop` gets while a run is live. Stopping is rare and deliberate, so it
/// needs to be reachable and legible, not loud — and a word on a rule cannot
/// out-shout the figure it sits beside.
///
/// The rule is painted rather than a text underline: an underline takes the
/// text's colour, which would put a full-ink line under a word that is
/// deliberately quiet.
pub(super) fn bare_verb(ui: &mut egui::Ui, text: &str) -> egui::Response {
    // Its own padding, well under the theme's `SP_MD`/`SP_SM` button box: that
    // box is sized for a filled control, and here it would hang the rule eight
    // pixels below the word and stretch it a dozen either side — an underline
    // that clearly belongs to something else.
    let response = ui.scope(|ui| {
        ui.spacing_mut().button_padding = egui::vec2(2.0, SP_XS);
        ui.add(egui::Button::new(text).frame(false))
    });
    let response = response.inner;
    let rule = if response.hovered() { INK } else { VERB_RULE };
    let rect = response.rect;
    ui.painter().hline(
        rect.x_range(),
        rect.bottom() - 1.0,
        Stroke::new(1.0, if ui.is_enabled() { rule } else { HAIRLINE }),
    );
    response
}

/// A full-bleed 2px gauge: how far the run has gone towards the limit that will
/// stop it. Painted past the side margin like [`rule`], whose geometry it
/// borrows and whose job it also does — it closes the status band against the
/// tab strip, which is why an unbounded run falls back to a plain rule rather
/// than leaving the edge off.
///
/// The fill is [`INK_MUTED`] and not an accent: a gauge that fills means "this
/// is about to stop", which is a fact and not an alarm.
pub(super) fn gauge(ui: &mut egui::Ui, ratio: f32) {
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 2.0), egui::Sense::hover());
    let track = ui.clip_rect().x_range();
    let y = rect.center().y;
    ui.painter().hline(track, y, Stroke::new(2.0, HAIRLINE));
    // Clamped here as well as at the source: this is the call that turns a
    // number into pixels, and a stray ratio would paint outside the panel.
    let filled = track.min + track.span() * ratio.clamp(0.0, 1.0);
    if filled > track.min {
        ui.painter().hline(
            egui::Rangef::new(track.min, filled),
            y,
            Stroke::new(2.0, INK_MUTED),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn section_headers_render_as_capitals() {
        assert_eq!(section("Quick start").text(), "QUICK START");
    }
}
