use crate::{
    config::Config,
    health::{self, HealthReport},
    liquidator::{estimate_profit_usd, Liquidator},
    oracle::OracleClient,
    protocol::LendingProtocol,
    rpc::Rpc,
    types::{BankConfig, PricedAsset},
};
use anyhow::Result;
use solana_sdk::pubkey::Pubkey;
use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};

/// A liquidatable position plus everything the liquidator needs to act.
///
/// Bank-derived fields are filled from BankConfig. `liquidator_marginfi_account`
/// is operator-supplied and stays zeroed until real execution is wired; the
/// liquidator's zero-pubkey check and the LIQUIDATE_LAYOUT_VERIFIED gate keep
/// that safe.
#[derive(Debug)]
pub struct Opportunity {
    pub position: Pubkey,
    pub owner: Pubkey,
    pub report: HealthReport,

    // Fields required for instruction construction.
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
    /// Liquidatee's active (bank, oracle) pairs for the health check.
    pub liquidatee_remaining_accounts: Vec<(Pubkey, Pubkey)>,
}

/// The outcome of one scan pass: stats plus the liquidatable positions
/// found, ranked by estimated profit (most profitable first).
#[derive(Debug, Clone)]
pub struct ScanResult {
    pub banks_total: usize,
    pub banks_priced: usize,
    pub positions_scanned: usize,
    pub scan_duration: Duration,
    pub opportunities: Vec<OppRow>,
}

/// One liquidatable position, flattened for display and ranking.
#[derive(Debug, Clone)]
pub struct OppRow {
    pub position: Pubkey,
    pub owner: Pubkey,
    pub health_factor: f64,
    pub collateral_usd: f64,
    pub debt_usd: f64,
    pub est_profit_usd: f64,
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

    /// Headless loop: scan, sleep, repeat.
    pub async fn run(&mut self) -> Result<()> {
        loop {
            if let Err(e) = self.scan_once().await {
                tracing::error!("scan failed: {e:#}");
            }
            tokio::time::sleep(Duration::from_secs(self.cfg.poll_interval_secs)).await;
        }
    }

    pub async fn scan_once(&mut self) -> Result<ScanResult> {
        let started = Instant::now();

        let banks     = self.protocol.load_banks(&self.rpc).await?;
        let positions = self.protocol.load_positions(&self.rpc).await?;
        tracing::info!(banks = banks.len(), positions = positions.len(), "state loaded");

        let oracle_keys: Vec<Pubkey> = banks.values().map(|b| b.oracle).collect();
        let prices = self.oracle.get_prices(&self.rpc, &oracle_keys).await?;
        tracing::info!(priced = prices.len(), "oracle prices fetched");

        let mut opportunities = Vec::new();
        let mut rows: Vec<OppRow> = Vec::new();

        for pos in &positions {
            let collateral = price_side(&pos.deposits, &banks, &prices, true);
            let liability  = price_side(&pos.borrows,  &banks, &prices, false);
            if collateral.is_empty() || liability.is_empty() { continue; }

            let report = health::assess(&collateral, &liability);
            if !report.liquidatable { continue; }
            // Dust filter: ignore positions too small to be worth acting on.
            if report.weighted_liability < self.cfg.min_debt_usd { continue; }
            // Drop artifacts: real debt but no priceable collateral means
            // the collateral bank's oracle was not parsed, not a true
            // opportunity.
            if report.weighted_collateral < 1.0 {
                tracing::debug!(
                    position = %pos.address,
                    "skipping: collateral unpriced (oracle coverage gap)"
                );
                continue;
            }

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

            rows.push(OppRow {
                position: pos.address,
                owner: pos.owner,
                health_factor: report.health_factor,
                collateral_usd: report.weighted_collateral,
                debt_usd: report.weighted_liability,
                est_profit_usd: estimate_profit_usd(report.weighted_liability),
            });

            opportunities.push(build_opportunity(pos, report, &banks, asset_bank, liab_bank));
        }

        // Rank by estimated profit, most profitable first.
        rows.sort_by(|a, b| b.est_profit_usd.total_cmp(&a.est_profit_usd));

        // Attempt execution if a liquidator is configured.
        if let Some(liquidator) = &self.liquidator {
            for opp in &opportunities {
                if let Err(e) = liquidator.try_liquidate(&self.rpc, opp).await {
                    tracing::error!(position = %opp.position, "liquidation failed: {e:#}");
                }
            }
        }

        let result = ScanResult {
            banks_total: banks.len(),
            banks_priced: prices.len(),
            positions_scanned: positions.len(),
            scan_duration: started.elapsed(),
            opportunities: rows,
        };

        tracing::info!(
            scanned      = result.positions_scanned,
            liquidatable = result.opportunities.len(),
            banks_priced = result.banks_priced,
            banks_total  = result.banks_total,
            elapsed_ms   = result.scan_duration.as_millis(),
            "pass complete"
        );

        Ok(result)
    }
}

/// Build an Opportunity. Bank-derived fields are filled from BankConfig.
/// `liquidator_marginfi_account` stays zeroed (operator-supplied); wire it
/// before arming real execution.
fn build_opportunity(
    pos: &crate::types::RawPosition,
    report: HealthReport,
    banks: &HashMap<Pubkey, BankConfig>,
    asset_bank: Option<Pubkey>,
    liab_bank: Option<Pubkey>,
) -> Opportunity {
    let asset_bank = asset_bank.unwrap_or_default();
    let liab_bank  = liab_bank.unwrap_or_default();

    let ab = banks.get(&asset_bank);
    let lb = banks.get(&liab_bank);

    // Remaining accounts: every active bank on the liquidatee plus its
    // oracle. Deduplicate so a bank used on both sides appears once.
    let mut seen = HashSet::new();
    let mut remaining = Vec::new();
    for (bank_addr, _) in pos.deposits.iter().chain(pos.borrows.iter()) {
        if !seen.insert(*bank_addr) { continue; }
        if let Some(b) = banks.get(bank_addr) {
            remaining.push((*bank_addr, b.oracle));
        }
    }

    Opportunity {
        position: pos.address,
        owner: pos.owner,
        marginfi_group: lb.or(ab).map(|b| b.group).unwrap_or_default(),
        asset_bank,
        asset_bank_oracle:          ab.map(|b| b.oracle).unwrap_or_default(),
        asset_bank_liquidity_vault: ab.map(|b| b.liquidity_vault).unwrap_or_default(),
        liab_bank,
        liab_bank_oracle:           lb.map(|b| b.oracle).unwrap_or_default(),
        liab_bank_liquidity_vault:  lb.map(|b| b.liquidity_vault).unwrap_or_default(),
        insurance_vault:            lb.map(|b| b.insurance_vault).unwrap_or_default(),
        liquidator_marginfi_account: Pubkey::default(),
        liquidate_asset_amount: 0,
        liquidatee_remaining_accounts: remaining,
        report,
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