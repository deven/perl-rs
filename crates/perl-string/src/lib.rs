//! Perl string types — octet sequences with an optional UTF-8 flag.

mod perl_string;
mod small_string;
mod string_slot;

pub use perl_string::PerlString;
pub use small_string::{SMALL_STRING_MAX, SmallString};
pub use string_slot::{PerlStringSlot, SLOT_INLINE_MAX};
