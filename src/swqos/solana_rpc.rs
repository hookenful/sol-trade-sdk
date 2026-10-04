use std::sync::atomic::{AtomicBool, Ordering};
use std::{sync::Arc, time::Duration, time::Instant};

use solana_client::rpc_config::RpcSendTransactionConfig;
use solana_commitment_config::CommitmentLevel;
use solana_sdk::transaction::VersionedTransaction;
use solana_transaction_status_client_types::UiTransactionEncoding;

use crate::swqos::SwqosClientTrait;
use crate::{
    common::{sdk_log, SolanaRpcClient},
    swqos::{common::poll_transaction_confirmation, SwqosType, TradeType},
};
use anyhow::Result;

#[derive(Clone)]
pub struct SolRpcClient {
    pub rpc_client: Arc<SolanaRpcClient>,
    stop_ping: Arc<AtomicBool>,
    instance_refs: Arc<()>,
}

const DEFAULT_RPC_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);

#[async_trait::async_trait]
impl SwqosClientTrait for SolRpcClient {
    async fn send_transaction(
        &self,
        trade_type: TradeType,
        transaction: &VersionedTransaction,
        wait_confirmation: bool,
    ) -> Result<()> {
        let submit_start = Instant::now();
        let signature = self
            .rpc_client
            .send_transaction_with_config(
                transaction,
                RpcSendTransactionConfig {
                    skip_preflight: true,
                    preflight_commitment: Some(CommitmentLevel::Processed),
                    encoding: Some(UiTransactionEncoding::Base64),
                    max_retries: Some(3),
                    min_context_slot: Some(0),
                },
            )
            .await?;

        sdk_log::log_swqos_submitted("Default", trade_type, submit_start.elapsed());

        let start_time = Instant::now();
        match poll_transaction_confirmation(&self.rpc_client, signature, wait_confirmation).await {
            Ok(_) => (),
            Err(e) => {
                println!(" signature: {:?}", signature);
                println!(" [rpc] {} confirmation failed: {:?}", trade_type, start_time.elapsed());
                return Err(e);
            }
        }
        if wait_confirmation {
            println!(" signature: {:?}", signature);
            println!(" [rpc] {} confirmed: {:?}", trade_type, start_time.elapsed());
        }

        Ok(())
    }

    async fn send_transactions(
        &self,
        trade_type: TradeType,
        transactions: &Vec<VersionedTransaction>,
        wait_confirmation: bool,
    ) -> Result<()> {
        for transaction in transactions {
            self.send_transaction(trade_type, transaction, wait_confirmation).await?;
        }
        Ok(())
    }

    fn get_tip_account(&self) -> Result<String> {
        Ok("".to_string())
    }

    fn get_swqos_type(&self) -> SwqosType {
        SwqosType::Default
    }
}

impl SolRpcClient {
    pub fn new(rpc_client: Arc<SolanaRpcClient>) -> Self {
        let client = Self {
            rpc_client,
            stop_ping: Arc::new(AtomicBool::new(false)),
            instance_refs: Arc::new(()),
        };
        let client_clone = client.clone();
        tokio::spawn(async move {
            client_clone.start_ping_task().await;
        });
        client
    }

    /// Warm the Default RPC client's connection immediately, then keep it hot.
    async fn start_ping_task(&self) {
        let rpc_client = self.rpc_client.clone();
        let stop_ping = self.stop_ping.clone();

        tokio::spawn(async move {
            if let Err(e) = Self::send_ping_request(&rpc_client).await {
                if sdk_log::sdk_log_enabled() {
                    eprintln!("Default RPC ping request failed: {}", e);
                }
            }

            let mut interval = tokio::time::interval(DEFAULT_RPC_KEEPALIVE_INTERVAL);
            loop {
                interval.tick().await;
                if stop_ping.load(Ordering::Relaxed) {
                    break;
                }
                if let Err(e) = Self::send_ping_request(&rpc_client).await {
                    if sdk_log::sdk_log_enabled() {
                        eprintln!("Default RPC ping request failed: {}", e);
                    }
                }
            }
        });
    }

    async fn send_ping_request(rpc_client: &SolanaRpcClient) -> Result<()> {
        rpc_client.get_health().await?;
        Ok(())
    }
}

impl Drop for SolRpcClient {
    fn drop(&mut self) {
        if Arc::strong_count(&self.instance_refs) != 1 {
            return;
        }
        self.stop_ping.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_client() -> SolRpcClient {
        SolRpcClient {
            rpc_client: Arc::new(SolanaRpcClient::new("http://127.0.0.1:8899".to_string())),
            stop_ping: Arc::new(AtomicBool::new(false)),
            instance_refs: Arc::new(()),
        }
    }

    #[test]
    fn dropping_clone_does_not_stop_default_rpc_ping_task() {
        let client = test_client();
        let stop_ping = client.stop_ping.clone();
        let clone = client.clone();

        drop(clone);
        assert!(!stop_ping.load(Ordering::Relaxed));

        drop(client);
        assert!(stop_ping.load(Ordering::Relaxed));
    }
}
