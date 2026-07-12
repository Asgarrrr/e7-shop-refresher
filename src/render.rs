//! Player-facing text for domain state: one wording shared by the console
//! and the window.

use crate::domain::control::{Controller, RefusalReason, Status, StopReason};
use crate::domain::shop::{ItemKind, ShopItem, ShopSnapshot};

pub(crate) fn kind_label(kind: ItemKind) -> &'static str {
    match kind {
        ItemKind::Equipment => "equipment",
        ItemKind::Hero => "hero",
        ItemKind::Token => "token",
        ItemKind::Unknown => "?",
    }
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
        line.push_str(&format!(" · {price} gold"));
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
        assert_eq!(kind_label(ItemKind::Equipment), "equipment");
        assert_eq!(kind_label(ItemKind::Hero), "hero");
        assert_eq!(kind_label(ItemKind::Token), "token");
        assert_eq!(kind_label(ItemKind::Unknown), "?");
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
