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
//! # Vocabulary
//!
//! A few words recur throughout these modules with a specific meaning:
//!
//! - **Envelope** — the sixteen bytes a value occupies: one discriminant byte and fifteen of payload.  `Value`,
//!   `ScalarPayload`, `ScalarCell`, and `PerlString` are all exactly this size, and assertions enforce it.
//! - **Tag** — the discriminant byte.  It carries more than which variant is present: the utf8 flag, the
//!   numification-warning bit, taint, the storage form, and (for strings) what is known about validity are all folded
//!   into *which variant* a value is, rather than stored as fields.  That folding is why a taint bit costs nothing:
//!   `Integer` and `IntegerTainted` are two variants, not one variant with a byte.
//! - **Tier** — which of the three storage forms a string uses: fifteen payload bytes directly, nibble-packed for
//!   sixteen to thirty characters of digit-dense text, or a heap buffer.
//! - **Band** — the length range a tier accepts.  The packed band is sixteen to thirty characters, established by the
//!   tier selector rather than checked by the encoder.
//! - **Scan state** — what is known about a string's content without re-reading it: whether it is ASCII, valid UTF-8,
//!   within Latin-1 range, malformed.  States only narrow as facts are learned; they never widen back to unknown.
//! - **A fused pass** — one traversal that computes several facts at once (validity, range, character count) where
//!   separate passes would re-read the same bytes.
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
mod numeric;

pub mod containers;
pub mod cow_buffer;
pub mod heap;
pub mod scalar;
pub mod string;
pub mod value;
