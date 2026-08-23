//! Player-facing text for domain state: one wording shared by the console
//! and the window.

#[cfg(feature = "gui")]
use crate::domain::control::Haul;
use crate::domain::control::{Controller, RefusalReason, Status, StopReason};
use crate::domain::shop::{ItemKind, ShopItem, ShopSnapshot};

/// The hunt tokens worth naming in the haul readout — the covenant bookmark and
/// mystic medal most players chase. Wire name → display label, in headline
/// order; every other bought item is bucketed into "+N other".
#[cfg(feature = "gui")]
pub(crate) const HAUL_HEADLINERS: [(&str, &str); 2] = [
    ("ticketrare_name", "Covenant"),
    ("ticketspecial_name", "Mystic"),
];

/// The haul as the readout shows it: the headline tokens with their counts
/// (shown even at zero), and the count of everything else bought this run.
#[cfg(feature = "gui")]
pub(crate) fn haul_tally(haul: &Haul) -> ([(&'static str, u32); HAUL_HEADLINERS.len()], u32) {
    let named = HAUL_HEADLINERS.map(|(wire, label)| (label, haul.count(wire)));
    let known = HAUL_HEADLINERS.map(|(wire, _)| wire);
    (named, haul.others(&known))
}

pub(crate) fn kind_label(kind: ItemKind) -> &'static str {
    match kind {
        ItemKind::Equipment => "Equipment",
        ItemKind::Hero => "Hero",
        ItemKind::Token => "Token",
        ItemKind::Unknown => "?",
    }
}

/// Thousands-grouped decimal (`1234567` -> `1,234,567`); the stdlib has no
/// locale-free grouping to lean on.
///
/// Reached only through `Display for Gold` and `Display for Crystals`, never at
/// a print site: called by hand at each place that shows an amount, one of them
/// forgets, and a run shows `250000` next to `250,000` in the same window. Stays
/// a free function because the grouping rule is a display concern this module
/// owns.
pub(crate) fn grouped(n: u32) -> String {
    // Digits into a fixed buffer (`u32::MAX` is ten digits, so it always fits)
    // rather than through `n.to_string()`, which allocates a second `String`
    // purely to iterate it. Every price cell and balance tile hits this on the
    // repaint path.
    let mut digits = ['0'; 10];
    let mut len = 0;
    let mut rest = n;
    loop {
        digits[len] = char::from_digit(rest % 10, 10).unwrap_or('0');
        len += 1;
        rest /= 10;
        if rest == 0 {
            break;
        }
    }
    let mut out = String::with_capacity(len + (len - 1) / 3);
    for index in 0..len {
        if index > 0 && (len - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digits[len - 1 - index]);
    }
    out
}

/// An amount, or an em-dash while it is unknown: the slot table's price column,
/// which is its only caller.
///
/// It used to be shared with the status bar's balances, and the split is the
/// point rather than an oversight. A dash holds a column's width open, which a
/// table needs and a strip does not — rendered in the strip it produced
/// "— skystones · — gold", punctuation outweighing information. The strip says
/// what it is waiting for in a sentence instead (`statusbar::balances_strip`),
/// so this policy is now the table's alone.
///
/// Generic over `Display`, not `u32`: grouping is the currency's own rendering
/// (`impl Display for Gold`/`Crystals`), so this function's only job is the
/// em-dash. A sealed `Amount` trait to block other types was considered and
/// rejected — every call site already reads a typed field out of the domain.
#[cfg(feature = "gui")]
pub(crate) fn amount_or_dash(value: Option<impl std::fmt::Display>) -> String {
    value.map_or_else(|| "—".to_owned(), |amount| amount.to_string())
}

/// The state word and an optional clause, split so the window can weight them —
/// word in the severity color, clause muted — while the console joins them via
/// `status_label`. The hint only offers "start" where the domain would actually
/// arm: an unrestricted filter reads "define a filter first".
pub(crate) fn status_summary(controller: &Controller) -> (&'static str, Option<&'static str>) {
    let unrestricted = controller.filter().is_unrestricted();
    match controller.status() {
        Status::Idle if unrestricted => ("Idle", Some("define a filter first")),
        Status::Idle => ("Idle", Some("ready to start")),
        Status::Watching => ("Watching", None),
        // An empty checklist never auto-resumes.
        Status::Paused if controller.checklist().is_empty() => {
            ("Paused", Some("buy, then refresh"))
        }
        // No dash inside the clause: the caller joins word and clause with one,
        // and a second turned the line into three fragments.
        Status::Paused => ("Paused", Some("buy what you want, it resumes on its own")),
        Status::Stopped(reason) => ("Stopped", Some(stop_reason_label(reason))),
    }
}

/// One-line status for the console: the summary, joined with an em dash.
pub(crate) fn status_label(controller: &Controller) -> String {
    match status_summary(controller) {
        (word, Some(hint)) => format!("{word} — {hint}"),
        (word, None) => word.to_owned(),
    }
}

/// Why the domain refused to arm, in the player's words. Named `*_label` like
/// its siblings because it is imported bare into `session/mod.rs`, where a lone
/// `refusal` would read as `Action::Refused`'s payload or `pcap`'s `Refusal`.
pub(crate) fn refusal_label(reason: RefusalReason) -> &'static str {
    match reason {
        RefusalReason::UnrestrictedFilter => {
            "define at least one filter criterion — an empty filter matches everything"
        }
    }
}

/// Why a hunt stopped, in the player's words — the clause `status_summary` puts
/// beside "Stopped", and the line the journal reports a halt with.
pub(crate) fn stop_reason_label(reason: StopReason) -> &'static str {
    match reason {
        // Not "player stopped": the caller prefixes "Stopped — ", and a clause
        // that repeats its own word says nothing twice.
        StopReason::PlayerStopped => "at your request",
        // The distinction this variant exists for, in the player's words —
        // see its doc: they must not be told their own stop did this.
        StopReason::SessionEnded => "the session ended on its own",
        // Comma, not a dash: the caller already spent the line's one dash.
        StopReason::ActuatorFailed => "the clicker failed, details in the journal",
        StopReason::Unresponsive => "the game stopped responding, details in the journal",
        // Skystones, not crystals. `max_spend` is crystals in the domain and
        // Skystones on screen — the rule `view::caption` states and these two
        // were breaking, against a window whose balance strip says SKYSTONES.
        StopReason::OutOfFunds => "no skystones left",
        StopReason::MaxRefreshes => "refresh limit reached",
        StopReason::MaxSpend => "skystone budget reached",
        StopReason::MaxMatches => "match limit reached",
        StopReason::Timeout => "session time limit reached",
    }
}

/// Prints the snapshot for the console build.
///
/// The `#[cfg]` sits on the body, not the item, for the same reason as
/// [`print_controls`]: the one caller (`Session::run`) stays
/// feature-independent, and gating the call would leave this `pub(crate)` item
/// with no caller in a `gui` build — `dead_code` under `-D warnings`.
///
/// Gated at all because the windowed build's stdout is an inert sink, so this
/// was a formatted write per slot per shop message on the session loop, for
/// nobody. With stdout redirected to a pipe whose reader has exited, `println!`
/// panics rather than returning — inside `session_loop`, which
/// `Session::run`'s own comment says must not be poisoned by console I/O.
pub(crate) fn render_shop(snapshot: &ShopSnapshot) {
    #[cfg(not(feature = "gui"))]
    {
        println!(
            "\n[{}]",
            snapshot.merchant.as_deref().unwrap_or("Secret Shop")
        );
        for (index, item) in snapshot.slots.iter().enumerate() {
            println!("  {}", format_item(item, index));
        }
    }
    // The body above is compiled out in the windowed build, leaving `snapshot`
    // unused; the parameter itself stays because the one caller is
    // feature-independent.
    #[cfg(feature = "gui")]
    let _ = snapshot;
}

/// `index` is the item's 0-based position, needed for the player-facing slot
/// number when the wire slot is omitted (`effective_slot`).
///
/// `write!` rather than `push_str(&format!(..))`: this also backs the shop
/// table's hover tooltip, and each `format!` would allocate a throwaway `String`
/// per present field plus one per substat. Writing into a `String` is
/// infallible, so the `Result`s are discarded rather than unwrapped.
pub(crate) fn format_item(item: &ShopItem, index: usize) -> String {
    use std::fmt::Write as _;

    let kind = kind_label(item.kind);

    let mut line = String::with_capacity(64);
    let _ = write!(line, "slot {} · {kind}", item.effective_slot(index));
    if let Some(name) = &item.name {
        let _ = write!(line, " · {name}");
    }
    if let Some(set) = &item.set {
        let _ = write!(line, " · set {set}");
    }
    if let Some(grade) = item.grade {
        let _ = write!(line, " · grade {grade}");
    }
    if let Some(price) = item.price {
        let _ = write!(line, " · {price} gold");
    }
    if !item.substats.is_empty() {
        line.push_str(" · [");
        for (position, stat) in item.substats.iter().enumerate() {
            if position > 0 {
                line.push_str(", ");
            }
            match stat.value {
                Some(value) => {
                    let _ = write!(line, "{} {value}", stat.name);
                }
                None => line.push_str(&stat.name),
            }
        }
        line.push(']');
    }
    if let Some(limit) = item.limit {
        let _ = write!(line, " · {}/{}", limit.remaining, limit.total);
    }
    line
}

/// The stdin key for the console lane. Silent in the windowed build, which has
/// no console to print into and an inert stdin.
///
/// The `#[cfg]` sits on the body, not the item, so the one caller
/// ([`Session::run`](crate::app::Session::run)) stays feature-independent.
/// Gating the *call* instead leaves this `pub(crate)` item with no caller in a
/// `gui` build, i.e. `dead_code` under `-D warnings`.
pub(crate) fn print_controls() {
    #[cfg(not(feature = "gui"))]
    println!("Commands: start, stop, [Enter] toggle, Ctrl+C to quit");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::control::{Controller, Event, Limits};
    use crate::domain::filter::Filter;

    #[test]
    fn kind_label_names_each_kind() {
        assert_eq!(kind_label(ItemKind::Equipment), "Equipment");
        assert_eq!(kind_label(ItemKind::Hero), "Hero");
        assert_eq!(kind_label(ItemKind::Token), "Token");
        assert_eq!(kind_label(ItemKind::Unknown), "?");
    }

    #[test]
    fn grouped_inserts_thousands_separators() {
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(88), "88");
        assert_eq!(grouped(1_000), "1,000");
        assert_eq!(grouped(1_234_567), "1,234,567");
    }

    #[cfg(feature = "gui")]
    #[test]
    fn haul_tally_names_the_headline_tokens_even_when_empty() {
        let (named, others) = haul_tally(&Haul::default());
        assert_eq!(named, [("Covenant", 0), ("Mystic", 0)]);
        assert_eq!(others, 0);
    }

    #[test]
    fn format_item_slot_falls_back_to_position_like_the_table() {
        // A slot-omitting item (slot 0) at position 1 must read "slot 2", the
        // same number the match header and the GUI table derive.
        let item = ShopItem::default();
        assert_eq!(item.slot, 0);
        assert!(format_item(&item, 1).starts_with("slot 2 · "));
    }

    /// Capturing stdout in a Rust test needs a dependency this crate does not
    /// have, so this asserts the property structurally instead: in the `gui`
    /// build `render_shop`'s body is compiled out entirely (see its `#[cfg]`),
    /// so the real assertion is that `#[cfg]` — this only documents that the
    /// call still completes on a full snapshot with nothing left unexercised.
    #[cfg(feature = "gui")]
    #[test]
    fn the_windowed_build_writes_no_shop_line() {
        let snapshot = ShopSnapshot {
            merchant: Some("Secret Shop".to_owned()),
            slots: vec![ShopItem::default(); 6],
            refresh: None,
        };
        render_shop(&snapshot);
    }

    #[test]
    fn status_summary_never_promises_start_while_unrestricted() {
        let ctrl = Controller::new(Filter::default(), Limits::default());
        assert_eq!(
            status_summary(&ctrl),
            ("Idle", Some("define a filter first"))
        );

        let mut armed = Controller::new(Filter::matching_default_items(), Limits::default());
        assert_eq!(status_summary(&armed), ("Idle", Some("ready to start")));
        let _ = armed.handle(Event::Start { now_ms: 0 });
        let _ = armed.handle(Event::Stop);
        assert_eq!(status_summary(&armed), ("Stopped", Some("at your request")));
    }

    #[test]
    fn status_label_joins_the_summary_for_the_console() {
        let ctrl = Controller::new(Filter::default(), Limits::default());
        assert_eq!(status_label(&ctrl), "Idle — define a filter first");

        let mut armed = Controller::new(Filter::matching_default_items(), Limits::default());
        let _ = armed.handle(Event::Start { now_ms: 0 });
        assert_eq!(status_label(&armed), "Watching");
    }
}
