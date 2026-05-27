//! Configuration loaded from a TOML file at startup.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// HTTP RPC endpoint.
    pub rpc_url: String,
    /// Which lending protocol to scan (e.g. "marginfi").
    pub protocol: String,
    /// Seconds between scan passes.
    #[serde(default = "default_interval")]
    pub poll_interval_secs: u64,
    /// Minimum estimated profit (USD) before a liquidation is acted on.
    #[serde(default)]
    pub min_profit_usd: f64,
    /// When true, only report opportunities; never build or send transactions.
    #[serde(default = "default_true")]
    pub dry_run: bool,
    /// Path to a Solana CLI keypair file. When set, the bot will attempt
    /// liquidations (gated by dry_run). When absent, it only reports.
    #[serde(default)]
    pub keypair_path: Option<String>,

    #[serde(default = "default_min_debt")]
    pub min_debt_usd: f64,
}

fn default_interval() -> u64 {
    15
}

fn default_true() -> bool {
    true
}

fn default_min_debt() -> f64 { 100.0 }

impl Config {
    /// Load and parse the config file at `path`.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading config file {}", path.display()))?;
        let cfg: Config = toml::from_str(&raw).context("parsing config TOML")?;
        Ok(cfg)
    }
}