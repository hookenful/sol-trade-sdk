use anyhow::{anyhow, Result};
use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::{pubkey, pubkey::Pubkey, sysvar};

use crate::PrecheckConfig;

/// Instruction discriminator for `PrecheckV1`.
pub const PRECHECK_V1_DISCRIMINATOR: u8 = 1;

/// Serialized payload length for `PrecheckV1`.
pub const PRECHECK_V1_PAYLOAD_LEN: usize = 1 + 8 + 1 + 8 + 8 + 8 + 8 + 8;

/// Default deployed precheck program id.
pub const DEFAULT_PRECHECK_PROGRAM_ID: Pubkey =
    pubkey!("HooKi9j7A9CN3Yr8D2MqwTj4XfKetWstqm1padU8imiE");

/// On-chain custom error code: liquidity lower than configured minimum.
pub const ERR_LIQUIDITY_TOO_LOW: u32 = 7_000;
/// On-chain custom error code: liquidity above configured maximum.
pub const ERR_LIQUIDITY_TOO_HIGH: u32 = 7_001;
/// On-chain custom error code: slot distance exceeds `max_slot_diff`.
pub const ERR_CONTEXT_SLOT_DIFFERENCE_REACHED: u32 = 7_002;
/// On-chain custom error code: bonding curve account is invalid.
pub const ERR_INVALID_CURVE_ACCOUNT: u32 = 7_003;
/// On-chain custom error code: liquidity difference lower than configured minimum.
pub const ERR_LIQUIDITY_DIFFERENCE_TOO_LOW: u32 = 7_004;
/// On-chain custom error code: liquidity difference above configured maximum.
pub const ERR_LIQUIDITY_DIFFERENCE_TOO_HIGH: u32 = 7_005;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PrecheckPayloadV1 {
    pub context_slot: u64,
    pub max_slot_diff: u8,
    pub min_liquidity_lamports: u64,
    pub max_liquidity_lamports: u64,
    pub base_liquidity_lamports: u64,
    pub min_liquidity_difference_lamports: u64,
    pub max_liquidity_difference_lamports: u64,
}

impl PrecheckPayloadV1 {
    #[inline]
    pub fn from_config(config: &PrecheckConfig) -> Self {
        Self {
            context_slot: config.context_slot,
            max_slot_diff: config.max_slot_diff,
            min_liquidity_lamports: config.min_liquidity_lamports,
            max_liquidity_lamports: config.max_liquidity_lamports,
            base_liquidity_lamports: config.base_liquidity_lamports,
            min_liquidity_difference_lamports: config.min_liquidity_difference_lamports,
            max_liquidity_difference_lamports: config.max_liquidity_difference_lamports,
        }
    }

    #[inline]
    pub fn to_bytes(self) -> [u8; PRECHECK_V1_PAYLOAD_LEN] {
        let mut bytes = [0u8; PRECHECK_V1_PAYLOAD_LEN];
        bytes[0] = PRECHECK_V1_DISCRIMINATOR;
        bytes[1..9].copy_from_slice(&self.context_slot.to_le_bytes());
        bytes[9] = self.max_slot_diff;
        bytes[10..18].copy_from_slice(&self.min_liquidity_lamports.to_le_bytes());
        bytes[18..26].copy_from_slice(&self.max_liquidity_lamports.to_le_bytes());
        bytes[26..34].copy_from_slice(&self.base_liquidity_lamports.to_le_bytes());
        bytes[34..42].copy_from_slice(&self.min_liquidity_difference_lamports.to_le_bytes());
        bytes[42..50].copy_from_slice(&self.max_liquidity_difference_lamports.to_le_bytes());
        bytes
    }
}

#[inline]
pub fn precheck_error_name(code: u32) -> Option<&'static str> {
    match code {
        ERR_LIQUIDITY_TOO_LOW => Some("LiquidityTooLow"),
        ERR_LIQUIDITY_TOO_HIGH => Some("LiquidityTooHigh"),
        ERR_CONTEXT_SLOT_DIFFERENCE_REACHED => Some("ContextSlotDifferenceReached"),
        ERR_INVALID_CURVE_ACCOUNT => Some("InvalidCurveAccount"),
        ERR_LIQUIDITY_DIFFERENCE_TOO_LOW => Some("LiquidityDifferenceTooLow"),
        ERR_LIQUIDITY_DIFFERENCE_TOO_HIGH => Some("LiquidityDifferenceTooHigh"),
        _ => None,
    }
}

#[inline]
pub fn build_precheck_v1_instruction(
    bonding_curve: Pubkey,
    config: &PrecheckConfig,
) -> Result<Instruction> {
    config.validate()?;
    if bonding_curve == Pubkey::default() {
        return Err(anyhow!("Precheck requires a non-default bonding curve account"));
    }

    let program_id = config.program_id.unwrap_or(DEFAULT_PRECHECK_PROGRAM_ID);
    let payload = PrecheckPayloadV1::from_config(config);

    Ok(Instruction::new_with_bytes(
        program_id,
        &payload.to_bytes(),
        vec![
            AccountMeta::new_readonly(sysvar::clock::id(), false),
            AccountMeta::new_readonly(bonding_curve, false),
        ],
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn precheck_payload_v1_serializes_expected_layout() {
        let payload = PrecheckPayloadV1 {
            context_slot: 42,
            max_slot_diff: 9,
            min_liquidity_lamports: 1_000,
            max_liquidity_lamports: 9_000,
            base_liquidity_lamports: 4_200,
            min_liquidity_difference_lamports: 11,
            max_liquidity_difference_lamports: 22,
        };
        let bytes = payload.to_bytes();
        assert_eq!(bytes.len(), PRECHECK_V1_PAYLOAD_LEN);
        assert_eq!(bytes[0], PRECHECK_V1_DISCRIMINATOR);
        assert_eq!(&bytes[1..9], &42u64.to_le_bytes());
        assert_eq!(bytes[9], 9);
        assert_eq!(&bytes[10..18], &1_000u64.to_le_bytes());
        assert_eq!(&bytes[18..26], &9_000u64.to_le_bytes());
        assert_eq!(&bytes[26..34], &4_200u64.to_le_bytes());
        assert_eq!(&bytes[34..42], &11u64.to_le_bytes());
        assert_eq!(&bytes[42..50], &22u64.to_le_bytes());
    }

    #[test]
    fn precheck_builder_rejects_invalid_liquidity_range() {
        let cfg = PrecheckConfig {
            program_id: None,
            context_slot: 1,
            max_slot_diff: 1,
            min_liquidity_lamports: 10,
            max_liquidity_lamports: 9,
            base_liquidity_lamports: 0,
            min_liquidity_difference_lamports: 0,
            max_liquidity_difference_lamports: 0,
        };

        let err = build_precheck_v1_instruction(Pubkey::new_unique(), &cfg).expect_err("must fail");
        assert!(err.to_string().contains("min_liquidity_lamports"));
    }

    #[test]
    fn precheck_builder_rejects_zero_max_slot_diff() {
        let cfg = PrecheckConfig {
            program_id: None,
            context_slot: 1,
            max_slot_diff: 0,
            min_liquidity_lamports: 1,
            max_liquidity_lamports: 2,
            base_liquidity_lamports: 0,
            min_liquidity_difference_lamports: 0,
            max_liquidity_difference_lamports: 0,
        };

        let err = build_precheck_v1_instruction(Pubkey::new_unique(), &cfg).expect_err("must fail");
        assert!(err.to_string().contains("max_slot_diff"));
    }

    #[test]
    fn precheck_builder_rejects_invalid_liquidity_difference_range() {
        let cfg = PrecheckConfig {
            program_id: None,
            context_slot: 1,
            max_slot_diff: 1,
            min_liquidity_lamports: 1,
            max_liquidity_lamports: 2,
            base_liquidity_lamports: 1,
            min_liquidity_difference_lamports: 3,
            max_liquidity_difference_lamports: 2,
        };

        let err = build_precheck_v1_instruction(Pubkey::new_unique(), &cfg).expect_err("must fail");
        assert!(err.to_string().contains("min_liquidity_difference_lamports"));
    }
}
