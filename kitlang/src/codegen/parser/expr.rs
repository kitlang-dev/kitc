use std::str::FromStr;

use pest::iterators::Pair;

use crate::codegen::ast::{Expr, Literal};
use crate::codegen::types::{AssignmentOperator, BinaryOperator, TypeId, UnaryOperator};
use crate::error::{CompilationError, CompileResult};
use crate::{Rule, parse_error};

use super::Parser;
use crate::codegen::type_ast::FieldInit;

impl Parser {
    pub(super) fn parse_expr(&self, pair: Pair<Rule>) -> CompileResult<Expr> {
        match pair.as_rule() {
            Rule::expr => {
                let inner = pair.into_inner().next().unwrap();
                self.parse_expr(inner)
            }
            Rule::assign => self.parse_assign_expr(pair),
            Rule::logical_or
            | Rule::logical_and
            | Rule::equality
            | Rule::comparison
            | Rule::additive
            | Rule::multiplicative
            | Rule::bitwise_or
            | Rule::bitwise_xor
            | Rule::bitwise_and
            | Rule::shift => {
                let mut inner = pair.into_inner();
                let mut left = self.parse_expr(inner.next().unwrap())?;
                while let Some(op_pair) = inner.next() {
                    let op = BinaryOperator::from_rule_pair(&op_pair)?;
                    let right = self.parse_expr(inner.next().unwrap())?;
                    left = Expr::BinaryOp {
                        op,
                        left: Box::new(left),
                        right: Box::new(right),
                        ty: TypeId::default(),
                    };
                }
                Ok(left)
            }
            Rule::unary => {
                let mut inner_pairs = pair.into_inner();
                let first_pair = inner_pairs.next().unwrap();
                match first_pair.as_rule() {
                    Rule::unary_op => {
                        let op_str = first_pair.as_str();
                        let op = UnaryOperator::from_str(op_str)
                            .map_err(|_| parse_error!("invalid unary operation: {op_str}"))?;
                        let expr = self.parse_expr(inner_pairs.next().unwrap())?;
                        Ok(Expr::UnaryOp {
                            op,
                            expr: Box::new(expr),
                            ty: TypeId::default(),
                        })
                    }
                    Rule::ADDRESS_OF_OP => {
                        let op = UnaryOperator::AddressOf;
                        let expr = self.parse_expr(inner_pairs.next().unwrap())?;
                        Ok(Expr::UnaryOp {
                            op,
                            expr: Box::new(expr),
                            ty: TypeId::default(),
                        })
                    }
                    Rule::postfix => self.parse_expr(first_pair),
                    Rule::primary => self.parse_expr(first_pair),
                    other => Err(parse_error!("Unexpected rule in unary: {other:?}")),
                }
            }
            Rule::identifier => Ok(Expr::Identifier {
                name: Self::pair_text(pair),
                ty: TypeId::default(),
            }),
            Rule::literal => {
                let inner = pair.into_inner().next().unwrap();
                match inner.as_rule() {
                    Rule::number => {
                        let num_pair = inner.into_inner().next().unwrap();
                        match num_pair.as_rule() {
                            Rule::integer => {
                                let s = num_pair.as_str();
                                let i = s.parse::<i64>().map_err(|e| {
                                    parse_error!("invalid integer literal '{s}': {:?}", e)
                                })?;
                                Ok(Expr::Literal {
                                    value: Literal::Int(i),
                                    ty: TypeId::default(),
                                })
                            }
                            Rule::float => {
                                let s = num_pair.as_str();
                                let f = s.parse::<f64>().map_err(|e| {
                                    parse_error!("invalid float literal '{s}': {:?}", e)
                                })?;
                                Ok(Expr::Literal {
                                    value: Literal::Float(f),
                                    ty: TypeId::default(),
                                })
                            }
                            _ => Err(parse_error!("Unexpected number type")),
                        }
                    }
                    Rule::boolean => Self::parse_bool_literal(inner.as_str()),
                    Rule::char_literal => todo!("char literal parsing not implemented"),
                    _ => Err(parse_error!(
                        "Unexpected literal type: {:?}",
                        inner.as_rule()
                    )),
                }
            }
            Rule::string => {
                let full = pair.as_str();
                let inner = &full[1..full.len() - 1];
                let unescaped = Self::unescape(inner).unwrap_or_else(|| inner.to_string());
                Ok(Expr::Literal {
                    value: Literal::String(unescaped),
                    ty: TypeId::default(),
                })
            }
            Rule::function_call_expr => {
                let mut inner = pair.into_inner();
                let callee = Self::pair_text(inner.next().unwrap());
                let args = inner
                    .filter(|p: &Pair<Rule>| p.as_rule() == Rule::expr)
                    .map(|p: Pair<Rule>| self.parse_expr(p))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(Expr::Call {
                    callee,
                    args,
                    ty: TypeId::default(),
                })
            }
            Rule::if_expr => {
                let mut inner = pair.into_inner();
                let cond = self.parse_expr(inner.next().unwrap())?;
                let then_branch = self.parse_expr(inner.next().unwrap())?;
                let else_branch = self.parse_expr(inner.next().unwrap())?;
                Ok(Expr::If {
                    cond: Box::new(cond),
                    then_branch: Box::new(then_branch),
                    else_branch: Box::new(else_branch),
                    ty: TypeId::default(),
                })
            }
            Rule::primary => {
                let text = pair.as_str();
                let mut inner = pair.into_inner();
                if inner.peek().is_none() {
                    match text {
                        "null" => Ok(Expr::Literal {
                            value: Literal::Null,
                            ty: TypeId::default(),
                        }),
                        "true" | "false" => Self::parse_bool_literal(text),
                        other => Err(parse_error!("Unknown primary keyword: {}", other)),
                    }
                } else {
                    let inner_pair = inner.next().unwrap();
                    match inner_pair.as_rule() {
                        Rule::identifier => Ok(Expr::Identifier {
                            name: Self::pair_text(inner_pair),
                            ty: TypeId::default(),
                        }),
                        Rule::literal
                        | Rule::function_call_expr
                        | Rule::array_literal
                        | Rule::struct_init
                        | Rule::union_init
                        | Rule::tuple_literal
                        | Rule::if_expr
                        | Rule::range_expr
                        | Rule::string
                        | Rule::expr
                        | Rule::unary => self.parse_expr(inner_pair),
                        _ => Err(parse_error!(
                            "Unexpected primary inner rule: {:?}",
                            inner_pair.as_rule()
                        )),
                    }
                }
            }
            Rule::postfix => {
                let mut inner = pair.into_inner();
                let mut expr = self.parse_expr(inner.next().unwrap())?;
                for field_pair in inner {
                    if field_pair.as_rule() == Rule::postfix_field {
                        let mut field_inner = field_pair.into_inner();
                        let field_name = Self::pair_text(
                            field_inner
                                .next()
                                .ok_or(parse_error!("Expected field name after '.'"))?,
                        );
                        expr = Expr::FieldAccess {
                            expr: Box::new(expr),
                            field_name,
                            ty: TypeId::default(),
                        };
                    }
                }
                Ok(expr)
            }
            Rule::struct_init => self.parse_struct_init(pair),
            Rule::range_expr => {
                let mut inner = pair.into_inner();
                let start = self.parse_expr(inner.next().unwrap())?;
                let end = self.parse_expr(inner.next().unwrap())?;
                Ok(Expr::RangeLiteral {
                    start: Box::new(start),
                    end: Box::new(end),
                })
            }
            other => Err(CompilationError::ParseError(format!(
                "Unexpected expr rule: {other:?}"
            ))),
        }
    }

    fn parse_assign_expr(&self, pair: Pair<Rule>) -> CompileResult<Expr> {
        let mut inner = pair.into_inner();
        let left_pair = inner.next().unwrap();
        let left = self.parse_expr(left_pair)?;
        if let Some(assign_op_pair) = inner.next() {
            let op = AssignmentOperator::from_rule_pair(&assign_op_pair)?;
            let right_assign_expr_pair = inner.next().unwrap();
            let right = self.parse_assign_expr(right_assign_expr_pair)?;
            Ok(Expr::Assign {
                op,
                left: Box::new(left),
                right: Box::new(right),
                ty: TypeId::default(),
            })
        } else {
            Ok(left)
        }
    }

    fn parse_struct_init(&self, pair: Pair<Rule>) -> CompileResult<Expr> {
        let mut inner = pair.into_inner();
        let type_pair = inner.next().unwrap();
        let struct_ty = self.parse_type(type_pair)?;
        let fields: Vec<FieldInit> = inner
            .filter(|p| p.as_rule() == Rule::field_init)
            .map(|p| self.parse_field_init(p))
            .collect::<Result<_, _>>()?;
        Ok(Expr::StructInit {
            ty: TypeId::default(),
            struct_type: Some(struct_ty),
            fields,
        })
    }

    fn parse_field_init(&self, pair: Pair<Rule>) -> CompileResult<FieldInit> {
        let mut inner = pair.into_inner();
        let name = Self::pair_text(inner.next().unwrap());
        let value = self.parse_expr(inner.next().unwrap())?;
        Ok(FieldInit { name, value })
    }

    fn parse_bool_literal(s: &str) -> CompileResult<Expr> {
        match s {
            "true" => Ok(Expr::Literal {
                value: Literal::Bool(true),
                ty: TypeId::default(),
            }),
            "false" => Ok(Expr::Literal {
                value: Literal::Bool(false),
                ty: TypeId::default(),
            }),
            _ => Err(parse_error!("invalid boolean literal: {}", s)),
        }
    }

    fn unescape(s: impl AsRef<str>) -> Option<String> {
        let s = s.as_ref();
        let mut out = String::with_capacity(s.len());
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c == '\\' {
                match chars.next()? {
                    'n' => out.push('\n'),
                    'r' => out.push('\r'),
                    't' => out.push('\t'),
                    '\\' => out.push('\\'),
                    '\'' => out.push('\''),
                    '"' => out.push('"'),
                    other => out.push(other),
                }
            } else {
                out.push(c);
            }
        }
        Some(out)
    }
}
