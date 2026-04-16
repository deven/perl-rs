//! Perl lexer, Pratt parser, and AST.
//!
//! The public API is: give source bytes, get an AST.
//!
//! ```
//! use perl_parser::parse;
//!
//! let program = parse(b"my $x = 42; print $x;").unwrap();
//! assert_eq!(program.statements.len(), 2);
//! ```

#![deny(clippy::unwrap_used, clippy::expect_used)]

pub mod ast;
pub mod error;
pub mod keyword;
pub mod source;
pub mod span;
pub mod symbols;
pub mod token;

pub(crate) mod lexer;
pub mod parser;

use ast::Program;
use error::ParseError;

/// Parse Perl source into an AST.
pub fn parse(src: &[u8]) -> Result<Program, ParseError> {
    let mut p = parser::Parser::new(src)?;
    p.parse_program()
}
