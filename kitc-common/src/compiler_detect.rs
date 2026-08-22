use std::collections::HashMap;
use std::env;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

use crate::compiler::Toolchain;

#[cfg(windows)]
use crate::msvc;

/// Information about the detected C compiler, including its system include paths.
///
/// This struct is populated at startup by [`init_compiler_info`] and cached globally.
/// It contains the detected toolchain type, compiler executable path, system include directories,
/// target triple (if cross-compiling), and a flag indicating whether cross-compilation is in effect.
#[derive(Debug, Clone)]
pub struct CompilerInfo {
    /// The detected compiler toolchain (GCC, Clang, MSVC, etc.).
    pub toolchain: Toolchain,

    /// Absolute path to the compiler executable.
    pub compiler_path: PathBuf,

    /// System include directories discovered by querying the compiler.
    pub system_include_dirs: Vec<PathBuf>,

    /// Extra environment variables that must be set when invoking this compiler.
    ///
    /// For MSVC this carries the Visual Studio build environment (`INCLUDE`, `LIB`, `PATH`, ...)
    /// captured from `vcvarsall.bat`, since `cl.exe` cannot find headers or link without it.
    ///
    /// Empty for GCC/Clang.
    pub env: HashMap<String, String>,

    /// Target triple if cross-compiling (e.g., "aarch64-linux-gnu").
    pub target_triple: Option<String>,

    /// True if the compiler targets a different architecture/OS than the host.
    pub is_cross_compiling: bool,
}

static COMPILER_INFO: OnceLock<Option<CompilerInfo>> = OnceLock::new();

/// Initialize compiler detection at startup.
///
/// Detects the system C compiler (respecting `KITC_CC`, `CC` env vars and PATH),
/// queries it for built-in system include paths, and caches the result globally.
///
/// Returns the detected [`CompilerInfo`], or `None` if no compiler was found
/// (in which case an error message is printed to stderr and the caller should
/// abort).
///
/// This should be called once at program startup to fail fast if no C compiler is available.
pub fn init_compiler_info() -> Option<CompilerInfo> {
    let info = detect_compiler();
    if info.is_none() {
        eprintln!(
            "Error: No C compiler found. Please install gcc, clang, or MSVC and ensure it's in PATH."
        );
        eprintln!("       You can also set KITC_CC to specify a compiler explicitly.");
    }
    COMPILER_INFO.set(info.clone()).ok();
    info
}

/// Returns the cached compiler info, running system detection on first use.
///
/// This is the single entry point shared by the CLI startup check ([`init_compiler_info`]) and the
/// library compilation path ([`crate::compiler::Toolchain::executable_path`]).
///
/// Because both go through the same routine, `KITC_CC`/`CC` overrides, candidate discovery,
/// and MSVC lookup are honored consistently everywhere a compiler is needed.
pub fn get_or_detect_compiler_info() -> Option<CompilerInfo> {
    if let Some(ci) = get_compiler_info() {
        return Some(ci.clone());
    }
    // Run detection once and cache the result (including `None`) so repeated lookups from the compile path don't re-run `vswhere`/PATH searches.
    let info = detect_compiler();
    let _ = COMPILER_INFO.set(info.clone());
    info
}

/// Get the previously initialized compiler info.
///
/// Returns `None` if [`init_compiler_info`] has not been called yet, or if it returned `None`.
pub fn get_compiler_info() -> Option<&'static CompilerInfo> {
    COMPILER_INFO.get().and_then(|opt| opt.as_ref())
}

/// Get system include directories from the detected compiler.
///
/// On Windows the MSVC toolchain and Windows SDK include directories are always
/// appended (via filesystem discovery), so standard headers like `stdlib.h` and
/// `stdio.h` resolve even when no usable compiler has been detected or the
/// dev shell `INCLUDE` variable is empty.
///
/// Returns an empty vector if the compiler has not been initialized yet (on a
/// non-Windows target with no detected compiler).
#[allow(unused_mut)] // MacOS and Windows targets actually need `dirs` to be mutable
pub fn get_system_include_dirs() -> Vec<PathBuf> {
    let mut dirs = get_compiler_info()
        .map(|ci| ci.system_include_dirs.clone())
        .unwrap_or_default();
    #[cfg(target_os = "macos")]
    dirs.extend(macos_sdk_include_dirs());
    #[cfg(windows)]
    dirs.extend(msvc::manual_include_dirs());
    dirs
}

/// Find the active Apple SDK's standard include directories via `xcrun`.
///
/// Clang on MacOS can report SDK paths differently depending on whether it was selected through Xcode
/// or the Command Line Tools.
///
/// This logic is logic separate from probing so headers remain discoverable when the compiler
/// probing output doesn't contain the SDK path.
#[cfg(target_os = "macos")]
fn macos_sdk_include_dirs() -> Vec<PathBuf> {
    let Ok(output) = Command::new("xcrun").args(["--show-sdk-path"]).output() else {
        return Vec::new();
    };

    if !output.status.success() {
        return Vec::new();
    }

    let sdk = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if sdk.is_empty() {
        return Vec::new();
    }

    let sdk = PathBuf::from(sdk);
    [sdk.join("usr/include"), sdk.join("usr/include/c++/v1")]
        .into_iter()
        .filter(|path| path.is_dir())
        .collect()
}

/// Returns the extra environment variables that must be applied when invoking
/// `compiler` with the given toolchain.
///
/// For GCC/Clang this is always empty. For MSVC it returns the Visual Studio
/// build environment (`INCLUDE`, `LIB`, `PATH`, ...) captured at startup,
/// falling back to capturing it on demand if the compiler wasn't initialized.
pub fn get_compiler_environment(toolchain: Toolchain, _compiler: &Path) -> HashMap<String, String> {
    if !toolchain.is_msvc() {
        return HashMap::new();
    }
    // Prefer the env captured at startup if it matches
    if let Some(info) = get_compiler_info()
        && info.toolchain.is_msvc()
    {
        return info.env.clone();
    }
    #[cfg(windows)]
    {
        msvc::msvc_environment()
    }
    #[cfg(not(windows))]
    {
        HashMap::new()
    }
}

/// Detect the system compiler and query its include paths.
fn detect_compiler() -> Option<CompilerInfo> {
    // 1. Check KITC_CC env var for explicit override
    if let Ok(cc) = env::var("KITC_CC") {
        let path = PathBuf::from(&cc);
        if let Some(info) = try_compiler(&path) {
            return Some(info);
        }
    }

    // 2. Check CC env var
    if let Ok(cc) = env::var("CC")
        && let Ok(path) = which::which(&cc)
        && let Some(info) = try_compiler(&path)
    {
        return Some(info);
    }

    // 3. Search for candidates on PATH
    let candidates = get_candidates();
    for name in candidates {
        if let Ok(path) = which::which(name)
            && let Some(info) = try_compiler(&path)
        {
            return Some(info);
        }
    }

    // 4. On Windows, MSVC's cl.exe is usually not on PATH (it's only added by the
    //    Visual Studio developer cmd). Locate an installed MSVC via vswhere.
    #[cfg(windows)]
    if let Some(path) = msvc::find_msvc()
        && let Some(info) = try_compiler(&path)
    {
        return Some(info);
    }

    // No compiler found. The caller (`init_compiler_info` for the CLI, or the
    // library path via `get_or_detect_compiler_info`) decides how to report this.
    None
}

fn get_candidates() -> Vec<&'static str> {
    #[cfg(windows)]
    {
        vec!["cl.exe", "clang-cl.exe", "cc", "clang", "gcc"]
    }
    #[cfg(not(windows))]
    {
        vec!["cc", "clang", "gcc"]
    }
}

fn try_compiler(path: &Path) -> Option<CompilerInfo> {
    // Resolve the underlying toolchain for this executable. This is shared with
    // `Toolchain::executable_path` so detection is consistent, and it follows the
    // Unix `cc` symlink to its real implementation (GCC or clang).
    let toolchain = crate::compiler::resolve_cc_toolchain(path);

    let system_include_dirs = match toolchain {
        Toolchain::Gcc | Toolchain::Clang => query_gcc_like_includes(path),
        #[cfg(windows)]
        Toolchain::Msvc => msvc::get_includes(),
        Toolchain::Other => query_gcc_like_includes(path),
    };
    let env = get_compiler_environment(toolchain, path);

    let target_triple = detect_target_triple(toolchain, path);
    let is_cross_compiling = target_triple.as_ref().is_some_and(|t| {
        !t.contains(std::env::consts::ARCH)
            || (cfg!(windows) && !t.contains("windows"))
            || (!cfg!(windows) && t.contains("windows"))
    });

    Some(CompilerInfo {
        toolchain,
        compiler_path: path.to_path_buf(),
        system_include_dirs,
        env,
        target_triple,
        is_cross_compiling,
    })
}

/// Queries a GCC/Clang-compatible compiler for its built-in system include paths.
///
/// Runs `compiler -E -Wp,-v -xc -` with an empty stdin and parses the stderr
/// output to extract the system include search paths. Reading from stdin rather
/// than `/dev/null` makes the query work on Windows too, where `/dev/null` does
/// not exist.
fn query_gcc_like_includes(compiler: &Path) -> Vec<PathBuf> {
    let output = Command::new(compiler)
        .args(["-E", "-Wp,-v", "-xc", "-"])
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .output();

    let Ok(output) = output else {
        return vec![];
    };

    let stderr = String::from_utf8_lossy(&output.stderr);
    parse_include_paths(&stderr)
}

fn parse_include_paths(stderr: &str) -> Vec<PathBuf> {
    let mut in_section = false;
    let mut paths = Vec::new();

    for line in stderr.lines() {
        let trimmed = line.trim();

        if trimmed.contains("#include") && trimmed.contains("search starts here") {
            in_section = true;
            continue;
        }

        if trimmed == "End of search list." {
            in_section = false;
            continue;
        }

        if in_section && !trimmed.is_empty() {
            // Skip framework directories on macOS for now
            if trimmed.contains("(framework directory)") {
                continue;
            }
            paths.push(PathBuf::from(trimmed));
        }
    }

    paths
}

fn detect_target_triple(toolchain: Toolchain, compiler: &Path) -> Option<String> {
    // Check if compiler name has target prefix (e.g., aarch64-linux-gnu-gcc)
    let name = compiler.file_stem().and_then(|s| s.to_str()).unwrap_or("");

    // Common cross-compiler prefixes
    let prefixes = [
        "aarch64-linux-gnu-",
        "arm-linux-gnueabihf-",
        "x86_64-w64-mingw32-",
        "i686-w64-mingw32-",
        "riscv64-linux-gnu-",
        "powerpc64le-linux-gnu-",
        "s390x-linux-gnu-",
    ];

    for prefix in &prefixes {
        if name.starts_with(prefix) {
            return Some(prefix.trim_end_matches('-').to_string());
        }
    }

    // For clang, we could try -print-target-triple
    if matches!(toolchain, Toolchain::Clang) {
        let output = Command::new(compiler)
            .args(["-print-target-triple"])
            .output()
            .ok()?;
        let triple = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !triple.is_empty() && triple != "unknown-unknown-unknown" {
            return Some(triple);
        }
    }

    None
}

/// Returns the minimal set of builtin headers that Kit bundles.
///
/// These headers (stddef.h, stdarg.h, stdint.h, stdbool.h, limits.h, float.h,
/// inttypes.h) match the minimal set that Clang provides in its resource directory.
/// They are embedded at compile time via `include_str!` and used by the includium
/// preprocessor when resolving system includes like `<stddef.h>`.
///
/// The headers are loaded from `kitc-common/resources/include/` at compile time.
pub fn get_builtin_headers() -> HashMap<&'static str, &'static str> {
    let mut map = HashMap::new();
    map.insert("stddef.h", include_str!("../resources/include/stddef.h"));
    map.insert("stdarg.h", include_str!("../resources/include/stdarg.h"));
    map.insert("stdint.h", include_str!("../resources/include/stdint.h"));
    map.insert("stdbool.h", include_str!("../resources/include/stdbool.h"));
    map.insert("limits.h", include_str!("../resources/include/limits.h"));
    map.insert("float.h", include_str!("../resources/include/float.h"));
    map.insert(
        "inttypes.h",
        include_str!("../resources/include/inttypes.h"),
    );
    map
}
