use crate::{
    common::spl_token::close_account,
    constants::{trade::trade::DEFAULT_SLIPPAGE, TOKEN_PROGRAM_2022},
    trading::core::{
        params::{PumpFunParams, SwapParams},
        traits::InstructionBuilder,
    },
};
use crate::{
    instruction::utils::pumpfun::{
        accounts, get_bonding_curve_pda, get_bonding_curve_v2_pda, get_creator,
        get_user_volume_accumulator_pda, global_constants::{self},
        BUY_DISCRIMINATOR,
        BUY_EXACT_SOL_IN_DISCRIMINATOR,
    },
    utils::calc::{
        common::{calculate_with_slippage_buy, calculate_with_slippage_sell},
        pumpfun::{get_buy_token_amount_from_sol_amount, get_sell_sol_amount_from_token_amount},
    },
};
use anyhow::{anyhow, Result};
use solana_sdk::instruction::AccountMeta;
use solana_sdk::{instruction::Instruction, pubkey::Pubkey, signer::Signer};

/// Instruction builder for PumpFun protocol
pub struct PumpFunInstructionBuilder;

#[async_trait::async_trait]
impl InstructionBuilder for PumpFunInstructionBuilder {
    async fn build_buy_instructions(&self, params: &SwapParams) -> Result<Vec<Instruction>> {
        // ========================================
        // Parameter validation and basic data preparation
        // ========================================
        let protocol_params = params
            .protocol_params
            .as_any()
            .downcast_ref::<PumpFunParams>()
            .ok_or_else(|| anyhow!("Invalid protocol params for PumpFun"))?;

        if params.input_amount.unwrap_or(0) == 0 {
            return Err(anyhow!("Amount cannot be zero"));
        }

        let bonding_curve = &protocol_params.bonding_curve;
        let creator_vault_pda = protocol_params.creator_vault;
        let creator = get_creator(&creator_vault_pda);

        // ========================================
        // Trade calculation and account address preparation
        // ========================================
        let buy_token_amount = match params.fixed_output_amount {
            Some(amount) => amount,
            None => get_buy_token_amount_from_sol_amount(
                bonding_curve.virtual_token_reserves as u128,
                bonding_curve.virtual_sol_reserves as u128,
                bonding_curve.real_token_reserves as u128,
                creator,
                params.input_amount.unwrap_or(0),
            ),
        };

        let max_sol_cost = calculate_with_slippage_buy(
            params.input_amount.unwrap_or(0),
            params.slippage_basis_points.unwrap_or(DEFAULT_SLIPPAGE),
        );

        let bonding_curve_addr = if bonding_curve.account == Pubkey::default() {
            get_bonding_curve_pda(&params.output_mint).unwrap()
        } else {
            bonding_curve.account
        };

        // Determine token program based on mayhem mode
        let is_mayhem_mode = bonding_curve.is_mayhem_mode;
        let token_program = protocol_params.token_program;
        let token_program_meta = if protocol_params.token_program == TOKEN_PROGRAM_2022 {
            crate::constants::TOKEN_PROGRAM_2022_META
        } else {
            crate::constants::TOKEN_PROGRAM_META
        };

        let associated_bonding_curve =
            if protocol_params.associated_bonding_curve == Pubkey::default() {
                crate::common::fast_fn::get_associated_token_address_with_program_id_fast(
                    &bonding_curve_addr,
                    &params.output_mint,
                    &token_program,
                )
            } else {
                protocol_params.associated_bonding_curve
            };

        let user_token_account =
            crate::common::fast_fn::get_associated_token_address_with_program_id_fast_use_seed(
                &params.payer.pubkey(),
                &params.output_mint,
                &token_program,
                params.open_seed_optimize,
            );

        let user_volume_accumulator =
            get_user_volume_accumulator_pda(&params.payer.pubkey()).unwrap();

        // ========================================
        // Build instructions
        // ========================================
        let mut instructions = Vec::with_capacity(3);

        if let Some(precheck) = &params.precheck {
            instructions.push(crate::instruction::hookie_precheck::build_precheck_v1_instruction(
                bonding_curve_addr,
                precheck,
            )?);
        }

        // Create associated token account
        if params.create_output_mint_ata {
            instructions.extend(
                crate::common::fast_fn::create_associated_token_account_idempotent_fast_use_seed(
                    &params.payer.pubkey(),
                    &params.payer.pubkey(),
                    &params.output_mint,
                    &token_program,
                    params.open_seed_optimize,
                ),
            );
        }

        let mut buy_data = [0u8; 24];
        let use_exact_sol_amount = params.use_exact_sol_amount.unwrap_or(true);
        if use_exact_sol_amount {
            // buy_exact_sol_in(spendable_sol_in: u64, min_tokens_out: u64)
            // Spend exactly the input SOL amount, get at least min_tokens_out
            let min_tokens_out = if params.use_exact_sol_amount == Some(true) {
                // Preset explicitly requested exact SOL mode: disable min output guard.
                1
            } else {
                calculate_with_slippage_sell(
                    buy_token_amount,
                    params.slippage_basis_points.unwrap_or(DEFAULT_SLIPPAGE),
                )
            };
            buy_data[..8].copy_from_slice(&BUY_EXACT_SOL_IN_DISCRIMINATOR);
            buy_data[8..16].copy_from_slice(&params.input_amount.unwrap_or(0).to_le_bytes());
            buy_data[16..24].copy_from_slice(&min_tokens_out.to_le_bytes());
        } else {
            // buy(token_amount: u64, max_sol_cost: u64)
            // Buy exactly token_amount tokens, pay up to max_sol_cost
            buy_data[..8].copy_from_slice(&BUY_DISCRIMINATOR);
            buy_data[8..16].copy_from_slice(&buy_token_amount.to_le_bytes());
            buy_data[16..24].copy_from_slice(&max_sol_cost.to_le_bytes());
        }

        // Determine fee recipient based on mayhem mode
        let fee_recipient_meta = if is_mayhem_mode {
            global_constants::MAYHEM_FEE_RECIPIENT_META
        } else {
            global_constants::FEE_RECIPIENT_META
        };

        let bonding_curve_v2 = get_bonding_curve_v2_pda(&params.output_mint).unwrap();
        let mut accounts: Vec<AccountMeta> = vec![
            global_constants::GLOBAL_ACCOUNT_META,
            fee_recipient_meta,
            AccountMeta::new_readonly(params.output_mint, false),
            AccountMeta::new(bonding_curve_addr, false),
            AccountMeta::new(associated_bonding_curve, false),
            AccountMeta::new(user_token_account, false),
            AccountMeta::new(params.payer.pubkey(), true),
            crate::constants::SYSTEM_PROGRAM_META,
            token_program_meta,
            AccountMeta::new(creator_vault_pda, false),
            accounts::EVENT_AUTHORITY_META,
            accounts::PUMPFUN_META,
            accounts::GLOBAL_VOLUME_ACCUMULATOR_META,
            AccountMeta::new(user_volume_accumulator, false),
            accounts::FEE_CONFIG_META,
            accounts::FEE_PROGRAM_META,
        ];
        accounts.push(AccountMeta::new_readonly(bonding_curve_v2, false)); // bonding_curve_v2 (readonly) at end

        instructions.push(Instruction::new_with_bytes(
            accounts::PUMPFUN,
            &buy_data,
            accounts,
        ));

        Ok(instructions)
    }

    async fn build_sell_instructions(&self, params: &SwapParams) -> Result<Vec<Instruction>> {
        // ========================================
        // Parameter validation and basic data preparation
        // ========================================
        let protocol_params = params
            .protocol_params
            .as_any()
            .downcast_ref::<PumpFunParams>()
            .ok_or_else(|| anyhow!("Invalid protocol params for PumpFun"))?;

        let token_amount = if let Some(amount) = params.input_amount {
            if amount == 0 {
                return Err(anyhow!("Amount cannot be zero"));
            }
            amount
        } else {
            return Err(anyhow!("Amount token is required"));
        };

        let bonding_curve = &protocol_params.bonding_curve;
        let creator_vault_pda = protocol_params.creator_vault;
        let creator = get_creator(&creator_vault_pda);

        // ========================================
        // Trade calculation and account address preparation
        // ========================================
        let sol_amount = get_sell_sol_amount_from_token_amount(
            bonding_curve.virtual_token_reserves as u128,
            bonding_curve.virtual_sol_reserves as u128,
            creator,
            token_amount,
        );

        let min_sol_output = match params.fixed_output_amount {
            Some(fixed) => fixed,
            None => calculate_with_slippage_sell(
                sol_amount,
                params.slippage_basis_points.unwrap_or(DEFAULT_SLIPPAGE),
            ),
        };

        let bonding_curve_addr = if bonding_curve.account == Pubkey::default() {
            get_bonding_curve_pda(&params.input_mint).unwrap()
        } else {
            bonding_curve.account
        };

        // Determine token program based on mayhem mode
        let is_mayhem_mode = bonding_curve.is_mayhem_mode;
        let token_program = protocol_params.token_program;
        let token_program_meta = if protocol_params.token_program == TOKEN_PROGRAM_2022 {
            crate::constants::TOKEN_PROGRAM_2022_META
        } else {
            crate::constants::TOKEN_PROGRAM_META
        };

        let associated_bonding_curve =
            if protocol_params.associated_bonding_curve == Pubkey::default() {
                crate::common::fast_fn::get_associated_token_address_with_program_id_fast(
                    &bonding_curve_addr,
                    &params.input_mint,
                    &token_program,
                )
            } else {
                protocol_params.associated_bonding_curve
            };

        let user_token_account =
            crate::common::fast_fn::get_associated_token_address_with_program_id_fast_use_seed(
                &params.payer.pubkey(),
                &params.input_mint,
                &token_program,
                params.open_seed_optimize,
            );

        // ========================================
        // Build instructions
        // ========================================
        let mut instructions = Vec::with_capacity(2);

        let mut sell_data = [0u8; 24];
        sell_data[..8].copy_from_slice(&[51, 230, 133, 164, 1, 127, 131, 173]); // Method ID
        sell_data[8..16].copy_from_slice(&token_amount.to_le_bytes());
        sell_data[16..24].copy_from_slice(&min_sol_output.to_le_bytes());

        // Determine fee recipient based on mayhem mode
        let fee_recipient_meta = if is_mayhem_mode {
            global_constants::MAYHEM_FEE_RECIPIENT_META
        } else {
            global_constants::FEE_RECIPIENT_META
        };

        let mut accounts: Vec<AccountMeta> = vec![
            global_constants::GLOBAL_ACCOUNT_META,
            fee_recipient_meta,
            AccountMeta::new_readonly(params.input_mint, false),
            AccountMeta::new(bonding_curve_addr, false),
            AccountMeta::new(associated_bonding_curve, false),
            AccountMeta::new(user_token_account, false),
            AccountMeta::new(params.payer.pubkey(), true),
            crate::constants::SYSTEM_PROGRAM_META,
            AccountMeta::new(creator_vault_pda, false),
            token_program_meta,
            accounts::EVENT_AUTHORITY_META,
            accounts::PUMPFUN_META,
            accounts::FEE_CONFIG_META,
            accounts::FEE_PROGRAM_META,
        ];

        // Cashback: Bonding Curve Sell expects UserVolumeAccumulator PDA at 0th remaining account (writable)
        if bonding_curve.is_cashback_coin {
            let user_volume_accumulator =
                get_user_volume_accumulator_pda(&params.payer.pubkey()).unwrap();
            accounts.push(AccountMeta::new(user_volume_accumulator, false));
        }
        // Program upgrade: bonding_curve_v2 (readonly) at end of account list
        let bonding_curve_v2 = get_bonding_curve_v2_pda(&params.input_mint).unwrap();
        accounts.push(AccountMeta::new_readonly(bonding_curve_v2, false));

        instructions.push(Instruction::new_with_bytes(accounts::PUMPFUN, &sell_data, accounts));

        // Optional: Close token account
        if protocol_params.close_token_account_when_sell.unwrap_or(false)
            || params.close_input_mint_ata
        {
            instructions.push(close_account(
                &token_program,
                &user_token_account,
                &params.payer.pubkey(),
                &params.payer.pubkey(),
                &[&params.payer.pubkey()],
            )?);
        }

        Ok(instructions)
    }
}

#[cfg(test)]
mod tests {
    use std::convert::TryInto;
    use std::sync::Arc;

    use super::PumpFunInstructionBuilder;
    use crate::common::GasFeeStrategy;
    use crate::instruction::hookie_precheck::DEFAULT_PRECHECK_PROGRAM_ID;
    use crate::instruction::utils::pumpfun::global_constants::FEE_RECIPIENT;
    use crate::instruction::utils::pumpfun::{
        get_creator_vault_pda, BUY_EXACT_SOL_IN_DISCRIMINATOR,
    };
    use crate::trading::core::params::{DexParamEnum, PumpFunParams, SwapParams};
    use crate::trading::core::traits::InstructionBuilder;
    use crate::PrecheckConfig;
    use solana_sdk::pubkey::Pubkey;
    use solana_sdk::signature::Keypair;

    fn make_buy_params(with_precheck: bool) -> SwapParams {
        let mint = Pubkey::new_unique();
        let creator = Pubkey::new_unique();
        let creator_vault = get_creator_vault_pda(&creator).unwrap();
        let pumpfun = PumpFunParams::from_trade(
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            mint,
            creator,
            creator_vault,
            1_073_000_000_000_000,
            30_000_000_000,
            793_100_000_000_000,
            1_500_000_000,
            None,
            FEE_RECIPIENT,
            crate::constants::TOKEN_PROGRAM,
            false,
        );

        SwapParams {
            rpc: None,
            payer: Arc::new(Keypair::new()),
            trade_type: crate::swqos::TradeType::Buy,
            input_mint: crate::constants::SOL_TOKEN_ACCOUNT,
            input_token_program: None,
            output_mint: mint,
            output_token_program: None,
            input_amount: Some(100_000_000),
            slippage_basis_points: Some(100),
            address_lookup_table_account: None,
            recent_blockhash: None,
            wait_transaction_confirmed: false,
            protocol_params: DexParamEnum::PumpFun(pumpfun),
            open_seed_optimize: false,
            swqos_clients: Vec::new(),
            middleware_manager: None,
            durable_nonce: None,
            with_tip: true,
            create_input_mint_ata: false,
            close_input_mint_ata: false,
            create_output_mint_ata: false,
            close_output_mint_ata: false,
            fixed_output_amount: None,
            gas_fee_strategy: GasFeeStrategy::new(),
            simulate: true,
            use_exact_sol_amount: Some(true),
            precheck: if with_precheck {
                Some(PrecheckConfig {
                    program_id: None,
                    context_slot: 123,
                    max_slot_diff: 5,
                    min_liquidity_lamports: 1_000_000_000,
                    max_liquidity_lamports: 2_000_000_000,
                    base_liquidity_lamports: 0,
                    min_liquidity_difference_lamports: 0,
                    max_liquidity_difference_lamports: 0,
                })
            } else {
                None
            },
        }
    }

    #[tokio::test]
    async fn pumpfun_buy_includes_precheck_instruction_first() {
        let builder = PumpFunInstructionBuilder;
        let params = make_buy_params(true);
        let instructions =
            builder.build_buy_instructions(&params).await.expect("build buy instructions");

        assert_eq!(instructions.len(), 2);
        assert_eq!(instructions[0].program_id, DEFAULT_PRECHECK_PROGRAM_ID);
        assert_eq!(
            instructions[1].program_id,
            crate::instruction::utils::pumpfun::accounts::PUMPFUN
        );
        assert_eq!(instructions[0].accounts.len(), 2);
        assert_eq!(instructions[0].accounts[0].pubkey, solana_sdk::sysvar::clock::id());
    }

    #[tokio::test]
    async fn pumpfun_buy_without_precheck_has_only_buy_instruction() {
        let builder = PumpFunInstructionBuilder;
        let params = make_buy_params(false);
        let instructions =
            builder.build_buy_instructions(&params).await.expect("build buy instructions");

        assert_eq!(instructions.len(), 1);
        assert_eq!(
            instructions[0].program_id,
            crate::instruction::utils::pumpfun::accounts::PUMPFUN
        );
    }

    #[tokio::test]
    async fn pumpfun_buy_exact_sol_from_preset_sets_min_tokens_out_to_one() {
        let builder = PumpFunInstructionBuilder;
        let mut params = make_buy_params(false);
        params.use_exact_sol_amount = Some(true);
        let instructions =
            builder.build_buy_instructions(&params).await.expect("build buy instructions");

        assert_eq!(instructions.len(), 1);
        let data = &instructions[0].data;
        assert_eq!(&data[..8], &BUY_EXACT_SOL_IN_DISCRIMINATOR);
        let min_tokens_out = u64::from_le_bytes(data[16..24].try_into().unwrap());
        assert_eq!(min_tokens_out, 1);
    }
}

/// Claim cashback for Bonding Curve (Pump program). Transfers native lamports from UserVolumeAccumulator to user.
pub fn claim_cashback_pumpfun_instruction(payer: &Pubkey) -> Option<Instruction> {
    const CLAIM_CASHBACK_DISCRIMINATOR: [u8; 8] = [37, 58, 35, 126, 190, 53, 228, 197];
    let user_volume_accumulator = get_user_volume_accumulator_pda(payer)?;
    let accounts = vec![
        AccountMeta::new(*payer, true), // user (signer, writable)
        AccountMeta::new(user_volume_accumulator, false), // user_volume_accumulator (writable, not signer)
        crate::constants::SYSTEM_PROGRAM_META,
        accounts::EVENT_AUTHORITY_META,
        accounts::PUMPFUN_META,
    ];
    Some(Instruction::new_with_bytes(accounts::PUMPFUN, &CLAIM_CASHBACK_DISCRIMINATOR, accounts))
}
