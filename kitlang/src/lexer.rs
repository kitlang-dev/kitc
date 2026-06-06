//! Tokenizer for Kit expressions.
//!
//! Tokenizes Kit expressions into a flat stream for the Pratt parser.
//! Pest handles program/declaration grammars; this covers only expressions.
//! Tokens carry byte spans for diagnostics.

use logos::{Lexer, Logos};
use std::ops::Range;

/// Byte range in source. Promotable to a richer `Span` type later.
pub type Span = Range<usize>;

/// Token with its source span.
#[derive(Debug, Clone, PartialEq)]
pub struct SpannedTok {
    pub kind: Tok,
    pub span: Span,
}

/// Token kind for Kit expressions. Keywords absent by design (handled by pest).
/// Multi-char tokens declared first for explicit priority.
#[derive(Logos, Debug, Clone, PartialEq)]
#[logos(skip r"[ \t\r\n]+")]
#[logos(skip(r"//[^\r\n]*", allow_greedy = true))]
#[logos(skip(r"/\*([^*]|\*+[^*/])*\*/"))]
pub enum Tok {
    // --- Punctuation ---
    #[token("(")]
    LParen,
    #[token(")")]
    RParen,
    #[token("[")]
    LBracket,
    #[token("]")]
    RBracket,
    #[token("{")]
    LBrace,
    #[token("}")]
    RBrace,
    #[token(",")]
    Comma,
    #[token(";")]
    Semi,
    #[token(".")]
    Dot,
    #[token(":")]
    Colon,
    #[token("...")]
    Ellipsis,

    // --- Arithmetic ---
    #[token("+")]
    Plus,
    #[token("-")]
    Minus,
    #[token("*")]
    Star,
    #[token("/")]
    Slash,
    #[token("%")]
    Percent,

    // --- Comparison / equality ---
    // Longer forms first.
    #[token("==")]
    EqEq,
    #[token("!=")]
    NotEq,
    #[token("<=")]
    LtEq,
    #[token(">=")]
    GtEq,
    #[token("<")]
    Lt,
    #[token(">")]
    Gt,

    // --- Logical (longer first) ---
    #[token("&&")]
    AndAnd,
    #[token("||")]
    OrOr,
    #[token("!")]
    Bang,

    // --- Bitwise ---
    // Note: `&&` and `||` are matched before `&` and `|` by virtue of being
    // longer; Logos longest-match ensures we never see a stray single `&`
    // that's actually the start of `&&`. Same for `<<` / `>>` below.
    #[token("&")]
    Amp,
    #[token("|")]
    Pipe,
    #[token("^")]
    Caret,
    #[token("~")]
    Tilde,
    #[token("<<")]
    Shl,
    #[token(">>")]
    Shr,

    // --- Increment / decrement ---
    #[token("++")]
    PlusPlus,
    #[token("--")]
    MinusMinus,

    // --- Assignment (longer forms first) ---
    #[token("<<=")]
    ShlEq,
    #[token(">>=")]
    ShrEq,
    #[token("+=")]
    PlusEq,
    #[token("-=")]
    MinusEq,
    #[token("*=")]
    StarEq,
    #[token("/=")]
    SlashEq,
    #[token("%=")]
    PercentEq,
    #[token("&=")]
    AmpEq,
    #[token("|=")]
    PipeEq,
    #[token("^=")]
    CaretEq,
    #[token("=")]
    Assign,

    // --- Keywords that appear in expressions ---
    #[token("if")]
    KwIf,
    #[token("then")]
    KwThen,
    #[token("else")]
    KwElse,
    #[token("true")]
    KwTrue,
    #[token("false")]
    KwFalse,
    #[token("null")]
    KwNull,
    #[token("this")]
    KwThis,
    #[token("Self")]
    KwSelf,
    #[token("sizeof")]
    KwSizeof,
    #[token("defined")]
    KwDefined,
    #[token("unsafe")]
    KwUnsafe,
    #[token("static")]
    KwStatic,
    #[token("implicit")]
    KwImplicit,
    #[token("empty")]
    KwEmpty,
    #[token("struct")]
    KwStruct,

    // --- Literals ---
    // Integer: one or more ASCII digits.
    #[regex(r"[0-9]+", priority = 4, callback = parse_int)]
    IntLit(i64),

    // Float: digits.digits, optional exponent.
    // Must come before IntLit conceptually (no `.` in IntLit) but Logos
    // priority + longest-match handles this; we just give it a higher
    // priority so a `1.0` source never matches as `IntLit(1)` + `.` + `0`.
    #[regex(r"[0-9]+\.[0-9]+([eE][+-]?[0-9]+)?", priority = 5, callback = parse_float)]
    FloatLit(f64),

    // Char literal: single character or escape between single quotes.
    #[regex(r"'(\\.|[^'\\])'", priority = 6, callback = parse_char)]
    CharLit(char),

    // String literal: any character or escape between double quotes.
    #[regex(r#""(\\.|[^"\\])*""#, priority = 6, callback = parse_string)]
    StringLit(String),

    // Identifier: must come last (lowest priority) and must NOT match the
    // keyword forms. The grammar's `identifier` rule uses negative lookahead
    // for the reserved words; we mirror that by giving keywords priority 2
    // and identifier priority 1.
    #[regex(r"[a-zA-Z_][a-zA-Z0-9_]*", priority = 1, callback = parse_ident)]
    Ident(String),
}

fn parse_int(lex: &mut Lexer<Tok>) -> Option<i64> {
    lex.slice().parse::<i64>().ok()
}

fn parse_float(lex: &mut Lexer<Tok>) -> Option<f64> {
    lex.slice().parse::<f64>().ok()
}

fn parse_char(lex: &mut Lexer<Tok>) -> Option<char> {
    // The slice includes the surrounding single quotes; strip them.
    let s = lex.slice();
    let inner = &s[1..s.len() - 1];
    unescape_char(inner)
}

fn parse_string(lex: &mut Lexer<Tok>) -> Option<String> {
    let s = lex.slice();
    let inner = &s[1..s.len() - 1];
    Some(unescape_str(inner))
}

fn parse_ident(lex: &mut Lexer<Tok>) -> Option<String> {
    Some(lex.slice().to_string())
}

fn unescape_char(s: &str) -> Option<char> {
    let mut chars = s.chars();
    let c = chars.next()?;
    if c == '\\' {
        let esc = chars.next()?;
        Some(match esc {
            'n' => '\n',
            'r' => '\r',
            't' => '\t',
            '\\' => '\\',
            '\'' => '\'',
            '"' => '"',
            '0' => '\0',
            other => other,
        })
    } else {
        Some(c)
    }
}

fn unescape_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(esc) = chars.next() {
                match esc {
                    'n' => out.push('\n'),
                    'r' => out.push('\r'),
                    't' => out.push('\t'),
                    '\\' => out.push('\\'),
                    '\'' => out.push('\''),
                    '"' => out.push('"'),
                    '0' => out.push('\0'),
                    other => out.push(other),
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Tokenize source into `Vec<SpannedTok>`. Drops whitespace, comments,
/// and unrecognized characters. Returns tokens in source order; EOF is
/// end of vector.
pub fn tokenize(source: &str) -> Vec<SpannedTok> {
    Tok::lexer(source)
        .spanned()
        .filter_map(|(res, span)| res.ok().map(|kind| SpannedTok { kind, span }))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(source: &str) -> Vec<Tok> {
        tokenize(source).into_iter().map(|t| t.kind).collect()
    }

    #[test]
    fn integer_literals() {
        assert_eq!(kinds("0"), vec![Tok::IntLit(0)]);
        assert_eq!(kinds("42"), vec![Tok::IntLit(42)]);
        assert_eq!(kinds("1234567890"), vec![Tok::IntLit(1234567890)]);
    }

    #[test]
    fn float_literals() {
        assert_eq!(kinds("3.14"), vec![Tok::FloatLit(3.14)]);
        assert_eq!(kinds("1.0e10"), vec![Tok::FloatLit(1.0e10)]);
        assert_eq!(kinds("2.5E-3"), vec![Tok::FloatLit(2.5E-3)]);
    }

    #[test]
    fn char_and_string_literals() {
        assert_eq!(kinds("'a'"), vec![Tok::CharLit('a')]);
        assert_eq!(kinds("'\\n'"), vec![Tok::CharLit('\n')]);
        assert_eq!(
            kinds(r#""hello""#),
            vec![Tok::StringLit("hello".to_string())]
        );
        assert_eq!(kinds(r#""a\nb""#), vec![Tok::StringLit("a\nb".to_string())]);
    }

    #[test]
    fn identifiers_and_keywords() {
        assert_eq!(kinds("foo"), vec![Tok::Ident("foo".to_string())]);
        assert_eq!(
            kinds("foo_bar_123"),
            vec![Tok::Ident("foo_bar_123".to_string())]
        );
        // Keywords take priority over the identifier regex.
        assert_eq!(kinds("if"), vec![Tok::KwIf]);
        assert_eq!(kinds("Self"), vec![Tok::KwSelf]);
        assert_eq!(kinds("null"), vec![Tok::KwNull]);
    }

    #[test]
    fn operators_longest_match() {
        // `&&` must match as a single token, not two `&`s.
        assert_eq!(kinds("&&"), vec![Tok::AndAnd]);
        // `<<=` must match as a single token, not `<<` + `=`.
        assert_eq!(kinds("<<="), vec![Tok::ShlEq]);
        // `==` must match as a single token, not two `=`s.
        assert_eq!(kinds("=="), vec![Tok::EqEq]);
    }

    #[test]
    fn arithmetic_operators() {
        assert_eq!(
            kinds("a + b * c"),
            vec![
                Tok::Ident("a".to_string()),
                Tok::Plus,
                Tok::Ident("b".to_string()),
                Tok::Star,
                Tok::Ident("c".to_string()),
            ]
        );
    }

    #[test]
    fn whitespace_is_skipped() {
        assert_eq!(kinds("  a \t +\n b  "), kinds("a+b"));
    }

    #[test]
    fn line_comments_are_skipped() {
        assert_eq!(kinds("a // ignore me\n+ b"), kinds("a+b"));
    }

    #[test]
    fn block_comments_are_skipped() {
        assert_eq!(kinds("a /* hi */ + b"), kinds("a+b"));
        assert_eq!(kinds("a/*\nmulti\nline\n*/+b"), kinds("a+b"));
    }

    #[test]
    fn spans_are_byte_accurate() {
        let toks = tokenize("  a + b  ");
        assert_eq!(toks[0].span, 2..3); // 'a'
        assert_eq!(toks[1].span, 4..5); // '+'
        assert_eq!(toks[2].span, 6..7); // 'b'
    }

    #[test]
    fn empty_source_produces_no_tokens() {
        assert!(tokenize("").is_empty());
    }

    #[test]
    fn unknown_characters_are_dropped() {
        // `$` is not a Kit token; the lexer drops it. This mirrors how pest
        // would surface the error at the grammar level, which is upstream
        // of the Pratt parser and not its concern.
        assert_eq!(
            kinds("a $ b"),
            vec![Tok::Ident("a".to_string()), Tok::Ident("b".to_string()),]
        );
    }
}
