# hookie-precheck

Pinocchio on-chain precheck program for PumpFun buy flow.

## Contract

- Instruction: `PrecheckV1` (`discriminator = 1`)
- Payload:
  - `context_slot: u64`
  - `max_slot_diff: u8`
  - `min_liquidity_lamports: u64`
  - `max_liquidity_lamports: u64`
  - `base_liquidity_lamports: u64`
  - `min_liquidity_difference_lamports: u64` (`0` disables lower-bound check)
  - `max_liquidity_difference_lamports: u64` (`0` disables upper-bound check)
- Difference formula:
  - `liquidity_difference = current_real_sol_reserves - base_liquidity_lamports` (directional, no `abs`)
- Accounts:
  - `SysvarClock` (readonly)
  - `bonding_curve` (readonly, owner must be PumpFun program `6EF8...`)
- Errors:
  - `LiquidityTooLow` (`7000`)
  - `LiquidityTooHigh` (`7001`)
  - `ContextSlotDifferenceReached` (`7002`)
  - `InvalidCurveAccount` (`7003`)
  - `LiquidityDifferenceTooLow` (`7004`)
  - `LiquidityDifferenceTooHigh` (`7005`)

## Build

```bash
cd onchain/hookie-precheck
cargo build-sbf --features bpf-entrypoint
```

Result artifact:

- `target/deploy/hookie_precheck.so`

## Vanity keypair and deploy

Generate vanity keypair (replace prefix):

```bash
solana-keygen grind --starts-with <prefix>:1
```

Deploy to mainnet:

```bash
solana program deploy target/deploy/hookie_precheck.so \
  --program-id /path/to/vanity-keypair.json \
  --upgrade-authority /path/to/upgrade-authority.json \
  --url https://api.mainnet-beta.solana.com
```

## Post-deploy checklist

Record and commit these values in repo/docs:

- `program_id`
- `upgrade_authority`
- artifact path + checksum for `.so`
- commit hash used for deployment

Then update SDK default program id in:

- `src/instruction/hookie_precheck.rs` (`DEFAULT_PRECHECK_PROGRAM_ID`)
- `onchain/hookie-precheck/src/lib.rs` (`ID`)
