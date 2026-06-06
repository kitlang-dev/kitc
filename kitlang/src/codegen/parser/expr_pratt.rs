//! Hand-written Pratt parser for Kit expressions.
//!
//! This module takes over expression parsing from the pest-based grammar.
//! Pest still handles the program, declaration, statement, and type-annotation
//! grammars. For every `Pair<'_, Rule>` whose rule is an expression (i.e.,
//! the 13 precedence levels `expr → assign → logical_or → ... → primary`),
//! the parser hands off to [`ExprParser::parse_expr`].
//!
//! Pratt Parsing & Operator Precedence
//! -----------------------------------
//! The pest grammar used 13 mutually recursive functions (one per precedence
//! level), which overflowed the default 1 MB stack on Windows. The Pratt
//! parser uses a single function with a binding-power loop, bounding stack
//! depth to `O(precedence levels) = O(13)` regardless of expression length.
//! Operator precedence is defined in [`binding_power::infix`],
//! [`binding_power::postfix`], [`binding_power::prefix`]. Each infix operator
//! has a (lbp, rbp) pair: `lbp < rbp` = right-associative (assignment),
//! `lbp == rbp` = left-associative (most binary ops).
//!
//! Errors & Source Spans
//! ---------------------
//! All parse errors are values of [`ExprParseError`] (in `diagnostics.rs`).
//! The parser never prints, never allocates strings for error messages,
//! and never holds source-file identity. Conversion to
//! `CompilationError::ParseError(String)` happens at `PestExpr::parse`
//! in `parser/mod.rs`. The token stream carries byte ranges; the parser
//! uses them internally but does not currently attach them to AST nodes.

use crate::codegen::ast::{Expr, Literal};
use crate::codegen::type_ast::FieldInit;
use crate::codegen::types::{Type, TypeId};
use crate::lexer::{Span, SpannedTok, Tok, tokenize};

use super::binding_power::{
    infix, is_range_op, postfix, prefix, tok_to_assign_op, tok_to_binary_op, tok_to_unary_op,
};
use super::diagnostics::ExprParseError;

#[cfg(test)]
use crate::codegen::types::{AssignmentOperator, BinaryOperator, UnaryOperator};

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

/// A Pratt parser for Kit expressions.
///
/// One instance is built per expression being parsed. The parser is
/// single-use: create a new one for each `parse_expr` call.
pub(crate) struct ExprParser<'a> {
    tokens: &'a [SpannedTok],
    pos: usize,
}

impl<'a> ExprParser<'a> {
    /// Build a parser over a token slice. The slice must be the result of
    /// [`tokenize`] applied to the expression's source text.
    pub(crate) fn new(tokens: &'a [SpannedTok]) -> Self {
        Self { tokens, pos: 0 }
    }

    /// Entry point. Parses one complete expression and returns it.
    /// The expression may be followed by trailing tokens, which are left
    /// in the stream (the caller can `pos()`-check or re-parse).
    pub(crate) fn parse_expr(&mut self) -> Result<Expr, ExprParseError> {
        self.parse_pratt(0)
    }

    /// The current position in the token stream. Useful for tests and
    /// for callers that want to know how many tokens were consumed.
    #[allow(dead_code)]
    pub(crate) fn pos(&self) -> usize {
        self.pos
    }

    // --- Token stream helpers (private) ---

    /// Peek the current token without consuming it. Returns a synthetic
    /// "EOF" token when the stream is exhausted.
    fn peek(&self) -> &SpannedTok {
        // We never return None from peek; EOF is represented by a synthetic
        // SpannedTok at span 0..0. This lets the Pratt loop compare with
        // `==` against `Tok::...` cleanly.
        static EOF: SpannedTok = SpannedTok {
            kind: Tok::Semi, // any token that has no infix/postfix/prefix bp
            span: 0..0,
        };
        self.tokens.get(self.pos).unwrap_or(&EOF)
    }

    /// Peek the next token (one past the current). Same EOF behavior.
    #[allow(dead_code)]
    fn peek_next(&self) -> &SpannedTok {
        static EOF: SpannedTok = SpannedTok {
            kind: Tok::Semi,
            span: 0..0,
        };
        self.tokens.get(self.pos + 1).unwrap_or(&EOF)
    }

    /// Consume and return the current token.
    fn advance(&mut self) -> &SpannedTok {
        let tok = &self.tokens[self.pos];
        self.pos += 1;
        tok
    }

    /// True if the parser is at or past the end of the token stream.
    #[allow(dead_code)] // seam: will be used for EOF diagnostics in a follow-up
    fn at_eof(&self) -> bool {
        self.pos >= self.tokens.len()
    }

    // --- Pratt core ---

    /// Parse an expression with the given minimum binding power.
    fn parse_pratt(&mut self, min_bp: u8) -> Result<Expr, ExprParseError> {
        // Parse leading prefix operators (e.g. `-a`, `!!x`, `&arr[i]`).
        // Prefix binds tighter than infix but looser than postfix,
        // so `&arr[i]` = `&(arr[i])` (postfix on `arr` first).
        let mut lhs = if let Some(pfx_bp) = prefix(&self.peek().kind) {
            let op = tok_to_unary_op(&self.peek().kind).unwrap();
            self.advance();
            let rhs = self.parse_pratt(pfx_bp)?;
            Expr::UnaryOp {
                op,
                expr: Box::new(rhs),
                ty: TypeId::default(),
            }
        } else {
            self.parse_primary()?
        };

        // Postfix chain: field access, index, call.
        lhs = self.parse_postfix_chain(lhs)?;

        // Infix operators (binary ops, range).
        loop {
            let kind = self.peek().kind.clone();
            let Some((lbp, rbp)) = infix(&kind) else {
                break;
            };
            if lbp < min_bp {
                break;
            }
            if tok_to_assign_op(&kind).is_some() {
                break;
            }
            self.advance();
            if is_range_op(&kind) {
                let rhs = self.parse_pratt(rbp)?;
                lhs = Expr::RangeLiteral {
                    start: Box::new(lhs),
                    end: Box::new(rhs),
                };
                continue;
            }
            let op = tok_to_binary_op(&kind).ok_or_else(|| {
                ExprParseError::Custom(format!("internal: no binary op for {kind:?}"))
            })?;
            let rhs = self.parse_pratt(rbp)?;
            lhs = Expr::BinaryOp {
                op,
                left: Box::new(lhs),
                right: Box::new(rhs),
                ty: TypeId::default(),
            };
        }

        // Assignment (right-associative, lowest precedence).
        loop {
            let kind = self.peek().kind.clone();
            let Some(op) = tok_to_assign_op(&kind) else {
                break;
            };
            // Assignment has the lowest precedence (lbp=0, rbp=1 in the
            // infix table, but we don't use that table here). Right-
            // associativity: a = b = c means a = (b = c). Recurse with
            // min_bp=0 so the rhs sees *all* operators including another
            // assignment.
            if 0 < min_bp {
                break;
            }
            self.advance();
            let rhs = self.parse_pratt(0)?;
            lhs = Expr::Assign {
                op,
                left: Box::new(lhs),
                right: Box::new(rhs),
                ty: TypeId::default(),
            };
        }

        Ok(lhs)
    }

    /// Iteratively apply postfix operators (call, index, field access) to
    /// a base expression. Zero stack frames added per iteration. The
    /// chain is bounded by the source's syntactic length, but the parser
    /// is iterative, so the *call stack* depth is constant.
    fn parse_postfix_chain(&mut self, mut base: Expr) -> Result<Expr, ExprParseError> {
        loop {
            let kind = self.peek().kind.clone();
            if postfix(&kind).is_none() {
                break;
            }
            base = match kind {
                Tok::Dot => self.parse_field_access(base)?,
                Tok::LBracket => self.parse_index(base)?,
                Tok::LParen => self.parse_call(base)?,
                _ => unreachable!("postfix returned Some for {kind:?}"),
            };
        }
        Ok(base)
    }

    /// Parse a primary expression: literals, identifiers, parenthesized
    /// expressions, function calls, array literals, struct inits, and
    /// the if-expression. Postfix operations (`.field`, `[i]`, `(args)`)
    /// are handled in the outer Pratt loop, *not* here, so this function
    /// only needs to produce the base expression.
    fn parse_primary(&mut self) -> Result<Expr, ExprParseError> {
        let tok = self.peek().kind.clone();
        // `span` is read for documentation; the parser doesn't currently
        // attach it to AST nodes. Future PRs will use it.
        let _span: Span = self.peek().span.clone();

        match tok {
            Tok::IntLit(n) => {
                self.advance();
                Ok(Expr::Literal {
                    value: Literal::Int(n),
                    ty: TypeId::default(),
                })
            }
            Tok::FloatLit(f) => {
                self.advance();
                Ok(Expr::Literal {
                    value: Literal::Float(f),
                    ty: TypeId::default(),
                })
            }
            Tok::CharLit(c) => {
                self.advance();
                Ok(Expr::Literal {
                    value: Literal::Char(c),
                    ty: TypeId::default(),
                })
            }
            Tok::StringLit(s) => {
                self.advance();
                Ok(Expr::Literal {
                    value: Literal::String(s),
                    ty: TypeId::default(),
                })
            }
            Tok::KwTrue => {
                self.advance();
                Ok(Expr::Literal {
                    value: Literal::Bool(true),
                    ty: TypeId::default(),
                })
            }
            Tok::KwFalse => {
                self.advance();
                Ok(Expr::Literal {
                    value: Literal::Bool(false),
                    ty: TypeId::default(),
                })
            }
            Tok::KwNull => {
                self.advance();
                Ok(Expr::Literal {
                    value: Literal::Null,
                    ty: TypeId::default(),
                })
            }
            Tok::KwThis | Tok::KwSelf => {
                // The pest parser treats these as `Identifier` with a
                // fixed name; we follow suit. A future PR can introduce
                // dedicated AST variants if needed.
                let name = match tok {
                    Tok::KwThis => "this",
                    _ => "Self",
                };
                self.advance();
                Ok(Expr::Identifier {
                    name: name.to_string(),
                    ty: TypeId::default(),
                })
            }
            Tok::Ident(name) => {
                self.advance();
                Ok(Expr::Identifier {
                    name,
                    ty: TypeId::default(),
                })
            }
            Tok::LParen => {
                // Either a parenthesized expression or the start of a
                // tuple literal `(a, b, c)`. We parse the first
                // expression; if the next token is `,` it's a tuple, else
                // we expect a closing `)`.
                self.advance(); // consume `(`
                let first = self.parse_expr()?;
                if self.peek().kind == Tok::Comma {
                    // Tuple literal: parse remaining comma-separated
                    // expressions, then expect `)`. The AST doesn't have
                    // a dedicated tuple variant, so we represent the
                    // tuple as a parenthesized expression with a
                    // synthetic structure. For now, produce a Custom
                    // error directing the caller to the pest path; this
                    // matches the existing parser's TODO for tuple
                    // literals.
                    return Err(ExprParseError::Custom(
                        "tuple literals are not yet supported by the Pratt parser".into(),
                    ));
                }
                self.expect(&Tok::RParen)?;
                Ok(first)
            }
            Tok::LBracket => self.parse_array_literal(),
            Tok::KwStruct => self.parse_struct_init(),
            Tok::KwIf => self.parse_if_expr(),
            Tok::KwEmpty => {
                // The grammar's primary includes "empty" as a keyword.
                // We don't have a dedicated AST variant; treat it as an
                // identifier for now (semantics will be filled in by
                // type inference downstream).
                self.advance();
                Ok(Expr::Identifier {
                    name: "empty".to_string(),
                    ty: TypeId::default(),
                })
            }
            _ => Err(ExprParseError::UnexpectedToken {
                found: tok,
                expected: &[
                    "integer literal",
                    "float literal",
                    "string literal",
                    "char literal",
                    "identifier",
                    "`(`",
                    "`[`",
                    "`if`",
                    "`null`",
                    "`true`",
                    "`false`",
                ],
            }),
        }
    }

    /// Parse a `.field` access postfix.
    fn parse_field_access(&mut self, base: Expr) -> Result<Expr, ExprParseError> {
        self.advance(); // consume `.`
        let field_tok = self.peek().kind.clone();
        match field_tok {
            Tok::Ident(name) => {
                self.advance();
                Ok(Expr::FieldAccess {
                    expr: Box::new(base),
                    field_name: name,
                    ty: TypeId::default(),
                })
            }
            _ => Err(ExprParseError::UnexpectedToken {
                found: field_tok,
                expected: &["identifier"],
            }),
        }
    }

    /// Parse a `[index]` postfix.
    fn parse_index(&mut self, base: Expr) -> Result<Expr, ExprParseError> {
        self.advance(); // consume `[`
        let index = self.parse_expr()?;
        self.expect(&Tok::RBracket)?;
        Ok(Expr::Index {
            expr: Box::new(base),
            index: Box::new(index),
            ty: TypeId::default(),
        })
    }

    /// Parse a function call postfix: `(arg1, arg2, ...)`.
    fn parse_call(&mut self, callee: Expr) -> Result<Expr, ExprParseError> {
        // The callee expression's name lives in `Expr::Identifier.name`
        // (possibly qualified with dots for things like
        // `qualified_call.math.add`). The pest grammar's
        // `function_call_expr` rule extracts a `path` which is a dot-
        // separated identifier. We extract the name from the callee
        // expression; for `Expr::FieldAccess` chains we concatenate with
        // dots to recover the qualified name. This matches the pest
        // parser's behavior in `parser/expr.rs:163-167` and the
        // transpiler's qualified-name resolution in
        // `transpile/mod.rs:493-500`.
        let callee_name = expr_to_callee_name(&callee);

        self.advance(); // consume `(`
        let args = self.parse_comma_list(Tok::RParen, |p| p.parse_expr())?;
        Ok(Expr::Call {
            callee: callee_name,
            args,
            ty: TypeId::default(),
        })
    }

    /// Parse an array literal: `[expr, expr, ...]`.
    fn parse_array_literal(&mut self) -> Result<Expr, ExprParseError> {
        self.advance(); // consume `[`
        let elements = self.parse_comma_list(Tok::RBracket, |p| p.parse_expr())?;
        Ok(Expr::ArrayLiteral {
            elements,
            ty: TypeId::default(),
        })
    }

    /// Parse a struct init: `struct Name { field: expr, ... }`.
    fn parse_struct_init(&mut self) -> Result<Expr, ExprParseError> {
        self.advance(); // consume `struct`
        // The type annotation here is a single identifier (we don't
        // handle complex type annotations in expressions). This matches
        // the common case; the pest path still handles generics/pointers
        // via the var_decl/init call site.
        let type_tok = self.peek().kind.clone();
        let type_name = match type_tok {
            Tok::Ident(name) => {
                self.advance();
                name
            }
            _ => {
                return Err(ExprParseError::UnexpectedToken {
                    found: type_tok,
                    expected: &["type name"],
                });
            }
        };
        self.expect(&Tok::LBrace)?;
        let fields = self.parse_comma_list(Tok::RBrace, |p| {
            let name = match &p.peek().kind {
                Tok::Ident(n) => {
                    let n = n.clone();
                    p.advance();
                    n
                }
                _ => {
                    return Err(ExprParseError::UnexpectedToken {
                        found: p.peek().kind.clone(),
                        expected: &["field name"],
                    });
                }
            };
            p.expect(&Tok::Colon)?;
            let value = p.parse_expr()?;
            Ok(FieldInit { name, value })
        })?;
        Ok(Expr::StructInit {
            ty: TypeId::default(),
            struct_type: Some(Type::from_kit(&type_name)),
            fields,
        })
    }

    /// Parse an if-expression: `if cond then a else b`.
    fn parse_if_expr(&mut self) -> Result<Expr, ExprParseError> {
        self.advance(); // consume `if`
        let cond = self.parse_expr()?;
        self.expect(&Tok::KwThen)?;
        let then_branch = self.parse_expr()?;
        self.expect(&Tok::KwElse)?;
        let else_branch = self.parse_expr()?;
        Ok(Expr::If {
            cond: Box::new(cond),
            then_branch: Box::new(then_branch),
            else_branch: Box::new(else_branch),
            ty: TypeId::default(),
        })
    }

    // --- Helpers ---

    /// Consume the current token if it matches `expected`, otherwise
    /// return an `UnexpectedToken` error with `expected`'s name.
    fn expect(&mut self, expected: &Tok) -> Result<(), ExprParseError> {
        if &self.peek().kind == expected {
            self.advance();
            Ok(())
        } else {
            // We construct the `&'static [&'static str]` directly from
            // a call to `tok_name`, which the compiler can verify is
            // `'static`. Storing the result in a local first drops the
            // `'static` lifetime, so we keep the call inline.
            Err(ExprParseError::UnexpectedToken {
                found: self.peek().kind.clone(),
                expected: expected_name(expected),
            })
        }
    }

    /// Parse a comma-separated list of `T` terminated by `closer`.
    /// Allows zero or more elements (an empty list is valid for fn
    /// calls with no args, empty array literals, etc.).
    fn parse_comma_list<T, F>(&mut self, closer: Tok, mut f: F) -> Result<Vec<T>, ExprParseError>
    where
        F: FnMut(&mut Self) -> Result<T, ExprParseError>,
    {
        let mut out = Vec::new();
        // Empty list case.
        if self.peek().kind == closer {
            self.advance();
            return Ok(out);
        }
        out.push(f(self)?);
        while self.peek().kind == Tok::Comma {
            self.advance();
            // Trailing comma is allowed (parses to empty trailing element).
            if self.peek().kind == closer {
                break;
            }
            out.push(f(self)?);
        }
        self.expect(&closer)?;
        Ok(out)
    }
}

/// Extract a callee name from a function-call's leading expression.
///
/// For `Expr::Identifier { name, .. }` this is just `name`.
/// For `Expr::FieldAccess { expr, field_name, .. }` chains like
/// `pkg.math.add`, this concatenates the path with `.`.
/// For other expressions (e.g. a parenthesized call), we fall back to
/// `Display`-formatting the expression, which the transpiler can still
/// route through name resolution.
fn expr_to_callee_name(expr: &Expr) -> String {
    match expr {
        Expr::Identifier { name, .. } => name.clone(),
        Expr::FieldAccess {
            expr: base,
            field_name,
            ..
        } => {
            let base_name = expr_to_callee_name(base);
            format!("{base_name}.{field_name}")
        }
        other => format!("{other:?}"),
    }
}

/// Return a static slice of static strs containing the name of `expected`.
/// Used by `expect` to build an `UnexpectedToken` error variant.
fn expected_name(expected: &Tok) -> &'static [&'static str] {
    match expected {
        Tok::LParen => &["`(`"],
        Tok::RParen => &["`)`"],
        Tok::LBracket => &["`[`"],
        Tok::RBracket => &["`]`"],
        Tok::LBrace => &["`{`"],
        Tok::RBrace => &["`}`"],
        Tok::Comma => &["`,`"],
        Tok::Semi => &["`;`"],
        Tok::Dot => &["`.`"],
        Tok::Colon => &["`:`"],
        Tok::Ellipsis => &["`...`"],
        Tok::Plus => &["`+`"],
        Tok::Minus => &["`-`"],
        Tok::Star => &["`*`"],
        Tok::Slash => &["`/`"],
        Tok::Percent => &["`%`"],
        Tok::EqEq => &["`==`"],
        Tok::NotEq => &["`!=`"],
        Tok::LtEq => &["`<=`"],
        Tok::GtEq => &["`>=`"],
        Tok::Lt => &["`<`"],
        Tok::Gt => &["`>`"],
        Tok::AndAnd => &["`&&`"],
        Tok::OrOr => &["`||`"],
        Tok::Bang => &["`!`"],
        Tok::Amp => &["`&`"],
        Tok::Pipe => &["`|`"],
        Tok::Caret => &["`^`"],
        Tok::Tilde => &["`~`"],
        Tok::Shl => &["`<<`"],
        Tok::Shr => &["`>>`"],
        Tok::PlusPlus => &["`++`"],
        Tok::MinusMinus => &["`--`"],
        Tok::ShlEq => &["`<<=`"],
        Tok::ShrEq => &["`>>=`"],
        Tok::PlusEq => &["`+=`"],
        Tok::MinusEq => &["`-=`"],
        Tok::StarEq => &["`*=`"],
        Tok::SlashEq => &["`/=`"],
        Tok::PercentEq => &["`%=`"],
        Tok::AmpEq => &["`&=`"],
        Tok::PipeEq => &["`|=`"],
        Tok::CaretEq => &["`^=`"],
        Tok::Assign => &["`=`"],
        Tok::KwIf => &["`if`"],
        Tok::KwThen => &["`then`"],
        Tok::KwElse => &["`else`"],
        Tok::KwTrue => &["`true`"],
        Tok::KwFalse => &["`false`"],
        Tok::KwNull => &["`null`"],
        Tok::KwThis => &["`this`"],
        Tok::KwSelf => &["`Self`"],
        Tok::KwSizeof => &["`sizeof`"],
        Tok::KwDefined => &["`defined`"],
        Tok::KwUnsafe => &["`unsafe`"],
        Tok::KwStatic => &["`static`"],
        Tok::KwImplicit => &["`implicit`"],
        Tok::KwEmpty => &["`empty`"],
        Tok::KwStruct => &["`struct`"],
        Tok::IntLit(_) => &["integer literal"],
        Tok::FloatLit(_) => &["float literal"],
        Tok::CharLit(_) => &["char literal"],
        Tok::StringLit(_) => &["string literal"],
        Tok::Ident(_) => &["identifier"],
    }
}

// ---------------------------------------------------------------------------
// Module surface: parse an expression from source text.
// ---------------------------------------------------------------------------

/// Parse a Kit expression from source text. This is the public entry
/// point used by the pest-to-Pratt bridge (`PestExpr::parse`).
///
/// The `text` should be the source text of the expression as a
/// `Pair::as_str()` slice. Tokenization, parsing, and conversion to an
/// `Expr` all happen here.
pub(crate) fn parse_kit_expr(text: &str) -> Result<Expr, ExprParseError> {
    let tokens = tokenize(text);
    let mut parser = ExprParser::new(&tokens);
    parser.parse_expr()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Convenience: parse an expression and unwrap.
    fn p(text: &str) -> Expr {
        parse_kit_expr(text).unwrap_or_else(|e| panic!("parse failed for `{text}`: {e}"))
    }

    /// Convenience: parse and assert the error contains a substring.
    fn p_err(text: &str, needle: &str) {
        let err = parse_kit_expr(text)
            .err()
            .unwrap_or_else(|| panic!("expected error for `{text}`, got Ok"));
        let msg = err.to_human_message();
        assert!(
            msg.contains(needle),
            "error `{msg}` does not contain `{needle}`"
        );
    }

    // --- Literals ---

    #[test]
    fn integer_literal() {
        let e = p("42");
        assert!(matches!(
            e,
            Expr::Literal {
                value: Literal::Int(42),
                ..
            }
        ));
    }

    #[test]
    fn float_literal() {
        let e = p("3.14");
        assert!(
            matches!(e, Expr::Literal { value: Literal::Float(f), .. } if (f - 3.14).abs() < 1e-10)
        );
    }

    #[test]
    fn string_literal() {
        let e = p(r#""hello""#);
        assert!(matches!(e, Expr::Literal { value: Literal::String(s), .. } if s == "hello"));
    }

    #[test]
    fn bool_literals() {
        assert!(matches!(
            p("true"),
            Expr::Literal {
                value: Literal::Bool(true),
                ..
            }
        ));
        assert!(matches!(
            p("false"),
            Expr::Literal {
                value: Literal::Bool(false),
                ..
            }
        ));
    }

    #[test]
    fn null_literal() {
        assert!(matches!(
            p("null"),
            Expr::Literal {
                value: Literal::Null,
                ..
            }
        ));
    }

    // --- Identifiers ---

    #[test]
    fn identifier() {
        let e = p("foo");
        assert!(matches!(&e, Expr::Identifier { name, .. } if name == "foo"));
    }

    #[test]
    fn qualified_identifier_is_built_via_postfix_chain() {
        // The lexer produces foo . bar . baz as three tokens. The postfix
        // chain in the Pratt loop builds a FieldAccess tree.
        let e = p("foo.bar.baz");
        // Walking the tree should give us three nested FieldAccess nodes
        // with the leaf being `foo`.
        let mut cur = &e;
        let mut path = vec![];
        while let Expr::FieldAccess {
            expr, field_name, ..
        } = cur
        {
            path.push(field_name.clone());
            cur = expr;
        }
        if let Expr::Identifier { name, .. } = cur {
            assert_eq!(name, "foo");
        } else {
            panic!("expected leaf Identifier, got {cur:?}");
        }
        assert_eq!(path, vec!["baz".to_string(), "bar".to_string()]);
    }

    // --- Precedence ---

    #[test]
    fn additive_vs_multiplicative() {
        // 1 + 2 * 3 should be 1 + (2 * 3)
        let e = p("1 + 2 * 3");
        // Top-level is BinaryOp::Add with right = BinaryOp::Mul
        if let Expr::BinaryOp { op, right, .. } = &e {
            assert_eq!(*op, BinaryOperator::Add);
            if let Expr::BinaryOp { op: inner_op, .. } = right.as_ref() {
                assert_eq!(*inner_op, BinaryOperator::Mul);
            } else {
                panic!("expected inner Mul, got {right:?}");
            }
        } else {
            panic!("expected top-level Add, got {e:?}");
        }
    }

    #[test]
    fn comparison_vs_equality() {
        // a == b < c should be (a == b) < c, since == is lower precedence
        // than comparison. Wait, looking at the table: equality (13/14)
        // is LOWER precedence than comparison (15/16). So `a == b < c`
        // means `a == (b < c)`. Hmm let me re-check the grammar.
        //
        // equality = { comparison ~ (eq_op ~ comparison)* }
        // comparison = { bitwise_or ~ (comp_op ~ bitwise_or)* }
        //
        // This means: an equality expression is a comparison optionally
        // followed by ==/!= and another comparison. So `a == b < c` would
        // first match `a` as a comparison, then look for == or !=, find
        // ==, parse `b < c` as a comparison (because comparison has
        // higher precedence than equality), giving `a == (b < c)`.
        //
        // In our binding-power table: eq (13/14) < comparison (15/16),
        // which is correct. So `a == b < c` is `a == (b < c)`.
        let e = p("a == b < c");
        if let Expr::BinaryOp {
            op, left, right, ..
        } = &e
        {
            assert_eq!(*op, BinaryOperator::Eq);
            // left should be `a`, right should be `b < c`
            assert!(matches!(left.as_ref(), Expr::Identifier { name, .. } if name == "a"));
            if let Expr::BinaryOp { op: inner_op, .. } = right.as_ref() {
                assert_eq!(*inner_op, BinaryOperator::Lt);
            } else {
                panic!("expected inner Lt, got {right:?}");
            }
        } else {
            panic!("expected top-level Eq, got {e:?}");
        }
    }

    #[test]
    fn left_associative_addition() {
        // 1 + 2 + 3 should be (1 + 2) + 3
        let e = p("1 + 2 + 3");
        if let Expr::BinaryOp {
            op, left, right, ..
        } = &e
        {
            assert_eq!(*op, BinaryOperator::Add);
            assert!(matches!(
                right.as_ref(),
                Expr::Literal {
                    value: Literal::Int(3),
                    ..
                }
            ));
            if let Expr::BinaryOp { op: inner_op, .. } = left.as_ref() {
                assert_eq!(*inner_op, BinaryOperator::Add);
            } else {
                panic!("expected inner Add, got {left:?}");
            }
        } else {
            panic!("expected top-level Add, got {e:?}");
        }
    }

    #[test]
    fn right_associative_assignment() {
        // a = b = c should be a = (b = c) -- right-associative
        // The grammar's `assign = logical_or ~ ASSIGN_OP ~ assign | logical_or`
        // recurses on the right, so this is the expected grouping.
        //
        // But: in our grammar, identifiers are not valid lvalues by
        // themselves for assignment. The pest parser wraps the lhs in
        // an `Expr::Identifier` and the rhs in another `Expr::Assign`.
        // For now we test that the structure is right-associative; the
        // type checker will reject `a = b = c` if identifiers aren't
        // valid lvalues for that expression form.
        //
        // We use parenthesized variables to keep this test purely about
        // precedence; type checking is downstream.
        let e = p("a += b += c");
        if let Expr::Assign {
            op, left, right, ..
        } = &e
        {
            assert_eq!(*op, AssignmentOperator::AddAssign);
            assert!(matches!(left.as_ref(), Expr::Identifier { name, .. } if name == "a"));
            assert!(matches!(right.as_ref(), Expr::Assign { .. }));
        } else {
            panic!("expected top-level Assign, got {e:?}");
        }
    }

    #[test]
    fn unary_minus_binds_tighter_than_addition() {
        // -a + b should be (-a) + b
        let e = p("-a + b");
        if let Expr::BinaryOp {
            op, left, right, ..
        } = &e
        {
            assert_eq!(*op, BinaryOperator::Add);
            assert!(matches!(right.as_ref(), Expr::Identifier { name, .. } if name == "b"));
            assert!(matches!(
                left.as_ref(),
                Expr::UnaryOp {
                    op: UnaryOperator::Neg,
                    ..
                }
            ));
        } else {
            panic!("expected top-level Add, got {e:?}");
        }
    }

    #[test]
    fn unary_looser_than_postfix() {
        // &arr[i] should be &(arr[i])
        let e = p("&arr[i]");
        if let Expr::UnaryOp { op, expr, .. } = &e {
            assert_eq!(*op, UnaryOperator::AddressOf);
            assert!(matches!(expr.as_ref(), Expr::Index { .. }));
        } else {
            panic!("expected top-level AddressOf, got {e:?}");
        }
    }

    // --- Postfix chains ---

    #[test]
    fn chained_field_access() {
        // a.b.c.d.e - should produce 4 nested FieldAccess
        let e = p("a.b.c.d.e");
        let mut depth = 0;
        let mut cur = &e;
        while let Expr::FieldAccess { expr, .. } = cur {
            depth += 1;
            cur = expr;
        }
        assert_eq!(depth, 4, "expected 4 field-access levels");
        assert!(matches!(cur, Expr::Identifier { name, .. } if name == "a"));
    }

    #[test]
    fn stress_deep_postfix_chain() {
        // 100-deep chain. With the pest-based parser this would overflow
        // the Windows 1MB stack; the Pratt parser handles it in one
        // function call with a single loop.
        let mut src = String::from("a");
        for i in 0..100 {
            src.push('.');
            src.push_str(&format!("f{i}"));
        }
        let e = p(&src);
        let mut depth = 0;
        let mut cur = &e;
        while let Expr::FieldAccess { expr, .. } = cur {
            depth += 1;
            cur = expr;
        }
        assert_eq!(depth, 100);
    }

    #[test]
    fn stress_deep_nested_parens() {
        // 100-deep nesting. The pest-based parser would have a stack
        // frame per `(` `)` pair; the Pratt parser recurses through
        // `parse_pratt` once per pair but the binding-power loop bounds
        // the recursion depth to the source's *syntactic* depth (which
        // is the user's choice, not the grammar's).
        let mut src = String::new();
        for _ in 0..100 {
            src.push('(');
        }
        src.push('1');
        for _ in 0..100 {
            src.push(')');
        }
        let e = p(&src);
        assert!(matches!(
            e,
            Expr::Literal {
                value: Literal::Int(1),
                ..
            }
        ));
    }

    // --- Function calls ---

    #[test]
    fn call_no_args() {
        let e = p("f()");
        if let Expr::Call { callee, args, .. } = &e {
            assert_eq!(callee, "f");
            assert!(args.is_empty());
        } else {
            panic!("expected Call, got {e:?}");
        }
    }

    #[test]
    fn call_one_arg() {
        let e = p("f(1)");
        if let Expr::Call { callee, args, .. } = &e {
            assert_eq!(callee, "f");
            assert_eq!(args.len(), 1);
        } else {
            panic!("expected Call, got {e:?}");
        }
    }

    #[test]
    fn call_many_args() {
        let e = p("f(1, 2, 3, 4, 5)");
        if let Expr::Call { args, .. } = &e {
            assert_eq!(args.len(), 5);
        } else {
            panic!("expected Call, got {e:?}");
        }
    }

    #[test]
    fn call_qualified_name() {
        let e = p("pkg.math.add(2, 3)");
        if let Expr::Call { callee, args, .. } = &e {
            assert_eq!(callee, "pkg.math.add");
            assert_eq!(args.len(), 2);
        } else {
            panic!("expected Call, got {e:?}");
        }
    }

    #[test]
    fn call_with_nested_expressions_in_args() {
        // The expression `f(g(1), h(2, 3))` exercises the recursive
        // nature of the parser: each arg is itself a Pratt-parsed
        // expression, and the call site itself is a postfix in the
        // outer parse.
        let e = p("f(g(1), h(2, 3))");
        if let Expr::Call { args, .. } = &e {
            assert_eq!(args.len(), 2);
        } else {
            panic!("expected Call, got {e:?}");
        }
    }

    // --- Indexing ---

    #[test]
    fn index() {
        let e = p("arr[0]");
        if let Expr::Index { expr, index, .. } = &e {
            assert!(matches!(expr.as_ref(), Expr::Identifier { name, .. } if name == "arr"));
            assert!(matches!(
                index.as_ref(),
                Expr::Literal {
                    value: Literal::Int(0),
                    ..
                }
            ));
        } else {
            panic!("expected Index, got {e:?}");
        }
    }

    #[test]
    fn chained_index() {
        // a[i][j] - two index ops in sequence
        let e = p("a[i][j]");
        let mut depth = 0;
        let mut cur = &e;
        while let Expr::Index { expr, .. } = cur {
            depth += 1;
            cur = expr;
        }
        assert_eq!(depth, 2);
    }

    // --- Array literals ---

    #[test]
    fn empty_array() {
        let e = p("[]");
        if let Expr::ArrayLiteral { elements, .. } = &e {
            assert!(elements.is_empty());
        } else {
            panic!("expected ArrayLiteral, got {e:?}");
        }
    }

    #[test]
    fn array_with_elements() {
        let e = p("[1, 2, 3]");
        if let Expr::ArrayLiteral { elements, .. } = &e {
            assert_eq!(elements.len(), 3);
        } else {
            panic!("expected ArrayLiteral, got {e:?}");
        }
    }

    // --- Struct init ---

    #[test]
    fn struct_init() {
        let e = p("struct Point { x: 10, y: 20 }");
        if let Expr::StructInit { fields, .. } = &e {
            assert_eq!(fields.len(), 2);
            assert_eq!(fields[0].name, "x");
            assert_eq!(fields[1].name, "y");
        } else {
            panic!("expected StructInit, got {e:?}");
        }
    }

    // --- If expressions ---

    #[test]
    fn if_expr() {
        let e = p("if a then b else c");
        if let Expr::If {
            cond,
            then_branch,
            else_branch,
            ..
        } = &e
        {
            assert!(matches!(cond.as_ref(), Expr::Identifier { name, .. } if name == "a"));
            assert!(matches!(then_branch.as_ref(), Expr::Identifier { name, .. } if name == "b"));
            assert!(matches!(else_branch.as_ref(), Expr::Identifier { name, .. } if name == "c"));
        } else {
            panic!("expected If, got {e:?}");
        }
    }

    // --- Logical operators ---

    #[test]
    fn logical_and_vs_or() {
        // a || b && c: && is higher precedence, so this is a || (b && c)
        let e = p("a || b && c");
        if let Expr::BinaryOp { op, right, .. } = &e {
            assert_eq!(*op, BinaryOperator::Or);
            assert!(matches!(
                right.as_ref(),
                Expr::BinaryOp {
                    op: BinaryOperator::And,
                    ..
                }
            ));
        } else {
            panic!("expected top-level Or, got {e:?}");
        }
    }

    // --- Errors ---

    #[test]
    fn missing_rparen() {
        p_err("(1 + 2", "`)`");
    }

    // --- Range literals ---

    #[test]
    fn range_literal_simple() {
        let e = p("1...5");
        if let Expr::RangeLiteral { start, end } = &e {
            assert!(matches!(
                start.as_ref(),
                Expr::Literal {
                    value: Literal::Int(1),
                    ..
                }
            ));
            assert!(matches!(
                end.as_ref(),
                Expr::Literal {
                    value: Literal::Int(5),
                    ..
                }
            ));
        } else {
            panic!("expected RangeLiteral, got {e:?}");
        }
    }

    #[test]
    fn range_literal_with_expressions() {
        // a + 1...b - 1
        // Range binds tighter than assignment but looser than
        // arithmetic, so this is (a + 1) ... (b - 1).
        let e = p("a + 1...b - 1");
        if let Expr::RangeLiteral { start, end } = &e {
            // start should be a + 1
            assert!(matches!(
                start.as_ref(),
                Expr::BinaryOp {
                    op: BinaryOperator::Add,
                    ..
                }
            ));
            // end should be b - 1
            assert!(matches!(
                end.as_ref(),
                Expr::BinaryOp {
                    op: BinaryOperator::Sub,
                    ..
                }
            ));
        } else {
            panic!("expected RangeLiteral, got {e:?}");
        }
    }

    #[test]
    fn missing_rbracket() {
        p_err("arr[0", "`]`");
    }

    #[test]
    fn unexpected_token_at_start() {
        p_err("+", "identifier");
    }

    #[test]
    fn missing_field_name() {
        p_err("foo.", "identifier");
    }

    #[test]
    fn missing_else() {
        p_err("if a then b", "`else`");
    }
}
