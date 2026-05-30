//! Drift Protocol v2 lending integration.
//!
//! STATUS: SCAFFOLDED. The trait is implemented so Drift appears in the
//! scanner, dashboard, and leaderboard; the parsers are stubs that return
//! empty banks/positions. Drift uses a fundamentally different account
//! model from MarginFi/Kamino (per-user SpotPosition arrays inside a
//! User account, SpotMarket accounts for collateral types), so it needs
//! its own IDL fetch + byte-offset reverse-engineering pass before the
//! parsers can be written. This pattern matches how Kamino was added:
//! scaffold first, verify offsets against live dumps, then ship.
//!
//! Program ID: dRiftyHA39MWEi3m9aunc5MzRF1JYuBsbn6VPcn33UH

use super::{ImplStatus, LendingProtocol};
use crate::{rpc::Rpc, types::{BankConfig, RawPosition}};
use anyhow::Result;
use async_trait::async_trait;
use solana_sdk::pubkey::Pubkey;
use std::{collections::HashMap, str::FromStr};

const DRIFT_PROGRAM_ID: &str = "dRiftyHA39MWEi3m9aunc5MzRF1JYuBsbn6VPcn33UH";

pub struct Drift { pub program_id: Pubkey }

impl Drift {
    pub fn new() -> Self {
        Self { program_id: Pubkey::from_str(DRIFT_PROGRAM_ID).unwrap() }
    }
}

#[async_trait]
impl LendingProtocol for Drift {
    fn name(&self) -> &'static str { "drift" }
    fn status(&self) -> ImplStatus { ImplStatus::Pending }

    async fn load_banks(&self, _rpc: &Rpc) -> Result<HashMap<Pubkey, BankConfig>> {
        tracing::info!("drift: SpotMarket layout verification pending; no banks loaded");
        Ok(HashMap::new())
    }

    async fn load_positions(
        &self,
        _rpc: &Rpc,
        _banks: &HashMap<Pubkey, BankConfig>,
    ) -> Result<Vec<RawPosition>> {
        tracing::info!("drift: User/SpotPosition layout verification pending; no positions loaded");
        Ok(Vec::new())
    }
}