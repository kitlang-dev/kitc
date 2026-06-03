use crate::codegen::types::{AssignmentOperator, BinaryOperator, Type, TypeId, UnaryOperator};

use super::ModulePath;
use super::type_ast::{
    EnumDefinition, FieldInit, ImplDefinition, RuleSet, StructDefinition, TraitDefinition, TypeDef,
};

/// Represents a metadata annotation (e.g., `#[extern]`, `#[inline]`, `#[meta(args)]`).
#[derive(Clone, Debug, PartialEq)]
pub struct Metadata {
    pub name: String,
    pub args: Vec<MetaArg>,
}

impl Metadata {
    pub fn has_name(&self, name: &str) -> bool {
        self.name == name
    }
}

/// An argument to a metadata annotation.
#[derive(Clone, Debug, PartialEq)]
pub enum MetaArg {
    Identifier(String),
    Literal(Literal),
}

/// Check if any metadata entry matches the given name.
pub fn has_meta(metas: &[Metadata], name: &str) -> bool {
    metas.iter().any(|m| m.has_name(name))
}

/// Returns `true` if the metadata list contains `#[extern]` or `#[expose]` - both of which support
/// name mangling.
pub fn has_no_mangle(metas: &[Metadata]) -> bool {
    has_meta(metas, "extern") || has_meta(metas, "expose")
}

/// Trait for AST declaration types that carry metadata attributes
/// (`#[extern]`, `#[expose]`, etc.).
///
/// Provides:
/// - Raw access to the metadata list
/// - Query methods for extern/expose checking
/// - A helper to determine the mangling module path
pub trait Attributed {
    /// Access the raw metadata list.
    fn metadata(&self) -> &[Metadata];

    /// Returns `true` if this declaration has `#[extern]` or `#[expose]` metadata.
    fn has_no_mangle(&self) -> bool {
        has_no_mangle(self.metadata())
    }

    /// Returns `true` if this declaration has `#[extern]` metadata.
    fn is_extern(&self) -> bool {
        has_meta(self.metadata(), "extern")
    }

    /// Returns `ModulePath::new()` (empty = no mangling) when `has_no_mangle()` is true,
    /// otherwise returns `current_module`. Use this as the module path argument to
    /// `mangle_name` / `mangle_enum_variant` to control name mangling.
    fn mangling_module(&self, current_module: &ModulePath) -> ModulePath {
        if self.has_no_mangle() {
            ModulePath::new()
        } else {
            current_module.clone()
        }
    }
}

/// Represents a C header inclusion.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Include {
    /// Path to header file (e.g., "stdio.h").
    pub path: String,
    /// Optional linked library name from `include "header.h" => "libname"`.
    pub linked_lib: Option<String>,
}

impl Include {
    /// Create an include without an associated library.
    pub fn new(path: String) -> Self {
        Self {
            path,
            linked_lib: None,
        }
    }

    /// Create an include with an associated library name (passed as `-l` to the linker).
    pub fn with_lib(path: String, lib: String) -> Self {
        Self {
            path,
            linked_lib: Some(lib),
        }
    }
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
    /// Whether this function is publicly visible to other modules.
    pub is_public: bool,
    /// Metadata annotations (e.g., `#[extern]`, `#[expose]`, `#[inline]`).
    pub metadata: Vec<Metadata>,
}

impl Attributed for Function {
    fn metadata(&self) -> &[Metadata] {
        &self.metadata
    }
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
    Identifier { name: String, ty: TypeId },
    /// Literal value.
    Literal { value: Literal, ty: TypeId },
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
        op: UnaryOperator,
        expr: Box<Expr>,
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
    /// Character literal (single ASCII character).
    Char(char),
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
    /// Whether this global is publicly visible to other modules.
    pub is_public: bool,
    /// Metadata annotations (e.g., `#[extern]`, `#[expose]`).
    pub metadata: Vec<Metadata>,
}

impl Attributed for GlobalDecl {
    fn metadata(&self) -> &[Metadata] {
        &self.metadata
    }
}

impl Literal {
    /// Escape a character for use in a C char or string literal.
    fn escape_char(c: char) -> String {
        match c {
            '\n' => "\\n".to_string(),
            '\r' => "\\r".to_string(),
            '\t' => "\\t".to_string(),
            '\\' => "\\\\".to_string(),
            '\'' => "\\'".to_string(),
            '"' => "\\\"".to_string(),
            c => c.to_string(),
        }
    }

    #[must_use]
    /// Convert this literal to its C representation.
    ///
    /// Float literals always get the `f` suffix (assumes `float` target).
    /// For `Float64`/`double` targets, use [`to_c_with_float()`] with false instead.
    pub fn to_c(&self) -> String {
        self.to_c_with_float(true)
    }

    /// Like `to_c()` but controls whether float literals get the `f` suffix.
    /// - `is_c_float == true` (Float -> C `float`): emit `3.14f`
    /// - `is_c_float == false` (Float64 -> C `double`): emit `3.14`
    #[must_use]
    pub fn to_c_with_float(&self, is_c_float: bool) -> String {
        match self {
            Literal::Int(i) => i.to_string(),
            Literal::Float(f) => {
                let suffix = if is_c_float { "f" } else { "" };
                if f.fract() == 0.0 {
                    format!("{f}.0{suffix}")
                } else {
                    format!("{f}{suffix}")
                }
            }
            Literal::Char(c) => format!("'{}'", Self::escape_char(*c)),
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

/// A parsed Kit module's top-level declarations (AST contents of one file).
///
/// This holds only the parsed declarations from a single `.kit` file.
/// Module-level metadata (imports, includes) is stored in `Module`.
#[derive(Clone, Debug, PartialEq)]
pub struct Program {
    /// The module path for this program, if known.
    pub module_path: Option<ModulePath>,
    /// Top-level global variable and constant declarations.
    pub globals: Vec<GlobalDecl>,
    /// Top-level function definitions.
    pub functions: Vec<Function>,
    /// Struct type definitions.
    pub structs: Vec<StructDefinition>,
    /// Enum type definitions.
    pub enums: Vec<EnumDefinition>,
    /// Trait definitions.
    pub traits: Vec<TraitDefinition>,
    /// Trait implementations.
    pub impls: Vec<ImplDefinition>,
    /// Rewrite rule sets.
    pub rulesets: Vec<RuleSet>,
    /// Type alias definitions.
    pub typedefs: Vec<TypeDef>,
}

impl Program {
    /// Create an empty program with no declarations.
    pub fn empty() -> Self {
        Self {
            module_path: None,
            globals: Vec::new(),
            functions: Vec::new(),
            structs: Vec::new(),
            enums: Vec::new(),
            traits: Vec::new(),
            impls: Vec::new(),
            rulesets: Vec::new(),
            typedefs: Vec::new(),
        }
    }
}
