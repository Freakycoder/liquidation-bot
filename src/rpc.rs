//! Thin async wrapper around the Solana RPC client.

use anyhow::{Context, Result};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::account::Account;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::pubkey::Pubkey;
use std::time::Duration;

pub struct Rpc {
    pub client: RpcClient,
}

impl Rpc {
    pub fn new(url: impl Into<String>) -> Self {
        let client = RpcClient::new_with_timeout_and_commitment(
            url.into(),
            Duration::from_secs(90),
            CommitmentConfig::confirmed(),
        );
        Self { client }
    }

    pub fn inner(&self) -> &RpcClient { &self.client }

    pub async fn get_accounts(
        &self,
        keys: &[Pubkey],
    ) -> Result<Vec<(Pubkey, Option<Account>)>> {
        let mut out = Vec::with_capacity(keys.len());
        for chunk in keys.chunks(100) {
            let accounts = self.client.get_multiple_accounts(chunk)
                .await.context("getMultipleAccounts failed")?;
            for (key, account) in chunk.iter().zip(accounts) {
                out.push((*key, account));
            }
        }
        Ok(out)
    }

    pub async fn current_slot(&self) -> Result<u64> {
        let fut = self.client.get_slot();
        match tokio::time::timeout(Duration::from_secs(3), fut).await {
            Ok(Ok(slot)) => Ok(slot),
            Ok(Err(e))   => Err(e.into()),
            Err(_)       => anyhow::bail!("get_slot timeout"),
        }
    }

    /// Median recent prioritization fee, micro-lamports per CU. Short
    /// timeout, returns 0 on any failure rather than propagating.
    pub async fn median_priority_fee(&self) -> Result<u64> {
        let fut = self.client.get_recent_prioritization_fees(&[]);
        let fees = match tokio::time::timeout(Duration::from_secs(3), fut).await {
            Ok(Ok(f)) => f,
            _ => return Ok(0),
        };
        if fees.is_empty() { return Ok(0); }
        let mut vals: Vec<u64> = fees.iter().map(|f| f.prioritization_fee).collect();
        vals.sort_unstable();
        Ok(vals[vals.len() / 2])
    }
}