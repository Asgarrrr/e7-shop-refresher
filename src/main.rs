// Release builds drop the console subsystem so double-clicking the .exe
// doesn't spawn a black terminal window. Debug builds keep it so cargo
// run still shows stderr / tracing output in the dev terminal.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;

use clap::Parser;

#[derive(Parser, Debug)]
#[command(version, about = "Graphical front-end for the E7 shop refresher")]
struct Cli {
    /// Path to the TOML config file. Defaults to a `config.toml` next to
    /// the .exe (portable mode) if one exists, otherwise
    /// `%APPDATA%\e7-shop-refresher\config.toml`.
    #[arg(short, long)]
    config: Option<PathBuf>,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let config_path = cli
        .config
        .unwrap_or_else(e7_shop_refresher::config::default_config_path);
    e7_shop_refresher::gui::run(&config_path)
}
