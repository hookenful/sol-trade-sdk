use crate::swqos::common::{
    default_http_client_builder, parse_http_json_submit_response, poll_transaction_confirmation,
    serialize_transaction_and_encode,
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

use crate::{common::SolanaRpcClient, constants::swqos::FLASHBLOCK_TIP_ACCOUNTS};

#[derive(Clone)]
pub struct FlashBlockClient {
    pub endpoint: String,
    pub auth_token: String,
    pub rpc_client: Arc<SolanaRpcClient>,
    pub http_client: Client,
    /// Shared stop signal for the connection warmup loop.
    stop_ping: Arc<AtomicBool>,
    /// Client-instance ref guard used to avoid stopping ping on temporary clone drop.
    instance_refs: Arc<()>,
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

impl FlashBlockClient {
    pub fn new(rpc_url: String, endpoint: String, auth_token: String) -> Self {
        let rpc_client = SolanaRpcClient::new(rpc_url);
        let http_client = default_http_client_builder().build().unwrap();
        let client = Self {
            rpc_client: Arc::new(rpc_client),
            endpoint,
            auth_token,
            http_client,
            stop_ping: Arc::new(AtomicBool::new(false)),
            instance_refs: Arc::new(()),
        };
        let client_clone = client.clone();
        tokio::spawn(async move {
            client_clone.start_ping_task().await;
        });
        client
    }

    async fn start_ping_task(&self) {
        let endpoint = self.endpoint.clone();
        let http_client = self.http_client.clone();
        let stop_ping = self.stop_ping.clone();

        tokio::spawn(async move {
            // Warm the TLS/TCP connection immediately, then keep the pool hot. A non-success
            // status is fine here; the request is intentionally side-effect free.
            let _ = Self::send_ping_request(&http_client, &endpoint).await;
            let mut interval = tokio::time::interval(Duration::from_secs(30));
            loop {
                interval.tick().await;
                if stop_ping.load(Ordering::Relaxed) {
                    break;
                }
                let _ = Self::send_ping_request(&http_client, &endpoint).await;
            }
        });
    }

    async fn send_ping_request(http_client: &Client, endpoint: &str) -> Result<()> {
        let response =
            http_client.get(endpoint).timeout(Duration::from_millis(1500)).send().await?;
        let _ = response.bytes().await;
        Ok(())
    }

    fn validate_submit_response(
        trade_type: TradeType,
        elapsed: Duration,
        status: reqwest::StatusCode,
        response_text: &str,
    ) -> Result<()> {
        let response_json = parse_http_json_submit_response(
            "FlashBlock",
            trade_type,
            elapsed,
            status,
            response_text,
        )?;

        if response_json.get("success") == Some(&serde_json::Value::Bool(false)) {
            let detail = response_json
                .get("message")
                .and_then(serde_json::Value::as_str)
                .unwrap_or(response_text);
            if crate::common::sdk_log::sdk_log_enabled() {
                crate::common::sdk_log::log_swqos_submission_failed(
                    "FlashBlock",
                    trade_type,
                    elapsed,
                    detail,
                );
            }
            return Err(anyhow::anyhow!("FlashBlock submission failed: {detail}"));
        }

        let has_success_true = response_json.get("success") == Some(&serde_json::Value::Bool(true));
        let has_result = response_json.get("result").is_some_and(|v| !v.is_null());
        let has_data_signatures = response_json
            .get("data")
            .and_then(|d| d.get("signatures"))
            .and_then(serde_json::Value::as_array)
            .is_some_and(|arr| !arr.is_empty())
            && response_json.get("code").is_none_or(|code| code == 200 || code == 0);

        if has_success_true || has_result || has_data_signatures {
            if crate::common::sdk_log::sdk_log_enabled() {
                crate::common::sdk_log::log_swqos_submitted("FlashBlock", trade_type, elapsed);
            }
            return Ok(());
        }

        let detail = format!("unexpected JSON (missing result/success): {response_text}");
        if crate::common::sdk_log::sdk_log_enabled() {
            crate::common::sdk_log::log_swqos_submission_failed(
                "FlashBlock",
                trade_type,
                elapsed,
                &detail,
            );
        }
        Err(anyhow::anyhow!("FlashBlock submission failed: {detail}"))
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
        let response = self
            .http_client
            .post(&url)
            .body(request_body)
            .header("Authorization", &self.auth_token)
            .header("Content-Type", "application/json")
            .header("Connection", "keep-alive")
            .header("Keep-Alive", "timeout=30, max=1000")
            .send()
            .await?;
        let status = response.status();
        let response_text = response.text().await?;

        Self::validate_submit_response(trade_type, start_time.elapsed(), status, &response_text)?;

        let start_time: Instant = Instant::now();
        match poll_transaction_confirmation(&self.rpc_client, signature, wait_confirmation).await {
            Ok(_) => (),
            Err(e) => {
                if crate::common::sdk_log::sdk_log_enabled() {
                    println!(" signature: {:?}", signature);
                    println!(
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
            println!(" signature: {:?}", signature);
            println!(
                " [{:width$}] {} confirmed: {:?}",
                "FlashBlock",
                trade_type,
                start_time.elapsed(),
                width = crate::common::sdk_log::SWQOS_LABEL_WIDTH
            );
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
        // Only the last real client instance should stop the shared ping task; temporary
        // Arc/worker clones must not tear down the warm connection loop.
        if Arc::strong_count(&self.instance_refs) != 1 {
            return;
        }
        self.stop_ping.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::swqos::common::test_http::{dummy_versioned_transaction, spawn_mock_http_server};

    fn test_client_for(endpoint: String) -> FlashBlockClient {
        FlashBlockClient {
            endpoint,
            auth_token: "test-token".to_string(),
            rpc_client: Arc::new(SolanaRpcClient::new("http://127.0.0.1:8899".to_string())),
            http_client: default_http_client_builder().build().unwrap(),
            stop_ping: Arc::new(AtomicBool::new(false)),
            instance_refs: Arc::new(()),
        }
    }

    #[tokio::test]
    async fn send_transaction_rejects_http_429_401_500() {
        let tx = dummy_versioned_transaction();
        for (status_line, body, expected_code) in [
            ("429 Too Many Requests", r#"{"error":"rate limited"}"#, "429"),
            ("401 Unauthorized", "unauthorized", "401"),
            ("500 Internal Server Error", "internal error", "500"),
        ] {
            let server = spawn_mock_http_server(status_line, body, Duration::ZERO).await;
            let client = test_client_for(server.base_url.clone());
            let err = client
                .send_transaction(TradeType::Buy, &tx, false)
                .await
                .expect_err("non-2xx HTTP status must fail");
            assert!(
                err.to_string().contains(expected_code),
                "expected error to mention {expected_code}, got: {err}"
            );
        }
    }

    #[tokio::test]
    async fn send_transaction_rejects_json_error_success_false_missing_result_and_invalid_json() {
        let tx = dummy_versioned_transaction();
        for body in [
            r#"{"jsonrpc":"2.0","error":{"code":-32000,"message":"rate limit"},"id":1}"#,
            r#"{"success":false,"message":"tip too low"}"#,
            r#"{"success":true,"error":"rejected"}"#,
            r#"{"code":400,"message":"bad request"}"#,
            r#"{}"#,
            "not-json",
        ] {
            let server = spawn_mock_http_server("200 OK", body, Duration::ZERO).await;
            let client = test_client_for(server.base_url.clone());
            assert!(
                client.send_transaction(TradeType::Buy, &tx, false).await.is_err(),
                "expected error for response body: {body}"
            );
        }
    }

    #[tokio::test]
    async fn send_transaction_accepts_success_true_result_and_data_signatures() {
        let tx = dummy_versioned_transaction();
        for body in [
            r#"{"success":true}"#,
            r#"{"jsonrpc":"2.0","result":"5sig","id":1}"#,
            r#"{"code":200,"message":"ok","data":{"signatures":["5sig"]}}"#,
        ] {
            let server = spawn_mock_http_server("200 OK", body, Duration::ZERO).await;
            let client = test_client_for(server.base_url.clone());
            client
                .send_transaction(TradeType::Buy, &tx, false)
                .await
                .unwrap_or_else(|e| panic!("expected Ok for {body}, got: {e}"));
        }
    }
}
