use thiserror::Error;

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

    #[error("Failed to compile C code:\n{}", String::from_utf8_lossy(.0))]
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

    #[error(transparent)]
    Io(std::io::Error),
}

impl From<String> for CompilationError {
    fn from(s: String) -> Self {
        CompilationError::TypeError(s)
    }
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
