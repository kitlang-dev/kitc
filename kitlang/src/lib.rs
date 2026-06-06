//! Core logic for the Kit language compiler: lexing, parsing, type
//! inference, module resolution, and C code generation.

/// AST types, module system, parser, code generation, and type infrastructure.
pub mod codegen;

pub use codegen::Toolchain;

/// The Kit language grammar, generated from a pest grammar file.
#[derive(pest_derive::Parser)]
#[grammar = "grammar/kit.pest"]
pub struct KitParser;

/// Tokenizer for expressions, used by the Pratt parser.
///
/// Pest still handles the program/declaration/statement grammar; the Pratt
/// parser only takes over expression parsing, and it needs a token stream
/// to do so. The Logos-based lexer here is that token stream.
pub(crate) mod lexer;

/// Compilation error types.
pub(crate) mod error;
