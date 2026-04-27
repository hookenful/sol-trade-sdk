use crate::swqos::common::{
    default_http_client_builder, poll_transaction_confirmation, serialize_transaction_and_encode,
};
use rand::seq::IndexedRandom;
use reqwest::Client;
use serde_json::json;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use std::{sync::Arc, time::Instant};
use tokio::task::JoinHandle;

use solana_transaction_status::UiTransactionEncoding;

use crate::swqos::SwqosClientTrait;
use crate::swqos::{SwqosType, TradeType};
use anyhow::Result;
use solana_sdk::transaction::VersionedTransaction;

use crate::{common::SolanaRpcClient, constants::swqos::FLASHBLOCK_TIP_ACCOUNTS};

#[derive(Clone)]
pub struct FlashBlockClient {
    pub endpoint: String,
    pub auth_token: String,
    pub rpc_client: Arc<SolanaRpcClient>,
    pub http_client: Client,
    pub ping_handle: Arc<tokio::sync::Mutex<Option<JoinHandle<()>>>>,
    pub stop_ping: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl SwqosClientTrait for FlashBlockClient {
    async fn send_transaction(
        &self,
        trade_type: TradeType,
        transaction: &VersionedTransaction,
        wait_confirmation: bool,
    ) -> Result<()> {
        self.send_transaction(trade_type, transaction, wait_confirmation).await
    }

    async fn send_transactions(
        &self,
        trade_type: TradeType,
        transactions: &Vec<VersionedTransaction>,
        wait_confirmation: bool,
    ) -> Result<()> {
        self.send_transactions(trade_type, transactions, wait_confirmation).await
    }

    fn get_tip_account(&self) -> Result<String> {
        let tip_account = *FLASHBLOCK_TIP_ACCOUNTS
            .choose(&mut rand::rng())
            .or_else(|| FLASHBLOCK_TIP_ACCOUNTS.first())
            .unwrap();
        Ok(tip_account.to_string())
    }

    fn get_swqos_type(&self) -> SwqosType {
        SwqosType::FlashBlock
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_ping_url_uses_base_endpoint() {
        assert_eq!(
            FlashBlockClient::build_ping_url("https://api.flashblock.trade"),
            "https://api.flashblock.trade/"
        );
        assert_eq!(
            FlashBlockClient::build_ping_url("https://api.flashblock.trade/"),
            "https://api.flashblock.trade/"
        );
    }
}

impl FlashBlockClient {
    pub fn new(rpc_url: String, endpoint: String, auth_token: String) -> Self {
        let rpc_client = SolanaRpcClient::new(rpc_url);
        let http_client = default_http_client_builder().build().unwrap();
        let client = Self {
            rpc_client: Arc::new(rpc_client),
            endpoint,
            auth_token,
            http_client,
            ping_handle: Arc::new(tokio::sync::Mutex::new(None)),
            stop_ping: Arc::new(AtomicBool::new(false)),
        };

        let client_clone = client.clone();
        tokio::spawn(async move {
            client_clone.start_ping_task().await;
        });

        client
    }

    #[inline]
    fn build_ping_url(endpoint: &str) -> String {
        let endpoint = endpoint.trim_end_matches('/');
        format!("{}/", endpoint)
    }

    async fn start_ping_task(&self) {
        let endpoint = self.endpoint.clone();
        let auth_token = self.auth_token.clone();
        let http_client = self.http_client.clone();
        let stop_ping = self.stop_ping.clone();

        let handle = tokio::spawn(async move {
            if let Err(e) = Self::send_ping_request(&http_client, &endpoint, &auth_token).await {
                if crate::common::sdk_log::sdk_log_enabled() {
                    tracing::warn!(target: "sol_trade_sdk", "FlashBlock ping request failed: {}", e);
                }
            }
            let mut interval = tokio::time::interval(Duration::from_secs(30));
            loop {
                interval.tick().await;
                if stop_ping.load(Ordering::Relaxed) {
                    break;
                }
                if let Err(e) = Self::send_ping_request(&http_client, &endpoint, &auth_token).await
                {
                    if crate::common::sdk_log::sdk_log_enabled() {
                        tracing::warn!(target: "sol_trade_sdk", "FlashBlock ping request failed: {}", e);
                    }
                }
            }
        });

        let mut ping_guard = self.ping_handle.lock().await;
        if let Some(old_handle) = ping_guard.as_ref() {
            old_handle.abort();
        }
        *ping_guard = Some(handle);
    }

    async fn send_ping_request(
        http_client: &Client,
        endpoint: &str,
        auth_token: &str,
    ) -> Result<()> {
        let response = http_client
            .get(Self::build_ping_url(endpoint))
            .header("Authorization", auth_token)
            .timeout(Duration::from_millis(1500))
            .send()
            .await?;
        if !response.status().is_success() && crate::common::sdk_log::sdk_log_enabled() {
            tracing::warn!(
                target: "sol_trade_sdk",
                "FlashBlock ping returned non-success status: {}",
                response.status()
            );
        }
        let _ = response.bytes().await;
        Ok(())
    }

    pub async fn send_transaction(
        &self,
        trade_type: TradeType,
        transaction: &VersionedTransaction,
        wait_confirmation: bool,
    ) -> Result<()> {
        let start_time = Instant::now();
        let (content, signature) =
            serialize_transaction_and_encode(transaction, UiTransactionEncoding::Base64)?;

        // FlashBlock API format
        let request_body = serde_json::to_string(&json!({
            "transactions": [content]
        }))?;

        let url = format!("{}/api/v2/submit-batch", self.endpoint);

        // Send request to FlashBlock
        let response_text = self
            .http_client
            .post(&url)
            .body(request_body)
            .header("Authorization", &self.auth_token)
            .header("Content-Type", "application/json")
            .header("Connection", "keep-alive")
            .header("Keep-Alive", "timeout=30, max=1000")
            .send()
            .await?
            .text()
            .await?;

        // Parse response
        if let Ok(response_json) = serde_json::from_str::<serde_json::Value>(&response_text) {
            if crate::common::sdk_log::sdk_log_enabled() {
                if response_json.get("success").is_some() || response_json.get("result").is_some() {
                    crate::common::sdk_log::log_swqos_submitted(
                        "FlashBlock",
                        trade_type,
                        start_time.elapsed(),
                    );
                } else if let Some(_error) = response_json.get("error") {
                    crate::common::sdk_log::log_swqos_submission_failed(
                        "FlashBlock",
                        trade_type,
                        start_time.elapsed(),
                        _error,
                    );
                }
            }
        } else if crate::common::sdk_log::sdk_log_enabled() {
            crate::common::sdk_log::log_swqos_submission_failed(
                "FlashBlock",
                trade_type,
                start_time.elapsed(),
                response_text,
            );
        }

        let start_time: Instant = Instant::now();
        match poll_transaction_confirmation(&self.rpc_client, signature, wait_confirmation).await {
            Ok(_) => (),
            Err(e) => {
                if crate::common::sdk_log::sdk_log_enabled() {
                    tracing::info!(target: "sol_trade_sdk", " signature: {:?}", signature);
                    tracing::warn!(
                        target: "sol_trade_sdk",
                    " [{:width$}] {} confirmation failed: {:?}",
                    "FlashBlock",
                    trade_type,
                    start_time.elapsed(),
                    width = crate::common::sdk_log::SWQOS_LABEL_WIDTH
                    );
                }
                return Err(e);
            }
        }
        if wait_confirmation && crate::common::sdk_log::sdk_log_enabled() {
            tracing::info!(target: "sol_trade_sdk", " signature: {:?}", signature);
            tracing::info!(target: "sol_trade_sdk", " [{:width$}] {} confirmed: {:?}", "FlashBlock", trade_type, start_time.elapsed(), width = crate::common::sdk_log::SWQOS_LABEL_WIDTH);
        }

        Ok(())
    }

    pub async fn send_transactions(
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
}

impl Drop for FlashBlockClient {
    fn drop(&mut self) {
        if Arc::strong_count(&self.ping_handle) != 1 {
            return;
        }

        self.stop_ping.store(true, Ordering::Relaxed);
        let ping_handle = self.ping_handle.clone();
        tokio::spawn(async move {
            let mut ping_guard = ping_handle.lock().await;
            if let Some(handle) = ping_guard.take() {
                handle.abort();
            }
        });
    }
}
