//! Hindley-Milner type inference for Kit, with the generic monomorphization logic.
//!
//! This module contains `TypeInferencer`, which walks the merged program (assembled by
//! `merge_modules_for_inference` in `frontend.rs`) and assigns a `TypeId` to every expression and
//! declaration, resolving type variables by unification. It also holds the per-inference-pass
//! monomorphization state (see `MonomorphState`).
//!
//! Generics & monomorphization
//! -----------------------------------
//! Kit supports generic structs, enums, and functions. Three specific words are used constantly
//! and are easy to confuse. Here's their definition:
//!
//! - *Template*: a generic *definition*: a struct/enum/function declared with `type_params` (like
//!   `struct Box[T]`). A template is never typed or emitted to C, it is only a "stencil" for
//!   monomorphs. Templates are stashed during `register_templates` and skipped by ordinary
//!   registration.
//! - *Application*: a *use* of a template with type arguments, represented in
//!   the type system as `Type::Instance { base, args }`.
//!   Examples: `Box[Int]`, a bare `Box` whose arguments are inferred, or a call to a generic
//!   function.
//!   An application is the thing monomorphization resolves, it is the site, not the definition and
//!   not the realization.
//! - *Monomorph*: a *concrete realization* of a template: the template cloned with its type
//!   parameters substituted by the application's arguments with its type parameters substituted by
//!   the application's arguments, registered under the deterministic name `<base>_<hash>`
//!   (see `monomorph_name` in `monomorph.rs`). Only monomorphs are typed and emitted.
//!
//! An application whose arguments are not yet all concrete gets a fresh *instance type variable*.
//! It is recorded in two places: `MonomorphState::instance_types` (so unification can later bind
//! it) and the *pending worklist*, the list of generic applications spotted during the current
//! pass whose arguments have not been resolved yet.
//!
//! At the end of the pass, `generate_monomorphs` walks the worklist. For each entry whose
//! arguments are now concrete it realizes the monomorph, binds the instance variables to
//! `Named(monomorph)`, and (for calls to generic functions) rewrites the call site to the
//! monomorph's name.
//!
//! Type Store & Unification
//! -----------------------------------
//! Type variables and their bindings live in `TypeStore`. `unify` follows bindings and
//! special-cases generic applications: two applications of the same template unify their
//! parameters pairwise, and an application unified with its own monomorph ties its parameters to
//! the monomorph 's concrete arguments.
use std::collections::{HashMap, HashSet};

use super::ast::{
    Block, Expr, ExprKind, Function, GlobalDecl, Literal, MatchStmt, Program, Stmt, StmtKind,
};
use super::monomorph::{MonomorphState, PendingInstance, TemplateDef};
use super::symbols::{EnumVariantInfo, SymbolTable};
use super::type_ast::{EnumDefinition, EnumVariant, FieldInit, StructDefinition, TypeParam};
use super::types::{
    AssignmentOperator, BinaryOperator, Type, TypeId, TypeStore, UnaryOperator, tuple_c_name,
};
use super::{Field, TypeDef};
use crate::codegen::parser::expr_pratt::callee_name;
use crate::error::{CompilationError, CompileResult, ErrorContext, Span};
use crate::type_err;

/// Map a template's type parameters to the type ids assigned to this application.
///
/// `params` parallels `type_params` (one id per parameter); the map lets annotation substitution
/// resolve `T`/`E` to the application's concrete (or freshly-unknown) type. Every generic path
/// rebuilds this, so it is defined here once.
fn param_id_map(type_params: &[TypeParam], params: &[TypeId]) -> HashMap<String, TypeId> {
    type_params
        .iter()
        .zip(params.iter())
        .map(|(tp, id)| (tp.name.clone(), *id))
        .collect()
}

/// Find a variant by name within an enum template.
///
/// Shared by the constructor and pattern paths so the "unknown variant" error stays consistent.
///
/// # Errors
///
/// Returns `TypeError` if no variant named `name` exists.
fn enum_variant<'a>(def: &'a EnumDefinition, name: &str) -> CompileResult<&'a EnumVariant> {
    def.variants
        .iter()
        .find(|v| v.name == name)
        .ok_or_else(|| type_err!("Unknown variant '{name}' in enum '{}'", def.name))
}

/// Type inference engine using Hindley-Milner algorithm.
#[derive(Default)]
pub struct TypeInferencer {
    pub store: TypeStore,
    symbols: SymbolTable,
    imported_structs: HashSet<String>,
    current_return_type: Option<TypeId>,
    source_file: String,
    source_text: String,

    /// Distinct tuple shapes referenced in the program, collected so codegen can emit one C struct
    /// definition per shape. Each entry is the generated C struct name and its element types
    /// (recursively flattened for nesting).
    tuple_shapes: Vec<(String, Vec<Type>)>,

    /// Monotonic counter for synthesizing unique temporary identifiers (e.g., tuple-destructure
    /// temporaries)
    fresh_counter: u32,

    /// Generic template definitions, instance type variables, the pending monomorph worklist, and
    /// realized monomorphs.
    pub(crate) monomorphs: MonomorphState,
}

impl TypeInferencer {
    /// Create a new type inferencer with an empty type store and symbol table.
    pub fn new() -> Self {
        Self {
            store: TypeStore::new(),
            symbols: SymbolTable::new(),
            imported_structs: HashSet::new(),
            current_return_type: None,
            source_file: String::new(),
            source_text: String::new(),
            tuple_shapes: Vec::new(),
            fresh_counter: 0,
            monomorphs: MonomorphState::default(),
        }
    }

    /// Borrow the distinct tuple shapes collected during inference.
    pub fn tuple_shapes(&self) -> &[(String, Vec<Type>)] {
        &self.tuple_shapes
    }

    /// Return a fresh, unique identifier name with the given prefix.
    fn fresh_name(&mut self, prefix: &str) -> String {
        let id = self.fresh_counter;
        self.fresh_counter += 1;
        format!("{prefix}{id}")
    }

    /// Record a tuple shape so its generated C struct is emitted exactly once.
    ///
    /// Recurses into nested tuple element types so nested shapes are also captured. Duplicates
    /// (same generated name) are stored only once.
    fn record_tuple(&mut self, elems: &[Type]) {
        let name = tuple_c_name(elems);
        if !self.tuple_shapes.iter().any(|(n, _)| n == &name) {
            let mut flattened = Vec::new();
            for el in elems {
                if let Type::Tuple(inner) = el {
                    self.record_tuple(inner);
                }
                flattened.push(el.clone());
            }
            self.tuple_shapes.push((name, flattened));
        }
    }

    /// Set the source file path and text for error context.
    pub fn with_source(mut self, file: String, text: String) -> Self {
        self.source_file = file;
        self.source_text = text;
        self
    }

    /// Get a reference to the symbol table (for use by code generation)
    pub fn symbols(&self) -> &SymbolTable {
        &self.symbols
    }

    /// Get a mutable reference to the symbol table (for registering C declarations)
    pub fn symbols_mut(&mut self) -> &mut SymbolTable {
        &mut self.symbols
    }

    /// Wrap a result with source context from an expression's span, if available.
    fn wrap_err<T>(&self, result: CompileResult<T>, span: Option<&Span>) -> CompileResult<T> {
        result.map_err(|e| {
            // Only attach context when we have both a span and real source text.
            // Tests create TypeInferencer without source, so skip in that case.
            if let Some(span) = span
                && !self.source_text.is_empty()
            {
                e.with_context(ErrorContext {
                    file: self.source_file.clone(),
                    source: self.source_text.clone(),
                    span: span.clone(),
                })
            } else {
                e
            }
        })
    }

    /// Check if a type name refers to a struct
    pub fn is_struct_type(&self, name: &str) -> bool {
        self.symbols.lookup_struct(name).is_some()
    }

    /// Mark a struct name as supplied by an imported C header.
    pub fn mark_imported_struct(&mut self, name: impl Into<String>) {
        self.imported_structs.insert(name.into());
    }

    /// Check whether a struct name must retain its external C spelling.
    pub fn is_imported_struct(&self, name: &str) -> bool {
        self.imported_structs.contains(name)
    }

    /// Infer types for an entire program.
    ///
    /// # Errors
    ///
    /// Returns `CompilationError` on type mismatches, unresolved types, or invalid
    /// generic applications discovered during inference.
    pub fn infer_program(&mut self, prog: &mut Program) -> CompileResult<()> {
        // Per-pass monomorphization state is rebuilt every pass; the fixpoint driver re-runs
        // `infer_program` until no monomorphs are realized.
        self.begin_monomorph_pass();

        // Generic definitions (non-empty `type_params`) are templates: they are not registered or
        // typed directly (see `register_*`); only their monomorphs are.
        self.register_enum_types(&prog.enums)?;
        self.register_struct_types(&prog.structs)?;
        self.register_typedefs(&prog.typedefs);

        // Infer global variable types first (before functions)
        self.infer_globals(&mut prog.globals)?;

        for func in &mut prog.functions {
            if func.type_params.is_empty() {
                self.infer_function(func)?;
            }
        }
        Ok(())
    }

    /// Infer types for global variable declarations
    fn infer_globals(&mut self, globals: &mut [GlobalDecl]) -> CompileResult<()> {
        for global in globals {
            if let Some(init_expr) = &mut global.init {
                let init_ty = self.infer_expr(init_expr)?;

                // Check if global has type annotation
                // If annotated, unify annotation with inferred type from initializer and use
                // annotation type as result (enforcing type from annotation)
                global.inferred = if let Some(ann) = &global.annotation {
                    let ann_ty = self.type_id_from_annotation(Some(ann), &HashMap::new())?;
                    self.unify(ann_ty, init_ty)?;
                    init_expr.ty = ann_ty;
                    ann_ty
                } else {
                    init_ty
                };

                self.symbols.define_global(&global.name, global.inferred);
            } else if let Some(ann) = &global.annotation {
                // Declaration without initializer -> just use annotation
                // Example: const int x;
                // No expression to infer type from, so we directly use the annotation
                global.inferred = self.type_id_from_annotation(Some(ann), &HashMap::new())?;
                self.symbols.define_global(&global.name, global.inferred);
            } else {
                // The Kit grammar allows both `:` type_annotation and `= expr` to be absent
                // independently (e.g. `var x;`), but without either there is no way to determine
                // the variable's type, so this is a semantic error.
                return Err(type_err!(
                    "Global variable '{}' declared without type annotation or initializer",
                    global.name
                ));
            }
        }
        Ok(())
    }

    /// Register enum types in the type store and symbol table
    fn register_enum_types(&mut self, enums: &[EnumDefinition]) -> CompileResult<()> {
        for enum_def in enums {
            // Generic (template) enums are not registered until monomorphized.
            if !enum_def.type_params.is_empty() {
                continue;
            }
            let mut resolved = enum_def.clone();
            for variant in &mut resolved.variants {
                for arg in &mut variant.args {
                    arg.ty =
                        self.type_id_from_annotation(arg.annotation.as_ref(), &HashMap::new())?;
                }
                self.symbols.define_enum_variant(variant);
            }
            self.symbols.define_enum(resolved);
        }
        Ok(())
    }

    /// Register struct types in the type store and symbol table
    fn register_struct_types(&mut self, structs: &[StructDefinition]) -> CompileResult<()> {
        for struct_def in structs {
            // Generic (template) structs are not registered: they have no concrete layout until
            // monomorphized
            if !struct_def.type_params.is_empty() {
                continue;
            }

            // Build field type list and update field types
            let mut updated_fields = Vec::new();
            for field in &struct_def.fields {
                let field_type_id =
                    self.type_id_from_annotation(field.annotation.as_ref(), &HashMap::new())?;

                updated_fields.push(Field {
                    name: field.name.clone(),
                    ty: field_type_id,
                    annotation: field.annotation.clone(),
                    is_const: field.is_const,
                    default: field.default.clone(),
                });
            }

            // Create updated struct definition with resolved field types
            let updated_struct_def = StructDefinition {
                name: struct_def.name.clone(),
                type_params: struct_def.type_params.clone(),
                fields: updated_fields,
                is_public: struct_def.is_public,
                metadata: struct_def.metadata.clone(),
            };

            let field_types: Vec<(String, TypeId)> = updated_struct_def
                .fields
                .iter()
                .map(|field| (field.name.clone(), field.ty))
                .collect();

            // Create struct type and register it
            let struct_type = Type::Struct {
                name: updated_struct_def.name.clone(),
                fields: field_types.clone(),
            };

            let _struct_type_id = self.store.new_known(struct_type);

            // Register updated struct in symbol table for field lookups
            self.symbols.define_struct(updated_struct_def);
        }
        Ok(())
    }

    /// Register typedef aliases in the type store so they can be resolved during unification.
    fn register_typedefs(&mut self, typedefs: &[TypeDef]) {
        for td in typedefs {
            self.store
                .register_typedef(td.name.clone(), td.type_def.clone());
        }
    }

    /// Infer types for a function definition
    fn infer_function(&mut self, func: &mut Function) -> CompileResult<()> {
        // Push a scope for function parameters and body
        self.symbols.push_scope();

        // Infer parameter types (fresh unknowns if unannotated)
        for param in &mut func.params {
            param.ty = self.type_id_from_annotation(param.annotation.as_ref(), &HashMap::new())?;
            self.symbols.define_var(&param.name, param.ty);
        }

        // Infer return type
        func.inferred_return = match func.return_type.as_ref() {
            Some(r) => Some(self.type_id_from_annotation(Some(r), &HashMap::new())?),
            None => Some(self.store.new_unknown()),
        };

        self.current_return_type = func.inferred_return;

        // Infer function body
        self.infer_block(&mut func.body)?;

        // Functions without an explicit return type that don't return a value implicitly return
        // void. Use find_rep since is_unknown doesn't follow bindings.
        if let Some(ret_id) = func.inferred_return {
            let rep = self.store.find_rep(ret_id);

            if self.store.is_unknown(rep) {
                let void_id = self.store.new_known(Type::Void);
                self.store.unify(ret_id, void_id)?;
                func.inferred_return = Some(void_id);
            }
        }

        self.current_return_type = None;

        // Pop function scope (discards params and local vars - they're no longer needed after
        // inference since codegen uses the AST's TypeId fields directly)
        self.symbols.pop_scope();

        // Register function signature in symbol table
        if let Some(ret_ty_id) = func.inferred_return {
            let param_ids: Vec<TypeId> = func.params.iter().map(|p| p.ty).collect();
            self.symbols
                .define_function(&func.name, param_ids.clone(), ret_ty_id);

            // Register function as a value (for higher-order calls like `g(f)`).
            // Resolve TypeIds back to Type values since Type::Function stores by value.
            let param_tys: Vec<Type> = param_ids
                .iter()
                .filter_map(|id| self.store.resolve(*id).ok())
                .collect();
            let ret_ty = self.store.resolve(ret_ty_id).ok();
            if param_tys.len() == param_ids.len()
                && let Some(ret_ty) = ret_ty
            {
                let fn_ty = Type::Function {
                    param_tys,
                    ret_ty: Box::new(ret_ty),
                };
                let fn_ty_id = self.store.new_known(fn_ty);
                self.symbols.define_global(&func.name, fn_ty_id);
            } else {
                eprintln!(
                    "Warning: function '{}' is not usable as a first-class value \
                     because a parameter or return type could not be resolved",
                    func.name
                );
            }
        }

        Ok(())
    }

    /// Infer types for a block of statements
    fn infer_block(&mut self, block: &mut Block) -> CompileResult<()> {
        self.symbols.push_scope();
        let mut i = 0;
        while i < block.stmts.len() {
            let mut replacements = self.infer_stmt(&mut block.stmts[i])?;
            // Replace the current statement with the first replacement, then splice
            // any extras (e.g. tuple-destructure bindings) immediately after it so
            // they share the enclosing scope.
            let first = replacements.remove(0);
            block.stmts[i] = first;
            // Insert any extras (e.g. tuple-destructure bindings) directly after
            // the statement; they share the enclosing scope. Advancing by 1 lets
            // the loop process those freshly-inserted statements next.
            for (j, extra) in replacements.into_iter().enumerate() {
                block.stmts.insert(i + 1 + j, extra);
            }
            i += 1;
        }
        self.symbols.pop_scope();
        Ok(())
    }

    /// Infer types for a single statement. Returns the statement itself followed
    /// by any additional statements produced by desugaring (e.g. tuple
    /// destructuring splices extra `VarDecl`s into the enclosing scope).
    fn infer_stmt(&mut self, stmt: &mut Stmt) -> CompileResult<Vec<Stmt>> {
        let span = stmt.span.clone();
        let extras = self.infer_stmt_inner(stmt);
        let extras = self.wrap_err(extras, Some(&span))?;
        let mut out = vec![stmt.clone()];
        out.extend(extras);
        Ok(out)
    }

    fn infer_stmt_inner(&mut self, stmt: &mut Stmt) -> CompileResult<Vec<Stmt>> {
        // Tuple destructuring assignment (`(a, b, _) = expr;`) lowers to a sequence
        // of `VarDecl`s that must live in the *enclosing* scope (so the bindings
        // outlive this statement). We replace `stmt` with the first declaration
        // (the RHS binding) and let the normal `VarDecl` arm below infer it, while
        // `extra_decls` carries the remaining bindings to be spliced into the
        // surrounding block by `infer_block`.
        let mut extra_decls: Vec<Stmt> = Vec::new();
        let is_destructure = matches!(
            &stmt.kind,
            StmtKind::Expr(Expr { kind: ExprKind::Assign { left, .. }, .. })
                if matches!(left.kind, ExprKind::TupleLit { .. })
        );
        if is_destructure {
            let mut decls = self.desugar_tuple_destructure(stmt)?;
            let first = decls.remove(0);
            stmt.kind = first.kind;
            extra_decls = decls;
        }

        match &mut stmt.kind {
            StmtKind::VarDecl {
                name,
                annotation,
                inferred,
                init,
            } => {
                if let Some(init_expr) = init {
                    let init_ty = self.infer_expr(init_expr)?;

                    *inferred = if let Some(ann) = annotation {
                        let ann_ty = self.type_id_from_annotation(Some(ann), &HashMap::new())?;
                        self.unify(ann_ty, init_ty)?;
                        init_expr.ty = ann_ty;
                        ann_ty
                    } else {
                        init_ty
                    };

                    self.symbols.define_var(name, *inferred);
                } else if let Some(ann) = annotation {
                    // Declaration without initializer -> just use annotation
                    *inferred = self.type_id_from_annotation(Some(ann), &HashMap::new())?;
                    self.symbols.define_var(name, *inferred);
                } else {
                    return Err(type_err!(
                        "Variable '{name}' declared without type annotation or initializer",
                    ));
                }
            }

            StmtKind::Expr(expr) => {
                self.infer_expr(expr)?;
            }

            StmtKind::Return(Some(expr)) => {
                let expr_ty = self.infer_expr(expr)?;
                if let Some(ret_ty) = self.current_return_type {
                    self.unify(ret_ty, expr_ty)?;
                    expr.ty = ret_ty;
                } else {
                    return Err(type_err!("Return statement outside of function"));
                }
            }

            // Void return - check if function expects void
            StmtKind::Return(None) => {
                if let Some(ret_ty) = self.current_return_type {
                    let void_ty = self.store.new_known(Type::Void);
                    self.unify(ret_ty, void_ty)?;
                } else {
                    return Err(type_err!("Return statement outside of function"));
                }
            }

            StmtKind::If {
                cond,
                then_branch,
                else_branch,
            } => {
                let cond_ty = self.infer_expr(cond)?;
                let bool_ty = self.store.new_known(Type::Bool);
                self.unify(cond_ty, bool_ty)?;

                self.infer_block(then_branch)?;
                if let Some(else_b) = else_branch {
                    self.infer_block(else_b)?;
                }
            }

            StmtKind::While { cond, body } => {
                let cond_ty = self.infer_expr(cond)?;
                let bool_ty = self.store.new_known(Type::Bool);
                self.unify(cond_ty, bool_ty)?;

                self.infer_block(body)?;
            }

            StmtKind::For { var, iter, body } => {
                let iter_ty = self.infer_expr(iter)?;

                // NOTE: RangeLiteral is typed as Void (see infer_range_literal),
                // so this accepts both integer-count and range-based for-loops.
                // Accept CArray for iterating over arrays (e.g. `for x in arr`).
                let iter_resolved = self.store.resolve(iter_ty)?;

                let var_ty = match &iter_resolved {
                    // For CArray, the loop variable gets the element type
                    Type::CArray(elem_type, _) => self.store.new_known(*elem_type.clone()),
                    // Int and Void use int as the loop variable (count-based)
                    Type::Int | Type::Void => self.store.new_known(Type::Int),
                    other => {
                        return Err(type_err!(
                            "For loop iterator must be Int, Range, or Array, found {other}"
                        ));
                    }
                };
                self.symbols.define_var(var, var_ty);

                self.infer_block(body)?;
            }

            StmtKind::Match(m) => {
                self.infer_match_stmt(m)?;
            }

            StmtKind::Break | StmtKind::Continue => {
                // No type inference needed - just control flow
            }

            StmtKind::Defer { body } => {
                // `infer_stmt` desugars a tuple-destructuring body into several
                // statements (a temp binding plus one per bound name). Keep them
                // all inside the defer body (a synthesized block) so none of the
                // bindings are silently dropped.
                let mut pieces = self.infer_stmt(body)?;
                if pieces.len() > 1 {
                    let first = pieces.remove(0);
                    for ex in &mut pieces {
                        self.infer_stmt_inner(ex)?;
                    }
                    let mut all = vec![first];
                    all.extend(pieces);
                    **body = Stmt {
                        kind: StmtKind::Block(Block { stmts: all }),
                        span: body.span.clone(),
                    };
                }
            }

            StmtKind::Block(block) => {
                self.infer_block(block)?;
            }
        }
        Ok(extra_decls)
    }

    /// Infer types for a match statement.
    fn infer_match_stmt(&mut self, m: &mut MatchStmt) -> CompileResult<()> {
        let matched_ty = self.infer_expr(&mut m.expr)?;
        for arm in &mut m.arms {
            let is_default = matches!(
                &arm.pattern,
                Expr {
                    kind: ExprKind::Identifier { name, .. },
                    ..
                } if name == "default" || name == "_"
            );
            self.symbols.push_scope();
            if !is_default {
                let pattern_ty = self.infer_pattern(&mut arm.pattern)?;
                self.unify(matched_ty, pattern_ty)?;
            }
            self.extract_pattern_bindings(&arm.pattern, matched_ty)?;
            self.infer_block(&mut arm.body)?;
            self.symbols.pop_scope();
        }
        Ok(())
    }

    /// Infer types for a pattern expression. Unlike `infer_expr`, this handles
    /// enum constructor patterns like `SomeInt(x)` by looking up the variant
    /// and treating identifier arguments as bindings rather than references.
    fn infer_pattern(&mut self, pattern: &mut Expr) -> CompileResult<TypeId> {
        match &mut pattern.kind {
            ExprKind::Identifier { name } if name == "_" || name == "default" => {
                let fresh = self.store.new_unknown();
                pattern.ty = fresh;
                Ok(fresh)
            }
            ExprKind::Identifier { .. } => {
                // Binding pattern: create an unknown type for the AST node
                let fresh = self.store.new_unknown();
                pattern.ty = fresh;
                Ok(fresh)
            }
            ExprKind::Call { callee, args } => {
                // Enum constructor pattern: `SomeVal(v)` or `SomeVal(1)`
                if let ExprKind::Identifier { name: variant_name } = &callee.kind
                    && let Some(info) = self.variant_info_by_simple_name(variant_name)
                {
                    // Generic (template) enums create their own application; the
                    // caller unifies its instance variable with the matched value.
                    if self.is_template_enum(&info.enum_name) {
                        return self.infer_generic_enum_pattern(
                            &mut pattern.ty,
                            &info.enum_name,
                            &info.variant_name,
                            args,
                        );
                    }
                    let enum_ty = self.store.new_known(Type::Named(info.enum_name.clone()));
                    pattern.ty = enum_ty;
                    let arg_types = info.arg_types.clone();
                    for (arg, &expected_ty) in args.iter_mut().zip(arg_types.iter()) {
                        let arg_ty = self.infer_pattern(arg)?;
                        self.unify(expected_ty, arg_ty)?;
                    }
                    return Ok(enum_ty);
                }
                self.infer_expr(pattern)
            }
            _ => self.infer_expr(pattern),
        }
    }

    /// Look up a generic enum template by declared name.
    ///
    /// Returns the cloned `EnumDefinition`; every generic-enum path uses this so
    /// the "missing template" internal error is reported in one place.
    ///
    /// # Errors
    /// Returns `TypeError` if `base` is not a registered enum template.
    fn template_enum(&self, base: &str) -> CompileResult<EnumDefinition> {
        match self.monomorphs.templates.get(base).cloned() {
            Some((_, TemplateDef::Enum(def))) => Ok(def),
            _ => Err(type_err!("internal error: missing template '{base}'")),
        }
    }

    /// Look up a generic struct template by declared name.
    ///
    /// Returns the cloned `StructDefinition`; mirrors `template_enum` for structs.
    ///
    /// # Errors
    /// Returns `TypeError` if `base` is not a registered struct template.
    fn template_struct(&self, base: &str) -> CompileResult<StructDefinition> {
        match self.monomorphs.templates.get(base).cloned() {
            Some((_, TemplateDef::Struct(def))) => Ok(def),
            _ => Err(type_err!("internal error: missing template '{base}'")),
        }
    }

    /// Look up a generic function template by declared name.
    ///
    /// Returns the cloned `Function`; mirrors `template_enum` for functions.
    ///
    /// # Errors
    /// Returns `TypeError` if `base` is not a registered function template.
    fn template_function(&self, base: &str) -> CompileResult<Function> {
        match self.monomorphs.templates.get(base).cloned() {
            Some((_, TemplateDef::Function(def))) => Ok(def),
            _ => Err(type_err!("internal error: missing template '{base}'")),
        }
    }

    /// Infer and unify a generic enum's constructor/pattern arguments.
    ///
    /// Each supplied argument's expected type comes from the variant field's
    /// annotation via the application's `id_map`. `is_pattern` selects pattern
    /// inference (`infer_pattern`, which binds identifiers rather than evaluating
    /// a value) versus expression inference (`infer_expr`); only the constructor
    /// path records the resolved type back onto the argument.
    ///
    /// # Errors
    /// Returns `TypeError` on an argument/field type mismatch.
    fn unify_variant_args(
        &mut self,
        variant: &EnumVariant,
        args: &mut [Expr],
        id_map: &HashMap<String, TypeId>,
        is_pattern: bool,
    ) -> CompileResult<()> {
        for (arg, field) in args.iter_mut().zip(variant.args.iter()) {
            let expected = self.type_id_from_annotation(field.annotation.as_ref(), id_map)?;
            if is_pattern {
                let arg_ty = self.infer_pattern(arg)?;
                self.unify(expected, arg_ty)?;
            } else {
                let arg_ty = self.infer_expr(arg)?;
                self.unify(arg_ty, expected)?;
                arg.ty = expected;
            }
        }
        Ok(())
    }

    /// Validate, default-fill, and type-check struct field initializers.
    ///
    /// Shared by the concrete and generic struct-init paths; the only difference
    /// is how an expected field type is obtained. With `id_map = Some`, fields use
    /// the application's substituted template annotation (generic); with `None`,
    /// fields use their registered annotation, or the value's inferred type when
    /// no annotation is present (concrete).
    ///
    /// # Errors
    /// Returns `TypeError` for an unknown field, a missing non-default field, or a
    /// value/field type mismatch.
    fn validate_and_infer_struct_fields(
        &mut self,
        def_fields: &[Field],
        fields: &mut Vec<FieldInit>,
        id_map: Option<&HashMap<String, TypeId>>,
        struct_name: &str,
    ) -> CompileResult<()> {
        // Unknown-field and required-field checks run before defaults are
        // injected, so injected fields never need re-validation.
        let provided: HashSet<String> = fields.iter().map(|f| f.name.clone()).collect();
        for fi in fields.iter() {
            if !def_fields.iter().any(|f| f.name == fi.name) {
                return Err(type_err!(
                    "Struct '{struct_name}' has no field '{}'",
                    fi.name
                ));
            }
        }
        for fd in def_fields {
            if !provided.contains(&fd.name) && fd.default.is_none() {
                return Err(type_err!(
                    "Struct '{struct_name}' field '{}' has no default value and was not provided in initialization",
                    fd.name
                ));
            }
        }
        // Inject default values for missing optional fields.
        for fd in def_fields {
            if !provided.contains(&fd.name)
                && let Some(default) = &fd.default
            {
                fields.push(FieldInit {
                    name: fd.name.clone(),
                    value: default.clone(),
                });
            }
        }
        for fi in fields.iter_mut() {
            let fd = def_fields
                .iter()
                .find(|f| f.name == fi.name)
                .ok_or_else(|| type_err!("Struct field '{}' not found in definition", fi.name))?;
            let inferred = self.infer_expr(&mut fi.value)?;
            let expected = match id_map {
                // Generic field: expect the application's substituted annotation.
                Some(im) => self.type_id_from_annotation(fd.annotation.as_ref(), im)?,
                // Concrete field: expect the declared type, or the inferred type
                // when the field carries no annotation.
                None => match &fd.annotation {
                    Some(ann) => self.store.new_known(ann.clone()),
                    None => inferred,
                },
            };
            self.unify(inferred, expected)?;
            fi.value.ty = expected;
        }
        Ok(())
    }

    /// Type an enum-constructor pattern against a generic (template) enum.
    ///
    /// Creates fresh parameter variables per application (unified with the matched
    /// value's parameters by the caller) so the pattern's bindings share the
    /// subject's parameters.
    ///
    /// # Errors
    /// Returns an internal `TypeError` if the template is missing.
    fn infer_generic_enum_pattern(
        &mut self,
        pattern_ty: &mut TypeId,
        enum_base: &str,
        variant_name: &str,
        args: &mut [Expr],
    ) -> Result<TypeId, CompilationError> {
        let (params, pattern_ty_instance) =
            self.instance_params_and_type(enum_base, &[], &HashMap::new())?;
        let def = self.template_enum(enum_base)?;
        let variant = enum_variant(&def, variant_name)?;
        let id_map = param_id_map(&def.type_params, &params);
        self.unify_variant_args(variant, args, &id_map, true)?;
        *pattern_ty = pattern_ty_instance;
        Ok(pattern_ty_instance)
    }

    /// Concrete argument types of a generic-enum variant pattern.
    ///
    /// Taken from the pattern's own instance application when its parameters match the match
    /// subject, or from the concrete monomorph the pattern already resolved to. Returns `None` when
    /// the application is still unknown.
    fn generic_variant_arg_types(
        &mut self,
        pattern_ty: TypeId,
        info: &EnumVariantInfo,
    ) -> CompileResult<Option<Vec<TypeId>>> {
        let rep = self.store.find_rep(pattern_ty);
        if let Some(inst) = self.monomorphs.instance_types.get(&rep).cloned() {
            let Some((_, TemplateDef::Enum(def))) =
                self.monomorphs.templates.get(&inst.base).cloned()
            else {
                return Ok(None);
            };
            let Some(variant) = def
                .variants
                .iter()
                .find(|v| v.name == info.variant_name)
                .cloned()
            else {
                return Ok(None);
            };
            let id_map = param_id_map(&def.type_params, &inst.params);
            let mut tys = Vec::new();
            for field in &variant.args {
                tys.push(self.type_id_from_annotation(field.annotation.as_ref(), &id_map)?);
            }
            return Ok(Some(tys));
        }
        // The pattern unified with a concrete monomorph already.
        if let Ok(Type::Named(name)) = self.store.resolve(rep)
            && self.is_monomorph_name(&name)
            && let Some(ed) = self.symbols.lookup_enum(&name)
        {
            let Some(variant) = ed.variants.iter().find(|v| v.name == info.variant_name) else {
                return Ok(None);
            };
            return Ok(Some(variant.args.iter().map(|a| a.ty).collect()));
        }
        Ok(None)
    }

    /// Walk a pattern expression tree and bind any identifiers as const variables.
    /// For enum variants like `SomeInt(x)`, the identifier `x` is bound to the
    /// variant's field type.
    fn extract_pattern_bindings(
        &mut self,
        pattern: &Expr,
        matched_ty: TypeId,
    ) -> CompileResult<()> {
        match &pattern.kind {
            ExprKind::Identifier { name } if name == "_" || name == "default" => Ok(()),
            ExprKind::Identifier { name } => {
                self.symbols.define_var(name, matched_ty);
                Ok(())
            }
            ExprKind::Call { callee, args } => {
                if let ExprKind::Identifier { name: variant_name } = &callee.kind
                    && let Some(info) = self.variant_info_by_simple_name(variant_name)
                {
                    // Generic (template) enums: bind against the argument types
                    // of the pattern's own application, so bindings share the
                    // match subject's parameters.
                    if self.is_template_enum(&info.enum_name) {
                        let expected_types = self.generic_variant_arg_types(pattern.ty, &info)?;
                        if let Some(expected_types) = expected_types {
                            if args.len() != expected_types.len() {
                                return Err(type_err!(
                                    "pattern '{}' has {} args but variant expects {}",
                                    variant_name,
                                    args.len(),
                                    expected_types.len(),
                                ));
                            }
                            for (arg, &expected_ty) in args.iter().zip(expected_types.iter()) {
                                self.extract_pattern_bindings(arg, expected_ty)?;
                            }
                            return Ok(());
                        }
                    }
                    if args.len() != info.arg_types.len() {
                        return Err(type_err!(
                            "pattern '{}' has {} args but variant expects {}",
                            variant_name,
                            args.len(),
                            info.arg_types.len(),
                        ));
                    }
                    for (arg, &expected_ty) in args.iter().zip(info.arg_types.iter()) {
                        self.extract_pattern_bindings(arg, expected_ty)?;
                    }
                    return Ok(());
                }
                for arg in args {
                    self.extract_pattern_bindings(arg, matched_ty)?;
                }
                Ok(())
            }
            ExprKind::Literal { .. } => Ok(()),
            ExprKind::StructInit { fields, .. } => {
                for field in fields {
                    self.extract_pattern_bindings(&field.value, matched_ty)?;
                }
                Ok(())
            }
            ExprKind::FieldAccess { expr, .. } => self.extract_pattern_bindings(expr, matched_ty),
            _ => Ok(()),
        }
    }

    /// Infer types for an expression
    fn infer_expr(&mut self, expr: &mut Expr) -> Result<TypeId, CompilationError> {
        let span = expr.span.clone();
        let result = self.infer_expr_inner(expr);
        self.wrap_err(result, Some(&span))
    }

    /// Lower a tuple-destructuring assignment statement into a `Block` of `VarDecl`s.
    ///
    /// The RHS is bound once to a synthetic temporary; each bound pattern name
    /// becomes a `VarDecl` initialized from `temp.__slotN` (nested tuples recurse).
    fn desugar_tuple_destructure(&mut self, stmt: &Stmt) -> CompileResult<Vec<Stmt>> {
        let (pattern, right) = match &stmt.kind {
            StmtKind::Expr(e) => match &e.kind {
                ExprKind::Assign { left, right, .. } => (left.clone(), right.clone()),
                _ => unreachable!("desugar_tuple_destructure called on non-assign expr"),
            },
            _ => unreachable!("desugar_tuple_destructure called on non-expr stmt"),
        };

        let mut right_infer = *right.clone();
        let right_ty_id = self.infer_expr(&mut right_infer)?;
        let tuple_ty = self
            .store
            .resolve(right_ty_id)
            .map_err(|e| type_err!("Failed to resolve tuple type: {e}"))?;

        let Type::Tuple(elems) = &tuple_ty else {
            return Err(type_err!(
                "Cannot destructure a non-tuple value of type {tuple_ty}"
            ));
        };

        // Bind the RHS once so its evaluation is not duplicated across slots.
        // (Scoping is handled by the caller splicing the resulting declarations
        // into the enclosing block, so the bindings outlive this statement.)
        let tmp = self.fresh_name("__kit_tuple_dest_");
        let tmp_ty_id = self.store.new_known(tuple_ty.clone());

        let mut decls: Vec<Stmt> = Vec::new();
        decls.push(Stmt {
            kind: StmtKind::VarDecl {
                name: tmp.clone(),
                annotation: Some(tuple_ty.clone()),
                inferred: tmp_ty_id,
                init: Some(right_infer),
            },
            span: stmt.span.clone(),
        });

        let base = Expr {
            kind: ExprKind::Identifier { name: tmp.clone() },
            ty: tmp_ty_id,
            span: stmt.span.clone(),
        };

        match &pattern.kind {
            ExprKind::TupleLit { elements: pats } => {
                if pats.len() != elems.len() {
                    return Err(type_err!(
                        "Tuple destructuring arity mismatch: pattern has {} elements but value has {}",
                        pats.len(),
                        elems.len()
                    ));
                }
                for (i, pat) in pats.iter().enumerate() {
                    let slot = Expr {
                        kind: ExprKind::Index {
                            expr: Box::new(base.clone()),
                            index: Box::new(Expr {
                                kind: ExprKind::Literal {
                                    value: Literal::Int(i as i64),
                                },
                                ty: TypeId::default(),
                                span: stmt.span.clone(),
                            }),
                        },
                        ty: TypeId::default(),
                        span: stmt.span.clone(),
                    };
                    self.emit_destructure_pattern(pat, &slot, &elems[i], &mut decls)?;
                }
            }
            _ => {
                return Err(type_err!(
                    "Tuple destructuring requires a tuple pattern on the left-hand side"
                ));
            }
        }

        Ok(decls)
    }

    /// Emit `VarDecl`s for one destructuring pattern element bound to `slot`.
    /// `_` is skipped; identifiers become declarations; nested `TupleLit`s recurse.
    fn emit_destructure_pattern(
        &mut self,
        pat: &Expr,
        slot: &Expr,
        elem_ty: &Type,
        decls: &mut Vec<Stmt>,
    ) -> CompileResult<()> {
        match &pat.kind {
            ExprKind::Identifier { name } if name == "_" => Ok(()),
            ExprKind::Identifier { name } => {
                if self.symbols.lookup_var(name).is_some() {
                    // Reassignment to an already-declared variable: emit a plain
                    // assignment instead of a fresh `VarDecl`, which would otherwise
                    // shadow or (in the same scope) redefine the existing binding.
                    let assign = Expr {
                        kind: ExprKind::Assign {
                            op: AssignmentOperator::Assign,
                            left: Box::new(Expr {
                                kind: ExprKind::Identifier { name: name.clone() },
                                ty: TypeId::default(),
                                span: slot.span.clone(),
                            }),
                            right: Box::new(slot.clone()),
                        },
                        ty: TypeId::default(),
                        span: slot.span.clone(),
                    };
                    decls.push(Stmt {
                        kind: StmtKind::Expr(assign),
                        span: slot.span.clone(),
                    });
                } else {
                    let elem_ty_id = self.store.new_known(elem_ty.clone());
                    self.symbols.define_var(name, elem_ty_id);
                    decls.push(Stmt {
                        kind: StmtKind::VarDecl {
                            name: name.clone(),
                            annotation: Some(elem_ty.clone()),
                            inferred: elem_ty_id,
                            init: Some(slot.clone()),
                        },
                        span: slot.span.clone(),
                    });
                }
                Ok(())
            }
            ExprKind::TupleLit { elements: subs } => {
                let Type::Tuple(sub_elems) = elem_ty else {
                    return Err(type_err!("Cannot destructure a non-tuple pattern element"));
                };
                if subs.len() != sub_elems.len() {
                    return Err(type_err!("Nested tuple destructuring arity mismatch"));
                }
                for (i, sub) in subs.iter().enumerate() {
                    let nested = Expr {
                        kind: ExprKind::Index {
                            expr: Box::new(slot.clone()),
                            index: Box::new(Expr {
                                kind: ExprKind::Literal {
                                    value: Literal::Int(i as i64),
                                },
                                ty: TypeId::default(),
                                span: slot.span.clone(),
                            }),
                        },
                        ty: TypeId::default(),
                        span: slot.span.clone(),
                    };
                    self.emit_destructure_pattern(sub, &nested, &sub_elems[i], decls)?;
                }
                Ok(())
            }
            _ => Err(type_err!("Unsupported tuple destructuring pattern")),
        }
    }

    fn infer_expr_inner(&mut self, expr: &mut Expr) -> Result<TypeId, CompilationError> {
        Ok(match &expr.kind {
            ExprKind::Identifier { .. } => self.infer_identifier(expr)?,
            ExprKind::Literal { .. } => self.infer_literal(expr)?,
            ExprKind::Call { .. } if self.is_call_enum_constructor(expr) => {
                self.infer_enum_constructor_call(expr)?
            }
            ExprKind::Call { .. } => self.infer_function_call(expr)?,
            ExprKind::UnaryOp { .. } => self.infer_unary_op(expr)?,
            ExprKind::BinaryOp { .. } => self.infer_binary_op(expr)?,
            ExprKind::Assign { .. } => self.infer_assign(expr)?,
            ExprKind::If { .. } => self.infer_if_expr(expr)?,
            ExprKind::RangeLiteral { .. } => self.infer_range_literal(expr)?,
            ExprKind::StructInit { .. } => self.infer_struct_init(expr)?,
            ExprKind::FieldAccess { .. } => self.infer_field_access(expr)?,
            ExprKind::EnumVariant { .. } => self.infer_enum_variant(expr)?,
            ExprKind::EnumInit { .. } => self.infer_enum_init(expr)?,
            ExprKind::ArrayLiteral { .. } => self.infer_array_literal(expr)?,
            ExprKind::TupleLit { .. } => self.infer_tuple_lit(expr)?,
            ExprKind::Index { .. } => self.infer_index(expr)?,
        })
    }

    fn is_call_enum_constructor(&self, expr: &Expr) -> bool {
        match &expr.kind {
            ExprKind::Call { callee, .. } => callee_name(callee)
                .is_some_and(|name| self.variant_info_by_simple_name(&name).is_some()),
            _ => false,
        }
    }

    /// Look up an enum variant by simple name.
    ///
    /// Constructor calls and patterns resolve to the *template* declaration of a generic enum
    /// (never to a monomorph, which is selected by the expression's type), so templates are
    /// preferred here deterministically; the symbol table would otherwise return whichever
    /// registered first (`HashMap` iteration order).
    fn variant_info_by_simple_name(&self, name: &str) -> Option<EnumVariantInfo> {
        if let Some((_, (_, TemplateDef::Enum(def)))) = self.monomorphs.templates.iter().find(
            |(_, (_, def))| {
                matches!(def, TemplateDef::Enum(e) if e.variants.iter().any(|v| v.name == name))
            },
        ) {
            return def
                .variants
                .iter()
                .find(|v| v.name == name)
                .map(|v| EnumVariantInfo {
                    enum_name: def.name.clone(),
                    variant_name: v.name.clone(),
                    arg_types: v.args.iter().map(|a| a.ty).collect(),
                    has_defaults: v.args.iter().any(|a| a.default.is_some()),
                });
        }
        self.symbols
            .lookup_enum_variant_by_simple_name(name)
            .cloned()
    }

    fn infer_identifier(&mut self, expr: &mut Expr) -> Result<TypeId, CompilationError> {
        let name = match &expr.kind {
            ExprKind::Identifier { name } => name.clone(),
            _ => unreachable!("infer_identifier called on non-Identifier"),
        };
        if let Some(global_ty) = self.symbols.lookup_global(&name) {
            expr.ty = global_ty;
            Ok(global_ty)
        } else if let Some(var_ty) = self.symbols.lookup_var(&name) {
            expr.ty = var_ty;
            Ok(var_ty)
        } else if let Some(variant_info) = self.symbols.lookup_enum_variant(&name).cloned() {
            let enum_ty = if self.is_template_enum(&variant_info.enum_name) {
                self.instance_params_and_type(&variant_info.enum_name, &[], &HashMap::new())?
                    .1
            } else {
                self.store
                    .new_known(Type::Named(variant_info.enum_name.clone()))
            };
            let span = expr.span.clone();
            expr.ty = enum_ty;
            expr.kind = ExprKind::EnumVariant {
                enum_name: variant_info.enum_name.clone(),
                variant_name: variant_info.variant_name.clone(),
            };
            expr.span = span;
            Ok(enum_ty)
        } else {
            // NOTE: fallback - enumerates ALL enums to resolve bare variant names (e.g. `Red`)
            // since earlier paths only find qualified names ("Color.Red") or variables/globals.
            let mut found = None;
            for enum_def in self.symbols.get_enums() {
                for variant in &enum_def.variants {
                    if variant.name == name {
                        found = Some(enum_def.name.clone());
                        break;
                    }
                }
                if found.is_some() {
                    break;
                }
            }
            // Generic (template) enums are stashed, not in the symbol table.
            if found.is_none() {
                for (enum_name, (_, def)) in &self.monomorphs.templates {
                    if let TemplateDef::Enum(ed) = def
                        && ed.variants.iter().any(|v| v.name == name)
                    {
                        found = Some(enum_name.clone());
                        break;
                    }
                }
            }
            if let Some(enum_name) = found {
                let enum_ty = if self.is_template_enum(&enum_name) {
                    self.instance_params_and_type(&enum_name, &[], &HashMap::new())?
                        .1
                } else {
                    self.store.new_known(Type::Named(enum_name.clone()))
                };
                let span = expr.span.clone();
                expr.ty = enum_ty;
                expr.kind = ExprKind::EnumVariant {
                    enum_name: enum_name.clone(),
                    variant_name: name.clone(),
                };
                expr.span = span;
                Ok(enum_ty)
            } else {
                Err(type_err!(
                    "Use of undeclared variable or enum variant '{name}'"
                ))
            }
        }
    }

    fn infer_literal(&mut self, expr: &mut Expr) -> Result<TypeId, CompilationError> {
        let lit = match &expr.kind {
            ExprKind::Literal { value } => value.clone(),
            _ => unreachable!("infer_literal called on non-Literal"),
        };
        let ty = match lit {
            Literal::Int(_) => Type::Int,
            Literal::Float(_) => Type::Float,
            Literal::Char(_) => Type::Char,
            Literal::Bool(_) => Type::Bool,
            Literal::String(_) => Type::CString,
            Literal::Null => Type::Ptr(Box::new(Type::Void)),
        };
        let type_id = self.store.new_known(ty);
        expr.ty = type_id;
        Ok(type_id)
    }

    fn infer_enum_constructor_call(&mut self, expr: &mut Expr) -> Result<TypeId, CompilationError> {
        let ExprKind::Call { callee, args } = &mut expr.kind else {
            unreachable!("infer_enum_constructor_call called on non-Call");
        };
        let callee_str = callee_name(callee).expect("guard ensures this is valid");
        let variant_info = self
            .variant_info_by_simple_name(&callee_str)
            .expect("guard ensures this exists");

        // Generic (template) enum: instantiate per application.
        if self.is_template_enum(&variant_info.enum_name) {
            return self.infer_generic_enum_ctor(
                &mut expr.ty,
                &variant_info.enum_name,
                &variant_info.variant_name,
                args,
            );
        }
        let args_clone = args.clone();
        let enum_def = self.symbols.lookup_enum(&variant_info.enum_name).cloned();
        let mut resolved_args = if let Some(ref ed) = enum_def {
            Self::resolve_default_args(&variant_info, ed, &args_clone)?
        } else {
            args_clone
        };

        if resolved_args.len() != variant_info.arg_types.len() {
            return Err(type_err!(
                "Enum variant '{}' expects {} arguments, got {}",
                variant_info.variant_name,
                variant_info.arg_types.len(),
                resolved_args.len()
            ));
        }

        let expected_types: Vec<_> = variant_info.arg_types.clone();
        let enum_ty = self
            .store
            .new_known(Type::Named(variant_info.enum_name.clone()));
        for (arg, expected_ty) in resolved_args.iter_mut().zip(expected_types.iter()) {
            let arg_ty = self.infer_expr(arg)?;
            self.unify(arg_ty, *expected_ty)?;
            arg.ty = *expected_ty;
        }
        *args = resolved_args;
        expr.ty = enum_ty;
        Ok(enum_ty)
    }

    /// Infer an enum constructor call against a generic (template) enum, e.g. `OneValue(1)`.
    ///
    /// The variant's fields are substituted with fresh parameter variables (filled by the
    /// arguments); the call's type is the instance variable, unified with whatever
    /// annotation/initializer context references it.
    ///
    /// # Errors
    /// Returns an internal `TypeError` if the template is missing.
    fn infer_generic_enum_ctor(
        &mut self,
        expr_ty: &mut TypeId,
        enum_base: &str,
        variant_name: &str,
        args: &mut Vec<Expr>,
    ) -> Result<TypeId, CompilationError> {
        let (params, instance_ty) =
            self.instance_params_and_type(enum_base, &[], &HashMap::new())?;
        let def = self.template_enum(enum_base)?;
        let variant = enum_variant(&def, variant_name)?;

        let mut resolved_args = Self::resolve_default_args(
            &EnumVariantInfo {
                enum_name: enum_base.to_string(),
                variant_name: variant_name.to_string(),
                arg_types: variant.args.iter().map(|a| a.ty).collect(),
                has_defaults: variant.args.iter().any(|a| a.default.is_some()),
            },
            &def,
            args,
        )?;

        if resolved_args.len() != variant.args.len() {
            return Err(type_err!(
                "Enum variant '{}' expects {} arguments, got {}",
                variant_name,
                variant.args.len(),
                resolved_args.len()
            ));
        }

        let id_map = param_id_map(&def.type_params, &params);
        self.unify_variant_args(variant, &mut resolved_args, &id_map, false)?;
        *args = resolved_args;

        *expr_ty = instance_ty;
        Ok(instance_ty)
    }

    /// Static trait-method dispatch: rewrite `receiver.method(args)` to a direct call to the
    /// mangled impl-method symbol.
    ///
    /// Returns `true` if a rewrite happened (the caller should re-run `infer_function_call` on the
    /// rewritten expression). Returns `false` if the callee is not a field-access method call, or
    /// the receiver's type has no impl providing `method`, in which case ordinary handling applies.
    fn try_rewrite_method_call(&mut self, expr: &mut Expr) -> CompileResult<bool> {
        let ExprKind::Call { callee, args } = &mut expr.kind else {
            return Ok(false);
        };
        let ExprKind::FieldAccess {
            expr: receiver,
            field_name,
        } = &mut callee.kind
        else {
            return Ok(false);
        };

        // Infer the receiver to learn its concrete type. If it isn't a value (e.g. a module-
        // qualified path like `module.func`), this isn't a trait-method dispatch; fall through to
        // ordinary call handling rather than resolving a module name as a value.
        let Ok(recv_ty) = self.infer_expr(receiver) else {
            return Ok(false);
        };
        let recv_type = self.store.resolve(recv_ty)?;
        let Some(symbol) = self.lookup_method(&recv_type, field_name) else {
            return Ok(false);
        };

        // Rewrite to a direct call `symbol(receiver, args...)`, where `receiver` becomes the
        // synthesized `this` argument. The mangled symbol is already in the symbol table.
        let receiver_expr = (**receiver).clone();
        **callee = Expr::new(
            ExprKind::Identifier {
                name: symbol.clone(),
            },
            self.store.new_unknown(),
            Span::default(),
        );
        let mut new_args = Vec::with_capacity(args.len() + 1);
        new_args.push(receiver_expr);
        new_args.extend(std::mem::take(args));
        *args = new_args;
        Ok(true)
    }

    fn infer_function_call(&mut self, expr: &mut Expr) -> Result<TypeId, CompilationError> {
        // A call `receiver.method(args)` where the receiver's type implements a trait
        // providing `method` is static dispatch. Rewrite it to a direct call to the mangled impl
        // method, passing the receiver as the synthesized `this` argument. If no impl method
        // matches, fall through to ordinary call handling.
        if self.try_rewrite_method_call(expr)? {
            return self.infer_function_call(expr);
        }

        let ExprKind::Call { callee, args } = &mut expr.kind else {
            unreachable!("infer_function_call called on non-Call");
        };

        // Named function: lookup by string name in symbol table.
        if let Some(name) = callee_name(callee)
            && let Some((param_tys, ret_ty)) = self.symbols.lookup_function(&name)
        {
            return self.infer_call_with_sig(&name, param_tys, ret_ty, args, &mut expr.ty);
        }

        // Generic (template) function: instantiate its type parameters against
        // the argument types, exactly like the reference's `makeGeneric`.
        if let Some(name) = callee_name(callee)
            && self.is_template_function(&name)
        {
            return self.infer_generic_function_call(&mut expr.ty, args, &name);
        }

        // Indirect call: infer callee type and check callability.
        let mut infer_failed_on_name = false;
        match self.infer_expr(callee) {
            Ok(callee_ty_id) => {
                if let Ok(callee_ty) = self.store.resolve(callee_ty_id) {
                    let sig = match &callee_ty {
                        Type::Function { param_tys, ret_ty } => Some((param_tys, ret_ty.as_ref())),
                        Type::Ptr(inner) => {
                            if let Type::Function { param_tys, ret_ty } = inner.as_ref() {
                                Some((param_tys, ret_ty.as_ref()))
                            } else {
                                None
                            }
                        }
                        _ => None,
                    };
                    if let Some((param_tys, ret_ty)) = sig {
                        if args.len() != param_tys.len() {
                            return Err(type_err!(
                                "Function expects {} arguments, got {}",
                                param_tys.len(),
                                args.len()
                            ));
                        }
                        for (arg, param_ty) in args.iter_mut().zip(param_tys.iter()) {
                            let arg_ty = self.infer_expr(arg)?;
                            let param_ty_id = self.store.new_known(param_ty.clone());
                            self.unify(arg_ty, param_ty_id)?;
                        }
                        let ret_ty_id = self.store.new_known((*ret_ty).clone());
                        expr.ty = ret_ty_id;
                        return Ok(ret_ty_id);
                    }
                    // Resolved to a non-callable type.
                    return Err(type_err!("Cannot call a value of type {callee_ty}"));
                }
            }
            Err(e) => {
                // infer_expr failed. If the callee is a pure name (identifier or
                // field-access chain), it may be an external symbol. Fall through to
                // the C interop path. Otherwise propagate the error.
                if callee_name(callee).is_none() {
                    return Err(e);
                }
                infer_failed_on_name = true;
            }
        }

        // The callee name is absent from the symbol table, so treat it as an external C function.
        //
        // Use a fresh type variable for the return type so it unifies with whatever the context
        // expects.
        if let Some(name) = callee_name(callee)
            && self.symbols.lookup_function(&name).is_none()
            && self.symbols.lookup_global(&name).is_none()
        {
            let ret_ty = self.store.new_unknown();
            for arg in args.iter_mut() {
                self.infer_expr(arg)?;
            }
            expr.ty = ret_ty;
            return Ok(ret_ty);
        }

        let msg = if infer_failed_on_name {
            format!(
                "Cannot call '{}': not a known function, global, or external symbol",
                callee_name(callee).as_deref().unwrap_or("?")
            )
        } else {
            "Expression is not callable".to_string()
        };
        Err(type_err!("{msg}"))
    }

    /// Infer a call to a known function from the symbol table.
    fn infer_call_with_sig(
        &mut self,
        name: &str,
        param_tys: Vec<TypeId>,
        ret_ty: TypeId,
        args: &mut [Expr],
        call_ty: &mut TypeId,
    ) -> Result<TypeId, CompilationError> {
        if !param_tys.is_empty() && args.len() != param_tys.len() {
            return Err(type_err!(
                "Function '{name}' expects {} arguments, got {}",
                param_tys.len(),
                args.len()
            ));
        }

        if param_tys.is_empty() {
            for arg in args.iter_mut() {
                self.infer_expr(arg)?;
            }
        } else {
            for (arg, param_ty) in args.iter_mut().zip(param_tys.iter()) {
                let arg_ty = self.infer_expr(arg)?;
                self.unify(arg_ty, *param_ty)?;
                arg.ty = *param_ty;
            }
        }

        *call_ty = ret_ty;
        Ok(ret_ty)
    }

    /// Infer a call to a generic (template) function.
    ///
    /// Mirrors the reference's `makeGeneric`: one fresh type variable per parameter, arguments
    /// unified against the substituted parameter types, and a pending monomorph whose `call_return`
    /// lets `generate_monomorphs` rewrite this call site once the parameters resolve.
    ///
    /// # Errors
    /// Returns an internal `TypeError` if the template is missing.
    fn infer_generic_function_call(
        &mut self,
        expr_ty: &mut TypeId,
        args: &mut [Expr],
        name: &str,
    ) -> Result<TypeId, CompilationError> {
        let def = self.template_function(name)?;

        // Fresh type variable per type parameter, then a substituted signature.
        let mut id_map: HashMap<String, TypeId> = HashMap::new();
        for tp in &def.type_params {
            id_map.insert(tp.name.clone(), self.store.new_unknown());
        }
        let sig: Vec<TypeId> = def
            .params
            .iter()
            .map(|p| self.type_id_from_annotation(p.annotation.as_ref(), &id_map))
            .collect::<CompileResult<_>>()?;

        if args.len() != sig.len() {
            return Err(type_err!(
                "Function '{name}' expects {} arguments, got {}",
                sig.len(),
                args.len()
            ));
        }
        for (arg, param_ty) in args.iter_mut().zip(sig.iter()) {
            let arg_ty = self.infer_expr(arg)?;
            self.unify(arg_ty, *param_ty)?;
            arg.ty = *param_ty;
        }

        // Default specialization: a type parameter with no value inferred from the arguments but
        // a trait constraint that has a `default Trait as Type` declaration is bound to that type.
        // Applied here (after argument unification) so only genuinely-stuck parameters are
        // defaulted, and the pending application resolves to a concrete monomorph this pass.
        for tp in &def.type_params {
            let Some(var_id) = id_map.get(&tp.name).copied() else {
                continue;
            };
            if self.store.is_unknown(var_id)
                && let Some(default_ty) = crate::codegen::specialize::default_for_constraints(
                    &tp.constraints,
                    &self.monomorphs.defaults,
                )
            {
                self.store.bind_if_unbound(var_id, default_ty);
            }
        }

        // Substituted return type (void when the template declares none).
        let ret_id = match &def.return_type {
            Some(r) => self.type_id_from_annotation(Some(r), &id_map)?,
            None => self.store.new_known(Type::Void),
        };

        let params: Vec<TypeId> = def
            .type_params
            .iter()
            .map(|tp| {
                id_map
                    .get(&tp.name)
                    .copied()
                    .expect("type parameter bound above")
            })
            .collect();
        self.monomorphs
            .pending
            .push(super::monomorph::PendingGeneric {
                base: name.to_string(),
                params,
                call_return: Some(ret_id),
            });

        *expr_ty = ret_id;
        Ok(ret_id)
    }

    fn infer_unary_op(&mut self, expr: &mut Expr) -> Result<TypeId, CompilationError> {
        let ExprKind::UnaryOp { op, expr: inner } = &mut expr.kind else {
            unreachable!("infer_unary_op called on non-UnaryOp");
        };
        let expr_ty = self.infer_expr(inner)?;

        let result_ty = match op {
            UnaryOperator::AddressOf => {
                let resolved = self.store.resolve(expr_ty)?;
                let ptr_ty = Type::Ptr(Box::new(resolved));
                self.store.new_known(ptr_ty)
            }
            UnaryOperator::Dereference => {
                let resolved = self.store.resolve(expr_ty)?;
                if let Type::Ptr(inner_ty) = resolved {
                    self.store.new_known(*inner_ty)
                } else {
                    return Err(type_err!("Cannot dereference non-pointer type: {resolved}"));
                }
            }
            _ => expr_ty,
        };

        expr.ty = result_ty;
        Ok(result_ty)
    }

    fn infer_binary_op(&mut self, expr: &mut Expr) -> Result<TypeId, CompilationError> {
        let ExprKind::BinaryOp { op, left, right } = &mut expr.kind else {
            unreachable!("infer_binary_op called on non-BinaryOp");
        };
        let left_ty = self.infer_expr(left)?;
        let right_ty = self.infer_expr(right)?;

        let result_ty = match op {
            BinaryOperator::And | BinaryOperator::Or => {
                let bool_ty = self.store.new_known(Type::Bool);
                self.unify(left_ty, bool_ty)?;
                self.unify(right_ty, bool_ty)?;
                bool_ty
            }
            BinaryOperator::Eq
            | BinaryOperator::Ne
            | BinaryOperator::Lt
            | BinaryOperator::Gt
            | BinaryOperator::Le
            | BinaryOperator::Ge => {
                self.unify(left_ty, right_ty)?;
                self.store.new_known(Type::Bool)
            }
            _ => {
                self.unify(left_ty, right_ty)?;
                left_ty
            }
        };

        expr.ty = result_ty;
        Ok(result_ty)
    }

    fn infer_assign(&mut self, expr: &mut Expr) -> Result<TypeId, CompilationError> {
        let ExprKind::Assign { op: _, left, right } = &mut expr.kind else {
            unreachable!("infer_assign called on non-Assign");
        };
        let right_ty = self.infer_expr(right)?;
        let left_ty = self.infer_expr(left)?;

        self.unify(left_ty, right_ty)?;

        expr.ty = left_ty;
        Ok(left_ty)
    }

    fn infer_if_expr(&mut self, expr: &mut Expr) -> Result<TypeId, CompilationError> {
        let ExprKind::If {
            cond,
            then_branch,
            else_branch,
        } = &mut expr.kind
        else {
            unreachable!("infer_if_expr called on non-If");
        };
        let cond_ty = self.infer_expr(cond)?;
        let bool_ty = self.store.new_known(Type::Bool);
        self.unify(cond_ty, bool_ty)?;

        let then_ty = self.infer_expr(then_branch)?;
        let else_ty = self.infer_expr(else_branch)?;

        self.unify(then_ty, else_ty)?;

        expr.ty = then_ty;
        Ok(then_ty)
    }

    fn infer_range_literal(&mut self, expr: &mut Expr) -> Result<TypeId, CompilationError> {
        let ExprKind::RangeLiteral { start, end } = &mut expr.kind else {
            unreachable!("infer_range_literal called on non-RangeLiteral");
        };
        let start_ty = self.infer_expr(start)?;
        let end_ty = self.infer_expr(end)?;

        let int_ty = self.store.new_known(Type::Int);
        self.unify(start_ty, int_ty)?;
        self.unify(end_ty, int_ty)?;

        Ok(self.store.new_known(Type::Void))
    }

    fn infer_struct_init(&mut self, expr: &mut Expr) -> Result<TypeId, CompilationError> {
        let ExprKind::StructInit {
            struct_type,
            fields,
        } = &mut expr.kind
        else {
            unreachable!("infer_struct_init called on non-StructInit");
        };

        let Some(st) = struct_type.as_ref() else {
            return Err(type_err!("StructInit missing type annotation"));
        };

        // Generic structs (templates) instantiate through `makeGeneric`-style
        // parameter binding; concrete structs use the registered definition.
        let base = match st {
            Type::Named(name) => Some(name.clone()),
            Type::Instance { base, .. } => Some(base.clone()),
            _ => None,
        };
        if let Some(base) = base
            && self.is_template_struct(&base)
        {
            return self.infer_generic_struct_init(&mut expr.ty, &base, st, fields);
        }

        let resolved_ty = if let Some(ref st) = *struct_type {
            self.store.new_known(st.clone())
        } else {
            return Err(type_err!("StructInit missing type annotation"));
        };

        // resolve struct type from annotation. Clone so the immutable borrow of
        // `self.symbols` is released before the mutable field-inference pass.
        let struct_def = {
            let resolved = self.store.resolve(resolved_ty)?;
            match resolved {
                Type::Named(name) => self
                    .symbols
                    .lookup_struct(&name)
                    .ok_or_else(|| type_err!("Unknown struct type '{name}'"))?
                    .clone(),
                Type::Struct { name, .. } => self
                    .symbols
                    .lookup_struct(&name)
                    .ok_or_else(|| type_err!("Unknown struct type '{name}'"))?
                    .clone(),
                _ => return Err(type_err!("StructInit requires a struct type")),
            }
        };

        // Concrete struct: validate, default-fill, and type-check fields against
        // their registered annotations.
        self.validate_and_infer_struct_fields(&struct_def.fields, fields, None, &struct_def.name)?;

        expr.ty = resolved_ty;
        Ok(resolved_ty)
    }

    /// Infer initialization of a generic (template) struct, e.g.
    /// `struct WrapperType { innerValue: 2 }`.
    ///
    /// Supplied type arguments (from a `WrapperType[Int]` annotation; partial applications
    /// allowed) are bound first; missing parameters become fresh unknowns filled by the field
    /// initializers.
    ///
    /// # Errors
    /// Returns an internal `TypeError` if the template is missing.
    fn infer_generic_struct_init(
        &mut self,
        expr_ty: &mut TypeId,
        base: &str,
        struct_type: &Type,
        fields: &mut Vec<FieldInit>,
    ) -> Result<TypeId, CompilationError> {
        let supplied: Vec<Type> = match struct_type {
            Type::Instance { args, .. } => args.clone(),
            _ => vec![],
        };
        let (params, instance_ty) =
            self.instance_params_and_type(base, &supplied, &HashMap::new())?;

        let def = self.template_struct(base)?;
        let id_map = param_id_map(&def.type_params, &params);

        // Generic struct: same field rules as the concrete path, but each field's
        // expected type is the application's substituted template annotation.
        self.validate_and_infer_struct_fields(&def.fields, fields, Some(&id_map), &def.name)?;

        *expr_ty = instance_ty;
        Ok(instance_ty)
    }

    fn infer_field_access(&mut self, expr: &mut Expr) -> Result<TypeId, CompilationError> {
        let ExprKind::FieldAccess {
            expr: inner,
            field_name,
        } = &mut expr.kind
        else {
            unreachable!("infer_field_access called on non-FieldAccess");
        };

        let container_ty = self.infer_expr(inner)?;

        // Generic (template) instance (parameters may still be unknown mid-pass):
        // resolve the field type through the template's substituted definition.
        let rep = self.store.find_rep(container_ty);
        if let Some(inst) = self.monomorphs.instance_types.get(&rep).cloned() {
            return self.infer_field_access_in_instance(&mut expr.ty, field_name, &inst);
        }

        let resolved = self.store.resolve(container_ty)?;

        let (struct_name, fields) = match resolved {
            Type::Struct { name, fields } => (name, fields),
            Type::Named(type_name) => {
                // Follow typedef aliases (e.g. `div_t` -> `_div_t`) so field access works on the
                // public typedef name, not just the underlying struct tag.
                //
                // This is platform-independent: it only matters for headers that expose a typedef'd
                // struct (which happens on MSVC/Windows). Headers using anonymous struct tags are
                // unaffected.
                let mut candidate = type_name.clone();
                loop {
                    match self
                        .store
                        .resolve_typedef_type(&Type::Named(candidate.clone()))
                    {
                        Some(Type::Named(next)) if next != candidate => candidate = next,
                        _ => break,
                    }
                }
                if let Some(struct_def) = self.symbols.lookup_struct(&candidate) {
                    let fields: Vec<(String, TypeId)> = struct_def
                        .fields
                        .iter()
                        .map(|f| (f.name.clone(), f.ty))
                        .collect();
                    (candidate, fields)
                } else if let Some(enum_def) = self.symbols.lookup_enum(&candidate) {
                    if let Some(variant) = enum_def
                        .variants
                        .iter()
                        .find(|v| v.args.iter().any(|a| a.name == *field_name))
                    {
                        let fields: Vec<(String, TypeId)> = variant
                            .args
                            .iter()
                            .map(|f| (f.name.clone(), f.ty))
                            .collect();
                        (type_name, fields)
                    } else {
                        return Err(type_err!(
                            "Enum '{}' has no field '{}'",
                            type_name,
                            field_name
                        ));
                    }
                } else {
                    return Err(type_err!(
                        "Cannot access field on unknown type '{}'",
                        type_name
                    ));
                }
            }
            _ => return Err(type_err!("Cannot access field on non-struct type")),
        };

        let field_type_id = fields
            .iter()
            .find(|(fname, _)| fname == field_name)
            .ok_or_else(|| {
                type_err!(
                    "Struct/variant '{}' has no field '{}'",
                    struct_name,
                    field_name
                )
            })?
            .1;

        expr.ty = field_type_id;
        Ok(field_type_id)
    }

    /// Infer field access on a generic instance, e.g. `b.innerValue` for a bare `WrapperType`.
    ///
    /// The field's type is the substituted template annotation, keeping self-referential types
    /// (e.g. `var x: Box;` with `x.value: Box[T]`) consistent with the application's parameters.
    ///
    /// # Errors
    /// Returns `TypeError` if the field is absent or its type cannot be resolved.
    fn infer_field_access_in_instance(
        &mut self,
        expr_ty: &mut TypeId,
        field_name: &str,
        inst: &PendingInstance,
    ) -> Result<TypeId, CompilationError> {
        if let Some((_, TemplateDef::Struct(def))) =
            self.monomorphs.templates.get(&inst.base).cloned()
        {
            let id_map = param_id_map(&def.type_params, &inst.params);
            let field = def
                .fields
                .iter()
                .find(|f| f.name == field_name)
                .ok_or_else(|| type_err!("Struct '{}' has no field '{}'", def.name, field_name))?;
            let field_ty = self.type_id_from_annotation(field.annotation.as_ref(), &id_map)?;
            *expr_ty = field_ty;
            return Ok(field_ty);
        }
        if let Some((_, TemplateDef::Enum(ed))) = self.monomorphs.templates.get(&inst.base).cloned()
        {
            let variant = ed
                .variants
                .iter()
                .find(|v| v.args.iter().any(|a| a.name == *field_name))
                .ok_or_else(|| type_err!("Enum '{}' has no field '{}'", ed.name, field_name))?;
            let id_map = param_id_map(&ed.type_params, &inst.params);
            let field = variant
                .args
                .iter()
                .find(|a| a.name == *field_name)
                .expect("variant field presence checked above");
            let field_ty = self.type_id_from_annotation(field.annotation.as_ref(), &id_map)?;
            *expr_ty = field_ty;
            return Ok(field_ty);
        }
        Err(type_err!(
            "Cannot access field on unknown type '{}'",
            inst.base
        ))
    }

    fn infer_enum_variant(&mut self, expr: &mut Expr) -> Result<TypeId, CompilationError> {
        let ExprKind::EnumVariant {
            enum_name,
            variant_name,
        } = &mut expr.kind
        else {
            unreachable!("infer_enum_variant called on non-EnumVariant");
        };

        // Simple variants of generic (template) enums are instances of the
        // monomorph their context selects.
        if self.is_template_enum(enum_name) {
            let (_, ty) = self.instance_params_and_type(enum_name, &[], &HashMap::new())?;
            expr.ty = ty;
            return Ok(ty);
        }

        let _variant_info = self
            .symbols
            .lookup_variant(enum_name, variant_name)
            .ok_or_else(|| type_err!("Unknown enum variant '{}.{}'", enum_name, variant_name))?;

        let enum_ty = self.store.new_known(Type::Named(enum_name.clone()));
        expr.ty = enum_ty;
        Ok(enum_ty)
    }

    fn infer_enum_init(&mut self, expr: &mut Expr) -> Result<TypeId, CompilationError> {
        let ExprKind::EnumInit {
            enum_name,
            variant_name,
            args,
        } = &mut expr.kind
        else {
            unreachable!("infer_enum_init called on non-EnumInit");
        };

        // Generic (template) enum: same instantiation as constructor calls.
        if self.is_template_enum(enum_name) {
            return self.infer_generic_enum_ctor(&mut expr.ty, enum_name, variant_name, args);
        }

        let (variant_info, enum_def) = {
            let info = self
                .symbols
                .lookup_variant(enum_name, variant_name)
                .ok_or_else(|| type_err!("Unknown enum variant '{}.{}'", enum_name, variant_name))?
                .clone();

            let enum_def = self
                .symbols
                .lookup_enum(enum_name)
                .ok_or_else(|| type_err!("Unknown enum '{}'", enum_name))?
                .clone();

            (info, enum_def)
        };

        let resolved_args = Self::resolve_default_args(&variant_info, &enum_def, args)?;
        *args = resolved_args;

        if args.len() != variant_info.arg_types.len() {
            return Err(type_err!(
                "Enum variant '{}.{}' expects {} arguments, got {}",
                enum_name,
                variant_name,
                variant_info.arg_types.len(),
                args.len()
            ));
        }

        for (arg, &expected_ty) in args.iter_mut().zip(variant_info.arg_types.iter()) {
            let arg_ty = self.infer_expr(arg)?;
            self.unify(arg_ty, expected_ty)?;
            arg.ty = expected_ty;
        }

        let enum_ty = self.store.new_known(Type::Named(enum_name.clone()));
        expr.ty = enum_ty;
        Ok(enum_ty)
    }

    /// Infer type for an array literal expression.
    /// All elements must unify to the same type, and the result is `CArray(element_type, len)`.
    /// Empty array literals (`[]`) are rejected because the element type can't be determined.
    fn infer_array_literal(&mut self, expr: &mut Expr) -> Result<TypeId, CompilationError> {
        let ExprKind::ArrayLiteral { elements } = &mut expr.kind else {
            unreachable!("infer_array_literal called on non-ArrayLiteral");
        };

        // At least one element needed to infer the element type
        if elements.is_empty() {
            return Err(type_err!(
                "Empty array literal '[]' is not supported; add at least one element or a type annotation"
            ));
        }

        // Infer the first element's type as the element type
        let elem_ty_id = self.infer_expr(&mut elements[0])?;

        // Unify all remaining elements with the first element's type
        for elem in elements.iter_mut().skip(1) {
            let e_ty = self.infer_expr(elem)?;
            self.unify(elem_ty_id, e_ty)?;
            elem.ty = elem_ty_id;
        }

        // Resolve the element type to store it concretely in the CArray type
        let elem_ty = self
            .store
            .resolve(elem_ty_id)
            .map_err(|e| type_err!("Failed to resolve array element type: {e}"))?;
        let array_ty = Type::CArray(Box::new(elem_ty), elements.len());
        expr.ty = self.store.new_known(array_ty);
        Ok(expr.ty)
    }

    /// Infer type for a tuple literal (e.g., `(1, "x", 2.0)`).
    /// Each element is inferred independently; the literal's type is the
    /// positional `Tuple` of element types. The shape is recorded so codegen
    /// emits a matching C struct.
    fn infer_tuple_lit(&mut self, expr: &mut Expr) -> Result<TypeId, CompilationError> {
        let ExprKind::TupleLit { elements } = &mut expr.kind else {
            unreachable!("infer_tuple_lit called on non-TupleLit");
        };

        if elements.is_empty() {
            return Err(type_err!(
                "Empty tuple literal '()' is not supported; use at least two elements"
            ));
        }

        let mut elem_type_ids = Vec::with_capacity(elements.len());
        for elem in elements.iter_mut() {
            elem_type_ids.push(self.infer_expr(elem)?);
        }

        let mut elem_types = Vec::with_capacity(elements.len());
        for (elem, id) in elements.iter_mut().zip(elem_type_ids.iter()) {
            let t = self
                .store
                .resolve(*id)
                .map_err(|e| type_err!("Failed to resolve tuple element type: {e}"))?;
            elem.ty = *id;
            elem_types.push(t);
        }

        let tuple_ty = Type::Tuple(elem_types.clone());
        self.record_tuple(&elem_types);
        expr.ty = self.store.new_known(tuple_ty);
        Ok(expr.ty)
    }

    /// Infer type for an array index expression (e.g., `arr[i]`).
    /// Resolves the container to get the element type, and unifies the index with Int.
    fn infer_index(&mut self, expr: &mut Expr) -> Result<TypeId, CompilationError> {
        let ExprKind::Index {
            expr: container,
            index,
        } = &mut expr.kind
        else {
            unreachable!("infer_index called on non-Index");
        };
        let container_ty = self.infer_expr(container)?;
        let resolved = self.store.resolve(container_ty)?;

        match resolved {
            Type::Tuple(elems) => {
                // Tuple slot access: index must be an integer literal in range.
                // (Runtime/non-literal indices are rejected, matching the
                // Haskell compiler, because the slot is a struct member name.)
                let ExprKind::Literal {
                    value: Literal::Int(i),
                } = &index.kind
                else {
                    return Err(type_err!(
                        "Tuple elements can only be accessed with integer literal indices"
                    ));
                };
                if *i < 0 || *i as usize >= elems.len() {
                    return Err(type_err!(
                        "Tuple index {i} out of range (tuple has {} elements)",
                        elems.len()
                    ));
                }
                let elem_ty = self.store.new_known(elems[*i as usize].clone());
                expr.ty = elem_ty;
                Ok(elem_ty)
            }
            _ => {
                let index_ty = self.infer_expr(index)?;
                let int_ty = self.store.new_known(Type::Int);
                self.unify(index_ty, int_ty)?;

                let elem_ty = match resolved {
                    Type::CArray(elem_type, _) => self.store.new_known(*elem_type),
                    Type::Ptr(inner) => self.store.new_known(*inner),
                    _ => {
                        return Err(type_err!("Cannot index non-array type: {resolved}"));
                    }
                };
                expr.ty = elem_ty;
                Ok(elem_ty)
            }
        }
    }

    /// Resolve default arguments for enum variant constructors.
    /// Returns a new Vec with default values filled in.
    /// Follows the Haskell compiler's `addDefaultArgs` function.
    fn resolve_default_args(
        variant_info: &EnumVariantInfo,
        enum_def: &EnumDefinition,
        provided_args: &[Expr],
    ) -> CompileResult<Vec<Expr>> {
        let total_required = variant_info.arg_types.len();
        let mut result = provided_args.to_vec();

        if result.len() < total_required {
            let variant = enum_def
                .variants
                .iter()
                .find(|v| v.name == variant_info.variant_name)
                .ok_or_else(|| {
                    type_err!(
                        "Variant '{}' not found in enum '{}'",
                        variant_info.variant_name,
                        variant_info.enum_name
                    )
                })?;

            let provided_len = result.len();
            for i in provided_len..total_required {
                if let Some(default_expr) = variant.args.get(i).and_then(|f| f.default.as_ref()) {
                    result.push(default_expr.clone());
                }
            }
        }
        Ok(result)
    }

    /// Unify two type IDs.
    ///
    /// Generic applications (instance type variables) unify specially:
    ///
    /// - two applications of the same template unify their parameters pairwise
    ///   (mirroring the reference's `unifyTemplateVars`, e.g. a bare
    ///   `WrapperType` annotation unified with an `WrapperType[Int]` initializer);
    /// - an application unified with the `Named(monomorph)` of its own template
    ///   unifies its parameters with the monomorph's concrete arguments;
    /// - an application unified with anything else is a type mismatch
    ///   (except an unconstrained unknown, which binds as usual).
    ///
    /// # Errors
    /// Returns `TypeError` on a mismatch between two distinct template applications or between an
    /// application and an incompatible concrete type.
    fn unify(&mut self, a: TypeId, b: TypeId) -> CompileResult<()> {
        let rep_a = self.store.find_rep(a);
        let rep_b = self.store.find_rep(b);

        if rep_a == rep_b {
            return Ok(());
        }

        let inst_a = self.monomorphs.instance_types.get(&rep_a).cloned();
        let inst_b = self.monomorphs.instance_types.get(&rep_b).cloned();

        match (inst_a, inst_b) {
            // Two applications of the same template: unify parameters pairwise.
            (Some(ia), Some(ib)) => {
                if ia.base != ib.base {
                    return Err(type_err!(
                        "Type mismatch: instance of '{}' vs instance of '{}'",
                        ia.base,
                        ib.base,
                    ));
                }
                if ia.params.len() != ib.params.len() {
                    return Err(type_err!(
                        "Type mismatch: incompatible applications of '{}'",
                        ia.base,
                    ));
                }
                for (pa, pb) in ia.params.iter().zip(ib.params.iter()) {
                    self.unify(*pa, *pb)?;
                }
                self.store.unify(a, b)
            }
            // Application unified with its own concrete monomorph.
            (Some(ia), None) => self.unify_instance_with_known(ia, rep_b, a, b),
            (None, Some(ib)) => self.unify_instance_with_known(ib, rep_a, b, a),
            (None, None) => self.store.unify(a, b),
        }
    }

    /// Unify an instance variable with a known type.
    ///
    /// Binds the instance variable to the monomorph when `known` is exactly that application's
    /// monomorph, errors on a mismatch with any other definite type, and falls back to a plain bind
    /// when `known` is still an unbound unknown.
    ///
    /// # Errors
    /// Returns `TypeError` when `known` is a definite, non-matching type.
    fn unify_instance_with_known(
        &mut self,
        inst: super::monomorph::PendingInstance,
        known_rep: TypeId,
        inst_id: TypeId,
        known_id: TypeId,
    ) -> CompileResult<()> {
        match self.store.resolve(known_rep) {
            // The application's monomorph: tie the instance's parameters to its
            // concrete arguments, then bind the instance variable to it.
            Ok(Type::Named(name)) => {
                if let Some((base, args)) = self.monomorphs.mono_instances.get(&name).cloned()
                    && base == inst.base
                    && args.len() == inst.params.len()
                {
                    let arg_ids: Vec<TypeId> = args
                        .iter()
                        .map(|arg| self.store.new_known(arg.clone()))
                        .collect();
                    for (param, arg_id) in inst.params.iter().zip(arg_ids.iter()) {
                        self.unify(*param, *arg_id)?;
                    }
                    return self.store.unify(inst_id, known_id);
                }
                Err(type_err!(
                    "Type mismatch: instance of '{}' vs {}",
                    inst.base,
                    name,
                ))
            }
            // Any other definite type side: a genuine mismatch.
            Ok(other) => Err(type_err!(
                "Type mismatch: instance of '{}' vs {}",
                inst.base,
                other,
            )),
            // The other side is still an unbound unknown: bind as usual.
            Err(_) => self.store.unify(inst_id, known_id),
        }
    }
}
