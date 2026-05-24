use crate::rpc::Rpc;
use anyhow::Result;
use solana_sdk::pubkey::Pubkey;
use std::collections::HashMap;

pub struct OracleClient;

impl OracleClient {
    pub fn new() -> Self {
        Self
    }

    /// Fetch and parse prices for the given oracle accounts.
    /// Returns a map of oracle pubkey -> USD price.
    /// Accounts that fail to fetch or parse are logged and skipped.
    pub async fn get_prices(
        &self,
        rpc: &Rpc,
        oracles: &[Pubkey],
    ) -> Result<HashMap<Pubkey, f64>> {
        let accounts = rpc.get_accounts(oracles).await?;
        let mut prices = HashMap::new();

        for (key, acc) in accounts {
            let Some(acc) = acc else {
                tracing::warn!(oracle = %key, "oracle account not found");
                continue;
            };
            match parse_pyth_push(&acc.data) {
                Ok(price) => {
                    tracing::debug!(oracle = %key, price, "fetched price");
                    prices.insert(key, price);
                }
                Err(e) => {
                    tracing::warn!(oracle = %key, err = %e, "price parse failed");
                }
            }
        }

        Ok(prices)
    }
}

/// Parse a Pyth **push** (legacy) price account into a USD f64.
///
/// Layout reference: https://github.com/pyth-network/pyth-client/blob/main/program/rust/src/accounts/price.rs
///
/// Byte offsets (all little-endian):
///   0  ..  4   magic         u32  must be 0xa1b2c3d4
///   4  ..  8   version       u32
///   8  .. 12   account type  u32  must be 3 (Price)
///  12  .. 16   size          u32
///  16  .. 20   price type    u32
///  20  .. 24   exponent      i32  <-- base-10 exponent
///  ...
/// 208  .. 216  agg.price     i64  <-- aggregate price mantissa
/// 216  .. 224  agg.conf      u64  confidence interval
///
/// Final value = agg.price * 10^exponent
///
/// NOTE: MarginFi banks may also use PythPull (price-update-v2) or
/// SwitchboardV2 oracles. Check each bank's oracle_setup field on Day 2.
/// This function only handles PythLegacy; skip banks with other types
/// until you add their parsers.
fn parse_pyth_push(data: &[u8]) -> Result<f64> {
    const MAGIC: u32 = 0xa1b2c3d4;
    const PRICE_ACCOUNT_TYPE: u32 = 3;
    const MIN_LEN: usize = 224;

    if data.len() < MIN_LEN {
        anyhow::bail!("account too short ({} bytes, need {MIN_LEN})", data.len());
    }

    let magic = u32::from_le_bytes(data[0..4].try_into()?);
    if magic != MAGIC {
        anyhow::bail!("bad magic 0x{magic:08x}, not a Pyth account");
    }

    let acct_type = u32::from_le_bytes(data[8..12].try_into()?);
    if acct_type != PRICE_ACCOUNT_TYPE {
        anyhow::bail!("account type {acct_type} is not a Price account (expected 3)");
    }

    let expo  = i32::from_le_bytes(data[20..24].try_into()?);
    let price = i64::from_le_bytes(data[208..216].try_into()?);

    if price <= 0 {
        anyhow::bail!("non-positive price mantissa ({price}), feed may be stale");
    }

    Ok(price as f64 * 10f64.powi(expo))
}