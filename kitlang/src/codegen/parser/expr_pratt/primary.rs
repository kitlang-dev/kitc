//! Primary expression parsers.
//!
//! These are implementations on `ExprParser` that are split into their own
//! module for readability. The main module declares `mod primary;`.

use crate::codegen::ast::{Expr, ExprKind, Literal};
use crate::codegen::type_ast::FieldInit;
use crate::codegen::types::{Type, TypeId, UnaryOperator};
use crate::lexer::Tok;

use super::super::binding_power::postfix;
use super::super::diagnostics::ExprParseError;
use super::ExprParser;

impl ExprParser<'_> {
    /// Iteratively apply postfix operators (call, index, field access) to
    /// a base expression. Zero stack frames added per iteration. The
    /// chain is bounded by the source's syntactic length, but the parser
    /// is iterative, so the *call stack* depth is constant.
    pub(crate) fn parse_postfix_chain(&mut self, mut base: Expr) -> Result<Expr, ExprParseError> {
        loop {
            let kind = self.peek().kind.clone();
            if postfix(&kind).is_none() {
                break;
            }
            base = match kind {
                Tok::Dot => self.parse_field_access(base)?,
                Tok::LBracket => self.parse_index(base)?,
                Tok::LParen => self.parse_call(base)?,
                Tok::PlusPlus | Tok::MinusMinus => {
                    let base_span = base.span.offset;
                    let (_, tok_end) = self.token_abs(&self.peek().span);
                    let op = if matches!(kind, Tok::PlusPlus) {
                        UnaryOperator::PostIncrement
                    } else {
                        UnaryOperator::PostDecrement
                    };
                    self.advance();
                    Expr {
                        kind: ExprKind::UnaryOp {
                            op,
                            expr: Box::new(base),
                        },
                        ty: TypeId::default(),
                        span: self.spanned(base_span, tok_end),
                    }
                }
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
    pub(crate) fn parse_primary(&mut self) -> Result<Expr, ExprParseError> {
        let tok = self.peek().kind.clone();
        let (tok_start, tok_end) = self.token_abs(&self.peek().span);

        match tok {
            Tok::IntLit(n) => {
                self.advance();
                Ok(Expr {
                    kind: ExprKind::Literal {
                        value: Literal::Int(n),
                    },
                    ty: TypeId::default(),
                    span: self.spanned(tok_start, tok_end),
                })
            }
            Tok::FloatLit(f) => {
                self.advance();
                Ok(Expr {
                    kind: ExprKind::Literal {
                        value: Literal::Float(f),
                    },
                    ty: TypeId::default(),
                    span: self.spanned(tok_start, tok_end),
                })
            }
            Tok::CharLit(c) => {
                self.advance();
                Ok(Expr {
                    kind: ExprKind::Literal {
                        value: Literal::Char(c),
                    },
                    ty: TypeId::default(),
                    span: self.spanned(tok_start, tok_end),
                })
            }
            Tok::StringLit(s) => {
                self.advance();
                Ok(Expr {
                    kind: ExprKind::Literal {
                        value: Literal::String(s),
                    },
                    ty: TypeId::default(),
                    span: self.spanned(tok_start, tok_end),
                })
            }
            Tok::KwTrue => {
                self.advance();
                Ok(Expr {
                    kind: ExprKind::Literal {
                        value: Literal::Bool(true),
                    },
                    ty: TypeId::default(),
                    span: self.spanned(tok_start, tok_end),
                })
            }
            Tok::KwFalse => {
                self.advance();
                Ok(Expr {
                    kind: ExprKind::Literal {
                        value: Literal::Bool(false),
                    },
                    ty: TypeId::default(),
                    span: self.spanned(tok_start, tok_end),
                })
            }
            Tok::KwNull => {
                self.advance();
                Ok(Expr {
                    kind: ExprKind::Literal {
                        value: Literal::Null,
                    },
                    ty: TypeId::default(),
                    span: self.spanned(tok_start, tok_end),
                })
            }
            Tok::KwThis | Tok::KwSelf => {
                let name = match tok {
                    Tok::KwThis => "this",
                    _ => "Self",
                };
                self.advance();
                Ok(Expr {
                    kind: ExprKind::Identifier {
                        name: name.to_string(),
                    },
                    ty: TypeId::default(),
                    span: self.spanned(tok_start, tok_end),
                })
            }
            Tok::Ident(name) => {
                self.advance();
                Ok(Expr {
                    kind: ExprKind::Identifier { name },
                    ty: TypeId::default(),
                    span: self.spanned(tok_start, tok_end),
                })
            }
            Tok::LParen => {
                self.advance(); // consume `(`
                let first = self.parse_expr()?;
                if self.peek().kind == Tok::Comma {
                    // `(e1, e2, ...)`: a tuple literal (>= 2 elements).
                    let first_span = first.span.clone();
                    let mut elements = vec![first];
                    while self.peek().kind == Tok::Comma {
                        self.advance();
                        // Trailing comma is allowed (parses to an empty trailing element).
                        if self.peek().kind == Tok::RParen {
                            break;
                        }
                        elements.push(self.parse_expr()?);
                    }
                    let (_, end) = self.token_abs(&self.peek().span);
                    self.expect(&Tok::RParen)?;
                    let span_end = end.max(first_span.offset + first_span.length);
                    Ok(Expr {
                        kind: ExprKind::TupleLit { elements },
                        ty: TypeId::default(),
                        span: self.spanned(first_span.offset, span_end),
                    })
                } else {
                    self.expect(&Tok::RParen)?;
                    // Parenthesized expression: preserve the inner expr's span
                    Ok(first)
                }
            }
            Tok::LBracket => self.parse_array_literal(),
            Tok::KwStruct => self.parse_struct_init(),
            Tok::KwIf => self.parse_if_expr(),
            Tok::KwEmpty => {
                self.advance();
                Ok(Expr {
                    kind: ExprKind::Identifier {
                        name: "empty".to_string(),
                    },
                    ty: TypeId::default(),
                    span: self.spanned(tok_start, tok_end),
                })
            }
            _ => {
                if self.at_eof() {
                    Err(ExprParseError::UnexpectedEof {
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
                    })
                } else {
                    Err(ExprParseError::UnexpectedToken {
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
                    })
                }
            }
        }
    }

    /// Parse a `.field` access postfix.
    fn parse_field_access(&mut self, base: Expr) -> Result<Expr, ExprParseError> {
        self.advance(); // consume `.`
        let field_tok = self.peek().kind.clone();
        let (_, field_end) = self.token_abs(&self.peek().span);
        match field_tok {
            Tok::Ident(name) => {
                let start = base.span.offset;
                self.advance();
                Ok(Expr {
                    kind: ExprKind::FieldAccess {
                        expr: Box::new(base),
                        field_name: name,
                    },
                    ty: TypeId::default(),
                    span: self.spanned(start, field_end),
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
        let start = base.span.offset;
        self.advance(); // consume `[`
        let index = self.parse_expr()?;
        let (_, end) = self.token_abs(&self.peek().span); // `]`
        self.expect(&Tok::RBracket)?;
        Ok(Expr {
            kind: ExprKind::Index {
                expr: Box::new(base),
                index: Box::new(index),
            },
            ty: TypeId::default(),
            span: self.spanned(start, end),
        })
    }

    /// Parse a function call postfix: `(arg1, arg2, ...)`.
    /// The callee is any expression; no rejection of indirect calls.
    fn parse_call(&mut self, callee: Expr) -> Result<Expr, ExprParseError> {
        let start = callee.span.offset;
        let paren_end = self.base_offset + self.peek().span.end + 1; // `)` relative to expr text +1
        self.advance(); // consume `(`
        let args = self.parse_comma_list(Tok::RParen, ExprParser::parse_expr)?;
        // after parse_comma_list the closing paren has been consumed, so peek is the next token
        // use the last arg's end or the computed paren end
        let end = args
            .last()
            .map_or(paren_end, |a| a.span.offset + a.span.length);
        Ok(Expr {
            kind: ExprKind::Call {
                callee: Box::new(callee),
                args,
            },
            ty: TypeId::default(),
            span: self.spanned(start, end),
        })
    }

    /// Parse an array literal: `[expr, expr, ...]`.
    fn parse_array_literal(&mut self) -> Result<Expr, ExprParseError> {
        let (arr_start, arr_end) = self.token_abs(&self.peek().span); // `[`
        self.advance(); // consume `[`
        let elements = self.parse_comma_list(Tok::RBracket, ExprParser::parse_expr)?;
        let end = elements
            .last()
            .map_or(arr_end, |a| a.span.offset + a.span.length);
        Ok(Expr {
            kind: ExprKind::ArrayLiteral { elements },
            ty: TypeId::default(),
            span: self.spanned(arr_start, end),
        })
    }

    /// Parse a struct init: `struct Name { field: expr, ... }`.
    fn parse_struct_init(&mut self) -> Result<Expr, ExprParseError> {
        let start = self.base_offset + self.peek().span.start;
        self.advance(); // consume `struct`
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
        let end = fields
            .last()
            .map_or(start + 6, |f| f.value.span.offset + f.value.span.length);
        Ok(Expr {
            kind: ExprKind::StructInit {
                struct_type: Some(Type::from_kit(&type_name)),
                fields,
            },
            ty: TypeId::default(),
            span: self.spanned(start, end),
        })
    }

    /// Parse an if-expression: `if cond then a else b`.
    fn parse_if_expr(&mut self) -> Result<Expr, ExprParseError> {
        let start = self.base_offset + self.peek().span.start;
        self.advance(); // consume `if`
        let cond = self.parse_expr()?;
        self.expect(&Tok::KwThen)?;
        let then_branch = self.parse_expr()?;
        self.expect(&Tok::KwElse)?;
        let else_branch = self.parse_expr()?;
        let end = else_branch.span.offset + else_branch.span.length;
        Ok(Expr {
            kind: ExprKind::If {
                cond: Box::new(cond),
                then_branch: Box::new(then_branch),
                else_branch: Box::new(else_branch),
            },
            ty: TypeId::default(),
            span: self.spanned(start, end),
        })
    }
}
