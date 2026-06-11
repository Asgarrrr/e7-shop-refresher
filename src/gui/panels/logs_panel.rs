use crate::gui::app::palette;
use crate::gui::logs::LogBuffer;

use super::section_header;

pub(in crate::gui) fn draw_logs(ui: &mut egui::Ui, logs: &LogBuffer) {
    // Stored in egui memory so the filter setting doesn't need to be
    // plumbed through ShopGui.
    let filter_id = egui::Id::new("log_min_level");
    let mut min_level = ui
        .ctx()
        .data(|d| d.get_temp::<LogLevel>(filter_id))
        .unwrap_or(LogLevel::Info);

    ui.horizontal(|ui| {
        section_header(ui, "Logs");
        if ui.button("Clear").clicked() {
            logs.clear();
        }
        ui.add_space(8.0);
        ui.label("Min level:");
        let prev = min_level;
        egui::ComboBox::from_id_salt("log_min_level_combo")
            .selected_text(min_level.label())
            .show_ui(ui, |ui| {
                for option in LogLevel::ALL {
                    ui.selectable_value(&mut min_level, *option, option.label());
                }
            });
        if min_level != prev {
            ui.ctx().data_mut(|d| d.insert_temp(filter_id, min_level));
        }
    });
    let lines = logs.snapshot();
    let threshold = min_level.to_tracing();
    egui::ScrollArea::vertical()
        .stick_to_bottom(true)
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for line in lines {
                if line.level > threshold {
                    continue;
                }
                let color = level_color(line.level);
                ui.colored_label(color, format!("{}  {}", level_glyph(line.level), line.text));
            }
        });
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl LogLevel {
    const ALL: &'static [Self] = &[
        Self::Trace,
        Self::Debug,
        Self::Info,
        Self::Warn,
        Self::Error,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Trace => "TRACE",
            Self::Debug => "DEBUG",
            Self::Info => "INFO",
            Self::Warn => "WARN",
            Self::Error => "ERROR",
        }
    }

    fn to_tracing(self) -> tracing::Level {
        match self {
            Self::Trace => tracing::Level::TRACE,
            Self::Debug => tracing::Level::DEBUG,
            Self::Info => tracing::Level::INFO,
            Self::Warn => tracing::Level::WARN,
            Self::Error => tracing::Level::ERROR,
        }
    }
}

// `tracing::Level` constants are associated consts on a struct, not
// enum variants — so they can't be matched against.
fn level_glyph(level: tracing::Level) -> &'static str {
    if level == tracing::Level::ERROR {
        "ERROR"
    } else if level == tracing::Level::WARN {
        "WARN "
    } else if level == tracing::Level::INFO {
        "INFO "
    } else if level == tracing::Level::DEBUG {
        "DEBUG"
    } else {
        "TRACE"
    }
}

fn level_color(level: tracing::Level) -> egui::Color32 {
    if level == tracing::Level::ERROR {
        palette::ERROR
    } else if level == tracing::Level::WARN {
        palette::WARN
    } else if level == tracing::Level::INFO {
        palette::SECTION_HEADER
    } else {
        palette::TEXT_DIM
    }
}
