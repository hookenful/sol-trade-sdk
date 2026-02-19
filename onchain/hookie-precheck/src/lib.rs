#![no_std]
#![allow(unexpected_cfgs)]

use pinocchio::{error::ProgramError, sysvars::clock::Clock};
use pinocchio::{AccountView, Address, ProgramResult};

pub const ID: Address = Address::new_from_array([
    249, 184, 21, 204, 5, 89, 248, 235, 70, 125, 249, 14, 218, 113, 77, 149, 169, 177, 168, 130,
    118, 209, 196, 134, 27, 76, 35, 176, 92, 69, 137, 191,
]);
pub const PUMPFUN_PROGRAM_ID: Address = Address::new_from_array([
    1, 86, 224, 246, 147, 102, 90, 207, 68, 219, 21, 104, 191, 23, 91, 170, 81, 137, 203, 151, 245,
    210, 255, 59, 101, 93, 43, 182, 253, 109, 24, 176,
]);

pub const PRECHECK_V1_DISCRIMINATOR: u8 = 1;
pub const PRECHECK_V1_DATA_LEN: usize = 1 + 8 + 1 + 8 + 8;

/// PumpFun account layout offset for `real_sol_reserves`.
/// Layout: [anchor_discriminator:8][virtual_token:8][virtual_sol:8][real_token:8][real_sol:8]
pub const REAL_SOL_RESERVES_OFFSET: usize = 8 + 8 + 8 + 8;
pub const REAL_SOL_RESERVES_END: usize = REAL_SOL_RESERVES_OFFSET + 8;

#[repr(u32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PrecheckError {
    LiquidityTooLow = 7_000,
    LiquidityTooHigh = 7_001,
    ContextSlotDifferenceReached = 7_002,
    InvalidCurveAccount = 7_003,
}

impl From<PrecheckError> for ProgramError {
    #[inline]
    fn from(value: PrecheckError) -> Self {
        ProgramError::Custom(value as u32)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PrecheckPayloadV1 {
    pub context_slot: u64,
    pub max_slot_diff: u8,
    pub min_liquidity_lamports: u64,
    pub max_liquidity_lamports: u64,
}

impl PrecheckPayloadV1 {
    #[inline]
    pub fn parse(instruction_data: &[u8]) -> Result<Self, ProgramError> {
        if instruction_data.len() != PRECHECK_V1_DATA_LEN {
            return Err(ProgramError::InvalidInstructionData);
        }
        if instruction_data[0] != PRECHECK_V1_DISCRIMINATOR {
            return Err(ProgramError::InvalidInstructionData);
        }

        let context_slot = read_u64_le(&instruction_data[1..9])?;
        let max_slot_diff = instruction_data[9];
        let min_liquidity_lamports = read_u64_le(&instruction_data[10..18])?;
        let max_liquidity_lamports = read_u64_le(&instruction_data[18..26])?;

        Ok(Self { context_slot, max_slot_diff, min_liquidity_lamports, max_liquidity_lamports })
    }

    #[inline]
    pub fn validate(self) -> Result<(), ProgramError> {
        if self.max_slot_diff == 0 {
            return Err(ProgramError::InvalidInstructionData);
        }
        if self.min_liquidity_lamports > self.max_liquidity_lamports {
            return Err(ProgramError::InvalidInstructionData);
        }
        Ok(())
    }
}

#[cfg(feature = "bpf-entrypoint")]
mod entrypoint {
    use super::process_instruction;
    use pinocchio::{no_allocator, nostd_panic_handler, program_entrypoint};

    program_entrypoint!(process_instruction, 2);
    no_allocator!();
    nostd_panic_handler!();
}

pub fn process_instruction(
    _program_id: &Address,
    accounts: &[AccountView],
    instruction_data: &[u8],
) -> ProgramResult {
    if accounts.len() < 2 {
        return Err(ProgramError::NotEnoughAccountKeys);
    }

    let payload = PrecheckPayloadV1::parse(instruction_data)?;
    payload.validate()?;

    let clock_account = &accounts[0];
    let bonding_curve_account = &accounts[1];

    let clock = Clock::from_account_view(clock_account)?;
    let slot_diff = clock
        .slot
        .checked_sub(payload.context_slot)
        .ok_or(PrecheckError::ContextSlotDifferenceReached)?;

    if slot_diff > payload.max_slot_diff as u64 {
        return Err(PrecheckError::ContextSlotDifferenceReached.into());
    }

    if !bonding_curve_account.owned_by(&PUMPFUN_PROGRAM_ID) {
        return Err(PrecheckError::InvalidCurveAccount.into());
    }

    let curve_data = bonding_curve_account.try_borrow()?;
    if curve_data.len() < REAL_SOL_RESERVES_END {
        return Err(PrecheckError::InvalidCurveAccount.into());
    }

    let real_sol_reserves =
        read_u64_le(&curve_data[REAL_SOL_RESERVES_OFFSET..REAL_SOL_RESERVES_END])?;

    if real_sol_reserves < payload.min_liquidity_lamports {
        return Err(PrecheckError::LiquidityTooLow.into());
    }
    if real_sol_reserves > payload.max_liquidity_lamports {
        return Err(PrecheckError::LiquidityTooHigh.into());
    }

    Ok(())
}

#[inline]
fn read_u64_le(bytes: &[u8]) -> Result<u64, ProgramError> {
    if bytes.len() < 8 {
        return Err(ProgramError::InvalidInstructionData);
    }
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&bytes[..8]);
    Ok(u64::from_le_bytes(buf))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload_bytes(
        discriminator: u8,
        context_slot: u64,
        max_slot_diff: u8,
        min_liquidity_lamports: u64,
        max_liquidity_lamports: u64,
    ) -> [u8; PRECHECK_V1_DATA_LEN] {
        let mut bytes = [0u8; PRECHECK_V1_DATA_LEN];
        bytes[0] = discriminator;
        bytes[1..9].copy_from_slice(&context_slot.to_le_bytes());
        bytes[9] = max_slot_diff;
        bytes[10..18].copy_from_slice(&min_liquidity_lamports.to_le_bytes());
        bytes[18..26].copy_from_slice(&max_liquidity_lamports.to_le_bytes());
        bytes
    }

    #[test]
    fn parse_and_validate_accepts_valid_payload() {
        let bytes = payload_bytes(PRECHECK_V1_DISCRIMINATOR, 42, 7, 1_000, 2_000);
        let payload = PrecheckPayloadV1::parse(&bytes).expect("payload should parse");
        assert_eq!(payload.context_slot, 42);
        assert_eq!(payload.max_slot_diff, 7);
        assert_eq!(payload.min_liquidity_lamports, 1_000);
        assert_eq!(payload.max_liquidity_lamports, 2_000);
        payload.validate().expect("payload should validate");
    }

    #[test]
    fn parse_rejects_invalid_discriminator() {
        let bytes = payload_bytes(99, 1, 1, 1, 2);
        let err = PrecheckPayloadV1::parse(&bytes).expect_err("must fail");
        assert_eq!(err, ProgramError::InvalidInstructionData);
    }

    #[test]
    fn parse_rejects_invalid_length() {
        let bytes = [0u8; PRECHECK_V1_DATA_LEN - 1];
        let err = PrecheckPayloadV1::parse(&bytes).expect_err("must fail");
        assert_eq!(err, ProgramError::InvalidInstructionData);
    }

    #[test]
    fn validate_rejects_zero_max_slot_diff() {
        let payload = PrecheckPayloadV1 {
            context_slot: 1,
            max_slot_diff: 0,
            min_liquidity_lamports: 1,
            max_liquidity_lamports: 2,
        };
        let err = payload.validate().expect_err("must fail");
        assert_eq!(err, ProgramError::InvalidInstructionData);
    }

    #[test]
    fn validate_rejects_invalid_liquidity_range() {
        let payload = PrecheckPayloadV1 {
            context_slot: 1,
            max_slot_diff: 1,
            min_liquidity_lamports: 3,
            max_liquidity_lamports: 2,
        };
        let err = payload.validate().expect_err("must fail");
        assert_eq!(err, ProgramError::InvalidInstructionData);
    }

    #[test]
    fn read_u64_le_rejects_short_slice() {
        let err = read_u64_le(&[1, 2, 3]).expect_err("must fail");
        assert_eq!(err, ProgramError::InvalidInstructionData);
    }
}
