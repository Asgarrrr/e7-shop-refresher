//! Player-facing text for domain state: one wording shared by the console
//! and the window.

#[cfg(feature = "gui")]
use crate::domain::control::Haul;
use crate::domain::control::{Controller, RefusalReason, Status, StopReason};
use crate::domain::shop::{ItemKind, ShopItem, ShopSnapshot};

/// The hunt tokens worth naming in the haul readout — the covenant bookmark and
/// mystic medal 90% of players chase. Wire name → display label, in headline
/// order. Every other bought item is bucketed into "+N other". Only the window
/// renders the haul (and the Setup tab's quick-add), so this display policy is
/// gated with it.
#[cfg(feature = "gui")]
pub(crate) const HAUL_HEADLINERS: [(&str, &str); 2] = [
    ("ticketrare_name", "Covenant"),
    ("ticketspecial_name", "Mystic"),
];

/// The haul as the readout shows it: the headline tokens with their counts (in
/// order, shown even at zero so the player sees what's hunted), and the count
/// of everything else bought this run.
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
/// The single number formatter, and since the currency pass it is reached only
/// through `Display for Gold` and `Display for Crystals`, never at a print site.
/// It used to be called by hand at each of the four places that show an amount,
/// and the fourth — the journal's `>> bought: … 250000 gold left` — forgot,
/// which is how a run showed `250000` and `250,000` in the same window. This
/// doc's old claim that every number reads the same is true now because there is
/// nowhere left to forget.
///
/// It stays a free `u32` function rather than moving onto the currencies: the
/// grouping rule is a display concern this module owns, and keeping it here is
/// what lets both currencies share one copy of it.
pub(crate) fn grouped(n: u32) -> String {
    // Digits are extracted least-significant-first into a fixed buffer —
    // `u32::MAX` is ten digits, so it always fits — rather than through
    // `n.to_string()`, which allocated a second `String` purely to iterate its
    // characters into the one this returns. Every price cell and balance tile
    // calls this on the repaint path.
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

/// An amount, or an em-dash while it is unknown. The one absent-value policy
/// shared by the status-bar balance tiles and the slot table's price column, so
/// "the server has not said" reads the same in both.
///
/// Generic over `Display` and no longer over `u32` specifically: the grouping is
/// the currency's own rendering now (`impl Display for Gold`/`Crystals`), and
/// this function is left with exactly one job — the em-dash. Renamed from
/// `grouped_or_dash` to say so, because a name promising grouping over a
/// parameter that no longer guarantees it is worse than no promise.
///
/// A sealed `Amount` trait implemented only for the two currencies was
/// considered, so that nothing else could be passed. Rejected: it would be a
/// name whose entire body is empty, adding no check the call sites do not
/// already have — every one of them reads a typed field straight out of the
/// domain, and the status bar's two balance tiles are additionally bound to
/// their currency by `ui::statusbar`'s per-ledger tile helpers.
#[cfg(feature = "gui")]
pub(crate) fn amount_or_dash(value: Option<impl std::fmt::Display>) -> String {
    value.map_or_else(|| "—".to_owned(), |amount| amount.to_string())
}

/// The state word (title-cased for display) and an optional clause, split so
/// the window can weight them — word in the severity color, clause muted —
/// while the console joins them via `status_label`. For `Stopped` the clause
/// is the stop reason. The hint only offers "start" where the domain would
/// actually arm: an unrestricted filter reads "define a filter first".
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
        Status::Paused => ("Paused", Some("buy — auto-resumes")),
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

/// Why the domain refused to arm, in the player's words. Named for what it
/// returns, like its `*_label` siblings above: it is imported bare into
/// `session/mod.rs`, where a lone `refusal` would also read as
/// `Action::Refused`'s payload or as `pcap`'s unrelated `Refusal`.
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
        StopReason::PlayerStopped => "player stopped",
        StopReason::SessionEnded => "session ended",
        StopReason::ActuatorFailed => "clicker failed — see the journal",
        StopReason::Unresponsive => "no response from the game — see the journal",
        StopReason::OutOfFunds => "out of crystals",
        StopReason::MaxRefreshes => "refresh limit reached",
        StopReason::MaxSpend => "crystal budget reached",
        StopReason::MaxMatches => "match limit reached",
        StopReason::Timeout => "session time limit reached",
    }
}

/// Merchant name, or the shared fallback when the snapshot omits it — the one
/// place the default label lives, so the console dump and the GUI header never
/// disagree.
pub(crate) fn merchant_label(merchant: Option<&str>) -> &str {
    merchant.unwrap_or("Secret Shop")
}

pub(crate) fn render_shop(snapshot: &ShopSnapshot) {
    println!("\n[{}]", merchant_label(snapshot.merchant.as_deref()));
    for (index, item) in snapshot.slots.iter().enumerate() {
        println!("  {}", format_item(item, index));
    }
}

/// `index` is the item's 0-based position, needed for the player-facing slot
/// number when the wire slot is omitted (`effective_slot`).
///
/// Appended with `write!` rather than `push_str(&format!(..))`: this is the shop
/// table's hover tooltip as well as the console dump, and each `format!` used to
/// allocate a throwaway `String` per present field only to copy it in and drop
/// it — the substat clause added one more per substat plus a `join`. Writing
/// into a `String` is infallible, so the `Result`s are discarded rather than
/// unwrapped.
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
        // `{price}`, not `grouped(price)`: a gold amount groups itself now, so
        // this line cannot drift from the table's price column the way the
        // journal's "bought" line had.
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

/// The stdin key for the console lane. Silent in the windowed build: that build
/// carries `windows_subsystem = "windows"`, so it has no console to print into
/// and stdin is inert — the line would be written to a sink nobody can read,
/// while `journal.rs` holds the invariant that player-facing text has one sink.
/// The `#[cfg]` sits on the body, not the item, so the one caller
/// ([`Session::run`](crate::app::Session::run)) stays feature-independent — the
/// other way round was tried and reverted: gating the *call* leaves this
/// `pub(crate)` item with no caller in a `gui` build, i.e. `dead_code` under
/// `-D warnings`.
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
        // A fresh run: the two hunt tokens are still listed (at zero, so the
        // player sees the target), and nothing sits in the bucket.
        let (named, others) = haul_tally(&Haul::default());
        assert_eq!(named, [("Covenant", 0), ("Mystic", 0)]);
        assert_eq!(others, 0);
    }

    #[test]
    fn format_item_slot_falls_back_to_position_like_the_table() {
        // A slot-omitting item (slot 0) at position 1 must read "slot 2", the
        // same number the match header and the GUI table derive — no more
        // "slot 0" in the detail line beside a "slot 2" header.
        let item = ShopItem::default();
        assert_eq!(item.slot, 0);
        assert!(format_item(&item, 1).starts_with("slot 2 · "));
    }

    #[test]
    fn merchant_label_falls_back_when_absent() {
        assert_eq!(merchant_label(Some("Secret Shop VIP")), "Secret Shop VIP");
        assert_eq!(merchant_label(None), "Secret Shop");
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
        // Stopped: the clause is the reason, not a redundant "(Start re-arms)".
        assert_eq!(status_summary(&armed), ("Stopped", Some("player stopped")));
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
