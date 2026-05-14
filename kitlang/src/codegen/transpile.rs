use std::collections::HashSet;
use std::env;
use std::fmt::Write;
use std::fs;
use std::path::PathBuf;

use crate::codegen::ast::{Block, Expr, Function, GlobalDecl, Program, Stmt};
use crate::codegen::frontend::{Compiler, merge_modules_for_inference};
use crate::codegen::module::{Module, ModulePath};
use crate::codegen::name_mangling::{
    mangle_enum_variant, mangle_function, mangle_global, mangle_type,
};
use crate::codegen::type_ast::{EnumDefinition, StructDefinition};
use crate::codegen::types::{ToCRepr, Type, TypeId};
use crate::error::{CompilationError, CompileResult};

use super::ast::Param;
use super::inference::TypeInferencer;

/// Walk all types referenced in a program and invoke `f` for each one.
/// Returns `Some(T)` for each type, with `None` on resolution failure.
// TODO: give Func a proper name
fn visit_program_types(inferencer: &TypeInferencer, prog: &Program, mut f: impl FnMut(&Type)) {
    fn emit(f: &mut impl FnMut(&Type), ty: &Type) {
        f(ty);
    }
    for s in &prog.structs {
        for field in &s.fields {
            if let Ok(ty) = inferencer.store.resolve(field.ty) {
                emit(&mut f, &ty);
            } else if let Some(ref ann) = field.annotation {
                emit(&mut f, ann);
            }
        }
    }
    for e in &prog.enums {
        for v in &e.variants {
            for a in &v.args {
                if let Ok(ty) = inferencer.store.resolve(a.ty) {
                    emit(&mut f, &ty);
                } else if let Some(ref ann) = a.annotation {
                    emit(&mut f, ann);
                }
            }
        }
    }
    for g in &prog.globals {
        if let Ok(ty) = inferencer.store.resolve(g.inferred) {
            emit(&mut f, &ty);
        }
    }
    for func in &prog.functions {
        if let Some(id) = func.inferred_return {
            if let Ok(ty) = inferencer.store.resolve(id) {
                emit(&mut f, &ty);
            }
        } else if let Some(ref r) = func.return_type {
            emit(&mut f, r);
        }
        for p in &func.params {
            if let Ok(ty) = inferencer.store.resolve(p.ty) {
                emit(&mut f, &ty);
            } else if let Some(ref ann) = p.annotation {
                emit(&mut f, ann);
            }
        }
        for stmt in &func.body.stmts {
            if let Stmt::VarDecl { inferred, .. } = stmt
                && let Ok(ty) = inferencer.store.resolve(*inferred)
            {
                emit(&mut f, &ty);
            }
        }
    }
}

/// Collect the set of C headers needed by all types referenced in a program.
fn collect_type_headers(
    inferencer: &crate::codegen::inference::TypeInferencer,
    prog: &Program,
) -> HashSet<String> {
    let mut headers = HashSet::new();
    visit_program_types(inferencer, prog, |t| {
        for h in t.to_c_repr().headers {
            headers.insert(h);
        }
    });
    headers
}

/// Collect type headers plus any C typedef declarations needed.
fn collect_type_headers_and_decls(
    inferencer: &crate::codegen::inference::TypeInferencer,
    prog: &Program,
) -> (HashSet<String>, Vec<String>) {
    let mut headers = HashSet::new();
    let mut decls: Vec<String> = Vec::new();
    visit_program_types(inferencer, prog, |t| {
        let c = t.to_c_repr();
        for h in c.headers {
            headers.insert(h);
        }
        if let Some(d) = c.declaration
            && !decls.contains(&d)
        {
            decls.push(d);
        }
    });
    (headers, decls)
}

impl Compiler {
    /// Generate C code from the merged program and write it to the flat output path.
    pub(crate) fn transpile_with_program(&mut self, prog: &Program) -> CompileResult<()> {
        let c_code = self.generate_flat_c_code(prog);
        fs::write(&self.c_output, c_code).map_err(CompilationError::Io)
    }

    /// Generate C code for a single merged/entry program (flat, no module awareness).
    /// Collects C includes from all registered modules, emits type declarations,
    /// global variables, and function implementations.
    fn generate_flat_c_code(&self, prog: &Program) -> String {
        let mut out = String::new();

        let mut all_c_includes = HashSet::new();
        for module in self.registry.all_modules() {
            for inc in &module.includes {
                all_c_includes.insert(inc.path.clone());
            }
        }
        for path in &all_c_includes {
            let _ = writeln!(out, "#include \"{}\"", path);
        }
        if !all_c_includes.is_empty() {
            out.push('\n');
        }

        let (seen_headers, seen_declarations) =
            collect_type_headers_and_decls(&self.inferencer, prog);

        for hdr in &seen_headers {
            let _ = writeln!(out, "#include {hdr}");
        }
        out.push('\n');

        for decl in &seen_declarations {
            out.push_str(decl);
            out.push('\n');
        }

        for struct_def in &prog.structs {
            out.push_str(&self.generate_struct_declaration(struct_def, &prog.structs));
            out.push('\n');
        }

        for enum_def in &prog.enums {
            out.push_str(&self.generate_enum_declaration(enum_def));
            out.push('\n');
        }

        for global in &prog.globals {
            out.push_str(&self.transpile_global(global));
            out.push('\n');
        }

        for func in &prog.functions {
            out.push_str(&self.transpile_function(func));
            out.push_str("\n\n");
        }
        out
    }

    /// Generate per-module `.c` and `.h` files, returning paths to all `.c` files.
    ///
    /// Runs type inference on the merged program first, then filters inferred types
    /// back to per-module programs for correct type-aware code generation.
    pub(crate) fn generate_per_module_files(
        &mut self,
        sorted_paths: &[ModulePath],
    ) -> CompileResult<Vec<PathBuf>> {
        debug_assert!(!sorted_paths.is_empty(), "no modules to generate");
        fs::create_dir_all(&self.build_dir).map_err(CompilationError::Io)?;

        let mut c_files = Vec::new();
        let saved_module = self.current_module.clone();

        let mut merged = merge_modules_for_inference(&self.registry, sorted_paths);
        self.inferencer.infer_program(&mut merged)?;

        for path in sorted_paths {
            self.current_module = path.clone();
            if let Some(module) = self.registry.get(path) {
                let func_names: HashSet<String> = module
                    .program
                    .functions
                    .iter()
                    .map(|f| f.name.clone())
                    .collect();
                let global_names: HashSet<String> = module
                    .program
                    .globals
                    .iter()
                    .map(|g| g.name.clone())
                    .collect();
                let struct_names: HashSet<String> = module
                    .program
                    .structs
                    .iter()
                    .map(|s| s.name.clone())
                    .collect();
                let enum_names: HashSet<String> = module
                    .program
                    .enums
                    .iter()
                    .map(|e| e.name.clone())
                    .collect();

                let filtered = Program {
                    module_path: Some(path.clone()),
                    globals: merged
                        .globals
                        .iter()
                        .filter(|g| global_names.contains(&g.name))
                        .cloned()
                        .collect(),
                    functions: merged
                        .functions
                        .iter()
                        .filter(|f| func_names.contains(&f.name))
                        .cloned()
                        .collect(),
                    structs: merged
                        .structs
                        .iter()
                        .filter(|s| struct_names.contains(&s.name))
                        .cloned()
                        .collect(),
                    enums: merged
                        .enums
                        .iter()
                        .filter(|e| enum_names.contains(&e.name))
                        .cloned()
                        .collect(),
                };

                let header = self.generate_module_header_from_program(&filtered, module);
                let h_name = format!("{}.h", path.join("_"));
                fs::write(self.build_dir.join(&h_name), header).map_err(CompilationError::Io)?;

                let c_code = self.generate_module_c_code_from_program(&filtered, module);
                let c_name = format!("{}.c", path.join("_"));
                let c_path = self.build_dir.join(&c_name);
                fs::write(&c_path, c_code).map_err(CompilationError::Io)?;

                c_files.push(c_path);
            }
        }

        self.current_module = saved_module;
        Ok(c_files)
    }

    /// Generate a header file for a module using the inferred program data.
    pub(crate) fn generate_module_header_from_program(
        &self,
        prog: &Program,
        module: &Module,
    ) -> String {
        let mut out = String::new();
        let guard = format!("KIT_MODULE_{}_H", module.path.join("_").to_uppercase());
        let _ = writeln!(out, "#ifndef {}", guard);
        let _ = writeln!(out, "#define {}", guard);
        out.push('\n');

        let seen_headers = collect_type_headers(&self.inferencer, prog);
        for hdr in &seen_headers {
            let _ = writeln!(out, "#include {hdr}");
        }
        out.push('\n');

        for import in &module.imports {
            if self.registry.contains(&import.path) {
                let dep = format!("{}.h", import.path.join("_"));
                let _ = writeln!(out, "#include \"{}\"", dep);
            }
        }
        if !module.imports.is_empty() {
            out.push('\n');
        }

        for struct_def in &prog.structs {
            out.push_str(&self.generate_struct_declaration(struct_def, &prog.structs));
            out.push('\n');
        }

        for enum_def in &prog.enums {
            out.push_str(&self.generate_enum_declaration(enum_def));
            out.push('\n');
        }

        for global in &prog.globals {
            if global.is_public {
                let ty = match self.inferencer.store.resolve(global.inferred) {
                    Ok(t) => self.type_to_c_name_with_module(&t, &module.path),
                    Err(_) => global
                        .annotation
                        .as_ref()
                        .map(|a| a.to_c_repr().name)
                        .unwrap_or_else(|| "int".to_string()),
                };
                let gname = mangle_global(&module.path, &global.name);
                let const_ = if global.is_const { "const " } else { "" };
                let _ = writeln!(out, "extern {const_}{ty} {};", gname);
            }
        }
        if prog.globals.iter().any(|g| g.is_public) {
            out.push('\n');
        }

        for func in &prog.functions {
            let ret = if func.name == "main" {
                "int".to_string()
            } else {
                func.inferred_return
                    .and_then(|id| self.inferencer.store.resolve(id).ok())
                    .map(|t| t.to_c_repr().name)
                    .or_else(|| func.return_type.as_ref().map(|t| t.to_c_repr().name))
                    .unwrap_or_else(|| "void".to_string())
            };
            let fname = if func.name == "main" {
                "main".to_string()
            } else {
                mangle_function(&module.path, &func.name)
            };
            let params = self.format_function_params_with_module(&func.params, &module.path);
            let _ = writeln!(out, "{} {}({});", ret, fname, params);
        }

        out.push('\n');
        let _ = writeln!(out, "#endif /* {} */", guard);
        out
    }

    /// Generate a per-module C source file using the filtered (inferred) program data.
    fn generate_module_c_code_from_program(&self, prog: &Program, module: &Module) -> String {
        let mut out = String::new();

        let header = format!("{}.h", module.path.join("_"));
        let _ = writeln!(out, "#include \"{}\"", header);

        for import in &module.imports {
            if self.registry.contains(&import.path) {
                let dep = format!("{}.h", import.path.join("_"));
                let _ = writeln!(out, "#include \"{}\"", dep);
            }
        }

        for inc in &module.includes {
            let _ = writeln!(out, "#include \"{}\"", inc.path);
        }
        out.push('\n');

        let seen_headers = collect_type_headers(&self.inferencer, prog);
        for hdr in &seen_headers {
            let _ = writeln!(out, "#include {hdr}");
        }
        if !seen_headers.is_empty() {
            out.push('\n');
        }

        for global in &prog.globals {
            out.push_str(&self.transpile_global(global));
            out.push('\n');
        }

        for func in &prog.functions {
            out.push_str(&self.transpile_function(func));
            out.push_str("\n\n");
        }

        out
    }

    /// Resolve a type ID to its C type name, falling back to a default on failure.
    fn resolve_type_to_c_name(&self, type_id: TypeId, fallback: &str) -> String {
        debug_assert!(
            type_id != TypeId::default(),
            "resolve_type_to_c_name: unresolved TypeId (default) for '{fallback}'",
        );
        self.inferencer
            .store
            .resolve(type_id)
            .map_or_else(|_| fallback.to_string(), |t| self.type_to_c_name(&t))
    }

    /// Convert a Kit `Type` to its C name, using the current module for name mangling.
    fn type_to_c_name(&self, t: &Type) -> String {
        self.type_to_c_name_with_module(t, &self.current_module)
    }

    /// Convert a Kit `Type` to its C name, using the given module path for mangling.
    fn type_to_c_name_with_module(&self, t: &Type, module: &ModulePath) -> String {
        if let Type::Named(name) = t {
            if self.inferencer.is_struct_type(name) {
                format!("struct {}", mangle_type(module, name))
            } else {
                mangle_type(module, name)
            }
        } else {
            t.to_c_repr().name
        }
    }

    /// Transpile a global variable declaration to C.
    fn transpile_global(&self, global: &GlobalDecl) -> String {
        let ty = self.resolve_type_to_c_name(global.inferred, "int");
        let const_prefix = if global.is_const { "const " } else { "" };
        let global_name = mangle_global(&self.current_module, &global.name);

        match &global.init {
            Some(expr) => {
                let init_str = self.transpile_expr(expr);
                format!("{const_prefix}{ty} {} = {init_str};", global_name)
            }
            None => format!("{const_prefix}{ty} {};", global_name),
        }
    }

    /// Generate a C struct declaration from a Kit struct definition.
    fn generate_struct_declaration(
        &self,
        struct_def: &StructDefinition,
        _all_structs: &[StructDefinition],
    ) -> String {
        let field_decls: Vec<String> = struct_def
            .fields
            .iter()
            .map(|field| {
                let ty = self
                    .inferencer
                    .store
                    .resolve(field.ty)
                    .ok()
                    .or(field.annotation.as_ref().cloned())
                    .unwrap_or(Type::Void);

                let prefix = if field.is_const { "const " } else { "" };
                let cname = self.type_to_c_name(&ty);
                format!("    {}{} {};", prefix, cname, field.name)
            })
            .collect();

        let struct_name = mangle_type(&self.current_module, &struct_def.name);
        format!("struct {} {{\n{}\n}};", struct_name, field_decls.join("\n"))
    }

    /// Generate a C enum declaration from a Kit enum definition.
    /// Simple enums (no data variants) become plain C `enum`s.
    /// Enums with data-carrying variants get a tagged-union layout.
    fn generate_enum_declaration(&self, enum_def: &EnumDefinition) -> String {
        let mut output = String::new();
        let enum_type_name = mangle_type(&self.current_module, &enum_def.name);
        let all_simple = enum_def.variants.iter().all(|v| v.args.is_empty());

        if all_simple {
            let variants: Vec<String> = enum_def
                .variants
                .iter()
                .map(|v| {
                    format!(
                        "    {}",
                        mangle_enum_variant(&self.current_module, &enum_def.name, &v.name)
                    )
                })
                .collect();

            output.push_str(&format!(
                "typedef enum {{\n{}\n}} {};\n\n",
                variants.join(",\n"),
                enum_type_name
            ));
        } else {
            let disc: Vec<String> = enum_def
                .variants
                .iter()
                .map(|v| {
                    format!(
                        "    {}",
                        mangle_enum_variant(&self.current_module, &enum_def.name, &v.name)
                    )
                })
                .collect();
            output.push_str(&format!(
                "typedef enum {{\n{}\n}} {}_Discriminant;\n\n",
                disc.join(",\n"),
                enum_type_name
            ));

            for v in enum_def.variants.iter().filter(|v| !v.args.is_empty()) {
                let fields: Vec<String> = v
                    .args
                    .iter()
                    .map(|arg| {
                        let ty = self
                            .inferencer
                            .store
                            .resolve(arg.ty)
                            .ok()
                            .or(arg.annotation.as_ref().cloned())
                            .unwrap_or(Type::Void);
                        format!("    {} {};", ty.to_c_repr().name, arg.name)
                    })
                    .collect();
                output.push_str(&format!(
                    "typedef struct {{\n{}\n}} {}_{}_data;\n\n",
                    fields.join("\n"),
                    enum_type_name,
                    v.name
                ));
            }

            let union_fields: Vec<String> = enum_def
                .variants
                .iter()
                .filter(|v| !v.args.is_empty())
                .map(|v| {
                    format!(
                        "    {}_{}_data {};",
                        enum_type_name,
                        v.name,
                        v.name.to_lowercase()
                    )
                })
                .collect();

            let body = format!(
                "    {}_Discriminant _discriminant;\n    union {{\n{}\n    }} _variant;",
                enum_type_name,
                union_fields.join("\n")
            );
            output.push_str(&format!(
                "typedef struct {{\n{}\n}} {};\n\n",
                body, enum_type_name
            ));
        }

        for v in enum_def.variants.iter().filter(|v| !v.args.is_empty()) {
            let params: Vec<String> = v
                .args
                .iter()
                .map(|arg| {
                    let ty = self
                        .inferencer
                        .store
                        .resolve(arg.ty)
                        .ok()
                        .or(arg.annotation.as_ref().cloned())
                        .unwrap_or(Type::Void);
                    format!("{} {}", ty.to_c_repr().name, arg.name)
                })
                .collect();
            let arg_names: Vec<String> = v.args.iter().map(|arg| arg.name.clone()).collect();
            let assigns: Vec<String> = v
                .args
                .iter()
                .enumerate()
                .map(|(i, arg)| {
                    format!(
                        "    result._variant.{}.{} = {};",
                        v.name.to_lowercase(),
                        arg.name,
                        arg_names[i]
                    )
                })
                .collect();
            let ctor = mangle_enum_variant(&self.current_module, &enum_def.name, &v.name);
            output.push_str(&format!(
                "{} {}_new({}) {{\n    {} result;\n    result._discriminant = {};\n{}\n    return result;\n}}\n\n",
                enum_type_name, ctor, params.join(", "),
                enum_type_name, ctor, assigns.join("\n")
            ));
        }

        output
    }

    /// Transpile a Kit function definition to C code.
    fn transpile_function(&self, func: &Function) -> String {
        debug_assert!(!func.name.is_empty(), "function with empty name");
        let return_type = if func.name == "main" {
            "int".to_string()
        } else {
            func.inferred_return
                .and_then(|id| self.inferencer.store.resolve(id).ok())
                .map(|t| t.to_c_repr().name)
                .or_else(|| func.return_type.as_ref().map(|t| t.to_c_repr().name))
                .unwrap_or_else(|| "void".to_string())
        };

        let func_name = if func.name == "main" && !self.current_module.is_empty() {
            "main".to_string()
        } else {
            mangle_function(&self.current_module, &func.name)
        };

        let params = self.format_function_params(&func.params);
        let mut body_code = self.transpile_block(&func.body);

        if func.name == "main" {
            let has_return = func.body.stmts.iter().any(|s| matches!(s, Stmt::Return(_)));
            if !has_return && let Some(pos) = body_code.rfind('}') {
                body_code.insert_str(pos, "return 0;\n");
            }
        }

        format!("{} {}({}) {}", return_type, func_name, params, body_code)
    }

    /// Transpile a block of Kit statements to C code.
    fn transpile_block(&self, block: &Block) -> String {
        let mut code = String::from("{\n");
        for stmt in &block.stmts {
            let stmt_code = match stmt {
                Stmt::VarDecl {
                    name,
                    annotation: _,
                    inferred,
                    init,
                } => {
                    let ty_str = self.resolve_type_to_c_name(*inferred, "int");
                    match init {
                        Some(expr) => format!("{ty_str} {name} = {};\n", self.transpile_expr(expr)),
                        None => format!("{ty_str} {name};\n"),
                    }
                }
                Stmt::Expr(expr) => format!("{};\n", self.transpile_expr(expr)),
                Stmt::Return(expr) => match expr {
                    Some(e) => format!("return {};\n", self.transpile_expr(e)),
                    None => "return;\n".to_string(),
                },
                Stmt::If {
                    cond,
                    then_branch,
                    else_branch,
                } => {
                    let mut s = format!("if ({}) ", self.transpile_expr(cond));
                    s.push_str(&self.transpile_block(then_branch));
                    if let Some(else_b) = else_branch {
                        s.push_str(" else ");
                        s.push_str(&self.transpile_block(else_b));
                    }
                    s.push('\n');
                    s
                }
                Stmt::While { cond, body } => {
                    let mut s = format!("while ({}) ", self.transpile_expr(cond));
                    s.push_str(&self.transpile_block(body));
                    s.push('\n');
                    s
                }
                Stmt::For { var, iter, body } => {
                    let mut s = if let Expr::RangeLiteral { start, end } = iter {
                        let start_str = self.transpile_expr(start);
                        let end_str = self.transpile_expr(end);
                        format!("for (int {var} = {start_str}; {var} < {end_str}; ++{var}) ")
                    } else {
                        let iter_str = self.transpile_expr(iter);
                        format!("for (int {var} = 0; {var} < {iter_str}; ++{var}) ")
                    };
                    s.push_str(&self.transpile_block(body));
                    s
                }
                Stmt::Break => "break;\n".to_string(),
                Stmt::Continue => "continue;\n".to_string(),
            };

            for line in stmt_code.lines() {
                code.push_str("    ");
                code.push_str(line);
                code.push('\n');
            }
        }
        code.push('}');
        code
    }

    /// Resolve a function name to (defining module, base function name).
    /// Supports both simple names (`add`) and qualified names (`math.add`).
    fn resolve_function_name(&self, name: &str) -> Option<(ModulePath, String)> {
        self.registry
            .resolve_qualified_name(name, &self.current_module)
    }

    /// Find the module that defines a global variable by name.
    fn find_global_module(&self, name: &str) -> Option<ModulePath> {
        self.registry
            .all_modules()
            .iter()
            .find(|m| m.program.globals.iter().any(|g| g.name == name))
            .map(|m| m.path.clone())
    }

    /// Remove intermediate `.c` and `.h` files from the build directory.
    pub(crate) fn cleanup_intermediate_files(&self, module_c_files: &[PathBuf]) {
        if env::var("KEEP_C").is_ok() {
            return;
        }
        for c_file in module_c_files {
            let _ = fs::remove_file(c_file);
        }
        if env::var("KEEP_H").is_err() {
            self.cleanup_build_dir();
        }
        let _ = fs::remove_file(&self.c_output);
    }

    /// Remove all intermediate files from the build directory.
    fn cleanup_build_dir(&self) {
        let Ok(entries) = fs::read_dir(&self.build_dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let is_intermediate = path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e == "h" || e == "c");
            if is_intermediate {
                let _ = fs::remove_file(&path);
            }
        }
        let _ = fs::remove_dir(&self.build_dir);
    }

    /// Transpile enum variant arguments with defaults filled in.
    fn transpile_enum_args_with_defaults(
        &self,
        enum_name: &str,
        variant_name: &str,
        args: &[Expr],
    ) -> String {
        let enum_def = self.inferencer.symbols().lookup_enum(enum_name);
        let variant = enum_def.and_then(|e| e.variants.iter().find(|v| v.name == *variant_name));

        let Some(variant) = variant else {
            return args
                .iter()
                .map(|a| self.transpile_expr(a))
                .collect::<Vec<_>>()
                .join(", ");
        };

        let mut full_args = args.to_vec();
        for i in args.len()..variant.args.len() {
            if let Some(default) = &variant.args[i].default {
                full_args.push(default.clone());
            }
        }
        full_args
            .iter()
            .map(|a| self.transpile_expr(a))
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Resolve a param's type to its C name using the current module.
    fn format_function_param_type(&self, p: &Param) -> String {
        self.inferencer
            .store
            .resolve(p.ty)
            .map(|t| self.type_to_c_name(&t))
            .or_else(|_| p.annotation.as_ref().map(|t| t.to_c_repr().name).ok_or(()))
            .unwrap_or_else(|_| "void*".to_string())
    }

    /// Resolve a param's type to its C name using the given module.
    fn format_function_param_type_with_module(&self, p: &Param, module: &ModulePath) -> String {
        self.inferencer
            .store
            .resolve(p.ty)
            .map(|t| self.type_to_c_name_with_module(&t, module))
            .or_else(|_| p.annotation.as_ref().map(|t| t.to_c_repr().name).ok_or(()))
            .unwrap_or_else(|_| "void*".to_string())
    }

    /// Format a slice of params as a comma-separated C parameter list.
    fn format_function_params(&self, params: &[Param]) -> String {
        params
            .iter()
            .map(|p| format!("{} {}", self.format_function_param_type(p), p.name))
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Format a slice of params as a comma-separated C parameter list using the given module.
    fn format_function_params_with_module(&self, params: &[Param], module: &ModulePath) -> String {
        params
            .iter()
            .map(|p| {
                format!(
                    "{} {}",
                    self.format_function_param_type_with_module(p, module),
                    p.name
                )
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Transpile a Kit expression to a C expression string.
    fn transpile_expr(&self, expr: &Expr) -> String {
        match expr {
            Expr::Identifier(name, _) => {
                if let Some(mod_path) = self.find_global_module(name) {
                    mangle_global(&mod_path, name)
                } else {
                    name.clone()
                }
            }
            Expr::Literal(lit, _) => lit.to_c(),
            Expr::Call {
                callee,
                args,
                ty: _,
            } => {
                if let Some(info) = self
                    .inferencer
                    .symbols()
                    .lookup_enum_variant_by_simple_name(callee)
                {
                    let a = args
                        .iter()
                        .map(|a| self.transpile_expr(a))
                        .collect::<Vec<_>>()
                        .join(", ");
                    let ctor = mangle_enum_variant(
                        &self.current_module,
                        &info.enum_name,
                        &info.variant_name,
                    );
                    format!("{}_new({})", ctor, a)
                } else {
                    let (mod_path, base_name) = match self.resolve_function_name(callee) {
                        Some((mp, bn)) => (Some(mp), bn),
                        None => {
                            // Extract the last segment for simple-name lookup
                            let last = callee.rsplit('.').next().unwrap_or(callee);
                            (None, last.to_string())
                        }
                    };
                    let mangled = if callee == "main" {
                        callee.clone()
                    } else if let Some(mp) = mod_path {
                        mangle_function(&mp, &base_name)
                    } else if self.inferencer.symbols().lookup_function(callee).is_some()
                        && !self.current_module.is_empty()
                    {
                        mangle_function(&self.current_module, callee)
                    } else {
                        callee.clone()
                    };
                    let a = args
                        .iter()
                        .map(|a| self.transpile_expr(a))
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!("{mangled}({a})")
                }
            }
            Expr::UnaryOp { op, expr, ty: _ } => {
                format!("{}({})", op.to_c_str(), self.transpile_expr(expr))
            }
            Expr::BinaryOp {
                op,
                left,
                right,
                ty: _,
            } => {
                let l = self.transpile_expr(left);
                let r = self.transpile_expr(right);
                format!("({l} {} {r})", op.to_c_str())
            }
            Expr::Assign {
                op,
                left,
                right,
                ty: _,
            } => {
                let l = self.transpile_expr(left);
                let r = self.transpile_expr(right);
                format!("{l} {} {r}", op.to_c_str())
            }
            Expr::If {
                cond,
                then_branch,
                else_branch,
                ty: _,
            } => {
                let c = self.transpile_expr(cond);
                let t = self.transpile_expr(then_branch);
                let e = self.transpile_expr(else_branch);
                format!("({c} ? {t} : {e})")
            }
            Expr::RangeLiteral { .. } => "/* range literal */ 0".to_string(),
            Expr::StructInit {
                ty,
                struct_type: _,
                fields,
            } => {
                let name = match self.inferencer.store.resolve(*ty) {
                    Ok(Type::Struct { name, .. }) | Ok(Type::Named(name)) => name,
                    Ok(_) => "UNKNOWN_STRUCT".to_string(),
                    Err(e) => {
                        eprintln!("Warning: Failed to resolve struct type: {}", e);
                        "UNKNOWN_STRUCT".to_string()
                    }
                };
                let inits = fields
                    .iter()
                    .map(|f| format!(".{} = {}", f.name, self.transpile_expr(&f.value)))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("(struct {}){{{}}}", name, inits)
            }
            Expr::FieldAccess {
                expr,
                field_name,
                ty: _,
            } => {
                format!("{}.{}", self.transpile_expr(expr), field_name)
            }
            Expr::EnumVariant {
                enum_name,
                variant_name,
                ty: _,
            } => {
                let is_simple = self
                    .inferencer
                    .symbols()
                    .lookup_enum(enum_name)
                    .map(|e| e.variants.iter().all(|v| v.args.is_empty()))
                    .unwrap_or(false);
                if is_simple {
                    mangle_enum_variant(&self.current_module, enum_name, variant_name)
                } else {
                    format!(
                        "{{.{} = {}, ._variant = {{0}}}}",
                        "_discriminant",
                        mangle_enum_variant(&self.current_module, enum_name, variant_name)
                    )
                }
            }
            Expr::EnumInit {
                enum_name,
                variant_name,
                args,
                ty: _,
            } => {
                if args.is_empty() {
                    let is_simple = self
                        .inferencer
                        .symbols()
                        .lookup_enum(enum_name)
                        .map(|e| e.variants.iter().all(|v| v.args.is_empty()))
                        .unwrap_or(false);
                    if is_simple {
                        mangle_enum_variant(&self.current_module, enum_name, variant_name)
                    } else {
                        format!(
                            "{{.{} = {}, ._variant = {{0}}}}",
                            "_discriminant",
                            mangle_enum_variant(&self.current_module, enum_name, variant_name)
                        )
                    }
                } else {
                    let a = self.transpile_enum_args_with_defaults(enum_name, variant_name, args);
                    let ctor = mangle_enum_variant(&self.current_module, enum_name, variant_name);
                    format!("{}_new({})", ctor, a)
                }
            }
        }
    }
}
