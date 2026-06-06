//! Operator binding powers and token-to-operator conversions.

use crate::codegen::types::{AssignmentOperator, BinaryOperator, UnaryOperator};
use crate::lexer::Tok;

/// Infix binding power: (left_bp, right_bp). None = not an infix operator.
/// lbp < rbp → right-associative (assignment); lbp == rbp → left-associative.
pub fn infix(tok: &Tok) -> Option<(u8, u8)> {
    match tok {
        Tok::Assign
        | Tok::PlusEq
        | Tok::MinusEq
        | Tok::StarEq
        | Tok::SlashEq
        | Tok::PercentEq
        | Tok::AmpEq
        | Tok::PipeEq
        | Tok::CaretEq
        | Tok::ShlEq
        | Tok::ShrEq => Some((0, 1)),
        Tok::Ellipsis => Some((2, 2)),
        Tok::OrOr => Some((3, 4)),
        Tok::AndAnd => Some((5, 6)),
        Tok::Pipe => Some((7, 8)),
        Tok::Caret => Some((9, 10)),
        Tok::Amp => Some((11, 12)),
        Tok::EqEq | Tok::NotEq => Some((13, 14)),
        Tok::Lt | Tok::Gt | Tok::LtEq | Tok::GtEq => Some((15, 16)),
        Tok::Shl | Tok::Shr => Some((17, 18)),
        Tok::Plus | Tok::Minus => Some((19, 20)),
        Tok::Star | Tok::Slash | Tok::Percent => Some((21, 22)),
        _ => None,
    }
}

/// Postfix binding power. All postfix ops use a single bp higher than any infix op.
pub fn postfix(tok: &Tok) -> Option<u8> {
    match tok {
        Tok::Dot | Tok::LBracket | Tok::LParen => Some(27),
        _ => None,
    }
}

/// Prefix (unary) binding power. Returns None for non-prefix tokens.
pub fn prefix(tok: &Tok) -> Option<u8> {
    match tok {
        Tok::Bang
        | Tok::Minus
        | Tok::Star
        | Tok::Amp
        | Tok::Tilde
        | Tok::PlusPlus
        | Tok::MinusMinus => Some(25),
        _ => None,
    }
}

/// True if the token is the range operator `...`.
pub fn is_range_op(tok: &Tok) -> bool {
    matches!(tok, Tok::Ellipsis)
}

/// Convert a `Tok` to a `BinaryOperator`. Returns None for non-binary operators.
pub fn tok_to_binary_op(tok: &Tok) -> Option<BinaryOperator> {
    Some(match tok {
        Tok::Plus => BinaryOperator::Add,
        Tok::Minus => BinaryOperator::Sub,
        Tok::Star => BinaryOperator::Mul,
        Tok::Slash => BinaryOperator::Div,
        Tok::Percent => BinaryOperator::Mod,
        Tok::EqEq => BinaryOperator::Eq,
        Tok::NotEq => BinaryOperator::Ne,
        Tok::Lt => BinaryOperator::Lt,
        Tok::Gt => BinaryOperator::Gt,
        Tok::LtEq => BinaryOperator::Le,
        Tok::GtEq => BinaryOperator::Ge,
        Tok::AndAnd => BinaryOperator::And,
        Tok::OrOr => BinaryOperator::Or,
        Tok::Amp => BinaryOperator::BitAnd,
        Tok::Pipe => BinaryOperator::BitOr,
        Tok::Caret => BinaryOperator::BitXor,
        Tok::Shl => BinaryOperator::Shl,
        Tok::Shr => BinaryOperator::Shr,
        _ => return None,
    })
}

/// Convert a `Tok` to an `AssignmentOperator`.
pub fn tok_to_assign_op(tok: &Tok) -> Option<AssignmentOperator> {
    Some(match tok {
        Tok::Assign => AssignmentOperator::Assign,
        Tok::PlusEq => AssignmentOperator::AddAssign,
        Tok::MinusEq => AssignmentOperator::SubAssign,
        Tok::StarEq => AssignmentOperator::MulAssign,
        Tok::SlashEq => AssignmentOperator::DivAssign,
        Tok::PercentEq => AssignmentOperator::ModAssign,
        Tok::AmpEq => AssignmentOperator::AndAssign,
        Tok::PipeEq => AssignmentOperator::OrAssign,
        Tok::CaretEq => AssignmentOperator::XorAssign,
        Tok::ShlEq => AssignmentOperator::ShlAssign,
        Tok::ShrEq => AssignmentOperator::ShrAssign,
        _ => return None,
    })
}

/// Convert a `Tok` to a `UnaryOperator`.
pub fn tok_to_unary_op(tok: &Tok) -> Option<UnaryOperator> {
    Some(match tok {
        Tok::Bang => UnaryOperator::Not,
        Tok::Minus => UnaryOperator::Neg,
        Tok::Tilde => UnaryOperator::BitNot,
        Tok::Amp => UnaryOperator::AddressOf,
        Tok::Star => UnaryOperator::Dereference,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::Tok;

    #[test]
    fn infix_precedence_order() {
        assert!(infix(&Tok::Assign).unwrap().0 < infix(&Tok::OrOr).unwrap().0);
        assert!(infix(&Tok::OrOr).unwrap().0 < infix(&Tok::AndAnd).unwrap().0);
        assert!(infix(&Tok::AndAnd).unwrap().0 < infix(&Tok::EqEq).unwrap().0);
        assert!(infix(&Tok::EqEq).unwrap().0 < infix(&Tok::Plus).unwrap().0);
        assert!(infix(&Tok::Plus).unwrap().0 < infix(&Tok::Star).unwrap().0);
    }

    #[test]
    fn assignment_is_right_associative() {
        let (lbp, rbp) = infix(&Tok::Assign).unwrap();
        assert!(lbp < rbp);
    }

    #[test]
    fn additive_is_left_associative() {
        // In Pratt parsers, left-associative operators have lbp < rbp
        // (right operand binds tighter, so a+b+c = (a+b)+c).
        let (lbp, rbp) = infix(&Tok::Plus).unwrap();
        assert!(lbp < rbp);
    }

    #[test]
    fn postfix_higher_than_infix() {
        assert!(postfix(&Tok::Dot).unwrap() > infix(&Tok::Star).unwrap().1);
    }

    #[test]
    fn prefix_between_postfix_and_infix() {
        assert!(prefix(&Tok::Minus).unwrap() > infix(&Tok::Star).unwrap().1);
        assert!(prefix(&Tok::Minus).unwrap() < postfix(&Tok::Dot).unwrap());
    }

    #[test]
    fn token_conversions() {
        assert_eq!(tok_to_binary_op(&Tok::Plus), Some(BinaryOperator::Add));
        assert_eq!(
            tok_to_assign_op(&Tok::PlusEq),
            Some(AssignmentOperator::AddAssign)
        );
        assert_eq!(tok_to_unary_op(&Tok::Minus), Some(UnaryOperator::Neg));
        assert_eq!(tok_to_binary_op(&Tok::Assign), None);
    }
}
