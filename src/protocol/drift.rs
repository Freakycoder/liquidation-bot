//! Drift Protocol v2 spot-lending integration.
//!
//! Offsets derived from the on-chain IDL and verified against live dumps:
//!   - SpotMarket `3x85u7SW...` (iSOL market, decimals=9, marketIndex=1)
//!   - User       `JEGbXkUW...` ("Main Account", 1 active spot position)
//!
//! Drift uses precision constants rather than 2^60 scaling:
//!   SPOT_BALANCE_PRECISION              = 10^9
//!   SPOT_CUMULATIVE_INTEREST_PRECISION  = 10^10
//!   SPOT_WEIGHT_PRECISION               = 10^4
//!   PRICE_PRECISION                     = 10^6  (used by Pyth oracles directly)
//!
//! Native token amount of a position =
//!     scaledBalance * cumulativeInterest / 10^10 / 10^decimals
//!
//! To make this fit the scanner's
//!     amount = shares * share_value / 10^decimals
//! formula, we encode:
//!     share_value = cumulativeInterest / 10^10   (asset_share for deposits,
//!                                                 liab_share for borrows)
//!     decimals    = mint_decimals
//!     oracle      = real Pyth oracle pubkey from SpotMarket
//!
//! Drift positions live INSIDE the User account (8-slot SpotPosition array),
//! not as separate accounts. One User → up to 8 (reserve, amount) entries
//! split between deposits and borrows by SpotBalanceType.

use super::{ImplStatus, LendingProtocol};
use crate::{rpc::Rpc, types::{BankConfig, RawPosition}};
use anyhow::Result;
use async_trait::async_trait;
use solana_account_decoder::UiAccountEncoding;
use solana_client::rpc_config::{RpcAccountInfoConfig, RpcProgramAccountsConfig};
use solana_client::rpc_filter::{Memcmp, MemcmpEncodedBytes, RpcFilterType};
use solana_sdk::{commitment_config::CommitmentConfig, pubkey::Pubkey};
use std::{collections::HashMap, str::FromStr};

const DRIFT_PROGRAM_ID: &str = "dRiftyHA39MWEi3m9aunc5MzRF1JYuBsbn6VPcn33UH";

// ─── SpotMarket offsets (verified against live SOL/iSOL market dump) ────────
// After 8-byte disc: pubkey(32), oracle(32), mint(32), vault(32), name[32],
// HistoricalOracleData(48), HistoricalIndexData(40), revenuePool(24),
// spotFeePool(24), InsuranceFund(112), totalSpotFee(u128=16),
// depositBalance(u128=16), borrowBalance(u128=16),
// cumulativeDepositInterest(u128=16), cumulativeBorrowInterest(u128=16),
// then the weights and decimals further down.
const SM_ORACLE:                  usize = 40;
const SM_MINT:                    usize = 72;
const SM_VAULT:                   usize = 104;
const SM_CUMULATIVE_DEPOSIT_INT:  usize = 464;
const SM_CUMULATIVE_BORROW_INT:   usize = 480;
const SM_MAINT_ASSET_WEIGHT:      usize = 644;
const SM_MAINT_LIAB_WEIGHT:       usize = 652;
const SM_DECIMALS:                usize = 680;
const SM_MARKET_INDEX:            usize = 684;

// ─── User offsets (verified against live dump JEGbXkUW...) ──────────────────
// After 8-byte disc: authority(32), delegate(32), name[32], then
// spotPositions[SpotPosition; 8] at offset 104, each slot 40 bytes.
const USER_AUTHORITY:           usize = 8;
const USER_SPOT_POSITIONS_START: usize = 104;
const USER_SPOT_POSITION_STRIDE: usize = 40;
const USER_SPOT_POSITION_SLOTS:  usize = 8;
// Within each SpotPosition slot:
const SPOT_SCALED_BALANCE:       usize = 0;   // u64
const SPOT_MARKET_INDEX:         usize = 32;  // u16
const SPOT_BALANCE_TYPE:         usize = 34;  // u8: 0=Deposit, 1=Borrow

const SPOT_BALANCE_PRECISION:        f64 = 1e9;   // not directly used; here for documentation
const SPOT_CUMULATIVE_INT_PRECISION: f64 = 1e10;
const SPOT_WEIGHT_PRECISION:         f64 = 1e4;

const BALANCE_TYPE_DEPOSIT: u8 = 0;
const BALANCE_TYPE_BORROW:  u8 = 1;

pub struct Drift { pub program_id: Pubkey }

impl Drift {
    pub fn new() -> Self {
        Self { program_id: Pubkey::from_str(DRIFT_PROGRAM_ID).unwrap() }
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

fn u32_at(data: &[u8], off: usize) -> Result<u32> {
    let b: [u8; 4] = data.get(off..off + 4)
        .ok_or_else(|| anyhow::anyhow!("u32_at({off}): short"))?
        .try_into()?;
    Ok(u32::from_le_bytes(b))
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

fn u16_at(data: &[u8], off: usize) -> Result<u16> {
    let b: [u8; 2] = data.get(off..off + 2)
        .ok_or_else(|| anyhow::anyhow!("u16_at({off}): short"))?
        .try_into()?;
    Ok(u16::from_le_bytes(b))
}

fn u8_at(data: &[u8], off: usize) -> Result<u8> {
    data.get(off).copied()
        .ok_or_else(|| anyhow::anyhow!("u8_at({off}): short"))
}

/// Drift indexes spot markets by `marketIndex`, not by their account pubkey.
/// Positions reference market_index; we look up the BankConfig by a
/// synthesized "bank id" derived from the index. To bridge into the
/// scanner's Pubkey-keyed banks map, we store SpotMarket records under
/// their real PDA pubkey but ALSO keep an index->pubkey side map. The
/// scanner only consumes the HashMap<Pubkey, BankConfig> view, so we
/// materialize position entries with the SpotMarket pubkey as the bank key.
struct SpotMarketParseResult {
    pubkey: Pubkey,
    market_index: u16,
    config: BankConfig,
}

fn parse_spot_market(address: &Pubkey, data: &[u8]) -> Result<SpotMarketParseResult> {
    let oracle = pubkey_at(data, SM_ORACLE)?;
    let mint   = pubkey_at(data, SM_MINT)?;
    let vault  = pubkey_at(data, SM_VAULT)?;

    let cdi = u128_at(data, SM_CUMULATIVE_DEPOSIT_INT)?;
    let cbi = u128_at(data, SM_CUMULATIVE_BORROW_INT)?;
    let asset_share_value = (cdi as f64) / SPOT_CUMULATIVE_INT_PRECISION;
    let liab_share_value  = (cbi as f64) / SPOT_CUMULATIVE_INT_PRECISION;

    // Both should be >= 1.0 (interest is monotonically non-decreasing
    // from 1.0). Plausibility guard against wrong offsets.
    if !(0.5..=100.0).contains(&asset_share_value)
        || !(0.5..=100.0).contains(&liab_share_value)
    {
        anyhow::bail!(
            "drift cumulative interest implausible: deposit={asset_share_value}, borrow={liab_share_value}"
        );
    }

    let maint_asset_w_raw = u32_at(data, SM_MAINT_ASSET_WEIGHT)?;
    let maint_liab_w_raw  = u32_at(data, SM_MAINT_LIAB_WEIGHT)?;
    let asset_weight_maint = (maint_asset_w_raw as f64) / SPOT_WEIGHT_PRECISION;
    let liab_weight_maint  = (maint_liab_w_raw  as f64) / SPOT_WEIGHT_PRECISION;

    let decimals_u32 = u32_at(data, SM_DECIMALS)?;
    if decimals_u32 > 18 {
        anyhow::bail!("drift spot market decimals out of range: {decimals_u32}");
    }
    let market_index = u16_at(data, SM_MARKET_INDEX)?;

    Ok(SpotMarketParseResult {
        pubkey: *address,
        market_index,
        config: BankConfig {
            address: *address,
            mint,
            oracle,
            decimals: decimals_u32 as u8,
            asset_weight_maint,
            liab_weight_maint,
            asset_share_value,
            liab_share_value,
            group: Pubkey::default(),    // Drift has no group-equivalent
            liquidity_vault: vault,
            insurance_vault: Pubkey::default(),
        },
    })
}

fn parse_user(
    address: &Pubkey,
    data: &[u8],
    market_index_to_pubkey: &HashMap<u16, Pubkey>,
) -> Result<Option<RawPosition>> {
    let owner = pubkey_at(data, USER_AUTHORITY)?;
    let mut deposits = Vec::new();
    let mut borrows  = Vec::new();

    for slot in 0..USER_SPOT_POSITION_SLOTS {
        let base = USER_SPOT_POSITIONS_START + slot * USER_SPOT_POSITION_STRIDE;
        let scaled_balance = u64_at(data, base + SPOT_SCALED_BALANCE)?;
        if scaled_balance == 0 { continue; }

        let market_index = u16_at(data, base + SPOT_MARKET_INDEX)?;
        let balance_type = u8_at(data, base + SPOT_BALANCE_TYPE)?;

        let bank_pk = match market_index_to_pubkey.get(&market_index) {
            Some(pk) => *pk,
            None => continue,  // unknown spot market (shouldn't happen, skip safely)
        };

        let shares = scaled_balance as f64;
        match balance_type {
            BALANCE_TYPE_DEPOSIT => deposits.push((bank_pk, shares)),
            BALANCE_TYPE_BORROW  => borrows.push((bank_pk, shares)),
            _ => continue,
        }
    }

    if borrows.is_empty() { return Ok(None); }
    Ok(Some(RawPosition { address: *address, owner, deposits, borrows }))
}

#[async_trait]
impl LendingProtocol for Drift {
    fn name(&self) -> &'static str { "drift" }
    fn status(&self) -> ImplStatus { ImplStatus::Live }

    async fn load_banks(&self, rpc: &Rpc) -> Result<HashMap<Pubkey, BankConfig>> {
        let disc = anchor_disc("SpotMarket");
        let raw = rpc.client
            .get_program_accounts_with_config(&self.program_id, rpc_config_with_disc(&disc))
            .await?;
        tracing::info!("fetched {} drift spot markets", raw.len());

        // Two views: the Pubkey-keyed map the scanner consumes, plus a
        // market_index -> pubkey side map for position parsing.
        let mut banks = HashMap::new();
        let mut index_map: HashMap<u16, Pubkey> = HashMap::new();
        for (addr, account) in raw {
            match parse_spot_market(&addr, &account.data) {
                Ok(r) => {
                    index_map.insert(r.market_index, r.pubkey);
                    banks.insert(addr, r.config);
                }
                Err(e) => tracing::warn!(market = %addr, err = %e, "drift spot market parse failed"),
            }
        }
        // Stash the index map on the type by writing it into a global slot.
        // (Drift's load_positions needs it; we pass it through a once_cell.)
        DRIFT_INDEX_MAP.write().unwrap().clone_from(&index_map);
        Ok(banks)
    }

    async fn load_positions(
        &self,
        rpc: &Rpc,
        _banks: &HashMap<Pubkey, BankConfig>,
    ) -> Result<Vec<RawPosition>> {
        let index_map = DRIFT_INDEX_MAP.read().unwrap().clone();
        if index_map.is_empty() {
            tracing::warn!("drift: no spot markets loaded yet; skipping positions");
            return Ok(Vec::new());
        }

        let disc = anchor_disc("User");
        let raw = rpc.client
            .get_program_accounts_with_config(&self.program_id, rpc_config_with_disc(&disc))
            .await?;
        tracing::info!("fetched {} drift user accounts", raw.len());

        let mut positions = Vec::new();
        for (addr, account) in raw {
            match parse_user(&addr, &account.data, &index_map) {
                Ok(Some(pos)) => positions.push(pos),
                Ok(None)      => {}
                Err(e)        => tracing::warn!(user = %addr, err = %e, "drift user parse failed"),
            }
        }
        Ok(positions)
    }
}

// Side-channel for the market_index -> pubkey map. Set during load_banks,
// read during load_positions. RwLock because banks is sometimes reloaded
// while positions are mid-parse.
use std::sync::RwLock;
use once_cell::sync::Lazy;
static DRIFT_INDEX_MAP: Lazy<RwLock<HashMap<u16, Pubkey>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));