mod config;
mod health;
mod liquidator;
mod oracle;
mod protocol;
mod rpc;
mod scanner;
mod tui;
mod types;

use anyhow::{Context, Result};
use clap::Parser;
use liquidator::Liquidator;
use std::io::IsTerminal;
use std::time::Duration;

#[derive(Parser)]
#[command(name = "liq-bot", about = "Solana lending protocol liquidation scanner")]
struct Cli {
    #[arg(short, long, default_value = "config.toml")]
    config: String,
    #[arg(long, help = "Run one scan pass and exit")]
    once: bool,
    #[arg(long, help = "Run headless: plain logs to stdout, no dashboard")]
    headless: bool,
    #[arg(long, help = "Diagnose unparsed oracle accounts and exit")]
    diagnose_oracles: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // The dashboard owns the terminal, so in dashboard mode tracing goes to
    // a log file. Headless and single-pass modes log to stdout as before.
    let dashboard = !cli.once && !cli.headless && std::io::stdout().is_terminal();

    if dashboard {
        let file = std::fs::File::create("liq-bot.log")
            .context("creating liq-bot.log")?;
        tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(move || file.try_clone().expect("clone log file handle"))
            .with_env_filter(env_filter())
            .init();
    } else {
        tracing_subscriber::fmt().with_env_filter(env_filter()).init();
    }

    let cfg = config::Config::load(&cli.config)?;
    let protocol = protocol::build(&cfg)?;

    if cli.diagnose_oracles {
        let rpc = rpc::Rpc::new(cfg.rpc_url.clone());
        let banks = protocol.load_banks(&rpc).await?;
        let keys: Vec<_> = banks.values().map(|b| b.oracle).collect();
        oracle::OracleClient::new().diagnose(&rpc, &keys).await?;
        return Ok(());
    }

    let liquidator = match &cfg.keypair_path {
        Some(path) => {
            let l = Liquidator::from_keypair_file(path, cfg.dry_run, cfg.min_profit_usd)?;
            tracing::info!(liquidator = %l.pubkey(), dry_run = cfg.dry_run, "execution enabled");
            Some(l)
        }
        None => {
            tracing::info!("no keypair configured: report-only mode");
            None
        }
    };

    tracing::info!(protocol = protocol.name(), "liq-bot starting");

    let poll_interval = Duration::from_secs(cfg.poll_interval_secs);
    let mut scanner = scanner::Scanner::new(cfg, protocol, liquidator);

    if cli.once {
        scanner.scan_once().await?;
    } else if dashboard {
        tui::run(scanner, poll_interval).await?;
    } else {
        scanner.run().await?;
    }
    Ok(())
}

fn env_filter() -> tracing_subscriber::EnvFilter {
    tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"))
}