//! Parser diagnostics: the internal error type used by the Pratt parser.
//!
//! This module exists to be the *single seam* through which future diagnostic
//! improvements (span attachment, "expected one of" rendering, error recovery)
//! can be added without touching the Pratt parser internals.
//!
//! Design rules:
//! 1. The Pratt parser produces `ExprParseError` values only. It never
//!    formats messages, never prints, never holds source files.
//! 2. The conversion to the public `CompilationError` happens at exactly
//!    one call site: the `PestExpr::parse` adapter in `parser/mod.rs`.
//! 3. Adding span support later is a per-variant field addition. The Pratt
//!    parser's control flow does not change.
//!
//! Span-free in v1 by design. The variant shapes anticipate the addition
//! of a `span: Span` field; today, the conversion site synthesizes a
//! span-less error and a string message.

use std::fmt;

use crate::lexer::Tok;

/// The internal error type produced by the Pratt parser.
///
/// Every variant carries a `&'static [&'static str]` of "expected" token
/// names where applicable, so a future diagnostic system can render
/// "expected one of: ..." with no extra work. Today, [`to_human_message`]
/// joins these into a string for `CompilationError::ParseError`.
#[derive(Debug, Clone)]
#[allow(dead_code)] // variants will be constructed by diagnostic work in a follow-up
pub(crate) enum ExprParseError {
    /// A token was found where it was not expected.
    UnexpectedToken {
        /// The token that was actually present.
        found: Tok,
        /// Human-readable names of what would have been acceptable.
        expected: &'static [&'static str],
    },
    /// The token stream ended before the expression was complete.
    UnexpectedEof {
        /// Human-readable names of what would have been acceptable.
        expected: &'static [&'static str],
    },
    /// A free-form parser-internal error. Used for cases that don't fit the "wrong token" / "ran
    /// out" shape (e.g. type annotations that look like expressions but aren't).
    Custom(String),
}

impl ExprParseError {
    /// Render this error as a human-readable string.
    ///
    /// This is the *only* place that turns structured error data into a
    /// string.
    ///
    /// TODO: replace this with a structured rendering (e.g. a `miette::Report`) without changing
    /// the parser.
    pub(crate) fn to_human_message(&self) -> String {
        match self {
            Self::UnexpectedToken { found, expected } => {
                let expected = expected.join(", ");
                format!("unexpected token `{found:?}`, expected one of: {expected}")
            }
            Self::UnexpectedEof { expected } => {
                format!(
                    "unexpected end of expression, expected one of: {}",
                    expected.join(", ")
                )
            }
            Self::Custom(msg) => msg.clone(),
        }
    }
}

impl fmt::Display for ExprParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_human_message())
    }
}

/// Map a `Tok` kind to a short human-readable name for error messages.
///
/// Returns `'static` strings so `ExprParseError` can carry them as
/// `&'static [&'static str]` without allocation. Used by the binding-power
/// helpers in `expr_pratt.rs` to populate the `expected` list.
#[allow(dead_code)] // seam: will be called from diagnostic work in a follow-up
pub(crate) fn tok_name(kind: &Tok) -> &'static str {
    match kind {
        Tok::LParen => "`(`",
        Tok::RParen => "`)`",
        Tok::LBracket => "`[`",
        Tok::RBracket => "`]`",
        Tok::LBrace => "`{`",
        Tok::RBrace => "`}`",
        Tok::Comma => "`,`",
        Tok::Semi => "`;`",
        Tok::Dot => "`.`",
        Tok::Colon => "`:`",
        Tok::Ellipsis => "`...`",
        Tok::Plus => "`+`",
        Tok::Minus => "`-`",
        Tok::Star => "`*`",
        Tok::Slash => "`/`",
        Tok::Percent => "`%`",
        Tok::EqEq => "`==`",
        Tok::NotEq => "`!=`",
        Tok::LtEq => "`<=`",
        Tok::GtEq => "`>=`",
        Tok::Lt => "`<`",
        Tok::Gt => "`>`",
        Tok::AndAnd => "`&&`",
        Tok::OrOr => "`||`",
        Tok::Bang => "`!`",
        Tok::Amp => "`&`",
        Tok::Pipe => "`|`",
        Tok::Caret => "`^`",
        Tok::Tilde => "`~`",
        Tok::Shl => "`<<`",
        Tok::Shr => "`>>`",
        Tok::PlusPlus => "`++`",
        Tok::MinusMinus => "`--`",
        Tok::ShlEq => "`<<=`",
        Tok::ShrEq => "`>>=`",
        Tok::PlusEq => "`+=`",
        Tok::MinusEq => "`-=`",
        Tok::StarEq => "`*=`",
        Tok::SlashEq => "`/=`",
        Tok::PercentEq => "`%=`",
        Tok::AmpEq => "`&=`",
        Tok::PipeEq => "`|=`",
        Tok::CaretEq => "`^=`",
        Tok::Assign => "`=`",
        Tok::KwIf => "`if`",
        Tok::KwThen => "`then`",
        Tok::KwElse => "`else`",
        Tok::KwTrue => "`true`",
        Tok::KwFalse => "`false`",
        Tok::KwNull => "`null`",
        Tok::KwThis => "`this`",
        Tok::KwSelf => "`Self`",
        Tok::KwSizeof => "`sizeof`",
        Tok::KwDefined => "`defined`",
        Tok::KwUnsafe => "`unsafe`",
        Tok::KwStatic => "`static`",
        Tok::KwImplicit => "`implicit`",
        Tok::KwEmpty => "`empty`",
        Tok::KwStruct => "`struct`",
        Tok::IntLit(_) => "integer literal",
        Tok::FloatLit(_) => "float literal",
        Tok::CharLit(_) => "char literal",
        Tok::StringLit(_) => "string literal",
        Tok::Ident(_) => "identifier",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unexpected_token_renders_expected_list() {
        let err = ExprParseError::UnexpectedToken {
            found: Tok::Plus,
            expected: &["identifier", "integer literal", "string literal"],
        };
        let msg = err.to_human_message();
        assert!(msg.contains("Plus"), "msg: {msg}");
        assert!(msg.contains("identifier"), "msg: {msg}");
        assert!(msg.contains("integer literal"), "msg: {msg}");
        assert!(msg.contains("string literal"), "msg: {msg}");
    }

    #[test]
    fn unexpected_eof_renders_expected_list() {
        let err = ExprParseError::UnexpectedEof {
            expected: &[")", ",", "binary operator"],
        };
        let msg = err.to_human_message();
        assert!(msg.contains("end of expression"), "msg: {msg}");
        assert!(msg.contains(")"), "msg: {msg}");
        assert!(msg.contains(","), "msg: {msg}");
    }

    #[test]
    fn custom_message_passes_through() {
        let err = ExprParseError::Custom("nope".to_string());
        assert_eq!(err.to_human_message(), "nope");
    }

    #[test]
    fn tok_name_is_stable() {
        // Sanity check: a few representative names.
        assert_eq!(tok_name(&Tok::Plus), "`+`");
        assert_eq!(tok_name(&Tok::LParen), "`(`");
        assert_eq!(tok_name(&Tok::KwIf), "`if`");
        assert_eq!(tok_name(&Tok::IntLit(0)), "integer literal");
        assert_eq!(tok_name(&Tok::StringLit(String::new())), "string literal");
    }
}
