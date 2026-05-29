use crate::{config::Config, rpc::Rpc, types::{BankConfig, RawPosition}};
use anyhow::{bail, Result};
use async_trait::async_trait;
use solana_sdk::pubkey::Pubkey;
use std::collections::HashMap;

pub mod marginfi;
pub mod kamino;

#[async_trait]
pub trait LendingProtocol: Send + Sync {
    fn name(&self) -> &'static str;
    async fn load_banks(&self, rpc: &Rpc) -> Result<HashMap<Pubkey, BankConfig>>;
    async fn load_positions(&self, rpc: &Rpc) -> Result<Vec<RawPosition>>;
}

pub fn build(cfg: &Config) -> Result<Box<dyn LendingProtocol>> {
    match cfg.protocol.as_str() {
        "marginfi" => Ok(Box::new(marginfi::MarginFi::new())),
        "kamino"   => Ok(Box::new(kamino::Kamino::new())),
        other => bail!("unsupported protocol: '{other}'"),
    }
}