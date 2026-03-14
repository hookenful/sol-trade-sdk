use crate::swqos::common::default_http_client_builder;
use crate::swqos::common::poll_transaction_confirmation;
use crate::swqos::common::serialize_transaction_and_encode;
use crate::swqos::serialization;
use rand::seq::IndexedRandom;
use reqwest::Client;
use std::sync::atomic::{AtomicBool, Ordering};
use std::{sync::Arc, time::Instant};

use solana_transaction_status::UiTransactionEncoding;
use std::time::Duration;
use tokio::task::JoinHandle;

use crate::swqos::SwqosClientTrait;
use crate::swqos::{SwqosType, TradeType};
use anyhow::Result;
use solana_sdk::transaction::VersionedTransaction;

use crate::{common::SolanaRpcClient, constants::swqos::BLOX_TIP_ACCOUNTS};

#[derive(Clone)]
pub struct BloxrouteClient {
    pub endpoint: String,
    pub auth_token: String,
    pub rpc_client: Arc<SolanaRpcClient>,
    pub http_client: Client,
    pub ping_handle: Arc<tokio::sync::Mutex<Option<JoinHandle<()>>>>,
    pub stop_ping: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl SwqosClientTrait for BloxrouteClient {
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
        let tip_account = *BLOX_TIP_ACCOUNTS
            .choose(&mut rand::rng())
            .or_else(|| BLOX_TIP_ACCOUNTS.first())
            .unwrap();
        Ok(tip_account.to_string())
    }

    fn get_swqos_type(&self) -> SwqosType {
        SwqosType::Bloxroute
    }
}

impl BloxrouteClient {
    pub fn new(rpc_url: String, endpoint: String, auth_token: String) -> Self {
        let rpc_client = SolanaRpcClient::new(rpc_url);
        let http_client = default_http_client_builder()
            .pool_idle_timeout(Duration::from_secs(120))
            .pool_max_idle_per_host(256)
            .build()
            .unwrap();
        let client = Self {
            rpc_client: Arc::new(rpc_client),
            endpoint,
            auth_token,
            http_client,
            ping_handle: Arc::new(tokio::sync::Mutex::new(None)),
            stop_ping: Arc::new(AtomicBool::new(false)),
        };

        // Start ping task
        let client_clone = client.clone();
        tokio::spawn(async move {
            client_clone.start_ping_task().await;
        });

        client
    }

    /// Start periodic ping task to keep connections active.
    async fn start_ping_task(&self) {
        let endpoint = self.endpoint.clone();
        let auth_token = self.auth_token.clone();
        let http_client = self.http_client.clone();
        let stop_ping = self.stop_ping.clone();

        let handle = tokio::spawn(async move {
            // Immediate first ping to warm connection and reduce first-submit cold start latency.
            if let Err(e) = Self::send_ping_request(&http_client, &endpoint, &auth_token).await {
                if crate::common::sdk_log::sdk_log_enabled() {
                    tracing::warn!(target: "sol_trade_sdk", "Bloxroute ping request failed: {}", e);
                }
            }

            // bloXroute docs recommend keep-alive every ~60 seconds.
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            loop {
                interval.tick().await;
                if stop_ping.load(Ordering::Relaxed) {
                    break;
                }
                if let Err(e) = Self::send_ping_request(&http_client, &endpoint, &auth_token).await
                {
                    if crate::common::sdk_log::sdk_log_enabled() {
                        tracing::warn!(target: "sol_trade_sdk", "Bloxroute ping request failed: {}", e);
                    }
                }
            }
        });

        // Update ping_handle - use Mutex to safely update.
        {
            let mut ping_guard = self.ping_handle.lock().await;
            if let Some(old_handle) = ping_guard.as_ref() {
                old_handle.abort();
            }
            *ping_guard = Some(handle);
        }
    }

    /// Send keep-alive request to bloXroute core endpoint.
    async fn send_ping_request(
        http_client: &Client,
        endpoint: &str,
        auth_token: &str,
    ) -> Result<()> {
        let ping_url = format!("{}/api/v2/rate-limit", endpoint);
        // Short timeout for ping; consume body so connection is returned to pool for reuse by submit.
        let response = http_client
            .get(&ping_url)
            .header("Authorization", auth_token)
            .timeout(Duration::from_millis(1500))
            .send()
            .await?;
        let status = response.status();
        let _ = response.bytes().await;
        if !status.is_success() && crate::common::sdk_log::sdk_log_enabled() {
            tracing::warn!(
                target: "sol_trade_sdk",
                "Bloxroute ping request returned non-success status: {}",
                status
            );
        }
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

        // Single format! for body to avoid json! + to_string() double allocation
        let body = format!(
            r#"{{"transaction":{{"content":"{}"}},"frontRunningProtection":false,"useStakedRPCs":true}}"#,
            content
        );

        let endpoint = format!("{}/api/v2/submit", self.endpoint);
        let response_text = self
            .http_client
            .post(&endpoint)
            .body(body)
            .header("Content-Type", "application/json")
            .header("Authorization", self.auth_token.as_str())
            .send()
            .await?
            .text()
            .await?;

        // Parse with from_str to avoid extra wait from .json().await
        if let Ok(response_json) = serde_json::from_str::<serde_json::Value>(&response_text) {
            if crate::common::sdk_log::sdk_log_enabled() {
                if response_json.get("result").is_some() {
                    println!(" [bloxroute] {} submitted: {:?}", trade_type, start_time.elapsed());
                } else if let Some(_error) = response_json.get("error") {
                    eprintln!(" [bloxroute] {} submission failed: {:?}", trade_type, _error);
                }
            }
        } else if crate::common::sdk_log::sdk_log_enabled() {
            eprintln!(" [bloxroute] {} submission failed: {:?}", trade_type, response_text);
        }

        let start_time: Instant = Instant::now();
        match poll_transaction_confirmation(&self.rpc_client, signature, wait_confirmation).await {
            Ok(_) => (),
            Err(e) => {
                if crate::common::sdk_log::sdk_log_enabled() {
                    println!(" signature: {:?}", signature);
                    println!(
                        " [bloxroute] {} confirmation failed: {:?}",
                        trade_type,
                        start_time.elapsed()
                    );
                }
                return Err(e);
            }
        }
        if wait_confirmation && crate::common::sdk_log::sdk_log_enabled() {
            println!(" signature: {:?}", signature);
            println!(" [bloxroute] {} confirmed: {:?}", trade_type, start_time.elapsed());
        }

        Ok(())
    }

    pub async fn send_transactions(
        &self,
        trade_type: TradeType,
        transactions: &Vec<VersionedTransaction>,
        _wait_confirmation: bool,
    ) -> Result<()> {
        let start_time = Instant::now();

        let contents = serialization::serialize_transactions_batch_sync(
            transactions.as_slice(),
            UiTransactionEncoding::Base64,
        )?;
        let entries: String = contents
            .iter()
            .map(|c| format!(r#"{{"transaction":{{"content":"{}"}}}}"#, c))
            .collect::<Vec<_>>()
            .join(",");
        let body = format!(r#"{{"entries":[{}]}}"#, entries);

        let endpoint = format!("{}/api/v2/submit-batch", self.endpoint);
        let response_text = self
            .http_client
            .post(&endpoint)
            .body(body)
            .header("Content-Type", "application/json")
            .header("Authorization", self.auth_token.as_str())
            .send()
            .await?
            .text()
            .await?;

        if crate::common::sdk_log::sdk_log_enabled() {
            if let Ok(response_json) = serde_json::from_str::<serde_json::Value>(&response_text) {
                if response_json.get("result").is_some() {
                    println!(" bloxroute {} submitted: {:?}", trade_type, start_time.elapsed());
                } else if let Some(_error) = response_json.get("error") {
                    eprintln!(" bloxroute {} submission failed: {:?}", trade_type, _error);
                }
            }
        }

        Ok(())
    }
}

impl Drop for BloxrouteClient {
    fn drop(&mut self) {
        // Only the last client instance should stop the shared ping task.
        if Arc::strong_count(&self.ping_handle) != 1 {
            return;
        }

        // Ensure ping task stops when client is destroyed.
        self.stop_ping.store(true, Ordering::Relaxed);

        // Try to stop ping task immediately.
        // Use tokio::spawn to avoid blocking Drop.
        let ping_handle = self.ping_handle.clone();
        tokio::spawn(async move {
            let mut ping_guard = ping_handle.lock().await;
            if let Some(handle) = ping_guard.as_ref() {
                handle.abort();
            }
            *ping_guard = None;
        });
    }
}
