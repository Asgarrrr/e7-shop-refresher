//! The Setup surface: draft filter/limits/timings owned by the window, the
//! widgets that edit them, and the single Apply that commits the changed
//! drafts to the session. Laid out as three groups by the player's real
//! priority — Hunt (what to buy) and Stop (when to quit) always open, Click
//! timing (expert tuning) collapsed — under one primary action.

use eframe::egui;

use super::theme;
use crate::actuator::plan::{self, DelayRange, Timings};
use crate::app::Command;
use crate::domain::control::Limits;
use crate::domain::filter::{Filter, SubstatReq};
use crate::domain::shop::ItemKind;
use crate::render::kind_label;

/// Draft criteria owned by the window until Apply pushes them to the session;
/// seeded from the controller's live criteria (and the startup timings) at
/// startup. Each draft carries the last-applied copy beside it so Apply lights
/// up only on a real change and sends nothing that has not moved. Edits are
/// session-only — config.toml is never rewritten.
pub struct EditorState {
    filter: Filter,
    limits: Limits,
    timings: Timings,
    /// The values the session is actually running: a draft is "dirty" (and
    /// Apply enabled) exactly when it differs from its applied twin.
    applied_filter: Filter,
    applied_limits: Limits,
    applied_timings: Timings,
    name_input: String,
    set_input: String,
    substat_input: String,
    /// Per-section disclosure, journal-style. Hunt and Stop open on arrival (the
    /// first things a player sets); the expert Click timing block stays folded.
    hunt_open: bool,
    stop_open: bool,
    timing_open: bool,
}

impl EditorState {
    pub fn new(filter: Filter, limits: Limits, timings: Timings) -> Self {
        Self {
            applied_filter: filter.clone(),
            applied_limits: limits.clone(),
            applied_timings: timings,
            filter,
            limits,
            timings,
            name_input: String::new(),
            set_input: String::new(),
            substat_input: String::new(),
            hunt_open: true,
            stop_open: true,
            timing_open: false,
        }
    }
}

/// The whole Setup surface: three journal-style collapsible sections over one
/// Apply. Returns the commands the player committed — one per draft that
/// changed, empty until Apply fires (or while it stays disabled).
pub fn edit_setup(ui: &mut egui::Ui, editor: &mut EditorState) -> Vec<Command> {
    // The section bar trails a peek of what it holds while folded, keeping the
    // intent visible without the controls. The summary is built only when the
    // section is folded (the bar drops it once open), so an expanded Setup tab
    // doesn't re-allocate discarded strings every frame. No space is inserted
    // *between* collapsed bars: they tile on the item spacing alone so their
    // hover strips meet with no dead seam (see `theme::collapsing_section`). An
    // open body gets its own trailing space to stand off the next bar.
    let hunt = (!editor.hunt_open).then(|| hunt_summary(&editor.filter));
    section(ui, "Hunt", hunt.as_deref(), &mut editor.hunt_open);
    if editor.hunt_open {
        hunt_body(ui, editor);
        ui.add_space(theme::SP_SM);
    }
    let stop = (!editor.stop_open).then(|| stop_summary(&editor.limits));
    section(ui, "Stop", stop.as_deref(), &mut editor.stop_open);
    if editor.stop_open {
        stop_body(ui, editor);
        ui.add_space(theme::SP_SM);
    }
    let timing = (!editor.timing_open).then(|| timing_summary(&editor.timings));
    section(ui, "Click timing", timing, &mut editor.timing_open);
    if editor.timing_open {
        timing_body(ui, editor);
        ui.add_space(theme::SP_SM);
    }
    ui.add_space(theme::SP_XL);
    commit_row(ui, editor)
}

/// One collapsible section bar (journal key) plus the breathing room its open
/// body needs. `summary` (present only while folded) trails the title. Toggles
/// `open` on click.
fn section(ui: &mut egui::Ui, title: &str, summary: Option<&str>, open: &mut bool) {
    if theme::collapsing_section(ui, title, summary, *open) {
        *open = !*open;
    }
    if *open {
        ui.add_space(theme::SP_SM);
    }
}

/// One-line recap of the hunt draft for the folded Hunt bar: the labels of what
/// the loop would buy (tokens named via the haul headliners, then kinds, then a
/// count of the finer criteria), so folding hides the controls, not the intent.
fn hunt_summary(filter: &Filter) -> String {
    let mut parts: Vec<String> = Vec::new();
    for name in &filter.names {
        // Reuse the haul's wire→label map so a hunted token reads "Covenant",
        // not "ticketrare_name"; an unknown id shows verbatim.
        let mut label = name.clone();
        for (wire, headliner) in crate::render::HAUL_HEADLINERS {
            if name == wire {
                label = headliner.to_owned();
                break;
            }
        }
        parts.push(label);
    }
    for kind in &filter.kinds {
        parts.push(kind_label(*kind).to_owned());
    }
    if !filter.sets.is_empty() {
        parts.push(count_label(filter.sets.len(), "set", "sets"));
    }
    if !filter.required_substats.is_empty() {
        parts.push(count_label(
            filter.required_substats.len(),
            "substat",
            "substats",
        ));
    }
    if parts.is_empty() {
        return "nothing selected".to_owned();
    }
    // Cap the trailing summary so it never crowds the title; the body has the rest.
    let cap = 3;
    if parts.len() <= cap {
        parts.join(", ")
    } else {
        format!("{} +{}", parts[..cap].join(", "), parts.len() - cap)
    }
}

/// One-line recap of the active stop limits for the folded Stop bar; "no limits"
/// when the run is uncapped.
fn stop_summary(limits: &Limits) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(n) = limits.max_refreshes {
        parts.push(count_label(n as usize, "refresh", "refreshes"));
    }
    if let Some(n) = limits.max_spend {
        parts.push(format!("{n} crystals"));
    }
    if let Some(n) = limits.max_matches {
        parts.push(count_label(n as usize, "match", "matches"));
    }
    if let Some(ms) = limits.max_duration_ms {
        parts.push(format!("{} min", ms.div_ceil(60_000)));
    }
    if parts.is_empty() {
        "no limits".to_owned()
    } else {
        parts.join(" · ")
    }
}

/// The Click timing bar is always folded on arrival, so its peek carries the
/// most weight: whether the player tuned any extra delay on top of the tuned
/// baselines, or left them at zero.
fn timing_summary(timings: &Timings) -> &'static str {
    if *timings == Timings::default() {
        "no extra delay"
    } else {
        "custom extra delay"
    }
}

/// `n singular` / `n plural`, e.g. `1 refresh` / `3 refreshes`.
fn count_label(n: usize, singular: &str, plural: &str) -> String {
    format!("{n} {}", if n == 1 { singular } else { plural })
}

/// Hunt: the item-interest criteria — what the loop buys. Open on arrival:
/// without at least one criterion the loop refuses to arm, so this is the first
/// thing the player sets.
fn hunt_body(ui: &mut egui::Ui, editor: &mut EditorState) {
    ui.horizontal(|ui| {
        // Unknown included: a config-seeded criterion must always be visible
        // and clearable.
        for kind in [
            ItemKind::Equipment,
            ItemKind::Hero,
            ItemKind::Token,
            ItemKind::Unknown,
        ] {
            let mut on = editor.filter.kinds.contains(&kind);
            if ui.checkbox(&mut on, kind_label(kind)).changed() {
                if on {
                    editor.filter.kinds.push(kind);
                } else {
                    editor.filter.kinds.retain(|kept| *kept != kind);
                }
            }
        }
    });
    ui.add_space(theme::SP_SM);
    string_list(
        ui,
        "names (exact internal ids)",
        &mut editor.filter.names,
        &mut editor.name_input,
    );
    quick_add_names(ui, &mut editor.filter.names);
    string_list(
        ui,
        "sets (exact internal ids)",
        &mut editor.filter.sets,
        &mut editor.set_input,
    );
    substat_reqs(
        ui,
        &mut editor.filter.required_substats,
        &mut editor.substat_input,
    );
    ui.add_space(theme::SP_XS);
    egui::Grid::new("hunt-numerics")
        .num_columns(2)
        .spacing([theme::SP_SM, theme::SP_XS])
        .show(ui, |ui| {
            optional_value(ui, "min substats", &mut editor.filter.min_substats, 1);
            ui.end_row();
            // Seeded above the covenant-bookmark price so a fresh cap still
            // matches the default hunt targets.
            optional_value(
                ui,
                "max price (gold)",
                &mut editor.filter.max_price,
                300_000,
            );
            ui.end_row();
        });
    ui.add_space(theme::SP_XS);
    ui.checkbox(&mut editor.filter.include_sold_out, "include sold out");
}

/// One-click add for the two tokens ~90% of players hunt (covenant bookmark,
/// mystic medal), spelling their internal ids so the player never types a
/// `ticketrare_name`. Reuses the haul's headliner table — one wire→label map.
fn quick_add_names(ui: &mut egui::Ui, names: &mut Vec<String>) {
    ui.horizontal(|ui| {
        ui.weak("quick add");
        for (wire, label) in crate::render::HAUL_HEADLINERS {
            let present = names.iter().any(|name| name == wire);
            if ui
                .add_enabled(!present, egui::Button::new(format!("+ {label}")))
                .clicked()
            {
                names.push(wire.to_owned());
            }
        }
    });
}

/// Stop: the run's safety rails. A uniform numeric block, so it lays in a grid
/// — the checkboxes and their values line up in two columns instead of drifting
/// with each label's width.
fn stop_body(ui: &mut egui::Ui, editor: &mut EditorState) {
    egui::Grid::new("stop-limits")
        .num_columns(2)
        .spacing([theme::SP_SM, theme::SP_XS])
        .show(ui, |ui| {
            optional_value(ui, "max refreshes", &mut editor.limits.max_refreshes, 10);
            ui.end_row();
            optional_value(ui, "max spend (crystals)", &mut editor.limits.max_spend, 30);
            ui.end_row();
            optional_value(ui, "max matches", &mut editor.limits.max_matches, 5);
            ui.end_row();
            duration_minutes(ui, &mut editor.limits.max_duration_ms);
            ui.end_row();
        });
}

/// Click timing: each click waits a fixed tuned delay, plus a random extra the
/// player dials in on top so the loop never clicks like a metronome. One
/// draggable bar per action on a shared time ruler — a solid segment for the
/// fixed wait, a bright segment for the random extra — grouped by phase. Folded
/// by default (via `edit_setup`).
fn timing_body(ui: &mut egui::Ui, editor: &mut EditorState) {
    let t = &mut editor.timings;
    ui.weak("Every click waits a fixed tuned delay, plus a random extra you add — drag a bar to give that click more human slack.");
    ui.add_space(theme::SP_SM);
    timing_legend(ui);
    ui.add_space(theme::SP_SM);
    routine_timeline(ui, t);
    ui.add_space(theme::SP_SM);
    timing_group(
        ui,
        "Open & refresh",
        &mut [
            ("shop opens", &mut t.shop_opened, plan::WAIT_SHOP_OPENED_MS),
            ("paid refresh", &mut t.refreshed, plan::WAIT_REFRESHED_MS),
            (
                "confirm refresh",
                &mut t.confirm_refresh_modal,
                plan::WAIT_CONFIRM_REFRESH_MODAL_MS,
            ),
        ],
    );
    timing_group(
        ui,
        "Buy",
        &mut [
            ("confirm buy", &mut t.buy_modal, plan::WAIT_BUY_MODAL_MS),
            (
                "between buys",
                &mut t.between_buys,
                plan::WAIT_BETWEEN_BUYS_MS,
            ),
            (
                "after a scroll",
                &mut t.scroll_settle,
                plan::WAIT_SCROLL_SETTLE_MS,
            ),
            (
                "after a purchase",
                &mut t.purchase_resumed,
                plan::WAIT_PURCHASE_RESUMED_MS,
            ),
        ],
    );
    timing_group(
        ui,
        "Recovery",
        &mut [("watchdog re-issue", &mut t.recovery, plan::WAIT_RECOVERY_MS)],
    );
}

/// Names the two segments of every meter so the bars read at a glance: a muted
/// swatch for the fixed tuned wait, a bright one for the random extra.
fn timing_legend(ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        legend_swatch(ui, theme::METER_BASE, "fixed tuned wait");
        ui.add_space(theme::SP_XL);
        legend_swatch(ui, theme::ACCENT, "random extra");
    });
}

/// One legend entry: a small rounded colour chip followed by its label.
fn legend_swatch(ui: &mut egui::Ui, color: egui::Color32, label: &str) {
    let (chip, _) = ui.allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
    ui.painter()
        .rect_filled(chip, egui::CornerRadius::same(3), color);
    ui.weak(label);
}

/// The distinct waits a single find-and-buy pass strings together, in click
/// order: the paid refresh, its confirm, the buy (the wait before its confirm),
/// and the resume. Shop-open (once), scroll / between-buys (multi-item) and the
/// watchdog (only on a miss) sit outside this steady loop, so the timeline stays
/// an honest "typical pass".
const ROUTINE: [(&str, u64); 4] = [
    ("refresh", plan::WAIT_REFRESHED_MS),
    ("confirm", plan::WAIT_CONFIRM_REFRESH_MODAL_MS),
    ("buy", plan::WAIT_BUY_MODAL_MS),
    ("resume", plan::WAIT_PURCHASE_RESUMED_MS),
];

/// A one-glance summary: the whole find-and-buy pass as a single stacked bar,
/// each action a segment sized to its share of the total, with the summed wait
/// on the right — so the player sees where the routine's time goes, not just the
/// eight rows in isolation. Uses baselines only (the fixed skeleton); the range
/// each row adds is tuned below.
fn routine_timeline(ui: &mut egui::Ui, t: &Timings) {
    // The per-pass extra each action can add, paired to ROUTINE by position, so
    // the total reads as a range when the player has dialled in any slack.
    let slack = [
        t.refreshed,
        t.confirm_refresh_modal,
        t.buy_modal,
        t.purchase_resumed,
    ];
    let base_total: u64 = ROUTINE.iter().map(|(_, b)| b).sum();
    let hi_total: u64 = base_total + slack.iter().map(|r| r.max_ms.max(r.min_ms)).sum::<u64>();

    ui.horizontal(|ui| {
        ui.label(theme::section("one refresh + buy"));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.weak(secs_range(base_total, hi_total));
        });
    });
    ui.add_space(theme::SP_XS);

    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 26.0), egui::Sense::hover());
    let painter = ui.painter().clone();
    painter.rect_filled(rect, egui::CornerRadius::same(4), theme::HAIRLINE);
    // Each action is its own rounded pill sized by its *resolved* share (baseline
    // + the slack dialled in), so dragging a bar below visibly rebalances this
    // summary. A clear gap between pills makes the action boundaries unmistakable;
    // inside a pill the fill uses the same two tones as the bars and the legend —
    // muted `METER_BASE` for the fixed baseline, bright `ACCENT` for the random
    // extra — so the colours mean the same thing everywhere, and the split reads
    // as "within this action" rather than a new segment.
    let scale = hi_total.max(1) as f32;
    const GAP: f32 = 4.0;
    let last = ROUTINE.len() - 1;
    let mut x = rect.left();
    for (i, (name, base)) in ROUTINE.iter().enumerate() {
        let extra = slack[i].max_ms.max(slack[i].min_ms);
        let seg_ms = base + extra;
        let seg_w = rect.width() * (seg_ms as f32 / scale);
        // Each pill ends a gap short of the next (the last reaches the edge).
        let pill_end = if i == last {
            rect.right()
        } else {
            x + seg_w - GAP
        };
        let split = x + (pill_end - x) * (*base as f32 / seg_ms.max(1) as f32);
        let has_extra = extra > 0 && pill_end > split;
        let round = egui::CornerRadius::same(5);
        let square = egui::CornerRadius::ZERO;
        // Muted fixed head of the pill: left corners always round; right corners
        // round too only when there is no bright tail to close the pill.
        painter.rect_filled(
            egui::Rect::from_min_max(egui::pos2(x, rect.top()), egui::pos2(split, rect.bottom())),
            egui::CornerRadius {
                ne: if has_extra { 0 } else { 5 },
                se: if has_extra { 0 } else { 5 },
                ..round
            },
            theme::METER_BASE,
        );
        if has_extra {
            // Bright random tail closes the pill's right corners.
            painter.rect_filled(
                egui::Rect::from_min_max(
                    egui::pos2(split, rect.top()),
                    egui::pos2(pill_end, rect.bottom()),
                ),
                egui::CornerRadius {
                    ne: 5,
                    se: 5,
                    ..square
                },
                theme::ACCENT,
            );
        }
        // Name centred over the pill when it is wide enough to hold it.
        if pill_end - x > 52.0 {
            painter.text(
                egui::pos2((x + pill_end) * 0.5, rect.center().y),
                egui::Align2::CENTER_CENTER,
                *name,
                egui::FontId::new(11.0, egui::FontFamily::Proportional),
                theme::INK,
            );
        }
        x += seg_w;
    }
    ui.add_space(theme::SP_SM);
}

/// The meter height and the fixed time ruler the bars sit on. The ruler is
/// constant (not fitted to the values) so a bar's length is a stable reading of
/// its wait and every row compares on the same scale; it clears the longest
/// baseline with room to drag real slack on top. The width is the row's — the
/// bars fill to the content edge, aligned under a fixed label column.
const METER_H: f32 = 22.0;
const RULER_MS: f32 = 2_500.0;
/// The label column: wide enough for the longest action name ("watchdog
/// re-issue") so every bar starts at the same x. Painted in an exact-size box so
/// a long label can never grow the column and shove its bar out of alignment.
const LABEL_W: f32 = 150.0;
/// The resolved-time column to the right of every bar: fixed so the values form
/// an aligned column and never sit over the bar or its grip.
const VALUE_W: f32 = 96.0;

/// One phase of the timing tab: a small-caps header over its bars.
fn timing_group(ui: &mut egui::Ui, title: &str, rows: &mut [(&str, &mut DelayRange, u64)]) {
    ui.label(theme::section(title));
    ui.add_space(theme::SP_XS);
    for (label, value, baseline) in rows.iter_mut() {
        timing_row(ui, label, value, *baseline);
    }
    ui.add_space(theme::SP_SM);
}

/// One action row: a fixed-width label column, the bar filling the middle, and a
/// fixed-width resolved-time column on the right — so every bar aligns, and the
/// values line up in their own column instead of floating over the bars.
fn timing_row(ui: &mut egui::Ui, label: &str, value: &mut DelayRange, baseline: u64) {
    ui.horizontal(|ui| {
        // Exact-size box + painted label: a plain `ui.label` grows its cell to
        // the text, so the longest name would push its bar right of the others.
        // Allocating the column and painting into it pins every bar's start.
        let (label_rect, _) =
            ui.allocate_exact_size(egui::vec2(LABEL_W, METER_H), egui::Sense::hover());
        ui.painter().with_clip_rect(label_rect).text(
            egui::pos2(label_rect.left(), label_rect.center().y),
            egui::Align2::LEFT_CENTER,
            label,
            egui::FontId::new(14.0, egui::FontFamily::Proportional),
            theme::INK_MUTED,
        );
        let bar_w = (ui.available_width() - VALUE_W - theme::SP_SM).max(80.0);
        timing_meter(ui, bar_w, baseline, value);
        ui.allocate_ui_with_layout(
            egui::vec2(VALUE_W, METER_H),
            egui::Layout::right_to_left(egui::Align::Center),
            |ui| {
                ui.monospace(resolved_band(baseline, value));
            },
        );
    });
}

/// One action's bar: a draggable meter on the shared ruler. The tuned baseline
/// is a muted fixed segment; dragging past it grows the bright random-extra
/// segment (the `max` of the range, drawn fresh in `[min, max]` at runtime).
/// Drag-to-set replaces the old min/max boxes — one gesture, and the bar is the
/// control. The resolved wait is shown in the row's value column, not inside, so
/// the grip never cuts through it.
fn timing_meter(ui: &mut egui::Ui, width: f32, baseline: u64, value: &mut DelayRange) {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(width, METER_H), egui::Sense::click_and_drag());

    // Drag or click sets the random extra: the pointer's x is the target total
    // wait on the ruler, and the slack is whatever sits past the fixed baseline
    // (never negative, never past the ruler's end).
    if let Some(pos) = response.interact_pointer_pos() {
        let frac = ((pos.x - rect.left()) / rect.width()).clamp(0.0, 1.0);
        let target_ms = frac * RULER_MS;
        let slack = (target_ms - baseline as f32).clamp(0.0, RULER_MS - baseline as f32);
        value.max_ms = slack.round() as u64;
        // Keep the invariant a config floor could otherwise break: min never
        // exceeds the max the player just set.
        value.min_ms = value.min_ms.min(value.max_ms);
    }
    if response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
    }

    let painter = ui.painter().clone();
    let radius = egui::CornerRadius::same(4);
    painter.rect_filled(rect, radius, theme::HAIRLINE);
    let total = baseline + value.max_ms.max(value.min_ms);
    let base_w = rect.width() * (baseline as f32 / RULER_MS);
    let total_w = rect.width() * (total as f32 / RULER_MS);
    painter.rect_filled(
        egui::Rect::from_min_size(rect.min, egui::vec2(base_w, rect.height())),
        radius,
        theme::METER_BASE,
    );
    if total_w > base_w {
        painter.rect_filled(
            egui::Rect::from_min_max(
                egui::pos2(rect.left() + base_w, rect.top()),
                egui::pos2(rect.left() + total_w, rect.bottom()),
            ),
            radius,
            theme::ACCENT,
        );
    }
    // Faint second-marks so the empty right of a bar reads as a time ruler you
    // drag along, not dead space. Painted over the fills, subtle enough not to
    // compete with them.
    for mark_ms in [1_000.0_f32, 2_000.0] {
        let x = rect.left() + rect.width() * (mark_ms / RULER_MS);
        painter.vline(
            x,
            rect.y_range(),
            egui::Stroke::new(1.0, egui::Color32::from_white_alpha(18)),
        );
    }
    // The grip sits at the draggable edge (the end of the slack, or the baseline
    // when there is none) — a bright cap that reads as "grab here to add slack".
    let grip_x = (rect.left() + total_w).clamp(rect.left() + 2.0, rect.right() - 2.0);
    painter.rect_filled(
        egui::Rect::from_min_max(
            egui::pos2(grip_x - 2.0, rect.top() + 2.0),
            egui::pos2(grip_x + 2.0, rect.bottom() - 2.0),
        ),
        egui::CornerRadius::same(2),
        theme::INK,
    );
}

/// The resolved wait the game will actually take: `baseline + min` to
/// `baseline + max`, in seconds. A point range (no slack, or a reversed one)
/// collapses to a single figure, matching the draw.
fn resolved_band(baseline: u64, value: &DelayRange) -> String {
    let lo = baseline + value.min_ms;
    let hi = baseline + value.max_ms.max(value.min_ms);
    secs_range(lo, hi)
}

/// `lo..hi` milliseconds as seconds; a zero-width range shows one figure. The
/// one place the timing UI turns ms into the `x.xx s` / `x.xx–y.yy s` reading,
/// shared by the per-action bars and the routine total.
fn secs_range(lo_ms: u64, hi_ms: u64) -> String {
    if lo_ms == hi_ms {
        format!("{:.2} s", lo_ms as f64 / 1000.0)
    } else {
        format!(
            "{:.2}–{:.2} s",
            lo_ms as f64 / 1000.0,
            hi_ms as f64 / 1000.0
        )
    }
}

/// The single commit: one primary Apply that sends every draft that moved and
/// re-seeds its applied twin. Disabled until something changed — and, when the
/// filter is the change, until it is restricted enough to arm (an unrestricted
/// filter the loop would refuse never reaches the session). Timing/limit-only
/// edits apply even while the filter sits unrestricted, since the domain only
/// gates arming on the filter.
fn commit_row(ui: &mut egui::Ui, editor: &mut EditorState) -> Vec<Command> {
    let dirty_filter = editor.filter != editor.applied_filter;
    let dirty_limits = editor.limits != editor.applied_limits;
    let dirty_timings = editor.timings != editor.applied_timings;
    let dirty = dirty_filter || dirty_limits || dirty_timings;
    // Only a *changed* filter must clear the arming bar; an already-applied
    // restricted filter lets limit/timing edits through untouched.
    let blocked = dirty_filter && editor.filter.is_unrestricted();

    let mut commands = Vec::new();
    ui.horizontal(|ui| {
        if blocked {
            ui.weak("add at least one hunt criterion before Apply");
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let clicked = ui
                .add_enabled_ui(dirty && !blocked, |ui| theme::primary_button(ui, "Apply"))
                .inner
                .clicked();
            if clicked {
                if dirty_filter {
                    commands.push(Command::SetFilter(editor.filter.clone()));
                    editor.applied_filter = editor.filter.clone();
                }
                if dirty_limits {
                    commands.push(Command::SetLimits(editor.limits.clone()));
                    editor.applied_limits = editor.limits.clone();
                }
                if dirty_timings {
                    commands.push(Command::SetTimings(editor.timings));
                    editor.applied_timings = editor.timings;
                }
            }
        });
    });
    ui.add_space(theme::SP_XS);
    ui.weak("edits apply to this session only — config.toml is unchanged");
    commands
}

/// Row remove control: a `✕` on a 24px-square target. `small_button` gave an
/// ~18px hit area that was easy to miss when pruning a list.
fn remove_button(ui: &mut egui::Ui) -> egui::Response {
    ui.add(egui::Button::new("✕").min_size(egui::vec2(24.0, 24.0)))
}

/// One editable any-of list: entries with a remove cross plus an add row.
fn string_list(ui: &mut egui::Ui, label: &str, values: &mut Vec<String>, input: &mut String) {
    ui.label(label);
    let mut removed = None;
    for (index, value) in values.iter().enumerate() {
        // Content-keyed row ids (duplicates are rejected on add): focus and
        // edit state survive a removal above the row.
        ui.push_id(egui::Id::new(value), |ui| {
            ui.horizontal(|ui| {
                ui.monospace(value);
                if remove_button(ui).clicked() {
                    removed = Some(index);
                }
            });
        });
    }
    if let Some(index) = removed {
        values.remove(index);
    }
    ui.horizontal(|ui| {
        ui.text_edit_singleline(input);
        if ui.button("add").clicked() {
            let value = input.trim();
            if !value.is_empty() && !values.iter().any(|kept| kept == value) {
                values.push(value.to_owned());
                input.clear();
            }
        }
    });
}

/// Required-substat rows: name, optional min threshold, remove cross.
fn substat_reqs(ui: &mut egui::Ui, reqs: &mut Vec<SubstatReq>, input: &mut String) {
    ui.label("required substats");
    let mut removed = None;
    for (index, req) in reqs.iter_mut().enumerate() {
        let row_id = egui::Id::new(&req.name);
        ui.push_id(row_id, |ui| {
            ui.horizontal(|ui| {
                ui.monospace(&req.name);
                let mut has_min = req.min.is_some();
                ui.checkbox(&mut has_min, "min");
                if has_min {
                    ui.add(egui::DragValue::new(req.min.get_or_insert(1.0)).speed(0.5));
                } else {
                    req.min = None;
                }
                if remove_button(ui).clicked() {
                    removed = Some(index);
                }
            });
        });
    }
    if let Some(index) = removed {
        reqs.remove(index);
    }
    ui.horizontal(|ui| {
        ui.text_edit_singleline(input);
        if ui.button("add").clicked() {
            let name = input.trim();
            if !name.is_empty() && !reqs.iter().any(|req| req.name == name) {
                reqs.push(SubstatReq {
                    name: name.to_owned(),
                    min: None,
                });
                input.clear();
            }
        }
    });
}

/// Checkbox-gated numeric criterion, laid as two grid cells (label, value) so a
/// column of them lines up. Unchecked means "no constraint", expressed by the
/// unchecked box — never a 0. A freshly checked box seeds a non-zero value and
/// dragging is floored at 1, but `clamp_existing_to_range` is off: a value
/// already present (e.g. a `max_refreshes = 0` seeded from config.toml) is shown
/// as-is, not silently rewritten to 1 on the first render — which would desync
/// the draft and make Apply send a value the player never chose.
fn optional_value<T: egui::emath::Numeric>(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut Option<T>,
    seed: T,
) {
    let mut on = value.is_some();
    ui.checkbox(&mut on, label);
    if on {
        ui.add(
            egui::DragValue::new(value.get_or_insert(seed))
                .range(T::from_f64(1.0)..=T::MAX)
                .clamp_existing_to_range(false),
        );
    } else {
        *value = None;
    }
}

/// The duration limit, edited in whole minutes (stored as ms). A grid row like
/// `optional_value`, kept apart for its minute↔ms conversion.
fn duration_minutes(ui: &mut egui::Ui, value: &mut Option<u64>) {
    let mut on = value.is_some();
    ui.checkbox(&mut on, "max duration (minutes)");
    if on {
        let ms = value.get_or_insert(60 * 60_000);
        // Ceil so a sub-minute config value never reads as 0; edits are whole
        // minutes (the player-facing unit) and only rewrite the stored value
        // when the player actually drags.
        let mut minutes = ms.div_ceil(60_000);
        if ui
            .add(egui::DragValue::new(&mut minutes).range(1..=u64::MAX / 60_000))
            .changed()
        {
            *ms = minutes.saturating_mul(60_000);
        }
    } else {
        *value = None;
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use egui_kittest::{Harness, kittest::Queryable};

    use super::*;

    fn named_filter() -> Filter {
        Filter {
            names: vec!["ticketrare_name".to_owned()],
            ..Filter::default()
        }
    }

    /// Drive `edit_setup` once, capturing whatever Apply committed. `run` settles
    /// over several frames and the final one is a quiet no-click, so only a
    /// non-empty commit is latched — the last frame must not wipe it.
    fn run_setup(editor: &mut EditorState) -> Vec<Command> {
        let sent = RefCell::new(Vec::new());
        let mut harness = Harness::new_ui(|ui| {
            let commands = edit_setup(ui, editor);
            if !commands.is_empty() {
                *sent.borrow_mut() = commands;
            }
        });
        harness.get_by_label("Apply").click();
        harness.run();
        drop(harness);
        sent.into_inner()
    }

    #[test]
    fn apply_sends_only_the_changed_draft() {
        // Applied twin is the default filter; the dirty draft is the named one,
        // so Apply commits exactly SetFilter — limits and timings never moved.
        let mut editor = EditorState::new(Filter::default(), Limits::default(), Timings::default());
        editor.filter = named_filter();
        assert_eq!(
            run_setup(&mut editor),
            vec![Command::SetFilter(named_filter())]
        );
    }

    #[test]
    fn apply_inert_while_nothing_changed() {
        let mut editor = EditorState::new(named_filter(), Limits::default(), Timings::default());
        assert!(run_setup(&mut editor).is_empty());
    }

    #[test]
    fn apply_blocked_while_the_dirty_filter_is_unrestricted() {
        // Clearing the only criterion leaves the draft dirty but unrestricted:
        // the loop would refuse it, so Apply must not send it.
        let mut editor = EditorState::new(named_filter(), Limits::default(), Timings::default());
        editor.filter.names.clear();
        assert!(editor.filter.is_unrestricted());
        assert!(run_setup(&mut editor).is_empty());
    }

    #[test]
    fn quick_add_seeds_a_hunt_token() {
        let mut editor = EditorState::new(Filter::default(), Limits::default(), Timings::default());
        let mut harness = Harness::new_ui(|ui| {
            edit_setup(ui, &mut editor);
        });
        harness.get_by_label("+ Covenant").click();
        harness.run();
        drop(harness);
        assert_eq!(editor.filter.names, vec!["ticketrare_name".to_owned()]);
    }

    #[test]
    fn kind_checkbox_updates_the_draft() {
        let mut editor = EditorState::new(named_filter(), Limits::default(), Timings::default());
        let mut harness = Harness::new_ui(|ui| {
            edit_setup(ui, &mut editor);
        });
        harness.get_by_label("Token").click();
        harness.run();
        drop(harness);
        assert!(editor.filter.kinds.contains(&ItemKind::Token));
    }

    #[test]
    fn timing_section_folds_its_body_until_opened() {
        // Click timing arrives folded, so its rows are hidden; clicking the bar
        // reveals them (a phase header is a body-only label). While folded, the
        // bar's accessible name trails its summary peek.
        let mut editor = EditorState::new(named_filter(), Limits::default(), Timings::default());
        let mut harness = Harness::new_ui(|ui| {
            edit_setup(ui, &mut editor);
        });
        assert!(harness.query_by_label("OPEN & REFRESH").is_none());
        harness
            .get_by_label("Click timing · no extra delay")
            .click();
        harness.run();
        harness.get_by_label("OPEN & REFRESH");
    }

    #[test]
    fn collapsed_sections_tile_with_no_hover_gap() {
        // Folded section bars must meet edge-to-edge: a gap between their
        // hit/fill rects leaves a dead seam where hovering lights a bar the
        // pointer is not over (the fill covers only the inner bar while egui
        // hit-tests wider). Their rects abutting is what keeps the hover strip
        // continuous and always under the cursor.
        let mut editor = EditorState::new(named_filter(), Limits::default(), Timings::default());
        editor.hunt_open = false;
        editor.stop_open = false;
        editor.timing_open = false;
        let mut harness = Harness::new_ui(|ui| {
            ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);
            edit_setup(ui, &mut editor);
        });
        harness.run();
        let hunt = harness.get_by_label("Hunt · Covenant").rect();
        let stop = harness.get_by_label("Stop · no limits").rect();
        let click = harness.get_by_label("Click timing · no extra delay").rect();
        assert_eq!(hunt.max.y, stop.min.y, "Hunt and Stop must tile");
        assert_eq!(stop.max.y, click.min.y, "Stop and Click timing must tile");
    }

    #[test]
    fn hunt_summary_names_the_hunted_tokens() {
        // A folded Hunt bar peeks what the loop would buy: the covenant token
        // reads by its haul label, not its wire id.
        assert_eq!(hunt_summary(&named_filter()), "Covenant");
        assert_eq!(hunt_summary(&Filter::default()), "nothing selected");
    }

    #[test]
    fn stop_summary_lists_active_limits() {
        let limits = Limits {
            max_refreshes: Some(1),
            max_matches: Some(5),
            ..Limits::default()
        };
        assert_eq!(stop_summary(&limits), "1 refresh · 5 matches");
        assert_eq!(stop_summary(&Limits::default()), "no limits");
    }

    #[test]
    fn seeded_zero_limit_is_not_silently_clamped() {
        // A config-seeded 0 (max_refreshes = 0 halts at the first check) must
        // survive rendering unchanged; the old DragValue clamp rewrote it to
        // 1, so Apply sent a limit the player never set.
        let limits = Limits {
            max_refreshes: Some(0),
            ..Limits::default()
        };
        let mut editor = EditorState::new(named_filter(), limits, Timings::default());
        let mut harness = Harness::new_ui(|ui| {
            edit_setup(ui, &mut editor);
        });
        harness.run();
        drop(harness);
        assert_eq!(editor.limits.max_refreshes, Some(0));
    }

    #[test]
    fn apply_sends_a_changed_limit() {
        let mut editor = EditorState::new(named_filter(), Limits::default(), Timings::default());
        editor.limits.max_refreshes = Some(7);
        let expected = Limits {
            max_refreshes: Some(7),
            ..Limits::default()
        };
        // The filter is unchanged from its applied twin, so only the limit ships.
        assert_eq!(run_setup(&mut editor), vec![Command::SetLimits(expected)]);
    }

    #[test]
    fn apply_sends_changed_timings() {
        let mut editor = EditorState::new(named_filter(), Limits::default(), Timings::default());
        let timings = Timings {
            refreshed: DelayRange {
                min_ms: 200,
                max_ms: 800,
            },
            ..Timings::default()
        };
        editor.timings = timings;
        assert_eq!(run_setup(&mut editor), vec![Command::SetTimings(timings)]);
    }
}
