//! Domain types shared across the bot.

use solana_sdk::pubkey::Pubkey;

#[derive(Debug, Clone)]
pub struct BankConfig {
    pub address: Pubkey,
    pub mint: Pubkey,
    pub oracle: Pubkey,
    pub decimals: u8,
    pub asset_weight_maint: f64,
    pub liab_weight_maint: f64,
    pub asset_share_value: f64,
    pub liab_share_value: f64,
    pub group: Pubkey,
    pub liquidity_vault: Pubkey,
    pub insurance_vault: Pubkey,
}

#[derive(Debug, Clone)]
pub struct RawPosition {
    pub address: Pubkey,
    pub owner: Pubkey,
    pub deposits: Vec<(Pubkey, f64)>,
    pub borrows: Vec<(Pubkey, f64)>,
}

#[derive(Debug, Clone)]
pub struct PricedAsset {
    pub mint: Pubkey,
    pub amount: f64,
    pub price: f64,
    pub weight: f64,
}

/// Forward-looking risk buckets: how many positions sit at each health
/// threshold across ALL scanned positions (not just currently
/// liquidatable). This is the opportunity pipeline.
#[derive(Debug, Clone, Default)]
pub struct RiskBuckets {
    pub liquidatable: usize, // hf < 1.00
    pub edge:         usize, // hf < 1.02
    pub at_risk:      usize, // hf < 1.05
    pub watch:        usize, // hf < 1.10
    pub total_priced: usize, // every position fully priced this scan
}

#[derive(Debug, Clone)]
pub struct WhaleRow {
    pub position: Pubkey,
    pub owner: Pubkey,
    pub debt_usd: f64,
    pub health_factor: f64,
    pub liquidatable: bool,
}

#[derive(Debug, Clone)]
pub struct DivergenceRow {
    pub mint: Pubkey,
    pub sources: usize,
    pub min_price: f64,
    pub max_price: f64,
    pub spread_pct: f64,
}

/// Chain state snapshot, taken once per scan.
#[derive(Debug, Clone, Default)]
pub struct ChainState {
    pub slot: u64,
    pub priority_fee_microlamports: u64,
}