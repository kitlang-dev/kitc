use std::convert::Infallible;
use std::env;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use which::which;

const CANDIDATES: &[&str] = &[
    #[cfg(windows)]
    "cl",
    "cc",
    "clang",
    "gcc",
];

type NoSearch = fn(&Path) -> Option<String>;

#[derive(Debug, Clone, Copy)]
pub enum Toolchain {
    Gcc,
    Clang,
    #[cfg(windows)]
    Msvc,
    Other,
}

impl FromStr for Toolchain {
    type Err = Infallible;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(match value {
            // FIXME: Assume it's GCC, but this could also be clang.
            // This *should* be fine as both take (almost?) the same arguments, but this should be
            // changed later on.
            "gcc" | "cc" => Toolchain::Gcc,
            "clang" => Toolchain::Clang,
            #[cfg(windows)]
            "cl" => Toolchain::Msvc,
            _ => Toolchain::Other,
        })
    }
}

/// Convert a path's file stem to a lowercase `String`.
///
/// This attempts to get the file stem (the filename without extension)
/// from the provided `Path`, convert it to UTF-8, and return a lowercase
/// `String`.
///
/// If the path has no file stem or the file stem is not valid UTF-8, this function returns `None`.
pub fn get_lowercase_exe(path: &Path) -> Option<String> {
    Some(path.file_stem().and_then(|s| s.to_str())?.to_lowercase())
}

/// Detect the toolchain for a given executable path.
///
/// If a custom search function `search_fn` is supplied, its result (if any)
/// overrides the simple filename-based detection.
fn detect_toolchain<SearchFn>(path: &Path, search_fn: Option<SearchFn>) -> Toolchain
where
    SearchFn: for<'a> FnOnce(&'a Path) -> Option<String>,
{
    if let Some(search) = search_fn
        && let Some(toolchain_str) = search(path)
    {
        return Toolchain::from_str(&toolchain_str).unwrap();
    }

    let exe = get_lowercase_exe(path).unwrap_or_default();
    Toolchain::from_str(&exe).unwrap()
}

impl Toolchain {
    /// Return (toolchain, full path to the discovered compiler executable) if one was found.
    ///
    /// Detection checks (in order)
    /// 1. `CC` env var (if set and resolves)
    /// 2. Known candidates on PATH (`cl`, `cc`, `clang`, `gcc`).
    ///
    /// The returned `Toolchain` enum describes the *detected* compiler type (gcc/clang/msvc).
    pub fn executable_path() -> Option<(Toolchain, PathBuf)> {
        // Check the CC environment variable
        if let Ok(env_cc) = env::var("CC")
            && let Ok(path) = which(&env_cc)
        {
            // Search PATH for known compilers
            let search_fn = |_: &Path| {
                let compilers = ["clang", "gcc"];

                // Look through each possible compiler and check if it exists
                let found = compilers.iter().map(which::which).find_map(Result::ok);

                // Get the executable name from the path
                found.as_deref().and_then(get_lowercase_exe)
            };

            return Some((detect_toolchain(&path, Some(search_fn)), path));
        }

        for name in CANDIDATES {
            if let Ok(path) = which(name) {
                let toolchain = if cfg!(unix) && *name == "cc" {
                    resolve_cc_toolchain(&path)
                } else {
                    detect_toolchain::<NoSearch>(&path, None)
                };
                return Some((toolchain, path));
            }
        }

        None
    }

    pub const fn is_msvc(&self) -> bool {
        #[cfg(windows)]
        {
            matches!(self, Toolchain::Msvc)
        }
        #[cfg(not(windows))]
        {
            // MSVC isn't on other OSes, so it's always false outside of Windows
            false
        }
    }

    pub const fn is_unix_like(&self) -> bool {
        matches!(self, Toolchain::Gcc | Toolchain::Clang)
    }

    /// Output flag used for the given toolchain (e.g. `-o` vs `/Fo`).
    pub const fn output_flag(&self) -> &'static str {
        match self {
            Toolchain::Gcc | Toolchain::Clang => "-o",
            #[cfg(windows)]
            Toolchain::Msvc => "/Fo",
            Toolchain::Other => "-o",
        }
    }

    /// Flags that should be passed to the compiler for C compilation.
    ///
    /// These are intentionally conservative and represent a set of safe, portable
    /// defaults per toolchain. `CompilerOptions` will combine these with link
    /// and target options.
    pub fn get_compiler_flags(&self) -> Vec<String> {
        match self {
            Toolchain::Gcc | Toolchain::Clang => {
                let flags = ["-std=c99", "-Wall", "-Wextra", "-pedantic"];
                flags.iter().map(ToString::to_string).collect()
            }
            #[cfg(windows)]
            Toolchain::Msvc => {
                vec!["/std:c11".to_string(), "/W4".to_string()]
            }
            Toolchain::Other => {
                vec![]
            }
        }
    }
}

/// `cc` is often a symlink to an actual compiler on the system, so
/// we need to get an actual path to the C compiler.
fn resolve_cc_toolchain(path: &Path) -> Toolchain {
    if cfg!(unix)
        && path.ends_with("cc")
        && let Ok(real_path) = std::fs::read_link(path)
    {
        return detect_toolchain::<NoSearch>(&real_path, None);
    }
    detect_toolchain::<NoSearch>(path, None)
}

#[derive(Debug, Clone)]
pub struct CompilerOptions {
    pub toolchain: Toolchain,
    /// Path to the compiler executable (cached to avoid re-detection)
    pub compiler_path: Option<PathBuf>,
    /// Source files to compile
    pub sources: Vec<PathBuf>,
    /// Single output (target) file
    pub output: Option<PathBuf>,
    pub link_opts: Vec<String>,
    /// Include search paths (`-I` flags)
    pub includes: Vec<PathBuf>,
}

#[derive(Debug, Clone, Copy)]
pub struct CompilerMeta(pub Toolchain);

impl CompilerOptions {
    /// Create a new `CompilerOptions` with the given toolchain metadata.
    pub const fn new(base_meta: CompilerMeta) -> Self {
        Self {
            toolchain: base_meta.0,
            compiler_path: None,
            sources: Vec::new(),
            output: None,
            link_opts: Vec::new(),
            includes: Vec::new(),
        }
    }

    /// Translate library names into toolchain-specific link arguments and append them.
    ///
    /// Example:
    /// - GCC/Clang: `["-lm", "-lpthread"]`
    /// - MSVC (Windows): `["m.lib"]`
    pub fn link_libs<S: AsRef<str>>(mut self, libs: &[S]) -> Self {
        for lib in libs {
            match self.toolchain {
                Toolchain::Gcc | Toolchain::Clang => {
                    self.link_opts.push(format!("-l{}", lib.as_ref()));
                }
                #[cfg(windows)]
                Toolchain::Msvc => {
                    self.link_opts.push(format!("{}.lib", lib.as_ref()));
                }
                Toolchain::Other => {}
            }
        }
        self
    }

    /// Add library search paths (translated per toolchain).
    pub fn lib_paths<P>(mut self, paths: &[P]) -> Self
    where
        P: Into<PathBuf> + AsRef<std::ffi::OsStr>,
    {
        for p in paths {
            let path: PathBuf = p.into();
            match self.toolchain {
                Toolchain::Gcc | Toolchain::Clang => {
                    self.link_opts.push(format!("-L{}", path.display()));
                }
                #[cfg(windows)]
                Toolchain::Msvc => {
                    self.link_opts.push(format!("/LIBPATH:{}", path.display()));
                }
                Toolchain::Other => {}
            }
        }
        self
    }

    /// Add source files to compile
    pub fn sources<P: Into<PathBuf> + AsRef<OsStr>>(mut self, items: &[P]) -> Self {
        for i in items {
            self.sources.push(i.into());
        }
        self
    }

    /// Set the output file
    pub fn output<P: Into<PathBuf>>(mut self, out: P) -> Self {
        self.output = Some(out.into());
        self
    }

    /// Set the compiler executable path (avoids re-detection).
    pub fn compiler_path(mut self, path: PathBuf) -> Self {
        self.compiler_path = Some(path);
        self
    }

    /// Add include search paths (`-I` flags).
    pub fn includes<P: Into<PathBuf> + Clone>(mut self, items: &[P]) -> Self {
        for i in items {
            self.includes.push(i.clone().into());
        }
        self
    }

    /// Build the compiler invocation.
    ///
    /// Returns `(path_to_compiler_executable, args_vec)`.
    ///
    /// # Errors
    ///
    /// - if `sources` is empty
    /// - if `output` is not set
    /// - if no system compiler can be found and no `compiler_path` was set
    pub fn build_invocation(&self) -> Result<(PathBuf, Vec<String>), String> {
        if self.sources.is_empty() {
            return Err("no source files specified in CompilerOptions".into());
        }
        let out = self
            .output
            .as_ref()
            .ok_or_else(|| "output (target) path not set in CompilerOptions".to_string())?;

        let compiler_path = self
            .compiler_path
            .clone()
            .or_else(|| Toolchain::executable_path().map(|(_, p)| p))
            .ok_or_else(|| "no system compiler found".to_string())?;

        let mut args = Vec::new();

        for inc in &self.includes {
            args.push(format!("-I{}", inc.display()));
        }

        for s in &self.sources {
            args.push(s.display().to_string());
        }

        args.push(self.toolchain.output_flag().to_string());
        args.push(out.display().to_string());

        args.extend(self.toolchain.get_compiler_flags());
        args.extend(self.link_opts.clone());

        Ok((compiler_path, args))
    }

    pub fn build(self) -> CompilerOptions {
        self
    }
}
