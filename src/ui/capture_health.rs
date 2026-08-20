//! The capture-health readout: turns the funnel and pipeline counters
//! `crate::capture::CaptureCounters` and `crate::stream::PipelineBudget`
//! already compute into the one sentence their own doc comments spell out,
//! instead of leaving a stuck player to set `RUST_LOG=debug`, restart, and
//! read a rotated log file.
//!
//! Rendered under the status bar's own content, gated on the same "a run
//! exists" condition the stat tiles already use (see `ShopApp::ui`) — while
//! Idle every counter here is a meaningless zero, not a diagnosis.

use eframe::egui;

use super::theme;

/// One frame's capture-health snapshot: two atomics reads
/// (`PacketSource::counters`) and two fields out of one short-lived-lock
/// snapshot (`PipelineBudget::snapshot`), copied under `Copy` so nothing here
/// holds either past the read. See `ShopApp::ui` for where the two are read
/// and folded into this.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct CaptureHealthView {
    pub delivered: u64,
    pub unparsed: u64,
    pub admitted: u64,
    pub dropped_segments: u64,
    pub resyncs: u64,
}

/// The diagnosis `capture::pcap::Funnel`'s own doc comment already spells
/// out, rendered as the one sentence a stuck player needs instead of the
/// numbers behind it.
///
/// `delivered == 0` covers two situations the counters alone cannot tell
/// apart: a backend that never counts anything, and one that has not
/// captured a packet yet (`PacketSource::counters`'s default is the same all
/// zero). Both get this same unalarmed wording rather than a diagnosis that
/// presumes a fault — an honest "nothing yet" beats a false "something is
/// broken".
pub(super) fn diagnosis(view: &CaptureHealthView) -> &'static str {
    if view.delivered == 0 {
        "no traffic has reached this process yet — the adapters are open, \
         but nothing has matched the capture filter"
    } else if view.admitted == 0 {
        // `unparsed` is climbing in lockstep with `delivered` here — the
        // same symptom `capture::pcap::link`'s untested VLAN strip names for
        // its own failure mode, now visible without a debug build.
        "traffic is being captured, but none of it looks like the game's"
    } else if view.dropped_segments > 0 || view.resyncs > 0 {
        "capture is healthy, but the byte stream dropped and had to resync \
         — a slow connection or a driver hiccup, not a filter problem"
    } else {
        "capture looks healthy"
    }
}

/// The compact row: the sentence above, with the raw counts a click away for
/// whoever is asked to paste them into a bug report. Never redesigns the
/// status bar's own content — this is new content appended after it.
pub(super) fn render_capture_health(ui: &mut egui::Ui, view: &CaptureHealthView) {
    ui.horizontal_wrapped(|ui| {
        ui.label(theme::section("Capture"));
        ui.label(diagnosis(view));
    });
    egui::CollapsingHeader::new("packet counts")
        .id_salt("capture_health_counts")
        .show(ui, |ui| {
            ui.monospace(format!(
                "packets seen {}   not parsed {}   forwarded {}   dropped {}   resyncs {}",
                view.delivered, view.unparsed, view.admitted, view.dropped_segments, view.resyncs
            ));
        });
}

#[cfg(test)]
mod tests {
    use egui_kittest::{Harness, kittest::Queryable};

    use super::*;

    #[test]
    fn zero_delivered_reads_as_no_traffic_yet() {
        let view = CaptureHealthView::default();
        let harness = Harness::new_ui(|ui| render_capture_health(ui, &view));
        harness.get_by_label(diagnosis(&view));
        assert!(
            diagnosis(&view).contains("yet"),
            "an unstarted or silent backend must not read as broken"
        );
    }

    #[test]
    fn delivered_without_admitted_reads_as_not_the_games_traffic() {
        let view = CaptureHealthView {
            delivered: 500,
            unparsed: 500,
            admitted: 0,
            ..CaptureHealthView::default()
        };
        let harness = Harness::new_ui(|ui| render_capture_health(ui, &view));
        harness.get_by_label("traffic is being captured, but none of it looks like the game's");
    }

    #[test]
    fn healthy_capture_with_no_drops_says_so() {
        let view = CaptureHealthView {
            delivered: 500,
            unparsed: 1,
            admitted: 499,
            dropped_segments: 0,
            resyncs: 0,
        };
        let harness = Harness::new_ui(|ui| render_capture_health(ui, &view));
        harness.get_by_label("capture looks healthy");
    }

    #[test]
    fn drops_and_resyncs_are_named_even_while_admitting_traffic() {
        let view = CaptureHealthView {
            delivered: 500,
            unparsed: 1,
            admitted: 499,
            dropped_segments: 3,
            resyncs: 1,
        };
        let harness = Harness::new_ui(|ui| render_capture_health(ui, &view));
        harness.get_by_label(diagnosis(&view));
        assert!(diagnosis(&view).contains("resync"));
    }

    /// The trait's defaulted `counters()` (a backend that counts nothing) and
    /// a session that has genuinely captured zero packets are the same
    /// `CaptureCounters::default()` value, and must render the same honest
    /// sentence — not four zeros dressed up as a fault.
    #[test]
    fn the_defaulted_backend_renders_the_same_honest_sentence_as_true_silence() {
        let defaulted = crate::capture::CaptureCounters::default();
        let view = CaptureHealthView {
            delivered: defaulted.delivered,
            unparsed: defaulted.unparsed,
            admitted: defaulted.admitted,
            ..CaptureHealthView::default()
        };
        assert_eq!(view, CaptureHealthView::default());
        let harness = Harness::new_ui(|ui| render_capture_health(ui, &view));
        harness.get_by_label(diagnosis(&view));
    }
}
