use std::fmt::Write as _;

use super::callee_name;
use super::parse_kit_expr;
use crate::codegen::ast::{Expr, ExprKind, Literal};
use crate::codegen::types::{AssignmentOperator, BinaryOperator, UnaryOperator};

/// Parse an expression and unwrap
fn p(text: &str) -> Expr {
    parse_kit_expr(text, text, 0).unwrap_or_else(|e| panic!("parse failed for `{text}`: {e}"))
}

/// Parse and assert the error contains a substring.
fn p_err(text: &str, needle: &str) {
    let err = parse_kit_expr(text, text, 0)
        .err()
        .unwrap_or_else(|| panic!("expected error for `{text}`, got Ok"));
    let msg = err.to_human_message();
    assert!(
        msg.contains(needle),
        "error `{msg}` does not contain `{needle}`"
    );
}

// --- Literals ---

/// Integer literal `42` is parsed as `Literal::Int(42)`.
#[test]
fn integer_literal() {
    let e = p("42");
    assert!(matches!(
        e,
        Expr {
            kind: ExprKind::Literal {
                value: Literal::Int(42),
                ..
            },
            ..
        }
    ));
}

/// Float literal `3.14` is parsed with correct precision.
#[test]
#[allow(clippy::approx_constant)] // 3.14 is not being approximated
fn float_literal() {
    let e = p("3.14");
    assert!(
        matches!(e, Expr { kind: ExprKind::Literal { value: Literal::Float(f), .. }, .. } if (f - 3.14).abs() < 1e-10)
    );
}

/// Double-quoted string `"hello"` is parsed as `Literal::String`.
#[test]
fn string_literal() {
    let e = p(r#""hello""#);
    assert!(
        matches!(e, Expr { kind: ExprKind::Literal { value: Literal::String(s), .. }, .. } if s == "hello")
    );
}

/// `true` and `false` both parse as `Literal::Bool`.
#[test]
fn bool_literals() {
    assert!(matches!(
        p("true"),
        Expr {
            kind: ExprKind::Literal {
                value: Literal::Bool(true),
                ..
            },
            ..
        }
    ));
    assert!(matches!(
        p("false"),
        Expr {
            kind: ExprKind::Literal {
                value: Literal::Bool(false),
                ..
            },
            ..
        }
    ));
}

/// `null` is parsed as `Literal::Null`.
#[test]
fn null_literal() {
    assert!(matches!(
        p("null"),
        Expr {
            kind: ExprKind::Literal {
                value: Literal::Null,
                ..
            },
            ..
        }
    ));
}

// --- Identifiers ---

/// Bare identifier `foo` is parsed as `Expr::Identifier`.
#[test]
fn identifier() {
    let e = p("foo");
    assert!(matches!(&e, Expr { kind: ExprKind::Identifier { name, .. }, .. } if name == "foo"));
}

#[test]
fn qualified_identifier_is_built_via_postfix_chain() {
    let e = p("foo.bar.baz");
    let mut cur = &e;
    let mut path = vec![];
    while let Expr {
        kind: ExprKind::FieldAccess {
            expr, field_name, ..
        },
        ..
    } = cur
    {
        path.push(field_name.clone());
        cur = expr;
    }
    if let Expr {
        kind: ExprKind::Identifier { name, .. },
        ..
    } = cur
    {
        assert_eq!(name, "foo");
    } else {
        panic!("expected leaf Identifier, got {cur:?}");
    }
    assert_eq!(path, vec!["baz".to_string(), "bar".to_string()]);
}

// --- Precedence ---

/// `1 + 2 * 3` - `*` binds tighter than `+`.
#[test]
fn additive_vs_multiplicative() {
    let e = p("1 + 2 * 3");
    if let Expr {
        kind: ExprKind::BinaryOp { op, right, .. },
        ..
    } = &e
    {
        assert_eq!(*op, BinaryOperator::Add);
        if let Expr {
            kind: ExprKind::BinaryOp { op: inner_op, .. },
            ..
        } = right.as_ref()
        {
            assert_eq!(*inner_op, BinaryOperator::Mul);
        } else {
            panic!("expected inner Mul, got {right:?}");
        }
    } else {
        panic!("expected top-level Add, got {e:?}");
    }
}

/// `a == b < c` - `==` is looser than `<`, parsed as `(a == (b < c))`.
#[test]
fn comparison_vs_equality() {
    let e = p("a == b < c");
    if let Expr {
        kind: ExprKind::BinaryOp {
            op, left, right, ..
        },
        ..
    } = &e
    {
        assert_eq!(*op, BinaryOperator::Eq);
        assert!(
            matches!(left.as_ref(), Expr { kind: ExprKind::Identifier { name, .. }, .. } if name == "a")
        );
        if let Expr {
            kind: ExprKind::BinaryOp { op: inner_op, .. },
            ..
        } = right.as_ref()
        {
            assert_eq!(*inner_op, BinaryOperator::Lt);
        } else {
            panic!("expected inner Lt, got {right:?}");
        }
    } else {
        panic!("expected top-level Eq, got {e:?}");
    }
}

/// `1 + 2 + 3` - `+` is left-associative: `((1 + 2) + 3)`.
#[test]
fn left_associative_addition() {
    let e = p("1 + 2 + 3");
    if let Expr {
        kind: ExprKind::BinaryOp {
            op, left, right, ..
        },
        ..
    } = &e
    {
        assert_eq!(*op, BinaryOperator::Add);
        assert!(matches!(
            right.as_ref(),
            Expr {
                kind: ExprKind::Literal {
                    value: Literal::Int(3),
                    ..
                },
                ..
            }
        ));
        if let Expr {
            kind: ExprKind::BinaryOp { op: inner_op, .. },
            ..
        } = left.as_ref()
        {
            assert_eq!(*inner_op, BinaryOperator::Add);
        } else {
            panic!("expected inner Add, got {left:?}");
        }
    } else {
        panic!("expected top-level Add, got {e:?}");
    }
}

/// `a += b += c` - `+=` is right-associative: `(a += (b += c))`.
#[test]
fn right_associative_assignment() {
    let e = p("a += b += c");
    if let Expr {
        kind: ExprKind::Assign {
            op, left, right, ..
        },
        ..
    } = &e
    {
        assert_eq!(*op, AssignmentOperator::AddAssign);
        assert!(
            matches!(left.as_ref(), Expr { kind: ExprKind::Identifier { name, .. }, .. } if name == "a")
        );
        assert!(matches!(
            right.as_ref(),
            Expr {
                kind: ExprKind::Assign { .. },
                ..
            }
        ));
    } else {
        panic!("expected top-level Assign, got {e:?}");
    }
}

/// `-a + b` - prefix `-` binds tighter than `+`.
#[test]
fn unary_minus_binds_tighter_than_addition() {
    let e = p("-a + b");
    if let Expr {
        kind: ExprKind::BinaryOp {
            op, left, right, ..
        },
        ..
    } = &e
    {
        assert_eq!(*op, BinaryOperator::Add);
        assert!(
            matches!(right.as_ref(), Expr { kind: ExprKind::Identifier { name, .. }, .. } if name == "b")
        );
        assert!(matches!(
            left.as_ref(),
            Expr {
                kind: ExprKind::UnaryOp {
                    op: UnaryOperator::Neg,
                    ..
                },
                ..
            }
        ));
    } else {
        panic!("expected top-level Add, got {e:?}");
    }
}

/// `&arr[i]` - prefix `&` is looser than postfix `[]`, parsed as `&(arr[i])`.
#[test]
fn unary_looser_than_postfix() {
    let e = p("&arr[i]");
    if let Expr {
        kind: ExprKind::UnaryOp { op, expr, .. },
        ..
    } = &e
    {
        assert_eq!(*op, UnaryOperator::AddressOf);
        assert!(matches!(
            expr.as_ref(),
            Expr {
                kind: ExprKind::Index { .. },
                ..
            }
        ));
    } else {
        panic!("expected top-level AddressOf, got {e:?}");
    }
}

// --- Postfix chains ---

/// `a.b.c.d.e` - field access chains to at least 4 levels.
#[test]
fn chained_field_access() {
    let e = p("a.b.c.d.e");
    let mut depth = 0;
    let mut cur = &e;
    while let Expr {
        kind: ExprKind::FieldAccess { expr, .. },
        ..
    } = cur
    {
        depth += 1;
        cur = expr;
    }
    assert_eq!(depth, 4, "expected 4 field-access levels");
    assert!(matches!(cur, Expr { kind: ExprKind::Identifier { name, .. }, .. } if name == "a"));
}

/// 100-level field-access chain `a.f0.f1...f99` does not overflow the stack.
#[test]
fn stress_deep_postfix_chain() {
    let mut src = String::from("a");
    for i in 0..100 {
        src.push('.');
        let _ = write!(src, "f{i}");
    }
    let e = p(&src);
    let mut depth = 0;
    let mut cur = &e;
    while let Expr {
        kind: ExprKind::FieldAccess { expr, .. },
        ..
    } = cur
    {
        depth += 1;
        cur = expr;
    }
    assert_eq!(depth, 100);
}

/// 100 levels of nested parentheses are parsed correctly.
#[test]
fn stress_deep_nested_parens() {
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
        Expr {
            kind: ExprKind::Literal {
                value: Literal::Int(1),
                ..
            },
            ..
        }
    ));
}

// --- Function calls ---

/// `f()` - function call with zero arguments.
#[test]
fn call_no_args() {
    let e = p("f()");
    if let Expr {
        kind: ExprKind::Call { callee, args, .. },
        ..
    } = &e
    {
        assert_eq!(callee_name(callee), Some("f".to_string()));
        assert!(args.is_empty());
    } else {
        panic!("expected Call, got {e:?}");
    }
}

/// `f(1)` - function call with one argument.
#[test]
fn call_one_arg() {
    let e = p("f(1)");
    if let Expr {
        kind: ExprKind::Call { callee, args, .. },
        ..
    } = &e
    {
        assert_eq!(callee_name(callee), Some("f".to_string()));
        assert_eq!(args.len(), 1);
    } else {
        panic!("expected Call, got {e:?}");
    }
}

/// `f(1, 2, 3, 4, 5)` - function call with five arguments.
#[test]
fn call_many_args() {
    let e = p("f(1, 2, 3, 4, 5)");
    if let Expr {
        kind: ExprKind::Call { args, .. },
        ..
    } = &e
    {
        assert_eq!(args.len(), 5);
    } else {
        panic!("expected Call, got {e:?}");
    }
}

/// `pkg.math.add(2, 3)` - qualified name via field-access chain is the callee.
#[test]
fn call_qualified_name() {
    let e = p("pkg.math.add(2, 3)");
    if let Expr {
        kind: ExprKind::Call { callee, args, .. },
        ..
    } = &e
    {
        assert_eq!(callee_name(callee), Some("pkg.math.add".to_string()));
        assert_eq!(args.len(), 2);
    } else {
        panic!("expected Call, got {e:?}");
    }
}

/// `f(g(1), h(2, 3))` - nested calls as arguments.
#[test]
fn call_with_nested_expressions_in_args() {
    let e = p("f(g(1), h(2, 3))");
    if let Expr {
        kind: ExprKind::Call { args, .. },
        ..
    } = &e
    {
        assert_eq!(args.len(), 2);
    } else {
        panic!("expected Call, got {e:?}");
    }
}

// --- Indexing ---

/// `arr[0]` - index expression with identifier base and integer index.
#[test]
fn index() {
    let e = p("arr[0]");
    if let Expr {
        kind: ExprKind::Index { expr, index, .. },
        ..
    } = &e
    {
        assert!(
            matches!(expr.as_ref(), Expr { kind: ExprKind::Identifier { name, .. }, .. } if name == "arr")
        );
        assert!(matches!(
            index.as_ref(),
            Expr {
                kind: ExprKind::Literal {
                    value: Literal::Int(0),
                    ..
                },
                ..
            }
        ));
    } else {
        panic!("expected Index, got {e:?}");
    }
}

/// `a[i][j]` - chained index produces two `Index` nodes.
#[test]
fn chained_index() {
    let e = p("a[i][j]");
    let mut depth = 0;
    let mut cur = &e;
    while let Expr {
        kind: ExprKind::Index { expr, .. },
        ..
    } = cur
    {
        depth += 1;
        cur = expr;
    }
    assert_eq!(depth, 2);
}

// --- Array literals ---

/// `[]` - empty array literal.
#[test]
fn empty_array() {
    let e = p("[]");
    if let Expr {
        kind: ExprKind::ArrayLiteral { elements, .. },
        ..
    } = &e
    {
        assert!(elements.is_empty());
    } else {
        panic!("expected ArrayLiteral, got {e:?}");
    }
}

/// `[1, 2, 3]` - array literal with three elements.
#[test]
fn array_with_elements() {
    let e = p("[1, 2, 3]");
    if let Expr {
        kind: ExprKind::ArrayLiteral { elements, .. },
        ..
    } = &e
    {
        assert_eq!(elements.len(), 3);
    } else {
        panic!("expected ArrayLiteral, got {e:?}");
    }
}

// --- Struct init ---

/// `struct Point { x: 10, y: 20 }` - struct init with two named fields.
#[test]
fn struct_init() {
    let e = p("struct Point { x: 10, y: 20 }");
    if let Expr {
        kind: ExprKind::StructInit { fields, .. },
        ..
    } = &e
    {
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0].name, "x");
        assert_eq!(fields[1].name, "y");
    } else {
        panic!("expected StructInit, got {e:?}");
    }
}

// --- If expressions ---

/// `if a then b else c` - if expression with all three branches.
#[test]
fn if_expr() {
    let e = p("if a then b else c");
    if let Expr {
        kind:
            ExprKind::If {
                cond,
                then_branch,
                else_branch,
                ..
            },
        ..
    } = &e
    {
        assert!(
            matches!(cond.as_ref(), Expr { kind: ExprKind::Identifier { name, .. }, .. } if name == "a")
        );
        assert!(
            matches!(then_branch.as_ref(), Expr { kind: ExprKind::Identifier { name, .. }, .. } if name == "b")
        );
        assert!(
            matches!(else_branch.as_ref(), Expr { kind: ExprKind::Identifier { name, .. }, .. } if name == "c")
        );
    } else {
        panic!("expected If, got {e:?}");
    }
}

// --- Logical operators ---

/// `a || b && c` - `&&` binds tighter than `||`, parsed as `(a || (b && c))`.
#[test]
fn logical_and_vs_or() {
    let e = p("a || b && c");
    if let Expr {
        kind: ExprKind::BinaryOp { op, right, .. },
        ..
    } = &e
    {
        assert_eq!(*op, BinaryOperator::Or);
        assert!(matches!(
            right.as_ref(),
            Expr {
                kind: ExprKind::BinaryOp {
                    op: BinaryOperator::And,
                    ..
                },
                ..
            }
        ));
    } else {
        panic!("expected top-level Or, got {e:?}");
    }
}

// --- Errors ---

/// `(1 + 2` - missing `)` produces error mentioning `)`.
#[test]
fn missing_rparen() {
    p_err("(1 + 2", "`)`");
}

// --- Range literals ---

/// `1...5` - range literal with integer start and end.
#[test]
fn range_literal_simple() {
    let e = p("1...5");
    if let Expr {
        kind: ExprKind::RangeLiteral { start, end, .. },
        ..
    } = &e
    {
        assert!(matches!(
            start.as_ref(),
            Expr {
                kind: ExprKind::Literal {
                    value: Literal::Int(1),
                    ..
                },
                ..
            }
        ));
        assert!(matches!(
            end.as_ref(),
            Expr {
                kind: ExprKind::Literal {
                    value: Literal::Int(5),
                    ..
                },
                ..
            }
        ));
    } else {
        panic!("expected RangeLiteral, got {e:?}");
    }
}

/// `a + 1...b - 1` - range bounds can be arbitrary expressions.
#[test]
fn range_literal_with_expressions() {
    let e = p("a + 1...b - 1");
    if let Expr {
        kind: ExprKind::RangeLiteral { start, end, .. },
        ..
    } = &e
    {
        assert!(matches!(
            start.as_ref(),
            Expr {
                kind: ExprKind::BinaryOp {
                    op: BinaryOperator::Add,
                    ..
                },
                ..
            }
        ));
        assert!(matches!(
            end.as_ref(),
            Expr {
                kind: ExprKind::BinaryOp {
                    op: BinaryOperator::Sub,
                    ..
                },
                ..
            }
        ));
    } else {
        panic!("expected RangeLiteral, got {e:?}");
    }
}

/// `arr[0` - missing `]` produces error mentioning `]`.
#[test]
fn missing_rbracket() {
    p_err("arr[0", "`]`");
}

/// `+` at start - parser rejects leading binary operator.
#[test]
fn unexpected_token_at_start() {
    p_err("+", "identifier");
}

/// `foo.` - trailing dot is rejected with expected identifier.
#[test]
fn missing_field_name() {
    p_err("foo.", "identifier");
}

/// `if a then b` - missing `else` produces error mentioning `else`.
#[test]
fn missing_else() {
    p_err("if a then b", "`else`");
}

/// `a b` - two adjacent expressions produce end-of-expression error.
#[test]
fn trailing_tokens_produce_error() {
    p_err("a b", "end of expression");
}

/// `a $ b` - `$` is an unrecognized character error.
#[test]
fn unrecognized_characters_produce_error() {
    let err = parse_kit_expr("a $ b", "a $ b", 0).expect_err("should error on $");
    let msg = err.to_human_message();
    assert!(msg.contains("unexpected character"), "msg: {msg}");
}

/// Literal exceeding i64 range produces overflow error.
#[test]
fn integer_overflow_is_detected() {
    let err = parse_kit_expr("99999999999999999999", "99999999999999999999", 0)
        .expect_err("should error on overflow literal");
    let msg = err.to_human_message();
    assert!(
        msg.contains("out of range"),
        "expected 'out of range', got: {msg}"
    );
}

/// Overflow in left operand of binary expression.
#[test]
fn integer_overflow_as_left_operand_is_detected() {
    let err = parse_kit_expr("99999999999999999999 + 1", "99999999999999999999 + 1", 0)
        .expect_err("should error on overflow literal");
    let msg = err.to_human_message();
    assert!(
        msg.contains("out of range"),
        "expected 'out of range', got: {msg}"
    );
}

/// Overflow in right operand of binary expression.
#[test]
fn integer_overflow_as_right_operand_is_detected() {
    let err = parse_kit_expr("1 + 99999999999999999999", "1 + 99999999999999999999", 0)
        .expect_err("should error on overflow literal");
    let msg = err.to_human_message();
    assert!(
        msg.contains("out of range"),
        "expected 'out of range', got: {msg}"
    );
}

/// Missing `)` uses `ExpectedEof` variant, not the `Semi` sentinel.
#[test]
fn eof_uses_unexpected_eof_not_semi() {
    let err = parse_kit_expr("(1 + 2", "(1 + 2", 0).expect_err("should error on missing )");
    let msg = err.to_human_message();
    assert!(
        msg.contains("end of expression"),
        "expected 'end of expression', got: {msg}"
    );
    // Also verify the existing test still works:
    let missing_rparen =
        parse_kit_expr("(1 + 2", "(1 + 2", 0).expect_err("should error on missing )");
    assert!(missing_rparen.to_human_message().contains("`)`"));
}

/// `f()()` - nested calls: result of `f()` is called with no args.
#[test]
fn indirect_call_is_parsed() {
    let e = p("f()()");
    match &e {
        Expr {
            kind: ExprKind::Call { callee, args, .. },
            ..
        } => {
            assert!(args.is_empty(), "outer call should have no args");
            match callee.as_ref() {
                Expr {
                    kind:
                        ExprKind::Call {
                            callee: inner,
                            args: inner_args,
                            ..
                        },
                    ..
                } => {
                    assert_eq!(callee_name(inner), Some("f".to_string()));
                    assert!(inner_args.is_empty());
                }
                _ => panic!("expected inner Call, got {callee:?}"),
            }
        }
        _ => panic!("expected outer Call, got {e:?}"),
    }
}

/// `x++` - postfix `++` parses to `PostIncrement`.
#[test]
fn postfix_increment_parses() {
    let e = p("x++");
    if let Expr {
        kind:
            ExprKind::UnaryOp {
                op: UnaryOperator::PostIncrement,
                expr,
                ..
            },
        ..
    } = &e
    {
        assert!(matches!(
            expr.as_ref(),
            Expr {
                kind: ExprKind::Identifier { name, .. },
                ..
            } if name == "x"
        ));
    } else {
        panic!("expected PostIncrement, got {e:?}");
    }
}

/// `x--` - postfix `--` parses to `PostDecrement`.
#[test]
fn postfix_decrement_parses() {
    let e = p("x--");
    if let Expr {
        kind:
            ExprKind::UnaryOp {
                op: UnaryOperator::PostDecrement,
                expr,
                ..
            },
        ..
    } = &e
    {
        assert!(matches!(
            expr.as_ref(),
            Expr {
                kind: ExprKind::Identifier { name, .. },
                ..
            } if name == "x"
        ));
    } else {
        panic!("expected PostDecrement, got {e:?}");
    }
}

/// `++x` - prefix `++` parses to `PreIncrement`.
#[test]
fn prefix_increment_parses() {
    let e = p("++x");
    if let Expr {
        kind:
            ExprKind::UnaryOp {
                op: UnaryOperator::PreIncrement,
                expr,
                ..
            },
        ..
    } = &e
    {
        assert!(matches!(
            expr.as_ref(),
            Expr {
                kind: ExprKind::Identifier { name, .. },
                ..
            } if name == "x"
        ));
    } else {
        panic!("expected PreIncrement, got {e:?}");
    }
}

/// `--x` - prefix `--` parses to `PreDecrement`.
#[test]
fn prefix_decrement_parses() {
    let e = p("--x");
    if let Expr {
        kind:
            ExprKind::UnaryOp {
                op: UnaryOperator::PreDecrement,
                expr,
                ..
            },
        ..
    } = &e
    {
        assert!(matches!(
            expr.as_ref(),
            Expr {
                kind: ExprKind::Identifier { name, .. },
                ..
            } if name == "x"
        ));
    } else {
        panic!("expected PreDecrement, got {e:?}");
    }
}

/// `++x + y` - prefix `++` binds tighter than addition.
#[test]
fn prefix_increment_precedence() {
    let e = p("++x + y");
    if let Expr {
        kind:
            ExprKind::BinaryOp {
                op: BinaryOperator::Add,
                left,
                right,
                ..
            },
        ..
    } = &e
    {
        assert!(matches!(
            left.as_ref(),
            Expr {
                kind: ExprKind::UnaryOp {
                    op: UnaryOperator::PreIncrement,
                    ..
                },
                ..
            }
        ));
        assert!(matches!(
            right.as_ref(),
            Expr {
                kind: ExprKind::Identifier { name, .. },
                ..
            } if name == "y"
        ));
    } else {
        panic!("expected Add, got {e:?}");
    }
}

/// `a[i]++` - postfix `++` applies to the result of array indexing.
#[test]
fn postfix_increment_after_index() {
    let e = p("a[i]++");
    if let Expr {
        kind:
            ExprKind::UnaryOp {
                op: UnaryOperator::PostIncrement,
                expr,
                ..
            },
        ..
    } = &e
    {
        assert!(matches!(
            expr.as_ref(),
            Expr {
                kind: ExprKind::Index { .. },
                ..
            }
        ));
    } else {
        panic!("expected PostIncrement, got {e:?}");
    }
}

/// `sizeof(i32)` - `sizeof` keyword is rejected (not supported in Kit).
#[test]
fn sizeof_is_not_supported() {
    let err = parse_kit_expr("sizeof(i32)", "sizeof(i32)", 0).expect_err("sizeof should error");
    let msg = err.to_human_message();
    assert!(msg.contains("Sizeof"), "msg: {msg}");
}

// --- Tuple literals ---

/// `(1, 2)` - tuple literal with two elements parses to `ExprKind::TupleLit`.
#[test]
fn tuple_literal_two_elements() {
    let e = p("(1, 2)");
    if let Expr {
        kind: ExprKind::TupleLit { elements },
        ..
    } = &e
    {
        assert_eq!(elements.len(), 2);
        assert!(matches!(
            elements[0],
            Expr {
                kind: ExprKind::Literal {
                    value: Literal::Int(1),
                    ..
                },
                ..
            }
        ));
    } else {
        panic!("expected TupleLit, got {e:?}");
    }
}

/// `(1)` - a single parenthesized expression is grouping, not a tuple literal.
#[test]
fn single_paren_is_grouping() {
    let e = p("(1)");
    assert!(
        matches!(
            &e,
            Expr {
                kind: ExprKind::Literal {
                    value: Literal::Int(1),
                    ..
                },
                ..
            }
        ),
        "expected grouped literal, got {e:?}"
    );
}

/// `(a, b) = t` - the left-hand side of a destructuring assignment parses as a
/// `TupleLit` of identifiers.
#[test]
fn destructuring_pattern_is_tuple_literal() {
    let e = p("(a, b) = t");
    if let Expr {
        kind: ExprKind::Assign { left, .. },
        ..
    } = &e
    {
        assert!(
            matches!(
                left.as_ref(),
                Expr {
                    kind: ExprKind::TupleLit { .. },
                    ..
                }
            ),
            "expected TupleLit pattern, got {left:?}"
        );
    } else {
        panic!("expected Assign, got {e:?}");
    }
}
