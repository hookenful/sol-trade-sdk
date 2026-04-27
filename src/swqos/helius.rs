//! Helius Sender SWQOS client.
//!
//! Ultra-low latency transaction submission with dual routing to validators and Jito.
//! All transactions must include tips, priority fees, and skip preflight.
//! - Without swqos_only: minimum tip 0.0002 SOL.
//! - With swqos_only=true: minimum tip 0.000005 SOL (much lower, benefit of Helius).
//! API: POST {endpoint}/fast with JSON-RPC sendTransaction.
//! Optional query: api-key (custom TPS only), swqos_only (SWQOS-only routing, lower min tip).

use crate::swqos::common::{
    default_http_client_builder, poll_transaction_confirmation, serialize_transaction_and_encode,
};
use anyhow::Result;
use rand::seq::IndexedRandom;
use reqwest::Client;
use serde_json::json;
use solana_sdk::transaction::VersionedTransaction;
use solana_transaction_status::UiTransactionEncoding;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;

use crate::common::SolanaRpcClient;
use crate::constants::swqos::{
    HELIUS_TIP_ACCOUNTS, SWQOS_MIN_TIP_HELIUS, SWQOS_MIN_TIP_HELIUS_SWQOS_ONLY,
};
use crate::swqos::{SwqosClientTrait, SwqosType, TradeType};

#[derive(Clone)]
pub struct HeliusClient {
    /// Cached full URL with query params (auth/swqos_only) to avoid per-request allocation.
    pub submit_url: String,
    pub rpc_client: Arc<SolanaRpcClient>,
    pub http_client: Client,
    pub ping_url: String,
    pub ping_handle: Arc<tokio::sync::Mutex<Option<JoinHandle<()>>>>,
    pub stop_ping: Arc<AtomicBool>,
    /// When true, min_tip_sol() returns 0.000005; else 0.0002.
    swqos_only: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_ping_url_strips_fast_path_and_query() {
        assert_eq!(
            HeliusClient::build_ping_url("https://sender.helius-rpc.com/fast?api-key=k"),
            "https://sender.helius-rpc.com/ping"
        );
        assert_eq!(
            HeliusClient::build_ping_url("https://sender.helius-rpc.com/fast"),
            "https://sender.helius-rpc.com/ping"
        );
        assert_eq!(
            HeliusClient::build_ping_url("https://sender.helius-rpc.com"),
            "https://sender.helius-rpc.com/ping"
        );
    }
}

impl HeliusClient {
    pub fn new(
        rpc_url: String,
        endpoint: String,
        api_key: Option<String>,
        swqos_only: bool,
    ) -> Self {
        let rpc_client = SolanaRpcClient::new(rpc_url);
        let http_client = default_http_client_builder().build().unwrap();
        let submit_url = Self::build_submit_url(&endpoint, api_key.as_deref(), swqos_only);
        let ping_url = Self::build_ping_url(&endpoint);
        let client = Self {
            submit_url,
            rpc_client: Arc::new(rpc_client),
            http_client,
            ping_url,
            ping_handle: Arc::new(tokio::sync::Mutex::new(None)),
            stop_ping: Arc::new(AtomicBool::new(false)),
            swqos_only,
        };

        let client_clone = client.clone();
        tokio::spawn(async move {
            client_clone.start_ping_task().await;
        });

        client
    }

    /// Build URL once at construction; no per-request allocation.
    #[inline]
    fn build_submit_url(endpoint: &str, api_key: Option<&str>, swqos_only: bool) -> String {
        let mut url = endpoint.to_string();
        let mut has_query = endpoint.contains('?');
        if let Some(key) = api_key {
            if !key.is_empty() {
                url.push_str(if has_query { "&" } else { "?" });
                url.push_str("api-key=");
                url.push_str(key);
                has_query = true;
            }
        }
        if swqos_only {
            url.push_str(if has_query { "&" } else { "?" });
            url.push_str("swqos_only=true");
        }
        url
    }

    #[inline]
    fn build_ping_url(endpoint: &str) -> String {
        let endpoint_no_query = endpoint.split('?').next().unwrap_or(endpoint);
        if let Some(base) = endpoint_no_query.strip_suffix("/fast") {
            format!("{}/ping", base)
        } else if endpoint_no_query.ends_with('/') {
            format!("{}ping", endpoint_no_query)
        } else {
            format!("{}/ping", endpoint_no_query)
        }
    }

    async fn start_ping_task(&self) {
        let ping_url = self.ping_url.clone();
        let http_client = self.http_client.clone();
        let stop_ping = self.stop_ping.clone();

        let handle = tokio::spawn(async move {
            if let Err(e) = Self::send_ping_request(&http_client, &ping_url).await {
                if crate::common::sdk_log::sdk_log_enabled() {
                    tracing::warn!(target: "sol_trade_sdk", "helius ping request failed: {}", e);
                }
            }
            let mut interval = tokio::time::interval(Duration::from_secs(30));
            loop {
                interval.tick().await;
                if stop_ping.load(Ordering::Relaxed) {
                    break;
                }
                if let Err(e) = Self::send_ping_request(&http_client, &ping_url).await {
                    if crate::common::sdk_log::sdk_log_enabled() {
                        tracing::warn!(target: "sol_trade_sdk", "helius ping request failed: {}", e);
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

    async fn send_ping_request(http_client: &Client, ping_url: &str) -> Result<()> {
        let response =
            http_client.get(ping_url).timeout(Duration::from_millis(1500)).send().await?;
        if !response.status().is_success() && crate::common::sdk_log::sdk_log_enabled() {
            tracing::warn!(
                target: "sol_trade_sdk",
                "helius ping returned non-success status: {}",
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

        let request_body = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "id": "1",
            "method": "sendTransaction",
            "params": [
                content,
                {
                    "encoding": "base64",
                    "skipPreflight": true,
                    "maxRetries": 0
                }
            ]
        }))?;

        let response = self
            .http_client
            .post(&self.submit_url)
            .body(request_body)
            .header("Content-Type", "application/json")
            .send()
            .await?;

        let status = response.status();
        let response_text = response.text().await?;

        if !status.is_success() {
            if crate::common::sdk_log::sdk_log_enabled() {
                tracing::warn!(
                    target: "sol_trade_sdk",
                    " [helius] {} submission failed after {:?} status={} body={}",
                    trade_type, start_time.elapsed(), status, response_text
                );
            }
            return Err(anyhow::anyhow!(
                "Helius Sender failed: status={} body={}",
                status,
                response_text
            ));
        }

        if let Ok(response_json) = serde_json::from_str::<serde_json::Value>(&response_text) {
            if response_json.get("error").is_some() {
                let err_msg = response_json["error"]
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                if crate::common::sdk_log::sdk_log_enabled() {
                    crate::common::sdk_log::log_swqos_submission_failed(
                        "helius",
                        trade_type,
                        start_time.elapsed(),
                        err_msg,
                    );
                }
                return Err(anyhow::anyhow!("Helius Sender error: {}", err_msg));
            }
            if response_json.get("result").is_some() && crate::common::sdk_log::sdk_log_enabled() {
                crate::common::sdk_log::log_swqos_submitted(
                    "helius",
                    trade_type,
                    start_time.elapsed(),
                );
            }
        } else if crate::common::sdk_log::sdk_log_enabled() {
            crate::common::sdk_log::log_swqos_submission_failed(
                "helius",
                trade_type,
                start_time.elapsed(),
                response_text,
            );
        }

        match poll_transaction_confirmation(&self.rpc_client, signature, wait_confirmation).await {
            Ok(_) => (),
            Err(e) => {
                if crate::common::sdk_log::sdk_log_enabled() {
                    tracing::warn!(
                        target: "sol_trade_sdk",
                        " [{:width$}] {} confirmation failed: {:?}",
                        "helius",
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
            tracing::info!(target: "sol_trade_sdk", " [{:width$}] {} confirmed: {:?}", "helius", trade_type, start_time.elapsed(), width = crate::common::sdk_log::SWQOS_LABEL_WIDTH);
        }
        Ok(())
    }
}

impl Drop for HeliusClient {
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

#[async_trait::async_trait]
impl SwqosClientTrait for HeliusClient {
    async fn send_transaction(
        &self,
        trade_type: TradeType,
        transaction: &VersionedTransaction,
        wait_confirmation: bool,
    ) -> Result<()> {
        HeliusClient::send_transaction(self, trade_type, transaction, wait_confirmation).await
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
        let tip_account = *HELIUS_TIP_ACCOUNTS
            .choose(&mut rand::rng())
            .or_else(|| HELIUS_TIP_ACCOUNTS.first())
            .unwrap();
        Ok(tip_account.to_string())
    }

    fn get_swqos_type(&self) -> SwqosType {
        SwqosType::Helius
    }

    #[inline(always)]
    fn min_tip_sol(&self) -> f64 {
        if self.swqos_only {
            SWQOS_MIN_TIP_HELIUS_SWQOS_ONLY
        } else {
            SWQOS_MIN_TIP_HELIUS
        }
    }
}
