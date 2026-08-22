use std::collections::HashMap;
use std::path::Path;

use includium::{CStandard, PreprocessorConfig, PreprocessorDriver};

use crate::error::{FfiError, FfiResult};
use kitc_common::{get_builtin_headers, get_system_include_dirs};

/// Target platform for preprocessing, affects includium's built-in behavior.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Target {
    #[default]
    Linux,
    Windows,
    MacOS,
}

/// Configuration for includium.
///
/// This struct controls how C headers are preprocessed before being parsed by tree-sitter-c. It
/// includes system include paths, user include paths, predefined macros, and target platform
/// settings.
#[derive(Clone, Debug)]
pub struct PreprocessConfig {
    /// System include directories (for `#include <...>`).
    ///
    /// By default, these are auto-detected from the system compiler at startup via
    /// [`kitc_common::get_system_include_dirs`]. Additional directories can be added with
    /// [`add_system_include_dir`].
    pub system_include_dirs: Vec<String>,

    /// User include directories (for `#include "..."`).
    ///
    /// These are searched after the header's own directory. Add with [`add_user_include_dir`].
    pub user_include_dirs: Vec<String>,

    /// Additional predefined macros (`-DNAME=value`).
    pub predefined_macros: HashMap<String, String>,

    /// Whether to inject Kit's builtin minimal system headers (stddef.h, stdarg.h, etc.).
    ///
    /// When `true` (default), the preprocessor will first try to resolve system includes against
    /// Kit's embedded minimal headers before falling back to system include directories. This
    /// allows parsing to succeed even when system headers are not available.
    pub use_builtin_headers: bool,

    /// Target platform, affects includium's built-in predefined macros and behavior.
    pub target: Target,
}

impl Default for PreprocessConfig {
    fn default() -> Self {
        let mut config = Self {
            system_include_dirs: Vec::new(),
            user_include_dirs: Vec::new(),
            predefined_macros: HashMap::new(),
            use_builtin_headers: true,
            target: Target::default(),
        };
        // Auto-detect system include directories from the compiler
        config.system_include_dirs = get_system_include_dirs()
            .into_iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect();
        config
    }
}

impl PreprocessConfig {
    /// Creates a new default configuration.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a system include directory (for `#include <...>`).
    pub fn add_system_include_dir(mut self, dir: &str) -> Self {
        self.system_include_dirs.push(dir.to_string());
        self
    }

    /// Adds a user include directory (for `#include "..."`).
    pub fn add_user_include_dir(mut self, dir: &str) -> Self {
        self.user_include_dirs.push(dir.to_string());
        self
    }

    /// Defines a preprocessor macro.
    pub fn define_macro(mut self, name: &str, value: &str) -> Self {
        self.predefined_macros
            .insert(name.to_string(), value.to_string());
        self
    }

    /// Sets the target platform.
    pub fn with_target(mut self, target: Target) -> Self {
        self.target = target;
        self
    }

    /// Sets the target platform to the host operating system.
    pub fn with_current_target(mut self) -> Self {
        self.target = if cfg!(target_os = "windows") {
            Target::Windows
        } else if cfg!(target_os = "macos") {
            Target::MacOS
        } else {
            Target::Linux
        };
        self
    }

    /// Enables or disables builtin header injection.
    ///
    /// When enabled (default), the preprocessor will first try to resolve
    /// system includes against Kit's minimal builtin headers (stddef.h, stdarg.h,
    /// stdint.h, stdbool.h, limits.h, float.h, inttypes.h) before falling back
    /// to system include directories.
    pub fn with_builtin_headers(mut self, use_builtin: bool) -> Self {
        self.use_builtin_headers = use_builtin;
        self
    }
}

/// Preprocess a C header file using includium.
///
/// Reads the header from disk, runs it through the includium preprocessor with the given
/// configuration (which resolves `#include` directives), and returns the fully preprocessed source
/// text.
///
/// # Errors
///
/// Returns `FfiError::Io` if the header file cannot be read.
pub fn preprocess_header(header_path: &Path, config: &PreprocessConfig) -> FfiResult<String> {
    let source = std::fs::read_to_string(header_path).map_err(FfiError::Io)?;

    let header_dir = header_path
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();

    preprocess_source(&source, &header_dir, config)
}

/// Preprocess a source string from memory (no file system access for the source itself).
///
/// The `config` parameter controls include resolution, predefined macros, and target platform.
///
/// # Errors
///
/// Returns `FfiError::Preprocess` if includium fails to preprocess the source.
pub fn preprocess_source_from_string(source: &str, config: &PreprocessConfig) -> FfiResult<String> {
    preprocess_source(source, "", config)
}

/// Preprocess a source string with the given configuration.
fn preprocess_source(
    source: &str,
    header_dir: &str,
    config: &PreprocessConfig,
) -> FfiResult<String> {
    let mut pp = PreprocessorDriver::new();

    // Set up the includium config based on target
    let pp_config = match config.target {
        Target::Linux => PreprocessorConfig::for_linux(),
        Target::Windows => PreprocessorConfig::for_windows(),
        Target::MacOS => PreprocessorConfig::for_macos(),
    }
    .with_standard(CStandard::C99);

    pp.apply_config(&pp_config);

    // Define user-provided macros
    for (name, value) in &config.predefined_macros {
        pp.define(name, None, value, false);
    }

    // Set up include resolver with builtin headers and system include dirs
    let builtin_headers = get_builtin_headers();
    let system_dirs = config.system_include_dirs.clone();
    let user_dirs: Vec<String> = {
        let mut dirs = vec![header_dir.to_string()];
        dirs.extend(config.user_include_dirs.clone());
        dirs
    };
    let use_builtin = config.use_builtin_headers;

    pp = pp.with_include_resolver(move |path, kind, _ctx| match kind {
        includium::IncludeKind::System => {
            // 1. Try builtin headers first (stddef.h, stdarg.h, etc.)
            if use_builtin && let Some(content) = builtin_headers.get(path) {
                return Some((*content).to_string());
            }

            // 2. Try system include directories
            for dir in &system_dirs {
                let full_path = Path::new(dir).join(path);
                if let Ok(content) = std::fs::read_to_string(&full_path) {
                    return Some(content);
                }
            }

            log::warn!("System header not found: <{}>", path);
            None
        }
        includium::IncludeKind::Local => {
            for dir in &user_dirs {
                let full_path = Path::new(dir).join(path);
                if let Ok(content) = std::fs::read_to_string(&full_path) {
                    return Some(content);
                }
            }
            log::warn!("Local header not found: \"{}\"", path);
            None
        }
    });

    // Process the source
    let result = pp
        .process(source)
        .map_err(|e| FfiError::Preprocess(format!("{e}")))?;

    Ok(result)
}
