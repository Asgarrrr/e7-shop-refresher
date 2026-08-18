//! The bottom journal: a collapsible title bar (with a latest-line peek while
//! collapsed) over the scrolling decision log.

use eframe::egui;

use crate::journal::LogLine;

use super::theme;

/// The journal's title bar: a caret (right = closed, down = open), the
/// "JOURNAL" label, and — while collapsed — a muted peek of the most recent
/// line so "what just happened" shows without expanding. Returns true on click.
///
/// One interactive widget spans the whole bar: while collapsed the hit + hover
/// area grows into the panel's inner `margin` so the *entire* bar toggles (the
/// panel's content ui inherits the panel's outer clip rect, so this isn't
/// clipped away). The hover fill is the full symmetric bar in both states so
/// the text stays centred; open, the click area drops the top margin only,
/// leaving the panel's top edge free for the resize handle. It carries the
/// accessible name (peek folded in).
/// The content is *painted*, not nested widgets — a nested interactive label
/// would steal hover over the text. egui's custom-widget idiom: allocate +
/// sense + `widget_info` + paint.
pub(super) fn render_journal_header(
    ui: &mut egui::Ui,
    open: bool,
    latest: Option<&str>,
    margin: egui::Margin,
) -> bool {
    let (content, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 22.0), egui::Sense::hover());
    let ml = f32::from(margin.left);
    let mt = f32::from(margin.top);
    // The hover fill is the full symmetric bar (top margin + text row + bottom
    // margin) in both states, so the text sits centred. The caller reserves
    // that same bottom margin below the header, so open the fill lands in real
    // space, not over the log. `hit` (what toggles) drops the top margin only
    // when open, leaving the panel's resize handle reachable.
    let highlight = content.expand2(egui::vec2(ml, mt));
    let hit = if open {
        egui::Rect::from_min_max(
            egui::pos2(highlight.left(), content.top()),
            highlight.right_bottom(),
        )
    } else {
        highlight
    };
    let response = ui.interact(hit, ui.id().with("journal_toggle"), egui::Sense::click());

    // The peek rides in the accessible name too, so assistive tooling (and the
    // tests) read the latest line, not just "Journal". Built inside the closure
    // — egui only calls it when AccessKit is live, a harness is reading, or the
    // bar was just clicked; outside, the `format!` would be paid every frame.
    let enabled = ui.is_enabled();
    response.widget_info(|| {
        let name = if !open && let Some(latest) = latest {
            format!("Journal · {latest}")
        } else {
            "Journal".to_owned()
        };
        egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, &name)
    });

    let hovered = response.hovered();
    let tint = if hovered {
        theme::INK_MUTED
    } else {
        theme::INK_FAINT
    };
    let bg = if hovered {
        theme::STRIPE
    } else {
        ui.visuals().panel_fill
    };
    let font = egui::TextStyle::Small.resolve(ui.style());

    // Highlight the full header bar; keep the text clipped to the content row
    // so the peek never spills into the right margin.
    if hovered {
        ui.painter().with_clip_rect(highlight).rect_filled(
            highlight,
            egui::CornerRadius::same(5),
            theme::STRIPE,
        );
    }
    let painter = ui.painter().with_clip_rect(content);
    theme::caret(&painter, content, open, tint);
    let title = painter.text(
        egui::pos2(content.left() + 20.0, content.center().y),
        egui::Align2::LEFT_CENTER,
        "JOURNAL",
        font.clone(),
        tint,
    );
    if !open && let Some(latest) = latest {
        painter.text(
            egui::pos2(title.right() + theme::SP_SM, content.center().y),
            egui::Align2::LEFT_CENTER,
            latest,
            font,
            theme::INK_FAINT,
        );
        // A soft fade to the background at the right edge instead of a hard cut
        // — egui has no cheap blur, but a transparent→bg gradient reads soft.
        paint_edge_fade(&painter, content, bg);
    }
    response.clicked()
}

/// The scrolling log body (no header): rendered only while the journal is open.
pub(super) fn render_journal_body(ui: &mut egui::Ui, journal: &[LogLine]) {
    // show_rows lays out only the visible slice; its uniform-row math
    // requires exactly one visual row per entry, so long lines extend into
    // a horizontal scroll instead of wrapping.
    let row_height = ui.text_style_height(&egui::TextStyle::Monospace);
    egui::ScrollArea::both()
        .id_salt("journal")
        .stick_to_bottom(true)
        .auto_shrink([false, false])
        .show_rows(ui, row_height, journal.len(), |ui, rows| {
            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
            // One buffer for the whole visible slice: the stamp is written into
            // it rather than returned as its own `String`, so a row costs the one
            // copy `RichText` makes and nothing else.
            let mut row = String::with_capacity(64);
            for line in &journal[rows] {
                row.clear();
                write_timestamp(&mut row, line.at_ms);
                row.push_str("  ");
                row.push_str(&line.text);
                ui.monospace(row.as_str());
            }
        });
}

/// A horizontal gradient (transparent → `bg`) over the right edge of `row`, so
/// overflowing text dissolves into the background rather than being clipped
/// hard. Cheap stand-in for a blur.
fn paint_edge_fade(painter: &egui::Painter, row: egui::Rect, bg: egui::Color32) {
    let fade = egui::Rect::from_min_max(
        egui::pos2(row.right() - 40.0, row.top()),
        row.right_bottom(),
    );
    let clear = egui::Color32::from_rgba_unmultiplied(bg.r(), bg.g(), bg.b(), 0);
    let mut mesh = egui::Mesh::default();
    mesh.colored_vertex(fade.left_top(), clear);
    mesh.colored_vertex(fade.left_bottom(), clear);
    mesh.colored_vertex(fade.right_top(), bg);
    mesh.colored_vertex(fade.right_bottom(), bg);
    mesh.indices.extend_from_slice(&[0, 1, 2, 2, 1, 3]);
    painter.add(mesh);
}

/// Session-relative `+m:ss` (hours appear once the session runs that long),
/// appended to the caller's buffer so a visible row needs no `String` of its own.
fn write_timestamp(out: &mut String, at_ms: u64) {
    use std::fmt::Write as _;

    let secs = at_ms / 1000;
    let (hours, minutes, seconds) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    // Writing to a `String` is infallible.
    let _ = if hours > 0 {
        write!(out, "+{hours}:{minutes:02}:{seconds:02}")
    } else {
        write!(out, "+{minutes}:{seconds:02}")
    };
}

#[cfg(test)]
mod tests {
    use egui_kittest::{Harness, kittest::Queryable};

    use super::*;

    fn timestamp(at_ms: u64) -> String {
        let mut out = String::new();
        write_timestamp(&mut out, at_ms);
        out
    }

    #[test]
    fn timestamp_rolls_into_hours() {
        assert_eq!(timestamp(59_000), "+0:59");
        assert_eq!(timestamp(61_000), "+1:01");
        assert_eq!(timestamp(3_661_000), "+1:01:01");
    }

    #[test]
    fn journal_lines_render_with_timestamps() {
        let lines = vec![LogLine {
            at_ms: 61_000,
            text: "refresh advised".into(),
        }];
        let harness = Harness::new_ui(|ui| render_journal_body(ui, &lines));
        harness.get_by_label("+1:01  refresh advised");
    }

    #[test]
    fn journal_header_click_reports_a_toggle() {
        // The header toggles the panel open/closed; the caller flips its
        // `journal_open` on a reported click. The whole row is the single
        // interactive widget, named "Journal".
        // `Harness::new_ui` takes `impl FnMut`, so the flag is captured mutably;
        // `drop(harness)` releases the borrow before the assert reads it.
        let mut toggled = false;
        let mut harness = Harness::new_ui(|ui| {
            if render_journal_header(ui, false, None, egui::Margin::symmetric(16, 10)) {
                toggled = true;
            }
        });
        harness.get_by_label("Journal").click();
        harness.run();
        drop(harness);
        assert!(toggled);
    }

    #[test]
    fn journal_header_peeks_latest_only_while_collapsed() {
        // Collapsed: the most recent line rides in the accessible name so the
        // player (and assistive tooling) sees it without expanding. Open: the
        // log below carries everything, so the name drops back to "Journal".
        let margin = egui::Margin::symmetric(16, 10);
        let collapsed = Harness::new_ui(|ui| {
            render_journal_header(ui, false, Some("bought a thing"), margin);
        });
        collapsed.get_by_label("Journal · bought a thing");

        let open = Harness::new_ui(|ui| {
            render_journal_header(ui, true, Some("bought a thing"), margin);
        });
        assert!(open.query_by_label("Journal · bought a thing").is_none());
    }
}
