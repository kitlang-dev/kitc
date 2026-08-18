//! Code generation pipeline: parsing, type inference, module resolution,
//! and C code generation from Kit AST.

/// Core AST data types: expressions, statements, functions, and programs.
pub mod ast;

/// Module system: paths, dependency graphs, registries, and name resolution.
pub mod module;

/// PEG-based parser that converts Kit source text into AST.
pub mod parser;

/// Type-level AST: struct, enum, and field definitions.
pub mod type_ast;

// -- Re-exports --

pub use ast::{
    Block, Expr, ExprKind, Function, GlobalDecl, Include, Literal, MatchArm, MatchStmt, MetaArg,
    Metadata, Param, Program, Stmt, StmtKind,
};
pub use kitc_common::Toolchain;
pub use module::{
    DeclBinding, DeclKind, DependencyEdge, DependencyGraph, ImportType, Module, ModuleImport,
    ModuleNode, ModulePath, ModuleRegistry, NameBinding,
};
pub use type_ast::{
    Field, FieldInit, ImplDefinition, RuleDecl, RuleSet, StructDefinition, TraitDefinition,
    TypeDef, TypeParam, UsingClause,
};

/// Compiler orchestration: module loading, graph building, and C compilation.
pub mod frontend;

/// Progress reporting trait and implementations.
pub mod progress;

pub use progress::{NoOpProgress, Progress, SimpleProgress};

/// Hindley-Milner type inference engine.
pub mod inference;

/// Monomorphization of generic (template) definitions.
pub mod monomorph;

/// Specialization of constrained type variables via `default Trait as Type` declarations.
pub mod specialize;

/// C header parsing and FFI declaration integration.
pub mod ffi;

/// Module-aware name mangling for C identifier generation.
pub mod name_mangling;

/// Symbol table for tracking variables and functions during inference.
pub mod symbols;

/// C code generation (transpilation) pass: Kit AST to C source.
pub mod transpile;

/// Shared deterministic hashing helpers for generated C identifiers.
pub(crate) mod hash;

/// Defer expansion pass: lowers `Defer` statements into inline cleanup code.
pub mod defer_expand;

/// Type system representation and C type mapping.
pub mod types;

#[cfg(test)]
mod ast_tests;

#[cfg(test)]
mod metadata_tests;
