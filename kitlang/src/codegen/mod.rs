//! The `codegen` module is responsible for generating executable code
//! from the parsed Kit Abstract Syntax Tree (AST).
//!
//! It orchestrates compilation process, translating AST into an
//! intermediate representation and then into target-specific machine code.

pub mod ast;
pub mod compiler;
pub mod parser;
pub mod type_ast;
pub use ast::{Block, Expr, Function, Include, Literal, Param, Program, Stmt};
pub use compiler::Toolchain;
pub use type_ast::{Field, FieldInit, StructDefinition};

/// Handles the initial parsing of Kitlang source files, constructs
/// Abstract Syntax Tree (AST), and orchestrates generation of C code
/// from this AST.
pub mod frontend;

/// Defines the core data structures used throughout the code generation process,
/// including representations for Kitlang types, C types, functions, statements,
/// expressions, and literals. It also handles the conversion logic between Kitlang
/// and C type systems.
pub mod types;

/// Symbol table for tracking variables and functions during type inference.
pub mod symbols;

/// Type inference engine using Hindley-Milner algorithm.
pub mod inference;

/// Name mangling utilities for module-aware C code generation.
pub mod name_mangling;
