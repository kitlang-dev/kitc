mod enum_gen;
mod header;
mod match_pattern;

use std::collections::HashSet;
use std::env;
use std::fmt::Write;
use std::fs;
use std::path::PathBuf;

use crate::codegen::ast::{
    Attributed, Block, Expr, ExprKind, Function, GlobalDecl, Literal, Program, Stmt, StmtKind,
};
use crate::codegen::hash;
use crate::codegen::module::{ModulePath, ModuleRegistry};
use crate::codegen::name_mangling::{mangle_enum_variant, mangle_name};
use crate::codegen::parser::expr_pratt::callee_name;
use crate::codegen::type_ast::FieldInit;
use crate::codegen::types::{ToCRepr, Type, TypeId, UnaryOperator};

use super::ast::Param;
use super::inference::TypeInferencer;

/// Context for C code generation, borrowing inference results and module registry.
///
/// Constructed after type inference completes - all methods are read-only on analysis data.
pub(crate) struct CodegenCtx<'a> {
    pub(crate) inferencer: &'a TypeInferencer,
    pub(crate) registry: &'a ModuleRegistry,
    pub(crate) current_module: ModulePath,
    pub(crate) build_dir: &'a PathBuf,
}

/// Check if a declaration in the given module field is marked #[extern] or #[expose].
macro_rules! is_unmangled_in_module {
    ($registry:expr, $mod_path:expr, $name:expr, $field:ident) => {
        $registry
            .get($mod_path)
            .and_then(|m| m.program.$field.iter().find(|item| item.name == $name))
            .is_some_and(|item| item.is_unmangled())
    };
}

/// Returns `true` if `expr` is a compile-time constant expression that is a valid file-scope initializer
/// in standard C.
///
/// Expressions that reference other globals or call functions are *not* constant expressions in C:
/// ```c
/// const int a = 42;
/// const int b = a + 1;
/// ```
/// GCC/Clang accept this as an extension but MSVC rejects it.
///
/// Such const globals must instead be emitted as `#define` macros (see [`CodegenCtx::transpile_global`]).
fn is_constant_initializer(expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::Literal { .. } => true,
        ExprKind::UnaryOp { expr: inner, .. } => is_constant_initializer(inner),
        ExprKind::BinaryOp { left, right, .. } => {
            is_constant_initializer(left) && is_constant_initializer(right)
        }
        ExprKind::EnumVariant { .. } => true,
        ExprKind::EnumInit { args, .. } => args.is_empty(),
        ExprKind::ArrayLiteral { elements } => elements.iter().all(is_constant_initializer),
        ExprKind::StructInit { fields, .. } => {
            fields.iter().all(|f| is_constant_initializer(&f.value))
        }
        _ => false,
    }
}

/// Walk all types referenced in a program and invoke `visitor` for each one.
fn visit_program_types<TypeVisitor: FnMut(&Type)>(
    inferencer: &TypeInferencer,
    prog: &Program,
    mut visitor: TypeVisitor,
) {
    for s in &prog.structs {
        for field in &s.fields {
            if let Ok(ty) = inferencer.store.resolve(field.ty) {
                visitor(&ty);
            } else if let Some(ref ann) = field.annotation {
                visitor(ann);
            }
        }
    }
    for e in &prog.enums {
        for v in &e.variants {
            for a in &v.args {
                if let Ok(ty) = inferencer.store.resolve(a.ty) {
                    visitor(&ty);
                } else if let Some(ref ann) = a.annotation {
                    visitor(ann);
                }
            }
        }
    }
    for g in &prog.globals {
        if let Ok(ty) = inferencer.store.resolve(g.inferred) {
            visitor(&ty);
        }
    }
    for func in &prog.functions {
        if let Some(id) = func.inferred_return {
            if let Ok(ty) = inferencer.store.resolve(id) {
                visitor(&ty);
            }
        } else if let Some(ref r) = func.return_type {
            visitor(r);
        }

        for p in &func.params {
            if let Ok(ty) = inferencer.store.resolve(p.ty) {
                visitor(&ty);
            } else if let Some(ref ann) = p.annotation {
                visitor(ann);
            }
        }

        for stmt in &func.body.stmts {
            if let Stmt {
                kind: StmtKind::VarDecl { inferred, .. },
                ..
            } = stmt
                && let Ok(ty) = inferencer.store.resolve(*inferred)
            {
                visitor(&ty);
            }
        }
    }

    for tdef in &prog.typedefs {
        visitor(&tdef.type_def);
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
        expr.ty
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
        let resolved = self.preferred_c_type(t);
        self.type_to_c_name_with_module(&resolved, &self.current_module)
    }

    /// Resolve `t` to the type whose name should be emitted in generated C.
    ///
    /// Typedefs are followed, but when one aliases another `Named` type - the private struct tag
    /// behind a public alias (`typedef struct _div_t { ... } div_t;`) - the public alias is kept
    /// instead of descending to the internal tag, so generated code refers to `div_t`, not the
    /// internal `_div_t` kitc never declares.
    ///
    /// Headers using an anonymous struct tag (`typedef struct { ... } T;`) resolve to a struct,
    /// not a `Named`, so they are unaffected on any target.
    fn preferred_c_type(&self, t: &Type) -> Type {
        let resolved = self
            .inferencer
            .store
            .resolve_typedef_type(t)
            .unwrap_or_else(|| t.clone());

        // If the typedef aliases another `Named` type (a struct tag or further typedef),
        // keep the public alias name rather than descending to the internal tag.
        let keep_alias = matches!(
            (t, &resolved),
            (Type::Named(name), Type::Named(underlying))
                if underlying.as_str() != name.as_str()
        );
        if keep_alias {
            return t.clone();
        }
        resolved
    }

    fn type_to_c_name_with_module(&self, t: &Type, module: &ModulePath) -> String {
        if let Type::Named(name) = t {
            if self.inferencer.is_imported_struct(name) {
                name.clone()
            } else if self.inferencer.is_struct_type(name) {
                format!("struct {}", mangle_name(module, name))
            } else {
                // A `Named` type that is a typedef alias for an imported C struct
                // (e.g. `div_t` -> `_div_t`) must be emitted under its public alias,
                // unmangled: the alias is the name actually declared by the header,
                // while the internal tag is an implementation detail kitc never
                // declares.
                let is_alias_of_imported = match self.inferencer.store.resolve_typedef_type(t) {
                    Some(Type::Named(target)) => {
                        target.as_str() != name.as_str()
                            && self.inferencer.is_imported_struct(target.as_str())
                    }
                    _ => false,
                };
                if is_alias_of_imported {
                    return name.clone();
                }
                mangle_name(module, name)
            }
        } else if let Type::Tuple(elems) = t {
            self.tuple_struct_name(elems)
        } else if let Type::Struct { name, .. } = t {
            // Struct value type: emit the mangled struct tag so it matches both
            // the struct definition and the `Type::Named` representation, which
            // otherwise produce two different C names for the same type.
            format!("struct {}", mangle_name(module, name))
        } else {
            t.to_c_repr().name
        }
    }

    /// Deterministic C struct name for a tuple shape.
    ///
    /// Built from the canonical C type names of the element types (so the name
    /// matches wherever the tuple is referenced), hashed with the same DJB2
    /// routine used for other generated identifiers.
    fn tuple_struct_name(&self, elems: &[Type]) -> String {
        let elem_names: Vec<String> = elems.iter().map(|e| self.type_to_c_name(e)).collect();
        let key = format!("{}|{}", elems.len(), elem_names.join("|"));
        format!("struct kit_tuple_{}", hash::djb2_str(&key))
    }

    /// Resolve a function's return type to its C name, defaulting to "int" for main and "void" otherwise.
    fn resolve_return_type_c_name(&self, func: &Function) -> String {
        if func.name == "main" {
            return "int".to_string();
        }
        func.inferred_return
            .and_then(|id| self.inferencer.store.resolve(id).ok())
            .map(|t| self.type_to_c_name(&t))
            .or_else(|| func.return_type.as_ref().map(|t| self.type_to_c_name(t)))
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
            Some(Expr {
                kind: ExprKind::ArrayLiteral { elements, .. },
                ..
            }) => {
                let elems = elements
                    .iter()
                    .map(|e| self.transpile_expr(e))
                    .collect::<Vec<_>>()
                    .join(", ");

                format!("{extern_prefix}{const_prefix}{decl} = {{{elems}}};")
            }
            // A const global whose initializer is not a valid C const expr (e.g. it references another
            // const global) cannot be declared as `const T x = <expr>;` in portable C.
            //
            // Emit it as a #define instead, matching the semantics of a compile-time constant to work
            // around this limitation.
            Some(expr) if global.is_const && !is_constant_initializer(expr) => {
                let init_str = self.transpile_expr(expr);
                format!("#define {global_name} ({init_str})")
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
            let has_return = func.body.stmts.iter().any(|s| {
                matches!(
                    s,
                    Stmt {
                        kind: StmtKind::Return(_),
                        ..
                    }
                )
            });
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
        match &stmt.kind {
            StmtKind::VarDecl {
                name,
                annotation: _,
                inferred,
                init,
            } => {
                let decl = self.format_var_decl(*inferred, name);
                match init {
                    Some(Expr {
                        kind: ExprKind::ArrayLiteral { elements, .. },
                        ..
                    }) => {
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
            StmtKind::Expr(expr) => format!("{};\n", self.transpile_expr(expr)),
            StmtKind::Return(expr) => match expr {
                Some(e) => format!("return {};\n", self.transpile_expr(e)),
                None => "return;\n".to_string(),
            },
            StmtKind::If {
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
            StmtKind::While { cond, body } => {
                let mut s = format!("while ({}) ", self.transpile_expr(cond));
                s.push_str(&self.transpile_block(body));
                s.push('\n');
                s
            }
            StmtKind::For { var, iter, body } => self.transpile_for(var, iter, body),
            StmtKind::Match(m) => self.transpile_match_stmt(m),
            StmtKind::Break => "break;\n".to_string(),
            StmtKind::Continue => "continue;\n".to_string(),
            StmtKind::Defer { .. } => {
                // Defer statements should have been expanded by the defer_expand pass
                // before reaching codegen. This is a compiler bug.
                panic!("encountered unexpanded Defer statement in codegen");
            }
            StmtKind::Block(block) => self.transpile_block(block),
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
            let _ = writeln!(
                body_code,
                "    {elem_c_name} {var} = {iter_str}[{idx_var}];"
            );
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
        } else if let Expr {
            kind: ExprKind::RangeLiteral { start, end, .. },
            ..
        } = iter
        {
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

    /// Check if a function name is declared in the current module's program.
    ///
    /// Returns `false` for C interop functions (registered from headers but not defined in any
    /// module's Kit source code).
    fn is_function_in_current_module(&self, name: &str) -> bool {
        self.registry
            .get(&self.current_module)
            .is_some_and(|m| m.program.functions.iter().any(|f| f.name == name))
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
            .or_else(|_| {
                p.annotation
                    .as_ref()
                    .map(|t| self.type_to_c_name(t))
                    .ok_or(())
            })
            .unwrap_or_else(|()| "void*".to_string())
    }

    fn format_function_params(&self, params: &[Param]) -> String {
        self.format_function_params_with_module(params, &self.current_module)
    }

    /// Format a type with a variable name, handling function-pointer declarator syntax.
    /// Function/ptr-to-function types need `ret (*name)(params)` instead of `ret(*)(params) name`.
    fn format_type_with_name(&self, ty: &Type, name: &str, module: &ModulePath) -> String {
        if let Some(fp) = self.format_fn_ptr_param(ty, name, module) {
            fp
        } else {
            // Resolve typedef aliases so the variable uses the underlying C type name.
            let resolved = self.preferred_c_type(ty);
            format!(
                "{} {name}",
                self.type_to_c_name_with_module(&resolved, module)
            )
        }
    }

    /// Format a variable declaration with proper C syntax.
    ///
    /// For `CArray` types (e.g., `CArray(Int, 3)`), this produces `int name[3]` instead of the
    /// default `int[3] name` which is invalid C.
    fn format_var_decl(&self, type_id: TypeId, name: &str) -> String {
        let resolved = self.inferencer.store.resolve(type_id);
        match resolved {
            Ok(Type::CArray(elem_type, size)) => {
                let elem_c_name = self.type_to_c_name(&elem_type);
                format!("{elem_c_name} {name}[{size}]")
            }
            Ok(ref ty) => self.format_type_with_name(ty, name, &self.current_module),
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
                // C requires function pointer parameters to embed the name in
                // the declarator: `ret (*name)(params)` instead of `ret(*)(params) name`.
                if let Ok(ty) = self.inferencer.store.resolve(p.ty)
                    && let Some(fp) = self.format_fn_ptr_param(&ty, &p.name, module)
                {
                    return fp;
                }
                format!(
                    "{} {}",
                    self.format_function_param_type_with_module(p, module),
                    p.name
                )
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// If `ty` is a function type or pointer-to-function type, format it as a C
    /// function pointer parameter declaration (`ret (*name)(params)`). Returns
    /// `None` for non-function types.
    fn format_fn_ptr_param(&self, ty: &Type, name: &str, module: &ModulePath) -> Option<String> {
        let (ret_ty, param_tys) = match ty {
            Type::Function { param_tys, ret_ty } => (ret_ty.as_ref(), param_tys.as_slice()),
            Type::Ptr(inner) if matches!(inner.as_ref(), Type::Function { .. }) => {
                if let Type::Function { param_tys, ret_ty } = inner.as_ref() {
                    (ret_ty.as_ref(), param_tys.as_slice())
                } else {
                    return None;
                }
            }
            _ => return None,
        };
        let ret_c = self.type_to_c_name_with_module(ret_ty, module);
        let params_c = param_tys
            .iter()
            .map(|t| self.type_to_c_name_with_module(t, module))
            .collect::<Vec<_>>()
            .join(", ");
        Some(format!("{ret_c} (*{name})({params_c})"))
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

    /// `mangled_enum_variant` that resolves a generic (template) enum through the
    /// expression's type to the concrete monomorph.
    fn mangled_enum_variant_for(
        &self,
        expr_ty: TypeId,
        enum_name: &str,
        variant_name: &str,
    ) -> String {
        let resolved = if self.inferencer.is_template_enum(enum_name) {
            self.resolved_enum_name(expr_ty, enum_name)
        } else {
            enum_name.to_string()
        };
        self.mangled_enum_variant(&resolved, variant_name)
    }

    /// Transpile a call, passing `call_ty` so generic-function and generic-enum
    /// call sites are mangled to their realized monomorph.
    fn transpile_call(&self, callee: &Expr, args: &[Expr], call_ty: TypeId) -> String {
        if let Some(name) = callee_name(callee) {
            self.transpile_named_call(&name, args, call_ty)
        } else {
            let callee_c = self.transpile_expr(callee);
            let a = args
                .iter()
                .map(|a| self.transpile_expr(a))
                .collect::<Vec<_>>()
                .join(", ");
            format!("({})({})", callee_c, a)
        }
    }

    /// Resolve the C enum name for a variant reference.
    ///
    /// Generic (template) enums are never emitted, so a reference is resolved through the monomorph
    /// selected by the expression's type; non-generic enums return `declared` unchanged.
    fn resolved_enum_name(&self, expr_ty: TypeId, declared: &str) -> String {
        if self.inferencer.is_template_enum(declared) {
            self.inferencer
                .store
                .resolve(expr_ty)
                .ok()
                .and_then(|t| match t {
                    Type::Named(name) if self.inferencer.is_monomorph_name(&name) => Some(name),
                    _ => None,
                })
                .unwrap_or_else(|| declared.to_string())
        } else {
            declared.to_string()
        }
    }

    /// Transpile a call whose callee resolves to a known name.
    ///
    /// Enum constructor calls are mangled to the monomorph when the variant belongs to a generic
    /// enum (its declared name would reference the unemitted template). The call's own type selects
    /// the monomorph, since template and monomorph variant-infos share the simple variant name and
    /// symbol-table lookup order is nondeterministic.
    fn transpile_named_call(&self, name: &str, args: &[Expr], call_ty: TypeId) -> String {
        if let Some(info) = self
            .inferencer
            .symbols()
            .lookup_enum_variant_by_simple_name(name)
        {
            let enum_name = match self.inferencer.store.resolve(call_ty) {
                Ok(Type::Named(n)) if self.inferencer.is_monomorph_name(&n) => n,
                _ if self.inferencer.is_template_enum(&info.enum_name) => {
                    self.resolved_enum_name(call_ty, &info.enum_name)
                }
                _ => info.enum_name.clone(),
            };
            let a = args
                .iter()
                .map(|a| self.transpile_expr(a))
                .collect::<Vec<_>>()
                .join(", ");
            let ctor = mangle_enum_variant(&self.current_module, &enum_name, &info.variant_name);
            return format!("{}_new({})", ctor, a);
        }
        let (mod_path, base_name) = if let Some((mp, bn)) = self.resolve_function_name(name) {
            (Some(mp), bn)
        } else {
            let last = name.rsplit('.').next().unwrap_or(name);
            (None, last.to_string())
        };
        let mangled = if name == "main" {
            name.to_string()
        } else if let Some(mp) = &mod_path {
            if is_unmangled_in_module!(self.registry, mp, base_name.as_str(), functions) {
                base_name
            } else {
                mangle_name(mp, &base_name)
            }
        } else if self.inferencer.symbols().lookup_function(name).is_some()
            && !self.current_module.is_empty()
            && self.is_function_in_current_module(name)
        {
            mangle_name(&self.current_module, name)
        } else if let Some(mp) = self.inferencer.monomorph_module(name).cloned() {
            // Monomorphized generic function: mangle with the module that
            // defines its template so callers match the emitted definition.
            mangle_name(&mp, name)
        } else {
            name.to_string()
        };
        let a = args
            .iter()
            .map(|a| self.transpile_expr(a))
            .collect::<Vec<_>>()
            .join(", ");
        format!("{mangled}({a})")
    }

    fn transpile_field_access(&self, expr: &Expr, field_name: &str) -> String {
        let container = self.transpile_expr(expr);
        let container_ty = Self::expr_type_id(expr);

        if let Ok(Type::Named(type_name)) = self.inferencer.store.resolve(container_ty)
            && let Some(enum_def) = self.inferencer.symbols().lookup_enum(&type_name)
            && let Some(variant) = enum_def
                .variants
                .iter()
                .find(|v| !v.args.is_empty() && v.args.iter().any(|a| a.name == *field_name))
        {
            return format!(
                "{}._variant.{}.{}",
                container,
                variant.name.to_lowercase(),
                field_name
            );
        }
        format!("{}.{}", container, field_name)
    }

    fn transpile_array_literal(&self, ty: TypeId, elements: &[Expr]) -> String {
        let array_c_name = self
            .inferencer
            .store
            .resolve(ty)
            .ok()
            .map_or_else(|| "int[]".to_string(), |t| self.type_to_c_name(&t));
        let elems = elements
            .iter()
            .map(|e| self.transpile_expr(e))
            .collect::<Vec<_>>()
            .join(", ");
        format!("({array_c_name}){{{elems}}}")
    }

    fn transpile_struct_init(&self, ty: TypeId, fields: &[FieldInit]) -> String {
        let name = match self.inferencer.store.resolve(ty) {
            Ok(Type::Struct { name, .. } | Type::Named(name)) => name,
            Ok(_) => "UNKNOWN_STRUCT".to_string(),
            Err(e) => {
                eprintln!("Warning: Failed to resolve struct type: {e}");
                "UNKNOWN_STRUCT".to_string()
            }
        };
        let mangled = mangle_name(&self.current_module, &name);
        let inits = fields
            .iter()
            .map(|f| format!(".{} = {}", f.name, self.transpile_expr(&f.value)))
            .collect::<Vec<_>>()
            .join(", ");
        format!("(struct {mangled}){{{inits}}}")
    }

    /// Transpile a tuple literal to a C compound literal.
    ///
    /// Each distinct tuple shape is backed by a generated `struct kit_tuple_*`
    /// (see `collect_tuple_shapes`); the literal becomes
    /// `(struct kit_tuple_xxx){ .__slot0 = e0, .__slot1 = e1, ... }`.
    fn transpile_tuple_lit(&self, expr: &Expr) -> String {
        let Some(Type::Tuple(elems)) = self.inferencer.store.resolve(expr.ty).ok().and_then(|t| {
            if let Type::Tuple(_) = &t {
                Some(t)
            } else {
                None
            }
        }) else {
            unreachable!("tuple literal expr.ty did not resolve to a Tuple type");
        };

        let name = self.tuple_struct_name(&elems);
        let inits = Self::elements_init(expr, self);
        format!("({name}){{ {inits} }}", inits = inits.join(", "))
    }

    /// Gather every distinct tuple shape used by the program and emit the C struct
    /// definitions into a single shared header (`kit_tuples.h`).
    ///
    /// Because the Rust backend emits per-module C files, defining each tuple
    /// struct once (here) and including it everywhere avoids C redefinition
    /// errors. Shapes come from both inference (recorded `tuple_shapes`) and a
    /// fallback scan of annotations/expression types, so annotation-only tuple
    /// types are still emitted.
    pub(crate) fn generate_tuple_header(
        &self,
        inferencer: &crate::codegen::inference::TypeInferencer,
        prog: &Program,
    ) -> String {
        let mut seen: HashSet<String> = HashSet::new();
        let mut shapes: Vec<(String, Vec<Type>)> = Vec::new();

        // Shapes recorded during inference.
        for (_, elems) in inferencer.tuple_shapes() {
            Self::record_tuple_shape(self, elems, &mut seen, &mut shapes);
        }
        // Fallback: annotations and expression types across the program.
        Self::scan_program_tuples(prog, inferencer, &mut |ty| {
            if let Type::Tuple(elems) = ty {
                Self::record_tuple_shape(self, elems, &mut seen, &mut shapes);
            }
        });

        let mut out = String::new();
        let _ = writeln!(out, "#ifndef KIT_TUPLES_H");
        let _ = writeln!(out, "#define KIT_TUPLES_H");
        out.push('\n');

        // Collect required system headers from element C representations.
        let mut headers: HashSet<String> = HashSet::new();
        for (_, elems) in &shapes {
            for e in elems {
                for h in &e.to_c_repr().headers {
                    headers.insert(h.clone());
                }
            }
        }
        let has_headers = !headers.is_empty();
        for h in headers {
            let _ = writeln!(out, "#include {h}");
        }
        if has_headers {
            out.push('\n');
        }

        for (name, elems) in &shapes {
            let _ = writeln!(out, "struct {} {{", name.trim_start_matches("struct "));
            for (i, e) in elems.iter().enumerate() {
                let _ = writeln!(out, "    {} __slot{i};", self.type_to_c_name(e));
            }
            let _ = writeln!(out, "}};\n");
        }

        let _ = writeln!(out, "#endif /* KIT_TUPLES_H */");
        out
    }

    /// Build the `.__slotN = expr` initializer list for a tuple literal's elements.
    fn elements_init(expr: &Expr, ctx: &CodegenCtx<'_>) -> Vec<String> {
        let mut out = Vec::new();
        if let ExprKind::TupleLit { elements } = &expr.kind {
            for (i, e) in elements.iter().enumerate() {
                out.push(format!(".__slot{i} = {}", ctx.transpile_expr(e)));
            }
        }
        out
    }

    /// Record a tuple shape (deduplicated by generated struct name), recursing into
    /// nested tuple element types so nested shapes are emitted too.
    fn record_tuple_shape(
        ctx: &CodegenCtx<'_>,
        elems: &[Type],
        seen: &mut HashSet<String>,
        shapes: &mut Vec<(String, Vec<Type>)>,
    ) {
        let name = ctx.tuple_struct_name(elems);
        if seen.insert(name.clone()) {
            let mut flat = Vec::new();
            for e in elems {
                if let Type::Tuple(inner) = e {
                    Self::record_tuple_shape(ctx, inner, seen, shapes);
                }
                flat.push(e.clone());
            }
            shapes.push((name, flat));
        }
    }

    /// Recursively scan a program for tuple types, invoking `visit` for each found
    /// `Type::Tuple` (including those reachable from expression types and annotations).
    fn scan_program_tuples(
        prog: &Program,
        inferencer: &crate::codegen::inference::TypeInferencer,
        visit: &mut dyn FnMut(&Type),
    ) {
        Self::scan_program_tuples_stmts(prog, inferencer, visit);
    }

    /// Walk all statements/expressions in the program to find tuple types.
    fn scan_program_tuples_stmts(
        prog: &Program,
        inferencer: &crate::codegen::inference::TypeInferencer,
        visit: &mut dyn FnMut(&Type),
    ) {
        fn walk_stmt(
            stmt: &Stmt,
            inferencer: &crate::codegen::inference::TypeInferencer,
            visit: &mut dyn FnMut(&Type),
        ) {
            match &stmt.kind {
                StmtKind::VarDecl {
                    init, annotation, ..
                } => {
                    if let Some(Type::Tuple(e)) = annotation {
                        visit(&Type::Tuple(e.clone()));
                    }
                    if let Some(init) = init {
                        walk_expr(init, inferencer, visit);
                    }
                }
                StmtKind::Expr(e) => walk_expr(e, inferencer, visit),
                StmtKind::Return(Some(e)) => walk_expr(e, inferencer, visit),
                StmtKind::If {
                    cond,
                    then_branch,
                    else_branch,
                } => {
                    walk_expr(cond, inferencer, visit);
                    walk_block(then_branch, inferencer, visit);
                    if let Some(b) = else_branch {
                        walk_block(b, inferencer, visit);
                    }
                }
                StmtKind::While { cond, body } => {
                    walk_expr(cond, inferencer, visit);
                    walk_block(body, inferencer, visit);
                }
                StmtKind::For { iter, body, .. } => {
                    walk_expr(iter, inferencer, visit);
                    walk_block(body, inferencer, visit);
                }
                StmtKind::Match(m) => {
                    walk_expr(&m.expr, inferencer, visit);
                    for arm in &m.arms {
                        walk_block(&arm.body, inferencer, visit);
                    }
                }
                StmtKind::Block(b) => walk_block(b, inferencer, visit),
                _ => {}
            }
        }

        fn walk_block(
            block: &Block,
            inferencer: &crate::codegen::inference::TypeInferencer,
            visit: &mut dyn FnMut(&Type),
        ) {
            for s in &block.stmts {
                walk_stmt(s, inferencer, visit);
            }
        }

        fn walk_expr(
            expr: &Expr,
            inferencer: &crate::codegen::inference::TypeInferencer,
            visit: &mut dyn FnMut(&Type),
        ) {
            if let Ok(t) = inferencer.store.resolve(expr.ty) {
                visit(&t);
            }
            match &expr.kind {
                ExprKind::Literal { .. }
                | ExprKind::Identifier { .. }
                | ExprKind::EnumVariant { .. } => {}
                ExprKind::Call { callee, args } => {
                    walk_expr(callee, inferencer, visit);
                    for a in args {
                        walk_expr(a, inferencer, visit);
                    }
                }
                ExprKind::UnaryOp { expr, .. } => walk_expr(expr, inferencer, visit),
                ExprKind::BinaryOp { left, right, .. } => {
                    walk_expr(left, inferencer, visit);
                    walk_expr(right, inferencer, visit);
                }
                ExprKind::Assign { left, right, .. } => {
                    walk_expr(left, inferencer, visit);
                    walk_expr(right, inferencer, visit);
                }
                ExprKind::If {
                    cond,
                    then_branch,
                    else_branch,
                } => {
                    walk_expr(cond, inferencer, visit);
                    walk_expr(then_branch, inferencer, visit);
                    walk_expr(else_branch, inferencer, visit);
                }
                ExprKind::RangeLiteral { start, end } => {
                    walk_expr(start, inferencer, visit);
                    walk_expr(end, inferencer, visit);
                }
                ExprKind::StructInit { fields, .. } => {
                    for f in fields {
                        walk_expr(&f.value, inferencer, visit);
                    }
                }
                ExprKind::FieldAccess { expr, .. } => walk_expr(expr, inferencer, visit),
                ExprKind::Index { expr, index } => {
                    walk_expr(expr, inferencer, visit);
                    walk_expr(index, inferencer, visit);
                }
                ExprKind::EnumInit { args, .. } => {
                    for a in args {
                        walk_expr(a, inferencer, visit);
                    }
                }
                ExprKind::ArrayLiteral { elements, .. } => {
                    for e in elements {
                        walk_expr(e, inferencer, visit);
                    }
                }
                ExprKind::TupleLit { elements } => {
                    for e in elements {
                        walk_expr(e, inferencer, visit);
                    }
                }
            }
        }

        for f in &prog.functions {
            walk_block(&f.body, inferencer, visit);
            for p in &f.params {
                if let Some(Type::Tuple(e)) = &p.annotation {
                    visit(&Type::Tuple(e.clone()));
                }
            }
            if let Some(Type::Tuple(e)) = &f.return_type {
                visit(&Type::Tuple(e.clone()));
            }
        }
        for g in &prog.globals {
            if let Some(init) = &g.init {
                walk_expr(init, inferencer, visit);
            }
            if let Some(Type::Tuple(e)) = &g.annotation {
                visit(&Type::Tuple(e.clone()));
            }
        }
        for s in &prog.structs {
            for f in &s.fields {
                if let Some(Type::Tuple(e)) = &f.annotation {
                    visit(&Type::Tuple(e.clone()));
                }
            }
        }
        for e in &prog.enums {
            for variant in &e.variants {
                for f in &variant.args {
                    if let Some(Type::Tuple(tt)) = &f.annotation {
                        visit(&Type::Tuple(tt.clone()));
                    }
                }
            }
        }
        for tr in &prog.traits {
            for m in &tr.methods {
                walk_block(&m.body, inferencer, visit);
                if let Some(Type::Tuple(tt)) = &m.return_type {
                    visit(&Type::Tuple(tt.clone()));
                }
                for p in &m.params {
                    if let Some(Type::Tuple(tt)) = &p.annotation {
                        visit(&Type::Tuple(tt.clone()));
                    }
                }
            }
        }
        for im in &prog.impls {
            for m in &im.methods {
                walk_block(&m.body, inferencer, visit);
                if let Some(Type::Tuple(tt)) = &m.return_type {
                    visit(&Type::Tuple(tt.clone()));
                }
                for p in &m.params {
                    if let Some(Type::Tuple(tt)) = &p.annotation {
                        visit(&Type::Tuple(tt.clone()));
                    }
                }
            }
        }
    }

    fn transpile_expr(&self, expr: &Expr) -> String {
        match &expr.kind {
            ExprKind::Identifier { name } => {
                if let Some(mod_path) = self.find_global_module(name) {
                    // Global variable reference.
                    if is_unmangled_in_module!(self.registry, &mod_path, name.as_str(), globals) {
                        name.clone()
                    } else {
                        mangle_name(&mod_path, name)
                    }
                } else if let Some((mp, bn)) = self.resolve_function_name(name)
                    && self.inferencer.symbols().lookup_function(&bn).is_some()
                {
                    // Function reference used as a value (e.g. `g(f)`).
                    // Reuse the call-path mangling for cross-module correctness + no_mangle.
                    if is_unmangled_in_module!(self.registry, &mp, bn.as_str(), functions) {
                        bn
                    } else {
                        mangle_name(&mp, &bn)
                    }
                } else {
                    name.clone()
                }
            }
            ExprKind::Literal { value: lit } => {
                let is_c_float = self.inferencer.store.resolve(expr.ty).is_ok_and(|t| {
                    matches!(t, Type::Float) // only C float gets the suffix, double does not
                });
                lit.to_c_with_float(is_c_float)
            }
            ExprKind::Call { callee, args } => self.transpile_call(callee, args, expr.ty),
            ExprKind::UnaryOp { op, expr: inner } => {
                let inner = self.transpile_expr(inner);
                match op {
                    UnaryOperator::PreIncrement => format!("++{inner}"),
                    UnaryOperator::PostIncrement => format!("{inner}++"),
                    UnaryOperator::PreDecrement => format!("--{inner}"),
                    UnaryOperator::PostDecrement => format!("{inner}--"),
                    _ => format!("{}({inner})", op.to_c_str()),
                }
            }
            ExprKind::BinaryOp { op, left, right } => {
                let l = self.transpile_expr(left);
                let r = self.transpile_expr(right);
                format!("({l} {} {r})", op.to_c_str())
            }
            ExprKind::Assign { op, left, right } => {
                let l = self.transpile_expr(left);
                let r = self.transpile_expr(right);
                format!("{l} {} {r}", op.to_c_str())
            }
            ExprKind::If {
                cond,
                then_branch,
                else_branch,
            } => {
                let c = self.transpile_expr(cond);
                let t = self.transpile_expr(then_branch);
                let e = self.transpile_expr(else_branch);
                format!("({c} ? {t} : {e})")
            }
            ExprKind::RangeLiteral { .. } => "/* range literal */ 0".to_string(),
            ExprKind::StructInit { fields, .. } => self.transpile_struct_init(expr.ty, fields),
            ExprKind::TupleLit { .. } => self.transpile_tuple_lit(expr),
            ExprKind::FieldAccess {
                expr: inner,
                field_name,
            } => self.transpile_field_access(inner, field_name),
            ExprKind::Index { expr: inner, index } => {
                // Tuple slot access: `t[i]` on a tuple-valued container becomes a
                // struct member access `t.__slotN` (only valid for int-literal i,
                // which inference already enforces). Otherwise this is array/ptr indexing.
                if let Ok(Type::Tuple(_)) = self.inferencer.store.resolve(inner.ty)
                    && let ExprKind::Literal {
                        value: Literal::Int(i),
                    } = &index.kind
                {
                    let container = self.transpile_expr(inner);
                    return format!("({container}).__slot{i}");
                }
                let container = self.transpile_expr(inner);
                let idx = self.transpile_expr(index);
                format!("({container})[{idx}]")
            }
            ExprKind::EnumInit {
                enum_name,
                variant_name,
                args,
            } if args.is_empty() => self.mangled_enum_variant_for(expr.ty, enum_name, variant_name),
            ExprKind::EnumVariant {
                enum_name,
                variant_name,
            } => self.mangled_enum_variant_for(expr.ty, enum_name, variant_name),
            ExprKind::ArrayLiteral { elements, .. } => {
                self.transpile_array_literal(expr.ty, elements)
            }
            ExprKind::EnumInit {
                enum_name,
                variant_name,
                args,
            } => {
                // Generic enums resolve to the monomorph selected by the
                // expression's type; their defaults live in that monomorph.
                let enum_name = if self.inferencer.is_template_enum(enum_name) {
                    self.resolved_enum_name(expr.ty, enum_name)
                } else {
                    enum_name.clone()
                };
                let a = self.transpile_enum_args_with_defaults(&enum_name, variant_name, args);
                let ctor = mangle_enum_variant(&self.current_module, &enum_name, variant_name);
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
        let ext = path.extension().and_then(|e| e.to_str());
        if matches!(ext, Some("h" | "c" | "obj")) {
            let _ = fs::remove_file(&path);
        }
    }
    let _ = fs::remove_dir(build_dir);
}
