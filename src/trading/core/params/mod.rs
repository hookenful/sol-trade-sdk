//! DEX protocol parameter types and [`SwapParams`].

mod bonk;
mod cached_quote;
#[cfg(test)]
mod cached_quote_mainnet;
mod cpmm_creator_fee_preparation;
mod dex_swap;
mod hop_spot;
mod meteora_damm_v2;
mod meteora_dbc;
mod meteora_dlmm;
mod pumpfun;
mod pumpswap;
mod pumpswap_preparation;
mod pumpswap_route_search;
pub(crate) use pumpswap_route_search::{canonical_conversion_pools, ConversionGraph};
mod raydium_amm_v4;
mod raydium_clmm;
mod raydium_cpmm;
mod stonkfun_preparation;
mod stonkfun_quote_route;
mod stonkfun_via_sol;
mod subscription_cache;
mod whirlpool;

pub use bonk::{BonkParams, LaunchLabParams, StonkFunParams};
pub use cached_quote::{
    CachedQuoteRequest, CachedQuoteWithImpact, CachedRouteStep, CachedSwapQuote,
};
pub use cpmm_creator_fee_preparation::PreparedCpmmCreatorFeeCollection;
pub use dex_swap::{DexParamEnum, SenderConcurrencyConfig, SwapParams};
pub use hop_spot::HopSpot;
pub use meteora_damm_v2::MeteoraDammV2Params;
pub use meteora_dbc::{DbcQuoteState, DbcTransferHook, MeteoraDbcParams};
#[cfg(test)]
pub(crate) use meteora_dlmm::fixture_pair as dlmm_fixture_pair;
pub use meteora_dlmm::{
    DlmmHopQuote, DlmmQuoteAccounts, DlmmQuoteState, MeteoraDlmmParams, QUOTE_BIN_ARRAYS,
};
pub use pumpfun::{PumpFunParams, PumpFunV3Params};
pub use pumpswap::PumpSwapParams;
pub use pumpswap_preparation::PreparedPumpSwapTrade;
pub use pumpswap_route_search::{
    PumpSwapRouteRejection, PumpSwapRouteSearch, PumpSwapRouteSelection,
};
pub use raydium_amm_v4::RaydiumAmmV4Params;
pub use raydium_clmm::{
    ClmmHopQuote, ClmmQuoteAccounts, ClmmQuoteState, RaydiumClmmParams, CLOCK_SYSVAR,
    QUOTE_TICK_ARRAYS,
};
pub use raydium_cpmm::{
    token_transfer_fee_for_epoch, CpmmQuoteAccounts, RaydiumCpmmParams, TokenTransferFee,
};
pub use stonkfun_preparation::PreparedStonkFunTrade;
pub use stonkfun_quote_route::{
    PumpFunQuoteHop, PumpFunQuoteRoute, PumpFunQuoteVenue, PumpSwapQuoteHop, PumpSwapQuoteRoute,
    PumpSwapQuoteVenue, StonkFunQuoteHop, StonkFunQuoteHopPreview, StonkFunQuoteRoute,
    StonkFunQuoteRoutePreview, StonkFunQuoteVenue,
};
pub use stonkfun_via_sol::{
    StonkFunMemeLeg, StonkFunSolHop, StonkFunViaQuoteParams, StonkFunViaSolParams,
};
pub use subscription_cache::{
    CacheReadContext, CachedAccount, PoolTradeHint, SubscriptionAccountCache,
};
pub use whirlpool::WhirlpoolParams;
/// User-facing parameters for a graduated StonkFun pool on the external CPMM venue.
pub type StonkFunSwapParams = RaydiumCpmmParams;
