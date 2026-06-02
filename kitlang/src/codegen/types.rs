use crate::Rule;
use crate::error::CompilationError;

use pest::iterators::Pair;
use strum::{EnumString, IntoStaticStr};

use std::collections::HashSet;
use std::str::FromStr;

/// Identity handle for a type in `TypeStore`.
///
/// Types need stable identity for inference - we can't use the enum alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TypeId(u32);

impl Default for TypeId {
    fn default() -> Self {
        Self(u32::MAX)
    }
}

/// Identity handle for a type variable (unknown type during inference).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TypeVarId(u32);

/// Represents a type variable used during inference.
///
/// Type variables start unbound and may later be bound to a `TypeId`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TypeVar {
    binding: Option<TypeId>,
}

/// Central type storage for type inference.
///
/// All type mutations go through here, making inference predictable.
#[derive(Default)]
pub struct TypeStore {
    nodes: Vec<TypeNode>,
    type_vars: Vec<TypeVar>,
    next_id: u32,
}

#[derive(Debug, Clone)]
enum TypeNode {
    /// Fully known Kit type
    Known(Type),
    /// Inference-only placeholder
    Unknown(TypeVarId),
}

impl TypeStore {
    pub const fn new() -> Self {
        Self {
            nodes: Vec::new(),
            type_vars: Vec::new(),
            next_id: 0,
        }
    }

    /// Create a new known type from a Type enum.
    pub fn new_known(&mut self, ty: Type) -> TypeId {
        let id = TypeId(self.next_id);
        self.next_id += 1;
        self.nodes.push(TypeNode::Known(ty));
        id
    }

    /// Create a new unknown type (type variable) for inference.
    pub fn new_unknown(&mut self) -> TypeId {
        let var_id = TypeVarId(
            u32::try_from(self.type_vars.len()).expect("type_vars.len() exceeds u32::MAX"),
        );
        self.type_vars.push(TypeVar { binding: None });
        let id = TypeId(self.next_id);
        self.next_id += 1;
        self.nodes.push(TypeNode::Unknown(var_id));
        id
    }

    /// Bind a type variable to a specific type ID.
    pub fn bind_type_var(&mut self, var_id: TypeVarId, ty: TypeId) -> Result<(), String> {
        if let Some(existing) = self.type_vars.get_mut(var_id.0 as usize) {
            if let Some(binding) = existing.binding {
                return Err(format!(
                    "Type variable {var_id:?} already bound to {binding:?}"
                ));
            }
            existing.binding = Some(ty);
            Ok(())
        } else {
            Err(format!("Type variable {var_id:?} does not exist"))
        }
    }

    /// Resolve a `TypeId` to its concrete Type.
    ///
    /// Follows type variable bindings. Returns error if any type variables remain unbound.
    pub fn resolve(&self, mut id: TypeId) -> Result<Type, String> {
        loop {
            if id.0 as usize >= self.nodes.len() {
                return Err(format!(
                    "Type ID {id:?} does not exist (nodes.len() = {})",
                    self.nodes.len()
                ));
            }
            let node = &self.nodes[id.0 as usize];

            id = match node {
                TypeNode::Known(ty) => return Ok(ty.clone()),
                TypeNode::Unknown(var_id) => self.resolve_var(id, *var_id)?,
            };
        }
    }

    fn resolve_var(&self, id: TypeId, var_id: TypeVarId) -> Result<TypeId, String> {
        let Some(var) = self.type_vars.get(var_id.0 as usize) else {
            return Err(format!(
                "Type variable {var_id:?} does not exist in TypeStore",
            ));
        };

        var.binding.ok_or_else(|| {
            format!("Cannot resolve type ID {id:?}: type variable {var_id:?} is unbound")
        })
    }

    /// Create a known type from an optional annotation, or an unknown type variable if None.
    pub fn known_or_unknown(&mut self, ann: Option<&Type>) -> TypeId {
        match ann {
            Some(t) => self.new_known(t.clone()),
            None => self.new_unknown(),
        }
    }

    /// Like `known_or_unknown`, but wraps the result in `Some` for optional type fields.
    pub fn known_or_unknown_some(&mut self, ann: Option<&Type>) -> Option<TypeId> {
        Some(self.known_or_unknown(ann))
    }

    /// Check if a `TypeId` is an unknown type variable.
    pub fn is_unknown(&self, id: TypeId) -> bool {
        matches!(self.nodes.get(id.0 as usize), Some(TypeNode::Unknown(_)))
    }

    fn get_node(&self, id: TypeId) -> &TypeNode {
        debug_assert!(
            (id.0 as usize) < self.nodes.len(),
            "get_node: invalid TypeId {} (max {})",
            id.0,
            self.nodes.len().saturating_sub(1),
        );
        &self.nodes[id.0 as usize]
    }

    /// Follow bindings to find the representative `TypeId`.
    pub fn find_rep(&self, mut id: TypeId) -> TypeId {
        loop {
            match self.nodes.get(id.0 as usize) {
                Some(TypeNode::Unknown(var_id)) => {
                    match self.type_vars.get(var_id.0 as usize) {
                        Some(TypeVar {
                            binding: Some(next_id),
                        }) => id = *next_id,
                        _ => return id, // Unbound
                    }
                }
                _ => return id, // Known
            }
        }
    }

    /// Unify two type IDs (the core inference algorithm).
    ///
    /// Makes two types agree by either binding unknowns or comparing known types structurally.
    pub fn unify(&mut self, a: TypeId, b: TypeId) -> Result<(), String> {
        let rep_a = self.find_rep(a);
        let rep_b = self.find_rep(b);

        if rep_a == rep_b {
            return Ok(());
        }

        match (self.get_node(rep_a).clone(), self.get_node(rep_b).clone()) {
            // Unknown + Anything
            (TypeNode::Unknown(var_id), _) => self.bind_type_var(var_id, rep_b),
            (_, TypeNode::Unknown(var_id)) => self.bind_type_var(var_id, rep_a),

            // Both Known -> structural comparison
            (TypeNode::Known(ty_a), TypeNode::Known(ty_b)) => self.unify_types(&ty_a, &ty_b),
        }
    }

    /// Unify two known Type enum values structurally.
    fn unify_types(&mut self, a: &Type, b: &Type) -> Result<(), String> {
        // Fast path: structurally identical simple types (Int8, Bool, CString, etc.)
        if a == b
            && !matches!(
                a,
                Type::Ptr(_) | Type::Tuple(_) | Type::CArray(..) | Type::Named(_)
            )
        {
            return Ok(());
        }

        match (a, b) {
            // Pointer types: unify inner types
            (Type::Ptr(t1), Type::Ptr(t2)) => self.unify_type_ids((**t1).clone(), (**t2).clone()),

            // Tuple types: unify element-wise
            (Type::Tuple(v1), Type::Tuple(v2)) => {
                if v1.len() != v2.len() {
                    return Err(format!(
                        "Cannot unify tuples of different sizes: {} vs {}",
                        v1.len(),
                        v2.len()
                    ));
                }
                for (elem1, elem2) in v1.iter().zip(v2.iter()) {
                    self.unify_type_ids(elem1.clone(), elem2.clone())?;
                }
                Ok(())
            }

            // Array types: unify element type and length
            (Type::CArray(elem1, len1), Type::CArray(elem2, len2)) => {
                if len1 != len2 {
                    return Err(format!(
                        "Cannot unify arrays of different sizes: {len1:?} vs {len2:?}"
                    ));
                }
                self.unify_type_ids((**elem1).clone(), (**elem2).clone())
            }

            // Named types: check string equality
            (Type::Named(n1), Type::Named(n2)) => {
                if n1 == n2 {
                    Ok(())
                } else {
                    Err(format!("Cannot unify different named types: {n1} vs {n2}"))
                }
            }

            // Numeric type promotion: allow mixed-width integer/float types to unify
            _ if Self::is_numeric(a) && Self::is_numeric(b) => Ok(()),

            // Everything else is a type mismatch
            _ => Err(format!("Type mismatch: {a:?} vs {b:?}")),
        }
    }

    /// Check if a Type is a numeric type (integer or floating-point).
    fn is_numeric(ty: &Type) -> bool {
        matches!(
            ty,
            Type::Int8
                | Type::Int16
                | Type::Int32
                | Type::Int64
                | Type::Uint8
                | Type::Uint16
                | Type::Uint32
                | Type::Uint64
                | Type::Float32
                | Type::Float64
                | Type::Int
                | Type::Float
                | Type::Size
        )
    }

    /// Helper to unify boxed Type values.
    fn unify_type_ids(&mut self, a: Type, b: Type) -> Result<(), String> {
        let a_id = self.new_known(a);
        let b_id = self.new_known(b);
        self.unify(a_id, b_id)
    }
}

/// Represents a type in the Kit language.
///
/// TODO: further description
#[derive(Clone, Debug, PartialEq, Hash)]
pub enum Type {
    /// User-defined named type (fallback for types not covered by other variants).
    Named(String),
    /// Pointer type (e.g., `Ptr(Int)` represents `int*`).
    Ptr(Box<Type>),
    /// 8-bit signed integer (`int8_t` in C).
    Int8,
    /// 16-bit signed integer (`int16_t` in C).
    Int16,
    /// 32-bit signed integer (`int32_t` in C).
    Int32,
    /// 64-bit signed integer (`int64_t` in C).
    Int64,
    /// 8-bit unsigned integer (`uint8_t` in C).
    Uint8,
    /// 16-bit unsigned integer (`uint16_t` in C).
    Uint16,
    /// 32-bit unsigned integer (`uint32_t` in C).
    Uint32,
    /// 64-bit unsigned integer (`uint64_t` in C).
    Uint64,
    /// 32-bit floating point (`float` in C).
    Float32,
    /// 64-bit floating point (`double` in C).
    Float64,
    /// Platform-dependent integer size (`int` in C).
    Int,
    /// Single-precision floating point (`float` in C).
    Float,
    /// Platform-dependent size type (`size_t` in C).
    Size,
    /// Character type (`char` in C).
    Char,
    /// Boolean type (`bool` from <stdbool.h> in C).
    Bool,
    /// C-style null-terminated string (`char*` in C).
    CString,
    /// Tuple type (represented as a struct in C).
    Tuple(Vec<Type>),
    /// C array type (TODO: is this variable length or fixed length?).
    ///
    /// ...
    CArray(Box<Type>, usize),
    /// Represents a void type (e.g., for functions with no return value).
    Void,
    /// User-defined struct type.
    Struct {
        /// Struct name (e.g., "Point").
        name: String,
        /// Field definitions for the struct.
        fields: Vec<(String, TypeId)>,
    },
}

impl Type {
    /// Parse a Kit type name string into a `Type` variant.
    /// Falls back to `Type::Named` for unknown types.
    pub fn from_kit(name: &str) -> Self {
        match name {
            "Int8" => Type::Int8,
            "Int16" => Type::Int16,
            "Int32" => Type::Int32,
            "Int64" => Type::Int64,
            "Uint8" => Type::Uint8,
            "Uint16" => Type::Uint16,
            "Uint32" => Type::Uint32,
            "Uint64" => Type::Uint64,
            "Float32" => Type::Float32,
            "Float64" => Type::Float64,
            "Int" => Type::Int,
            "Float" => Type::Float,
            "Size" => Type::Size,
            "Char" => Type::Char,
            "Bool" => Type::Bool,
            "CString" => Type::CString,
            "Void" => Type::Void,
            _ => Type::Named(name.to_string()),
        }
    }
}

/// C type representation: name, optional typedef declaration, and required headers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CRepr {
    pub name: String,
    pub declaration: Option<String>,
    pub headers: HashSet<String>,
}

pub trait ToCRepr {
    fn to_c_repr(&self) -> CRepr;
}

impl ToCRepr for Type {
    fn to_c_repr(&self) -> CRepr {
        match self {
            Type::Int8 => simple_c_type("int8_t", &["stdint.h"]),
            Type::Int16 => simple_c_type("int16_t", &["stdint.h"]),
            Type::Int32 => simple_c_type("int32_t", &["stdint.h"]),
            Type::Int64 => simple_c_type("int64_t", &["stdint.h"]),
            Type::Uint8 => simple_c_type("uint8_t", &["stdint.h"]),
            Type::Uint16 => simple_c_type("uint16_t", &["stdint.h"]),
            Type::Uint32 => simple_c_type("uint32_t", &["stdint.h"]),
            Type::Uint64 => simple_c_type("uint64_t", &["stdint.h"]),
            Type::Float32 | Type::Float => simple_c_type("float", &[]),
            Type::Float64 => simple_c_type("double", &[]),
            Type::Int => simple_c_type("int", &[]),
            Type::Size => simple_c_type("size_t", &["stddef.h"]),
            Type::Char => simple_c_type("char", &[]),
            Type::Bool => simple_c_type("bool", &["stdbool.h"]),
            Type::CString => simple_c_type("char*", &[]),
            Type::Void => simple_c_type("void", &[]),
            Type::Ptr(inner) => {
                let inner_repr = inner.to_c_repr();
                let headers = inner_repr.headers;
                CRepr {
                    name: format!("{}*", inner_repr.name),
                    declaration: inner_repr.declaration,
                    headers,
                }
            }
            Type::Tuple(_elements) => CRepr {
                name: "/* tuple */ void*".to_string(),
                declaration: None,
                headers: HashSet::new(),
            },
            Type::CArray(elem_type, size) => {
                let elem_repr = elem_type.to_c_repr();
                CRepr {
                    name: format!("{}[{}]", elem_repr.name, size),
                    declaration: None,
                    headers: elem_repr.headers,
                }
            }
            Type::Named(name) => simple_c_type(name, &[]),
            Type::Struct { name, fields: _ } => CRepr {
                name: format!("struct {}", name),
                declaration: None,
                headers: HashSet::new(),
            },
        }
    }
}

fn simple_c_type(name: &str, headers: &[&str]) -> CRepr {
    let mut h = HashSet::new();
    for header in headers {
        h.insert(format!("<{header}>"));
    }
    CRepr {
        name: name.to_string(),
        declaration: None,
        headers: h,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, EnumString, IntoStaticStr)]
pub enum BinaryOperator {
    #[strum(serialize = "+")]
    Add,
    #[strum(serialize = "-")]
    Sub,
    #[strum(serialize = "*")]
    Mul,
    #[strum(serialize = "/")]
    Div,
    #[strum(serialize = "%")]
    Mod,
    #[strum(serialize = "==")]
    Eq,
    /// Not equal
    #[strum(serialize = "!=")]
    Ne,
    /// Less than
    #[strum(serialize = "<")]
    Lt,
    /// Greater than
    #[strum(serialize = ">")]
    Gt,
    /// Less than or equal
    #[strum(serialize = "<=")]
    Le,
    /// Greater than or equal
    #[strum(serialize = ">=")]
    Ge,
    #[strum(serialize = "&&")]
    And,
    #[strum(serialize = "||")]
    Or,
    #[strum(serialize = "&")]
    BitAnd,
    #[strum(serialize = "|")]
    BitOr,
    #[strum(serialize = "^")]
    BitXor,
    /// Shift Left
    #[strum(serialize = "<<")]
    Shl,
    /// Shift Right
    #[strum(serialize = ">>")]
    Shr,
}

impl BinaryOperator {
    /// Return the C operator string for this binary operator.
    pub fn to_c_str(&self) -> &'static str {
        (*self).into()
    }

    /// Construct a `BinaryOperator` from a Pest parse pair (matched on `Rule::*_op`).
    pub fn from_rule_pair(pair: &Pair<Rule>) -> Result<Self, CompilationError> {
        Self::from_str(pair.as_str())
            .map_err(|_| CompilationError::InvalidOperator(pair.as_str().to_string()))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, EnumString, IntoStaticStr)]
pub enum UnaryOperator {
    #[strum(serialize = "-")]
    Neg,
    #[strum(serialize = "!")]
    Not,
    #[strum(serialize = "~")]
    BitNot,
    #[strum(serialize = "&")]
    AddressOf,
    #[strum(serialize = "*")]
    Dereference,
}

impl UnaryOperator {
    /// Return the C operator string for this unary operator.
    pub fn to_c_str(&self) -> &'static str {
        (*self).into()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, EnumString, IntoStaticStr)]
pub enum AssignmentOperator {
    /// Simple assignment
    #[strum(serialize = "=")]
    Assign,
    /// Add assignment (+=)
    #[strum(serialize = "+=")]
    AddAssign,
    /// Subtract assignment (-=)
    #[strum(serialize = "-=")]
    SubAssign,
    /// Multiply assignment (*=)
    #[strum(serialize = "*=")]
    MulAssign,
    /// Divide assignment (/=)
    #[strum(serialize = "/=")]
    DivAssign,
    /// Modulo assignment (%=)
    #[strum(serialize = "%=")]
    ModAssign,
    /// Bitwise and assignment (&=)
    #[strum(serialize = "&=")]
    AndAssign,
    /// Bitwise or assignment (|=)
    #[strum(serialize = "|=")]
    OrAssign,
    /// Bitwise xor assignment (^=)
    #[strum(serialize = "^=")]
    XorAssign,
    /// Shift left assignment (<<=)
    #[strum(serialize = "<<=")]
    ShlAssign,
    /// Shift right assignment (>>=)
    #[strum(serialize = ">>=")]
    ShrAssign,
}

impl AssignmentOperator {
    /// Return the C operator string for this assignment operator.
    pub fn to_c_str(&self) -> &'static str {
        (*self).into()
    }

    /// Construct an `AssignmentOperator` from a Pest parse pair.
    pub fn from_rule_pair(pair: &Pair<Rule>) -> Result<Self, CompilationError> {
        Self::from_str(pair.as_str())
            .map_err(|_| CompilationError::InvalidOperator(pair.as_str().to_string()))
    }
}
