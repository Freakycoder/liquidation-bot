use crate::{config::Config, rpc::Rpc, types::{BankConfig, RawPosition}};
use anyhow::{bail, Result};
use async_trait::async_trait;
use solana_sdk::pubkey::Pubkey;
use std::collections::HashMap;

pub mod marginfi;
pub mod kamino;
pub mod drift;
pub mod solend;

/// Implementation status for the dashboard. Pending protocols are
/// registered (trait wired) but their parsers are not yet verified
/// against on-chain layouts; they return empty banks/positions and
/// are surfaced honestly in the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImplStatus { Live, Pending }

#[async_trait]
pub trait LendingProtocol: Send + Sync {
    fn name(&self) -> &'static str;
    fn status(&self) -> ImplStatus { ImplStatus::Live }
    async fn load_banks(&self, rpc: &Rpc) -> Result<HashMap<Pubkey, BankConfig>>;
    async fn load_positions(
        &self,
        rpc: &Rpc,
        banks: &HashMap<Pubkey, BankConfig>,
    ) -> Result<Vec<RawPosition>>;
}

pub fn build(cfg: &Config) -> Result<Box<dyn LendingProtocol>> {
    match cfg.protocol.as_str() {
        "marginfi" => Ok(Box::new(marginfi::MarginFi::new())),
        "kamino"   => Ok(Box::new(kamino::Kamino::new())),
        "drift"    => Ok(Box::new(drift::Drift::new())),
        "solend"   => Ok(Box::new(solend::Solend::new())),
        other => bail!("unsupported protocol: '{other}'"),
    }
}