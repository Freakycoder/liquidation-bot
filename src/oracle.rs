use crate::rpc::Rpc;
use anyhow::Result;
use solana_sdk::pubkey::Pubkey;
use std::collections::HashMap;

pub struct OracleClient;

impl OracleClient {
    pub fn new() -> Self { Self }

    pub async fn get_prices(
        &self,
        rpc: &Rpc,
        oracles: &[Pubkey],
    ) -> Result<HashMap<Pubkey, f64>> {
        let accounts = rpc.get_accounts(oracles).await?;
        let mut prices = HashMap::new();
        let mut failed = 0usize;

        for (key, acc) in accounts {
            let Some(acc) = acc else { continue };
            match parse_any_oracle(&acc.data) {
                Ok(price) => { prices.insert(key, price); }
                Err(_)    => { failed += 1; }
            }
        }
        if failed > 0 {
            tracing::info!(failed, parsed = prices.len(), "oracle parse summary");
        }
        Ok(prices)
    }
}

/// Try each known oracle format in turn.
fn parse_any_oracle(data: &[u8]) -> Result<f64> {
    if let Ok(p) = parse_pyth_pull(data)   { return Ok(p); }
    if let Ok(p) = parse_pyth_legacy(data) { return Ok(p); }
    if let Ok(p) = parse_switchboard(data) { return Ok(p); }
    anyhow::bail!("no known oracle format matched ({} bytes)", data.len())
}

/// Pyth Pull: PriceUpdateV2 account.
/// 8 disc | 32 write_authority | 1 verification_level |
/// PriceFeedMessage: 32 feed_id | 8 price(i64) | 8 conf(u64) | 4 expo(i32) ...
fn parse_pyth_pull(data: &[u8]) -> Result<f64> {
    const PRICE_OFF: usize = 8 + 32 + 1 + 32; // 73
    const EXPO_OFF:  usize = PRICE_OFF + 8 + 8; // 89
    if data.len() < EXPO_OFF + 4 {
        anyhow::bail!("too short for PriceUpdateV2");
    }
    let price = i64::from_le_bytes(data[PRICE_OFF..PRICE_OFF + 8].try_into()?);
    let expo  = i32::from_le_bytes(data[EXPO_OFF..EXPO_OFF + 4].try_into()?);
    if price <= 0 { anyhow::bail!("non-positive pull price"); }
    // exponent is typically negative; guard against absurd values
    if !(-18..=0).contains(&expo) { anyhow::bail!("implausible expo {expo}"); }
    Ok(price as f64 * 10f64.powi(expo))
}

/// Pyth legacy push account.
fn parse_pyth_legacy(data: &[u8]) -> Result<f64> {
    const MAGIC: u32 = 0xa1b2c3d4;
    if data.len() < 224 { anyhow::bail!("too short for legacy pyth"); }
    let magic = u32::from_le_bytes(data[0..4].try_into()?);
    if magic != MAGIC { anyhow::bail!("bad magic"); }
    let expo  = i32::from_le_bytes(data[20..24].try_into()?);
    let price = i64::from_le_bytes(data[208..216].try_into()?);
    if price <= 0 { anyhow::bail!("non-positive legacy price"); }
    Ok(price as f64 * 10f64.powi(expo))
}

/// Switchboard On-Demand PullFeedAccountData.
/// The accepted result is an i128 scaled by 1e18, located in the `result`
/// sub-struct. Offset derived from the account layout: 8 disc + 32 + ...
/// VERIFY against a live Switchboard feed; the offset below is the
/// documented position of `result.value` and may need adjustment.
fn parse_switchboard(data: &[u8]) -> Result<f64> {
    const VALUE_OFF: usize = 8 + 32 + 8; // disc + feed_hash + ... -> result.value
    if data.len() < VALUE_OFF + 16 {
        anyhow::bail!("too short for switchboard");
    }
    let raw = i128::from_le_bytes(data[VALUE_OFF..VALUE_OFF + 16].try_into()?);
    if raw <= 0 { anyhow::bail!("non-positive switchboard value"); }
    let price = raw as f64 / 1e18;
    if !(0.0001..=1_000_000.0).contains(&price) {
        anyhow::bail!("implausible switchboard price {price}");
    }
    Ok(price)
}