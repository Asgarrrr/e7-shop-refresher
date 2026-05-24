// Release builds drop the console subsystem so double-clicking the .exe
// doesn't spawn a black terminal window. Debug builds keep it so cargo
// run still shows stderr / tracing output in the dev terminal.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;

use clap::Parser;

#[derive(Parser, Debug)]
#[command(version, about = "Graphical front-end for the E7 shop refresher")]
struct Cli {
    /// Path to the TOML config file
    #[arg(short, long, default_value = "config.toml")]
    config: PathBuf,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    e7_shop_refresher::gui::run(&cli.config)
}
