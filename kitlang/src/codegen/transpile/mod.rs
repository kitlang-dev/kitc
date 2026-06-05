mod enum_gen;
mod header;

use std::collections::HashSet;
use std::env;
use std::fs;
use std::path::PathBuf;

use crate::codegen::ast::{Attributed, Block, Expr, Function, GlobalDecl, Program, Stmt};
use crate::codegen::module::{ModulePath, ModuleRegistry};
use crate::codegen::name_mangling::{mangle_enum_variant, mangle_name};
use crate::codegen::types::{ToCRepr, Type, TypeId};

use super::ast::Param;
use super::inference::TypeInferencer;

/// Context for C code generation, borrowing inference results and module registry.
/// Constructed after type inference completes - all methods are read-only on analysis data.
pub(crate) struct CodegenCtx<'a> {
    pub(crate) inferencer: &'a TypeInferencer,
    pub(crate) registry: &'a ModuleRegistry,
    pub(crate) current_module: ModulePath,
    pub(crate) build_dir: &'a PathBuf,
}

/// Check if a declaration in the given module field is marked #[extern] or #[expose].
macro_rules! has_no_mangle_in_module {
    ($registry:expr, $mod_path:expr, $name:expr, $field:ident) => {
        $registry
            .get($mod_path)
            .and_then(|m| m.program.$field.iter().find(|item| item.name == $name))
            .is_some_and(|item| item.has_no_mangle())
    };
}

/// Walk all types referenced in a program and invoke `f` for each one.
fn visit_program_types(inferencer: &TypeInferencer, prog: &Program, mut f: impl FnMut(&Type)) {
    for s in &prog.structs {
        for field in &s.fields {
            if let Ok(ty) = inferencer.store.resolve(field.ty) {
                f(&ty);
            } else if let Some(ref ann) = field.annotation {
                f(ann);
            }
        }
    }
    for e in &prog.enums {
        for v in &e.variants {
            for a in &v.args {
                if let Ok(ty) = inferencer.store.resolve(a.ty) {
                    f(&ty);
                } else if let Some(ref ann) = a.annotation {
                    f(ann);
                }
            }
        }
    }
    for g in &prog.globals {
        if let Ok(ty) = inferencer.store.resolve(g.inferred) {
            f(&ty);
        }
    }
    for func in &prog.functions {
        if let Some(id) = func.inferred_return {
            if let Ok(ty) = inferencer.store.resolve(id) {
                f(&ty);
            }
        } else if let Some(ref r) = func.return_type {
            f(r);
        }

        for p in &func.params {
            if let Ok(ty) = inferencer.store.resolve(p.ty) {
                f(&ty);
            } else if let Some(ref ann) = p.annotation {
                f(ann);
            }
        }

        for stmt in &func.body.stmts {
            if let Stmt::VarDecl { inferred, .. } = stmt
                && let Ok(ty) = inferencer.store.resolve(*inferred)
            {
                f(&ty);
            }
        }
    }

    for tdef in &prog.typedefs {
        f(&tdef.type_def);
    }
}

/// Collect type headers plus any C typedef declarations needed.
pub(super) fn collect_type_headers_and_decls(
    inferencer: &TypeInferencer,
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

impl CodegenCtx<'_> {
    fn expr_type_id(expr: &Expr) -> TypeId {
        match expr {
            Expr::Identifier { ty, .. }
            | Expr::Literal { ty, .. }
            | Expr::Call { ty, .. }
            | Expr::UnaryOp { ty, .. }
            | Expr::BinaryOp { ty, .. }
            | Expr::Assign { ty, .. }
            | Expr::If { ty, .. }
            | Expr::StructInit { ty, .. }
            | Expr::FieldAccess { ty, .. }
            | Expr::EnumVariant { ty, .. }
            | Expr::EnumInit { ty, .. } => *ty,
            Expr::ArrayLiteral { ty, .. } => *ty,
            Expr::Index { ty, .. } => *ty,
            Expr::RangeLiteral { .. } => TypeId::default(),
        }
    }

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

    fn type_to_c_name(&self, t: &Type) -> String {
        // Resolve typedef aliases so variables use the underlying C type name
        let resolved = self
            .inferencer
            .store
            .resolve_typedef_type(t)
            .unwrap_or_else(|| t.clone());
        self.type_to_c_name_with_module(&resolved, &self.current_module)
    }

    fn type_to_c_name_with_module(&self, t: &Type, module: &ModulePath) -> String {
        if let Type::Named(name) = t {
            if self.inferencer.is_struct_type(name) {
                format!("struct {}", mangle_name(module, name))
            } else {
                mangle_name(module, name)
            }
        } else {
            t.to_c_repr().name
        }
    }

    /// Resolve a function's return type to its C name, defaulting to "int" for main and "void" otherwise.
    fn resolve_return_type_c_name(&self, func: &Function) -> String {
        if func.name == "main" {
            return "int".to_string();
        }
        func.inferred_return
            .and_then(|id| self.inferencer.store.resolve(id).ok())
            .map(|t| t.to_c_repr().name)
            .or_else(|| func.return_type.as_ref().map(|t| t.to_c_repr().name))
            .unwrap_or_else(|| "void".to_string())
    }

    fn transpile_global(&self, global: &GlobalDecl) -> String {
        let module = global.mangling_module(&self.current_module);
        let global_name = mangle_name(&module, &global.name);
        let decl = self.format_var_decl(global.inferred, &global_name);
        let const_prefix = if global.is_const { "const " } else { "" };
        let extern_prefix = if global.is_extern() { "extern " } else { "" };

        match &global.init {
            // Array literals as initializers need plain brace-enclosed lists
            Some(Expr::ArrayLiteral { elements, .. }) => {
                let elems = elements
                    .iter()
                    .map(|e| self.transpile_expr(e))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{extern_prefix}{const_prefix}{decl} = {{{elems}}};")
            }
            Some(expr) => {
                let init_str = self.transpile_expr(expr);
                format!("{extern_prefix}{const_prefix}{decl} = {init_str};")
            }
            None => format!("{extern_prefix}{const_prefix}{decl};"),
        }
    }

    fn transpile_function(&self, func: &Function) -> String {
        debug_assert!(!func.name.is_empty(), "function with empty name");
        let return_type = self.resolve_return_type_c_name(func);
        let module = func.mangling_module(&self.current_module);
        let func_name = if func.name == "main" && !self.current_module.is_empty() {
            "main".to_string()
        } else {
            mangle_name(&module, &func.name)
        };

        let params = self.format_function_params(&func.params);
        let mut body_code = self.transpile_block(&func.body);

        if func.name == "main" {
            let has_return = func.body.stmts.iter().any(|s| matches!(s, Stmt::Return(_)));
            if !has_return && let Some(pos) = body_code.rfind('}') {
                body_code.insert_str(pos, "return 0;\n");
            }
        }

        let extern_prefix = if func.is_extern() { "extern " } else { "" };
        format!(
            "{extern_prefix}{} {}({}) {}",
            return_type, func_name, params, body_code
        )
    }

    fn transpile_stmt(&self, stmt: &Stmt) -> String {
        match stmt {
            Stmt::VarDecl {
                name,
                annotation: _,
                inferred,
                init,
            } => {
                let decl = self.format_var_decl(*inferred, name);
                match init {
                    Some(Expr::ArrayLiteral { elements, .. }) => {
                        let elems = elements
                            .iter()
                            .map(|e| self.transpile_expr(e))
                            .collect::<Vec<_>>()
                            .join(", ");
                        format!("{decl} = {{{elems}}};\n")
                    }
                    Some(expr) => format!("{decl} = {};\n", self.transpile_expr(expr)),
                    None => format!("{decl};\n"),
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
            Stmt::For { var, iter, body } => self.transpile_for(var, iter, body),
            Stmt::Break => "break;\n".to_string(),
            Stmt::Continue => "continue;\n".to_string(),
        }
    }

    fn transpile_for(&self, var: &str, iter: &Expr, body: &Block) -> String {
        let is_carray = self
            .inferencer
            .store
            .resolve(Self::expr_type_id(iter))
            .is_ok_and(|t| matches!(t, Type::CArray(..)));
        if is_carray {
            let Type::CArray(elem_type, size) = self
                .inferencer
                .store
                .resolve(Self::expr_type_id(iter))
                .expect("checked above")
            else {
                unreachable!("is_carray guard ensures this");
            };
            let iter_str = self.transpile_expr(iter);
            let elem_c_name = self.type_to_c_name(&elem_type);
            let idx_var = format!("__kit_{var}_idx");
            let mut s = format!("for (int {idx_var} = 0; {idx_var} < {size}; ++{idx_var}) ");
            let mut body_code = String::from("{\n");
            body_code.push_str(&format!(
                "    {elem_c_name} {var} = {iter_str}[{idx_var}];\n"
            ));
            for stmt in &body.stmts {
                let stmt_code = self.transpile_stmt(stmt);
                for line in stmt_code.lines() {
                    body_code.push_str("    ");
                    body_code.push_str(line);
                    body_code.push('\n');
                }
            }
            body_code.push('}');
            s.push_str(&body_code);
            s
        } else if let Expr::RangeLiteral { start, end } = iter {
            let start_str = self.transpile_expr(start);
            let end_str = self.transpile_expr(end);
            let mut s = format!("for (int {var} = {start_str}; {var} < {end_str}; ++{var}) ");
            s.push_str(&self.transpile_block(body));
            s
        } else {
            let iter_str = self.transpile_expr(iter);
            let mut s = format!("for (int {var} = 0; {var} < {iter_str}; ++{var}) ");
            s.push_str(&self.transpile_block(body));
            s
        }
    }

    fn transpile_block(&self, block: &Block) -> String {
        let mut code = String::from("{\n");
        for stmt in &block.stmts {
            let stmt_code = self.transpile_stmt(stmt);
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
    fn resolve_function_name(&self, name: &str) -> Option<(ModulePath, String)> {
        self.registry
            .resolve_qualified_name(name, &self.current_module)
    }

    // XXX: searches ALL modules, ignores import visibility.
    // Works for flat codegen; per-module mode relies on C linker to catch mismatches.
    fn find_global_module(&self, name: &str) -> Option<ModulePath> {
        self.registry
            .all_modules()
            .iter()
            .find(|m| m.program.globals.iter().any(|g| g.name == name))
            .map(|m| m.path.clone())
    }

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

    fn format_function_param_type_with_module(&self, p: &Param, module: &ModulePath) -> String {
        self.inferencer
            .store
            .resolve(p.ty)
            .map(|t| self.type_to_c_name_with_module(&t, module))
            .or_else(|_| p.annotation.as_ref().map(|t| t.to_c_repr().name).ok_or(()))
            .unwrap_or_else(|()| "void*".to_string())
    }

    fn format_function_params(&self, params: &[Param]) -> String {
        self.format_function_params_with_module(params, &self.current_module)
    }

    /// Format a variable declaration with proper C syntax.
    ///
    /// For CArray types (e.g., `CArray(Int, 3)`), this produces `int name[3]` instead of the
    /// default `int[3] name` which is invalid C.
    fn format_var_decl(&self, type_id: TypeId, name: &str) -> String {
        let resolved = self.inferencer.store.resolve(type_id);
        match resolved {
            Ok(Type::CArray(elem_type, size)) => {
                let elem_c_name = self.type_to_c_name(&elem_type);
                format!("{elem_c_name} {name}[{size}]")
            }
            _ => {
                let ty_str = self.resolve_type_to_c_name(type_id, "int");
                format!("{ty_str} {name}")
            }
        }
    }

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

    fn mangled_enum_variant(&self, enum_name: &str, variant_name: &str) -> String {
        let is_simple = self
            .inferencer
            .symbols()
            .lookup_enum(enum_name)
            .is_some_and(|e| e.variants.iter().all(|v| v.args.is_empty()));
        if is_simple {
            mangle_enum_variant(&self.current_module, enum_name, variant_name)
        } else {
            // HACK: {0} zero-initializes the entire union - valid C99 for any type.
            format!(
                "{{.{} = {}, ._variant = {{0}}}}",
                "_discriminant",
                mangle_enum_variant(&self.current_module, enum_name, variant_name)
            )
        }
    }

    fn transpile_expr(&self, expr: &Expr) -> String {
        match expr {
            Expr::Identifier { name, .. } => {
                if let Some(mod_path) = self.find_global_module(name) {
                    if has_no_mangle_in_module!(self.registry, &mod_path, name.as_str(), globals) {
                        name.clone()
                    } else {
                        mangle_name(&mod_path, name)
                    }
                } else {
                    name.clone()
                }
            }
            Expr::Literal { value: lit, ty, .. } => {
                let is_c_float = self.inferencer.store.resolve(*ty).is_ok_and(|t| {
                    matches!(t, Type::Float) // only C float gets the suffix, double does not
                });
                lit.to_c_with_float(is_c_float)
            }
            Expr::Call { callee, args, .. } => {
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
                    // XXX: name resolution cascade - qualified name -> module-scoped -> bare (C interop)
                    let (mod_path, base_name) =
                        if let Some((mp, bn)) = self.resolve_function_name(callee) {
                            (Some(mp), bn)
                        } else {
                            let last = callee.rsplit('.').next().unwrap_or(callee);
                            (None, last.to_string())
                        };

                    // XXX: 5-condition mangling ladder:
                    // 1. main is never mangled
                    // 2. extern/expose items skip mangling
                    // 3. known functions in non-empty module get module prefix
                    // 4. everything else passes through as-is (C interop)
                    let mangled = if callee == "main" {
                        callee.clone()
                    } else if let Some(mp) = mod_path {
                        if has_no_mangle_in_module!(
                            self.registry,
                            &mp,
                            base_name.as_str(),
                            functions
                        ) {
                            base_name.clone()
                        } else {
                            mangle_name(&mp, &base_name)
                        }
                    } else if self.inferencer.symbols().lookup_function(callee).is_some()
                        && !self.current_module.is_empty()
                    {
                        mangle_name(&self.current_module, callee)
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
            Expr::UnaryOp { op, expr, .. } => {
                format!("{}({})", op.to_c_str(), self.transpile_expr(expr))
            }
            Expr::BinaryOp {
                op, left, right, ..
            } => {
                let l = self.transpile_expr(left);
                let r = self.transpile_expr(right);
                format!("({l} {} {r})", op.to_c_str())
            }
            Expr::Assign {
                op, left, right, ..
            } => {
                let l = self.transpile_expr(left);
                let r = self.transpile_expr(right);
                format!("{l} {} {r}", op.to_c_str())
            }
            Expr::If {
                cond,
                then_branch,
                else_branch,
                ..
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
                    Ok(Type::Struct { name, .. } | Type::Named(name)) => name,
                    Ok(_) => "UNKNOWN_STRUCT".to_string(),
                    Err(e) => {
                        eprintln!("Warning: Failed to resolve struct type: {}", e);
                        "UNKNOWN_STRUCT".to_string()
                    }
                };
                let mangled = mangle_name(&self.current_module, &name);
                let inits = fields
                    .iter()
                    .map(|f| format!(".{} = {}", f.name, self.transpile_expr(&f.value)))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("(struct {}){{{}}}", mangled, inits)
            }
            Expr::FieldAccess {
                expr, field_name, ..
            } => {
                let container = self.transpile_expr(expr);
                let container_ty = Self::expr_type_id(expr);

                // Try to resolve the inferred type of the container expression
                if let Ok(Type::Named(type_name)) = self.inferencer.store.resolve(container_ty)
                // We only care about named types (structs/enums), not primitives or generics
                && let Some(enum_def) = self.inferencer.symbols().lookup_enum(&type_name)
                // Ensure the named type is actually an enum in our symbol table
                // and retrieve its definition
                && let Some(variant) = enum_def.variants.iter().find(|v| {
                    // Look for a variant that has at least one field/argument
                    // and where any of those fields match the requested field name
                    !v.args.is_empty() && v.args.iter().any(|a| a.name == *field_name)
                }) {
                    // If we found a matching enum variant + field, build a fully qualified access path:
                    // container -> variant (lowercased) -> field
                    return format!(
                        "{}._variant.{}.{}",
                        container,
                        variant.name.to_lowercase(),
                        field_name
                    );
                }
                format!("{}.{}", container, field_name)
            }
            Expr::Index { expr, index, .. } => {
                let container = self.transpile_expr(expr);
                let idx = self.transpile_expr(index);
                format!("({container})[{idx}]")
            }
            Expr::EnumInit {
                enum_name,
                variant_name,
                args,
                ..
            } if args.is_empty() => self.mangled_enum_variant(enum_name, variant_name),
            Expr::EnumVariant {
                enum_name,
                variant_name,
                ..
            } => self.mangled_enum_variant(enum_name, variant_name),
            Expr::ArrayLiteral { elements, ty } => {
                // Resolve the array type to get the element type name for the compound literal
                let array_c_name = self
                    .inferencer
                    .store
                    .resolve(*ty)
                    .ok()
                    .map(|t| self.type_to_c_name(&t))
                    .unwrap_or_else(|| "int[]".to_string());

                // Construct the compound literal
                let elems = elements
                    .iter()
                    .map(|e| self.transpile_expr(e))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("({array_c_name}){{{elems}}}")
            }
            Expr::EnumInit {
                enum_name,
                variant_name,
                args,
                ..
            } => {
                let a = self.transpile_enum_args_with_defaults(enum_name, variant_name, args);
                let ctor = mangle_enum_variant(&self.current_module, enum_name, variant_name);
                format!("{}_new({})", ctor, a)
            }
        }
    }
}

/// Remove intermediate `.c` and `.h` files from the build directory.
pub(crate) fn cleanup_intermediate_files(module_c_files: &[PathBuf], build_dir: &PathBuf) {
    if env::var("KEEP_C").is_ok() {
        return;
    }
    for c_file in module_c_files {
        let _ = fs::remove_file(c_file);
    }
    if env::var("KEEP_H").is_err() {
        cleanup_build_dir(build_dir);
    }
}

fn cleanup_build_dir(build_dir: &PathBuf) {
    let Ok(entries) = fs::read_dir(build_dir) else {
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
    let _ = fs::remove_dir(build_dir);
}
