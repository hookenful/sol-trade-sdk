//! RPC-counter unit tests verifying collapsed/batched RPC behavior for
//! LaunchLab (Bonk/StonkFun), Raydium CPMM, Meteora DBC/DAMM v2, PumpSwap, and PumpFun.

use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex,
};

use async_trait::async_trait;
use base64::Engine;
use serde_json::{json, Value};
use solana_client::client_error::{ClientError, ClientErrorKind, Result as ClientResult};
use solana_client::rpc_client::RpcClientConfig;
use solana_client::rpc_request::RpcRequest;
use solana_client::rpc_sender::{RpcSender, RpcTransportStats};
use solana_commitment_config::CommitmentConfig;
use solana_sdk::pubkey::Pubkey;

use crate::common::SolanaRpcClient;
use crate::constants::{TOKEN_PROGRAM, TOKEN_PROGRAM_2022, WSOL_TOKEN_ACCOUNT};
use crate::instruction::utils::bonk::accounts::BONK;
use crate::instruction::utils::bonk_types::{
    PoolState as BonkPoolState, POOL_STATE_DISCRIMINATOR as BONK_POOL_DISCRIMINATOR,
};
use crate::instruction::utils::meteora_damm_v2_types::{
    BaseFeeStruct as DammBaseFee, Pool as DammPool, PoolFeesStruct as DammPoolFees,
};
use crate::instruction::utils::meteora_dbc::{get_extra_account_metas_address, ExtraAccountMeta};
use crate::instruction::utils::meteora_dbc_types::{
    DbcBaseFee, DbcConfig, DbcCurvePoint, DbcPool, ACTIVATION_SLOT,
    CONFIG_WITH_TRANSFER_HOOK_DISCRIMINATOR, TRANSFER_HOOK_POOL_DISCRIMINATOR,
};
use crate::instruction::utils::pumpfun::{
    accounts as pf_accounts, get_bonding_curve_pda, get_creator_vault_pda,
    get_fee_sharing_config_pda, global_constants as pf_global, GLOBAL_ACCOUNT_DISCRIMINATOR,
    SHARING_CONFIG_ACCOUNT_DISCRIMINATOR,
};
use crate::instruction::utils::pumpswap::{
    accounts as ps_accounts, find_by_mint, get_canonical_pool_pda, get_pool_v2_pda,
};
use crate::instruction::utils::pumpswap_types::{
    Pool as PumpSwapPool, POOL_DISCRIMINATOR as PUMPSWAP_POOL_DISCRIMINATOR,
    POOL_SIZE as PUMPSWAP_POOL_SIZE,
};
use crate::instruction::utils::raydium_cpmm::accounts::RAYDIUM_CPMM;
use crate::instruction::utils::raydium_cpmm_types::{
    AmmConfig, PoolState as CpmmPoolState, AMM_CONFIG_DISCRIMINATOR,
    POOL_STATE_DISCRIMINATOR as CPMM_POOL_DISCRIMINATOR,
};
use crate::trading::core::params::{
    BonkParams, MeteoraDammV2Params, MeteoraDbcParams, PumpFunImmutableMeta, PumpFunParams,
    PumpSwapParams, RaydiumCpmmParams, TokenTransferFee, CLOCK_SYSVAR,
};
use spl_token_2022_interface::{
    extension::{
        transfer_fee::{TransferFee, TransferFeeConfig},
        BaseStateWithExtensionsMut, ExtensionType, StateWithExtensionsMut,
    },
    state::Mint as Token2022Mint,
};

static NEXT_RPC_ID: AtomicU64 = AtomicU64::new(1);

type RpcRequestLog = Arc<Mutex<Vec<(RpcRequest, Vec<Pubkey>)>>>;

#[derive(Clone)]
struct CountingRpc {
    url: String,
    slot: u64,
    epoch: u64,
    accounts: HashMap<Pubkey, (Pubkey, Vec<u8>)>,
    requests: RpcRequestLog,
}

impl CountingRpc {
    fn new(slot: u64) -> Self {
        let id = NEXT_RPC_ID.fetch_add(1, Ordering::Relaxed);
        Self {
            url: format!("counting-rpc://{id}"),
            slot,
            epoch: 0,
            accounts: HashMap::new(),
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn epoch(mut self, epoch: u64) -> Self {
        self.epoch = epoch;
        self
    }

    fn account(mut self, key: Pubkey, owner: Pubkey, data: Vec<u8>) -> Self {
        self.accounts.insert(key, (owner, data));
        self
    }

    fn requests(&self) -> RpcRequestLog {
        self.requests.clone()
    }

    fn client(self) -> SolanaRpcClient {
        SolanaRpcClient::new_sender(
            self,
            RpcClientConfig::with_commitment(CommitmentConfig::confirmed()),
        )
    }
}

fn account_json(owner: &Pubkey, data: &[u8]) -> Value {
    json!({
        "lamports": 1_000_000,
        "data": [base64::engine::general_purpose::STANDARD.encode(data), "base64"],
        "owner": owner.to_string(),
        "executable": false,
        "rentEpoch": 0,
        "space": data.len(),
    })
}

fn rpc_err(msg: String) -> ClientError {
    ClientErrorKind::Custom(msg).into()
}

#[async_trait]
impl RpcSender for CountingRpc {
    async fn send(&self, request: RpcRequest, params: Value) -> ClientResult<Value> {
        let keys: Vec<Pubkey> = match request {
            RpcRequest::GetAccountInfo => {
                params[0].as_str().and_then(|s| s.parse::<Pubkey>().ok()).into_iter().collect()
            }
            RpcRequest::GetMultipleAccounts => params[0]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|v| v.as_str().and_then(|s| s.parse::<Pubkey>().ok()))
                .collect(),
            _ => Vec::new(),
        };
        self.requests.lock().unwrap().push((request, keys.clone()));
        let context = json!({ "slot": self.slot });
        match request {
            RpcRequest::GetAccountInfo => {
                let pubkey = keys.first().ok_or_else(|| rpc_err("missing key".into()))?;
                let value =
                    self.accounts.get(pubkey).map(|(owner, data)| account_json(owner, data));
                Ok(json!({ "context": context, "value": value }))
            }
            RpcRequest::GetMultipleAccounts => {
                let values: Vec<Value> = keys
                    .iter()
                    .map(|key| match self.accounts.get(key) {
                        Some((owner, data)) => account_json(owner, data),
                        None => Value::Null,
                    })
                    .collect();
                Ok(json!({ "context": context, "value": values }))
            }
            RpcRequest::GetEpochInfo => Ok(json!({
                "absoluteSlot": self.slot,
                "blockHeight": self.slot,
                "epoch": self.epoch,
                "slotIndex": 0,
                "slotsInEpoch": 432_000,
                "transactionCount": 0,
            })),
            other => Err(rpc_err(format!("unexpected RPC request: {other:?}"))),
        }
    }

    fn get_transport_stats(&self) -> RpcTransportStats {
        RpcTransportStats::default()
    }

    fn url(&self) -> String {
        self.url.clone()
    }
}

fn spl_mint_bytes(supply: u64) -> Vec<u8> {
    let mut data = vec![0u8; 82];
    data[36..44].copy_from_slice(&supply.to_le_bytes());
    data[45] = 1; // initialized
    data
}

fn spl_token_account_bytes(mint: Pubkey, amount: u64) -> Vec<u8> {
    let mut data = vec![0u8; 165];
    data[..32].copy_from_slice(mint.as_ref());
    data[64..72].copy_from_slice(&amount.to_le_bytes());
    data[108] = 1; // initialized
    data
}

fn bonk_global_config_bytes(trade_fee_rate: u64) -> Vec<u8> {
    let mut data = vec![0u8; 64];
    data[..8].copy_from_slice(&[149, 8, 156, 202, 160, 252, 176, 217]);
    data[16] = 0; // curve_type = 0
    data[27..35].copy_from_slice(&trade_fee_rate.to_le_bytes());
    data
}

fn bonk_platform_config_bytes(platform_fee_rate: u64, creator_fee_rate: u64) -> Vec<u8> {
    let mut data = vec![0u8; 736];
    data[..8].copy_from_slice(&[160, 78, 128, 0, 248, 83, 230, 160]);
    data[104..112].copy_from_slice(&platform_fee_rate.to_le_bytes());
    data[720..728].copy_from_slice(&creator_fee_rate.to_le_bytes());
    data
}

fn encode_bonk_pool(state: &BonkPoolState) -> Vec<u8> {
    let mut data = Vec::with_capacity(8 + crate::instruction::utils::bonk_types::POOL_STATE_SIZE);
    data.extend_from_slice(&BONK_POOL_DISCRIMINATOR);
    data.extend_from_slice(&state.epoch.to_le_bytes());
    data.push(state.auth_bump);
    data.push(state.status);
    data.push(state.base_decimals);
    data.push(state.quote_decimals);
    data.push(state.migrate_type);
    for value in [
        state.supply,
        state.total_base_sell,
        state.virtual_base,
        state.virtual_quote,
        state.real_base,
        state.real_quote,
        state.total_quote_fund_raising,
        state.quote_protocol_fee,
        state.platform_fee,
        state.migrate_fee,
        state.vesting_schedule.total_locked_amount,
        state.vesting_schedule.cliff_period,
        state.vesting_schedule.unlock_period,
        state.vesting_schedule.start_time,
        state.vesting_schedule.allocated_share_amount,
    ] {
        data.extend_from_slice(&value.to_le_bytes());
    }
    for key in [
        state.global_config,
        state.platform_config,
        state.base_mint,
        state.quote_mint,
        state.base_vault,
        state.quote_vault,
        state.creator,
    ] {
        data.extend_from_slice(key.as_ref());
    }
    for pad in state.padding {
        data.extend_from_slice(&pad.to_le_bytes());
    }
    data
}

fn encode_cpmm_pool(state: &CpmmPoolState) -> Vec<u8> {
    let mut data =
        Vec::with_capacity(8 + crate::instruction::utils::raydium_cpmm_types::POOL_STATE_SIZE);
    data.extend_from_slice(&CPMM_POOL_DISCRIMINATOR);
    for key in [
        state.amm_config,
        state.pool_creator,
        state.token0_vault,
        state.token1_vault,
        state.lp_mint,
        state.token0_mint,
        state.token1_mint,
        state.token0_program,
        state.token1_program,
        state.observation_key,
    ] {
        data.extend_from_slice(key.as_ref());
    }
    data.push(state.auth_bump);
    data.push(state.status);
    data.push(state.lp_mint_decimals);
    data.push(state.mint0_decimals);
    data.push(state.mint1_decimals);
    for value in [
        state.lp_supply,
        state.protocol_fees_token0,
        state.protocol_fees_token1,
        state.fund_fees_token0,
        state.fund_fees_token1,
        state.open_time,
        state.recent_epoch,
    ] {
        data.extend_from_slice(&value.to_le_bytes());
    }
    data.push(state.creator_fee_on);
    data.push(u8::from(state.enable_creator_fee));
    data.extend_from_slice(&state.padding1);
    data.extend_from_slice(&state.creator_fees_token0.to_le_bytes());
    data.extend_from_slice(&state.creator_fees_token1.to_le_bytes());
    for pad in state.padding {
        data.extend_from_slice(&pad.to_le_bytes());
    }
    data
}

fn encode_amm_config(config: &AmmConfig) -> Vec<u8> {
    let mut data =
        Vec::with_capacity(8 + crate::instruction::utils::raydium_cpmm_types::AMM_CONFIG_SIZE);
    data.extend_from_slice(&AMM_CONFIG_DISCRIMINATOR);
    data.push(config.bump);
    data.push(u8::from(config.disable_create_pool));
    data.extend_from_slice(&config.index.to_le_bytes());
    data.extend_from_slice(&config.trade_fee_rate.to_le_bytes());
    data.extend_from_slice(&config.protocol_fee_rate.to_le_bytes());
    data.extend_from_slice(&config.fund_fee_rate.to_le_bytes());
    data.extend_from_slice(&config.create_pool_fee.to_le_bytes());
    data.extend_from_slice(config.protocol_owner.as_ref());
    data.extend_from_slice(config.fund_owner.as_ref());
    data.extend_from_slice(&config.creator_fee_rate.to_le_bytes());
    data.extend_from_slice(&config.creator_fee_share_rate.to_le_bytes());
    for pad in config.padding {
        data.extend_from_slice(&pad.to_le_bytes());
    }
    data
}

#[tokio::test]
async fn bonk_from_pool_state_uses_single_batched_rpc_and_skips_epoch_for_spl() {
    let pool_key = Pubkey::new_unique();
    let base_mint = Pubkey::new_unique();
    let quote_mint = Pubkey::new_unique();
    let global_config = Pubkey::new_unique();
    let platform_config = Pubkey::new_unique();
    let state = BonkPoolState {
        global_config,
        platform_config,
        base_mint,
        quote_mint,
        base_vault: Pubkey::new_unique(),
        quote_vault: Pubkey::new_unique(),
        creator: Pubkey::new_unique(),
        virtual_base: 1_000_000_000,
        virtual_quote: 30_000_000_000,
        real_base: 100_000,
        real_quote: 500_000,
        ..BonkPoolState::default()
    };

    let rpc_builder = CountingRpc::new(500)
        .account(pool_key, BONK, encode_bonk_pool(&state))
        .account(base_mint, TOKEN_PROGRAM, spl_mint_bytes(1_000_000))
        .account(quote_mint, TOKEN_PROGRAM, spl_mint_bytes(1_000_000))
        .account(global_config, BONK, bonk_global_config_bytes(2_500))
        .account(platform_config, BONK, bonk_platform_config_bytes(10_000, 1_000));
    let requests = rpc_builder.requests();
    let rpc = rpc_builder.client();

    let params = BonkParams::from_pool_state(&rpc, &pool_key, &state).await.unwrap();
    assert_eq!(params.trade_fee_rate, 2_500);
    assert_eq!(params.platform_fee_rate, 10_000);
    assert_eq!(params.creator_fee_rate, 1_000);
    assert_eq!(params.mint_token_program, TOKEN_PROGRAM);
    assert_eq!(params.quote_token_program, TOKEN_PROGRAM);

    let recorded = requests.lock().unwrap().clone();
    assert_eq!(recorded.len(), 1, "from_pool_state must make exactly 1 batched RPC");
    assert_eq!(recorded[0].0, RpcRequest::GetMultipleAccounts);

    requests.lock().unwrap().clear();
    let params_full = BonkParams::from_pool_by_rpc(&rpc, &pool_key).await.unwrap();
    assert_eq!(params_full.pool_state, pool_key);
    let recorded_full = requests.lock().unwrap().clone();
    assert_eq!(recorded_full.len(), 2, "from_pool_by_rpc must make 2 RPCs (pool + batch)");
    assert_eq!(recorded_full[0].0, RpcRequest::GetAccountInfo);
    assert_eq!(recorded_full[1].0, RpcRequest::GetMultipleAccounts);
}

#[tokio::test]
async fn raydium_cpmm_from_pool_state_uses_single_batched_rpc_and_skips_epoch_for_spl() {
    let pool_key = Pubkey::new_unique();
    let amm_config_key = Pubkey::new_unique();
    let token0_mint = Pubkey::new_unique();
    let token1_mint = Pubkey::new_unique();
    let token0_vault = Pubkey::new_unique();
    let token1_vault = Pubkey::new_unique();
    let pool = CpmmPoolState {
        amm_config: amm_config_key,
        token0_vault,
        token1_vault,
        token0_mint,
        token1_mint,
        token0_program: TOKEN_PROGRAM,
        token1_program: TOKEN_PROGRAM,
        observation_key: Pubkey::new_unique(),
        protocol_fees_token0: 10,
        fund_fees_token0: 5,
        creator_fees_token0: 2,
        protocol_fees_token1: 20,
        fund_fees_token1: 3,
        creator_fees_token1: 1,
        ..CpmmPoolState::default()
    };
    let amm_config_data = encode_amm_config(&AmmConfig {
        trade_fee_rate: 2_500,
        protocol_fee_rate: 120_000,
        fund_fee_rate: 40_000,
        creator_fee_rate: 1_000,
        ..AmmConfig::default()
    });
    let pool_data = encode_cpmm_pool(&pool);

    // Intentionally omit CLOCK_SYSVAR to prove SPL mints never call get_epoch_info.
    let rpc_builder = CountingRpc::new(600)
        .account(pool_key, RAYDIUM_CPMM, pool_data)
        .account(amm_config_key, RAYDIUM_CPMM, amm_config_data)
        .account(token0_vault, TOKEN_PROGRAM, spl_token_account_bytes(token0_mint, 1_000))
        .account(token1_vault, TOKEN_PROGRAM, spl_token_account_bytes(token1_mint, 2_000))
        .account(token0_mint, TOKEN_PROGRAM, spl_mint_bytes(10_000))
        .account(token1_mint, TOKEN_PROGRAM, spl_mint_bytes(10_000));
    let requests = rpc_builder.requests();
    let rpc = rpc_builder.client();

    // Pass stale fee counters in the caller's PoolState to prove from_pool_state
    // reads the pool atomically alongside both vaults in the same single RPC slot
    // (avoiding cross-slot fee/vault torn reads).
    let mut stale_pool = pool.clone();
    stale_pool.protocol_fees_token0 = 999;
    stale_pool.protocol_fees_token1 = 1_999;

    let params = RaydiumCpmmParams::from_pool_state(&rpc, &pool_key, &stale_pool).await.unwrap();
    assert_eq!(params.base_reserve, 1_000 - 17);
    assert_eq!(params.quote_reserve, 2_000 - 24);
    assert_eq!(params.trade_fee_rate, 2_500);

    let recorded = requests.lock().unwrap().clone();
    assert_eq!(recorded.len(), 1, "from_pool_state must make 1 batched RPC without get_epoch_info");
    assert_eq!(recorded[0].0, RpcRequest::GetMultipleAccounts);
    assert!(
        recorded[0].1.contains(&pool_key),
        "from_pool_state must read pool alongside vaults in the same RPC slot"
    );

    requests.lock().unwrap().clear();
    let params_full = RaydiumCpmmParams::from_pool_address_by_rpc(&rpc, &pool_key).await.unwrap();
    assert_eq!(params_full.base_reserve, params.base_reserve);
    assert_eq!(requests.lock().unwrap().len(), 2);
}

fn encode_dbc_config_with_hook(config: &DbcConfig, hook_program: Pubkey) -> Vec<u8> {
    let mut data = vec![0u8; 8 + 1040 + 32];
    data[..8].copy_from_slice(&CONFIG_WITH_TRANSFER_HOOK_DISCRIMINATOR);
    let body = &mut data[8..8 + 1040];
    body[0..32].copy_from_slice(config.quote_mint.as_ref());
    body[96..104].copy_from_slice(&config.base_fee.cliff_fee_numerator.to_le_bytes());
    body[104..112].copy_from_slice(&config.base_fee.second_factor.to_le_bytes());
    body[112..120].copy_from_slice(&config.base_fee.third_factor.to_le_bytes());
    body[120..122].copy_from_slice(&config.base_fee.first_factor.to_le_bytes());
    body[122] = config.base_fee.base_fee_mode;
    body[226] = config.activation_type;
    body[384..400].copy_from_slice(&config.sqrt_start_price.to_le_bytes());
    body[400..416].copy_from_slice(&u128::MAX.to_le_bytes());
    body[416..432].copy_from_slice(&1_000_000_000u128.to_le_bytes());
    data[8 + 1040..8 + 1040 + 32].copy_from_slice(hook_program.as_ref());
    data
}

fn encode_dbc_hook_pool(pool: &DbcPool) -> Vec<u8> {
    let mut data = vec![0u8; 8 + 416];
    data[..8].copy_from_slice(&TRANSFER_HOOK_POOL_DISCRIMINATOR);
    let body = &mut data[8..];
    body[64..96].copy_from_slice(pool.config.as_ref());
    body[128..160].copy_from_slice(pool.base_mint.as_ref());
    body[160..192].copy_from_slice(pool.base_vault.as_ref());
    body[192..224].copy_from_slice(pool.quote_vault.as_ref());
    body[272..288].copy_from_slice(&pool.sqrt_price.to_le_bytes());
    body[288..296].copy_from_slice(&pool.activation_point.to_le_bytes());
    data
}

fn encode_extra_account_metas(metas: &[ExtraAccountMeta]) -> Vec<u8> {
    let mut value = Vec::new();
    value.extend_from_slice(&(metas.len() as u32).to_le_bytes());
    for m in metas {
        value.push(m.discriminator);
        value.extend_from_slice(&m.address_config);
        value.push(u8::from(m.is_signer));
        value.push(u8::from(m.is_writable));
    }
    let mut out = Vec::new();
    out.extend_from_slice(&[105, 37, 101, 197, 75, 251, 102, 26]); // EXECUTE_DISCRIMINATOR
    out.extend_from_slice(&(value.len() as u32).to_le_bytes());
    out.extend_from_slice(&value);
    out
}

#[tokio::test]
async fn meteora_dbc_uses_context_slot_and_batches_or_reuses_hook_metas() {
    let pool_key = Pubkey::new_unique();
    let config_key = Pubkey::new_unique();
    let base_mint = Pubkey::new_unique();
    let quote_mint = WSOL_TOKEN_ACCOUNT;
    let hook_program = Pubkey::new_unique();
    let validation = get_extra_account_metas_address(&base_mint, &hook_program);
    let fixed_extra = Pubkey::new_unique();
    let expected_meta = ExtraAccountMeta {
        discriminator: 0,
        address_config: fixed_extra.to_bytes(),
        is_signer: false,
        is_writable: false,
    };

    let config = Arc::new(DbcConfig {
        quote_mint,
        base_fee: DbcBaseFee {
            cliff_fee_numerator: 20_000_000, // 2%
            first_factor: 10,                // 10 periods
            second_factor: 10,               // 10 slots per period -> triggers slot read
            third_factor: 1_000_000,         // linear reduction per period
            base_fee_mode: 0,
        },
        activation_type: ACTIVATION_SLOT,
        sqrt_start_price: 1u128 << 64,
        transfer_hook_program: Some(hook_program),
        curve: vec![DbcCurvePoint { sqrt_price: u128::MAX, liquidity: 1_000_000_000 }],
        ..DbcConfig::default()
    });
    let pool = DbcPool {
        config: config_key,
        base_mint,
        base_vault: Pubkey::new_unique(),
        quote_vault: Pubkey::new_unique(),
        sqrt_price: 1u128 << 64,
        activation_point: 1_000,
        transfer_hook: true,
        ..DbcPool::default()
    };

    // Slot 1050 = 5 periods past activation -> fee drops from 20M to 15M.
    // CountingRpc rejects GetSlot!
    let rpc_builder = CountingRpc::new(1_050)
        .account(
            pool_key,
            crate::instruction::utils::meteora_dbc::accounts::METEORA_DBC,
            encode_dbc_hook_pool(&pool),
        )
        .account(
            config_key,
            crate::instruction::utils::meteora_dbc::accounts::METEORA_DBC,
            encode_dbc_config_with_hook(&config, hook_program),
        )
        .account(base_mint, TOKEN_PROGRAM_2022, spl_mint_bytes(1_000_000))
        .account(quote_mint, TOKEN_PROGRAM, spl_mint_bytes(1_000_000))
        .account(validation, hook_program, encode_extra_account_metas(&[expected_meta]));
    let requests = rpc_builder.requests();
    let rpc = rpc_builder.client();

    // 1. With cached config + cached hook metas: 1 single GetMultipleAccounts of [base, quote], 0 GetSlot.
    let params_cached = MeteoraDbcParams::from_pool_with_cached_by_rpc(
        &rpc,
        &pool_key,
        &pool,
        Some(config.clone()),
        Some(vec![expected_meta]),
    )
    .await
    .unwrap();
    assert_eq!(params_cached.quote.as_ref().unwrap().fee_numerator, 15_000_000);
    {
        let reqs = requests.lock().unwrap();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].0, RpcRequest::GetMultipleAccounts);
        assert_eq!(reqs[0].1, vec![base_mint, quote_mint]);
    }

    // 2. Without cached hook metas: still 1 single GetMultipleAccounts of [base, quote, validation], 0 GetSlot.
    requests.lock().unwrap().clear();
    let params_batched =
        MeteoraDbcParams::from_pool_with_cached_by_rpc(&rpc, &pool_key, &pool, Some(config), None)
            .await
            .unwrap();
    assert_eq!(params_batched.quote.as_ref().unwrap().fee_numerator, 15_000_000);
    assert_eq!(
        params_batched.transfer_hook,
        Some(crate::trading::core::params::DbcTransferHook::Metas {
            program: hook_program,
            metas: vec![expected_meta],
        })
    );
    {
        let reqs = requests.lock().unwrap();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].0, RpcRequest::GetMultipleAccounts);
        assert_eq!(reqs[0].1, vec![base_mint, quote_mint, validation]);
    }
}

#[tokio::test]
async fn meteora_damm_v2_uses_context_slot_without_get_slot() {
    let pool_key = Pubkey::new_unique();
    let token_a = Pubkey::new_unique();
    let token_b = WSOL_TOKEN_ACCOUNT;
    let pool = DammPool {
        token_a_mint: token_a,
        token_b_mint: token_b,
        token_a_vault: Pubkey::new_unique(),
        token_b_vault: Pubkey::new_unique(),
        activation_type: 0, // slot-based
        activation_point: 100,
        sqrt_price: 1u128 << 64,
        liquidity: 1_000_000,
        pool_fees: DammPoolFees {
            base_fee: DammBaseFee {
                cliff_fee_numerator: 10_000_000,
                fee_scheduler_mode: 0,
                number_of_period: 5,
                period_frequency: 10,
                reduction_factor: 1_000_000,
                ..DammBaseFee::default()
            },
            ..DammPoolFees::default()
        },
        ..DammPool::default()
    };

    // CountingRpc rejects GetSlot! Slot 130 => 3 periods elapsed => fee drops from 10M to 7M.
    let rpc_builder = CountingRpc::new(130)
        .account(token_a, TOKEN_PROGRAM, spl_mint_bytes(1_000_000))
        .account(token_b, TOKEN_PROGRAM, spl_mint_bytes(1_000_000));
    let requests = rpc_builder.requests();
    let rpc = rpc_builder.client();

    let params = MeteoraDammV2Params::from_pool_by_rpc(&rpc, &pool_key, &pool).await.unwrap();
    assert_eq!(params.quote.as_ref().unwrap().fee_numerator, 7_000_000);
    let reqs = requests.lock().unwrap();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].0, RpcRequest::GetMultipleAccounts);
}

fn encode_pumpswap_pool(pool: &PumpSwapPool) -> Vec<u8> {
    let mut data = Vec::with_capacity(8 + PUMPSWAP_POOL_SIZE);
    data.extend_from_slice(&PUMPSWAP_POOL_DISCRIMINATOR);
    data.push(pool.pool_bump);
    data.extend_from_slice(&pool.index.to_le_bytes());
    data.extend_from_slice(pool.creator.as_ref());
    data.extend_from_slice(pool.base_mint.as_ref());
    data.extend_from_slice(pool.quote_mint.as_ref());
    data.extend_from_slice(pool.lp_mint.as_ref());
    data.extend_from_slice(pool.pool_base_token_account.as_ref());
    data.extend_from_slice(pool.pool_quote_token_account.as_ref());
    data.extend_from_slice(&pool.lp_supply.to_le_bytes());
    data.extend_from_slice(pool.coin_creator.as_ref());
    data.push(u8::from(pool.is_mayhem_mode));
    data.push(u8::from(pool.is_cashback_coin));
    data.extend_from_slice(&pool.virtual_quote_reserves.to_le_bytes());
    data
}

#[tokio::test]
async fn pumpswap_find_by_mint_batches_v2_and_canonical_in_single_rpc() {
    let mint = Pubkey::new_unique();
    let v2_pda = get_pool_v2_pda(&mint).unwrap();
    let canonical_pda = get_canonical_pool_pda(&mint);
    let pool = PumpSwapPool {
        base_mint: mint,
        quote_mint: WSOL_TOKEN_ACCOUNT,
        pool_base_token_account: Pubkey::new_unique(),
        pool_quote_token_account: Pubkey::new_unique(),
        coin_creator: Pubkey::new_unique(),
        lp_supply: 500,
        ..PumpSwapPool::default()
    };

    // Only canonical exists: must resolve in 1 GetMultipleAccounts RPC instead of 2 sequential GetAccounts.
    let rpc_builder = CountingRpc::new(700).account(
        canonical_pda,
        ps_accounts::AMM_PROGRAM,
        encode_pumpswap_pool(&pool),
    );
    let requests = rpc_builder.requests();
    let rpc = rpc_builder.client();

    let (found_addr, found_pool) = find_by_mint(&rpc, &mint).await.unwrap();
    assert_eq!(found_addr, canonical_pda);
    assert_eq!(found_pool.base_mint, mint);
    {
        let reqs = requests.lock().unwrap();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].0, RpcRequest::GetMultipleAccounts);
        assert_eq!(reqs[0].1, vec![v2_pda, canonical_pda]);
    }

    // When both exist, v2 wins in 1 RPC.
    let rpc_both = CountingRpc::new(701)
        .account(v2_pda, ps_accounts::AMM_PROGRAM, encode_pumpswap_pool(&pool))
        .account(canonical_pda, ps_accounts::AMM_PROGRAM, encode_pumpswap_pool(&pool));
    let reqs_both = rpc_both.requests();
    let (winner, _) = find_by_mint(&rpc_both.client(), &mint).await.unwrap();
    assert_eq!(winner, v2_pda);
    assert_eq!(reqs_both.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn pumpswap_known_pool_addresses_loads_snapshot_in_single_batch_rpc() {
    let pool_key = Pubkey::new_unique();
    let base_mint = Pubkey::new_unique();
    let quote_mint = WSOL_TOKEN_ACCOUNT;
    let base_vault = Pubkey::new_unique();
    let quote_vault = Pubkey::new_unique();
    let pool = PumpSwapPool {
        base_mint,
        quote_mint,
        pool_base_token_account: base_vault,
        pool_quote_token_account: quote_vault,
        coin_creator: Pubkey::new_unique(),
        virtual_quote_reserves: -100,
        ..PumpSwapPool::default()
    };

    let rpc_builder = CountingRpc::new(702)
        .account(pool_key, ps_accounts::AMM_PROGRAM, encode_pumpswap_pool(&pool))
        .account(base_vault, TOKEN_PROGRAM, spl_token_account_bytes(base_mint, 5_000))
        .account(quote_vault, TOKEN_PROGRAM, spl_token_account_bytes(quote_mint, 1_100))
        .account(base_mint, TOKEN_PROGRAM, spl_mint_bytes(10_000));
    let requests = rpc_builder.requests();
    let rpc = rpc_builder.client();

    let params = PumpSwapParams::from_known_pool_addresses_by_rpc(
        &rpc,
        &pool_key,
        &base_mint,
        &quote_mint,
        &base_vault,
        &quote_vault,
    )
    .await
    .unwrap();
    assert_eq!(params.pool, pool_key);
    assert_eq!(params.pool_base_token_reserves, 5_000);
    assert_eq!(params.effective_quote_reserves().unwrap(), 1_000);

    let reqs = requests.lock().unwrap();
    let multi_calls: Vec<_> =
        reqs.iter().filter(|(req, _)| *req == RpcRequest::GetMultipleAccounts).collect();
    assert_eq!(
        multi_calls.len(),
        1,
        "must fetch [pool, base_vault, quote_vault, base_mint] in 1 batch"
    );
    assert_eq!(multi_calls[0].1, vec![pool_key, base_vault, quote_vault, base_mint]);
}

fn encode_pumpfun_bonding_curve(
    creator: Pubkey,
    virtual_token: u64,
    virtual_sol: u64,
    complete: bool,
    is_mayhem: bool,
    is_cashback: bool,
) -> Vec<u8> {
    let mut data = vec![0u8; 8 + 75];
    data[..8].copy_from_slice(&[23, 183, 248, 55, 96, 216, 172, 96]);
    data[8..16].copy_from_slice(&virtual_token.to_le_bytes());
    data[16..24].copy_from_slice(&virtual_sol.to_le_bytes());
    data[24..32].copy_from_slice(&virtual_token.to_le_bytes());
    data[32..40].copy_from_slice(&virtual_sol.to_le_bytes());
    data[40..48].copy_from_slice(&1_000_000_000_000_000u64.to_le_bytes());
    data[48] = u8::from(complete);
    data[49..81].copy_from_slice(creator.as_ref());
    data[81] = u8::from(is_mayhem);
    data[82] = u8::from(is_cashback);
    data
}

fn encode_pumpfun_global(fee_recipient: Pubkey) -> Vec<u8> {
    let mut data = vec![0u8; 73];
    data[..8].copy_from_slice(&GLOBAL_ACCOUNT_DISCRIMINATOR);
    data[8] = 1;
    data[41..73].copy_from_slice(fee_recipient.as_ref());
    data
}

fn encode_pumpfun_sharing_config(mint: Pubkey) -> Vec<u8> {
    let mut data = vec![0u8; 43];
    data[..8].copy_from_slice(&SHARING_CONFIG_ACCOUNT_DISCRIMINATOR);
    data[10] = 1; // ACTIVE
    data[11..43].copy_from_slice(mint.as_ref());
    data
}

#[tokio::test]
async fn pumpfun_from_mint_batches_into_one_rpc_and_cached_meta_skips_mint_and_global() {
    let mint = Pubkey::new_unique();
    let creator = Pubkey::new_unique();
    let global_fee = Pubkey::new_unique();
    let curve_pda = get_bonding_curve_pda(&mint).unwrap();
    let sharing_pda = get_fee_sharing_config_pda(&mint).unwrap();

    let rpc_builder = CountingRpc::new(800)
        .account(
            curve_pda,
            pf_accounts::PUMPFUN,
            encode_pumpfun_bonding_curve(creator, 900_000, 40_000, false, false, true),
        )
        .account(mint, TOKEN_PROGRAM_2022, spl_mint_bytes(1_000_000))
        .account(
            pf_global::GLOBAL_ACCOUNT,
            pf_accounts::PUMPFUN,
            encode_pumpfun_global(global_fee),
        );
    let requests = rpc_builder.requests();
    let rpc = rpc_builder.client();

    // First call: 1 single GetMultipleAccounts (down from 4 separate getAccount calls).
    let params = PumpFunParams::from_mint_by_rpc(&rpc, &mint).await.unwrap();
    assert_eq!(params.token_program, TOKEN_PROGRAM_2022);
    assert_eq!(params.fee_recipient, global_fee);
    assert!(params.bonding_curve.is_cashback_coin);
    assert_eq!(params.creator_vault, get_creator_vault_pda(&creator).unwrap());
    {
        let reqs = requests.lock().unwrap();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].0, RpcRequest::GetMultipleAccounts);
        assert!(reqs[0].1.contains(&pf_global::GLOBAL_ACCOUNT));
    }

    // Second call on same RPC hits TTL cache for GLOBAL_ACCOUNT (not re-requested).
    requests.lock().unwrap().clear();
    let params2 = PumpFunParams::from_mint_by_rpc(&rpc, &mint).await.unwrap();
    assert_eq!(params2.fee_recipient, global_fee);
    {
        let reqs = requests.lock().unwrap();
        assert_eq!(reqs.len(), 1);
        assert!(!reqs[0].1.contains(&pf_global::GLOBAL_ACCOUNT));
    }

    // Cached-meta exit skips reading mint and global accounts completely.
    requests.lock().unwrap().clear();
    let meta = PumpFunImmutableMeta::from_params(&params);
    let cached_params = PumpFunParams::from_cached_meta_by_rpc(&rpc, &mint, &meta).await.unwrap();
    assert_eq!(cached_params.token_program, TOKEN_PROGRAM_2022);
    assert_eq!(cached_params.creator_vault, params.creator_vault);
    assert_eq!(cached_params.fee_recipient, global_fee);
    {
        let reqs = requests.lock().unwrap();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].0, RpcRequest::GetMultipleAccounts);
        assert_eq!(reqs[0].1, vec![curve_pda, sharing_pda]);
        assert!(!reqs[0].1.contains(&mint), "cached meta exit must not re-read mint");
        assert!(
            !reqs[0].1.contains(&pf_global::GLOBAL_ACCOUNT),
            "cached meta exit must not re-read global"
        );
    }

    // If fee sharing activates after buy, cached-meta exit detects it in the same 1 RPC!
    let rpc_sharing = CountingRpc::new(801)
        .account(
            curve_pda,
            pf_accounts::PUMPFUN,
            encode_pumpfun_bonding_curve(creator, 900_000, 40_000, false, false, true),
        )
        .account(sharing_pda, pf_accounts::FEE_PROGRAM, encode_pumpfun_sharing_config(mint));
    let reqs_sharing = rpc_sharing.requests();
    let upgraded =
        PumpFunParams::from_cached_meta_by_rpc(&rpc_sharing.client(), &mint, &meta).await.unwrap();
    let expected_sharing_vault = get_creator_vault_pda(&sharing_pda).unwrap();
    assert_eq!(upgraded.fee_sharing_creator_vault_if_active, Some(expected_sharing_vault));
    assert_eq!(upgraded.creator_vault, expected_sharing_vault);
    assert_eq!(reqs_sharing.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn pumpfun_cached_meta_honors_global_ttl_and_resolves_unknown_token_program_in_one_rpc() {
    let mint = Pubkey::new_unique();
    let creator = Pubkey::new_unique();
    let stale_fee = Pubkey::new_unique();
    let fresh_global_fee = Pubkey::new_unique();
    let curve_pda = get_bonding_curve_pda(&mint).unwrap();
    let sharing_pda = get_fee_sharing_config_pda(&mint).unwrap();

    // 1. Cold RPC with a rotated Global fee recipient must refresh Global in the same
    // single GetMultipleAccounts batch rather than serving stale `cached.fee_recipient`.
    let rpc_builder = CountingRpc::new(810)
        .account(
            curve_pda,
            pf_accounts::PUMPFUN,
            encode_pumpfun_bonding_curve(creator, 900_000, 40_000, false, false, false),
        )
        .account(
            pf_global::GLOBAL_ACCOUNT,
            pf_accounts::PUMPFUN,
            encode_pumpfun_global(fresh_global_fee),
        );
    let requests = rpc_builder.requests();
    let rpc = rpc_builder.client();

    let stale_meta = PumpFunImmutableMeta {
        token_program: TOKEN_PROGRAM,
        creator,
        creator_vault: get_creator_vault_pda(&creator).unwrap(),
        is_mayhem_mode: false,
        is_cashback_coin: false,
        quote_mint: WSOL_TOKEN_ACCOUNT,
        fee_sharing_creator_vault_if_active: None,
        fee_recipient: stale_fee,
    };

    let refreshed = PumpFunParams::from_cached_meta_by_rpc(&rpc, &mint, &stale_meta).await.unwrap();
    assert_eq!(
        refreshed.fee_recipient, fresh_global_fee,
        "cold global cache must refresh GLOBAL_ACCOUNT instead of pinning stale fee_recipient"
    );
    assert_eq!(refreshed.token_program, TOKEN_PROGRAM);
    {
        let reqs = requests.lock().unwrap();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].0, RpcRequest::GetMultipleAccounts);
        assert_eq!(reqs[0].1, vec![curve_pda, sharing_pda, pf_global::GLOBAL_ACCOUNT]);
    }

    // 2. Second call on the same RPC hits the 90s Global TTL cache and omits GLOBAL_ACCOUNT.
    requests.lock().unwrap().clear();
    let warm = PumpFunParams::from_cached_meta_by_rpc(&rpc, &mint, &stale_meta).await.unwrap();
    assert_eq!(warm.fee_recipient, fresh_global_fee);
    {
        let reqs = requests.lock().unwrap();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].1, vec![curve_pda, sharing_pda]);
    }

    // 3. Mayhem coins keep their cached mayhem fee recipient without reading GLOBAL_ACCOUNT.
    let mayhem_fee = pf_global::MAYHEM_FEE_RECIPIENTS[0];
    let rpc_mayhem = CountingRpc::new(811).account(
        curve_pda,
        pf_accounts::PUMPFUN,
        encode_pumpfun_bonding_curve(creator, 900_000, 40_000, false, true, false),
    );
    let mayhem_reqs = rpc_mayhem.requests();
    let mayhem_meta =
        PumpFunImmutableMeta { is_mayhem_mode: true, fee_recipient: mayhem_fee, ..stale_meta };
    let mayhem_params =
        PumpFunParams::from_cached_meta_by_rpc(&rpc_mayhem.client(), &mint, &mayhem_meta)
            .await
            .unwrap();
    assert_eq!(mayhem_params.fee_recipient, mayhem_fee);
    {
        let reqs = mayhem_reqs.lock().unwrap();
        assert_eq!(reqs.len(), 1);
        assert!(!reqs[0].1.contains(&pf_global::GLOBAL_ACCOUNT));
    }

    // 4. Unknown `token_program == Pubkey::default()` resolves a legacy SPL Token mint in the
    // same single GetMultipleAccounts batch without fabricating TOKEN_PROGRAM_2022.
    let legacy_mint = Pubkey::new_unique();
    let legacy_curve = get_bonding_curve_pda(&legacy_mint).unwrap();
    let legacy_sharing = get_fee_sharing_config_pda(&legacy_mint).unwrap();
    let rpc_legacy = CountingRpc::new(812)
        .account(
            legacy_curve,
            pf_accounts::PUMPFUN,
            encode_pumpfun_bonding_curve(creator, 900_000, 40_000, false, false, false),
        )
        .account(legacy_mint, TOKEN_PROGRAM, spl_mint_bytes(1_000_000))
        .account(
            pf_global::GLOBAL_ACCOUNT,
            pf_accounts::PUMPFUN,
            encode_pumpfun_global(fresh_global_fee),
        );
    let legacy_reqs = rpc_legacy.requests();
    let unknown_tp_meta = PumpFunImmutableMeta {
        token_program: Pubkey::default(),
        fee_recipient: Pubkey::default(),
        ..stale_meta
    };
    let legacy_params = PumpFunParams::from_cached_meta_by_rpc(
        &rpc_legacy.client(),
        &legacy_mint,
        &unknown_tp_meta,
    )
    .await
    .unwrap();
    assert_eq!(legacy_params.token_program, TOKEN_PROGRAM);
    assert_eq!(legacy_params.fee_recipient, fresh_global_fee);
    {
        let reqs = legacy_reqs.lock().unwrap();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].0, RpcRequest::GetMultipleAccounts);
        assert_eq!(
            reqs[0].1,
            vec![legacy_curve, legacy_mint, legacy_sharing, pf_global::GLOBAL_ACCOUNT]
        );
    }
}

fn clock_sysvar_bytes(epoch: u64) -> Vec<u8> {
    let mut data = vec![0u8; 40];
    data[16..24].copy_from_slice(&epoch.to_le_bytes());
    data
}

fn token_2022_mint_with_transfer_fee(
    supply: u64,
    newer_epoch: u64,
    older_bps: u16,
    older_max: u64,
    newer_bps: u16,
    newer_max: u64,
) -> Vec<u8> {
    let len = ExtensionType::try_calculate_account_len::<Token2022Mint>(&[
        ExtensionType::TransferFeeConfig,
    ])
    .unwrap();
    let mut data = vec![0u8; len];
    let mut state =
        StateWithExtensionsMut::<Token2022Mint>::unpack_uninitialized(&mut data).unwrap();
    state.base = Token2022Mint { supply, decimals: 6, is_initialized: true, ..Default::default() };
    state.pack_base();
    state.init_account_type().unwrap();
    let ext = state.init_extension::<TransferFeeConfig>(true).unwrap();
    ext.older_transfer_fee = TransferFee {
        epoch: 0u64.into(),
        maximum_fee: older_max.into(),
        transfer_fee_basis_points: older_bps.into(),
    };
    ext.newer_transfer_fee = TransferFee {
        epoch: newer_epoch.into(),
        maximum_fee: newer_max.into(),
        transfer_fee_basis_points: newer_bps.into(),
    };
    data
}

#[tokio::test]
async fn bonk_and_cpmm_token2022_transfer_fee_uses_clock_or_falls_back_to_epoch_info() {
    let base_mint = Pubkey::new_unique();
    let quote_mint = Pubkey::new_unique();
    let fee_mint_bytes = token_2022_mint_with_transfer_fee(1_000_000, 10, 150, 5_000, 325, 9_000);
    let expected_older = TokenTransferFee { basis_points: 150, maximum_fee: 5_000 };
    let expected_newer = TokenTransferFee { basis_points: 325, maximum_fee: 9_000 };

    // --- LaunchLab (BonkParams::from_pool_state) ---
    let bonk_pool = Pubkey::new_unique();
    let global_config = Pubkey::new_unique();
    let platform_config = Pubkey::new_unique();
    let bonk_state = BonkPoolState {
        global_config,
        platform_config,
        base_mint,
        quote_mint,
        base_vault: Pubkey::new_unique(),
        quote_vault: Pubkey::new_unique(),
        creator: Pubkey::new_unique(),
        virtual_base: 1_000_000_000,
        virtual_quote: 30_000_000_000,
        ..BonkPoolState::default()
    };

    // With CLOCK_SYSVAR at epoch 12 (>= 10): 1 GetMultipleAccounts RPC, 0 GetEpochInfo, newer fee.
    let bonk_with_clock = CountingRpc::new(900)
        .account(base_mint, TOKEN_PROGRAM_2022, fee_mint_bytes.clone())
        .account(quote_mint, TOKEN_PROGRAM, spl_mint_bytes(1_000_000))
        .account(global_config, BONK, bonk_global_config_bytes(2_500))
        .account(platform_config, BONK, bonk_platform_config_bytes(10_000, 1_000))
        .account(CLOCK_SYSVAR, solana_sdk::sysvar::ID, clock_sysvar_bytes(12));
    let bonk_clock_reqs = bonk_with_clock.requests();
    let bonk_params_clock =
        BonkParams::from_pool_state(&bonk_with_clock.client(), &bonk_pool, &bonk_state)
            .await
            .unwrap();
    assert_eq!(bonk_params_clock.base_transfer_fee, expected_newer);
    assert_eq!(bonk_params_clock.quote_transfer_fee, TokenTransferFee::default());
    {
        let reqs = bonk_clock_reqs.lock().unwrap();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].0, RpcRequest::GetMultipleAccounts);
    }

    // Without CLOCK_SYSVAR: falls back to GetEpochInfo (epoch 7 < 10 -> older fee).
    let bonk_no_clock = CountingRpc::new(901)
        .epoch(7)
        .account(base_mint, TOKEN_PROGRAM_2022, fee_mint_bytes.clone())
        .account(quote_mint, TOKEN_PROGRAM, spl_mint_bytes(1_000_000))
        .account(global_config, BONK, bonk_global_config_bytes(2_500))
        .account(platform_config, BONK, bonk_platform_config_bytes(10_000, 1_000));
    let bonk_no_clock_reqs = bonk_no_clock.requests();
    let bonk_params_fallback =
        BonkParams::from_pool_state(&bonk_no_clock.client(), &bonk_pool, &bonk_state)
            .await
            .unwrap();
    assert_eq!(bonk_params_fallback.base_transfer_fee, expected_older);
    {
        let reqs = bonk_no_clock_reqs.lock().unwrap();
        assert_eq!(reqs.len(), 2);
        assert_eq!(reqs[0].0, RpcRequest::GetMultipleAccounts);
        assert_eq!(reqs[1].0, RpcRequest::GetEpochInfo);
    }

    // --- Raydium CPMM (RaydiumCpmmParams::from_pool_state) ---
    let cpmm_pool = Pubkey::new_unique();
    let amm_config = Pubkey::new_unique();
    let vault0 = Pubkey::new_unique();
    let vault1 = Pubkey::new_unique();
    let cpmm_state = CpmmPoolState {
        amm_config,
        token0_vault: vault0,
        token1_vault: vault1,
        token0_mint: base_mint,
        token1_mint: quote_mint,
        token0_program: TOKEN_PROGRAM_2022,
        token1_program: TOKEN_PROGRAM,
        observation_key: Pubkey::new_unique(),
        ..CpmmPoolState::default()
    };
    let amm_config_bytes = encode_amm_config(&AmmConfig {
        trade_fee_rate: 2_500,
        protocol_fee_rate: 120_000,
        fund_fee_rate: 40_000,
        creator_fee_rate: 1_000,
        ..AmmConfig::default()
    });

    let cpmm_pool_bytes = encode_cpmm_pool(&cpmm_state);

    // With CLOCK_SYSVAR at epoch 12: 1 GetMultipleAccounts RPC, 0 GetEpochInfo, newer fee.
    let cpmm_with_clock = CountingRpc::new(902)
        .account(cpmm_pool, RAYDIUM_CPMM, cpmm_pool_bytes.clone())
        .account(amm_config, RAYDIUM_CPMM, amm_config_bytes.clone())
        .account(vault0, TOKEN_PROGRAM_2022, spl_token_account_bytes(base_mint, 1_000))
        .account(vault1, TOKEN_PROGRAM, spl_token_account_bytes(quote_mint, 2_000))
        .account(base_mint, TOKEN_PROGRAM_2022, fee_mint_bytes.clone())
        .account(quote_mint, TOKEN_PROGRAM, spl_mint_bytes(1_000_000))
        .account(CLOCK_SYSVAR, solana_sdk::sysvar::ID, clock_sysvar_bytes(12));
    let cpmm_clock_reqs = cpmm_with_clock.requests();
    let cpmm_params_clock =
        RaydiumCpmmParams::from_pool_state(&cpmm_with_clock.client(), &cpmm_pool, &cpmm_state)
            .await
            .unwrap();
    assert_eq!(cpmm_params_clock.base_transfer_fee, expected_newer);
    assert_eq!(cpmm_params_clock.quote_transfer_fee, TokenTransferFee::default());
    {
        let reqs = cpmm_clock_reqs.lock().unwrap();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].0, RpcRequest::GetMultipleAccounts);
    }

    // Without CLOCK_SYSVAR: falls back to GetEpochInfo (epoch 7 -> older fee).
    let cpmm_no_clock = CountingRpc::new(903)
        .epoch(7)
        .account(cpmm_pool, RAYDIUM_CPMM, cpmm_pool_bytes)
        .account(amm_config, RAYDIUM_CPMM, amm_config_bytes)
        .account(vault0, TOKEN_PROGRAM_2022, spl_token_account_bytes(base_mint, 1_000))
        .account(vault1, TOKEN_PROGRAM, spl_token_account_bytes(quote_mint, 2_000))
        .account(base_mint, TOKEN_PROGRAM_2022, fee_mint_bytes)
        .account(quote_mint, TOKEN_PROGRAM, spl_mint_bytes(1_000_000));
    let cpmm_no_clock_reqs = cpmm_no_clock.requests();
    let cpmm_params_fallback =
        RaydiumCpmmParams::from_pool_state(&cpmm_no_clock.client(), &cpmm_pool, &cpmm_state)
            .await
            .unwrap();
    assert_eq!(cpmm_params_fallback.base_transfer_fee, expected_older);
    {
        let reqs = cpmm_no_clock_reqs.lock().unwrap();
        assert_eq!(reqs.len(), 2);
        assert_eq!(reqs[0].0, RpcRequest::GetMultipleAccounts);
        assert_eq!(reqs[1].0, RpcRequest::GetEpochInfo);
    }
}
