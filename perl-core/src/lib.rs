//! Perl core types.
//!
//! This crate provides the fundamental value representation:
//!
//! - [`Value`] — the top-level enum with compact variants for common cases
//!   (integers, floats, small strings) and `Arc`-wrapped variants for shared
//!   values (full scalars, arrays, hashes, code, regex).
//!
//! - [`Scalar`] — the full Perl SV: parallel int/num/string caches with
//!   flag-driven validity, magic chain, stash for blessed objects.
//!
//! - [`ScalarFlags`] — bitflags for cache validity (INT_VALID, NUM_VALID,
//!   STR_VALID, REF_VALID) and metadata (READONLY, UTF8, TAINT, MAGICAL,
//!   WEAK).
//!
//! - Type aliases: `Sv`, `Av`, `Hv` for `Arc<RwLock<T>>` wrapped types.
//!
//! # Design Principles
//!
//! - **Compact by default.**  `Value::Int(42)` is 8 bytes, no heap allocation.
//!   Only values that need shared identity, multi-representation caching, or
//!   magic are upgraded to a full `Scalar` behind `Arc<RwLock<>>`.
//!
//! - **Upgrade, never downgrade.**  Once a value becomes `Value::Scalar(Sv)`,
//!   it stays that way.  Identity via `Arc` address must be preserved.
//!
//! - **Flag-driven coercion.**  The `Scalar` struct uses `ScalarFlags` to track
//!   which representation slots are valid.  The coercion engine checks flags
//!   for the fast path and caches new representations lazily.

pub mod flags;
pub mod perl_string;
pub mod scalar;
pub mod small_string;
pub mod string_slot;
pub mod value;

pub use flags::ScalarFlags;
pub use perl_string::PerlString;
pub use scalar::Scalar;
pub use small_string::{SMALL_STRING_MAX, SmallString};
pub use string_slot::{PerlStringSlot, SLOT_INLINE_MAX};
pub use value::{Av, Hv, Sv, Value};
