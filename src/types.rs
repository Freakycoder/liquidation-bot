//! Domain types shared across the bot.
//!
//! Every protocol implementation maps its own on-chain account layouts
//! into these types, so the scanner and health logic stay protocol-agnostic.

use solana_sdk::pubkey::Pubkey;

/// Static config for one lending bank/reserve.
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
    /// The MarginFi group this bank belongs to. Parsed from the Bank
    /// account header (group pubkey at data offset 8, per the IDL).
    pub group: Pubkey,
    /// The bank's liquidity vault (holds the underlying token).
    pub liquidity_vault: Pubkey,
    /// The bank's insurance vault.
    pub insurance_vault: Pubkey,
}

/// A borrower account with raw share amounts (not yet converted to tokens).
/// Conversion: ui_amount = shares * share_value / 10^decimals
#[derive(Debug, Clone)]
pub struct RawPosition {
    pub address: Pubkey,
    pub owner: Pubkey,
    /// (bank address, asset shares)
    pub deposits: Vec<(Pubkey, f64)>,
    /// (bank address, liability shares)
    pub borrows: Vec<(Pubkey, f64)>,
}

/// An asset valued at a point in time, ready for health math.
#[derive(Debug, Clone)]
pub struct PricedAsset {
    pub mint: Pubkey,
    pub amount: f64,
    pub price: f64,
    pub weight: f64,
}