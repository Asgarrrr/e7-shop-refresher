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
pub(crate) fn haul_tally(haul: &Haul) -> ([(&'static str, u32); 2], u32) {
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
/// locale-free grouping to lean on. Shared by the status-bar balances, the
/// slot table's prices, and the console item line so every number reads the
/// same.
pub(crate) fn grouped(n: u32) -> String {
    let digits = n.to_string();
    let len = digits.len();
    let mut out = String::with_capacity(len + (len - 1) / 3);
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (len - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// A grouped balance/price, or an em-dash while the value is unknown. The one
/// `Option<u32>` readout shared by the status-bar balance tiles and the slot
/// table's price column, so an absent value reads the same in both.
#[cfg(feature = "gui")]
pub(crate) fn grouped_or_dash(value: Option<u32>) -> String {
    value.map_or_else(|| "—".to_owned(), grouped)
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
        Status::Stopped(reason) => ("Stopped", Some(describe(reason))),
    }
}

/// One-line status for the console: the summary, joined with an em dash.
pub(crate) fn status_label(controller: &Controller) -> String {
    match status_summary(controller) {
        (word, Some(hint)) => format!("{word} — {hint}"),
        (word, None) => word.to_owned(),
    }
}

pub(crate) fn refusal(reason: RefusalReason) -> &'static str {
    match reason {
        RefusalReason::UnrestrictedFilter => {
            "define at least one filter criterion — an empty filter matches everything"
        }
    }
}

pub(crate) fn describe(reason: StopReason) -> &'static str {
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
pub(crate) fn format_item(item: &ShopItem, index: usize) -> String {
    let kind = kind_label(item.kind);

    let mut line = format!("slot {} · {kind}", item.effective_slot(index));
    if let Some(name) = &item.name {
        line.push_str(&format!(" · {name}"));
    }
    if let Some(set) = &item.set {
        line.push_str(&format!(" · set {set}"));
    }
    if let Some(grade) = item.grade {
        line.push_str(&format!(" · grade {grade}"));
    }
    if let Some(price) = item.price {
        line.push_str(&format!(" · {} gold", grouped(price)));
    }
    if !item.substats.is_empty() {
        let stats: Vec<String> = item
            .substats
            .iter()
            .map(|stat| match stat.value {
                Some(value) => format!("{} {value}", stat.name),
                None => stat.name.clone(),
            })
            .collect();
        line.push_str(&format!(" · [{}]", stats.join(", ")));
    }
    if let Some(limit) = item.limit {
        line.push_str(&format!(" · {}/{}", limit.remaining, limit.total));
    }
    line
}

pub(crate) fn print_controls() {
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
        armed.handle(Event::Start { now_ms: 0 });
        armed.handle(Event::Stop);
        // Stopped: the clause is the reason, not a redundant "(Start re-arms)".
        assert_eq!(status_summary(&armed), ("Stopped", Some("player stopped")));
    }

    #[test]
    fn status_label_joins_the_summary_for_the_console() {
        let ctrl = Controller::new(Filter::default(), Limits::default());
        assert_eq!(status_label(&ctrl), "Idle — define a filter first");

        let mut armed = Controller::new(Filter::matching_default_items(), Limits::default());
        armed.handle(Event::Start { now_ms: 0 });
        assert_eq!(status_label(&armed), "Watching");
    }
}
