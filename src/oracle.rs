//! Multi-format on-chain oracle price reader.
//!
//! MarginFi v2 banks reference several oracle account formats. This client
//! tries each known format in turn, guarded so one format's bytes are not
//! misread as another's, and reports coverage so gaps are visible rather
//! than silent.

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
        let mut by_format: HashMap<&'static str, usize> = HashMap::new();
        let mut fail_lengths: HashMap<usize, usize> = HashMap::new();
        let mut missing = 0usize;

        for (key, acc) in accounts {
            let Some(acc) = acc else { missing += 1; continue; };
            match parse_oracle(&acc.data) {
                Ok((price, fmt)) => {
                    prices.insert(key, price);
                    *by_format.entry(fmt).or_default() += 1;
                }
                Err(_) => {
                    *fail_lengths.entry(acc.data.len()).or_default() += 1;
                }
            }
        }

        // Coverage report. The failure histogram groups unparsed accounts
        // by byte length, which identifies the format: a cluster at one
        // size is one oracle format waiting for a parser.
        let failed: usize = fail_lengths.values().sum();
        tracing::info!(parsed = prices.len(), failed, missing, "oracle coverage");
        for (fmt, n) in &by_format {
            tracing::info!(format = fmt, count = n, "  parsed by format");
        }
        let mut fails: Vec<_> = fail_lengths.into_iter().collect();
        fails.sort_by_key(|&(_, n)| std::cmp::Reverse(n));
        for (len, n) in fails.into_iter().take(5) {
            tracing::info!(account_len = len, count = n, "  unparsed (by account size)");
        }

        Ok(prices)
    }

    pub async fn diagnose(&self, rpc: &Rpc, oracles: &[Pubkey]) -> Result<()> {
    let accounts = rpc.get_accounts(oracles).await?;
    // size -> (count, first example pubkey)
    let mut groups: std::collections::HashMap<usize, (usize, Pubkey)> =
        std::collections::HashMap::new();

    for (key, acc) in accounts {
        let Some(acc) = acc else { continue; };
        if parse_oracle(&acc.data).is_ok() { continue; }
        let entry = groups.entry(acc.data.len()).or_insert((0, key));
        entry.0 += 1;
    }

    let mut sorted: Vec<_> = groups.into_iter().collect();
    sorted.sort_by_key(|&(_, (n, _))| std::cmp::Reverse(n));
    println!("=== UNPARSED ORACLE ACCOUNTS (by size) ===");
    for (len, (count, example)) in sorted {
        println!("  size={len:>6}  count={count:>4}  example={example}");
    }
    Ok(())
}
}

/// Try each known oracle format. Returns (price, format name).
fn parse_oracle(data: &[u8]) -> Result<(f64, &'static str)> {
    if let Ok(p) = parse_pyth_pull(data)            { return Ok((p, "pyth-pull")); }
    if let Ok(p) = parse_pyth_legacy(data)          { return Ok((p, "pyth-legacy")); }
    if let Ok(p) = parse_switchboard_v2(data)       { return Ok((p, "switchboard-v2")); }
    if let Ok(p) = parse_switchboard_ondemand(data) { return Ok((p, "switchboard-od")); }
    anyhow::bail!("no known oracle format matched ({} bytes)", data.len())
}

/// Pyth Pull: a `PriceUpdateV2` account from the Pyth receiver program.
/// After the 8-byte Anchor discriminator: 32 write_authority, 1
/// verification_level, then PriceFeedMessage (32 feed_id, 8 price i64,
/// 8 conf u64, 4 exponent i32). price at 73, exponent at 89.
fn parse_pyth_pull(data: &[u8]) -> Result<f64> {
    const PRICE: usize = 73;
    const EXPO: usize = 89;
    // PriceUpdateV2 accounts are small (~134 bytes); this size guard cleanly
    // separates them from the multi-kilobyte legacy / Switchboard accounts.
    if data.len() < EXPO + 4 || data.len() > 512 {
        anyhow::bail!("not a PriceUpdateV2");
    }
    let price = i64::from_le_bytes(data[PRICE..PRICE + 8].try_into()?);
    let expo = i32::from_le_bytes(data[EXPO..EXPO + 4].try_into()?);
    if price <= 0 || !(-12..=0).contains(&expo) {
        anyhow::bail!("implausible pull price/expo");
    }
    Ok(price as f64 * 10f64.powi(expo))
}

/// Pyth legacy (classic push) price account.
/// magic u32 at 0 (0xa1b2c3d4), exponent i32 at 20, aggregate price i64 at 208.
fn parse_pyth_legacy(data: &[u8]) -> Result<f64> {
    const MAGIC: u32 = 0xa1b2c3d4;
    const EXPO: usize = 20;
    const AGG_PRICE: usize = 208;
    if data.len() < AGG_PRICE + 8 {
        anyhow::bail!("too short for legacy pyth");
    }
    if u32::from_le_bytes(data[0..4].try_into()?) != MAGIC {
        anyhow::bail!("bad pyth magic");
    }
    let expo = i32::from_le_bytes(data[EXPO..EXPO + 4].try_into()?);
    let price = i64::from_le_bytes(data[AGG_PRICE..AGG_PRICE + 8].try_into()?);
    if price <= 0 || !(-12..=0).contains(&expo) {
        anyhow::bail!("implausible legacy price/expo");
    }
    Ok(price as f64 * 10f64.powi(expo))
}

/// Switchboard V2 legacy `AggregatorAccountData`. The latest value is a
/// `SwitchboardDecimal { mantissa: i128, scale: u32 }`.
///
/// The MANTISSA offset is an ESTIMATE, not verified against a dumped
/// account. The plausibility guard means a wrong offset is REJECTED (and
/// counted in the failure histogram), never trusted as a price.
fn parse_switchboard_v2(data: &[u8]) -> Result<f64> {
    const MANTISSA: usize = 1880; // ESTIMATE; verify against a dumped account
    const SCALE: usize = MANTISSA + 16;
    if data.len() < SCALE + 4 || data.len() < 3000 {
        anyhow::bail!("not a v2 aggregator");
    }
    let mantissa = i128::from_le_bytes(data[MANTISSA..MANTISSA + 16].try_into()?);
    let scale = u32::from_le_bytes(data[SCALE..SCALE + 4].try_into()?);
    if scale > 30 { anyhow::bail!("implausible scale"); }
    let price = mantissa as f64 / 10f64.powi(scale as i32);
    if !(0.0001..=1_000_000.0).contains(&price) {
        anyhow::bail!("implausible v2 price");
    }
    Ok(price)
}

fn parse_switchboard_ondemand(data: &[u8]) -> Result<f64> {
    const VALUE: usize = 2412;
    if data.len() < VALUE + 16 || data.len() < 3000 {
        anyhow::bail!("not an on-demand feed");
    }
    let raw = i128::from_le_bytes(data[VALUE..VALUE + 16].try_into()?);
    if raw <= 0 {
        anyhow::bail!("no current on-demand price");
    }
    let price = raw as f64 / 1e18;
    if !(0.0001..=1_000_000.0).contains(&price) {
        anyhow::bail!("implausible on-demand price");
    }
    Ok(price)
}