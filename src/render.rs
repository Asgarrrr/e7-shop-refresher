//! Player-facing text for domain state: one wording shared by the console
//! and the window.

use crate::domain::control::{Controller, Status, StopReason};
use crate::domain::shop::{ItemKind, ShopItem, ShopSnapshot};

pub(crate) fn kind_label(kind: ItemKind) -> &'static str {
    match kind {
        ItemKind::Equipment => "equipment",
        ItemKind::Hero => "hero",
        ItemKind::Token => "token",
        ItemKind::Unknown => "?",
    }
}

pub(crate) fn status_label(controller: &Controller) -> &'static str {
    match controller.status() {
        Status::Idle => "idle (`start` arms the watch)",
        Status::Watching => "watching",
        // An empty checklist never auto-resumes.
        Status::Paused if controller.checklist().is_empty() => "paused (buy, then refresh)",
        Status::Paused => "paused (buy — auto-resumes)",
        Status::Stopped(_) => "stopped (`start` re-arms)",
    }
}

pub(crate) fn describe(reason: StopReason) -> &'static str {
    match reason {
        StopReason::PlayerStopped => "player stopped",
        StopReason::OutOfFunds => "out of crystals",
        StopReason::MaxRefreshes => "refresh limit reached",
        StopReason::MaxSpend => "crystal budget reached",
        StopReason::MaxMatches => "match limit reached",
        StopReason::Timeout => "session time limit reached",
    }
}

pub(crate) fn render_shop(snapshot: &ShopSnapshot) {
    let merchant = snapshot.merchant.as_deref().unwrap_or("Secret Shop");
    println!("\n[{merchant}]");
    for item in &snapshot.slots {
        println!("  {}", format_item(item));
    }
}

pub(crate) fn format_item(item: &ShopItem) -> String {
    let kind = kind_label(item.kind);

    let mut line = format!("slot {} · {kind}", item.slot);
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

    #[test]
    fn kind_label_names_each_kind() {
        assert_eq!(kind_label(ItemKind::Equipment), "equipment");
        assert_eq!(kind_label(ItemKind::Hero), "hero");
        assert_eq!(kind_label(ItemKind::Token), "token");
        assert_eq!(kind_label(ItemKind::Unknown), "?");
    }
}
