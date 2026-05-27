//! MarginFi v2 implementation. Offsets derived from the on-chain IDL.

use super::LendingProtocol;
use crate::{rpc::Rpc, types::{BankConfig, RawPosition}};
use anyhow::Result;
use async_trait::async_trait;
use solana_account_decoder::UiAccountEncoding;
use solana_client::rpc_config::{RpcAccountInfoConfig, RpcProgramAccountsConfig};
use solana_client::rpc_filter::{Memcmp, MemcmpEncodedBytes, RpcFilterType};
use solana_sdk::{commitment_config::CommitmentConfig, pubkey::Pubkey};
use std::{collections::HashMap, str::FromStr};

const MARGINFI_PROGRAM_ID: &str = "MFv2hWf31Z9kbCa1snEPYctwafyhdvnV7FZnsebVacA";

// ── Bank offsets (absolute, from start of account data) ──
const BANK_MINT:               usize = 8;
const BANK_MINT_DECIMALS:      usize = 40;
// config begins at 296; BankConfig-relative offsets added below
const BANK_CONFIG_START:       usize = 296;
const CFG_ASSET_WEIGHT_MAINT:  usize = BANK_CONFIG_START + 16;   // 312
const CFG_LIAB_WEIGHT_MAINT:   usize = BANK_CONFIG_START + 48;   // 344
const CFG_ORACLE_SETUP:        usize = BANK_CONFIG_START + 313;  // 609
const CFG_ORACLE_KEYS:         usize = BANK_CONFIG_START + 314;  // 610
const BANK_ASSET_SHARE_VALUE:  usize = 80;
const BANK_LIAB_SHARE_VALUE:   usize = 96;

// OracleSetup enum (from IDL variant order)
const ORACLE_PYTH_LEGACY:        u8 = 1;
const ORACLE_PYTH_PUSH:          u8 = 3;
const ORACLE_STAKED_PYTH_PUSH:   u8 = 5;

// ── MarginfiAccount offsets ──
const MFI_AUTHORITY:       usize = 40;
const MFI_BALANCES_START:  usize = 72;
const BALANCE_SLOTS:       usize = 16;
const BALANCE_STRIDE:      usize = 104;
const BAL_ACTIVE:          usize = 0;
const BAL_BANK_PK:         usize = 1;
const BAL_ASSET_SHARES:    usize = 40;
const BAL_LIAB_SHARES:     usize = 56;

pub struct MarginFi { pub program_id: Pubkey }

impl MarginFi {
    pub fn new() -> Self {
        Self { program_id: Pubkey::from_str(MARGINFI_PROGRAM_ID).unwrap() }
    }
}

fn anchor_disc(type_name: &str) -> [u8; 8] {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(format!("account:{type_name}"));
    h.finalize()[..8].try_into().unwrap()
}

fn rpc_config_with_disc(disc: &[u8; 8]) -> RpcProgramAccountsConfig {
    RpcProgramAccountsConfig {
        filters: Some(vec![RpcFilterType::Memcmp(Memcmp::new(
            0, MemcmpEncodedBytes::Bytes(disc.to_vec()),
        ))]),
        account_config: RpcAccountInfoConfig {
            encoding: Some(UiAccountEncoding::Base64),
            commitment: Some(CommitmentConfig::confirmed()),
            ..Default::default()
        },
        with_context: None,
    }
}

fn pubkey_at(data: &[u8], off: usize) -> Result<Pubkey> {
    let b: [u8; 32] = data.get(off..off + 32)
        .ok_or_else(|| anyhow::anyhow!("pubkey_at({off}): short (len={})", data.len()))?
        .try_into()?;
    Ok(Pubkey::from(b))
}

/// WrappedI80F48: 16 raw bytes, signed, 48 fractional bits. value = raw / 2^48
fn i80f48_at(data: &[u8], off: usize) -> Result<f64> {
    let b: [u8; 16] = data.get(off..off + 16)
        .ok_or_else(|| anyhow::anyhow!("i80f48_at({off}): short"))?
        .try_into()?;
    Ok(i128::from_le_bytes(b) as f64 / (1i128 << 48) as f64)
}

fn u8_at(data: &[u8], off: usize) -> Result<u8> {
    data.get(off).copied()
        .ok_or_else(|| anyhow::anyhow!("u8_at({off}): short"))
}

fn parse_bank(address: &Pubkey, data: &[u8]) -> Result<BankConfig> {
    let mint               = pubkey_at(data, BANK_MINT)?;
    let decimals           = u8_at(data, BANK_MINT_DECIMALS)?;
    let asset_share_value  = i80f48_at(data, BANK_ASSET_SHARE_VALUE)?;
    let liab_share_value   = i80f48_at(data, BANK_LIAB_SHARE_VALUE)?;
    let asset_weight_maint = i80f48_at(data, CFG_ASSET_WEIGHT_MAINT)?;
    let liab_weight_maint  = i80f48_at(data, CFG_LIAB_WEIGHT_MAINT)?;
    let oracle_setup       = u8_at(data, CFG_ORACLE_SETUP)?;
    let oracle             = pubkey_at(data, CFG_ORACLE_KEYS)?;

    // Self-check: OracleSetup has 18 variants (0..=17). A value outside
    // that range means the offset is wrong, almost certainly because
    // RatePoint is not 8 bytes. The delta tells you how much to shift.
    if oracle_setup > 17 {
        tracing::warn!(
            bank = %address, raw = oracle_setup,
            "oracle_setup out of range — offset wrong, check RatePoint size"
        );
    }

    Ok(BankConfig {
        address: *address, mint, oracle, decimals,
        asset_weight_maint, liab_weight_maint,
        asset_share_value, liab_share_value,
    })
}

fn parse_marginfi_account(address: &Pubkey, data: &[u8]) -> Result<Option<RawPosition>> {
    let owner = pubkey_at(data, MFI_AUTHORITY)?;
    let mut deposits = Vec::new();
    let mut borrows  = Vec::new();

    for slot in 0..BALANCE_SLOTS {
        let base = MFI_BALANCES_START + slot * BALANCE_STRIDE;
        if u8_at(data, base + BAL_ACTIVE)? == 0 { continue; }
        let bank_pk      = pubkey_at(data, base + BAL_BANK_PK)?;
        let asset_shares = i80f48_at(data, base + BAL_ASSET_SHARES)?;
        let liab_shares  = i80f48_at(data, base + BAL_LIAB_SHARES)?;
        if asset_shares > 0.0 { deposits.push((bank_pk, asset_shares)); }
        if liab_shares  > 0.0 { borrows.push((bank_pk, liab_shares)); }
    }

    if borrows.is_empty() { return Ok(None); }
    Ok(Some(RawPosition { address: *address, owner, deposits, borrows }))
}

#[async_trait]
impl LendingProtocol for MarginFi {
    fn name(&self) -> &'static str { "marginfi-v2" }

    async fn load_banks(&self, rpc: &Rpc) -> Result<HashMap<Pubkey, BankConfig>> {
        let disc = anchor_disc("Bank");
        let raw = rpc.client
            .get_program_accounts_with_config(&self.program_id, rpc_config_with_disc(&disc))
            .await?;
        tracing::info!("fetched {} bank accounts", raw.len());
        let mut banks = HashMap::new();
        for (addr, account) in raw {
            match parse_bank(&addr, &account.data) {
                Ok(cfg) => { banks.insert(addr, cfg); }
                Err(e)  => tracing::warn!(bank = %addr, err = %e, "bank parse failed"),
            }
        }
        Ok(banks)
    }

    async fn load_positions(&self, rpc: &Rpc) -> Result<Vec<RawPosition>> {
        let disc = anchor_disc("MarginfiAccount");
        let raw = rpc.client
            .get_program_accounts_with_config(&self.program_id, rpc_config_with_disc(&disc))
            .await?;
        tracing::info!("fetched {} marginfi accounts", raw.len());
        let mut positions = Vec::new();
        for (addr, account) in raw {
            match parse_marginfi_account(&addr, &account.data) {
                Ok(Some(pos)) => positions.push(pos),
                Ok(None)      => {}
                Err(e)        => tracing::warn!(account = %addr, err = %e, "parse failed"),
            }
        }
        Ok(positions)
    }
}