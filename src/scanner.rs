use crate::{
    config::Config,
    health::{self, HealthReport},
    liquidator::{estimate_profit_usd, Liquidator},
    oracle::OracleClient,
    protocol::{ImplStatus, LendingProtocol},
    rpc::Rpc,
    types::{
        BankConfig, ChainState, DivergenceRow, PricedAsset, RiskBuckets,
        WhaleRow,
    },
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
    pub status: ImplStatus,
    pub banks_total: usize,
    pub banks_priced: usize,
    pub positions_scanned: usize,
    pub scan_duration: Duration,
    pub opportunities: Vec<OppRow>,
    pub risk_buckets: RiskBuckets,
    pub whale_watch: Vec<WhaleRow>,
    pub oracle_divergence: Vec<DivergenceRow>,
    pub chain: ChainState,
    pub total_profit_usd: f64,
}

#[derive(Debug, Clone)]
pub struct OppRow {
    pub position: Pubkey,
    pub owner: Pubkey,
    pub health_factor: f64,
    pub collateral_usd: f64,
    pub debt_usd: f64,
    pub debt_repaid_usd: f64,
    pub bonus_usd: f64,
    pub cost_usd: f64,
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

    pub fn name(&self) -> &'static str { self.protocol.name() }
    pub fn impl_status(&self) -> ImplStatus { self.protocol.status() }

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
        let proto_status = self.protocol.status();

        let chain = ChainState::default();

        if proto_status == ImplStatus::Pending {
            return Ok(ScanResult {
                protocol_name: proto_name,
                status: proto_status,
                banks_total: 0,
                banks_priced: 0,
                positions_scanned: 0,
                scan_duration: started.elapsed(),
                opportunities: Vec::new(),
                risk_buckets: RiskBuckets::default(),
                whale_watch: Vec::new(),
                oracle_divergence: Vec::new(),
                chain,
                total_profit_usd: 0.0,
            });
        }

        let banks = self.protocol.load_banks(&self.rpc).await?;
        let positions = self.protocol.load_positions(&self.rpc, &banks).await?;
        tracing::info!(
            protocol = %proto_name,
            banks = banks.len(),
            positions = positions.len(),
            "state loaded"
        );

        let oracle_keys: Vec<Pubkey> = banks.values().map(|b| b.oracle).collect();
        let prices: HashMap<Pubkey, f64> = if self.protocol.name() == "kamino" {
            banks.values().map(|b| (b.oracle, 1.0)).collect()
        } else {
            self.oracle.get_prices(&self.rpc, &oracle_keys).await?
        };
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

        // Oracle divergence: group banks by mint, find mints whose oracles
        // disagree. For Kamino (self-priced) we use the bank's stored
        // share_value directly. For MarginFi we use oracle prices.
        let oracle_divergence = compute_oracle_divergence(
            &banks, &prices, self.protocol.name() == "kamino",
        );

        let mut opportunities = Vec::new();
        let mut rows: Vec<OppRow> = Vec::new();
        let mut whales: Vec<WhaleRow> = Vec::new();
        let mut risk = RiskBuckets::default();
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

            // Plausibility gate, shared by risk buckets, whale watch, and
            // opportunity scoring. Positions that fail this are noise.
            const MAX_PLAUSIBLE_USD: f64 = 1_000_000.0;
            let ratio = report.weighted_liability
                / report.weighted_collateral.max(1.0);
            let plausible =
                report.weighted_collateral >= 1.0
                && report.weighted_liability >= 1.0
                && report.weighted_liability <= MAX_PLAUSIBLE_USD
                && report.weighted_collateral <= MAX_PLAUSIBLE_USD
                && ratio <= 100.0;

            if !plausible {
                continue;
            }

            risk.total_priced += 1;
            let hf = report.health_factor;
            if hf < 1.10 { risk.watch += 1; }
            if hf < 1.05 { risk.at_risk += 1; }
            if hf < 1.02 { risk.edge += 1; }
            if hf < 1.00 { risk.liquidatable += 1; }

            // Whale watch: only positions large enough to be interesting.
            // Threshold of $10k keeps the vector small (a few hundred max)
            // so sort+truncate is cheap.
            if report.weighted_liability >= 10_000.0 {
                whales.push(WhaleRow {
                    position: pos.address,
                    owner: pos.owner,
                    debt_usd: report.weighted_liability,
                    health_factor: hf,
                    liquidatable: report.liquidatable,
                });
            }

            if !report.liquidatable { continue; }
            if report.weighted_liability < self.cfg.min_debt_usd { continue; }

            let asset_bank = largest_bank(&pos.deposits, &banks, &prices, true);
            let liab_bank  = largest_bank(&pos.borrows,  &banks, &prices, false);

            let (bonus, cost) = profit_components(report.weighted_liability);
            let est_profit = bonus - cost;

            rows.push(OppRow {
                position: pos.address,
                owner: pos.owner,
                health_factor: hf,
                collateral_usd: report.weighted_collateral,
                debt_usd: report.weighted_liability,
                debt_repaid_usd: report.weighted_liability,
                bonus_usd: bonus,
                cost_usd: cost,
                est_profit_usd: est_profit,
            });

            opportunities.push(build_opportunity(pos, report, &banks, asset_bank, liab_bank));
        }

        rows.sort_by(|a, b| b.est_profit_usd.total_cmp(&a.est_profit_usd));

        // Top whales by debt, keep 6.
        whales.sort_by(|a, b| b.debt_usd.total_cmp(&a.debt_usd));
        whales.truncate(6);

        let total_profit_usd: f64 = rows.iter().map(|r| r.est_profit_usd).sum();

        if let Some(liquidator) = &self.liquidator {
            for opp in &opportunities {
                if let Err(e) = liquidator.try_liquidate(&self.rpc, opp).await {
                    tracing::error!(position = %opp.position, "liquidation failed: {e:#}");
                }
            }
        }

        let result = ScanResult {
            protocol_name: proto_name.clone(),
            status: proto_status,
            banks_total: banks.len(),
            banks_priced,
            positions_scanned: positions.len(),
            scan_duration: started.elapsed(),
            opportunities: rows,
            risk_buckets: risk,
            whale_watch: whales,
            oracle_divergence,
            chain,
            total_profit_usd,
        };

        tracing::info!(
            protocol         = %proto_name,
            scanned          = result.positions_scanned,
            liquidatable     = result.opportunities.len(),
            risk_edge        = result.risk_buckets.edge,
            risk_watch       = result.risk_buckets.watch,
            divergent_mints  = result.oracle_divergence.len(),
            skipped_incomplete,
            elapsed_ms       = result.scan_duration.as_millis(),
            slot             = result.chain.slot,
            "pass complete"
        );

        Ok(result)
    }
}

fn profit_components(debt_usd: f64) -> (f64, f64) {
    const LIQUIDATOR_BONUS: f64 = 0.025;
    const COST_ALLOWANCE_USD: f64 = 0.50;
    (debt_usd * LIQUIDATOR_BONUS, COST_ALLOWANCE_USD)
}

fn compute_oracle_divergence(
    banks: &HashMap<Pubkey, BankConfig>,
    prices: &HashMap<Pubkey, f64>,
    kamino_mode: bool,
) -> Vec<DivergenceRow> {
    // Map mint -> distinct price values observed.
    let mut by_mint: HashMap<Pubkey, Vec<f64>> = HashMap::new();
    for bank in banks.values() {
        let price = if kamino_mode {
            // Kamino: per-reserve self-price stored in share_value.
            if bank.asset_share_value > 0.0 { bank.asset_share_value } else { continue }
        } else {
            match prices.get(&bank.oracle) {
                Some(p) if *p > 0.0 => *p,
                _ => continue,
            }
        };
        by_mint.entry(bank.mint).or_default().push(price);
    }

    let mut out = Vec::new();
    for (mint, mut ps) in by_mint {
        // Dedup near-identical readings so multiple banks pointing at the
        // same oracle don't inflate `sources`.
        ps.sort_by(|a, b| a.total_cmp(b));
        ps.dedup_by(|a, b| (*a - *b).abs() / b.abs().max(1e-9) < 1e-6);
        if ps.len() < 2 { continue; }
        let min = ps.iter().copied().fold(f64::INFINITY, f64::min);
        let max = ps.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let spread_pct = (max - min) / ((min + max) / 2.0) * 100.0;
        out.push(DivergenceRow {
            mint, sources: ps.len(), min_price: min, max_price: max, spread_pct,
        });
    }
    out.sort_by(|a, b| b.spread_pct.total_cmp(&a.spread_pct));
    out.truncate(8);
    out
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
    entries: &[(Pubkey, f64)],
    banks:   &HashMap<Pubkey, BankConfig>,
    prices:  &HashMap<Pubkey, f64>,
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
    entries: &[(Pubkey, f64)],
    banks:   &HashMap<Pubkey, BankConfig>,
    prices:  &HashMap<Pubkey, f64>,
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