//! Code generation pipeline: parsing, type inference, module resolution,
//! and C code generation from Kit AST.

/// Core AST data types: expressions, statements, functions, and programs.
pub mod ast;

/// C compiler toolchain detection and linker flag construction.
pub mod compiler;

/// Module system: paths, dependency graphs, registries, and name resolution.
pub mod module;

/// PEG-based parser that converts Kit source text into AST.
pub mod parser;

/// Type-level AST: struct, enum, and field definitions.
pub mod type_ast;

// -- Re-exports --

pub use ast::{Block, Expr, Function, GlobalDecl, Include, Literal, Param, Program, Stmt};
pub use compiler::Toolchain;
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

/// Hindley-Milner type inference engine.
pub mod inference;

/// Module-aware name mangling for C identifier generation.
pub mod name_mangling;

/// Symbol table for tracking variables and functions during inference.
pub mod symbols;

/// C code generation (transpilation) pass: Kit AST to C source.
pub mod transpile;

/// Type system representation and C type mapping.
pub mod types;
