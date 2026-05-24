use logos::Logos;
use std::num::{ParseFloatError, ParseIntError};

#[derive(Debug, Clone, PartialEq, Default)]
pub enum LexError {
    ParseInt,
    ParseFloat,
    #[default]
    Other,
}

impl From<ParseIntError> for LexError {
    fn from(_: ParseIntError) -> Self {
        LexError::ParseInt
    }
}

impl From<ParseFloatError> for LexError {
    fn from(_: ParseFloatError) -> Self {
        LexError::ParseFloat
    }
}

#[derive(Logos, Debug, PartialEq)]
#[logos(error = LexError)]
pub enum Token {
    #[token("function")]
    Function,
    #[token("import")]
    Import,
    #[token("using")]
    Using,
    #[token("struct")]
    Struct,
    #[token("enum")]
    Enum,
    #[token("abstract")]
    Abstract,
    #[token("trait")]
    Trait,
    #[token("implement")]
    Implement,
    #[token("var")]
    Var,
    #[token("const")]
    Const,
    #[token("as")]
    As,
    #[token("match")]
    Match,
    #[token("for")]
    For,
    #[token("while")]
    While,
    #[token("return")]
    Return,
    #[token("sizeof")]
    Sizeof,
    #[token("include")]
    Include,
    #[token("static")]
    Static,
    #[token("public")]
    Public,
    #[token("private")]
    Private,
    #[token("if")]
    If,
    #[token("else")]
    Else,
    #[token("then")]
    Then,
    #[token("true")]
    True,
    #[token("false")]
    False,
    #[token("null")]
    Null,
    #[token("this")]
    This,

    #[token("{")]
    LeftBrace,
    #[token("}")]
    RightBrace,
    #[token("(")]
    LeftParen,
    #[token(")")]
    RightParen,
    #[token(";")]
    Semicolon,
    #[token(",")]
    Comma,
    #[token(":")]
    Colon,
    #[token(".")]
    Dot,
    #[token("=")]
    Equals,
    #[token("~")]
    Tilde,
    #[token("...")]
    TripleDot,

    #[regex(r#""([^"\\]|\\.)*""#, |lex| {
        let s = lex.slice();
        let s = &s[1..s.len()-1]; // Remove surrounding quotes
        s.replace("\\\"", "\"") // Handle escaped quotes
    })]
    StringLiteral(String),
    #[regex(r"[a-zA-Z_][a-zA-Z0-9_]*", |lex| lex.slice().to_string())]
    Identifier(String),
    #[regex(r"[0-9]+", |lex| lex.slice().parse().map_err(|_| LexError::ParseInt))]
    IntLiteral(i64),
    #[regex(r"[0-9]+\.[0-9]+", |lex| lex.slice().parse().map_err(|_| LexError::ParseFloat))]
    FloatLiteral(f64),

    // Skip whitespace and comments
    #[regex(r"[ \t\n\f]+", logos::skip)]
    #[regex(r"//[^\n]*", logos::skip)]
    #[regex(r"/\*([^*]|\*[^/])*\*/", logos::skip)]
    Whitespace,
}
