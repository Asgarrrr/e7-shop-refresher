//! The capture-health readout: turns the funnel and pipeline counters
//! `crate::capture::CaptureCounters` and `crate::stream::PipelineBudget`
//! already compute into the one sentence their own doc comments spell out,
//! instead of leaving a stuck player to set `RUST_LOG=debug`, restart, and
//! read a rotated log file.
//!
//! Rendered at the top of the **open journal panel**, not in the status bar.
//! The status bar is what a player looks at while a run is going well, so a
//! diagnostic readout standing in it permanently reads as a running verdict on
//! a session that is fine. The journal is the panel someone opens to find out
//! what is happening, and it is already collapsed by default — which makes its
//! disclosure the honest gate for everything here.
//!
//! Also gated on the same "a run exists" condition the status bar's haul row
//! uses (see `ShopApp::ui`): while Idle every counter here is a meaningless
//! zero, not a diagnosis. And bounded to the run the player is looking at
//! rather than to the process — see [`capture_view`], which is what makes the
//! amber below mean "now" instead of "at some point since launch".
//!
//! This module renders a cause; it does not work one out. Every re-anchor is
//! attributed where it happens, by the thread that saw it, and arrives here as
//! a [`ResyncCause`]. It used to be the other way round — one `resyncs` total,
//! and a sentence here that guessed — and the guess was wrong for four of the
//! seven causes there were then.
//!
//! Nor does it decide which re-anchors are worth a player's attention. Not every
//! one is: the stream table used to fill with the game's own closed connections,
//! re-anchoring flows that had nothing left to deliver — a patched build logged
//! 46 of those in ~90 s. `Reassembler` has since stopped producing them by
//! retiring a flow when its connection ends, and `PipelineStats::dominant_resync`
//! keeps whatever is left of them out of [`CaptureHealthView::dominant_resync`],
//! so both the sentence and the amber below are already only about faults, and
//! neither re-states the rule.

use eframe::egui;

use crate::capture::CaptureCounters;
use crate::stream::{PipelineStats, ResyncCause, RunBaseline};

use super::theme;

/// One frame's capture-health snapshot: the two atomics reads
/// (`PacketSource::counters`) and pipeline fields (`PipelineBudget::snapshot`)
/// that the sentence in [`diagnosis`] actually branches on, copied under
/// `Copy` so nothing here holds either past the read. [`capture_view`] folds the
/// two into this, once per frame.
///
/// Only what the sentence needs, on purpose: this used to also carry
/// `unparsed`, `dropped_bytes` and a raw `resyncs` count for a table of
/// numbers rendered beneath the sentence. The table was cut — a player who
/// opens the journal to find out what is wrong gets an answer already worked
/// out for them, not four counters to interpret themselves — and every field
/// that existed only to fill a table cell went with it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct CaptureHealthView {
    pub delivered: u64,
    pub admitted: u64,
    pub dropped_segments: u64,
    /// What caused the dominant *costly* re-anchor of **this run**, already
    /// resolved by `PipelineStats::since`. Resolved there and carried here, so
    /// both the tie-break rule and the "housekeeping does not vote" rule live
    /// once, beside the counters they read. `None` therefore means "no fault",
    /// not "no re-anchor".
    pub dominant_resync: Option<ResyncCause>,
}

/// Folds one frame's two counter sources into the view, over the run `baseline`
/// bounds — and, by subtraction, out of the counters this readout must not
/// describe.
///
/// `PipelineBudget` counts for the *process*: `app::setup` builds it once, a
/// clone rides in `SessionHandles` for the window's whole life, and nothing
/// resets it, deliberately, because the same structure guards the live memory
/// leases. So a single re-anchor — one funnel overflow lasting milliseconds —
/// used to word this sentence and light it amber for the rest of the process.
/// A stop does not clear it either: `Command::Stop` disarms the `WatchGate` and
/// leaves the pipeline standing, so the next run inherited the previous one's
/// fault and a session healthy for an hour still read as broken. The fix is a
/// subtraction, not a reset; the budget's lifetime and byte accounting are
/// untouched.
///
/// Nothing here detects the edge that starts a run, because nothing here can.
/// The counters move on the capture thread and the gate opens on the session
/// loop, both ahead of a frame that repaints at 4 Hz — so a frame that took its
/// own baseline took it from counters that could already carry this run's first
/// re-anchor, and reported "capture looks healthy" over a stream that had
/// re-anchored. `app::session::SessionGate::arm` publishes the baseline on the
/// read-modify-write that opens the gate instead; see `stream::RunBaselineCell`.
///
/// `delivered` and `admitted` are deliberately not rebased. They come from
/// `CaptureHealth`, which counts what the tap sees, and the tap runs from launch
/// rather than from Start (see `app::ingest`); the two arms that read them ask
/// whether the game's traffic is visible at all, which no stop and no start
/// changes.
pub(super) fn capture_view(
    counters: &CaptureCounters,
    pipeline: &PipelineStats,
    baseline: RunBaseline,
) -> CaptureHealthView {
    let run = pipeline.since(baseline);
    CaptureHealthView {
        delivered: counters.delivered,
        admitted: counters.admitted,
        dropped_segments: run.dropped_segments,
        // Resolved by the snapshot that owns the tie-break rule rather than
        // re-derived here — the habit that produced a sentence naming the
        // wrong cause — and asked of this run's counts, not the process's.
        dominant_resync: run.dominant_resync(),
    }
}

/// The one sentence a stuck player needs instead of the numbers behind it —
/// and, while nothing is wrong, a plain statement of what capture is doing.
///
/// # Why the two "nothing yet" arms state rather than advise
///
/// This row appears the moment a session is armed, and the ordinary way a run
/// begins is: press Start in the lobby, then walk to the Secret Shop. Both
/// arms describe that walk, and both used to state it as a finding — the
/// second not merely alarming but wrong. An idle game keeps its connection to
/// `game_port` alive and the kernel filter admits all of it, so its keepalive
/// ACKs reach `parse_segment` and are refused for carrying no stream bytes:
/// `unparsed` climbs, `admitted` stays at zero, and the row announced that
/// none of this looked like the game's traffic when every byte of it was.
///
/// The instruction that *does* belong to this moment is already given, once,
/// where an instruction belongs — `app::session`'s `>> watching — open the
/// shop`, emitted on the transition into `Watching`. A journal line is stamped
/// into a timeline and reads as "this happened now"; a status row is read as
/// "this is true, still". Repeating the advice here would turn it into a
/// standing verdict on a session that is behaving perfectly.
///
/// So these two sentences say only what is so. What to check *if the shop is
/// already open* is real, has nowhere else to live, and would be a false alarm
/// if shown unprompted — it goes to [`detail`], which renders only once the
/// journal is open, where a player arrives by going looking. No timer decides
/// between the two readings, because none could: nothing here can know whether
/// the player is in the shop, and a session may sit armed for ten minutes
/// before they open it.
///
/// # What `admitted` counts, and what that costs the second arm
///
/// "No shop data has arrived yet" is not what `admitted == 0` measures, and
/// never was: `admitted` counts every frame `parse_segment` turned into a
/// segment, and a zero-payload SYN-ACK is one — `capture::pcap`'s
/// `first server-to-client segment admitted` line has been observed reading
/// `payload=0 syn=true`. Since 2026-08-22 FIN and RST are admitted too, so that
/// the reassembler can retire a closed flow instead of holding its slot.
///
/// That widening only ever moves a session *out* of this arm, and only for the
/// benign reading of it. `admitted` cannot decrease, and the fault reading — a
/// link strip this adapter defeats, which [`detail`] names — is a session in
/// which nothing parses at all, so no close is admitted there either. What is
/// left in the arm is therefore slightly more specific to the fault than it was,
/// not less; what it loses is a player whose game opened and closed a connection
/// before they reached the shop, who now reads "capture looks healthy". That is
/// also true of them.
///
/// `delivered == 0` also covers two situations the counters cannot tell apart:
/// a backend that never counts anything, and one that has not captured a packet
/// yet (`PacketSource::counters`'s default is the same all zero). Both get the
/// same unalarmed wording — an honest "nothing yet" beats a false "something is
/// broken".
pub(super) fn diagnosis(view: &CaptureHealthView) -> &'static str {
    if view.delivered == 0 {
        "waiting for the game's traffic"
    } else if view.admitted == 0 {
        "the game's connection is visible, but no shop data has arrived yet"
    } else if let Some(cause) = view.dominant_resync {
        resync_sentence(cause)
    } else if view.dropped_segments > 0 {
        // Drops with no re-anchor behind them are one specific, benign thing:
        // `app::ingest` discarding the bytes that arrive while the player's own
        // "watch just armed" marker is still waiting for a queue slot. Those
        // bytes belong to the epoch that marker discards anyway. Every other
        // drop in the pipeline records a cause alongside it and is answered by
        // the arm above, so this must not borrow that arm's alarm.
        "capture is healthy — a few bytes were discarded while the watch was \
         being armed, which costs nothing"
    } else {
        "capture looks healthy"
    }
}

/// What to tell the player about `cause`, in the terms of what they could
/// actually do about it.
///
/// The split that matters is not one way per cause: it is whether the machine
/// was outrun by the wire, whether this process was outrun by itself, or whether
/// a packet is simply gone. The old single sentence asserted the first for all
/// three, and sent players with a busy PC to go and look at their router.
///
/// The match is exhaustive over [`ResyncCause`] because the type is, not because
/// every variant can arrive: `PipelineStats::dominant_resync` never yields a
/// housekeeping cause, so `StreamReclaimed`'s arm is an obligation the compiler
/// imposes and `a_run_of_lossless_evictions_reads_as_healthy` pins shut. It is
/// worded truthfully all the same — an arm nobody checks is how a wrong sentence
/// survives a rename.
const fn resync_sentence(cause: ResyncCause) -> &'static str {
    match cause {
        ResyncCause::DriverRing => {
            "capture is healthy, but the capture driver dropped packets and the stream \
             re-anchored — this machine could not keep up with the traffic"
        }
        ResyncCause::CaptureFunnel => {
            "capture is healthy, but this app fell behind its own capture queue and the \
             stream re-anchored — a busy machine, not a network problem"
        }
        ResyncCause::ByteQuota => {
            "capture is healthy, but the pipeline's memory budget filled and the stream \
             re-anchored — bytes arrived faster than the server link drained them"
        }
        ResyncCause::MetadataQueue => {
            "capture is healthy, but reassembly fell behind the capture thread and the \
             stream re-anchored — bytes arrived faster than they could be handled"
        }
        ResyncCause::ReassemblyStream => {
            "capture is healthy, but a gap in the game's byte stream never filled and it \
             re-anchored — a lost packet a passive tap can never be shown again"
        }
        ResyncCause::ReassemblyShared => {
            "capture is healthy, but the reassembly buffer filled up waiting on gaps and \
             every connection re-anchored — traffic on the game's port outran this machine"
        }
        // Not "other connections on the game's port": the kernel filter admits
        // the game server's own source port and nothing else, so the flows that
        // filled the table are the game's own earlier connections. The old
        // wording sent a player hunting for foreign traffic that cannot exist.
        ResyncCause::StreamEvicted => {
            "capture is healthy, but the game's own past connections filled the stream table \
             and a flow still holding part of a message re-anchored — those bytes are gone"
        }
        ResyncCause::StreamReclaimed => {
            "capture is healthy — a finished connection's slot was reused for a new one, and \
             nothing was waiting in it"
        }
        // Nothing about the stream table, deliberately: an abort is the server
        // hanging up, and the eviction sentence above would send a player after
        // a table that had no part in it.
        ResyncCause::ConnectionReset => {
            "capture is healthy, but the game's server cut a connection off while part of a \
             message was still waiting on a lost packet — those bytes are gone"
        }
    }
}

/// What to check when [`diagnosis`]'s statement is *not* the ordinary case —
/// or `None`, when the sentence is the whole story.
///
/// This is the half of a diagnosis that has no honest place on a status row.
/// Both states it covers are, overwhelmingly, a player who has not opened the
/// shop yet; both can also be a real misconfiguration, and no counter here can
/// separate them (`capture::link`'s doc says the same from the other side).
/// Shown unprompted it would be a false alarm on nearly every session, and
/// withheld entirely it would leave the one player who *is* misconfigured with
/// a row that says "waiting" forever.
///
/// Inside the journal it is neither: nobody opens that panel except to find
/// out why something is not working, so the reader has already supplied the
/// context — "the shop is open and this is still what it says" — that the app
/// itself cannot observe.
const fn detail(view: &CaptureHealthView) -> Option<&'static str> {
    if view.delivered == 0 {
        Some(
            "Expected until the shop is open. If it already is, `game_port` in config.toml \
             is wrong.",
        )
    } else if view.admitted == 0 {
        Some(
            "Expected while the shop is closed. If it is already open, this adapter's \
             framing is not being stripped — a VLAN tag is the known case.",
        )
    } else {
        None
    }
}

/// The sentence's colour: primary ink for every ordinary state, and the theme's
/// amber only when a re-anchor actually cost the run something.
///
/// One condition, [`CaptureHealthView::dominant_resync`], and it is the same one
/// [`diagnosis`] branches on — so the colour and the sentence cannot disagree
/// about whether anything is wrong. It reads `is_some()` and nothing else
/// because the filtering already happened in `PipelineStats::dominant_resync`;
/// a second opinion here is how the two would drift.
///
/// Full ink because this is the block's message, and the [`theme::INK_FAINT`]
/// note below it — where there is one — is what supports it, not the other
/// way round.
///
/// Amber is the one accent, and it carries information rather than decorating.
/// A readout that is loud whatever it says trains the eye to skip it, which is
/// the same failure as a sentence that always claims a fault — the reader stops
/// reading it either way.
fn diagnosis_color(view: &CaptureHealthView) -> egui::Color32 {
    if view.dominant_resync.is_some() {
        theme::AMBER
    } else {
        theme::INK
    }
}

/// The whole readout: the sentence, and the note behind it where there is one.
///
/// Flat, with no disclosure of its own. It had one while it lived in the status
/// bar, because a permanently visible strip had to hide its detail; inside a
/// panel that is itself collapsed by default, a second nested toggle only asks
/// the player to click twice for something they already opened the journal to
/// find. One gate, and it is the journal's.
pub(super) fn render_capture_health(ui: &mut egui::Ui, view: &CaptureHealthView) {
    // Tighter than the panel's rhythm, the same step `statusbar::balances_strip`
    // takes: these rows are one readout, and the default gap between them reads
    // as separate things stacked.
    ui.spacing_mut().item_spacing.y = theme::SP_XS;
    // Plain text, and nothing beside it. No "CAPTURE" heading, because a label
    // naming the only thing in a region is chrome; and no status dot either,
    // because the sentence already says what the dot would have coloured, and a
    // glyph repeating it is ornament rather than information. What the line
    // needed was weight of its own, not company: `emphasis` is the size the
    // status bar gives its own status word, so the two headline the same way.
    ui.label(theme::emphasis(diagnosis(view)).color(diagnosis_color(view)));
    if let Some(detail) = detail(view) {
        ui.weak(detail);
    }
}

#[cfg(test)]
mod tests {
    use egui_kittest::{Harness, kittest::Queryable};

    use crate::stream::{PipelineBudget, RunBaselineCell};

    use super::*;

    /// A live budget and the cell the arming edge publishes into: the two halves
    /// `SessionHandles` carries, in the state they are in when the window opens.
    fn tracked_budget() -> (PipelineBudget, RunBaselineCell) {
        let budget = PipelineBudget::new();
        let run = RunBaselineCell::new(budget.clone());
        (budget, run)
    }

    /// What `app::session::SessionGate::arm` does when the gate opens, without
    /// the gate: read the counters, then make that read the run's zero. Spelled
    /// out rather than hidden behind one method, because the two steps straddle
    /// the store that opens the gate and only the session may perform them.
    fn arm(run: &RunBaselineCell) {
        run.publish(run.counters_now());
    }

    /// A tap that is seeing the game's shop traffic, so a verdict is what
    /// [`diagnosis`] answers with rather than one of the two "nothing yet" arms.
    /// The same counts [`resynced`] carries.
    fn traffic() -> CaptureCounters {
        CaptureCounters {
            delivered: 500,
            admitted: 499,
            ..CaptureCounters::default()
        }
    }

    /// A healthy session that then re-anchored `times` times for `cause`.
    fn resynced(cause: ResyncCause, times: u64) -> CaptureHealthView {
        CaptureHealthView {
            delivered: 500,
            admitted: 499,
            dropped_segments: times,
            dominant_resync: Some(cause),
        }
    }

    /// Nothing captured, and the game's connection visible but silent: the two
    /// states that make up the ordinary first seconds of a run, when Start has
    /// been pressed in the lobby and the player is walking to the Secret Shop.
    fn pre_shop_states() -> [CaptureHealthView; 2] {
        [
            CaptureHealthView::default(),
            CaptureHealthView {
                delivered: 500,
                admitted: 0,
                ..CaptureHealthView::default()
            },
        ]
    }

    /// Both used to read as findings about that walk — "nothing has matched the
    /// capture filter", "none of it looks like the game's" — and the second was
    /// simply false: an idle game keeps its connection alive, so those unparsed
    /// frames *are* the game's, carrying no shop bytes yet.
    ///
    /// The row states what is so and stops there. It does not advise either:
    /// `app::session` already emits `>> watching — open the shop` into the
    /// journal on the same transition, and an instruction repeated on a status
    /// row stops being an instruction and becomes a verdict.
    #[test]
    fn the_pre_shop_states_report_no_fault_and_give_no_advice() {
        for view in pre_shop_states() {
            let sentence = diagnosis(&view);
            let harness = Harness::new_ui(|ui| render_capture_health(ui, &view));
            harness.get_by_label(sentence);
            assert!(
                !sentence.contains("open the"),
                "the journal owns the instruction: {sentence}"
            );
            assert!(
                !sentence.contains("cannot") && !sentence.contains("check"),
                "nothing is wrong yet, so nothing may read as wrong: {sentence}"
            );
        }
    }

    /// What to check when the shop *is* already open is real, and it survives
    /// — inside the journal panel, which a player reaches only by opening it.
    /// Withheld entirely, the one genuinely misconfigured player is left with a
    /// row that says "waiting" forever.
    #[test]
    fn what_to_check_is_rendered_once_the_journal_is_open() {
        let [nothing_captured, only_keepalives] = pre_shop_states();

        let filter_note = detail(&nothing_captured).expect("the silent state has a note");
        assert!(filter_note.contains("game_port"), "{filter_note}");

        // `capture::link`'s untested VLAN strip is the fault behind this one.
        let strip_note = detail(&only_keepalives).expect("the keepalive state has a note");
        assert!(strip_note.contains("VLAN"), "{strip_note}");

        for (view, note) in [
            (nothing_captured, filter_note),
            (only_keepalives, strip_note),
        ] {
            assert!(
                note.starts_with("Expected"),
                "the ordinary reading opens the note, the fault follows it: {note}"
            );
            // No disclosure of its own any more: once this renders at all, the
            // player has already opened the journal and the note is on screen.
            let harness = Harness::new_ui(|ui| render_capture_health(ui, &view));
            harness.get_by_label(note);
        }
    }

    /// A healthy session has nothing to disclose, so the panel stays what it
    /// always was — the sentence, and nothing dressed up as a caveat.
    #[test]
    fn a_healthy_session_discloses_no_caveat() {
        assert_eq!(detail(&resynced(ResyncCause::DriverRing, 1)), None);
        assert_eq!(
            detail(&CaptureHealthView {
                delivered: 500,
                admitted: 499,
                ..CaptureHealthView::default()
            }),
            None
        );
    }

    #[test]
    fn healthy_capture_with_no_drops_says_so() {
        let view = CaptureHealthView {
            delivered: 500,
            admitted: 499,
            ..CaptureHealthView::default()
        };
        let harness = Harness::new_ui(|ui| render_capture_health(ui, &view));
        harness.get_by_label("capture looks healthy");
    }

    #[test]
    fn drops_and_resyncs_are_named_even_while_admitting_traffic() {
        let view = resynced(ResyncCause::DriverRing, 3);
        let harness = Harness::new_ui(|ui| render_capture_health(ui, &view));
        harness.get_by_label(diagnosis(&view));
        assert!(diagnosis(&view).contains("re-anchored"));
    }

    /// The defect this whole attribution chain exists to fix.
    ///
    /// Every cause used to render one sentence — "a slow connection or a driver
    /// hiccup" — and four of the seven there were then are neither. A player
    /// whose machine was merely busy was sent to troubleshoot a working network.
    #[test]
    fn each_cause_gets_a_sentence_of_its_own() {
        let mut seen: Vec<&'static str> = ResyncCause::ALL
            .into_iter()
            .map(|cause| diagnosis(&resynced(cause, 1)))
            .collect();
        let total = seen.len();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), total, "two causes share a sentence");
    }

    /// Only `DriverRing` and the two reassembly causes are about the wire or a
    /// lost packet. The rest are this process falling behind, and must not send
    /// the player after their connection.
    #[test]
    fn the_causes_inside_this_process_do_not_blame_the_network() {
        for cause in [
            ResyncCause::CaptureFunnel,
            ResyncCause::ByteQuota,
            ResyncCause::MetadataQueue,
            ResyncCause::StreamEvicted,
            ResyncCause::StreamReclaimed,
        ] {
            let sentence = diagnosis(&resynced(cause, 1));
            assert!(
                !sentence.contains("connection dropped") && !sentence.contains("slow connection"),
                "{cause:?} blamed the network: {sentence}"
            );
        }
    }

    /// The eviction sentence used to blame "other connections on the game's
    /// port". The kernel filter admits the game server's own source port and
    /// nothing else, so those connections are the game's own — the wording sent
    /// a player looking for foreign traffic that cannot be there.
    #[test]
    fn the_eviction_sentence_does_not_send_the_player_after_foreign_traffic() {
        let sentence = diagnosis(&resynced(ResyncCause::StreamEvicted, 1));
        assert!(
            !sentence.contains("other connections"),
            "the flows that fill the table are the game's own: {sentence}"
        );
        assert!(sentence.contains("re-anchored"), "{sentence}");
    }

    /// An abort is the server hanging up. The nearest existing sentence was the
    /// eviction one, which names a stream table that had no part in it — the
    /// same mistake, one layer along, as sending a player after foreign traffic.
    #[test]
    fn the_abort_sentence_does_not_blame_the_stream_table() {
        let sentence = diagnosis(&resynced(ResyncCause::ConnectionReset, 1));
        assert!(!sentence.contains("stream table"), "{sentence}");
        assert!(sentence.contains("server"), "{sentence}");
    }

    /// The defect: a lossless eviction is not a fault, and 46 of them in ~90 s
    /// were painting a healthy run amber. The filter lives in
    /// `PipelineStats::dominant_resync`, so this goes through the real budget
    /// rather than through a hand-built view.
    #[test]
    fn a_run_of_lossless_evictions_reads_as_healthy() {
        let (budget, run) = tracked_budget();
        arm(&run);
        for _ in 0..46 {
            budget.record_resync(ResyncCause::StreamReclaimed);
        }

        let view = capture_view(&traffic(), &budget.snapshot(), run.baseline());

        assert_eq!(view.dominant_resync, None);
        assert_eq!(diagnosis(&view), "capture looks healthy");
        assert_eq!(diagnosis_color(&view), theme::INK);
        // Counted all the same: the events are real, they are just not faults.
        assert_eq!(budget.snapshot().since(run.baseline()).resyncs, 46);
        let harness = Harness::new_ui(|ui| render_capture_health(ui, &view));
        harness.get_by_label("capture looks healthy");
    }

    /// And an eviction that *did* lose buffered bytes is still a fault — it
    /// still names itself, and still goes amber — even against forty-six times
    /// as many lossless ones.
    #[test]
    fn an_eviction_that_lost_buffered_bytes_still_names_itself_in_amber() {
        let (budget, run) = tracked_budget();
        arm(&run);
        for _ in 0..46 {
            budget.record_resync(ResyncCause::StreamReclaimed);
        }
        budget.record_resync(ResyncCause::StreamEvicted);

        let view = capture_view(&traffic(), &budget.snapshot(), run.baseline());

        assert_eq!(view.dominant_resync, Some(ResyncCause::StreamEvicted));
        assert_eq!(diagnosis_color(&view), theme::AMBER);
        assert!(diagnosis(&view).contains("re-anchored"));
        let harness = Harness::new_ui(|ui| render_capture_health(ui, &view));
        harness.get_by_label(diagnosis(&view));
    }

    /// Drops with no cause behind them are the "watch just armed" discard, and
    /// used to render the resync sentence — an alarm over bytes that were being
    /// thrown away on purpose.
    #[test]
    fn drops_without_a_resync_read_as_the_benign_arming_discard() {
        let view = CaptureHealthView {
            delivered: 500,
            admitted: 499,
            dropped_segments: 2,
            ..CaptureHealthView::default()
        };
        let harness = Harness::new_ui(|ui| render_capture_health(ui, &view));
        harness.get_by_label(diagnosis(&view));
        assert!(diagnosis(&view).contains("armed"));
        assert!(!diagnosis(&view).contains("re-anchored"));
    }

    /// Amber only when a re-anchor happened. A readout that is loud whatever it
    /// says trains the eye to skip it.
    #[test]
    fn only_a_real_re_anchor_lights_the_sentence_up() {
        for view in pre_shop_states() {
            assert_eq!(diagnosis_color(&view), theme::INK);
        }
        let healthy = CaptureHealthView {
            delivered: 500,
            admitted: 499,
            ..CaptureHealthView::default()
        };
        assert_eq!(diagnosis_color(&healthy), theme::INK);
        assert_eq!(
            diagnosis_color(&resynced(ResyncCause::DriverRing, 1)),
            theme::AMBER
        );
    }

    /// The defect the per-run baseline exists for, at the surface it showed on:
    /// the budget counts for the process, so a re-anchor that happened before
    /// this run used to word the sentence and light it amber for the rest of the
    /// session.
    #[test]
    fn a_re_anchor_from_before_the_run_neither_words_it_nor_lights_it_up() {
        let (budget, run) = tracked_budget();
        budget.record_resync(ResyncCause::CaptureFunnel);
        budget.record_drop(512);

        arm(&run);
        let view = capture_view(&traffic(), &budget.snapshot(), run.baseline());

        assert_eq!(view.dominant_resync, None);
        assert_eq!(diagnosis(&view), "capture looks healthy");
        assert_eq!(diagnosis_color(&view), theme::INK);
        let harness = Harness::new_ui(|ui| render_capture_health(ui, &view));
        harness.get_by_label("capture looks healthy");
    }

    /// And a re-anchor *inside* the run still says exactly what it said before,
    /// in exactly the same amber.
    #[test]
    fn a_re_anchor_during_the_run_still_names_its_cause_in_amber() {
        let (budget, run) = tracked_budget();
        arm(&run);
        budget.record_resync(ResyncCause::DriverRing);
        budget.record_drop(512);

        let view = capture_view(&traffic(), &budget.snapshot(), run.baseline());

        assert_eq!(view, resynced(ResyncCause::DriverRing, 1));
        assert_eq!(diagnosis_color(&view), theme::AMBER);
        let harness = Harness::new_ui(|ui| render_capture_health(ui, &view));
        harness.get_by_label(diagnosis(&view));
        assert!(diagnosis(&view).contains("re-anchored"));
    }

    /// The blind spot the arming edge closes, and the reason the baseline is not
    /// the window's to take.
    ///
    /// The gate opens on the session loop and the capture thread records against
    /// it immediately, while the window repaints at 4 Hz and is outranked by that
    /// same capture thread — so the first re-anchor of a run routinely lands
    /// before the first frame that could have noticed the run at all. A baseline
    /// taken by that frame would have included this re-anchor and subtracted it
    /// away for good, leaving "capture looks healthy" over a stream that had
    /// re-anchored.
    #[test]
    fn a_re_anchor_before_the_first_frame_is_still_reported_for_that_run() {
        let (budget, run) = tracked_budget();
        // Whatever the process did before this run, which stays subtracted.
        budget.record_resync(ResyncCause::ReassemblyShared);

        arm(&run);
        // The capture thread, inside the 250 ms before the next repaint.
        budget.record_resync(ResyncCause::CaptureFunnel);
        budget.record_drop(512);

        // The first frame to sample anything at all is this one.
        let view = capture_view(&traffic(), &budget.snapshot(), run.baseline());

        assert_eq!(view.dominant_resync, Some(ResyncCause::CaptureFunnel));
        assert_eq!(view.dropped_segments, 1);
        assert_eq!(diagnosis_color(&view), theme::AMBER);
        let harness = Harness::new_ui(|ui| render_capture_health(ui, &view));
        harness.get_by_label(diagnosis(&view));
    }

    /// A stop does not rebuild the pipeline, so the next run inherits the
    /// counters. It must not inherit the verdict — while the run that just
    /// ended keeps its own, which is still what is on screen.
    #[test]
    fn a_stop_and_a_fresh_start_leave_the_new_run_its_own_verdict() {
        let (budget, run) = tracked_budget();
        arm(&run);
        budget.record_resync(ResyncCause::ByteQuota);

        // A stop publishes nothing: the finished run keeps its verdict, which is
        // still the answer to what the player is looking at.
        let stopped = capture_view(&traffic(), &budget.snapshot(), run.baseline());
        assert_eq!(stopped.dominant_resync, Some(ResyncCause::ByteQuota));
        assert_eq!(diagnosis_color(&stopped), theme::AMBER);

        arm(&run);
        let restarted = capture_view(&traffic(), &budget.snapshot(), run.baseline());
        assert_eq!(restarted.dominant_resync, None);
        assert_eq!(diagnosis_color(&restarted), theme::INK);
    }

    /// A run that has just armed reads as the ordinary first seconds of a
    /// session, whatever the process did before it.
    #[test]
    fn a_freshly_armed_run_reads_as_the_pre_shop_state() {
        let (budget, run) = tracked_budget();
        budget.record_resync(ResyncCause::ReassemblyShared);
        budget.record_drop(4096);

        arm(&run);
        let view = capture_view(
            &CaptureCounters::default(),
            &budget.snapshot(),
            run.baseline(),
        );

        assert_eq!(view, pre_shop_states()[0]);
    }

    /// Before the first arming the cell holds `RunBaseline::default()`, and that
    /// is also the truth: a shut gate forwards nothing, so nothing behind these
    /// counters can have moved.
    #[test]
    fn a_window_opened_before_any_run_starts_from_zero() {
        let (budget, run) = tracked_budget();

        let view = capture_view(&traffic(), &budget.snapshot(), run.baseline());

        assert_eq!(view.dominant_resync, None);
        assert_eq!(view.dropped_segments, 0);
    }

    /// The trait's defaulted `counters()` (a backend that counts nothing) and
    /// a session that has genuinely captured zero packets are the same
    /// `CaptureCounters::default()` value, and must render the same honest
    /// sentence — not four zeros dressed up as a fault.
    #[test]
    fn the_defaulted_backend_renders_the_same_honest_sentence_as_true_silence() {
        let defaulted = CaptureCounters::default();
        let view = CaptureHealthView {
            delivered: defaulted.delivered,
            admitted: defaulted.admitted,
            ..CaptureHealthView::default()
        };
        assert_eq!(view, CaptureHealthView::default());
        let harness = Harness::new_ui(|ui| render_capture_health(ui, &view));
        harness.get_by_label(diagnosis(&view));
    }
}
