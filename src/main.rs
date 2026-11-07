//! HFT for dummies: a market maker on Bullet that borrows its price from Binance.
//!
//! Start reading at `events.rs` (the vocabulary), then `bot.rs` (the loop),
//! `market_maker.rs` (the decisions) and `strategy.rs` (the prices).

mod backtest;
mod binance;
mod book;
mod bot;
mod bullet;
mod bybit;
mod channel;
mod clock;
mod config;
mod events;
mod executor;
mod fair_value;
mod flow;
mod history;
mod hyperliquid;
mod hyperliquid_sign;
mod indicators;
mod inventory;
mod key;
mod liveness;
mod market_maker;
mod momentum;
mod recording;
mod strategy;
mod telemetry;
mod volatility;

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use bullet_rust_sdk::Keypair;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(about = "A simple market maker for the HFT for dummies series")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    /// Trade with real money, whatever `dry_run` says in the config.
    #[arg(long, global = true)]
    live: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Run the bot (the default).
    Run {
        #[arg(default_value = "bot.toml")]
        config: PathBuf,
    },
    /// Record the market a config quotes (no trading), to replay with `backtest`.
    Record {
        config: PathBuf,
        /// The file to write, e.g. recordings/sol.jsonl.
        out: PathBuf,
    },
    /// Replay a recording through configs (files or folders) and compare them.
    Backtest {
        recording: PathBuf,
        #[arg(required = true)]
        configs: Vec<PathBuf>,
        /// How long our orders, changes and cancels take to reach the
        /// exchange (0 = instantly). Try what the bot really sees, e.g. 150.
        #[arg(long, default_value_t = 0)]
        latency_ms: u64,
    },
    /// Make a new trading key for the bot, to approve in the exchange's app.
    Keygen {
        /// Where to write it (default: bot.key for Bullet, hyperliquid.key for Hyperliquid).
        path: Option<PathBuf>,
        #[arg(long, default_value = "mainnet")]
        network: String,
        /// Which exchange the key is for.
        #[arg(long, default_value = "bullet", value_parser = ["bullet", "hyperliquid"])]
        venue: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    // A backtest replays thousands of bots' worth of log lines: only errors.
    let quiet = matches!(cli.command, Some(Command::Backtest { .. }));
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| if quiet { "error" } else { "info" }.into()),
        )
        // Colours only in a terminal, not in a server's log files.
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
        .init();

    match cli.command.unwrap_or(Command::Run {
        config: "bot.toml".into(),
    }) {
        Command::Run { config } => bot::run(&config, cli.live).await,
        Command::Record { config, out } => recording::record(&config, &out).await,
        Command::Backtest {
            recording,
            configs,
            latency_ms,
        } => backtest::backtest(
            &recording,
            &configs,
            std::time::Duration::from_millis(latency_ms),
        ),
        Command::Keygen {
            path,
            network,
            venue,
        } => {
            if venue == "hyperliquid" {
                keygen_hyperliquid(&path.unwrap_or("hyperliquid.key".into()), &network)
            } else {
                keygen(&path.unwrap_or("bot.key".into()), &network)
            }
        }
    }
}

fn keygen(path: &PathBuf, network: &str) -> Result<()> {
    refuse_to_overwrite(path)?;
    let keypair = Keypair::generate();
    keypair.write_to_file(path)?;
    only_you_can_read(path)?;
    let address = keypair.address();
    println!(
        "Wrote a new key to {}. Keep it secret; never commit it.\n",
        path.display()
    );
    println!("Its address: {address}\n");
    println!("Next: open this link (logged in to Bullet) and click Confirm Delegation:");
    println!("  {}", bullet::delegate_link(network, &address));
    Ok(())
}

/// Hyperliquid keys are Ethereum-style, so they're made differently. You
/// approve this one as an "API wallet": it can trade but can't withdraw.
fn keygen_hyperliquid(path: &Path, network: &str) -> Result<()> {
    refuse_to_overwrite(path)?;
    let wallet = hyperliquid_sign::Wallet::generate();
    std::fs::write(path, wallet.secret_hex() + "\n")?;
    only_you_can_read(path)?;
    println!(
        "Wrote a new key to {}. Keep it secret; never commit it.\n",
        path.display()
    );
    println!("Its address: {}\n", wallet.address());
    let app = if network == "testnet" {
        "https://app.hyperliquid-testnet.xyz/API"
    } else {
        "https://app.hyperliquid.xyz/API"
    };
    println!("Next: open {app} (wallet connected), paste that address as the API wallet,");
    println!("name it hft-for-dummies, and click Authorize.");
    Ok(())
}

fn refuse_to_overwrite(path: &Path) -> Result<()> {
    if path.exists() {
        bail!("{} already exists; not overwriting a key", path.display());
    }
    Ok(())
}

fn only_you_can_read(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    let _ = path; // nothing to do elsewhere
    Ok(())
}
