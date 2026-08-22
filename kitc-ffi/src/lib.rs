//! C header parsing and FFI declaration extraction for the Kit compiler.
//!
//! This crate handles the C interop layer: it preprocesses C headers using
//! [includium](https://crates.io/crates/includium), parses them with
//! [tree-sitter-c](https://crates.io/crates/tree-sitter-c), and extracts
//! function prototypes, struct/union definitions, typedefs, globals, and enums
//! into a language-agnostic representation (`CDeclarations`).
//!
//! Key features:
//! - **Preprocessing**: Resolves `#include` directives using includium, with
//!   automatic system include path detection from the compiler.
//! - **Builtin headers**: Minimal embedded headers (stddef.h, stdarg.h, stdint.h,
//!   stdbool.h, limits.h, float.h, inttypes.h) for parsing when system headers
//!   are unavailable.
//! - **Parsing**: Uses tree-sitter-c for robust C99 parsing.
//! - **Type mapping**: Converts C types to Kit's internal type system.
//!
//! # Example
//!
//! ```no_run
//! use kitc_ffi::{extract_header, PreprocessConfig};
//! let decls = extract_header("path/to/header.h", &PreprocessConfig::new())?;
//! # Ok::<(), kitc_ffi::FfiError>(())
//! ```

pub mod error;
pub mod parse;
pub mod preprocess;
pub mod types;

use std::path::Path;

pub use error::{FfiError, FfiResult};
pub use kitc_common::{
    CompilerInfo, Toolchain, get_compiler_info, get_system_include_dirs, init_compiler_info,
};
pub use preprocess::{PreprocessConfig, Target};
pub use types::*;

/// Extract all C declarations from a header file.
///
/// Preprocesses the header with includium (resolving `#include` directives using
/// the configured include paths), then parses the preprocessed output with
/// tree-sitter-c to extract function prototypes, structs, unions, typedefs,
/// global variables, and enums.
///
/// # Arguments
/// * `header_path` - Path to the C header file.
/// * `config` - Preprocessor configuration (include paths, macros, target platform).
///
/// # Errors
/// Returns `FfiError::HeaderNotFound` if the header file doesn't exist,
/// or `FfiError::Preprocess`/`FfiError::Parse` if preprocessing or parsing fails.
pub fn extract_header(header_path: &str, config: &PreprocessConfig) -> FfiResult<CDeclarations> {
    let path = Path::new(header_path);
    if !path.exists() {
        return Err(FfiError::HeaderNotFound(header_path.to_string()));
    }

    let preprocessed = preprocess::preprocess_header(path, config)?;
    let decls = parse::parse_c_header(&preprocessed)?;
    Ok(decls)
}

/// Extract declarations from a header source string.
///
/// Preprocesses the source through includium first, then parses.
/// Use this when the header source is already in memory.
/// For already-preprocessed source, use `extract_from_preprocessed`.
///
/// # Errors
///
/// Returns `FfiError::Preprocess` if preprocessing fails or
/// `FfiError::Parse` if parsing fails.
pub fn extract_header_from_source(
    source: &str,
    config: &PreprocessConfig,
) -> FfiResult<CDeclarations> {
    let preprocessed = preprocess::preprocess_source_from_string(source, config)?;
    parse::parse_c_header(&preprocessed)
}

/// Extract declarations from preprocessed C source (no preprocessing step).
///
/// Useful when the caller has already preprocessed the header, or when parsing
/// test strings that contain no preprocessor directives.
///
/// # Errors
///
/// Returns `FfiError::Parse` if the source cannot be parsed.
pub fn extract_from_preprocessed(source: &str) -> FfiResult<CDeclarations> {
    parse::parse_c_header(source)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_simple_function() {
        let source = "int add(int a, int b);";
        let _config = PreprocessConfig::new().with_builtin_headers(false);
        let decls = extract_from_preprocessed(source).unwrap();
        assert_eq!(decls.functions.len(), 1);
        assert_eq!(decls.functions[0].name, "add");
        assert_eq!(decls.functions[0].params.len(), 2);
        assert_eq!(decls.functions[0].return_type, CType::Int);
    }

    #[test]
    fn test_extract_void_function() {
        let source = "void greet(const char *name);";
        let _config = PreprocessConfig::new().with_builtin_headers(false);
        let decls = extract_from_preprocessed(source).unwrap();
        assert_eq!(decls.functions.len(), 1);
        assert_eq!(decls.functions[0].name, "greet");
        assert_eq!(decls.functions[0].return_type, CType::Void);
        assert_eq!(decls.functions[0].params.len(), 1);
    }

    #[test]
    fn test_extract_variadic_function() {
        let source = "int printf(const char *format, ...);";
        let _config = PreprocessConfig::new().with_builtin_headers(false);
        let decls = extract_from_preprocessed(source).unwrap();
        assert_eq!(decls.functions.len(), 1);
        assert_eq!(decls.functions[0].name, "printf");
        assert!(decls.functions[0].is_variadic);
    }

    #[test]
    fn test_extract_typedef() {
        let source = "typedef unsigned long size_t;";
        let _config = PreprocessConfig::new().with_builtin_headers(false);
        let decls = extract_from_preprocessed(source).unwrap();
        assert_eq!(decls.typedefs.len(), 1);
        assert_eq!(decls.typedefs[0].name, "size_t");
    }

    #[test]
    fn test_extract_empty_header() {
        let source = "/* just a comment */";
        let decls = extract_from_preprocessed(source).unwrap();
        assert!(decls.is_empty());
    }

    #[test]
    fn test_pointer_return_type() {
        let source = "void *malloc(size_t size);";
        let decls = extract_from_preprocessed(source).unwrap();
        assert_eq!(decls.functions.len(), 1);
        assert_eq!(decls.functions[0].name, "malloc");
        match &decls.functions[0].return_type {
            CType::Ptr(inner, _) => assert_eq!(**inner, CType::Void),
            other => panic!("Expected pointer to void, got {other}"),
        }
    }

    #[test]
    fn test_const_char_ptr() {
        let source = "const char *greeting(void);";
        let decls = extract_from_preprocessed(source).unwrap();
        assert_eq!(decls.functions.len(), 1);
        assert_eq!(decls.functions[0].name, "greeting");
        match &decls.functions[0].return_type {
            CType::Ptr(inner, qualifiers) => {
                assert_eq!(**inner, CType::Char);
                assert!(qualifiers.contains(&CQualifier::Const));
            }
            other => panic!("Expected const char pointer, got {other}"),
        }
    }
}
