//! Top status bar: the status word + one contextual action on the first row,
//! and the stat-tile metrics row (balances | refreshes + per-token haul) under
//! it.

use eframe::egui;

use crate::app::Command;
use crate::domain::control::Status;
use crate::domain::shop::{Crystals, Gold};
use crate::install::{Fetcher, Progress};

use super::theme;
use super::view::ViewState;
use crate::render::amount_or_dash;

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
/// stock one — the saturated element is already Start, one row below.
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

/// Top chrome: the error banner, then two rows — status (dot + word + clause)
/// with the one contextual button on the right, and under it a row of stat
/// tiles.
#[must_use]
pub(super) fn render_status_bar(
    ui: &mut egui::Ui,
    view: &ViewState,
    outcome: Option<&str>,
    session_alive: bool,
    fetcher: &Fetcher,
) -> Option<Command> {
    let mut clicked = None;
    if let Some(outcome) = outcome {
        error_banner(ui, outcome, fetcher);
        ui.separator();
    }

    ui.add_space(theme::SP_XS);
    let color = theme::status_color(view.status_kind);
    let armed = matches!(view.status_kind, Status::Watching | Status::Paused);
    // Button width first (right-aligned), then status fills the rest, so the
    // clause has room and does not truncate.
    ui.horizontal(|ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            // Explicit, never Toggle: the 4 Hz poll can show a label 250 ms
            // stale, and a toggle raced by an auto-stop would re-arm the loop.
            let (label, command) = if armed {
                ("Stop", Command::Stop)
            } else {
                ("Start", Command::Start)
            };
            ui.add_enabled_ui(session_alive, |ui| {
                if theme::primary_button(ui, label).clicked() {
                    clicked = Some(command);
                }
            });
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                status_dot(ui, color);
                ui.add_space(theme::SP_SM);
                ui.add(
                    egui::Label::new(theme::emphasis(view.status_word).color(theme::INK))
                        .truncate(),
                );
                if let Some(hint) = view.status_hint {
                    ui.add_space(theme::SP_SM);
                    ui.add(egui::Label::new(egui::RichText::new(hint).weak()).truncate());
                }
            });
        });
    });
    // The run's readouts appear only once a run exists; while Idle they would
    // be all zeros.
    ui.add_space(theme::SP_SM);
    row_separator(ui);
    ui.add_space(theme::SP_SM);
    // Wrapped: a large Gold balance plus the haul tiles can exceed the panel's
    // minimum width, and clipping would take tiles off-screen.
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = theme::SP_SM;
        skystone_tile(ui, view.crystal_balance);
        gold_tile(ui, view.gold_balance);
        if !matches!(view.status_kind, Status::Idle) {
            // Size the divider to the tiles already laid.
            let tile_height = ui.min_size().y;
            group_divider(ui, tile_height);
            stat_tile(
                ui,
                "Refreshes",
                value_over_limit(view.progress.refreshes, view.limits.max_refreshes),
            );
            for (label, count) in &view.haul {
                stat_tile(ui, label, count.to_string());
            }
            if view.haul_others > 0 {
                stat_tile(ui, "Other", format!("+{}", view.haul_others));
            }
        }
    });
    ui.add_space(theme::SP_XS);
    clicked
}

/// The crystal balance tile. "Skystones" is the game's word; the code says
/// crystals.
///
/// A function per currency, not two `stat_tile` calls: those differed only by a
/// string literal and a `view` field, so swapping the arguments compiled and
/// mislabelled both balances. `Option<Crystals>` and `Option<Gold>` do not.
fn skystone_tile(ui: &mut egui::Ui, balance: Option<Crystals>) {
    stat_tile(ui, "Skystones", amount_or_dash(balance));
}

/// The gold balance tile — see [`skystone_tile`] for why each currency has its
/// own.
fn gold_tile(ui: &mut egui::Ui, balance: Option<Gold>) {
    stat_tile(ui, "Gold", amount_or_dash(balance));
}

/// One KPI tile: a small grey uppercase label over its value in full ink.
fn stat_tile(ui: &mut egui::Ui, label: &str, value: String) {
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = theme::SP_XS;
        ui.label(theme::section(label));
        ui.label(egui::RichText::new(value).color(theme::INK));
    });
}

/// A full-bleed hairline between the status row and the metrics row: painted
/// across the panel's clip rect, past the side margin, so it reaches the edges
/// like the tab and table rules. Dimmed, since it divides rows and not zones.
fn row_separator(ui: &mut egui::Ui) {
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 1.0), egui::Sense::hover());
    ui.painter().hline(
        ui.clip_rect().x_range(),
        rect.center().y,
        egui::Stroke::new(1.0, theme::HAIRLINE.gamma_multiply(0.5)),
    );
}

/// A group divider between the balance tiles and the counter tiles, sized to
/// the tiles rather than the full row height.
fn group_divider(ui: &mut egui::Ui, height: f32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(1.0, height), egui::Sense::hover());
    ui.painter().vline(
        rect.center().x,
        rect.y_range(),
        egui::Stroke::new(1.0, theme::HAIRLINE),
    );
}

/// A small painted status dot, not a `●` glyph, so it never depends on the
/// stock font carrying the symbol.
fn status_dot(ui: &mut egui::Ui, color: egui::Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(7.0, 7.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 3.5, color);
}

/// `3/10` against a limit, `3/—` without one.
fn value_over_limit(value: u32, limit: Option<u32>) -> String {
    match limit {
        Some(limit) => format!("{value}/{limit}"),
        None => format!("{value}/—"),
    }
}

#[cfg(test)]
mod tests {
    use egui_kittest::{Harness, kittest::Queryable};

    use crate::domain::control::{Controller, Event, Limits};
    use crate::domain::filter::Filter;

    use super::super::view::view_state;
    use super::*;

    fn idle_view() -> ViewState {
        let controller = Controller::new(Filter::default(), Limits::default());
        view_state(&controller)
    }

    fn watching_view() -> ViewState {
        let mut controller = Controller::new(Filter::matching_default_items(), Limits::default());
        let _ = controller.handle(Event::Start { now_ms: 0 });
        view_state(&controller)
    }

    #[test]
    fn value_over_limit_renders_missing_limit_as_dash() {
        assert_eq!(value_over_limit(3, Some(10)), "3/10");
        assert_eq!(value_over_limit(3, None), "3/—");
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

    #[test]
    fn idle_status_bar_hides_stop_and_toggle() {
        let view = idle_view();
        let harness = Harness::new_ui(|ui| {
            let _ = render_status_bar(ui, &view, None, true, &Fetcher::new());
        });
        assert!(harness.query_by_label("Stop").is_none());
        assert!(harness.query_by_label("Toggle").is_none());
    }

    #[test]
    fn status_bar_shows_currencies_and_a_clean_status() {
        let view = idle_view();
        let harness = Harness::new_ui(|ui| {
            let _ = render_status_bar(ui, &view, None, true, &Fetcher::new());
        });
        harness.get_by_label("SKYSTONES");
        harness.get_by_label("GOLD");
        harness.get_by_label("Idle");
        harness.get_by_label("define a filter first");
    }

    #[test]
    fn run_tiles_hidden_only_while_idle() {
        let idle = idle_view();
        let idle_bar = Harness::new_ui(|ui| {
            let _ = render_status_bar(ui, &idle, None, true, &Fetcher::new());
        });
        idle_bar.get_by_label("SKYSTONES");
        assert!(idle_bar.query_by_label("REFRESHES").is_none());

        let limits = Limits {
            max_refreshes: Some(10),
            ..Limits::default()
        };
        let mut controller = Controller::new(Filter::matching_default_items(), limits);
        let _ = controller.handle(Event::Start { now_ms: 0 });
        let armed = view_state(&controller);
        let armed_bar = Harness::new_ui(|ui| {
            let _ = render_status_bar(ui, &armed, None, true, &Fetcher::new());
        });
        for label in ["REFRESHES", "0/10", "COVENANT", "MYSTIC"] {
            armed_bar.get_by_label(label);
        }
        assert!(armed_bar.query_by_label("SPENT").is_none());
        assert!(armed_bar.query_by_label("MATCHES").is_none());

        // The final totals survive a stop: an auto-stop is exactly when the
        // player wants to read them.
        let mut controller = Controller::new(Filter::matching_default_items(), Limits::default());
        let _ = controller.handle(Event::Start { now_ms: 0 });
        let _ = controller.handle(Event::Stop);
        let stopped = view_state(&controller);
        let stopped_bar = Harness::new_ui(|ui| {
            let _ = render_status_bar(ui, &stopped, None, true, &Fetcher::new());
        });
        stopped_bar.get_by_label("REFRESHES");
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
        harness.get_by_label("Start").click();
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
