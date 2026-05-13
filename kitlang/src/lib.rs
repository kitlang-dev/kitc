//! Core logic for the Kit language compiler: lexing, parsing, type
//! inference, module resolution, and C code generation.

/// AST types, module system, parser, code generation, and type infrastructure.
pub mod codegen;

/// Logos-based lexer for tokenization of Kit source files.
pub mod lexer;

pub use logos;

pub use codegen::Toolchain;

/// The Kit language grammar, generated from a pest grammar file.
#[derive(pest_derive::Parser)]
#[grammar = "grammar/kit.pest"]
pub struct KitParser;

/// Compilation error types.
pub(crate) mod error;
