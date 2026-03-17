use crate::codegen::types::{AssignmentOperator, BinaryOperator, Type, TypeId, UnaryOperator};

use super::type_ast::{EnumDefinition, FieldInit, StructDefinition};
use std::collections::HashSet;

/// Represents a C header inclusion.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Include {
    /// Path to header file (e.g., "stdio.h").
    pub path: String,
}

/// Represents a function definition in Kit.
#[derive(Clone, Debug, PartialEq)]
pub struct Function {
    /// Function name.
    pub name: String,
    /// List of function parameters.
    pub params: Vec<Param>,
    /// Return type annotation (`None` for void inference).
    pub return_type: Option<Type>,
    /// Inferred return type ID.
    pub inferred_return: Option<TypeId>,
    /// Function body as a block of statements.
    pub body: Block,
}

/// Represents a function parameter.
#[derive(Clone, Debug, PartialEq)]
pub struct Param {
    /// Parameter name.
    pub name: String,
    /// Parameter type annotation (if specified).
    pub annotation: Option<Type>,
    /// Inferred parameter type ID.
    pub ty: TypeId,
}

/// Represents a block of statements (e.g., function body or scope block).
#[derive(Clone, Debug, PartialEq)]
pub struct Block {
    /// List of statements in the block.
    pub stmts: Vec<Stmt>,
}

/// Represents a statement in Kit.
#[derive(Clone, Debug, PartialEq)]
pub enum Stmt {
    /// Variable declaration (with optional type annotation and initializer).
    VarDecl {
        /// Variable name.
        name: String,
        /// Type annotation (`None` for type inference).
        annotation: Option<Type>,
        /// Inferred variable type ID.
        inferred: TypeId,
        /// Initializer expression (`None` for uninitialized).
        init: Option<Expr>,
    },
    /// Expression statement.
    Expr(Expr),
    /// Return statement (with optional return value).
    Return(Option<Expr>),
    /// If-else statement.
    If {
        /// The condition to evaluate.
        cond: Expr,
        /// The block to execute if the condition is true.
        then_branch: Block,
        /// The block to execute if the condition is false.
        else_branch: Option<Block>,
    },
    /// While loop statement.
    While {
        /// The condition to evaluate.
        cond: Expr,
        /// The block to execute as long as the condition is true.
        body: Block,
    },
    /// For loop statement.
    For {
        /// The name of the loop variable.
        var: String,
        /// The expression to iterate over.
        iter: Expr,
        /// The block to execute for each iteration.
        body: Block,
    },
    /// Break statement.
    Break,
    /// Continue statement.
    Continue,
}

/// Represents an expression in Kit.
#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    /// Variable or function identifier.
    Identifier(String, TypeId),
    /// Literal value.
    Literal(Literal, TypeId),
    /// Function call.
    Call {
        /// Name of the callee function.
        callee: String,
        /// Arguments passed to the function.
        args: Vec<Expr>,
        /// Inferred return type.
        ty: TypeId,
    },
    /// Unary operation.
    UnaryOp {
        /// The unary operator.
        op: UnaryOperator,
        /// The operand expression.
        expr: Box<Expr>,
        /// Inferred result type.
        ty: TypeId,
    },
    /// Binary operation.
    BinaryOp {
        op: BinaryOperator,
        left: Box<Expr>,
        right: Box<Expr>,
        /// Inferred result type.
        ty: TypeId,
    },
    /// Assignment operation.
    Assign {
        op: AssignmentOperator,
        left: Box<Expr>,
        right: Box<Expr>,
        /// Inferred result type.
        ty: TypeId,
    },
    /// If-then-else expression.
    If {
        /// The condition to evaluate.
        cond: Box<Expr>,
        /// The expression to evaluate if the condition is true.
        then_branch: Box<Expr>,
        /// The expression to evaluate if the condition is false.
        else_branch: Box<Expr>,
        /// Inferred result type.
        ty: TypeId,
    },
    /// Range literal expression (e.g., `1...10`).
    RangeLiteral {
        /// The start of the range.
        start: Box<Expr>,
        /// The end of the range (inclusive).
        end: Box<Expr>,
    },
    /// Struct initialization expression (e.g., `Point { x: 10, y: 20 }`).
    StructInit {
        /// The struct type being instantiated (filled during inference).
        ty: TypeId,
        /// The parsed type annotation (for lookup during inference).
        struct_type: Option<Type>,
        /// Field initializers.
        fields: Vec<FieldInit>,
    },
    /// Field access expression (e.g., `p.x` or `a.b.c`).
    FieldAccess {
        /// The expression to access field from.
        expr: Box<Expr>,
        /// The field name to access.
        field_name: String,
        /// Inferred result type.
        ty: TypeId,
    },
    /// Enum variant constructor (simple variant without arguments).
    EnumVariant {
        /// The enum type name.
        enum_name: String,
        /// The variant name.
        variant_name: String,
        /// Inferred type.
        ty: TypeId,
    },
    /// Enum initialization (variant with arguments).
    EnumInit {
        /// The enum type name.
        enum_name: String,
        /// The variant name.
        variant_name: String,
        /// Arguments to the variant constructor.
        args: Vec<Expr>,
        /// Inferred type.
        ty: TypeId,
    },
}

/// Represents a literal value in Kit.
#[derive(Clone, Debug, PartialEq)]
pub enum Literal {
    /// Signed integer literal.
    Int(i64),
    /// Floating-point literal.
    Float(f64),
    /// String literal (without quotes).
    String(String),
    /// Boolean literal.
    Bool(bool),
    /// Null pointer literal.
    Null,
}

/// Represents a global variable or constant declaration.
#[derive(Clone, Debug, PartialEq)]
pub struct GlobalDecl {
    /// Variable name.
    pub name: String,
    /// Type annotation (`None` for type inference).
    pub annotation: Option<Type>,
    /// Inferred variable type ID.
    pub inferred: TypeId,
    /// Initializer expression (`None` for uninitialized).
    pub init: Option<Expr>,
    /// Whether this is a const declaration.
    pub is_const: bool,
}

impl Literal {
    /// Converts the literal to its C representation string.
    #[must_use]
    pub fn to_c(&self) -> String {
        match self {
            Literal::Int(i) => i.to_string(),
            Literal::Float(f) => {
                // Ensure float literals have 'f' suffix in C
                if f.fract() == 0.0 {
                    format!("{f}.0f")
                } else {
                    format!("{f}f")
                }
            }
            Literal::String(s) => {
                // Escape special characters for C string literals
                let escaped: String = s
                    .chars()
                    .map(|c| match c {
                        '\\' => "\\\\".to_string(),
                        '\"' => "\\\"".to_string(),
                        '\n' => "\\n".to_string(),
                        '\t' => "\\t".to_string(),
                        _ => c.to_string(),
                    })
                    .collect();
                format!("\"{escaped}\"")
            }
            Literal::Bool(b) => b.to_string(),
            Literal::Null => "NULL".to_string(),
        }
    }
}

/// A fully parsed Kit program.
#[derive(Clone, Debug, PartialEq)]
pub struct Program {
    /// C header inclusions required by the program.
    pub includes: Vec<Include>,
    /// Kit module imports (not directly used in C generation).
    pub imports: HashSet<String>,
    /// Top-level global variable and constant declarations.
    pub globals: Vec<GlobalDecl>,
    /// Top-level function definitions.
    pub functions: Vec<Function>,
    /// Struct type definitions.
    pub structs: Vec<StructDefinition>,
    /// Enum type definitions.
    pub enums: Vec<EnumDefinition>,
}
