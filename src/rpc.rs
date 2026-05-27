//! Thin async wrapper around the Solana RPC client.

use anyhow::{Context, Result};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::account::Account;
use solana_sdk::pubkey::Pubkey;
use std::time::Duration;
use solana_sdk::commitment_config::CommitmentConfig;

/// Wraps the nonblocking RPC client with helpers the bot needs.
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

    /// Access the underlying client for protocol-specific calls
    /// (e.g. get_program_accounts_with_config).
    pub fn inner(&self) -> &RpcClient {
        &self.client
    }

    /// Fetch many accounts at once, chunking to the 100-account limit
    /// of the getMultipleAccounts RPC method. Missing accounts come
    /// back as `None` in the same order as the input keys.
    pub async fn get_accounts(
        &self,
        keys: &[Pubkey],
    ) -> Result<Vec<(Pubkey, Option<Account>)>> {
        let mut out = Vec::with_capacity(keys.len());
        for chunk in keys.chunks(100) {
            let accounts = self
                .client
                .get_multiple_accounts(chunk)
                .await
                .context("getMultipleAccounts failed")?;
            for (key, account) in chunk.iter().zip(accounts) {
                out.push((*key, account));
            }
        }
        Ok(out)
    }
}