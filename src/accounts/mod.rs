//! Accounts for the Pump.fun Solana Program
//!
//! This module contains the definitions for the accounts used by the Pump.fun program.
//!
//! # Accounts
//!
//! - `BondingCurve`: Represents a bonding curve account.
//! - `Global`: Represents the global configuration account.

mod bonding_curve;
mod global;
mod amm_pool;
mod amm_global;

pub use bonding_curve::*;
pub use global::*;
pub use amm_pool::*;
pub use amm_global::*;