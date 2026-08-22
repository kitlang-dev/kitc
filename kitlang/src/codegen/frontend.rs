use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::path::{Component as PathComponent, Path, PathBuf};
use std::process::Command;
use std::slice;
use std::time::Instant;
use walkdir::WalkDir;

use pest::Parser;
use pest::error::{InputLocation, LineColLocation};

use crate::codegen::{
    ast::{Expr, ExprKind, Include, Program, Stmt, StmtKind},
    inference::TypeInferencer,
    module::{ImportType, Module, ModuleImport, ModulePath, ModuleRegistry},
    parser::Parser as CodeParser,
    progress::Progress,
    transpile::{self, CodegenCtx},
    type_ast::{TypeParam, UsingClause},
    types::Type,
};
use crate::error::{self, CompileResult};
use crate::{KitParser, Rule, error::CompilationError};
use kitc_common::{CompilerMeta, CompilerOptions, Toolchain};

/// The Kit compiler, orchestrating module loading, type inference, and C code generation.
pub struct Compiler {
    pub(crate) files: Vec<PathBuf>,
    pub(crate) output: PathBuf,
    pub(crate) build_dir: PathBuf,
    pub(crate) libs: Vec<String>,
    pub(crate) source_paths: Vec<(PathBuf, ModulePath)>,
    pub(crate) inferencer: TypeInferencer,
    pub(crate) registry: ModuleRegistry,
    pub(crate) user_cflags: Vec<String>,
    pub(crate) user_lib_paths: Vec<PathBuf>,
    pub(crate) cc_override: Option<PathBuf>,
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

/// Recursively walk a directory tree collecting `.kit` files, for `**` double-wildcard imports.
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
/// - `Single` returns the module path itself.
/// - `Wildcard` (`.*`) returns all `.kit` files in the module's directory.
/// - `DoubleWildcard` (`.**`) returns all `.kit` files recursively.
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

    let pairs = KitParser::parse(Rule::program, &input).map_err(|e| {
        let (line, col) = match &e.line_col {
            LineColLocation::Pos((l, c)) => (*l, *c),
            LineColLocation::Span((l, c), _) => (*l, *c),
        };
        let (offset, length) = match &e.location {
            InputLocation::Pos(pos) => (*pos, 0),
            InputLocation::Span((start, end)) => (*start, end - start),
        };
        let ctx = error::ErrorContext {
            file: file.display().to_string(),
            source: input.clone(),
            span: error::Span {
                line,
                column: col,
                offset,
                length,
            },
        };
        CompilationError::ParseError(format!("{}: {}", file.display(), e)).with_context(ctx)
    })?;

    let parser = CodeParser::new().with_source(file.display().to_string(), input.clone());
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
    let mut defaults = Vec::new();
    let mut usings = Vec::new();

    for pair in pairs {
        match pair.as_rule() {
            Rule::include_stmt => includes.push(parser.parse_include(pair)?),
            Rule::import_stmt => imports.push(parser.parse_import(pair)?),
            Rule::var_decl => globals.push(parser.parse_global_var_decl(&pair)?),
            Rule::function_decl => functions.push(parser.parse_function(pair, &[])?),
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
            Rule::default_decl => defaults.push(parser.parse_default_decl(pair)?),
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
        defaults,
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
/// Errors from module parsing are tracked in `registry.failed` to prevent cascading "dependency
/// not found" errors. Import loading uses error accumulation (like the Haskell compiler's for
/// `forMWithErrors`) to report all failures at once.
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
            merged.typedefs.extend(module.program.typedefs.clone());
            merged.traits.extend(module.program.traits.iter().cloned());
            merged.impls.extend(module.program.impls.iter().cloned());
            merged
                .defaults
                .extend(module.program.defaults.iter().cloned());
        }
    }

    merged.module_path = sorted_paths.last().cloned();
    merged
}

/// Validate generic declarations and applications before type inference.
///
/// All checks are syntactic; they run on the merged program before inference and the
/// monomorphization fixpoint:
///
/// - every `Type::Instance` must reference a generic definition and supply no
///   more type arguments than the definition declares (fewer are allowed:
///   missing arguments become fresh unknowns, mirroring `makeGeneric` in the
///   reference, e.g. `var b: WrapperType;`);
/// - type parameters used in generic bodies must be declared by that generic
///   definition (or an enclosing one, e.g. a trait method body).
///
/// # Errors
///
/// Returns `TypeError` for an `Instance` that references an unknown generic or
/// supplies too many type arguments, or a `TypeParam` used outside its generic.
pub(crate) fn validate_generics(merged: &Program) -> CompileResult<()> {
    let arity: HashMap<&str, usize> = merged
        .structs
        .iter()
        .filter(|s| !s.type_params.is_empty())
        .map(|s| (s.name.as_str(), s.type_params.len()))
        .chain(
            merged
                .enums
                .iter()
                .filter(|e| !e.type_params.is_empty())
                .map(|e| (e.name.as_str(), e.type_params.len())),
        )
        .collect();

    for instance in collect_instances(merged) {
        // `collect_instances` only yields `Type::Instance`, so this binding can never fail. The
        // `else` branch is a guard against later changes to `collect_instances`.
        let Type::Instance { base, args } = &instance else {
            continue;
        };

        // A non-generic type was used with type arguments (e.g. using `File` as `File[Int]`).
        let expected = arity.get(base.as_str()).copied().ok_or_else(|| {
            CompilationError::TypeError(format!(
                "type '{}' is not generic but is used with type arguments",
                base,
            ))
        })?;

        // Encountered a genuine user error where more type arguments than the generic declares
        // (e.g. `List[K, V]`) for a single-parameter `List[T]`.
        if args.len() > expected {
            return Err(CompilationError::TypeError(format!(
                "type '{}' expects {} type argument(s), got {}",
                base,
                expected,
                args.len(),
            )));
        }
    }

    validate_type_param_scopes(merged)
}

/// All `Type::Instance` occurrences reachable from a program's definitions, cloned so the returned
/// list is independent of the program's lifetime.
fn collect_instances(merged: &Program) -> Vec<Type> {
    let mut out = Vec::new();

    let visit = |t: &Type, out: &mut Vec<Type>| {
        if let Type::Instance { .. } = t {
            out.push(t.clone());
        }
    };

    for s in &merged.structs {
        for f in &s.fields {
            walk_type(&f.annotation, &mut |t| visit(t, &mut out));
        }
    }

    for e in &merged.enums {
        for v in &e.variants {
            for a in &v.args {
                walk_type(&a.annotation, &mut |t| visit(t, &mut out));
            }
        }
    }

    for f in &merged.functions {
        for p in &f.params {
            walk_type(&p.annotation, &mut |t| visit(t, &mut out));
        }
        walk_type(&f.return_type, &mut |t| visit(t, &mut out));
        for stmt in &f.body.stmts {
            walk_stmt_types(stmt, &mut |t| visit(t, &mut out));
        }
    }

    for g in &merged.globals {
        walk_type(&g.annotation, &mut |t| visit(t, &mut out));
    }

    for t in &merged.traits {
        for m in &t.methods {
            for p in &m.params {
                walk_type(&p.annotation, &mut |t| visit(t, &mut out));
            }
            walk_type(&m.return_type, &mut |t| visit(t, &mut out));
        }
    }

    for i in &merged.impls {
        for m in &i.methods {
            for p in &m.params {
                walk_type(&p.annotation, &mut |t| visit(t, &mut out));
            }
            walk_type(&m.return_type, &mut |t| visit(t, &mut out));
        }
    }

    out
}

/// Visit every type in an expression statement tree, invoking `visit` on
/// `Type::Instance` leaves found.
fn walk_stmt_types(stmt: &Stmt, visit: &mut dyn FnMut(&Type)) {
    match &stmt.kind {
        StmtKind::VarDecl {
            annotation, init, ..
        } => {
            walk_type(annotation, visit);
            if let Some(init) = init {
                walk_expr_types(init, visit);
            }
        }
        StmtKind::Expr(e) => walk_expr_types(e, visit),
        StmtKind::Return(e) => {
            if let Some(e) = e {
                walk_expr_types(e, visit);
            }
        }
        StmtKind::Defer { body } => walk_stmt_types(body, visit),
        StmtKind::Block(b) => {
            for s in &b.stmts {
                walk_stmt_types(s, visit);
            }
        }
        StmtKind::If {
            cond,
            then_branch,
            else_branch,
        } => {
            walk_expr_types(cond, visit);
            for s in &then_branch.stmts {
                walk_stmt_types(s, visit);
            }
            if let Some(e) = else_branch {
                for s in &e.stmts {
                    walk_stmt_types(s, visit);
                }
            }
        }
        StmtKind::While { cond, body } => {
            walk_expr_types(cond, visit);
            for s in &body.stmts {
                walk_stmt_types(s, visit);
            }
        }
        StmtKind::For { iter, body, .. } => {
            walk_expr_types(iter, visit);
            for s in &body.stmts {
                walk_stmt_types(s, visit);
            }
        }
        StmtKind::Match(m) => {
            walk_expr_types(&m.expr, visit);
            for arm in &m.arms {
                for s in &arm.body.stmts {
                    walk_stmt_types(s, visit);
                }
            }
        }
        StmtKind::Break | StmtKind::Continue => {}
    }
}

#[allow(clippy::only_used_in_recursion)]
fn walk_expr_types(expr: &Expr, visit: &mut dyn FnMut(&Type)) {
    match &expr.kind {
        ExprKind::BinaryOp { left, right, .. } => {
            walk_expr_types(left, visit);
            walk_expr_types(right, visit);
        }
        ExprKind::UnaryOp { expr, .. } => walk_expr_types(expr, visit),
        ExprKind::Assign { left, right, .. } => {
            walk_expr_types(left, visit);
            walk_expr_types(right, visit);
        }
        ExprKind::Call { callee, args } => {
            walk_expr_types(callee, visit);
            for a in args {
                walk_expr_types(a, visit);
            }
        }
        ExprKind::Index { expr, index } => {
            walk_expr_types(expr, visit);
            walk_expr_types(index, visit);
        }
        ExprKind::FieldAccess { expr, .. } => walk_expr_types(expr, visit),
        ExprKind::If {
            cond,
            then_branch,
            else_branch,
        } => {
            walk_expr_types(cond, visit);
            walk_expr_types(then_branch, visit);
            walk_expr_types(else_branch, visit);
        }
        ExprKind::RangeLiteral { start, end } => {
            walk_expr_types(start, visit);
            walk_expr_types(end, visit);
        }
        ExprKind::StructInit { fields, .. } => {
            for f in fields {
                walk_expr_types(&f.value, visit);
            }
        }
        ExprKind::ArrayLiteral { elements } => {
            for e in elements {
                walk_expr_types(e, visit);
            }
        }
        ExprKind::TupleLit { elements } => {
            for e in elements {
                walk_expr_types(e, visit);
            }
        }
        ExprKind::EnumInit { args, .. } => {
            for a in args {
                walk_expr_types(a, visit);
            }
        }
        ExprKind::Identifier { .. } | ExprKind::Literal { .. } | ExprKind::EnumVariant { .. } => {}
    }
}

fn walk_type(t: &Option<Type>, visit: &mut dyn FnMut(&Type)) {
    let inner = |t: &Type, visit: &mut dyn FnMut(&Type)| match t {
        Type::Instance { args, .. } => {
            visit(t);
            for a in args {
                walk_type(&Some(a.clone()), visit);
            }
        }
        Type::Ptr(inner) => walk_type(&Some((**inner).clone()), visit),
        Type::Tuple(elems) => {
            for e in elems {
                walk_type(&Some(e.clone()), visit);
            }
        }
        Type::Function { param_tys, ret_ty } => {
            for p in param_tys {
                walk_type(&Some(p.clone()), visit);
            }
            walk_type(&Some((**ret_ty).clone()), visit);
        }
        _ => {}
    };
    if let Some(t) = t {
        inner(t, visit);
    }
}

/// Every `Type::TypeParam` in a definition's signature must be declared in that definition's
/// `type_params` (or, for trait/impl methods, the enclosing trait's).
fn validate_type_param_scopes(merged: &Program) -> CompileResult<()> {
    let structs: HashMap<&str, Vec<&str>> = merged
        .structs
        .iter()
        .map(|s| (s.name.as_str(), names_of(&s.type_params)))
        .collect();

    let enums: HashMap<&str, Vec<&str>> = merged
        .enums
        .iter()
        .map(|e| (e.name.as_str(), names_of(&e.type_params)))
        .collect();

    for s in &merged.structs {
        let declared = structs
            .get(s.name.as_str())
            .map_or(&[] as &[&str], Vec::as_slice);
        for f in &s.fields {
            check_type_param_refs(&f.annotation, declared, &s.name)?;
        }
    }

    for e in &merged.enums {
        let declared = enums
            .get(e.name.as_str())
            .map_or(&[] as &[&str], Vec::as_slice);

        for v in &e.variants {
            for a in &v.args {
                check_type_param_refs(&a.annotation, declared, &e.name)?;
            }
        }
    }

    for f in &merged.functions {
        let declared = names_of(&f.type_params);
        for p in &f.params {
            check_type_param_refs(&p.annotation, &declared, &f.name)?;
        }
        check_type_param_refs(&f.return_type, &declared, &f.name)?;
        for stmt in &f.body.stmts {
            check_stmt_type_param_refs(stmt, &declared, &f.name)?;
        }
    }

    for t in &merged.traits {
        let declared = names_of(&t.params);
        for m in &t.methods {
            for p in &m.params {
                check_type_param_refs(&p.annotation, &declared, &t.name)?;
            }
            check_type_param_refs(&m.return_type, &declared, &t.name)?;
        }
    }

    for i in &merged.impls {
        let declared = names_of(&i.params);
        for m in &i.methods {
            for p in &m.params {
                check_type_param_refs(&p.annotation, &declared, &i.name)?;
            }
            check_type_param_refs(&m.return_type, &declared, &i.name)?;
        }
    }

    Ok(())
}

fn names_of(params: &[TypeParam]) -> Vec<&str> {
    params.iter().map(|tp| tp.name.as_str()).collect()
}

/// Check a single type annotation for unknown type-parameter references.
fn check_type_param_refs(t: &Option<Type>, declared: &[&str], owner: &str) -> CompileResult<()> {
    let mut err: Option<CompilationError> = None;
    let mut check = |ty: &Type| {
        if let Type::TypeParam(name) = ty
            && !declared.contains(&name.as_str())
            && err.is_none()
        {
            err = Some(CompilationError::TypeError(format!(
                "type parameter '{}' used in '{}' is not declared",
                name, owner,
            )));
        }
    };
    walk_type(t, &mut check);
    match err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

fn check_stmt_type_param_refs(stmt: &Stmt, declared: &[&str], owner: &str) -> CompileResult<()> {
    let mut err: Option<CompilationError> = None;
    let mut check = |ty: &Type| {
        if let Type::TypeParam(name) = ty
            && !declared.contains(&name.as_str())
            && err.is_none()
        {
            err = Some(CompilationError::TypeError(format!(
                "type parameter '{}' used in '{}' is not declared",
                name, owner,
            )));
        }
    };
    walk_stmt_types(stmt, &mut check);
    match err {
        Some(e) => Err(e),
        None => Ok(()),
    }
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
        user_cflags: Vec<String>,
        user_lib_paths: Vec<String>,
        cc_override: Option<PathBuf>,
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

        let build_dir = {
            let mut dir = output_path.parent().unwrap_or(Path::new(".")).to_path_buf();
            if let Some(stem) = output_path.file_stem().and_then(|s| s.to_str()) {
                dir.push(format!("{}_modules", stem));
            } else {
                dir.push("kit_modules");
            }
            dir
        };

        let user_lib_paths_buf: Vec<PathBuf> =
            user_lib_paths.into_iter().map(PathBuf::from).collect();

        Self {
            files,
            output: output_path,
            build_dir,
            libs,
            source_paths: parsed_source_paths,
            inferencer: TypeInferencer::new(),
            registry: ModuleRegistry::new(),
            user_cflags,
            user_lib_paths: user_lib_paths_buf,
            cc_override,
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

    /// Maximum number of inference/monomorphization passes (like the reference's
    /// `ctxRecursionLimit`).
    const MONOMORPH_PASS_LIMIT: usize = 64;

    /// Compile a Kit source file to an executable.
    ///
    /// The compilation pipeline:
    /// 1. Build the module dependency graph
    /// 2. Register C header declarations from includes
    /// 3. Type inference on the merged program, in a fixpoint that also
    ///    generates monomorphs of generic templates
    /// 4. Generate per-module `.c` and `.h` files
    /// 5. Invoke the system C compiler to link everything into an executable
    ///
    /// # Errors
    ///
    /// Returns `CompilationError` if module loading, header processing, type
    /// inference, code generation, or C compilation fails.
    pub fn compile(&mut self, progress: &dyn Progress) -> CompileResult<()> {
        progress.stage("Loading modules");
        let modules_start = Instant::now();
        let sorted_paths = self.build_module_graph()?;
        progress.stage_done("Loading modules", modules_start.elapsed());

        // Set source context on the inferencer for error reporting.
        if let Some(first_file) = self.files.first()
            && let Ok(source_text) = fs::read_to_string(first_file)
        {
            self.inferencer = std::mem::take(&mut self.inferencer)
                .with_source(first_file.to_string_lossy().to_string(), source_text);
        }

        progress.stage("Processing C headers");
        let headers_start = Instant::now();
        // Register C header declarations from all modules' include statements.
        // This must happen BEFORE type inference so C function signatures are available.
        for module in self.registry.all_modules() {
            let source_path = module.source_path.clone();
            let includes = module.includes.clone();
            if !includes.is_empty() {
                log::info!(
                    "Processing {} include(s) for module '{}'",
                    includes.len(),
                    module.path
                );
                super::ffi::register_module_includes(
                    &includes,
                    &source_path,
                    &mut self.inferencer,
                )?;
            }
        }
        progress.stage_done("Processing C headers", headers_start.elapsed());

        progress.stage("Type checking");
        let inference_start = Instant::now();
        // Type inference on the merged program, in a fixpoint with monomorph
        // generation (mirrors the reference's `typeIterative`): every pass
        // types concrete declarations and records generic applications; the
        // realized monomorphs are merged in and re-typed on the next pass,
        // until no new monomorphs are produced.
        let mut merged = merge_modules_for_inference(&self.registry, &sorted_paths);
        validate_generics(&merged)?;
        for path in &sorted_paths {
            if let Some(module) = self.registry.get(path) {
                self.inferencer.register_templates(path, &module.program);
                self.inferencer.register_impls(path, &module.program)?;
            }
        }
        // Validate trait implementations (duplicate / missing trait / incomplete / mismatched
        // signature) before codegen. Runs after every module's impls are registered; trait
        // definitions come from the merged program so cross-module trait/impl pairs are checked
        // consistently.
        self.inferencer.validate_trait_impls(&merged.traits)?;
        // Prepared impl methods are emitted and inferred like ordinary functions; appending them to
        // the merged program lets the fixpoint infer their bodies and transpile emit them.
        let impl_methods = std::mem::take(&mut self.inferencer.monomorphs.impl_methods);
        merged.functions.extend(impl_methods);
        let mut passes = 0;
        loop {
            self.inferencer.infer_program(&mut merged)?;
            let new_monomorphs = self.inferencer.generate_monomorphs(&mut merged)?;
            if new_monomorphs == 0 {
                break;
            }
            passes += 1;
            if passes > Self::MONOMORPH_PASS_LIMIT {
                return Err(CompilationError::TypeError(format!(
                    "Maximum number of compile passes exceeded while monomorphizing \
                     generic definitions ({} passes)",
                    passes,
                )));
            }
        }
        self.inferencer.validate_monomorphs()?;
        progress.stage_done("Type checking", inference_start.elapsed());

        progress.stage("Expanding defer statements");
        super::defer_expand::expand_defers(&mut merged);

        progress.stage("Generating C code");
        let codegen_start = Instant::now();
        // Generate per-module C code
        let mut ctx = CodegenCtx {
            inferencer: &self.inferencer,
            registry: &self.registry,
            current_module: ModulePath::new(),
            build_dir: &self.build_dir,
        };
        let module_c_files = ctx.generate_per_module_files(&sorted_paths, &merged)?;
        progress.stage_done("Generating C code", codegen_start.elapsed());

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

        // Resolve the C compiler to invoke. A `--cc` override takes precedence
        // over auto-detection; otherwise we detect a system toolchain.
        let (detected_toolchain, detected_path) = if let Some(cc) = &self.cc_override {
            let path = cc.clone();
            let toolchain = Toolchain::from_path_lossy(&path);
            (toolchain, path)
        } else {
            Toolchain::executable_path().ok_or(CompilationError::ToolchainNotFound)?
        };

        if matches!(detected_toolchain, Toolchain::Other) {
            return Err(CompilationError::UnsupportedToolchain(
                detected_path.display().to_string(),
            ));
        }

        let opts = CompilerOptions::new(CompilerMeta(detected_toolchain))
            .compiler_path(detected_path.clone())
            .link_libs(&self.libs)
            .user_cflags(&self.user_cflags)
            .user_lib_paths(&self.user_lib_paths)
            .sources(&source_strs)
            .output(&target_path)
            .includes(slice::from_ref(&self.build_dir))
            .build();

        let (compiler_path, args) = opts.build_invocation()?;

        progress.stage("Compiling C");
        let c_compile_start = Instant::now();
        let mut cmd = Command::new(compiler_path);
        cmd.args(&args);

        if detected_toolchain.is_msvc() {
            // cl.exe defaults object-file output to the CWD. Redirect `.obj` files into the
            // build directory so they get cleaned up with the other intermediates.
            cmd.arg(format!("/Fo{}/", self.build_dir.display()));

            // MSVC's cl.exe requires the VS build environment (INCLUDE, LIB, PATH) even when
            // invoked by absolute path from outside a developer prompt.
            let env = kitc_common::compiler_detect::get_compiler_environment(
                detected_toolchain,
                &detected_path,
            );
            for (key, value) in env {
                cmd.env(key, value);
            }
        }

        let output = cmd.output().map_err(CompilationError::Io)?;
        let status = output.status;

        if !status.success() {
            return Err(CompilationError::CCompileError(output.stderr));
        }
        progress.stage_done("Compiling C", c_compile_start.elapsed());

        transpile::cleanup_intermediate_files(&module_c_files, &self.build_dir);

        Ok(())
    }
}
