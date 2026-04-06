//! Parse errors.

use crate::span::Span;

/// A parse error with location.
#[derive(Clone, Debug)]
pub struct ParseError {
    pub message: String,
    pub span: Span,
}

impl ParseError {
    pub fn new(message: impl Into<String>, span: Span) -> Self {
        ParseError { message: message.into(), span }
    }
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "error at byte {}: {}", self.span.start, self.message)
    }
}

impl std::error::Error for ParseError {}
