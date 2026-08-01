//! Perl core types: the value representation and the heap that owns it.
//!
//! - [`value`] — `Value`, the universal slot value, and `ScalarPayload`, the payload of a promoted scalar.  Both carry
//!   compact scalar cases inline and hold references to shared nodes.
//! - [`scalar`] — `ScalarCell`, a promoted scalar: the payload plus the identity-level state that survives assignment.
//! - [`containers`] — `PerlArray` and `PerlHash`.
//! - [`string`] — `PerlString`, an octet sequence with its per-string state: the utf8 flag as a semantic claim, the
//!   numification-warning bit, taint, and the scan cache recording what is known about Rust-level validity.
//! - [`cow_buffer`] — the reference-counted copy-on-write buffer behind heap strings.
//! - [`heap`] — `HeapArc`/`HeapWeak`, the façade over shared ownership that the slab backend will replace.
//!
//! Two private modules hold representations that never allocate: `inline` for content of fifteen payload bytes or
//! fewer, `packed` for the nibble encoding that carries sixteen to thirty characters of digit-dense text.
//!
//! # Design principles
//!
//! - **Compact by default.**  A value occupies its slot directly — array element, hash value, pad entry — and only
//!   values needing shared identity, per-identity state, or magic are promoted to a cell behind a shared reference.
//!
//! - **Upgrade, never downgrade.**  Once a value is promoted, it stays promoted: its address is its identity.
//!
//! - **Representation is canonical.**  Content determines its representation uniquely, so equal Perl strings are equal
//!   representations — which is what lets equality and hashing work on the representation rather than by decoding
//!   first.
//!
//! - **State that survives assignment lives outside the payload.**  Blessing and readonly are properties of the
//!   container; taint, caches, and the warning bit travel with the value.

#![deny(clippy::unwrap_used, clippy::expect_used)]

mod inline;
mod packed;

pub mod containers;
pub mod cow_buffer;
pub mod heap;
pub mod scalar;
pub mod string;
pub mod value;
