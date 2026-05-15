use crate::codegen::types::TypeId;

use super::ast::{Expr, Function, GlobalDecl};
use super::types::Type;

#[derive(Clone, Debug, PartialEq)]
pub struct StructDefinition {
    pub name: String,
    pub fields: Vec<Field>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Field {
    pub name: String,
    pub ty: TypeId,
    pub annotation: Option<Type>,
    pub is_const: bool,
    pub default: Option<Expr>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FieldInit {
    pub name: String,
    pub value: Expr,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EnumVariant {
    pub name: String,
    pub parent: String,
    pub args: Vec<Field>,
    pub default: Option<Expr>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EnumDefinition {
    pub name: String,
    pub variants: Vec<EnumVariant>,
}

/// A trait definition with methods and type parameters.
#[derive(Clone, Debug, PartialEq)]
pub struct TraitDefinition {
    pub name: String,
    pub params: Vec<TypeParam>,
    pub methods: Vec<Function>,
    pub fields: Vec<GlobalDecl>,
    pub is_public: bool,
}

/// A type parameter with optional default.
#[derive(Clone, Debug, PartialEq)]
pub struct TypeParam {
    pub name: String,
    pub default: Option<Type>,
}

/// A trait implementation for a specific type.
#[derive(Clone, Debug, PartialEq)]
pub struct ImplDefinition {
    pub name: String,
    pub trait_type: Type,
    pub for_type: Type,
    pub params: Vec<TypeParam>,
    pub methods: Vec<Function>,
}

/// A set of rewrite rules.
#[derive(Clone, Debug, PartialEq)]
pub struct RuleSet {
    pub name: String,
    pub rules: Vec<RuleDecl>,
}

/// A single rewrite rule with pattern and optional body.
#[derive(Clone, Debug, PartialEq)]
pub struct RuleDecl {
    pub pattern: Expr,
    pub body: Option<Expr>,
}

/// A type alias declaration (`typedef X = Y`).
#[derive(Clone, Debug, PartialEq)]
pub struct TypeDef {
    pub name: String,
    pub type_def: Type,
}

/// A clause in a `using` statement.
#[derive(Clone, Debug, PartialEq)]
pub enum UsingClause {
    RuleSet(Type),
    Implicit(Expr),
}
