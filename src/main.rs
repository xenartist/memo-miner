//! MEMO Miner: mint MEMO on X1 from the terminal, with a wallet made by the official Solana CLI.

mod app;
mod chain;
mod curve;
mod input;
mod network;
mod rpc;
mod setup;

use anyhow::Result;
use clap::Parser;
use crossterm::event::{DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture};
use crossterm::execute;
use network::Network;
use std::path::PathBuf;

#[derive(Parser)]
#[command(version, about = "Mint MEMO on X1 from the terminal. MEMO Engraves Memories Onchain.")]
struct Cli {
    /// Use X1 testnet (for development)
    #[arg(long, hide = true)]
    testnet: bool,
    /// Keep the miner's wallet in this directory instead of ~/.config/solana (for development)
    #[arg(long, hide = true)]
    dir: Option<PathBuf>,
}

fn restore() {
    let _ = execute!(std::io::stdout(), DisableMouseCapture, DisableBracketedPaste);
    ratatui::restore();
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let network = if cli.testnet { Network::Testnet } else { Network::Mainnet };
    // The miner only ever uses ~/.config/solana/memo-miner.json
    let wallet = match cli.dir {
        Some(d) => d.join(setup::WALLET_FILE),
        None => setup::default_wallet_path()?,
    };

    let terminal = ratatui::init();
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore();
        hook(info);
    }));
    execute!(std::io::stdout(), EnableMouseCapture, EnableBracketedPaste)?;
    let result = app::App::new(network, wallet).run(terminal).await;
    restore();
    result
}
