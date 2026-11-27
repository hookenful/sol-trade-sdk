use crate::swqos::common::{
    default_http_client_builder, poll_transaction_confirmation, serialize_transaction_and_encode,
    FormatBase64VersionedTransaction,
};
use rand::seq::IndexedRandom;
use reqwest::Client;
use serde_json::json;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use std::{sync::Arc, time::Instant};

use solana_transaction_status_client_types::UiTransactionEncoding;

use crate::swqos::SwqosClientTrait;
use crate::swqos::{SwqosType, TradeType};
use anyhow::Result;
use solana_sdk::transaction::VersionedTransaction;

use crate::{common::SolanaRpcClient, constants::swqos::JITO_TIP_ACCOUNTS};

const JITO_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);
const JITO_KEEPALIVE_TIMEOUT: Duration = Duration::from_millis(1500);

#[derive(Clone)]
pub struct JitoClient {
    pub endpoint: String,
    pub auth_token: String,
    pub rpc_client: Arc<SolanaRpcClient>,
    pub http_client: Client,
    /// Shared stop signal for the background HTTP keepalive loop.
    stop_ping: Arc<AtomicBool>,
    /// Client-instance ref guard used to avoid stopping ping on temporary clone drop.
    instance_refs: Arc<()>,
    /// Abort handle for immediate cleanup when the last client instance drops.
    ping_handle: Option<tokio::task::AbortHandle>,
}

#[async_trait::async_trait]
impl SwqosClientTrait for JitoClient {
    async fn send_transaction(
        &self,
        trade_type: TradeType,
        transaction: &VersionedTransaction,
        wait_confirmation: bool,
    ) -> Result<()> {
        self.send_transaction_impl(trade_type, transaction, wait_confirmation).await
    }

    async fn send_transactions(
        &self,
        trade_type: TradeType,
        transactions: &Vec<VersionedTransaction>,
        wait_confirmation: bool,
    ) -> Result<()> {
        self.send_transactions_impl(trade_type, transactions, wait_confirmation).await
    }

    fn get_tip_account(&self) -> Result<String> {
        if let Some(acc) = JITO_TIP_ACCOUNTS.choose(&mut rand::rng()) {
            Ok(acc.to_string())
        } else {
            Err(anyhow::anyhow!("no valid tip accounts found"))
        }
    }

    fn get_swqos_type(&self) -> SwqosType {
        SwqosType::Jito
    }
}

impl JitoClient {
    pub fn new(rpc_url: String, endpoint: String, auth_token: String) -> Self {
        let rpc_client = SolanaRpcClient::new(rpc_url);
        let http_client = default_http_client_builder().build().unwrap();
        let stop_ping = Arc::new(AtomicBool::new(false));
        let ping_handle = Self::spawn_ping_task(&http_client, &endpoint, &stop_ping);
        Self {
            rpc_client: Arc::new(rpc_client),
            endpoint,
            auth_token,
            http_client,
            stop_ping,
            instance_refs: Arc::new(()),
            ping_handle,
        }
    }

    fn spawn_ping_task(
        http_client: &Client,
        endpoint: &str,
        stop_ping: &Arc<AtomicBool>,
    ) -> Option<tokio::task::AbortHandle> {
        let handle = tokio::runtime::Handle::try_current().ok()?;
        let endpoint = endpoint.to_string();
        let http_client = http_client.clone();
        let stop_ping = stop_ping.clone();

        Some(
            handle
                .spawn(async move {
                    // Warm the TLS/TCP connection immediately (first tick fires at t=0),
                    // then keep the pool hot. A non-success status is fine here; the
                    // request is intentionally side-effect free and avoids send rate limits.
                    let mut interval = tokio::time::interval(JITO_KEEPALIVE_INTERVAL);
                    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                    loop {
                        interval.tick().await;
                        if stop_ping.load(Ordering::Relaxed) {
                            break;
                        }
                        let _ = Self::send_ping_request(&http_client, &endpoint).await;
                    }
                })
                .abort_handle(),
        )
    }

    async fn send_ping_request(http_client: &Client, endpoint: &str) -> Result<()> {
        let response = http_client.get(endpoint).timeout(JITO_KEEPALIVE_TIMEOUT).send().await?;
        let _ = response.bytes().await;
        Ok(())
    }

    pub async fn send_transaction_impl(
        &self,
        trade_type: TradeType,
        transaction: &VersionedTransaction,
        wait_confirmation: bool,
    ) -> Result<()> {
        let start_time = Instant::now();
        let (content, signature) =
            serialize_transaction_and_encode(transaction, UiTransactionEncoding::Base64)?;

        let request_body = serde_json::to_string(&json!({
            "id": 1,
            "jsonrpc": "2.0",
            "method": "sendTransaction",
            "params": [
                content,
                {
                    "encoding": "base64"
                }
            ]
        }))?;

        let endpoint = if self.auth_token.is_empty() {
            format!("{}/api/v1/transactions", self.endpoint)
        } else {
            format!("{}/api/v1/transactions?uuid={}", self.endpoint, self.auth_token)
        };
        let response = if self.auth_token.is_empty() {
            self.http_client.post(&endpoint)
        } else {
            self.http_client.post(&endpoint).header("x-jito-auth", &self.auth_token)
        };
        let response_text = response
            .body(request_body)
            .header("Content-Type", "application/json")
            .send()
            .await?
            .text()
            .await?;

        if crate::common::sdk_log::sdk_log_enabled() {
            if let Ok(response_json) = serde_json::from_str::<serde_json::Value>(&response_text) {
                if response_json.get("result").is_some() {
                    crate::common::sdk_log::log_swqos_submitted(
                        "jito",
                        trade_type,
                        start_time.elapsed(),
                    );
                } else if let Some(_error) = response_json.get("error") {
                    eprintln!(
                        " [jito] {} submission failed after {:?}: {:?}",
                        trade_type,
                        start_time.elapsed(),
                        _error
                    );
                }
            } else {
                crate::common::sdk_log::log_swqos_submission_failed(
                    "jito",
                    trade_type,
                    start_time.elapsed(),
                    response_text,
                );
            }
        }

        let start_time: Instant = Instant::now();
        match poll_transaction_confirmation(&self.rpc_client, signature, wait_confirmation).await {
            Ok(_) => (),
            Err(e) => {
                if crate::common::sdk_log::sdk_log_enabled() {
                    println!(" signature: {:?}", signature);
                    println!(
                        " [{:width$}] {} confirmation failed: {:?}",
                        "jito",
                        trade_type,
                        start_time.elapsed(),
                        width = crate::common::sdk_log::SWQOS_LABEL_WIDTH
                    );
                }
                return Err(e);
            }
        }
        if wait_confirmation && crate::common::sdk_log::sdk_log_enabled() {
            println!(" signature: {:?}", signature);
            println!(
                " [{:width$}] {} confirmed: {:?}",
                "jito",
                trade_type,
                start_time.elapsed(),
                width = crate::common::sdk_log::SWQOS_LABEL_WIDTH
            );
        }

        Ok(())
    }

    pub async fn send_transactions_impl(
        &self,
        trade_type: TradeType,
        transactions: &Vec<VersionedTransaction>,
        _wait_confirmation: bool,
    ) -> Result<()> {
        let start_time = Instant::now();
        let txs_base64 =
            transactions.iter().map(|tx| tx.to_base64_string()).collect::<Vec<String>>();
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "sendBundle",
            "params": [
                txs_base64,
                { "encoding": "base64" }
            ],
            "id": 1,
        });

        let endpoint = if self.auth_token.is_empty() {
            format!("{}/api/v1/bundles", self.endpoint)
        } else {
            format!("{}/api/v1/bundles?uuid={}", self.endpoint, self.auth_token)
        };
        let response = if self.auth_token.is_empty() {
            self.http_client.post(&endpoint)
        } else {
            self.http_client.post(&endpoint).header("x-jito-auth", &self.auth_token)
        };
        let response_text = response
            .body(body.to_string())
            .header("Content-Type", "application/json")
            .send()
            .await?
            .text()
            .await?;

        if crate::common::sdk_log::sdk_log_enabled() {
            if let Ok(response_json) = serde_json::from_str::<serde_json::Value>(&response_text) {
                if response_json.get("result").is_some() {
                    println!(" jito {} submitted: {:?}", trade_type, start_time.elapsed());
                } else if let Some(_error) = response_json.get("error") {
                    eprintln!(
                        " jito {} submission failed after {:?}: {:?}",
                        trade_type,
                        start_time.elapsed(),
                        _error
                    );
                }
            }
        }

        Ok(())
    }
}

impl Drop for JitoClient {
    fn drop(&mut self) {
        // Only the last client instance should stop the shared keepalive task.
        if Arc::strong_count(&self.instance_refs) != 1 {
            return;
        }
        self.stop_ping.store(true, Ordering::Relaxed);
        if let Some(handle) = &self.ping_handle {
            handle.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn dropping_clone_does_not_stop_jito_ping_task() {
        let client = JitoClient {
            endpoint: "http://127.0.0.1:1".to_string(),
            auth_token: String::new(),
            rpc_client: Arc::new(SolanaRpcClient::new("http://127.0.0.1:8899".to_string())),
            http_client: default_http_client_builder().build().unwrap(),
            stop_ping: Arc::new(AtomicBool::new(false)),
            instance_refs: Arc::new(()),
            ping_handle: None,
        };
        let stop_ping = client.stop_ping.clone();
        let clone = client.clone();

        drop(clone);
        assert!(!stop_ping.load(Ordering::Relaxed));

        drop(client);
        assert!(stop_ping.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn new_warms_http_connection_and_stops_on_drop() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 512];
            let n = stream.read(&mut buf).await.unwrap();
            let req = String::from_utf8_lossy(&buf[..n]).into_owned();
            let _ = stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: keep-alive\r\n\r\nok",
                )
                .await;
            req
        });

        let client = JitoClient::new(
            "http://127.0.0.1:8899".to_string(),
            format!("http://{addr}"),
            String::new(),
        );
        let stop_ping = client.stop_ping.clone();

        let request = tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .expect("jito keepalive should send immediate warmup request")
            .unwrap();
        assert!(request.starts_with("GET / HTTP/"), "unexpected warmup request: {request}");

        drop(client);
        assert!(stop_ping.load(Ordering::Relaxed));
    }
}
