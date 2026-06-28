//! Parse errors.

use crate::span::Span;

/// A parse error with location.
#[derive(Clone, Debug)]
pub struct ParseError {
    pub message: String,
    pub span: Span,
}

/// What was left open when a body ran to end of input without its close (§5.5.6).  A one-to-one mirror of `FrameRole`
/// over the body-shaped cases — `FrameRole::unterminated_kind()` is the identity projection — with room to grow
/// frameless variants (an unterminated `__DATA__` section, say) that no role stands behind.  The organizing axis is the
/// *condition*, "what didn't terminate," not the lexing context: a future frameless case attaches here as its own
/// variant rather than scattering the one concept across unrelated error enums that merely reuse the word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnterminatedKind {
    Format,
    Prototype,
    Signature,
    String,
    LiteralString,
    QuoteWords,
    Heredoc,
    LiteralHeredoc,
    Regex,
    LiteralRegex,
    SubstRegex,
    LiteralSubstRegex,
    SubstReplacement,
    LiteralSubstReplacement,
    EvalSubstReplacement,
    TrSearchList,
    TrReplacementList,
}

impl ParseError {
    pub fn new(message: impl Into<String>, span: Span) -> Self {
        ParseError { message: message.into(), span }
    }

    /// Build the error for a body that reached end of input without closing.  `token` fills the message slot for the
    /// string and heredoc families — the close character or the heredoc tag — and is ignored by every other kind.  The
    /// `|` groupings here are message coincidences, not structure: several kinds render the same wording, an
    /// implementation detail of this match rather than a claim about the roles (§5.5.6).
    pub fn unterminated(kind: UnterminatedKind, token: Option<&str>, span: Span) -> Self {
        use UnterminatedKind::*;
        let message = match kind {
            String | LiteralString | QuoteWords | Heredoc | LiteralHeredoc => {
                format!("Can't find string terminator \"{}\" anywhere before EOF", token.unwrap_or_default())
            }
            Regex | LiteralRegex => "Search pattern not terminated".to_string(),
            SubstRegex | LiteralSubstRegex => "Substitution pattern not terminated".to_string(),
            SubstReplacement | LiteralSubstReplacement | EvalSubstReplacement => "Substitution replacement not terminated".to_string(),
            TrSearchList => "Transliteration pattern not terminated".to_string(),
            TrReplacementList => "Transliteration replacement not terminated".to_string(),
            Format => "Format not terminated".to_string(),
            Prototype => "Prototype not terminated".to_string(),
            // Signatures are not yet framed, so this arm is unreached.  perl emits a parameter-syntax error rather than
            // a clean "not terminated" here, so the wording is deferred until the signature frame lands (§5.5.6).
            Signature => "Signature not terminated".to_string(),
        };
        ParseError::new(message, span)
    }
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "error at byte {}: {}", self.span.start, self.message)
    }
}

impl std::error::Error for ParseError {}
