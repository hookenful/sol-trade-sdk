use anyhow::Context as _;
use anyhow::Result;
use arc_swap::ArcSwap;
use quinn::{
    crypto::rustls::QuicClientConfig, ClientConfig, Connection, Endpoint, IdleTimeout,
    TransportConfig,
};
use rand::seq::IndexedRandom as _;
use solana_sdk::signer::Signer;
use solana_sdk::{signature::Keypair, transaction::VersionedTransaction};
use solana_tls_utils::{new_dummy_x509_certificate, SkipServerVerification};
use std::time::Instant;
use std::{
    net::{SocketAddr, ToSocketAddrs as _},
    sync::Arc,
    time::Duration,
};
use tokio::sync::Mutex;
use tokio::time::timeout;

use crate::common::SolanaRpcClient;
use crate::swqos::common::poll_transaction_confirmation;
use crate::swqos::serialization::serialize_transaction_bincode_sync;
use crate::swqos::SwqosClientTrait;
use crate::{
    constants::swqos::SPEEDLANDING_TIP_ACCOUNTS,
    swqos::{SwqosType, TradeType},
};

const ALPN_TPU_PROTOCOL_ID: &[u8] = b"solana-tpu";
/// QUIC TLS SNI：与 Speedlanding 官方客户端一致，固定为 `speed-landing`（勿用 PoP 主机名，否则易握手失败）。
const SPEED_SERVER: &str = "speed-landing";
const KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(25);
const MAX_IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const SEND_TIMEOUT: Duration = Duration::from_secs(5);

pub struct SpeedlandingClient {
    pub rpc_client: Arc<SolanaRpcClient>,
    endpoint: Endpoint,
    client_config: ClientConfig,
    addr: SocketAddr,
    connection: ArcSwap<Connection>,
    reconnect: Mutex<()>,
    connect_timeout: Duration,
    send_timeout: Duration,
}

impl SpeedlandingClient {
    pub async fn new(rpc_url: String, endpoint_string: String, api_key: String) -> Result<Self> {
        let rpc_client = SolanaRpcClient::new(rpc_url);
        // Speedlanding QUIC：与官方一致使用 `solana_tls_utils::new_dummy_x509_certificate`（Ed25519 dummy cert）+ SNI `speed-landing`。
        let keypair_bytes = bs58::decode(api_key.trim()).into_vec().map_err(|e| {
            anyhow::anyhow!("Speedlanding api_token base58 解码失败（mTLS 用）: {}", e)
        })?;
        let keypair = Keypair::try_from(keypair_bytes.as_slice()).map_err(|e| {
            anyhow::anyhow!("Speedlanding api_token 无法解析为 Solana keypair（mTLS 用）: {}", e)
        })?;
        let (cert, key) = new_dummy_x509_certificate(&keypair);
        let mut crypto = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(SkipServerVerification::new())
            .with_client_auth_cert(vec![cert], key)
            .context("failed to configure client certificate")?;

        crypto.alpn_protocols = vec![ALPN_TPU_PROTOCOL_ID.to_vec()];

        let client_crypto = QuicClientConfig::try_from(crypto)
            .context("failed to convert rustls config into quinn crypto config")?;
        let mut client_config = ClientConfig::new(Arc::new(client_crypto));
        let mut transport = TransportConfig::default();
        transport.keep_alive_interval(Some(KEEP_ALIVE_INTERVAL));
        transport.max_idle_timeout(Some(IdleTimeout::try_from(MAX_IDLE_TIMEOUT)?));
        client_config.transport_config(Arc::new(transport));

        let mut endpoint = Endpoint::client("0.0.0.0:0".parse()?)?;
        endpoint.set_default_client_config(client_config.clone());
        let addr = endpoint_string
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| anyhow::anyhow!("Address not resolved"))?;
        let connecting = endpoint.connect(addr, SPEED_SERVER)?;
        let connection = timeout(CONNECT_TIMEOUT, connecting)
            .await
            .context("Speedlanding QUIC connect timeout")?
            .with_context(|| {
                format!(
                    "Speedlanding QUIC handshake failed（请确认：1) 机器人登记的身份与钱包公钥 {} 一致 2) 本机 UDP 可访问 {} 3) region 与 PoP 匹配）",
                    keypair.pubkey(),
                    endpoint_string
                )
            })?;

        Ok(Self {
            rpc_client: Arc::new(rpc_client),
            endpoint,
            client_config,
            addr,
            connection: ArcSwap::from_pointee(connection),
            reconnect: Mutex::new(()),
            connect_timeout: CONNECT_TIMEOUT,
            send_timeout: SEND_TIMEOUT,
        })
    }

    async fn connect_fresh(&self) -> Result<Connection> {
        let connecting =
            self.endpoint.connect_with(self.client_config.clone(), self.addr, SPEED_SERVER)?;
        timeout(self.connect_timeout, connecting)
            .await
            .context("Speedlanding QUIC reconnect timeout")?
            .with_context(|| {
                format!(
                    "Speedlanding QUIC re-handshake failed（对端 {} SNI {}）",
                    self.addr, SPEED_SERVER
                )
            })
    }

    /// Ensure we have a live connection: if current one is closed, reconnect under lock so
    /// concurrent senders wait and then all use the new connection.
    async fn ensure_connected(&self) -> Result<Arc<Connection>> {
        let current = self.connection.load_full();
        if current.close_reason().is_none() {
            return Ok(current);
        }

        let _guard = self.reconnect.lock().await;
        let current = self.connection.load_full();
        if current.close_reason().is_some() {
            let connection = Arc::new(self.connect_fresh().await?);
            self.connection.store(connection.clone());
            return Ok(connection);
        }
        Ok(current)
    }

    /// Force-replace a stale or timed-out connection even when `close_reason().is_none()`
    /// (half-open QUIC). Serializes concurrent callers via `self.reconnect`: if another task
    /// already replaced `stale` with a healthy connection (`!Arc::ptr_eq(&current, stale)`),
    /// reuses it; otherwise closes the stale connection and dials a fresh one.
    async fn reconnect_if_stale(&self, stale: &Arc<Connection>) -> Result<Arc<Connection>> {
        let _guard = self.reconnect.lock().await;
        let current = self.connection.load_full();
        if !Arc::ptr_eq(&current, stale) && current.close_reason().is_none() {
            return Ok(current);
        }
        current.close(0u32.into(), b"stale_reconnect");
        if !Arc::ptr_eq(&current, stale) {
            stale.close(0u32.into(), b"stale_reconnect");
        }
        let connection = Arc::new(self.connect_fresh().await?);
        self.connection.store(connection.clone());
        Ok(connection)
    }

    async fn try_send_bytes(connection: &Connection, payload: &[u8]) -> Result<()> {
        let mut stream = connection.open_uni().await?;
        stream.write_all(payload).await?;
        stream.finish()?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl SwqosClientTrait for SpeedlandingClient {
    async fn send_transaction(
        &self,
        trade_type: TradeType,
        transaction: &VersionedTransaction,
        wait_confirmation: bool,
    ) -> Result<()> {
        let start_time = Instant::now();
        let (buf_guard, signature) = serialize_transaction_bincode_sync(transaction)?;
        let mut connection = self.ensure_connected().await?;
        let mut send_result =
            timeout(self.send_timeout, Self::try_send_bytes(&connection, &buf_guard)).await;
        let need_retry = matches!(&send_result, Ok(Err(_)) | Err(_));
        if need_retry {
            if crate::common::sdk_log::sdk_log_enabled() {
                tracing::warn!(
                    target: "sol_trade_sdk",
                    trade_type = %trade_type,
                    elapsed = ?start_time.elapsed(),
                    "Speedlanding QUIC initial send failed; reconnecting and retrying"
                );
            }
            connection = match self.reconnect_if_stale(&connection).await {
                Ok(conn) => conn,
                Err(e) => {
                    if crate::common::sdk_log::sdk_log_enabled() {
                        crate::common::sdk_log::log_swqos_submission_failed(
                            "Speedlanding",
                            trade_type,
                            start_time.elapsed(),
                            &e,
                        );
                    }
                    return Err(e);
                }
            };
            send_result =
                timeout(self.send_timeout, Self::try_send_bytes(&connection, &buf_guard)).await;
        }
        match send_result.context("Speedlanding QUIC send timeout") {
            Ok(Ok(())) => {
                if crate::common::sdk_log::sdk_log_enabled() {
                    crate::common::sdk_log::log_swqos_submitted(
                        "Speedlanding",
                        trade_type,
                        start_time.elapsed(),
                    );
                }
            }
            Ok(Err(e)) => {
                if crate::common::sdk_log::sdk_log_enabled() {
                    crate::common::sdk_log::log_swqos_submission_failed(
                        "Speedlanding",
                        trade_type,
                        start_time.elapsed(),
                        &e,
                    );
                }
                return Err(e);
            }
            Err(e) => {
                connection.close(0u32.into(), b"send_timeout");
                if crate::common::sdk_log::sdk_log_enabled() {
                    crate::common::sdk_log::log_swqos_submission_failed(
                        "Speedlanding",
                        trade_type,
                        start_time.elapsed(),
                        "timeout",
                    );
                }
                return Err(e);
            }
        }
        drop(buf_guard);
        match poll_transaction_confirmation(&self.rpc_client, signature, wait_confirmation).await {
            Ok(_) => (),
            Err(e) => {
                if crate::common::sdk_log::sdk_log_enabled() {
                    println!(" signature: {:?}", signature);
                    crate::common::sdk_log::log_swqos_submission_failed(
                        "Speedlanding",
                        trade_type,
                        start_time.elapsed(),
                        &e,
                    );
                }
                return Err(e);
            }
        }
        if wait_confirmation && crate::common::sdk_log::sdk_log_enabled() {
            println!(" signature: {:?}", signature);
            println!(
                " [{:width$}] {} confirmed: {:?}",
                "Speedlanding",
                trade_type,
                start_time.elapsed(),
                width = crate::common::sdk_log::SWQOS_LABEL_WIDTH
            );
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
        let tip_account = *SPEEDLANDING_TIP_ACCOUNTS
            .choose(&mut rand::rng())
            .or_else(|| SPEEDLANDING_TIP_ACCOUNTS.first())
            .unwrap();
        Ok(tip_account.to_string())
    }

    fn get_swqos_type(&self) -> SwqosType {
        SwqosType::Speedlanding
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::swqos::common::test_http::dummy_versioned_transaction;
    use quinn::crypto::rustls::QuicServerConfig;
    use rcgen::{CertificateParams, KeyPair as RcgenKeyPair};
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::oneshot;

    fn make_server_config(max_uni_streams: u32) -> Arc<quinn::ServerConfig> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let tls_key = RcgenKeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)
            .expect("generate test QUIC server key");
        let cert_params =
            CertificateParams::new(vec![SPEED_SERVER.to_string()]).expect("cert params");
        let cert = cert_params.self_signed(&tls_key).expect("self sign test cert");
        let cert_der = CertificateDer::from(cert.der().to_vec());
        let key_der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(tls_key.serialize_der()));

        let mut server_crypto = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .expect("tls versions")
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)
        .expect("server cert");
        server_crypto.alpn_protocols = vec![ALPN_TPU_PROTOCOL_ID.to_vec()];

        let quic_crypto =
            QuicServerConfig::try_from(server_crypto).expect("quinn server crypto config");
        let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(quic_crypto));
        let mut transport = TransportConfig::default();
        transport.max_concurrent_uni_streams(max_uni_streams.into());
        server_config.transport_config(Arc::new(transport));
        Arc::new(server_config)
    }

    async fn make_test_client(addr: SocketAddr, send_timeout: Duration) -> SpeedlandingClient {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let keypair = Keypair::new();
        let api_key = bs58::encode(keypair.to_bytes()).into_string();
        let mut client =
            SpeedlandingClient::new("http://127.0.0.1:8899".to_string(), addr.to_string(), api_key)
                .await
                .expect("connect test speedlanding client");
        client.send_timeout = send_timeout;
        client.connect_timeout = Duration::from_secs(2);
        client
    }

    #[tokio::test]
    async fn retry_after_half_open_timeout_forces_reconnect_and_closes_stale_connection() {
        let stalled_cfg = make_server_config(0);
        let healthy_cfg = make_server_config(64);
        let server_endpoint =
            Endpoint::server((*healthy_cfg).clone(), "127.0.0.1:0".parse().unwrap())
                .expect("bind local QUIC server");
        let server_addr = server_endpoint.local_addr().expect("server local_addr");

        let accepted_count = Arc::new(AtomicUsize::new(0));
        let accepted_count_srv = accepted_count.clone();
        let (payload_tx, payload_rx) = oneshot::channel::<Vec<u8>>();

        let server_task = tokio::spawn(async move {
            let mut payload_tx = Some(payload_tx);
            let mut held_stalled_conn = None;
            while let Some(incoming) = server_endpoint.accept().await {
                let idx = accepted_count_srv.fetch_add(1, Ordering::SeqCst);
                let cfg = if idx == 0 { stalled_cfg.clone() } else { healthy_cfg.clone() };
                let Ok(connecting) = incoming.accept_with(cfg) else {
                    break;
                };
                let Ok(conn) = connecting.await else {
                    continue;
                };
                if idx == 0 {
                    // Keep the first connection alive with 0 uni-stream credit so `open_uni()`
                    // hangs with `close_reason().is_none()` (half-open/stalled QUIC).
                    held_stalled_conn = Some(conn);
                } else {
                    if let Ok(mut recv) = conn.accept_uni().await {
                        if let Ok(bytes) = recv.read_to_end(64 * 1024).await {
                            if let Some(tx) = payload_tx.take() {
                                let _ = tx.send(bytes);
                            }
                        }
                    }
                    // Keep second connection alive until client finishes assertions.
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    drop(held_stalled_conn);
                    break;
                }
            }
        });

        let client = make_test_client(server_addr, Duration::from_millis(150)).await;
        let stale_conn = client.connection.load_full();
        assert!(
            stale_conn.close_reason().is_none(),
            "initial connection must be open (simulating half-open QUIC)"
        );

        let tx = dummy_versioned_transaction();
        let expected_bytes = serialize_transaction_bincode_sync(&tx).unwrap().0.to_vec();

        client
            .send_transaction(TradeType::Buy, &tx, false)
            .await
            .expect("retry after half-open timeout must reconnect and succeed");

        let replacement_conn = client.connection.load_full();
        assert!(
            !Arc::ptr_eq(&stale_conn, &replacement_conn),
            "retry must replace the timed-out connection with a new Arc<Connection>"
        );
        assert_ne!(
            stale_conn.stable_id(),
            replacement_conn.stable_id(),
            "retry must open a distinct QUIC connection"
        );
        assert!(
            stale_conn.close_reason().is_some(),
            "stale connection must be explicitly closed during reconnect"
        );
        assert!(
            replacement_conn.close_reason().is_none(),
            "replacement connection must remain open"
        );
        assert_eq!(accepted_count.load(Ordering::SeqCst), 2);

        let received_bytes = timeout(Duration::from_secs(2), payload_rx)
            .await
            .expect("server must receive payload on replacement connection")
            .expect("payload channel open");
        assert_eq!(received_bytes, expected_bytes);

        server_task.abort();
    }

    #[tokio::test]
    async fn concurrent_reconnect_if_stale_deduplicates_via_ptr_eq() {
        let healthy_cfg = make_server_config(64);
        let server_endpoint =
            Endpoint::server((*healthy_cfg).clone(), "127.0.0.1:0".parse().unwrap())
                .expect("bind local QUIC server");
        let server_addr = server_endpoint.local_addr().expect("server local_addr");

        let accepted_count = Arc::new(AtomicUsize::new(0));
        let accepted_count_srv = accepted_count.clone();
        let server_task = tokio::spawn(async move {
            let mut conns = Vec::new();
            while let Some(incoming) = server_endpoint.accept().await {
                accepted_count_srv.fetch_add(1, Ordering::SeqCst);
                if let Ok(conn) = incoming.await {
                    conns.push(conn);
                }
            }
        });

        let client = Arc::new(make_test_client(server_addr, Duration::from_millis(200)).await);
        let stale_conn = client.connection.load_full();
        assert_eq!(accepted_count.load(Ordering::SeqCst), 1);

        let c1 = client.clone();
        let c2 = client.clone();
        let s1 = stale_conn.clone();
        let s2 = stale_conn.clone();
        let (r1, r2) =
            tokio::join!(async move { c1.reconnect_if_stale(&s1).await.unwrap() }, async move {
                c2.reconnect_if_stale(&s2).await.unwrap()
            },);

        assert!(stale_conn.close_reason().is_some(), "stale connection must be closed");
        assert!(!Arc::ptr_eq(&stale_conn, &r1));
        assert!(
            Arc::ptr_eq(&r1, &r2),
            "second concurrent caller must reuse the new connection via !Arc::ptr_eq"
        );
        assert_eq!(
            accepted_count.load(Ordering::SeqCst),
            2,
            "concurrent stale reconnects must open only one replacement connection"
        );

        server_task.abort();
    }
}
