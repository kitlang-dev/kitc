use std::fmt::Write;
use thiserror::Error;

/// A source-code location, used to attach line/column information to compiler
/// errors so the CLI can render a source snippet.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Span {
    /// 1-based line number.
    pub line: usize,
    /// 1-based column number.
    pub column: usize,
    /// Byte offset of the span start within the source.
    pub offset: usize,
    /// Length of the span in bytes.
    pub length: usize,
}

impl Span {
    /// Build a `Span` from a pest `Span`.
    pub fn from_pest(span: &pest::Span<'_>) -> Self {
        let (line, column) = span.start_pos().line_col();
        Self {
            line,
            column,
            offset: span.start(),
            length: span.end() - span.start(),
        }
    }

    /// Build a `Span` from a byte offset into source text.
    ///
    /// Computes line/column by scanning the source up to the offset.
    /// Clamps `offset` to the source length so out-of-bounds offsets
    /// don't panic (e.g. tests construct `TypeInferencer` without
    /// source text, but the Pratt parser still produces spans with
    /// real offsets from the file).
    pub fn from_offset(source: &str, offset: usize, length: usize) -> Self {
        let offset = offset.min(source.len());
        let line = source[..offset].lines().count();
        let column = offset - source[..offset].rfind('\n').map_or(0, |i| i + 1) + 1;
        Self {
            line,
            column,
            offset,
            length,
        }
    }
}

/// Optional source context attached to an error so the CLI can print a snippet.
#[derive(Debug, Clone, Default)]
pub struct ErrorContext {
    /// The source file the error refers to.
    pub file: String,
    /// The full source text, used to render the snippet line.
    pub source: String,
    /// The location of the error within `source`.
    pub span: Span,
}

pub type CompileResult<T> = Result<T, CompilationError>;

#[derive(Error, Debug)]
pub enum CompilationError {
    #[error("Failed to compile: {0}")]
    CompileError(String),

    #[error("Failed to parse: {0}")]
    ParseError(String),

    #[error("Invalid operator: {0}")]
    InvalidOperator(String),

    #[error("Type error: {0}")]
    TypeError(String),

    #[error(r#"C compiler/linker failed.
The generated C code may have compiled, but linking failed (missing library?).
Pass --libs <name> or use include "path" => "libname" in Kit to link libraries.

{}"#, String::from_utf8_lossy(.0))]
    CCompileError(Vec<u8>),

    #[error("Failed to find system C toolchain")]
    ToolchainNotFound,

    #[error("Invalid output path")]
    InvalidOutputPath,

    #[error("Unsupported toolchain: {0}")]
    UnsupportedToolchain(String),

    #[error("Module not found: {path}")]
    ModuleNotFound { path: String },

    #[error("Circular module dependency detected: {cycle}")]
    CircularImport { cycle: String },

    #[error("Duplicate symbol '{name}' in module {module}")]
    DuplicateSymbol { name: String, module: String },

    #[error("Symbol '{name}' is private in module '{module}'")]
    PrivateSymbol { name: String, module: String },

    /// Wraps another error with source context (file, text, and span) so the
    /// CLI can render a source-code snippet. The inner error's message is
    /// preserved via `Display`.
    #[error("{inner}")]
    WithContext {
        inner: Box<CompilationError>,
        ctx: ErrorContext,
    },

    #[error(transparent)]
    Io(std::io::Error),
}

impl From<kitc_common::error::Error> for CompilationError {
    fn from(err: kitc_common::error::Error) -> Self {
        match err {
            kitc_common::error::Error::CompileError(msg) => CompilationError::CompileError(msg),
            kitc_common::error::Error::Io(e) => CompilationError::Io(e),
        }
    }
}

impl CompilationError {
    /// Wrap this error with source context (file, source text, and span) so the
    /// CLI can render a snippet. Returns the error unchanged if context is
    /// already present (to avoid nested wrappers).
    pub fn with_context(self, ctx: ErrorContext) -> Self {
        if matches!(self, CompilationError::WithContext { .. }) {
            return self;
        }
        CompilationError::WithContext {
            inner: Box::new(self),
            ctx,
        }
    }

    /// Render the error together with a source-code snippet if context is
    /// available. Falls back to the plain message otherwise.
    pub fn render(&self) -> String {
        if let CompilationError::WithContext { inner, ctx } = self {
            let mut out = String::new();
            let loc = if ctx.file.is_empty() {
                format!("{}:{}:{}", "<source>", ctx.span.line, ctx.span.column)
            } else {
                format!("{}:{}:{}", ctx.file, ctx.span.line, ctx.span.column)
            };
            let _ = write!(out, "error: {inner}\n  --> {loc}\n");
            render_snippet(&mut out, ctx);
            out
        } else {
            format!("{self}")
        }
    }
}

/// Append a single-line source snippet with a caret pointing at the error column to `out`.
fn render_snippet(out: &mut String, ctx: &ErrorContext) {
    let line_idx = ctx.span.line.saturating_sub(1);
    let Some(line) = ctx.source.lines().nth(line_idx) else {
        return;
    };
    let gutter = format!("{} | ", ctx.span.line);
    let _ = writeln!(out, "{gutter}{line}");

    let pad: String =
        std::iter::repeat_n(' ', gutter.len() + ctx.span.column.saturating_sub(1)).collect();
    let caret_len = ctx.span.length.max(1);
    let carets: String = std::iter::repeat_n('^', caret_len).collect();
    let _ = writeln!(out, "{pad}{carets}");
}

/// Helper macro to create a `CompilationError::ParseError`
#[macro_export]
macro_rules! parse_error {
    ( $($arg:tt)* ) => {
        $crate::error::CompilationError::ParseError(format!($($arg)*))
    };
}

/// Helper macro to create a `CompilationError::TypeError`
#[macro_export]
macro_rules! type_err {
    ( $($arg:tt)* ) => {
        $crate::error::CompilationError::TypeError(format!($($arg)*))
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_without_context_falls_back_to_message() {
        let err = CompilationError::ParseError("boom".to_string());
        assert_eq!(err.render(), "Failed to parse: boom");
    }

    #[test]
    fn render_with_context_includes_location_and_snippet() {
        let ctx = ErrorContext {
            file: "main.kit".to_string(),
            source: "var x = ;\n".to_string(),
            span: Span {
                line: 1,
                column: 9,
                offset: 8,
                length: 1,
            },
        };
        let err = CompilationError::ParseError("unexpected token".to_string()).with_context(ctx);
        let rendered = err.render();
        assert!(rendered.contains("Failed to parse: unexpected token"));
        assert!(rendered.contains("main.kit:1:9"));
        assert!(rendered.contains("var x = ;"));
        assert!(rendered.contains('^'));
    }
}
