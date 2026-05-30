//! Solend Protocol lending integration.
//!
//! STATUS: SCAFFOLDED. Same approach as Drift: trait implemented for
//! dashboard registration, parsers stubbed pending byte-offset
//! verification against live Solend Reserve and Obligation accounts.
//! Solend's account model is closer to Kamino's (Reserves + Obligations)
//! but its u128-fraction representation differs and oracle wiring uses
//! Pyth + Switchboard like MarginFi rather than self-pricing.
//!
//! Program ID: So1endDq2YkqhipRh3WViPa8hdiSpxWy6z3Z6tMCpAo

use super::{ImplStatus, LendingProtocol};
use crate::{rpc::Rpc, types::{BankConfig, RawPosition}};
use anyhow::Result;
use async_trait::async_trait;
use solana_sdk::pubkey::Pubkey;
use std::{collections::HashMap, str::FromStr};

const SOLEND_PROGRAM_ID: &str = "So1endDq2YkqhipRh3WViPa8hdiSpxWy6z3Z6tMCpAo";

pub struct Solend { pub program_id: Pubkey }

impl Solend {
    pub fn new() -> Self {
        Self { program_id: Pubkey::from_str(SOLEND_PROGRAM_ID).unwrap() }
    }
}

#[async_trait]
impl LendingProtocol for Solend {
    fn name(&self) -> &'static str { "solend" }
    fn status(&self) -> ImplStatus { ImplStatus::Pending }

    async fn load_banks(&self, _rpc: &Rpc) -> Result<HashMap<Pubkey, BankConfig>> {
        tracing::info!("solend: Reserve layout verification pending; no banks loaded");
        Ok(HashMap::new())
    }

    async fn load_positions(
        &self,
        _rpc: &Rpc,
        _banks: &HashMap<Pubkey, BankConfig>,
    ) -> Result<Vec<RawPosition>> {
        tracing::info!("solend: Obligation layout verification pending; no positions loaded");
        Ok(Vec::new())
    }
}