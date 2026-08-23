//! Top status bar: the error banner, over whichever of two bands the moment
//! calls for — the idle band (plan + one wide command) or the run band (the
//! headline figure, its gauge, the haul and the balances).

use eframe::egui;

use crate::app::Command;
use crate::domain::control::Status;
use crate::install::{Fetcher, Progress};

use super::theme;
use super::view::{self, ViewState};

/// How often to look again while a download is running. The window idles at
/// 4 Hz; a button that says "Downloading…" needs to notice when it stops.
const REPAINT_WHILE_FETCHING: std::time::Duration = std::time::Duration::from_millis(150);

/// Splits an error line around the first URL in it: `(before, url, after)`.
///
/// A URL ends at the first whitespace, so a trailing `)` or `.` stays attached
/// — both are legal URL characters, and `INSTALL_HINT` is written so the
/// address is followed by a space.
fn split_help_url(text: &str) -> Option<(&str, &str, &str)> {
    let start = text.find("https://")?;
    let rest = &text[start..];
    let len = rest.find(char::is_whitespace).unwrap_or(rest.len());
    Some((&text[..start], &rest[..len], &rest[len..]))
}

/// Strips the error-taxonomy prefixes, which name which layer failed to someone
/// who cannot act on either. If a string is reworded the prefix simply survives.
fn without_error_prefixes(text: &str) -> &str {
    let mut rest = text;
    for prefix in ["session error: ", "network capture: "] {
        rest = rest.strip_prefix(prefix).unwrap_or(rest);
    }
    rest
}

/// The error banner: one sentence, then an action row under it.
///
/// Stacked, not inline: a link reads as a word in a flowing sentence but a
/// stateful button does not, and inlining it left `Restart now` mid-clause with
/// the sentence still telling the player to restart the app. The button is the
/// stock one — loud enough against the quiet band below it, and a banner that
/// out-shouts nothing is a banner nobody reads twice.
fn error_banner(ui: &mut egui::Ui, text: &str, fetcher: &Fetcher) {
    let text = without_error_prefixes(text);
    let color = ui.visuals().error_fg_color;
    let Some((headline, url, hint)) = split_help_url(text) else {
        ui.colored_label(color, text);
        return;
    };
    // Only an installer earns the stacked layout and the action row: a
    // documentation URL must never end up under a button offering to download
    // and run it.
    if !url.to_ascii_lowercase().ends_with(".exe") {
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            ui.colored_label(color, headline);
            ui.hyperlink_to(url, url);
            ui.colored_label(color, hint);
        });
        return;
    }
    ui.vertical(|ui| {
        ui.colored_label(color, headline.trim());
        ui.add_space(theme::SP_SM);
        install_row(ui, url, hint.trim(), fetcher);
    });
}

/// The action row: one button, and beside it whatever the player needs next.
/// A fetch in flight disables it, making [`Fetcher::start`]'s refusal of a
/// second worker visible rather than silent.
fn install_row(ui: &mut egui::Ui, url: &str, hint: &str, fetcher: &Fetcher) {
    ui.horizontal_wrapped(|ui| match fetcher.progress() {
        Progress::Fetching => {
            ui.add_enabled(false, egui::Button::new("Downloading…"));
        }
        Progress::Checking => {
            ui.add_enabled(false, egui::Button::new("Checking…"));
        }
        Progress::Launched => restart_row(ui, fetcher, None),
        Progress::RestartFailed(reason) => restart_row(ui, fetcher, Some(&reason)),
        Progress::Failed(reason) => {
            // The retry stays available: the common failures here are transient
            // (a proxy, a captive portal, a dropped connection).
            if ui.button("Retry download").clicked() {
                fetcher.start();
            }
            ui.add_space(theme::SP_SM);
            ui.colored_label(ui.visuals().error_fg_color, reason);
        }
        Progress::Idle => {
            if ui
                .button("Download Npcap")
                .on_hover_text(format!(
                    "{url}\nchecked against a pinned hash before it runs"
                ))
                .clicked()
            {
                fetcher.start();
            }
            if !hint.is_empty() {
                ui.add_space(theme::SP_SM);
                ui.colored_label(theme::INK_FAINT, hint);
            }
        }
    });
    if matches!(fetcher.progress(), Progress::Fetching | Progress::Checking) {
        ui.ctx().request_repaint_after(REPAINT_WHILE_FETCHING);
    }
}

/// The restart control, before and after a restart that did not work. The tap
/// is opened once, inside the session that already died, so only a fresh
/// process picks Npcap up (see `install::relaunch`).
///
/// One function for both states: a failed restart must not fall through to
/// [`Progress::Failed`], whose `Retry download` would find the verified
/// installer on disk, launch a *second* Npcap setup, and overwrite the restart
/// error with `Launched`.
fn restart_row(ui: &mut egui::Ui, fetcher: &Fetcher, failure: Option<&str>) {
    if ui
        .button("Restart now")
        .on_hover_text("starts a fresh copy and closes this window")
        .clicked()
    {
        match crate::install::relaunch() {
            Ok(()) => ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close),
            // Same cell as every other failure, so a relaunch that did not
            // happen cannot read as one that did.
            Err(err) => fetcher.restart_failed(format!("could not restart: {err}")),
        }
    }
    ui.add_space(theme::SP_SM);
    match failure {
        None => {
            ui.colored_label(theme::INK_MUTED, "once the Npcap setup is finished");
        }
        Some(reason) => {
            ui.colored_label(ui.visuals().error_fg_color, reason);
            ui.add_space(theme::SP_SM);
            ui.colored_label(
                theme::INK_FAINT,
                "or close this window and open the app again",
            );
        }
    }
}

/// Top chrome: the error banner, then whichever of the two bands the moment
/// calls for.
///
/// Two bands and not one row that changes, because the two moments want
/// opposite things. Before a run there is nothing to read — every figure is a
/// dash — so the band explains the plan and carries one wide, quiet command.
/// During a run there is nothing to decide, so the band collapses to what
/// moves: the figure, the gauge under it, and the verb reduced to a word.
#[must_use]
pub(super) fn render_status_bar(
    ui: &mut egui::Ui,
    view: &ViewState,
    outcome: Option<&str>,
    session_alive: bool,
    fetcher: &Fetcher,
) -> Option<Command> {
    if let Some(outcome) = outcome {
        error_banner(ui, outcome, fetcher);
        ui.separator();
    }
    ui.add_space(theme::SP_XS);
    // No trailing space: the panel carries no bottom padding either, because
    // each band ends on the full-bleed line that is its edge against the tab
    // strip. Anything added here would reopen the gap that line exists to close.
    if matches!(view.status_kind, Status::Idle) {
        idle_band(ui, view, session_alive)
    } else {
        run_band(ui, view, session_alive)
    }
}

/// Before the first run: the invitation, the plan, and the slab that starts it.
fn idle_band(ui: &mut egui::Ui, view: &ViewState, session_alive: bool) -> Option<Command> {
    ui.label(theme::section(view.status_hint.unwrap_or("ready to start")));
    ui.add_space(theme::SP_XS);
    ui.add(
        egui::Label::new(
            egui::RichText::new(view.plan.as_deref().unwrap_or_default()).color(theme::INK_MUTED),
        )
        .truncate(),
    );
    ui.add_space(theme::SP_SM);
    // Start, never Toggle: the 4 Hz poll can render a band 250 ms stale, and a
    // toggle raced by an auto-stop would re-arm the loop the domain just left.
    let clicked = command(
        ui,
        "Start watching",
        Command::Start,
        session_alive,
        theme::slab_button,
    );
    // The band's own edge, since the panel no longer draws one. The run band
    // gets this line for free from its gauge; here the slab needs air under it
    // first, or the rule reads as the control's underline.
    ui.add_space(theme::SP_SM);
    theme::rule(ui, theme::HAIRLINE);
    clicked
}

/// While a run exists — armed or finished: what it has done, how close that is
/// to stopping it, and the way out.
fn run_band(ui: &mut egui::Ui, view: &ViewState, session_alive: bool) -> Option<Command> {
    balances_strip(ui, view);
    let armed = matches!(view.status_kind, Status::Watching | Status::Paused);
    let (label, action) = if armed {
        ("Stop", Command::Stop)
    } else {
        ("Start", Command::Start)
    };
    let dial = view::dial(view.progress, &view.limits, view.elapsed_ms);
    // Beside the figure: the cap it runs against, or — with nothing bounding the
    // run — the one companion that still moves. Both can be absent, and an
    // absent one adds no widget rather than an empty one.
    let companion = match &dial.against {
        Some(cap) => Some(format!("/ {}", cap.limit)),
        None => view::refresh_rate(view.progress.refreshes, view.elapsed_ms)
            .map(|rate| format!("· {rate}")),
    };
    // The verb is placed first because the row lays out from the right; the
    // status then fills what is left, so a long stop reason never pushes the
    // command off the panel.
    //
    // The status sits here and nowhere else. This slot used to hold the words
    // "This run", which are true of every frame and therefore tell nobody
    // anything, while the reason for a stop was parked at the foot of the band —
    // where, in the states that have no reason to give, it left a gap between
    // the gauge and the tab strip that read as two rules with nothing between
    // them. One line, in the band's best position, removes both.
    let clicked = command(ui, label, action, session_alive, |ui, label| {
        let response = theme::bare_verb(ui, label);
        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
            ui.add(
                egui::Label::new(egui::RichText::new(status_line(view)).color(theme::INK_MUTED))
                    .truncate(),
            );
        });
        response
    });

    ui.add_space(theme::SP_SM);
    // The haul keeps its own row: beside the headline it runs off the panel.
    // Measured in `examples/haul_variants.rs`, sample `wide case` — ends at
    // x = 483 where the panel stops at 424. That sample is a wide reachable run
    // rather than the widest, so 59px is a floor, which is all it takes to rule
    // the arrangement out.
    ui.label(headline_job(ui, &dial, companion));
    ui.add_space(theme::SP_SM);
    // Wrapped: a narrow window must fold the tokens rather than clip them.
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = theme::SP_XS;
        // Chained rather than appended after the loop, so one rule opens every
        // gap and `Other` cannot end up spaced unlike its neighbours.
        let others = (view.haul_others > 0).then(|| ("Other", format!("+{}", view.haul_others)));
        let tokens = view
            .haul
            .iter()
            .map(|(label, count)| (*label, count.to_string()))
            .chain(others);
        for (index, (label, count)) in tokens.enumerate() {
            if index > 0 {
                ui.add_space(theme::SP_XL);
            }
            token(ui, label, &count);
        }
    });

    // Last, and full width: the gauge closes the band and doubles as its edge
    // against the tab strip. A run with no limit has no proportion to show and
    // still needs that edge, so it falls back to the plain rule.
    //
    // It was briefly moved up under the figure, on the theory that a bar at the
    // foot read as a second divider beside the tab strip's. The real cause of
    // that was the stop clause below it, which is absent in most states and
    // left a gap between the two lines. With the clause moved into the leading
    // slot the foot is the gauge's proper place.
    ui.add_space(theme::SP_SM);
    match &dial.against {
        Some(cap) => theme::gauge(ui, cap.ratio),
        None => theme::rule(ui, theme::HAIRLINE),
    }
    clicked
}

/// The purses the run draws on — each one named, then its amount — in their own
/// strip above everything else.
///
/// Above the band and not below it: a balance is what a run starts *from*, and
/// parked under the figures it read as a total the run had produced.
///
/// It renders even when both balances are unknown, showing a sentence in their
/// place. Returning early instead made the whole strip — row, rule and spacing
/// — appear the moment the first refresh landed, shoving everything below it
/// down by 33 points mid-run; `the_band_is_the_same_height_before_and_after_the_first_balance`
/// holds that shut. A dash placeholder was the other way out and reads as
/// "— skystones · — gold", punctuation outweighing information.
///
/// Kept thin: [`theme::SP_XS`] either side of its rule rather than the
/// [`theme::SP_SM`] the band's blocks get, so it reads as their frame rather
/// than as a block of its own.
fn balances_strip(ui: &mut egui::Ui, view: &ViewState) {
    // Name paired with amount in one expression, so swapping the two is visible
    // on a single line.
    let purses = [
        view.crystal_balance
            .map(|balance| ("Skystones", balance.to_string())),
        view.gold_balance
            .map(|balance| ("Gold", balance.to_string())),
    ];
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = theme::SP_XS;
        if purses.iter().all(Option::is_none) {
            // Names where each figure comes from rather than saying "loading":
            // skystones arrive on a refresh, gold on a purchase echo, so the
            // player knows which of their own actions fills which half.
            ui.add(
                egui::Label::new(
                    egui::RichText::new(
                        "skystones arrive with the first refresh, gold with the first purchase",
                    )
                    .small()
                    .color(theme::INK_FAINT),
                )
                .truncate(),
            );
        }
        for (index, (name, amount)) in purses.iter().flatten().enumerate() {
            // A gap wide enough to group each name with its own amount, in
            // place of the separator this line used to need.
            if index > 0 {
                ui.add_space(theme::SP_XL);
            }
            ui.label(theme::section(name));
            ui.label(egui::RichText::new(amount).small().color(theme::INK_MUTED));
        }
    });
    ui.add_space(theme::SP_XS);
    theme::rule(ui, theme::HAIRLINE);
    ui.add_space(theme::SP_XS);
}

/// The run's headline figure, a size the theme's text styles have no name for:
/// it is the one thing on screen meant to be read from across a desk.
const FIGURE_SIZE: f32 = 34.0;

/// The headline as one line of text: the figure, the cap it runs against, and
/// what it counts.
///
/// Split out of [`run_band`] so a test can hold the job: the shared baseline
/// lives in its glyph positions, and through the band it could only be inferred
/// from pixels.
fn headline_job(
    ui: &egui::Ui,
    dial: &view::Dial,
    companion: Option<String>,
) -> egui::text::LayoutJob {
    let figure = egui::FontId::proportional(FIGURE_SIZE);
    // The figure's own descent, handed to every run — that is what lands them
    // all on its baseline. See [`run`].
    let (figure_ascent, figure_line) = line_metrics(ui, &figure);
    let descent = figure_line - figure_ascent;
    let mut job = egui::text::LayoutJob {
        // Only `max_rows` is load-bearing: `Label` overwrites `max_width` with
        // the same value and `overflow_character` is already the default. Both
        // are spelled out for the test, which has no `Label` to inherit from.
        wrap: egui::text::TextWrapping {
            max_width: ui.available_width(),
            max_rows: 1,
            overflow_character: Some('…'),
            ..Default::default()
        },
        ..Default::default()
    };
    job.append(&dial.value, 0.0, run(ui, figure, theme::INK, descent));
    if let Some(companion) = companion {
        job.append(
            &format!(" {companion}"),
            theme::SP_XS,
            run(
                ui,
                egui::TextStyle::Body.resolve(ui.style()),
                theme::INK_FAINT,
                descent,
            ),
        );
    }
    job.append(
        &format!(" {}", dial.caption.to_uppercase()),
        theme::SP_XS,
        run(
            ui,
            egui::TextStyle::Small.resolve(ui.style()),
            theme::INK_FAINT,
            descent,
        ),
    );
    job
}

/// Where a font's baseline sits under the top of its line box, and how tall that
/// box is. epaint publishes both only on a glyph it has placed, hence the probe.
///
/// `font_ascent`, not `font_face_ascent`: the family's ascent is what cancels in
/// [`run`]'s algebra. The two agree only while `'0'` resolves to the primary
/// face.
fn line_metrics(ui: &egui::Ui, font_id: &egui::FontId) -> (f32, f32) {
    // `fonts_mut`: laying the glyph out to answer mutates the font cache.
    ui.ctx().fonts_mut(|fonts| {
        let galley = fonts.layout_no_wrap("0".to_owned(), font_id.clone(), theme::INK);
        galley
            .rows
            .first()
            .and_then(|placed| placed.row.glyphs.first())
            .map_or((0.0, 0.0), |glyph| (glyph.font_ascent, glyph.line_height))
    })
}

/// One run of the headline's single line, stood on the shared baseline that
/// `descent` defines.
///
/// epaint has no baseline `valign` — only `TOP`, `BOTTOM` (the default) and
/// `Center`, none of which align baselines. Its placement rule is published, at
/// `epaint-0.35.0/src/text/text_layout.rs:978`:
///
/// ```text
/// baseline = font_face_ascent
///          + valign_factor * (max_row_height - line_height)
///          + 0.5 * (font_height - font_face_height)
/// ```
///
/// At `BOTTOM` the factor is 1, so on natural line heights the first two terms
/// give `max_row_height - descent` — each run sitting on a descent of its own,
/// which is 4px of sag between a 34px figure and an 11px caption. Give every run
/// `own_ascent + one shared descent` and the ascents cancel, leaving one value
/// for the row. `the_headlines_runs_share_one_baseline` fails without this.
///
/// The third term is not controlled here, which is why the rule is quoted whole:
/// it is zero only while every glyph comes from its family's primary face, as
/// all of this line's do. A glyph that falls back is shifted by the same
/// constant as its neighbours, so it lands as far off baseline as epaint puts it
/// by default — no worse, but not fixed either.
fn run(
    ui: &egui::Ui,
    font_id: egui::FontId,
    color: egui::Color32,
    descent: f32,
) -> egui::TextFormat {
    let (ascent, _) = line_metrics(ui, &font_id);
    egui::TextFormat {
        font_id,
        color,
        line_height: Some(ascent + descent),
        ..Default::default()
    }
}

/// The run's state as one sentence: the word, and the clause behind it when
/// there is one. `Watching` stands alone, since a run doing its job has nothing
/// to explain.
fn status_line(view: &ViewState) -> String {
    match view.status_hint {
        Some(hint) => format!("{} — {hint}", view.status_word),
        None => view.status_word.to_owned(),
    }
}

/// Places the band's one command and reports the click.
///
/// The explicit id is the point, for the reason `editor::commit_row` gives at
/// length: the error banner above is conditional, and everything behind a
/// conditional widget is renumbered when it appears — this command included.
/// A `Ui`'s own id is the stable one; only the *unique* id egui derives for a
/// child folds in the sequence. Both bands salt with the same name so the
/// command keeps one identity across a start.
///
/// The `horizontal` around it bounds the row's height. A `UiBuilder` carrying a
/// layout claims the whole available rect, so a vertically centred command
/// placed straight into the panel sits halfway down the window and drags the
/// panel down with it.
fn command(
    ui: &mut egui::Ui,
    label: &str,
    action: Command,
    session_alive: bool,
    add: impl FnOnce(&mut egui::Ui, &str) -> egui::Response,
) -> Option<Command> {
    let mut clicked = None;
    ui.horizontal(|ui| {
        ui.scope_builder(
            egui::UiBuilder::new()
                .id(ui.id().with("run_command"))
                .layout(egui::Layout::right_to_left(egui::Align::Center)),
            |ui| {
                ui.add_enabled_ui(session_alive, |ui| {
                    if add(ui, label).clicked() {
                        clicked = Some(action);
                    }
                });
            },
        );
    });
    clicked
}

/// One counted token: a small grey uppercase name, then its count in full ink,
/// on one line — the arrangement [`balances_strip`] uses one block higher.
///
/// On one line and not stacked: a stacked tile is a `ui.vertical` as wide as
/// `max(name, count)`, which the name always wins, so the counts end up spaced
/// by the length of the words above them rather than by anything of their own.
fn token(ui: &mut egui::Ui, label: &str, count: &str) {
    ui.label(theme::section(label));
    ui.label(egui::RichText::new(count).color(theme::INK));
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use egui_kittest::{
        Harness,
        kittest::{NodeT, Queryable},
    };

    use crate::domain::control::{Controller, Event, Limits};
    use crate::domain::filter::Filter;
    use crate::domain::shop::{Crystals, RefreshMeta, ShopSnapshot};

    use super::super::view::view_state;
    use super::*;

    fn idle_view() -> ViewState {
        let controller = Controller::new(Filter::default(), Limits::default());
        view_state(&controller, 0)
    }

    fn watching_view() -> ViewState {
        let mut controller = Controller::new(Filter::matching_default_items(), Limits::default());
        let _ = controller.handle(Event::Start { now_ms: 0 });
        view_state(&controller, 0)
    }

    /// A run with every rail armed, six minutes in: 42 refreshes of 60, but 126
    /// crystals of 150 — so the crystal budget is the rail about to fire.
    fn bounded_run() -> ViewState {
        let limits = Limits {
            max_refreshes: Some(60),
            max_spend: Some(Crystals::new(150)),
            max_matches: Some(5),
            max_duration_ms: Some(20 * 60_000),
        };
        let mut controller = Controller::new(Filter::matching_default_items(), limits);
        let _ = controller.handle(Event::Start { now_ms: 0 });
        let mut view = view_state(&controller, 6 * 60_000);
        // Set on the projection rather than driven through 42 refresh events:
        // this is a test of the band, and the domain's own accounting has its
        // tests in `domain::control`.
        view.progress = crate::domain::control::Progress {
            refreshes: 42,
            spent: Crystals::new(126),
            matches_found: 3,
        };
        view
    }

    /// A literal, not an import: `INSTALL_HINT` is behind
    /// `cfg(all(windows, feature = "pcap-backend"))` and these tests run on
    /// every lane. Only structure is asserted, so the wording can move.
    const NPCAP_ERROR: &str = "session error: network capture: Npcap is missing, and the capture needs it. https://dev-libs.wireshark.org/windows/packages/Npcap/npcap-1.88.exe Keep the installer's defaults, then restart this app.";

    #[test]
    fn split_help_url_finds_the_address_inside_a_sentence() {
        let (before, url, after) = split_help_url(NPCAP_ERROR).expect("the hint carries a URL");
        assert!(before.ends_with("the capture needs it. "), "got: {before}");
        assert_eq!(
            url,
            "https://dev-libs.wireshark.org/windows/packages/Npcap/npcap-1.88.exe"
        );
        assert!(after.starts_with(" Keep the"), "got: {after}");
        // The banner renders these three pieces and nothing else, so a lost
        // character is a lost word.
        assert_eq!(format!("{before}{url}{after}"), NPCAP_ERROR);
    }

    #[test]
    fn split_help_url_declines_text_without_one() {
        assert!(split_help_url("session error: the game window vanished").is_none());
        // Only https. A cleartext http link in an error banner is one the app
        // should not be teaching anyone to click.
        assert!(split_help_url("see http://example.invalid/x").is_none());
    }

    /// Only an `.exe` gets the action row: a documentation link must never sit
    /// under a button offering to download and run it.
    #[test]
    fn a_documentation_url_stays_an_inline_link() {
        let view = idle_view();
        let doc = "capture refused: see https://npcap.com/#download for the driver";
        let harness = Harness::new_ui(|ui| {
            let _ = render_status_bar(ui, &view, Some(doc), true, &Fetcher::new());
        });
        harness.get_by_label("https://npcap.com/#download");
        assert!(
            harness.query_by_label("Download Npcap").is_none(),
            "a doc page must not get the installer button"
        );
    }

    #[test]
    fn split_help_url_keeps_a_trailing_url_whole() {
        let (before, url, after) = split_help_url("get it from https://npcap.com").expect("a URL");
        assert_eq!(before, "get it from ");
        assert_eq!(url, "https://npcap.com");
        assert!(after.is_empty());
    }

    #[test]
    fn the_taxonomy_prefixes_are_stripped_before_the_player_sees_them() {
        assert_eq!(
            without_error_prefixes("session error: network capture: Npcap is missing."),
            "Npcap is missing."
        );
        // Unrecognised text survives untouched rather than trimmed by guess.
        assert_eq!(
            without_error_prefixes("the game window vanished"),
            "the game window vanished"
        );
    }

    #[test]
    fn the_npcap_banner_offers_the_download_as_a_link() {
        let view = idle_view();
        let harness = Harness::new_ui(|ui| {
            let _ = render_status_bar(ui, &view, Some(NPCAP_ERROR), true, &Fetcher::new());
        });
        // The address is the button's hover text, not characters to select out
        // of a 270-character sentence.
        harness.get_by_label("Download Npcap");
    }

    /// A 90-character address rendered inline is what turned this banner into
    /// six lines of red.
    #[test]
    fn an_installer_url_becomes_a_button_not_an_address() {
        let view = idle_view();
        let msg = "Npcap is missing, and the capture needs it. \
                   https://dev-libs.wireshark.org/windows/packages/Npcap/npcap-1.88.exe \
                   Keep the installer's defaults, then restart this app.";
        let harness = Harness::new_ui(|ui| {
            let _ = render_status_bar(ui, &view, Some(msg), true, &Fetcher::new());
        });
        harness.get_by_label("Download Npcap");
        assert!(
            harness
                .query_by_label(
                    "https://dev-libs.wireshark.org/windows/packages/Npcap/npcap-1.88.exe"
                )
                .is_none(),
            "the raw address should not be rendered beside the button"
        );
    }

    /// Set through `Fetcher`, never by clicking `Restart now`: that button
    /// really does spawn a copy of the running executable — here, the test.
    fn after_a_failed_restart() -> Fetcher {
        let fetcher = Fetcher::new();
        fetcher.restart_failed("could not restart: access denied".to_owned());
        fetcher
    }

    /// `Retry download` here would launch a *second* Npcap setup off the
    /// installer still on disk, and set `Launched`, erasing the restart error.
    #[test]
    fn a_failed_restart_is_not_offered_another_download() {
        let view = idle_view();
        let fetcher = after_a_failed_restart();
        let harness = Harness::new_ui(|ui| {
            let _ = render_status_bar(ui, &view, Some(NPCAP_ERROR), true, &fetcher);
        });
        assert!(
            harness.query_by_label("Retry download").is_none(),
            "a failed restart must not offer the button that spawns another installer"
        );
    }

    /// The way out by hand is offered too, since a relaunch that failed once
    /// will usually fail again (`current_exe` failing is not weather).
    #[test]
    fn a_failed_restart_keeps_the_restart_and_says_why() {
        let view = idle_view();
        let fetcher = after_a_failed_restart();
        let harness = Harness::new_ui(|ui| {
            let _ = render_status_bar(ui, &view, Some(NPCAP_ERROR), true, &fetcher);
        });
        harness.get_by_label("Restart now");
        harness.get_by_label("could not restart: access denied");
        harness.get_by_label("or close this window and open the app again");
    }

    #[test]
    fn an_error_without_a_url_still_renders() {
        let view = idle_view();
        let harness = Harness::new_ui(|ui| {
            let _ = render_status_bar(
                ui,
                &view,
                Some("session error: the game window vanished"),
                true,
                &Fetcher::new(),
            );
        });
        harness.get_by_label("the game window vanished");
    }

    /// The same defect [`super::super::editor::commit_row`] carried, one
    /// surface up: the banner and its separator are rendered ahead of the
    /// status row, so an error arriving renumbers every widget behind them —
    /// the one button included. It never produced egui's `changed id between
    /// passes` line, because the banner also pushes the row down and a moved
    /// rect is not a collision; what it does drop is the same thing, the
    /// interaction state egui keys by id, at the moment an error appears under
    /// a cursor already on the button.
    #[test]
    fn the_status_button_keeps_its_id_when_the_error_banner_appears() {
        let view = idle_view();
        let outcome = RefCell::new(None::<&str>);
        let fetcher = Fetcher::new();
        let mut harness = Harness::new_ui(|ui| {
            let _ = render_status_bar(ui, &view, *outcome.borrow(), true, &fetcher);
        });
        harness.run();
        let quiet = harness.get_by_label("Start watching").accesskit_node().id();

        *outcome.borrow_mut() = Some("session error: the game window vanished");
        harness.run();

        assert_eq!(
            harness.get_by_label("Start watching").accesskit_node().id(),
            quiet,
            "the id egui keys the button's interaction state by"
        );
    }

    #[test]
    fn idle_status_bar_hides_stop_and_toggle() {
        let view = idle_view();
        let harness = Harness::new_ui(|ui| {
            let _ = render_status_bar(ui, &view, None, true, &Fetcher::new());
        });
        assert!(harness.query_by_label("Stop").is_none());
        assert!(harness.query_by_label("Toggle").is_none());
    }

    /// A band has to stay a band.
    ///
    /// An `egui::UiBuilder` carrying a layout claims the whole *available*
    /// rect, not one row: a vertically centred command placed straight into the
    /// panel landed halfway down the window and dragged the panel down to reach
    /// it, which pushed the tab strip, the shop table and the journal off
    /// screen. Every label assertion in this module still passed — the widgets
    /// were all there, in a panel eight hundred pixels tall — so nothing under
    /// `query_by_label` can catch this. Only the geometry can.
    #[test]
    fn neither_band_grows_to_fill_the_window() {
        for (name, view) in [("idle", idle_view()), ("run", bounded_run())] {
            let measured = std::cell::Cell::new((0.0, 0.0));
            let harness = Harness::new_ui(|ui| {
                let available = ui.available_height();
                let _ = render_status_bar(ui, &view, None, true, &Fetcher::new());
                measured.set((ui.min_rect().height(), available));
            });
            drop(harness);
            let (used, available) = measured.get();
            // Measured at 95 points for the idle band and 125 for the run one,
            // out of 584 offered. The ceiling leaves room for font metrics to
            // differ across lanes while staying under both failures it guards
            // against: a band that takes the whole window (~450), and one whose
            // tokens wrap to a letter per line (~270).
            assert!(
                used < 200.0,
                "the {name} band took {used} of {available} points"
            );
        }
    }

    /// Before a run, every figure is a dash — so the band shows none of them,
    /// and says what a run *would* do instead.
    #[test]
    fn the_idle_band_shows_the_plan_and_no_figures() {
        let view = idle_view();
        let harness = Harness::new_ui(|ui| {
            let _ = render_status_bar(ui, &view, None, true, &Fetcher::new());
        });
        harness.get_by_label("DEFINE A FILTER FIRST");
        harness.get_by_label("Nothing selected to hunt — no limits set");
        harness.get_by_label("Start watching");
        // The run's readouts belong to a run.
        assert!(harness.query_by_label("COVENANT").is_none());
        // "Idle" is folded into the lead line above, not written twice: the
        // idle band leads on the clause, the run band on the word.
        assert!(harness.query_by_label("Idle").is_none());
    }

    /// The plan reads back the two Setup summaries, so a player can see what
    /// they armed without opening the tab.
    #[test]
    fn the_plan_names_the_hunt_and_the_rails() {
        let limits = Limits {
            max_refreshes: Some(60),
            ..Limits::default()
        };
        // Named criteria, not `matching_default_items`: that fixture hunts the
        // `Unknown` kind, whose label is "?" — which would assert nothing about
        // the wire→label mapping the plan line goes through.
        let filter = Filter {
            names: vec![
                "ticketrare_name".to_owned(),
                "ticketspecial_name".to_owned(),
            ],
            ..Filter::default()
        };
        let controller = Controller::new(filter, limits);
        let view = view_state(&controller, 0);
        let harness = Harness::new_ui(|ui| {
            let _ = render_status_bar(ui, &view, None, true, &Fetcher::new());
        });
        harness.get_by_label("Hunting Covenant, Mystic — stops at 60 refreshes");
    }

    /// The dial follows the rail that will fire, not a fixed one: here the
    /// crystal budget at 84% beats refreshes at 70%.
    #[test]
    fn the_run_band_dials_the_limit_that_will_stop_the_run() {
        let view = bounded_run();
        let harness = Harness::new_ui(|ui| {
            let _ = render_status_bar(ui, &view, None, true, &Fetcher::new());
        });
        // A run doing its job explains nothing: the word stands alone.
        harness.get_by_label("Watching");
        // One node: the headline is one galley. Asserting the whole string pins
        // the three runs' order and spacing too.
        harness.get_by_label("126 / 150 SKYSTONES SPENT");
        harness.get_by_label("COVENANT");
        // Neither balance known, so the strip holds its row with the sentence
        // and names no purse.
        assert!(harness.query_by_label("SKYSTONES").is_none());
        assert!(harness.query_by_label("GOLD").is_none());
        harness
            .get_by_label("skystones arrive with the first refresh, gold with the first purchase");
        assert!(
            harness.query_by_label("REFRESHES").is_none(),
            "the refresh rail is armed but not the nearest one"
        );
    }

    /// The headline's three runs stand on one baseline.
    ///
    /// The only test that holds [`run`]'s `line_height` construction: setting it
    /// back to `None` restores the full sag and leaves every other test green,
    /// since the accessible string is identical either way. Read off
    /// `glyph.pos.y`, which *is* the baseline in epaint's layout, not a proxy.
    #[test]
    fn the_headlines_runs_share_one_baseline() {
        let view = bounded_run();
        let dial = view::dial(view.progress, &view.limits, view.elapsed_ms);
        let spread = std::cell::Cell::new(f32::NAN);
        let runs = std::cell::Cell::new(0);
        let mut harness = Harness::new_ui(|ui| {
            // The real theme: on stock sizes the three runs are close enough
            // that the sag would be too small to trust.
            theme::apply(ui.ctx());
            let job = headline_job(ui, &dial, Some("/ 150".to_owned()));
            let galley = ui.ctx().fonts_mut(|fonts| fonts.layout_job(job));
            let row = &galley.rows.first().expect("the headline is one row").row;
            let baselines: Vec<f32> = row.glyphs.iter().map(|glyph| glyph.pos.y).collect();
            let low = baselines.iter().copied().fold(f32::INFINITY, f32::min);
            let high = baselines.iter().copied().fold(f32::NEG_INFINITY, f32::max);
            spread.set(high - low);
            // Guards the assertion itself: a job that laid out to nothing, or to
            // one run, would report a spread of zero and prove nothing.
            runs.set(galley.job.sections.len());
        });
        harness.run();

        assert_eq!(
            runs.get(),
            3,
            "the headline should carry figure, cap and caption"
        );
        assert!(
            spread.get() < 0.5,
            "the runs sit on {} different baselines",
            spread.get()
        );
    }

    /// The strip has to hold its place, or the first refresh shoves the window
    /// down by 33 points mid-run.
    ///
    /// No label assertion can see this — both frames hold exactly the widgets
    /// they should, and only the difference in height is wrong.
    #[test]
    fn the_band_is_the_same_height_before_and_after_the_first_balance() {
        let measure = |view: ViewState| {
            let used = std::cell::Cell::new(0.0);
            let harness = Harness::new_ui(|ui| {
                let _ = render_status_bar(ui, &view, None, true, &Fetcher::new());
                used.set(ui.min_rect().height());
            });
            drop(harness);
            used.get()
        };

        let before = measure(bounded_run());
        let mut after_view = bounded_run();
        after_view.crystal_balance = Some(Crystals::new(19_874));
        let after = measure(after_view);

        assert!(
            (before - after).abs() < 1.0,
            "the band jumps {} points when the first balance lands ({before} then {after})",
            (before - after).abs()
        );
    }

    /// A purse the session *has* been told about earns its name and its amount;
    /// the one it has not is simply absent, rather than holding a slot open
    /// with a dash.
    #[test]
    fn the_strip_carries_only_the_balances_the_session_knows() {
        let mut controller = Controller::new(Filter::matching_default_items(), Limits::default());
        let _ = controller.handle(Event::Start { now_ms: 0 });
        // A shop message carries the crystal balance; gold only ever arrives
        // echoed by a purchase, and none has happened here.
        let _ = controller.handle(Event::Snapshot {
            snapshot: ShopSnapshot {
                merchant: None,
                slots: vec![],
                refresh: Some(RefreshMeta {
                    crystal_balance: Crystals::new(19_874),
                    cost: Crystals::new(3),
                }),
            },
            now_ms: 0,
        });
        let view = view_state(&controller, 60_000);
        let harness = Harness::new_ui(|ui| {
            let _ = render_status_bar(ui, &view, None, true, &Fetcher::new());
        });
        harness.get_by_label("SKYSTONES");
        harness.get_by_label("19,874");
        assert!(
            harness.query_by_label("GOLD").is_none(),
            "no purchase has echoed a gold balance yet"
        );
    }

    /// With nothing bounding the run there is no proportion to show, so the
    /// count keeps the one companion that still moves.
    #[test]
    fn an_unbounded_run_shows_a_rate_instead_of_a_cap() {
        let mut controller = Controller::new(Filter::matching_default_items(), Limits::default());
        let _ = controller.handle(Event::Start { now_ms: 0 });
        let mut view = view_state(&controller, 6 * 60_000);
        view.progress.refreshes = 42;
        let harness = Harness::new_ui(|ui| {
            let _ = render_status_bar(ui, &view, None, true, &Fetcher::new());
        });
        harness.get_by_label("42 · 7.0 / min REFRESHES");
    }

    /// The final totals survive a stop — an auto-stop is exactly when the
    /// player wants to read them — and the reason names itself.
    #[test]
    fn a_stopped_run_keeps_its_figures_and_says_why() {
        let mut controller = Controller::new(Filter::matching_default_items(), Limits::default());
        let _ = controller.handle(Event::Start { now_ms: 0 });
        let _ = controller.handle(Event::Stop);
        let view = view_state(&controller, 60_000);
        let harness = Harness::new_ui(|ui| {
            let _ = render_status_bar(ui, &view, None, true, &Fetcher::new());
        });
        // Word and clause as one sentence, in the band's leading slot — the
        // place a constant "This run" used to occupy.
        harness.get_by_label("Stopped — at your request");
        harness.get_by_label("Start");
    }

    #[test]
    fn idle_start_click_emits_start() {
        let view = idle_view();
        let mut clicked = None;
        let mut harness = Harness::new_ui(|ui| {
            if let Some(command) = render_status_bar(ui, &view, None, true, &Fetcher::new()) {
                clicked = Some(command);
            }
        });
        harness.get_by_label("Start watching").click();
        harness.run();
        drop(harness);
        assert_eq!(clicked, Some(Command::Start));
    }

    #[test]
    fn armed_status_bar_hides_start_and_toggle() {
        let view = watching_view();
        let harness = Harness::new_ui(|ui| {
            let _ = render_status_bar(ui, &view, None, true, &Fetcher::new());
        });
        assert!(harness.query_by_label("Start").is_none());
        assert!(harness.query_by_label("Toggle").is_none());
    }

    #[test]
    fn armed_stop_click_emits_stop() {
        let view = watching_view();
        let mut clicked = None;
        let mut harness = Harness::new_ui(|ui| {
            if let Some(command) = render_status_bar(ui, &view, None, true, &Fetcher::new()) {
                clicked = Some(command);
            }
        });
        harness.get_by_label("Stop").click();
        harness.run();
        drop(harness);
        assert_eq!(clicked, Some(Command::Stop));
    }
}
