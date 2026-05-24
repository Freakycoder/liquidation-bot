use super::LendingProtocol;
use crate::{rpc::Rpc, types::{BankConfig, RawPosition}};
use anyhow::Result;
use async_trait::async_trait;
use solana_sdk::pubkey::Pubkey;
use std::{collections::HashMap, str::FromStr};

const MARGINFI_PROGRAM_ID: &str = "MFv2hWf31Z9kbCa1snEPYctwafyhdvnV7FZnsebVacA";

pub struct MarginFi {
    pub program_id: Pubkey,
}

impl MarginFi {
    pub fn new() -> Self {
        Self {
            program_id: Pubkey::from_str(MARGINFI_PROGRAM_ID).unwrap(),
        }
    }
}

#[async_trait]
impl LendingProtocol for MarginFi {
    fn name(&self) -> &'static str { "marginfi-v2" }

    async fn load_banks(&self, _rpc: &Rpc) -> Result<HashMap<Pubkey, BankConfig>> {
        todo!("Day 2 - load MarginFi banks")
    }

    async fn load_positions(&self, _rpc: &Rpc) -> Result<Vec<RawPosition>> {
        todo!("Day 3 - load MarginFi positions")
    }
}