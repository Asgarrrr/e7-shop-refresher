//! The bottom journal: a collapsible title bar (with a latest-line peek while
//! collapsed) over the scrolling decision log.

use eframe::egui;

use crate::journal::LogLine;

use super::theme;

/// The journal's title bar, with a peek of the most recent line while
/// collapsed. Returns true on click.
///
/// One interactive widget spans the whole bar: collapsed, the hit area grows
/// into the panel's `margin` so all of it toggles; open, it drops the top
/// margin only, leaving the resize handle reachable. Painted, not nested
/// widgets — a nested interactive label would steal hover over the text.
pub(super) fn render_journal_header(
    ui: &mut egui::Ui,
    open: bool,
    latest: Option<&str>,
    margin: egui::Margin,
) -> bool {
    // The peek reads the same convention the body strips — a collapsed
    // journal showing a raw `">> "` would be the one place the marker survived.
    let latest = latest.map(|line| strip_marker(line).1);
    let (content, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 22.0), egui::Sense::hover());
    let ml = f32::from(margin.left);
    let mt = f32::from(margin.top);
    // The caller reserves this bottom margin, so the open fill lands in real
    // space and not over the log.
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

    // Built inside the closure: outside, the `format!` would be paid every
    // frame rather than only when something reads the widget info.
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

    // The text stays clipped to the content row so the peek never spills into
    // the right margin.
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
        paint_edge_fade(&painter, content, bg);
    }
    response.clicked()
}

/// Every producer prefixes an event line with `">> "` and, under a MATCH,
/// its detail lines with three spaces — see the modules under `src/app` and
/// `src/actuator`. This is the one place that convention is read back: the
/// marker becomes color and indent instead of literal characters, so the
/// domain wording never has to know how the window ends up drawing it.
///
/// Returns whether `text` was a detail line, and the text with its marker
/// removed.
fn strip_marker(text: &str) -> (bool, &str) {
    if let Some(detail) = text.strip_prefix("   ") {
        (true, detail)
    } else {
        (false, text.strip_prefix(">> ").unwrap_or(text))
    }
}

/// The color an event line paints, from the level [`crate::journal::EventLog`]
/// stamped it with. Reuses the visuals `theme::apply` already set for the
/// error banner and the status bar, rather than a palette of its own.
fn severity_color(ui: &egui::Ui, level: tracing::Level) -> egui::Color32 {
    match level {
        tracing::Level::ERROR => ui.visuals().error_fg_color,
        tracing::Level::WARN => ui.visuals().warn_fg_color,
        _ => theme::INK_MUTED,
    }
}

/// The scrolling log body, rendered only while the journal is open.
pub(super) fn render_journal_body(ui: &mut egui::Ui, journal: &[LogLine]) {
    // `show_rows` lays out only the visible slice, and its uniform-row math
    // requires exactly one visual row per entry — so long lines extend into a
    // horizontal scroll instead of wrapping.
    let row_height = ui.text_style_height(&egui::TextStyle::Monospace);
    let font = egui::TextStyle::Monospace.resolve(ui.style());
    egui::ScrollArea::both()
        .id_salt("journal")
        .stick_to_bottom(true)
        .auto_shrink([false, false])
        .show_rows(ui, row_height, journal.len(), |ui, rows| {
            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
            let mut stamp = String::with_capacity(16);
            for line in &journal[rows] {
                stamp.clear();
                write_timestamp(&mut stamp, line.at_ms);
                stamp.push_str("  ");
                let (detail, body) = strip_marker(&line.text);
                let color = if detail {
                    theme::INK_FAINT
                } else {
                    severity_color(ui, line.level)
                };
                // One job per row, timestamp and body in the same monospace
                // font so the columns still line up, but only the body takes
                // the severity color — a warning is a warning, its stamp
                // isn't.
                let mut job = egui::text::LayoutJob::default();
                job.append(
                    &stamp,
                    0.0,
                    egui::TextFormat::simple(font.clone(), theme::INK_FAINT),
                );
                if detail {
                    // Two more spaces than a plain line, so a MATCH's targets
                    // read as nested under it rather than merely dimmer.
                    job.append("  ", 0.0, egui::TextFormat::simple(font.clone(), color));
                }
                job.append(body, 0.0, egui::TextFormat::simple(font.clone(), color));
                ui.label(job);
            }
        });
}

/// A horizontal gradient (transparent → `bg`) over the right edge of `row`, so
/// overflowing text dissolves into the background rather than being clipped
/// hard. egui has no cheap blur; this reads soft enough.
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

/// Session-relative `+m:ss`, appended to the caller's buffer so a visible row
/// needs no `String` of its own.
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

    fn line(at_ms: u64, text: &str, level: tracing::Level) -> LogLine {
        LogLine {
            at_ms,
            text: text.into(),
            level,
        }
    }

    #[test]
    fn timestamp_rolls_into_hours() {
        assert_eq!(timestamp(59_000), "+0:59");
        assert_eq!(timestamp(61_000), "+1:01");
        assert_eq!(timestamp(3_661_000), "+1:01:01");
    }

    #[test]
    fn journal_lines_render_with_timestamps() {
        let lines = vec![line(61_000, "refresh advised", tracing::Level::INFO)];
        let harness = Harness::new_ui(|ui| render_journal_body(ui, &lines));
        harness.get_by_label("+1:01  refresh advised");
    }

    #[test]
    fn the_event_marker_is_stripped_not_shown() {
        let lines = vec![line(0, ">> watching — open the shop", tracing::Level::INFO)];
        let harness = Harness::new_ui(|ui| render_journal_body(ui, &lines));
        harness.get_by_label("+0:00  watching — open the shop");
    }

    #[test]
    fn a_match_detail_line_keeps_its_indent_once_the_marker_is_gone() {
        let lines = vec![line(
            0,
            "   Reforged Sword · 250,000g",
            tracing::Level::INFO,
        )];
        let harness = Harness::new_ui(|ui| render_journal_body(ui, &lines));
        // Two more spaces than a plain line's single gap after the stamp —
        // the indent that used to be the three literal spaces themselves.
        harness.get_by_label("+0:00    Reforged Sword · 250,000g");
    }

    #[test]
    fn journal_header_click_reports_a_toggle() {
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

    #[test]
    fn the_peek_strips_the_event_marker_too() {
        let margin = egui::Margin::symmetric(16, 10);
        let collapsed = Harness::new_ui(|ui| {
            render_journal_header(ui, false, Some(">> bought a thing"), margin);
        });
        collapsed.get_by_label("Journal · bought a thing");
    }
}
