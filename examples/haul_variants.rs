//! The header's haul row, four ways, stacked in one window so the difference is
//! a glance rather than an argument.
//!
//! **This bench has already been used, and the band has moved on.** It was
//! written to choose between these four; `Inline` won, and `statusbar::token`
//! now ships it. What is kept here is the comparison that led there, so a future
//! change to that row can be argued against the same measurements instead of
//! rediscovering them — and so the rejected options keep their reasons.
//!
//! Read it as a record, with two consequences. `Stacked` is what the band used
//! to do, not what it does. And the headline these variants stand under is built
//! the way the band built it *then* — three labels in an `Align::Max` row —
//! which the band has since replaced with a single `LayoutJob` because that row
//! sagged its small text 4px below the figure's baseline. That difference does
//! not move the numbers this bench exists to report, all of which are horizontal
//! (see [`Placed`]); it does mean the headline you see here is a period detail.
//!
//! What was under test is narrow: the row of token counts between the run's
//! headline figure and the gauge that closes the band. Measured on the window as
//! it shipped then, its labels sat on an even 12px gutter while its *values*
//! landed at x = 24, 93, 142 — gaps of 64 and 42. A tile was a `ui.vertical`, so
//! each was as wide as `max(label, value)`, and the label won by an order of
//! magnitude: `COVENANT` is 57px, its `1` is 4. The values therefore had no
//! rhythm of their own — their spacing was a by-product of how long the words
//! above them happened to be, and that row was the one carrying full ink.
//!
//! Each variant prints the x it actually placed its values at, and can draw a
//! guide down each one. Read the numbers, not the impression: this file exists
//! because the impression is what disagreed.
//!
//! Run:
//!
//! ```text
//! cargo run --example haul_variants --no-default-features --features gui
//! ```
//!
//! Not part of the shipped binary. It renders no session and holds no state
//! beyond its own two controls.
//!
//! The palette comes from the real [`theme`] by path rather than by copy: a
//! mock that carries its own hexes can drift from the window it claims to be
//! previewing, and the whole point here is a judgement about spacing under the
//! app's true fonts and metrics. The one cost is that `theme.rs`'s doc links
//! resolve against this file's root, so `cargo doc --examples` — which nothing
//! in this repo runs — would report them broken.

#[allow(dead_code)]
#[path = "../src/ui/theme.rs"]
mod theme;

use eframe::egui;

/// The shipped window's only width, so the row is judged in the space it has.
/// Not imported from `ui`: that module pins it for a window this example does
/// not open, and a second copy here cannot silently change the real one.
const WINDOW_WIDTH: f32 = 440.0;

/// How the haul row places its tokens.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Variant {
    /// What the band did before this bench: a `ui.vertical` per token, each as
    /// wide as its own label. The layout under test, not a candidate.
    Stacked,
    /// The same tiles on a common pitch — every one as wide as the widest
    /// label, so the values fall on a grid instead of behind the words.
    Grid,
    /// Label and value on one line, the arrangement `balances_strip` already
    /// uses one block higher in the same band. **This is the one that shipped**
    /// — `statusbar::token`.
    Inline,
    /// The same inline pairs, but pushed onto the headline's own row instead of
    /// given a row of their own — which is the only variant that addresses why
    /// the block tapers rather than only how its numbers are spaced.
    Beside,
}

impl Variant {
    const ALL: [Self; 4] = [Self::Stacked, Self::Grid, Self::Inline, Self::Beside];

    fn title(self) -> &'static str {
        match self {
            Self::Stacked => "Stacked — what it replaced",
            Self::Grid => "Grid — a common pitch",
            Self::Inline => "Inline — shipped",
            Self::Beside => "Beside — on the headline's row",
        }
    }
}

/// A band to lay out: the haul exactly as `ViewState` hands it over, already
/// including the `+N other` bucket, and the headline it has to share a window
/// with.
struct Sample {
    name: &'static str,
    /// What the figure counts. One of the four `view::caption` returns, and the
    /// widest of them is what decides whether [`Variant::Beside`] is possible
    /// at all — so a bench that only ever shows the middling one proves nothing.
    caption: &'static str,
    /// Between the figure and its caption: the cap the run is bounded by, or
    /// the refresh rate when nothing bounds it.
    companion: &'static str,
    tokens: &'static [(&'static str, &'static str)],
}

/// Four shapes, because every layout here is driven by text length and by
/// nothing else. A variant that only holds for the first of these does not
/// hold.
const SAMPLES: [Sample; 4] = [
    Sample {
        name: "as shipped",
        caption: "matches found",
        companion: "/ 5",
        tokens: &[("Covenant", "1"), ("Mystic", "0"), ("Other", "+1")],
    },
    Sample {
        name: "one token",
        caption: "refreshes",
        companion: "· 6.4 / min",
        tokens: &[("Covenant", "3")],
    },
    Sample {
        name: "long counts",
        caption: "matches found",
        companion: "/ 5",
        tokens: &[("Covenant", "128"), ("Mystic", "1,004"), ("Other", "+37")],
    },
    // A wide run, every piece of it reachable: `skystones spent` is the longest
    // caption the dial has, a four-figure crystal budget is a legal limit, and
    // the counts are the ones above. Nothing here is invented to make a variant
    // fail — if it fails, it fails on a run someone can have.
    //
    // It is *not* the widest reachable run, and the name is a floor rather than
    // a ceiling. `max_spend` and `max_matches` are edited through
    // `editor::optional_field`, whose range is `1..=T::MAX` on a `u32`-backed
    // currency — so `/ 4294967295` is as legal as `/ 1000`, and the figure is
    // pinned to `"2"` in `headline` where a real run reaches `1000`. Any
    // overflow this sample reports is therefore a lower bound on the real one.
    // That only strengthens the conclusion it was used for; it would not
    // support a conclusion that something *just* fits.
    Sample {
        name: "wide case",
        caption: "skystones spent",
        companion: "/ 1000",
        tokens: &[("Covenant", "128"), ("Mystic", "1,004"), ("Other", "+37")],
    },
];

/// The band's headline, reproduced above each variant so the row is judged in
/// its neighbourhood rather than on a blank page.
///
/// This mirrored `statusbar::run_band` when the bench was written and no longer
/// does: the band now lays its headline out as one `LayoutJob`, because the
/// bottom-aligned row below drops small text ~4px under the figure's baseline.
/// Deliberately left as it was — every number this bench reports is horizontal,
/// so the difference does not touch them, and rewriting the neighbourhood would
/// change what the recorded measurements were taken against.
///
/// The size is a second copy of `statusbar::FIGURE_SIZE`, unlike [`WINDOW_WIDTH`]
/// only in that nothing enforces the match: retune the band's figure and this
/// keeps measuring the old one.
const FIGURE_SIZE: f32 = 34.0;

/// The headline's three pieces, in the order and register the band gives them.
/// Placed by the caller, which owns the bounded box they stand in. Returns
/// where the caption ends — the edge the haul has to stay clear of.
fn headline(ui: &mut egui::Ui, sample: &Sample, figure: &egui::FontId) -> f32 {
    ui.label(
        egui::RichText::new("2")
            .font(figure.clone())
            .color(theme::INK),
    );
    ui.label(egui::RichText::new(sample.companion).color(theme::INK_FAINT));
    ui.label(theme::section(sample.caption)).rect.right()
}

/// The bounded box the headline stands in: one figure-line tall, bottom
/// aligned, so the companion and the caption sit on the figure's baseline
/// rather than halfway up it.
fn figure_row(ui: &mut egui::Ui, sample: &Sample) {
    let figure = egui::FontId::proportional(FIGURE_SIZE);
    let line = ui.ctx().fonts_mut(|fonts| fonts.row_height(&figure));
    ui.allocate_ui_with_layout(
        egui::vec2(ui.available_width(), line),
        egui::Layout::left_to_right(egui::Align::Max),
        |ui| headline(ui, sample, &figure),
    );
}

/// One token's value, in the register the band gives it. Returns where it
/// landed, which is the whole measurement.
fn value(ui: &mut egui::Ui, text: &str) -> f32 {
    ui.label(egui::RichText::new(text).color(theme::INK))
        .rect
        .left()
}

/// How wide `text` lays out in one of the theme's registers. Asked of the font
/// rather than guessed, so a retune of the text styles carries here.
///
/// `fonts_mut`, for the reason `statusbar` gives at its own call: laying the
/// glyphs out to answer is a mutation of the font cache.
fn text_width(ui: &egui::Ui, text: &str, style: &egui::TextStyle) -> f32 {
    let font = style.resolve(ui.style());
    ui.ctx().fonts_mut(|fonts| {
        fonts
            .layout_no_wrap(text.to_owned(), font, theme::INK)
            .size()
            .x
    })
}

/// The widest label in a haul, under the style the section header resolves to.
///
/// `TextStyle::Small` is a *restatement* of what `theme::section` does — it
/// returns `.small().weak()` — and nothing links the two. If `section` ever
/// stops being small, `Grid`'s pitch is computed from the wrong font and the
/// bench reports a tidy grid that the band would not draw. There is no way to
/// ask a `RichText` which style it resolved to without laying it out, so this is
/// a coupling to watch rather than one to fix here.
fn widest_label(ui: &egui::Ui, tokens: &[(&str, &str)]) -> f32 {
    tokens
        .iter()
        .map(|(label, _)| text_width(ui, &label.to_uppercase(), &egui::TextStyle::Small))
        .fold(0.0_f32, f32::max)
}

/// Where a variant put its values, and how it fared against the two edges it
/// has to live between.
struct Placed {
    /// The left edge of each value, in screen coordinates.
    xs: Vec<f32>,
    /// For the variants that share the headline's row. `None` for the ones that
    /// own a row, which cannot collide with anything.
    fit: Option<Fit>,
}

/// A row shared with the headline is bounded on both sides, and only measuring
/// both catches it. The first version of this checked the left gap alone,
/// reported room to spare, and was overflowing the panel on the right the whole
/// time — visible in the picture, absent from the number.
struct Fit {
    /// Pixels between the end of the caption and the start of the haul.
    gap: f32,
    /// Where the haul ends, in the same window coordinates as [`Placed::xs`].
    ///
    /// Reported as a position rather than as an overflow against some bound,
    /// because picking the bound is exactly where this went wrong twice: the
    /// allocated box's `max_rect` came back wider than the panel, so measuring
    /// against it announced a clean fit while the last token was being cut off
    /// at the window's edge. A position can be compared to
    /// [`CONTENT_RIGHT`] by anyone, and cannot quietly agree with itself.
    row_right: f32,
}

/// Where the panel's content stops: the shipped width less the side inset the
/// whole window is laid out against. Anything reported past this is off screen.
const CONTENT_RIGHT: f32 = WINDOW_WIDTH - theme::EDGE as f32;

/// The inline pairs, in the register the band gives them. Shared by
/// [`Variant::Inline`] and [`Variant::Beside`] so the two cannot drift into
/// being different arrangements that merely look alike.
fn inline_pairs(ui: &mut egui::Ui, tokens: &[(&str, &str)], xs: &mut Vec<f32>) {
    for (index, (label, count)) in tokens.iter().enumerate() {
        // The gap that groups each name with its own number, the same one
        // `balances_strip` opens between its two purses.
        if index > 0 {
            ui.add_space(theme::SP_XL);
        }
        ui.label(theme::section(label));
        xs.push(value(ui, count));
    }
}

/// Lays the haul out one way and reports where each value's left edge ended up.
fn haul_row(ui: &mut egui::Ui, variant: Variant, sample: &Sample) -> Placed {
    let tokens = sample.tokens;
    let mut placed = Vec::with_capacity(tokens.len());
    let mut fit = None;
    match variant {
        Variant::Stacked => {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = theme::SP_MD;
                for (label, count) in tokens {
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = theme::SP_XS;
                        ui.label(theme::section(label));
                        placed.push(value(ui, count));
                    });
                }
            });
        }
        Variant::Grid => {
            // `Grid` with a floor on the column width, and not a hand-rolled
            // row of fixed-size boxes: `allocate_ui_with_layout` advances the
            // cursor by the child's *content*, not by the size asked for, so
            // the obvious spelling silently gives the tiles their old uneven
            // pitch back — and staggers them down the screen while it is at it.
            //
            // The floor is the widest label, so the narrow tokens stop
            // collapsing onto the wide one's shoulder. Left as a floor rather
            // than an exact width because a count long enough to beat it should
            // still get its room; the point is a common pitch, not a cage.
            let pitch = widest_label(ui, tokens);
            egui::Grid::new("haul")
                .spacing(egui::vec2(theme::SP_MD, theme::SP_XS))
                .min_col_width(pitch)
                .show(ui, |ui| {
                    for (label, _) in tokens {
                        ui.label(theme::section(label));
                    }
                    ui.end_row();
                    for (_, count) in tokens {
                        placed.push(value(ui, count));
                    }
                    ui.end_row();
                });
        }
        Variant::Inline => {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = theme::SP_XS;
                inline_pairs(ui, tokens, &mut placed);
            });
        }
        Variant::Beside => {
            // One bounded box, one figure-line tall: `Align::Max` has to mean
            // "the bottom of this row" and not "the bottom of the window", or
            // the caption and the counts float up the figure's middle.
            //
            // The band's comment then said the tiles could not live here,
            // because a tile — a `ui.vertical` — nested in a right-to-left
            // layout is handed a near-zero width and wraps its label to one
            // letter per line. True of a tile; not true of this. There is no
            // nested `vertical` to starve — the pairs are plain labels in
            // sequence, which take their own width. That objection is gone from
            // the band too: the tiles it described no longer exist.
            let figure = egui::FontId::proportional(FIGURE_SIZE);
            let line = ui.ctx().fonts_mut(|fonts| fonts.row_height(&figure));
            ui.allocate_ui_with_layout(
                egui::vec2(ui.available_width(), line),
                egui::Layout::left_to_right(egui::Align::Max),
                |ui| {
                    let caption_right = headline(ui, sample, &figure);
                    // Right-aligned by egui, not by arithmetic of mine. The
                    // first spelling measured the haul up front and spaced into
                    // the difference — which meant reimplementing the layout's
                    // own spacing rules, and getting them wrong: it under-counted
                    // the inter-widget gaps, pushed the row off the panel, and
                    // still reported room to spare. A prediction that can
                    // disagree with the layout is worse than no prediction,
                    // because it disagrees quietly.
                    let haul =
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Max), |ui| {
                            ui.spacing_mut().item_spacing.x = theme::SP_XS;
                            // Emitted right to left, so each pair is written
                            // value first: the reversal is in the placing, not
                            // in what the row ends up reading.
                            for (index, (label, count)) in tokens.iter().enumerate().rev() {
                                if index + 1 < tokens.len() {
                                    ui.add_space(theme::SP_XL);
                                }
                                placed.push(value(ui, count));
                                ui.label(theme::section(label));
                            }
                        });
                    // Both edges, off what was actually placed. Either one alone
                    // is a number that can read healthy while the row is broken.
                    let used = haul.response.rect;
                    fit = Some(Fit {
                        gap: used.left() - caption_right,
                        row_right: used.right(),
                    });
                    // Placed right to left; reported left to right.
                    placed.reverse();
                },
            );
        }
    }
    Placed { xs: placed, fit }
}

/// The x positions as a sentence: where the values landed, and the pitch
/// between them. Relative to the band's own left, so the numbers are the ones
/// quoted against `theme::EDGE` and not screen coordinates.
fn readout(placed: &Placed, origin: f32) -> String {
    // Screen coordinates into the window's own, so every number in this bench
    // is quoted on the same scale as `theme::EDGE` and `CONTENT_RIGHT`.
    let window_x = |x: f32| x - origin + f32::from(theme::EDGE);
    let at: Vec<String> = placed
        .xs
        .iter()
        .map(|x| format!("{:.0}", window_x(*x)))
        .collect();
    let pitch: Vec<String> = placed
        .xs
        .windows(2)
        .map(|pair| format!("{:.0}", pair[1] - pair[0]))
        .collect();
    let where_ = if pitch.is_empty() {
        format!("value at x = {}", at.join(", "))
    } else {
        format!(
            "values at x = {} · pitch {}",
            at.join(", "),
            pitch.join(", ")
        )
    };
    // Spelled out rather than left to the picture: these are the numbers that
    // decide whether the variant is possible at all.
    let fit = match &placed.fit {
        None => String::new(),
        Some(fit) => {
            let right = window_x(fit.row_right);
            let verdict = if right > CONTENT_RIGHT {
                format!("SPILLS {:.0}px off screen", right - CONTENT_RIGHT)
            } else if fit.gap < 0.5 {
                "COLLIDES with the caption".to_owned()
            } else {
                "fits".to_owned()
            };
            format!(
                " · gap {:.0} · ends at {right:.0} of {CONTENT_RIGHT:.0} · {verdict}",
                fit.gap
            )
        }
    };
    format!("{where_}{fit}")
}

struct Demo {
    sample: usize,
    guides: bool,
}

impl eframe::App for Demo {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default()
            .frame(
                egui::Frame::central_panel(ui.style())
                    .inner_margin(egui::Margin::symmetric(theme::EDGE, 12)),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    for (index, sample) in SAMPLES.iter().enumerate() {
                        ui.selectable_value(&mut self.sample, index, sample.name);
                    }
                    ui.add_space(theme::SP_MD);
                    ui.checkbox(&mut self.guides, "guides");
                });
                ui.add_space(theme::SP_SM);
                theme::rule(ui, theme::HAIRLINE);

                let sample = &SAMPLES[self.sample];
                // Scrolled: the four blocks plus their readouts outgrow a
                // window pinned to the shipped width, and a readout clipped
                // off the bottom is the one line here nobody can do without.
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| self.variants(ui, sample));
            });
    }
}

impl Demo {
    /// One block per variant, each the shipped band's foot: the headline figure,
    /// the haul under test, and the gauge that closes it.
    fn variants(&self, ui: &mut egui::Ui, sample: &Sample) {
        for variant in Variant::ALL {
            ui.add_space(theme::SP_XL);
            ui.label(egui::RichText::new(variant.title()).color(theme::INK_MUTED));
            ui.add_space(theme::SP_SM);

            let origin = ui.min_rect().left();
            let top = ui.cursor().top();
            // `Beside` draws the headline itself — that *is* the variant. The
            // others get it above them, so every block is judged in the same
            // neighbourhood.
            if variant != Variant::Beside {
                figure_row(ui, sample);
                ui.add_space(theme::SP_SM);
            }
            let placed = haul_row(ui, variant, sample);
            ui.add_space(theme::SP_SM);
            theme::gauge(ui, 0.4);

            // Painted after the block, over it: a guide drawn first would sit
            // under the text it is there to be compared with.
            if self.guides {
                let bottom = ui.cursor().top();
                for x in &placed.xs {
                    ui.painter().vline(
                        *x,
                        egui::Rangef::new(top, bottom),
                        egui::Stroke::new(1.0, theme::ACCENT.gamma_multiply(0.55)),
                    );
                }
            }
            ui.add_space(theme::SP_XS);
            ui.label(
                egui::RichText::new(readout(&placed, origin))
                    .small()
                    .color(theme::INK_FAINT),
            );
        }
    }
}

/// The sample to open on, from the first argument — any prefix of a sample's
/// name, unknown or absent falling back to the first.
///
/// A bench nobody can drive without clicking is a bench that cannot be
/// captured while something else owns the foreground, which is most of the time
/// on the machine this is for.
fn opening_sample() -> usize {
    let Some(arg) = std::env::args().nth(1) else {
        return 0;
    };
    let arg = arg.to_lowercase();
    SAMPLES
        .iter()
        .position(|sample| sample.name.starts_with(arg.as_str()))
        .unwrap_or(0)
}

fn main() -> eframe::Result {
    let sample = opening_sample();
    eframe::run_native(
        "Haul row — variants",
        eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_inner_size([WINDOW_WIDTH, 860.0])
                .with_min_inner_size([WINDOW_WIDTH, 480.0])
                .with_max_inner_size([WINDOW_WIDTH, 10_000.0]),
            ..Default::default()
        },
        Box::new(move |cc| {
            theme::apply(&cc.egui_ctx);
            Ok(Box::new(Demo {
                sample,
                guides: true,
            }))
        }),
    )
}
