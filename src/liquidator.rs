//! Liquidation execution: keypair loading, instruction construction,
//! profitability filtering, and transaction submission.
//!
//! SAFETY: this module sends real transactions with real funds when
//! dry_run = false. It refuses to act unless estimated net profit clears
//! the configured threshold, unless the account layout has been verified,
//! and unless every required account is set. Keep dry_run = true until you
//! have verified the instruction layout against the MarginFi IDL on devnet.

use crate::rpc::Rpc;
use crate::scanner::Opportunity;
use anyhow::{Context, Result};
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    transaction::Transaction,
};
use std::str::FromStr;

/// Set to `true` ONLY after verifying the lendingAccountLiquidate account
/// order against the MarginFi IDL and completing a successful devnet
/// dry-run. While false, build_liquidate_ix refuses to construct the
/// instruction. This is the single switch that arms real execution.
const LIQUIDATE_LAYOUT_VERIFIED: bool = false;
const MARGINFI_PROGRAM_ID: &str = "MFv2hWf31Z9kbCa1snEPYctwafyhdvnV7FZnsebVacA";

/// Holds the signer and submits liquidation transactions.
pub struct Liquidator {
    keypair: Keypair,
    program_id: Pubkey,
    dry_run: bool,
    min_profit_usd: f64,
}

impl Liquidator {
    /// Load the liquidator keypair from a standard Solana CLI keypair file
    /// (a JSON array of 64 bytes).
    pub fn from_keypair_file(path: &str, dry_run: bool, min_profit_usd: f64) -> Result<Self> {
        let bytes = std::fs::read_to_string(path)
            .with_context(|| format!("reading keypair file {path}"))?;
        let nums: Vec<u8> = serde_json::from_str(&bytes)
            .context("parsing keypair file as JSON byte array")?;
        let keypair = Keypair::from_bytes(&nums)
            .context("constructing keypair from bytes")?;

        Ok(Self {
            keypair,
            program_id: Pubkey::from_str(MARGINFI_PROGRAM_ID).unwrap(),
            dry_run,
            min_profit_usd,
        })
    }

    /// The liquidator's public key.
    pub fn pubkey(&self) -> Pubkey {
        self.keypair.pubkey()
    }

    /// Attempt to liquidate one opportunity. Returns Ok(Some(sig)) if a
    /// transaction was sent, Ok(None) if it was skipped (unprofitable or
    /// dry run), Err on failure.
    pub async fn try_liquidate(
        &self,
        rpc: &Rpc,
        opp: &Opportunity,
    ) -> Result<Option<String>> {
        // Profitability gate.
        let est_profit = estimate_profit_usd(opp.report.weighted_liability);
        if est_profit < self.min_profit_usd {
            tracing::info!(
                position = %opp.position,
                est_profit_usd = format!("{est_profit:.2}"),
                threshold = self.min_profit_usd,
                "skipping: below profit threshold"
            );
            return Ok(None);
        }

        if self.dry_run {
            tracing::warn!(
                position = %opp.position,
                est_profit_usd = format!("{est_profit:.2}"),
                "DRY RUN: would liquidate (set dry_run = false to execute)"
            );
            return Ok(None);
        }

        // Build and send.
        let ix = self.build_liquidate_ix(opp)?;
        let blockhash = rpc.client
            .get_latest_blockhash()
            .await
            .context("fetching blockhash")?;

        let tx = Transaction::new_signed_with_payer(
            &[ix],
            Some(&self.keypair.pubkey()),
            &[&self.keypair],
            blockhash,
        );

        let sig = rpc.client
            .send_and_confirm_transaction(&tx)
            .await
            .context("submitting liquidation transaction")?;

        tracing::warn!(
            position = %opp.position,
            signature = %sig,
            est_profit_usd = format!("{est_profit:.2}"),
            "LIQUIDATION SENT"
        );

        Ok(Some(sig.to_string()))
    }

    /// Construct the MarginFi `lendingAccountLiquidate` instruction.
    ///
    /// The account order and signer/writable flags below are the documented
    /// MarginFi v2 layout but have NOT been verified against the on-chain
    /// IDL in this build. This function refuses to build the instruction
    /// until LIQUIDATE_LAYOUT_VERIFIED is set true, and refuses any
    /// opportunity with unset (zero) accounts.
    ///
    /// Before flipping the flag, fetch the IDL:
    ///   anchor idl fetch MFv2hWf31Z9kbCa1snEPYctwafyhdvnV7FZnsebVacA
    /// and confirm the discriminator, the account order, each is_signer /
    /// is_writable flag, the asset_amount arg, and the trailing remaining
    /// accounts (the liquidatee's active banks plus oracles for the health
    /// check, without which the transaction fails).
    fn build_liquidate_ix(&self, opp: &Opportunity) -> Result<Instruction> {
        // Hard safety gate.
        if !LIQUIDATE_LAYOUT_VERIFIED {
            anyhow::bail!(
                "lendingAccountLiquidate account layout not yet verified \
                 against the IDL; refusing to build instruction. See \
                 LIQUIDATE_LAYOUT_VERIFIED in liquidator.rs"
            );
        }

        // Reject an opportunity with unset accounts rather than sending
        // a transaction full of system-program (zero) pubkeys.
        let zero = Pubkey::default();
        for (label, key) in [
            ("marginfi_group", opp.marginfi_group),
            ("asset_bank", opp.asset_bank),
            ("asset_bank_oracle", opp.asset_bank_oracle),
            ("asset_bank_liquidity_vault", opp.asset_bank_liquidity_vault),
            ("liab_bank", opp.liab_bank),
            ("liab_bank_oracle", opp.liab_bank_oracle),
            ("liab_bank_liquidity_vault", opp.liab_bank_liquidity_vault),
            ("insurance_vault", opp.insurance_vault),
            ("liquidator_marginfi_account", opp.liquidator_marginfi_account),
        ] {
            if key == zero {
                anyhow::bail!("opportunity field `{label}` is unset (zero pubkey)");
            }
        }
        if opp.liquidate_asset_amount == 0 {
            anyhow::bail!("liquidate_asset_amount is 0; refusing to send a no-op");
        }

        let disc = anchor_ix_disc("lending_account_liquidate");
        let asset_amount: u64 = opp.liquidate_asset_amount;

        let mut data = Vec::with_capacity(16);
        data.extend_from_slice(&disc);
        data.extend_from_slice(&asset_amount.to_le_bytes());

        let mut accounts = vec![
            AccountMeta::new_readonly(opp.marginfi_group, false),
            AccountMeta::new(opp.asset_bank, false),
            AccountMeta::new_readonly(opp.asset_bank_oracle, false),
            AccountMeta::new(opp.liab_bank, false),
            AccountMeta::new_readonly(opp.liab_bank_oracle, false),
            AccountMeta::new(opp.liquidator_marginfi_account, false),
            AccountMeta::new(self.keypair.pubkey(), true),
            AccountMeta::new(opp.position, false),
            AccountMeta::new(opp.liab_bank_liquidity_vault, false),
            AccountMeta::new(opp.asset_bank_liquidity_vault, false),
            AccountMeta::new(opp.insurance_vault, false),
            AccountMeta::new_readonly(spl_token_program_id(), false),
        ];

        // Remaining accounts: the liquidatee's active banks each followed
        // by that bank's oracle, for the program's health check.
        for (bank, oracle) in &opp.liquidatee_remaining_accounts {
            accounts.push(AccountMeta::new_readonly(*bank, false));
            accounts.push(AccountMeta::new_readonly(*oracle, false));
        }

        Ok(Instruction { program_id: self.program_id, accounts, data })
    }
}

/// Estimate net USD profit from liquidating a position carrying
/// `weighted_liability` USD of debt. Shared by the scanner (for ranking)
/// and the executor (for the profitability gate).
pub fn estimate_profit_usd(weighted_liability: f64) -> f64 {
    // The liquidator's share of MarginFi's liquidation fee, roughly 2.5%.
    const LIQUIDATOR_BONUS: f64 = 0.025;
    // Flat allowance for tx fee, priority fee, and swap slippage, in USD.
    const COST_ALLOWANCE_USD: f64 = 0.50;
    weighted_liability * LIQUIDATOR_BONUS - COST_ALLOWANCE_USD
}

/// Anchor instruction discriminator: SHA256("global:<name>")[0..8].
fn anchor_ix_disc(name: &str) -> [u8; 8] {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(format!("global:{name}"));
    h.finalize()[..8].try_into().unwrap()
}

fn spl_token_program_id() -> Pubkey {
    Pubkey::from_str("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA").unwrap()
}