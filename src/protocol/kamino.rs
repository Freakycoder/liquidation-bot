//! Kamino Lend implementation.
//!
//! Kamino Lend is a Solana borrow/lend protocol whose on-chain accounts
//! follow Anchor layouts, the same general shape as MarginFi but with
//! different structs:
//!   - Reserve: one per supported token (analogous to MarginFi's Bank).
//!     Holds the mint, decimals, the configured oracle (Pyth or Switchboard),
//!     LTV/liquidation-threshold/liquidation-bonus, the lending-market
//!     parent, and the collateral/liquidity exchange rates.
//!   - Obligation: per-borrower position. Holds up to 8 deposit slots and
//!     8 borrow slots, each pointing at a Reserve plus the amount.
//!
//! The byte offsets below come from the published Kamino Lend struct
//! definitions. They are UNVERIFIED against live dumps in this build. The
//! parser guards reject implausible reads the same way the MarginFi parser
//! does, so a wrong offset is reported, not trusted. Verify by dumping one
//! Reserve and one Obligation from mainnet (see CROSS-CHECK STEPS at the
//! bottom of this file) before relying on the numbers.

use super::LendingProtocol;
use crate::{rpc::Rpc, types::{BankConfig, RawPosition}};
use anyhow::Result;
use async_trait::async_trait;
use solana_account_decoder::UiAccountEncoding;
use solana_client::rpc_config::{RpcAccountInfoConfig, RpcProgramAccountsConfig};
use solana_client::rpc_filter::{Memcmp, MemcmpEncodedBytes, RpcFilterType};
use solana_sdk::{commitment_config::CommitmentConfig, pubkey::Pubkey};
use std::{collections::HashMap, str::FromStr};

/// Kamino Lend mainnet program ID.
const KAMINO_LEND_PROGRAM_ID: &str = "KLend2g3cP87fffoy8q1mQqGKjrxjC8boSyAYavgmjD";

// ── Reserve byte offsets ────────────────────────────────────────────────────
//
// Reserve layout (post 8-byte Anchor disc):
//   version:                    u64       at offset 8
//   last_update.slot:           u64       at offset 16
//   last_update.stale:          u8        at offset 24
//   last_update.price_status:   u8        at offset 25
//   last_update.placeholder:    [u8; 6]   at offset 26
//   lending_market:             Pubkey    at offset 32
//   farm_collateral:            Pubkey    at offset 64
//   farm_debt:                  Pubkey    at offset 96
//   liquidity (ReserveLiquidity) at offset 128 — large struct, see below
//   reserve_liquidity_padding   ...
//   collateral (ReserveCollateral) — exchange rate, mint info
//   config (ReserveConfig) — LTV, liquidation threshold/bonus, oracle keys
//
// ReserveLiquidity (starts at 128):
//   mint_pubkey:                Pubkey  at +0    (= 128)
//   mint_decimals:              u64     at +32   (= 160)
//   supply_vault:               Pubkey  at +40   (= 168)
//   fee_vault:                  Pubkey  at +72   (= 200)
//   available_amount:           u64     at +104  (= 232)
//   borrowed_amount_sf:         u128    at +112  (= 240)
//   market_price_sf:            u128    at +128  (= 256) ← live oracle price, scaled
//   market_price_last_updated_ts u64    at +144  (= 272)
//
// ReserveConfig oracle keys: the active price feed pubkey lives in the
// config block. Kamino historically used Pyth legacy and Switchboard V2;
// newer reserves use Pyth Pull. The "token_info" sub-struct inside config
// carries scope_configuration / pyth_configuration / switchboard_config
// variants and each has its own oracle key field.
const RESERVE_MINT:               usize = 128;
const RESERVE_MINT_DECIMALS:      usize = 160;
const RESERVE_LIQUIDITY_VAULT:    usize = 168;
const RESERVE_MARKET_PRICE_SF:    usize = 256;
const RESERVE_LENDING_MARKET:     usize = 32;
// The configured oracle pubkey location varies by oracle type. The "scope
// chain" / pyth / switchboard configurations each store a 32-byte pubkey,
// and the active one is identified by an enum byte. For the first cut, we
// read the price directly from market_price_sf (which Kamino refreshes
// each block from the oracle) and skip resolving the oracle account at
// all. This is the cleaner approach for Kamino specifically: the protocol
// already publishes the price inline, no separate oracle fetch needed.
//
// market_price_sf is a u128 scaled by 2^60 (Kamino's "scaled fraction").
const KAMINO_SF_SHIFT: u32 = 60;

// ── Obligation byte offsets ─────────────────────────────────────────────────
//
// Obligation layout (post 8-byte Anchor disc):
//   tag:                        u64       at offset 8
//   last_update:                LastUpdate at offset 16 (16 bytes)
//   lending_market:             Pubkey    at offset 32
//   owner:                      Pubkey    at offset 64
//   deposits:                   [ObligationCollateral; 8]  at offset 96
//   ...
//   borrows:                    [ObligationLiquidity; 5]   at offset 96 + 8*72 = 672
//
// ObligationCollateral (72 bytes each):
//   deposit_reserve:            Pubkey  at +0
//   deposited_amount:           u64     at +32
//   market_value_sf:            u128    at +40
//   padding:                    [u8; 16] at +56
//
// ObligationLiquidity (112 bytes each):
//   borrow_reserve:             Pubkey  at +0
//   cumulative_borrow_rate_sf:  u128    at +32
//   padding:                    [u8; 16] at +48
//   borrowed_amount_sf:         u128    at +64   ← scaled-fraction amount
//   market_value_sf:            u128    at +80
//   padding:                    [u8; 16] at +96
//
// A slot is "active" iff deposit_reserve/borrow_reserve != Pubkey::default()
// (Kamino zeroes inactive slots rather than using an explicit active flag).

const OBLIGATION_OWNER:             usize = 64;
const OBLIGATION_DEPOSITS_START:    usize = 96;
const OBLIGATION_DEPOSIT_STRIDE:    usize = 72;
const OBLIGATION_DEPOSIT_SLOTS:     usize = 8;
const DEPOSIT_RESERVE:              usize = 0;
const DEPOSIT_AMOUNT:               usize = 32;

const OBLIGATION_BORROWS_START:     usize = 96 + 8 * 72; // 672
const OBLIGATION_BORROW_STRIDE:     usize = 112;
const OBLIGATION_BORROW_SLOTS:      usize = 5;
const BORROW_RESERVE:               usize = 0;
const BORROW_AMOUNT_SF:             usize = 64;

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

/// Convert Kamino's "scaled fraction" (u128 with 60 implicit fractional
/// bits) to f64.
fn sf_to_f64(raw: u128) -> f64 {
    (raw as f64) / ((1u128 << KAMINO_SF_SHIFT) as f64)
}

fn parse_reserve(address: &Pubkey, data: &[u8]) -> Result<BankConfig> {
    let mint           = pubkey_at(data, RESERVE_MINT)?;
    let decimals_u64   = u64_at(data, RESERVE_MINT_DECIMALS)?;
    let liquidity_vault = pubkey_at(data, RESERVE_LIQUIDITY_VAULT)?;
    let lending_market = pubkey_at(data, RESERVE_LENDING_MARKET)?;
    let market_price_sf = u128_at(data, RESERVE_MARKET_PRICE_SF)?;

    if decimals_u64 > 18 {
        anyhow::bail!("kamino reserve decimals out of range ({decimals_u64})");
    }
    let decimals = decimals_u64 as u8;

    // Convert the scaled-fraction market price to a plain USD price. Kamino
    // already publishes the per-token USD price here, so we do not need to
    // resolve a separate oracle account. We surface this through the same
    // BankConfig the scanner uses by storing it as a synthetic "share_value"
    // and using a constant oracle key per bank that the OracleClient maps
    // identity-style: but simpler is to set asset_share_value/liab_share_value
    // to 1.0 here and let the protocol report the price directly.
    //
    // For this first pass we embed the price into the share_value channel:
    // we set decimals=0 so the scanner's amount = shares * share_value /
    // 10^0 = shares * share_value formula yields shares*price directly.
    // That keeps the existing scanner contract unchanged at the cost of
    // a slightly awkward overload of share_value's semantics for Kamino.
    let usd_price = sf_to_f64(market_price_sf);
    if !(0.000_001..=1_000_000.0).contains(&usd_price) {
        anyhow::bail!("kamino market_price_sf out of plausible range: {usd_price}");
    }

    // Kamino's liquidation-threshold and ltv are stored in the config block
    // as percentage values. We fill them in conservatively here and refine
    // once we dump a real Reserve. The maintenance weights used by the
    // health math are derived from the liquidation threshold:
    //   asset_weight_maint = liquidation_threshold (e.g. 0.85)
    //   liab_weight_maint  = 1.0 (Kamino does not inflate debt the way
    //                             MarginFi does, but using 1.0 here is
    //                             conservative and matches Kamino's
    //                             "borrow factor = 1" reserves).
    // PLACEHOLDER until config block offsets are verified:
    let asset_weight_maint = 0.85;
    let liab_weight_maint  = 1.0;

    Ok(BankConfig {
        address: *address,
        mint,
        // Kamino does not use a separate oracle account: the price lives in
        // the Reserve itself. The oracle field is filled with the Reserve's
        // own address so the scanner has a non-zero pubkey, and the
        // Kamino-specific OracleClient path treats this address as a marker.
        oracle: *address,
        decimals,
        asset_weight_maint,
        liab_weight_maint,
        // SEE COMMENT ABOVE: we embed price into share_value so the scanner's
        // existing amount formula produces the right number without changing
        // the scanner. asset and liab share the same per-token price.
        asset_share_value: usd_price,
        liab_share_value: usd_price,
        group: lending_market,
        liquidity_vault,
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
    fn name(&self) -> &'static str { "kamino-lend" }

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

// ────────────────────────────────────────────────────────────────────────────
// CROSS-CHECK STEPS (do these before trusting any number this module produces)
// ────────────────────────────────────────────────────────────────────────────
//
// 1. Reserve dump: pick a known Kamino reserve (e.g. SOL or USDC) and run
//      solana account <RESERVE_PUBKEY> --url <rpc> --output-file r.bin
//      xxd r.bin | head -60
//    Verify:
//      - bytes 8..16 are a small u64 (version, typically 1)
//      - bytes 32..64 are the lending_market pubkey (a published Kamino mkt)
//      - bytes 128..160 are the token mint pubkey
//      - bytes 160..168 hold the decimals as u64 (9 for SOL, 6 for USDC)
//      - bytes 256..272 are the u128 market_price_sf. Divide by 2^60. For
//        USDC expect ~1.0; for SOL expect ~SOL's current spot price.
//    If any of these are off, the offset for that field is wrong; adjust
//    the const at the top of this file.
//
// 2. Obligation dump: pick an obligation (Kamino UI -> open a position ->
//    use its address) and run the same xxd. Verify:
//      - owner at offset 64
//      - deposit_reserve pubkeys at 96, 168, 240, ... at stride 72
//      - borrow_reserve pubkeys at 672, 784, ... at stride 112
//      - borrowed_amount_sf at +64 within each borrow slot decodes to a
//        sensible token amount once divided by 2^60.
//
// 3. After verifying, run the scanner with protocol="kamino" in config.toml
//    and look at the dashboard. Coverage should be 100% of reserves (since
//    Kamino publishes prices inline) and opportunities should match what
//    Kamino's own UI shows as liquidatable.