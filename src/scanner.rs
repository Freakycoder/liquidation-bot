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
    sync::Arc,
    time::{Duration, Instant},
};

#[derive(Debug)]
pub struct Opportunity {
    pub position: Pubkey,
    pub owner: Pubkey,
    pub report: HealthReport,
    pub marginfi_group: Pubkey,
    pub asset_bank: Pubkey,
    pub asset_bank_oracle: Pubkey,
    pub asset_bank_liquidity_vault: Pubkey,
    pub liab_bank: Pubkey,
    pub liab_bank_oracle: Pubkey,
    pub liab_bank_liquidity_vault: Pubkey,
    pub insurance_vault: Pubkey,
    pub liquidator_marginfi_account: Pubkey,
    pub liquidate_asset_amount: u64,
    pub liquidatee_remaining_accounts: Vec<(Pubkey, Pubkey)>,
}

#[derive(Debug, Clone)]
pub struct ScanResult {
    pub protocol_name: String,
    pub banks_total: usize,
    pub banks_priced: usize,
    pub positions_scanned: usize,
    pub scan_duration: Duration,
    pub opportunities: Vec<OppRow>,
}

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
    rpc:        Arc<Rpc>,
    protocol:   Box<dyn LendingProtocol>,
    oracle:     OracleClient,
    liquidator: Option<Liquidator>,
}

impl Scanner {
    pub fn new(
        cfg: Config,
        rpc: Arc<Rpc>,
        protocol: Box<dyn LendingProtocol>,
        liquidator: Option<Liquidator>,
    ) -> Self {
        Self { cfg, rpc, protocol, oracle: OracleClient::new(), liquidator }
    }

    pub async fn run(&mut self) -> Result<()> {
        loop {
            if let Err(e) = self.scan_once().await {
                tracing::error!(protocol = self.protocol.name(), "scan failed: {e:#}");
            }
            tokio::time::sleep(Duration::from_secs(self.cfg.poll_interval_secs)).await;
        }
    }

    pub async fn scan_once(&mut self) -> Result<ScanResult> {
        let started = Instant::now();
        let proto_name = self.protocol.name().to_string();

        let banks     = self.protocol.load_banks(&self.rpc).await?;
        let positions = self.protocol.load_positions(&self.rpc).await?;
        tracing::info!(
            protocol = %proto_name,
            banks = banks.len(),
            positions = positions.len(),
            "state loaded"
        );

        let oracle_keys: Vec<Pubkey> = banks.values().map(|b| b.oracle).collect();
        let prices = self.oracle.get_prices(&self.rpc, &oracle_keys).await?;

        let banks_priced = banks
            .values()
            .filter(|b| prices.contains_key(&b.oracle))
            .count();
        tracing::info!(
            protocol = %proto_name,
            distinct_oracles = prices.len(),
            banks_priced,
            banks_total = banks.len(),
            "oracle prices fetched"
        );

        let mut opportunities = Vec::new();
        let mut rows: Vec<OppRow> = Vec::new();
        let mut skipped_incomplete = 0usize;

        for pos in &positions {
            let (collateral, liability) = match (
                price_side(&pos.deposits, &banks, &prices, true),
                price_side(&pos.borrows,  &banks, &prices, false),
            ) {
                (Some(c), Some(l)) => (c, l),
                _ => { skipped_incomplete += 1; continue; }
            };
            if collateral.is_empty() || liability.is_empty() { continue; }

            let report = health::assess(&collateral, &liability);
            if !report.liquidatable { continue; }
            if report.weighted_liability < self.cfg.min_debt_usd { continue; }

            const MAX_PLAUSIBLE_USD: f64 = 5_000_000.0;
            if report.weighted_liability > MAX_PLAUSIBLE_USD
                || report.weighted_collateral > MAX_PLAUSIBLE_USD
            {
                tracing::warn!(
                    protocol = %proto_name,
                    position = %pos.address,
                    debt_usd = report.weighted_liability,
                    collateral_usd = report.weighted_collateral,
                    "skipping: implausible valuation"
                );
                continue;
            }
            if report.weighted_collateral < 1.0 { continue; }

            let asset_bank = largest_bank(&pos.deposits, &banks, &prices, true);
            let liab_bank  = largest_bank(&pos.borrows,  &banks, &prices, false);

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

        rows.sort_by(|a, b| b.est_profit_usd.total_cmp(&a.est_profit_usd));

        if let Some(liquidator) = &self.liquidator {
            for opp in &opportunities {
                if let Err(e) = liquidator.try_liquidate(&self.rpc, opp).await {
                    tracing::error!(position = %opp.position, "liquidation failed: {e:#}");
                }
            }
        }

        let result = ScanResult {
            protocol_name: proto_name.clone(),
            banks_total: banks.len(),
            banks_priced,
            positions_scanned: positions.len(),
            scan_duration: started.elapsed(),
            opportunities: rows,
        };

        tracing::info!(
            protocol         = %proto_name,
            scanned          = result.positions_scanned,
            liquidatable     = result.opportunities.len(),
            skipped_incomplete,
            banks_priced     = result.banks_priced,
            banks_total      = result.banks_total,
            elapsed_ms       = result.scan_duration.as_millis(),
            "pass complete"
        );

        Ok(result)
    }
}

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

    let mut seen = HashSet::new();
    let mut remaining = Vec::new();
    for (bank_addr, _) in pos.deposits.iter().chain(pos.borrows.iter()) {
        if !seen.insert(*bank_addr) { continue; }
        if let Some(b) = banks.get(bank_addr) {
            remaining.push((*bank_addr, b.oracle));
        }
    }

    Opportunity {
        position: pos.address, owner: pos.owner,
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
) -> Option<Vec<PricedAsset>> {
    let mut out = Vec::with_capacity(entries.len());
    for (bank_addr, shares) in entries {
        let bank = banks.get(bank_addr)?;
        let price = prices.get(&bank.oracle).copied()?;
        let (share_value, weight) = if is_collateral {
            (bank.asset_share_value, bank.asset_weight_maint)
        } else {
            (bank.liab_share_value, bank.liab_weight_maint)
        };
        let amount = shares * share_value / 10f64.powi(bank.decimals as i32);
        out.push(PricedAsset { mint: bank.mint, amount, price, weight });
    }
    Some(out)
}

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