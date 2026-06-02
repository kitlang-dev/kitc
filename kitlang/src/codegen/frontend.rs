use std::collections::HashSet;
use std::env;
use std::fs;
use std::path::{Component as PathComponent, Path, PathBuf};
use std::process::Command;
use std::slice;
use walkdir::WalkDir;

use pest::Parser;

use crate::codegen::{
    ast::{Include, Program},
    compiler::{CompilerMeta, CompilerOptions, Toolchain},
    inference::TypeInferencer,
    module::{ImportType, Module, ModuleImport, ModulePath, ModuleRegistry},
    parser::Parser as CodeParser,
    type_ast::UsingClause,
};
use crate::error::CompileResult;
use crate::{KitParser, Rule, error::CompilationError};

/// The Kit compiler, orchestrating module loading, type inference, and C code generation.
pub struct Compiler {
    pub(crate) files: Vec<PathBuf>,
    pub(crate) output: PathBuf,
    pub(crate) c_output: PathBuf,
    pub(crate) build_dir: PathBuf,
    pub(crate) libs: Vec<String>,
    pub(crate) source_paths: Vec<(PathBuf, ModulePath)>,
    pub(crate) inferencer: TypeInferencer,
    pub(crate) current_module: ModulePath,
    pub(crate) registry: ModuleRegistry,
}

/// Parse a `--source-path` CLI argument into a directory and optional module prefix.
/// Format: `dir` or `dir:prefix`
fn parse_source_path(s: &str) -> Option<(PathBuf, ModulePath)> {
    let parts: Vec<&str> = s.split(':').collect();
    match parts.as_slice() {
        [dir] if !dir.is_empty() => Some((PathBuf::from(dir), ModulePath::new())),
        [dir, prefix] if !dir.is_empty() && !prefix.is_empty() => {
            let path = ModulePath(prefix.split('.').map(String::from).collect());
            Some((PathBuf::from(dir), path))
        }
        _ => None,
    }
}

/// Strip a module prefix from a full module path, returning the remainder.
/// Returns `None` if the path does not start with the given prefix.
fn strip_module_prefix(path: &ModulePath, prefix: &ModulePath) -> Option<ModulePath> {
    if prefix.is_empty() {
        return Some(path.clone());
    }
    let path_inner = path.as_slice();
    let prefix_inner = prefix.as_slice();
    if path_inner.len() >= prefix_inner.len() && &path_inner[..prefix_inner.len()] == prefix_inner {
        Some(ModulePath(path_inner[prefix_inner.len()..].to_vec()))
    } else {
        None
    }
}

/// Find a module file on disk given its module path and the configured source paths.
/// Checks for both direct `.kit` files and `_mod.kit` directory entry-points.
fn find_module_file(path: &ModulePath, source_paths: &[(PathBuf, ModulePath)]) -> Option<PathBuf> {
    for (dir, prefix) in source_paths {
        if let Some(remaining) = strip_module_prefix(path, prefix) {
            let file_path = dir.join(remaining.join("/")).with_extension("kit");
            if file_path.exists() {
                return Some(file_path);
            }
            let mod_file = dir.join(remaining.join("/")).join("_mod.kit");
            if mod_file.exists() {
                return Some(mod_file);
            }
        }
    }
    None
}

/// Determine the module path for a given file path by matching against source paths.
fn determine_module_path(file: &Path, source_paths: &[(PathBuf, ModulePath)]) -> ModulePath {
    let stem = file
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default();

    let Some(parent) = file.parent() else {
        return ModulePath(vec![stem.to_owned()]);
    };

    for (dir, prefix) in source_paths {
        let Ok(rel) = parent.strip_prefix(dir) else {
            continue;
        };

        let mut parts = prefix.0.clone();

        parts.extend(rel.iter().filter_map(|c| c.to_str()).map(str::to_owned));

        if stem != "_mod" {
            parts.push(stem.to_owned());
        }

        return ModulePath(parts);
    }

    ModulePath(vec![stem.to_owned()])
}
/// Collect all `.kit` file paths in a directory (non-recursive), excluding `prelude.kit`.
fn collect_kit_files_in_dir_shallow(dir: &Path, base_path: &ModulePath) -> Vec<ModulePath> {
    let Ok(dir) = dir.canonicalize() else {
        return Vec::new();
    };
    WalkDir::new(&dir)
        .max_depth(1)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .filter(|e| e.path().extension().and_then(|e| e.to_str()) == Some("kit"))
        .filter_map(|e| {
            let stem = e.path().file_stem()?;
            let stem_str = stem.to_string_lossy();
            if stem_str == "prelude" {
                return None;
            }
            let mut mod_path = base_path.clone();
            mod_path.push(stem_str.to_string());
            Some(mod_path)
        })
        .collect()
}

/// Recursively walk a directory tree collecting `.kit` files, used for `**` double-wildcard imports.
fn walk_kit_files(dir: &Path, base_path: &ModulePath, results: &mut Vec<ModulePath>) {
    let Ok(dir) = dir.canonicalize() else {
        return;
    };
    for entry in WalkDir::new(&dir).into_iter().filter_map(Result::ok) {
        let entry_path = entry.path();
        if !entry.file_type().is_file() {
            continue;
        }
        if entry_path.extension().and_then(|e| e.to_str()) != Some("kit") {
            continue;
        }
        let Some(stem) = entry_path.file_stem() else {
            continue;
        };
        let stem_str = stem.to_string_lossy();
        if stem_str == "prelude" {
            continue;
        }
        let parent = entry_path.parent().unwrap_or(dir.as_path());
        let rel = parent.strip_prefix(&dir).unwrap_or(Path::new(""));
        let mut mod_path = base_path.clone();
        for component in rel.components() {
            if let PathComponent::Normal(c) = component {
                mod_path.push(c.to_string_lossy().to_string());
            }
        }
        if stem_str != "_mod" {
            mod_path.push(stem_str.to_string());
        }
        results.push(mod_path);
    }
}

/// Resolve an import statement to concrete module paths.
///
/// `Single` returns the module path itself.
/// `Wildcard` (`.*`) returns all `.kit` files in the module's directory.
/// `DoubleWildcard` (`.**`) returns all `.kit` files recursively.
fn resolve_wildcard_import(
    path: &ModulePath,
    import_type: ImportType,
    source_paths: &[(PathBuf, ModulePath)],
) -> Vec<ModulePath> {
    match import_type {
        ImportType::Single => vec![path.clone()],
        ImportType::Wildcard => {
            let mut results = Vec::new();
            for (dir, prefix) in source_paths {
                let Some(remaining) = strip_module_prefix(path, prefix) else {
                    continue;
                };
                let dir_path = dir.join(remaining.join("/"));
                if !dir_path.is_dir() {
                    continue;
                }
                results.extend(collect_kit_files_in_dir_shallow(&dir_path, path));
            }
            results.sort_by_key(|a| a.join("."));
            results
        }
        ImportType::DoubleWildcard => {
            let mut results = Vec::new();
            for (dir, prefix) in source_paths {
                let Some(remaining) = strip_module_prefix(path, prefix) else {
                    continue;
                };
                let dir_path = dir.join(remaining.join("/"));
                walk_kit_files(&dir_path, path, &mut results);
            }
            results.sort_by_key(|a| a.join("."));
            results
        }
    }
}

/// The result of parsing a single `.kit` file.
struct ParsedFile {
    includes: Vec<Include>,
    imports: Vec<ModuleImport>,
    program: Program,
    usings: Vec<UsingClause>,
}

/// Parse a single `.kit` file, returning a `ParsedFile`.
fn parse_kit_file(file: &Path) -> CompileResult<ParsedFile> {
    debug_assert!(
        file.exists(),
        "parse_kit_file: no such file: {}",
        file.display()
    );
    let input = fs::read_to_string(file).map_err(CompilationError::Io)?;

    let pairs = KitParser::parse(Rule::program, &input)
        .map_err(|e| CompilationError::ParseError(format!("{}: {}", file.display(), e)))?;

    let parser = CodeParser::new();
    let mut includes = Vec::new();
    let mut imports = Vec::new();
    let mut globals = Vec::new();
    let mut functions = Vec::new();
    let mut structs = Vec::new();
    let mut enums = Vec::new();
    let mut traits = Vec::new();
    let mut impls = Vec::new();
    let mut rulesets = Vec::new();
    let mut typedefs = Vec::new();
    let mut usings = Vec::new();

    for pair in pairs {
        match pair.as_rule() {
            Rule::include_stmt => includes.push(parser.parse_include(pair)?),
            Rule::import_stmt => imports.push(parser.parse_import(pair)?),
            Rule::var_decl => globals.push(parser.parse_global_var_decl(&pair)?),
            Rule::function_decl => functions.push(parser.parse_function(pair)?),
            Rule::type_def => {
                let mut inner = pair.into_inner();
                let (metadata, is_public) = CodeParser::parse_metadata_and_modifiers(inner.next());
                for child in inner {
                    match child.as_rule() {
                        Rule::enum_def => {
                            enums.push(parser.parse_enum_def(
                                child,
                                metadata.clone(),
                                is_public,
                            )?);
                        }
                        Rule::struct_def => structs.push(parser.parse_struct_def(
                            child,
                            metadata.clone(),
                            is_public,
                        )?),
                        _ => {}
                    }
                }
            }
            Rule::trait_def => traits.push(parser.parse_trait_def(pair)?),
            Rule::trait_impl => impls.push(parser.parse_trait_impl(pair)?),
            Rule::rule_set => rulesets.push(parser.parse_rule_set(pair)?),
            Rule::typedef_stmt => typedefs.push(parser.parse_typedef(pair)?),
            Rule::using_stmt => usings.extend(parser.parse_using(pair)?),
            _ => {}
        }
    }

    let program = Program {
        module_path: None,
        globals,
        functions,
        structs,
        enums,
        traits,
        impls,
        rulesets,
        typedefs,
    };

    Ok(ParsedFile {
        includes,
        imports,
        program,
        usings,
    })
}

/// Resolve prelude modules for a given module path.
///
/// Following the Haskell compiler's convention, for a module path like
/// `["pkg1", "pkg2", "mymod"]`, we look for:
/// - `pkg1.pkg2.prelude`
/// - `pkg1.prelude`
/// - `prelude`
///
/// These are loaded first so their declarations are available to the module.
fn resolve_preludes(
    module_path: &ModulePath,
    source_paths: &[(PathBuf, ModulePath)],
) -> Vec<ModuleImport> {
    let mut preludes = Vec::new();
    let mut prefix = ModulePath::new();
    let components = module_path.as_slice();

    for i in 0..components.len() {
        let mut prelude_path = prefix.clone();
        prelude_path.push("prelude".to_string());

        // Skip root-level prelude (checked after the loop)
        if !prelude_path.is_empty() {
            preludes.push(ModuleImport::new(prelude_path, ImportType::Single));
        }

        if i < components.len() - 1 {
            prefix.push(components[i].clone());
        }
    }

    // Always try the root prelude
    let root_prelude = ModulePath::from_parts(&["prelude"]);
    preludes.push(ModuleImport::new(root_prelude, ImportType::Single));

    // Filter to only those that exist
    preludes.retain(|import| find_module_file(&import.path, source_paths).is_some());

    preludes
}

/// Load a module and all its dependencies recursively into the registry.
///
/// Errors from module parsing are tracked in `registry.failed` to prevent
/// cascading "dependency not found" errors. Import loading uses error accumulation
/// (like the Haskell compiler's `forMWithErrors`) to report all failures at once.
fn load_module_recursive(
    file: &Path,
    source_paths: &[(PathBuf, ModulePath)],
    registry: &mut ModuleRegistry,
    loaded: &mut HashSet<PathBuf>,
) -> CompileResult<()> {
    debug_assert!(
        file.exists(),
        "module file does not exist: {}",
        file.display()
    );

    let canonical = file.canonicalize().map_err(CompilationError::Io)?;

    if loaded.contains(&canonical) {
        return Ok(());
    }

    loaded.insert(canonical.clone());

    let parsed = parse_kit_file(file).inspect_err(|_| {
        let module_path = determine_module_path(file, source_paths);
        registry.failed.insert(module_path);
    })?;
    let ParsedFile {
        includes,
        imports,
        program,
        usings,
    } = parsed;
    let module_path = determine_module_path(file, source_paths);

    // Load preludes first (following Haskell compiler convention).
    // Skip prelude resolution if the module itself is named "prelude" to avoid infinite recursion.
    let prelude_imports = if module_path.as_slice().last().map(String::as_str) == Some("prelude") {
        Vec::new()
    } else {
        resolve_preludes(&module_path, source_paths)
    };
    for prelude in &prelude_imports {
        if !registry.contains(&prelude.path)
            && let Some(prelude_file) = find_module_file(&prelude.path, source_paths)
        {
            if registry.failed.contains(&prelude.path) {
                continue;
            }
            load_module_recursive(&prelude_file, source_paths, registry, loaded)?;
        }
    }

    // Resolve wildcard imports to concrete module paths
    let mut resolved_imports = Vec::new();
    for import in &imports {
        match import.import_type {
            ImportType::Single => resolved_imports.push(import.clone()),
            ImportType::Wildcard | ImportType::DoubleWildcard => {
                let concrete_paths =
                    resolve_wildcard_import(&import.path, import.import_type, source_paths);
                for concrete in concrete_paths {
                    resolved_imports.push(ModuleImport::new(concrete, ImportType::Single));
                }
            }
        }
    }

    let module = Module {
        path: module_path.clone(),
        source_path: canonical.clone(),
        imports: resolved_imports.clone(),
        includes,
        program: Program {
            module_path: Some(module_path.clone()),
            ..program
        },
        is_c_module: false,
        mod_using: usings,
    };

    registry.register(module)?;

    // Recursively load imported modules, accumulating errors like Haskell's forMWithErrors.
    let mut errors: Vec<CompilationError> = Vec::new();
    for import in &resolved_imports {
        if registry.contains(&import.path) {
            continue;
        }
        if registry.failed.contains(&import.path) {
            errors.push(CompilationError::ModuleNotFound {
                path: format!("{} (dependency failed to compile)", import.path),
            });
            continue;
        }
        if let Some(import_file) = find_module_file(&import.path, source_paths) {
            if let Err(e) = load_module_recursive(&import_file, source_paths, registry, loaded) {
                registry.failed.insert(import.path.clone());
                errors.push(e);
            }
        } else {
            errors.push(CompilationError::ModuleNotFound {
                path: import.path.to_string(),
            });
        }
    }

    if errors.len() == 1 {
        return Err(errors.swap_remove(0));
    }
    if !errors.is_empty() {
        return Err(CompilationError::CompileError(format!(
            "Multiple errors loading modules:\n{}",
            errors
                .iter()
                .map(|e| format!("  - {e}"))
                .collect::<Vec<_>>()
                .join("\n"),
        )));
    }

    Ok(())
}

/// Merge all module programs into a single program for type inference.
/// Functions from non-entry modules are prepended to serve as C forward declarations.
pub(crate) fn merge_modules_for_inference(
    registry: &ModuleRegistry,
    sorted_paths: &[ModulePath],
) -> Program {
    let mut merged = Program::empty();

    for path in sorted_paths {
        if let Some(module) = registry.get(path) {
            merged.globals.extend(module.program.globals.clone());
            merged
                .functions
                .extend(module.program.functions.iter().cloned());
            merged.structs.extend(module.program.structs.clone());
            merged.enums.extend(module.program.enums.clone());
        }
    }

    merged.module_path = sorted_paths.last().cloned();
    merged
}

impl Compiler {
    /// Get standard library search paths from environment variables and system defaults.
    fn get_stdlib_paths() -> Vec<(PathBuf, ModulePath)> {
        if let Ok(std_path) = env::var("KIT_STD_PATH") {
            return vec![(PathBuf::from(std_path), ModulePath::new())];
        }

        if let Ok(exe_path) = env::current_exe()
            && let Some(exe_dir) = exe_path.parent()
        {
            let std_dir = exe_dir.join("std");
            if std_dir.join("kit").exists() {
                return vec![(std_dir, ModulePath::from_parts(&["kit"]))];
            }
        }

        #[cfg(target_os = "linux")]
        {
            let default = PathBuf::from("/usr/lib/kit");
            if default.exists() {
                return vec![(default, ModulePath::from_parts(&["kit"]))];
            }
        }

        #[cfg(target_os = "macos")]
        {
            let default = PathBuf::from("/usr/local/lib/kit");
            if default.exists() {
                return vec![(default, ModulePath::from_parts(&["kit"]))];
            }
        }

        Vec::new()
    }

    /// Create a new compiler instance with the given source files and configuration.
    pub fn new(
        files: Vec<PathBuf>,
        output: impl AsRef<Path>,
        libs: Vec<String>,
        source_paths: &[String],
    ) -> Self {
        let mut parsed_source_paths: Vec<(PathBuf, ModulePath)> = source_paths
            .iter()
            .filter_map(|sp| parse_source_path(sp))
            .collect();

        if parsed_source_paths.is_empty() {
            parsed_source_paths.push((PathBuf::from("src"), ModulePath::new()));
        }

        parsed_source_paths.extend(Self::get_stdlib_paths());

        let output_path = output.as_ref().to_path_buf();
        let c_output = output_path.with_extension("c");

        let build_dir = {
            let mut dir = output_path.parent().unwrap_or(Path::new(".")).to_path_buf();
            if let Some(stem) = output_path.file_stem().and_then(|s| s.to_str()) {
                dir.push(format!("{}_modules", stem));
            } else {
                dir.push("kit_modules");
            }
            dir
        };

        Self {
            files,
            output: output_path,
            c_output,
            build_dir,
            libs,
            source_paths: parsed_source_paths,
            inferencer: TypeInferencer::new(),
            current_module: ModulePath::new(),
            registry: ModuleRegistry::new(),
        }
    }

    /// Build the module dependency graph by loading the entry file and all imports.
    fn build_module_graph(&mut self) -> CompileResult<Vec<ModulePath>> {
        let source_paths = self.source_paths.clone();
        let mut loaded = HashSet::new();
        let mut registry = ModuleRegistry::new();

        for file in &self.files {
            load_module_recursive(file, &source_paths, &mut registry, &mut loaded)?;
        }

        registry.finalize_graph()?;
        let sorted = registry.topological_sort()?;
        debug_assert_eq!(
            sorted.len(),
            registry.module_count(),
            "topological sort missed modules"
        );
        self.registry = registry;
        Ok(sorted)
    }

    /// Compile a Kit source file to an executable.
    ///
    /// The compilation pipeline:
    /// 1. Build the module dependency graph
    /// 2. Generate per-module `.c` and `.h` files
    /// 3. Generate a merged flat `.c` file for backward compatibility
    /// 4. Invoke the system C compiler to link everything into an executable
    pub fn compile(&mut self) -> CompileResult<()> {
        let sorted_paths = self.build_module_graph()?;

        let module_c_files = self.generate_per_module_files(&sorted_paths)?;

        // Collect linked library names from include statements
        for module in self.registry.all_modules() {
            for inc in &module.includes {
                if let Some(ref lib) = inc.linked_lib
                    && !self.libs.contains(lib)
                {
                    self.libs.push(lib.clone());
                }
            }
        }

        self.current_module = ModulePath::new();

        // HACK: infer_program runs twice on the same inferencer.
        // First inside generate_per_module_files, then here for flat output.
        // FIXME: reset inferencer or skip the redundant flat path entirely.
        let mut merged = merge_modules_for_inference(&self.registry, &sorted_paths);
        self.inferencer.infer_program(&mut merged)?;
        self.transpile_with_program(&merged)?;

        let target_path = self
            .output
            .clone()
            .into_os_string()
            .into_string()
            .map_err(|_| CompilationError::InvalidOutputPath)?;

        let source_strs: Vec<String> = module_c_files
            .iter()
            .map(|c_file| c_file.to_string_lossy().into_owned())
            .collect();

        let (detected_toolchain, detected_path) =
            Toolchain::executable_path().ok_or(CompilationError::ToolchainNotFound)?;

        if matches!(detected_toolchain, Toolchain::Other) {
            return Err(CompilationError::UnsupportedToolchain(
                detected_path.display().to_string(),
            ));
        }

        let opts = CompilerOptions::new(CompilerMeta(detected_toolchain))
            .compiler_path(detected_path)
            .link_libs(&self.libs)
            .lib_paths(&["/usr/local/lib"])
            .sources(&source_strs)
            .output(&target_path)
            .includes(slice::from_ref(&self.build_dir))
            .build();

        let (compiler_path, args) = opts
            .build_invocation()
            .map_err(CompilationError::CompileError)?;

        let output = Command::new(compiler_path)
            .args(&args)
            .output()
            .map_err(CompilationError::Io)?;
        let status = output.status;

        if !status.success() {
            return Err(CompilationError::CCompileError(output.stderr));
        }

        self.cleanup_intermediate_files(&module_c_files);

        Ok(())
    }
}
