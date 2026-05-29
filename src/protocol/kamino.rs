//! Kamino Lend implementation.
//!
//! All byte offsets derived from the on-chain IDL (Reserve, ReserveLiquidity,
//! Obligation, ObligationCollateral, ObligationLiquidity) and cross-checked
//! against a live Reserve dump (reserve 8PYYKF4Z..., decimals=9, price≈$0.54).
//! Plausibility guards reject any read that doesn't make sense, the same
//! pattern the MarginFi parser uses.
//!
//! One unusual thing worth knowing: Kamino publishes the per-token USD price
//! INSIDE the Reserve account, as `marketPriceSf` (u128 scaled by 2^60). This
//! means there is no separate oracle account to fetch. The OracleClient is
//! bypassed entirely for Kamino. We encode the price into BankConfig by:
//!   - storing the USD price in `asset_share_value` and `liab_share_value`
//!   - setting `decimals = 0` so the scanner's formula
//!         amount = shares * share_value / 10^decimals
//!     reduces to `amount = shares * usd_price`
//!   - setting `oracle = reserve_address` so the scanner's
//!         prices.contains_key(&bank.oracle)
//!     coverage check works once we seed `prices` with reserve→1.0 entries.

use super::LendingProtocol;
use crate::{rpc::Rpc, types::{BankConfig, RawPosition}};
use anyhow::Result;
use async_trait::async_trait;
use solana_account_decoder::UiAccountEncoding;
use solana_client::rpc_config::{RpcAccountInfoConfig, RpcProgramAccountsConfig};
use solana_client::rpc_filter::{Memcmp, MemcmpEncodedBytes, RpcFilterType};
use solana_sdk::{commitment_config::CommitmentConfig, pubkey::Pubkey};
use std::{collections::HashMap, str::FromStr};

const KAMINO_LEND_PROGRAM_ID: &str = "KLend2g3cP87fffoy8q1mQqGKjrxjC8boSyAYavgmjD";

// ─── Reserve offsets (verified against IDL + live dump) ─────────────────────
// After 8-byte disc:
//   version u64 @8, lastUpdate (16 bytes) @16,
//   lendingMarket @32, farmCollateral @64, farmDebt @96,
//   liquidity (ReserveLiquidity) @128.
// Inside ReserveLiquidity (starting at 128):
//   mintPubkey @+0=128, supplyVault @+32=160, feeVault @+64=192,
//   totalAvailableAmount u64 @+96=224,
//   borrowedAmountSf u128 @+104=232,
//   marketPriceSf u128 @+120=248,            ← USD price, scaled by 2^60
//   marketPriceLastUpdatedTs u64 @+136=264,
//   mintDecimals u64 @+144=272.
const RESERVE_LENDING_MARKET:  usize = 32;
const RESERVE_MINT:            usize = 128;
const RESERVE_SUPPLY_VAULT:    usize = 160;
const RESERVE_MARKET_PRICE_SF: usize = 248;
const RESERVE_MINT_DECIMALS:   usize = 272;

// ─── Obligation offsets (verified against IDL) ──────────────────────────────
// After 8-byte disc:
//   tag u64 @8, lastUpdate @16, lendingMarket @32, owner @64.
//   deposits [ObligationCollateral; 8] starts at 96; each slot is 136 bytes.
//   After deposits at 1184:
//     lowestReserveDepositLiquidationLtv u64 (8),
//     depositedValueSf u128 (16),
//   then borrows [ObligationLiquidity; 5] at 1208; each slot is 200 bytes.
//
// ObligationCollateral (136 bytes): depositReserve @+0, depositedAmount u64 @+32.
// ObligationLiquidity (200 bytes):  borrowReserve @+0, borrowedAmountSf u128 @+88.
const OBLIGATION_OWNER:          usize = 64;
const OBLIGATION_DEPOSITS_START: usize = 96;
const OBLIGATION_DEPOSIT_STRIDE: usize = 136;
const OBLIGATION_DEPOSIT_SLOTS:  usize = 8;
const DEPOSIT_RESERVE:           usize = 0;
const DEPOSIT_AMOUNT:            usize = 32;
const OBLIGATION_BORROWS_START:  usize = 1208;
const OBLIGATION_BORROW_STRIDE:  usize = 200;
const OBLIGATION_BORROW_SLOTS:   usize = 5;
const BORROW_RESERVE:            usize = 0;
const BORROW_AMOUNT_SF:          usize = 88;

const KAMINO_SF_SHIFT: u32 = 60;

pub struct Kamino { pub program_id: Pubkey }

impl Kamino {
    pub fn new() -> Self {
        Self { program_id: Pubkey::from_str(KAMINO_LEND_PROGRAM_ID).unwrap() }
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

fn u64_at(data: &[u8], off: usize) -> Result<u64> {
    let b: [u8; 8] = data.get(off..off + 8)
        .ok_or_else(|| anyhow::anyhow!("u64_at({off}): short"))?
        .try_into()?;
    Ok(u64::from_le_bytes(b))
}

fn u128_at(data: &[u8], off: usize) -> Result<u128> {
    let b: [u8; 16] = data.get(off..off + 16)
        .ok_or_else(|| anyhow::anyhow!("u128_at({off}): short"))?
        .try_into()?;
    Ok(u128::from_le_bytes(b))
}

fn sf_to_f64(raw: u128) -> f64 {
    (raw as f64) / ((1u128 << KAMINO_SF_SHIFT) as f64)
}

fn parse_reserve(address: &Pubkey, data: &[u8]) -> Result<BankConfig> {
    let mint            = pubkey_at(data, RESERVE_MINT)?;
    let supply_vault    = pubkey_at(data, RESERVE_SUPPLY_VAULT)?;
    let lending_market  = pubkey_at(data, RESERVE_LENDING_MARKET)?;
    let market_price_sf = u128_at(data, RESERVE_MARKET_PRICE_SF)?;
    let decimals_u64    = u64_at(data, RESERVE_MINT_DECIMALS)?;

    if decimals_u64 > 18 {
        anyhow::bail!("kamino reserve decimals out of range ({decimals_u64})");
    }
    let usd_price = sf_to_f64(market_price_sf);
    if !(0.000_001..=1_000_000.0).contains(&usd_price) {
        anyhow::bail!("kamino market_price_sf implausible: {usd_price}");
    }

    Ok(BankConfig {
        address: *address,
        mint,
        oracle: *address,                  // marker: we self-price via reserve
        decimals: 0,                        // see module comment: price math
        asset_weight_maint: 0.85,           // refined once we parse ReserveConfig
        liab_weight_maint:  1.0,
        asset_share_value: usd_price,
        liab_share_value:  usd_price,
        group: lending_market,
        liquidity_vault: supply_vault,
        insurance_vault: Pubkey::default(),
    })
}

fn parse_obligation(address: &Pubkey, data: &[u8]) -> Result<Option<RawPosition>> {
    let owner = pubkey_at(data, OBLIGATION_OWNER)?;
    let zero = Pubkey::default();

    let mut deposits = Vec::new();
    for i in 0..OBLIGATION_DEPOSIT_SLOTS {
        let base = OBLIGATION_DEPOSITS_START + i * OBLIGATION_DEPOSIT_STRIDE;
        let reserve = pubkey_at(data, base + DEPOSIT_RESERVE)?;
        if reserve == zero { continue; }
        let amount = u64_at(data, base + DEPOSIT_AMOUNT)? as f64;
        if amount > 0.0 { deposits.push((reserve, amount)); }
    }

    let mut borrows = Vec::new();
    for i in 0..OBLIGATION_BORROW_SLOTS {
        let base = OBLIGATION_BORROWS_START + i * OBLIGATION_BORROW_STRIDE;
        let reserve = pubkey_at(data, base + BORROW_RESERVE)?;
        if reserve == zero { continue; }
        let amount_sf = u128_at(data, base + BORROW_AMOUNT_SF)?;
        let amount = sf_to_f64(amount_sf);
        if amount > 0.0 { borrows.push((reserve, amount)); }
    }

    if borrows.is_empty() { return Ok(None); }
    Ok(Some(RawPosition { address: *address, owner, deposits, borrows }))
}

#[async_trait]
impl LendingProtocol for Kamino {
    fn name(&self) -> &'static str { "kamino" }

    async fn load_banks(&self, rpc: &Rpc) -> Result<HashMap<Pubkey, BankConfig>> {
        let disc = anchor_disc("Reserve");
        let raw = rpc.client
            .get_program_accounts_with_config(&self.program_id, rpc_config_with_disc(&disc))
            .await?;
        tracing::info!("fetched {} kamino reserves", raw.len());
        let mut banks = HashMap::new();
        for (addr, account) in raw {
            match parse_reserve(&addr, &account.data) {
                Ok(cfg) => { banks.insert(addr, cfg); }
                Err(e)  => tracing::warn!(reserve = %addr, err = %e, "kamino reserve parse failed"),
            }
        }
        Ok(banks)
    }

    async fn load_positions(&self, rpc: &Rpc) -> Result<Vec<RawPosition>> {
        let disc = anchor_disc("Obligation");
        let raw = rpc.client
            .get_program_accounts_with_config(&self.program_id, rpc_config_with_disc(&disc))
            .await?;
        tracing::info!("fetched {} kamino obligations", raw.len());
        let mut positions = Vec::new();
        for (addr, account) in raw {
            match parse_obligation(&addr, &account.data) {
                Ok(Some(pos)) => positions.push(pos),
                Ok(None)      => {}
                Err(e)        => tracing::warn!(obligation = %addr, err = %e, "kamino obligation parse failed"),
            }
        }
        Ok(positions)
    }
}