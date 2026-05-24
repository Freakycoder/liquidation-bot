use crate::{
    config::Config,
    health::{self, HealthReport},
    liquidator::Liquidator,
    oracle::OracleClient,
    protocol::LendingProtocol,
    rpc::Rpc,
    types::{BankConfig, PricedAsset},
};
use anyhow::Result;
use solana_sdk::pubkey::Pubkey;
use std::{collections::HashMap, time::Duration};

/// A liquidatable position plus everything the liquidator needs to act.
///
/// The scanner can fully populate `report`, `position`, and `owner` from
/// the data it already loads. The remaining fields (banks, vaults, oracles,
/// the liquidator's own account) require additional data wiring described
/// in `from_health`. Until that wiring is complete, execution stays gated
/// behind dry_run and the unset fields are zeroed.
#[derive(Debug)]
pub struct Opportunity {
    pub position: Pubkey,
    pub owner: Pubkey,
    pub report: HealthReport,

    // ── fields required for instruction construction ──
    pub marginfi_group: Pubkey,
    pub asset_bank: Pubkey,
    pub asset_bank_oracle: Pubkey,
    pub asset_bank_liquidity_vault: Pubkey,
    pub liab_bank: Pubkey,
    pub liab_bank_oracle: Pubkey,
    pub liab_bank_liquidity_vault: Pubkey,
    pub insurance_vault: Pubkey,
    pub liquidator_marginfi_account: Pubkey,
    /// Amount of collateral to seize, native units. 0 = compute max.
    pub liquidate_asset_amount: u64,
}

pub struct Scanner {
    cfg:        Config,
    rpc:        Rpc,
    protocol:   Box<dyn LendingProtocol>,
    oracle:     OracleClient,
    liquidator: Option<Liquidator>,
}

impl Scanner {
    pub fn new(
        cfg: Config,
        protocol: Box<dyn LendingProtocol>,
        liquidator: Option<Liquidator>,
    ) -> Self {
        let rpc = Rpc::new(cfg.rpc_url.clone());
        Self { cfg, rpc, protocol, oracle: OracleClient::new(), liquidator }
    }

    pub async fn run(&mut self) -> Result<()> {
        loop {
            if let Err(e) = self.scan_once().await {
                tracing::error!("scan failed: {e:#}");
            }
            tokio::time::sleep(Duration::from_secs(self.cfg.poll_interval_secs)).await;
        }
    }

    pub async fn scan_once(&mut self) -> Result<()> {
        let banks     = self.protocol.load_banks(&self.rpc).await?;
        let positions = self.protocol.load_positions(&self.rpc).await?;
        tracing::info!(banks = banks.len(), positions = positions.len(), "state loaded");

        let oracle_keys: Vec<Pubkey> = banks.values().map(|b| b.oracle).collect();
        let prices = self.oracle.get_prices(&self.rpc, &oracle_keys).await?;
        tracing::info!(priced = prices.len(), "oracle prices fetched");

        let mut opportunities = Vec::new();
        for pos in &positions {
            let collateral = price_side(&pos.deposits, &banks, &prices, true);
            let liability  = price_side(&pos.borrows,  &banks, &prices, false);
            if collateral.is_empty() || liability.is_empty() { continue; }

            let report = health::assess(&collateral, &liability);
            if !report.liquidatable { continue; }

            // Pick the largest-value bank on each side as the liquidation pair.
            let asset_bank = largest_bank(&pos.deposits, &banks, &prices, true);
            let liab_bank  = largest_bank(&pos.borrows,  &banks, &prices, false);

            tracing::warn!(
                position       = %pos.address,
                owner          = %pos.owner,
                health_factor  = format!("{:.4}", report.health_factor),
                collateral_usd = format!("${:.2}", report.weighted_collateral),
                debt_usd       = format!("${:.2}", report.weighted_liability),
                "LIQUIDATABLE"
            );

            opportunities.push(build_opportunity(pos, report, &banks, asset_bank, liab_bank));
        }

        // Attempt execution if a liquidator is configured.
        if let Some(liquidator) = &self.liquidator {
            for opp in &opportunities {
                if let Err(e) = liquidator.try_liquidate(&self.rpc, opp).await {
                    tracing::error!(position = %opp.position, "liquidation failed: {e:#}");
                }
            }
        }

        tracing::info!(
            scanned      = positions.len(),
            liquidatable = opportunities.len(),
            "pass complete"
        );
        Ok(())
    }
}

/// Build an Opportunity. Bank-derived fields are filled from BankConfig;
/// fields not yet tracked (liquidity vaults, insurance vault, the
/// liquidator's own marginfi account) are zeroed and must be wired before
/// real execution. The liquidator's profitability gate and dry_run flag
/// keep this safe in the meantime.
fn build_opportunity(
    pos: &crate::types::RawPosition,
    report: HealthReport,
    banks: &HashMap<Pubkey, BankConfig>,
    asset_bank: Option<Pubkey>,
    liab_bank: Option<Pubkey>,
) -> Opportunity {
    let asset_bank = asset_bank.unwrap_or_default();
    let liab_bank  = liab_bank.unwrap_or_default();

    let asset_oracle = banks.get(&asset_bank).map(|b| b.oracle).unwrap_or_default();
    let liab_oracle  = banks.get(&liab_bank).map(|b| b.oracle).unwrap_or_default();

    Opportunity {
        position: pos.address,
        owner: pos.owner,
        report,
        // marginfi_group lives in the Bank/MarginfiAccount header; parse
        // it at offset 8 and thread it through if you wire real execution.
        marginfi_group: Pubkey::default(),
        asset_bank,
        asset_bank_oracle: asset_oracle,
        asset_bank_liquidity_vault: Pubkey::default(),
        liab_bank,
        liab_bank_oracle: liab_oracle,
        liab_bank_liquidity_vault: Pubkey::default(),
        insurance_vault: Pubkey::default(),
        liquidator_marginfi_account: Pubkey::default(),
        liquidate_asset_amount: 0,
    }
}

fn price_side(
    entries:       &[(Pubkey, f64)],
    banks:         &HashMap<Pubkey, BankConfig>,
    prices:        &HashMap<Pubkey, f64>,
    is_collateral: bool,
) -> Vec<PricedAsset> {
    entries.iter().filter_map(|(bank_addr, shares)| {
        let bank  = banks.get(bank_addr)?;
        let price = prices.get(&bank.oracle).copied()?;
        let (share_value, weight) = if is_collateral {
            (bank.asset_share_value, bank.asset_weight_maint)
        } else {
            (bank.liab_share_value, bank.liab_weight_maint)
        };
        let amount = shares * share_value / 10f64.powi(bank.decimals as i32);
        Some(PricedAsset { mint: bank.mint, amount, price, weight })
    }).collect()
}

/// The bank holding the highest USD value on one side of a position.
fn largest_bank(
    entries:       &[(Pubkey, f64)],
    banks:         &HashMap<Pubkey, BankConfig>,
    prices:        &HashMap<Pubkey, f64>,
    is_collateral: bool,
) -> Option<Pubkey> {
    entries.iter().filter_map(|(bank_addr, shares)| {
        let bank  = banks.get(bank_addr)?;
        let price = prices.get(&bank.oracle).copied()?;
        let sv = if is_collateral { bank.asset_share_value } else { bank.liab_share_value };
        let value = shares * sv / 10f64.powi(bank.decimals as i32) * price;
        Some((*bank_addr, value))
    })
    .max_by(|a, b| a.1.total_cmp(&b.1))
    .map(|(addr, _)| addr)
}