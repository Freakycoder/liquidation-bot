mod config;
mod health;
mod liquidator;
mod oracle;
mod protocol;
mod rpc;
mod scanner;
mod types;

use anyhow::Result;
use clap::Parser;
use liquidator::Liquidator;

#[derive(Parser)]
#[command(name = "liq-bot", about = "Solana lending protocol liquidation scanner")]
struct Cli {
    #[arg(short, long, default_value = "config.toml")]
    config: String,
    #[arg(long, help = "Run one scan pass and exit")]
    once: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let cfg = config::Config::load(&cli.config)?;
    let protocol = protocol::build(&cfg)?;

    // Build the liquidator only if a keypair is configured.
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

    let mut scanner = scanner::Scanner::new(cfg, protocol, liquidator);
    if cli.once {
        scanner.scan_once().await?;
    } else {
        scanner.run().await?;
    }
    Ok(())
}