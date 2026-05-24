//! Domain types shared across the bot.
//!
//! Every protocol implementation maps its own on-chain account layouts
//! into these types, so the scanner and health logic stay protocol-agnostic.

use solana_sdk::pubkey::Pubkey;

/// Static configuration of a lending "bank" / "reserve": one supported asset.
#[derive(Debug, Clone)]
pub struct BankConfig {
    /// The bank/reserve account address.
    pub address: Pubkey,
    /// Mint of the asset this bank holds.
    pub mint: Pubkey,
    /// Oracle account that prices this asset.
    pub oracle: Pubkey,
    /// Token decimals, for converting raw amounts to UI units.
    pub decimals: u8,
    /// Maintenance weight applied when this asset is used as collateral.
    /// Typically <= 1.0 (collateral is discounted).
    pub asset_weight_maint: f64,
    /// Maintenance weight applied when this asset is borrowed.
    /// Typically >= 1.0 (debt is inflated).
    pub liab_weight_maint: f64,
}

/// A borrower account with its deposit/borrow amounts already converted
/// to UI token units (i.e. raw amount / 10^decimals).
#[derive(Debug, Clone)]
pub struct RawPosition {
    /// The borrower's position account address.
    pub address: Pubkey,
    /// The wallet that owns the position.
    pub owner: Pubkey,
    /// (bank address, deposited amount in UI token units).
    pub deposits: Vec<(Pubkey, f64)>,
    /// (bank address, borrowed amount in UI token units).
    pub borrows: Vec<(Pubkey, f64)>,
}

/// An asset valued at a point in time, ready for health math.
#[derive(Debug, Clone)]
pub struct PricedAsset {
    pub mint: Pubkey,
    /// Amount in UI token units.
    pub amount: f64,
    /// USD price per unit.
    pub price: f64,
    /// Risk weight applied for the side (collateral or liability) it sits on.
    pub weight: f64,
}