//! Health-factor math.
//!
//! This module is deliberately free of any Solana dependency so the core
//! risk logic can be tested in isolation and reasoned about cleanly.

use crate::types::PricedAsset;

/// The result of assessing one borrower position.
#[derive(Debug, Clone)]
pub struct HealthReport {
    /// Sum of collateral value after applying each asset's weight (USD).
    pub weighted_collateral: f64,
    /// Sum of liability value after applying each asset's weight (USD).
    pub weighted_liability: f64,
    /// weighted_collateral / weighted_liability.
    pub health_factor: f64,
    /// True once the position has crossed the liquidation threshold.
    pub liquidatable: bool,
}

/// Assess a position from its priced collateral and liabilities.
///
/// The health factor is the ratio of weighted collateral value to
/// weighted liability value. Collateral is discounted by a maintenance
/// weight below 1.0, debt is inflated by a weight above 1.0, so a
/// position becomes liquidatable once the ratio falls below 1.0.
pub fn assess(collateral: &[PricedAsset], liability: &[PricedAsset]) -> HealthReport {
    let weighted_collateral: f64 = collateral
        .iter()
        .map(|a| a.amount * a.price * a.weight)
        .sum();
    let weighted_liability: f64 = liability
        .iter()
        .map(|a| a.amount * a.price * a.weight)
        .sum();

    let health_factor = if weighted_liability <= 0.0 {
        f64::INFINITY
    } else {
        weighted_collateral / weighted_liability
    };

    HealthReport {
        weighted_collateral,
        weighted_liability,
        health_factor,
        liquidatable: weighted_liability > 0.0 && health_factor < 1.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::pubkey::Pubkey;

    fn asset(amount: f64, price: f64, weight: f64) -> PricedAsset {
        PricedAsset { mint: Pubkey::new_unique(), amount, price, weight }
    }

    #[test]
    fn healthy_position_is_not_liquidatable() {
        // 10 SOL collateral @ $150 with 0.8 weight  -> 1200 weighted
        // 500 USDC debt @ $1 with 1.0 weight        ->  500 weighted
        let report = assess(&[asset(10.0, 150.0, 0.8)], &[asset(500.0, 1.0, 1.0)]);
        assert!(!report.liquidatable);
        assert!(report.health_factor > 1.0);
    }

    #[test]
    fn underwater_position_is_liquidatable() {
        // SOL price crashes: 10 SOL @ $60 with 0.8 weight -> 480 weighted
        // 500 USDC debt -> 500 weighted, health factor 0.96
        let report = assess(&[asset(10.0, 60.0, 0.8)], &[asset(500.0, 1.0, 1.0)]);
        assert!(report.liquidatable);
        assert!(report.health_factor < 1.0);
    }

    #[test]
    fn position_with_no_debt_is_infinitely_healthy() {
        let report = assess(&[asset(1.0, 1.0, 1.0)], &[]);
        assert!(!report.liquidatable);
        assert_eq!(report.health_factor, f64::INFINITY);
    }

    #[test]
    fn empty_position_is_not_liquidatable() {
        let report = assess(&[], &[]);
        assert!(!report.liquidatable);
    }
}