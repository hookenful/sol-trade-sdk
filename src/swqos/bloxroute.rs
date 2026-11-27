use crate::swqos::common::{
    default_http_client_builder, parse_http_json_submit_response, poll_transaction_confirmation,
    serialize_transaction_and_encode,
};
use crate::swqos::serialization;
use rand::seq::IndexedRandom;
use reqwest::Client;
use std::sync::atomic::{AtomicBool, Ordering};
use std::{sync::Arc, time::Instant};

use solana_transaction_status_client_types::UiTransactionEncoding;
use std::time::Duration;

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
    /// Shared stop signal for the connection warmup loop.
    stop_ping: Arc<AtomicBool>,
    /// Client-instance ref guard used to avoid stopping ping on temporary clone drop.
    instance_refs: Arc<()>,
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
            "bloxroute",
            trade_type,
            elapsed,
            status,
            response_text,
        )?;

        let has_result = response_json.get("result").is_some_and(|v| !v.is_null());
        let has_signature = response_json
            .get("signature")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|s| !s.is_empty());

        if has_result || has_signature {
            if crate::common::sdk_log::sdk_log_enabled() {
                crate::common::sdk_log::log_swqos_submitted("bloxroute", trade_type, elapsed);
            }
            return Ok(());
        }

        let detail = format!("unexpected JSON (missing result/signature): {response_text}");
        if crate::common::sdk_log::sdk_log_enabled() {
            crate::common::sdk_log::log_swqos_submission_failed(
                "bloxroute",
                trade_type,
                elapsed,
                &detail,
            );
        }
        Err(anyhow::anyhow!("bloxroute submission failed: {detail}"))
    }

    fn validate_batch_response(
        trade_type: TradeType,
        elapsed: Duration,
        status: reqwest::StatusCode,
        response_text: &str,
    ) -> Result<()> {
        let response_json = parse_http_json_submit_response(
            "bloxroute",
            trade_type,
            elapsed,
            status,
            response_text,
        )?;

        let has_result = response_json.get("result").is_some_and(|v| !v.is_null());
        let has_signature = response_json
            .get("signature")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|s| !s.is_empty());
        let has_valid_transactions = response_json
            .get("transactions")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|txs| {
                !txs.is_empty()
                    && txs.iter().all(|tx| {
                        let submitted_ok =
                            tx.get("submitted").and_then(serde_json::Value::as_bool) != Some(false);
                        let no_entry_error = match tx.get("error") {
                            None | Some(serde_json::Value::Null) => true,
                            Some(serde_json::Value::String(s)) => s.is_empty(),
                            Some(_) => false,
                        };
                        let has_entry_ack =
                            tx.get("submitted").and_then(serde_json::Value::as_bool) == Some(true)
                                || tx
                                    .get("signature")
                                    .and_then(serde_json::Value::as_str)
                                    .is_some_and(|s| !s.is_empty());
                        submitted_ok && no_entry_error && has_entry_ack
                    })
            });

        if has_result || has_signature || has_valid_transactions {
            if crate::common::sdk_log::sdk_log_enabled() {
                crate::common::sdk_log::log_swqos_submitted("bloxroute", trade_type, elapsed);
            }
            return Ok(());
        }

        let detail = format!("unexpected batch JSON response: {response_text}");
        if crate::common::sdk_log::sdk_log_enabled() {
            crate::common::sdk_log::log_swqos_submission_failed(
                "bloxroute",
                trade_type,
                elapsed,
                &detail,
            );
        }
        Err(anyhow::anyhow!("bloxroute batch submission failed: {detail}"))
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
        let response = self
            .http_client
            .post(&endpoint)
            .body(body)
            .header("Content-Type", "application/json")
            .header("Authorization", self.auth_token.as_str())
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
                        "bloxroute",
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
                "bloxroute",
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
        let response = self
            .http_client
            .post(&endpoint)
            .body(body)
            .header("Content-Type", "application/json")
            .header("Authorization", self.auth_token.as_str())
            .send()
            .await?;
        let status = response.status();
        let response_text = response.text().await?;

        Self::validate_batch_response(trade_type, start_time.elapsed(), status, &response_text)?;

        Ok(())
    }
}

impl Drop for BloxrouteClient {
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

    fn test_client_for(endpoint: String) -> BloxrouteClient {
        BloxrouteClient {
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
            ("429 Too Many Requests", r#"{"message":"rate limit exceeded"}"#, "429"),
            ("401 Unauthorized", "unauthorized", "401"),
            ("500 Internal Server Error", "server failure", "500"),
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
    async fn send_transaction_rejects_json_error_missing_result_and_invalid_json() {
        let tx = dummy_versioned_transaction();
        for body in [
            r#"{"jsonrpc":"2.0","error":{"code":-32000,"message":"rejected"},"id":1}"#,
            r#"{"error":"bad request"}"#,
            r#"{"signature":""}"#,
            r#"{}"#,
            "plain-text-error",
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
    async fn send_transaction_accepts_signature_and_json_rpc_result() {
        let tx = dummy_versioned_transaction();
        for body in
            [r#"{"signature":"5sigBase58"}"#, r#"{"jsonrpc":"2.0","result":"5sigBase58","id":1}"#]
        {
            let server = spawn_mock_http_server("200 OK", body, Duration::ZERO).await;
            let client = test_client_for(server.base_url.clone());
            client
                .send_transaction(TradeType::Buy, &tx, false)
                .await
                .unwrap_or_else(|e| panic!("expected Ok for {body}, got: {e}"));
        }
    }

    #[tokio::test]
    async fn send_transactions_batch_validates_errors_and_accepts_valid_responses() {
        let txs = vec![dummy_versioned_transaction()];
        for (status_line, body) in [
            ("429 Too Many Requests", "rate limited"),
            ("500 Internal Server Error", "internal error"),
            ("200 OK", r#"{"error":{"code":-32000,"message":"batch failed"}}"#),
            (
                "200 OK",
                r#"{"transactions":[{"signature":"5sig","submitted":false,"error":"BlockhashNotFound"}]}"#,
            ),
            ("200 OK", r#"{}"#),
            ("200 OK", "not-json"),
        ] {
            let server = spawn_mock_http_server(status_line, body, Duration::ZERO).await;
            let client = test_client_for(server.base_url.clone());
            assert!(
                client.send_transactions(TradeType::Buy, &txs, false).await.is_err(),
                "expected batch error for {status_line} / {body}"
            );
        }

        for body in [
            r#"{"transactions":[{"signature":"5sig","submitted":true,"error":""}]}"#,
            r#"{"jsonrpc":"2.0","result":"ok","id":1}"#,
        ] {
            let server = spawn_mock_http_server("200 OK", body, Duration::ZERO).await;
            let client = test_client_for(server.base_url.clone());
            client
                .send_transactions(TradeType::Buy, &txs, false)
                .await
                .unwrap_or_else(|e| panic!("expected batch Ok for {body}, got: {e}"));
        }
    }
}
